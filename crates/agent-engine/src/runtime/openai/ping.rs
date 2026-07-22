//! Model ping / health check.
//!
//! Sends a minimal chat completion (`max_tokens: 1`, message `"hi"`) to each
//! configured model in parallel and classifies the response.

use std::time::{Duration, Instant};

use serde_json::json;

use super::registry;
use super::types::ProviderConfig;
use agent_core::auth::broker;

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

pub async fn ping_model(cfg: &ProviderConfig, provider_key: &str) -> PingResult {
    let body = json!({
        "model": cfg.model,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
        "stream": false,
    });

    let start = Instant::now();
    // Credential applied broker-side; the ping path never sees a key.
    let broker_handle = broker::global_broker();
    let fut = broker_handle.proxy(broker::ProxyRequest {
        provider: cfg.provider.clone(),
        method: broker::ProxyMethod::Post,
        path: "/chat/completions".to_string(),
        body: Some(body),
        stream: false,
        body_bytes: None,
    });

    let status = match tokio::time::timeout(TIMEOUT, fut).await {
        Err(_) => PingStatus::Timeout,
        Ok(Err(_)) => PingStatus::Error,
        Ok(Ok(resp)) => match resp.status {
            200..=299 => PingStatus::Online,
            401 | 403 => PingStatus::Unauthorized,
            404 => PingStatus::NotFound,
            429 => PingStatus::RateLimited,
            _ => PingStatus::Error,
        },
    };

    PingResult {
        provider_key: provider_key.to_string(),
        model_id: cfg.model.clone(),
        status,
        latency_ms: start.elapsed().as_millis() as u64,
    }
}

/// Ping every model of every configured provider in parallel.
/// Results are sent through `tx` as they arrive (not batched).
pub async fn ping_all_configured(
    tx: tokio::sync::mpsc::UnboundedSender<(String, PingStatus, u64)>,
) {
    let specs = registry::providers();
    let mut handles = Vec::new();

    for spec in specs {
        let Some(base_cfg) = registry::resolve_provider_model(spec.key, spec.default_model) else {
            continue;
        };
        for (model_id, _label, _tier) in spec.models {
            let cfg = ProviderConfig {
                base_url: base_cfg.base_url.clone(),
                model: (*model_id).to_string(),
                provider: base_cfg.provider.clone(),
            };
            let key = spec.key.to_string();
            let tx = tx.clone();
            handles.push(tokio::spawn(async move {
                let result = ping_model(&cfg, &key).await;
                let full_key = format!("{}/{}", result.provider_key, result.model_id);
                let _ = tx.send((full_key, result.status, result.latency_ms));
            }));
        }
    }

    for h in handles {
        let _ = h.await;
    }
    // tx drops here — receiver sees None and knows all pings are done
}
