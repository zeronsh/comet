//! Slash commands, cached per `(harness, cwd)`.
//!
//! ACP advertises commands per session, and a session is defined by its cwd:
//! project skills under `<project>/.claude/skills` exist only for a session
//! opened there. So the cache unit is the workspace, not the harness.
//!
//! Two writers feed it. A cold read probes the harness, which spawns a
//! short-lived agent process (and, for Claude, runs that project's SessionStart
//! hooks) — hence the TTL, the single-flight, and the negative caching. A
//! running chat feeds it for free through `AgentEvent::AvailableCommands`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use zeron_proto::{HarnessId, SlashCommand};

/// Bounded because a user with many worktrees would otherwise accumulate one
/// entry per directory, forever.
const MAX_ENTRIES: usize = 16;
const FRESH_TTL: Duration = Duration::from_secs(600);
const NEGATIVE_TTL: Duration = Duration::from_secs(30);

type Key = (HarnessId, String);
type Probed = Result<Vec<SlashCommand>, String>;

enum Entry {
    Fresh {
        commands: Vec<SlashCommand>,
        at: Instant,
    },
    Failed {
        error: String,
        at: Instant,
    },
    InFlight {
        tx: broadcast::Sender<Probed>,
    },
}

struct Slot {
    entry: Entry,
    /// Move-to-front stand-in: eviction drops the least recently touched.
    touched: Instant,
}

/// What a locked lookup resolves to, decided before any mutation.
///
/// `get`'s classifying match borrows `slot.entry` immutably; the stale-entry
/// arm needs to overwrite that same field, which the borrow checker rejects
/// while the match is live. So the match only reads and produces one of
/// these owned values, and the write happens after the match expression has
/// ended (still inside the same lock acquisition, so the decision stays
/// atomic with the write).
enum Lookup {
    Fresh(Vec<SlashCommand>),
    Failed(String),
    InFlight(broadcast::Receiver<Probed>),
    /// A cold key: absent, or present but stale. The engine never serves a
    /// stale list — it has no way to push a correction afterwards — so stale
    /// counts as a miss. `existed` tells the write side whether the coming
    /// insert grows the map (and so needs an eviction pass) or just replaces
    /// a slot that was already counted.
    Miss {
        existed: bool,
    },
}

pub struct CommandCache {
    fresh_ttl: Duration,
    negative_ttl: Duration,
    slots: Mutex<HashMap<Key, Slot>>,
}

impl Default for CommandCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandCache {
    pub fn new() -> Self {
        Self::with_ttls(FRESH_TTL, NEGATIVE_TTL)
    }

