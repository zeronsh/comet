//! Session notification sounds — the herdr approach (state-transition chimes
//! played through the platform's own audio CLI, zero Rust audio deps):
//!
//! - two short chimes embedded in the binary (`assets/sounds/*.wav`, synthesized
//!   in-repo — no external assets): **done** (run finished) and **request**
//!   (agent is asking a question);
//! - playback = write to a temp file, hand it to the system player on a
//!   background thread: `afplay` (macOS), PowerShell `Media.SoundPlayer`
//!   (Windows), first of `paplay`/`pw-play`/`aplay`/`ffplay`/`mpv` (Linux —
//!   WAV, so even bare ALSA `aplay` decodes it);
//! - `ZERON_DISABLE_SOUND` env kill-switch + the `soundEnabled` ui-setting;
//! - failures are logged and swallowed — a missing player must never bother
//!   the session flow.

#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(target_arch = "wasm32"))]
const DISABLE_ENV: &str = "ZERON_DISABLE_SOUND";
#[cfg(not(target_arch = "wasm32"))]
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(not(target_arch = "wasm32"))]
static SOUND_DONE: &[u8] = include_bytes!("../assets/sounds/done.wav");
#[cfg(not(target_arch = "wasm32"))]
static SOUND_REQUEST: &[u8] = include_bytes!("../assets/sounds/request.wav");

/// Which notification chime to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// An agent turn completed successfully.
    Done,
    /// The agent is waiting on a question (→ AwaitingInput).
    Request,
}

/// Play a chime on a background thread. Silently a no-op when disabled or no
/// player is available.
#[cfg(not(target_arch = "wasm32"))]
pub fn play(sound: Sound) {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return;
    }
    std::thread::spawn(move || {
        let data = match sound {
            Sound::Done => SOUND_DONE,
            Sound::Request => SOUND_REQUEST,
        };
        if let Err(err) = play_bytes(data) {
            tracing::debug!(?sound, error = %err, "notification sound playback failed");
        }
    });
}

/// Browser builds deliberately have no host audio/player effect. A session
/// transition must still render, but it must never spawn a thread, write a
/// temporary file, or launch a native process.
#[cfg(target_arch = "wasm32")]
pub fn play(_sound: Sound) {}

#[cfg(not(target_arch = "wasm32"))]
fn play_bytes(data: &[u8]) -> Result<(), String> {
    // The system players want a file path; write the embedded bytes out.
    let tmp = temp_path();
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    let result = run_player(&tmp);
    let _ = std::fs::remove_file(&tmp);
    result
}

#[cfg(not(target_arch = "wasm32"))]
fn temp_path() -> PathBuf {
    let id = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("zeron-sound-{}-{id}.wav", std::process::id()))
}

#[cfg(target_os = "macos")]
fn run_player(path: &Path) -> Result<(), String> {
    run_checked("afplay", &[], path)
}

#[cfg(windows)]
fn run_player(path: &Path) -> Result<(), String> {
    // SoundPlayer handles WAV natively; PlaySync keeps the process alive for
    // the chime's duration.
    let script = format!(
        "(New-Object Media.SoundPlayer '{}').PlaySync()",
        path.display()
    );
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .output()
        .map_err(|e| format!("powershell failed: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("powershell exited with {}", output.status))
    }
}

#[cfg(all(not(target_arch = "wasm32"), not(any(windows, target_os = "macos"))))]
fn run_player(path: &Path) -> Result<(), String> {
    // WAV everywhere, so even bare ALSA aplay decodes it (herdr must exclude
    // aplay because it ships mp3s).
    let players: &[(&str, &[&str])] = &[
        ("paplay", &[]),
        ("pw-play", &[]),
        ("aplay", &["-q"]),
        ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
        ("mpv", &["--no-video", "--really-quiet"]),
    ];
    let mut errors = Vec::new();
    for (program, args) in players {
        match run_checked(program, args, path) {
            Ok(()) => return Ok(()),
            Err(err) => errors.push(err),
        }
    }
    Err(format!("no audio player available: {}", errors.join("; ")))
}

#[cfg(not(target_arch = "wasm32"))]
fn run_checked(program: &str, args: &[&str], path: &Path) -> Result<(), String> {
    // Bounded wait: a wedged audio daemon must not accumulate zombie threads.
    let mut child = std::process::Command::new(program)
        .args(args)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{program} exited with {status}")),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{program} timed out"));
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program}: {err}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Notification decision (shared by sound and desktop banners)
// ---------------------------------------------------------------------------

