//! Integration test for taskboard worker executing tasks via Claude Code CLI.
//!
//! This test hits a real LLM API — it costs money and takes time.
//! Run explicitly: `cargo test -p eli --test taskboard_integration`

use std::time::Duration;

use eli::taskboard::store::TaskStore;
use eli::taskboard::worker::TaskWorker;
use eli::taskboard::{NewTask, TaskFilter};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Test that the worker can claim a task, execute it via Claude Code, and complete it.
#[tokio::test]
async fn worker_executes_task_via_claude_code() {
    // Skip if no CLI available
    let cli_check = tokio::process::Command::new("which")
        .arg("claude")
        .output()
        .await;
    if cli_check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("skipping: claude CLI not found");
        return;
    }

    let store = TaskStore::open_memory().expect("open in-memory store");
    let workspace = std::env::current_dir().expect("current dir");

    // Create a simple task
    let task_id = store
        .create(NewTask {
            kind: "test".into(),
            session_origin: "integration-test".into(),
            context: json!({"prompt": "Reply with exactly the word 'pong'. Nothing else."}),
            parent: None,
            priority: 1,
            metadata: json!({}),
        })
        .await
        .expect("create task");

    // Start worker in background
    let cancel = CancellationToken::new();
    let worker = TaskWorker::new(
        store.clone(),
        vec!["test".into()],
        "test-worker".into(),
        workspace,
    )
    .with_poll_interval(Duration::from_millis(500));

    let cancel_clone = cancel.clone();
    let worker_handle = tokio::spawn(async move {
        worker.run(cancel_clone).await;
    });

    // Wait for the task to complete (with timeout)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if tokio::time::Instant::now() > deadline {
            cancel.cancel();
            worker_handle.await.unwrap();
            panic!("task did not complete within 120 seconds");
        }

        if let Some(task) = store.get(task_id).await
            && task.status.is_terminal()
        {
            // Task finished
            println!("Task status: {}", task.status.label());
            println!(
                "Result: {}",
                task.result
                    .as_ref()
                    .map(|r| serde_json::to_string_pretty(r).unwrap_or_default())
                    .unwrap_or_else(|| "(none)".into())
            );

            assert_eq!(
                task.status.label(),
                "done",
                "task should complete successfully"
            );
            let output = task
                .result
                .as_ref()
                .and_then(|r| r.get("output"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // The model should respond with something containing "pong"
            assert!(
                output.to_lowercase().contains("pong"),
                "output should contain 'pong', got: {output}"
            );
            break;
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    cancel.cancel();
    worker_handle.await.unwrap();
}

/// Test that multiple tasks are processed sequentially by the serial worker.
#[tokio::test]
async fn worker_processes_multiple_tasks_sequentially() {
    let cli_check = tokio::process::Command::new("which")
        .arg("claude")
        .output()
        .await;
    if cli_check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("skipping: claude CLI not found");
        return;
    }

    let store = TaskStore::open_memory().expect("open store");
    let workspace = std::env::current_dir().expect("cwd");

    // Create two tasks
    let id1 = store
        .create(NewTask {
            kind: "test".into(),
            session_origin: "seq-test".into(),
            context: json!({"prompt": "Reply with exactly: alpha"}),
            parent: None,
            priority: 2, // higher priority, should run first
            metadata: json!({}),
        })
        .await
        .unwrap();

    let id2 = store
        .create(NewTask {
            kind: "test".into(),
            session_origin: "seq-test".into(),
            context: json!({"prompt": "Reply with exactly: beta"}),
            parent: None,
            priority: 1,
            metadata: json!({}),
        })
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let worker = TaskWorker::new(
        store.clone(),
        vec!["test".into()],
        "seq-worker".into(),
        workspace,
    )
    .with_poll_interval(Duration::from_millis(500));

    let cancel_clone = cancel.clone();
    let worker_handle = tokio::spawn(async move {
        worker.run(cancel_clone).await;
    });

    // Wait for both tasks to complete
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        if tokio::time::Instant::now() > deadline {
            cancel.cancel();
            worker_handle.await.unwrap();
            panic!("tasks did not complete within 180 seconds");
        }

        let t1 = store.get(id1).await;
        let t2 = store.get(id2).await;

        if t1.as_ref().map(|t| t.status.is_terminal()).unwrap_or(false)
            && t2.as_ref().map(|t| t.status.is_terminal()).unwrap_or(false)
        {
            let t1 = t1.unwrap();
            let t2 = t2.unwrap();
            println!("Task 1: {} — {}", t1.status.label(), t1.result.is_some());
            println!("Task 2: {} — {}", t2.status.label(), t2.result.is_some());

            // Both should be done
            assert_eq!(t1.status.label(), "done");
            assert_eq!(t2.status.label(), "done");
            break;
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    cancel.cancel();
    worker_handle.await.unwrap();
}

/// Verify the task board CLI commands work end-to-end with the store.
#[tokio::test]
async fn task_store_lifecycle() {
    let store = TaskStore::open_memory().expect("open store");

    // Create
    let id = store
        .create(NewTask {
            kind: "review".into(),
            session_origin: "lifecycle-test".into(),
            context: json!({"prompt": "review this code"}),
            parent: None,
            priority: 1,
            metadata: json!({"tag": "test"}),
        })
        .await
        .unwrap();

    // List - should find it
    let tasks = store
        .list(TaskFilter {
            kind: Some("review".into()),
            ..Default::default()
        })
        .await;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, id);

    // Claim
    let claimed = store.claim_next(&["review".into()], "agent-1").await;
    assert!(claimed.is_some());

    // Verify claimed state
    let task = store.get(id).await.unwrap();
    assert_eq!(task.status.label(), "claimed");
    assert_eq!(task.assigned_to.as_deref(), Some("agent-1"));

    // Complete
    store
        .complete(id, json!({"output": "looks good"}))
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status.label(), "done");
    assert_eq!(task.result.unwrap()["output"], "looks good");

    // Can't cancel a completed task
    let cancel_result = store.cancel(id, "oops".into()).await;
    assert!(cancel_result.is_err());

    // Stats
    let active = store.active_count().await;
    assert_eq!(active, 0);
}
