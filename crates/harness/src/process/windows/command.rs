//! Native command preparation, separate from handle ownership and launch.
use super::Stdio;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Globalization::CompareStringOrdinal;
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};

/// The agent command subset: argv, environment, cwd and standard streams.
/// No raw shell strings or process breakaway flags are exposed.
#[derive(Debug)]
pub struct Command {
    metadata: std::process::Command,
    clear_env: bool,
    pub(super) stdio: [Stdio; 3],
}
impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            metadata: std::process::Command::new(program),
            clear_env: false,
            stdio: [Stdio::inherit(); 3],
        }
    }
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.metadata.arg(arg);
        self
    }
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.metadata.args(args);
        self
    }
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.metadata.env(key, value);
        self
    }
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.metadata.env_remove(key);
        self
    }
    pub fn env_clear(&mut self) -> &mut Self {
        self.metadata.env_clear();
        self.clear_env = true;
        self
    }
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.metadata.current_dir(dir);
        self
    }
    pub fn stdin(&mut self, stdio: Stdio) -> &mut Self {
        self.stdio[0] = stdio;
        self
    }
    pub fn stdout(&mut self, stdio: Stdio) -> &mut Self {
        self.stdio[1] = stdio;
        self
    }
    pub fn stderr(&mut self, stdio: Stdio) -> &mut Self {
        self.stdio[2] = stdio;
        self
    }
    /// Windows managed children always terminate on owner drop, as in the
    /// previous managed spawn helper. Unix callers retain Tokio's flag behavior.
    pub fn kill_on_drop(&mut self, _enabled: bool) -> &mut Self {
        self
    }
    pub(crate) fn as_std_mut(&mut self) -> &mut std::process::Command {
        &mut self.metadata
    }

    pub(super) fn prepare(&self) -> io::Result<Prepared> {
        let program = self.metadata.get_program();
        let program_wide = wide(program)?;
        if program_wide.len() == 1 || program_wide.contains(&(b'"' as u16)) {
            return Err(invalid("empty or quoted executable name"));
        }
        let mut environment: Vec<(OsString, OsString)> = if self.clear_env {
            Vec::new()
        } else {
            std::env::vars_os().collect()
        };
        let mut child_path = None;
        for (key, value) in self.metadata.get_envs() {
            let encoded = wide(key)?;
            if encoded.len() == 1 || encoded[..encoded.len() - 1].contains(&(b'=' as u16)) {
                return Err(invalid("invalid environment variable name"));
            }
            environment.retain(|(existing, _)| compare(existing, key) != std::cmp::Ordering::Equal);
            if compare(key, OsStr::new("PATH")) == std::cmp::Ordering::Equal {
                child_path = value;
            }
            if let Some(value) = value {
                wide(value)?;
                environment.push((key.into(), value.into()));
            }
        }
        environment.sort_by(|(a, _), (b, _)| compare(a, b));
        let mut block = Vec::new();
        for (key, value) in environment {
            block.extend(key.encode_wide());
            block.push(b'=' as u16);
            block.extend(value.encode_wide());
            block.push(0);
        }
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        let executable = resolve(program, child_path)?;
        // No implicit interpreter is allowed, even for an explicit override.
        if executable
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
        {
            return Err(invalid("batch scripts are not native agent executables"));
        }
        let mut line = vec![b'"' as u16];
        line.extend_from_slice(&program_wide[..program_wide.len() - 1]);
        line.push(b'"' as u16);
        for arg in self.metadata.get_args() {
            line.push(b' ' as u16);
            quote(arg, &mut line)?;
        }
        line.push(0);
        if line.len() > 32767 {
            return Err(invalid("Windows command line exceeds 32767 UTF-16 units"));
        }
        Ok(Prepared {
            executable: wide(executable.as_os_str())?,
            line,
            environment: block,
            cwd: self
                .metadata
                .get_current_dir()
                .map(|dir| wide(dir.as_os_str()))
                .transpose()?,
        })
    }
}

pub(super) struct Prepared {
    pub executable: Vec<u16>,
    pub line: Vec<u16>,
    pub environment: Vec<u16>,
    pub cwd: Option<Vec<u16>>,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut result: Vec<_> = value.encode_wide().collect();
    if result.contains(&0) {
        return Err(invalid("embedded NUL"));
    }
    result.push(0);
    Ok(result)
}
// Windows environment keys use OS ordinal case folding, not Unicode lowercase.
fn compare(a: &OsStr, b: &OsStr) -> std::cmp::Ordering {
    let a: Vec<_> = a.encode_wide().collect();
    let b: Vec<_> = b.encode_wide().collect();
    let result =
        unsafe { CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1) };
    match result {
        1 => std::cmp::Ordering::Less,
        2 => std::cmp::Ordering::Equal,
        3 => std::cmp::Ordering::Greater,
        _ => unreachable!("valid environment keys"),
    }
}
fn resolve(program: &OsStr, child_path: Option<&OsStr>) -> io::Result<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        if !path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            let mut suffixed = program.to_os_string();
            suffixed.push(".exe");
            if Path::new(&suffixed).is_file() {
                return std::path::absolute(suffixed);
            }
        }
        return std::path::absolute(path);
    }
    // Rust's Windows search order: explicit child PATH, application directory,
    // system directories, parent PATH. No implicit cwd search or PATHEXT.
    let mut dirs = Vec::new();
    if let Some(path) = child_path {
        dirs.extend(std::env::split_paths(path));
    }
    if let Some(dir) = std::env::current_exe()?.parent() {
        dirs.push(dir.into());
    }
    for get_dir in [GetSystemDirectoryW, GetWindowsDirectoryW] {
        let mut buffer = vec![0u16; 32768];
        let len = unsafe { get_dir(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        if len > 0 && len < buffer.len() {
            dirs.push(OsString::from_wide(&buffer[..len]).into());
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.into_iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| {
            let mut candidate = dir.join(program);
            if !program.as_encoded_bytes().contains(&b'.') {
                candidate.set_extension("exe");
            }
            candidate
                .is_file()
                .then(|| std::path::absolute(candidate).ok())
                .flatten()
        })
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
}
// MS CRT argv rules: double backslashes before quotes and the closing quote.
// Always quoting preserves empty arguments; shell metacharacters stay literal.
fn quote(arg: &OsStr, line: &mut Vec<u16>) -> io::Result<()> {
    let encoded = wide(arg)?;
    line.push(b'"' as u16);
    let mut slashes = 0;
    for &unit in &encoded[..encoded.len() - 1] {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        let escaped = unit == b'"' as u16;
        line.extend(std::iter::repeat_n(
            b'\\' as u16,
            if escaped { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        line.push(unit);
    }
    line.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    line.push(b'"' as u16);
    Ok(())
}
