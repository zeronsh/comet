//! #296 through the engine: completed tools must not park a pending ACP turn.
//! Separate binary because the diagnostic watchdog setting is process-wide.
use std::{sync::Arc, time::Duration};
use zeron_doc::{
    MessagePart, MessageRole, MessageStatus, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus,
};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::AcpHarness;
use zeron_proto::{HarnessId, RunRequest, SandboxLevel, SessionStatus};

async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("engine state must advance");
}

#[tokio::test]
async fn quiet_acp_prompt_stays_working_until_response() {
    // SAFETY: this is the only test in this binary, on a current-thread runtime,
    // and no engine or harness tasks have started yet.
    unsafe {
        std::env::set_var("ZERON_TURN_QUIESCE_MS", "100");
    }
    let dir = tempfile::tempdir().unwrap();
    let registry = HarnessRegistry::new();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../harness/tests/fixtures/acp-lifecycle.py");
    registry.register(Arc::new(AcpHarness::pi().with_executable(fixture)));
    let core = EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Pi, None).unwrap();
    let chat = "acp-quiet-regression";
    let handle = core.doc_host.open(chat).unwrap();
    let doc = handle.doc();
    let queue = |id: &str, payload| {
        doc.queue_command(&SessionCommandEntry {
            id: id.into(),
            payload,
            issued_by: "viewer".into(),
            issued_at: chrono::Utc::now().timestamp_millis(),
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        })
        .unwrap();
    };
    queue(
        "first",
        SessionCommandPayload::Run {
            message_id: "first-user".into(),
            request: RunRequest {
                prompt: "tools".into(),
                harness: None,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                cwd: dir.path().display().to_string(),
                sandbox: SandboxLevel::WorkspaceWrite,
                auto_approve: true,
                attachments: Vec::new(),
                worktree: None,
                resume: None,
            },
        },
    );
    wait_for(|| {
        doc.read_entries().unwrap_or_default().iter().any(|entry| {
            entry.parts.iter().any(
                |part| matches!(part, MessagePart::Tool { id, resolved: true, .. } if id == "3"),
            )
        })
    })
    .await;
    // Observe many watchdog windows during the peer's post-tool model wait.
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            core.sessions.session_status(chat).unwrap().status,
            SessionStatus::Working
        );
        assert!(
            !doc.read_entries()
                .unwrap()
                .iter()
                .any(|entry| entry.role == MessageRole::Assistant
                    && entry.status == Some(MessageStatus::Complete)),
            "pending turn was finalized"
        );
    }
    queue(
        "followup",
        SessionCommandPayload::Steer {
            prompt: "second".into(),
            message_id: Some("second-user".into()),
        },
    );
    wait_for(|| {
        doc.read_entries().unwrap_or_default().iter().any(|entry| {
            entry
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::Text { text, .. } if text == "second"))
        })
    })
    .await;
    wait_for(|| {
        core.sessions
            .session_status(chat)
            .is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    let entries = doc.read_entries().unwrap();
    let assistants: Vec<_> = entries
        .iter()
        .filter(|entry| entry.role == MessageRole::Assistant)
        .collect();
    assert_eq!(assistants.len(), 2, "{entries:?}");
    assert!(
        assistants
            .iter()
            .all(|entry| entry.status == Some(MessageStatus::Complete)),
        "{entries:?}"
    );
    assert!(
        assistants[0].parts.iter().any(
            |part| matches!(part, MessagePart::Text { text, .. } if text.contains("finished"))
        ),
        "original output must stay in its turn: {entries:?}"
    );
    // ACP can still emit unowned autonomous activity. It has no pending
    // session/prompt response, so the existing fallback must remain enabled.
    queue(
        "self-continue",
        SessionCommandPayload::Steer {
            prompt: "self-continue".into(),
            message_id: Some("third-user".into()),
        },
    );
    wait_for(|| {
        doc.read_entries().unwrap_or_default().iter().any(|entry| {
            entry.status == Some(MessageStatus::Complete)
                && entry.parts.iter().any(
                    |part| matches!(part, MessagePart::Text { text, .. } if text == "autonomous"),
                )
        })
    })
    .await;
    assert_eq!(
        core.sessions.session_status(chat).unwrap().status,
        SessionStatus::Idle
    );
}
