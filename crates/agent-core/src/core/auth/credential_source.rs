//! Remote credential source — Option C (task #157, epic #155).
//!
//! A Synaps client can resolve its provider **access token** from a broker over
//! the network instead of the local `auth.json`. This lets many machines share
//! one OAuth credential without copying the secret to each disk.
//!
//! INVARIANT (the whole point — enforced by construction + tests):
//! the `Remote` path NEVER reads or holds a refresh token, NEVER writes
//! `auth.json`, and NEVER refreshes client-side. It only fetches short-lived
//! access tokens from the broker and caches them in memory. The single
//! refresher is the broker (Anthropic rotates the refresh token on every
//! refresh, so exactly one party may refresh).

use std::time::{SystemTime, UNIX_EPOCH};

/// Where a client gets its provider credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// Read + refresh the local `auth.json` (default — unchanged behavior).
    Local,
    /// Fetch short-lived access tokens from a broker over the network.
    Remote {
        /// Broker base URL, e.g. `https://jade.jade:8181` (no trailing slash).
        endpoint: String,
        /// Per-machine bearer presented TO the broker. This is the machine's own
        /// identity, NOT the provider credential.
        machine_token: String,
    },
}

impl CredentialSource {
    /// Build from explicit config values. Returns `Remote` iff a non-empty
    /// endpoint is given; otherwise `Local`. Trailing slashes on the endpoint
    /// are trimmed so callers can join paths uniformly.
    pub fn from_parts(endpoint: Option<String>, machine_token: Option<String>) -> Self {
        match endpoint {
            Some(e) if !e.trim().is_empty() => CredentialSource::Remote {
                endpoint: e.trim().trim_end_matches('/').to_string(),
                machine_token: machine_token.unwrap_or_default().trim().to_string(),
            },
            _ => CredentialSource::Local,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, CredentialSource::Remote { .. })
    }
}

/// An access token as returned by the broker's `GET /token`.
///
/// Deliberately has **no** refresh-token field: a Remote client must never
/// receive or hold one. This is the invariant made structural — there is no
/// place to put a refresh token even if the broker mistakenly sent one.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BrokerToken {
    pub access_token: String,
    /// Absolute expiry, unix-epoch **milliseconds** (matches
    /// `OAuthCredentials.expires`).
    pub expires: u64,
}

/// Current unix time in milliseconds.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// True if `expires_ms` is already past, or within `margin_ms` of now.
///
/// The margin absorbs clock skew + request latency so a client refetches
/// slightly early rather than presenting a token that dies mid-flight.
pub fn is_expired_with_margin(expires_ms: u64, margin_ms: u64) -> bool {
    now_millis().saturating_add(margin_ms) >= expires_ms
}

/// Default refetch margin: 5 minutes (mirrors `is_token_expired`).
pub const DEFAULT_MARGIN_MS: u64 = 5 * 60 * 1000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_local_when_no_endpoint() {
        assert_eq!(CredentialSource::from_parts(None, None), CredentialSource::Local);
        assert_eq!(
            CredentialSource::from_parts(Some("   ".into()), Some("m".into())),
            CredentialSource::Local
        );
    }

    #[test]
    fn from_parts_remote_when_endpoint_set() {
        let s = CredentialSource::from_parts(Some("https://jade.jade:8181".into()), Some("tok".into()));
        assert_eq!(
            s,
            CredentialSource::Remote {
                endpoint: "https://jade.jade:8181".into(),
                machine_token: "tok".into()
            }
        );
        assert!(s.is_remote());
    }

    #[test]
    fn from_parts_trims_trailing_slash_and_whitespace() {
        let s = CredentialSource::from_parts(Some("  https://b/  ".into()), Some("  tok  ".into()));
        assert_eq!(
            s,
            CredentialSource::Remote { endpoint: "https://b".into(), machine_token: "tok".into() }
        );
    }

    #[test]
    fn remote_with_missing_machine_token_defaults_empty() {
        let s = CredentialSource::from_parts(Some("https://b".into()), None);
        assert_eq!(
            s,
            CredentialSource::Remote { endpoint: "https://b".into(), machine_token: String::new() }
        );
    }

    #[test]
    fn local_is_not_remote() {
        assert!(!CredentialSource::Local.is_remote());
    }

    #[test]
    fn expiry_far_future_not_expired() {
        let far = now_millis() + 60 * 60 * 1000; // +1h
        assert!(!is_expired_with_margin(far, DEFAULT_MARGIN_MS));
    }

    #[test]
    fn expiry_past_is_expired() {
        let past = now_millis().saturating_sub(1000);
        assert!(is_expired_with_margin(past, 0));
    }

    #[test]
    fn expiry_within_margin_is_expired() {
        // expires in 2 minutes, margin 5 minutes -> treated as expired (refetch early)
        let soon = now_millis() + 2 * 60 * 1000;
        assert!(is_expired_with_margin(soon, DEFAULT_MARGIN_MS));
        // ...but with a 1-minute margin it is NOT yet expired
        assert!(!is_expired_with_margin(soon, 60 * 1000));
    }

    #[test]
    fn broker_token_deserializes_without_refresh_field() {
        let json = r#"{"access_token":"sk-abc","expires":1750000000000}"#;
        let t: BrokerToken = serde_json::from_str(json).unwrap();
        assert_eq!(t.access_token, "sk-abc");
        assert_eq!(t.expires, 1_750_000_000_000);
    }
}
