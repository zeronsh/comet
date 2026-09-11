//! Actual CodexHarness JSON-RPC decoding through the engine's document writer.
//! Fixtures run offline; no installed agent or model account is involved.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, SubagentStatus};
use zeron_engine::{EngineCore, EngineProfile, HarnessRegistry};
use zeron_harness::CodexHarness;
use zeron_proto::{HarnessId, RunRequest, SandboxLevel, SessionStatus};

const CHAT: &str = "codex-subagents";

fn assemble(dir: &Path) -> (EngineCore, EngineProfile) {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/tests/fixtures/fake-codex.sh");
    let registry = Arc::new(HarnessRegistry::new());
    registry.register(Arc::new(CodexHarness::new().with_executable(fixture)));
    let profile = EngineProfile::development(dir, "test-org", "test-user");
    let core = EngineCore::assemble_with_profile(profile.clone(), registry, HarnessId::Codex, None)
        .unwrap();
    (core, profile)
}

fn entries(core: &EngineCore, doc: &str) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(doc)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap_or_default()
}

fn text(entries: &[SessionMessageEntry], role: MessageRole) -> String {
    entries
        .iter()
        .filter(|e| e.role == role)
        .flat_map(|e| &e.parts)
        .filter_map(|p| match p {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn check_persistence(
    scenario: &str,
    alpha_status: SubagentStatus,
    alpha_text: &str,
    alpha_users: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let (core, profile) = assemble(dir.path());
    let request = RunRequest {
        prompt: format!("scenario:{scenario}"),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: dir.path().display().to_string(),
        sandbox: SandboxLevel::ReadOnly,
        auto_approve: true,
        attachments: vec![],
        worktree: None,
        resume: None,
    };
    core.sessions
        .dispatch(
            CHAT,
            HarnessId::Codex,
            request.clone(),
            Some("user-prompt".into()),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if core
                .sessions
                .session_status(CHAT)
                .is_some_and(|s| s.status == SessionStatus::Idle)
                && entries(&core, CHAT).iter().any(|e| {
                    e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("parent completes");
    let parent = entries(&core, CHAT);
    let chips: Vec<_> = parent
        .iter()
        .flat_map(|e| &e.parts)
        .filter_map(|p| match p {
            MessagePart::Tool {
                id,
                call,
                subagent_ref,
                subagent_status,
                ..
            } if call.is_subagent_spawn() => {
                Some((id.clone(), subagent_ref.clone(), *subagent_status))
            }
            _ => None,
        })
        .collect();
    let alpha_doc = format!("{CHAT}--sub--spawn-alpha");
    let beta_doc = format!("{CHAT}--sub--spawn-beta");
    assert_eq!(
        chips,
        [
            (
                "spawn-alpha".into(),
                Some(alpha_doc.clone()),
                Some(alpha_status)
            ),
            (
                "spawn-beta".into(),
                Some(beta_doc.clone()),
                Some(SubagentStatus::Done)
            ),
        ]
    );
    for part in parent.iter().flat_map(|e| &e.parts) {
        if let MessagePart::Tool {
            call, subagent_ref, ..
        } = part
            && !call.is_subagent_spawn()
        {
            assert!(subagent_ref.is_none());
        }
    }
    assert!(!text(&parent, MessageRole::Assistant).contains("alpha"));
    assert!(!text(&parent, MessageRole::Assistant).contains("beta"));
    let alpha = entries(&core, &alpha_doc);
    let beta = entries(&core, &beta_doc);
    assert_eq!(text(&alpha, MessageRole::Assistant), alpha_text);
    assert_eq!(text(&alpha, MessageRole::User), alpha_users);
    assert!(text(&beta, MessageRole::Assistant).contains("beta answer"));
    assert!(
        alpha
            .iter()
            .chain(&beta)
            .all(|e| e.status == Some(MessageStatus::Complete))
    );
    core.shutdown().await;
    drop(core);

    // Check persisted identities directly without opening phantom documents.
    let db = rusqlite::Connection::open(profile.store_root().join("docs.sqlite3")).unwrap();
    let ids: Vec<String> = db.prepare("SELECT doc_id FROM snapshots WHERE doc_id LIKE 'codex-subagents--sub--%' ORDER BY doc_id").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(ids, [alpha_doc.clone(), beta_doc.clone()]);
    drop(db);
    let (reopened, _) = assemble(dir.path());
    assert_eq!(entries(&reopened, &alpha_doc), alpha);
    assert_eq!(entries(&reopened, &beta_doc), beta);
    let mut request = request;
    request.prompt = "scenario:resumed-child".into();
    request.resume = Some(
        if scenario.starts_with("v1") {
            "resume-with-child-v1"
        } else {
            "resume-with-child-v2"
        }
        .into(),
    );
    reopened
        .sessions
        .dispatch(
            CHAT,
            HarnessId::Codex,
            request,
            Some("user-followup".into()),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if text(&entries(&reopened, &alpha_doc), MessageRole::Assistant)
                .ends_with("resumed alpha\n\n")
                && reopened
                    .sessions
                    .session_status(CHAT)
                    .is_some_and(|s| s.status == SessionStatus::Idle)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("resumed child appends to its original document");
    assert_eq!(
        text(&entries(&reopened, &alpha_doc), MessageRole::Assistant),
        format!("{alpha_text}resumed alpha\n\n")
    );
    let parent = entries(&reopened, CHAT);
    assert_eq!(
        parent
            .iter()
            .flat_map(|e| &e.parts)
            .filter(|p| matches!(p, MessagePart::Tool { call, .. } if call.is_subagent_spawn()))
            .count(),
        2
    );
    assert!(parent.iter().flat_map(|e| &e.parts).any(|p| matches!(p,
        MessagePart::Tool { id, subagent_status: Some(SubagentStatus::Done), .. } if id == "spawn-alpha"
    )));
    reopened.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn v1_children_persist_under_their_spawn_chips() {
    check_persistence(
        "v1-subagents",
        SubagentStatus::Done,
        "alpha answer",
        "Inspect alpha",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_followup_appends_to_the_original_document_and_updates_the_original_chip() {
    check_persistence(
        "v2-lifecycle",
        SubagentStatus::Failed,
        "first alpha\n\nsecond alpha\n\n",
        "First assignment",
    )
    .await;
}
