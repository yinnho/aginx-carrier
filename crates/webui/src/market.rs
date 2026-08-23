//! 装分身页后端——DupHub 市场列表 / 权限预览 / 一键安装（webui 第二刀）。
//!
//! 权限预览是 ARCHITECTURE §11.4 坑 3 的第一实现：安装 = 装代码，消费级
//! 必须在安装前看到分身声明了什么（flows/tools/shell_allow/mcp_servers）。
//! 拉取走 carrier_clone::hub 的 dup 文件级端点（带 SSRF 防护）；安装走
//! kernel 的 `clone_install_files` 正规管线（校验→落盘→spawn→入网钩子）。

use std::collections::BTreeMap;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use carrier_clone::hub;
use futures::stream::{self, StreamExt};

use std::sync::Arc;

use crate::server::WebState;

/// Hub 列表每页条数（Hub 的 total 是当页数不是总数——UI 只能 load-more）。
const LIST_LIMIT: u32 = 24;
/// 预览子集文件上限（防滥用：只拉 template.json + flows + skills + SOUL.md）。
const PREVIEW_MAX_FILES: usize = 40;
const PREVIEW_CONCURRENCY: usize = 4;
/// 安装全量文件并行度（fetch_dup_files 是串行逐文件，大分身不可接受）。
const INSTALL_CONCURRENCY: usize = 8;

fn hub_err(status: StatusCode, code: &str, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg, "code": code }))).into_response()
}

fn ok_json(v: serde_json::Value) -> Response {
    Json(v).into_response()
}

// ============================================================
// GET /api/market?q=&page=
// ============================================================

#[derive(serde::Deserialize)]
pub struct MarketQuery {
    q: Option<String>,
    page: Option<u32>,
}

pub async fn list(State(st): State<Arc<WebState>>, Query(q): Query<MarketQuery>) -> Response {
    let hub_cfg = st.kernel.config.hub.clone();
    let key = hub::read_api_key(&hub_cfg.api_key_env)
        .ok()
        .filter(|k| !k.trim().is_empty());
    let page = q.page.unwrap_or(1).max(1);
    let json = match hub::search_templates_json(
        &hub_cfg.url,
        key.as_deref().unwrap_or(""),
        q.q.as_deref(),
        Some(LIST_LIMIT),
        Some(page),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            return hub_err(
                StatusCode::BAD_GATEWAY,
                "hub_unreachable",
                &format!("无法连接 Hub（{}）: {e:#}", hub_cfg.url),
            )
        }
    };
    let templates = json.get("templates").cloned().unwrap_or_default();
    let has_more = templates
        .as_array()
        .is_some_and(|a| a.len() as u32 == LIST_LIMIT);
    ok_json(serde_json::json!({
        "hub_url": hub_cfg.url,
        "api_key_env": hub_cfg.api_key_env,
        "key_configured": key.is_some(),
        "templates": templates,
        "page": page,
        "has_more": has_more,
    }))
}

// ============================================================
// GET /api/market/{name}/preview —— 权限预览
// ============================================================

