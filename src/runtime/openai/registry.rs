//! Provider registry — catalog of known OpenAI-compatible endpoints.
//!
//! Ported from `openai-runtime::registry`, extended to accept a config map
//! override for API keys (checked before env vars).

use super::types::ProviderConfig;
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct ProviderSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub env_vars: &'static [&'static str],
    pub default_model: &'static str,
    pub models: &'static [(&'static str, &'static str, &'static str)], // (id, label, tier)
}

pub fn providers() -> &'static [ProviderSpec] {
    static PROVIDERS: std::sync::LazyLock<Vec<ProviderSpec>> = std::sync::LazyLock::new(|| vec![
        ProviderSpec {
            key: "openai",
            name: "OpenAI",
            base_url: "https://api.openai.com/v1",
            env_vars: &["OPENAI_API_KEY"],
            default_model: "gpt-4.1",
            models: &[
                ("gpt-4.1", "GPT-4.1", "A+"),
                ("gpt-4.1-mini", "GPT-4.1 Mini", "B+"),
                ("gpt-4.1-nano", "GPT-4.1 Nano", "B"),
                ("o3", "o3", "S+"),
                ("o4-mini", "o4-mini", "A+"),
                ("codex-mini", "Codex Mini", "A"),
            ],
        },
        ProviderSpec {
            key: "groq",
            name: "Groq",
            base_url: "https://api.groq.com/openai/v1",
            env_vars: &["GROQ_API_KEY"],
            default_model: "llama-3.3-70b-versatile",
            models: &[
                ("llama-3.3-70b-versatile", "Llama 3.3 70B", "S"),
                ("llama-3.1-8b-instant", "Llama 3.1 8B", "B"),
                ("meta-llama/llama-4-scout-17b-16e-instruct", "Llama 4 Scout", "A"),
                ("meta-llama/llama-4-maverick-17b-128e-instruct", "Llama 4 Maverick", "S"),
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
                ("qwen/qwen3-coder-480b-a35b-instruct", "Qwen3 Coder 480B", "S+"),
                ("mistralai/mistral-large-3-675b-instruct-2512", "Mistral Large 675B", "A+"),
                ("meta/llama-3.3-70b-instruct", "Llama 3.3 70B", "A"),
                ("meta/llama-4-maverick-17b-128e-instruct", "Llama 4 Maverick", "S"),
                ("meta/llama-4-scout-17b-16e-instruct", "Llama 4 Scout", "A"),
                ("nvidia/llama-3.1-nemotron-ultra-253b-v1", "Nemotron Ultra 253B", "A+"),
                ("mistralai/devstral-2-123b-instruct-2512", "Devstral 2 123B", "S+"),
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
                ("mistralai/mistral-small-3.1-24b-instruct", "Mistral Small 3.1", "A"),
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
                ("accounts/fireworks/models/llama-v3p3-70b-instruct", "Llama 3.3 70B", "S"),
                ("accounts/fireworks/models/qwen2p5-coder-32b-instruct", "Qwen2.5 Coder 32B", "A"),
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
                ("meta-llama/Llama-3.3-70B-Instruct-Turbo", "Llama 3.3 70B", "S"),
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
            models: &[
                ("deepseek-ai/DeepSeek-V3-0324", "DeepSeek V3", "S"),
            ],
        },
        ProviderSpec {
            key: "codestral",
            name: "Codestral (Mistral)",
            base_url: "https://api.mistral.ai/v1",
            env_vars: &["CODESTRAL_API_KEY"],
            default_model: "codestral-latest",
            models: &[
                ("codestral-latest", "Codestral", "B+"),
            ],
        },
        ProviderSpec {
            key: "perplexity",
            name: "Perplexity",
            base_url: "https://api.perplexity.ai",
            env_vars: &["PERPLEXITY_API_KEY", "PPLX_API_KEY"],
            default_model: "llama-3.1-sonar-large-128k-online",
            models: &[
                ("llama-3.1-sonar-large-128k-online", "Sonar Large", "A+"),
            ],
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
    ]);
    &PROVIDERS
}

/// Look up a provider by key and resolve its API key (config override first, then env vars).
pub fn resolve_provider(
    key: &str,
    overrides: &BTreeMap<String, String>,
) -> Option<(ProviderConfig, &'static str)> {
    let specs = providers();
    let spec = specs.into_iter().find(|s| s.key == key)?;
    let api_key = resolve_api_key(spec.key, spec.env_vars, overrides)?;
    Some((
        ProviderConfig {
            base_url: spec.base_url.to_string(),
            api_key,
            model: spec.default_model.to_string(),
        },
        spec.default_model,
    ))
}

/// Resolve a provider + specific model.
pub fn resolve_provider_model(
    key: &str,
    model: &str,
    overrides: &BTreeMap<String, String>,
) -> Option<ProviderConfig> {
    // Special case: local provider — dynamic URL from config/env
    if key == "local" {
        return Some(resolve_local(model, overrides));
    }
    let specs = providers();
    let spec = specs.into_iter().find(|s| s.key == key)?;
    let api_key = resolve_api_key(spec.key, spec.env_vars, overrides)?;
    Some(ProviderConfig {
        base_url: spec.base_url.to_string(),
        api_key,
        model: model.to_string(),
    })
}

/// Resolve `"provider/model"` shorthand.
pub fn resolve_shorthand(s: &str, overrides: &BTreeMap<String, String>) -> Option<ProviderConfig> {
    let (provider_key, model) = s.split_once('/')?;
    resolve_provider_model(provider_key, model, overrides)
}

/// Resolve a local model endpoint (Ollama, LM Studio, vLLM, llama.cpp, etc.)
///
/// URL resolution: `provider.local.url` in config → `LOCAL_ENDPOINT` env → `http://localhost:11434/v1`
/// API key: `provider.local` in config → `LOCAL_API_KEY` env → `"local"` (most local servers don't need one)
fn resolve_local(model: &str, overrides: &BTreeMap<String, String>) -> ProviderConfig {
    let base_url = overrides
        .get("local.url")
        .filter(|s| !s.is_empty())
        .cloned()
        .or_else(|| std::env::var("LOCAL_ENDPOINT").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "http://localhost:11434/v1".to_string());

    let api_key = overrides
        .get("local")
        .filter(|s| !s.is_empty())
        .cloned()
        .or_else(|| std::env::var("LOCAL_API_KEY").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "local".to_string());

    ProviderConfig {
        base_url,
        api_key,
        model: model.to_string(),
    }
}

/// List all providers with key status.
pub fn list_providers(
    overrides: &BTreeMap<String, String>,
) -> Vec<(&'static str, &'static str, bool, usize)> {
    providers()
        .into_iter()
        .map(|s| {
            let has_key = resolve_api_key(s.key, s.env_vars, overrides).is_some();
            (s.key, s.name, has_key, s.models.len())
        })
        .collect()
}

/// List models for a provider.
pub fn list_models(key: &str) -> Option<Vec<(&'static str, &'static str, &'static str)>> {
    let specs = providers();
    let spec = specs.into_iter().find(|s| s.key == key)?;
    Some(spec.models.to_vec())
}

/// Find all providers with a resolvable API key.
pub fn configured_providers(
    overrides: &BTreeMap<String, String>,
) -> Vec<(&'static str, &'static str, &'static str)> {
    providers()
        .into_iter()
        .filter_map(|s| {
            resolve_api_key(s.key, s.env_vars, overrides)
                .map(|_| (s.key, s.name, s.default_model))
        })
        .collect()
}

/// Resolve an API key. Config override (keyed by provider `key`) wins over env vars.
fn resolve_api_key(
    provider_key: &str,
    env_vars: &[&str],
    overrides: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(v) = overrides.get(provider_key) {
        if !v.is_empty() {
            return Some(v.clone());
        }
    }
    let from_env = env_vars.iter().find_map(|var| {
        std::env::var(var).ok().filter(|v| !v.is_empty())
    });
    if from_env.is_some() {
        return from_env;
    }

    // For "openai" provider, also check OAuth credentials in auth.json
    if provider_key == "openai" {
        if let Ok(Some(openai_creds)) = crate::auth::load_openai_auth() {
            if openai_creds.auth_type == "oauth" && !openai_creds.access.is_empty() {
                return Some(openai_creds.access.clone());
            }
        }
    }

    None
}
