//! agmem — 记忆工具 CLI 的库面（M35）。
//!
//! kv_get / kv_set / kv_list / kv_delete / memory_tree 五个工具的语义从
//! carrier-runtime 的 tools/kv.rs 与 tools/memory.rs 整体搬来（行为同构：
//! 同样的身份三元组隔离、同样的输出文案、同样的截断边界处理）。runtime
//! 侧只留 `agmem_bridge`：同名 ToolDefinition + spawn `agmem tool <name>`。
//!
//! 两张脸：
//! - 人/流程脚本：`agmem kv get <key>`、`agmem kv set <k> <v>`、
//!   `agmem kv list [prefix]`、`agmem kv del <k>`、`agmem tree search <q>`…
//! - 机读（runtime 桥用）：`agmem tool <name>`，stdin 收工具入参 JSON，
//!   stdout 出 D1 信封（`{"ok":true,"data":"…"}` / `{"ok":false,"error":…}`）。
//!
//! 与 agf 的路径分工不同，本 CLI 直开 memory substrate（rusqlite WAL +
//! busy_timeout=5000，substrate.rs open 内建），默认
//! `$HOME/.aginx/carrier/data/carrier.db` —— 与守护同一库、同一迁移链，
//! 并发安全（M35a spike 2026-09-03）。身份经 stdin JSON 保留键 `_ctx`
//! 注入：
//!
//! ```json
//! {
//!   "key": "theme",
//!   "_ctx": {
//!     "agent_id": "mo", "owner_id": "default", "user_id": "u1@im",
//!     "home_dir": "/home"
//!   }
//! }
//! ```
//!
//! `_ctx` 缺席 = 人面/裸调用，走 `--agent/--owner/--user` 旗标默认值。
//! kv 域按 (agent, owner, user) 隔离——人面默认身份看不到化身私域是
//! 正确行为，不是 bug。

pub mod kv;
pub mod tree;

use carrier_memory::MemorySubstrate;
use carrier_types::error::{CarrierError, CarrierResult};
use serde_json::Value;
use std::path::PathBuf;

/// 本 CLI 承载的全部工具名（与 runtime 桥的 definitions 一一对应；
/// kv_delete 是 CLI 补位——substrate 一直有 delete，上游工具面漏了）。
pub const TOOL_NAMES: &[&str] = &[
    "kv_get",
    "kv_set",
    "kv_list",
    "kv_delete",
    "memory_tree",
];

/// kv/tree 域的身份三元组 + substrate 定位。
#[derive(Debug, Clone)]
pub struct AgmemCtx {
    pub agent_id: String,
    pub owner_id: String,
    pub user_id: String,
    /// substrate 所在 HOME（默认 $HOME；人面可用 --db 直给库路径）。
    pub home_dir: Option<PathBuf>,
}

/// 人面默认身份。owner="default" 对齐 runtime memory.rs 的
/// `ctx.owner_id.unwrap_or("default")`；agent="me" 对齐手机注册的主
/// 化身；user="local" 标记"这是本机人手，不是任何渠道来信"。
pub fn default_identity() -> AgmemCtx {
    AgmemCtx {
        agent_id: "me".to_string(),
        owner_id: "default".to_string(),
        user_id: "local".to_string(),
        home_dir: None,
    }
}

/// 从入参 JSON 提取 `_ctx`（缺席 = 人面/裸调用 → 默认身份）。
pub fn ctx_of(input: &Value, fallback: AgmemCtx) -> AgmemCtx {
    let Some(c) = input.get("_ctx") else {
        return fallback;
    };
    AgmemCtx {
        agent_id: c
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&fallback.agent_id)
            .to_string(),
        owner_id: c
            .get("owner_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&fallback.owner_id)
            .to_string(),
        user_id: c
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&fallback.user_id)
            .to_string(),
        home_dir: c
            .get("home_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from),
    }
}

