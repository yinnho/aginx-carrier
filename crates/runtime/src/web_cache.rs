//! Web cache — 已整体搬到 `agb` crate（M31 D3 批1，单真源）。kernel/runtime
//! 内旧调用点经本 re-export 不变；M31b 拆完 fetch_engine 管线后本 shim 一并退役。
pub use agb::web_cache::*;
