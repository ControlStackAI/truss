use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use truss_bench::{
    load_swebench_tasks, BenchmarkAdapter, BenchmarkRunner, SweBenchAdapter, SweBenchCheckout,
    SweBenchPatchSource, SweBenchStyleAdapter, SweBenchStyleTask, SWEBENCH_LEDGER_KIND,
};
use truss_core::ctx::Ctx;
use truss_core::durable::DurableConfig;
use truss_core::error::ProviderError;
use truss_core::memory::SqliteMemoryStore;
use truss_core::message::Block;
use truss_core::provider::{CompletionRequest, CompletionResponse, Provider, StopReason, Usage};
use truss_core::store::Store;

#[derive(Clone, Default)]
struct BenchProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl Provider for BenchProvider {
    fn name(&self) -> &str {
        "bench-provider"
    }

    fn default_model(&self) -> &str {
        "gpt-4o-mini"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let system = req.system.clone().unwrap_or_default();
        let body = if system.contains("Extract only durable") {
            r#"{"lessons":[]}"#
        } else {
            "benchmark fixture complete"
        };
        Ok(CompletionResponse {
            content: vec![Block::Text(body.into())],
            stop_reason: StopReason::EndTurn,
            model: "gpt-4o-mini".into(),
            usage: Usage {
                input_tokens: 8,
                output_tokens: 4,
            },
        })
    }
}

fn fixture_task() -> SweBenchStyleTask {
    let workdir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("swe-style-pass");
    SweBenchStyleTask::new(
        "swe-style-fixture",
        "SWE-style fixture",
        workdir,
        "Run the local fixture grader successfully.",
        "sh check.sh",
    )
}

fn swebench_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("swebench-verified")
}

fn copy_fixture_repo(name: &str) -> std::path::PathBuf {
    let src = swebench_fixture_path().join("repo");
    let dst = std::env::temp_dir().join(format!(
        "truss-swebench-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dst.join(entry.file_name())).unwrap();
    }
    dst
}

#[cfg(feature = "swebench-live")]
fn unique_temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "truss-swebench-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[cfg(feature = "swebench-live")]
fn python_bin() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| "python3".into())
}

#[test]
fn swe_bench_style_adapter_maps_fixture_to_contract() {
    let task = fixture_task();
    let contract = SweBenchStyleAdapter.to_contract(&task).unwrap();

    assert_eq!(contract.contract.id, "swe-style-fixture");
    assert_eq!(contract.contract.intent, task.intent);
    assert_eq!(contract.done.len(), 1);
    assert_eq!(contract.done[0].kind, "check");
    let run = contract.done[0].run.as_deref().unwrap();
    assert!(run.contains("cd '"));
    assert!(run.contains("sh check.sh"));
    assert_eq!(contract.done[0].expect_exit, Some(0));
}

#[tokio::test]
async fn benchmark_runner_reports_pass_and_cost() {
    let store = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&store).unwrap());
    let provider = Arc::new(BenchProvider::default());
    let mut runner = BenchmarkRunner::new("fixture-suite", provider, memory);
    runner.base_cfg = DurableConfig {
        max_steps: 2,
        ..DurableConfig::default()
    };

    let report = runner
        .run(
            &Ctx::new(),
            &store,
            &SweBenchStyleAdapter,
            &[fixture_task()],
        )
        .await
        .unwrap();

    assert_eq!(report.benchmark_id, "fixture-suite");
    assert_eq!(report.task_count, 1);
    assert_eq!(report.pass_count, 1);
    assert_eq!(report.fail_count, 0);
    assert!(report.total_usage.cost_usd > 0.0);
    assert!(report.total_usage.steps > 0);
    assert_eq!(report.tasks[0].task_id, "swe-style-fixture");
    assert!(report.tasks[0].passed);
}

#[tokio::test]
async fn benchmark_report_serializes_round_trip() {
    let store = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&store).unwrap());
    let provider = Arc::new(BenchProvider::default());
    let mut runner = BenchmarkRunner::new("round-trip-suite", provider, memory);
    runner.base_cfg = DurableConfig {
        max_steps: 2,
        ..DurableConfig::default()
    };

    let report = runner
        .run(
            &Ctx::new(),
            &store,
            &SweBenchStyleAdapter,
            &[fixture_task()],
        )
        .await
        .unwrap();
    let encoded = serde_json::to_string(&report).unwrap();
    let decoded: truss_bench::BenchmarkReport = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, report);
}

