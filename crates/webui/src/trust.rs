//! 请求信任围栏——dsh api-request-trust 的 Rust 裁剪版。
//!
//! 不是鉴权层，是 confused-deputy 围栏：拦恶意网页对 loopback 服务的
//! CSRF / DNS-rebinding。三条检查（POST JSON-only 在 server 层做）：
//! 1. Host 头 hostname 必须 loopback——rebinding 唯一不可伪造的头
//! 2. `sec-fetch-site: cross-site` 一律拒绝
//! 3. Origin 存在时必须与 Host authority 相等（"null" 解析失败即拒）

use axum::http::{HeaderMap, StatusCode};

/// 校验一个请求是否来自可信来源。`listen_host` 预留非回环监听时的
/// allowlist 扩展位；当前只认 loopback。
pub fn verify(headers: &HeaderMap, _listen_host: &str) -> Result<(), StatusCode> {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let hostname = host_authority(host).ok_or(StatusCode::FORBIDDEN)?;
    if !is_loopback_hostname(hostname) {
        return Err(StatusCode::FORBIDDEN);
    }

    if headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("cross-site"))
    {
        return Err(StatusCode::FORBIDDEN);
    }

    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        if origin.eq_ignore_ascii_case("null") {
            return Err(StatusCode::FORBIDDEN);
        }
        let origin_host = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
            .and_then(host_authority)
            .ok_or(StatusCode::FORBIDDEN)?;
        if !origin_host.eq_ignore_ascii_case(hostname) {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    Ok(())
}

/// 取 host:port 的 host 部分（剥端口、剥 IPv6 括号）。
fn host_authority(authority: &str) -> Option<&str> {
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 字面量 [::1]:8703
        rest.split_once(']').map(|(h, _)| h)
    } else if authority.matches(':').count() <= 1 {
        authority.split(':').next()
    } else {
        // 多冒号无括号不是合法 authority
        None
    }
}

fn is_loopback_hostname(hostname: &str) -> bool {
    hostname == "localhost"
        || hostname.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        || hostname == "::1"
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        m
    }

    #[test]
    fn plain_loopback_passes() {
        assert!(verify(&hm(&[("host", "127.0.0.1:8703")]), "127.0.0.1").is_ok());
        assert!(verify(&hm(&[("host", "localhost:8703")]), "127.0.0.1").is_ok());
        assert!(verify(&hm(&[("host", "[::1]:8703")]), "127.0.0.1").is_ok());
        assert!(verify(&hm(&[("host", "127.255.1.2")]), "127.0.0.1").is_ok());
    }

    #[test]
    fn missing_or_non_loopback_host_refused() {
        assert_eq!(verify(&hm(&[]), ""), Err(StatusCode::BAD_REQUEST));
        assert_eq!(
            verify(&hm(&[("host", "evil.example.com:8703")]), ""),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            verify(&hm(&[("host", "192.168.1.5:8703")]), ""),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn cross_site_fetch_metadata_refused() {
        let m = hm(&[("host", "localhost:8703"), ("sec-fetch-site", "cross-site")]);
        assert_eq!(verify(&m, ""), Err(StatusCode::FORBIDDEN));
        let ok = hm(&[("host", "localhost:8703"), ("sec-fetch-site", "same-origin")]);
        assert!(verify(&ok, "").is_ok());
    }

    #[test]
    fn origin_must_match_host() {
        let ok = hm(&[("host", "localhost:8703"), ("origin", "http://localhost:8703")]);
        assert!(verify(&ok, "").is_ok());
        let bad = hm(&[("host", "localhost:8703"), ("origin", "http://evil.com")]);
        assert_eq!(verify(&bad, ""), Err(StatusCode::FORBIDDEN));
        let null_origin = hm(&[("host", "localhost:8703"), ("origin", "null")]);
        assert_eq!(verify(&null_origin, ""), Err(StatusCode::FORBIDDEN));
        // 无 Origin 放行（Host 已绑定请求）
        let no_origin = hm(&[("host", "localhost:8703")]);
        assert!(verify(&no_origin, "").is_ok());
    }
}
