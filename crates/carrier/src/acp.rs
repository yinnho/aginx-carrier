//! stdio ACP 桥 — 把分身暴露到 aginx 网关（agent:// 第一刀）。
//!
//! 网关按 `~/.aginx/agents/<clone>/aginx.toml` 拉起本命令，stdin/stdout 走
//! ndjson JSON-RPC（ACP 标准，协议全文见 aginx 仓 ACP.md）。`initialize` 只
//! 做握手零开销；`session/new` 才进程内 boot kernel（lazy，boot 失败回
//! JSON-RPC error 不崩桥）；`session/prompt` 直投 `kernel.send_message`，
//! 回包走 `agent_message_chunk` + `end_turn`。逐 token 流式与工具事件随
//! `send_message_streaming` 接入（后续阶段）。
//!
//! stdout 只允许 ACP 消息（tracing 全走 stderr）。prompt 处理并发化：
//! reader 线程喂数，prompt 每个起 tokio task，`session/cancel` 打标志位让
//! in-flight 轮尽快返回 `cancelled`。

use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use carrier_kernel::kernel::CarrierKernel;
use carrier_memory::session::SessionTicket;
use carrier_types::agent::AgentId;

/// One ACP session = one kernel agent + a cancellation flag.
struct Session {
    agent_id: AgentId,
    cancelled: Arc<AtomicBool>,
}

struct BridgeState {
    clone: String,
    /// Lazy kernel boot — `initialize` must stay free.
    kernel: Mutex<Option<Arc<CarrierKernel>>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

/// Run the bridge until stdin closes. Never returns normally on I/O death —
/// errors bubble to `main` and kill the process (the gateway respawns on
/// demand).
///
/// stdin sniffing: if the first line is an ACP JSON-RPC message, run the ACP
/// bridge; otherwise run one-shot `ask` mode (the gateway's `PromptAdapter`
/// writes a bare prompt line to stdin and expects stdout lines back). Both
/// modes share the same `aginx.toml` command entry.
pub fn run(clone: String) -> anyhow::Result<()> {
    // All logs to stderr — stdout is protocol-only.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Sniff the first stdin line to pick ACP-bridge vs one-shot-ask mode.
    //
    // NOTE: must NOT hold a `StdinLock` across the reader-thread spawn below —
    // the lock guards the process-global stdin mutex, and `run()` stays alive
    // inside `block_on` forever, so a leaked lock deadlocks the reader thread
    // (first message works via `tx.send(first)`, everything after hangs).
    // `Stdin::read_line` locks-and-releases per call and reads from the same
    // global buffer, so no line is lost.
    let mut first_raw = String::new();
    if std::io::stdin().read_line(&mut first_raw)? == 0 {
        return Ok(()); // EOF before any input
    }
    let first = first_raw.trim_end_matches(['\r', '\n']).to_string();
    let is_acp = first.trim().starts_with('{')
        && serde_json::from_str::<serde_json::Value>(first.trim())
            .map(|v| v.get("jsonrpc").is_some())
            .unwrap_or(false);

    if !is_acp {
        // One-shot ask: remaining lines join the first as the message.
        let mut msg = first;
        for l in std::io::stdin()
            .lock()
            .lines()
            .map_while(Result::ok)
        {
            msg.push('\n');
            msg.push_str(&l);
        }
        return run_ask(&clone, &msg);
    }

    let state = Arc::new(BridgeState {
        clone,
        kernel: Mutex::new(None),
        sessions: Mutex::new(HashMap::new()),
    });
    // Serialize stdout writes across concurrent prompt handlers.
    let stdout_lock = Arc::new(Mutex::new(std::io::stdout()));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tx.send(first).ok();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        while let Some(line) = rx.recv().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let msg: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    emit(
                        &stdout_lock,
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": null,
                            "error": {"code": -32700, "message": format!("Parse error: {e}")},
                        }),
                    );
                    continue;
                }
            };
            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let id = msg.get("id").cloned();
            let params = msg.get("params").cloned().unwrap_or(serde_json::json!({}));

            // Notifications (no id) we act on: session/cancel. Others dropped.
            let Some(id) = id else {
                if method == "session/cancel" {
                    let sid = params
                        .get("sessionId")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    if let Some(sess) = state.sessions.lock().unwrap().get(sid) {
                        sess.cancelled.store(true, Ordering::Relaxed);
                    }
                }
                continue;
            };

            match method {
                "initialize" => respond(
                    &stdout_lock, &id,
                    serde_json::json!({
                        "protocolVersion": 1,
                        "agentInfo": {
                            "name": state.clone,
                            "title": format!("aginx-carrier · {}", state.clone),
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "agentCapabilities": {
                            "loadSession": false,
                            "promptCapabilities": {
                                "image": false, "audio": false, "embeddedContext": true,
                            },
                        },
                        "authMethods": [],
                    }),
                ),
                "session/new" => match boot_and_register(&state) {
                    Ok(session_id) => respond(
                        &stdout_lock, &id,
                        serde_json::json!({"sessionId": session_id}),
                    ),
                    Err(e) => respond_error(&stdout_lock, &id, -32000, &e),
                },
                "session/prompt" => {
                    let st = Arc::clone(&state);
                    let out = Arc::clone(&stdout_lock);
                    tokio::spawn(async move {
                        handle_prompt(st, out, id, params).await;
                    });
                }
                "session/set_mode" => respond(&stdout_lock, &id, serde_json::Value::Null),
                _ => respond_error(&stdout_lock, &id, -32601, &format!("Method not found: {method}")),
            }
        }
    });
    Ok(())
}

