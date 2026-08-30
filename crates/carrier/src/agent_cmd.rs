//! `aginx-carrier agent` — 化身管理 CLI 面（CARRIER.md §3.3 下载类形态���。
//!
//! AginxOS 融合后的唯一化身管理入口（webui 已随 `web` 子命令退役）：
//! aterm 启动器与脚本经此 install/list/remove/update。安装/更新本体复用
//! kernel 的 `clone_install_files` 正规管线（格式校验→落盘→spawn→入网
//! 钩子），更新语义 = 定义版本 diff（DupHub latest_version 对比本地
//! template.json version，落后才重装，`.dup/` 历史保留）。

use carrier_kernel::kernel::CarrierKernel;

use crate::AgentAction;

pub fn run(action: AgentAction) -> anyhow::Result<()> {
    // 一次性进程：内嵌 runtime 跑完即退（同 acp 每.prompt 一个进程的形态）。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async(action))
}

async fn run_async(action: AgentAction) -> anyhow::Result<()> {
    // 远程句柄是纯注册表文件操作：不 boot kernel（装卸竞态教训）也
    // 不需要 brain。
    if let AgentAction::Remote { action } = action {
        return crate::remote::run_remote(action);
    }
    // kernel boot 对缺失 brain 是硬失败——CLI 面兜底写骨架（安装/列表
    // 本身不需要 brain，不该被它挡住）。
    aginx_carrier::wiring::seed_brain_skeleton_if_missing();
    // 裸 boot、不起后台 agent 循环/心跳（acp 同款先例）：一次性进程的
    // 后台写手会在 remove_dir_all 遍历期间往 workspace 落日志，实测把
    // 卸载打成 "Directory not empty"。装/卸/查都不需要常驻循环。
    let kernel = carrier_kernel::kernel::CarrierKernel::boot(None)?;
    match action {
        AgentAction::Install { name } => install(&kernel, &name).await,
        AgentAction::List => {
            list(&kernel);
            Ok(())
        }
        AgentAction::Remove { name } => remove(&kernel, &name),
        AgentAction::Update { name } => update(&kernel, &name).await,
        AgentAction::Remote { .. } => unreachable!("Remote 已在 boot 前分派"),
    }
}

/// 读 Hub API key（环境变量 → ~/.aginx/carrier/.env 兜底，hub::read_api_key）。
fn read_key(api_key_env: &str) -> anyhow::Result<String> {
    carrier_clone::hub::read_api_key(api_key_env)
        .ok()
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("未配置 Hub API key（环境变量 {api_key_env}）"))
}

fn daemon_restart_hint() {
    println!("提示：若 `aginx-carrier start` 守护进程正在运行，重启后它才能看到变化。");
}

async fn install(kernel: &CarrierKernel, name: &str) -> anyhow::Result<()> {
    if !carrier_clone::market::valid_clone_name(name) {
        anyhow::bail!("化身名只允许小写字母、数字与连字符（1-64 位）");
    }
    if crate::remote::find(name).is_some() {
        anyhow::bail!("{name} 已是远程化身句柄——先 `agent remote remove {name}` 再安装本地化身");
    }
    let hub_cfg = kernel.config.hub.clone();
    let key = read_key(&hub_cfg.api_key_env)?;
    let existed = kernel.registry.find_by_name(name).is_some();

    println!("从 DupHub（{}）拉取化身 {name} …", hub_cfg.url);
    let files = carrier_clone::market::fetch_install_files(&hub_cfg.url, &key, name)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("已拉取 {} 个文件，安装中 …", files.len());

    let (id, agent_name, display_name) = kernel.clone_install_files(name, files).await?;
    if existed {
        println!("已重装：{display_name}（{agent_name}，id={id}；.dup/ 历史保留）");
    } else {
        println!("已安装：{display_name}（{agent_name}，id={id}）");
    }
    daemon_restart_hint();
    Ok(())
}

fn list(kernel: &CarrierKernel) {
    let mut entries = kernel.registry.list();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let remotes = crate::remote::load();
    if entries.is_empty() && remotes.is_empty() {
        println!(
            "（本机还没有化身——`agent install <name>` 从 DupHub 安装；`agent remote add <别名> <agent://地址>` 注册远程化身）"
        );
        return;
    }
    println!("{:<24} {:<12} {:<10} DISPLAY", "NAME", "VERSION", "STATE");
    for e in entries {
        println!(
            "{:<24} {:<12} {:<10} {}",
            e.name,
            e.manifest.version,
            format!("{:?}", e.state).to_lowercase(),
            e.manifest.display_name
        );
    }
    // 远程化身同构入列：STATE=remote，DISPLAY 带转发目标。
    for r in remotes {
        println!(
            "{:<24} {:<12} {:<10} {} → {}/{}",
            r.name, "-", "remote", r.display_name, r.url, r.agent
        );
    }
}

fn remove(kernel: &CarrierKernel, name: &str) -> anyhow::Result<()> {
    let entry = kernel
        .registry
        .find_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("本机没有叫 {name} 的化身"))?;

    // kill_agent 一并清：后台任务/调度/能力/事件/cron/持久化/aginx 离网。
    kernel.kill_agent(entry.id)?;
    let ws = kernel.config.effective_workspaces_dir().join(name);
    if ws.exists() {
        std::fs::remove_dir_all(&ws)?;
    }
    println!("已卸载 {name}（workspace 已删除，已离网）");
    daemon_restart_hint();
    Ok(())
}

/// 本地定义版本：workspace 的 template.json 是活真源（化身自我进化可能
/// 已改版本）；缺失时退回注册表 manifest 版本。
fn local_version(kernel: &CarrierKernel, name: &str, fallback: &str) -> String {
    let path = kernel
        .config
        .effective_workspaces_dir()
        .join(name)
        .join("template.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| carrier_clone::parse_template_manifest_lenient(&s))
        .map(|t| t.version)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

async fn update(kernel: &CarrierKernel, name: &str) -> anyhow::Result<()> {
    let entry = kernel
        .registry
        .find_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("本机没有叫 {name} 的化身——先 `agent install {name}`"))?;

    let hub_cfg = kernel.config.hub.clone();
    let key = read_key(&hub_cfg.api_key_env)?;
    let latest = carrier_clone::market::hub_latest_version(&hub_cfg.url, &key, name)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let local = local_version(kernel, name, &entry.manifest.version);

    println!("本地版本：{}  Hub 最新版本：{latest}", if local.is_empty() { "（无版本号）" } else { &local });
    if !latest.is_empty() && latest == local {
        println!("已是最新版本 {latest}");
        return Ok(());
    }

    println!("拉取新版本定义层 …");
    let files = carrier_clone::market::fetch_install_files(&hub_cfg.url, &key, name)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (id, agent_name, display_name) = kernel.clone_install_files(name, files).await?;
    println!(
        "已更新：{display_name}（{agent_name}，id={id}）→ {latest}；本地定义层已覆盖，.dup/ 历史保留"
    );
    daemon_restart_hint();
    Ok(())
}
