//! Context-window pressure measurement.
//!
//! CC-1 is observability-only: the gauge reports how full the model context is
//! for the already-assembled request, but it does not change projection,
//! summarization, or run control.

use std::collections::VecDeque;

const DEFAULT_GROWTH_WINDOW: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextGauge {
    context_window: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextPressure {
    pub tokens: u64,
    pub window: u32,
    pub pressure: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThresholdEstimate {
    pub steps_to_threshold: u64,
}

/// Forward estimator for CC-3 compaction anticipation.
///
/// The durable loop feeds this with the same provider-reported projected input
/// token counts used by [`ContextGauge`]. It keeps a short moving average of
/// positive or flat per-step deltas; it never tokenizes messages itself.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextGrowthEstimator {
    recent_deltas: VecDeque<u64>,
    max_deltas: usize,
    last_tokens: Option<u64>,
}

impl ContextGauge {
    pub fn new(context_window: u32) -> Self {
        Self {
            context_window: context_window.max(1),
        }
    }

    pub fn measure(&self, projected_tokens: u64) -> ContextPressure {
        ContextPressure {
            tokens: projected_tokens,
            window: self.context_window,
            pressure: projected_tokens as f64 / self.context_window as f64,
        }
    }
}

impl Default for ContextGrowthEstimator {
    fn default() -> Self {
        Self::new(DEFAULT_GROWTH_WINDOW)
    }
}

impl ContextGrowthEstimator {
    pub fn new(max_deltas: usize) -> Self {
        Self {
            recent_deltas: VecDeque::new(),
            max_deltas: max_deltas.max(1),
            last_tokens: None,
        }
    }

    pub fn from_projected_tokens(tokens: impl IntoIterator<Item = u64>) -> Self {
        let mut estimator = Self::default();
        for tokens in tokens {
            estimator.observe(tokens);
        }
        estimator
    }

    pub fn observe(&mut self, projected_tokens: u64) {
        if let Some(last_tokens) = self.last_tokens {
            self.push_delta(projected_tokens.saturating_sub(last_tokens));
        }
        self.last_tokens = Some(projected_tokens);
    }

    pub fn estimate(
        &self,
        current_tokens: u64,
        context_window: u32,
        threshold: f64,
    ) -> Option<ThresholdEstimate> {
        let threshold_tokens =
            ((context_window.max(1) as f64) * threshold.clamp(0.0, 1.0)).ceil() as u64;
        if current_tokens >= threshold_tokens {
            return Some(ThresholdEstimate {
                steps_to_threshold: 0,
            });
        }

        let avg_growth = self.average_growth()?;
        if avg_growth <= f64::EPSILON {
            return None;
        }

        let remaining = threshold_tokens - current_tokens;
        Some(ThresholdEstimate {
            steps_to_threshold: (remaining as f64 / avg_growth).ceil() as u64,
        })
    }

    fn push_delta(&mut self, delta: u64) {
        if self.recent_deltas.len() == self.max_deltas {
            self.recent_deltas.pop_front();
        }
        self.recent_deltas.push_back(delta);
    }

    fn average_growth(&self) -> Option<f64> {
        if self.recent_deltas.is_empty() {
            return None;
        }
        let total: u64 = self.recent_deltas.iter().sum();
        Some(total as f64 / self.recent_deltas.len() as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_is_projected_tokens_over_window() {
        let gauge = ContextGauge::new(200_000);
        let pressure = gauge.measure(50_000);

        assert_eq!(pressure.tokens, 50_000);
        assert_eq!(pressure.window, 200_000);
        assert_eq!(pressure.pressure, 0.25);
    }

    #[test]
    fn zero_window_is_clamped_to_one() {
        let pressure = ContextGauge::new(0).measure(2);

        assert_eq!(pressure.window, 1);
        assert_eq!(pressure.pressure, 2.0);
    }

    #[test]
    fn estimator_uses_recent_average_growth_for_steps_to_threshold() {
        let mut estimator = ContextGrowthEstimator::new(3);
        for tokens in [10, 20, 35, 50] {
            estimator.observe(tokens);
        }

        let estimate = estimator.estimate(50, 100, 0.80).unwrap();

        assert_eq!(estimate.steps_to_threshold, 3);
    }

    #[test]
    fn estimator_handles_flat_growth_without_dividing_by_zero() {
        let estimator = ContextGrowthEstimator::from_projected_tokens([40, 40, 40]);

        assert_eq!(estimator.estimate(40, 100, 0.70), None);
    }

    #[test]
    fn estimator_reports_zero_steps_once_threshold_is_crossed() {
        let estimator = ContextGrowthEstimator::from_projected_tokens([30, 50, 70]);

        let estimate = estimator.estimate(75, 100, 0.70).unwrap();

        assert_eq!(estimate.steps_to_threshold, 0);
    }
}