#[tokio::test]
async fn paired_learning_benchmark_run_produces_delta() {
    let store = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&store).unwrap());
    let provider = Arc::new(BenchProvider::default());
    let mut runner = BenchmarkRunner::new("delta-suite", provider, memory);
    runner.base_cfg = DurableConfig {
        max_steps: 2,
        ..DurableConfig::default()
    };

    let report = runner
        .run(
            &Ctx::new(),
            &store,
            &SweBenchStyleAdapter,
            &[fixture_task()],
        )
        .await
        .unwrap();

    assert_eq!(report.tasks[0].eval_report.arms.len(), 2);
    assert_eq!(report.tasks[0].delta.pass_rate, 0.0);
    assert!(report.tasks[0].delta.steps >= 0);
}

#[test]
fn swebench_loader_parses_verified_jsonl_record() {
    let tasks = load_swebench_tasks(swebench_fixture_path().join("one-task.jsonl")).unwrap();

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].instance_id, "fake-repo__fake-1");
    assert_eq!(tasks[0].repo, "fake/repo");
    assert_eq!(tasks[0].base_commit.len(), 40);
    assert_eq!(tasks[0].fail_to_pass, vec!["check.sh"]);
    assert_eq!(tasks[0].pass_to_pass, Vec::<String>::new());
    assert!(tasks[0].problem_statement.contains("answer.txt"));
}

#[test]
fn swebench_loader_parses_json_string_test_lists() {
    let tasks =
        load_swebench_tasks(swebench_fixture_path().join("string-list-task.jsonl")).unwrap();

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].instance_id, "fake-repo__fake-string-lists");
    assert_eq!(
        tasks[0].fail_to_pass,
        vec!["check.sh", "tests/test_answer.py::test_answer"]
    );
    assert_eq!(tasks[0].pass_to_pass, vec!["tests/test_existing.py"]);
}

#[test]
fn swebench_adapter_maps_task_to_test_based_contract() {
    let task = load_swebench_tasks(swebench_fixture_path().join("one-task.jsonl"))
        .unwrap()
        .remove(0);
    let repo = copy_fixture_repo("adapter");
    let adapter = SweBenchAdapter::local(
        SweBenchCheckout::Single(repo.clone()),
        SweBenchPatchSource::Relative("candidate.patch".into()),
        Some("sh check.sh".into()),
    );

    let contract = adapter.to_contract(&task).unwrap();

    assert_eq!(contract.contract.id, task.instance_id);
    assert_eq!(contract.contract.intent, task.problem_statement);
    assert_eq!(contract.done.len(), 1);
    assert_eq!(contract.done[0].kind, "check");
    assert_eq!(contract.done[0].expect_exit, Some(0));
    let run = contract.done[0].run.as_deref().unwrap();
    assert!(run.contains("git apply --check"));
    assert!(run.contains("candidate.patch"));
    assert!(run.contains("sh check.sh"));
}

