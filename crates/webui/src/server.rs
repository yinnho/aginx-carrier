//! Web UI HTTP 面——`/api/*` + 内嵌 SPA。
//!
//! 桌面形态（`aginx-carrier web`）专属。信任围栏（trust::verify）套在
//! 全部路由前；聊天走 SSE 流式（`send_message_streaming` → axum Sse）。

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use carrier_kernel::kernel::CarrierKernel;
use futures::StreamExt;
use serde::Deserialize;
use tracing::{error, info};

use crate::env_file;
use crate::trust;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const STYLE_CSS: &str = include_str!("../assets/style.css");
const APP_JS: &str = include_str!("../assets/app.js");

pub struct WebState {
    pub(crate) kernel: Arc<CarrierKernel>,
    pub(crate) listen_host: String,
    /// 网关 agent 台账（第三刀）：added 清单 + per (agent,sender,cwd) 会话
    pub(crate) tool_store: Arc<crate::tool_store::ToolStore>,
}

/// 起 Web UI 监听。bind 失败只报错不 panic。
pub async fn serve(kernel: Arc<CarrierKernel>, listen: String) {
    let state = Arc::new(WebState {
        tool_store: Arc::new(crate::tool_store::ToolStore::load(
            kernel.config.home_dir.join("webui/external-tools.json"),
        )),
        kernel,
        listen_host: listen
            .rsplit_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or_else(|| listen.clone()),
    });
    let app = build_app(state);
    match tokio::net::TcpListener::bind(&listen).await {
        Ok(listener) => {
            info!(%listen, "Web UI 在线");
            if let Err(e) = axum::serve(listener, app).await {
                error!(error = %e, "web ui server 异常退出");
            }
        }
        Err(e) => {
            error!(%listen, error = %e, "web ui bind 失败");
        }
    }
}

fn build_app(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/style.css", get(style_css))
        .route("/assets/app.js", get(app_js))
        .route("/api/agents", get(list_agents))
        .route("/api/chat/{agent}", post(chat))
        .route("/api/history", get(history))
        .route("/api/brain", get(get_brain).put(put_brain))
        .route("/api/key", post(post_key))
        // 装分身页（webui 第二刀）：DupHub 市场列表 / 权限预览 / 一键安装
        .route("/api/market", get(crate::market::list))
        .route("/api/market/{name}/preview", get(crate::market::preview))
        .route("/api/market/{name}/install", post(crate::market::install))
        // 接入本地工具（webui 第三刀）：网关 agent 列表 / 添加 / 移除
        .route("/api/tools", get(tools_list))
        .route("/api/tools/{id}/add", post(tools_add))
        .route("/api/tools/{id}/remove", post(tools_remove))
        // 切续接会话（历史对话框点选 / 新建）
        .route("/api/tools/{id}/session", post(tools_set_session))
        // 历史会话列表：透传网关台账 sessions/list（§2.4.1）
        .route("/api/tool-sessions", get(tool_sessions))
        // 远程网关联系人（webui 第五刀，统一 aginx 流程缺口3）
        .route("/api/gateways", get(gateways_list).post(gateways_add))
        .route("/api/gateways/{target}/remove", post(gateways_remove))
        .route("/api/gateways/{target}/bind", post(gateways_bind))
        .route("/api/remote-agents", get(remote_agents))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            trust_middleware,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

async fn trust_middleware(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // POST 仅收 JSON——跨站表单发不出 application/json（无预检），写面即闭。
    let is_write = req.method() != axum::http::Method::GET
        && req.method() != axum::http::Method::HEAD;
    if is_write {
        let json_ok = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json"));
        if !json_ok {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "json only").into_response();
        }
    }
    match trust::verify(&headers, &state.listen_host) {
        Ok(()) => next.run(req).await,
        Err(code) => (code, "untrusted").into_response(),
    }
}

async fn index() -> Response {
    ([("cache-control", "no-cache"), ("content-type", "text/html; charset=utf-8")], INDEX_HTML).into_response()
}

async fn style_css() -> Response {
    ([("cache-control", "no-cache"), ("content-type", "text/css; charset=utf-8")], STYLE_CSS).into_response()
}

