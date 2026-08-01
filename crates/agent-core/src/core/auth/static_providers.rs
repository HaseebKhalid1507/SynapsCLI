//! Static-key provider connection metadata, owned by the credential broker.
//!
//! This table is the broker-side source of truth for where a static API key
//! may be discovered (env vars / login config) and where broker-proxied
//! requests for that provider are allowed to go. Runtime code never resolves
//! these secrets itself — it references providers by `key` and the broker
//! applies the credential.
//!
//! The engine-side model catalog (`runtime::openai::registry`) joins on `key`
//! for model listings; base URLs and credential discovery live here so the
//! broker can pin proxy destinations and never accept attacker-supplied URLs
//! for static-key providers.

/// Connection/credential metadata for one static-key provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticProviderSpec {
    /// Canonical provider key (also the login-config `provider.<key>` name).
    pub key: &'static str,
    /// Human-readable name (safe for UI/status output).
    pub name: &'static str,
    /// Pinned HTTPS base URL. Broker-proxied requests are joined onto this;
    /// clients cannot override it for static-key providers.
    pub base_url: &'static str,
    /// Environment variables the broker may discover the key from.
    pub env_vars: &'static [&'static str],
}

/// The local OpenAI-compatible endpoint pseudo-provider (Ollama, LM Studio…).
/// Its endpoint URL is non-secret user configuration; its optional key is
/// still broker-owned.
pub const LOCAL_PROVIDER_KEY: &str = "local";

/// Default URL for the local endpoint when nothing is configured.
pub const LOCAL_DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

/// All known static-key providers. Broker-owned metadata; no secrets here.
pub const STATIC_PROVIDERS: &[StaticProviderSpec] = &[
    StaticProviderSpec {
        key: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        env_vars: &["GROQ_API_KEY"],
    },
    StaticProviderSpec {
        key: "cerebras",
        name: "Cerebras",
        base_url: "https://api.cerebras.ai/v1",
        env_vars: &["CEREBRAS_API_KEY"],
    },
    StaticProviderSpec {
        key: "nvidia",
        name: "NVIDIA NIM",
        base_url: "https://integrate.api.nvidia.com/v1",
        env_vars: &["NVIDIA_API_KEY"],
    },
    StaticProviderSpec {
        key: "sambanova",
        name: "SambaNova",
        base_url: "https://api.sambanova.ai/v1",
        env_vars: &["SAMBANOVA_API_KEY"],
    },
    StaticProviderSpec {
        key: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        env_vars: &["OPENROUTER_API_KEY"],
    },
    StaticProviderSpec {
        key: "google",
        name: "Google AI Studio",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        env_vars: &["GOOGLE_API_KEY"],
    },
    StaticProviderSpec {
        key: "deepinfra",
        name: "DeepInfra",
        base_url: "https://api.deepinfra.com/v1/openai",
        env_vars: &["DEEPINFRA_API_KEY", "DEEPINFRA_TOKEN"],
    },
    StaticProviderSpec {
        key: "huggingface",
        name: "HuggingFace",
        base_url: "https://router.huggingface.co/v1",
        env_vars: &["HUGGINGFACE_API_KEY", "HF_TOKEN"],
    },
    StaticProviderSpec {
        key: "fireworks",
        name: "Fireworks AI",
        base_url: "https://api.fireworks.ai/inference/v1",
        env_vars: &["FIREWORKS_API_KEY"],
    },
    StaticProviderSpec {
        key: "hyperbolic",
        name: "Hyperbolic",
        base_url: "https://api.hyperbolic.xyz/v1",
        env_vars: &["HYPERBOLIC_API_KEY"],
    },
    StaticProviderSpec {
        key: "scaleway",
        name: "Scaleway",
        base_url: "https://api.scaleway.ai/v1",
        env_vars: &["SCALEWAY_API_KEY"],
    },
    StaticProviderSpec {
        key: "siliconflow",
        name: "SiliconFlow",
        base_url: "https://api.siliconflow.cn/v1",
        env_vars: &["SILICONFLOW_API_KEY"],
    },
    StaticProviderSpec {
        key: "together",
        name: "Together AI",
        base_url: "https://api.together.xyz/v1",
        env_vars: &["TOGETHER_API_KEY"],
    },
    StaticProviderSpec {
        key: "chutes",
        name: "Chutes AI",
        base_url: "https://llm.chutes.ai/v1",
        env_vars: &["CHUTES_API_KEY"],
    },
    StaticProviderSpec {
        key: "codestral",
        name: "Codestral (Mistral)",
        base_url: "https://api.mistral.ai/v1",
        env_vars: &["CODESTRAL_API_KEY"],
    },
    StaticProviderSpec {
        key: "perplexity",
        name: "Perplexity",
        base_url: "https://api.perplexity.ai",
        env_vars: &["PERPLEXITY_API_KEY", "PPLX_API_KEY"],
    },
    StaticProviderSpec {
        key: "ovhcloud",
        name: "OVHcloud",
        base_url: "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
        env_vars: &["OVH_AI_ENDPOINTS_ACCESS_TOKEN"],
    },
    StaticProviderSpec {
        key: "inferx",
        name: "InferX",
        base_url: "https://model.inferx.net/endpoints/v1",
        env_vars: &["INFERX_API_KEY"],
    },
    StaticProviderSpec {
        key: "kimi",
        name: "Kimi (Moonshot AI)",
        base_url: "https://api.moonshot.ai/v1",
        env_vars: &["MOONSHOT_API_KEY", "KIMI_API_KEY"],
    },
];