/// Boot the kernel (once) and register a session for the bridge's clone.
fn boot_and_register(state: &BridgeState) -> Result<String, String> {
    let mut guard = state.kernel.lock().unwrap();
    if guard.is_none() {
        let kernel = CarrierKernel::boot(None).map_err(|e| format!("kernel boot failed: {e}"))?;
        let kernel = Arc::new(kernel);
        // 桥内 kernel 也要挂 self_handle：session/prompt 的轮次要能调跨 agent
        // 工具（clone_install/cron_create…），否则 "Kernel handle not available"
        // 连环失败（run_ask 一直有，这条路径漏了）。
        kernel.set_self_handle();
        *guard = Some(kernel);
    }
    let kernel = Arc::clone(guard.as_ref().unwrap());
    drop(guard);

    let entry = kernel
        .registry
        .find_by_name(&state.clone)
        .ok_or_else(|| format!("clone '{}' not installed on this carrier", state.clone))?;

    let session_id = uuid::Uuid::new_v4().to_string();
    state.sessions.lock().unwrap().insert(
        session_id.clone(),
        Arc::new(Session {
            agent_id: entry.id,
            cancelled: Arc::new(AtomicBool::new(false)),
        }),
    );
    Ok(session_id)
}

/// Flatten ACP prompt content blocks into the text the kernel turn consumes.
/// We declared image/audio capability false, so anything else is an error.
fn prompt_text(prompt: &serde_json::Value) -> Result<String, String> {
    let blocks = prompt
        .as_array()
        .ok_or_else(|| "prompt must be a content block array".to_string())?;
    let mut out = String::new();
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                out.push_str(b.get("text").and_then(|t| t.as_str()).unwrap_or(""));
                out.push('\n');
            }
            Some("resource") => {
                let res = b.get("resource").cloned().unwrap_or_default();
                out.push_str(&format!(
                    "[resource {}]\n{}\n",
                    res.get("uri").and_then(|u| u.as_str()).unwrap_or(""),
                    res.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                ));
            }
            Some("resource_link") => {
                out.push_str(&format!(
                    "[file {}]\n",
                    b.get("uri").and_then(|u| u.as_str()).unwrap_or(""),
                ));
            }
            other => return Err(format!("unsupported prompt block type: {other:?}")),
        }
    }
    Ok(out.trim().to_string())
}

