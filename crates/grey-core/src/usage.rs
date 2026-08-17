//! Per-session token usage tracking and cost estimation.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::{ProviderModelRef, UsageConfig};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostRate {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
}

impl CostRate {
    pub const ZERO: CostRate = CostRate {
        input_per_1m: 0.0,
        output_per_1m: 0.0,
    };

    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        (input_tokens as f64 / 1_000_000.0) * self.input_per_1m
            + (output_tokens as f64 / 1_000_000.0) * self.output_per_1m
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnUsage {
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub cached: bool,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionUsage {
    pub session_id: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub turns: Vec<TurnUsage>,
}

impl SessionUsage {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            ..Default::default()
        }
    }

    fn add_turn(&mut self, turn: TurnUsage) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(turn.input_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(turn.output_tokens);
        self.total_cost_usd += turn.cost_usd;
        self.turns.push(turn);
    }
}

pub struct UsageTracker {
    sessions: Mutex<HashMap<String, SessionUsage>>,
    cost_table: HashMap<String, CostRate>,
}

impl UsageTracker {
    pub fn new(config: &UsageConfig) -> Self {
        let cost_table = build_cost_table(config);
        Self {
            sessions: Mutex::new(HashMap::new()),
            cost_table,
        }
    }

    pub fn record(&self, session_id: &str, mut turn: TurnUsage) {
        let rate = self
            .cost_table
            .get(&turn.model)
            .copied()
            .unwrap_or(CostRate::ZERO);
        if turn.cost_usd == 0.0 && !turn.cached {
            turn.cost_usd = rate.cost(turn.input_tokens, turn.output_tokens);
        }
        let mut sessions = self.sessions.lock().unwrap();
        let entry = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionUsage::new(session_id));
        entry.add_turn(turn);
    }

    pub fn session_usage(&self, session_id: &str) -> Option<SessionUsage> {
        self.sessions.lock().unwrap().get(session_id).cloned()
    }

    pub fn aggregate(&self) -> SessionUsage {
        let sessions = self.sessions.lock().unwrap();
        let mut agg = SessionUsage::new("__aggregate__");
        for session in sessions.values() {
            agg.total_input_tokens = agg
                .total_input_tokens
                .saturating_add(session.total_input_tokens);
            agg.total_output_tokens = agg
                .total_output_tokens
                .saturating_add(session.total_output_tokens);
            agg.total_cost_usd += session.total_cost_usd;
            agg.turns.extend(session.turns.iter().cloned());
        }
        agg
    }

    pub fn format_panel(&self, session_id: &str) -> String {
        match self.session_usage(session_id) {
            Some(usage) => format!(
                "Tokens: {} in / {} out\nCost: ${:.4}\nTurns: {}",
                usage.total_input_tokens,
                usage.total_output_tokens,
                usage.total_cost_usd,
                usage.turns.len()
            ),
            None => format!("No usage recorded for session {session_id}"),
        }
    }

    pub fn persist_json(&self, session_id: &str) -> Option<String> {
        self.session_usage(session_id)
            .and_then(|usage| serde_json::to_string(&usage).ok())
    }

    pub fn load_json(&self, session_id: &str, json: &str) -> Result<(), String> {
        let usage: SessionUsage =
            serde_json::from_str(json).map_err(|e| format!("decoding usage_json: {e}"))?;
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.to_string(), usage);
        Ok(())
    }
}

fn build_cost_table(config: &UsageConfig) -> HashMap<String, CostRate> {
    let mut table = HashMap::new();
    for (model, rate) in &config.cost_per_1m_input {
        let output_rate = config.cost_per_1m_output.get(model).copied().unwrap_or(0.0);
        table.insert(
            model.clone(),
            CostRate {
                input_per_1m: *rate,
                output_per_1m: output_rate,
            },
        );
    }
    for (model, rate) in &config.cost_per_1m_output {
        table
            .entry(model.clone())
            .and_modify(|c| c.output_per_1m = *rate)
            .or_insert_with(|| CostRate {
                input_per_1m: 0.0,
                output_per_1m: *rate,
            });
    }
    table
}

