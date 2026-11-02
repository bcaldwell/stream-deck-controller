//! USB suspend support: while the server has the screen off (brightness 0) the client lets go of
//! the HID device so the kernel can autosuspend it, then watches the device's sysfs
//! `power/runtime_status`. A key press makes the deck wake itself (remote wakeup), which shows
//! up as `active`; that is the cue to reopen it. The press itself is not delivered to anyone.
//!
//! [`Sleeper`] is the pure state machine (time and status are passed in, so it is testable
//! without a device); `session.rs` does the actual release/reopen.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

/// Brightness 0 must have been in effect this long before the device is released.
pub const SETTLE: Duration = Duration::from_secs(1);
/// How often `runtime_status` is read while sleeping.
pub const POLL_EVERY: Duration = Duration::from_millis(250);
/// If the device has not suspended this long after release, autosuspend is not working
/// (udev rule missing?): reopen and stop trying, or the deck would never see key presses.
pub const SUSPEND_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, PartialEq, Eq)]
enum State {
    Awake,
    /// Brightness 0 was received; release the device at `until` unless something changes.
    Settling {
        until: Instant,
    },
    /// Device released. `seen_suspended` guards against the `active` it still reports during
    /// the autosuspend delay right after release.
    Asleep {
        since: Instant,
        next_poll: Instant,
        seen_suspended: bool,
    },
}

/// What polling the status found.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    /// Still asleep (or nothing due).
    Idle,
    /// Active again after having been suspended (or unreadable): reopen; a key press woke it.
    Woken,
    /// Never suspended: reopen, do not send Wake, and stop using suspend.
    GaveUp,
}

#[derive(Debug)]
pub struct Sleeper {
    enabled: bool,
    state: State,
    /// Keys that received a new frame while asleep (the latest is in the image cache).
    dirty: BTreeSet<u8>,
    /// Brightness to apply on reopen, when the server has not told us one while asleep.
    pending_brightness: Option<u8>,
}

impl Sleeper {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            state: State::Awake,
            dirty: BTreeSet::new(),
            pending_brightness: None,
        }
    }

    pub fn is_asleep(&self) -> bool {
        matches!(self.state, State::Asleep { .. })
    }

    /// The server set a brightness. Returns true if the device is asleep and must be reopened
    /// to apply it (the caller then applies `percent` after [`Sleeper::take_dirty`] redraws).
    pub fn brightness(&mut self, percent: u8, now: Instant) -> bool {
        if percent == 0 {
            if self.enabled && self.state == State::Awake {
                self.state = State::Settling {
                    until: now + SETTLE,
                };
            }
            if self.is_asleep() {
                self.pending_brightness = None;
            }
            return false;
        }
        match self.state {
            State::Asleep { .. } => {
                self.state = State::Awake;
                self.pending_brightness = None;
                true
            }
            State::Settling { .. } => {
                self.state = State::Awake;
                false
            }
            State::Awake => false,
        }
    }

    /// True once when the settle time is over: the caller releases the device now.
    pub fn take_due(&mut self, now: Instant) -> bool {
        match self.state {
            State::Settling { until } if now >= until => {
                self.state = State::Asleep {
                    since: now,
                    next_poll: now + POLL_EVERY,
                    seen_suspended: false,
                };
                self.dirty.clear();
                true
            }
            _ => false,
        }
    }

    /// A frame arrived. True if it is deferred because the device is asleep (`changed`: it
    /// differs from what the cache held, so it has to be drawn on reopen).
    pub fn image(&mut self, key: u8, changed: bool) -> bool {
        if !self.is_asleep() {
            return false;
        }
        if changed {
            self.dirty.insert(key);
        }
        true
    }

    /// Keys to redraw after a reopen.
    pub fn take_dirty(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.dirty).into_iter().collect()
    }

    /// The connection came up while asleep: the brightness to apply when a press wakes the
    /// deck, unless the server decides otherwise first.
    pub fn set_pending_brightness(&mut self, percent: u8) {
        if self.is_asleep() {
            self.pending_brightness = Some(percent);
        }
    }

    pub fn take_pending_brightness(&mut self) -> Option<u8> {
        self.pending_brightness.take()
    }

    /// When the state machine next needs attention.
    pub fn next_at(&self) -> Option<Instant> {
        match self.state {
            State::Awake => None,
            State::Settling { until } => Some(until),
            State::Asleep { next_poll, .. } => Some(next_poll),
        }
    }

    /// Reads the device status if a poll is due. `status` is the content of
    /// `power/runtime_status` (`None`: unreadable, e.g. the device is gone).
    pub fn poll(&mut self, now: Instant, status: impl FnOnce() -> Option<String>) -> Poll {
        let State::Asleep {
            since,
            next_poll,
            seen_suspended,
        } = &mut self.state
        else {
            return Poll::Idle;
        };
        if now < *next_poll {
            return Poll::Idle;
        }
        *next_poll = now + POLL_EVERY;
        let active = match status() {
            Some(s) => s.trim() == "active",
            None => {
                // gone: let the reopen discover it
                self.state = State::Awake;
                return Poll::Woken;
            }
        };
        if !active {
            *seen_suspended = true;
            return Poll::Idle;
        }
        if *seen_suspended {
            self.state = State::Awake;
            return Poll::Woken;
        }
        if now.duration_since(*since) >= SUSPEND_TIMEOUT {
            self.state = State::Awake;
            self.enabled = false;
            return Poll::GaveUp;
        }
        Poll::Idle
    }
}

