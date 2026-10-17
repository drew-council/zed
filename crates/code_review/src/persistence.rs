use anyhow::{Context as _, Result};
use db::{
    sqlez::{connection::Connection, domain::Domain, thread_safe_connection::ThreadSafeConnection},
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

    // Never edit a migration that has shipped; append new ones. Zed's other databases share this
    // migrator, so a changed migration makes all of them fall back to memory.
    const MIGRATIONS: &[&str] = &[
        sql!(
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
        ),
        // `data` is a comment as JSON (see `comments::StoredComment`), so fields can be added
        // without a migration.
        sql!(
        CREATE TABLE code_review_comments(
            review_key TEXT NOT NULL REFERENCES code_review_sessions(review_key)
                ON DELETE CASCADE ON UPDATE CASCADE,
            comment_id INTEGER NOT NULL,
            data TEXT NOT NULL,
            PRIMARY KEY(review_key, comment_id)
        ) STRICT;
        ),
    ];
}

db::static_connection!(CodeReviewDb, []);

/// The state of one review as stored. Reviewed marks: repository-relative unix paths, the
/// file's diff content hash when it was marked (`None` = adopt the current one), and hunk keys.
/// Comments: their ids and JSON.
#[derive(Debug, Default)]
pub(crate) struct StoredReview {
    pub files: Vec<(String, Option<u64>)>,
    pub hunks: Vec<(String, String)>,
    pub comments: Vec<(u64, String)>,
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

    /// Records that the review was opened and reads its marks and comments.
    pub(crate) fn open_review(
        &self,
        review_key: String,
    ) -> impl Future<Output = Result<StoredReview>> + use<> {
        self.queue(move |connection| {
            ensure_session(connection, review_key.as_str())?;
            let files = connection.select_bound::<&str, (String, Option<u64>)>(sql!(
                SELECT path, content_hash FROM code_review_reviewed_files WHERE review_key = ?
            ))?(review_key.as_str())?;
            let hunks = connection.select_bound::<&str, (String, String)>(sql!(
                SELECT path, hunk_key FROM code_review_reviewed_hunks WHERE review_key = ?
            ))?(review_key.as_str())?;
            let comments = connection.select_bound::<&str, (u64, String)>(sql!(
                SELECT comment_id, data FROM code_review_comments WHERE review_key = ?
                ORDER BY comment_id
            ))?(review_key.as_str())?;
            Ok(StoredReview {
                files,
                hunks,
                comments,
            })
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

impl CodeReviewDb {
    pub(crate) fn save_comment(
        &self,
        review_key: String,
        comment_id: u64,
        data: String,
    ) -> impl Future<Output = Result<()>> + use<> {
        self.queue(move |connection| {
            ensure_session(connection, review_key.as_str())?;
            connection.exec_bound::<(&str, u64, &str)>(sql!(
                INSERT INTO code_review_comments(review_key, comment_id, data) VALUES (?, ?, ?)
                ON CONFLICT(review_key, comment_id) DO UPDATE SET data = excluded.data
            ))?((review_key.as_str(), comment_id, data.as_str()))
        })
    }

    pub(crate) fn delete_comment(
        &self,
        review_key: String,
        comment_id: u64,
    ) -> impl Future<Output = Result<()>> + use<> {
        self.queue(move |connection| {
            connection.exec_bound::<(&str, u64)>(sql!(
                DELETE FROM code_review_comments WHERE review_key = ? AND comment_id = ?
            ))?((review_key.as_str(), comment_id))
        })
    }

    pub(crate) fn sync(&self) -> impl Future<Output = Result<()>> + use<> {
        self.queue(|_| Ok(()))
    }

    pub(crate) fn touch_review(
        &self,
        review_key: String,
    ) -> impl Future<Output = Result<()>> + use<> {
        self.queue(move |connection| ensure_session(connection, review_key.as_str()))
    }

    /// Forgets the reviews (marks and comments) not opened for `max_age_seconds`.
    pub(crate) fn prune(&self, max_age_seconds: i64) -> impl Future<Output = Result<()>> + use<> {
        self.queue(move |connection| {
            connection.exec_bound::<i64>(sql!(
                DELETE FROM code_review_sessions WHERE last_opened_at < unixepoch() - ?
            ))?(max_age_seconds)
        })
    }

    #[cfg(test)]
    pub(crate) fn set_last_opened_at(
        &self,
        review_key: String,
        last_opened_at: i64,
    ) -> impl Future<Output = Result<()>> + use<> {
        self.queue(move |connection| {
            connection.exec_bound::<(i64, &str)>(sql!(
                UPDATE code_review_sessions SET last_opened_at = ? WHERE review_key = ?
            ))?((last_opened_at, review_key.as_str()))
        })
    }
}

/// Marks and comments reference their review's session row, which pruning may have deleted
/// while the review was open. Every write counts as opening the review, so that a review kept
/// open (in a tab, or in a Zed that runs for months) isn't pruned as unopened.
fn ensure_session(connection: &Connection, review_key: &str) -> Result<()> {
    connection.exec_bound::<&str>(sql!(
        INSERT INTO code_review_sessions(review_key) VALUES (?)
        ON CONFLICT(review_key) DO UPDATE SET last_opened_at = unixepoch()
    ))?(review_key)
}