async fn handle_prompt(
    state: Arc<BridgeState>,
    out: Arc<Mutex<std::io::Stdout>>,
    id: serde_json::Value,
    params: serde_json::Value,
) {
    let sid = params
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let Some(session) = state.sessions.lock().unwrap().get(&sid).cloned() else {
        respond_error(&out, &id, -32001, "unknown sessionId");
        return;
    };
    session.cancelled.store(false, Ordering::Relaxed);
    let text = match prompt_text(params.get("prompt").unwrap_or(&serde_json::Value::Null)) {
        Ok(t) => t,
        Err(e) => {
            respond_error(&out, &id, -32602, &e);
            return;
        }
    };

    let kernel = Arc::clone(state.kernel.lock().unwrap().as_ref().unwrap());
    let cancelled = Arc::clone(&session.cancelled);

    // 借用机制：`session/prompt` 带 `sessionTicket` → 无状态借用轮（会话票据进/
    // 出，主人服务器零持久化）。不带 ticket → 维持现有持久 session（向后兼容）。
    if let Some(ticket_val) = params.get("sessionTicket").cloned() {
        // 3.2 素材：materials[{name, contentBase64}]——仅本轮有效，轮末销毁。
        let materials = match parse_materials(params.get("materials")) {
            Ok(m) => m,
            Err(e) => {
                respond_error(&out, &id, -32002, &e);
                return;
            }
        };
        let active_flow = params
            .get("activeFlow")
            .and_then(|f| f.as_str())
            .map(|s| s.to_string());
        // 借用者身份：网关把鉴权身份透传下来做准入/配额；本地直连不带。
        let borrower = params
            .get("borrower")
            .and_then(|b| b.as_str())
            .map(|s| s.to_string());
        handle_borrowed_prompt(
            Arc::clone(&out),
            id,
            sid,
            kernel,
            session.agent_id,
            text,
            ticket_val,
            materials,
            active_flow,
            borrower,
        )
        .await;
        return;
    }

    // Each ACP session runs in its own kernel session (`acp:<id>` label —
    // session isolation refuses unlabeled turns). channel_type keeps the
    // channel-side paths happy without claiming a real channel.
    // kernel_handle 传 self——跨 agent 工具（clone_install 等）依赖它。
    let kh: Arc<dyn carrier_runtime::kernel_handle::KernelHandle> = kernel.clone();
    let turn = kernel.send_message_with_handle(
        session.agent_id,
        &text,
        Some(kh),
        Some(format!("acp:{sid}")),
        Some("aginx".to_string()),
        None,
        Some("acp".to_string()),
        None,
        None,
    );
    tokio::pin!(turn);

    let result = tokio::select! {
        r = &mut turn => r,
        // Poll the cancel flag while the turn runs.
        _ = wait_cancelled(Arc::clone(&cancelled)) => {
            Err(carrier_types::error::CarrierError::Internal("cancelled".into()).into())
        }
    };

    match result {
        Ok(r) if !r.response.trim().is_empty() => {
            update(
                &out, &sid,
                serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": r.response},
                }),
            );
            respond(&out, &id, serde_json::json!({"stopReason": "end_turn"}));
        }
        // Silent turn (agent chose not to reply) — still a clean end_turn.
        Ok(_) => respond(&out, &id, serde_json::json!({"stopReason": "end_turn"})),
        Err(e) if e.to_string().contains("cancelled") => {
            respond(&out, &id, serde_json::json!({"stopReason": "cancelled"}))
        }
        Err(e) => respond_error(&out, &id, -32002, &format!("agent turn failed: {e}")),
    }
}

/// 借用轮处理：会话票据进/出，走 `run_borrowed_turn`（无状态，主人服务器零持久化）。
#[allow(clippy::too_many_arguments)]
async fn handle_borrowed_prompt(
    out: Arc<Mutex<std::io::Stdout>>,
    id: serde_json::Value,
    sid: String,
    kernel: Arc<CarrierKernel>,
    agent_id: AgentId,
    text: String,
    ticket_val: serde_json::Value,
    materials: Vec<carrier_kernel::messaging::BorrowedMaterial>,
    active_flow: Option<String>,
    borrower: Option<String>,
) {
    let ticket: SessionTicket = match serde_json::from_value(ticket_val) {
        Ok(t) => t,
        Err(e) => {
            respond_error(&out, &id, -32602, &format!("invalid sessionTicket: {e}"));
            return;
        }
    };

    match kernel
        .run_borrowed_turn(
            agent_id,
            ticket,
            &text,
            Some(Arc::clone(&kernel) as Arc<dyn carrier_runtime::kernel_handle::KernelHandle>),
            active_flow.as_deref(),
            &materials,
            None,
            borrower.as_deref(),
        )
        .await
    {
        Ok(result) => {
            if !result.response.trim().is_empty() {
                update(
                    &out,
                    &sid,
                    serde_json::json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": result.response},
                    }),
                );
            }
            // 返回更新后的票据 + 回传产物——会话与产物的真源都在用户侧，服务器无状态。
            respond(
                &out,
                &id,
                serde_json::json!({
                    "stopReason": "end_turn",
                    "sessionTicket": result.ticket,
                    "files": result.files,
                }),
            );
        }
        Err(e) => respond_error(&out, &id, -32002, &format!("borrowed turn failed: {e}")),
    }
}

