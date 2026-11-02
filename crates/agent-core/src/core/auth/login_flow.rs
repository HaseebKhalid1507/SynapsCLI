//! Shared plumbing for the browser (authorization code + PKCE) logins of
//! Anthropic and OpenAI Codex: where the flow's progress goes (a
//! [`LoginSink`]), which endpoints it talks to ([`BrowserEndpoints`]) and
//! how it fails ([`LoginFailure`]).
//!
//! The interactive `synaps login` uses [`TerminalSink`], which prints exactly
//! what the flow always printed, opens the browser and offers the stdin
//! paste fallback. `synaps auth connect --json` uses its own sink that never
//! opens a browser, never reads a paste and prints the authorize URL only on
//! its one `open_url` stdout line.

use super::callback::CallbackStartError;
use super::open_browser;

/// Closed classification of a browser-login failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoginFailureKind {
    /// The provider redirected back with an OAuth `error` (user refused).
    Denied,
    /// A callback (or paste) whose `state` is not the one this flow sent.
    StateMismatch,
    /// The fixed callback port could not be bound.
    PortBusy,
    /// The provider answered, but not with something usable.
    ProviderError,
    /// No usable answer from the provider (connect, TLS, timeout).
    Network,
    /// A local failure.
    Internal,
}

/// A browser-login failure: its kind plus the exact message the interactive
/// login has always shown. The message is for terminals only, never JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoginFailure {
    pub(crate) kind: LoginFailureKind,
    message: String,
}

impl LoginFailure {
    pub(crate) fn new(kind: LoginFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(LoginFailureKind::Internal, message)
    }
}

impl From<LoginFailure> for String {
    fn from(f: LoginFailure) -> Self {
        f.message
    }
}

impl From<CallbackStartError> for LoginFailure {
    fn from(e: CallbackStartError) -> Self {
        let kind = match e {
            CallbackStartError::Config(_) => LoginFailureKind::Internal,
            CallbackStartError::Bind(_) => LoginFailureKind::PortBusy,
        };
        Self::new(kind, String::from(e))
    }
}

/// Where a browser login's progress goes.
pub(crate) trait LoginSink: Send {
    /// The callback listener is up and `url` is the authorize URL to visit;
    /// `allow` is the exact authorize endpoint it starts with. An error
    /// ends the flow.
    fn authorize(&mut self, url: &str, allow: &str) -> Result<(), LoginFailure>;
    /// The callback arrived with the expected state; the code exchange
    /// starts now.
    fn exchanging(&mut self);
    /// Whether the flow also reads a pasted redirect URL from stdin.
    fn paste_fallback(&self) -> bool;
}

/// The interactive terminal: today's prompts on stderr, the browser opened
/// for the user, and the paste fallback on stdin.
pub(crate) struct TerminalSink {
    /// The bold first line, e.g. `Opening browser to sign in...`.
    pub(crate) opening: &'static str,
}

impl LoginSink for TerminalSink {
    fn authorize(&mut self, url: &str, _allow: &str) -> Result<(), LoginFailure> {
        eprintln!("\n\x1b[1m{}\x1b[0m\n", self.opening);
        if let Err(e) = open_browser(url) {
            eprintln!("Could not open browser automatically: {}", e);
        }
        eprintln!("\x1b[2mIf the browser didn't open, visit this URL:\x1b[0m");
        eprintln!("\x1b[36m{}\x1b[0m\n", url);
        Ok(())
    }

    fn exchanging(&mut self) {
        eprintln!("\n\x1b[1mExchanging code for tokens...\x1b[0m");
    }

    fn paste_fallback(&self) -> bool {
        true
    }
}

/// Endpoints of one provider's browser login. Production values come from
/// each provider's constants; tests point the token endpoint at a loopback
/// fake and the listener at a free port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BrowserEndpoints {
    /// Exact `scheme://host/path` of the authorize page (the `allow` value).
    pub(crate) authorize_url: String,
    pub(crate) token_url: String,
    pub(crate) callback_host: String,
    pub(crate) callback_port: u16,
    pub(crate) callback_path: String,
    /// The redirect registered with the provider (sent in the authorize URL
    /// and the token request).
    pub(crate) redirect_uri: String,
}

impl BrowserEndpoints {
    /// Anthropic (Claude Pro/Max) production endpoints.
    pub(crate) fn anthropic() -> Self {
        Self {
            authorize_url: super::AUTHORIZE_URL.to_string(),
            token_url: super::TOKEN_URL.to_string(),
            callback_host: super::CALLBACK_HOST.to_string(),
            callback_port: super::CALLBACK_PORT,
            callback_path: "/callback".to_string(),
            redirect_uri: format!("http://localhost:{}/callback", super::CALLBACK_PORT),
        }
    }

    /// OpenAI Codex (ChatGPT) production endpoints.
    pub(crate) fn openai_codex() -> Self {
        super::openai_codex::browser_endpoints()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_authorize_endpoints_are_the_contract_allow_list() {
        // auth-json-v1 §5: the app accepts exactly these `allow` values.
        assert_eq!(
            BrowserEndpoints::anthropic().authorize_url,
            "https://claude.ai/oauth/authorize"
        );
        assert_eq!(
            BrowserEndpoints::openai_codex().authorize_url,
            "https://auth.openai.com/oauth/authorize"
        );
    }

    #[test]
    fn production_callbacks_are_the_fixed_loopback_ports() {
        let a = BrowserEndpoints::anthropic();
        assert_eq!(
            (a.callback_host.as_str(), a.callback_port, a.callback_path.as_str()),
            ("127.0.0.1", 53692, "/callback")
        );
        assert_eq!(a.redirect_uri, "http://localhost:53692/callback");
        let c = BrowserEndpoints::openai_codex();
        assert_eq!(
            (c.callback_host.as_str(), c.callback_port, c.callback_path.as_str()),
            ("127.0.0.1", 1455, "/auth/callback")
        );
        assert_eq!(c.redirect_uri, "http://localhost:1455/auth/callback");
    }

    #[test]
    fn production_anthropic_url_is_unchanged() {
        let ep = BrowserEndpoints::anthropic();
        assert_eq!(
            super::super::pkce::build_auth_url_at(&ep.authorize_url, &ep.redirect_uri, "ch", "st"),
            super::super::build_auth_url("ch", "st", 53692)
        );
    }

    #[test]
    fn bind_failure_is_port_busy_and_keeps_its_message() {
        let f = LoginFailure::from(CallbackStartError::Bind("Failed to bind x".into()));
        assert_eq!(f.kind, LoginFailureKind::PortBusy);
        assert_eq!(String::from(f), "Failed to bind x");
        let f = LoginFailure::from(CallbackStartError::Config("invalid callback path"));
        assert_eq!(f.kind, LoginFailureKind::Internal);
    }
}
