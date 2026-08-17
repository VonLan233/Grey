//! Provider router: resolve provider+model by task type or explicit override.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Result};
use futures_util::stream::BoxStream;
use grey_core::{GreyConfig, Provider, ProviderEvent, ProviderModelRef, RouteRule, TaskKind};

use crate::fallback::FallbackChain;
use crate::{anthropic, mock, openai};

pub struct ProviderRouter {
    providers: HashMap<String, Arc<dyn Provider>>,
    routes: Vec<RouteRule>,
    fallback: FallbackChain,
    default_provider: String,
    default_model: String,
}

impl std::fmt::Debug for ProviderRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRouter")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("routes", &self.routes)
            .field("default_provider", &self.default_provider)
            .field("default_model", &self.default_model)
            .finish()
    }
}

pub struct ResolvedProvider {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub provider_id: String,
    pub fallback_chain: Vec<ProviderModelRef>,
}

impl std::fmt::Debug for ResolvedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedProvider")
            .field("provider_id", &self.provider_id)
            .field("model", &self.model)
            .field("fallback_chain", &self.fallback_chain)
            .finish()
    }
}

impl ProviderRouter {
    pub fn from_config(cfg: &GreyConfig) -> Result<Self> {
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        for entry in &cfg.providers {
            if providers.contains_key(&entry.id) {
                continue;
            }
            let provider: Box<dyn Provider> = match entry.protocol.as_str() {
                "mock" => Box::new(mock::MockProvider::new(entry.id.clone())),
                "openai" => Box::new(openai::OpenAiCompatibleProvider::new_with_usage(
                    entry.base_url.clone(),
                    if entry.api_key.is_empty() {
                        None
                    } else {
                        Some(entry.api_key.clone())
                    },
                    entry.include_usage,
                )?),
                "anthropic" => Box::new(anthropic::AnthropicProvider::new(
                    entry.base_url.clone(),
                    if entry.api_key.is_empty() {
                        None
                    } else {
                        Some(entry.api_key.clone())
                    },
                    if entry.version.is_empty() {
                        "2023-06-01".to_string()
                    } else {
                        entry.version.clone()
                    },
                    if entry.max_tokens == 0 { 4096 } else { entry.max_tokens },
                )?),
                "gemini" => Box::new(crate::gemini::GeminiProvider::new(
                    entry.base_url.clone(),
                    if entry.api_key.is_empty() {
                        None
                    } else {
                        Some(entry.api_key.clone())
                    },
                )?),
                proto => bail!(
                    "unknown protocol `{proto}` for provider `{}`; expected: mock, openai, anthropic",
                    entry.id
                ),
            };
            providers.insert(entry.id.clone(), Arc::from(provider));
        }

        Ok(Self {
            providers,
            routes: cfg.routes.clone(),
            fallback: FallbackChain::from_config(&cfg.fallback),
            default_provider: cfg.default_provider.clone(),
            default_model: cfg.default_model.clone(),
        })
    }

    pub fn resolve(&self, task: &TaskKind) -> Result<ResolvedProvider> {
        if let Some(route) = self.routes.iter().find(|r| r.match_kind == *task) {
            return self.resolve_explicit(&route.provider, &route.model);
        }
        self.resolve_explicit(&self.default_provider, &self.default_model)
    }

    pub fn resolve_explicit(&self, provider_id: &str, model: &str) -> Result<ResolvedProvider> {
        let provider = self
            .providers
            .get(provider_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown provider `{provider_id}`; configured: {}",
                    self.provider_list()
                )
            })?
            .clone();

        let primary = ProviderModelRef::new(provider_id, model);
        let chain = self.fallback.resolve(&primary);

        Ok(ResolvedProvider {
            provider,
            model: model.to_string(),
            provider_id: provider_id.to_string(),
            fallback_chain: chain,
        })
    }

    pub fn provider_list(&self) -> String {
        let mut ids: Vec<&str> = self.providers.keys().map(String::as_str).collect();
        ids.sort();
        ids.join(", ")
    }

    pub fn fallback(&self) -> &FallbackChain {
        &self.fallback
    }

    pub async fn stream_chat<'a>(
        &'a self,
        request: &'a grey_core::ChatRequest,
        resolved: &'a ResolvedProvider,
    ) -> Result<BoxStream<'a, ProviderEvent>> {
        resolved.provider.stream_chat(request).await
    }
}

