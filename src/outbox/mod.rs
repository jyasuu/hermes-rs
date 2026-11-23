//! Durable outbox (SQLite): jobs survive crashes and restarts.
//!
//! One row per (message, target). Lifecycle:
//!
//! ```text
//! pending ──claim──▶ in_flight ──ok──────────────▶ delivered ──prune──▶ (gone)
//!    ▲                  │  │
//!    └─ retry/defer ────┘  └─ permanent error / retries exhausted ─▶ dead ──replay──▶ pending
//!    └─ lease expired / restart (crash recovery)
//! ```
//!
//! Delivery is **at-least-once**: a crash between "target accepted it" and
//! "row marked delivered" redelivers, carrying the same idempotency key.

pub mod worker;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub type DbResult<T> = Result<T, String>;
fn db<T>(r: rusqlite::Result<T>) -> DbResult<T> {
    r.map_err(|e| format!("outbox database error: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Pending,
    InFlight,
    Delivered,
    Dead,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Pending => "pending",
            JobState::InFlight => "in_flight",
            JobState::Delivered => "delivered",
            JobState::Dead => "dead",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => JobState::Pending,
            "in_flight" => JobState::InFlight,
            "delivered" => JobState::Delivered,
            "dead" => JobState::Dead,
            _ => return None,
        })
    }
}

/// A delivery to enqueue: payload already rendered for this target.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub target_index: usize,
    pub target_label: String,
    pub payload: String,
}

