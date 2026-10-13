use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;
use truss_core::contract::{BudgetSpec, ConstraintSpec, Contract, DoneClause, ExecutorSpec, Meta};

use crate::benchmark::{
    BenchmarkAdapter, BenchmarkError, BenchmarkLedgerRecord, BenchmarkTask, BenchmarkTaskReport,
};

pub const SWEBENCH_LEDGER_KIND: &str = "swebench_result";

/// A typed SWE-bench Verified task record loaded from a cached JSON/JSONL export.
///
/// The canonical HuggingFace dataset fields use upper-case test-list names; the
/// lower-case aliases make local fixtures and generated JSON easier to read.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SweBenchTask {
    pub instance_id: String,
    pub repo: String,
    pub base_commit: String,
    pub problem_statement: String,
    #[serde(default)]
    pub patch: String,
    #[serde(default)]
    pub test_patch: String,
    #[serde(
        rename = "FAIL_TO_PASS",
        alias = "fail_to_pass",
        default,
        deserialize_with = "deserialize_test_list"
    )]
    pub fail_to_pass: Vec<String>,
    #[serde(
        rename = "PASS_TO_PASS",
        alias = "pass_to_pass",
        default,
        deserialize_with = "deserialize_test_list"
    )]
    pub pass_to_pass: Vec<String>,
    #[serde(default)]
    pub hints_text: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub environment_setup_commit: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn deserialize_test_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Array(_) => serde_json::from_value(value).map_err(D::Error::custom),
        serde_json::Value::String(text) => serde_json::from_str(&text).map_err(D::Error::custom),
        other => Err(D::Error::custom(format!(
            "expected test list array or JSON-encoded array string, got {other}"
        ))),
    }
}

impl SweBenchTask {
    pub fn title_text(&self) -> String {
        format!("SWE-bench Verified {}", self.instance_id)
    }

    pub fn tests(&self) -> Vec<&str> {
        self.fail_to_pass
            .iter()
            .chain(self.pass_to_pass.iter())
            .map(String::as_str)
            .collect()
    }
}

impl BenchmarkTask for SweBenchTask {
    fn id(&self) -> &str {
        &self.instance_id
    }

    fn title(&self) -> &str {
        &self.instance_id
    }

    fn intent(&self) -> &str {
        &self.problem_statement
    }
}

#[derive(Debug, Error)]
pub enum SweBenchLoadError {
    #[error("failed to read SWE-bench dataset {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON in SWE-bench dataset {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid JSONL in SWE-bench dataset {path} at line {line}: {source}")]
    JsonLine {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("SWE-bench dataset {path} must be a JSON array, single object, or JSONL records")]
    UnsupportedShape { path: PathBuf },
}

