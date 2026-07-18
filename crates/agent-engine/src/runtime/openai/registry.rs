//! Provider registry — catalog of known OpenAI-compatible endpoints.
//!
//! Connection/credential metadata (base URLs, key discovery) is broker-owned
//! (`agent_core::auth::static_providers` / `agent_core::auth::broker`). This
//! module keeps the engine-side model catalog and produces credential-free
//! routing data: no function here returns or accepts an API key.

use super::types::ProviderConfig;
use agent_core::auth::broker;
use serde::Deserialize;

#[derive(Debug)]
pub struct ProviderSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub env_vars: &'static [&'static str],
    pub default_model: &'static str,
    pub models: &'static [(&'static str, &'static str, &'static str)], // (id, label, tier)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModelInfo {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProviderModelsResponse {
    data: Vec<ProviderModelsItem>,
}

#[derive(Debug, Deserialize)]
struct ProviderModelsItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

pub fn parse_provider_models_response(
    body: &str,
) -> Result<Vec<ProviderModelInfo>, serde_json::Error> {
    let response: ProviderModelsResponse = serde_json::from_str(body)?;
    Ok(response
        .data
        .into_iter()
        .filter(|item| !item.id.trim().is_empty())
        .map(|item| ProviderModelInfo {
            id: item.id,
            name: item.name.filter(|name| !name.trim().is_empty()),
        })
        .collect())
}