/// 解析 3.2 素材参数：`materials: [{name, contentBase64}]`。空/缺省 → 空集。
fn parse_materials(
    val: Option<&serde_json::Value>,
) -> Result<Vec<carrier_kernel::messaging::BorrowedMaterial>, String> {
    use base64::Engine as _;

    let Some(arr) = val else {
        return Ok(Vec::new());
    };
    let arr = arr.as_array().ok_or("materials must be an array")?;
    let mut out = Vec::with_capacity(arr.len());
    for m in arr {
        let name = m
            .get("name")
            .and_then(|s| s.as_str())
            .ok_or("material entry missing name")?
            .to_string();
        let b64 = m
            .get("contentBase64")
            .and_then(|s| s.as_str())
            .ok_or_else(|| format!("material {name:?} missing contentBase64"))?;
        let content = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("material {name:?} base64 decode failed: {e}"))?;
        out.push(carrier_kernel::messaging::BorrowedMaterial { name, content });
    }
    Ok(out)
}

/// A tiny future that resolves once the cancel flag is raised (5ms poll —
/// cancellation is a rare, human-timeout-scale event).
async fn wait_cancelled(flag: Arc<AtomicBool>) {
    loop {
        if flag.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// One-shot `ask` mode — the gateway `PromptAdapter` contract: prompt on
/// stdin, response lines on stdout, exit 0 on success / non-zero on failure.
///
/// Streams each `TextDelta` to stdout as it arrives so the gateway can emit
/// `chunk` notifications progressively; a trailing newline flushes the last
/// line through the adapter's line reader.
fn run_ask(clone: &str, message: &str) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let kernel = CarrierKernel::boot(None)
            .map_err(|e| anyhow::anyhow!("kernel boot failed: {e}"))?;
        let kernel = Arc::new(kernel);
        kernel.set_self_handle();
        let kh: Arc<dyn carrier_runtime::kernel_handle::KernelHandle> = kernel.clone();
        let entry = kernel
            .registry
            .find_by_name(clone)
            .ok_or_else(|| anyhow::anyhow!("clone '{clone}' not installed on this carrier"))?;
        let agent_id = entry.id;

        let (mut rx, handle) = kernel
            .send_message_streaming(
                agent_id,
                message,
                Some(kh),
                Some(format!("ask:{clone}")),
                None,
                None,
                Some("ask".to_string()),
                None,
            )
            .await
            .map_err(|e| anyhow::anyhow!("turn failed to start: {e}"))?;

        use std::io::Write as _;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        while let Some(ev) = rx.recv().await {
            if let carrier_runtime::llm_driver::StreamEvent::TextDelta { text } = ev {
                write!(out, "{text}").ok();
                out.flush().ok();
            }
        }
        match handle.await {
            Ok(Ok(result)) => {
                writeln!(out).ok();
                out.flush().ok();
                if result.response.trim().is_empty() {
                    // Silent turn — still a clean exit.
                    return Ok(());
                }
                Ok(())
            }
            Ok(Err(e)) => Err(anyhow::anyhow!("agent turn failed: {e}")),
            Err(e) => Err(anyhow::anyhow!("agent turn task panicked: {e}")),
        }
    })
}


fn emit(out: &Arc<Mutex<std::io::Stdout>>, value: serde_json::Value) {
    let mut w = out.lock().unwrap();
    let _ = writeln!(w, "{value}");
    let _ = w.flush();
}

fn respond(out: &Arc<Mutex<std::io::Stdout>>, id: &serde_json::Value, result: serde_json::Value) {
    emit(
        out,
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
}

fn respond_error(
    out: &Arc<Mutex<std::io::Stdout>>,
    id: &serde_json::Value,
    code: i64,
    message: &str,
) {
    emit(
        out,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }),
    );
}

fn update(out: &Arc<Mutex<std::io::Stdout>>, session_id: &str, update: serde_json::Value) {
    emit(
        out,
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": update},
        }),
    );
}

/// ACP.md 金样本互锁测试（协议立法层）。
///
/// ACP.md 住在隔壁 aginx 仓（`../aginx/ACP.md`，本仓 CARGO_MANIFEST_DIR 起
/// `../../../`）——协议权威与三端实现由金样本互锁，文档与实现打架即测试红。
/// 独立 clone（如 GitHub CI）没有该文件时跳过，不阻断。
#[cfg(test)]
mod golden_tests {
    use super::*;

