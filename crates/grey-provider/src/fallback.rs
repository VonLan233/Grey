//! Unified failover chain: provider-level + model-level fallback.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use grey_core::{FallbackConfig, ProviderHealth, ProviderModelRef};

const FAILURE_THRESHOLD: u32 = 3;
const INITIAL_COOLDOWN: Duration = Duration::from_secs(60);
const MAX_COOLDOWN: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Default)]
struct HealthState {
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
}

pub struct FallbackChain {
    provider_order: Vec<String>,
    model_fallbacks: HashMap<ProviderModelRef, Vec<ProviderModelRef>>,
    health: Mutex<HashMap<ProviderModelRef, HealthState>>,
}

impl FallbackChain {
    pub fn from_config(config: &FallbackConfig) -> Self {
        Self::try_from_config(config).expect("invalid fallback provider/model reference")
    }

    pub fn try_from_config(config: &FallbackConfig) -> Result<Self> {
        let mut model_fallbacks = HashMap::new();
        for (key, vals) in &config.models {
            let pmr: ProviderModelRef = key
                .parse()
                .map_err(|error: String| anyhow::anyhow!(error))
                .with_context(|| format!("invalid fallback model key `{key}`"))?;
            let mut fallbacks = Vec::with_capacity(vals.len());
            for value in vals {
                fallbacks.push(value.parse().map_err(|error: String| {
                    anyhow::anyhow!("invalid fallback target `{value}`: {error}")
                })?);
            }
            if !fallbacks.is_empty() {
                model_fallbacks.insert(pmr, fallbacks);
            }
        }

        Ok(Self {
            provider_order: config.providers.clone(),
            model_fallbacks,
            health: Mutex::new(HashMap::new()),
        })
    }

    pub fn resolve(&self, primary: &ProviderModelRef) -> Vec<ProviderModelRef> {
        let mut result = vec![primary.clone()];
        let mut seen: std::collections::HashSet<ProviderModelRef> =
            std::collections::HashSet::new();
        seen.insert(primary.clone());

        if let Some(model_fbs) = self.model_fallbacks.get(primary) {
            for fb in model_fbs {
                if self.is_healthy(fb) && seen.insert(fb.clone()) {
                    result.push(fb.clone());
                }
            }
        }

        let primary_provider = &primary.provider;
        for pid in &self.provider_order {
            if pid == primary_provider {
                continue;
            }
            let candidate = ProviderModelRef::new(pid.clone(), primary.model.clone());
            if self.is_healthy(&candidate) && seen.insert(candidate.clone()) {
                result.push(candidate);
            }
        }
        result
    }

    pub fn empty() -> Self {
        Self {
            provider_order: Vec::new(),
            model_fallbacks: HashMap::new(),
            health: Mutex::new(HashMap::new()),
        }
    }

    pub fn mark_failed(&self, pmr: &ProviderModelRef, _error: &str) {
        let mut health = self.health.lock().unwrap();
        let state = health.entry(pmr.clone()).or_default();
        state.consecutive_failures += 1;
        if state.consecutive_failures >= FAILURE_THRESHOLD {
            let multiplier = 1u32
                .checked_shl((state.consecutive_failures - FAILURE_THRESHOLD).min(10))
                .unwrap_or(1 << 10);
            let cooldown = INITIAL_COOLDOWN
                .checked_mul(multiplier)
                .unwrap_or(MAX_COOLDOWN)
                .min(MAX_COOLDOWN);
            state.cooldown_until = Some(Instant::now() + cooldown);
        }
    }

    pub fn mark_success(&self, pmr: &ProviderModelRef) {
        let mut health = self.health.lock().unwrap();
        if let Some(state) = health.get_mut(pmr) {
            state.consecutive_failures = 0;
            state.cooldown_until = None;
        }
    }

    pub fn is_healthy(&self, pmr: &ProviderModelRef) -> bool {
        let health = self.health.lock().unwrap();
        match health.get(pmr) {
            None => true,
            Some(state) => match state.cooldown_until {
                None => true,
                Some(until) => Instant::now() >= until,
            },
        }
    }

