use std::collections::HashMap;
use std::sync::Arc;

use crate::extensions::runtime::process::RegisteredProviderSpec;
use crate::extensions::runtime::ExtensionHandler;

pub struct RegisteredProvider {
    pub plugin_id: String,
    pub provider_id: String,
    pub runtime_id: String,
    pub spec: RegisteredProviderSpec,
    pub handler: Option<Arc<dyn ExtensionHandler>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredProviderSummary {
    pub runtime_id: String,
    pub display_name: String,
    pub models: Vec<String>,
}

#[derive(Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, RegisteredProvider>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        plugin_id: &str,
        spec: RegisteredProviderSpec,
    ) -> Result<String, String> {
        self.register_with_handler(plugin_id, spec, None)
    }

    pub fn register_with_handler(
        &mut self,
        plugin_id: &str,
        spec: RegisteredProviderSpec,
        handler: Option<Arc<dyn ExtensionHandler>>,
    ) -> Result<String, String> {
        let runtime_id = format!("{}:{}", plugin_id, spec.id);
        if self.providers.contains_key(&runtime_id) {
            return Err(format!("provider '{}' is already registered", runtime_id));
        }
        self.providers.insert(runtime_id.clone(), RegisteredProvider {
            plugin_id: plugin_id.to_string(),
            provider_id: spec.id.clone(),
            runtime_id: runtime_id.clone(),
            spec,
            handler,
        });
        Ok(runtime_id)
    }

    pub fn unregister_plugin(&mut self, plugin_id: &str) {
        self.providers.retain(|_, provider| provider.plugin_id != plugin_id);
    }

    pub fn get(&self, runtime_id: &str) -> Option<&RegisteredProvider> {
        self.providers.get(runtime_id)
    }

    pub fn list(&self) -> Vec<&RegisteredProvider> {
        let mut providers: Vec<_> = self.providers.values().collect();
        providers.sort_by(|a, b| a.runtime_id.cmp(&b.runtime_id));
        providers
    }

    pub fn len(&self) -> usize {
        self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn parse_model_id(model: &str) -> Option<(&str, &str, &str)> {
        let mut parts = model.split(':');
        let plugin_id = parts.next()?;
        let provider_id = parts.next()?;
        let model_id = parts.next()?;
        if parts.next().is_some() || plugin_id.is_empty() || provider_id.is_empty() || model_id.is_empty() {
            return None;
        }
        Some((plugin_id, provider_id, model_id))
    }

    pub fn model_runtime_id(plugin_id: &str, provider_id: &str, model_id: &str) -> String {
        format!("{}:{}:{}", plugin_id, provider_id, model_id)
    }

    pub fn summaries(&self) -> Vec<RegisteredProviderSummary> {
        self.list()
            .into_iter()
            .map(|provider| RegisteredProviderSummary {
                runtime_id: provider.runtime_id.clone(),
                display_name: provider.spec.display_name.clone(),
                models: provider
                    .spec
                    .models
                    .iter()
                    .map(|model| Self::model_runtime_id(&provider.plugin_id, &provider.provider_id, &model.id))
                    .collect(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str) -> RegisteredProviderSpec {
        RegisteredProviderSpec {
            id: id.to_string(),
            display_name: "Local".to_string(),
            description: "Local provider".to_string(),
            models: vec![],
            config_schema: None,
        }
    }

    #[test]
    fn register_namespaces_provider_by_plugin() {
        let mut registry = ProviderRegistry::new();
        let id = registry.register("plugin", spec("local")).unwrap();

        assert_eq!(id, "plugin:local");
        assert!(registry.get("plugin:local").is_some());
    }

    #[test]
    fn duplicate_runtime_provider_id_is_rejected() {
        let mut registry = ProviderRegistry::new();
        registry.register("plugin", spec("local")).unwrap();
        let err = registry.register("plugin", spec("local")).unwrap_err();

        assert!(err.contains("already registered"));
    }

    #[test]
    fn unregister_plugin_removes_its_providers_only() {
        let mut registry = ProviderRegistry::new();
        registry.register("one", spec("local")).unwrap();
        registry.register("two", spec("local")).unwrap();

        registry.unregister_plugin("one");

        assert!(registry.get("one:local").is_none());
        assert!(registry.get("two:local").is_some());
    }

    #[test]
    fn model_ids_use_three_part_namespace() {
        assert_eq!(
            ProviderRegistry::parse_model_id("plugin:local:model-a"),
            Some(("plugin", "local", "model-a"))
        );
        assert_eq!(ProviderRegistry::model_runtime_id("plugin", "local", "model-a"), "plugin:local:model-a");
        assert!(ProviderRegistry::parse_model_id("plugin:local").is_none());
        assert!(ProviderRegistry::parse_model_id("plugin:local:model:extra").is_none());
    }
}
