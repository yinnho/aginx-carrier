//! agmem — 记忆工具 CLI（M35）。
//!
//! 两张脸：人面子命令（kv get/set/list/del、tree search/topic/source/
//! global/drill/leaves），机读面 `agmem tool <name>`（stdin 收工具入参
//! JSON，stdout 出 D1 信封；runtime 的 agmem_bridge 消费）。身份三元组
//! 经 stdin JSON 保留键 `_ctx` 注入——见 lib.rs 头注。

use clap::{Parser, Subcommand};
use serde_json::Value;

#[derive(Parser)]
#[command(
    name = "agmem",
    version,
    about = "记忆工具 CLI — kv 存取 + 记忆树检索",
    long_about = "agmem 是记忆面工具的 CLI 形态（M35 外置成包）。\nkv 按 (agent, owner, user) 三元组隔离；tree 是已摄取对话/邮件/文档的\n回溯索引。人面子命令直接给参数；机读面 `agmem tool <name>` 从 stdin\n读 JSON，stdout 出 D1 信封（{\"ok\":true,\"data\":…}）。"
)]
struct Cli {
    /// substrate 库路径（缺省 $HOME/.aginx/carrier/data/carrier.db）
    #[arg(long, global = true)]
    db: Option<String>,
    /// 身份三元组：agent（默认 me）
    #[arg(long, global = true, default_value = "me")]
    agent: String,
    /// 身份三元组：owner（默认 default）
    #[arg(long, global = true, default_value = "default")]
    owner: String,
    /// 身份三元组：user（默认 local —— 本机人手，非渠道来信）
    #[arg(long, global = true, default_value = "local")]
    user: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// kv：读一个键
    Get {
        /// 键名
        key: String,
    },
    /// kv：写一个键（值缺省读 stdin；能解析成 JSON 就存 JSON，否则存字符串）
    Set {
        /// 键名
        key: String,
        /// 要存的值（缺省读 stdin）
        value: Option<String>,
    },
    /// kv：列键（可选前缀过滤）
    List {
        /// 前缀（如 entity.）
        prefix: Option<String>,
    },
    /// kv：删一个键
    Del {
        /// 键名
        key: String,
    },
    /// tree：实体搜索（后续 topic 查询的入口）
    Search {
        /// 搜索词
        query: String,
        /// 实体种类过滤（如 person）
        #[arg(long)]
        kind: Option<String>,
        /// 最多返回几条（默认 5）
        #[arg(long)]
        limit: Option<u64>,
    },
    /// tree：按实体查主题子树
    Topic {
        /// search_entities 给出的规范实体 ID
        entity_id: String,
        /// 过滤词
        #[arg(long)]
        query: Option<String>,
        /// 时间窗（天）
        #[arg(long)]
        days: Option<u64>,
    },
    /// tree：按来源查（chat/email/document）
    Source {
        /// 来源 ID
        #[arg(long)]
        source_id: Option<String>,
        /// 来源种类
        #[arg(long)]
        source_kind: Option<String>,
        /// 过滤词
        #[arg(long)]
        query: Option<String>,
        /// 时间窗（天）
        #[arg(long)]
        days: Option<u64>,
    },
    /// tree：全局查
    Global {
        /// 过滤词
        #[arg(long)]
        query: Option<String>,
        /// 时间窗（天）
        #[arg(long)]
        days: Option<u64>,
    },
    /// tree：展开摘要节点的子层
    Drill {
        /// 摘要节点 ID
        node_id: String,
        /// 层数（1–3，默认 1）
        #[arg(long)]
        depth: Option<u64>,
    },
    /// tree：按 chunk ID 取叶子原文
    Leaves {
        /// chunk ID 列表
        chunk_ids: Vec<String>,
    },
    /// 机读面：工具名 + stdin JSON 入参 → stdout D1 信封（runtime 桥用）
    Tool {
        /// 工具名（kv_get / kv_set / kv_list / kv_delete / memory_tree）
        name: String,
    },
}

fn main() -> anyhow::Result<()> {
    // CLI 人面常接 `| head`：Rust 默认忽略 SIGPIPE，写已关闭管道会以
    // "failed printing to stdout: Broken pipe" panic 收场。恢复默认处置
    // = 安静地死于 SIGPIPE，与普通 CLI 一致（agb/agf 同款，设备实测过）。
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(cli))
}

