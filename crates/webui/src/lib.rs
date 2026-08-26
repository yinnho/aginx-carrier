//! Web UI——桌面形态的客户端面（ARCHITECTURE §11.3.1 Web-first 定案）。
//!
//! `aginx-carrier web` 起 axum 监听 loopback：`/api/*` JSON+SSE、`/` serve
//! 内嵌单页 SPA（vanilla JS + Alpine.js，include_str! 内嵌零文件系统读）。
//! 浏览器即客户端；信任模型照 dsh（Host 围栏防 DNS rebinding / 拒跨站 /
//! POST 仅 JSON），无 token——loopback 即信任边界。

pub mod env_file;
pub mod market;
pub mod server;
pub mod trust;

// agent:// 客户端与联系人台账已抽到 carrier-gateway（runtime 的 gateway_hub
// 工具需要复用，而 runtime 不能依赖 webui）；re-export 保住全部现有 use 路径。
pub use carrier_gateway::agent_client;
pub use carrier_gateway::tool_store;

pub use server::serve;
