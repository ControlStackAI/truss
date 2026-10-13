//! Budget metering and enforcement.
//!
//! A [`BudgetMeter`] is cheap to clone (atomics behind an `Arc`) so the same
//! meter can be shared across a parent run and its sub-agents — spend is counted
//! against one shared pool. The agent loop calls [`BudgetMeter::check`] before and
//! after every model call; exceeding any ceiling aborts the run with
//! [`HarnessError::BudgetExceeded`] rather than silently overspending.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::error::HarnessError;

/// Hard ceilings for a run. `None` means "no limit on this axis".
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct BudgetLimits {
    pub max_total_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_steps: Option<u64>,
}

#[derive(Debug, Default)]
struct Spent {
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    /// USD * 1_000_000, so we can accumulate atomically without floats.
    cost_micros: AtomicU64,
    steps: AtomicU64,
}

/// A point-in-time view of spend, for reporting and escalation.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct BudgetSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub steps: u64,
}

impl BudgetSnapshot {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Shared, clonable budget meter.
#[derive(Clone, Default)]
pub struct BudgetMeter {
    limits: BudgetLimits,
    spent: Arc<Spent>,
    slice_limits: Option<BudgetLimits>,
    slice_spent: Option<Arc<Spent>>,
}

impl BudgetMeter {
    pub fn new(limits: BudgetLimits) -> Self {
        Self {
            limits,
            spent: Arc::new(Spent::default()),
            slice_limits: None,
            slice_spent: None,
        }
    }

    /// Create a child meter that draws from this meter's shared tree-wide spend
    /// while enforcing an additional per-child slice.
    pub fn child_slice(&self, slice_limits: BudgetLimits) -> Self {
        Self {
            limits: self.limits.clone(),
            spent: self.spent.clone(),
            slice_limits: Some(slice_limits),
            slice_spent: Some(Arc::new(Spent::default())),
        }
    }

    /// Record one model call's token usage and cost.
    pub fn record_usage(&self, input_tokens: u64, output_tokens: u64, cost_usd: f64) {
        self.spent
            .input_tokens
            .fetch_add(input_tokens, Ordering::Relaxed);
        self.spent
            .output_tokens
            .fetch_add(output_tokens, Ordering::Relaxed);
        self.spent
            .cost_micros
            .fetch_add((cost_usd * 1_000_000.0) as u64, Ordering::Relaxed);
        if let Some(slice_spent) = &self.slice_spent {
            slice_spent
                .input_tokens
                .fetch_add(input_tokens, Ordering::Relaxed);
            slice_spent
                .output_tokens
                .fetch_add(output_tokens, Ordering::Relaxed);
            slice_spent
                .cost_micros
                .fetch_add((cost_usd * 1_000_000.0) as u64, Ordering::Relaxed);
        }
    }

    /// Record one agent step (one trip through the loop).
    pub fn record_step(&self) {
        record_step_with_limit(&self.spent.steps, self.limits.max_steps);
        if let Some(slice_spent) = &self.slice_spent {
            record_step_with_limit(
                &slice_spent.steps,
                self.slice_limits
                    .as_ref()
                    .and_then(|limits| limits.max_steps),
            );
        }
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        Self::snapshot_spent(&self.spent)
    }

    /// Spend inside this child slice only. Root meters return the root snapshot.
    pub fn slice_snapshot(&self) -> BudgetSnapshot {
        self.slice_spent
            .as_ref()
            .map_or_else(|| self.snapshot(), |spent| Self::snapshot_spent(spent))
    }

    fn snapshot_spent(spent: &Spent) -> BudgetSnapshot {
        BudgetSnapshot {
            input_tokens: spent.input_tokens.load(Ordering::Relaxed),
            output_tokens: spent.output_tokens.load(Ordering::Relaxed),
            cost_usd: spent.cost_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            steps: spent.steps.load(Ordering::Relaxed),
        }
    }

    /// Fraction (0.0–1.0+) of the most-consumed limit, for escalation warnings.
    /// Returns 0.0 when no limits are set.
    pub fn fraction_used(&self) -> f64 {
        let s = self.snapshot();
        let mut frac: f64 = 0.0;
        if let Some(max) = self.limits.max_total_tokens {
            if max > 0 {
                frac = frac.max(s.total_tokens() as f64 / max as f64);
            }
        }
        if let Some(max) = self.limits.max_cost_usd {
            if max > 0.0 {
                frac = frac.max(s.cost_usd / max);
            }
        }
        if let Some(max) = self.limits.max_steps {
            if max > 0 {
                frac = frac.max(s.steps as f64 / max as f64);
            }
        }
        frac
    }

    /// Returns `Err` if any ceiling has been reached.
    pub fn check(&self) -> Result<(), HarnessError> {
        let s = self.snapshot();
        if let Some(max) = self.limits.max_total_tokens {
            if s.total_tokens() >= max {
                return Err(HarnessError::BudgetExceeded(format!(
                    "{} tokens >= limit {}",
                    s.total_tokens(),
                    max
                )));
            }
        }
        if let Some(max) = self.limits.max_cost_usd {
            if s.cost_usd >= max {
                return Err(HarnessError::BudgetExceeded(format!(
                    "${:.4} >= limit ${:.4}",
                    s.cost_usd, max
                )));
            }
        }
        if let Some(max) = self.limits.max_steps {
            if s.steps >= max {
                return Err(HarnessError::BudgetExceeded(format!(
                    "{} steps >= limit {}",
                    s.steps, max
                )));
            }
        }
        if let Some(slice_limits) = &self.slice_limits {
            Self::check_snapshot(self.slice_snapshot(), slice_limits, "child budget slice")?;
        }
        Ok(())
    }

    fn check_snapshot(
        s: BudgetSnapshot,
        limits: &BudgetLimits,
        label: &str,
    ) -> Result<(), HarnessError> {
        if let Some(max) = limits.max_total_tokens {
            if s.total_tokens() >= max {
                return Err(HarnessError::BudgetExceeded(format!(
                    "{label}: {} tokens >= limit {}",
                    s.total_tokens(),
                    max
                )));
            }
        }
        if let Some(max) = limits.max_cost_usd {
            if s.cost_usd >= max {
                return Err(HarnessError::BudgetExceeded(format!(
                    "{label}: ${:.4} >= limit ${:.4}",
                    s.cost_usd, max
                )));
            }
        }
        if let Some(max) = limits.max_steps {
            if s.steps >= max {
                return Err(HarnessError::BudgetExceeded(format!(
                    "{label}: {} steps >= limit {}",
                    s.steps, max
                )));
            }
        }
        Ok(())
    }
}

fn record_step_with_limit(counter: &AtomicU64, limit: Option<u64>) {
    match limit {
        Some(max) => {
            let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current < max).then_some(current + 1)
            });
        }
        None => {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_token_ceiling() {
        let m = BudgetMeter::new(BudgetLimits {
            max_total_tokens: Some(100),
            ..Default::default()
        });
        assert!(m.check().is_ok());
        m.record_usage(60, 50, 0.0);
        assert!(m.check().is_err());
    }

    #[test]
    fn fraction_tracks_largest_axis() {
        let m = BudgetMeter::new(BudgetLimits {
            max_cost_usd: Some(10.0),
            ..Default::default()
        });
        m.record_usage(0, 0, 8.0);
        assert!((m.fraction_used() - 0.8).abs() < 1e-9);
    }
}
