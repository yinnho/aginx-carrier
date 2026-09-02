//! HTML→Markdown / 外部内容包裹 — 已整体搬到 `agb` crate（M31 D3 批1）。
//! 旧调用点经本 re-export 不变；M31b 后随 fetch_engine 管线退役。
pub use agb::web_content::*;
