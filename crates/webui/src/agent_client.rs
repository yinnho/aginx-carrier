//! agent:// 协议客户端——webui 经网关路由外部 CLI 工具（第三刀）。
//!
//! agc（CLI）之后生态的第二个协议客户端，以库形态内嵌于 webui。
//! 连接一次性：TLS 连 relay → `connect` 握手（按 target 路由到网关）→
//! `initialize` → `prompt`（chunk 流式）/ `sessions/list`。协议形状逐字对齐
//! agc/src/main.rs。
//!
//! 会话续接（批2 语义）：网关翻译器（ACP.md §2.5/§2.6）把 CLI 方言翻成
//! 纯文本 chunk，并从 agent 输出收割真会话 id——最终 result 帧带
//! `sessionId`/`costUsd`/`durationMs`/`numTurns`。本客户端只管把
//! sessionId 存盘、下轮回喂 `prompt.sessionId` → 网关拼 `--resume`
//!（aginx.toml `[session] resume_args` 模板）。历史会话列表来自网关
//! 台账 `sessions/list`（§2.4.1），不再解析 agent 私有输出方言。

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

/// 网关台账会话条目（sessions/list 结果，§2.4.1）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySession {
    pub session_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub last_ts: String,
    #[serde(default)]
    pub turns: u64,
}