/// Proxy endpoint allowlist for OpenAI-compatible providers (and the local
/// endpoint). The typed broker proxy may only reach these relative paths —
/// a signed proxy request can never be steered at other same-host endpoints
/// (key-management, billing, admin APIs, …).
pub const OPENAI_COMPAT_ALLOWED_PATHS: &[&str] = &["/models", "/chat/completions"];

/// The per-provider proxy path allowlist. Every static provider in the table
/// is OpenAI-compatible today, so they share one catalog; the lookup is
/// per-provider so a future non-uniform provider gets its own list instead of
/// a widened shared one. Unknown providers get an empty allowlist (fail closed).
pub fn allowed_proxy_paths(key: &str) -> &'static [&'static str] {
    if key == LOCAL_PROVIDER_KEY || static_provider(key).is_some() {
        OPENAI_COMPAT_ALLOWED_PATHS
    } else {
        &[]
    }
}

/// Look up a static provider spec by key.
pub fn static_provider(key: &str) -> Option<&'static StaticProviderSpec> {
    STATIC_PROVIDERS.iter().find(|s| s.key == key)
}

/// True if `key` names a static-key provider (not `local`, not OAuth).
pub fn is_static_provider(key: &str) -> bool {
    static_provider(key).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::str::FromStr;

    /// Registry invariant: provider keys are unique.
    #[test]
    fn static_provider_keys_are_unique() {
        let mut seen = HashSet::new();
        for spec in STATIC_PROVIDERS {
            assert!(
                seen.insert(spec.key),
                "duplicate static provider key: {}",
                spec.key
            );
        }
    }

    /// Registry invariant: pinned base URLs are HTTPS — the broker must never
    /// send a bearer key over plaintext to a remote host.
    #[test]
    fn static_provider_base_urls_are_https() {
        for spec in STATIC_PROVIDERS {
            assert!(
                spec.base_url.starts_with("https://"),
                "{} base_url must be https: {}",
                spec.key,
                spec.base_url
            );
        }
    }

    /// Cross-registry invariant: static provider keys never collide with
    /// canonical OAuth provider IDs or the local pseudo-provider.
    #[test]
    fn static_keys_do_not_collide_with_oauth_ids_or_local() {
        for spec in STATIC_PROVIDERS {
            assert!(
                crate::core::auth::provider::OAuthProviderId::from_str(spec.key).is_err(),
                "static provider key {} collides with an OAuth provider id",
                spec.key
            );
            assert_ne!(spec.key, LOCAL_PROVIDER_KEY);
        }
    }

    #[test]
    fn lookup_finds_known_and_rejects_unknown() {
        assert_eq!(static_provider("groq").unwrap().name, "Groq");
        assert!(static_provider("local").is_none());
        assert!(static_provider("anthropic").is_none());
        assert!(!is_static_provider("no-such-provider"));
    }

    /// Allowlist invariant: every proxyable provider gets exactly the
    /// cataloged OpenAI-compatible paths; unknown keys get nothing.
    #[test]
    fn allowed_proxy_paths_fail_closed() {
        for spec in STATIC_PROVIDERS {
            assert_eq!(allowed_proxy_paths(spec.key), OPENAI_COMPAT_ALLOWED_PATHS);
        }
        assert_eq!(
            allowed_proxy_paths(LOCAL_PROVIDER_KEY),
            OPENAI_COMPAT_ALLOWED_PATHS
        );
        assert!(allowed_proxy_paths("anthropic").is_empty());
        assert!(allowed_proxy_paths("no-such-provider").is_empty());
        for path in OPENAI_COMPAT_ALLOWED_PATHS {
            assert!(path.starts_with('/'), "catalog paths are relative: {path}");
        }
    }
}
