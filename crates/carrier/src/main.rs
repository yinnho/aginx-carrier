//! aginx-carrier — 分身 OS（aginx 网络上的一个网站）
//!
//! 从 OpenCarrier fork 而来的个人部署版：单操作者、私有化、aginx 原生。
//! 定位与总纲见 aginx 生态 docs/AGINX-CARRIER-VISION.md。

mod acp;
mod start;

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
    /// stdio ACP 桥：被 aginx 网关拉起，把分身暴露到 agent:// 网络
    Acp {
        /// 分身名称
        #[arg(long)]
        clone: String,
    },
    /// 显示版本与本地数据目录等信息
    Info,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Start => start::run()?,
        Command::Acp { clone } => acp::run(clone)?,
        Command::Info => {
            let data_dir = dirs::home_dir()
                .map(|h| h.join(".aginx").join("carrier"))
                .expect("无法解析用户主目录");
            println!("aginx-carrier {}", env!("CARGO_PKG_VERSION"));
            println!("数据目录: {}", data_dir.display());
        }
    }
    Ok(())
}
