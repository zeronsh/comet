//! ConPTY launch with atomic job membership. portable-pty's Windows launcher
//! runs the child before returning, so assigning its returned child is too late
//! to contain processes created by shell startup scripts.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::sync::{Arc, Mutex};

use portable_pty::{Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize};
use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, WaitForSingleObject,
};
use zeron_harness::windows_process::{Attributes, Job};

/// One teardown operation shared by natural exit, close, and shutdown. Empty
/// resource slots mean ownership moved, not that ConPTY and its reader closed.
pub(super) struct Cleanup {
    result: Mutex<Option<bool>>,
    ready: std::sync::Condvar,
    changed: tokio::sync::Notify,
}
impl Cleanup {
    pub(super) fn start(session: &mut super::LiveTerminal) -> Arc<Self> {
        if let Some(cleanup) = &session.cleanup {
            return cleanup.clone();
        }
        let cleanup = Arc::new(Self {
            result: Mutex::new(None),
            ready: std::sync::Condvar::new(),
            changed: tokio::sync::Notify::new(),
        });
        session.cleanup = Some(cleanup.clone());
        let master = session.master.take();
        let writer = session.writer.take();
        let reader = session.reader_thread.take();
        let completion = cleanup.clone();
        // Independent of Tokio: synchronous close/final-owner destruction can
        // wait even on a current-thread runtime. Keep reading during ConPTY close.
        if std::thread::Builder::new()
            .name("pty-close".into())
            .spawn(move || {
                drop(writer);
                drop(master);
                completion.finish(reader.is_none_or(|reader| reader.join().is_ok()));
            })
            .is_err()
        {
            cleanup.finish(false);
        }
        cleanup
    }

    fn finish(&self, success: bool) {
        *super::lock(&self.result) = Some(success);
        self.ready.notify_all();
        self.changed.notify_waiters();
    }

    pub(super) fn wait(&self, timeout: std::time::Duration) -> bool {
        let (result, _) = self
            .ready
            .wait_timeout_while(super::lock(&self.result), timeout, |result| {
                result.is_none()
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *result == Some(true)
    }

    pub(super) async fn wait_async(&self) -> bool {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(result) = *super::lock(&self.result) {
                return result;
            }
            changed.await;
        }
    }
}

struct Console(HPCON);
impl Drop for Console {
    fn drop(&mut self) {
        // SAFETY: this is the unique owner of a live pseudoconsole.
        unsafe { ClosePseudoConsole(self.0) };
    }
}

struct Master {
    console: Console,
    reader: File,
    writer: Mutex<Option<File>>,
    size: Mutex<PtySize>,
}
impl MasterPty for Master {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        hresult(unsafe { ResizePseudoConsole(self.console.0, coord(size)) })?;
        *super::lock(&self.size) = size;
        Ok(())
    }
    fn get_size(&self) -> anyhow::Result<PtySize> {
        Ok(*super::lock(&self.size))
    }
    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn io::Read + Send>> {
        Ok(Box::new(self.reader.try_clone()?))
    }
    fn take_writer(&self) -> anyhow::Result<Box<dyn io::Write + Send>> {
        Ok(Box::new(super::lock(&self.writer).take().ok_or_else(
            || io::Error::other("PTY writer already taken"),
        )?))
    }
}

#[derive(Debug)]
struct Process {
    handle: OwnedHandle,
    thread: Option<OwnedHandle>,
    pid: u32,
    job: Arc<Job>,
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.job.terminate();
    }
}
#[derive(Debug)]
struct Killer(Arc<Job>);
impl ChildKiller for Killer {
    fn kill(&mut self) -> io::Result<()> {
        self.0.terminate()
    }
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(Self(self.0.clone()))
    }
}
impl ChildKiller for Process {
    fn kill(&mut self) -> io::Result<()> {
        self.job.terminate()
    }
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(Killer(self.job.clone()))
    }
}
impl Process {
    fn wait_for(&self, timeout: u32) -> io::Result<Option<ExitStatus>> {
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), timeout) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.handle.as_raw_handle(), &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                self.job.terminate()?;
                Ok(Some(ExitStatus::with_exit_code(code)))
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
impl Child for Process {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_for(0)
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        // Terminals starts the output reader before scheduling this wait. Even
        // shell startup output is therefore drained, and earlier setup failures
        // can drop a never-started process without blocking ClosePseudoConsole.
        if let Some(thread) = self.thread.take() {
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
        }
        self.wait_for(INFINITE)?
            .ok_or_else(|| io::Error::other("PTY wait returned without exit"))
    }
    fn process_id(&self) -> Option<u32> {
        Some(self.pid)
    }
    fn as_raw_handle(&self) -> Option<RawHandle> {
        Some(self.handle.as_raw_handle())
    }
}

