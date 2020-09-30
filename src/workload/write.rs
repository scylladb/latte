use std::sync::Arc;

use async_trait::async_trait;
use cassandra_cpp::{stmt, BindRustType, PreparedStatement, Session};

use crate::workload::{Result, Workload, WorkloadError, WorkloadStats};

/// A workload that writes tiny rows to the table
///
/// Preparation:
/// ```
/// CREATE TABLE write(pk BIGINT PRIMARY KEY, c1 BIGINT, c2 BIGINT, c3 BIGINT, c4 BIGINT, c5 BIGINT)
/// ```
///
/// Benchmarked query:
/// ```
/// INSERT INTO write(pk, c1, c2, c3, c4, c5) VALUES (?, 1, 2, 3, 4, 5)
/// ```
pub struct Write<S>
where
    S: AsRef<Session> + Sync + Send,
{
    session: S,
    row_count: u64,
    write_statement: PreparedStatement,
}

impl<S> Write<S>
where
    S: AsRef<Session> + Sync + Send,
{
    pub async fn new(session: S, row_count: u64) -> Result<Self> {
        if row_count == 0 {
            return Err(WorkloadError::Other(
                "Number of rows cannot be 0 for a write workload".to_owned(),
            ));
        }

        let s = session.as_ref();
        let result = s.execute(&stmt!(
            "CREATE TABLE IF NOT EXISTS write (pk BIGINT PRIMARY KEY, \
            c1 BIGINT, c2 BIGINT, c3 BIGINT, c4 BIGINT, c5 BIGINT)"
        ));
        result.await?;

        let write_statement = s
            .prepare("INSERT INTO write(pk, c1, c2, c3, c4, c5) VALUES (?, 1, 2, 3, 4, 5)")?
            .await?;
        Ok(Write {
            session,
            row_count,
            write_statement,
        })
    }
}

#[async_trait]
impl<S> Workload for Write<S>
where
    S: AsRef<Session> + Sync + Send,
{
    fn populate_count(&self) -> u64 {
        0
    }

    async fn populate(self: Arc<Self>, _iteration: u64) -> Result<WorkloadStats>
    where
        S: 'async_trait,
    {
        Ok(WorkloadStats {
            partition_count: 0,
            row_count: 0,
        })
    }

    async fn run(self: Arc<Self>, iteration: u64) -> Result<WorkloadStats>
    where
        S: 'async_trait,
    {
        let s = self.session.as_ref();
        let mut statement = self.write_statement.bind();
        statement.bind(0, (iteration % self.row_count) as i64)?;
        let result = s.execute(&statement);
        result.await?;
        Ok(WorkloadStats {
            partition_count: 1,
            row_count: 1,
        })
    }
}