#[tokio::test]
async fn swebench_runner_reports_resolution_delta_and_ledger_record() {
    let mut task = load_swebench_tasks(swebench_fixture_path().join("one-task.jsonl"))
        .unwrap()
        .remove(0);
    let repo = copy_fixture_repo("runner");
    task.test_patch.clear();
    let adapter = SweBenchAdapter::local(
        SweBenchCheckout::Single(repo),
        SweBenchPatchSource::Relative("candidate.patch".into()),
        Some("sh check.sh".into()),
    );
    let store = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&store).unwrap());
    let provider = Arc::new(BenchProvider::default());
    let mut runner = BenchmarkRunner::new("swebench-fixture-suite", provider, memory);
    runner.base_cfg = DurableConfig {
        max_steps: 2,
        ..DurableConfig::default()
    };

    let ctx = Ctx::new();
    let report = runner.run(&ctx, &store, &adapter, &[task]).await.unwrap();

    assert_eq!(report.benchmark_id, "swebench-fixture-suite");
    assert_eq!(report.task_count, 1);
    assert_eq!(report.pass_count, 1);
    assert_eq!(report.fail_count, 0);
    assert!(report.total_usage.cost_usd > 0.0);
    assert!(report.tasks[0].passed);
    assert_eq!(report.tasks[0].delta.pass_rate, 0.0);
    assert!(report.tasks[0].ledger_ref.is_some());

    let entries = store.entries(&ctx.run_id).unwrap();
    let ledger = entries
        .iter()
        .find(|entry| entry.kind == SWEBENCH_LEDGER_KIND)
        .expect("swebench ledger record");
    assert_eq!(ledger.payload["instance_id"], "fake-repo__fake-1");
    assert_eq!(ledger.payload["resolved"], true);
    assert!(ledger.payload["patch_hash"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
}

#[cfg(feature = "swebench-live")]
#[tokio::test]
async fn swebench_live_empty_prediction_fails_closed() {
    let task = load_swebench_tasks(swebench_fixture_path().join("one-task.jsonl"))
        .unwrap()
        .remove(0);
    let temp = unique_temp_dir("empty-live");
    std::fs::create_dir_all(&temp).unwrap();
    let prediction = temp.join("empty.jsonl");
    std::fs::write(
        &prediction,
        serde_json::json!({
            "instance_id": task.instance_id,
            "model_name_or_path": "fake-model",
            "model_patch": ""
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    let adapter = SweBenchAdapter::official_docker(
        SweBenchCheckout::Single(copy_fixture_repo("empty-live")),
        truss_bench::SweBenchLiveConfig {
            dataset_path: swebench_fixture_path().join("one-task.jsonl"),
            predictions_path: prediction,
            run_id: "empty-live-regression".into(),
            python: python_bin(),
            max_workers: 1,
        },
    );
    let store = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&store).unwrap());
    let provider = Arc::new(BenchProvider::default());
    let mut runner = BenchmarkRunner::new("swebench-empty-live", provider, memory);
    runner.base_cfg = DurableConfig {
        max_steps: 2,
        ..DurableConfig::default()
    };

    let ctx = Ctx::new();
    let report = runner.run(&ctx, &store, &adapter, &[task]).await.unwrap();

    assert_eq!(report.pass_count, 0);
    assert_eq!(report.fail_count, 1);
    assert!(!report.tasks[0].passed);
    let entries = store.entries(&ctx.run_id).unwrap();
    let ledger = entries
        .iter()
        .find(|entry| entry.kind == SWEBENCH_LEDGER_KIND)
        .expect("swebench ledger record");
    assert_eq!(ledger.payload["resolved"], false);
}

#[cfg(feature = "swebench-live")]
#[test]
fn swebench_live_unresolved_official_report_fails_closed() {
    let task = load_swebench_tasks(swebench_fixture_path().join("one-task.jsonl"))
        .unwrap()
        .remove(0);
    let temp = unique_temp_dir("unresolved-live");
    let fake_pkg = temp.join("fakepkg");
    let harness = fake_pkg.join("swebench").join("harness");
    std::fs::create_dir_all(&harness).unwrap();
    std::fs::write(fake_pkg.join("swebench").join("__init__.py"), "").unwrap();
    std::fs::write(harness.join("__init__.py"), "").unwrap();
    std::fs::write(
        harness.join("run_evaluation.py"),
        r#"
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--dataset_name")
parser.add_argument("--predictions_path")
parser.add_argument("--run_id")
parser.add_argument("--instance_ids", nargs="+")
parser.add_argument("--max_workers")
parser.add_argument("--report_dir")
parser.add_argument("--rewrite_reports", nargs="?")
args = parser.parse_args()

instance_id = args.instance_ids[0]
model = "fake-model"
for line in Path(args.predictions_path).read_text().splitlines():
    if not line.strip():
        continue
    row = json.loads(line)
    if row.get("instance_id") == instance_id:
        model = row.get("model_name_or_path", model)
        break

Path(f"{model}.{args.run_id}.json").write_text(json.dumps({
    "total_instances": 1,
    "submitted_instances": 1,
    "completed_instances": 1,
    "resolved_instances": 0,
    "unresolved_instances": 1,
    "empty_patch_instances": 0,
    "error_instances": 0,
    "completed_ids": [instance_id],
    "submitted_ids": [instance_id],
    "resolved_ids": [],
    "unresolved_ids": [instance_id],
    "empty_patch_ids": [],
    "error_ids": [],
    "schema_version": 2,
}))
"#,
    )
    .unwrap();
    let prediction = temp.join("prediction.jsonl");
    std::fs::write(
        &prediction,
        serde_json::json!({
            "instance_id": task.instance_id,
            "model_name_or_path": "fake-model",
            "model_patch": "diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-a\n+b\n"
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    let adapter = SweBenchAdapter::official_docker(
        SweBenchCheckout::Single(copy_fixture_repo("unresolved-live")),
        truss_bench::SweBenchLiveConfig {
            dataset_path: swebench_fixture_path().join("one-task.jsonl"),
            predictions_path: prediction,
            run_id: "unresolved-live-regression".into(),
            python: python_bin(),
            max_workers: 1,
        },
    );
    let contract = adapter.to_contract(&task).unwrap();
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(contract.done[0].run.as_deref().unwrap())
        .env("PYTHONPATH", fake_pkg)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("did not resolve"),
        "stderr should identify the official unresolved report: {stderr}"
    );
}
