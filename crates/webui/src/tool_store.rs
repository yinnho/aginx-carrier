//! 网关 agent 的添加台账 + 联系人记忆（webui 第三刀，批2 语义）。
//!
//! `~/.aginx/carrier/webui/external-tools.json`：added 清单（哪些网关
//! agent 进了联系人）+ per (agent, sender) 的联系人记忆（claude 真
//! session_id 与消息流水）。会话列表不再本地记——历史会话对话框透传
//! 网关台账 sessions/list（§2.4.1）；点选某条历史 → set_active_session
//! 把续接 id 记到联系人，下轮 prompt 回喂。原子写：tmp + rename；
//! 损坏重置为空（台账丢失只是重新添加，不致命）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredMessage {
    role: String,
    text: String,
    #[serde(default)]
    ts: String,
}

/// 一个联系人的本地记忆：当前续接会话 + 消息流水。
/// 换会话（切历史/新建）只切 session_id，流水线性追加（微信式）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredContact {
    /// agent 侧会话 id（网关收割），下轮回喂续接；None = 下轮开新会话
    session_id: Option<String>,
    messages: Vec<StoredMessage>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreData {
    #[serde(default)]
    added: Vec<String>,
    /// key = "{agent}/{sender}"
    #[serde(default)]
    contacts: HashMap<String, StoredContact>,
}

pub struct ToolStore {
    path: PathBuf,
    data: Mutex<StoreData>,
}

impl ToolStore {
    /// 载入或新建。损坏时重置为空并 warn（不阻断 webui 启动）。
    pub fn load(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| {
                if path.exists() {
                    tracing::warn!(path = %path.display(), "external-tools.json 损坏，已重置");
                }
                StoreData::default()
            });
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    fn persist(&self, data: &StoreData) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.tmp");
        match serde_json::to_string_pretty(data) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&tmp, s).and_then(|_| std::fs::rename(&tmp, &self.path)) {
                    tracing::warn!(error = %e, "external-tools.json 写盘失败");
                }
            }
            Err(e) => tracing::warn!(error = %e, "external-tools.json 序列化失败"),
        }
    }

    pub fn added(&self) -> Vec<String> {
        self.data.lock().unwrap().added.clone()
    }

    pub fn is_added(&self, id: &str) -> bool {
        self.data.lock().unwrap().added.iter().any(|a| a == id)
    }

    pub fn add(&self, id: &str) {
        let mut data = self.data.lock().unwrap();
        if !data.added.iter().any(|a| a == id) {
            data.added.push(id.to_string());
            self.persist(&data);
        }
    }

    /// 移出台账并清掉该工具全部联系人记忆。
    pub fn remove(&self, id: &str) {
        let mut data = self.data.lock().unwrap();
        data.added.retain(|a| a != id);
        let prefix = format!("{id}/");
        data.contacts.retain(|k, _| !k.starts_with(&prefix));
        self.persist(&data);
    }

    fn contact_key(agent: &str, sender: &str) -> String {
        format!("{agent}/{sender}")
    }

    /// 取联系人记忆。返回 (当前续接 session_id, 消息流水)。
    pub fn session(&self, agent: &str, sender: &str) -> (Option<String>, Vec<(String, String)>) {
        let data = self.data.lock().unwrap();
        data.contacts
            .get(&Self::contact_key(agent, sender))
            .map(|c| {
                (
                    c.session_id.clone(),
                    c.messages.iter().map(|m| (m.role.clone(), m.text.clone())).collect(),
                )
            })
            .unwrap_or((None, Vec::new()))
    }

    /// 切续接会话（历史对话框点选 / 新建）。None = 下轮开新会话。
    pub fn set_active_session(&self, agent: &str, sender: &str, sid: Option<&str>) {
        let mut data = self.data.lock().unwrap();
        let entry = data
            .contacts
            .entry(Self::contact_key(agent, sender))
            .or_default();
        entry.session_id = sid.map(String::from);
        self.persist(&data);
    }

    /// 追加一轮消息（user + assistant）并更新续接 session_id（收割到才更新）。
    pub fn append_turn(
        &self,
        agent: &str,
        sender: &str,
        user_text: &str,
        assistant_text: &str,
        session_id: Option<&str>,
    ) {
        let mut data = self.data.lock().unwrap();
        let entry = data
            .contacts
            .entry(Self::contact_key(agent, sender))
            .or_default();
        let ts = chrono_like_now();
        entry.messages.push(StoredMessage {
            role: "user".into(),
            text: user_text.to_string(),
            ts: ts.clone(),
        });
        entry.messages.push(StoredMessage {
            role: "assistant".into(),
            text: assistant_text.to_string(),
            ts,
        });
        if let Some(sid) = session_id {
            entry.session_id = Some(sid.to_string());
        }
        self.persist(&data);
    }
}