async fn app_js() -> Response {
    ([("cache-control", "no-cache"), ("content-type", "application/javascript; charset=utf-8")], APP_JS).into_response()
}

async fn list_agents(State(st): State<Arc<WebState>>) -> Response {
    let agents: Vec<serde_json::Value> = st
        .kernel
        .registry
        .list()
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id.to_string(),
                "name": e.name,
                "display_name": if e.manifest.display_name.is_empty() { e.name.clone() } else { e.manifest.display_name.clone() },
                "description": e.manifest.description,
                "state": format!("{:?}", e.state),
                "emoji": e.identity.emoji,
                "color": e.identity.color,
                "model": e.manifest.model.modality,
                "last_active": e.last_active.to_rfc3339(),
            })
        })
        .collect();
    Json(serde_json::json!({ "agents": agents })).into_response()
}

#[derive(Deserialize)]
struct ChatBody {
    message: String,
    sender_id: String,
}

async fn chat(
    State(st): State<Arc<WebState>>,
    AxumPath(agent): AxumPath<String>,
    Json(body): Json<ChatBody>,
) -> Response {
    // 分叉：本机分身注册表优先；未命中且在网关工具台账 → agent:// 路径
    let entry = match st.kernel.registry.find_by_name(&agent) {
        Some(e) => e,
        None => {
            if st.tool_store.is_added(&agent) {
                return tool_chat(&st, &agent, body).await;
            }
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown agent"}))).into_response();
        }
    };
    let agent_id = entry.id;
    let sender_id = format!("web:{}", body.sender_id);
    let kh: Arc<dyn carrier_runtime::kernel_handle::KernelHandle> = kernel_self_handle(&st.kernel);

    let (rx, join) = match st.kernel.send_message_streaming(
        agent_id,
        &body.message,
        Some(kh),
        Some(sender_id),
        None,
        None,
        Some("web".to_string()),
        None,
    ).await {
        Ok(pair) => pair,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|ev| (ev, rx))
    })
    .filter_map(move |ev| async move {
        match ev {
            carrier_runtime::llm_driver::StreamEvent::TextDelta { text } => Some::<Result<Event, Infallible>>(Ok(sse_frame(serde_json::json!({
                "type": "delta", "text": text,
            })))),
            carrier_runtime::llm_driver::StreamEvent::ThinkingDelta { text } => Some::<Result<Event, Infallible>>(Ok(sse_frame(serde_json::json!({
                "type": "thinking", "text": text,
            })))),
            carrier_runtime::llm_driver::StreamEvent::ToolExecutionResult { name, result_preview, is_error, .. } => {
                Some::<Result<Event, Infallible>>(Ok(sse_frame(serde_json::json!({
                    "type": "tool", "name": name, "preview": result_preview, "is_error": is_error,
                }))))
            }
            _ => None,
        }
    });

    // 轮结束（含错误）后补 done/error 帧再关流。
    let done_overlay = futures::stream::once(async move {
        match join.await {
            Ok(Ok(result)) => Ok(sse_frame(serde_json::json!({
                "type": "done",
                "response": result.response,
                "iterations": result.iterations,
                "tokens": result.total_usage.input_tokens + result.total_usage.output_tokens,
                "silent": result.silent,
            }))),
            Ok(Err(e)) => Ok(sse_frame(serde_json::json!({ "type": "error", "message": e.to_string() }))),
            Err(e) => Ok(sse_frame(serde_json::json!({ "type": "error", "message": format!("join error: {e}") }))),
        }
    });

    Sse::new(stream.chain(done_overlay)).keep_alive(KeepAlive::default()).into_response()
}

fn sse_frame(v: serde_json::Value) -> Event {
    Event::default().data(v.to_string())
}

// ── 接入本地工具（webui 第三刀）：网关 agent 经 agent:// 协议路由 ──

/// 复合联系人 id 语法：`@<target>~<agent>`（第五刀）。
/// `~` 分隔——URL 路径段安全（`/` 有 percent-encode 陷阱），裸 id =
/// 本机网关 agent（第三刀语义，向后兼容）。
fn parse_remote_contact(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix('@')?;
    let (target, agent) = rest.split_once('~')?;
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if ok(target) && ok(agent) {
        Some((target, agent))
    } else {
        None
    }
}

