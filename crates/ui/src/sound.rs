//! Session notification sounds — the herdr approach (state-transition chimes
//! played through the platform's own audio CLI, zero Rust audio deps):
//!
//! - three short chimes embedded in the binary (`assets/sounds/*.wav`, synthesized
//!   in-repo — no external assets): **done** (run finished), **request**
//!   (agent is asking a question), and **attention** (run failed or durable
//!   connection outage);
//! - playback = write to a temp file, hand it to the system player on a
//!   background thread: `afplay` (macOS), PowerShell `Media.SoundPlayer`
//!   (Windows), first of `paplay`/`pw-play`/`aplay`/`ffplay`/`mpv` (Linux —
//!   WAV, so even bare ALSA `aplay` decodes it);
//! - `ZERON_DISABLE_SOUND` env kill-switch + the `soundEnabled` ui-setting;
//! - failures are logged and swallowed — a missing player must never bother
//!   the session flow.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const DISABLE_ENV: &str = "ZERON_DISABLE_SOUND";
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

static SOUND_DONE: &[u8] = include_bytes!("../assets/sounds/done.wav");
static SOUND_REQUEST: &[u8] = include_bytes!("../assets/sounds/request.wav");
static SOUND_ATTENTION: &[u8] = include_bytes!("../assets/sounds/attention.wav");

/// Which notification chime to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// An agent turn completed successfully.
    Done,
    /// The agent is waiting on a question (→ AwaitingInput).
    Request,
    /// A run failed or the durable connection state degraded.
    Attention,
}

/// Play a chime on a background thread. Silently a no-op when disabled or no
/// player is available.
pub fn play(sound: Sound) {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return;
    }
    std::thread::spawn(move || {
        let data = match sound {
            Sound::Done => SOUND_DONE,
            Sound::Request => SOUND_REQUEST,
            Sound::Attention => SOUND_ATTENTION,
        };
        if let Err(err) = play_bytes(data) {
            tracing::debug!(?sound, error = %err, "notification sound playback failed");
        }
    });
}

fn play_bytes(data: &[u8]) -> Result<(), String> {
    // The system players want a file path. Exclusive creation prevents a
    // predictable-name collision (including a pre-planted symlink) from
    // redirecting the embedded bytes to another file.
    let (mut file, tmp) = create_temp_file().map_err(|e| e.to_string())?;
    file.write_all(data).map_err(|e| e.to_string())?;
    drop(file);
    let result = run_player(&tmp.path);
    drop(tmp);
    result
}

struct TempSoundFile {
    path: PathBuf,
}

impl Drop for TempSoundFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn create_temp_file() -> std::io::Result<(std::fs::File, TempSoundFile)> {
    for _ in 0..128 {
        let id = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("zeron-sound-{}-{id}.wav", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((file, TempSoundFile { path })),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a notification sound file",
    ))
}

#[cfg(target_os = "macos")]
fn run_player(path: &Path) -> Result<(), String> {
    run_checked("afplay", &[], path)
}

