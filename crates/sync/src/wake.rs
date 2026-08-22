//! Suspend/resume detection without OS hooks — wake is an EVENT, not a
//! timeout.
//!
//! A 1s ticker compares wall-clock elapsed to monotonic elapsed since the
//! previous tick. Monotonic clocks (macOS `mach_absolute_time`, Linux
//! `CLOCK_MONOTONIC`) exclude suspend, so a wall jump far beyond the tick
//! means the process just woke from system sleep. Subscribers — room actors,
//! relay links, the token refresh loop — reconnect/refresh immediately
//! instead of discovering half-open sockets by silence-lease timeout
//! (Discord/Slack-style instant recovery; user report: "doesn't fix until I
//! restart the app" / "shouldn't take a minute").
//!
//! The detector task is a lazily-spawned process-wide singleton; `subscribe`
//! must first be called from within a tokio runtime (every caller is an
//! async context). Broadcast receivers that lag simply miss duplicate wake
//! events — each subscriber treats ANY received event as "reconnect now",
//! so a missed one is at worst covered by the next silence lease.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::broadcast;

const TICK: Duration = Duration::from_secs(1);
/// Wall time must outrun monotonic time by this much in one tick to count as
/// a suspend — far above scheduler jitter, far below any real sleep.
const JUMP_THRESHOLD: Duration = Duration::from_secs(5);

static CHANNEL: OnceLock<broadcast::Sender<()>> = OnceLock::new();
static ONLINE: OnceLock<broadcast::Sender<()>> = OnceLock::new();
/// OS-reported network path status. `false` (the default, and the permanent
/// value on platforms without a path monitor) means "assume a path exists" —
/// backoff loops behave exactly as before. Only an explicit
/// [`set_path_online(false)`] from a platform monitor flips this true.
static PATH_OFFLINE: AtomicBool = AtomicBool::new(false);

/// Subscribe to system-wake events (the detector spawns on first call).
pub fn subscribe() -> broadcast::Receiver<()> {
    CHANNEL
        .get_or_init(|| {
            let (tx, _) = broadcast::channel(4);
            let detector = tx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(TICK);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                interval.tick().await; // consume the immediate first tick
                let mut wall = SystemTime::now();
                let mut mono = Instant::now();
                loop {
                    interval.tick().await;
                    let wall_elapsed = SystemTime::now()
                        .duration_since(wall)
                        .unwrap_or(Duration::ZERO);
                    let mono_elapsed = mono.elapsed();
                    if wall_elapsed > mono_elapsed + JUMP_THRESHOLD {
                        tracing::info!(
                            slept_s = wall_elapsed.as_secs(),
                            "wake: system resumed from suspend"
                        );
                        let _ = detector.send(());
                    }
                    wall = SystemTime::now();
                    mono = Instant::now();
                }
            });
            tx
        })
        .subscribe()
}

fn online_channel() -> &'static broadcast::Sender<()> {
    ONLINE.get_or_init(|| broadcast::channel(4).0)
}

/// Subscribe to connectivity-regained events. There is no cross-platform OS
/// hook for "the wifi came back", so the signal is empirical: ANY successful
/// WebSocket dial in this process ([`crate::dial::connect_ws`]) broadcasts
/// here, and every socket waiting out a reconnect backoff treats it as
/// "redial now with fresh backoff". One recovered socket un-parks the whole
/// fleet instead of each one sleeping out its own (up to 30s) delay — the
/// network-flap sibling of the suspend/resume event above. Waiters should
/// `try_recv`-drain stale events before arming, so only successes that happen
/// DURING their wait cut it short.
pub fn subscribe_online() -> broadcast::Receiver<()> {
    online_channel().subscribe()
}

/// Broadcast that a dial just succeeded (see [`subscribe_online`]).
pub fn notify_online() {
    let _ = online_channel().send(());
}

/// Report the OS network-path status (macOS `NWPathMonitor`; other platforms
/// simply never call this). Two effects:
///
/// - offline → online broadcasts an online event, so every parked backoff
///   redials the instant the path returns instead of waiting out its timer;
/// - while offline, [`path_is_offline`] tells backoff waiters to park on the
///   event buses rather than burn dial attempts the OS says cannot succeed.
pub fn set_path_online(online: bool) {
    let was_offline = PATH_OFFLINE.swap(!online, Ordering::Relaxed);
    if online && was_offline {
        tracing::info!("wake: network path restored");
        notify_online();
    } else if !online && !was_offline {
        tracing::info!("wake: network path lost; parking reconnect backoffs");
    }
}

/// True while the OS reports no viable network path. Waiters treat this as
/// "park until an event", never as a hard guarantee — the monitor may be
/// absent or wrong, so parked waits keep a coarse safety timer.
pub fn path_is_offline() -> bool {
    PATH_OFFLINE.load(Ordering::Relaxed)
}
