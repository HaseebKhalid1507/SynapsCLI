pub mod anthropic {
    use crate::auth::OAuthCredentials;
    use reqwest::Client;

    pub async fn login() -> Result<OAuthCredentials, String> {
        crate::auth::login().await
    }
    pub async fn refresh(client: &Client, refresh: &str) -> Result<OAuthCredentials, String> {
        crate::auth::refresh_token(client, refresh).await
    }
}

pub mod openai_codex {
    use crate::auth::OAuthCredentials;
    use reqwest::Client;

    pub async fn login() -> Result<OAuthCredentials, String> {
        super::super::openai_codex::login().await
    }
    pub async fn refresh(client: &Client, refresh: &str) -> Result<OAuthCredentials, String> {
        super::super::openai_codex::refresh_token(client, refresh).await
    }
}

pub mod xai {
    use crate::auth::OAuthCredentials;
    use reqwest::Client;
    pub async fn login() -> Result<OAuthCredentials, String> {
        super::super::xai::login().await
    }
    pub async fn refresh(client: &Client, refresh: &str) -> Result<OAuthCredentials, String> {
        super::super::xai::refresh_token(client, refresh).await
    }
}