/// A job claimed by a worker.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub message_id: String,
    pub endpoint: String,
    pub target_index: usize,
    pub target_label: String,
    pub payload: String,
    /// Attempts already made (not counting the one being run now).
    pub attempts: u32,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enqueued {
    /// This many jobs were stored.
    Accepted(usize),
    /// A message with this id was already accepted for this endpoint.
    Duplicate,
    /// Too many jobs waiting: shed load.
    Full,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub pending: u64,
    pub in_flight: u64,
    pub delivered: u64,
    pub dead: u64,
    /// Age of the oldest job still waiting to be delivered.
    pub oldest_pending_age_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: i64,
    pub message_id: String,
    pub endpoint: String,
    pub target_label: String,
    pub state: String,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub enum Selector {
    All,
    Id(i64),
    Endpoint(String),
}

// ---------------------------------------------------------------------------
// Blocking store
// ---------------------------------------------------------------------------

/// Plain synchronous SQL layer. Every time-dependent method takes `now` (ms)
/// explicitly so behaviour is deterministic in tests.
pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> DbResult<Self> {
        let existed = path.exists();
        let conn = db(Connection::open(path))?;
        // WAL lets `hermes-admin` read/replay while the server runs.
        // synchronous=FULL: a 202 is a promise, so fsync before replying.
        db(conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS outbox (
                 id              INTEGER PRIMARY KEY AUTOINCREMENT,
                 message_id      TEXT    NOT NULL,
                 endpoint        TEXT    NOT NULL,
                 target_index    INTEGER NOT NULL,
                 target_label    TEXT    NOT NULL,
                 payload         TEXT    NOT NULL,
                 state           TEXT    NOT NULL,
                 attempts        INTEGER NOT NULL DEFAULT 0,
                 next_attempt_at INTEGER NOT NULL,
                 lease_until     INTEGER,
                 last_error      TEXT,
                 created_at      INTEGER NOT NULL,
                 updated_at      INTEGER NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS outbox_dedupe ON outbox(endpoint, message_id, target_index);
             CREATE INDEX IF NOT EXISTS outbox_due ON outbox(state, next_attempt_at);",
        ))?;

        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            // Payloads are stored in clear text: owner-only.
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let _ = existed;
        Ok(Self { conn })
    }

    pub fn enqueue(
        &mut self,
        message_id: &str,
        endpoint: &str,
        jobs: &[NewJob],
        max_pending: u64,
        now: i64,
    ) -> DbResult<Enqueued> {
        let tx = db(self.conn.transaction_with_behavior(TransactionBehavior::Immediate))?;

        let seen: Option<i64> = db(tx
            .query_row(
                "SELECT 1 FROM outbox WHERE endpoint=?1 AND message_id=?2 LIMIT 1",
                params![endpoint, message_id],
                |r| r.get(0),
            )
            .optional())?;
        if seen.is_some() {
            return Ok(Enqueued::Duplicate);
        }

        let waiting: i64 = db(tx.query_row(
            "SELECT COUNT(*) FROM outbox WHERE state IN ('pending','in_flight')",
            [],
            |r| r.get(0),
        ))?;
        if waiting as u64 + jobs.len() as u64 > max_pending {
            return Ok(Enqueued::Full);
        }

        for j in jobs {
            db(tx.execute(
                "INSERT INTO outbox (message_id, endpoint, target_index, target_label, payload,
                                     state, attempts, next_attempt_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
                params![message_id, endpoint, j.target_index as i64, j.target_label, j.payload, now],
            ))?;
        }
        db(tx.commit())?;
        Ok(Enqueued::Accepted(jobs.len()))
    }

    /// Takes the next due job and leases it to the caller.
    pub fn claim(&mut self, now: i64, lease_ms: i64) -> DbResult<Option<Job>> {
        let tx = db(self.conn.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let job = db(tx
            .query_row(
                "SELECT id, message_id, endpoint, target_index, target_label, payload, attempts, created_at
                 FROM outbox WHERE state='pending' AND next_attempt_at <= ?1
                 ORDER BY next_attempt_at, id LIMIT 1",
                params![now],
                |r| {
                    Ok(Job {
                        id: r.get(0)?,
                        message_id: r.get(1)?,
                        endpoint: r.get(2)?,
                        target_index: r.get::<_, i64>(3)? as usize,
                        target_label: r.get(4)?,
                        payload: r.get(5)?,
                        attempts: r.get::<_, i64>(6)? as u32,
                        created_at: r.get(7)?,
                    })
                },
            )
            .optional())?;
        if let Some(j) = &job {
            db(tx.execute(
                "UPDATE outbox SET state='in_flight', lease_until=?2, updated_at=?3 WHERE id=?1",
                params![j.id, now + lease_ms, now],
            ))?;
        }
        db(tx.commit())?;
        Ok(job)
    }

    pub fn complete(&self, id: i64, attempts: u32, now: i64) -> DbResult<()> {
        db(self.conn.execute(
            "UPDATE outbox SET state='delivered', attempts=?2, last_error=NULL, lease_until=NULL, updated_at=?3
             WHERE id=?1",
            params![id, attempts, now],
        ))
        .map(|_| ())
    }

    /// Back to `pending`, due at `next_at`.
    pub fn reschedule(&self, id: i64, attempts: u32, next_at: i64, error: &str, now: i64) -> DbResult<()> {
        db(self.conn.execute(
            "UPDATE outbox SET state='pending', attempts=?2, next_attempt_at=?3, last_error=?4,
                               lease_until=NULL, updated_at=?5 WHERE id=?1",
            params![id, attempts, next_at, error, now],
        ))
        .map(|_| ())
    }

    pub fn dead(&self, id: i64, attempts: u32, error: &str, now: i64) -> DbResult<()> {
        db(self.conn.execute(
            "UPDATE outbox SET state='dead', attempts=?2, last_error=?3, lease_until=NULL, updated_at=?4
             WHERE id=?1",
            params![id, attempts, error, now],
        ))
        .map(|_| ())
    }

    /// Crash recovery: in-flight jobs whose lease ran out go back to pending.
    pub fn reclaim_expired(&self, now: i64) -> DbResult<usize> {
        db(self.conn.execute(
            "UPDATE outbox SET state='pending', next_attempt_at=?1, lease_until=NULL, updated_at=?1
             WHERE state='in_flight' AND lease_until <= ?1",
            params![now],
        ))
    }

    /// Startup recovery: nothing of ours can still be running, so every
    /// in-flight job is an orphan regardless of its lease.
    pub fn reclaim_all_in_flight(&self, now: i64) -> DbResult<usize> {
        db(self.conn.execute(
            "UPDATE outbox SET state='pending', next_attempt_at=?1, lease_until=NULL, updated_at=?1
             WHERE state='in_flight'",
            params![now],
        ))
    }

    pub fn prune(&self, now: i64, delivered_keep_ms: i64, dead_keep_ms: i64) -> DbResult<usize> {
        db(self.conn.execute(
            "DELETE FROM outbox WHERE (state='delivered' AND updated_at < ?1)
                                   OR (state='dead'      AND updated_at < ?2)",
            params![now - delivered_keep_ms, now - dead_keep_ms],
        ))
    }

    pub fn stats(&self, now: i64) -> DbResult<Stats> {
        let mut s = Stats::default();
        let mut stmt = db(self.conn.prepare("SELECT state, COUNT(*) FROM outbox GROUP BY state"))?;
        let rows = db(stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))))?;
        for row in rows {
            let (state, n) = db(row)?;
            match JobState::parse(&state) {
                Some(JobState::Pending) => s.pending = n,
                Some(JobState::InFlight) => s.in_flight = n,
                Some(JobState::Delivered) => s.delivered = n,
                Some(JobState::Dead) => s.dead = n,
                None => {}
            }
        }
        let oldest: Option<i64> = db(self.conn.query_row(
            "SELECT MIN(created_at) FROM outbox WHERE state IN ('pending','in_flight')",
            [],
            |r| r.get(0),
        ))?;
        s.oldest_pending_age_ms = oldest.map(|t| (now - t).max(0));
        Ok(s)
    }

    /// Which targets have jobs waiting: `(endpoint, target_index, label, count)`.
    pub fn pending_targets(&self) -> DbResult<Vec<(String, usize, String, u64)>> {
        let mut stmt = db(self.conn.prepare(
            "SELECT endpoint, target_index, target_label, COUNT(*) FROM outbox
             WHERE state IN ('pending','in_flight') GROUP BY endpoint, target_index, target_label",
        ))?;
        let rows = db(stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize, r.get::<_, String>(2)?, r.get::<_, i64>(3)? as u64))
        }))?;
        rows.map(|r| db(r)).collect()
    }

    pub fn list(&self, state: Option<JobState>, limit: usize) -> DbResult<Vec<JobRow>> {
        let mut stmt = db(self.conn.prepare(
            "SELECT id, message_id, endpoint, target_label, state, attempts, last_error, created_at, updated_at
             FROM outbox WHERE (?1 IS NULL OR state = ?1) ORDER BY id DESC LIMIT ?2",
        ))?;
        let rows = db(stmt.query_map(params![state.map(JobState::as_str), limit as i64], |r| {
            Ok(JobRow {
                id: r.get(0)?,
                message_id: r.get(1)?,
                endpoint: r.get(2)?,
                target_label: r.get(3)?,
                state: r.get(4)?,
                attempts: r.get::<_, i64>(5)? as u32,
                last_error: r.get(6)?,
                created_at: r.get(7)?,
                updated_at: r.get(8)?,
            })
        }))?;
        rows.map(|r| db(r)).collect()
    }

    /// Dead letters back to `pending` with a fresh attempt budget.
    pub fn replay(&self, sel: &Selector, now: i64) -> DbResult<usize> {
        let (clause, id, ep) = selector_sql(sel);
        db(self.conn.execute(
            &format!(
                "UPDATE outbox SET state='pending', attempts=0, next_attempt_at=?1, updated_at=?1
                 WHERE state='dead' AND {clause}"
            ),
            params![now, id, ep],
        ))
    }

    pub fn purge_dead(&self, sel: &Selector) -> DbResult<usize> {
        let (clause, id, ep) = selector_sql(sel);
        // ?1 is unused here (it carries `now` in `replay`); bind NULL to keep
        // the shared clause's numbering.
        db(self.conn.execute(
            &format!("DELETE FROM outbox WHERE state='dead' AND {clause}"),
            params![Option::<i64>::None, id, ep],
        ))
    }
}

