//! Approval modes, command queue execution, and permission hardening (P0-P2,P4-P9).

mod common;

use std::time::Duration;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use chaos_core::{
    engine::CoreEvent,
    mcp,
    permission::{PermissionId, PermissionState},
    types::*,
};
use common::*;
use serde_json::json;

async fn wait_permission_state(
    h: &mut Harness,
    state: PermissionState,
) -> chaos_core::permission::PermissionRequest {
    match h
        .wait_for(
            "permission event",
            move |e| matches!(e, CoreEvent::Permission(r) if r.state == state),
        )
        .await
    {
        CoreEvent::Permission(r) => r,
        _ => unreachable!(),
    }
}

async fn wait_run_state(
    h: &Harness,
    id: PermissionId,
    want: &str,
) -> chaos_core::permission::PermissionRequest {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let req = h
            .core
            .with_engine(move |e, _| chaos_core::permission::get(e.conn(), id).unwrap())
            .await
            .unwrap()
            .unwrap();
        if req.run_state.as_deref() == Some(want) {
            return req;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for run_state {want}; got {req:#?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn exec_perm(command: &str, cwd: &str) -> serde_json::Value {
    json!({"permission_kind": {
        "title": command,
        "kind": "execute",
        "raw_input": {"command": command, "cwd": cwd}
    }})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_set_approval_mode_is_not_agent_authorized_or_auto_allowed() {
    for protocol in [1, 2] {
        let mut h = Harness::new().await;
        let _bot = h
            .agent(
                "bot",
                script(json!({"protocol": protocol, "turns": [{"steps": [
                    {"permission_kind": {"title": "a tool", "name": "chat_set_approval_mode", "kind": "execute"}},
                    {"text": "done"}
                ]}]})),
            )
            .await;
        h.say("@bot try to change policy").await;
        let all = {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            loop {
                let o = h
                    .fake("bot")
                    .observed
                    .lock()
                    .unwrap()
                    .permission_outcomes
                    .clone();
                if !o.is_empty() || tokio::time::Instant::now() > deadline {
                    break o;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        assert_eq!(all.len(), 1, "v{protocol}: {all:#?}");
        assert_eq!(
            all[0]["outcome"]["optionId"],
            json!("deny"),
            "v{protocol}: {}",
            all[0]
        );

        // The request must not be auto-approved as a named chat tool.
        let log = h.core.permission_log(h.main, 50).await.unwrap();
        let row = log
            .iter()
            .find(|r| r.title.as_ref() == "chat_set_approval_mode")
            .expect("policy-change request logged");
        assert_ne!(
            row.state,
            PermissionState::AutoApproved,
            "v{protocol}: {row:#?}"
        );

        // Calling the chat tool directly is also rejected for agents.
        let scope = mcp::Scope {
            agent: h.id("bot"),
            channel: h.main,
            epoch: 0,
        };
        let err = h
            .core
            .tool(
                scope,
                "chat_set_approval_mode",
                json!({"scope": "workspace", "mode": "yolo"}),
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("human") || err.contains("authorized") || err.contains("not allowed"),
            "v{protocol}: {err}"
        );
        // Effective mode remains unchanged.
        let st = h
            .core
            .tool(scope, "chat_approval_mode_status", json!({}))
            .await
            .unwrap();
        assert_eq!(st["effective"], json!("ask-all"), "v{protocol}: {st}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_mode_override_precedence_applies_to_new_requests() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", script(json!({"turns": []}))).await;
    let scope = mcp::Scope {
        agent: bot.id,
        channel: h.main,
        epoch: 0,
    };

    h.core
        .set_approval_mode(bot.id, ApprovalMode::AcceptEdits)
        .await
        .unwrap();
    let st = h
        .core
        .tool(scope, "chat_approval_mode_status", json!({}))
        .await
        .unwrap();
    assert_eq!(st["effective"], json!("accept-edits"), "{st}");

    h.core
        .set_workspace_approval_mode(Some(ApprovalMode::Yolo))
        .await
        .unwrap();
    let st = h
        .core
        .tool(scope, "chat_approval_mode_status", json!({}))
        .await
        .unwrap();
    assert_eq!(st["effective"], json!("yolo"), "{st}");

    h.core
        .set_project_approval_mode(project_id(), Some(ApprovalMode::AskAll))
        .await
        .unwrap();
    let st = h
        .core
        .tool(scope, "chat_approval_mode_status", json!({}))
        .await
        .unwrap();
    assert_eq!(st["effective"], json!("ask-all"), "{st}");

    h.core
        .set_channel_approval_mode(h.main, Some(ApprovalMode::AcceptEdits))
        .await
        .unwrap();
    let st = h
        .core
        .tool(scope, "chat_approval_mode_status", json!({}))
        .await
        .unwrap();
    assert_eq!(st["effective"], json!("accept-edits"), "{st}");

    // Clearing a higher-priority override falls back correctly.
    h.core
        .set_channel_approval_mode(h.main, None)
        .await
        .unwrap();
    let st = h
        .core
        .tool(scope, "chat_approval_mode_status", json!({}))
        .await
        .unwrap();
    assert_eq!(st["effective"], json!("ask-all"), "{st}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_command_denied_never_executes_and_approval_executes_once() {
    let mut h = Harness::new().await;
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let marker = std::env::temp_dir().join(format!("cc-deny-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let cmd = format!("echo ran > '{}'", marker.display());

    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                exec_perm(&cmd, &cwd),
                exec_perm("echo approved", &cwd),
                {"text": "done"}
            ]}]})),
        )
        .await;
    h.core
        .set_approval_mode(bot.id, ApprovalMode::AcceptEdits)
        .await
        .unwrap();

    h.say("@bot run commands").await;
    let first = wait_permission_state(&mut h, PermissionState::Pending).await;
    let second = wait_permission_state(&mut h, PermissionState::Pending).await;
    // both queued for execution
    let req1 = h
        .core
        .with_engine(move |e, _| {
            chaos_core::permission::get(e.conn(), first.id)
                .unwrap()
                .unwrap()
        })
        .await
        .unwrap();
    let req2 = h
        .core
        .with_engine(move |e, _| {
            chaos_core::permission::get(e.conn(), second.id)
                .unwrap()
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!(req1.run_state.as_deref(), Some("queued"), "{req1:#?}");
    assert_eq!(req2.run_state.as_deref(), Some("queued"), "{req2:#?}");

    h.core
        .answer_permission(req1.id, h.hx.id, false, None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!marker.exists(), "denied queued command must never execute");
    let denied = h
        .core
        .with_engine(move |e, _| {
            chaos_core::permission::get(e.conn(), req1.id)
                .unwrap()
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!(denied.run_state.as_deref(), Some("denied"), "{denied:#?}");

    h.core
        .answer_permission(req2.id, h.hx.id, true, None)
        .await
        .unwrap();
    let done = wait_run_state(&h, req2.id, "done").await;
    assert_eq!(done.run_exit_code, Some(0), "{done:#?}");
    assert!(
        done.run_output
            .as_deref()
            .unwrap_or("")
            .contains("approved"),
        "{done:#?}"
    );
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_command_or_cwd_fails_closed_for_queued_execution() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                {"permission_kind": {"title": "run", "kind": "execute", "raw_input": {"command": "echo hi"}}},
                {"permission_kind": {"title": "run2", "kind": "execute", "raw_input": {"cwd": "/tmp"}}},
                {"text": "done"}
            ]}]})),
        )
        .await;
    h.core
        .set_approval_mode(bot.id, ApprovalMode::AcceptEdits)
        .await
        .unwrap();
    // ensure no agent cwd fallback confusion by not relying on it; clear by not setting, but agent has cwd.
    // To force cwd-unrunnable, we need a case where neither extracted nor agent cwd resolves.
    // We'll still get unrunnable if command missing; for cwd missing case, simulate by having agent cwd unknown:
    // easier: request with command but cwd absent and agent cwd removed.
    let _ = h
        .core
        .with_engine(move |e, _| {
            e.set_agent_cwd(
                bot.id,
                std::path::PathBuf::from("/definitely/not/a/real/dir-for-cc-test"),
            );
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .unwrap();

    h.say("@bot queue").await;
    let a = wait_permission_state(&mut h, PermissionState::Pending).await;
    let b = wait_permission_state(&mut h, PermissionState::Pending).await;
    let ra = h
        .core
        .with_engine(move |e, _| {
            chaos_core::permission::get(e.conn(), a.id)
                .unwrap()
                .unwrap()
        })
        .await
        .unwrap();
    let rb = h
        .core
        .with_engine(move |e, _| {
            chaos_core::permission::get(e.conn(), b.id)
                .unwrap()
                .unwrap()
        })
        .await
        .unwrap();
    // first has command but cwd resolves to non-existent dir? we stored resolved cwd string if agent_cwd exists;
    // it becomes queued but execution must fail closed at run time (cwd not available). second missing command -> unrunnable.
    assert_eq!(rb.run_state.as_deref(), Some("unrunnable"), "{rb:#?}");

    // Approving unrunnable stays unrunnable/does not execute.
    h.core
        .answer_permission(rb.id, h.hx.id, true, None)
        .await
        .unwrap();
    let rb2 = h
        .core
        .with_engine(move |e, _| {
            chaos_core::permission::get(e.conn(), b.id)
                .unwrap()
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!(rb2.run_state.as_deref(), Some("unrunnable"), "{rb2:#?}");

    // First: queued (cwd was "resolved" to a path string) but directory invalid -> execution fails closed.
    h.core
        .answer_permission(ra.id, h.hx.id, true, None)
        .await
        .unwrap();
    let failed = wait_run_state(&h, ra.id, "failed").await;
    let out = failed.run_output.as_deref().unwrap_or("").to_lowercase();
    assert!(
        out.contains("working directory")
            || out.contains("not available")
            || out.contains("refusing"),
        "{failed:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_zero_exit_and_launch_failure_are_reported_to_agent() {
    let mut h = Harness::new().await;
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                exec_perm("sh -lc 'echo oops 1>&2; exit 7'", &cwd),
                exec_perm("echo ok", "/path/that/does/not/exist/cc"),
                {"text": "done"}
            ]}]})),
        )
        .await;
    h.core
        .set_approval_mode(bot.id, ApprovalMode::AcceptEdits)
        .await
        .unwrap();
    h.say("@bot run").await;
    let a = wait_permission_state(&mut h, PermissionState::Pending).await;
    let b = wait_permission_state(&mut h, PermissionState::Pending).await;

    h.core
        .answer_permission(a.id, h.hx.id, true, None)
        .await
        .unwrap();
    let failed = wait_run_state(&h, a.id, "failed").await;
    assert_eq!(failed.run_exit_code, Some(7), "{failed:#?}");
    assert!(
        failed.run_output.as_deref().unwrap_or("").contains("oops"),
        "{failed:#?}"
    );

    h.core
        .answer_permission(b.id, h.hx.id, true, None)
        .await
        .unwrap();
    let failed2 = wait_run_state(&h, b.id, "failed").await;
    let out = failed2.run_output.as_deref().unwrap_or("").to_lowercase();
    assert!(
        out.contains("working directory") || out.contains("failed"),
        "{failed2:#?}"
    );

    // Agent receives result notifications in channel history.
    let history = h.history().await;
    let bodies: Vec<String> = history.iter().map(|m| m.body.to_string()).collect();
    assert!(
        bodies
            .iter()
            .any(|b| b.contains(&format!("#{}", a.id)) && b.contains("failed")),
        "{bodies:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_and_oversized_output_are_handled() {
    let _guard = ENV_LOCK.lock().await;
    // SAFETY: serialized by ENV_LOCK to avoid leaking into other tests.
    unsafe {
        std::env::set_var("CHAOS_COMMAND_TIMEOUT_SECS", "1");
        std::env::set_var("CHAOS_COMMAND_MAX_OUTPUT_BYTES", "256");
    }
    let mut h = Harness::new().await;
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                exec_perm("sleep 5", &cwd),
                exec_perm("python3 - <<'PY'\nprint('A'*5000)\nPY", &cwd),
                {"text": "done"}
            ]}]})),
        )
        .await;
    h.core
        .set_approval_mode(bot.id, ApprovalMode::AcceptEdits)
        .await
        .unwrap();
    h.say("@bot run").await;
    let a = wait_permission_state(&mut h, PermissionState::Pending).await;
    let b = wait_permission_state(&mut h, PermissionState::Pending).await;

    h.core
        .answer_permission(a.id, h.hx.id, true, None)
        .await
        .unwrap();
    let timed = wait_run_state(&h, a.id, "failed").await;
    assert!(
        timed
            .run_output
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("timed out"),
        "{timed:#?}"
    );

    h.core
        .answer_permission(b.id, h.hx.id, true, None)
        .await
        .unwrap();
    let outreq = wait_run_state(&h, b.id, "done").await;
    let out = outreq.run_output.as_deref().unwrap_or("");
    assert!(
        out.len() <= 600,
        "output should be bounded/truncated, got {}",
        out.len()
    );
    assert!(
        out.contains("truncated") || out.contains("A"),
        "{outreq:#?}"
    );

    unsafe {
        std::env::remove_var("CHAOS_COMMAND_TIMEOUT_SECS");
        std::env::remove_var("CHAOS_COMMAND_MAX_OUTPUT_BYTES");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_process_inheriting_pipes_does_not_hang_runner() {
    let mut h = Harness::new().await;
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                exec_perm("sleep 30 &", &cwd),
                {"text": "done"}
            ]}]})),
        )
        .await;
    h.core
        .set_approval_mode(bot.id, ApprovalMode::AcceptEdits)
        .await
        .unwrap();
    h.say("@bot run").await;
    let req = wait_permission_state(&mut h, PermissionState::Pending).await;
    h.core
        .answer_permission(req.id, h.hx.id, true, None)
        .await
        .unwrap();
    // Shell exits immediately but background child inherits stdio; runner must
    // terminate the process group and report completion instead of hanging.
    let done = wait_run_state(&h, req.id, "failed").await;
    let out = done.run_output.as_deref().unwrap_or("").to_lowercase();
    assert!(
        out.contains("background processes were terminated") || out.contains("did not close"),
        "{done:#?}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_process_with_redirected_pipes_is_terminated() {
    let mut h = Harness::new().await;
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let pid_file = std::env::temp_dir().join(format!(
        "cc-bg-redirected-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let escaped_path = pid_file.to_string_lossy().replace('\'', "'\\''");
    let command = format!("sleep 30 >/dev/null 2>&1 & echo $! > '{escaped_path}'");
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                exec_perm(&command, &cwd),
                {"text": "done"}
            ]}]})),
        )
        .await;
    h.core
        .set_approval_mode(bot.id, ApprovalMode::AcceptEdits)
        .await
        .unwrap();
    h.say("@bot run").await;
    let req = wait_permission_state(&mut h, PermissionState::Pending).await;
    h.core
        .answer_permission(req.id, h.hx.id, true, None)
        .await
        .unwrap();

    let done = wait_run_state(&h, req.id, "failed").await;
    assert!(
        done.run_output
            .as_deref()
            .unwrap_or("")
            .contains("background processes remained"),
        "{done:#?}"
    );
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("background child writes its pid")
        .trim()
        .parse()
        .expect("background child pid is numeric");
    let _ = std::fs::remove_file(&pid_file);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let alive = process_alive(pid);
        if !alive || tokio::time::Instant::now() >= deadline {
            assert!(!alive, "background child {pid} survived command completion");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether `pid` is still running. A zombie has exited and only waits for its
/// (possibly non-reaping) new parent, so it does not count.
fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let state = stat
            .rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().next())
            .unwrap_or("");
        !matches!(state, "Z" | "X" | "")
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
