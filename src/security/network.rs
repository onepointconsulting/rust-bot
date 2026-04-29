use std::net::IpAddr;
use std::sync::{LazyLock, OnceLock};
use ipnet::IpNet;

static BLOCKED_NETWORKS: LazyLock<Vec<IpNet>> = LazyLock::new(|| vec![
    "0.0.0.0/8".parse().unwrap(),
    "10.0.0.0/8".parse().unwrap(),
    "100.64.0.0/10".parse().unwrap(),   // carrier-grade NAT
    "127.0.0.0/8".parse().unwrap(),
    "169.254.0.0/16".parse().unwrap(),  // link-local / cloud metadata
    "172.16.0.0/12".parse().unwrap(),
    "192.168.0.0/16".parse().unwrap(),
    "::1/128".parse().unwrap(),
    "fc00::/7".parse().unwrap(),        // unique local
    "fe80::/10".parse().unwrap(),       // link-local v6
]);

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

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    
    #[test]
    fn test_configure_ssrf_whitelist() {
        let whitelist = vec!["100.64.0.0/10", "192.168.0.0/16"];
        configure_ssrf_whitelist(whitelist.into_iter().map(|s| s.to_string()).collect());
        let allowed_networks = ALLOWED_NETWORKS.get().unwrap();
        assert_eq!(allowed_networks.len(), 2);
        assert!(allowed_networks.contains(&"100.64.0.0/10".parse().unwrap()));
        assert!(allowed_networks.contains(&"192.168.0.0/16".parse().unwrap()));
    }

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
        let addr = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 0));
        configure_ssrf_whitelist(vec!["100.64.0.0/10".to_string()]);
        assert!(!is_private(addr));
    }
}

