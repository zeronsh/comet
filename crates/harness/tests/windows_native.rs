//! Windows-native ACP transport coverage. No npm download or credentials;
//! `.cmd`/`.bat` shims launch through a wrapped cmd.exe with literal arguments.
#![cfg(all(windows, feature = "native-fixture"))]

use futures::StreamExt;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use zeron_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel};

#[test]
fn managed_process_protocol_progresses_with_one_blocking_worker() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        use zeron_harness::process::Stdio;
        let mut command =
            zeron_harness::process::Command::new(env!("CARGO_BIN_EXE_harness-native-fixture"));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let reply = tokio::time::timeout(Duration::from_secs(5), async {
            stdin
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
                .await
                .unwrap();
            lines.next_line().await.unwrap().unwrap()
        })
        .await;
        // Release even when the progress assertion fails.
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        let reply: Value = serde_json::from_str(&reply.expect("protocol I/O starved")).unwrap();
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], 1);
    });
}

#[tokio::test]
async fn output_captures_both_streams_with_default_or_null_stdio() {
    for null_streams in [false, true] {
        let mut command =
            zeron_harness::process::Command::new(env!("CARGO_BIN_EXE_harness-native-fixture"));
        command.arg("--capture-output");
        if null_streams {
            command
                .stdout(zeron_harness::process::Stdio::null())
                .stderr(zeron_harness::process::Stdio::null());
        }
        let output = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"captured stdout");
        assert_eq!(output.stderr, b"captured stderr");
    }
}

struct ProcessHandle(*mut std::ffi::c_void);

#[tokio::test]
async fn native_launch_matches_tokio_argv_environment_cwd_and_path() {
    use zeron_harness::process::{Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    let exe = fixture(dir.path());
    let arguments = [
        "",
        "plain",
        "a b",
        "tab\there",
        "quote\"here",
        "\\",
        "trailing \\",
        "slashes\\\\\"quote",
        "line\nbreak",
        "日本語 😀",
        "%PATH% & | > < ^ ! $()",
    ];
    for clear in [false, true] {
        // Search the explicit child PATH with the .exe suffix omitted.
        let name = exe.file_stem().unwrap();
        let mut native = Command::new(name);
        let mut baseline = tokio::process::Command::new(name);
        if clear {
            native.env_clear();
            baseline.env_clear();
        }
        native
            .arg("--launch-report")
            .args(arguments)
            .current_dir(dir.path())
            .env("PATH", dir.path())
            .env("zeron_launch_marker", "old")
            .env("ZERON_LAUNCH_MARKER", "new 日本語")
            .env("ZERON_LAUNCH_REMOVED", "old")
            .env_remove("zeron_launch_removed")
            .env("ZERON_ä_KEY", "unicode value")
            .stdin(Stdio::null());
        baseline
            .arg("--launch-report")
            .args(arguments)
            .current_dir(dir.path())
            .env("PATH", dir.path())
            .env("zeron_launch_marker", "old")
            .env("ZERON_LAUNCH_MARKER", "new 日本語")
            .env("ZERON_LAUNCH_REMOVED", "old")
            .env_remove("zeron_launch_removed")
            .env("ZERON_ä_KEY", "unicode value")
            .stdin(std::process::Stdio::null())
            .creation_flags(0x08000000)
            .kill_on_drop(true);
        let expected = baseline.output().await.unwrap();
        let actual = tokio::time::timeout(Duration::from_secs(5), native.output())
            .await
            .unwrap()
            .unwrap();
        assert!(expected.status.success() && actual.status.success());
        let expected: Value = serde_json::from_slice(&expected.stdout).unwrap();
        let actual: Value = serde_json::from_slice(&actual.stdout).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual["argv"], serde_json::json!(arguments));
        assert_eq!(actual["marker"], "new 日本語");
        assert_eq!(actual["removed"], Value::Null);
        assert_eq!(actual["unicode"], "unicode value");
    }
}

