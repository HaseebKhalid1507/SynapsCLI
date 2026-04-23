//! Model ping / health check.
//!
//! Sends a minimal chat completion (`max_tokens: 1`, message `"hi"`) to each
//! configured model in parallel and classifies the response.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures::future::join_all;
use serde_json::json;

use super::registry;
use super::types::ProviderConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingStatus {
    Online,
    RateLimited,
    Unauthorized,
    NotFound,
    Error,
    Timeout,
}

impl PingStatus {
    pub fn icon(&self) -> &'static str {
        match self {
            PingStatus::Online => "✅",
            PingStatus::RateLimited => "⏳",
            PingStatus::Unauthorized => "🔒",
            PingStatus::NotFound => "❌",
            PingStatus::Error => "⚠️",
            PingStatus::Timeout => "⌛",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            PingStatus::Online => "online",
            PingStatus::RateLimited => "429 rate limited",
            PingStatus::Unauthorized => "401 unauthorized",
            PingStatus::NotFound => "404 not found",
            PingStatus::Error => "error",
            PingStatus::Timeout => "timeout",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PingResult {
    pub provider_key: String,
    pub model_id: String,
    pub status: PingStatus,
    pub latency_ms: u64,
}

const TIMEOUT: Duration = Duration::from_secs(10);

pub async fn ping_model(
    client: &reqwest::Client,
    cfg: &ProviderConfig,
    provider_key: &str,
) -> PingResult {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = json!({
        "model": cfg.model,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
    });

    let start = Instant::now();
    let fut = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send();

    let status = match tokio::time::timeout(TIMEOUT, fut).await {
        Err(_) => PingStatus::Timeout,
        Ok(Err(_)) => PingStatus::Error,
        Ok(Ok(resp)) => {
            let code = resp.status().as_u16();
            match code {
                200..=299 => PingStatus::Online,
                401 | 403 => PingStatus::Unauthorized,
                404 => PingStatus::NotFound,
                429 => PingStatus::RateLimited,
                _ => PingStatus::Error,
            }
        }
    };

    PingResult {
        provider_key: provider_key.to_string(),
        model_id: cfg.model.clone(),
        status,
        latency_ms: start.elapsed().as_millis() as u64,
    }
}

/// Ping every model of every configured provider in parallel.
pub async fn ping_all_configured(
    client: &reqwest::Client,
    overrides: &BTreeMap<String, String>,
) -> Vec<PingResult> {
    let specs = registry::providers();
    let mut tasks = Vec::new();

    for spec in specs {
        // Skip providers with no resolvable key.
        let Some(base_cfg) = registry::resolve_provider_model(spec.key, spec.default_model, overrides) else {
            continue;
        };
        for (model_id, _label, _tier) in spec.models {
            let cfg = ProviderConfig {
                base_url: base_cfg.base_url.clone(),
                api_key: base_cfg.api_key.clone(),
                model: (*model_id).to_string(),
            };
            let client = client.clone();
            let key = spec.key.to_string();
            tasks.push(async move { ping_model(&client, &cfg, &key).await });
        }
    }

    join_all(tasks).await
}