    pub fn healthy_refs<'a>(&self, refs: &'a [ProviderModelRef]) -> Vec<&'a ProviderModelRef> {
        refs.iter().filter(|r| self.is_healthy(r)).collect()
    }
}

impl ProviderHealth for FallbackChain {
    fn is_healthy(&self, reference: &ProviderModelRef) -> bool {
        self.is_healthy(reference)
    }

    fn mark_failed(&self, reference: &ProviderModelRef, error: &str) {
        self.mark_failed(reference, error)
    }

    fn mark_success(&self, reference: &ProviderModelRef) {
        self.mark_success(reference)
    }
}

impl fmt::Debug for FallbackChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FallbackChain")
            .field("provider_order", &self.provider_order)
            .field("model_fallbacks", &self.model_fallbacks)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pmr(provider: &str, model: &str) -> ProviderModelRef {
        ProviderModelRef::new(provider, model)
    }

    #[test]
    fn resolve_returns_primary_first() {
        let chain = FallbackChain::empty();
        let primary = pmr("a", "m1");
        let refs = chain.resolve(&primary);
        assert_eq!(refs, vec![primary]);
    }

    #[test]
    fn resolve_includes_model_fallbacks() {
        let mut config = FallbackConfig::default();
        config.models.insert(
            "a/m1".to_string(),
            vec!["b/m2".to_string(), "c/m3".to_string()],
        );
        let chain = FallbackChain::from_config(&config);
        let refs = chain.resolve(&pmr("a", "m1"));
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0], pmr("a", "m1"));
        assert_eq!(refs[1], pmr("b", "m2"));
        assert_eq!(refs[2], pmr("c", "m3"));
    }

    #[test]
    fn resolve_includes_provider_order_fallbacks() {
        let config = FallbackConfig {
            providers: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        };
        let chain = FallbackChain::from_config(&config);
        let refs = chain.resolve(&pmr("a", "m1"));
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0], pmr("a", "m1"));
        assert_eq!(refs[1], pmr("b", "m1"));
        assert_eq!(refs[2], pmr("c", "m1"));
    }

    #[test]
    fn resolve_deduplicates() {
        let mut models = std::collections::HashMap::new();
        models.insert("a/m1".to_string(), vec!["b/m1".to_string()]);
        let config = FallbackConfig {
            providers: vec!["a".into(), "b".into()],
            models,
        };
        let chain = FallbackChain::from_config(&config);
        let refs = chain.resolve(&pmr("a", "m1"));
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn mark_failed_sets_cooldown_after_threshold() {
        let chain = FallbackChain::empty();
        let pmr = pmr("a", "m1");
        assert!(chain.is_healthy(&pmr));
        chain.mark_failed(&pmr, "err1");
        chain.mark_failed(&pmr, "err2");
        assert!(chain.is_healthy(&pmr));
        chain.mark_failed(&pmr, "err3");
        assert!(!chain.is_healthy(&pmr));
    }

    #[test]
    fn mark_success_resets_health() {
        let chain = FallbackChain::empty();
        let pmr = pmr("a", "m1");
        chain.mark_failed(&pmr, "e1");
        chain.mark_failed(&pmr, "e2");
        chain.mark_failed(&pmr, "e3");
        assert!(!chain.is_healthy(&pmr));
        chain.mark_success(&pmr);
        assert!(chain.is_healthy(&pmr));
    }

    #[test]
    fn unknown_ref_is_healthy() {
        let chain = FallbackChain::empty();
        assert!(chain.is_healthy(&pmr("unknown", "m")));
    }

    #[test]
    fn healthy_refs_filters_unhealthy() {
        let chain = FallbackChain::empty();
        let r1 = pmr("a", "m1");
        let r2 = pmr("b", "m2");
        for _ in 0..3 {
            chain.mark_failed(&r1, "err");
        }
        let refs = vec![r1.clone(), r2.clone()];
        let healthy = chain.healthy_refs(&refs);
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0], &r2);
    }
}
