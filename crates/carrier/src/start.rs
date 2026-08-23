//! `aginx-carrier start` — 服务器/守护形态（个人部署）。
//!
//! kernel + iLink 通道 + cron/heartbeat 后台，进程常驻。组装接线与桌面
//! 形态共用 `aginx_carrier::wiring`；HTTP API（AGENT-APP-API 对外契约）
//! 是后续增量，这里先保通道在线。

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
    let kernel = aginx_carrier::wiring::boot_kernel()?;

    // ── 系统分身：clone-creator（克隆大师）未注册则用内嵌定义层装上。
    // 分身只经它生成，不手工摆文件。──
    aginx_carrier::wiring::seed_system_creator(&kernel).await;

    // ── aginx 入网同步：workspace 里已装但 ~/.aginx/agents/ 缺登记的分身
    // 补写 aginx.toml（clone_install 是增量钩子，这里是启动对账）。──
    aginx_carrier::wiring::sync_aginx_registrations(&kernel);

    let cm = aginx_carrier::wiring::boot_channels(&kernel).await?;

    let tool_count = cm.tool_definitions().len();
    info!(
        tools = tool_count,
        "aginx-carrier 守护进程就绪（iLink 通道在线，Ctrl-C 退出）"
    );

    tokio::signal::ctrl_c().await?;
    info!("收到退出信号，bye");
    Ok(())
}
