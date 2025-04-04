use crate::config::PRINT_RETRY_ERROR_LIMIT;
use crate::stats::latency::LatencyDistributionRecorder;
use scylla::errors::ExecutionError;
use scylla::response::query_result::QueryResult;
use scylla::response::PagingStateResponse;
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Debug)]
pub struct SessionStats {
    pub req_count: u64,
    pub req_errors: HashSet<String>,
    pub req_error_count: u64,
    pub req_retry_errors: HashSet<String>,
    pub req_retry_count: u64,
    pub row_count: u64,
    pub queue_length: u64,
    pub mean_queue_length: f32,
    pub resp_times_ns: LatencyDistributionRecorder,
}

impl SessionStats {
    pub fn new() -> SessionStats {
        Default::default()
    }

    pub fn start_request(&mut self) -> Instant {
        if self.req_count > 0 {
            self.mean_queue_length +=
                (self.queue_length as f32 - self.mean_queue_length) / self.req_count as f32;
        }
        self.queue_length += 1;
        Instant::now()
    }

    pub fn complete_request(
        &mut self,
        duration: Duration,
        total_rows: Option<u64>,
        rs: &Result<(QueryResult, PagingStateResponse), ExecutionError>,
    ) {
        self.queue_length -= 1;
        self.resp_times_ns.record(duration);
        self.req_count += 1;
        match rs {
            Ok((ref page, _)) => match total_rows {
                Some(n) => self.row_count += n,
                None => {
                    self.row_count += page
                        .clone()
                        .into_rows_result()
                        .expect("Failed to read rows from the response")
                        .rows_num() as u64
                }
            },
            Err(e) => {
                self.req_error_count += 1;
                self.req_errors.insert(format!("{e}"));
            }
        }
    }

    pub fn complete_request_batch(
        &mut self,
        duration: Duration,
        total_rows: Option<u64>,
        rs: &Result<QueryResult, ExecutionError>,
    ) {
        self.queue_length -= 1;
        self.resp_times_ns.record(duration);
        self.req_count += 1;
        match rs {
            Ok(rs) => match total_rows {
                Some(n) => self.row_count += n,
                None => {
                    self.row_count += rs
                        .clone()
                        .into_rows_result()
                        .map(|r| r.rows_num())
                        .unwrap_or(0) as u64
                }
            },
            Err(e) => {
                self.req_error_count += 1;
                self.req_errors.insert(format!("{e}"));
            }
        }
    }

    pub fn store_retry_error(&mut self, error_str: String) {
        self.req_retry_count += 1;
        if self.req_retry_count <= PRINT_RETRY_ERROR_LIMIT {
            self.req_retry_errors.insert(error_str);
        }
    }

    /// Resets all accumulators
    pub fn reset(&mut self) {
        self.req_error_count = 0;
        self.row_count = 0;
        self.req_count = 0;
        self.req_retry_count = 0;
        self.mean_queue_length = 0.0;
        self.req_errors.clear();
        self.req_retry_errors.clear();
        self.resp_times_ns.clear();

        // note that current queue_length is *not* reset to zero because there
        // might be pending requests and if we set it to zero, that would underflow
    }
}

impl Default for SessionStats {
    fn default() -> Self {
        SessionStats {
            req_count: 0,
            req_errors: HashSet::new(),
            req_error_count: 0,
            req_retry_errors: HashSet::new(),
            req_retry_count: 0,
            row_count: 0,
            queue_length: 0,
            mean_queue_length: 0.0,
            resp_times_ns: LatencyDistributionRecorder::default(),
        }
    }
}
