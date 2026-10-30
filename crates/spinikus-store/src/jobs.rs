use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::{
    id::JobId,
    job::{self, Job, JobKind, JobPayload, JobStatus, NewJob}
};
use sqlx::SqliteConnection;

use crate::{SqliteStore, StoreError, db::format_timestamp, error::parse_column};

const ENTITY: &str = "job";

#[async_trait]
pub trait JobRepository: Send + Sync {
    /// Queues a job. If it has a dedupe key and a job with that key is still queued or
    /// running, that job is returned instead and nothing is queued.
    async fn enqueue_job(&self, new: NewJob) -> Result<Job, StoreError>;

    async fn get_job(&self, id: JobId) -> Result<Job, StoreError>;

    /// Jobs newest first, optionally only those in `status`, starting before the `before` cursor.
    async fn list_jobs(&self, status: Option<JobStatus>, before: Option<JobId>, limit: u32) -> Result<Vec<Job>, StoreError>;

    /// Leases the next due job of one of `kinds` to `worker` for `lease`, starting its next
    /// attempt: the queued job with the earliest `run_at`, or a running job whose lease ran out.
    /// Jobs whose lease ran out on their last attempt are failed first.
    async fn claim_job(&self, worker: &str, kinds: &[JobKind], lease: Duration) -> Result<Option<Job>, StoreError>;

    /// Extends the lease `worker` holds to `lease` from now. Fails with `Rejected` if the
    /// worker no longer holds it, in which case it should stop working on the job.
    async fn extend_job_lease(&self, id: JobId, worker: &str, lease: Duration) -> Result<Job, StoreError>;

    /// Marks the job `worker` holds as succeeded.
    async fn complete_job(&self, id: JobId, worker: &str) -> Result<Job, StoreError>;

    /// Records a failed attempt of the job `worker` holds. If `retry` is set and attempts
    /// remain, the job is queued again after `job::retry_delay`; otherwise it fails.
    async fn fail_job(&self, id: JobId, worker: &str, error: &str, retry: bool) -> Result<Job, StoreError>;

    /// Cancels a queued job. A running job cannot be canceled here: its handler owns it.
    async fn cancel_job(&self, id: JobId) -> Result<Job, StoreError>;

    /// Deletes finished jobs that finished before `before`, returning how many.
    async fn delete_finished_jobs(&self, before: Timestamp) -> Result<u64, StoreError>;
}

macro_rules! columns {
    () => {
        "id, kind, payload, status, attempts, max_attempts, run_at, dedupe_key, locked_by, locked_until, last_error,
         created_at, updated_at, finished_at"
    };
}

macro_rules! now {
    () => {
        "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')"
    };
}

#[derive(sqlx::FromRow)]
struct JobRow {
    id: String,
    kind: String,
    payload: String,
    status: String,
    attempts: u32,
    max_attempts: u32,
    run_at: String,
    dedupe_key: Option<String>,
    locked_by: Option<String>,
    locked_until: Option<String>,
    last_error: Option<String>,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>
}

impl TryFrom<JobRow> for Job {
    type Error = StoreError;

    fn try_from(row: JobRow) -> Result<Self, Self::Error> {
        Ok(Job {
            id: parse_column(ENTITY, "id", &row.id)?,
            kind: parse_column(ENTITY, "kind", &row.kind)?,
            payload: serde_json::from_str::<JobPayload>(&row.payload).map_err(|error| StoreError::InvalidRow {
                entity: ENTITY,
                column: "payload",
                problem: error.to_string()
            })?,
            status: parse_column(ENTITY, "status", &row.status)?,
            attempts: row.attempts,
            max_attempts: row.max_attempts,
            run_at: parse_column(ENTITY, "run_at", &row.run_at)?,
            dedupe_key: row.dedupe_key,
            locked_by: row.locked_by,
            locked_until: row.locked_until.map(|at| parse_column(ENTITY, "locked_until", &at)).transpose()?,
            last_error: row.last_error,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?,
            finished_at: row.finished_at.map(|at| parse_column(ENTITY, "finished_at", &at)).transpose()?
        })
    }
}

