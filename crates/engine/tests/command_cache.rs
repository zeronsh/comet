//! A running session feeds the command cache, so an active chat never pays for
//! a discovery probe.

use std::sync::Arc;

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::Harness;
use zeron_harness::mock::MockHarness;
use zeron_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, SlashCommand};

const CHAT: &str = "chat-commands";

fn registry_with(harness: Arc<dyn Harness>) -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(harness);
    Arc::new(registry)
}

fn script() -> Vec<AgentEvent> {
    vec![
        AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: "/tmp/project".into(),
            session_id: "hs-1".into(),
            assistant_message_id: "a-1".into(),
        },
        AgentEvent::AvailableCommands {
            commands: vec![SlashCommand {
                name: "ask-matt".into(),
                description: "A project skill".into(),
                input_hint: None,
            }],
        },
        AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("hs-1".into()),
        },
    ]
}

#[tokio::test]
async fn a_running_session_fills_the_command_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness: Arc<dyn Harness> = Arc::new(MockHarness { script: script() });
    let core = EngineCore::assemble(dir.path(), registry_with(harness), HarnessId::Mock, None)
        .expect("engine core assembles");

    let request = RunRequest {
        prompt: "hi".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp/project".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
        // Upstream's worktree-send durability work (#159) added this field.
        // This test is about the command cache, so it runs in the cwd itself.
        worktree: None,
    };
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, request, None)
        .await
        .expect("dispatch");

    let cache = core.sessions.command_cache();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let cached = cache
            .get(HarnessId::Mock, Some("/tmp/project"), |_| async {
                Err::<Vec<SlashCommand>, String>("probe".into())
            })
            .await;
        if let Ok(commands) = cached {
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].name, "ask-matt");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run never fed the cache"
        );
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
}