/// `(clause, id, endpoint)`. The clause always mentions both `?2` and `?3`
/// (a NULL means "don't filter on this"), which keeps the parameter count
/// identical for every selector.
fn selector_sql(sel: &Selector) -> (&'static str, Option<i64>, Option<String>) {
    const CLAUSE: &str = "(?2 IS NULL OR id = ?2) AND (?3 IS NULL OR endpoint = ?3)";
    match sel {
        Selector::All => (CLAUSE, None, None),
        Selector::Id(i) => (CLAUSE, Some(*i), None),
        Selector::Endpoint(e) => (CLAUSE, None, Some(e.clone())),
    }
}

// ---------------------------------------------------------------------------
// Async handle
// ---------------------------------------------------------------------------

/// Cloneable async front for the [`Store`]. SQLite calls run on the blocking
/// thread pool so they never stall the async runtime.
#[derive(Clone)]
pub struct Outbox {
    store: Arc<Mutex<Store>>,
    /// Wakes idle workers when something is enqueued.
    pub wake: Arc<Notify>,
}

impl Outbox {
    pub fn open(path: &Path) -> DbResult<Self> {
        Ok(Self { store: Arc::new(Mutex::new(Store::open(path)?)), wake: Arc::new(Notify::new()) })
    }

    async fn run<T, F>(&self, f: F) -> DbResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Store) -> DbResult<T> + Send + 'static,
    {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut guard)
        })
        .await
        .map_err(|e| format!("outbox task failed: {e}"))?
    }

    pub async fn enqueue(
        &self,
        message_id: String,
        endpoint: String,
        jobs: Vec<NewJob>,
        max_pending: u64,
    ) -> DbResult<Enqueued> {
        let r = self.run(move |s| s.enqueue(&message_id, &endpoint, &jobs, max_pending, now_ms())).await?;
        if matches!(r, Enqueued::Accepted(_)) {
            self.wake.notify_waiters();
        }
        Ok(r)
    }

    pub async fn claim(&self, lease_ms: i64) -> DbResult<Option<Job>> {
        self.run(move |s| s.claim(now_ms(), lease_ms)).await
    }
    pub async fn complete(&self, id: i64, attempts: u32) -> DbResult<()> {
        self.run(move |s| s.complete(id, attempts, now_ms())).await
    }
    pub async fn reschedule(&self, id: i64, attempts: u32, next_at: i64, error: String) -> DbResult<()> {
        self.run(move |s| s.reschedule(id, attempts, next_at, &error, now_ms())).await
    }
    pub async fn dead(&self, id: i64, attempts: u32, error: String) -> DbResult<()> {
        self.run(move |s| s.dead(id, attempts, &error, now_ms())).await
    }
    pub async fn reclaim_expired(&self) -> DbResult<usize> {
        self.run(|s| s.reclaim_expired(now_ms())).await
    }
    pub async fn reclaim_all_in_flight(&self) -> DbResult<usize> {
        self.run(|s| s.reclaim_all_in_flight(now_ms())).await
    }
    pub async fn prune(&self, delivered_keep_ms: i64, dead_keep_ms: i64) -> DbResult<usize> {
        self.run(move |s| s.prune(now_ms(), delivered_keep_ms, dead_keep_ms)).await
    }
    pub async fn pending_targets(&self) -> DbResult<Vec<(String, usize, String, u64)>> {
        self.run(|s| s.pending_targets()).await
    }
    pub async fn stats(&self) -> DbResult<Stats> {
        self.run(|s| s.stats(now_ms())).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_000_000;

    fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Store::open(&dir.path().join("o.db")).unwrap(), dir)
    }

    fn jobs(n: usize) -> Vec<NewJob> {
        (0..n)
            .map(|i| NewJob { target_index: i, target_label: format!("t{i}"), payload: format!("{{\"i\":{i}}}") })
            .collect()
    }

    #[test]
    fn enqueue_then_claim_in_order_and_lease() {
        let (mut s, _d) = store();
        assert_eq!(s.enqueue("m1", "/e", &jobs(2), 100, T0).unwrap(), Enqueued::Accepted(2));
        let a = s.claim(T0, 5_000).unwrap().unwrap();
        let b = s.claim(T0, 5_000).unwrap().unwrap();
        assert_eq!((a.target_index, b.target_index), (0, 1));
        assert_eq!((a.message_id.as_str(), a.attempts, a.payload.as_str()), ("m1", 0, "{\"i\":0}"));
        assert!(s.claim(T0, 5_000).unwrap().is_none(), "both leased");
        let st = s.stats(T0 + 10).unwrap();
        assert_eq!((st.pending, st.in_flight), (0, 2));
        assert_eq!(st.oldest_pending_age_ms, Some(10));
    }

    #[test]
    fn same_message_id_is_a_duplicate_per_endpoint() {
        let (mut s, _d) = store();
        assert_eq!(s.enqueue("m", "/a", &jobs(1), 100, T0).unwrap(), Enqueued::Accepted(1));
        assert_eq!(s.enqueue("m", "/a", &jobs(1), 100, T0).unwrap(), Enqueued::Duplicate);
        assert_eq!(s.enqueue("m", "/b", &jobs(1), 100, T0).unwrap(), Enqueued::Accepted(1), "other endpoint");
        assert_eq!(s.stats(T0).unwrap().pending, 2, "duplicate stored nothing");
    }

    #[test]
    fn duplicate_wins_over_full_and_full_is_all_or_nothing() {
        let (mut s, _d) = store();
        s.enqueue("m1", "/e", &jobs(2), 3, T0).unwrap();
        assert_eq!(s.enqueue("m1", "/e", &jobs(2), 3, T0).unwrap(), Enqueued::Duplicate);
        assert_eq!(s.enqueue("m2", "/e", &jobs(2), 3, T0).unwrap(), Enqueued::Full);
        assert_eq!(s.stats(T0).unwrap().pending, 2, "nothing of m2 stored");
        assert_eq!(s.enqueue("m3", "/e", &jobs(1), 3, T0).unwrap(), Enqueued::Accepted(1));
    }

    #[test]
    fn delivered_jobs_do_not_count_towards_the_cap() {
        let (mut s, _d) = store();
        s.enqueue("m1", "/e", &jobs(1), 1, T0).unwrap();
        let j = s.claim(T0, 1000).unwrap().unwrap();
        s.complete(j.id, 1, T0).unwrap();
        assert_eq!(s.enqueue("m2", "/e", &jobs(1), 1, T0).unwrap(), Enqueued::Accepted(1));
    }

    #[test]
    fn reschedule_respects_next_attempt_time() {
        let (mut s, _d) = store();
        s.enqueue("m", "/e", &jobs(1), 10, T0).unwrap();
        let j = s.claim(T0, 1000).unwrap().unwrap();
        s.reschedule(j.id, 1, T0 + 5_000, "boom", T0).unwrap();
        assert!(s.claim(T0 + 4_999, 1000).unwrap().is_none(), "not due yet");
        let again = s.claim(T0 + 5_000, 1000).unwrap().unwrap();
        assert_eq!(again.attempts, 1);
    }

    #[test]
    fn expired_lease_is_reclaimed_but_live_lease_is_not() {
        let (mut s, _d) = store();
        s.enqueue("m", "/e", &jobs(1), 10, T0).unwrap();
        s.claim(T0, 10_000).unwrap().unwrap(); // worker "crashes" holding it
        assert_eq!(s.reclaim_expired(T0 + 9_999).unwrap(), 0);
        assert_eq!(s.reclaim_expired(T0 + 10_000).unwrap(), 1);
        assert!(s.claim(T0 + 10_000, 1000).unwrap().is_some(), "redelivered");
    }

    #[test]
    fn startup_recovery_reclaims_every_orphan_immediately() {
        let (mut s, _d) = store();
        s.enqueue("m", "/e", &jobs(2), 10, T0).unwrap();
        s.claim(T0, 3_600_000).unwrap();
        s.claim(T0, 3_600_000).unwrap();
        assert_eq!(s.reclaim_all_in_flight(T0 + 1).unwrap(), 2);
        assert_eq!(s.stats(T0 + 1).unwrap().pending, 2);
    }

    #[test]
    fn dead_letters_can_be_listed_replayed_and_purged_selectively() {
        let (mut s, _d) = store();
        s.enqueue("a", "/x", &jobs(1), 10, T0).unwrap();
        s.enqueue("b", "/y", &jobs(1), 10, T0).unwrap();
        s.enqueue("c", "/y", &jobs(1), 10, T0).unwrap();
        for _ in 0..3 {
            let j = s.claim(T0, 1000).unwrap().unwrap();
            s.dead(j.id, 4, "HTTP 500", T0).unwrap();
        }
        let dead = s.list(Some(JobState::Dead), 10).unwrap();
        assert_eq!(dead.len(), 3);
        assert_eq!(dead[0].last_error.as_deref(), Some("HTTP 500"));

        assert_eq!(s.replay(&Selector::Endpoint("/x".into()), T0 + 1).unwrap(), 1);
        assert_eq!(s.replay(&Selector::Id(dead[0].id), T0 + 1).unwrap(), 1);
        let st = s.stats(T0 + 1).unwrap();
        assert_eq!((st.pending, st.dead), (2, 1));
        let replayed = s.claim(T0 + 1, 1000).unwrap().unwrap();
        assert_eq!(replayed.attempts, 0, "replay gives a fresh attempt budget");

        assert_eq!(s.purge_dead(&Selector::All).unwrap(), 1);
        assert_eq!(s.stats(T0 + 1).unwrap().dead, 0);
    }

    #[test]
    fn replay_all_only_touches_dead() {
        let (mut s, _d) = store();
        s.enqueue("a", "/x", &jobs(1), 10, T0).unwrap();
        s.enqueue("b", "/x", &jobs(1), 10, T0).unwrap();
        let j = s.claim(T0, 1000).unwrap().unwrap();
        s.dead(j.id, 2, "e", T0).unwrap();
        assert_eq!(s.replay(&Selector::All, T0).unwrap(), 1);
    }

    #[test]
    fn prune_uses_separate_retention_for_delivered_and_dead() {
        let (mut s, _d) = store();
        s.enqueue("a", "/e", &jobs(2), 10, T0).unwrap();
        let ok = s.claim(T0, 1000).unwrap().unwrap();
        let bad = s.claim(T0, 1000).unwrap().unwrap();
        s.complete(ok.id, 1, T0).unwrap();
        s.dead(bad.id, 1, "e", T0).unwrap();
        // delivered kept 1h, dead kept 7d; look 2h later
        assert_eq!(s.prune(T0 + 7_200_000, 3_600_000, 604_800_000).unwrap(), 1);
        let st = s.stats(T0 + 7_200_000).unwrap();
        assert_eq!((st.delivered, st.dead), (0, 1));
        // after pruning, the message id can be accepted again
        let mut s2 = s;
        assert_eq!(s2.enqueue("a", "/e", &jobs(1), 10, T0).unwrap(), Enqueued::Duplicate, "dead row still blocks");
    }

    #[test]
    fn pending_targets_groups_waiting_jobs_only() {
        let (mut s, _d) = store();
        s.enqueue("m1", "/e", &jobs(2), 100, T0).unwrap();
        s.enqueue("m2", "/e", &jobs(1), 100, T0).unwrap();
        let j = s.claim(T0, 1000).unwrap().unwrap(); // m1/t0 in flight: still counts
        let done = s.claim(T0, 1000).unwrap().unwrap(); // m1/t1
        s.complete(done.id, 1, T0).unwrap(); // delivered: must not count
        let _ = j;
        let mut rows = s.pending_targets().unwrap();
        rows.sort();
        assert_eq!(rows, vec![("/e".to_string(), 0, "t0".to_string(), 2)]);
    }

    #[test]
    fn data_survives_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        {
            let mut s = Store::open(&path).unwrap();
            s.enqueue("m", "/e", &jobs(1), 10, T0).unwrap();
        }
        let mut s = Store::open(&path).unwrap();
        assert_eq!(s.claim(T0, 1000).unwrap().unwrap().message_id, "m");
    }

    #[cfg(unix)]
    #[test]
    fn database_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        Store::open(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
