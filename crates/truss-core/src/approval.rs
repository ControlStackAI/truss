//! Human approval-gate policy.
//!
//! Approval gates are deliberately opt-in. A resolved policy can suspend a
//! durable run before high-risk tools execute or after model spend crosses an
//! operator-selected threshold. The decision itself is still the signed
//! `human_review` ledger entry; this module only decides whether a gate should
//! be raised.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetSnapshot;

/// Signal name used by approval-gate suspensions. The durable signal table is
/// the wake-up mechanism; the signed `human_review` entry remains the authority.
pub const APPROVAL_GATE_SIGNAL: &str = "approval_gate";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ApprovalGatePolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub high_risk_tools: Vec<String>,
    #[serde(default)]
    pub spend_threshold_usd: Option<f64>,
    #[serde(default)]
    pub per_call_spend_threshold_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApprovalGatePolicyLayer {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub high_risk_tools: Option<Vec<String>>,
    #[serde(default)]
    pub spend_threshold_usd: Option<f64>,
    #[serde(default)]
    pub per_call_spend_threshold_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalGateTrigger {
    Tool {
        tool_name: String,
        tool_call_id: String,
        pattern: String,
    },
    Spend {
        spend_threshold_usd: Option<f64>,
        per_call_spend_threshold_usd: Option<f64>,
        cumulative_cost_usd: f64,
        call_cost_usd: f64,
    },
}

impl ApprovalGatePolicy {
    pub fn resolve(
        global: Option<&ApprovalGatePolicyLayer>,
        agent: Option<&ApprovalGatePolicyLayer>,
        contract: Option<&ApprovalGatePolicyLayer>,
    ) -> Self {
        let mut cfg = ApprovalGatePolicy::default();
        for layer in [global, agent, contract].into_iter().flatten() {
            if let Some(enabled) = layer.enabled {
                cfg.enabled = enabled;
            }
            if let Some(tools) = &layer.high_risk_tools {
                cfg.high_risk_tools = tools.clone();
            }
            if let Some(threshold) = layer.spend_threshold_usd {
                cfg.spend_threshold_usd = valid_threshold(threshold);
            }
            if let Some(threshold) = layer.per_call_spend_threshold_usd {
                cfg.per_call_spend_threshold_usd = valid_threshold(threshold);
            }
        }
        cfg
    }

    pub fn should_gate_tool(
        &self,
        tool_name: &str,
        tool_call_id: &str,
    ) -> Option<ApprovalGateTrigger> {
        if !self.enabled {
            return None;
        }
        self.high_risk_tools
            .iter()
            .find(|pattern| tool_pattern_matches(pattern, tool_name))
            .map(|pattern| ApprovalGateTrigger::Tool {
                tool_name: tool_name.to_string(),
                tool_call_id: tool_call_id.to_string(),
                pattern: pattern.clone(),
            })
    }

    pub fn should_gate_spend(
        &self,
        before: &BudgetSnapshot,
        after: &BudgetSnapshot,
        call_cost_usd: f64,
    ) -> Option<ApprovalGateTrigger> {
        if !self.enabled {
            return None;
        }
        let cumulative_crossed = self
            .spend_threshold_usd
            .is_some_and(|threshold| before.cost_usd < threshold && after.cost_usd >= threshold);
        let per_call_crossed = self
            .per_call_spend_threshold_usd
            .is_some_and(|threshold| call_cost_usd >= threshold);
        (cumulative_crossed || per_call_crossed).then_some(ApprovalGateTrigger::Spend {
            spend_threshold_usd: self.spend_threshold_usd,
            per_call_spend_threshold_usd: self.per_call_spend_threshold_usd,
            cumulative_cost_usd: after.cost_usd,
            call_cost_usd,
        })
    }
}

fn valid_threshold(value: f64) -> Option<f64> {
    value.is_finite().then_some(value).filter(|v| *v > 0.0)
}

fn tool_pattern_matches(pattern: &str, tool_name: &str) -> bool {
    if pattern == tool_name || pattern == "*" {
        return true;
    }
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return false;
    };
    tool_name.starts_with(prefix) && tool_name.ends_with(suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_layers_global_agent_contract() {
        let global = ApprovalGatePolicyLayer {
            enabled: Some(true),
            high_risk_tools: Some(vec!["bash".into()]),
            spend_threshold_usd: Some(1.0),
            per_call_spend_threshold_usd: None,
        };
        let agent = ApprovalGatePolicyLayer {
            high_risk_tools: Some(vec!["write_*".into()]),
            ..Default::default()
        };
        let contract = ApprovalGatePolicyLayer {
            spend_threshold_usd: Some(0.25),
            ..Default::default()
        };

        let resolved = ApprovalGatePolicy::resolve(Some(&global), Some(&agent), Some(&contract));
        assert!(resolved.enabled);
        assert_eq!(resolved.high_risk_tools, vec!["write_*"]);
        assert_eq!(resolved.spend_threshold_usd, Some(0.25));
    }

    #[test]
    fn default_off_does_not_gate() {
        let policy = ApprovalGatePolicy {
            high_risk_tools: vec!["bash".into()],
            spend_threshold_usd: Some(0.01),
            ..Default::default()
        };
        assert!(policy.should_gate_tool("bash", "c1").is_none());
        assert!(policy
            .should_gate_spend(
                &BudgetSnapshot::default(),
                &BudgetSnapshot {
                    cost_usd: 1.0,
                    ..Default::default()
                },
                1.0,
            )
            .is_none());
    }
}