/// 联系人 id →（连接端点，对端 agent id）。裸 id = 本机网关配置；
/// `@target~agent` = 地址簿远程网关（secret 复用本机同网凭证；
/// 绑定过则注入网关层 token——私有网关的 Bound 身份）。
async fn endpoint_for_contact(
    st: &Arc<WebState>,
    id: &str,
) -> Result<(crate::agent_client::AgentEndpoint, String), String> {
    if let Some((target, agent)) = parse_remote_contact(id) {
        let url = st
            .tool_store
            .gateway_url(target)
            .ok_or_else(|| format!("远程网关 {target} 不在地址簿"))?;
        let mut ep = crate::agent_client::AgentEndpoint::from_url_with_local_secret(&url)
            .ok_or_else(|| format!("网关地址无效: {url}"))?;
        ep.auth_token = st.tool_store.gateway_token(target);
        Ok((ep, agent.to_string()))
    } else {
        let ep = crate::agent_client::AgentEndpoint::from_gateway_config()
            .ok_or_else(|| "本机网关未配置（~/.aginx/config.toml [relay] 段缺失）".to_string())?;
        Ok((ep, id.to_string()))
    }
}

/// 网关 agent 现列：listAgents 透传 + 台账标记。网关/relay 不可达时
/// 返回空列表 + gateway_error（前端显示网关状态条，联系人不受影响）。
/// 已添加的远程联系人（复合 id）随 store 元数据一并下发——零网络。
async fn tools_list(State(st): State<Arc<WebState>>) -> Response {
    let remote: Vec<serde_json::Value> = st
        .tool_store
        .added_remote()
        .into_iter()
        .map(|(id, m)| {
            serde_json::json!({
                "id": id,
                "name": m.name,
                "description": m.description,
                "agent_type": m.agent_type,
                "gateway": m.gateway,
                "kind": "remote",
                "added": true,
            })
        })
        .collect();
    let tools = match gateway_call().await {
        Ok(agents) => agents
            .into_iter()
            .map(|a| {
                serde_json::json!({
                    "id": a.id,
                    "name": a.name,
                    "description": a.description,
                    "agent_type": a.agent_type,
                    "kind": "gateway",
                    "added": st.tool_store.is_added(&a.id),
                })
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            // 网关不可达：已添加的仍下发（stale 元数据）——联系人不消失，
            // 前端顶部横幅提示网关状态；聊天时才会真正报错。
            let stale = st
                .tool_store
                .added()
                .into_iter()
                .filter(|id| !id.starts_with('@'))
                .map(|id| {
                    serde_json::json!({
                        "id": id,
                        "name": id,
                        "description": "",
                        "agent_type": "",
                        "kind": "gateway",
                        "added": true,
                    })
                })
                .collect::<Vec<_>>();
            return Json(serde_json::json!({
                "tools": stale,
                "remote": remote,
                "gateway_error": e,
            }))
            .into_response();
        }
    };
    Json(serde_json::json!({ "tools": tools, "remote": remote })).into_response()
}

/// 添加联系人时可选带展示元数据（远程联系人——本机离线渲染用）。
#[derive(Deserialize, Default)]
struct AddBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    gateway: Option<String>,
}

async fn tools_add(
    State(st): State<Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<AddBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    // 复合 id 必须语法合法且网关在地址簿（否则聊天时才报错，太晚）
    if id.starts_with('@') {
        let Some((target, _)) = parse_remote_contact(&id) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid remote contact id（应为 @网关~分身）"})),
            )
                .into_response();
        };
        if st.tool_store.gateway_url(target).is_none() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("远程网关 {target} 不在地址簿，先添加网关")})),
            )
                .into_response();
        }
        if let Some(name) = body.name {
            st.tool_store.set_remote_meta(
                &id,
                crate::tool_store::StoredAgentMeta {
                    name,
                    description: body.description.unwrap_or_default(),
                    agent_type: body.agent_type.unwrap_or_default(),
                    gateway: body.gateway.unwrap_or_else(|| target.to_string()),
                },
            );
        }
    }
    st.tool_store.add(&id);
    info!(agent = %id, "网关工具已添加");
    StatusCode::CREATED.into_response()
}