    pub fn with_ttls(fresh_ttl: Duration, negative_ttl: Duration) -> Self {
        Self {
            fresh_ttl,
            negative_ttl,
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// One spelling per directory. `None` is the host's home, which is what an
    /// older client (no `cwd` field) and a project-less chat both mean.
    pub fn normalize(cwd: Option<&str>) -> String {
        let raw = cwd.map(str::trim).filter(|c| !c.is_empty()).unwrap_or("~");
        let expanded = crate::sessions::expand_home(raw);
        let trimmed = expanded.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    }

    pub fn len(&self) -> usize {
        self.slots.lock().expect("cache lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The list for one workspace. `probe` receives the normalized cwd and runs
    /// only on a miss; concurrent readers of one cold key share a single run.
    pub async fn get<F, Fut>(&self, harness: HarnessId, cwd: Option<&str>, probe: F) -> Probed
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Probed>,
    {
        let key = (harness, Self::normalize(cwd));
        // One lock acquisition covers both the classify and the write: if we
        // dropped the lock between them, two callers could each classify a
        // cold key as a miss before either had inserted its InFlight slot,
        // and both would then start their own probe — defeating single-flight.
        let waiter = {
            let mut slots = self.slots.lock().expect("cache lock");
            let now = Instant::now();
            // The immutable borrow of `slot.entry` this match creates lives
            // only for the match expression: `lookup` holds owned values, so
            // the borrow is gone by the time we reach the write below.
            let lookup = match slots.get(&key) {
                Some(slot) => match &slot.entry {
                    Entry::Fresh { commands, at } if now.duration_since(*at) < self.fresh_ttl => {
                        Lookup::Fresh(commands.clone())
                    }
                    Entry::Failed { error, at } if now.duration_since(*at) < self.negative_ttl => {
                        Lookup::Failed(error.clone())
                    }
                    Entry::InFlight { tx } => Lookup::InFlight(tx.subscribe()),
                    // Stale: a miss. The engine never serves a stale list,
                    // because it has no way to push a correction afterwards.
                    _ => Lookup::Miss { existed: true },
                },
                None => Lookup::Miss { existed: false },
            };
            match lookup {
                Lookup::Fresh(commands) => {
                    if let Some(slot) = slots.get_mut(&key) {
                        slot.touched = now;
                    }
                    return Ok(commands);
                }
                Lookup::Failed(error) => {
                    if let Some(slot) = slots.get_mut(&key) {
                        slot.touched = now;
                    }
                    return Err(error);
                }
                Lookup::InFlight(rx) => {
                    if let Some(slot) = slots.get_mut(&key) {
                        slot.touched = now;
                    }
                    Some(rx)
                }
                Lookup::Miss { existed } => {
                    let (tx, _) = broadcast::channel(4);
                    slots.insert(
                        key.clone(),
                        Slot {
                            entry: Entry::InFlight { tx },
                            touched: now,
                        },
                    );
                    if !existed {
                        self.evict_locked(&mut slots);
                    }
                    None
                }
            }
        };
        if let Some(mut rx) = waiter {
            return match rx.recv().await {
                Ok(result) => result,
                Err(_) => Err("command discovery was dropped".into()),
            };
        }
        let started = Instant::now();
        let result = probe(key.1.clone()).await;
        self.commit(key, started, result)
    }

    /// A running session's own list. It came from a real session in that cwd,
    /// so it outranks anything a probe could produce.
    pub fn note_live(&self, harness: HarnessId, cwd: &str, commands: Vec<SlashCommand>) {
        if commands.is_empty() {
            return;
        }
        let key = (harness, Self::normalize(Some(cwd)));
        let mut slots = self.slots.lock().expect("cache lock");
        let now = Instant::now();
        let previous = slots.insert(
            key,
            Slot {
                entry: Entry::Fresh {
                    commands: commands.clone(),
                    at: now,
                },
                touched: now,
            },
        );
        // Waiters on an in-flight probe get the better answer immediately.
        if let Some(Slot {
            entry: Entry::InFlight { tx },
            ..
        }) = previous
        {
            let _ = tx.send(Ok(commands));
        }
        self.evict_locked(&mut slots);
    }

    fn commit(&self, key: Key, started: Instant, result: Probed) -> Probed {
        let mut slots = self.slots.lock().expect("cache lock");
        let now = Instant::now();
        // A live write that landed while the probe ran is newer and better.
        if let Some(Slot {
            entry: Entry::Fresh { commands, at },
            ..
        }) = slots.get(&key)
            && *at > started
        {
            return Ok(commands.clone());
        }
        let entry = match &result {
            Ok(commands) => Entry::Fresh {
                commands: commands.clone(),
                at: now,
            },
            Err(error) => Entry::Failed {
                error: error.clone(),
                at: now,
            },
        };
        if let Some(Slot {
            entry: Entry::InFlight { tx },
            ..
        }) = slots.insert(
            key,
            Slot {
                entry,
                touched: now,
            },
        ) {
            let _ = tx.send(result.clone());
        }
        self.evict_locked(&mut slots);
        result
    }

    /// Drop the least recently touched settled entries. In-flight ones are
    /// skipped: evicting one orphans its waiters.
    fn evict_locked(&self, slots: &mut HashMap<Key, Slot>) {
        while slots.len() > MAX_ENTRIES {
            let victim = slots
                .iter()
                .filter(|(_, slot)| !matches!(slot.entry, Entry::InFlight { .. }))
                .min_by_key(|(_, slot)| slot.touched)
                .map(|(key, _)| key.clone());
            match victim {
                Some(key) => {
                    slots.remove(&key);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn cmd(name: &str) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: String::new(),
            input_hint: None,
        }
    }

    fn short() -> CommandCache {
        CommandCache::with_ttls(Duration::from_millis(80), Duration::from_millis(80))
    }

    #[test]
    fn normalize_folds_tilde_and_trailing_separator() {
        let home = CommandCache::normalize(Some("~"));
        assert!(home.starts_with('/'), "{home}");
        assert_eq!(CommandCache::normalize(None), home, "None means home");
        assert_eq!(
            CommandCache::normalize(Some("/repo/")),
            CommandCache::normalize(Some("/repo"))
        );
    }

    #[tokio::test]
    async fn a_fresh_entry_is_served_without_probing() {
        let cache = CommandCache::new();
        let probes = AtomicUsize::new(0);
        for _ in 0..2 {
            let got = cache
                .get(HarnessId::Mock, Some("/repo"), |_| async {
                    probes.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![cmd("a")])
                })
                .await
                .expect("probe ok");
            assert_eq!(got, vec![cmd("a")]);
        }
        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "second read must be cached"
        );
    }

    #[tokio::test]
    async fn a_stale_entry_is_a_miss() {
        let cache = short();
        let probes = AtomicUsize::new(0);
        for _ in 0..2 {
            let _ = cache
                .get(HarnessId::Mock, Some("/repo"), |_| async {
                    probes.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![cmd("a")])
                })
                .await;
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        assert_eq!(probes.load(Ordering::SeqCst), 2, "stale must re-probe");
    }

    #[tokio::test]
    async fn a_failure_is_cached_then_expires() {
        let cache = short();
        let probes = AtomicUsize::new(0);
        let call = || async {
            cache
                .get(HarnessId::Mock, Some("/repo"), |_| async {
                    probes.fetch_add(1, Ordering::SeqCst);
                    Err::<Vec<SlashCommand>, String>("adapter missing".into())
                })
                .await
        };
        assert_eq!(call().await.unwrap_err(), "adapter missing");
        assert_eq!(call().await.unwrap_err(), "adapter missing");
        assert_eq!(probes.load(Ordering::SeqCst), 1, "negative TTL holds");
        tokio::time::sleep(Duration::from_millis(120)).await;
        let _ = call().await;
        assert_eq!(probes.load(Ordering::SeqCst), 2, "negative TTL expires");
    }

    #[tokio::test]
    async fn concurrent_reads_of_a_cold_key_probe_once() {
        let cache = std::sync::Arc::new(CommandCache::new());
        let probes = std::sync::Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let cache = cache.clone();
            let probes = probes.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .get(HarnessId::Mock, Some("/repo"), move |_| async move {
                        probes.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        Ok(vec![cmd("a")])
                    })
                    .await
            }));
        }
        for task in tasks {
            assert_eq!(task.await.expect("join").expect("probe ok"), vec![cmd("a")]);
        }
        assert_eq!(probes.load(Ordering::SeqCst), 1, "single-flight");
    }