impl ProviderRouter {
    pub fn list_provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.providers.keys().cloned().collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grey_core::FallbackConfig;

    fn config_with_mock() -> GreyConfig {
        let mut cfg = GreyConfig::default();
        cfg.default_provider = "mock".into();
        cfg.default_model = "test-model".into();
        cfg.providers = vec![grey_core::ProviderEntry {
            id: "mock".into(),
            protocol: "mock".into(),
            ..Default::default()
        }];
        cfg
    }

    #[test]
    fn resolve_default_provider() {
        let router = ProviderRouter::from_config(&config_with_mock()).unwrap();
        let resolved = router.resolve(&TaskKind::Default).unwrap();
        assert_eq!(resolved.provider_id, "mock");
        assert_eq!(resolved.model, "test-model");
    }

    #[test]
    fn resolve_with_route() {
        let mut cfg = config_with_mock();
        cfg.routes = vec![RouteRule {
            match_kind: TaskKind::Planning,
            provider: "mock".into(),
            model: "planning-model".into(),
        }];
        let router = ProviderRouter::from_config(&cfg).unwrap();
        let resolved = router.resolve(&TaskKind::Planning).unwrap();
        assert_eq!(resolved.model, "planning-model");
    }

    #[test]
    fn resolve_explicit_override() {
        let router = ProviderRouter::from_config(&config_with_mock()).unwrap();
        let resolved = router.resolve_explicit("mock", "override-model").unwrap();
        assert_eq!(resolved.model, "override-model");
    }

    #[test]
    fn unknown_provider_is_error() {
        let router = ProviderRouter::from_config(&config_with_mock()).unwrap();
        let err = router.resolve_explicit("nonexistent", "m").unwrap_err();
        assert!(err.to_string().contains("unknown provider"));
    }

    #[test]
    fn unknown_protocol_is_error() {
        let mut cfg = config_with_mock();
        cfg.providers = vec![grey_core::ProviderEntry {
            id: "bad".into(),
            protocol: "unknown-proto".into(),
            ..Default::default()
        }];
        let err = ProviderRouter::from_config(&cfg).unwrap_err();
        assert!(err.to_string().contains("unknown protocol"));
    }

    #[test]
    fn fallback_chain_includes_primary() {
        let router = ProviderRouter::from_config(&config_with_mock()).unwrap();
        let resolved = router.resolve(&TaskKind::Default).unwrap();
        assert!(!resolved.fallback_chain.is_empty());
        assert_eq!(resolved.fallback_chain[0].provider, "mock");
    }

    #[test]
    fn list_provider_ids_sorted() {
        let mut cfg = config_with_mock();
        cfg.providers.push(grey_core::ProviderEntry {
            id: "zebra".into(),
            protocol: "mock".into(),
            ..Default::default()
        });
        cfg.providers.push(grey_core::ProviderEntry {
            id: "alpha".into(),
            protocol: "mock".into(),
            ..Default::default()
        });
        let router = ProviderRouter::from_config(&cfg).unwrap();
        let ids = router.list_provider_ids();
        assert_eq!(ids, vec!["alpha", "mock", "zebra"]);
    }

    #[test]
    fn fallback_from_config_propagates() {
        let mut cfg = config_with_mock();
        cfg.fallback = FallbackConfig {
            providers: vec!["mock".into()],
            models: HashMap::new(),
        };
        let router = ProviderRouter::from_config(&cfg).unwrap();
        let resolved = router.resolve(&TaskKind::Default).unwrap();
        assert!(resolved.fallback_chain.len() >= 1);
    }
}