use zeron_proto::{
    Session,
    view::{Indicator, effective_indicator},
};

/// Notification baseline is separate from the visual activity indicator:
/// going idle can mean cancellation, expiry, or an internal handoff.
#[derive(Debug, Clone)]
pub(crate) struct SessionNotificationState {
    indicator: Indicator,
    last_completed_turn: Option<String>,
    fresh: bool,
}

impl SessionNotificationState {
    pub(crate) fn new(session: &Session, now: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            indicator: effective_indicator(Some(session), now),
            last_completed_turn: session.last_completed_turn.clone(),
            fresh: now
                .signed_duration_since(session.updated_at)
                .num_milliseconds()
                <= zeron_proto::view::SESSION_STALE_MS,
        }
    }

    /// Call after saving the new baseline, including when delivery is pending
    /// or outputs are disabled. Suppressed pings must never be replayed later.
    pub(crate) fn sound_since(&self, prev: &Self, send_pending: bool) -> Option<Sound> {
        if self.indicator == Indicator::AwaitingInput && prev.indicator != Indicator::AwaitingInput
        {
            return Some(Sound::Request);
        }
        if !send_pending
            && self.fresh
            && self.last_completed_turn.is_some()
            && self.last_completed_turn != prev.last_completed_turn
        {
            return Some(Sound::Done);
        }
        None
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn baseline(indicator: Indicator, turn: Option<&str>) -> SessionNotificationState {
        SessionNotificationState {
            indicator,
            last_completed_turn: turn.map(str::to_owned),
            fresh: true,
        }
    }

    #[test]
    fn interrupted_and_expired_activity_never_chime() {
        let working = baseline(Indicator::Working, Some("old"));
        let idle = baseline(Indicator::None, Some("old"));
        assert_eq!(idle.sound_since(&working, false), None);
        assert_eq!(
            baseline(Indicator::Errored, Some("old")).sound_since(&working, false),
            None
        );
        // An older host without explicit completion metadata is silent too.
        assert_eq!(
            baseline(Indicator::None, None).sound_since(&baseline(Indicator::Working, None), false),
            None
        );
    }

    #[test]
    fn ordinary_queue_completions_survive_coalesced_working_states() {
        let first = baseline(Indicator::Working, None);
        let second = baseline(Indicator::Working, Some("first"));
        assert_eq!(second.sound_since(&first, false), Some(Sound::Done));
        assert_eq!(second.sound_since(&second, false), None);
        let last = baseline(Indicator::None, Some("second"));
        assert_eq!(last.sound_since(&second, false), Some(Sound::Done));
        assert_eq!(last.sound_since(&last, false), None);
    }

    #[test]
    fn pending_send_consumes_completion_but_preserves_input_requests() {
        let before = baseline(Indicator::Working, None);
        let settled = baseline(Indicator::None, Some("first"));
        assert_eq!(settled.sound_since(&before, true), None);
        assert_eq!(settled.sound_since(&settled, false), None);
        let question = baseline(Indicator::AwaitingInput, Some("first"));
        assert_eq!(question.sound_since(&settled, true), Some(Sound::Request));
        assert_eq!(question.sound_since(&question, false), None);
    }

    #[test]
    fn stale_completion_is_consumed_without_replaying_on_a_heartbeat() {
        let before = baseline(Indicator::Working, None);
        let mut stale = baseline(Indicator::None, Some("old"));
        stale.fresh = false;
        assert_eq!(stale.sound_since(&before, false), None);
        let refreshed = baseline(Indicator::Working, Some("old"));
        assert_eq!(refreshed.sound_since(&stale, false), None);
    }

    #[test]
    fn temp_paths_are_unique() {
        assert_ne!(temp_path(), temp_path());
    }

    #[test]
    fn embedded_chimes_are_wav() {
        for data in [SOUND_DONE, SOUND_REQUEST] {
            assert!(data.len() > 1000);
            assert_eq!(&data[..4], b"RIFF");
            assert_eq!(&data[8..12], b"WAVE");
        }
    }
}
