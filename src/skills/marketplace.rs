//! Marketplace URL normalization + metadata fetch.

/// Convert a user-provided URL to the HTTPS URL that yields `marketplace.json`.
/// - `https://github.com/<owner>/<repo>[.git][/]` → raw.githubusercontent.com form.
/// - Any other `https://…` URL is returned unchanged (caller assumed to have pasted a raw URL).
/// - `http://`, `ssh://`, `git@…`, empty → error.
pub fn normalize_marketplace_url(input: &str) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("URL is empty".into());
    }
    if !s.starts_with("https://") {
        return Err("only https:// URLs are supported".into());
    }
    // GitHub rewrite.
    let gh_prefix = "https://github.com/";
    if let Some(rest) = s.strip_prefix(gh_prefix) {
        let rest = rest.trim_end_matches('/');
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err("GitHub URL must be https://github.com/<owner>/<repo>".into());
        }
        return Ok(format!(
            "https://raw.githubusercontent.com/{}/{}/HEAD/.synaps-plugin/marketplace.json",
            parts[0], parts[1]
        ));
    }
    Ok(s.to_string())
}

/// Owner-scoped host key for TOFU trust: `github.com/<owner>`.
/// Returns Err if URL is malformed, not https, or lacks an owner path segment.
pub fn trust_host_for_source(source_url: &str) -> Result<String, String> {
    let s = source_url.trim();
    if !s.starts_with("https://") {
        return Err("only https:// source URLs are supported".into());
    }
    let rest = &s["https://".len()..];
    let (host_raw, path) = rest.split_once('/').ok_or("missing path in URL")?;
    let host = host_raw.strip_prefix("www.").unwrap_or(host_raw);
    let owner = path.split('/').next().ok_or("missing owner in URL")?;
    if owner.is_empty() { return Err("missing owner in URL".into()); }
    Ok(format!("{}/{}", host, owner))
}

/// Is the given source URL's owner-host already trusted?
pub fn is_trusted(source_url: &str, trusted_hosts: &[String]) -> bool {
    match trust_host_for_source(source_url) {
        Ok(h) => trusted_hosts.iter().any(|t| t == &h),
        Err(_) => false,
    }
}

use crate::skills::manifest::MarketplaceManifest;

pub fn validate_manifest(m: &MarketplaceManifest) -> Result<(), String> {
    for p in &m.plugins {
        let s = p.source.trim();
        if s.starts_with("./") || s.starts_with("../") || !s.contains("://") {
            return Err(format!(
                "plugin '{}' uses unsupported relative source path '{}'",
                p.name, s
            ));
        }
        if !s.starts_with("https://") {
            return Err(format!(
                "plugin '{}' source must be https:// (got '{}')",
                p.name, s
            ));
        }
    }
    Ok(())
}

use std::time::Duration;

pub async fn fetch_manifest(url: &str) -> Result<MarketplaceManifest, String> {
    let https_url = if url.starts_with("http://") {
        // only allowed from internal callers; public callers go via normalize.
        return Err("only https:// URLs are supported".into());
    } else {
        url.to_string()
    };
    let body = fetch_raw(&https_url).await?;
    let m: MarketplaceManifest = serde_json::from_str(&body)
        .map_err(|e| format!("invalid marketplace.json: {}", e))?;
    validate_manifest(&m)?;
    Ok(m)
}

