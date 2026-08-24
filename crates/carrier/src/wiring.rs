//! 组装接线：kernel boot + iLink 通道。
//!
//! 从 opencarrier `api::server::run_daemon` 的通道段抽取的 iLink 子集，
//! `aginx-carrier start`（守护形态）与桌面形态（crates/desktop 内嵌运行时）
//! 共用——同一套身体，不同的宿主。

use std::sync::Arc;

use carrier_kernel::kernel::CarrierKernel;
use carrier_runtime::channel_manager::ChannelManager;
use carrier_runtime::kernel_handle::KernelHandle;
use tracing::info;

/// Boot the kernel: disk restore + self handle + 后台 agent 循环/心跳/cron。
pub fn boot_kernel() -> anyhow::Result<Arc<CarrierKernel>> {
    let kernel = CarrierKernel::boot(None)?;
    let kernel = Arc::new(kernel);
    kernel.set_self_handle();
    kernel.start_background_agents();
    Ok(kernel)
}

/// Wire up the iLink channel（opencarrier run_daemon 通道段的 iLink 子集）：
/// sender router、cron 投递/通知存储、iLink watcher、微信工具、
/// weixin_sessions DB 持久化回调、出站 send/deliver/probe 与工具分发器注入。
///
/// 调用方负责 `cm.start().await`（start 守护形态与桌面形态时机一致，
/// 但保持显式以便宿主插自己的启动钩子）。
pub async fn boot_channels(kernel: &Arc<CarrierKernel>) -> anyhow::Result<ChannelManager> {
    let kh: Arc<dyn KernelHandle> = kernel.clone();
    let mut cm = ChannelManager::new(kh);

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
        let load_fn: carrier_ilink::token::SessionsLoadFn =
            Arc::new(move || match store2.load_all() {
                Ok(rows) => rows
                    .into_iter()
                    .map(weixin_row_to_token_file)
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

    // webhook 入站通道：出站侧注册（异步轮回复的日志归宿，防 bridge 报
    // Channel-not-found）+ 路由种入。HTTP 监听在 start.rs（daemon 形态专属，
    // 移动端不起监听）。send_fn 捕获同一 channels map，start 后注册对出站
    // 查表可见；WebhookChannel::start 是 noop，不被 start() 调到也成立。
    if kernel.config.webhook.enabled {
        cm.register("webhook", Box::new(carrier_webhook::WebhookChannel));
        for hook in &kernel.config.webhook.hooks {
            cm.set_sender_route(&hook.name, &hook.agent);
            info!(hook = %hook.name, agent = %hook.agent, "webhook route seeded");
        }
    }

    // 出站通道：send（cron 主动推送探针）+ deliver（富媒体投递）注入 kernel。
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
    register_token_file_bindings(kernel, &cm);

    Ok(cm)
}

/// Boot-time aginx 入网对账：kernel 里已装但 `~/.aginx/agents/` 缺登记的
/// 分身补写 aginx.toml。clone_install 是增量钩子；这里是启动兜底，覆盖
/// 手工导入/拷贝 workspace、aginx.toml 丢失等情况。
///
/// 已存在的登记**不覆盖**——保留手工编辑；只有缺失才补。
pub fn sync_aginx_registrations(kernel: &Arc<CarrierKernel>) {
    for entry in kernel.registry.list() {
        if carrier_kernel::aginx_net::registration_exists_default(&entry.name) {
            continue;
        }
        match carrier_kernel::aginx_net::register_clone_default(
            &entry.name,
            &entry.manifest.display_name,
            &entry.manifest.description,
            &entry.manifest.version,
        ) {
            Ok(()) => tracing::info!(agent = %entry.name, "aginx registration reconciled"),
            Err(e) => tracing::warn!(agent = %entry.name, error = %e, "aginx registration failed"),
        }
    }
}

/// 系统分身种子：未注册 clone-creator（克隆大师）时，用内嵌定义层走正规
/// 安装管线装上。此后所有分身由它生成——分身不手工摆文件。
///
/// 已注册即跳过（升级定义层走 dup 管线或 REINSTALL，boot 不覆盖）。
/// 失败只告警不挡启动：裸系统（无克隆大师）仍可跑，修复后重启补种。
pub async fn seed_system_creator(kernel: &Arc<CarrierKernel>) {
    if kernel
        .registry
        .find_by_name(carrier_clone::system_creator::SYSTEM_CREATOR_NAME)
        .is_some()
    {
        return;
    }
    let files = carrier_clone::system_creator::system_creator_files();
    match kernel
        .clone_install_files(carrier_clone::system_creator::SYSTEM_CREATOR_NAME, files)
        .await
    {
        Ok((id, name, display_name)) => {
            tracing::info!(id = %id, name = %name, display_name = %display_name, "系统分身已种子：clone-creator");
        }
        Err(e) => {
            tracing::warn!(error = %e, "clone-creator 种子失败（不影响启动，重启重试）");
        }
    }
}

/// WeixinSessionRow（DB 行）→ BotTokenFile（通道会话形状）。
/// DB 加载回调与 `aginx-carrier notify` 一次性进程共用——两份手抄必漂移。
pub fn weixin_row_to_token_file(
    r: carrier_memory::weixin_store::WeixinSessionRow,
) -> carrier_ilink::models::BotTokenFile {
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
}

/// Read weixin token files and register user_id → bind_agent sender routes
/// for inbound routing.
///
/// 会话真源是 `senders/<uid>/session.json`（save_session 的落点）；
/// `weixin-sessions/` 是 opencarrier 老目录——保留兼容一拍（有旧文件
/// 仍生效），新文件只认 senders/。曾长期只扫老目录：扫码绑定的
/// bind_agent 路由从未注册，新扫码用户消息一律 "No agent resolved" 丢弃。
fn register_token_file_bindings(kernel: &Arc<CarrierKernel>, cm: &ChannelManager) {
    let home = kernel.config.home_dir.clone();
    let mut token_files: Vec<std::path::PathBuf> = Vec::new();
    // 新位置：senders/<uid>/session.json
    if let Ok(sender_entries) = std::fs::read_dir(home.join("senders")) {
        for entry in sender_entries.flatten() {
            let session = entry.path().join("session.json");
            if session.is_file() {
                token_files.push(session);
            }
        }
    }
    // 老位置：weixin-sessions/*.json（兼容）
    if let Ok(entries) = std::fs::read_dir(home.join("weixin-sessions")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                token_files.push(path);
            }
        }
    }
    for path in token_files {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(tf) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        // 新文件恒有 channel:"weixin"；老文件可能缺字段——缺省放行（老目录
        // 本来就只有 weixin 会话），明确非 weixin 的才拒。
        if tf.get("channel")
            .and_then(|v| v.as_str())
            .is_some_and(|c| c != "weixin")
        {
            continue;
        }
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
