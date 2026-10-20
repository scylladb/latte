use chrono::Utc;
use std::time::{Duration, Instant};

/// Lag behind the request schedule above which a worker is reported as falling behind.
const LAG_WARN_THRESHOLD: Duration = Duration::from_secs(1);

/// Time spent waiting for the stats consumer above which a worker is reported.
const STATS_WAIT_WARN_THRESHOLD: Duration = Duration::from_millis(100);

/// Minimum time between two warnings about the same worker.
const WARN_INTERVAL: Duration = Duration::from_secs(30);

/// What a worker observed during one sampling period.
pub struct PeriodObservation {
    /// The biggest delay between the scheduled and the actual start of a cycle
    pub max_lag: Duration,
    /// The biggest number of cycles in flight during the period
    pub max_in_flight: usize,
    /// Mean latency of the requests completed in the period
    pub mean_request_latency: Option<Duration>,
    /// Number of cycles completed in the period
    pub cycles: u64,
    /// Duration of the period
    pub duration: Duration,
    /// Time the worker spent blocked on a full stats queue instead of issuing requests
    pub stats_wait: Duration,
}

/// Detects a worker falling behind its request schedule and describes it.
///
/// A worker which completes far fewer cycles than its in-flight cycles and request latency
/// allow is not limited by the database, but is not polled often enough itself.
pub struct ScheduleLagMonitor {
    worker_id: usize,
    concurrency: usize,
    last_warning: Option<Instant>,
}

impl ScheduleLagMonitor {
    pub fn new(worker_id: usize, concurrency: usize) -> Self {
        Self {
            worker_id,
            concurrency,
            last_warning: None,
        }
    }

    /// Returns a message to report for the period, if any.
    /// Repeated warnings are throttled, and a message is returned once the worker recovers.
    pub fn check(&mut self, now: Instant, obs: &PeriodObservation) -> Option<String> {
        let is_lagging =
            obs.max_lag >= LAG_WARN_THRESHOLD || obs.stats_wait >= STATS_WAIT_WARN_THRESHOLD;
        let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
        if !is_lagging {
            return self.last_warning.take().map(|_| {
                format!(
                    "{timestamp}: [INFO] Worker {} is back on schedule",
                    self.worker_id
                )
            });
        }
        if self
            .last_warning
            .is_some_and(|last| now.duration_since(last) < WARN_INTERVAL)
        {
            return None;
        }
        self.last_warning = Some(now);
        let limit_note = if obs.max_in_flight >= self.concurrency {
            " (concurrency limit reached)"
        } else {
            ""
        };
        let request_latency = obs.mean_request_latency.map_or("n/a".to_string(), |l| {
            format!("{:.3}ms", l.as_secs_f64() * 1000.0)
        });
        Some(format!(
            "{timestamp}: [WARN] Worker {} is {:.3}s behind schedule: {} cycles in {:.3}s, \
             up to {}/{} cycles in flight{limit_note}, mean request latency {request_latency}, \
             {:.3}s blocked on a full stats queue",
            self.worker_id,
            obs.max_lag.as_secs_f64(),
            obs.cycles,
            obs.duration.as_secs_f64(),
            obs.max_in_flight,
            self.concurrency,
            obs.stats_wait.as_secs_f64(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(max_lag_ms: u64, in_flight: usize, stats_wait_ms: u64) -> PeriodObservation {
        PeriodObservation {
            max_lag: Duration::from_millis(max_lag_ms),
            max_in_flight: in_flight,
            mean_request_latency: Some(Duration::from_micros(4200)),
            cycles: 50,
            duration: Duration::from_secs(1),
            stats_wait: Duration::from_millis(stats_wait_ms),
        }
    }

    #[test]
    fn stays_silent_while_on_schedule() {
        let mut monitor = ScheduleLagMonitor::new(0, 64);
        let now = Instant::now();
        assert_eq!(monitor.check(now, &observation(10, 5, 0)), None);
        assert_eq!(monitor.check(now, &observation(999, 64, 99)), None);
    }

    #[test]
    fn warns_about_lag_and_reports_in_flight_cycles() {
        let mut monitor = ScheduleLagMonitor::new(3, 64);
        let msg = monitor
            .check(Instant::now(), &observation(2500, 2, 0))
            .unwrap();
        assert!(
            msg.contains("[WARN] Worker 3 is 2.500s behind schedule"),
            "{msg}"
        );
        assert!(msg.contains("up to 2/64 cycles in flight,"), "{msg}");
        assert!(msg.contains("mean request latency 4.200ms"), "{msg}");
        assert!(!msg.contains("concurrency limit reached"), "{msg}");
    }

    #[test]
    fn marks_lag_caused_by_concurrency_limit() {
        let mut monitor = ScheduleLagMonitor::new(0, 64);
        let msg = monitor
            .check(Instant::now(), &observation(2500, 64, 0))
            .unwrap();
        assert!(
            msg.contains("64/64 cycles in flight (concurrency limit reached)"),
            "{msg}"
        );
    }

    #[test]
    fn reports_missing_request_latency() {
        let mut monitor = ScheduleLagMonitor::new(0, 64);
        let mut obs = observation(2500, 0, 0);
        obs.mean_request_latency = None;
        let msg = monitor.check(Instant::now(), &obs).unwrap();
        assert!(msg.contains("mean request latency n/a"), "{msg}");
    }

    #[test]
    fn warns_about_blocking_on_stats_queue() {
        let mut monitor = ScheduleLagMonitor::new(0, 64);
        let msg = monitor
            .check(Instant::now(), &observation(10, 0, 1500))
            .unwrap();
        assert!(
            msg.contains("1.500s blocked on a full stats queue"),
            "{msg}"
        );
    }

    #[test]
    fn throttles_repeated_warnings_and_reports_recovery() {
        let mut monitor = ScheduleLagMonitor::new(1, 64);
        let start = Instant::now();
        assert!(monitor.check(start, &observation(2000, 1, 0)).is_some());
        assert!(monitor
            .check(start + Duration::from_secs(1), &observation(3000, 1, 0))
            .is_none());
        assert!(monitor
            .check(start + WARN_INTERVAL, &observation(4000, 1, 0))
            .is_some());
        let msg = monitor
            .check(start + WARN_INTERVAL, &observation(10, 1, 0))
            .unwrap();
        assert!(msg.contains("[INFO] Worker 1 is back on schedule"), "{msg}");
        assert!(monitor
            .check(start + WARN_INTERVAL, &observation(10, 1, 0))
            .is_none());
    }
}
