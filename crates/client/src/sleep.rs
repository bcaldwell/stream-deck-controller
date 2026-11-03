//! USB suspend support: while the server has the screen off (brightness 0) the client lets go of
//! the HID device so the kernel can autosuspend it, then watches the device's sysfs
//! `power/runtime_status`. A key press makes the deck wake itself (remote wakeup), which shows
//! up as `active`; that is the cue to reopen it. The press itself is not delivered to anyone.
//!
//! [`Sleeper`] is the pure state machine (time and status are passed in, so it is testable
//! without a device); `session.rs` does the actual release/reopen.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

/// Brightness 0 must have been in effect this long before the device is released.
const SETTLE: Duration = Duration::from_secs(1);
/// How often `runtime_status` is read while sleeping. This is also the wake latency.
const POLL_EVERY: Duration = Duration::from_millis(100);
/// If the device has not suspended this long after release, autosuspend is not working
/// (udev rule missing?): reopen and stop trying, or the deck would never see key presses.
const SUSPEND_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, PartialEq, Eq)]
enum State {
    Awake,
    /// Brightness 0 was received; release the device at `until` unless something changes.
    Settling {
        until: Instant,
    },
    /// Device released. `suspended` guards against the `active` it still reports during the
    /// autosuspend delay right after release.
    Asleep {
        since: Instant,
        next_poll: Instant,
        suspended: bool,
    },
}

/// What the sleeper wants done.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Release the device now.
    Release,
    /// The deck woke up (or vanished): reopen it.
    Wake,
    /// Never suspended: reopen it, do not treat it as a wake, and stop using suspend.
    GiveUp,
}

#[derive(Debug)]
pub struct Sleeper {
    enabled: bool,
    state: State,
}

impl Sleeper {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            state: State::Awake,
        }
    }

    pub fn is_asleep(&self) -> bool {
        matches!(self.state, State::Asleep { .. })
    }

    /// The server set a brightness. A non-zero one while asleep makes the caller reopen the
    /// device, which this records.
    pub fn brightness(&mut self, percent: u8, now: Instant) {
        match (percent, &self.state) {
            (0, State::Awake) if self.enabled => {
                self.state = State::Settling {
                    until: now + SETTLE,
                };
            }
            (1.., _) => self.state = State::Awake,
            _ => {}
        }
    }

    /// When the state machine next needs attention.
    pub fn next_at(&self) -> Option<Instant> {
        match self.state {
            State::Awake => None,
            State::Settling { until } => Some(until),
            State::Asleep { next_poll, .. } => Some(next_poll),
        }
    }

    /// Called when [`Sleeper::next_at`] has passed. `status` reads the device's
    /// `power/runtime_status` (`None`: unreadable, e.g. the device is gone).
    pub fn step(&mut self, now: Instant, status: impl FnOnce() -> Option<String>) -> Step {
        match &mut self.state {
            State::Awake => Step::Nothing,
            State::Settling { until } if now >= *until => {
                self.state = State::Asleep {
                    since: now,
                    next_poll: now + POLL_EVERY,
                    suspended: false,
                };
                Step::Release
            }
            State::Settling { .. } => Step::Nothing,
            State::Asleep {
                since,
                next_poll,
                suspended,
            } => {
                if now < *next_poll {
                    return Step::Nothing;
                }
                *next_poll = now + POLL_EVERY;
                match status().as_deref().map(str::trim) {
                    Some("active") if !*suspended => {
                        if now.duration_since(*since) < SUSPEND_TIMEOUT {
                            return Step::Nothing;
                        }
                        self.state = State::Awake;
                        self.enabled = false;
                        Step::GiveUp
                    }
                    Some("active") | None => {
                        // woken by a key press, or gone: let the reopen find out
                        self.state = State::Awake;
                        Step::Wake
                    }
                    Some(_) => {
                        *suspended = true;
                        Step::Nothing
                    }
                }
            }
        }
    }
}

/// `power/runtime_status` of the USB device with this serial (Linux sysfs; readable by anyone).
pub fn find_status_path(serial: &str) -> Option<PathBuf> {
    find_status_path_in(Path::new("/sys/bus/usb/devices"), serial)
}

