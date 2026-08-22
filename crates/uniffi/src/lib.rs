//! aginx-carrier 移动形态：UniFFI 绑定层。
//!
//! 手机 App = aginxium（连别人的分身）+ 内嵌 aginx-carrier 运行时（养自己的
//! 分身）。本 crate 把 kernel/通道接线暴露给 Kotlin/Swift：`init_carrier()`
//! 拿到 `CarrierRuntime`，`list_clones()` 枚举分身，`send_message(clone,
//! text, listener)` 把 agent 轮的流式事件回灌 UI。

use std::sync::Arc;

use carrier_kernel::kernel::CarrierKernel;
use carrier_runtime::kernel_handle::KernelHandle;
use carrier_runtime::llm_driver::StreamEvent;

uniffi::setup_scaffolding!();

/// FFI 出向事件：StreamEvent 的扁平投影（UI 只需要渲染要的部分）。
#[derive(uniffi::Enum)]
pub enum ChatEvent {
    Delta { text: String },
    ToolStart { name: String },
    ToolDone {
        name: String,
        preview: String,
        is_error: bool,
    },
    Phase { phase: String, detail: Option<String> },
    Done {
        response: String,
        input_tokens: u64,
        output_tokens: u64,
    },
    Error { message: String },
}

/// Kotlin/Swift 侧的流式回调。
#[uniffi::export(with_foreign)]
pub trait ChatListener: Send + Sync {
    fn on_event(&self, event: ChatEvent);
}

#[derive(uniffi::Record)]
pub struct CloneInfo {
    pub name: String,
    pub display_name: String,
    pub description: String,
}

/// 进程内分身运行时。一次 init，常驻到 App 退出。
#[derive(uniffi::Object)]
pub struct CarrierRuntime {
    kernel: Arc<CarrierKernel>,
    runtime: tokio::runtime::Runtime,
}

impl CarrierRuntime {
    fn new_inner() -> anyhow::Result<Self> {
        // Android/iOS 宿主各自初始化 tracing；重复 init 无害（try_init 忽略错误）。
        let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).try_init();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let kernel = runtime.block_on(async {
            let kernel = aginx_carrier::wiring::boot_kernel()?;
            let _cm = aginx_carrier::wiring::boot_channels(&kernel).await?;
            // ChannelManager 由 kernel 内部 spawn 的 poll 线程自维持；
            // 这里只需保 kernel 引用，cm 析构不停通道（threads detached）。
            Ok::<Arc<CarrierKernel>, anyhow::Error>(kernel)
        })?;
        Ok(Self { kernel, runtime })
    }

    /// 列出已安装分身（kernel registry）。
    pub fn list_clones(&self) -> Vec<CloneInfo> {
        self.kernel
            .registry
            .list()
            .into_iter()
            .map(|e| CloneInfo {
                name: e.name,
                display_name: e.manifest.display_name,
                description: e.manifest.description,
            })
            .collect()
    }

    /// 发消息给分身，流式事件经 listener 回吐。同步签名——UniFFI 0.28
    /// 的 async 支持在 foreign callback 场景受限，这里用内部 runtime 驱
    /// 动异步，事件经 Arc<dyn ChatListener> 立即转发（跨线程安全）。
    pub fn send_message(&self, clone_name: String, text: String, listener: Arc<dyn ChatListener>) {
        let kernel = Arc::clone(&self.kernel);
        let Some(entry) = kernel.registry.find_by_name(&clone_name) else {
            listener.on_event(ChatEvent::Error {
                message: format!("clone not installed: {clone_name}"),
            });
            return;
        };
        let agent_id = entry.id;
        let kh: Arc<dyn KernelHandle> = kernel.clone();
        // 与 acp 桥同款会话标签：桌面/App 侧消息走 `app:` 前缀，隔离于
        // channel（acp:/weixin:）与 task:。
        let sender_id = format!("app:{clone_name}");

        self.runtime.spawn(async move {
            let result = kernel
                .send_message_streaming(
                    agent_id,
                    &text,
                    Some(kh),
                    Some(sender_id),
                    None,
                    None,
                    Some("app".to_string()),
                    None,
                )
                .await;

            let (mut rx, handle) = match result {
                Ok(pair) => pair,
                Err(e) => {
                    listener.on_event(ChatEvent::Error {
                        message: format!("send failed: {e}"),
                    });
                    return;
                }
            };

            while let Some(ev) = rx.recv().await {
                let out = match ev {
                    StreamEvent::TextDelta { text } => ChatEvent::Delta { text },
                    StreamEvent::ToolUseStart { name, .. } => ChatEvent::ToolStart { name },
                    StreamEvent::ToolUseEnd { name, .. } => ChatEvent::ToolDone {
                        name,
                        preview: String::new(),
                        is_error: false,
                    },
                    StreamEvent::ToolExecutionResult {
                        name,
                        result_preview,
                        is_error,
                        ..
                    } => ChatEvent::ToolDone {
                        name,
                        preview: result_preview,
                        is_error,
                    },
                    StreamEvent::ThinkingDelta { .. } | StreamEvent::ToolInputDelta { .. } => continue, // UI 可后补 thinking 渲染
                    StreamEvent::PhaseChange { phase, detail } => ChatEvent::Phase { phase, detail },
                    StreamEvent::ContentComplete { .. } => continue, // Done 由 JoinHandle 的 AgentLoopResult 出
                };
                listener.on_event(out);
            }

            match handle.await {
                Ok(Ok(result)) => {
                    listener.on_event(ChatEvent::Done {
                        response: result.response,
                        input_tokens: result.total_usage.input_tokens,
                        output_tokens: result.total_usage.output_tokens,
                    });
                }
                Ok(Err(e)) => {
                    listener.on_event(ChatEvent::Error {
                        message: format!("turn failed: {e}"),
                    });
                }
                Err(e) => {
                    listener.on_event(ChatEvent::Error {
                        message: format!("turn task panicked: {e}"),
                    });
                }
            }
        });
    }

    /// 丢弃 kernel：后台 agent 循环/心跳/cron/通道 watcher 随之停止
    /// （它们持有 kernel 的弱引用或独立 task，runtime drop 时 tokio 强停）。
    pub fn shutdown(self: Arc<Self>) {
        // Arc 消费即释放；无显式 teardown 钩子。
    }
}

#[uniffi::export]
pub fn init_carrier() -> Result<Arc<CarrierRuntime>, InitError> {
    CarrierRuntime::new_inner()
        .map(Arc::new)
        .map_err(|e: anyhow::Error| InitError::Boot(e.to_string()))
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum InitError {
    #[error("kernel boot failed: {0}")]
    Boot(String),
}