/// Low-level: GET the URL and return the body. Used by fetch_manifest and
/// by tests (which need http:// against a local loopback server).
pub(crate) async fn fetch_raw(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("reqwest build: {}", e))?;
    let resp = client.get(url).send().await
        .map_err(|e| format!("failed to fetch: {}", e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("failed to fetch marketplace.json: {}", status));
    }
    resp.text().await.map_err(|e| format!("read body: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_github_url_to_raw() {
        let out = normalize_marketplace_url("https://github.com/maha-media/pi-skills").unwrap();
        assert_eq!(out, "https://raw.githubusercontent.com/maha-media/pi-skills/HEAD/.synaps-plugin/marketplace.json");
    }

    #[test]
    fn normalize_github_url_with_git_suffix() {
        let out = normalize_marketplace_url("https://github.com/a/b.git").unwrap();
        assert_eq!(out, "https://raw.githubusercontent.com/a/b/HEAD/.synaps-plugin/marketplace.json");
    }

    #[test]
    fn normalize_github_url_with_trailing_slash() {
        let out = normalize_marketplace_url("https://github.com/a/b/").unwrap();
        assert_eq!(out, "https://raw.githubusercontent.com/a/b/HEAD/.synaps-plugin/marketplace.json");
    }

    #[test]
    fn normalize_non_github_url_passes_through() {
        let raw = "https://example.com/some/path/marketplace.json";
        assert_eq!(normalize_marketplace_url(raw).unwrap(), raw);
    }

    #[test]
    fn reject_http_url() {
        let err = normalize_marketplace_url("http://github.com/a/b").unwrap_err();
        assert!(err.contains("https"));
    }

    #[test]
    fn reject_empty_url() {
        assert!(normalize_marketplace_url("").is_err());
    }

    #[test]
    fn reject_ssh_url() {
        assert!(normalize_marketplace_url("git@github.com:a/b.git").is_err());
    }

    #[test]
    fn derive_trust_host_from_github_url() {
        assert_eq!(
            trust_host_for_source("https://github.com/maha-media/pi-web.git").unwrap(),
            "github.com/maha-media"
        );
    }

    #[test]
    fn derive_trust_host_from_gitlab_url() {
        assert_eq!(
            trust_host_for_source("https://gitlab.com/org/repo.git").unwrap(),
            "gitlab.com/org"
        );
    }

    #[test]
    fn derive_trust_host_strips_www() {
        assert_eq!(
            trust_host_for_source("https://www.github.com/x/y").unwrap(),
            "github.com/x"
        );
    }

    #[test]
    fn derive_trust_host_rejects_no_owner() {
        assert!(trust_host_for_source("https://example.com/").is_err());
    }

    #[test]
    fn derive_trust_host_rejects_http() {
        assert!(trust_host_for_source("http://github.com/a/b").is_err());
    }

    #[test]
    fn validate_manifest_accepts_https_sources() {
        let m: crate::skills::manifest::MarketplaceManifest = serde_json::from_str(r#"
            {"name":"x","plugins":[{"name":"p","source":"https://github.com/a/b.git"}]}
        "#).unwrap();
        assert!(validate_manifest(&m).is_ok());
    }

    #[test]
    fn validate_manifest_rejects_relative_source() {
        let m: crate::skills::manifest::MarketplaceManifest = serde_json::from_str(r#"
            {"name":"x","plugins":[{"name":"p","source":"./p"}]}
        "#).unwrap();
        let err = validate_manifest(&m).unwrap_err();
        assert!(err.contains("relative"));
        assert!(err.contains("'p'"));
    }

    #[test]
    fn validate_manifest_rejects_http_source() {
        let m: crate::skills::manifest::MarketplaceManifest = serde_json::from_str(r#"
            {"name":"x","plugins":[{"name":"p","source":"http://x/y"}]}
        "#).unwrap();
        let err = validate_manifest(&m).unwrap_err();
        assert!(err.contains("https"));
    }

    #[test]
    fn validate_manifest_reports_first_bad_entry() {
        let m: crate::skills::manifest::MarketplaceManifest = serde_json::from_str(r#"
            {"name":"x","plugins":[
                {"name":"ok","source":"https://github.com/a/b.git"},
                {"name":"bad","source":"./b"}
            ]}
        "#).unwrap();
        let err = validate_manifest(&m).unwrap_err();
        assert!(err.contains("'bad'"));
    }

    #[tokio::test]
    async fn fetch_marketplace_json_success() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let body = r#"{"name":"mk","plugins":[]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let url = format!("http://127.0.0.1:{}/x", port);
        // Bypass the https-only guard by calling fetch_raw (internal helper).
        let body = fetch_raw(&url).await.unwrap();
        assert!(body.contains(r#""name":"mk""#));
    }

    #[tokio::test]
    async fn fetch_marketplace_json_404_returns_error() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        let url = format!("http://127.0.0.1:{}/x", port);
        let err = fetch_raw(&url).await.unwrap_err();
        assert!(err.contains("404"));
    }
}