    #[tokio::test]
    async fn a_live_write_resolves_waiters_and_beats_the_late_probe() {
        let cache = std::sync::Arc::new(CommandCache::new());
        let reader = {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache
                    .get(HarnessId::Mock, Some("/repo"), |_| async {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        Ok(vec![cmd("from-probe")])
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        cache.note_live(HarnessId::Mock, "/repo", vec![cmd("from-session")]);
        assert_eq!(
            reader.await.expect("join").expect("resolved"),
            vec![cmd("from-session")],
            "the waiter takes the live list"
        );
        let after = cache
            .get(HarnessId::Mock, Some("/repo"), |_| async {
                panic!("must not probe")
            })
            .await
            .expect("cached");
        assert_eq!(after, vec![cmd("from-session")], "late probe discarded");
    }

    #[tokio::test]
    async fn eviction_bounds_the_map_and_spares_in_flight_entries() {
        let cache = std::sync::Arc::new(CommandCache::new());
        let slow = {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache
                    .get(HarnessId::Mock, Some("/slow"), |_| async {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Ok(vec![cmd("slow")])
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        for i in 0..20 {
            let path = format!("/repo{i}");
            let _ = cache
                .get(HarnessId::Mock, Some(&path), |_| async {
                    Ok(vec![cmd("x")])
                })
                .await;
        }
        assert!(cache.len() <= 16, "bounded, got {}", cache.len());
        assert_eq!(
            slow.await.expect("join").expect("probe ok"),
            vec![cmd("slow")],
            "an in-flight entry must never be evicted out from under its waiters"
        );
    }
}
