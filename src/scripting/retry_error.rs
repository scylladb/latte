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
        min_interval_float * (2u64.pow(current_attempt_num.try_into().unwrap_or(0)) as f64);

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
