use anyhow::{Context as _, Result};
use db::{
    sqlez::{
        connection::Connection, domain::Domain, thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};
use futures::channel::oneshot;
use std::future::Future;

/// Review state kept in Zed's database, keyed by review (`ReviewKey`, DESIGN §2.6) rather than by
/// workspace, so a review comes back in any window that opens it.
///
/// Every query goes through [`ThreadSafeConnection::write`]'s queue, in the order the methods
/// below are called (not when their futures are polled), so writes land in the order they were
/// made and reads see every earlier write.
pub(crate) struct CodeReviewDb(ThreadSafeConnection);

impl Domain for CodeReviewDb {
    const NAME: &str = stringify!(CodeReviewDb);

    // Never edit a migration that has shipped; append new ones. Milestone 5 appends the draft
    // comments table (`code_review_comments`, keyed by `review_key` like the tables below).
    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE code_review_sessions(
            review_key TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            last_opened_at INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;
        CREATE TABLE code_review_reviewed_files(
            review_key TEXT NOT NULL REFERENCES code_review_sessions(review_key)
                ON DELETE CASCADE ON UPDATE CASCADE,
            path TEXT NOT NULL,
            content_hash INTEGER,
            PRIMARY KEY(review_key, path)
        ) STRICT;
        CREATE TABLE code_review_reviewed_hunks(
            review_key TEXT NOT NULL REFERENCES code_review_sessions(review_key)
                ON DELETE CASCADE ON UPDATE CASCADE,
            path TEXT NOT NULL,
            hunk_key TEXT NOT NULL,
            PRIMARY KEY(review_key, path, hunk_key)
        ) STRICT;
    )];
}

db::static_connection!(CodeReviewDb, []);

/// The reviewed marks of one review as stored: repository-relative unix paths, the file's diff
/// content hash when it was marked (`None` = adopt the current one), and hunk keys.
#[derive(Debug, Default)]
pub(crate) struct StoredMarks {
    pub files: Vec<(String, Option<u64>)>,
    pub hunks: Vec<(String, String)>,
}

impl CodeReviewDb {
    fn queue<T, Job>(&self, job: Job) -> impl Future<Output = Result<T>> + use<T, Job>
    where
        T: Send + 'static,
        Job: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        // `write` queues the job right away; its own future borrows `self`, so the result comes
        // back through the channel instead.
        drop(self.write(move |connection| {
            sender.send(job(connection)).ok();
        }));
        async move { receiver.await.context("the database queue was closed")? }
    }

    /// Records that the review was opened and reads its marks.
    pub(crate) fn open_review(&self, review_key: String) -> impl Future<Output = Result<StoredMarks>> + use<> {
        self.queue(move |connection| {
            connection.exec_bound::<&str>(sql!(
                INSERT INTO code_review_sessions(review_key) VALUES (?)
                ON CONFLICT(review_key) DO UPDATE SET last_opened_at = unixepoch()
            ))?(review_key.as_str())?;
            let files = connection.select_bound::<&str, (String, Option<u64>)>(sql!(
                SELECT path, content_hash FROM code_review_reviewed_files WHERE review_key = ?
            ))?(review_key.as_str())?;
            let hunks = connection.select_bound::<&str, (String, String)>(sql!(
                SELECT path, hunk_key FROM code_review_reviewed_hunks WHERE review_key = ?
            ))?(review_key.as_str())?;
            Ok(StoredMarks { files, hunks })
        })
    }

    pub(crate) fn set_file_reviewed(
        &self,
        review_key: String,
        path: String,
        content_hash: Option<u64>,
    ) -> impl Future<Output = Result<()>> + use<> {
        self.queue(move |connection| {
            ensure_session(connection, review_key.as_str())?;
            connection.exec_bound::<(&str, &str, Option<u64>)>(sql!(
                INSERT INTO code_review_reviewed_files(review_key, path, content_hash)
                VALUES (?, ?, ?)
                ON CONFLICT(review_key, path) DO UPDATE SET content_hash = excluded.content_hash
            ))?((review_key.as_str(), path.as_str(), content_hash))
        })
    }

    pub(crate) fn unset_file_reviewed(
        &self,
        review_key: String,
        path: String,
    ) -> impl Future<Output = Result<()>> + use<> {
        self.queue(move |connection| {
            connection.exec_bound::<(&str, &str)>(sql!(
                DELETE FROM code_review_reviewed_files WHERE review_key = ? AND path = ?
            ))?((review_key.as_str(), path.as_str()))
        })
    }

    pub(crate) fn set_hunk_reviewed(
        &self,
        review_key: String,
        path: String,
        hunk_key: String,
    ) -> impl Future<Output = Result<()>> + use<> {
        self.queue(move |connection| {
            ensure_session(connection, review_key.as_str())?;
            connection.exec_bound::<(&str, &str, &str)>(sql!(
                INSERT OR IGNORE INTO code_review_reviewed_hunks(review_key, path, hunk_key)
                VALUES (?, ?, ?)
            ))?((review_key.as_str(), path.as_str(), hunk_key.as_str()))
        })
    }

    pub(crate) fn unset_hunk_reviewed(
        &self,
        review_key: String,
        path: String,
        hunk_key: String,
    ) -> impl Future<Output = Result<()>> + use<> {
        self.queue(move |connection| {
            connection.exec_bound::<(&str, &str, &str)>(sql!(
                DELETE FROM code_review_reviewed_hunks
                WHERE review_key = ? AND path = ? AND hunk_key = ?
            ))?((review_key.as_str(), path.as_str(), hunk_key.as_str()))
        })
    }
}

/// Marks reference their review's session row, which pruning (not implemented yet) may delete.
fn ensure_session(connection: &Connection, review_key: &str) -> Result<()> {
    connection.exec_bound::<&str>(sql!(
        INSERT OR IGNORE INTO code_review_sessions(review_key) VALUES (?)
    ))?(review_key)
}
