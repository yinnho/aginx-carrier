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
    kernel: Arc<CarrierKernel>,
    listen_host: String,
}

/// 起 Web UI 监听。bind 失败只报错不 panic。
pub async fn serve(kernel: Arc<CarrierKernel>, listen: String) {
    let state = Arc::new(WebState {
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
    let Some(entry) = st.kernel.registry.find_by_name(&agent) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown agent"}))).into_response();
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