fn coord(size: PtySize) -> COORD {
    COORD {
        X: size.cols as i16,
        Y: size.rows as i16,
    }
}
fn hresult(result: i32) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::other(format!(
            "ConPTY failed: HRESULT {result:#x}"
        )))
    } else {
        Ok(())
    }
}
fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut result: Vec<_> = value.encode_wide().collect();
    if result.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "embedded NUL"));
    }
    result.push(0);
    Ok(result)
}
fn pipe() -> io::Result<(File, File)> {
    let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: output pointers are valid; null attributes disable inheritance.
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned two distinct owned handles.
    Ok(unsafe { (File::from_raw_handle(read), File::from_raw_handle(write)) })
}

pub(super) fn open(
    shell: &str,
    cwd: &str,
    size: PtySize,
) -> anyhow::Result<(Box<dyn MasterPty + Send>, Box<dyn Child + Send + Sync>)> {
    // Only an executable name is accepted, matching Terminals::open_with_shell.
    // Quotes cannot occur in a Windows file name; reject instead of interpreting.
    anyhow::ensure!(!shell.contains('"'), "invalid shell executable name");
    // Retain portable-pty's registry-refreshed Windows environment, including
    // PATH updates made after Zeron started.
    let builder = CommandBuilder::new(shell);
    let executable = builder
        .get_env("PATH")
        .and_then(|path| {
            std::env::split_paths(path)
                .filter(|dir| !dir.as_os_str().is_empty())
                .find_map(|dir| {
                    let candidate = dir.join(shell);
                    if candidate.is_file() {
                        Some(candidate)
                    } else if candidate.extension().is_none()
                        && candidate.with_extension("exe").is_file()
                    {
                        Some(candidate.with_extension("exe"))
                    } else {
                        None
                    }
                })
        })
        .unwrap_or_else(|| shell.into());
    let executable = wide(executable.as_os_str())?;
    let mut command = vec![b'"' as u16];
    command.extend_from_slice(&executable[..executable.len() - 1]);
    command.extend([b'"' as u16, 0]);
    let cwd = wide(OsStr::new(cwd))?;
    let mut environment = std::collections::BTreeMap::new();
    let env_key = |key: &OsStr| -> OsString {
        key.to_str()
            .map(|key| key.to_lowercase().into())
            .unwrap_or_else(|| key.to_owned())
    };
    for (key, value) in std::env::vars_os() {
        let value = builder.get_env(&key).unwrap_or(&value).to_owned();
        environment.insert(env_key(&key), (key, value));
    }
    for (key, value) in builder.iter_full_env_as_str() {
        environment.insert(env_key(OsStr::new(key)), (key.into(), value.into()));
    }
    for (key, value) in [
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "Zeron"),
    ] {
        environment.insert(env_key(OsStr::new(key)), (key.into(), value.into()));
    }
    let mut block = Vec::new();
    for (_, (key, value)) in environment {
        block.extend(key.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    let job = Arc::new(Job::new()?);
    let (input, writer) = pipe()?;
    let (reader, output) = pipe()?;
    let mut console = 0;
    hresult(unsafe {
        CreatePseudoConsole(
            coord(size),
            input.as_raw_handle(),
            output.as_raw_handle(),
            0,
            &mut console,
        )
    })?;
    let console = Console(console);
    drop(input);
    drop(output);
    let jobs = [job.as_handle().as_raw_handle()];
    let mut attrs = Attributes::for_console(&console.0, &jobs)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    startup.lpAttributeList = attrs.ptr();
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: all buffers and the initialized attribute list remain alive through
    // CreateProcessW. Job membership is established as part of creation, even
    // if the owner exits before this call returns. Suspension is only for the
    // output-reader startup ordering, not job assignment.
    if unsafe {
        CreateProcessW(
            executable.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            block.as_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: these two handles are newly owned on successful CreateProcessW.
    let handle = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
    let child = Process {
        handle,
        thread: Some(thread),
        pid: info.dwProcessId,
        job,
    };
    drop(attrs); // Process creation has finished borrowing the console and job list.
    Ok((
        Box::new(Master {
            console,
            reader,
            writer: Mutex::new(Some(writer)),
            size: Mutex::new(size),
        }),
        Box::new(child),
    ))
}
