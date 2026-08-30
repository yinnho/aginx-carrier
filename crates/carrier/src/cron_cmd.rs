//! `aginx-carrier cron` — 任务面 CLI（CARRIER.md §3.4-3）。
//!
//! aclone（AginxOS 薄壳 TUI）与脚本经此看任务、开关任务。创建不走 CLI
//! （一期）：化身在对话里用 cron_create 自建（链式流水线同理）。
//!
//! **列表** = JSON Lines 一行一任务，字段 `id / name / schedule /
//! next_fire / last_result / enabled`（另有 agent / one_shot / late）。
//! stdout 只放数据行，空列表零行；schedule 三形态 at/every/cron 原样
//! 序列化（`{"kind":"every","every_secs":600}`）。
//!
//! **离线语义（立法）**：手机是会关机的节点。开机/daemon 重启后，
//! `due_jobs()` 把 `next_run <= now` 的任务立即视为到期——**补跑一枪**，
//! Every/Cron 随后 re-anchor 到 now+间隔/下一匹配槽，At 过去即触发，
//! 绝不连环补 N 次。`late` 字段实时反映「应跑未跑」：daemon 停机或
//! 在飞阻塞时，列表仍能看到哪些任务迟到待补。
//!
//! **开关与常驻 daemon 的并发模型**：pause/resume/remove 走 kernel 的
//! set_enabled/remove_job——DB 定点写回（enabled 列 / 整行删除），常驻
//! daemon 每 tick（15s）`reconcile_from_db` 把 DB 的 enabled/���在性采进
//! 内存。暂停语义 = 在飞轮跑完、下一槽不再触发（不杀正在执行的轮）。

use std::str::FromStr;

use carrier_kernel::kernel::CarrierKernel;

use crate::CronAction;

pub fn run(action: CronAction) -> anyhow::Result<()> {
    // 一次性进程：内嵌 runtime 跑完即退（agent_cmd 同款形态）。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async(action))
}

async fn run_async(action: CronAction) -> anyhow::Result<()> {
    // CLI 面兜底写骨架（列表/开关不需要 brain，不该被它挡住）。
    aginx_carrier::wiring::seed_brain_skeleton_if_missing();
    // 裸 boot、不起后台 agent 循环/心跳：读 DB 加载 cron 表 + registry。
    let kernel = CarrierKernel::boot(None)?;
    match action {
        CronAction::List { agent } => {
            list(&kernel, agent.as_deref());
            Ok(())
        }
        CronAction::Pause { id } => toggle(&kernel, &id, false),
        CronAction::Resume { id } => toggle(&kernel, &id, true),
        CronAction::Remove { id } => remove(&kernel, &id),
    }
}

/// agent_id → 化身名（CLI 消费面用名字；查不到退回 id 串）。
fn agent_names(kernel: &CarrierKernel) -> std::collections::HashMap<carrier_types::agent::AgentId, String> {
    kernel
        .registry
        .list()
        .into_iter()
        .map(|e| (e.id, e.name))
        .collect()
}

fn list(kernel: &CarrierKernel, agent_filter: Option<&str>) {
    let names = agent_names(kernel);
    let mut metas = kernel.cron_scheduler.list_all_metas();
    // DashMap 遍历序不定——按 (agent, name) 排序稳住输出。
    metas.sort_by(|a, b| {
        let ka = names.get(&a.job.agent_id).cloned().unwrap_or_default();
        let kb = names.get(&b.job.agent_id).cloned().unwrap_or_default();
        ka.cmp(&kb).then_with(|| a.job.name.cmp(&b.job.name))
    });
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MAX);
    let mut printed = 0usize;
    for meta in metas {
        let job = &meta.job;
        let agent_name = names
            .get(&job.agent_id)
            .cloned()
            .unwrap_or_else(|| job.agent_id.to_string());
        if let Some(f) = agent_filter {
            if agent_name != f {
                continue;
            }
        }
        // late = 应跑未跑（daemon 停机/在飞阻塞/关机刚回来的补跑前一刻）。
        let late = job.enabled && job.next_run.map(|t| t.timestamp() < now_secs).unwrap_or(false);
        let line = serde_json::json!({
            "id": job.id.to_string(),
            "name": job.name,
            "schedule": job.schedule,
            "next_fire": job.next_run,
            "last_result": meta.last_status,
            "enabled": job.enabled,
            "agent": agent_name,
            "one_shot": meta.one_shot,
            "late": late,
        });
        println!("{}", line);
        printed += 1;
    }
    if printed == 0 {
        eprintln!("（没有任务——化身在对话里用 cron_create 自建；`cron list --agent <名>` 过滤）");
    }
}

fn toggle(kernel: &CarrierKernel, id: &str, enabled: bool) -> anyhow::Result<()> {
    let job_id = carrier_types::scheduler::CronJobId::from_str(id)
        .map_err(|e| anyhow::anyhow!("{id} 不是合法任务 id（UUID）: {e}"))?;
    kernel.cron_scheduler.set_enabled(job_id, enabled)?;
    let verb = if enabled { "已恢复" } else { "已暂停" };
    println!("{verb}：{id}（在飞轮跑完，下一槽{}）", if enabled { "起继续" } else { "不再触发" });
    Ok(())
}

fn remove(kernel: &CarrierKernel, id: &str) -> anyhow::Result<()> {
    let job_id = carrier_types::scheduler::CronJobId::from_str(id)
        .map_err(|e| anyhow::anyhow!("{id} 不是合法任务 id（UUID）: {e}"))?;
    let job = kernel.cron_scheduler.remove_job(job_id)?;
    println!("已删除：{}（{id}）", job.name);
    Ok(())
}
