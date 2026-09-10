//! Cross-platform executable discovery shared by native and ACP harnesses.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
    Unix,
    Windows,
}

impl Platform {
    pub(crate) fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    home_dir_with(&|key| std::env::var_os(key), Platform::current())
}

/// Resolve a portable user base directory without assuming Unix's `/` exists.
pub(crate) fn home_or_current_dir() -> PathBuf {
    home_or_current_dir_with(
        &|key| std::env::var_os(key),
        &std::env::current_dir,
        Platform::current(),
    )
}

fn home_or_current_dir_with(
    env: &impl Fn(&str) -> Option<OsString>,
    current_dir: &impl Fn() -> std::io::Result<PathBuf>,
    platform: Platform,
) -> PathBuf {
    home_dir_with(env, platform)
        .or_else(|| current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Reject Windows batch-script overrides before `Command` can implicitly route
/// them through `cmd.exe`. Native-agent overrides must remain shell-free.
pub(crate) fn validate_native_override(path: &Path) -> Result<PathBuf, crate::HarnessError> {
    validate_native_override_with(path, Platform::current())
}

fn validate_native_override_with(
    path: &Path,
    platform: Platform,
) -> Result<PathBuf, crate::HarnessError> {
    let is_batch = platform == Platform::Windows
        && path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
            });
    if is_batch {
        return Err(crate::HarnessError::NotInstalled(format!(
            "{} is a Windows batch script; configure the native .exe executable instead",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

pub(crate) fn find_on_paths(exe: &str, extra: Vec<PathBuf>) -> Option<PathBuf> {
    find_on_paths_with(
        exe,
        extra,
        &|key| std::env::var_os(key),
        crate::shell_env::login_shell_path().map(OsString::from),
        Platform::current(),
    )
}

fn home_dir_with(env: &impl Fn(&str) -> Option<OsString>, platform: Platform) -> Option<PathBuf> {
    env("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            (platform == Platform::Windows)
                .then(|| env("USERPROFILE").filter(|value| !value.is_empty()))
                .flatten()
        })
        .map(PathBuf::from)
}

fn node_version_manager_bins_with(
    env: &impl Fn(&str) -> Option<OsString>,
    platform: Platform,
) -> Vec<PathBuf> {
    let home = home_dir_with(env, platform);
    let mut dirs = Vec::new();

    if platform == Platform::Windows {
        if let Some(active) = env_path(env, "FNM_MULTISHELL_PATH") {
            dirs.push(active);
        }
    }

    let mut fnm_roots: Vec<PathBuf> = env("FNM_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if platform == Platform::Windows {
        if let Some(roaming) = env_path(env, "APPDATA").or_else(|| {
            env_path(env, "USERPROFILE").map(|profile| profile.join("AppData").join("Roaming"))
        }) {
            fnm_roots.push(roaming.join("fnm"));
        }
    }
    if let Some(home) = &home {
        fnm_roots.push(home.join(".local").join("share").join("fnm"));
        fnm_roots.push(home.join("Library").join("Application Support").join("fnm"));
        fnm_roots.push(home.join(".fnm"));
    }
    for root in fnm_roots {
        let default = root.join("aliases").join("default");
        dirs.push(if platform == Platform::Windows {
            default
        } else {
            default.join("bin")
        });
    }

    if platform == Platform::Windows {
        if let Some(dir) = env_path(env, "NVM_SYMLINK") {
            dirs.push(dir);
        }
        if let Some(root) = env_path(env, "VOLTA_HOME").or_else(|| {
            env_path(env, "LOCALAPPDATA")
                .or_else(|| {
                    env_path(env, "USERPROFILE")
                        .map(|profile| profile.join("AppData").join("Local"))
                })
                .map(|local| local.join("Volta"))
        }) {
            dirs.push(root.join("bin"));
        }
        if let Some(dir) = env_path(env, "PNPM_HOME") {
            dirs.push(dir);
        }
    } else if let Some(home) = &home {
        dirs.push(home.join(".volta").join("bin"));
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join("Library").join("pnpm"));
        dirs.push(home.join(".local").join("share").join("pnpm"));

        let nvm = home.join(".nvm").join("versions").join("node");
        if let Ok(entries) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path().join("bin"))
                .collect();
            versions.sort();
            versions.reverse();
            dirs.append(&mut versions);
        }
    }
    dirs
}

fn env_path(env: &impl Fn(&str) -> Option<OsString>, key: &str) -> Option<PathBuf> {
    env(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn executable_name(exe: &str, platform: Platform) -> OsString {
    let path = Path::new(exe);
    if platform == Platform::Windows && path.extension().is_none() {
        let mut name = OsString::from(exe);
        name.push(".exe");
        name
    } else {
        OsString::from(exe)
    }
}

fn normalize_extra(path: PathBuf, platform: Platform) -> PathBuf {
    if platform == Platform::Windows && path.extension().is_none() {
        path.with_extension("exe")
    } else {
        path
    }
}

fn is_runnable_candidate(path: &Path, platform: Platform) -> bool {
    if platform == Platform::Windows
        && !path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return false;
    }
    path.is_file()
}

pub(crate) fn find_on_paths_with(
    exe: &str,
    extra: Vec<PathBuf>,
    env: &impl Fn(&str) -> Option<OsString>,
    login_shell_path: Option<OsString>,
    platform: Platform,
) -> Option<PathBuf> {
    find_on_paths_matching_with(exe, extra, env, login_shell_path, platform, |_| true)
}

pub(crate) fn find_on_paths_matching_with(
    exe: &str,
    extra: Vec<PathBuf>,
    env: &impl Fn(&str) -> Option<OsString>,
    login_shell_path: Option<OsString>,
    platform: Platform,
    mut predicate: impl FnMut(&Path) -> bool,
) -> Option<PathBuf> {
    let name = executable_name(exe, platform);
    let from_path = |path: OsString| {
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join(&name))
            .collect::<Vec<_>>()
    };
    let mut candidates = env("PATH").map(from_path).unwrap_or_default();
    if let Some(shell_path) = login_shell_path {
        candidates.extend(from_path(shell_path));
    }
    candidates.extend(
        extra
            .into_iter()
            .map(|path| normalize_extra(path, platform)),
    );
    candidates.extend(
        node_version_manager_bins_with(env, platform)
            .into_iter()
            .map(|dir| dir.join(&name)),
    );
    // npm exposes codex.cmd on Windows, but native harnesses must not spawn
    // batch scripts. Resolve its platform package's payload instead. Keep
    // direct executables first, then inspect only the prefixes already searched.
    if platform == Platform::Windows && exe == "codex" {
        let (package, triple) = if cfg!(target_arch = "aarch64") {
            ("codex-win32-arm64", "aarch64-pc-windows-msvc")
        } else {
            ("codex-win32-x64", "x86_64-pc-windows-msvc")
        };
        let payloads: Vec<_> = candidates
            .iter()
            .filter_map(|candidate| candidate.parent())
            .flat_map(|prefix| {
                let scope = prefix.join("node_modules").join("@openai");
                [
                    scope
                        .join("codex")
                        .join("node_modules")
                        .join("@openai")
                        .join(package),
                    scope.join(package),
                ]
                .map(|root| {
                    root.join("vendor")
                        .join(triple)
                        .join("bin")
                        .join("codex.exe")
                })
            })
            .collect();
        candidates.extend(payloads);
    }

    candidates
        .into_iter()
        .find(|path| is_runnable_candidate(path, platform) && predicate(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(values: &[(&str, OsString)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let values: HashMap<String, OsString> = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        move |key| values.get(key).cloned()
    }

    fn joined(paths: &[&Path]) -> OsString {
        std::env::join_paths(paths).unwrap()
    }

    #[test]
    fn windows_discovery_uses_only_regular_exe_files_and_skips_empty_path_entries() {
        let temp = tempfile::tempdir().unwrap();
        let extensionless = temp.path().join("extensionless");
        let cmd = temp.path().join("cmd");
        let directory = temp.path().join("directory");
        let executable = temp.path().join("executable");
        std::fs::create_dir_all(&extensionless).unwrap();
        std::fs::create_dir_all(&cmd).unwrap();
        std::fs::create_dir_all(directory.join("agent.exe")).unwrap();
        std::fs::create_dir_all(&executable).unwrap();
        std::fs::write(extensionless.join("agent"), b"shim").unwrap();
        std::fs::write(cmd.join("agent.cmd"), b"@echo off").unwrap();
        std::fs::write(executable.join("agent.exe"), b"MZ").unwrap();

        let path = joined(&[Path::new(""), &extensionless, &cmd, &directory, &executable]);
        let found = find_on_paths_with(
            "agent",
            Vec::new(),
            &env(&[("PATH", path)]),
            None,
            Platform::Windows,
        );
        assert_eq!(found, Some(executable.join("agent.exe")));
    }

    #[test]
    fn windows_extra_candidates_require_exe_case_insensitively() {
        let temp = tempfile::tempdir().unwrap();
        let cmd = temp.path().join("tool.cmd");
        let executable = temp.path().join("tool.EXE");
        std::fs::write(&cmd, b"cmd").unwrap();
        std::fs::write(&executable, b"MZ").unwrap();

        assert_eq!(
            find_on_paths_with(
                "tool",
                vec![cmd, executable.clone()],
                &env(&[]),
                None,
                Platform::Windows,
            ),
            Some(executable)
        );
    }

    #[test]
    fn windows_node_manager_dirs_use_explicit_environment_and_userprofile_fallback() {
        let profile = PathBuf::from(r"C:\Users\Ada");
        let nvm = PathBuf::from(r"D:\Node Current");
        let volta = PathBuf::from(r"E:\Volta");
        let pnpm = PathBuf::from(r"F:\pnpm");
        let lookup = env(&[
            ("USERPROFILE", profile.clone().into_os_string()),
            ("NVM_SYMLINK", nvm.clone().into_os_string()),
            ("VOLTA_HOME", volta.clone().into_os_string()),
            ("PNPM_HOME", pnpm.clone().into_os_string()),
        ]);

        assert_eq!(home_dir_with(&lookup, Platform::Windows), Some(profile));
        let bins = node_version_manager_bins_with(&lookup, Platform::Windows);
        assert!(bins.contains(&nvm));
        assert!(bins.contains(&volta.join("bin")));
        assert!(bins.contains(&pnpm));
    }

    #[test]
    fn windows_node_manager_discovery_finds_active_and_default_installations() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("Profile with spaces");
        let roaming = profile.join("AppData/Roaming");
        let local = profile.join("AppData/Local");
        let active = temp.path().join("active fnm");
        let explicit = temp.path().join("custom fnm");
        let defaults = [
            active.clone(),
            explicit.join("aliases/default"),
            roaming.join("fnm/aliases/default"),
            local.join("Volta/bin"),
        ];
        for dir in &defaults {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("node.exe"), b"MZ").unwrap();
        }
        let lookup = env(&[
            ("USERPROFILE", profile.into_os_string()),
            ("FNM_MULTISHELL_PATH", active.into_os_string()),
            ("FNM_DIR", explicit.into_os_string()),
        ]);
        // Exercise selection, including fallback after a stale active link.
        for dir in defaults {
            assert_eq!(
                find_on_paths_with("node", vec![], &lookup, None, Platform::Windows),
                Some(dir.join("node.exe"))
            );
            std::fs::remove_file(dir.join("node.exe")).unwrap();
        }
        assert_eq!(
            find_on_paths_with("node", vec![], &lookup, None, Platform::Windows),
            None
        );
    }

    #[test]
    fn windows_node_manager_defaults_honor_redirected_appdata_and_volta_override() {
        let roaming = PathBuf::from("redirected roaming");
        let local = PathBuf::from("redirected local");
        let volta = PathBuf::from("custom volta");
        let lookup = env(&[
            ("APPDATA", roaming.clone().into_os_string()),
            ("LOCALAPPDATA", local.clone().into_os_string()),
            ("VOLTA_HOME", volta.clone().into_os_string()),
        ]);
        let bins = node_version_manager_bins_with(&lookup, Platform::Windows);
        assert!(bins.contains(&roaming.join("fnm/aliases/default")));
        assert!(bins.contains(&volta.join("bin")));
        assert!(!bins.contains(&local.join("Volta/bin")));
        let bins = node_version_manager_bins_with(&lookup, Platform::Unix);
        assert!(!bins.contains(&roaming.join("fnm/aliases/default")));
        assert!(!bins.contains(&volta.join("bin")));
    }

    #[test]
    fn unix_discovery_preserves_exact_name_and_source_order() {
        let temp = tempfile::tempdir().unwrap();
        let path_dir = temp.path().join("path");
        let shell_dir = temp.path().join("shell");
        let extra_dir = temp.path().join("extra");
        for dir in [&path_dir, &shell_dir, &extra_dir] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("agent"), b"shim").unwrap();
        }
        let found = find_on_paths_with(
            "agent",
            vec![extra_dir.join("agent")],
            &env(&[("PATH", joined(&[&path_dir]))]),
            Some(joined(&[&shell_dir])),
            Platform::Unix,
        );
        assert_eq!(found, Some(path_dir.join("agent")));
    }

    #[test]
    fn windows_native_overrides_reject_batch_scripts_case_insensitively() {
        for path in [
            Path::new(r"C:\tools\agent.cmd"),
            Path::new(r"C:\tools\agent.CmD"),
            Path::new(r"C:\tools\agent.BAT"),
        ] {
            let error = validate_native_override_with(path, Platform::Windows).unwrap_err();
            assert!(error.to_string().contains(".exe"));
        }
        assert_eq!(
            validate_native_override_with(Path::new(r"C:\tools\agent.EXE"), Platform::Windows)
                .unwrap(),
            PathBuf::from(r"C:\tools\agent.EXE")
        );
        assert!(validate_native_override_with(Path::new("agent.cmd"), Platform::Unix).is_ok());
    }

    #[test]
    fn portable_base_dir_uses_current_dir_when_home_is_unset() {
        let current = PathBuf::from(r"D:\work\comet");
        assert_eq!(
            home_or_current_dir_with(&env(&[]), &|| Ok(current.clone()), Platform::Windows,),
            current
        );
    }
}