#[tokio::test]
async fn invalid_launch_inputs_fail_without_starting_a_child() {
    use zeron_harness::process::Command;
    let exe = env!("CARGO_BIN_EXE_harness-native-fixture");
    for command in [
        Command::new(exe).arg("NUL\0argument"),
        Command::new(exe).env("BAD=KEY", "value"),
        Command::new(exe).env("KEY", "NUL\0value"),
        Command::new(exe).current_dir("NUL\0cwd"),
    ] {
        assert_eq!(
            command.spawn().unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let mut missing = Command::new(dir.path().join("missing.exe"));
    assert_eq!(
        missing.spawn().unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    let mut bad_cwd = Command::new(exe);
    bad_cwd.current_dir(dir.path().join("missing"));
    assert!(bad_cwd.spawn().is_err());
}
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
    async fn assert_exited(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while unsafe { WaitForSingleObject(self.0, 0) } != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "fixture process was not reaped"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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
            peer.assert_exited().await;
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

async fn exercise_tree(prompt: &str, drop_stream: bool) {
    let dir = tempfile::tempdir().unwrap();
    let harness = AcpHarness::grok()
        .with_executable(fixture(dir.path()))
        .with_graces(Duration::from_millis(100), Duration::from_millis(100));
    let (_steer, steering) = mpsc::channel(1);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| {
            let (_, rx) = oneshot::channel();
            rx
        }),
        steering,
        interrupt: interrupt.clone(),
    };
    let mut stream = harness
        .run(request(dir.path(), prompt, None), controls)
        .await
        .unwrap();
    let handles = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if let AgentEvent::TextDelta { text } = event.unwrap() {
                let value: Value = serde_json::from_str(&text).unwrap();
                let mut pids = vec![value["pid"].as_u64().unwrap() as u32];
                pids.extend(
                    value["descendants"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_u64().unwrap() as u32),
                );
                assert_eq!(pids.len(), 3);
                return pids
                    .into_iter()
                    .map(ProcessHandle::open)
                    .collect::<Vec<_>>();
            }
        }
        panic!("missing tree metadata");
    })
    .await
    .expect("tree startup deadline");
    if drop_stream {
        drop(stream);
    } else {
        interrupt.cancel();
        let mut statuses = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                if let AgentEvent::Done { status, .. } = event.unwrap() {
                    statuses.push(status);
                }
            }
        })
        .await
        .expect("unresponsive agent must settle before fixture watchdog");
        assert_eq!(statuses, vec![DoneStatus::Interrupted]);
    }
    for handle in handles {
        handle.assert_exited().await;
    }
}

#[tokio::test]
async fn unresponsive_cancel_kills_child_and_grandchild_holding_stdio() {
    exercise_tree("ignore-cancel-tree", false).await;
}

#[tokio::test]
async fn dropping_run_stream_kills_the_tree() {
    exercise_tree("ignore-cancel-tree", true).await;
}

#[tokio::test]
async fn cooperative_cancel_also_cleans_up_descendants() {
    exercise_tree("cooperative-tree", false).await;
}

#[tokio::test]
async fn process_exit_drains_buffered_output_despite_inherited_descendant_pipes() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = zeron_harness::process::Command::new(fixture(dir.path()));
    command
        .arg("--output-tree")
        .current_dir(dir.path())
        .stdin(zeron_harness::process::Stdio::null())
        .stdout(zeron_harness::process::Stdio::piped())
        .stderr(zeron_harness::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // Like the protocol drivers, wait for pipe EOF before calling child.wait().
    // Root-exit monitoring must terminate descendants independently of wait().
    tokio::time::timeout(Duration::from_secs(5), async {
        use tokio::io::AsyncReadExt;
        tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))
    })
    .await
    .expect("descendant pipes must close on root exit")
    .unwrap();
    assert!(child.wait().await.unwrap().success());
    assert_eq!(
        out,
        format!("{}FINAL-STDOUT", "o".repeat(128 * 1024)).as_bytes()
    );
    assert_eq!(
        err,
        format!("{}FINAL-STDERR", "e".repeat(128 * 1024)).as_bytes()
    );
}

/// Runs only when spawned by `batch_overrides_launch_through_cmd`: records
/// the argv the inner program received under the batch shim (everything after
/// the `--` separator, so agent-shaped flags stay literal filters).
#[test]
fn batch_override_helper() {
    let Some(file) = std::env::var_os("ZERON_TEST_BATCH_ARGS_FILE") else {
        return;
    };
    let argv: Vec<String> = std::env::args().collect();
    let forwarded = match argv.iter().position(|arg| arg == "--") {
        Some(index) => argv[index + 1..].to_vec(),
        None => Vec::new(),
    };
    std::fs::write(file, forwarded.join("\n")).unwrap();
}

