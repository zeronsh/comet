//! Synchronous pipe handles adapted by Tokio's blocking file I/O. These are not
//! std::process::ChildStdout handles, whose public conversion requires overlapped
//! handles. All local handles stay non-inheritable, including during launch.
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::io::{BorrowedHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;

#[derive(Clone, Copy, Debug)]
pub struct Stdio(Mode);
#[derive(Clone, Copy, Debug)]
enum Mode {
    Inherit,
    Null,
    Pipe,
}
impl Stdio {
    pub fn inherit() -> Self {
        Self(Mode::Inherit)
    }
    pub fn null() -> Self {
        Self(Mode::Null)
    }
    pub fn piped() -> Self {
        Self(Mode::Pipe)
    }

    pub(super) fn open(self, index: usize) -> io::Result<(OwnedHandle, Option<tokio::fs::File>)> {
        let input = index == 0;
        let null = || -> io::Result<OwnedHandle> {
            Ok(OpenOptions::new()
                .read(input)
                .write(!input)
                .open("NUL")?
                .into())
        };
        let (child, parent) = match self.0 {
            Mode::Null => (null()?, None),
            Mode::Inherit => {
                let handle = unsafe {
                    GetStdHandle([STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE][index])
                };
                let owned = if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    null()?
                } else {
                    unsafe { BorrowedHandle::borrow_raw(handle) }.try_clone_to_owned()?
                };
                (owned, None)
            }
            Mode::Pipe => {
                let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
                if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: successful CreatePipe returns two newly owned handles.
                let (read, write) = unsafe {
                    (
                        OwnedHandle::from_raw_handle(read),
                        OwnedHandle::from_raw_handle(write),
                    )
                };
                let (child, parent) = if input { (read, write) } else { (write, read) };
                (child, Some(tokio::fs::File::from_std(File::from(parent))))
            }
        };
        Ok((child, parent))
    }
}
