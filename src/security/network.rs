use ipnet::IpNet;
use std::net::IpAddr;
use std::sync::{LazyLock, OnceLock};
use url::Url;

static BLOCKED_NETWORKS: LazyLock<Vec<IpNet>> = LazyLock::new(|| {
    vec![
        "0.0.0.0/8".parse().unwrap(),
        "10.0.0.0/8".parse().unwrap(),
        "100.64.0.0/10".parse().unwrap(), // carrier-grade NAT
        "127.0.0.0/8".parse().unwrap(),
        "169.254.0.0/16".parse().unwrap(), // link-local / cloud metadata
        "172.16.0.0/12".parse().unwrap(),
        "192.168.0.0/16".parse().unwrap(),
        "::1/128".parse().unwrap(),
        "fc00::/7".parse().unwrap(),  // unique local
        "fe80::/10".parse().unwrap(), // link-local v6
    ]
});

static ALLOWED_NETWORKS: OnceLock<Vec<IpNet>> = OnceLock::new();

pub fn configure_ssrf_whitelist(whitelist: Vec<String>) {
    let parsed: Vec<IpNet> = whitelist
        .iter()
        .filter_map(|s| {
            s.parse()
                .map_err(|_| log::warn!("Invalid SSRF whitelist entry ignored: '{s}'"))
                .ok()
        })
        .collect();
    ALLOWED_NETWORKS.set(parsed).unwrap_or_else(|_| {
        log::warn!("SSRF whitelist has already been configured and cannot be changed.");
    });
}

/// Returns `true` if the address falls within a blocked (private/reserved) network,
/// unless it is explicitly whitelisted via [`configure_ssrf_whitelist`].
pub fn is_private(addr: IpAddr) -> bool {
    if let Some(allowed) = ALLOWED_NETWORKS.get() {
        if allowed.iter().any(|net| net.contains(&addr)) {
            return false;
        }
    }
    BLOCKED_NETWORKS.iter().any(|net| net.contains(&addr))
}

/// Validate that a URL is safe to fetch: checks scheme, hostname presence, and
/// that all resolved IP addresses are public (not private/internal).
///
/// Returns `(true, "")` when safe, or `(false, reason)` on any violation.
pub async fn validate_url_target(url: &str) -> (bool, String) {
    let parsed = match Url::parse(url) {
        Ok(p) => p,
        Err(e) => return (false, e.to_string()),
    };

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        let scheme = parsed.scheme();
        let label = if scheme.is_empty() { "none" } else { scheme };
        return (false, format!("Only http/https allowed, got '{label}'"));
    }

    let host = match parsed.host_str() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => return (false, "Missing domain".to_string()),
    };

    let lookup_target = format!("{host}:80");
    let addrs = match tokio::net::lookup_host(&*lookup_target).await {
        Ok(a) => a,
        Err(_) => return (false, format!("Cannot resolve hostname: {host}")),
    };

    for addr in addrs {
        let ip = addr.ip();
        if is_private(ip) {
            return (
                false,
                format!("Blocked: {host} resolves to private/internal address {ip}"),
            );
        }
    }

    (true, String::new())
}

/// Validate an already-fetched URL (e.g. after redirect). Only checks the IP, skips DNS.
pub async fn validate_resolved_url(url: &str) -> (bool, String) {
    let parsed = match Url::parse(url) {
        Ok(p) => p,
        Err(e) => return (false, e.to_string()),
    };
    let hostname = match parsed.host_str() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => return (false, "Missing domain".to_string()),
    };
    if let Ok(addr) = hostname.parse::<IpAddr>() {
        if is_private(addr) {
            return (
                false,
                format!("Redirect target is a private address: {addr}"),
            );
        }
        return (true, String::new());
    }
    // hostname is a domain name — resolve it.
    let lookup_target = format!("{hostname}:80");
    let addrs = match tokio::net::lookup_host(&*lookup_target).await {
        Ok(a) => a,
        // Cannot resolve → treat as safe, matching Python's gaierror behaviour.
        Err(_) => return (true, String::new()),
    };
    for addr in addrs {
        let ip = addr.ip();
        if is_private(ip) {
            return (
                false,
                format!("Redirect target {hostname} resolves to private address {ip}"),
            );
        }
    }
    (true, String::new())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    // #[test]
    // fn test_configure_ssrf_whitelist() {
    //     let whitelist = vec!["100.64.0.0/10", "192.168.0.0/16"];
    //     configure_ssrf_whitelist(whitelist.into_iter().map(|s| s.to_string()).collect());
    //     let allowed_networks = ALLOWED_NETWORKS.get().unwrap();
    //     assert_eq!(allowed_networks.len(), 2);
    //     assert!(allowed_networks.contains(&"100.64.0.0/10".parse().unwrap()));
    //     assert!(allowed_networks.contains(&"192.168.0.0/16".parse().unwrap()));
    // }

    #[test]
    fn test_is_private() {
        // Use 10.0.0.1 — in BLOCKED_NETWORKS but never added to any whitelist by other tests.
        let addr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        assert!(is_private(addr));
    }

    #[test]
    fn test_is_not_private() {
        let addr = IpAddr::V4(Ipv4Addr::new(142, 250, 151, 113));
        assert!(!is_private(addr));
    }

    #[test]
    fn test_is_private_whitelisted() {
        // 100.64.0.0/10 is set by test_configure_ssrf_whitelist when tests run
        // together, and by this call when running in isolation. Either way the
        // whitelist contains this range, so the address must not be considered private.
        configure_ssrf_whitelist(vec![
            "100.64.0.0/10".to_string(),
            "192.168.0.0/16".to_string(),
        ]);
        let addr = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 0));
        println!("ALLOWED_NETWORKS: {:?}", ALLOWED_NETWORKS.get().unwrap());
        assert!(!is_private(addr));
    }

    // ── validate_url_target ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_validate_url_invalid_scheme() {
        let (ok, msg) = validate_url_target("ftp://example.com/file").await;
        assert!(!ok);
        assert!(msg.contains("Only http/https allowed"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_validate_url_missing_host() {
        let (ok, msg) = validate_url_target("http:///path").await;
        assert!(!ok);
        println!("msg: {msg}");
        assert!(msg.contains("Cannot resolve hostname"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_validate_url_loopback_blocked() {
        // 127.0.0.1 resolves directly without DNS — always blocked.
        let (ok, msg) = validate_url_target("http://127.0.0.1/").await;
        assert!(!ok);
        assert!(msg.contains("Blocked"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_validate_url_private_range_blocked() {
        let (ok, msg) = validate_url_target("http://10.0.0.1/").await;
        assert!(!ok);
        assert!(msg.contains("Blocked"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_validate_url_invalid_url_string() {
        let (ok, _msg) = validate_url_target("not a url at all").await;
        assert!(!ok);
    }

    #[tokio::test]
    async fn test_validate_resolved_url_literal_private_ip_blocked() {
        let (ok, msg) = validate_resolved_url("http://127.0.0.1/").await;
        assert!(!ok);
        assert!(msg.contains("private address"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_validate_resolved_url_literal_public_ip_allowed() {
        let (ok, msg) = validate_resolved_url("https://google.com").await;
        assert!(ok);
        assert!(msg.is_empty());
    }
}