pub fn providers() -> &'static [ProviderSpec] {
    static PROVIDERS: std::sync::LazyLock<Vec<ProviderSpec>> = std::sync::LazyLock::new(|| {
        vec![
            ProviderSpec {
                key: "groq",
                name: "Groq",
                base_url: "https://api.groq.com/openai/v1",
                env_vars: &["GROQ_API_KEY"],
                default_model: "llama-3.3-70b-versatile",
                models: &[
                    ("llama-3.3-70b-versatile", "Llama 3.3 70B", "S"),
                    ("llama-3.1-8b-instant", "Llama 3.1 8B", "B"),
                    (
                        "meta-llama/llama-4-scout-17b-16e-instruct",
                        "Llama 4 Scout",
                        "A",
                    ),
                    (
                        "meta-llama/llama-4-maverick-17b-128e-instruct",
                        "Llama 4 Maverick",
                        "S",
                    ),
                ],
            },
            ProviderSpec {
                key: "cerebras",
                name: "Cerebras",
                base_url: "https://api.cerebras.ai/v1",
                env_vars: &["CEREBRAS_API_KEY"],
                default_model: "llama3.1-8b",
                models: &[
                    ("qwen-3-235b-a22b-instruct-2507", "Qwen3 235B", "S+"),
                    ("llama3.1-8b", "Llama 3.1 8B", "B"),
                ],
            },
            ProviderSpec {
                key: "nvidia",
                name: "NVIDIA NIM",
                base_url: "https://integrate.api.nvidia.com/v1",
                env_vars: &["NVIDIA_API_KEY"],
                default_model: "meta/llama-3.3-70b-instruct",
                models: &[
                    (
                        "qwen/qwen3-coder-480b-a35b-instruct",
                        "Qwen3 Coder 480B",
                        "S+",
                    ),
                    (
                        "mistralai/mistral-large-3-675b-instruct-2512",
                        "Mistral Large 675B",
                        "A+",
                    ),
                    ("meta/llama-3.3-70b-instruct", "Llama 3.3 70B", "A"),
                    (
                        "meta/llama-4-maverick-17b-128e-instruct",
                        "Llama 4 Maverick",
                        "S",
                    ),
                    ("meta/llama-4-scout-17b-16e-instruct", "Llama 4 Scout", "A"),
                    (
                        "nvidia/llama-3.1-nemotron-ultra-253b-v1",
                        "Nemotron Ultra 253B",
                        "A+",
                    ),
                    (
                        "mistralai/devstral-2-123b-instruct-2512",
                        "Devstral 2 123B",
                        "S+",
                    ),
                    ("minimaxai/minimax-m2.5", "MiniMax M2.5", "S+"),
                    ("stepfun-ai/step-3.5-flash", "Step 3.5 Flash", "S+"),
                ],
            },
            ProviderSpec {
                key: "sambanova",
                name: "SambaNova",
                base_url: "https://api.sambanova.ai/v1",
                env_vars: &["SAMBANOVA_API_KEY"],
                default_model: "Meta-Llama-3.3-70B-Instruct",
                models: &[
                    ("QwQ-32B", "QwQ 32B", "A+"),
                    ("Meta-Llama-3.3-70B-Instruct", "Llama 3.3 70B", "S"),
                    ("Meta-Llama-3.1-8B-Instruct", "Llama 3.1 8B", "B"),
                    ("DeepSeek-R1", "DeepSeek R1", "S+"),
                    ("DeepSeek-R1-Distill-Llama-70B", "R1 Distill 70B", "A"),
                    ("Qwen3-32B", "Qwen3 32B", "A"),
                ],
            },
            ProviderSpec {
                key: "openrouter",
                name: "OpenRouter",
                base_url: "https://openrouter.ai/api/v1",
                env_vars: &["OPENROUTER_API_KEY"],
                default_model: "meta-llama/llama-3.3-70b-instruct",
                models: &[
                    ("qwen/qwen3-coder", "Qwen3 Coder", "S+"),
                    ("meta-llama/llama-3.3-70b-instruct", "Llama 3.3 70B", "S"),
                    ("deepseek/deepseek-chat-v3-0324", "DeepSeek V3", "S"),
                    ("google/gemma-3-27b-it", "Gemma 3 27B", "A"),
                    (
                        "mistralai/mistral-small-3.1-24b-instruct",
                        "Mistral Small 3.1",
                        "A",
                    ),
                ],
            },
            ProviderSpec {
                key: "google",
                name: "Google AI Studio",
                base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
                env_vars: &["GOOGLE_API_KEY"],
                default_model: "gemini-2.5-flash",
                models: &[
                    ("gemini-2.5-flash", "Gemini 2.5 Flash", "A+"),
                    ("gemini-2.0-flash", "Gemini 2.0 Flash", "B+"),
                    ("gemma-3-27b-it", "Gemma 3 27B", "A"),
                ],
            },
            ProviderSpec {
                key: "deepinfra",
                name: "DeepInfra",
                base_url: "https://api.deepinfra.com/v1/openai",
                env_vars: &["DEEPINFRA_API_KEY", "DEEPINFRA_TOKEN"],
                default_model: "meta-llama/Llama-3.3-70B-Instruct",
                models: &[
                    ("meta-llama/Llama-3.3-70B-Instruct", "Llama 3.3 70B", "S"),
                    ("Qwen/Qwen2.5-Coder-32B-Instruct", "Qwen2.5 Coder 32B", "A"),
                    ("deepseek-ai/DeepSeek-V3-0324", "DeepSeek V3", "S"),
                ],
            },
            ProviderSpec {
                key: "huggingface",
                name: "HuggingFace",
                base_url: "https://router.huggingface.co/v1",
                env_vars: &["HUGGINGFACE_API_KEY", "HF_TOKEN"],
                default_model: "meta-llama/Llama-3.3-70B-Instruct",
                models: &[
                    ("meta-llama/Llama-3.3-70B-Instruct", "Llama 3.3 70B", "S"),
                    ("Qwen/Qwen2.5-72B-Instruct", "Qwen2.5 72B", "A"),
                ],
            },
            ProviderSpec {
                key: "fireworks",
                name: "Fireworks AI",
                base_url: "https://api.fireworks.ai/inference/v1",
                env_vars: &["FIREWORKS_API_KEY"],
                default_model: "accounts/fireworks/models/llama-v3p3-70b-instruct",
                models: &[
                    (
                        "accounts/fireworks/models/llama-v3p3-70b-instruct",
                        "Llama 3.3 70B",
                        "S",
                    ),
                    (
                        "accounts/fireworks/models/qwen2p5-coder-32b-instruct",
                        "Qwen2.5 Coder 32B",
                        "A",
                    ),
                ],
            },
            ProviderSpec {
                key: "hyperbolic",
                name: "Hyperbolic",
                base_url: "https://api.hyperbolic.xyz/v1",
                env_vars: &["HYPERBOLIC_API_KEY"],
                default_model: "meta-llama/Llama-3.3-70B-Instruct",
                models: &[
                    ("meta-llama/Llama-3.3-70B-Instruct", "Llama 3.3 70B", "S"),
                    ("Qwen/Qwen2.5-Coder-32B-Instruct", "Qwen2.5 Coder 32B", "A"),
                    ("deepseek-ai/DeepSeek-V3-0324", "DeepSeek V3", "S"),
                ],
            },
            ProviderSpec {
                key: "scaleway",
                name: "Scaleway",
                base_url: "https://api.scaleway.ai/v1",
                env_vars: &["SCALEWAY_API_KEY"],
                default_model: "llama-3.3-70b-instruct",
                models: &[
                    ("llama-3.3-70b-instruct", "Llama 3.3 70B", "S"),
                    ("qwen3-235b-a22b", "Qwen3 235B", "S+"),
                ],
            },
            ProviderSpec {
                key: "siliconflow",
                name: "SiliconFlow",
                base_url: "https://api.siliconflow.cn/v1",
                env_vars: &["SILICONFLOW_API_KEY"],
                default_model: "Qwen/Qwen3-8B",
                models: &[
                    ("Qwen/Qwen3-8B", "Qwen3 8B", "A-"),
                    ("deepseek-ai/DeepSeek-R1", "DeepSeek R1", "S+"),
                ],
            },
            ProviderSpec {
                key: "together",
                name: "Together AI",
                base_url: "https://api.together.xyz/v1",
                env_vars: &["TOGETHER_API_KEY"],
                default_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
                models: &[
                    (
                        "meta-llama/Llama-3.3-70B-Instruct-Turbo",
                        "Llama 3.3 70B",
                        "S",
                    ),
                    ("Qwen/Qwen2.5-Coder-32B-Instruct", "Qwen2.5 Coder 32B", "A"),
                    ("deepseek-ai/DeepSeek-V3", "DeepSeek V3", "S"),
                ],
            },
            ProviderSpec {
                key: "chutes",
                name: "Chutes AI",
                base_url: "https://llm.chutes.ai/v1",
                env_vars: &["CHUTES_API_KEY"],
                default_model: "deepseek-ai/DeepSeek-V3-0324",
                models: &[("deepseek-ai/DeepSeek-V3-0324", "DeepSeek V3", "S")],
            },
            ProviderSpec {
                key: "codestral",
                name: "Codestral (Mistral)",
                base_url: "https://api.mistral.ai/v1",
                env_vars: &["CODESTRAL_API_KEY"],
                default_model: "codestral-latest",
                models: &[("codestral-latest", "Codestral", "B+")],
            },
            ProviderSpec {
                key: "perplexity",
                name: "Perplexity",
                base_url: "https://api.perplexity.ai",
                env_vars: &["PERPLEXITY_API_KEY", "PPLX_API_KEY"],
                default_model: "llama-3.1-sonar-large-128k-online",
                models: &[("llama-3.1-sonar-large-128k-online", "Sonar Large", "A+")],
            },
            ProviderSpec {
                key: "ovhcloud",
                name: "OVHcloud",
                base_url: "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
                env_vars: &["OVH_AI_ENDPOINTS_ACCESS_TOKEN"],
                default_model: "Meta-Llama-3.3-70B-Instruct",
                models: &[
                    ("Meta-Llama-3.3-70B-Instruct", "Llama 3.3 70B", "S"),
                    ("Qwen/QwQ-32B", "QwQ 32B", "A+"),
                ],
            },
        ]
    });
    &PROVIDERS
}

