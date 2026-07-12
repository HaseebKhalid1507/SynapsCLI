//! Provider-local Google Vertex AI OAuth helpers.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_registration_are_distinct_from_gemini_cli() {
        assert_eq!(PROVIDER, "google-vertex");
        let error = VertexRegistration::new(None).unwrap_err();
        assert_eq!(error.code(), "registration_required");
        assert!(!error.to_string().contains("681255809395"));
    }

    #[test]
    fn validates_context_before_network() {
        assert!(VertexContext::new("my-project-123", "us-central1").is_ok());
        for project in ["", "UPPER", "-bad", "bad_", "a"] {
            assert!(VertexContext::new(project, "us-central1").is_err(), "{project}");
        }
        for location in ["", "US-CENTRAL1", "../evil", "us_central1"] {
            assert!(VertexContext::new("my-project-123", location).is_err(), "{location}");
        }
    }

    #[test]
    fn authorization_is_pkce_offline_and_exact_scope() {
        let registration = VertexRegistration::new(Some("synaps.apps.googleusercontent.com")).unwrap();
        let url = build_authorize_url(&registration, "challenge", "state", "http://127.0.0.1:3210/oauth2callback").unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["scope"], CLOUD_PLATFORM_SCOPE);
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["access_type"], "offline");
        assert_eq!(query["state"], "state");
        assert!(!query.contains_key("client_secret"));
    }

    #[test]
    fn refresh_rotation_preserves_omitted_refresh_token() {
        let old = VertexCredentials::new("access-old", "refresh-old", 1);
        let next = parse_token_response(br#"{"access_token":"access-new","expires_in":3600}"#, Some(&old), 10).unwrap();
        assert_eq!(next.refresh_token(), "refresh-old");
        assert_eq!(next.access_token(), "access-new");
        let rotated = parse_token_response(br#"{"access_token":"a","refresh_token":"refresh-new","expires_in":2}"#, Some(&old), 10).unwrap();
        assert_eq!(rotated.refresh_token(), "refresh-new");
    }

    #[test]
    fn only_exact_oauth_endpoints_and_loopback_test_seam_pass() {
        assert!(validate_oauth_endpoint(TOKEN_URL, false).is_ok());
        assert!(validate_oauth_endpoint("https://oauth2.googleapis.com.evil.test/token", false).is_err());
        assert!(validate_oauth_endpoint("http://127.0.0.1:9000/token", true).is_ok());
        assert!(validate_oauth_endpoint("http://10.0.0.1/token", true).is_err());
    }
}
