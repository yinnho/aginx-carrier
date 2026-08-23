//! agent:// 协议客户端——webui 经网关路由外部 CLI 工具（第三刀）。
//!
//! agc（CLI）之后生态的第二个协议客户端，以库形态内嵌于 webui。
//! 连接一次性：TLS 连 relay → `connect` 握手（按 target 路由到网关）→
//! `initialize` → `prompt`（chunk 流式）。协议形状逐字对齐 agc/src/main.rs。
//!
//! 会话续接的关键分工：网关只回显入参 sessionId，真会话 id 在 agent 的
//! 输出流里（claude `--output-format stream-json` 的 result 行）。本客户端
//! 从 chunk 流收割之，调用方存盘、下轮回喂 `prompt.sessionId` → 网关拼
//! `--resume`（aginx.toml `[session] resume_args` 模板）。

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls;
use tokio_rustls::TlsConnector;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// chunk 间隔上限——agent 单次工具调用/长思考可能数分钟无输出，
/// 对齐网关侧 agent timeout（claude toml timeout=600）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_LINE_SIZE: usize = 8 * 1024 * 1024;

/// 网关端点（relay 形态）。默认取自本机网关配置 `~/.aginx/config.toml`
/// 的 `[relay]` 段——webui 与网关同机同用户，零新增配置。
#[derive(Debug, Clone)]
pub struct AgentEndpoint {
    /// relay 路由 id（如 "qi7o6bj5"），connect 消息的 target
    pub target: String,
    /// 实际连接的 relay host（如 "relay.aginx.net"）
    pub host: String,
    pub port: u16,
    /// TLS SNI 域名
    pub tls_domain: String,
    pub relay_secret: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GatewayAgent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub agent_type: String,
}

/// prompt 一轮的收尾事实（来自 agent 输出流的 result 行）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentPromptResult {
    pub text: String,
    pub session_id: Option<String>,
    pub is_error: bool,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub num_turns: Option<u64>,
}

/// chunk 流里一行 agent 输出的归类（纯函数，单测金样本在此）。
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    /// 助手可见文本（claude assistant 消息的 text 块，整块粒度）
    Delta(String),
    /// 思考过程（thinking 块）
    Thinking(String),
    /// 工具调用（工具名，如 "Write"）
    Tool(String),
    /// result 收尾行
    Result(AgentPromptResult),
    /// 其余（system/user/tool_result/无法解析的行）
    Ignore,
}

impl AgentEndpoint {
    /// 解析 `agent://<id>.relay.<domain>[:port]`（relay 形态；direct 形态
    /// 待网关提供本地入站面后再补，client 只换连接方式）。
    pub fn parse_url(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("agent://")?;
        let host = rest.split('/').next()?;
        let (host, explicit_port) = match host.rfind(':') {
            Some(i) if !host.starts_with('[') => {
                (&host[..i], host[i + 1..].parse::<u16>().ok())
            }
            _ => (host, None),
        };
        let parts: Vec<&str> = host.split(".relay.").collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            return None;
        }
        Some(Self {
            target: parts[0].to_string(),
            host: host.to_string(),
            port: explicit_port.unwrap_or(8443),
            tls_domain: format!("relay.{}", parts[1]),
            relay_secret: None,
        })
    }

    /// 从本机网关配置 `~/.aginx/config.toml` 读端点（id/domain/port/
    /// use_tls/relay_secret）。网关未装或未配置时返回 None。
    pub fn from_gateway_config() -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct RelaySection {
            id: Option<String>,
            domain: Option<String>,
            port: Option<u16>,
            relay_secret: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct GatewayConfig {
            relay: Option<RelaySection>,
        }
        let path = dirs::home_dir()?.join(".aginx/config.toml");
        let content = std::fs::read_to_string(path).ok()?;
        let cfg: GatewayConfig = toml::from_str(&content).ok()?;
        let relay = cfg.relay?;
        let target = relay.id?;
        let domain = relay.domain?;
        Some(Self {
            host: format!("{target}.relay.{domain}"),
            target,
            port: relay.port.unwrap_or(8443),
            tls_domain: format!("relay.{domain}"),
            relay_secret: relay.relay_secret,
        })
    }
}