pub async fn preview(State(st): State<Arc<WebState>>, AxumPath(name): AxumPath<String>) -> Response {
    let hub_cfg = st.kernel.config.hub.clone();
    let Some(key) = hub::read_api_key(&hub_cfg.api_key_env)
        .ok()
        .filter(|k| !k.trim().is_empty())
    else {
        return hub_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_key",
            &format!("未配置 Hub API key（{}）——请先在市场页填写", hub_cfg.api_key_env),
        );
    };

    // 详情：file_size / visibility / price / purchased（manifest 只有 sha256，算不出大小）。
    let detail = match hub::get_template(&hub_cfg.url, &key, &name).await {
        Ok(v) => v,
        Err(e) => {
            let (s, c, m) = classify_hub_err(&format!("{e:#}"));
            return hub_err(s, c, &m);
        }
    };

    let manifest = match hub::fetch_dup_manifest(&hub_cfg.url, &key, &name, None).await {
        Ok(m) => m,
        Err(e) => {
            let (s, c, m) = classify_hub_err(&format!("{e:#}"));
            return hub_err(s, c, &m);
        }
    };

    let paths = preview_paths(&manifest.files);
    let files = match fetch_parallel(&hub_cfg.url, &key, &name, &paths, PREVIEW_CONCURRENCY).await {
        Ok(f) => f,
        Err(e) => {
            let (s, c, m) = classify_hub_err(&e);
            return hub_err(s, c, &m);
        }
    };

    let extracted = extract_preview(&files);
    let installed = st.kernel.registry.find_by_name(&name).is_some();
    ok_json(serde_json::json!({
        "name": name,
        "latest_version": detail.get("latest_version").cloned().unwrap_or_default(),
        "display_name": extracted.get("display_name").cloned().unwrap_or_else(|| serde_json::json!(name)),
        "description": extracted.get("description").cloned().unwrap_or_default(),
        "author": detail.get("author").cloned().unwrap_or_default(),
        "tags": extracted.get("tags").cloned().unwrap_or_default(),
        "visibility": detail.get("visibility").cloned().unwrap_or_default(),
        "price": detail.get("price").cloned().unwrap_or_else(|| serde_json::json!(0)),
        "purchased": detail.get("purchased").cloned().unwrap_or(serde_json::Value::Bool(false)),
        "file_count": manifest.files.len(),
        "total_bytes": version_file_size(&detail),
        "name_valid": valid_clone_name(&name),
        "installed": installed,
        "mcp_servers": extracted.get("mcp_servers").cloned().unwrap_or_default(),
        "plugins": extracted.get("plugins").cloned().unwrap_or_default(),
        "flows": extracted.get("flows").cloned().unwrap_or_default(),
        "format_errors": extracted.get("format_errors").cloned().unwrap_or_default(),
    }))
}

/// 从详情 JSON 里取最新版本的 file_size（versions[].version == latest_version）。
fn version_file_size(detail: &serde_json::Value) -> serde_json::Value {
    let latest = detail
        .get("latest_version")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    detail
        .get("versions")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            latest
                .as_ref()
                .and_then(|l| arr.iter().find(|e| e.get("version").and_then(|v| v.as_str()) == Some(l.as_str())))
                .or_else(|| arr.first())
                .and_then(|e| e.get("file_size").cloned())
        })
        .unwrap_or_else(|| serde_json::json!(0))
}

/// 预览要拉的文件子集：template.json + flows frontmatter + skills（触发废弃
/// 告警）+ SOUL.md。knowledge/references 等大文件不拉。
fn preview_paths(files: &BTreeMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    for p in files.keys() {
        let wanted = p == "template.json"
            || p == "SOUL.md"
            || p == "profile.md"
            || (p.starts_with("flows/")
                && (p.ends_with("flow.md") || p.ends_with("SKILL.md")))
            || p.starts_with("skills/");
        if wanted {
            out.push(p.clone());
            if out.len() >= PREVIEW_MAX_FILES {
                break;
            }
        }
    }
    out
}

// ============================================================
// POST /api/market/{name}/install
// ============================================================

pub async fn install(State(st): State<Arc<WebState>>, AxumPath(name): AxumPath<String>) -> Response {
    let hub_cfg = st.kernel.config.hub.clone();
    let Some(key) = hub::read_api_key(&hub_cfg.api_key_env)
        .ok()
        .filter(|k| !k.trim().is_empty())
    else {
        return hub_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_key",
            &format!("未配置 Hub API key（{}）——请先在市场页填写", hub_cfg.api_key_env),
        );
    };
    if !valid_clone_name(&name) {
        return hub_err(
            StatusCode::BAD_REQUEST,
            "bad_name",
            "分身名只允许小写字母、数字与连字符（1-64 位）",
        );
    }

    let manifest = match hub::fetch_dup_manifest(&hub_cfg.url, &key, &name, None).await {
        Ok(m) => m,
        Err(e) => {
            let (s, c, m) = classify_hub_err(&format!("{e:#}"));
            return hub_err(s, c, &m);
        }
    };
    let paths: Vec<String> = manifest.files.keys().cloned().collect();
    let files = match fetch_parallel(&hub_cfg.url, &key, &name, &paths, INSTALL_CONCURRENCY).await {
        Ok(f) => f,
        Err(e) => {
            let (s, c, m) = classify_hub_err(&e);
            return hub_err(s, c, &m);
        }
    };

    match st.kernel.clone_install_files(&name, files).await {
        Ok((id, agent_name, display_name)) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "agent_id": id,
                "name": agent_name,
                "display_name": display_name,
            })),
        )
            .into_response(),
        // clone_install_files 的错误是结构化中文文案（非法 name/格式校验拒收/落盘失败）
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ============================================================
// 内部工具
// ============================================================