/// Look up a provider by key. Returns routing data only when the broker
/// reports a credential is available; the key itself never leaves the broker.
pub fn resolve_provider(key: &str) -> Option<(ProviderConfig, &'static str)> {
    let specs = providers();
    let spec = specs.iter().find(|s| s.key == key)?;
    if !broker::static_key_configured(spec.key) {
        return None;
    }
    Some((
        ProviderConfig {
            base_url: spec.base_url.to_string(),
            model: spec.default_model.to_string(),
            provider: spec.key.to_string(),
        },
        spec.default_model,
    ))
}

/// Resolve a provider + specific model to credential-free routing data.
pub fn resolve_provider_model(key: &str, model: &str) -> Option<ProviderConfig> {
    // Special case: local provider — dynamic (non-secret) URL configuration.
    if key == "local" {
        return Some(resolve_local(model));
    }
    let specs = providers();
    let spec = specs.iter().find(|s| s.key == key)?;
    if !broker::static_key_configured(spec.key) {
        return None;
    }
    Some(ProviderConfig {
        base_url: spec.base_url.to_string(),
        model: model.to_string(),
        provider: spec.key.to_string(),
    })
}

/// Resolve `"provider/model"` shorthand.
pub fn resolve_shorthand(s: &str) -> Option<ProviderConfig> {
    let (provider_key, model) = s.split_once('/')?;
    resolve_provider_model(provider_key, model)
}

/// Resolve `"openai-codex/model"` shorthand if Codex OAuth is configured.
pub fn resolve_codex_shorthand(s: &str) -> Option<ProviderConfig> {
    let (provider_key, model) = s.split_once('/')?;
    if provider_key != "openai-codex" {
        return None;
    }
    Some(ProviderConfig {
        base_url: "https://chatgpt.com/backend-api".to_string(),
        // Credentials are resolved immediately before the request through the
        // credential broker. Routing metadata never carries secrets.
        model: model.to_string(),
        provider: "openai-codex".to_string(),
    })
}

