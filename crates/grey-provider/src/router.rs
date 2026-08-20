//! Provider router: resolve provider+model by task type or explicit override.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Result};
use futures_util::{stream::BoxStream, StreamExt};
use grey_core::{
    GreyConfig, Provider, ProviderCandidate, ProviderEvent, ProviderFailure, ProviderFailureKind,
    ProviderModelRef, RouteRule, TaskKind,
};

use crate::fallback::FallbackChain;
use crate::{anthropic, mock, openai, responses};

pub struct ProviderRouter {
    providers: HashMap<String, Arc<dyn Provider>>,
    routes: Vec<RouteRule>,
    fallback: Arc<FallbackChain>,
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
                bail!("duplicate provider id `{}`", entry.id);
            }
            if entry.auth == grey_core::ProviderAuth::ChatgptOauth
                && (entry.protocol != "openai_responses"
                    || !entry.api_key.is_empty()
                    || !entry.base_url.is_empty())
            {
                bail!(
                    "provider `{}` may use chatgpt_oauth only with openai_responses and empty api_key/base_url",
                    entry.id
                );
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
                )?
                .with_response_max_bytes(cfg.runtime.response_max_bytes)),
                "openai_responses" => Box::new(
                    if entry.auth == grey_core::ProviderAuth::ChatgptOauth {
                        responses::ResponsesProvider::new_chatgpt(
                            crate::chatgpt_oauth::ChatgptOauth::new()?,
                        )?
                    } else {
                        responses::ResponsesProvider::new(
                            entry.base_url.clone(),
                            if entry.api_key.is_empty() {
                                None
                            } else {
                                Some(entry.api_key.clone())
                            },
                        )?
                    }
                    .with_response_max_bytes(cfg.runtime.response_max_bytes),
                ),
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
                )?
                .with_response_max_bytes(cfg.runtime.response_max_bytes)),
                "gemini" => Box::new(crate::gemini::GeminiProvider::new(
                    entry.base_url.clone(),
                    if entry.api_key.is_empty() {
                        None
                    } else {
                        Some(entry.api_key.clone())
                    },
                )?
                .with_response_max_bytes(cfg.runtime.response_max_bytes)),
                proto => bail!(
                    "unknown protocol `{proto}` for provider `{}`; expected: mock, openai, openai_responses, anthropic, gemini",
                    entry.id
                ),
            };
            providers.insert(entry.id.clone(), Arc::from(provider));
        }

        Ok(Self {
            providers,
            routes: cfg.routes.clone(),
            fallback: Arc::new(FallbackChain::try_from_config(&cfg.fallback)?),
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

    pub fn resolve_candidates(&self, refs: &[ProviderModelRef]) -> Result<Vec<ProviderCandidate>> {
        refs.iter()
            .map(|reference| {
                let provider = self.providers.get(&reference.provider).ok_or_else(|| {
                    anyhow::anyhow!(
                        "fallback provider `{}` is not configured",
                        reference.provider
                    )
                })?;
                Ok(ProviderCandidate::new_with_id(
                    provider.clone(),
                    reference.provider.clone(),
                    reference.model.clone(),
                ))
            })
            .collect()
    }

    pub fn fallback(&self) -> &FallbackChain {
        &self.fallback
    }

    pub fn fallback_handle(&self) -> Arc<FallbackChain> {
        self.fallback.clone()
    }

    pub async fn stream_chat<'a>(
        &'a self,
        request: &'a grey_core::ChatRequest,
        resolved: &'a ResolvedProvider,
    ) -> Result<BoxStream<'a, ProviderEvent>> {
        let refs = self
            .fallback
            .healthy_refs(&resolved.fallback_chain)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let refs = if refs.is_empty() {
            vec![ProviderModelRef::new(
                resolved.provider_id.clone(),
                resolved.model.clone(),
            )]
        } else {
            refs
        };
        let output = async_stream::stream! {
            let mut last_error = None;
            for reference in refs {
                let Some(provider) = self.providers.get(&reference.provider).cloned() else {
                    yield ProviderEvent::Error(ProviderFailure::new(
                        ProviderFailureKind::Protocol,
                        format!("unknown fallback provider `{}`", reference.provider),
                    ));
                    return;
                };
                let mut candidate_request = request.clone();
                candidate_request.model.clone_from(&reference.model);
                let mut stream = match provider.stream_chat(&candidate_request).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        let failure = ProviderFailure::from_error(error);
                        if failure.allows_retry_or_fallback() {
                            self.fallback.mark_failed(&reference, &failure);
                            last_error = Some(failure);
                            continue;
                        }
                        yield ProviderEvent::Error(failure);
                        return;
                    }
                };
                let mut visible_output = false;
                let mut completed = false;
                let mut should_fallback = false;
                while let Some(event) = stream.next().await {
                    match &event {
                        ProviderEvent::Delta(_) | ProviderEvent::ToolCall(_) => {
                            visible_output = true;
                        }
                        ProviderEvent::Done(_) => {
                            completed = true;
                            self.fallback.mark_success(&reference);
                        }
                        ProviderEvent::Error(failure) => {
                            if visible_output {
                                if failure.allows_retry_or_fallback() {
                                    self.fallback.mark_failed(&reference, failure);
                                }
                                yield event;
                                return;
                            }
                            if failure.allows_retry_or_fallback() {
                                self.fallback.mark_failed(&reference, failure);
                                last_error = Some(failure.clone());
                                should_fallback = true;
                                break;
                            }
                            yield event;
                            return;
                        }
                    }
                    if completed {
                        yield event;
                        return;
                    }
                    if !matches!(event, ProviderEvent::Error(_)) {
                        yield event;
                    }
                }
                if should_fallback {
                    continue;
                }
                let failure = ProviderFailure::new(
                    ProviderFailureKind::Protocol,
                    format!("{reference} stream ended before completion"),
                );
                yield ProviderEvent::Error(failure);
                return;
            }
            yield ProviderEvent::Error(
                last_error.unwrap_or_else(|| ProviderFailure::new(
                    ProviderFailureKind::Protocol,
                    "all provider candidates failed",
                ))
            );
        };
        Ok(Box::pin(output))
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
    use futures_util::stream;
    use grey_core::FallbackConfig;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn config_with_mock() -> GreyConfig {
        GreyConfig {
            default_provider: "mock".into(),
            default_model: "test-model".into(),
            providers: vec![grey_core::ProviderEntry {
                id: "mock".into(),
                protocol: "mock".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
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
    fn selects_responses_only_for_the_exact_protocol_not_the_model_name() {
        let mut cfg = config_with_mock();
        cfg.providers = vec![
            grey_core::ProviderEntry {
                id: "responses".into(),
                protocol: "openai_responses".into(),
                base_url: "https://example.test/v1".into(),
                ..Default::default()
            },
            grey_core::ProviderEntry {
                id: "chat".into(),
                protocol: "openai".into(),
                base_url: "https://example.test/v1".into(),
                ..Default::default()
            },
        ];
        let router = ProviderRouter::from_config(&cfg).unwrap();

        assert_eq!(
            router
                .resolve_explicit("responses", "ordinary-model")
                .unwrap()
                .provider
                .id(),
            "openai_responses"
        );
        assert_eq!(
            router
                .resolve_explicit("chat", "gpt-responses-looking-model")
                .unwrap()
                .provider
                .id(),
            "openai"
        );
    }

    #[test]
    fn chatgpt_oauth_is_confined_to_unoverridden_responses_provider() {
        for (protocol, api_key, base_url) in [
            ("openai", "", ""),
            ("openai_responses", "key", ""),
            ("openai_responses", "", "https://example.test"),
        ] {
            let mut cfg = config_with_mock();
            cfg.providers = vec![grey_core::ProviderEntry {
                id: "oauth".into(),
                protocol: protocol.into(),
                auth: grey_core::ProviderAuth::ChatgptOauth,
                api_key: api_key.into(),
                base_url: base_url.into(),
                ..Default::default()
            }];
            assert!(ProviderRouter::from_config(&cfg).is_err());
        }

        let mut cfg = config_with_mock();
        cfg.providers = vec![grey_core::ProviderEntry {
            id: "oauth".into(),
            protocol: "openai_responses".into(),
            auth: grey_core::ProviderAuth::ChatgptOauth,
            ..Default::default()
        }];
        assert!(ProviderRouter::from_config(&cfg).is_ok());
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
        assert!(!resolved.fallback_chain.is_empty());
    }

    struct FailingThenSuccessProvider {
        pub id: &'static str,
        pub calls: Arc<AtomicUsize>,
        pub events: Vec<grey_core::ProviderEvent>,
    }

    #[async_trait::async_trait]
    impl grey_core::Provider for FailingThenSuccessProvider {
        fn id(&self) -> &str {
            self.id
        }

        async fn stream_chat<'a>(
            &'a self,
            _request: &'a grey_core::ChatRequest,
        ) -> anyhow::Result<futures_util::stream::BoxStream<'a, grey_core::ProviderEvent>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(stream::iter(self.events.clone())))
        }
    }

    fn router_for_events(
        primary_events: Vec<grey_core::ProviderEvent>,
    ) -> (
        ProviderRouter,
        ResolvedProvider,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "primary".into(),
            Arc::new(FailingThenSuccessProvider {
                id: "primary",
                calls: primary_calls.clone(),
                events: primary_events,
            }) as Arc<dyn grey_core::Provider>,
        );
        providers.insert(
            "fallback".into(),
            Arc::new(FailingThenSuccessProvider {
                id: "fallback",
                calls: fallback_calls.clone(),
                events: vec![
                    grey_core::ProviderEvent::Delta("from fallback".into()),
                    grey_core::ProviderEvent::Done(grey_core::Usage {
                        input_tokens: 2,
                        output_tokens: 3,
                    }),
                ],
            }) as Arc<dyn grey_core::Provider>,
        );
        let fallback = Arc::new(
            FallbackChain::try_from_config(&grey_core::FallbackConfig {
                providers: vec!["primary".into(), "fallback".into()],
                models: Default::default(),
            })
            .unwrap(),
        );
        let router = ProviderRouter {
            providers,
            routes: vec![],
            fallback,
            default_provider: "primary".into(),
            default_model: "m".into(),
        };
        let resolved = router.resolve(&TaskKind::Default).unwrap();
        (router, resolved, primary_calls, fallback_calls)
    }

    #[tokio::test]
    async fn stream_chat_falls_back_when_primary_fails_before_visible_output() {
        let (router, resolved, primary_calls, fallback_calls) =
            router_for_events(vec![grey_core::ProviderEvent::Error(
                grey_core::ProviderFailure::new(
                    grey_core::ProviderFailureKind::Server,
                    "simulated provider failure",
                ),
            )]);
        assert_eq!(resolved.provider_id, "primary");
        assert_eq!(resolved.fallback_chain.len(), 2);

        let request = grey_core::ChatRequest::new(
            "m",
            vec![grey_core::ChatMessage::new(grey_core::Role::User, "hello")],
        );
        let stream = router.stream_chat(&request, &resolved).await.unwrap();
        let (text, calls, usage) = grey_core::collect(stream).await.unwrap();

        assert_eq!(text, "from fallback");
        assert_eq!(calls.len(), 0);
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_chat_auth_failure_never_fallback() {
        let (router, resolved, primary_calls, fallback_calls) =
            router_for_events(vec![grey_core::ProviderEvent::Error(
                grey_core::ProviderFailure::new(
                    grey_core::ProviderFailureKind::Auth,
                    "login required",
                ),
            )]);
        let request = grey_core::ChatRequest::new("m", vec![]);
        let stream = router.stream_chat(&request, &resolved).await.unwrap();
        let error = grey_core::collect(stream).await.unwrap_err();

        assert_eq!(
            error
                .downcast_ref::<grey_core::ProviderFailure>()
                .unwrap()
                .kind(),
            grey_core::ProviderFailureKind::Auth
        );
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn repeated_auth_failures_never_poison_health_or_fallback() {
        let (router, resolved, primary_calls, fallback_calls) =
            router_for_events(vec![grey_core::ProviderEvent::Error(
                grey_core::ProviderFailure::new(
                    grey_core::ProviderFailureKind::Auth,
                    "login required",
                ),
            )]);
        let request = grey_core::ChatRequest::new("m", vec![]);

        for _ in 0..4 {
            let stream = router.stream_chat(&request, &resolved).await.unwrap();
            assert!(grey_core::collect(stream).await.is_err());
        }

        assert_eq!(primary_calls.load(Ordering::SeqCst), 4);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stream_chat_visible_output_failure_never_fallback() {
        let (router, resolved, primary_calls, fallback_calls) = router_for_events(vec![
            grey_core::ProviderEvent::Delta("partial".into()),
            grey_core::ProviderEvent::Error(grey_core::ProviderFailure::new(
                grey_core::ProviderFailureKind::Server,
                "server failed",
            )),
        ]);
        let request = grey_core::ChatRequest::new("m", vec![]);
        let stream = router.stream_chat(&request, &resolved).await.unwrap();
        let error = grey_core::collect(stream).await.unwrap_err();

        assert_eq!(
            error
                .downcast_ref::<grey_core::ProviderFailure>()
                .unwrap()
                .kind(),
            grey_core::ProviderFailureKind::Server
        );
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }
}