/// What to do after reopening following a key-press wake.
#[derive(Debug, PartialEq, Eq)]
pub struct WakePlan {
    /// Brightness to set on the device.
    pub brightness: Option<u8>,
    /// Tell the server (`ClientMessage::Wake`).
    pub send_wake: bool,
}

/// Connected: the server restores the brightness on `Wake`, so only apply one we were told to.
/// Disconnected: nobody will, so show the configured brightness.
pub fn wake_plan(connected: bool, pending: Option<u8>, configured: u8) -> WakePlan {
    WakePlan {
        brightness: if connected {
            pending
        } else {
            Some(pending.unwrap_or(configured))
        },
        send_wake: connected,
    }
}

/// `power/runtime_status` of the USB device with this serial (Linux sysfs; readable by anyone).
/// With no serial match, a lone Elgato device is used.
pub fn find_status_path(serial: Option<&str>) -> Option<PathBuf> {
    find_status_path_in(Path::new("/sys/bus/usb/devices"), serial)
}

fn find_status_path_in(root: &Path, serial: Option<&str>) -> Option<PathBuf> {
    let read = |d: &Path, f: &str| {
        std::fs::read_to_string(d.join(f))
            .ok()
            .map(|s| s.trim().to_string())
    };
    let mut elgato = Vec::new();
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let dir = entry.path();
        if read(&dir, "idVendor").as_deref() != Some("0fd9")
            || !dir.join("power/runtime_status").exists()
        {
            continue;
        }
        if serial.is_some() && read(&dir, "serial").as_deref() == serial {
            return Some(dir.join("power/runtime_status"));
        }
        elgato.push(dir);
    }
    match elgato.as_slice() {
        [only] => Some(only.join("power/runtime_status")),
        _ => None,
    }
}

