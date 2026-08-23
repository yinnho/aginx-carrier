//! `.env` 文件行级 upsert——设置页写 API key 用。
//!
//! 读-改-写保留其他行；key 立即 `set_var` 进进程（brain 驱动读 env）。
//! dotenv 启动时已加载过一次，后续以文件为准。

use std::io::Write;
use std::path::Path;

/// 环境变量名合法性：`[A-Z_][A-Z0-9_]*`。
pub fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// 把 `NAME=value` 写进 .env（已存在则原位替换，否则追加）。保留其余行与注释。
pub fn upsert_env_line(path: &Path, name: &str, value: &str) -> std::io::Result<()> {
    if !valid_env_name(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid env name {name:?}"),
        ));
    }
    let mut lines: Vec<String> = match std::fs::read_to_string(path) {
        Ok(content) => content
            .lines()
            .map(|l| l.to_string())
            .filter(|l| !l.starts_with(&format!("{name}=")))
            .collect(),
        Err(_) => Vec::new(),
    };
    lines.push(format!("{name}={value}"));
    let mut f = std::fs::File::create(path)?;
    for line in &lines {
        writeln!(f, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_replaces_preserves() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(".env");

        // 新建
        upsert_env_line(&p, "API_KEY", "v1").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "API_KEY=v1\n");

        // 替换 + 保留他行
        std::fs::write(&p, "# comment\nOTHER=x\nAPI_KEY=old\n").unwrap();
        upsert_env_line(&p, "API_KEY", "new").unwrap();
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(content.contains("# comment\n"));
        assert!(content.contains("OTHER=x\n"));
        assert!(!content.contains("old"));
        assert!(content.ends_with("API_KEY=new\n"));
    }

    #[test]
    fn rejects_bad_names() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(".env");
        assert!(upsert_env_line(&p, "lower", "x").is_err());
        assert!(upsert_env_line(&p, "9NUM", "x").is_err());
        assert!(upsert_env_line(&p, "WITH SPACE", "x").is_err());
        assert!(upsert_env_line(&p, "_OK_1", "x").is_ok());
    }
}
