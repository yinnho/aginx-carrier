//! `aginx-carrier start` — 分身守护进程（个人部署形态）。
//!
//! boot kernel + ChannelManager（iLink）+ cron/heartbeat 后台，进程常驻。
//! 只复刻 opencarrier api server 里**通道相关**的接线（server.rs 的
//! run_daemon 通道段）：sender router、cron 投递/通知存储、iLink watcher、
//! 微信工具注册、weixin_sessions DB 持久化回调、出站 send/deliver/probe
//! 注入。HTTP API 是 Phase 7 服务器形态的事，这里只保通道在线。

use std::sync::Arc;

use carrier_kernel::kernel::CarrierKernel;
use carrier_runtime::kernel_handle::KernelHandle;
use tracing::info;

/// Boot the kernel and run channel services until Ctrl-C.
pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let kernel = CarrierKernel::boot(None)?;
    let kernel = Arc::new(kernel);
    kernel.set_self_handle();
    kernel.start_background_agents();

    // ── iLink 通道接线（opencarrier run_daemon 通道段的 iLink 子集）──
    let kh: Arc<dyn KernelHandle> = kernel.clone();
    let mut cm = carrier_runtime::channel_manager::ChannelManager::new(kh);

    // Sender-based routing: senders/<sender_id>/config.json + 新 sender 自动
    // 归给第一个 agent。UUID 老路由迁到 agent 名。
    {
        let router = Arc::new(carrier_runtime::plugin::router::SenderRouter::new(
            &kernel.config.home_dir,
        ));
        router.migrate_uuid_to_names(|uuid| {
            uuid.parse::<carrier_types::agent::AgentId>()
                .ok()
                .and_then(|id| kernel.registry.get(id).map(|e| e.manifest.name.clone()))
        });
        cm.set_sender_router(router);
        info!("Sender-based routing enabled");
    }

    // Cron 投递存储（last-channel 追踪）+ 通知路由存储。
    {
        let store = Arc::new(kernel.memory.cron_delivery().clone());
        cm.set_cron_delivery(store);
    }
    {
        let store = Arc::new(kernel.memory.notify_store().clone());
        cm.set_notify_store(store);
    }

    cm.register("weixin", Box::new(carrier_ilink::SessionWatcher::new()));

    // 微信 iLink 工具（扫码登录/发消息/发图/发视频/状态）进工具分发器。
    {
        let dispatcher = cm.tool_dispatcher();
        let mut builtin = carrier_runtime::plugin::BuiltinPlugin::new(
            "weixin".to_string(),
            "1.0.0".to_string(),
            std::path::PathBuf::new(),
        );
        builtin.register_tool(Box::new(carrier_ilink::WeixinQrLoginTool));
        builtin.register_tool(Box::new(carrier_ilink::WeixinSendMessageTool));
        builtin.register_tool(Box::new(carrier_ilink::WeixinSendImageTool));
        builtin.register_tool(Box::new(carrier_ilink::WeixinSendVideoTool));
        builtin.register_tool(Box::new(carrier_ilink::WeixinStatusTool));
        dispatcher.register(Arc::new(builtin));
    }

    // weixin_sessions DB 持久化回调——必须在 cm.start() **之前**装：iLink
    // watcher 的 start() 走 load_from_dir，回调没装就永远读 JSON 旁路、
    // 无视 DB 表（opencarrier 踩过：装晚了的 boot 全走 JSON）。
    {
        let store = kernel.memory.weixin_store().clone();
        let persist_fn: carrier_ilink::token::SessionPersistFn = Arc::new(move |tf| {
            let row = carrier_memory::weixin_store::WeixinSessionRow {
                channel: tf.channel.clone(),
                sender_key: tf.sender_key.clone(),
                bot_id: tf.bot_id.clone(),
                bot_token: tf.bot_token.clone(),
                baseurl: tf.baseurl.clone(),
                ilink_bot_id: tf.ilink_bot_id.clone(),
                user_id: tf.user_id.clone(),
                expires_at: tf.expires_at,
                bind_agent: tf.bind_agent.clone(),
                context_tokens: serde_json::to_string(&tf.context_tokens).unwrap_or_default(),
            };
            if let Err(e) = store.upsert(&row) {
                tracing::warn!("Failed to persist weixin session to DB: {e}");
            }
        });
        let store2 = kernel.memory.weixin_store().clone();
        let load_fn: carrier_ilink::token::SessionsLoadFn = Arc::new(move || match store2.load_all()
        {
            Ok(rows) => rows
                .into_iter()
                .map(|r| {
                    let ctx: std::collections::HashMap<String, String> =
                        serde_json::from_str(&r.context_tokens).unwrap_or_default();
                    carrier_ilink::models::BotTokenFile {
                        channel: r.channel,
                        sender_key: r.sender_key,
                        bot_id: r.bot_id,
                        bot_token: r.bot_token,
                        baseurl: r.baseurl,
                        ilink_bot_id: r.ilink_bot_id,
                        user_id: r.user_id,
                        expires_at: r.expires_at,
                        bind_agent: r.bind_agent,
                        context_tokens: ctx,
                    }
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!("Failed to load weixin sessions from DB: {e}");
                Vec::new()
            }
        });
        carrier_ilink::token::WEIXIN_STATE.set_persist_fns(persist_fn, load_fn);
        info!("WeixinState DB persistence callbacks installed");
    }

    cm.start().await;

    // 出站通道：send（cron 主动推送探针）+ deliver���富媒体投递）注入 kernel。
    {
        let send_fn = cm.make_channel_send_fn();
        *kernel.channel_send_fn.write().unwrap() = Some(send_fn);
        let deliver_fn = cm.make_channel_deliver_fn();
        *kernel.channel_deliver_fn.write().unwrap() = Some(deliver_fn);
        let probe = cm.make_supports_proactive_fn();
        *kernel.channel_supports_proactive_fn.write().unwrap() = Some(probe);
    }
    // 工具分发器注入 kernel（agent 工具调用走通道工具）。
    {
        let dispatcher = cm.tool_dispatcher();
        let mut guard = kernel
            .plugins
            .plugin_tool_dispatcher
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(dispatcher);
    }

    // weixin-sessions/*.json 的 bind_agent → sender 路由（扫码登录后的绑定）。
    register_token_file_bindings(&kernel, &cm);

    let tool_count = cm.tool_definitions().len();
    info!(
        tools = tool_count,
        "aginx-carrier 守护进程就绪（iLink 通道在线，Ctrl-C 退出）"
    );

    tokio::signal::ctrl_c().await?;
    info!("收到退出信号，bye");
    Ok(())
}