fn fallback_of(cli: &Cli) -> agmem::AgmemCtx {
    agmem::AgmemCtx {
        agent_id: cli.agent.clone(),
        owner_id: cli.owner.clone(),
        user_id: cli.user.clone(),
        home_dir: None,
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let db_flag = cli.db.as_ref().map(std::path::PathBuf::from);
    let fallback = fallback_of(&cli);
    match &cli.command {
        Command::Tool { name } => {
            let mut raw = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw)?;
            let input: Value = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("stdin 不是合法 JSON 入参: {e}"))?;
            match agmem::execute_tool(name, &input, db_flag.as_ref(), fallback).await {
                None => {
                    print_envelope_error(&format!("unknown tool: {name}"));
                    std::process::exit(1);
                }
                Some(Ok(data)) => {
                    println!(
                        "{}",
                        serde_json::json!({"ok": true, "data": data, "meta": {"tool": name}})
                    );
                }
                Some(Err(e)) => {
                    print_envelope_error(&e.to_string());
                    std::process::exit(1);
                }
            }
        }
        human => {
            // 人面：参数拼回工具 JSON 入参，走同一条 execute_tool 单真源。
            let (name, input) = args_to_input(human)?;
            let input_val = Value::Object(input);
            match agmem::execute_tool(name, &input_val, db_flag.as_ref(), fallback).await {
                None => anyhow::bail!("unknown tool: {name}"),
                Some(Ok(data)) => println!("{data}"),
                Some(Err(e)) => agmem::bail_human(&e),
            }
        }
    }
    Ok(())
}

/// 人面子命令 → (工具名, 工具入参 JSON)。与工具 schema 同构，仅做参数搬运。
/// set 的值缺省读 stdin（sh-first：`… | agmem set k` / `agmem set k < in`）；
/// 能解析成 JSON 就按 JSON 存（数字/对象原样），否则存裸字符串。
fn args_to_input(cmd: &Command) -> anyhow::Result<(&'static str, serde_json::Map<String, Value>)> {
    use serde_json::json;
    let mut m = serde_json::Map::new();
    match cmd {
        Command::Get { key } => {
            m.insert("key".into(), json!(key));
            Ok(("kv_get", m))
        }
        Command::Set { key, value } => {
            let raw = match value {
                Some(v) => v.clone(),
                None => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    buf
                }
            };
            let parsed: Value = serde_json::from_str(&raw).unwrap_or(Value::String(raw));
            m.insert("key".into(), json!(key));
            m.insert("value".into(), parsed);
            Ok(("kv_set", m))
        }
        Command::List { prefix } => {
            if let Some(p) = prefix {
                m.insert("prefix".into(), json!(p));
            }
            Ok(("kv_list", m))
        }
        Command::Del { key } => {
            m.insert("key".into(), json!(key));
            Ok(("kv_delete", m))
        }
        Command::Search {
            query,
            kind,
            limit,
        } => {
            m.insert("mode".into(), json!("search_entities"));
            m.insert("query".into(), json!(query));
            if let Some(k) = kind {
                m.insert("kinds".into(), json!([k]));
            }
            if let Some(l) = limit {
                m.insert("limit".into(), json!(l));
            }
            Ok(("memory_tree", m))
        }
        Command::Topic {
            entity_id,
            query,
            days,
        } => {
            m.insert("mode".into(), json!("query_topic"));
            m.insert("entity_id".into(), json!(entity_id));
            if let Some(q) = query {
                m.insert("query".into(), json!(q));
            }
            if let Some(d) = days {
                m.insert("time_window_days".into(), json!(d));
            }
            Ok(("memory_tree", m))
        }
        Command::Source {
            source_id,
            source_kind,
            query,
            days,
        } => {
            m.insert("mode".into(), json!("query_source"));
            if let Some(s) = source_id {
                m.insert("source_id".into(), json!(s));
            }
            if let Some(s) = source_kind {
                m.insert("source_kind".into(), json!(s));
            }
            if let Some(q) = query {
                m.insert("query".into(), json!(q));
            }
            if let Some(d) = days {
                m.insert("time_window_days".into(), json!(d));
            }
            Ok(("memory_tree", m))
        }
        Command::Global { query, days } => {
            m.insert("mode".into(), json!("query_global"));
            if let Some(q) = query {
                m.insert("query".into(), json!(q));
            }
            if let Some(d) = days {
                m.insert("time_window_days".into(), json!(d));
            }
            Ok(("memory_tree", m))
        }
        Command::Drill { node_id, depth } => {
            m.insert("mode".into(), json!("drill_down"));
            m.insert("node_id".into(), json!(node_id));
            if let Some(d) = depth {
                m.insert("max_depth".into(), json!(d));
            }
            Ok(("memory_tree", m))
        }
        Command::Leaves { chunk_ids } => {
            m.insert("mode".into(), json!("fetch_leaves"));
            m.insert("chunk_ids".into(), json!(chunk_ids));
            Ok(("memory_tree", m))
        }
        // 已在 run() 里分走；穷尽匹配留这层保险
        Command::Tool { .. } => unreachable!("Tool 在 run() 先行分派"),
    }
}

fn print_envelope_error(msg: &str) {
    println!("{}", serde_json::json!({"ok": false, "error": msg}));
    eprintln!("agmem: {msg}");
}
