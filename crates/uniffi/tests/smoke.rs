//! 集成冒烟：UniFFI 绑定层在宿主进程内真能驱动 kernel。
//!
//! 不连 iLink（senders/ 与 DB 会话都空，通道 watcher 空转不报错），
//! 只验 Rust 侧绑定表面能 boot + list + 走通 send_message 的事件流。
//! 真机 Kotlin/Swift 壳的联调留给 Phase 7 移动形态落地时。

use std::sync::Arc;

struct RecordingListener {
    events: std::sync::Mutex<Vec<String>>,
}

impl carrier_uniffi::ChatListener for RecordingListener {
    fn on_event(&self, event: carrier_uniffi::ChatEvent) {
        let tag = match event {
            carrier_uniffi::ChatEvent::Delta { .. } => "delta",
            carrier_uniffi::ChatEvent::ToolStart { .. } => "tool_start",
            carrier_uniffi::ChatEvent::ToolDone { .. } => "tool_done",
            carrier_uniffi::ChatEvent::Phase { .. } => "phase",
            carrier_uniffi::ChatEvent::Done { .. } => "done",
            carrier_uniffi::ChatEvent::Error { .. } => "error",
        };
        self.events.lock().unwrap().push(tag.to_string());
    }
}

/// boot 只建运行时；list_clones 至少能返回（本机 dev 环境有 gaokao-advisor
/// 就 >0，干净机器 0 也是合法）。send_message 走真 turn 要 LLM key——
/// 没 key 时 listener 收到 Error 也算"绑定层通路正常"（证明事件流回灌）。
#[test]
fn uniffi_binding_boot_and_list() {
    let rt = carrier_uniffi::init_carrier().expect("boot");
    let clones = rt.list_clones();
    // 不断言数量——测试机状态不定；能返回即绑定层工作。
    println!("clones: {:?}", clones.len());

    let listener = Arc::new(RecordingListener {
        events: std::sync::Mutex::new(Vec::new()),
    });
    if let Some(first) = clones.first() {
        // 无 LLM key 环境会收 Error，有 key 会收 delta...+done；两者都合法。
        rt.send_message(first.name.clone(), "ping".to_string(), listener.clone());
        // 等待事件流收尾（Done 或 Error），限时 30s。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let evts = listener.events.lock().unwrap();
            let done = evts.iter().any(|e| e == "done" || e == "error");
            let count = evts.len();
            drop(evts);
            if done || std::time::Instant::now() > deadline {
                println!("send_message events: {count}");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(!listener.events.lock().unwrap().is_empty(), "no events");
    }
}
