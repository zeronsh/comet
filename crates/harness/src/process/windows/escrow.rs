//! Isolate inheritable handles from unrelated std/Tokio CreateProcess calls.
//! HANDLE_LIST alone only protects incoming inheritance: making local handles
//! inheritable would still leak them into concurrent ordinary subprocesses.
//! Microsoft's documented solution is an inert parent process holding duplicates:
//! https://devblogs.microsoft.com/oldnewthing/20200306-00/?p=103538
//!
//! This suspended copy of our image never executes application code. It joins
//! the job atomically, then the agent inherits that job and its selected handles
//! through PARENT_PROCESS. Keep it alive until agent exit so parent-liveness
//! checks work. Job termination also kills the escrow; no helper binary ships.
use crate::windows_process::{Attributes, Job};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use windows_sys::Win32::Foundation::{
    DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessW, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, PROCESS_INFORMATION, STARTUPINFOEXW, TerminateProcess,
};

#[derive(Debug)]
pub(super) struct Escrow {
    pub(super) process: OwnedHandle,
    #[cfg(test)]
    pub(super) pid: u32,
}
impl Drop for Escrow {
    fn drop(&mut self) {
        // This handle can only refer to our never-resumed helper. On an early
        // return before agent ownership exists, dropping the job is a backstop.
        unsafe { TerminateProcess(self.process.as_raw_handle(), 1) };
    }
}
impl Escrow {
    pub(super) fn new(job: &Job) -> io::Result<Self> {
        let image: Vec<u16> = std::env::current_exe()?
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let jobs = [job.as_handle().as_raw_handle()];
        let mut attrs = Attributes::for_job(&jobs)?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attrs.ptr();
        let mut info = PROCESS_INFORMATION::default();
        // No inherited handles and no execution. Explicit image, no command
        // interpreter, no global job assignment to the application itself.
        if unsafe {
            CreateProcessW(
                image.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NO_WINDOW | CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                std::ptr::null(),
                &startup.StartupInfo,
                &mut info,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
        // Closing the thread handle does not resume it.
        drop(unsafe { OwnedHandle::from_raw_handle(info.hThread) });
        Ok(Self {
            process,
            #[cfg(test)]
            pid: info.dwProcessId,
        })
    }
    pub(super) fn duplicate(&self, handles: &[OwnedHandle]) -> io::Result<Vec<RawHandle>> {
        handles
            .iter()
            .map(|handle| {
                let mut remote = std::ptr::null_mut();
                // These handle values belong to the escrow's table, not ours. They
                // must never be wrapped in OwnedHandle in this process. Escrow/job
                // destruction closes all duplicates, including partial failures.
                if unsafe {
                    DuplicateHandle(
                        GetCurrentProcess(),
                        handle.as_raw_handle(),
                        self.process.as_raw_handle(),
                        &mut remote,
                        0,
                        1,
                        DUPLICATE_SAME_ACCESS,
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(remote)
            })
            .collect()
    }
    pub(super) fn release(&self, handles: &[RawHandle]) -> io::Result<()> {
        for &handle in handles {
            let mut temporary = std::ptr::null_mut();
            // Move each remote duplicate back as non-inheritable, then close it.
            // Keeping the helper alive must not keep agent pipes artificially open.
            if unsafe {
                DuplicateHandle(
                    self.process.as_raw_handle(),
                    handle,
                    GetCurrentProcess(),
                    &mut temporary,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            drop(unsafe { OwnedHandle::from_raw_handle(temporary) });
        }
        Ok(())
    }
}
