//! Windows-only native agent launcher. A job-contained inert parent supplies
//! creation-time membership and stdio; no post-creation assignment or local
//! inheritable handles can escape into unrelated subprocess launches.
mod command;
mod escrow;
mod stdio;
#[cfg(test)]
mod tests;
pub use command::Command;
pub use stdio::Stdio;
pub type ChildStdin = tokio::fs::File;
pub type ChildStdout = tokio::fs::File;

use crate::windows_process::Job;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::process::{ExitStatus, Output};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, WaitForSingleObject,
};

#[derive(Debug)]
pub struct Child {
    process: OwnedHandle,
    pid: u32,
    status: Option<ExitStatus>,
    exited: tokio::sync::oneshot::Receiver<()>,
    pub(super) job: Arc<Job>,
    escrow: escrow::Escrow,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<tokio::fs::File>,
}
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.job.terminate();
    }
}
impl Child {
    pub fn id(&self) -> Option<u32> {
        self.status.is_none().then_some(self.pid)
    }
    pub fn start_kill(&mut self) -> io::Result<()> {
        self.job.terminate()
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_some() {
            return Ok(self.status);
        }
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                self.status = Some(ExitStatus::from_raw(code));
                let _ = self.job.terminate();
                Ok(self.status)
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        // Closing stdin allows children waiting for EOF to exit, like Tokio.
        drop(self.stdin.take());
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }
        (&mut self.exited)
            .await
            .map_err(|_| io::Error::other("agent exit monitor stopped"))?;
        self.try_wait()?
            .ok_or_else(|| io::Error::other("agent exit wait failed"))
    }
    pub async fn wait_with_output(mut self) -> io::Result<Output> {
        drop(self.stdin.take());
        let (mut stdout, mut stderr) = (self.stdout.take(), self.stderr.take());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (status, _, _) = tokio::try_join!(
            self.wait(),
            async {
                if let Some(pipe) = &mut stdout {
                    pipe.read_to_end(&mut out).await?;
                }
                Ok::<_, io::Error>(())
            },
            async {
                if let Some(pipe) = &mut stderr {
                    pipe.read_to_end(&mut err).await?;
                }
                Ok::<_, io::Error>(())
            }
        )?;
        Ok(Output {
            status,
            stdout: out,
            stderr: err,
        })
    }
}

impl Command {
    pub fn spawn(&mut self) -> io::Result<Child> {
        spawn_prepared(self, |_| Ok(()))
    }

    pub async fn output(&mut self) -> io::Result<Output> {
        self.stdout(Stdio::piped()).stderr(Stdio::piped());
        self.spawn()?.wait_with_output().await
    }
}

// A private hook makes the critical interval testable: the process exists but
// is still suspended, and no monitor has been started. Production uses a no-op.
fn spawn_prepared(
    command: &Command,
    after_create: impl FnOnce(&Child) -> io::Result<()>,
) -> io::Result<Child> {
    let mut prepared = command.prepare()?;
    let job = Arc::new(Job::new()?);
    let escrow = escrow::Escrow::new(&job)?;
    let (input, stdin) = command.stdio[0].open(0)?;
    let (output, stdout) = command.stdio[1].open(1)?;
    let (error, stderr) = command.stdio[2].open(2)?;
    let handles = escrow.duplicate(&[input, output, error])?;
    let parent = [escrow.process.as_raw_handle()];
    let mut attrs = crate::windows_process::Attributes::for_parent(&parent, &handles)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = attrs.ptr();
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: buffers/attribute values and all borrowed handles remain alive
    // through creation. The job handle is excluded from inherited handles.
    if unsafe {
        CreateProcessW(
            prepared.executable.as_ptr(),
            prepared.line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_NO_WINDOW
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            prepared.environment.as_ptr().cast(),
            prepared
                .cwd
                .as_ref()
                .map_or(std::ptr::null(), |cwd| cwd.as_ptr()),
            &startup.StartupInfo,
            &mut info,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // Child owns the job immediately: every subsequent fallible setup step is
    // protected by Drop. A hard owner crash is protected by kernel job membership.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
    let (done, exited) = tokio::sync::oneshot::channel();
    let child = Child {
        process,
        pid: info.dwProcessId,
        status: None,
        exited,
        job,
        escrow,
        stdin,
        stdout,
        stderr,
    };
    child.escrow.release(&handles)?;
    after_create(&child)?;
    let process = child.process.try_clone()?;
    let cleanup_job = child.job.clone();
    // Do not consume Tokio blocking capacity needed by the child's own pipes.
    std::thread::Builder::new()
        .name("agent-exit".into())
        .spawn(move || {
            unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
            let _ = cleanup_job.terminate();
            let _ = done.send(());
        })?;
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok(child)
}
