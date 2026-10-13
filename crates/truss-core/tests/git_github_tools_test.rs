use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use truss_core::ctx::Ctx;
use truss_core::policy::Policy;
use truss_core::store::{tool_call_key, Store};
use truss_core::tools::{GitJournal, ToolRegistry};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn git_status_records_signed_hashed_tool_output() {
    let repo = test_repo("status-ledger");
    init_repo(&repo);
    write_file(&repo, "tracked.txt", "hello\n");
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-m", "initial"]);
    write_file(&repo, "tracked.txt", "hello\nchanged\n");

    let store = Store::open_memory().unwrap();
    store
        .create_session("git-status", Some("mock"), "local", "local")
        .unwrap();
    let mut registry = ToolRegistry::new();
    registry.register_git_tools(Some(GitJournal {
        store: store.clone(),
        session_id: "git-status".into(),
    }));
    let ctx = Ctx::new().with_policy(Policy::workdir(repo.clone()));

    let input = json!({ "repo": repo });
    let out = registry
        .call_with_ledger(&ctx, "git_status", input, None)
        .await
        .expect("git status");
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["clean"], false);
    assert_eq!(value["files"][0]["path"], "tracked.txt");

    let entries = store.entries("git-status").unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.kind == "git_tool_call")
        .expect("git provenance entry");
    assert_eq!(entry.payload["tool"], "git_status");
    assert_eq!(entry.payload["side_effect"], false);
    assert_eq!(
        entry.payload["result_sha256"],
        sha256_str(&out),
        "ledger binds the exact status output"
    );
    assert!(entry.payload["input_sha256"].as_str().unwrap().len() == 64);
    assert!(!entry.signature.is_empty());
    assert!(store.verify_chain("git-status").unwrap().is_ok());
}

#[tokio::test]
async fn git_commit_replays_recorded_result_without_recommitting_on_resume() {
    let repo = test_repo("commit-replay");
    init_repo(&repo);
    write_file(&repo, "tracked.txt", "one\n");
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-m", "initial"]);
    write_file(&repo, "tracked.txt", "two\n");

    let store = Store::open_memory().unwrap();
    store
        .create_session("git-commit", Some("mock"), "local", "local")
        .unwrap();
    let mut registry = ToolRegistry::new();
    registry.register_git_tools(Some(GitJournal {
        store: store.clone(),
        session_id: "git-commit".into(),
    }));
    let ctx = Ctx::new().with_policy(Policy::workdir(repo.clone()));
    let input = json!({ "repo": repo, "message": "update tracked", "all": true });

    let first = registry
        .call_with_ledger(&ctx, "git_commit", input.clone(), None)
        .await
        .expect("first commit");
    let second = registry
        .call_with_ledger(&ctx, "git_commit", input.clone(), None)
        .await
        .expect("replayed commit");
    assert_eq!(first, second);
    assert_eq!(
        git(&repo, &["rev-list", "--count", "HEAD"]).trim(),
        "2",
        "resume replay must not create a second commit"
    );

    let memo_key = tool_call_key("git_commit", &input);
    assert_eq!(
        store.memo_get("git-commit", &memo_key).unwrap().unwrap(),
        (first.clone(), false)
    );
    assert_eq!(
        store
            .entries("git-commit")
            .unwrap()
            .iter()
            .filter(|entry| entry.kind == "git_tool_call")
            .count(),
        1,
        "memo replay should not append duplicate provenance"
    );
    assert!(store.verify_chain("git-commit").unwrap().is_ok());
}

#[tokio::test]
async fn git_commit_memo_without_signed_provenance_is_rejected() {
    let repo = test_repo("commit-memo-boundary");
    init_repo(&repo);
    write_file(&repo, "tracked.txt", "one\n");
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-m", "initial"]);
    write_file(&repo, "tracked.txt", "two\n");

    let store = Store::open_memory().unwrap();
    store
        .create_session("git-boundary", Some("mock"), "local", "local")
        .unwrap();
    let mut registry = ToolRegistry::new();
    registry.register_git_tools(Some(GitJournal {
        store: store.clone(),
        session_id: "git-boundary".into(),
    }));
    let ctx = Ctx::new().with_policy(Policy::workdir(repo.clone()));
    let input = json!({ "repo": repo, "message": "update tracked", "all": true });
    let memo_key = tool_call_key("git_commit", &input);
    store
        .memo_put("git-boundary", &memo_key, "{\"sha\":\"fake\"}", false)
        .unwrap();

    let err = registry
        .call_with_ledger(&ctx, "git_commit", input, None)
        .await
        .expect_err("unsigned memo must not replay");
    assert!(err
        .to_string()
        .contains("git memo missing matching signed provenance"));
    assert_eq!(
        git(&repo, &["rev-list", "--count", "HEAD"]).trim(),
        "1",
        "trust-boundary failure must not execute the commit"
    );
}

fn test_repo(label: &str) -> PathBuf {
    let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("truss-{label}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn init_repo(repo: &Path) {
    git(repo, &["init"]);
    git(repo, &["config", "user.name", "Truss Test"]);
    git(repo, &["config", "user.email", "truss@example.invalid"]);
}

fn write_file(repo: &Path, path: &str, content: &str) {
    std::fs::write(repo.join(path), content).unwrap();
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("run git {args:?}: {e}"));
    if !output.status.success() {
        panic!(
            "git {args:?} failed: {}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn sha256_str(value: &str) -> String {
    let mut h = Sha256::new();
    h.update(value.as_bytes());
    format!("{:x}", h.finalize())
}