/// RFC3339 UTC 时间戳（毫秒精度——同秒消息按插入序展示）。
fn chrono_like_now() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (secs, ms) = (dur.as_secs(), dur.subsec_millis());
    // 1970-01-01T00:00:00.000Z + secs → 手写换算避免引入 chrono 依赖
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil_from_days（Howard Hinnant 算法）
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &std::path::Path) -> ToolStore {
        ToolStore::load(dir.join("external-tools.json"))
    }

    #[test]
    fn round_trip_add_append_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        assert!(!s.is_added("claude"));

        s.add("claude");
        assert!(s.is_added("claude"));
        s.add("claude"); // 幂等
        assert_eq!(s.added(), vec!["claude".to_string()]);

        s.append_turn("claude", "web:u1", "你好", "收到", Some("sid-1"));
        let (sid, msgs) = s.session("claude", "web:u1");
        assert_eq!(sid.as_deref(), Some("sid-1"));
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], ("user".into(), "你好".into()));
        assert_eq!(msgs[1], ("assistant".into(), "收到".into()));

        // 另一 sender 隔离
        let (sid2, msgs2) = s.session("claude", "web:u2");
        assert_eq!(sid2, None);
        assert!(msgs2.is_empty());

        // remove 清台账 + 联系人
        s.remove("claude");
        assert!(!s.is_added("claude"));
        let (sid3, _) = s.session("claude", "web:u1");
        assert_eq!(sid3, None);

        // 重新 load（持久化验证）
        let s2 = store(tmp.path());
        assert!(!s2.is_added("claude"));
    }

    #[test]
    fn set_active_session_switches_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.add("quanyi");
        s.append_turn("quanyi", "web:u1", "第一句", "收到", Some("sid-a"));
        assert_eq!(s.session("quanyi", "web:u1").0.as_deref(), Some("sid-a"));

        // 历史对话框点选旧会话
        s.set_active_session("quanyi", "web:u1", Some("sid-old"));
        assert_eq!(s.session("quanyi", "web:u1").0.as_deref(), Some("sid-old"));

        // 新建会话
        s.set_active_session("quanyi", "web:u1", None);
        assert_eq!(s.session("quanyi", "web:u1").0, None);

        // 未收割到 id 的轮（raw 方言）不动续接 id
        s.set_active_session("quanyi", "web:u1", Some("sid-b"));
        s.append_turn("quanyi", "web:u1", "再问", "答", None);
        assert_eq!(s.session("quanyi", "web:u1").0.as_deref(), Some("sid-b"));
    }

    #[test]
    fn persist_across_reload() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let s = store(tmp.path());
            s.add("copilot");
            s.append_turn("copilot", "web:u2", "q", "a", Some("sid-2"));
        }
        let s2 = store(tmp.path());
        assert!(s2.is_added("copilot"));
        let (sid, msgs) = s2.session("copilot", "web:u2");
        assert_eq!(sid.as_deref(), Some("sid-2"));
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn corrupt_file_resets() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("external-tools.json");
        std::fs::write(&path, "{broken json").unwrap();
        let s = ToolStore::load(path);
        assert!(!s.is_added("claude"));
        // 仍可正常写
        s.add("claude");
        assert!(s.is_added("claude"));
    }

    #[test]
    fn timestamp_shape() {
        let ts = chrono_like_now();
        // 2026-08-24T..:..:...###Z（毫秒精度）
        assert_eq!(ts.len(), 24);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[19..20], ".");
    }
}