/// 与网关 handler 同一套字符集门：字母数字 + 连字符 + 下划线。
/// 非法 id 不入库、不回喂（防注入 resume 参数）。
pub fn valid_session_id(sid: &str) -> bool {
    !sid.is_empty()
        && sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 解析 claude `--output-format stream-json` 的一行输出。
/// 容错优先：任何认不出的行一律 Ignore，整个会话没有 result 行时
/// 调用方把全部 Delta 拼接当纯文本（其他 CLI 直通形态）。
pub fn parse_stream_line(line: &str) -> StreamItem {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return StreamItem::Ignore;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => {
            // 一条 assistant 消息含多块。全部扫完再决定：有 text 块 → Delta
            //（多块拼接）；否则首个非空 thinking → Thinking；否则首个
            // tool_use 名 → Tool（工具事件通常伴随独立消息，这里降级兜底）。
            let blocks = v
                .pointer("/message/content")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            let mut texts = Vec::new();
            let mut thinking: Option<String> = None;
            let mut tool: Option<String> = None;
            for b in &blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            if !t.is_empty() {
                                texts.push(t.to_string());
                            }
                        }
                    }
                    Some("thinking") => {
                        if thinking.is_none() {
                            if let Some(t) = b.get("thinking").and_then(|t| t.as_str()) {
                                if !t.is_empty() {
                                    thinking = Some(t.to_string());
                                }
                            }
                        }
                    }
                    Some("tool_use") => {
                        if tool.is_none() {
                            if let Some(n) = b.get("name").and_then(|n| n.as_str()) {
                                tool = Some(n.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            if !texts.is_empty() {
                StreamItem::Delta(texts.join("\n"))
            } else if let Some(t) = thinking {
                StreamItem::Thinking(t)
            } else if let Some(n) = tool {
                StreamItem::Tool(n)
            } else {
                StreamItem::Ignore
            }
        }
        Some("result") => StreamItem::Result(AgentPromptResult {
            text: v
                .get("result")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            session_id: v
                .get("session_id")
                .and_then(|s| s.as_str())
                .filter(|s| valid_session_id(s))
                .map(String::from),
            is_error: v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false),
            cost_usd: v.get("total_cost_usd").and_then(|c| c.as_f64()),
            duration_ms: v.get("duration_ms").and_then(|d| d.as_u64()),
            num_turns: v.get("num_turns").and_then(|n| n.as_u64()),
        }),
        _ => StreamItem::Ignore,
    }
}

type ConnStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// 一次性协议连接（TLS relay 形态）。
pub struct AgentConn {
    reader: BufReader<tokio::io::ReadHalf<ConnStream>>,
    writer: tokio::io::WriteHalf<ConnStream>,
    next_id: i64,
}

impl AgentConn {
    /// TLS 连 relay → connect 握手（target 路由 + relay secret）。
    pub async fn connect(ep: &AgentEndpoint) -> Result<Self, String> {
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((ep.tls_domain.as_str(), ep.port)),
        )
        .await
        .map_err(|_| format!("连接 {}:{} 超时", ep.tls_domain, ep.port))?
        .map_err(|e| format!("连接 {}:{} 失败: {e}", ep.tls_domain, ep.port))?;

        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS 配置失败: {e}"))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let domain = rustls_pki_types::ServerName::try_from(ep.tls_domain.clone())
            .map_err(|e| format!("TLS 域名无效: {e}"))?;
        let tls_stream = connector
            .connect(domain, stream)
            .await
            .map_err(|e| format!("TLS 握手失败: {e}"))?;
        let (r, w) = tokio::io::split(tls_stream);
        let mut conn = Self {
            reader: BufReader::new(r),
            writer: w,
            next_id: 1,
        };

        let mut connect_msg = json!({"type": "connect", "target": ep.target});
        if let Some(ref secret) = ep.relay_secret {
            connect_msg["token"] = json!(secret);
        }
        conn.send(&connect_msg).await?;
        let resp = conn.recv_timeout(RPC_TIMEOUT).await?;
        match resp.get("type").and_then(|v| v.as_str()) {
            Some("connected") => Ok(conn),
            Some("error") => Err(resp
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("relay 未知错误")
                .to_string()),
            other => Err(format!("relay 意外响应: {other:?}")),
        }
    }

    async fn send(&mut self, msg: &Value) -> Result<(), String> {
        let mut data = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        data.push('\n');
        self.writer
            .write_all(data.as_bytes())
            .await
            .map_err(|e| format!("发送失败: {e}"))
    }

    /// 读一行（跳过 relay ping/pong 心跳）。连接关闭 → Err。
    async fn recv(&mut self) -> Result<Value, String> {
        loop {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .await
                .map_err(|e| format!("读取失败: {e}"))?;
            if n == 0 {
                return Err("连接已关闭".to_string());
            }
            if line.len() > MAX_LINE_SIZE {
                return Err(format!("行超长（{} 字节）", line.len()));
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<Value>(line) {
                match val.get("type").and_then(|v| v.as_str()) {
                    Some("ping") | Some("pong") => continue,
                    _ => return Ok(val),
                }
            }
        }
    }

    async fn recv_timeout(&mut self, timeout: Duration) -> Result<Value, String> {
        tokio::time::timeout(timeout, self.recv())
            .await
            .map_err(|_| "等待响应超时".to_string())?
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// 协议握手。public 网关无需 authToken。
    pub async fn initialize(&mut self) -> Result<(), String> {
        let id = self.alloc_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "0.1.0",
                "clientInfo": {"name": "aginx-carrier-webui", "version": "0.1.0"}
            }
        }))
        .await?;
        let resp = self.recv_timeout(RPC_TIMEOUT).await?;
        if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
            return Err(err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("initialize 失败")
                .to_string());
        }
        Ok(())
    }

    /// 网关已注册 agent 列表。
    pub async fn list_agents(&mut self) -> Result<Vec<GatewayAgent>, String> {
        let id = self.alloc_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "listAgents",
        }))
        .await?;
        let resp = self.recv_timeout(RPC_TIMEOUT).await?;
        if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
            return Err(err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("listAgents 失败")
                .to_string());
        }
        let agents = resp
            .pointer("/result/agents")
            .cloned()
            .or_else(|| resp.get("agents").cloned())
            .unwrap_or(Value::Null);
        serde_json::from_value(agents).map_err(|e| format!("agents 解析失败: {e}"))
    }

    /// 发一轮 prompt，chunk 流经 `on_item` 转发（返回 false = 客户端断开，
    /// 停止转发并尽快结束）。收尾返回 result 行事实；整个流没有 result 行
    /// 时（非 claude CLI 直通形态）返回 Delta 拼接的纯文本。
    pub async fn prompt<F>(
        &mut self,
        agent: &str,
        message: &str,
        cwd: Option<&str>,
        session_id: Option<&str>,
        mut on_item: F,
    ) -> Result<AgentPromptResult, String>
    where
        F: FnMut(&StreamItem) -> bool,
    {
        let id = self.alloc_id();
        let mut params = json!({"agent": agent, "message": message});
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd);
        }
        if let Some(sid) = session_id {
            params["sessionId"] = json!(sid);
        }
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "prompt",
            "params": params
        }))
        .await?;

        let mut deltas: Vec<String> = Vec::new();
        let mut result: Option<AgentPromptResult> = None;
        loop {
            // idle watchdog：单次等待上限，不做整轮墙钟（长任务合法）
            let resp = tokio::time::timeout(IDLE_TIMEOUT, self.recv())
                .await
                .map_err(|_| "agent 响应超时（600s 无输出）".to_string())??;

            if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("prompt 失败");
                return Err(msg.to_string());
            }

            // 终帧：result 带 stopReason，或回显我们的请求 id
            let is_final = resp
                .pointer("/result/stopReason")
                .is_some()
                || resp.get("id").and_then(|v| v.as_i64()) == Some(id);
            if is_final {
                return Ok(result.unwrap_or_else(|| AgentPromptResult {
                    text: deltas.join(""),
                    ..Default::default()
                }));
            }

            // chunk 通知：params.text = agent stdout 的一行
            if resp.get("method").and_then(|m| m.as_str()) == Some("chunk") {
                let tagged = resp.get("id").and_then(|v| v.as_i64());
                if tagged.is_none() || tagged == Some(id) {
                    if let Some(text) = resp.pointer("/params/text").and_then(|t| t.as_str()) {
                        match parse_stream_line(text) {
                            StreamItem::Delta(t) => {
                                deltas.push(t.clone());
                                if !on_item(&StreamItem::Delta(t)) {
                                    return Err("客户端断开".to_string());
                                }
                            }
                            item @ (StreamItem::Thinking(_) | StreamItem::Tool(_)) => {
                                if !on_item(&item) {
                                    return Err("客户端断开".to_string());
                                }
                            }
                            StreamItem::Result(r) => {
                                result = Some(r);
                            }
                            StreamItem::Ignore => {}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AgentEndpoint 解析 ──

    #[test]
    fn parse_relay_url_default_port() {
        let ep = AgentEndpoint::parse_url("agent://qi7o6bj5.relay.aginx.net").unwrap();
        assert_eq!(ep.target, "qi7o6bj5");
        assert_eq!(ep.host, "qi7o6bj5.relay.aginx.net");
        assert_eq!(ep.port, 8443);
        assert_eq!(ep.tls_domain, "relay.aginx.net");
    }

    #[test]
    fn parse_relay_url_explicit_port_and_path() {
        let ep =
            AgentEndpoint::parse_url("agent://abc.relay.example.com:9443/claude").unwrap();
        assert_eq!(ep.target, "abc");
        assert_eq!(ep.port, 9443);
        assert_eq!(ep.tls_domain, "relay.example.com");
    }

    #[test]
    fn parse_rejects_non_relay_and_garbage() {
        assert!(AgentEndpoint::parse_url("agent://192.168.1.1:86/claude").is_none());
        assert!(AgentEndpoint::parse_url("https://relay.aginx.net").is_none());
        assert!(AgentEndpoint::parse_url("agent://only.relay.").is_none());
        assert!(AgentEndpoint::parse_url("agent://.relay.x.com").is_none());
    }

    #[test]
    fn gateway_config_toml_shape() {
        // 金样本：与 ~/.aginx/config.toml 的 [relay] 段同形
        let toml_src = r#"
[server]
mode = "relay"

[relay]
id = "test123"
domain = "relay.example.net"
port = 8443
use_tls = true
url = "test123.relay.example.net:8443"
relay_secret = "s3cret"
"#;
        #[derive(serde::Deserialize)]
        struct RelaySection {
            id: Option<String>,
            domain: Option<String>,
            port: Option<u16>,
            relay_secret: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct GatewayConfig {
            relay: Option<RelaySection>,
        }
        let cfg: GatewayConfig = toml::from_str(toml_src).unwrap();
        let relay = cfg.relay.unwrap();
        assert_eq!(relay.id.as_deref(), Some("test123"));
        assert_eq!(relay.domain.as_deref(), Some("relay.example.net"));
        assert_eq!(relay.port, Some(8443));
        assert_eq!(relay.relay_secret.as_deref(), Some("s3cret"));
    }

    // ── parse_stream_line ──

    #[test]
    fn stream_assistant_text_block() {
        let line = r#"{"type":"assistant","message":{"content":[{"text":"收到","type":"text"}],"id":"msg_1","role":"assistant"},"session_id":"abc"}"#;
        assert_eq!(parse_stream_line(line), StreamItem::Delta("收到".into()));
    }

    #[test]
    fn stream_assistant_multi_text_blocks_joined() {
        let line = r#"{"type":"assistant","message":{"content":[{"text":"第一段","type":"text"},{"text":"第二段","type":"text"}]}}"#;
        assert_eq!(
            parse_stream_line(line),
            StreamItem::Delta("第一段\n第二段".into())
        );
    }

    #[test]
    fn stream_assistant_thinking_block() {
        let line = r#"{"type":"assistant","message":{"content":[{"thinking":"让我想想","type":"thinking"}]}}"#;
        assert_eq!(parse_stream_line(line), StreamItem::Thinking("让我想想".into()));
    }

    #[test]
    fn stream_assistant_tool_use_block() {
        let line = r#"{"type":"assistant","message":{"content":[{"input":{"file_path":"/x"},"name":"Write","type":"tool_use"}]}}"#;
        assert_eq!(parse_stream_line(line), StreamItem::Tool("Write".into()));
    }

    #[test]
    fn stream_assistant_mixed_blocks_prefers_text() {
        // text + tool_use 同一条消息：文本优先（工具事件下一条消息还会来）
        let line = r#"{"type":"assistant","message":{"content":[{"thinking":"想","type":"thinking"},{"text":"结论","type":"text"}]}}"#;
        assert_eq!(parse_stream_line(line), StreamItem::Delta("结论".into()));
    }

    #[test]
    fn stream_result_line_full_fields() {
        let line = r#"{"type":"result","subtype":"success","result":"西瓜","session_id":"78284656-2bf4-4540-baed-6f5b17032c8e","is_error":false,"total_cost_usd":0.0133878,"duration_ms":15681,"num_turns":1}"#;
        match parse_stream_line(line) {
            StreamItem::Result(r) => {
                assert_eq!(r.text, "西瓜");
                assert_eq!(
                    r.session_id.as_deref(),
                    Some("78284656-2bf4-4540-baed-6f5b17032c8e")
                );
                assert!(!r.is_error);
                assert_eq!(r.cost_usd, Some(0.0133878));
                assert_eq!(r.duration_ms, Some(15681));
                assert_eq!(r.num_turns, Some(1));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn stream_result_rejects_poison_session_id() {
        let line = r#"{"type":"result","result":"x","session_id":"a;rm -rf"}"#;
        match parse_stream_line(line) {
            StreamItem::Result(r) => assert_eq!(r.session_id, None),
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn stream_system_and_user_and_garbage_ignored() {
        let init = r#"{"type":"system","subtype":"init","session_id":"abc","tools":["Bash"]}"#;
        let tool_result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(parse_stream_line(init), StreamItem::Ignore);
        assert_eq!(parse_stream_line(tool_result), StreamItem::Ignore);
        assert_eq!(parse_stream_line("not json at all"), StreamItem::Ignore);
        assert_eq!(parse_stream_line(""), StreamItem::Ignore);
    }

    #[test]
    fn session_id_charset_gate() {
        assert!(valid_session_id("78284656-2bf4-4540-baed-6f5b17032c8e"));
        assert!(valid_session_id("abc_123"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("a;rm"));
        assert!(!valid_session_id("a b"));
        assert!(!valid_session_id("a/b"));
    }
}
