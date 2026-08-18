/// Structured usage info returned by a provider fetcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchUsageInfo {
    pub provider: String,
    /// `true` if the provider has a usage API.
    pub supported: bool,
    /// Set when the API call failed.
    pub error: Option<String>,

    /// Usage counters (`None` = not available for this provider).
    pub used: Option<u64>,
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    /// ISO date string, e.g. `"2026-05-01"`.
    pub reset_date: Option<String>,

    /// Tavily-specific breakdown.
    pub search_used: Option<u64>,
    pub extract_used: Option<u64>,
    pub crawl_used: Option<u64>,
}

impl SearchUsageInfo {
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            supported: false,
            error: None,
            used: None,
            limit: None,
            remaining: None,
            reset_date: None,
            search_used: None,
            extract_used: None,
            crawl_used: None,
        }
    }

    /// Human-readable multi-line string for `/status` output.
    pub fn format(&self) -> String {
        let mut lines = vec![format!("🔍 Web Search: {}", self.provider)];

        if !self.supported {
            lines.push("   Usage tracking: not available for this provider".to_string());
            return lines.join("\n");
        }

        if let Some(error) = &self.error {
            lines.push(format!("   Usage: unavailable ({error})"));
            return lines.join("\n");
        }

        match (self.used, self.limit) {
            (Some(used), Some(limit)) => {
                lines.push(format!("   Usage: {used} / {limit} requests"));
            }
            (Some(used), None) => {
                lines.push(format!("   Usage: {used} requests"));
            }
            _ => {}
        }

        let mut breakdown_parts = Vec::new();
        if let Some(search_used) = self.search_used {
            breakdown_parts.push(format!("Search: {search_used}"));
        }
        if let Some(extract_used) = self.extract_used {
            breakdown_parts.push(format!("Extract: {extract_used}"));
        }
        if let Some(crawl_used) = self.crawl_used {
            breakdown_parts.push(format!("Crawl: {crawl_used}"));
        }
        if !breakdown_parts.is_empty() {
            lines.push(format!("   Breakdown: {}", breakdown_parts.join(" | ")));
        }

        if let Some(remaining) = self.remaining {
            lines.push(format!("   Remaining: {remaining} requests"));
        }

        if let Some(reset_date) = &self.reset_date {
            lines.push(format!("   Resets: {reset_date}"));
        }

        lines.join("\n")
    }
}

pub async fn fetch_search_usage(provider: &str, _api_key: Option<&str>) -> SearchUsageInfo {
    let p = if provider.is_empty() {
        "duckduckgo"
    } else {
        provider
    };
    SearchUsageInfo::new(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_provider() {
        let info = SearchUsageInfo::new("duckduckgo");
        assert_eq!(
            info.format(),
            "🔍 Web Search: duckduckgo\n   Usage tracking: not available for this provider"
        );
    }

    #[test]
    fn api_error() {
        let info = SearchUsageInfo {
            supported: true,
            error: Some("401 Unauthorized".to_string()),
            ..SearchUsageInfo::new("brave")
        };
        assert_eq!(
            info.format(),
            "🔍 Web Search: brave\n   Usage: unavailable (401 Unauthorized)"
        );
    }

    #[test]
    fn full_tavily_breakdown() {
        let info = SearchUsageInfo {
            supported: true,
            used: Some(42),
            limit: Some(1000),
            remaining: Some(958),
            reset_date: Some("2026-05-01".to_string()),
            search_used: Some(30),
            extract_used: Some(10),
            crawl_used: Some(2),
            ..SearchUsageInfo::new("tavily")
        };
        let text = info.format();
        assert!(text.contains("Usage: 42 / 1000 requests"));
        assert!(text.contains("Breakdown: Search: 30 | Extract: 10 | Crawl: 2"));
        assert!(text.contains("Remaining: 958 requests"));
        assert!(text.contains("Resets: 2026-05-01"));
    }
}