async fn tools_remove(
    State(st): State<Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    st.tool_store.remove(&id);
    info!(agent = %id, "网关工具已移除");
    StatusCode::CREATED.into_response()
}

#[derive(Deserialize)]
struct SetSessionBody {
    sender_id: String,
    /// 点选的历史会话 id；null = 新建会话（下轮不带 --resume）
    session_id: Option<String>,
}

async fn tools_set_session(
    State(st): State<Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<SetSessionBody>,
) -> Response {
    if let Some(ref sid) = body.session_id {
        if !crate::agent_client::valid_session_id(sid) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid sessionId"})),
            )
                .into_response();
        }
    }
    let sender_id = format!("web:{}", body.sender_id);
    st.tool_store
        .set_active_session(&id, &sender_id, body.session_id.as_deref());
    Json(serde_json::json!({"status": "ok"})).into_response()
}

#[derive(Deserialize)]
struct ToolSessionsQuery {
    agent: String,
    sender: String,
}

/// 历史会话列表：透传网关台账 sessions/list（§2.4.1——网关经手的轮按
/// 注册名记账），同时下发该联系人当前续接 id（前端标"当前"）。
/// 网关不可达时 200 + 空表 + gateway_error（对话框里提示，不炸）。
/// 联系人 id 支持复合形式（远程网关台账 = 对方网关经手的轮）。
async fn tool_sessions(
    State(st): State<Arc<WebState>>,
    Query(q): Query<ToolSessionsQuery>,
) -> Response {
    let sender_id = format!("web:{}", q.sender);
    let active = st.tool_store.session(&q.agent, &sender_id).0;
    match sessions_for_contact(&st, &q.agent).await {
        Ok(sessions) => Json(serde_json::json!({
            "sessions": sessions,
            "active_session_id": active,
        }))
        .into_response(),
        Err(e) => Json(serde_json::json!({
            "sessions": [],
            "active_session_id": active,
            "gateway_error": e,
        }))
        .into_response(),
    }
}

/// 网关 RPC 短连接（listAgents / sessions/list 共用面）。
async fn gateway_connect(
) -> Result<crate::agent_client::AgentConn, String> {
    let ep = crate::agent_client::AgentEndpoint::from_gateway_config()
        .ok_or_else(|| "本机网关未配置（~/.aginx/config.toml [relay] 段缺失）".to_string())?;
    let mut conn = crate::agent_client::AgentConn::connect(&ep).await?;
    conn.initialize().await?;
    Ok(conn)
}

async fn gateway_call() -> Result<Vec<crate::agent_client::GatewayAgent>, String> {
    let mut conn = gateway_connect().await?;
    conn.list_agents().await
}

/// 联系人 id（含复合远程形式）→ 该网关台账会话列表。
async fn sessions_for_contact(
    st: &Arc<WebState>,
    id: &str,
) -> Result<Vec<crate::agent_client::GatewaySession>, String> {
    let (ep, agent) = endpoint_for_contact(st, id).await?;
    let mut conn = crate::agent_client::AgentConn::connect(&ep).await?;
    conn.initialize().await?;
    conn.sessions_list(&agent).await
}

// ── 远程网关联系人（webui 第五刀：统一 aginx 流程缺口3；第六刀加配对绑定） ──

async fn gateways_list(State(st): State<Arc<WebState>>) -> Response {
    let gateways: Vec<serde_json::Value> = st
        .tool_store
        .gateways()
        .into_iter()
        .map(|g| {
            serde_json::json!({
                "target": g.target,
                "url": g.url,
                "bound": g.bound,
                "device_name": g.device_name,
            })
        })
        .collect();
    Json(serde_json::json!({ "gateways": gateways })).into_response()
}

#[derive(Deserialize)]
struct GatewayAddBody {
    url: String,
}

