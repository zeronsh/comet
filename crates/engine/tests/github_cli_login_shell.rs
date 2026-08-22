//! End-to-end: `gh` visible only through the login shell's PATH is usable.
//!
//! This file must stay a single test: it mutates process env (SHELL/PATH/HOME)
//! and warms the process-global login-shell snapshot cache, so it needs its
//! own test binary with no parallel siblings.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use zeron_engine::ChangeRequestResolver;

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git fixture command starts");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn github_cli_on_login_shell_path_only_resolves_pull_request() {
    let dir = tempfile::tempdir().unwrap();
    let direct_bin = dir.path().join("direct-bin");
    std::fs::create_dir(&direct_bin).unwrap();
    let git_executable = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|path| path.join("git"))
        .find(|path| path.is_file())
        .expect("git available for checkout inspection");
    std::os::unix::fs::symlink(&git_executable, direct_bin.join("git")).unwrap();

    let shell_bin = dir.path().join("shell-bin");
    std::fs::create_dir(&shell_bin).unwrap();
    write_executable(
        &shell_bin.join("gh"),
        r##"#!/bin/sh
if [ "$GH_PROMPT_DISABLED" != "1" ]; then
  echo "interactive auth was not disabled" >&2
  exit 2
fi
printf '%s\n' '[{"number":90,"title":"Login shell pull request","url":"https://github.com/acme/zeron/pull/90","state":"OPEN","baseRefName":"main","headRefName":"feature/status","updatedAt":"2026-08-15T12:00:00Z","isCrossRepository":false,"headRepositoryOwner":{"login":"acme"}}]'
"##,
    );

    let fake_shell = dir.path().join("fake-shell");
    write_executable(
        &fake_shell,
        &format!(
            "#!/bin/sh\nPATH=\"{}:/usr/bin:/bin\"; export PATH\n\
             while [ \"$#\" -gt 0 ]; do\n\
               if [ \"$1\" = \"-c\" ]; then shift; exec /bin/sh -c \"$1\"; fi\n\
               shift\n\
             done\nexit 1\n",
            shell_bin.display()
        ),
    );

    let checkout = dir.path().join("checkout");
    std::fs::create_dir(&checkout).unwrap();
    run_git(&checkout, &["init", "-q", "-b", "main"]);
    run_git(&checkout, &["checkout", "-q", "-b", "feature/status"]);
    run_git(
        &checkout,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/zeron.git",
        ],
    );

    // A GUI/service-launch environment: only the git shim is available
    // directly, while the user's shell makes `gh` available through its
    // login PATH. Keeping the direct PATH isolated makes this deterministic
    // even on systems that install the real GitHub CLI in /usr/bin.
    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        std::env::set_var("SHELL", &fake_shell);
        std::env::set_var("HOME", dir.path());
        std::env::set_var("PATH", &direct_bin);
        std::env::remove_var("ZERON_NO_LOGIN_SHELL");
    }

    let resolution = ChangeRequestResolver::new()
        .resolve_github(&checkout)
        .await
        .expect("gh resolves through the login-shell PATH");
    let pull_request = resolution.change_request.expect("pull request");
    assert_eq!(pull_request.number, 90);
    assert_eq!(pull_request.title, "Login shell pull request");
}