/// clone_install_files 的 name 规则前置预检（安装端是硬门禁，提前友好提示）。
fn valid_clone_name(name: &str) -> bool {
    let n = name.len();
    (1..=64).contains(&n)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// 并行拉一组 dup 文件（fetch_dup_files 是串行逐文件，这里带并发度）。
/// 错误返回原始 bail 文本，由 classify_hub_err 统一分类。
async fn fetch_parallel(
    hub_url: &str,
    key: &str,
    name: &str,
    paths: &[String],
    concurrency: usize,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let results: Vec<(String, Result<Vec<u8>, String>)> = stream::iter(paths.to_vec())
        .map(|p| {
            let hub = hub_url.to_string();
            let key = key.to_string();
            let name = name.to_string();
            async move {
                let r = hub::fetch_dup_file(&hub, &key, &name, &p, None)
                    .await
                    .map_err(|e| format!("{e:#}"));
                (p, r)
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;
    let mut out = BTreeMap::new();
    for (p, r) in results {
        out.insert(p, r?);
    }
    Ok(out)
}

/// hub.rs 的 bail 文本稳定携带 StatusCode Display（"拉取 manifest 失败
/// {name}: 401 Unauthorized - ..."）。扫第一个 ": NNN" 段做分类。
fn status_in_err(text: &str) -> Option<u16> {
    let mut from = 0usize;
    while let Some(p) = text[from..].find(": ") {
        let s = from + p + 2;
        let digits: String = text[s..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() == 3 {
            if let Ok(n) = digits.parse::<u16>() {
                return Some(n);
            }
        }
        from = s;
    }
    None
}

fn classify_hub_err(text: &str) -> (StatusCode, &'static str, String) {
    match status_in_err(text) {
        Some(401) => (StatusCode::UNAUTHORIZED, "bad_key", "Hub API key 无效或已失效，请到市场页更新".to_string()),
        Some(402) => (StatusCode::PAYMENT_REQUIRED, "paid", "付费模版——需先在 DupHub 购买后再安装".to_string()),
        Some(403) => (StatusCode::FORBIDDEN, "private", "私有分身，当前 key 无权访问".to_string()),
        Some(404) => (StatusCode::NOT_FOUND, "not_found", "Hub 上不存在这个分身".to_string()),
        _ => (StatusCode::BAD_GATEWAY, "hub_unreachable", format!("Hub 通信失败: {text}")),
    }
}

/// 权限预览提取（纯函数，主单测点）。
///
/// 输入：预览子集文件（template.json + flows/*/[flow|SKILL].md + skills/**）。
/// 输出：flows 权限清单 + mcp_servers/plugins/tags + 安装期格式校验错误。
pub fn extract_preview(files: &BTreeMap<String, Vec<u8>>) -> serde_json::Value {
    let mut display_name = String::new();
    let mut description = String::new();
    let mut tags: Vec<String> = Vec::new();
    let mut mcp_servers: Vec<String> = Vec::new();
    let mut plugins: Vec<String> = Vec::new();

    if let Some(b) = files.get("template.json") {
        if let Some(t) = carrier_clone::parse_template_manifest_lenient(&String::from_utf8_lossy(b)) {
            if !t.display_name.is_empty() {
                display_name = t.display_name;
            }
            if !t.description.is_empty() {
                description = t.description;
            }
            tags = t.tags;
            mcp_servers = t.mcp_servers;
            plugins = t.plugins;
        }
    }

    let mut flows = Vec::new();
    for (path, bytes) in files {
        let is_flow = path.starts_with("flows/")
            && (path.ends_with("flow.md") || path.ends_with("SKILL.md"));
        if !is_flow {
            continue;
        }
        let def = carrier_types::flow::parse_flow_def(&String::from_utf8_lossy(bytes));
        // frontmatter name 为空时用目录名（flows/<dir>/flow.md → <dir>）
        let dir = path
            .trim_start_matches("flows/")
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        let name = if def.name.is_empty() { dir } else { def.name.clone() };
        let privilege = match def.privilege {
            carrier_types::flow::FlowPrivilege::System => "system",
            carrier_types::flow::FlowPrivilege::Agent => "agent",
        };
        flows.push(serde_json::json!({
            "path": path,
            "name": name,
            "description": def.description,
            "tools": def.tools,
            "shell_allow": def.shell_allow,
            "deny_tools": def.deny_tools,
            "privilege": privilege,
            "entry": def.entry.unwrap_or(true),
            "elevates": def.elevates(),
            "max_level": format!("{:?}", def.required_max_tool_level()).to_lowercase(),
        }));
    }

    // 安装期是同一套校验的硬门禁——预览先跑一遍，错误非空则前端禁装。
    let format_errors = carrier_clone::validate_install_format(files).unwrap_or_default();

    serde_json::json!({
        "display_name": display_name,
        "description": description,
        "tags": tags,
        "mcp_servers": mcp_servers,
        "plugins": plugins,
        "flows": flows,
        "format_errors": format_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn extract_preview_golden() {
        let template_json = r#"{
            "version": "3", "name": "demo", "display_name": "演示分身",
            "description": "演示用", "tags": ["测试"],
            "mcp_servers": ["aginxbrowser"], "plugins": []
        }"#;
        let good_flow = "---\nname: writer\ndescription: 写文章的流程说明\ntools:\n  - file_read\n  - shell_exec\nshell_allow:\n  - python3 output/scripts/*\n---\n正文内容";
        let bad_desc_flow = "---\ndescription: \ntools:\n  - file_read\n---\n正文";
        let f = files(&[
            ("template.json", template_json),
            ("flows/writer/flow.md", good_flow),
            ("flows/broken/flow.md", bad_desc_flow),
            ("skills/old/legacy.md", "老布局"),
            ("knowledge/big.md", "不该进预览"),
        ]);
        let v = extract_preview(&f);

        assert_eq!(v["display_name"], "演示分身");
        assert_eq!(v["mcp_servers"][0], "aginxbrowser");
        assert_eq!(v["tags"][0], "测试");

        let flows = v["flows"].as_array().unwrap();
        assert_eq!(flows.len(), 2);
        let writer = flows
            .iter()
            .find(|f| f["name"] == "writer")
            .expect("writer flow");
        assert_eq!(writer["elevates"], true);
        assert_eq!(writer["privilege"], "agent");
        assert_eq!(writer["shell_allow"][0], "python3 output/scripts/*");
        assert_eq!(writer["tools"][1], "shell_exec");

        // 空名字 flow 兜底目录名；空 description 被格式校验抓住
        assert!(flows.iter().any(|f| f["name"] == "broken"));
        let errs = v["format_errors"].as_array().unwrap();
        assert!(
            errs.iter().any(|e| e.as_str().unwrap_or("").contains("broken")),
            "空 description 应产生格式错误: {errs:?}"
        );
        // skills/ 废弃布局���警
        assert!(
            errs.iter().any(|e| e.as_str().unwrap_or("").contains("skills")),
            "skills/ 废弃布局应告警: {errs:?}"
        );
    }

    #[test]
    fn status_in_err_finds_codes() {
        assert_eq!(
            status_in_err("拉取 manifest 失败 demo: 402 Payment Required - body"),
            Some(402)
        );
        assert_eq!(
            status_in_err("拉取文件失败 demo [a.md]: 403 Forbidden - x"),
            Some(403)
        );
        assert_eq!(status_in_err("无法连接 Hub: tcp connect error"), None);
        // body 里带 404 不应误报（": 404" 只在状态位出现）
        assert_eq!(
            status_in_err("拉取 manifest 失败 demo: 401 Unauthorized - err 404 found"),
            Some(401)
        );
    }

    #[test]
    fn classify_maps_all_codes() {
        assert_eq!(classify_hub_err("x: 401 Unauthorized").1, "bad_key");
        assert_eq!(classify_hub_err("x: 402 Payment Required").1, "paid");
        assert_eq!(classify_hub_err("x: 403 Forbidden").1, "private");
        assert_eq!(classify_hub_err("x: 404 Not Found").1, "not_found");
        assert_eq!(classify_hub_err("tcp closed").1, "hub_unreachable");
    }

    #[test]
    fn clone_name_rules() {
        assert!(valid_clone_name("ai-writer"));
        assert!(valid_clone_name("a"));
        assert!(!valid_clone_name("AI_Writer"));
        assert!(!valid_clone_name("-bad"));
        assert!(!valid_clone_name(""));
        assert!(!valid_clone_name(&"x".repeat(65)));
    }

    #[test]
    fn hub_default_url_is_duphub() {
        assert_eq!(
            carrier_types::config::HubConfig::default().url,
            "https://duphub.com"
        );
    }
}