/// 添加远程网关：先探活（connect + initialize），通了才进地址簿——
/// 地址簿里没有死地址。探活后的 listAgents 失败 = 网关活着但私有且
/// 未绑定 → 仍收录，`needs_bind: true`（前端弹配对码输入）。响应带
/// 该网关 agent 列表与已添加标记。
async fn gateways_add(
    State(st): State<Arc<WebState>>,
    Json(body): Json<GatewayAddBody>,
) -> Response {
    let Some(ep) = crate::agent_client::AgentEndpoint::from_url_with_local_secret(&body.url)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "地址无效，应为 agent://<id>.relay.<domain>"})),
        )
            .into_response();
    };
    let mut ep = ep;
    ep.auth_token = st.tool_store.gateway_token(&ep.target);
    let target = ep.target.clone();

    // 探活三步走：connect 失败/initialize 失败 = 连不上（502 不收录）；
    // listAgents 被拒 = 网关活着但私有且未绑定 → 收录 + needs_bind。
    let mut conn = match crate::agent_client::AgentConn::connect(&ep).await {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("连不上对方网关: {e}")})),
            )
                .into_response();
        }
    };
    if let Err(e) = conn.initialize().await {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("握手失败: {e}")})),
        )
            .into_response();
    }
    let agents = match conn.list_agents().await {
        Ok(a) => a,
        Err(denied) => {
            st.tool_store.add_gateway(&target, &body.url);
            info!(target = %target, "远程网关已收录（私有，待配对）");
            return Json(serde_json::json!({
                "target": target,
                "needs_bind": true,
                "gateway_error": denied,
                "agents": [],
            }))
            .into_response();
        }
    };
    st.tool_store.add_gateway(&target, &body.url);
    info!(target = %target, "远程网关已收录");
    let agents: Vec<serde_json::Value> = agents
        .into_iter()
        .map(|a| {
            let cid = format!("@{target}~{}", a.id);
            serde_json::json!({
                "id": a.id,
                "contact_id": cid,
                "name": a.name,
                "description": a.description,
                "agent_type": a.agent_type,
                "added": st.tool_store.is_added(&cid),
            })
        })
        .collect();
    Json(serde_json::json!({"target": target, "agents": agents})).into_response()
}

#[derive(Deserialize)]
struct GatewayBindBody {
    pair_code: String,
    #[serde(default)]
    device_name: Option<String>,
}

/// 配对绑定（第六刀）：pairCode → bindDevice → 凭证入 store。
/// 成功后立即用新 token 拉 agent 列表（私有网关绑定前看不到）。
async fn gateways_bind(
    State(st): State<Arc<WebState>>,
    AxumPath(target): AxumPath<String>,
    Json(body): Json<GatewayBindBody>,
) -> Response {
    let Some(url) = st.tool_store.gateway_url(&target) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "网关不在地址簿"})),
        )
            .into_response();
    };
    let pair_code = body.pair_code.trim().to_string();
    if pair_code.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "配对码不能为空"})),
        )
            .into_response();
    }
    let Some(ep) = crate::agent_client::AgentEndpoint::from_url_with_local_secret(&url) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "地址簿里的 URL 已失效"})),
        )
            .into_response();
    };
    // bindDevice 是未鉴权也放行的两个方法之一（ACP.md §2.2）——不带 token 直连
    let device_name = body.device_name.unwrap_or_else(|| "webui".to_string());
    let device = match async {
        let mut conn = crate::agent_client::AgentConn::connect(&ep).await?;
        conn.initialize().await?;
        conn.bind_device(&pair_code, &device_name).await
    }
    .await
    {
        Ok(d) => d,
        Err(e) => {
            // 网关侧拒（码错/过期/已绑他机）原样透传给前端
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("配对失败: {e}")})),
            )
                .into_response();
        }
    };
    st.tool_store
        .set_gateway_binding(&target, &device.token, &device.device_name);
    info!(target = %target, device = %device.device_name, "远程网关配对绑定成功");

    // 新 token 立刻生效：拉 agent 列表返回（绑定前 needs_bind 状态下是空的）
    let mut ep = ep;
    ep.auth_token = Some(device.token.clone());
    let agents: Vec<serde_json::Value> = match async {
        let mut conn = crate::agent_client::AgentConn::connect(&ep).await?;
        conn.initialize().await?;
        conn.list_agents().await
    }
    .await
    {
        Ok(list) => list
            .into_iter()
            .map(|a| {
                let cid = format!("@{target}~{}", a.id);
                serde_json::json!({
                    "id": a.id,
                    "contact_id": cid,
                    "name": a.name,
                    "description": a.description,
                    "agent_type": a.agent_type,
                    "added": st.tool_store.is_added(&cid),
                })
            })
            .collect(),
        Err(e) => {
            tracing::warn!(target = %target, error = %e, "绑定后拉 agent 列表失败");
            Vec::new()
        }
    };
    Json(serde_json::json!({
        "target": target,
        "device_name": device.device_name,
        "agents": agents,
    }))
    .into_response()
}