pub fn turn_usage_from_provider_model(
    provider_model: &ProviderModelRef,
    usage: crate::Usage,
    cached: bool,
    timestamp: i64,
) -> TurnUsage {
    TurnUsage {
        provider: provider_model.provider.clone(),
        model: provider_model.model.clone(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cost_usd: 0.0,
        cached,
        timestamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UsageConfig;
    use std::collections::HashMap;

    fn config_with_rates(input: f64, output: f64) -> UsageConfig {
        let mut cost_input = HashMap::new();
        cost_input.insert("model-a".to_string(), input);
        let mut cost_output = HashMap::new();
        cost_output.insert("model-a".to_string(), output);
        UsageConfig {
            track: true,
            cost_per_1m_input: cost_input,
            cost_per_1m_output: cost_output,
        }
    }

    fn turn(model: &str, input: u64, output: u64) -> TurnUsage {
        TurnUsage {
            provider: "p".into(),
            model: model.into(),
            input_tokens: input,
            output_tokens: output,
            cost_usd: 0.0,
            cached: false,
            timestamp: 100,
        }
    }

    #[test]
    fn cost_rate_zero_is_zero() {
        assert_eq!(CostRate::ZERO.cost(1000, 2000), 0.0);
    }

    #[test]
    fn cost_rate_computes_per_million() {
        let rate = CostRate {
            input_per_1m: 5.0,
            output_per_1m: 15.0,
        };
        assert_eq!(rate.cost(1_000_000, 0), 5.0);
        assert_eq!(rate.cost(0, 1_000_000), 15.0);
        assert_eq!(rate.cost(500_000, 500_000), 2.5 + 7.5);
    }

    #[test]
    fn record_accumulates_per_session() {
        let tracker = UsageTracker::new(&config_with_rates(5.0, 15.0));
        tracker.record("s1", turn("model-a", 1000, 2000));
        tracker.record("s1", turn("model-a", 500, 500));

        let usage = tracker.session_usage("s1").unwrap();
        assert_eq!(usage.total_input_tokens, 1500);
        assert_eq!(usage.total_output_tokens, 2500);
        assert_eq!(usage.turns.len(), 2);
    }

    #[test]
    fn record_computes_cost_from_table() {
        let tracker = UsageTracker::new(&config_with_rates(5.0, 15.0));
        tracker.record("s1", turn("model-a", 1_000_000, 1_000_000));

        let usage = tracker.session_usage("s1").unwrap();
        assert!((usage.total_cost_usd - 20.0).abs() < 1e-9);
    }

    #[test]
    fn record_with_unknown_model_has_zero_cost() {
        let tracker = UsageTracker::new(&config_with_rates(5.0, 15.0));
        tracker.record("s1", turn("unknown-model", 1_000_000, 1_000_000));

        let usage = tracker.session_usage("s1").unwrap();
        assert_eq!(usage.total_cost_usd, 0.0);
    }

    #[test]
    fn cached_turn_does_not_recompute_cost() {
        let tracker = UsageTracker::new(&config_with_rates(5.0, 15.0));
        let mut cached_turn = turn("model-a", 1_000_000, 0);
        cached_turn.cached = true;
        cached_turn.cost_usd = 0.42;
        tracker.record("s1", cached_turn);

        let usage = tracker.session_usage("s1").unwrap();
        assert!((usage.total_cost_usd - 0.42).abs() < 1e-9);
    }

    #[test]
    fn aggregate_sums_across_sessions() {
        let tracker = UsageTracker::new(&config_with_rates(5.0, 15.0));
        tracker.record("s1", turn("model-a", 1000, 2000));
        tracker.record("s2", turn("model-a", 500, 500));

        let agg = tracker.aggregate();
        assert_eq!(agg.total_input_tokens, 1500);
        assert_eq!(agg.total_output_tokens, 2500);
        assert_eq!(agg.turns.len(), 2);
    }

    #[test]
    fn format_panel_for_unknown_session() {
        let tracker = UsageTracker::new(&UsageConfig::default());
        let panel = tracker.format_panel("nope");
        assert!(panel.contains("No usage"));
    }

    #[test]
    fn format_panel_for_known_session() {
        let tracker = UsageTracker::new(&config_with_rates(5.0, 15.0));
        tracker.record("s1", turn("model-a", 1000, 2000));
        let panel = tracker.format_panel("s1");
        assert!(panel.contains("1000"));
        assert!(panel.contains("2000"));
        assert!(panel.contains("Turns: 1"));
    }

    #[test]
    fn persist_and_load_json_roundtrip() {
        let tracker = UsageTracker::new(&config_with_rates(5.0, 15.0));
        tracker.record("s1", turn("model-a", 1000, 2000));

        let json = tracker.persist_json("s1").unwrap();
        let tracker2 = UsageTracker::new(&UsageConfig::default());
        tracker2.load_json("s1", &json).unwrap();

        let usage = tracker2.session_usage("s1").unwrap();
        assert_eq!(usage.total_input_tokens, 1000);
        assert_eq!(usage.total_output_tokens, 2000);
        assert_eq!(usage.turns.len(), 1);
    }

    #[test]
    fn build_cost_table_only_input() {
        let mut input = HashMap::new();
        input.insert("model-a".to_string(), 5.0);
        let cfg = UsageConfig {
            track: true,
            cost_per_1m_input: input,
            cost_per_1m_output: HashMap::new(),
        };
        let table = build_cost_table(&cfg);
        let rate = table.get("model-a").unwrap();
        assert_eq!(rate.input_per_1m, 5.0);
        assert_eq!(rate.output_per_1m, 0.0);
    }

    #[test]
    fn build_cost_table_only_output() {
        let mut output = HashMap::new();
        output.insert("model-a".to_string(), 15.0);
        let cfg = UsageConfig {
            track: true,
            cost_per_1m_input: HashMap::new(),
            cost_per_1m_output: output,
        };
        let table = build_cost_table(&cfg);
        let rate = table.get("model-a").unwrap();
        assert_eq!(rate.input_per_1m, 0.0);
        assert_eq!(rate.output_per_1m, 15.0);
    }
}