    fn doc() -> Option<String> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../aginx/ACP.md");
        match std::fs::read_to_string(path) {
            Ok(d) => Some(d),
            Err(_) => {
                eprintln!("SKIP golden_tests: {path} 不存在（独立 clone 无 aginx 仓）");
                None
            }
        }
    }

    fn golden(doc: &str, name: &str) -> serde_json::Value {
        let marker = format!("<!-- golden: {name} -->");
        let start = doc
            .find(&marker)
            .unwrap_or_else(|| panic!("ACP.md 缺金样本标记: {name}"));
        let rest = &doc[start + marker.len()..];
        let fence = rest
            .find("```json")
            .unwrap_or_else(|| panic!("金样本 {name} 后缺 ```json 围栏"));
        let body = &rest[fence + "```json".len()..];
        let end = body
            .find("```")
            .unwrap_or_else(|| panic!("金样本 {name} 围栏未闭合"));
        serde_json::from_str(body[..end].trim())
            .unwrap_or_else(|e| panic!("金样本 {name} 不是合法 JSON: {e}"))
    }

    /// 票据 wire 形状 camelCase：入↔出 roundtrip 必须逐字节等——锁死
    /// SessionTicket 的 rename_all 与字段名（turnSummaries/contextWindowTokens）。
    #[test]
    fn ticket_roundtrip_identical() {
        let Some(doc) = doc() else { return };
        let sample = golden(&doc, "ticket_v1");
        let ticket: SessionTicket =
            serde_json::from_value(sample.clone()).expect("ticket_v1 应可解析为 SessionTicket");
        let back = serde_json::to_value(&ticket).expect("SessionTicket 应可序列化");
        assert_eq!(back, sample, "票据 wire 形状漂移：文档样本与 serde 不一致");
        assert_eq!(ticket.version, SessionTicket::CURRENT_VERSION);
    }

    /// session/prompt（内部层）的借用五件套参数名 + prompt 块形状。
    #[test]
    fn internal_session_prompt_params_shape() {
        let Some(doc) = doc() else { return };
        let v = golden(&doc, "internal_session_prompt_borrowed_request");
        let params = v.get("params").expect("params 必填");
        for key in ["sessionId", "prompt", "sessionTicket", "materials", "activeFlow", "borrower"] {
            assert!(params.get(key).is_some(), "session/prompt 缺参数 {key}");
        }
        let block = params.pointer("/prompt/0").expect("prompt[0] 必填");
        assert_eq!(block.get("type").and_then(|t| t.as_str()), Some("text"));
        let ticket_val = params.get("sessionTicket").unwrap().clone();
        serde_json::from_value::<SessionTicket>(ticket_val)
            .expect("session/prompt 内嵌票据应可解析");
    }

    /// 素材条目形状：parse_materials 期望 {name, contentBase64}（后者可 base64 解码）。
    #[test]
    fn material_entry_shape() {
        let Some(doc) = doc() else { return };
        let v = golden(&doc, "material_entry");
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["contentBase64", "name"]);
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(v.get("contentBase64").unwrap().as_str().unwrap())
            .expect("金样本 contentBase64 应可解码");
    }

    /// 产物条目形状：BorrowedOutputFile 序列化键 = {name, contentBase64}。
    #[test]
    fn output_file_serialization_keys() {
        let Some(doc) = doc() else { return };
        let sample = golden(&doc, "output_file_entry");
        let f = carrier_kernel::messaging::BorrowedOutputFile {
            name: sample.get("name").unwrap().as_str().unwrap().to_string(),
            content_base64: sample.get("contentBase64").unwrap().as_str().unwrap().to_string(),
        };
        let back = serde_json::to_value(&f).unwrap();
        let mut keys: Vec<&str> = back.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["contentBase64", "name"]);
    }

    /// 桥最终响应（内部层）：stopReason 用 ACP 词汇 end_turn + 票据/产物在场。
    #[test]
    fn internal_final_result_shape() {
        let Some(doc) = doc() else { return };
        let v = golden(&doc, "internal_final_result_borrowed");
        assert_eq!(
            v.pointer("/result/stopReason").and_then(|s| s.as_str()),
            Some("end_turn")
        );
        serde_json::from_value::<SessionTicket>(
            v.pointer("/result/sessionTicket").unwrap().clone(),
        )
        .expect("最终响应内嵌票据应可解析");
        let file = v.pointer("/result/files/0").expect("files[0] 必填");
        assert!(file.get("name").is_some() && file.get("contentBase64").is_some());
    }

    /// session/update 通知形状：桥只发 agent_message_chunk。
    #[test]
    fn session_update_notification_shape() {
        let Some(doc) = doc() else { return };
        let v = golden(&doc, "internal_session_update_notification");
        assert_eq!(
            v.get("method").and_then(|m| m.as_str()),
            Some("session/update")
        );
        assert_eq!(
            v.pointer("/params/update/sessionUpdate").and_then(|s| s.as_str()),
            Some("agent_message_chunk")
        );
        assert!(v.pointer("/params/update/content/text")
            .and_then(|t| t.as_str())
            .is_some());
    }
}