fn find_status_path_in(root: &Path, serial: &str) -> Option<PathBuf> {
    let read = |d: &Path, f: &str| std::fs::read_to_string(d.join(f)).ok();
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|d| {
            read(d, "idVendor").is_some_and(|v| v.trim() == "0fd9")
                && read(d, "serial").is_some_and(|s| s.trim() == serial)
        })
        .map(|d| d.join("power/runtime_status"))
        .filter(|p| p.exists())
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

    /// Asleep since t0 + SETTLE.
    fn asleep(t0: Instant) -> Sleeper {
        let mut s = Sleeper::new(true);
        s.brightness(0, t0);
        assert_eq!(s.step(t0 + SETTLE, || None), Step::Release);
        s
    }

    #[test]
    fn sleeps_after_brightness_zero_settles() {
        let t0 = Instant::now();
        let mut s = Sleeper::new(true);
        assert_eq!(s.next_at(), None);
        s.brightness(0, t0);
        assert_eq!(s.next_at(), Some(t0 + SETTLE));
        assert_eq!(s.step(t0 + MS(999), || None), Step::Nothing);
        assert!(!s.is_asleep());
        assert_eq!(s.step(t0 + SETTLE, || None), Step::Release);
        assert!(s.is_asleep());
        // repeated zeros while asleep change nothing
        s.brightness(0, t0 + SETTLE * 2);
        assert!(s.is_asleep());
    }

    #[test]
    fn brightness_during_settle_cancels() {
        let t0 = Instant::now();
        let mut s = Sleeper::new(true);
        s.brightness(0, t0);
        s.brightness(50, t0 + MS(400));
        assert_eq!(s.next_at(), None);
        // a repeated 0 does not push the deadline out
        s.brightness(0, t0 + SETTLE * 4);
        s.brightness(0, t0 + SETTLE * 4 + MS(500));
        assert_eq!(s.step(t0 + SETTLE * 5, || None), Step::Release);
    }

    #[test]
    fn disabled_never_sleeps() {
        let mut s = Sleeper::new(false);
        s.brightness(0, Instant::now());
        assert_eq!(s.next_at(), None);
    }

    #[test]
    fn wakes_when_status_turns_active_after_suspending() {
        let t0 = Instant::now();
        let mut s = asleep(t0);
        let t = t0 + SETTLE;
        assert_eq!(s.next_at(), Some(t + POLL_EVERY));
        // before the poll is due nothing is read
        assert_eq!(s.step(t + MS(50), || panic!("not due")), Step::Nothing);
        // still reports active during the autosuspend delay: not a wake
        assert_eq!(s.step(t + MS(100), status("active\n")), Step::Nothing);
        assert_eq!(s.step(t + MS(200), status("suspending")), Step::Nothing);
        assert_eq!(s.step(t + MS(300), status("suspended\n")), Step::Nothing);
        assert!(s.is_asleep());
        assert_eq!(s.step(t + MS(400), status("active\n")), Step::Wake);
        assert!(!s.is_asleep());
        assert_eq!(s.next_at(), None);
    }

    #[test]
    fn unreadable_status_triggers_reopen() {
        let t0 = Instant::now();
        let mut s = asleep(t0);
        assert_eq!(s.step(t0 + SETTLE + POLL_EVERY, || None), Step::Wake);
    }

    #[test]
    fn gives_up_when_the_device_never_suspends() {
        let t0 = Instant::now();
        let mut s = asleep(t0);
        let t = t0 + SETTLE;
        assert_eq!(
            s.step(t + SUSPEND_TIMEOUT - MS(1), status("active")),
            Step::Nothing
        );
        assert_eq!(
            s.step(t + SUSPEND_TIMEOUT + POLL_EVERY, status("active")),
            Step::GiveUp
        );
        assert!(!s.is_asleep());
        // and suspend stays off afterwards
        s.brightness(0, t + SUSPEND_TIMEOUT);
        assert_eq!(s.next_at(), None);
    }

    #[test]
    fn server_brightness_wakes_a_sleeping_deck() {
        let t0 = Instant::now();
        let mut s = asleep(t0);
        s.brightness(0, t0 + SETTLE * 2);
        assert!(s.is_asleep(), "still off");
        s.brightness(40, t0 + SETTLE * 3);
        assert!(!s.is_asleep());
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
            find_status_path_in(&root, "BBB"),
            Some(root.join("1-2/power/runtime_status"))
        );
        assert_eq!(find_status_path_in(&root, "CCC"), None, "not an Elgato");
        assert_eq!(find_status_path_in(&root, "ZZZ"), None);
        assert_eq!(find_status_path_in(&root.join("nope"), "AAA"), None);
        std::fs::remove_dir_all(root).ok();
    }
}