async fn gateways_remove(
    State(st): State<Arc<WebState>>,
    AxumPath(target): AxumPath<String>,
) -> Response {
    if st.tool_store.gateway_url(&target).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "网关不在地址簿"})),
        )
            .into_response();
    }
    st.tool_store.remove_gateway(&target);
    info!(target = %target, "远程网关已移除（联系人级联清理）");
    StatusCode::CREATED.into_response()
}

#[derive(Deserialize)]
struct RemoteAgentsQuery {
    gateway: String,
}

/// 某个已收录远程网关的 agent 列表（工具页点开网关时拉取）。
async fn remote_agents(State(st): State<Arc<WebState>>, Query(q): Query<RemoteAgentsQuery>) -> Response {
    let Some(url) = st.tool_store.gateway_url(&q.gateway) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "网关不在地址簿"})),
        )
            .into_response();
    };
    let Some(mut ep) = crate::agent_client::AgentEndpoint::from_url_with_local_secret(&url)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "地址簿里的 URL 已失效"})),
        )
            .into_response();
    };
    ep.auth_token = st.tool_store.gateway_token(&q.gateway);
    let mut conn = match crate::agent_client::AgentConn::connect(&ep).await {
        Ok(c) => c,
        Err(e) => {
            return Json(serde_json::json!({"agents": [], "gateway_error": e})).into_response();
        }
    };
    if let Err(e) = conn.initialize().await {
        return Json(serde_json::json!({"agents": [], "gateway_error": e})).into_response();
    }
    let agents = match conn.list_agents().await {
        Ok(a) => a,
        Err(denied) => {
            // 网关活着但拒绝列表 = 私有且未绑定（token 失效同此表现）
            return Json(
                serde_json::json!({"agents": [], "needs_bind": true, "gateway_error": denied}),
            )
            .into_response();
        }
    };
    let target = &q.gateway;
    let agents: Vec<serde_json::Value> = agents
        .into_iter()
        .map(|a| {
            let cid = format!("@{target}~{}", a.id);
            serde_json::json!({
                "id": a.id,
                "contact_id": cid,
                "name": a.name,
                "description": a.description,
                "agent_type": a.agent_type,
                "added": st.tool_store.is_added(&cid),
            })
        })
        .collect();
    Json(serde_json::json!({"agents": agents})).into_response()
}

