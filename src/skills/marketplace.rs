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
}
