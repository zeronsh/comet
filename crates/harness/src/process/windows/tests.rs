//! Exercise the actual creation interval, not just cleanup after successful spawn.
use super::*;
use std::io::BufRead;
use std::os::windows::io::AsHandle;
use std::os::windows::process::CommandExt;
use std::time::{Duration, Instant};
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
};

// Safety net for failed assertions: kill only the fixture process whose owned
// handle we acquired while it was alive, never by a recycled PID.
struct FixtureProcess(OwnedHandle);
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        unsafe { TerminateProcess(self.0.as_raw_handle(), 99) };
    }
}
impl FixtureProcess {
    fn assert_exited(&self) {
        assert_eq!(
            unsafe { WaitForSingleObject(self.0.as_raw_handle(), 5000) },
            WAIT_OBJECT_0
        );
    }
}
fn parked_command() -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "process::windows::tests::parked_child_helper"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[test]
fn parked_child_helper() {
    if let Ok(handle) = std::env::var("ZERON_TEST_INHERITED_EVENT") {
        // Signal only if the parent's sentinel handle accidentally survived the
        // launch allow-list. Handle reuse in this child cannot signal that event.
        unsafe {
            windows_sys::Win32::System::Threading::SetEvent(handle.parse::<usize>().unwrap() as _)
        };
    }
    if std::env::var_os("ZERON_TEST_PARKED_CHILD").is_some() {
        std::thread::sleep(Duration::from_secs(15));
    }
}

#[tokio::test]
async fn unrelated_inheritable_handles_are_not_passed_to_agents() {
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Threading::CreateEventW;
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let event = unsafe { CreateEventW(&security, 1, 0, std::ptr::null()) };
    assert!(!event.is_null());
    let event = unsafe { OwnedHandle::from_raw_handle(event) };
    let mut command = parked_command();
    command.env(
        "ZERON_TEST_INHERITED_EVENT",
        (event.as_raw_handle() as usize).to_string(),
    );
    let mut child = command.spawn().unwrap();
    assert!(child.wait().await.unwrap().success());
    assert_eq!(
        unsafe { WaitForSingleObject(event.as_raw_handle(), 0) },
        WAIT_TIMEOUT
    );
}

#[test]
fn creation_is_already_in_job_and_setup_failure_rolls_back() {
    let mut processes = Vec::new();
    let result = spawn_prepared(&parked_command(), |child| {
        processes.push(FixtureProcess(child.process.try_clone()?));
        processes.push(FixtureProcess(child.escrow.process.try_clone()?));
        let mut member = 0;
        assert_ne!(
            unsafe {
                IsProcessInJob(
                    child.process.as_raw_handle(),
                    child.job.as_handle().as_raw_handle(),
                    &mut member,
                )
            },
            0
        );
        assert_ne!(member, 0, "child was created outside its job");
        Err(io::Error::other("injected setup failure before resume"))
    });
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("injected setup failure")
    );
    for process in processes {
        process.assert_exited();
    }
}

#[test]
fn ordinary_subprocess_cannot_keep_agent_pipes_open() {
    use std::io::Read;
    let mut command = parked_command();
    command.stdout(Stdio::piped());
    let mut outsider = None;
    let mut reader = None;
    let result = spawn_prepared(&command, |child| {
        // A raw std launch deliberately does not participate in our allow-list.
        let foreign = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "process::windows::tests::parked_child_helper"])
            .env("ZERON_TEST_PARKED_CHILD", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        outsider = Some(foreign);
        let raw = child.stdout.as_ref().unwrap().as_raw_handle();
        reader = Some(std::fs::File::from(
            unsafe { std::os::windows::io::BorrowedHandle::borrow_raw(raw) }
                .try_clone_to_owned()?,
        ));
        Err(io::Error::other("injected setup failure"))
    });
    assert!(result.is_err());
    let mut outsider = outsider.unwrap();
    let guard = FixtureProcess(outsider.as_handle().try_clone_to_owned().unwrap());
    let (done, rx) = std::sync::mpsc::channel();
    let read = std::thread::spawn(move || {
        let _ = done.send(reader.unwrap().read_to_end(&mut Vec::new()));
    });
    let eof = rx.recv_timeout(Duration::from_secs(2));
    let still_running = outsider.try_wait().unwrap().is_none();
    outsider.kill().unwrap();
    outsider.wait().unwrap();
    read.join().unwrap();
    drop(guard);
    assert!(
        still_running,
        "the unrelated fixture exited before checking isolation"
    );
    assert!(
        eof.is_ok(),
        "an unrelated subprocess inherited an agent pipe"
    );
    assert_eq!(eof.unwrap().unwrap(), 0);
}

#[test]
fn crash_interval_owner_helper() {
    let Some(pid_file) = std::env::var_os("ZERON_TEST_CRASH_PID_FILE") else {
        return;
    };
    let mut command = parked_command();
    command.env("ZERON_TEST_PARKED_CHILD", "1");
    let _ = spawn_prepared(&command, |child| {
        std::fs::write(pid_file, format!("{},{}", child.pid, child.escrow.pid))?;
        // Tell the parent that CreateProcessW returned, without starting the
        // monitor or resuming the child. Parent kills this owner at this point.
        let mut gate = String::new();
        let _ = std::io::stdin().lock().read_line(&mut gate);
        std::process::exit(77);
    });
    panic!("crash owner must not finish launch");
}

#[test]
fn owner_death_immediately_after_creation_kills_suspended_child() {
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("pid");
    let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "process::windows::tests::crash_interval_owner_helper",
        ])
        .env("ZERON_TEST_CRASH_PID_FILE", &pid_file)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .unwrap();
    let owner_guard = FixtureProcess(owner.as_handle().try_clone_to_owned().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    let pids = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pids) = text
                .split(',')
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()
            {
                if pids.len() == 2 {
                    break pids;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "owner never reached creation interval"
        );
        assert!(
            owner.try_wait().unwrap().is_none(),
            "owner exited before creation"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let children: Vec<_> = pids
        .into_iter()
        .map(|pid| {
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
            assert!(!handle.is_null());
            FixtureProcess(unsafe { OwnedHandle::from_raw_handle(handle) })
        })
        .collect();
    owner.kill().unwrap(); // hard exit: none of the owner's Rust destructors run
    owner.wait().unwrap();
    for child in children {
        child.assert_exited();
    }
    drop(owner_guard);
}