/// 网关 agent 聊天：一次性 agent:// 连接，纯文本 chunk → SSE delta 帧，
/// 终帧收割 session_id 入台账（下轮 --resume 续接）。cwd 不再传——
/// 注册项绑定的 folder 就是会话锚定点（网关 spawn 落点）。联系人 id
/// 支持复合形式（远程网关分身——sessionId 续接走对方 carrier 会话）。
async fn tool_chat(st: &Arc<WebState>, tool_id: &str, body: ChatBody) -> Response {
    let (ep, agent_id) = match endpoint_for_contact(st, tool_id).await {
        Ok(pair) => pair,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": e})),
            )
                .into_response()
        }
    };
    let sender_id = format!("web:{}", body.sender_id);
    let (resume_id, _) = st.tool_store.session(tool_id, &sender_id);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    let tool_id = tool_id.to_string();
    let message = body.message.clone();
    let store = st.tool_store.clone();
    tokio::spawn(async move {
        let send = |v: serde_json::Value| {
            let _ = tx.try_send(Ok(sse_frame(v)));
        };
        let outcome = async {
            let mut conn = crate::agent_client::AgentConn::connect(&ep)
                .await
                .map_err(|e| format!("网关连接失败: {e}"))?;
            conn.initialize().await?;
            conn.prompt(&agent_id, &message, resume_id.as_deref(), |text| {
                send(serde_json::json!({"type": "delta", "text": text}));
                true
            })
            .await
        }
        .await;

        match outcome {
            Ok(result) => {
                store.append_turn(
                    &tool_id,
                    &sender_id,
                    &message,
                    &result.text,
                    result.session_id.as_deref(),
                );
                send(serde_json::json!({
                    "type": "done",
                    "response": result.text,
                    "iterations": 1,
                    "tokens": result.num_turns.unwrap_or(0),
                    "cost_usd": result.cost_usd,
                    "duration_ms": result.duration_ms,
                    "silent": false,
                }));
            }
            Err(e) => send(serde_json::json!({"type": "error", "message": e})),
        }
    });

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|ev| (ev, rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

fn kernel_self_handle(kernel: &Arc<CarrierKernel>) -> Arc<dyn carrier_runtime::kernel_handle::KernelHandle> {
    use carrier_runtime::kernel_handle::KernelHandle;
    kernel.clone() as Arc<dyn KernelHandle>
}

#[derive(Deserialize)]
struct HistoryQuery {
    agent: String,
    sender: String,
}

async fn history(State(st): State<Arc<WebState>>, Query(q): Query<HistoryQuery>) -> Response {
    // 分叉：本机分身优先；未命中且在网关工具台账 → store 读
    if st.kernel.registry.find_by_name(&q.agent).is_none() {
        if st.tool_store.is_added(&q.agent) {
            let sender_id = format!("web:{}", q.sender);
            let (_, messages) = st.tool_store.session(&q.agent, &sender_id);
            let messages = messages
                .into_iter()
                .map(|(role, text)| serde_json::json!({"role": role, "text": text}))
                .collect::<Vec<_>>();
            return Json(serde_json::json!({ "messages": messages })).into_response();
        }
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown agent"}))).into_response();
    }
    let Some(entry) = st.kernel.registry.find_by_name(&q.agent) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown agent"}))).into_response();
    };
    let sender_id = format!("web:{}", q.sender);
    let sessions = st
        .kernel
        .memory
        .list_user_sessions(&entry.name, &sender_id)
        .unwrap_or_default();
    // 最新一组 transcript，剥 system 行，Text/Blocks 归一为纯文本给前端。
    let messages = sessions
        .last()
        .map(|(_, msgs)| msgs.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|m| m.role != carrier_types::message::Role::System)
        .map(|m| {
            serde_json::json!({
                "role": m.role,
                "text": message_text(m),
            })
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "messages": messages })).into_response()
}

fn message_text(m: &carrier_types::message::Message) -> String {
    match &m.content {
        carrier_types::message::MessageContent::Text(t) => t.clone(),
        carrier_types::message::MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| {
                if let carrier_types::message::ContentBlock::Text { text, .. } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

async fn get_brain(State(st): State<Arc<WebState>>) -> Response {
    let cfg = st.kernel.brain_read().config().clone();
    Json(cfg).into_response()
}

async fn put_brain(State(st): State<Arc<WebState>>, Json(cfg): Json<carrier_types::brain::BrainConfig>) -> Response {
    match st.kernel.update_brain(|current| {
        current.base_url = cfg.base_url.clone();
        current.api_key_env = cfg.api_key_env.clone();
        current.default_modality = cfg.default_modality.clone();
        current.modalities = cfg.modalities.clone();
    }) {
        Ok(()) => Json(serde_json::json!({"status": "ok"})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct KeyBody {
    name: String,
    value: String,
}

async fn post_key(State(st): State<Arc<WebState>>, Json(body): Json<KeyBody>) -> Response {
    if !env_file::valid_env_name(&body.name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid env name"})),
        )
            .into_response();
    }
    let path = st.kernel.config.home_dir.join(".env");
    if let Err(e) = env_file::upsert_env_line(&path, &body.name, &body.value) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }
    std::env::set_var(&body.name, &body.value);
    info!(name = %body.name, "API key 已写入 .env 并生效");
    Json(serde_json::json!({"status": "ok"})).into_response()
}
