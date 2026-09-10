//! Windows-native ACP transport coverage. No shell, npm download, or credentials.
#![cfg(all(windows, feature = "native-fixture"))]

use futures::StreamExt;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel};

struct ProcessHandle(*mut std::ffi::c_void);
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
    fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
}
impl ProcessHandle {
    fn open(pid: u32) -> Self {
        let handle = unsafe { OpenProcess(0x00100000, 0, pid) }; // SYNCHRONIZE
        assert!(
            !handle.is_null(),
            "open live fixture process: {}",
            std::io::Error::last_os_error()
        );
        Self(handle)
    }
    fn assert_exited(&self) {
        assert_eq!(
            unsafe { WaitForSingleObject(self.0, 5000) },
            0,
            "fixture process was not reaped"
        );
    }
}
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn fixture(dir: &Path) -> PathBuf {
    let bin = dir.join("Native O'Brien 日本語 & space.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_harness-native-fixture"), &bin).unwrap();
    bin
}
fn request(cwd: &Path, prompt: &str, resume: Option<&str>) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: cwd.display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        attachments: Vec::new(),
        worktree: None,
        resume: resume.map(str::to_owned),
    }
}

async fn exercise(prompt: &str, resume: Option<&str>) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("Workspace O'Brien 日本語 & ! %");
    std::fs::create_dir(&cwd).unwrap();
    let harness = AcpHarness::grok().with_executable(fixture(dir.path()));
    let (steer_tx, steering) = mpsc::channel(4);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering,
        interrupt: interrupt.clone(),
    };
    let operation = async {
        let mut stream = harness
            .run(request(&cwd, prompt, resume), controls)
            .await
            .unwrap();
        let mut events = Vec::new();
        let mut peer = None;
        while let Some(event) = stream.next().await {
            let event = event.expect("native ACP event");
            if let AgentEvent::TextDelta { text } = &event {
                let echoed: Value = serde_json::from_str(text).expect("fixture echo");
                assert_eq!(echoed["prompt"], prompt);
                assert_eq!(Path::new(echoed["cwd"].as_str().unwrap()), cwd);
                assert_eq!(
                    echoed["argv"],
                    serde_json::json!(["--no-auto-update", "agent", "--no-leader", "stdio"])
                );
                if prompt == "wait-for-cancel" {
                    peer = Some(ProcessHandle::open(echoed["pid"].as_u64().unwrap() as u32));
                    interrupt.cancel();
                }
            }
            events.push(event);
        }
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::TextDelta { .. })),
            "missing native echo: {events:?}"
        );
        let expected_session = resume.unwrap_or("native-session");
        assert!(events.iter().any(|e| matches!(e, AgentEvent::SessionStarted { session_id, .. } if session_id == expected_session)), "wrong session: {events:?}");
        let statuses: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Done { status, error, .. } => {
                    assert!(error.is_none(), "{error:?}");
                    Some(*status)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![if prompt == "wait-for-cancel" {
                DoneStatus::Interrupted
            } else {
                DoneStatus::Completed
            }]
        );
        if let Some(peer) = peer {
            peer.assert_exited();
        }
    };
    tokio::time::timeout(Duration::from_secs(12), operation)
        .await
        .expect("native session deadline");
    drop(steer_tx);
    assert!(
        !cwd.join("injected.txt").exists(),
        "prompt was interpreted by a shell"
    );
}

#[tokio::test]
async fn native_stdio_preserves_unicode_paths_and_prompt_metacharacters() {
    exercise("hello 日本語 & echo injected > injected.txt | %PATH% !NAME! 'quoted' \"double\"\nsecond line", None).await;
}
#[tokio::test]
async fn native_session_load_round_trips_resume_identity() {
    exercise("resume echo", Some("native-resumed-session")).await;
}
#[tokio::test]
async fn native_protocol_cancel_settles_and_reaps_the_direct_child() {
    exercise("wait-for-cancel", None).await;
}

#[tokio::test]
async fn batch_overrides_are_rejected_before_any_command_runs() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("batch-was-executed.txt");
    let script = dir.path().join("unsupported.CmD");
    std::fs::write(&script, "@echo ran>batch-was-executed.txt\r\n").unwrap();
    let harnesses: Vec<Box<dyn Harness>> = vec![
        Box::new(AcpHarness::grok().with_executable(script.clone())),
        Box::new(zeron_harness::ClaudeHarness::new().with_executable(script.clone())),
        Box::new(zeron_harness::CodexHarness::new().with_executable(script)),
    ];
    for harness in harnesses {
        let (_steer, steering) = mpsc::channel(1);
        let controls = RunControls {
            request_input: Box::new(|_| {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(Vec::new());
                rx
            }),
            steering,
            interrupt: CancellationToken::new(),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            harness.run(request(dir.path(), "unsafe & | > %PATH%", None), controls),
        )
        .await
        .expect("batch rejection must precede startup");
        let error = match result {
            Err(error) => error.to_string(),
            Ok(_) => panic!("{} accepted a batch override", harness.display_name()),
        };
        assert!(
            error.contains(".exe"),
            "missing native executable guidance: {error}"
        );
        assert!(
            !marker.exists(),
            "{} executed the batch override",
            harness.display_name()
        );
    }
}
