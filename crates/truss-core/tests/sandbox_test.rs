use std::sync::Arc;

use serde_json::json;
use truss_core::ctx::Ctx;
use truss_core::events::AgentEvent;
use truss_core::sandbox::{
    Sandbox, SandboxCommand, SandboxConfig, SandboxConfigLayer, SandboxMode, WorkingCopySandbox,
};
use truss_core::store::Store;
use truss_core::tools::{SelfCheckTool, Tool};

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "truss-sandbox-test-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test]
async fn working_copy_sandbox_does_not_mutate_source_dir() {
    let source = temp_path("source");
    std::fs::create_dir_all(source.join("src")).unwrap();
    std::fs::write(source.join("src/input.txt"), "original").unwrap();

    let sandbox = WorkingCopySandbox::new(&source);
    let output = sandbox
        .run(
            SandboxCommand::shell("printf changed > src/input.txt && printf generated > new.txt"),
            &Ctx::new(),
        )
        .await
        .expect("sandbox command succeeds");

    assert_eq!(output.exit, Some(0));
    assert!(!output.workdir_hash.is_empty());
    assert_eq!(
        std::fs::read_to_string(source.join("src/input.txt")).unwrap(),
        "original"
    );
    assert!(!source.join("new.txt").exists());

    let _ = std::fs::remove_dir_all(source);
}

#[tokio::test]
async fn self_validation_writes_signed_verifiable_journal_entry_and_event() {
    let source = temp_path("self-check");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("ok.txt"), "ok").unwrap();

    let store = Store::open_memory().unwrap();
    store
        .create_session("self-check-session", Some("test"), "local", "local")
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = Ctx::new()
        .with_sandbox(Arc::new(WorkingCopySandbox::new(&source)))
        .with_events(tx);
    let tool = SelfCheckTool::new(
        "self_check",
        store.clone(),
        "self-check-session",
        Some("test -f ok.txt".into()),
    );

    let result = tool.call(&ctx, json!({})).await.unwrap();

    assert!(result.contains("self_validation evidence recorded"));
    let entries = store.entries("self-check-session").unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.kind == "self_validation")
        .expect("self_validation entry recorded");
    assert_eq!(entry.payload["cmd"], "test -f ok.txt");
    assert_eq!(entry.payload["exit"], 0);
    assert_eq!(entry.payload["evidence_only"], true);
    assert!(!entry.payload["workdir_hash"].as_str().unwrap().is_empty());
    assert!(!entry.signature.is_empty());
    assert_eq!(store.verify_chain("self-check-session").unwrap(), Ok(1));

    let event = rx.try_recv().expect("self-validation event emitted");
    assert!(matches!(
        event,
        AgentEvent::SelfValidation {
            exit: 0,
            workdir_hash,
            ..
        } if !workdir_hash.is_empty()
    ));

    let _ = std::fs::remove_dir_all(source);
}

#[test]
fn sandbox_config_defaults_off_and_layers_global_agent_contract() {
    let global = SandboxConfigLayer {
        mode: Some(SandboxMode::WorkingCopy),
        checkout: Some("global".into()),
    };
    let agent = SandboxConfigLayer {
        checkout: Some("agent".into()),
        ..SandboxConfigLayer::default()
    };
    let contract = SandboxConfigLayer {
        mode: Some(SandboxMode::NoSandbox),
        ..SandboxConfigLayer::default()
    };

    assert_eq!(SandboxConfig::default().mode, SandboxMode::NoSandbox);

    let resolved = SandboxConfig::resolve(Some(&global), Some(&agent), Some(&contract));
    assert_eq!(resolved.mode, SandboxMode::NoSandbox);
    assert_eq!(
        resolved.checkout.as_deref(),
        Some(std::path::Path::new("agent"))
    );

    let inherited = SandboxConfig::resolve(Some(&global), Some(&agent), None);
    assert_eq!(inherited.mode, SandboxMode::WorkingCopy);
    assert_eq!(
        inherited.checkout.as_deref(),
        Some(std::path::Path::new("agent"))
    );
}
