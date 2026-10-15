//! When update checks run (DST-R51): at start, then whenever the update
//! interval has passed since the last check, tested every update tick by
//! wall clock, so a check overdue after sleep runs within a tick of waking.
//! Pure: the caller passes the time.

use std::time::{Duration, SystemTime};

/// DST Tunables: update interval.
pub const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// DST Tunables: update tick.
pub const UPDATE_TICK: Duration = Duration::from_secs(60);

/// The time of the last check, failed or not: a failed check is retried at
/// the next interval, not at the next tick (DST-R51).
#[derive(Debug, Default)]
pub struct Schedule {
    last: Option<SystemTime>,
}

impl Schedule {
    /// Whether a check is due at `now`; if it is, `now` becomes the last
    /// check. The first call is always due (the check at start).
    ///
    /// A wall clock set back before the last check restarts the interval
    /// from `now`, so a clock that was wrong neither holds checks back until
    /// it catches up nor causes an extra check.
    pub fn take_due(&mut self, now: SystemTime) -> bool {
        let due = match self.last {
            None => true,
            Some(last) => match now.duration_since(last) {
                Ok(elapsed) => elapsed >= UPDATE_INTERVAL,
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
    fn forcing_checks_at_the_next_tick() {
        // DST-R56: an update check at once when update required is entered
        let mut schedule = Schedule::default();
        assert!(schedule.take_due(t0()));
        schedule.force();
        assert!(schedule.take_due(t0() + MINUTE));
        assert!(!schedule.take_due(t0() + 2 * MINUTE));
    }
}
