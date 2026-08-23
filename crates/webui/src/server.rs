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
        // 目录选择器（第三刀补）：home 门内只读目录浏览
        .route("/api/fs/browse", get(fs_browse))
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
    /// 网关 agent（第三刀）可选工作目录；分身聊天忽略
    cwd: Option<String>,
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

/// 网关 agent 现列：listAgents 透传 + 台账标记。网关/relay 不可达时
/// 返回空列表 + gateway_error（前端显示网关状态条，联系人不受影响）。
async fn tools_list(State(st): State<Arc<WebState>>) -> Response {
    let default_cwd_for = |id: &str| {
        st.kernel
            .config
            .home_dir
            .join("external")
            .join(id)
            .to_string_lossy()
            .to_string()
    };
    let tools = match gateway_list_agents().await {
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
                    "default_cwd": default_cwd_for(&a.id),
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
                .map(|id| {
                    serde_json::json!({
                        "id": id,
                        "name": id,
                        "description": "",
                        "agent_type": "",
                        "kind": "gateway",
                        "added": true,
                        "default_cwd": default_cwd_for(&id),
                    })
                })
                .collect::<Vec<_>>();
            return Json(serde_json::json!({
                "tools": stale,
                "gateway_error": e,
            }))
            .into_response();
        }
    };
    Json(serde_json::json!({ "tools": tools })).into_response()
}

async fn tools_add(
    State(st): State<Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
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

/// 目录选择器数据面：只列 home 内的子目录（只读、不含文件、滤 dotfile）。
/// canonicalize 防穿越；不存在/越界一律回落 home——与网关 cwd 校验同款门。
async fn fs_browse(Query(q): Query<FsBrowseQuery>) -> Response {
    let Some(home) = dirs::home_dir() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "no home dir"})),
        )
            .into_response();
    };
    let requested = q
        .path
        .map(expand_tilde)
        .unwrap_or_else(|| home.to_string_lossy().to_string());
    let dir = match std::fs::canonicalize(&requested) {
        Ok(p) if p.starts_with(&home) && p.is_dir() => p,
        _ => home.clone(),
    };
    let mut entries: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    let parent = dir.parent().filter(|p| p.starts_with(&home)).map(|p| {
        p.to_string_lossy()
            .trim_end_matches('/')
            .to_string()
    });
    Json(serde_json::json!({
        "path": dir.to_string_lossy(),
        "parent": parent,
        "entries": entries,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct FsBrowseQuery {
    path: Option<String>,
}

async fn gateway_list_agents(
) -> Result<Vec<crate::agent_client::GatewayAgent>, String> {
    let ep = crate::agent_client::AgentEndpoint::from_gateway_config()
        .ok_or_else(|| "本机网关未配置（~/.aginx/config.toml [relay] 段缺失）".to_string())?;
    let mut conn = crate::agent_client::AgentConn::connect(&ep).await?;
    conn.initialize().await?;
    conn.list_agents().await
}

/// `~/x` → `<home>/x`；其余原样（网关侧还有 canonicalize+home 门兜底）。
fn expand_tilde(p: String) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    p
}

/// 网关 agent 聊天：一次性 agent:// 连接，chunk → parse → SSE 帧，
/// result 行收割 session_id 入台账（下轮 --resume 续接）。
async fn tool_chat(st: &Arc<WebState>, tool_id: &str, body: ChatBody) -> Response {
    let Some(ep) = crate::agent_client::AgentEndpoint::from_gateway_config() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "本机网关未配置（~/.aginx/config.toml [relay] 段缺失）"})),
        )
            .into_response();
    };
    // cwd：显式传入优先（~ 展开），默认 ~/.aginx/carrier/external/<tool_id>
    // （自动创建）。最终校验交给网关（canonicalize + home 门）。
    let default_cwd = st.kernel.config.home_dir.join("external").join(tool_id);
    let cwd = body
        .cwd
        .clone()
        .map(expand_tilde)
        .unwrap_or_else(|| default_cwd.to_string_lossy().to_string());
    if let Some(parent_err) = std::fs::create_dir_all(&default_cwd).err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("工作目录创建失败: {parent_err}")})),
        )
            .into_response();
    }
    let sender_id = format!("web:{}", body.sender_id);
    let (resume_id, _) = st.tool_store.session(tool_id, &sender_id, &cwd);

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
            conn.prompt(
                &tool_id,
                &message,
                Some(&cwd),
                resume_id.as_deref(),
                |item| match item {
                    crate::agent_client::StreamItem::Delta(t) => {
                        send(serde_json::json!({"type": "delta", "text": t}));
                        true
                    }
                    crate::agent_client::StreamItem::Thinking(t) => {
                        send(serde_json::json!({"type": "thinking", "text": t}));
                        true
                    }
                    crate::agent_client::StreamItem::Tool(name) => {
                        send(serde_json::json!({"type": "tool", "name": name, "preview": "", "is_error": false}));
                        true
                    }
                    _ => true,
                },
            )
            .await
        }
        .await;

        match outcome {
            Ok(result) => {
                store.append_turn(
                    &tool_id,
                    &sender_id,
                    &cwd,
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
    /// 网关 agent（第三刀）会话目录（换目录=新会话）
    cwd: Option<String>,
}

async fn history(State(st): State<Arc<WebState>>, Query(q): Query<HistoryQuery>) -> Response {
    // 分叉：本机分身优先；未命中且在网关工具台账 → store 读
    if st.kernel.registry.find_by_name(&q.agent).is_none() {
        if st.tool_store.is_added(&q.agent) {
            let sender_id = format!("web:{}", q.sender);
            let cwd = q.cwd.unwrap_or_else(|| {
                st.kernel
                    .config
                    .home_dir
                    .join("external")
                    .join(&q.agent)
                    .to_string_lossy()
                    .to_string()
            });
            let (_, messages) = st.tool_store.session(&q.agent, &sender_id, &cwd);
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
