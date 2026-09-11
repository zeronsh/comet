//! Session notification sounds — the herdr approach (state-transition chimes
//! played through the platform's own audio CLI, zero Rust audio deps):
//!
//! - short chimes embedded in the binary (`assets/sounds/*.wav`, synthesized
//!   in-repo — no external assets): **done** (run finished) and **request**
//!   (agent is asking a question), plus an **Appshot** confirmation cue;
//! - macOS Appshots use a preloaded native player; other cues write to a
//!   temp file and use the system player on a
//!   background thread: `afplay` (macOS), PowerShell `Media.SoundPlayer`
//!   (Windows), first of `paplay`/`pw-play`/`aplay`/`ffplay`/`mpv` (Linux —
//!   WAV, so even bare ALSA `aplay` decodes it);
//! - `ZERON_DISABLE_SOUND` env kill-switch + the `soundEnabled` ui-setting;
//! - failures are logged and swallowed — a missing player must never bother
//!   the session flow.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DISABLE_ENV: &str = "ZERON_DISABLE_SOUND";
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(any(target_os = "macos", target_os = "linux"))]
static SOUND_APPSHOT: &[u8] = include_bytes!("../assets/sounds/appshot.wav");

static SOUND_DONE: &[u8] = include_bytes!("../assets/sounds/done.wav");
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
pub fn play(sound: Sound) {
    let data = match sound {
        Sound::Done => SOUND_DONE,
        Sound::Request => SOUND_REQUEST,
    };
    play_in_background(data);
}

/// Confirm captured pixels with a soft shutter and clear chime.
/// The caller honors the dedicated capture sound setting.
pub fn play_appshot() {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return;
    }
    #[cfg(target_os = "macos")]
    if macos_appshot::play() {
        return;
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    play_in_background(SOUND_APPSHOT);
}

/// Load the macOS capture cue before the first capture, without playing it.
pub fn prepare_appshot() {
    #[cfg(target_os = "macos")]
    if std::env::var_os(DISABLE_ENV).is_none() {
        macos_appshot::sender();
    }
}

#[cfg(target_os = "macos")]
mod macos_appshot {
    use objc::rc::{StrongPtr, autoreleasepool};
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use std::sync::{OnceLock, mpsc};

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {}

    // Keep native objects on one worker. A bounded mailbox avoids a backlog of
    // stale confirmations when captures arrive in a burst.
    pub(super) fn sender() -> Option<&'static mpsc::SyncSender<()>> {
        static SENDER: OnceLock<Option<mpsc::SyncSender<()>>> = OnceLock::new();
        SENDER
            .get_or_init(|| {
                let (tx, rx) = mpsc::sync_channel(1);
                std::thread::Builder::new()
                    .name("appshot-audio".into())
                    .spawn(move || {
                        let player = autoreleasepool(load);
                        while rx.recv().is_ok() {
                            if std::env::var_os(super::DISABLE_ENV).is_some() {
                                continue;
                            }
                            let started = std::time::Instant::now();
                            let played = player.as_ref().is_some_and(|player| {
                                autoreleasepool(|| unsafe {
                                    // Pause preserves prepared audio resources; stop would discard them.
                                    let _: () = msg_send![**player, pause];
                                    let _: () = msg_send![**player, setCurrentTime: 0.0_f64];
                                    let played: bool = msg_send![**player, play];
                                    played
                                })
                            });
                            tracing::debug!(
                                elapsed_ms = started.elapsed().as_millis(),
                                played,
                                "Appshot native playback requested"
                            );
                            if !played {
                                super::play_in_background(super::SOUND_APPSHOT);
                            }
                        }
                    })
                    .ok()
                    .map(|_| tx)
            })
            .as_ref()
    }

    fn load() -> Option<StrongPtr> {
        let player = decode()?;
        let ready: bool = unsafe { msg_send![*player, prepareToPlay] };
        if !ready {
            tracing::warn!("Could not prepare native Appshot audio; using system player");
            return None;
        }
        Some(player)
    }

    fn decode() -> Option<StrongPtr> {
        unsafe {
            let data: *mut Object = msg_send![class!(NSData),
                dataWithBytes: super::SOUND_APPSHOT.as_ptr()
                length: super::SOUND_APPSHOT.len()];
            let allocated: *mut Object = msg_send![class!(AVAudioPlayer), alloc];
            let mut error: *mut Object = std::ptr::null_mut();
            let player: *mut Object = msg_send![allocated, initWithData: data error: &mut error];
            if player.is_null() {
                tracing::warn!("Could not decode native Appshot audio; using system player");
                return None;
            }
            Some(StrongPtr::new(player))
        }
    }

    #[test]
    fn embedded_appshot_decodes_natively_without_playback() {
        autoreleasepool(|| {
            let player = decode().expect("embedded Appshot should decode in AVAudioPlayer");
            let duration: f64 = unsafe { msg_send![*player, duration] };
            let playing: bool = unsafe { msg_send![*player, isPlaying] };
            assert!((duration - 0.67).abs() < 0.001);
            assert!(!playing);
        });
    }

    pub(super) fn play() -> bool {
        match sender().map(|tx| tx.try_send(())) {
            Some(Ok(()) | Err(mpsc::TrySendError::Full(()))) => true,
            _ => false,
        }
    }
}

fn play_in_background(data: &'static [u8]) {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return;
    }
    std::thread::spawn(move || {
        if let Err(err) = play_bytes(data) {
            tracing::debug!(error = %err, "sound playback failed");
        }
    });
}

fn play_bytes(data: &[u8]) -> Result<(), String> {
    // The system players want a file path; write the embedded bytes out.
    let tmp = temp_path();
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    let result = run_player(&tmp);
    let _ = std::fs::remove_file(&tmp);
    result
}

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

#[cfg(not(any(windows, target_os = "macos")))]
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

#[cfg(test)]
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
        for data in [
            SOUND_DONE,
            SOUND_REQUEST,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            SOUND_APPSHOT,
        ] {
            assert!(data.len() > 1000);
            assert_eq!(&data[..4], b"RIFF");
            assert_eq!(&data[8..12], b"WAVE");
        }
    }
}
