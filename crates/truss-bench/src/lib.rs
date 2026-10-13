//! Benchmark and simulated-user testing tools for Truss.
//!
//! This crate deliberately builds on `truss-core` instead of replacing it:
//! benchmark tasks become core `Contract`s, and benchmark runs reuse the SI-4
//! paired eval path.

pub mod benchmark;
pub mod simulated_user;
pub mod swebench;

pub use benchmark::{
    BenchmarkAdapter, BenchmarkError, BenchmarkLedgerRecord, BenchmarkReport, BenchmarkRunner,
    BenchmarkTask, BenchmarkTaskReport, BenchmarkVerdict, SweBenchStyleAdapter, SweBenchStyleTask,
};
pub use simulated_user::{
    InteractionMetrics, InteractionReport, InteractionTurn, SimulatedUser, SimulatedUserConfig,
};
pub use swebench::{
    load_swebench_tasks, SweBenchAdapter, SweBenchCheckout, SweBenchLoadError, SweBenchPatchSource,
    SweBenchTask, SWEBENCH_LEDGER_KIND,
};

#[cfg(feature = "swebench-live")]
pub use swebench::SweBenchLiveConfig;
