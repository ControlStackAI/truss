use truss_core::compaction_eval::{run_default_eval, CompactionEvalArm};

#[tokio::test]
async fn three_arms_produce_deterministic_metrics() {
    let first = run_default_eval().await.unwrap();
    let second = run_default_eval().await.unwrap();

    assert_eq!(first, second);
    assert_eq!(first.arms.len(), 3);
    assert_eq!(first.arms[0].arm, CompactionEvalArm::Off);
    assert_eq!(first.arms[1].arm, CompactionEvalArm::Threshold);
    assert_eq!(first.arms[2].arm, CompactionEvalArm::Anticipate);
}

#[tokio::test]
async fn report_serializes_and_prints_the_comparison_table() {
    let report = run_default_eval().await.unwrap();

    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("momentum_preserved_steps"));

    let table = report.pretty_table();
    assert!(table.contains("| OFF |"));
    assert!(table.contains("| THRESHOLD |"));
    assert!(table.contains("| ANTICIPATE |"));
}

#[tokio::test]
async fn measured_relationships_are_reported_honestly() {
    let report = run_default_eval().await.unwrap();
    let off = &report.arms[0];
    let threshold = &report.arms[1];
    let anticipate = &report.arms[2];

    assert!(off.completed);
    assert!(threshold.completed);
    assert!(anticipate.completed);
    assert!(!off.completed_without_context_wall);
    assert!(threshold.completed_without_context_wall);
    assert!(anticipate.completed_without_context_wall);
    assert_eq!(off.compaction_performed_events, 0);
    assert!(threshold.compaction_performed_events > 0);
    assert!(anticipate.compaction_performed_events > 0);
    assert!(threshold.peak_pressure < off.peak_pressure);
    assert!(anticipate.peak_pressure <= threshold.peak_pressure);
    assert!(threshold.tokens_summarized_away > 0);
    assert!(anticipate.tokens_summarized_away > 0);
}