#[tokio::test]
async fn batch_overrides_launch_through_cmd() {
    let dir = tempfile::tempdir().unwrap();
    let received = dir.path().join("batch-args.txt");
    let injected = dir.path().join("injected.txt");
    let script = dir.path().join("shim.CmD");
    std::fs::write(
        &script,
        format!(
            "@echo off\r\n\"{}\" --exact batch_override_helper -- %*\r\n",
            std::env::current_exe().unwrap().display()
        ),
    )
    .unwrap();
    let harnesses: Vec<Box<dyn Harness>> = vec![
        Box::new(AcpHarness::grok().with_executable(script.clone())),
        Box::new(zeron_harness::ClaudeHarness::new().with_executable(script.clone())),
        Box::new(zeron_harness::CodexHarness::new().with_executable(script)),
    ];
    for harness in harnesses {
        let expected_prefix: Vec<String> = match harness.id() {
            HarnessId::Grok => vec![
                "--no-auto-update".into(),
                "agent".into(),
                "--no-leader".into(),
                "stdio".into(),
            ],
            HarnessId::ClaudeCode => vec!["--print".into()],
            HarnessId::Codex => vec!["app-server".into()],
            other => panic!("unexpected harness: {other:?}"),
        };
        let _ = std::fs::remove_file(&received);
        // The harness owns its child's environment; reach the helper through
        // the inherited process env. No other test in this binary reads it.
        // SAFETY: written before any child exists in this iteration.
        unsafe { std::env::set_var("ZERON_TEST_BATCH_ARGS_FILE", &received) };
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
        // Consume the whole stream: the shim cannot speak any agent protocol,
        // so the run must fail loudly — but only AFTER a safe launch.
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            harness.run(request(dir.path(), "prompt & | > %PATH%", None), controls),
        )
        .await
        .expect("override launch must not stall");
        match result {
            Ok(mut stream) => {
                let mut surfaced_failure = false;
                while let Some(event) = stream.next().await {
                    match event {
                        Err(_) | Ok(AgentEvent::Done { error: Some(_), .. }) => {
                            surfaced_failure = true;
                        }
                        Ok(_) => {}
                    }
                }
                assert!(
                    surfaced_failure,
                    "{} accepted a batch override as a protocol peer",
                    harness.display_name()
                );
            }
            Err(error) => {
                let error = error.to_string();
                assert!(
                    !error.contains("batch"),
                    "batch scripts must not be rejected as overrides: {error}"
                );
            }
        }
        let output = std::fs::read_to_string(&received)
            .unwrap_or_else(|_| panic!("{} launched nothing", harness.display_name()));
        let forwarded: Vec<&str> = output.lines().collect();
        assert!(
            forwarded.len() >= expected_prefix.len()
                && forwarded
                    .iter()
                    .zip(&expected_prefix)
                    .all(|(got, want)| got == want),
            "{} forwarded {forwarded:?}, expected it to start with {expected_prefix:?}",
            harness.display_name()
        );
        assert!(
            !injected.exists(),
            "{} let a prompt through a shell interpretation",
            harness.display_name()
        );
        // SAFETY: no other test in this binary reads this variable.
        unsafe { std::env::remove_var("ZERON_TEST_BATCH_ARGS_FILE") };
    }
}

#[tokio::test]
async fn application_exit_without_destructors_kills_owned_processes() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    // The owner is deliberately outside our job wrapper. It creates its own
    // managed job, then exits without running Drop after the test opens handles.
    let mut owner = tokio::process::Command::new(fixture(dir.path()))
        .arg("--job-owner")
        .current_dir(dir.path())
        .creation_flags(0x08000000)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(owner.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("owner startup deadline")
        .unwrap()
        .unwrap();
    let pids: Vec<u32> = serde_json::from_str(&line).unwrap();
    assert_eq!(pids.len(), 2);
    let handles: Vec<_> = pids.into_iter().map(ProcessHandle::open).collect();
    owner
        .stdin
        .take()
        .unwrap()
        .write_all(b"exit\n")
        .await
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), owner.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    for handle in handles {
        handle.assert_exited().await;
    }
}
