//! When update checks run (DST-R51): at start, then whenever the update
//! interval has passed since the last check, tested every update tick by
//! wall clock, so a check overdue after sleep runs within a tick of waking.
//! A check that failed is tried again sooner: after the failed-check retry,
//! doubled after each further failure up to the failed-check retry cap, and
//! spread by the failed-check jitter so that installs don't all try again
//! at once. Pure: the caller passes the time and the jitter.

use std::time::{Duration, SystemTime};

/// DST Tunables: update interval.
pub const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// DST Tunables: update tick.
pub const UPDATE_TICK: Duration = Duration::from_secs(60);

/// DST Tunables: failed-check retry, the wait after a first failed check.
pub const FAILED_CHECK_RETRY: Duration = Duration::from_secs(15 * 60);

/// DST Tunables: failed-check retry cap, the longest wait after failed
/// checks in a row, before jitter.
pub const FAILED_CHECK_RETRY_CAP: Duration = Duration::from_secs(60 * 60);

/// DST Tunables: failed-check jitter, the most a wait after a failed check
/// is shortened or lengthened by, in percent.
pub const FAILED_CHECK_JITTER_PERCENT: u32 = 20;

/// The time of the last check, failed or not, and the wait after it: the
/// interval, or the failed-check retry after a failed check, never longer
/// than the interval (DST-R51). The required-state config reads keep their
/// own, with their own interval and no retry (DST-R56).
#[derive(Debug)]
pub struct Schedule {
    interval: Duration,
    last: Option<SystemTime>,
    /// The wait after a failed check, until a check completes.
    retry: Option<Duration>,
    /// Failed checks in a row.
    failures: u32,
}

impl Default for Schedule {
    /// The update checks' schedule.
    fn default() -> Self {
        Self::every(UPDATE_INTERVAL)
    }
}

impl Schedule {
    /// Due at the first call, then once `interval` has passed since the last.
    pub fn every(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            retry: None,
            failures: 0,
        }
    }

    /// Whether a check is due at `now`; if it is, `now` becomes the last
    /// check. The first call is always due (the check at start).
    ///
    /// A wall clock set back before the last check restarts the interval
    /// from `now`, so a clock that was wrong neither holds checks back until
    /// it catches up nor causes an extra check.
    pub fn take_due(&mut self, now: SystemTime) -> bool {
        let wait = self.retry.map_or(self.interval, |r| r.min(self.interval));
        let due = match self.last {
            None => true,
            Some(last) => match now.duration_since(last) {
                Ok(elapsed) => elapsed >= wait,
                Err(_) => {
                    self.last = Some(now);
                    false
                }
            },
        };
        if due {
            self.last = Some(now);
        }
        due
    }

    /// Makes the next tick check, whenever the last check was (DST-R56's
    /// check at once when update required is entered).
    pub fn force(&mut self) {
        self.last = None;
    }

    /// The check that was due failed: the next one waits the failed-check
    /// retry, doubled for each further failure in a row up to the cap, then
    /// shortened or lengthened by `jitter` (from 0 to 1) times the
    /// failed-check jitter (DST-R51).
    pub fn failed(&mut self, jitter: f64) {
        self.failures = self.failures.saturating_add(1);
        let doublings = (self.failures - 1).min(8);
        let wait = (FAILED_CHECK_RETRY * 2u32.pow(doublings)).min(FAILED_CHECK_RETRY_CAP);
        let spread = f64::from(FAILED_CHECK_JITTER_PERCENT) / 100.0;
        let factor = 1.0 - spread + 2.0 * spread * jitter.clamp(0.0, 1.0);
        self.retry = Some(wait.mul_f64(factor));
    }

    /// The check that was due completed: the next one waits the interval.
    pub fn completed(&mut self) {
        self.failures = 0;
        self.retry = None;
    }

    /// Starts the interval again from `now`, as though it was due then.
    pub fn restart(&mut self, now: SystemTime) {
        self.last = Some(now);
    }
}

