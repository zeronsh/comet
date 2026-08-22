//! Live probe: drive the real grok CLI through AcpHarness and print the
//! event stream, verifying subagent spawn correlation + the disk-tailed
//! transcript end-to-end. Needs a logged-in `grok` on PATH.
//!
//!     cargo run -p zeron-harness --example grok_subagent_probe -- /tmp/probe-dir

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use zeron_proto::{AgentEvent, RunRequest, SandboxLevel, UserInputAnswer};

#[tokio::main]
async fn main() {
    let cwd = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/probe-grok-viz".into());
    std::fs::create_dir_all(&cwd).unwrap();
    let (_steer_tx, steering) = mpsc::channel(8);
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: q.options.first().cloned().into_iter().collect(),
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = RunRequest {
        prompt: "Use spawn_subagent to launch ONE subagent of type general with description \
                 'Viz probe' and prompt: 'Run the terminal command: echo viz-probe-ok && sleep 3. \
                 Then reply with the word finished.'. Wait for it with \
                 get_command_or_subagent_output and tell me its result."
            .into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd,
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    let mut stream = AcpHarness::grok()
        .run(request, controls)
        .await
        .expect("run starts");
    // stderr is unbuffered; a SIGTERM'd run still shows everything.
    let mut tagged = 0u32;
    let mut parent_done = false;
    loop {
        // After the parent turn settles the session stays alive for the
        // steering mailbox — bound the wait for the subagent's tagged Done.
        let ev = match tokio::time::timeout(std::time::Duration::from_secs(90), stream.next()).await
        {
            Ok(Some(ev)) => ev,
            Ok(None) => break,
            Err(_) => {
                eprintln!("--- timed out waiting (parent_done={parent_done}, tagged={tagged})");
                std::process::exit(2);
            }
        };
        match ev {
            Ok(AgentEvent::Subagent {
                parent_tool_use_id,
                event,
            }) => {
                tagged += 1;
                eprintln!("SUB[{parent_tool_use_id}] {event:?}");
                if matches!(*event, AgentEvent::Done { .. }) && parent_done {
                    eprintln!("--- tagged events total: {tagged}");
                    std::process::exit(0);
                }
            }
            Ok(AgentEvent::Done { status, .. }) => {
                parent_done = true;
                eprintln!("EV Done({status:?}) [parent]");
            }
            Ok(AgentEvent::TextDelta { text }) => eprintln!("TXT {}", text.trim_end()),
            Ok(AgentEvent::ReasoningDelta { .. }) => {}
            Ok(other) => eprintln!("EV {other:?}"),
            Err(e) => eprintln!("ERR {e}"),
        }
    }
    eprintln!("--- stream ended; tagged events total: {tagged}");
}