pub fn load_swebench_tasks(path: impl AsRef<Path>) -> Result<Vec<SweBenchTask>, SweBenchLoadError> {
    let path = path.as_ref();
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
    {
        return load_jsonl(path);
    }

    let text = fs::read_to_string(path).map_err(|source| SweBenchLoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|source| SweBenchLoadError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(|item| {
                serde_json::from_value(item).map_err(|source| SweBenchLoadError::Json {
                    path: path.to_path_buf(),
                    source,
                })
            })
            .collect(),
        serde_json::Value::Object(_) => {
            let task = serde_json::from_value(value).map_err(|source| SweBenchLoadError::Json {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(vec![task])
        }
        _ => Err(SweBenchLoadError::UnsupportedShape {
            path: path.to_path_buf(),
        }),
    }
}

fn load_jsonl(path: &Path) -> Result<Vec<SweBenchTask>, SweBenchLoadError> {
    let file = fs::File::open(path).map_err(|source| SweBenchLoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = BufReader::new(file);
    let mut tasks = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| SweBenchLoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let task = serde_json::from_str(&line).map_err(|source| SweBenchLoadError::JsonLine {
            path: path.to_path_buf(),
            line: index + 1,
            source,
        })?;
        tasks.push(task);
    }
    Ok(tasks)
}

#[derive(Debug, Clone)]
pub enum SweBenchCheckout {
    Single(PathBuf),
    ByInstanceRoot(PathBuf),
}

impl SweBenchCheckout {
    fn workdir(&self, task: &SweBenchTask) -> PathBuf {
        match self {
            SweBenchCheckout::Single(path) => path.clone(),
            SweBenchCheckout::ByInstanceRoot(root) => {
                root.join(safe_instance_dir(&task.instance_id))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum SweBenchPatchSource {
    /// A single patch file, usually for a one-task smoke run.
    File(PathBuf),
    /// A directory containing `<instance_id>.patch` files.
    Directory(PathBuf),
    /// A patch path relative to each task checkout.
    Relative(PathBuf),
    /// Official SWE-bench predictions JSONL with `instance_id` + `model_patch`.
    PredictionsJsonl(PathBuf),
}

impl SweBenchPatchSource {
    fn patch_path(&self, task: &SweBenchTask, workdir: &Path) -> Option<PathBuf> {
        match self {
            SweBenchPatchSource::File(path) => Some(path.clone()),
            SweBenchPatchSource::Directory(dir) => {
                Some(dir.join(format!("{}.patch", task.instance_id)))
            }
            SweBenchPatchSource::Relative(path) => Some(workdir.join(path)),
            SweBenchPatchSource::PredictionsJsonl(_) => None,
        }
    }

    fn patch_hash(&self, task: &SweBenchTask, workdir: &Path) -> String {
        match self {
            SweBenchPatchSource::PredictionsJsonl(path) => patch_hash_from_predictions(path, task)
                .unwrap_or_else(|e| format!("unavailable:{e}")),
            _ => self
                .patch_path(task, workdir)
                .map(|path| hash_file(&path).unwrap_or_else(|e| format!("unavailable:{e}")))
                .unwrap_or_else(|| "unavailable:no-patch-path".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SweBenchAdapter {
    checkout: SweBenchCheckout,
    patch_source: SweBenchPatchSource,
    grader: SweBenchGrader,
}

#[derive(Debug, Clone)]
enum SweBenchGrader {
    Local {
        test_command: Option<String>,
    },
    #[cfg(feature = "swebench-live")]
    Official(SweBenchLiveConfig),
}

#[cfg(feature = "swebench-live")]
#[derive(Debug, Clone)]
pub struct SweBenchLiveConfig {
    pub dataset_path: PathBuf,
    pub predictions_path: PathBuf,
    pub run_id: String,
    pub python: String,
    pub max_workers: usize,
}

impl SweBenchAdapter {
    pub fn local(
        checkout: SweBenchCheckout,
        patch_source: SweBenchPatchSource,
        test_command: Option<String>,
    ) -> Self {
        Self {
            checkout,
            patch_source,
            grader: SweBenchGrader::Local { test_command },
        }
    }

    #[cfg(feature = "swebench-live")]
    pub fn official_docker(checkout: SweBenchCheckout, config: SweBenchLiveConfig) -> Self {
        Self {
            checkout,
            patch_source: SweBenchPatchSource::PredictionsJsonl(config.predictions_path.clone()),
            grader: SweBenchGrader::Official(config),
        }
    }

    pub fn workdir_for(&self, task: &SweBenchTask) -> PathBuf {
        self.checkout.workdir(task)
    }

    pub fn patch_hash_for(&self, task: &SweBenchTask) -> String {
        let workdir = self.workdir_for(task);
        self.patch_source.patch_hash(task, &workdir)
    }
}

impl BenchmarkAdapter for SweBenchAdapter {
    type Task = SweBenchTask;

    fn to_contract(&self, task: &Self::Task) -> Result<Contract, BenchmarkError> {
        let workdir = absolute_or_current(&self.workdir_for(task)).map_err(|reason| {
            BenchmarkError::Adapt {
                task_id: task.instance_id.clone(),
                reason,
            }
        })?;
        let run = match &self.grader {
            SweBenchGrader::Local { test_command } => {
                local_grader_command(task, &workdir, &self.patch_source, test_command.as_deref())
            }
            #[cfg(feature = "swebench-live")]
            SweBenchGrader::Official(config) => official_grader_command(task, config),
        }?;

        Ok(Contract {
            contract: Meta {
                id: task.instance_id.clone(),
                title: task.title_text(),
                intent: task.problem_statement.clone(),
            },
            done: vec![DoneClause {
                kind: "check".into(),
                desc: "Apply the candidate patch, install the task test patch, and run FAIL_TO_PASS + PASS_TO_PASS tests.".into(),
                run: Some(run),
                expect_exit: Some(0),
                expect_contains: Vec::new(),
                reviewer_context: Vec::new(),
            }],
            budget: BudgetSpec {
                max_steps: Some(8),
                ..BudgetSpec::default()
            },
            constraints: ConstraintSpec {
                write_scope: Some(vec![workdir.clone()]),
                read_scope: Some(vec![workdir]),
                network: Some("none".into()),
                ..ConstraintSpec::default()
            },
            reviewers: Some(0),
            memory: Default::default(),
            sandbox: Default::default(),
            approval_gates: Default::default(),
            mcp: Default::default(),
            executor: ExecutorSpec::default(),
        })
    }

    fn ledger_record(
        &self,
        task: &Self::Task,
        report: &BenchmarkTaskReport,
    ) -> Result<Option<BenchmarkLedgerRecord>, BenchmarkError> {
        Ok(Some(BenchmarkLedgerRecord {
            kind: SWEBENCH_LEDGER_KIND.into(),
            payload: json!({
                "instance_id": task.instance_id,
                "patch_hash": self.patch_hash_for(task),
                "resolved": report.passed,
            }),
        }))
    }
}

fn local_grader_command(
    task: &SweBenchTask,
    workdir: &Path,
    patch_source: &SweBenchPatchSource,
    test_command: Option<&str>,
) -> Result<String, BenchmarkError> {
    let patch_path =
        patch_source
            .patch_path(task, workdir)
            .ok_or_else(|| BenchmarkError::Adapt {
                task_id: task.instance_id.clone(),
                reason: "local SWE-bench grading requires a candidate patch file".into(),
            })?;
    let test_command = test_command
        .map(str::to_string)
        .unwrap_or_else(|| pytest_command(task));
    let test_patch_step = if task.test_patch.trim().is_empty() {
        "true".to_string()
    } else {
        format!(
            "cat > .truss-swebench-test.patch <<'{}'\n{}\n{}\nif [ -s .truss-swebench-test.patch ]; then if git apply --check .truss-swebench-test.patch; then git apply .truss-swebench-test.patch; elif git apply --reverse --check .truss-swebench-test.patch; then echo 'test patch already applied'; else git apply --check .truss-swebench-test.patch; fi; fi",
            heredoc_marker(task),
            task.test_patch,
            heredoc_marker(task),
        )
    };

    Ok(format!(
        "set -eu\ncd {}\nif [ -s {} ]; then if git apply --check {}; then git apply {}; elif git apply --reverse --check {}; then echo 'candidate patch already applied'; else git apply --check {}; fi; fi\n{}\n{}",
        shell_quote_path(workdir),
        shell_quote_path(&patch_path),
        shell_quote_path(&patch_path),
        shell_quote_path(&patch_path),
        shell_quote_path(&patch_path),
        shell_quote_path(&patch_path),
        test_patch_step,
        test_command,
    ))
}

#[cfg(feature = "swebench-live")]
fn official_grader_command(
    task: &SweBenchTask,
    config: &SweBenchLiveConfig,
) -> Result<String, BenchmarkError> {
    if config.max_workers == 0 {
        return Err(BenchmarkError::Adapt {
            task_id: task.instance_id.clone(),
            reason: "swebench live max_workers must be at least 1".into(),
        });
    }
    let report_dir = config
        .predictions_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("reports")
        .join(safe_instance_dir(&config.run_id))
        .join(safe_instance_dir(&task.instance_id));
    Ok(format!(
        r#"set -eu
python_bin={}
dataset={}
predictions={}
run_id={}
instance_id={}
report_dir={}
pred_dir=$(dirname "$predictions")

"$python_bin" - "$predictions" "$instance_id" <<'PY'
import json
import sys
from pathlib import Path

predictions = Path(sys.argv[1])
instance_id = sys.argv[2]
found = False
for line in predictions.read_text().splitlines():
    if not line.strip():
        continue
    row = json.loads(line)
    if row.get("instance_id") != instance_id:
        continue
    found = True
    patch = row.get("model_patch", row.get("patch"))
    if patch is None:
        raise SystemExit(f"prediction for {{instance_id}} is missing model_patch")
    if not patch.strip():
        raise SystemExit(f"prediction for {{instance_id}} has an empty model_patch")
    break
if not found:
    raise SystemExit(f"prediction for {{instance_id}} not found")
PY

mkdir -p "$report_dir"
rm -f "$pred_dir"/*."$run_id".json
(
  cd "$pred_dir"
  "$python_bin" -m swebench.harness.run_evaluation \
    --dataset_name "$dataset" \
    --predictions_path "$predictions" \
    --run_id "$run_id" \
    --instance_ids "$instance_id" \
    --max_workers {} \
    --report_dir "$report_dir"
)

"$python_bin" - "$pred_dir" "$run_id" "$instance_id" <<'PY'
import json
import sys
from pathlib import Path

pred_dir = Path(sys.argv[1])
run_id = sys.argv[2]
instance_id = sys.argv[3]
matches = sorted(pred_dir.glob(f"*.{{run_id}}.json"))
if len(matches) != 1:
    raise SystemExit(f"expected exactly one SWE-bench summary for run_id {{run_id}}, found {{len(matches)}}")
report_path = matches[0]
report = json.loads(report_path.read_text())

def ids(name):
    return set(report.get(name) or [])

if instance_id not in ids("submitted_ids"):
    raise SystemExit(f"official report {{report_path}} does not include submitted instance {{instance_id}}")
if instance_id in ids("empty_patch_ids"):
    raise SystemExit(f"official report {{report_path}} marked {{instance_id}} as empty_patch")
if instance_id in ids("error_ids"):
    raise SystemExit(f"official report {{report_path}} marked {{instance_id}} as error")
if instance_id not in ids("completed_ids"):
    raise SystemExit(f"official report {{report_path}} did not complete {{instance_id}}")
if instance_id not in ids("resolved_ids"):
    raise SystemExit(f"official report {{report_path}} did not resolve {{instance_id}}")
print(f"official SWE-bench report {{report_path}} resolved {{instance_id}}")
PY
"#,
        shell_quote(&config.python),
        shell_quote_path(&config.dataset_path),
        shell_quote_path(&config.predictions_path),
        shell_quote(&config.run_id),
        shell_quote(&task.instance_id),
        shell_quote_path(&report_dir),
        config.max_workers,
    ))
}

fn pytest_command(task: &SweBenchTask) -> String {
    let tests = task.tests();
    if tests.is_empty() {
        "python -m pytest".into()
    } else {
        format!(
            "python -m pytest {}",
            tests
                .iter()
                .map(|test| shell_quote(test))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
}

fn patch_hash_from_predictions(path: &Path, task: &SweBenchTask) -> Result<String, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if value.get("instance_id").and_then(|v| v.as_str()) != Some(task.instance_id.as_str()) {
            continue;
        }
        let patch = value
            .get("model_patch")
            .or_else(|| value.get("patch"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| "prediction record missing model_patch".to_string())?;
        return Ok(hash_bytes(patch.as_bytes()));
    }
    Err(format!(
        "prediction record for {} not found",
        task.instance_id
    ))
}

fn hash_file(path: &Path) -> Result<String, String> {
    fs::read(path)
        .map(|bytes| hash_bytes(&bytes))
        .map_err(|e| e.to_string())
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("sha256:{}", hex::encode(h.finalize()))
}

fn absolute_or_current(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|e| e.to_string())
}

fn safe_instance_dir(instance_id: &str) -> String {
    instance_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn heredoc_marker(task: &SweBenchTask) -> String {
    format!(
        "TRUSS_SWEBENCH_TEST_PATCH_{}",
        safe_instance_dir(&task.instance_id)
    )
}

fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\\''"))
}