/// prompt 一轮的收尾事实（来自最终 result 帧——网关收割字段）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentPromptResult {
    /// 本轮全部纯文本 chunk 拼接
    pub text: String,
    /// agent 真会话 id（收割，下轮续接用）
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub num_turns: Option<u64>,
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

    /// 解析任意 agent:// URL 并注入本机 relay secret——同一张 relay 网
    /// 网级凭证共享（本机网关配置的 [relay] 段是真源）。跨网网关需要
    /// 各自 secret 时再扩展 per-gateway 覆盖。
    pub fn from_url_with_local_secret(url: &str) -> Option<Self> {
        let mut ep = Self::parse_url(url)?;
        if ep.relay_secret.is_none() {
            ep.relay_secret = Self::from_gateway_config().and_then(|e| e.relay_secret);
        }
        Some(ep)
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
        // config 的 domain 字段是完整 relay 域名（如 "relay.aginx.net"），
        // 与 agent:// URL 里 `.relay.` 后缀的语义不同——不另拼前缀
        let tls_domain = relay.domain?;
        Some(Self {
            host: format!("{target}.{tls_domain}"),
            target,
            port: relay.port.unwrap_or(8443),
            tls_domain,
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

/// 从最终 result 帧收割收尾事实（纯函数，金样本在此）。
/// `text` 是本轮 chunk 拼接（最终帧不带全文）。
fn harvest_final(result: &Value, text: String) -> AgentPromptResult {
    AgentPromptResult {
        text,
        session_id: result
            .get("sessionId")
            .and_then(|s| s.as_str())
            .filter(|s| valid_session_id(s))
            .map(String::from),
        cost_usd: result.get("costUsd").and_then(|c| c.as_f64()),
        duration_ms: result.get("durationMs").and_then(|d| d.as_u64()),
        num_turns: result.get("numTurns").and_then(|n| n.as_u64()),
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

    /// 通用 RPC：发请求 → 等同 id 响应。error 帧 → Err(message)。
    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.alloc_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        loop {
            let resp = self.recv_timeout(RPC_TIMEOUT).await?;
            if resp.get("id").and_then(|v| v.as_i64()) != Some(id) {
                continue; // 乱序帧（chunk 等），等本请求的响应
            }
            if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
                return Err(err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or(method)
                    .to_string());
            }
            return Ok(resp);
        }
    }

    /// 协议握手。public 网关无需 authToken。
    pub async fn initialize(&mut self) -> Result<(), String> {
        self.rpc(
            "initialize",
            json!({
                "protocolVersion": "0.1.0",
                "clientInfo": {"name": "aginx-carrier-webui", "version": "0.1.0"}
            }),
        )
        .await
        .map(|_| ())
    }

    /// 网关已注册 agent 列表。
    pub async fn list_agents(&mut self) -> Result<Vec<GatewayAgent>, String> {
        let resp = self.rpc("listAgents", json!({})).await?;
        let agents = resp
            .pointer("/result/agents")
            .cloned()
            .or_else(|| resp.get("agents").cloned())
            .unwrap_or(Value::Null);
        serde_json::from_value(agents).map_err(|e| format!("agents 解析失败: {e}"))
    }

    /// 网关台账会话列表（§2.4.1：网关经手的轮按注册名记账）。
    pub async fn sessions_list(&mut self, agent: &str) -> Result<Vec<GatewaySession>, String> {
        let resp = self.rpc("sessions/list", json!({"agent": agent})).await?;
        let sessions = resp
            .pointer("/result/sessions")
            .cloned()
            .unwrap_or(Value::Null);
        serde_json::from_value(sessions).map_err(|e| format!("sessions 解析失败: {e}"))
    }

    /// 发一轮 prompt。chunk 已是网关翻译后的纯文本，经 `on_chunk` 转发
    ///（返回 false = 客户端断开，停止转发）。终帧（result.stopReason，
    /// 无 id——ack 被吞铁则吃掉）收割 sessionId/成本三件套。
    pub async fn prompt<F>(
        &mut self,
        agent: &str,
        message: &str,
        session_id: Option<&str>,
        mut on_chunk: F,
    ) -> Result<AgentPromptResult, String>
    where
        F: FnMut(&str) -> bool,
    {
        let mut params = json!({"agent": agent, "message": message});
        if let Some(sid) = session_id {
            params["sessionId"] = json!(sid);
        }
        let id = self.alloc_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "prompt",
            "params": params
        }))
        .await?;

        let mut deltas: Vec<String> = Vec::new();
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

            // 终帧：result 带 stopReason 且无 id（外部协议 §2.5）
            if resp.pointer("/result/stopReason").is_some() {
                let text = deltas.concat();
                let result = resp.get("result").cloned().unwrap_or(Value::Null);
                return Ok(harvest_final(&result, text));
            }

            // chunk 通知：params.text = 网关翻译后的纯文本段
            if resp.get("method").and_then(|m| m.as_str()) == Some("chunk") {
                if let Some(text) = resp.pointer("/params/text").and_then(|t| t.as_str()) {
                    if !on_chunk(text) {
                        return Err("客户端断开".to_string());
                    }
                    deltas.push(text.to_string());
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
    fn from_url_keeps_parsed_fields_without_local_config() {
        // 解析不依赖本机配置存在（secret 注入是尽力而为）
        let ep = AgentEndpoint::from_url_with_local_secret("agent://sv1.relay.aginx.net:9443")
            .unwrap();
        assert_eq!(ep.target, "sv1");
        assert_eq!(ep.port, 9443);
        assert_eq!(ep.tls_domain, "relay.aginx.net");
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

    // ── 终帧收割（§2.5 翻译轮最终结果） ──

    #[test]
    fn final_frame_harvests_session_and_costs() {
        // 金样本 = 网关外部协议 §2.5（external_final_result_translated）
        let result: Value = serde_json::from_str(
            r#"{"stopReason": "endTurn", "sessionId": "b8f713a4-ea3b-4d6c-920b-87dc2a0403f0", "costUsd": 0.015, "durationMs": 8400, "numTurns": 1}"#,
        )
        .unwrap();
        let r = harvest_final(&result, "已记".into());
        assert_eq!(r.text, "已记");
        assert_eq!(
            r.session_id.as_deref(),
            Some("b8f713a4-ea3b-4d6c-920b-87dc2a0403f0")
        );
        assert_eq!(r.cost_usd, Some(0.015));
        assert_eq!(r.duration_ms, Some(8400));
        assert_eq!(r.num_turns, Some(1));
    }

    #[test]
    fn final_frame_missing_fields_and_poison_sid() {
        let r = harvest_final(&json!({"stopReason": "endTurn"}), String::new());
        assert!(r.session_id.is_none() && r.cost_usd.is_none());
        let r = harvest_final(&json!({"sessionId": "a;rm -rf"}), "x".into());
        assert!(r.session_id.is_none(), "毒 sessionId 拒收");
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

    // ── 台账会话条目解析（§2.4.1 wire 形状） ──

    #[test]
    fn gateway_session_wire_shape() {
        let v: Value = serde_json::from_str(
            r#"[{"sessionId": "d903c124-0892-4818-af0a-fc8c9f7e29c7", "title": "看一下以前的会话", "lastTs": "2026-08-24T00:03:23Z", "turns": 2}]"#,
        )
        .unwrap();
        let sessions: Vec<GatewaySession> = serde_json::from_value(v).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "d903c124-0892-4818-af0a-fc8c9f7e29c7");
        assert_eq!(sessions[0].title, "看一下以前的会话");
        assert_eq!(sessions[0].turns, 2);
        assert!(sessions[0].last_ts.ends_with('Z'));
    }
}