/// substrate 库路径：显式 --db > _ctx.home_dir > $HOME。
pub fn db_path_of(ctx: &AgmemCtx, db_flag: Option<&PathBuf>) -> PathBuf {
    if let Some(p) = db_flag {
        return p.clone();
    }
    let home = ctx
        .home_dir
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/home"));
    home.join(".aginx").join("carrier").join("data").join("carrier.db")
}

/// 打开 substrate（WAL + busy_timeout 在 open 内建；迁移同步进行）。
pub fn open_substrate(path: &std::path::Path) -> CarrierResult<MemorySubstrate> {
    MemorySubstrate::open(path)
}

/// 机读面单真源入口：工具名 + 入参 JSON → String 结果。
/// 人面子命令在 main.rs 把参数拼回工具入参后也走这里。
pub async fn execute_tool(
    name: &str,
    input: &Value,
    db_flag: Option<&PathBuf>,
    fallback: AgmemCtx,
) -> Option<CarrierResult<String>> {
    let ctx = ctx_of(input, fallback);
    match name {
        // kv 面：短平快，直接在当前线程查（WAL 读不挡守护）。
        "kv_get" => Some(kv::kv_get(input, &ctx, db_flag)),
        "kv_set" => Some(kv::kv_set(input, &ctx, db_flag)),
        "kv_list" => Some(kv::kv_list(input, &ctx, db_flag)),
        "kv_delete" => Some(kv::kv_delete(input, &ctx, db_flag)),
        // tree 面：substrate 的 tree_* 是同步调用，包 blocking 线程
        // 保持与守护 runtime 同样的不挡调度器纪律。
        "memory_tree" => Some(
            tokio::task::spawn_blocking({
                let input = input.clone();
                let ctx = ctx.clone();
                let db_flag = db_flag.cloned();
                move || tree::memory_tree(&input, &ctx, db_flag.as_ref())
            })
            .await
            .map_err(|e| CarrierError::Internal(e.to_string()))
            .and_then(|r| r),
        ),
        _ => None,
    }
}

/// 人面错误出口：agf 同款——非零退出 + stderr 一行。
pub fn bail_human(e: &CarrierError) -> ! {
    eprintln!("agmem: {e}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ctx_of_takes_identity_and_home() {
        let input = json!({
            "key": "k",
            "_ctx": {
                "agent_id": "mo", "owner_id": "o1", "user_id": "u@im",
                "home_dir": "/tmp/h"
            }
        });
        let c = ctx_of(&input, default_identity());
        assert_eq!(c.agent_id, "mo");
        assert_eq!(c.owner_id, "o1");
        assert_eq!(c.user_id, "u@im");
        assert_eq!(c.home_dir, Some(PathBuf::from("/tmp/h")));
        // db 路径随之指向该 HOME 的 carrier.db
        assert_eq!(
            db_path_of(&c, None),
            PathBuf::from("/tmp/h/.aginx/carrier/data/carrier.db")
        );
    }

    #[test]
    fn ctx_of_falls_back_to_default_identity() {
        let c = ctx_of(&json!({"key": "k"}), default_identity());
        assert_eq!(c.agent_id, "me");
        assert_eq!(c.owner_id, "default");
        assert_eq!(c.user_id, "local");
    }

    #[test]
    fn db_flag_wins_over_ctx_home() {
        let c = AgmemCtx {
            home_dir: Some(PathBuf::from("/tmp/h")),
            ..default_identity()
        };
        assert_eq!(
            db_path_of(&c, Some(&PathBuf::from("/tmp/x.db"))),
            PathBuf::from("/tmp/x.db")
        );
    }

    #[test]
    fn tool_names_are_the_contract() {
        // runtime 桥按这个名字表对齐 definitions；改名 = 破金样本。
        assert_eq!(TOOL_NAMES, &["kv_get", "kv_set", "kv_list", "kv_delete", "memory_tree"]);
    }
}
