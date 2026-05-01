use std::collections::HashMap;

use crate::extensions::runtime::process::RegisteredProviderSpec;

#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredProvider {
    pub plugin_id: String,
    pub provider_id: String,
    pub runtime_id: String,
    pub spec: RegisteredProviderSpec,
}

#[derive(Debug, Default)]
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
        let runtime_id = format!("{}:{}", plugin_id, spec.id);
        if self.providers.contains_key(&runtime_id) {
            return Err(format!("provider '{}' is already registered", runtime_id));
        }
        self.providers.insert(runtime_id.clone(), RegisteredProvider {
            plugin_id: plugin_id.to_string(),
            provider_id: spec.id.clone(),
            runtime_id: runtime_id.clone(),
            spec,
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
}