/// Read `weixin-sessions/*.json` token files and register
/// user_id/bot_id → bind_agent sender routes for inbound routing.
fn register_token_file_bindings(
    kernel: &Arc<CarrierKernel>,
    cm: &carrier_runtime::channel_manager::ChannelManager,
) {
    let token_dir = kernel.config.home_dir.join("weixin-sessions");
    let Ok(entries) = std::fs::read_dir(&token_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(tf) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let (Some(bot_id), Some(agent)) = (
            tf.get("bot_id").and_then(|v| v.as_str()),
            tf.get("bind_agent").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        if agent.is_empty() {
            continue;
        }
        // Resolve a UUID bind_agent to an agent name for consistency.
        let agent_ref = if let Ok(id) = agent.parse::<carrier_types::agent::AgentId>() {
            kernel
                .registry
                .get(id)
                .map(|e| e.manifest.name.clone())
                .unwrap_or_else(|| agent.to_string())
        } else {
            agent.to_string()
        };
        if let Some(uid) = tf.get("user_id").and_then(|v| v.as_str()) {
            if !uid.is_empty() && cm.get_sender_route(uid).is_none() {
                cm.set_sender_route(uid, &agent_ref);
                info!(
                    bot = bot_id,
                    agent = %agent_ref,
                    "Registered WeChat binding from token file"
                );
            }
        }
    }
}
