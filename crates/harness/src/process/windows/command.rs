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
        let mut child_pathext = None;
        for (key, value) in self.metadata.get_envs() {
            let encoded = wide(key)?;
            if encoded.len() == 1 || encoded[..encoded.len() - 1].contains(&(b'=' as u16)) {
                return Err(invalid("invalid environment variable name"));
            }
            environment.retain(|(existing, _)| compare(existing, key) != std::cmp::Ordering::Equal);
            if compare(key, OsStr::new("PATH")) == std::cmp::Ordering::Equal {
                child_path = value;
            }
            if compare(key, OsStr::new("PATHEXT")) == std::cmp::Ordering::Equal {
                child_pathext = value;
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
        let executable = resolve(program, child_path, child_pathext)?;
        let (application, line) = if is_batch_script(&executable) {
            // npm exposes CLIs as `.cmd`/`.bat` shims. Run them through
            // `cmd.exe /d /s /c` with cross-spawn-style per-argument escaping:
            // the shim itself is the only interpreted layer, agent arguments
            // stay literal, and the Job Object still owns the whole tree
            // through cmd.exe's membership.
            let command = batch_command(&executable, self.metadata.get_args())?;
            command
        } else {
            let mut line = vec![b'"' as u16];
            line.extend_from_slice(&program_wide[..program_wide.len() - 1]);
            line.push(b'"' as u16);
            for arg in self.metadata.get_args() {
                line.push(b' ' as u16);
                quote(arg, &mut line)?;
            }
            (wide(executable.as_os_str())?, line)
        };
        let mut line = line;
        line.push(0);
        if line.len() > 32767 {
            return Err(invalid("Windows command line exceeds 32767 UTF-16 units"));
        }
        Ok(Prepared {
            executable: application,
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

fn is_batch_script(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
}

/// Extensions tried when a bare program name is searched on PATH, in PATHEXT
/// order (the default matches `cmd.exe`). Only what `CreateProcessW` can run
/// directly (`.exe`/`.com`) or what the batch wrapper above can launch
/// (`.bat`/`.cmd`) is considered.
fn launchable_extensions(child_pathext: Option<&OsStr>) -> Vec<&'static str> {
    const DEFAULT: [&str; 4] = ["exe", "cmd", "bat", "com"];
    let Some(pathext) = child_pathext else {
        return DEFAULT.to_vec();
    };
    let text = pathext.to_string_lossy();
    let launchable: Vec<&'static str> = text
        .split(';')
        .map(|entry| entry.trim().trim_start_matches('.'))
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            DEFAULT
                .iter()
                .find(|ok| entry.eq_ignore_ascii_case(ok))
                .copied()
        })
        .collect();
    if launchable.is_empty() {
        DEFAULT.to_vec()
    } else {
        launchable
    }
}

/// Build `"<cmd.exe>" /d /s /c "<shim> <args…>"`.
fn batch_command<'a>(
    batch: &Path,
    args: impl IntoIterator<Item = &'a OsStr>,
) -> io::Result<(Vec<u16>, Vec<u16>)> {
    let interpreter = system_directory()?.join("cmd.exe");
    let mut line = Vec::new();
    quote_batch_argument(interpreter.as_os_str(), true, &mut line)?;
    for flag in ["/d", "/s", "/c"] {
        line.push(b' ' as u16);
        line.extend(flag.encode_utf16());
    }
    line.push(b' ' as u16);
    line.push(b'"' as u16);
    quote_batch_argument(batch.as_os_str(), true, &mut line)?;
    for arg in args {
        line.push(b' ' as u16);
        quote_batch_argument(arg, false, &mut line)?;
    }
    line.push(b'"' as u16);
    Ok((wide(interpreter.as_os_str())?, line))
}

fn system_directory() -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if len == 0 || len >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(len);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

/// One cmd.exe argument, following cross-spawn's escaping: backslashes before
/// a quote (and at the end) are doubled, embedded quotes are backslash-escaped,
/// and everything is wrapped in quotes so spaces and `=`-shaped arguments stay
/// whole. `metacharacters` (the shim path itself, never agent arguments) also
/// neutralizes cmd's `()  %  !  ^  "  backtick  <  >  &  |` with carets and
/// strips newlines and tabs — a path, unlike a prompt, never needs them.
fn quote_batch_argument(arg: &OsStr, metacharacters: bool, line: &mut Vec<u16>) -> io::Result<()> {
    let encoded = wide(arg)?;
    let units = &encoded[..encoded.len() - 1];
    line.push(b'"' as u16);
    let mut slashes = 0;
    for &unit in units {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        let escaped_quote = unit == b'"' as u16;
        line.extend(std::iter::repeat_n(
            b'\\' as u16,
            if escaped_quote {
                slashes * 2 + 1
            } else {
                slashes
            },
        ));
        slashes = 0;
        if metacharacters
            && matches!(
                unit as u8,
                b'(' | b')' | b'%' | b'!' | b'^' | b'`' | b'<' | b'>' | b'&' | b'|'
            )
        {
            line.push(b'^' as u16);
        }
        if metacharacters && matches!(unit, 0x0d | 0x0a | 0x09) {
            line.push(b' ' as u16);
        } else {
            line.push(unit);
        }
    }
    line.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    line.push(b'"' as u16);
    Ok(())
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
fn resolve(
    program: &OsStr,
    child_path: Option<&OsStr>,
    child_pathext: Option<&OsStr>,
) -> io::Result<PathBuf> {
    let parent_pathext = std::env::var_os("PATHEXT");
    let pathext = launchable_extensions(child_pathext.or(parent_pathext.as_deref()));
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        if !program.as_encoded_bytes().contains(&b'.') {
            for extension in &pathext {
                let mut suffixed = program.to_os_string();
                suffixed.push(".");
                suffixed.push(extension);
                if Path::new(&suffixed).is_file() {
                    return std::path::absolute(suffixed);
                }
            }
        }
        // An explicit `.cmd`/`.bat` (and any real file) resolves as given.
        return std::path::absolute(path);
    }
    // Rust's Windows search order: explicit child PATH, application directory,
    // system directories, parent PATH. No implicit cwd search; PATHEXT names
    // the extension variants per directory (`.cmd`/`.bat` become batch spawns).
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
    let named_variants = |dir: &Path| -> Vec<PathBuf> {
        if program.as_encoded_bytes().contains(&b'.') {
            return vec![dir.join(program)];
        }
        pathext
            .iter()
            .map(|extension| {
                let mut name = program.to_os_string();
                name.push(".");
                name.push(extension);
                dir.join(name)
            })
            .collect()
    };
    dirs.into_iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| named_variants(&dir))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| std::path::absolute(candidate).ok())
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
