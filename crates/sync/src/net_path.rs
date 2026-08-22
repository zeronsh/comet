//! OS network-path monitoring → [`crate::wake::set_path_online`].
//!
//! macOS: `NWPathMonitor` (Network.framework, plain C surface + one ObjC
//! block). The instant the OS says the path is back, every parked reconnect
//! backoff redials; while the OS says there is no path, backoff waiters park
//! on the event buses instead of burning dial attempts (see `wake.rs`).
//!
//! Other platforms: no-op — `path_is_offline()` stays false and the empirical
//! signals (suspend detector, sibling-dial successes, focus probe) carry the
//! recovery story unchanged, exactly as before this module existed.

/// Start the process-wide path monitor. Idempotent; call from engine startup.
pub fn spawn_path_monitor() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        #[cfg(target_os = "macos")]
        imp::start();
    });
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_char, c_void};

    use block2::{Block, RcBlock};

    // nw_path_monitor_t / nw_path_t / dispatch_queue_t are ObjC objects;
    // opaque pointers are all this bridge needs.
    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        fn nw_path_monitor_create() -> *mut c_void;
        fn nw_path_monitor_set_update_handler(
            monitor: *mut c_void,
            handler: &Block<dyn Fn(*mut c_void)>,
        );
        fn nw_path_monitor_set_queue(monitor: *mut c_void, queue: *mut c_void);
        fn nw_path_monitor_start(monitor: *mut c_void);
        fn nw_path_get_status(path: *mut c_void) -> i32;
    }

    unsafe extern "C" {
        fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut c_void;
    }

    /// `nw_path_status_unsatisfied`. Only a definitive "no path" parks the
    /// dialers — `invalid`/`satisfiable` stay optimistic, so a confused
    /// monitor can only ever make us dial too much, never go silent.
    const NW_PATH_STATUS_UNSATISFIED: i32 = 2;

    pub(super) fn start() {
        unsafe {
            let monitor = nw_path_monitor_create();
            if monitor.is_null() {
                tracing::warn!("net_path: NWPathMonitor unavailable");
                return;
            }
            let handler = RcBlock::new(|path: *mut c_void| {
                let status = unsafe { nw_path_get_status(path) };
                let online = status != NW_PATH_STATUS_UNSATISFIED;
                crate::wake::set_path_online(online);
                if online {
                    // Interface handovers (wifi→hotspot, VPN up/down) arrive
                    // as satisfied→satisfied updates. Old sockets are dead on
                    // the new path either way, so kick parked dials on every
                    // viable-path report — waiters drain stale events before
                    // arming, so a redundant kick costs nothing.
                    crate::wake::notify_online();
                }
            });
            let queue = dispatch_queue_create(c"zeron.net-path".as_ptr(), std::ptr::null_mut());
            nw_path_monitor_set_queue(monitor, queue);
            nw_path_monitor_set_update_handler(monitor, &handler);
            nw_path_monitor_start(monitor);
            // The monitor (and its handler copy) lives for the process.
            std::mem::forget(handler);
        }
    }
}