/// Resolve a local model endpoint (Ollama, LM Studio, vLLM, llama.cpp, etc.)
///
/// URL resolution (non-secret): `provider.local.url` config → `LOCAL_ENDPOINT`
/// env → `http://localhost:11434/v1`. Any optional key stays broker-owned and
/// is applied at request time by the broker proxy.
fn resolve_local(model: &str) -> ProviderConfig {
    ProviderConfig {
        base_url: broker::local_endpoint_url(),
        model: model.to_string(),
        provider: "local".to_string(),
    }
}

pub async fn fetch_provider_models(provider_key: &str) -> Result<Vec<ProviderModelInfo>, String> {
    let spec = providers()
        .iter()
        .find(|spec| spec.key == provider_key)
        .ok_or_else(|| format!("unknown provider: {provider_key}"))?;
    // The broker applies the credential; this path never sees the key.
    let response = broker::global_broker()
        .proxy(broker::ProxyRequest {
            provider: spec.key.to_string(),
            method: broker::ProxyMethod::Get,
            path: "/models".to_string(),
            body: None,
            stream: false,
        })
        .await
        .map_err(|e| e.to_string())?;
    if !(200..300).contains(&response.status) {
        return Err(format!("model list failed: HTTP {}", response.status));
    }
    parse_provider_models_response(&response.body)
        .map_err(|e| format!("failed to parse model list: {e}"))
}

/// List all providers with (non-secret) key status.
pub fn list_providers() -> Vec<(&'static str, &'static str, bool, usize)> {
    providers()
        .iter()
        .map(|s| {
            let has_key = broker::static_key_configured(s.key);
            (s.key, s.name, has_key, s.models.len())
        })
        .collect()
}

/// List models for a provider.
pub fn list_models(key: &str) -> Option<Vec<(&'static str, &'static str, &'static str)>> {
    let specs = providers();
    let spec = specs.iter().find(|s| s.key == key)?;
    Some(spec.models.to_vec())
}

/// Find all providers with a broker-available credential.
pub fn configured_providers() -> Vec<(&'static str, &'static str, &'static str)> {
    providers()
        .iter()
        .filter(|s| broker::static_key_configured(s.key))
        .map(|s| (s.key, s.name, s.default_model))
        .collect()
}

#[cfg(test)]
mod model_list_tests {
    use super::*;

    /// Cross-registry invariant: every engine provider joins onto a broker
    /// static-provider spec with identical connection metadata, so the URL a
    /// route uses is exactly the URL the broker pins for the credential.
    #[test]
    fn engine_registry_matches_broker_static_provider_table() {
        for spec in providers() {
            let core = agent_core::auth::static_provider(spec.key).unwrap_or_else(|| {
                panic!(
                    "engine provider '{}' missing from broker static table",
                    spec.key
                )
            });
            assert_eq!(
                spec.base_url, core.base_url,
                "base_url drift for {}",
                spec.key
            );
            assert_eq!(
                spec.env_vars, core.env_vars,
                "env_vars drift for {}",
                spec.key
            );
            assert_eq!(spec.name, core.name, "name drift for {}", spec.key);
        }
        assert_eq!(
            providers().len(),
            agent_core::auth::static_providers::STATIC_PROVIDERS.len(),
            "broker table and engine catalog must cover the same providers"
        );
    }

    /// Routing data is credential-free by construction: `ProviderConfig` has
    /// no field that could hold a key, and local resolution keeps only the
    /// non-secret endpoint URL.
    #[test]
    fn resolve_local_carries_endpoint_but_no_credential() {
        let cfg = resolve_provider_model("local", "llama3").expect("local always resolves");
        assert_eq!(cfg.provider, "local");
        assert_eq!(cfg.model, "llama3");
        assert!(!cfg.base_url.is_empty());
        let debug = format!("{cfg:?}");
        assert!(
            !debug.to_lowercase().contains("api_key"),
            "no key field may exist: {debug}"
        );
    }

    #[test]
    fn parses_openrouter_models_response() {
        let json = r#"{
            "data": [
                { "id": "qwen/qwen3-coder", "name": "Qwen: Qwen3 Coder" },
                { "id": "openai/gpt-oss-120b" }
            ]
        }"#;

        let models = parse_provider_models_response(json).expect("parse models");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "qwen/qwen3-coder");
        assert_eq!(models[0].name.as_deref(), Some("Qwen: Qwen3 Coder"));
        assert_eq!(models[1].id, "openai/gpt-oss-120b");
        assert_eq!(models[1].name, None);
    }
}