pub fn read_status(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn status(s: &'static str) -> impl FnOnce() -> Option<String> {
        move || Some(s.to_string())
    }

    /// Asleep since t0 + 1s.
    fn asleep(t0: Instant) -> Sleeper {
        let mut s = Sleeper::new(true);
        assert!(!s.brightness(0, t0));
        assert!(s.take_due(t0 + SETTLE));
        s
    }

    #[test]
    fn sleeps_after_brightness_zero_settles() {
        let t0 = Instant::now();
        let mut s = Sleeper::new(true);
        assert_eq!(s.next_at(), None);
        assert!(!s.brightness(0, t0));
        assert_eq!(s.next_at(), Some(t0 + SETTLE));
        assert!(!s.take_due(t0 + MS(999)));
        assert!(!s.is_asleep());
        assert!(s.take_due(t0 + SETTLE));
        assert!(s.is_asleep());
        assert!(!s.take_due(t0 + SETTLE * 2), "released once");
        // repeated zeros while asleep change nothing
        assert!(!s.brightness(0, t0 + SETTLE * 2));
        assert!(s.is_asleep());
    }

    #[test]
    fn brightness_during_settle_cancels() {
        let t0 = Instant::now();
        let mut s = Sleeper::new(true);
        s.brightness(0, t0);
        assert!(!s.brightness(50, t0 + MS(400)), "nothing to reopen");
        assert!(!s.take_due(t0 + SETTLE * 3));
        assert!(!s.is_asleep());
        // a repeated 0 does not push the deadline out
        s.brightness(0, t0 + SETTLE * 4);
        s.brightness(0, t0 + SETTLE * 4 + MS(500));
        assert!(s.take_due(t0 + SETTLE * 5));
    }

    #[test]
    fn disabled_never_sleeps() {
        let t0 = Instant::now();
        let mut s = Sleeper::new(false);
        s.brightness(0, t0);
        assert!(!s.take_due(t0 + SETTLE * 10));
        assert_eq!(s.next_at(), None);
    }

    #[test]
    fn wakes_when_status_turns_active_after_suspending() {
        let t0 = Instant::now();
        let mut s = asleep(t0);
        let t = t0 + SETTLE;
        // before the first poll is due nothing is read
        assert_eq!(s.next_at(), Some(t + POLL_EVERY));
        assert_eq!(s.poll(t + MS(100), || panic!("not due")), Poll::Idle);
        // still reports active during the autosuspend delay: not a wake
        assert_eq!(s.poll(t + MS(250), status("active\n")), Poll::Idle);
        assert_eq!(s.poll(t + MS(500), status("suspending")), Poll::Idle);
        assert_eq!(s.poll(t + MS(750), status("suspended\n")), Poll::Idle);
        assert_eq!(s.poll(t + MS(1000), status("suspended\n")), Poll::Idle);
        assert!(s.is_asleep());
        assert_eq!(s.poll(t + MS(1250), status("active\n")), Poll::Woken);
        assert!(!s.is_asleep());
        assert_eq!(s.next_at(), None);
    }

    #[test]
    fn unreadable_status_triggers_reopen() {
        let t0 = Instant::now();
        let mut s = asleep(t0);
        assert_eq!(s.poll(t0 + SETTLE + POLL_EVERY, || None), Poll::Woken);
        assert!(!s.is_asleep());
    }

    #[test]
    fn gives_up_when_the_device_never_suspends() {
        let t0 = Instant::now();
        let mut s = asleep(t0);
        let t = t0 + SETTLE;
        assert_eq!(
            s.poll(t + SUSPEND_TIMEOUT - MS(1), status("active")),
            Poll::Idle
        );
        assert_eq!(
            s.poll(t + SUSPEND_TIMEOUT + POLL_EVERY, status("active")),
            Poll::GaveUp
        );
        assert!(!s.is_asleep());
        // and suspend stays off afterwards
        s.brightness(0, t + SUSPEND_TIMEOUT);
        assert!(!s.take_due(t + SUSPEND_TIMEOUT * 2));
    }

    #[test]
    fn frames_while_asleep_are_deferred_until_reopen() {
        let t0 = Instant::now();
        let mut s = Sleeper::new(true);
        assert!(!s.image(1, true), "awake: drawn immediately");
        s.brightness(0, t0);
        assert!(!s.image(2, true), "settling: still drawn");
        s.take_due(t0 + SETTLE);
        assert!(s.image(3, true));
        assert!(s.image(3, true));
        assert!(s.image(5, false), "unchanged: deferred but nothing to draw");
        assert!(s.image(4, true));
        assert_eq!(s.take_dirty(), vec![3, 4]);
        assert!(s.take_dirty().is_empty());
    }

    #[test]
    fn server_brightness_wakes_a_sleeping_deck() {
        let t0 = Instant::now();
        let mut s = asleep(t0);
        s.set_pending_brightness(60);
        assert!(!s.brightness(0, t0 + SETTLE * 2), "still off");
        assert_eq!(s.take_pending_brightness(), None, "server said off");
        s.set_pending_brightness(60);
        assert!(s.brightness(40, t0 + SETTLE * 3), "reopen to apply");
        assert!(!s.is_asleep());
        assert_eq!(s.take_pending_brightness(), None, "superseded by 40");
    }

    #[test]
    fn disconnect_does_not_wake_and_press_wake_plan() {
        let t0 = Instant::now();
        let mut s = asleep(t0);
        // nothing about the link changes the state; only status/brightness do
        assert_eq!(
            s.poll(t0 + SETTLE + POLL_EVERY, status("suspended")),
            Poll::Idle
        );
        assert!(s.is_asleep());
        // reconnect while asleep remembers the brightness for the eventual wake
        s.set_pending_brightness(60);
        assert_eq!(
            wake_plan(true, s.take_pending_brightness(), 60),
            WakePlan {
                brightness: Some(60),
                send_wake: true
            }
        );
        assert_eq!(
            wake_plan(true, None, 60),
            WakePlan {
                brightness: None,
                send_wake: true
            },
            "the server restores it on Wake"
        );
        assert_eq!(
            wake_plan(false, None, 60),
            WakePlan {
                brightness: Some(60),
                send_wake: false
            }
        );
    }

    #[test]
    fn finds_status_path_by_serial() {
        let root = std::env::temp_dir().join(format!("sdc-sysfs-{}", std::process::id()));
        let mk = |name: &str, vendor: &str, serial: &str| {
            let d = root.join(name);
            std::fs::create_dir_all(d.join("power")).unwrap();
            std::fs::write(d.join("idVendor"), format!("{vendor}\n")).unwrap();
            std::fs::write(d.join("serial"), format!("{serial}\n")).unwrap();
            std::fs::write(d.join("power/runtime_status"), "suspended\n").unwrap();
        };
        mk("1-1", "0fd9", "AAA");
        mk("1-2", "0fd9", "BBB");
        mk("1-3", "1d6b", "CCC");
        assert_eq!(
            find_status_path_in(&root, Some("BBB")),
            Some(root.join("1-2/power/runtime_status"))
        );
        assert_eq!(
            find_status_path_in(&root, Some("ZZZ")),
            None,
            "two decks, no match"
        );
        std::fs::remove_dir_all(root.join("1-2")).unwrap();
        assert_eq!(
            find_status_path_in(&root, Some("ZZZ")),
            Some(root.join("1-1/power/runtime_status")),
            "a lone deck is used"
        );
        assert_eq!(find_status_path_in(&root.join("nope"), None), None);
        std::fs::remove_dir_all(root).ok();
    }
}
