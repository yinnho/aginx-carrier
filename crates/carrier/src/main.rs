//! aginx-carrier — 分身 OS（aginx 网络上的一个网站）
//!
//! 从 OpenCarrier fork 而来的个人部署版：单操作者、私有化、aginx 原生。
//! 定位与总纲见 aginx 生态 docs/AGINX-CARRIER-VISION.md。

mod acp;
mod start;
mod web;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "aginx-carrier",
    version,
    about = "分身 OS — 托管数字分身的 Agent 运行时",
    long_about = "aginx-carrier 是 aginx Agent 互联网上托管分身的运行时。\n自己的分身跟着自己的设备走；用别人的分身时算力在他家、数据在你家。"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 运行时分身守护进程（个人部署形态）
    Start,
    /// 桌面形态：carrier 自带 Web UI，浏览器即客户端（ARCHITECTURE §11.3.1）
    Web {
        /// 监听地址（默认仅回环；公网暴露走 nginx 反代）
        #[arg(long, default_value = "127.0.0.1:8703")]
        listen: String,
    },
    /// stdio ACP 桥：被 aginx 网关拉起，把分身暴露到 agent:// 网络
    Acp {
        /// 分身名称
        #[arg(long)]
        clone: String,
        /// 续接会话 id（网关 `${SESSION_ID}` 注入；缺省 = 新会话并铸造 id）
        #[arg(long)]
        session: Option<String>,
    },
    /// 显示版本与本地数据目录等信息
    Info,
    /// 用户侧票据仓库（借用机制的会话真源在用户侧）
    Ticket {
        #[command(subcommand)]
        action: TicketAction,
    },
}

#[derive(Subcommand)]
enum TicketAction {
    /// 列出某分身下的全部票据
    List {
        /// 分身名称
        #[arg(long)]
        clone: String,
    },
    /// 从 JSON 文件导入票据（如 agc --save-ticket 的产出）
    Import {
        /// 分身名称
        #[arg(long)]
        clone: String,
        /// 对话标签（空 = default）
        #[arg(long, default_value = "default")]
        label: String,
        /// 票据 JSON 文件
        #[arg(long)]
        file: String,
    },
    /// 导出票据到 JSON 文件（喂给 agc --ticket）
    Export {
        /// 分身名称
        #[arg(long)]
        clone: String,
        /// 对话标签
        #[arg(long, default_value = "default")]
        label: String,
        /// 输出文件
        #[arg(long)]
        file: String,
    },
    /// 删除一张票据（开新对话）
    Delete {
        /// 分身名称
        #[arg(long)]
        clone: String,
        /// 对话标签
        #[arg(long, default_value = "default")]
        label: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Start => start::run()?,
        Command::Web { listen } => web::run(listen)?,
        Command::Acp { clone, session } => acp::run(clone, session)?,
        Command::Info => {
            let data_dir = dirs::home_dir()
                .map(|h| h.join(".aginx").join("carrier"))
                .expect("无法解析用户主目录");
            println!("aginx-carrier {}", env!("CARGO_PKG_VERSION"));
            println!("数据目录: {}", data_dir.display());
        }
        Command::Ticket { action } => ticket_cmd(action)?,
    }
    Ok(())
}

/// 用户侧票据仓库 CLI。个人安装版"同一台机器既是主人又是用户"，直接复用
/// 本机 carrier.db（v33 borrow_tickets 表）；App/远程宿主走 lib API。
fn ticket_cmd(action: TicketAction) -> anyhow::Result<()> {
    use carrier_memory::MemorySubstrate;

    let substrate = MemorySubstrate::open(
        &dirs::home_dir()
            .map(|h| h.join(".aginx").join("carrier").join("data").join("carrier.db"))
            .expect("无法解析用户主目录"),
    )?;
    let store = substrate.tickets();
    match action {
        TicketAction::List { clone } => {
            let entries = store.list(&clone)?;
            if entries.is_empty() {
                println!("（无票据）");
                return Ok(());
            }
            for e in entries {
                println!(
                    "{:<16} {:>4} msgs  {}",
                    e.label, e.message_count, e.updated_at
                );
            }
        }
        TicketAction::Import { clone, label, file } => {
            let raw = std::fs::read_to_string(&file)?;
            let ticket: carrier_memory::session::SessionTicket = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("{file} 不是合法票据 JSON: {e}"))?;
            store.save(&clone, &label, &ticket)?;
            println!("已存入 {} / {}（{} msgs）", clone, label, ticket.messages.len());
        }
        TicketAction::Export { clone, label, file } => {
            let ticket = store
                .load(&clone, &label)?
                .ok_or_else(|| anyhow::anyhow!("没有 {} / {} 的票据", clone, label))?;
            std::fs::write(&file, serde_json::to_string_pretty(&ticket)?)?;
            println!("已导出到 {}（{} msgs）", file, ticket.messages.len());
        }
        TicketAction::Delete { clone, label } => {
            if store.delete(&clone, &label)? {
                println!("已删除 {} / {}", clone, label);
            } else {
                println!("没有 {} / {} 的票据", clone, label);
            }
        }
    }
    Ok(())
}
