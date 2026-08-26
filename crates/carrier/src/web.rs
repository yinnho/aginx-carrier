//! `aginx-carrier web` — 桌面形态（ARCHITECTURE §11.3.1 Web-first）。
//!
//! 与 start 守护形态同一套接线（kernel + 通道全量），外加 carrier 自带
//! 的 Web UI HTTP 面。首启兜底：无 brain.json 时预写骨架（boot 对缺失
//! brain 是硬失败），用户随后在设置页填 key。

use tracing::info;

pub fn run(listen: String) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main(listen))
}

async fn async_main(listen: String) -> anyhow::Result<()> {
    seed_brain_skeleton_if_missing();

    let kernel = aginx_carrier::wiring::boot_kernel()?;
    aginx_carrier::wiring::seed_system_creator(&kernel).await;
    aginx_carrier::wiring::seed_system_me(&kernel).await;
    aginx_carrier::wiring::sync_aginx_registrations(&kernel);
    let _cm = aginx_carrier::wiring::boot_channels(&kernel).await?;

    info!(%listen, "桌面形态启动：浏览器打开 http://{listen}");
    carrier_webui::serve(kernel, listen).await;
    Ok(())
}

/// 首启兜底：`~/.aginx/carrier/brain.json` 不存在则写骨架——boot 硬要求
/// brain 可加载；base_url 为空时设置页横幅引导先配置。
fn seed_brain_skeleton_if_missing() {
    let path = carrier_types::config::home_dir().join("brain.json");
    if path.exists() {
        return;
    }
    let skeleton = serde_json::json!({
        "base_url": "",
        "api_key_env": "AGINXBRAIN_API_KEY",
        "default_modality": "chat",
        "modalities": { "chat": { "description": "默认对话" } }
    });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, serde_json::to_string_pretty(&skeleton).unwrap_or_default()) {
        Ok(()) => info!(path = %path.display(), "已写入 brain.json 骨架（设置页待配置）"),
        Err(e) => {
            // boot 会再报一次更具体的错，这里只提示来源
            eprintln!("brain.json 骨架写入失败（{e}）；若已有配置可忽略");
        }
    }
}