/// A jitter from 0 to 1 for [`Schedule::failed`], from the clock's
/// sub-second part: enough to spread installs that failed together.
pub fn jitter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    f64::from(nanos % 1001) / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: Duration = Duration::from_secs(60);
    const HOUR: Duration = Duration::from_secs(60 * 60);

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    /// Ticks once a minute from `from` to `to`, both included, and returns
    /// the times that checked.
    fn tick_through(schedule: &mut Schedule, from: SystemTime, to: SystemTime) -> Vec<SystemTime> {
        let mut checks = Vec::new();
        let mut now = from;
        while now <= to {
            if schedule.take_due(now) {
                checks.push(now);
            }
            now += MINUTE;
        }
        checks
    }

    #[test]
    fn the_interval_is_six_hours_and_the_tick_one_minute() {
        // DST Tunables: update interval 6 hours, update tick 1 minute
        assert_eq!(UPDATE_INTERVAL, 6 * HOUR);
        assert_eq!(UPDATE_TICK, MINUTE);
    }

    #[test]
    fn a_check_at_start_then_every_six_hours() {
        // DST-R51: a check at each start, then whenever the update interval
        // has passed since the last check
        let mut schedule = Schedule::default();
        let checks = tick_through(&mut schedule, t0(), t0() + 13 * HOUR);
        assert_eq!(checks, vec![t0(), t0() + 6 * HOUR, t0() + 12 * HOUR]);
    }

    #[test]
    fn a_check_overdue_after_sleep_runs_at_the_first_tick_after_waking() {
        // DST-R51: tested every update tick by wall clock, so a check overdue
        // after sleep runs within a tick of waking
        let mut schedule = Schedule::default();
        assert!(schedule.take_due(t0()));
        // Asleep from +1 h to +9 h: the first tick after waking checks.
        assert!(!schedule.take_due(t0() + HOUR));
        assert!(schedule.take_due(t0() + 9 * HOUR));
        // And the interval runs from that check.
        assert!(!schedule.take_due(t0() + 14 * HOUR));
        assert!(schedule.take_due(t0() + 15 * HOUR));
    }

    #[test]
    fn a_clock_set_back_restarts_the_interval() {
        // DST-R51, DST-R98: by wall clock, at most once per interval; a clock
        // set back neither checks at once nor holds checks back
        let mut schedule = Schedule::default();
        assert!(schedule.take_due(t0()));
        assert!(!schedule.take_due(t0() - 24 * HOUR));
        assert!(!schedule.take_due(t0() - 19 * HOUR));
        assert!(schedule.take_due(t0() - 18 * HOUR));
    }

    #[test]
    fn another_interval_and_a_restart() {
        // DST-R56: the required-state config reads every 15 minutes, one
        // interval after any read
        let mut schedule = Schedule::every(15 * MINUTE);
        assert!(schedule.take_due(t0()));
        assert!(!schedule.take_due(t0() + 14 * MINUTE));
        schedule.restart(t0() + 14 * MINUTE);
        assert!(!schedule.take_due(t0() + 15 * MINUTE));
        assert!(schedule.take_due(t0() + 29 * MINUTE));
    }

    #[test]
    fn forcing_checks_at_the_next_tick() {
        // DST-R56: an update check at once when update required is entered
        let mut schedule = Schedule::default();
        assert!(schedule.take_due(t0()));
        schedule.force();
        assert!(schedule.take_due(t0() + MINUTE));
        assert!(!schedule.take_due(t0() + 2 * MINUTE));
    }

    #[test]
    fn the_failed_check_tunables() {
        // DST Tunables: failed-check retry 15 minutes, cap 1 hour, jitter 20%
        assert_eq!(FAILED_CHECK_RETRY, 15 * MINUTE);
        assert_eq!(FAILED_CHECK_RETRY_CAP, HOUR);
        assert_eq!(FAILED_CHECK_JITTER_PERCENT, 20);
    }

    #[test]
    fn a_failed_check_is_tried_again_after_the_retry_doubling_to_the_cap() {
        // DST-R51: a failed check waits the failed-check retry, doubled after
        // each further failure in a row up to the cap; without jitter (0.5)
        // 15, 30, 60, 60 minutes
        let mut schedule = Schedule::default();
        let mut now = t0();
        assert!(schedule.take_due(now));
        for wait in [15u32, 30, 60, 60] {
            schedule.failed(0.5);
            assert!(!schedule.take_due(now + (wait - 1) * MINUTE), "{wait}");
            now += wait * MINUTE;
            assert!(schedule.take_due(now), "{wait}");
        }
    }

    #[test]
    fn a_completed_check_waits_the_interval_again() {
        // DST-R51: only a check that completes starts the update interval
        // again
        let mut schedule = Schedule::default();
        assert!(schedule.take_due(t0()));
        schedule.failed(0.5);
        assert!(schedule.take_due(t0() + 15 * MINUTE));
        schedule.completed();
        let checks = tick_through(&mut schedule, t0() + 16 * MINUTE, t0() + 7 * HOUR);
        assert_eq!(checks, vec![t0() + 15 * MINUTE + 6 * HOUR]);
        // And a later failure starts again from the first retry.
        schedule.failed(0.5);
        let at = t0() + 15 * MINUTE + 6 * HOUR;
        assert!(!schedule.take_due(at + 14 * MINUTE));
        assert!(schedule.take_due(at + 15 * MINUTE));
    }

    #[test]
    fn jitter_spreads_the_wait_by_up_to_a_fifth_either_way() {
        // DST-R51, DST Tunables: failed-check jitter 20%, so installs that
        // failed together don't all try again at once
        // 12 minutes at the least, 18 at the most.
        for (jitter, not_yet, due) in [(0.0, 11u32, 12u32), (1.0, 17, 19)] {
            let mut schedule = Schedule::default();
            assert!(schedule.take_due(t0()));
            schedule.failed(jitter);
            assert!(!schedule.take_due(t0() + not_yet * MINUTE), "{jitter}");
            assert!(schedule.take_due(t0() + due * MINUTE), "{jitter}");
        }
        assert!((0.0..=1.0).contains(&jitter()));
    }

    #[test]
    fn a_retry_never_waits_longer_than_the_interval() {
        // DST-R51: the failed-check retry only ever brings a check forward
        let mut schedule = Schedule::every(20 * MINUTE);
        assert!(schedule.take_due(t0()));
        schedule.failed(0.5);
        schedule.failed(0.5);
        assert!(schedule.take_due(t0() + 20 * MINUTE));
    }
}
