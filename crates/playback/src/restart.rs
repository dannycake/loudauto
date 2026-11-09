//! When Soloist exits, decide what to do next: restart it after a backoff, show the
//! expired screen, or stop and show the error. Pure logic; the supervision loop feeds it
//! each `PlaybackEvent::Exited` and the time it arrived.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Waits before each restart in a row; the last one repeats.
const BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
/// This many failures within `WINDOW` is a crash loop; one exactly `WINDOW` old counts.
const GIVE_UP_AFTER: usize = 5;
const WINDOW: Duration = Duration::from_secs(120);
/// Soloist's exit code for an expired build.
const EXPIRED: i32 = 10;

/// What to do after Soloist exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Relaunch Soloist after this long.
    RestartAfter(Duration),
    /// Exit code 10: the build expired. Restarting won't help.
    Expired,
    /// Too many failures too fast: stop and show the error with Soloist's log tail.
    GiveUp,
}

/// Restart policy for one Soloist supervisor. Every exit but code 10 is a failure: an
/// error code, a signal (`None`) or a clean 0 all mean Soloist stopped when it shouldn't
/// have. Don't feed it exits the supervisor caused itself (a stop, a planned relaunch).
#[derive(Debug, Default)]
pub struct RestartPolicy {
    /// Index into `BACKOFF` for the next restart.
    step: usize,
    /// When the failures still in the window happened, oldest first.
    failures: VecDeque<Instant>,
}

impl RestartPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Soloist exited with `code` at `now`.
    pub fn on_exit(&mut self, code: Option<i32>, now: Instant) -> Decision {
        if code == Some(EXPIRED) {
            return Decision::Expired;
        }
        self.failures
            .retain(|&at| now.saturating_duration_since(at) <= WINDOW);
        self.failures.push_back(now);
        if self.failures.len() >= GIVE_UP_AFTER {
            return Decision::GiveUp;
        }
        let wait = BACKOFF[self.step.min(BACKOFF.len() - 1)];
        self.step += 1;
        Decision::RestartAfter(wait)
    }

    /// Soloist has run for 2 minutes: the next restart waits 1 s again. The failure
    /// window is left alone; it ages out by itself.
    pub fn on_healthy(&mut self) {
        self.step = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Feed one exit per `(seconds after t0, code)` and collect the decisions.
    fn run(p: &mut RestartPolicy, t0: Instant, exits: &[(u64, Option<i32>)]) -> Vec<Decision> {
        exits
            .iter()
            .map(|&(at, code)| p.on_exit(code, t0 + secs(at)))
            .collect()
    }

    #[test]
    fn backoff_sequence() {
        // 60 s apart, so the 120 s window never holds 5 failures.
        let mut p = RestartPolicy::new();
        let got = run(
            &mut p,
            Instant::now(),
            &[(0, Some(1)), (60, Some(1)), (120, Some(1)), (180, Some(1))],
        );
        let want = [1, 2, 5, 10].map(|s| Decision::RestartAfter(secs(s)));
        assert_eq!(got, want);
    }

    #[test]
    fn backoff_holds_at_30_s() {
        let mut p = RestartPolicy::new();
        let exits: Vec<_> = (0..8).map(|i| (i * 60, Some(1))).collect();
        let got = run(&mut p, Instant::now(), &exits);
        let want = [1, 2, 5, 10, 30, 30, 30, 30].map(|s| Decision::RestartAfter(secs(s)));
        assert_eq!(got, want);
    }

    #[test]
    fn fifth_within_two_minutes_gives_up() {
        // Any exit but 10 is a failure: an error code, a signal (`None`), even a clean 0.
        let mut p = RestartPolicy::new();
        let got = run(
            &mut p,
            Instant::now(),
            &[
                (0, Some(1)),
                (25, None),
                (50, Some(0)),
                (75, Some(134)),
                (100, Some(1)),
            ],
        );
        let mut want: Vec<_> = [1, 2, 5, 10]
            .map(|s| Decision::RestartAfter(secs(s)))
            .into();
        want.push(Decision::GiveUp);
        assert_eq!(got, want);
    }

    #[test]
    fn window_is_120_s_inclusive() {
        // A failure exactly 120 s old is still in the window...
        let t0 = Instant::now();
        let mut p = RestartPolicy::new();
        let got = run(
            &mut p,
            t0,
            &[(0, None), (30, None), (60, None), (90, None), (120, None)],
        );
        assert_eq!(got[4], Decision::GiveUp);

        // ...one a moment older has dropped out, so the 5th exit restarts (on 30 s).
        let mut p = RestartPolicy::new();
        run(&mut p, t0, &[(0, None), (30, None), (60, None), (90, None)]);
        let later = t0 + secs(120) + Duration::from_millis(1);
        assert_eq!(p.on_exit(None, later), Decision::RestartAfter(secs(30)));
    }

    #[test]
    fn exit_10_is_expired() {
        let mut p = RestartPolicy::new();
        assert_eq!(p.on_exit(Some(10), Instant::now()), Decision::Expired);
    }

    #[test]
    fn exit_10_is_expired_even_with_a_full_window() {
        let t0 = Instant::now();
        let mut p = RestartPolicy::new();
        let got = run(
            &mut p,
            t0,
            &[(0, None), (1, None), (2, None), (3, None), (4, None)],
        );
        assert_eq!(got[4], Decision::GiveUp);
        assert_eq!(p.on_exit(Some(10), t0 + secs(5)), Decision::Expired);
    }

    #[test]
    fn exit_10_is_not_a_failure() {
        // Expiries between failures neither fill the window (six exits in 50 s, but only
        // four failures: no GiveUp) nor advance the backoff (still 1, 2, 5, 10 s).
        let t0 = Instant::now();
        let mut p = RestartPolicy::new();
        let got = run(
            &mut p,
            t0,
            &[
                (0, Some(1)),
                (10, Some(1)),
                (20, Some(10)),
                (30, Some(1)),
                (40, Some(10)),
            ],
        );
        let want = [
            Decision::RestartAfter(secs(1)),
            Decision::RestartAfter(secs(2)),
            Decision::Expired,
            Decision::RestartAfter(secs(5)),
            Decision::Expired,
        ];
        assert_eq!(got, want);
        assert_eq!(
            p.on_exit(Some(1), t0 + secs(50)),
            Decision::RestartAfter(secs(10))
        );
    }

    #[test]
    fn healthy_resets() {
        let t0 = Instant::now();
        let mut p = RestartPolicy::new();
        run(&mut p, t0, &[(0, Some(1)), (60, Some(1)), (120, Some(1))]);
        p.on_healthy();
        let got = run(&mut p, t0, &[(300, Some(1)), (360, Some(1))]);
        assert_eq!(got, [1, 2].map(|s| Decision::RestartAfter(secs(s))));
    }

    #[test]
    fn healthy_keeps_the_crash_window() {
        // `on_healthy` resets the backoff only; the failures still in the window count.
        let t0 = Instant::now();
        let mut p = RestartPolicy::new();
        run(&mut p, t0, &[(0, None), (10, None), (20, None), (30, None)]);
        p.on_healthy();
        assert_eq!(p.on_exit(None, t0 + secs(40)), Decision::GiveUp);
    }
}
