use chrono::Utc;
use rand::random;
use std::time::Duration;
use tracing::error;

use super::context::Context;
use super::db_error::DbError;

pub fn get_exponential_retry_interval(
    min_interval: Duration,
    max_interval: Duration,
    current_attempt_num: u64,
) -> Duration {
    let min_interval_float: f64 = min_interval.as_secs_f64();
    let mut current_interval: f64 =
        min_interval_float * ((1u64 << current_attempt_num.min(63)) as f64);

    // Add jitter
    current_interval += random::<f64>() * min_interval_float;
    current_interval -= min_interval_float / 2.0;

    Duration::from_secs_f64(current_interval.min(max_interval.as_secs_f64()))
}

pub async fn handle_retry_error(ctxt: &Context, current_attempt_num: u64, current_error: DbError) {
    let current_retry_interval = get_exponential_retry_interval(
        ctxt.retry_interval.min,
        ctxt.retry_interval.max,
        current_attempt_num,
    );

    let mut next_attempt_str = String::new();
    let is_last_attempt = current_attempt_num == ctxt.retry_number;
    if !is_last_attempt {
        next_attempt_str += &format!("[Retry in {} ms]", current_retry_interval.as_millis());
    }
    let err_msg = format!(
        "{}: [ERROR][Attempt {}/{}]{} {}",
        Utc::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        current_attempt_num,
        ctxt.retry_number,
        next_attempt_str,
        current_error,
    );
    error!("{}", err_msg);
    if !is_last_attempt {
        ctxt.stats.try_lock().unwrap().store_retry_error(err_msg);
        tokio::time::sleep(current_retry_interval).await;
    } else {
        eprintln!("{err_msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_grows_with_attempts_and_is_capped_by_max() {
        let min = Duration::from_millis(100);
        let max = Duration::from_secs(5);
        let first = get_exponential_retry_interval(min, max, 1);
        assert!(first >= Duration::from_millis(150) && first < Duration::from_millis(250));
        assert!(get_exponential_retry_interval(min, max, 10) <= max);
    }

    #[test]
    fn large_attempt_numbers_do_not_overflow() {
        let min = Duration::from_millis(100);
        let max = Duration::from_secs(5);
        // Attempt 64 used to wrap the power to 0, making the jittered interval
        // negative and panicking Duration::from_secs_f64.
        for attempt in [63, 64, 65, 1_000, u64::MAX] {
            let interval = get_exponential_retry_interval(min, max, attempt);
            assert!(interval <= max, "attempt {attempt}: {interval:?} > max");
            assert!(
                interval >= max - min / 2,
                "attempt {attempt}: {interval:?} lost the exponential cap"
            );
        }
    }
}