#[cfg(windows)]
fn run_player(path: &Path) -> Result<(), String> {
    // SoundPlayer handles WAV natively; PlaySync keeps the process alive for
    // the chime's duration. Pass the path through the child environment rather
    // than interpolating it into PowerShell source (paths may contain quotes).
    let script = "(New-Object Media.SoundPlayer $env:ZERON_SOUND_PATH).PlaySync()";
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .env("ZERON_SOUND_PATH", path)
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
        if self.indicator == Indicator::Errored && prev.indicator != Indicator::Errored {
            return Some(Sound::Attention);
        }
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

/// The engine already holds raw transport degradation for four seconds before
/// exposing `Offline` or `Reconnecting`. Notify once when that durable state is
/// first crossed; booting into an outage is seeded silently by the shell.
pub(crate) fn connectivity_sound_since(
    current: zeron_proto::ConnectivityState,
    previous: zeron_proto::ConnectivityState,
) -> Option<Sound> {
    use zeron_proto::ConnectivityState as State;
    let degraded = matches!(current, State::Offline | State::Reconnecting);
    let was_degraded = matches!(previous, State::Offline | State::Reconnecting);
    (degraded && !was_degraded).then_some(Sound::Attention)
}

/// A newly attached engine can report `Connected` while its four-second
/// degradation grace is still measuring an outage that predates the UI. Arm
/// alerts only after a longer healthy observation, or after recovery from a
/// boot-time outage. Runtime replacement resets the observation flag.
#[derive(Debug, Default)]
pub(crate) struct ConnectivityNotificationState {
    previous: Option<zeron_proto::ConnectivityState>,
    first_observed_at: Option<Instant>,
    armed: bool,
}

impl ConnectivityNotificationState {
    const STARTUP_QUIET: Duration = Duration::from_secs(5);

    pub(crate) fn update(
        &mut self,
        current: zeron_proto::ConnectivityState,
        observed: bool,
        now: Instant,
    ) -> Option<Sound> {
        if !observed {
            *self = Self::default();
            return None;
        }

        let first = *self.first_observed_at.get_or_insert(now);
        let previous = self.previous.replace(current);
        if !self.armed {
            if now.duration_since(first) >= Self::STARTUP_QUIET {
                self.armed = true;
                return previous.and_then(|previous| connectivity_sound_since(current, previous));
            }
            return None;
        }
        previous.and_then(|previous| connectivity_sound_since(current, previous))
    }
}

/// Coalesce attention requests delivered by independent state watches. The
/// persistent gate also collapses multiple failures in one session snapshot.
#[derive(Debug, Default)]
pub(crate) struct AttentionSoundGate {
    last_played: Option<Instant>,
}

impl AttentionSoundGate {
    const COALESCE: Duration = Duration::from_millis(250);

    pub(crate) fn should_play(&mut self, now: Instant) -> bool {
        if self
            .last_played
            .is_some_and(|last| now.duration_since(last) < Self::COALESCE)
        {
            return false;
        }
        self.last_played = Some(now);
        true
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
        // An older host without explicit completion metadata is silent too.
        assert_eq!(
            baseline(Indicator::None, None).sound_since(&baseline(Indicator::Working, None), false),
            None
        );
    }

    #[test]
    fn a_run_error_chimes_once_and_never_masquerades_as_completion() {
        let working = baseline(Indicator::Working, Some("old"));
        let errored = baseline(Indicator::Errored, Some("failed"));
        assert_eq!(errored.sound_since(&working, false), Some(Sound::Attention));
        assert_eq!(errored.sound_since(&errored, false), None);
    }

    #[test]
    fn durable_connectivity_degradation_chimes_once_per_outage() {
        use zeron_proto::ConnectivityState as State;

        assert_eq!(
            connectivity_sound_since(State::Connected, State::Disabled),
            None
        );
        assert_eq!(
            connectivity_sound_since(State::Reconnecting, State::Connected),
            Some(Sound::Attention)
        );
        assert_eq!(
            connectivity_sound_since(State::Offline, State::Reconnecting),
            None
        );
        assert_eq!(
            connectivity_sound_since(State::Connected, State::Offline),
            None
        );
        assert_eq!(
            connectivity_sound_since(State::Offline, State::Connected),
            Some(Sound::Attention)
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
    fn temp_files_are_exclusive_and_cleaned_up() {
        let (first_file, first) = create_temp_file().expect("reserve first temp file");
        let (second_file, second) = create_temp_file().expect("reserve second temp file");
        assert_ne!(first.path, second.path);
        assert!(first.path.exists());
        assert!(second.path.exists());
        drop(first_file);
        drop(second_file);
        let first_path = first.path.clone();
        let second_path = second.path.clone();
        drop(first);
        drop(second);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    #[test]
    fn embedded_chimes_are_wav() {
        for data in [SOUND_DONE, SOUND_REQUEST, SOUND_ATTENTION] {
            assert!(data.len() > 1000);
            assert_eq!(&data[..4], b"RIFF");
            assert_eq!(&data[8..12], b"WAVE");
        }
    }

    #[test]
    fn connectivity_boot_outages_seed_silently_then_later_outages_alert() {
        use zeron_proto::ConnectivityState as State;
        let t0 = Instant::now();

        // Warm daemon: the first authoritative snapshot is already degraded.
        let mut warm = ConnectivityNotificationState::default();
        assert_eq!(warm.update(State::Disabled, false, t0), None);
        assert_eq!(warm.update(State::Offline, true, t0), None);
        assert_eq!(
            warm.update(State::Connected, true, t0 + Duration::from_secs(6)),
            None
        );
        assert_eq!(
            warm.update(State::Offline, true, t0 + Duration::from_secs(7)),
            Some(Sound::Attention)
        );

        // Cold daemon: the engine's grace initially masks the existing outage.
        let mut cold = ConnectivityNotificationState::default();
        assert_eq!(cold.update(State::Connected, true, t0), None);
        assert_eq!(
            cold.update(State::Offline, true, t0 + Duration::from_secs(4)),
            None
        );
        assert_eq!(
            cold.update(State::Connected, true, t0 + Duration::from_secs(5)),
            None
        );
        assert_eq!(
            cold.update(State::Reconnecting, true, t0 + Duration::from_secs(6)),
            Some(Sound::Attention)
        );

        // A quiet healthy boot may not publish another frame until the first
        // genuine outage; elapsed time arms that transition itself.
        let mut healthy = ConnectivityNotificationState::default();
        assert_eq!(healthy.update(State::Connected, true, t0), None);
        assert_eq!(
            healthy.update(State::Offline, true, t0 + Duration::from_secs(6)),
            Some(Sound::Attention)
        );

        // Replacing the runtime returns to bootstrap semantics even if the
        // prior runtime had already armed alerts.
        assert_eq!(
            healthy.update(State::Disabled, false, t0 + Duration::from_secs(7)),
            None
        );
        assert_eq!(
            healthy.update(State::Offline, true, t0 + Duration::from_secs(7)),
            None
        );
    }

    #[test]
    fn attention_gate_coalesces_session_and_connectivity_watch_callbacks() {
        let t0 = Instant::now();
        let mut gate = AttentionSoundGate::default();
        assert!(gate.should_play(t0));
        assert!(!gate.should_play(t0));
        assert!(!gate.should_play(t0 + Duration::from_millis(200)));
        assert!(gate.should_play(t0 + Duration::from_millis(250)));
    }
}
