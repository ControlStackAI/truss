//! Model pricing, used to turn token counts into dollars for budget enforcement.
//!
//! Prices are USD per million tokens (input, output). The table is a pragmatic
//! snapshot — unknown models cost `0.0` (and we warn), so budgets degrade to
//! token/step limits rather than failing closed. Keep this updated or load it
//! from config later; it is deliberately a single, replaceable function.

pub const DEFAULT_CONTEXT_WINDOW: u32 = 128_000;

/// Returns (input_usd_per_mtok, output_usd_per_mtok) for a model id, matching by
/// prefix so dated snapshots (`claude-opus-4-8-2026xxxx`) resolve to their family.
fn rate_per_mtok(model: &str) -> Option<(f64, f64)> {
    let m = model.to_ascii_lowercase();
    let table: &[(&str, f64, f64)] = &[
        ("claude-opus-4", 15.0, 75.0),
        ("claude-sonnet-4", 3.0, 15.0),
        ("claude-haiku-4", 1.0, 5.0),
        ("claude-3-5-sonnet", 3.0, 15.0),
        ("claude-3-5-haiku", 0.80, 4.0),
        ("gpt-5", 1.25, 10.0),
        ("gpt-4o-mini", 0.15, 0.60),
        ("gpt-4o", 2.50, 10.0),
        ("o1", 15.0, 60.0),
    ];
    table
        .iter()
        .find(|(prefix, _, _)| m.starts_with(prefix))
        .map(|(_, i, o)| (*i, *o))
}

/// Context-window size in tokens for known model families.
///
/// Unknown models fall back to the conservative historical default used by
/// `ModelCapabilities`.
pub fn context_window(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    let table: &[(&str, u32)] = &[
        ("claude-opus-4", 200_000),
        ("claude-sonnet-4", 200_000),
        ("claude-haiku-4", 200_000),
        ("claude-3-5-sonnet", 200_000),
        ("claude-3-5-haiku", 200_000),
        ("gpt-5", 256_000),
        ("gpt-4o-mini", 128_000),
        ("gpt-4o", 128_000),
        ("o1", 128_000),
    ];
    table
        .iter()
        .find(|(prefix, _)| m.starts_with(prefix))
        .map_or(DEFAULT_CONTEXT_WINDOW, |(_, window)| *window)
}

/// Cost in USD for a completed call. Unknown models return `0.0`.
pub fn cost_usd(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    match rate_per_mtok(model) {
        Some((in_rate, out_rate)) => {
            (input_tokens as f64 / 1_000_000.0) * in_rate
                + (output_tokens as f64 / 1_000_000.0) * out_rate
        }
        None => 0.0,
    }
}

/// Whether we have a price for this model (callers may warn when false).
pub fn is_priced(model: &str) -> bool {
    rate_per_mtok(model).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_window_matches_known_model_family() {
        assert_eq!(context_window("claude-opus-4-8"), 200_000);
        assert_eq!(context_window("gpt-5.5-codex"), 256_000);
    }

    #[test]
    fn context_window_falls_back_for_unknown_models() {
        assert_eq!(
            context_window("unknown-frontier-model"),
            DEFAULT_CONTEXT_WINDOW
        );
    }
}
