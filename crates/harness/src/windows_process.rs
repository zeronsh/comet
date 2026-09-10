//! Windows process-tree ownership. Job handles are never inherited by children.
use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::System::Console::HPCON;
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_PARENT_PROCESS, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    UpdateProcThreadAttribute,
};

/// Owns a session's entire process tree, including descendants of exited parents.
/// Closing the owner process also closes this handle and kills the job.
#[derive(Debug)]
pub struct Job(OwnedHandle);

/// Creation-time job and inherited-handle lists. Values are borrowed until
/// CreateProcessW returns; the opaque attribute buffer is word-aligned and owned.
pub struct Attributes<'a> {
    buffer: Vec<usize>,
    initialized: bool,
    _values: std::marker::PhantomData<&'a [std::os::windows::io::RawHandle]>,
}
impl<'a> Attributes<'a> {
    pub(crate) fn for_job(jobs: &'a [std::os::windows::io::RawHandle]) -> io::Result<Self> {
        let mut attrs = Self::new(1)?;
        attrs.add(
            PROC_THREAD_ATTRIBUTE_JOB_LIST,
            jobs.as_ptr().cast(),
            std::mem::size_of_val(jobs),
        )?;
        Ok(attrs)
    }
    pub(crate) fn for_parent(
        parent: &'a [std::os::windows::io::RawHandle; 1],
        handles: &'a [std::os::windows::io::RawHandle],
    ) -> io::Result<Self> {
        let mut attrs = Self::new(2)?;
        attrs.add(
            PROC_THREAD_ATTRIBUTE_PARENT_PROCESS,
            parent.as_ptr().cast(),
            std::mem::size_of_val(parent),
        )?;
        attrs.add(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            handles.as_ptr().cast(),
            std::mem::size_of_val(handles),
        )?;
        Ok(attrs)
    }

    /// Borrow a live pseudoconsole and job list through process creation.
    pub fn for_console(
        console: &'a HPCON,
        jobs: &'a [std::os::windows::io::RawHandle],
    ) -> io::Result<Self> {
        let mut attrs = Self::new(2)?;
        // PSEUDOCONSOLE takes the handle value, unlike the pointer-to-list attributes.
        attrs.add(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            *console as *const _,
            std::mem::size_of::<HPCON>(),
        )?;
        attrs.add(
            PROC_THREAD_ATTRIBUTE_JOB_LIST,
            jobs.as_ptr().cast(),
            std::mem::size_of_val(jobs),
        )?;
        Ok(attrs)
    }

    fn new(count: u32) -> io::Result<Self> {
        let mut bytes = 0;
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut attrs = Self {
            buffer: vec![0; bytes.div_ceil(std::mem::size_of::<usize>())],
            initialized: false,
            _values: std::marker::PhantomData,
        };
        if unsafe { InitializeProcThreadAttributeList(attrs.ptr(), count, 0, &mut bytes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        attrs.initialized = true;
        Ok(attrs)
    }

    // Private: constructors supply valid values borrowed for the list's lifetime.
    fn add(
        &mut self,
        attribute: u32,
        value: *const std::ffi::c_void,
        bytes: usize,
    ) -> io::Result<()> {
        if unsafe {
            UpdateProcThreadAttribute(
                self.ptr(),
                0,
                attribute as usize,
                value,
                bytes,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }
}
impl Drop for Attributes<'_> {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { DeleteProcThreadAttributeList(self.ptr()) };
        }
    }
}

impl Job {
    /// Borrow the non-inheritable job for a creation-time JOB_LIST attribute.
    pub fn as_handle(&self) -> BorrowedHandle<'_> {
        use std::os::windows::io::AsHandle;
        self.0.as_handle()
    }

    pub fn new() -> io::Result<Self> {
        // SAFETY: null security attributes create a non-inheritable, unnamed job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned an owned, valid handle.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the job and fixed-size information buffer remain valid for the call.
        if unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: this handle owns only processes assigned to this session.
        if unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