fn not_found(id: JobId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

fn lease_not_held(id: JobId) -> StoreError {
    StoreError::Rejected { entity: ENTITY, id: id.to_string(), problem: "the worker does not hold the job's lease" }
}

/// A SQLite date modifier that adds `duration` to a time, such as `+30.000 seconds`.
fn plus(duration: Duration) -> String {
    format!("+{:.3} seconds", duration.as_secs_f64())
}

async fn fetch(conn: &mut SqliteConnection, id: JobId) -> Result<Job, StoreError> {
    let row: Option<JobRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM jobs WHERE id = ?"))
        .bind(id.to_string())
        .fetch_optional(conn)
        .await?;
    row.ok_or_else(|| not_found(id))?.try_into()
}

/// For an update that matched nothing: `NotFound` if the job is missing, `otherwise` if not.
async fn explain_miss(conn: &mut SqliteConnection, id: JobId, otherwise: StoreError) -> StoreError {
    match fetch(conn, id).await {
        Ok(_) => otherwise,
        Err(error) => error
    }
}

#[async_trait]
impl JobRepository for SqliteStore {
    async fn enqueue_job(&self, new: NewJob) -> Result<Job, StoreError> {
        let mut tx = self.begin_write().await?;
        let row: Option<JobRow> = sqlx::query_as(concat!(
            "INSERT INTO jobs (id, kind, payload, status, max_attempts, run_at, dedupe_key)
             VALUES (?, ?, ?, 'queued', ?, coalesce(?, ", now!(), "), ?)
             ON CONFLICT (dedupe_key) WHERE dedupe_key IS NOT NULL AND status IN ('queued', 'running') DO NOTHING
             RETURNING ",
            columns!()
        ))
        .bind(JobId::new().to_string())
        .bind(new.kind.as_str())
        .bind(serde_json::Value::Object(new.payload).to_string())
        .bind(new.max_attempts)
        .bind(new.run_at.map(format_timestamp))
        .bind(&new.dedupe_key)
        .fetch_optional(&mut *tx)
        .await?;

        let row = match row {
            Some(row) => row,
            None => {
                sqlx::query_as(concat!(
                    "SELECT ", columns!(), " FROM jobs WHERE dedupe_key = ? AND status IN ('queued', 'running')"
                ))
                .bind(&new.dedupe_key)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        row.try_into()
    }

    async fn get_job(&self, id: JobId) -> Result<Job, StoreError> {
        fetch(&mut *self.pool().acquire().await?, id).await
    }

    async fn list_jobs(&self, status: Option<JobStatus>, before: Option<JobId>, limit: u32) -> Result<Vec<Job>, StoreError> {
        // `~` sorts after every character used in IDs, so no cursor means "from the newest".
        let rows: Vec<JobRow> = sqlx::query_as(concat!(
            "SELECT ", columns!(), " FROM jobs WHERE (?1 IS NULL OR status = ?1) AND id < ?2 ORDER BY id DESC LIMIT ?3"
        ))
        .bind(status.map(JobStatus::as_str))
        .bind(before.map(|id| id.to_string()).unwrap_or_else(|| "~".to_string()))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        rows.into_iter().map(Job::try_from).collect()
    }

    async fn claim_job(&self, worker: &str, kinds: &[JobKind], lease: Duration) -> Result<Option<Job>, StoreError> {
        let kinds = serde_json::to_string(&kinds).expect("job kinds serialize to JSON");
        let mut tx = self.begin_write().await?;

        sqlx::query(concat!(
            "UPDATE jobs
             SET status = 'failed', last_error = 'the lease ran out on the last attempt', locked_by = NULL,
                 locked_until = NULL, finished_at = ", now!(), ", updated_at = ", now!(), "
             WHERE status = 'running' AND locked_until < ", now!(), " AND attempts >= max_attempts"
        ))
        .execute(&mut *tx)
        .await?;

        let row: Option<JobRow> = sqlx::query_as(concat!(
            "UPDATE jobs
             SET last_error = CASE WHEN status = 'running' THEN 'the lease ran out' ELSE last_error END,
                 status = 'running', attempts = attempts + 1, locked_by = ?,
                 locked_until = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?), updated_at = ", now!(), "
             WHERE id = (
                 SELECT id FROM jobs
                 WHERE kind IN (SELECT value FROM json_each(?))
                   AND ((status = 'queued' AND run_at <= ", now!(), ") OR (status = 'running' AND locked_until < ", now!(), "))
                 ORDER BY run_at, id
                 LIMIT 1
             )
             RETURNING ",
            columns!()
        ))
        .bind(worker)
        .bind(plus(lease))
        .bind(kinds)
        .fetch_optional(&mut *tx)
        .await?;

        tx.commit().await?;
        row.map(Job::try_from).transpose()
    }

    async fn extend_job_lease(&self, id: JobId, worker: &str, lease: Duration) -> Result<Job, StoreError> {
        let mut conn = self.pool().acquire().await?;
        let row: Option<JobRow> = sqlx::query_as(concat!(
            "UPDATE jobs SET locked_until = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?), updated_at = ", now!(), "
             WHERE id = ? AND status = 'running' AND locked_by = ?
             RETURNING ",
            columns!()
        ))
        .bind(plus(lease))
        .bind(id.to_string())
        .bind(worker)
        .fetch_optional(&mut *conn)
        .await?;

        match row {
            Some(row) => row.try_into(),
            None => Err(explain_miss(&mut conn, id, lease_not_held(id)).await)
        }
    }

    async fn complete_job(&self, id: JobId, worker: &str) -> Result<Job, StoreError> {
        let mut conn = self.pool().acquire().await?;
        let row: Option<JobRow> = sqlx::query_as(concat!(
            "UPDATE jobs
             SET status = 'succeeded', locked_by = NULL, locked_until = NULL, finished_at = ", now!(), ",
                 updated_at = ", now!(), "
             WHERE id = ? AND status = 'running' AND locked_by = ?
             RETURNING ",
            columns!()
        ))
        .bind(id.to_string())
        .bind(worker)
        .fetch_optional(&mut *conn)
        .await?;

        match row {
            Some(row) => row.try_into(),
            None => Err(explain_miss(&mut conn, id, lease_not_held(id)).await)
        }
    }

    async fn fail_job(&self, id: JobId, worker: &str, error: &str, retry: bool) -> Result<Job, StoreError> {
        let mut tx = self.begin_write().await?;
        let job = fetch(&mut tx, id).await?;
        if job.status != JobStatus::Running || job.locked_by.as_deref() != Some(worker) {
            return Err(lease_not_held(id));
        }

        if retry && job.attempts < job.max_attempts {
            sqlx::query(concat!(
                "UPDATE jobs
                 SET status = 'queued', run_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?), last_error = ?,
                     locked_by = NULL, locked_until = NULL, updated_at = ", now!(), "
                 WHERE id = ?"
            ))
            .bind(plus(job::retry_delay(job.attempts)))
            .bind(error)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(concat!(
                "UPDATE jobs
                 SET status = 'failed', last_error = ?, locked_by = NULL, locked_until = NULL,
                     finished_at = ", now!(), ", updated_at = ", now!(), "
                 WHERE id = ?"
            ))
            .bind(error)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        }

        let job = fetch(&mut tx, id).await?;
        tx.commit().await?;
        Ok(job)
    }

    async fn cancel_job(&self, id: JobId) -> Result<Job, StoreError> {
        let mut conn = self.pool().acquire().await?;
        let row: Option<JobRow> = sqlx::query_as(concat!(
            "UPDATE jobs SET status = 'canceled', finished_at = ", now!(), ", updated_at = ", now!(), "
             WHERE id = ? AND status = 'queued'
             RETURNING ",
            columns!()
        ))
        .bind(id.to_string())
        .fetch_optional(&mut *conn)
        .await?;

        match row {
            Some(row) => row.try_into(),
            None => {
                let rejected = StoreError::Rejected { entity: ENTITY, id: id.to_string(), problem: "only a queued job can be canceled" };
                Err(explain_miss(&mut conn, id, rejected).await)
            }
        }
    }

    async fn delete_finished_jobs(&self, before: Timestamp) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM jobs WHERE finished_at < ?")
            .bind(format_timestamp(before))
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::test_support::store;

    const LEASE: Duration = Duration::from_secs(60);

    fn kind(kind: &str) -> JobKind {
        kind.parse().unwrap()
    }

    fn new_job(kind_name: &str) -> NewJob {
        NewJob::new(kind(kind_name), json!({ "service_id": "svc_1" }).as_object().unwrap().clone())
    }

    /// Moves a column into the past, standing in for time passing.
    async fn backdate(store: &SqliteStore, id: JobId, column: &str) {
        let query = match column {
            "run_at" => "UPDATE jobs SET run_at = '2000-01-01T00:00:00.000Z' WHERE id = ?",
            "locked_until" => "UPDATE jobs SET locked_until = '2000-01-01T00:00:00.000Z' WHERE id = ?",
            _ => unreachable!()
        };
        sqlx::query(query).bind(id.to_string()).execute(store.pool()).await.unwrap();
    }

    #[tokio::test]
    async fn enqueues_a_job() {
        let (_dir, store) = store().await;

        let job = store.enqueue_job(new_job("backup.run")).await.unwrap();

        assert_eq!(store.get_job(job.id).await.unwrap(), job);
        assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Queued, 0, 3));
        assert_eq!(job.payload["service_id"], "svc_1");
        assert!(job.run_at <= Timestamp::now());
    }

    #[tokio::test]
    async fn a_dedupe_key_returns_the_unfinished_job() {
        let (_dir, store) = store().await;
        let keyed = || NewJob { dedupe_key: Some("deploy:svc_1".to_string()), ..new_job("deployment.prepare") };
        let first = store.enqueue_job(keyed()).await.unwrap();

        let again = store.enqueue_job(keyed()).await.unwrap();
        store.claim_job("w1", &[kind("deployment.prepare")], LEASE).await.unwrap();
        let while_running = store.enqueue_job(keyed()).await.unwrap();
        store.complete_job(first.id, "w1").await.unwrap();
        let after_finishing = store.enqueue_job(keyed()).await.unwrap();

        assert_eq!(again.id, first.id);
        assert_eq!(while_running.id, first.id);
        assert_ne!(after_finishing.id, first.id);
        assert_eq!(after_finishing.status, JobStatus::Queued);
    }

    #[tokio::test]
    async fn claims_due_jobs_of_the_given_kinds_in_order() {
        let (_dir, store) = store().await;
        let later = store.enqueue_job(new_job("backup.run")).await.unwrap();
        let earlier = store.enqueue_job(new_job("backup.run")).await.unwrap();
        backdate(&store, earlier.id, "run_at").await;
        store.enqueue_job(new_job("cleanup.images")).await.unwrap();
        let future = NewJob { run_at: Some(Timestamp::now() + jiff::SignedDuration::from_hours(1)), ..new_job("backup.run") };
        store.enqueue_job(future).await.unwrap();

        let backups = [kind("backup.run")];
        let first = store.claim_job("w1", &backups, LEASE).await.unwrap().unwrap();
        let second = store.claim_job("w1", &backups, LEASE).await.unwrap().unwrap();
        let none = store.claim_job("w1", &backups, LEASE).await.unwrap();

        assert_eq!((first.id, second.id), (earlier.id, later.id));
        assert!(none.is_none(), "the cleanup job and the future job are not claimed");
        assert_eq!((first.status, first.attempts, first.locked_by.as_deref()), (JobStatus::Running, 1, Some("w1")));
        assert!(first.locked_until.unwrap() > Timestamp::now() + jiff::SignedDuration::from_secs(55));
        assert!(store.claim_job("w1", &[], LEASE).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn concurrent_claims_never_share_a_job() {
        let (_dir, store) = store().await;
        for _ in 0..3 {
            store.enqueue_job(new_job("backup.run")).await.unwrap();
        }

        let mut claims = tokio::task::JoinSet::new();
        for n in 0..6 {
            let store = store.clone();
            claims.spawn(async move { store.claim_job(&format!("w{n}"), &[kind("backup.run")], LEASE).await.unwrap() });
        }

        let mut claimed: Vec<JobId> = claims.join_all().await.into_iter().flatten().map(|job| job.id).collect();
        claimed.sort();
        claimed.dedup();
        assert_eq!(claimed.len(), 3);
    }

    #[tokio::test]
    async fn only_the_lease_holder_can_extend_or_complete() {
        let (_dir, store) = store().await;
        store.enqueue_job(new_job("backup.run")).await.unwrap();
        let job = store.claim_job("w1", &[kind("backup.run")], Duration::from_secs(5)).await.unwrap().unwrap();

        let extended = store.extend_job_lease(job.id, "w1", LEASE).await.unwrap();
        let foreign_extend = store.extend_job_lease(job.id, "w2", LEASE).await;
        let foreign_complete = store.complete_job(job.id, "w2").await;
        let foreign_fail = store.fail_job(job.id, "w2", "boom", true).await;
        let completed = store.complete_job(job.id, "w1").await.unwrap();
        let missing = store.complete_job(JobId::new(), "w1").await;

        assert!(extended.locked_until > job.locked_until);
        assert!(matches!(foreign_extend, Err(StoreError::Rejected { .. })), "{foreign_extend:?}");
        assert!(matches!(foreign_complete, Err(StoreError::Rejected { .. })), "{foreign_complete:?}");
        assert!(matches!(foreign_fail, Err(StoreError::Rejected { .. })), "{foreign_fail:?}");
        assert!(matches!(missing, Err(StoreError::NotFound { .. })), "{missing:?}");
        assert_eq!(completed.status, JobStatus::Succeeded);
        assert_eq!((completed.locked_by, completed.locked_until), (None, None));
        assert!(completed.finished_at.is_some());
    }

    #[tokio::test]
    async fn failed_attempts_are_retried_with_backoff_until_out_of_attempts() {
        let (_dir, store) = store().await;
        let job = store.enqueue_job(NewJob { max_attempts: 2, ..new_job("backup.run") }).await.unwrap();
        let kinds = [kind("backup.run")];

        store.claim_job("w1", &kinds, LEASE).await.unwrap().unwrap();
        let retrying = store.fail_job(job.id, "w1", "connection reset", true).await.unwrap();
        let too_early = store.claim_job("w1", &kinds, LEASE).await.unwrap();
        backdate(&store, job.id, "run_at").await;
        store.claim_job("w1", &kinds, LEASE).await.unwrap().unwrap();
        let failed = store.fail_job(job.id, "w1", "connection reset again", true).await.unwrap();

        assert_eq!((retrying.status, retrying.attempts), (JobStatus::Queued, 1));
        assert_eq!(retrying.last_error.as_deref(), Some("connection reset"));
        let delay = retrying.run_at.duration_since(retrying.updated_at);
        assert!((jiff::SignedDuration::from_secs(29)..=jiff::SignedDuration::from_secs(31)).contains(&delay), "{delay:?}");
        assert!(too_early.is_none());
        assert_eq!((failed.status, failed.attempts), (JobStatus::Failed, 2));
        assert!(failed.finished_at.is_some());
    }

    #[tokio::test]
    async fn a_permanent_failure_is_not_retried() {
        let (_dir, store) = store().await;
        let job = store.enqueue_job(new_job("backup.run")).await.unwrap();
        store.claim_job("w1", &[kind("backup.run")], LEASE).await.unwrap();

        let failed = store.fail_job(job.id, "w1", "bucket does not exist", false).await.unwrap();

        assert_eq!((failed.status, failed.attempts), (JobStatus::Failed, 1));
    }

    #[tokio::test]
    async fn a_lapsed_lease_is_claimed_again_until_out_of_attempts() {
        let (_dir, store) = store().await;
        let job = store.enqueue_job(NewJob { max_attempts: 2, ..new_job("backup.run") }).await.unwrap();
        let kinds = [kind("backup.run")];
        store.claim_job("w1", &kinds, LEASE).await.unwrap().unwrap();

        let still_leased = store.claim_job("w2", &kinds, LEASE).await.unwrap();
        backdate(&store, job.id, "locked_until").await;
        let reclaimed = store.claim_job("w2", &kinds, LEASE).await.unwrap().unwrap();
        let stale_worker = store.complete_job(job.id, "w1").await;
        backdate(&store, job.id, "locked_until").await;
        let nothing = store.claim_job("w3", &kinds, LEASE).await.unwrap();

        assert!(still_leased.is_none());
        assert_eq!((reclaimed.id, reclaimed.attempts, reclaimed.locked_by.as_deref()), (job.id, 2, Some("w2")));
        assert_eq!(reclaimed.last_error.as_deref(), Some("the lease ran out"));
        assert!(matches!(stale_worker, Err(StoreError::Rejected { .. })), "{stale_worker:?}");
        assert!(nothing.is_none());
        let failed = store.get_job(job.id).await.unwrap();
        assert_eq!(failed.status, JobStatus::Failed);
        assert_eq!(failed.last_error.as_deref(), Some("the lease ran out on the last attempt"));
    }

    #[tokio::test]
    async fn only_queued_jobs_can_be_canceled() {
        let (_dir, store) = store().await;
        let queued = store.enqueue_job(new_job("backup.run")).await.unwrap();
        let running = store.enqueue_job(new_job("cleanup.images")).await.unwrap();
        store.claim_job("w1", &[kind("cleanup.images")], LEASE).await.unwrap();

        let canceled = store.cancel_job(queued.id).await.unwrap();
        let refused = store.cancel_job(running.id).await;
        let missing = store.cancel_job(JobId::new()).await;

        assert_eq!(canceled.status, JobStatus::Canceled);
        assert!(canceled.finished_at.is_some());
        assert!(matches!(refused, Err(StoreError::Rejected { .. })), "{refused:?}");
        assert!(matches!(missing, Err(StoreError::NotFound { .. })), "{missing:?}");
    }

    #[tokio::test]
    async fn lists_newest_first_by_status() {
        let (_dir, store) = store().await;
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(store.enqueue_job(new_job("backup.run")).await.unwrap().id);
        }
        store.cancel_job(ids[1]).await.unwrap();

        let all: Vec<_> = store.list_jobs(None, None, 10).await.unwrap().into_iter().map(|job| job.id).collect();
        let queued: Vec<_> =
            store.list_jobs(Some(JobStatus::Queued), None, 10).await.unwrap().into_iter().map(|job| job.id).collect();
        let page: Vec<_> = store.list_jobs(None, Some(ids[2]), 1).await.unwrap().into_iter().map(|job| job.id).collect();

        assert_eq!(all, vec![ids[2], ids[1], ids[0]]);
        assert_eq!(queued, vec![ids[2], ids[0]]);
        assert_eq!(page, vec![ids[1]]);
    }

    #[tokio::test]
    async fn deletes_jobs_finished_before_a_time() {
        let (_dir, store) = store().await;
        let finished = store.enqueue_job(new_job("backup.run")).await.unwrap();
        let queued = store.enqueue_job(new_job("backup.run")).await.unwrap();
        store.cancel_job(finished.id).await.unwrap();

        let none_yet = store.delete_finished_jobs("2000-01-01T00:00:00Z".parse().unwrap()).await.unwrap();
        let deleted = store.delete_finished_jobs(Timestamp::now() + jiff::SignedDuration::from_secs(1)).await.unwrap();

        assert_eq!((none_yet, deleted), (0, 1));
        assert!(matches!(store.get_job(finished.id).await, Err(StoreError::NotFound { .. })));
        store.get_job(queued.id).await.unwrap();
    }
}
