//! Google Gemini (Code Assist) runtime — broker-proxied only.
//!
//! Experimental. See docs/google-gemini-oauth-spec.md.

pub mod setup;
pub mod translate;

pub use setup::{
    setup_user, setup_user_with_sleeper, IneligibleTier, LoadCodeAssistResponse,
    OnboardUserRequest, SetupError, TierMetadata, UserData, ONBOARDING_MAX_ATTEMPTS,
    ONBOARDING_POLL_INTERVAL,
};

pub use translate::{
    from_stream_line, translate_generate_content_request, GeminiFunctionCall, GeminiPart,
    GeminiStreamEvent, MAX_INBOUND_LINE_BYTES,
};
