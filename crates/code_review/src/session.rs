use crate::persistence::{CodeReviewDb, StoredMarks};
use buffer_diff::BufferDiffSnapshot;
use collections::{HashMap, HashSet};
use git::repository::RepoPath;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task, WeakEntity,
};
use language::{Anchor, BufferSnapshot};
use std::{future::Future, ops::Range};
use util::ResultExt as _;

/// Identifies a review across tabs and restarts (DESIGN §2.6): the repository plus what is
/// reviewed, never a head SHA, so new commits or pushes keep the review's state.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReviewKey(pub SharedString);

/// The open sessions by key, so every tab of a review shares one.
#[derive(Default)]
struct ReviewSessions(HashMap<ReviewKey, WeakEntity<ReviewSession>>);

impl Global for ReviewSessions {}

/// The reviewed marks of one review, loaded from and saved to [`CodeReviewDb`].
///
/// Marks are only deleted when the user unmarks them. A file is reviewed while its diff is the
/// one it was marked with, and a hunk while its content is: a diff that changes can change back
/// (a stash, a branch switch, an undo), and the review key may be switched before the diffs
/// catch up.
pub(crate) struct ReviewSession {
    key: ReviewKey,
    db: CodeReviewDb,
    loaded: bool,
    /// Reviewed files, with their diff's content hash when they were marked (`None`, from older
    /// versions: adopt the hash the file has the next time it is seen).
    reviewed_files: HashMap<RepoPath, Option<u64>>,
    reviewed_hunks: HashMap<RepoPath, HashSet<SharedString>>,
    /// The content hash of each file's diff as last seen.
    content_hashes: HashMap<RepoPath, u64>,
    _load_task: Task<()>,
}

#[derive(Clone, Debug)]
pub(crate) enum ReviewSessionEvent {
    Loaded,
    /// Files whose reviewed state changed, with the new state.
    FilesChanged(Vec<(RepoPath, bool)>),
    HunksChanged,
    /// Loading or saving the marks failed.
    Failed(SharedString),
}

impl EventEmitter<ReviewSessionEvent> for ReviewSession {}

impl ReviewSession {
    /// The open session for `key`, or a new one that loads its marks.
    pub(crate) fn for_key(key: ReviewKey, cx: &mut App) -> Entity<Self> {
        let sessions = cx.default_global::<ReviewSessions>();
        sessions.0.retain(|_, session| session.upgrade().is_some());
        if let Some(session) = sessions.0.get(&key).and_then(WeakEntity::upgrade) {
            return session;
        }
        let session = cx.new(|cx| Self::new(key.clone(), cx));
        cx.default_global::<ReviewSessions>()
            .0
            .insert(key, session.downgrade());
        session
    }

    fn new(key: ReviewKey, cx: &mut Context<Self>) -> Self {
        let db = CodeReviewDb::global(cx);
        let load = db.open_review(key.0.to_string());
        let load_task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| match result {
                Ok(marks) => this.apply_stored_marks(marks, cx),
                // Marks stay unchangeable, so that nothing overwrites the stored ones.
                Err(error) => {
                    log::error!("loading reviewed marks: {error:#}");
                    cx.emit(ReviewSessionEvent::Failed(
                        format!("Couldn't load the reviewed marks: {error:#}").into(),
                    ));
                }
            })
            .log_err();
        });
        Self {
            key,
            db,
            loaded: false,
            reviewed_files: HashMap::default(),
            reviewed_hunks: HashMap::default(),
            content_hashes: HashMap::default(),
            _load_task: load_task,
        }
    }

    fn apply_stored_marks(&mut self, marks: StoredMarks, cx: &mut Context<Self>) {
        for (path, content_hash) in marks.files {
            match RepoPath::new(&path) {
                Ok(repo_path) => {
                    self.reviewed_files.insert(repo_path, content_hash);
                }
                Err(error) => log::warn!("skipping reviewed file {path:?}: {error:#}"),
            }
        }
        for (path, hunk_key) in marks.hunks {
            match RepoPath::new(&path) {
                Ok(repo_path) => {
                    self.reviewed_hunks
                        .entry(repo_path)
                        .or_default()
                        .insert(hunk_key.into());
                }
                Err(error) => log::warn!("skipping reviewed hunk in {path:?}: {error:#}"),
            }
        }
        self.loaded = true;
        cx.emit(ReviewSessionEvent::Loaded);
        cx.notify();
    }

    pub(crate) fn key(&self) -> &ReviewKey {
        &self.key
    }

    /// Marks can't change until the stored ones are loaded (which takes milliseconds), or the
    /// load would overwrite them.
    pub(crate) fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub(crate) fn is_file_reviewed(&self, path: &RepoPath) -> bool {
        match self.reviewed_files.get(path) {
            None => false,
            Some(None) => true,
            Some(Some(content_hash)) => self
                .content_hashes
                .get(path)
                .is_none_or(|current| current == content_hash),
        }
    }

    pub(crate) fn has_reviewed_hunks(&self) -> bool {
        self.reviewed_hunks.values().any(|hunks| !hunks.is_empty())
    }

    pub(crate) fn is_hunk_reviewed(&self, path: &RepoPath, hunk_key: &str) -> bool {
        self.reviewed_hunks
            .get(path)
            .is_some_and(|hunks| hunks.contains(hunk_key))
    }

    /// Marks the file reviewed with its diff's current hash, `content_hash`.
    pub(crate) fn mark_file(&mut self, path: &RepoPath, content_hash: u64, cx: &mut Context<Self>) {
        if !self.loaded || self.is_file_reviewed(path) {
            return;
        }
        self.content_hashes.insert(path.clone(), content_hash);
        self.reviewed_files.insert(path.clone(), Some(content_hash));
        persist(
            self.db.set_file_reviewed(
                self.key.0.to_string(),
                path.as_unix_str().to_string(),
                Some(content_hash),
            ),
            cx,
        );
        cx.emit(ReviewSessionEvent::FilesChanged(vec![(path.clone(), true)]));
        cx.notify();
    }

    pub(crate) fn unmark_file(&mut self, path: &RepoPath, cx: &mut Context<Self>) {
        if !self.loaded || !self.is_file_reviewed(path) {
            return;
        }
        self.reviewed_files.remove(path);
        persist(
            self.db
                .unset_file_reviewed(self.key.0.to_string(), path.as_unix_str().to_string()),
            cx,
        );
        cx.emit(ReviewSessionEvent::FilesChanged(vec![(
            path.clone(),
            false,
        )]));
        cx.notify();
    }

    /// Returns whether the hunk is reviewed afterwards.
    pub(crate) fn toggle_hunk(
        &mut self,
        path: &RepoPath,
        hunk_key: SharedString,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.loaded {
            return self.is_hunk_reviewed(path, &hunk_key);
        }
        let key = self.key.0.to_string();
        let unix_path = path.as_unix_str().to_string();
        let hunks = self.reviewed_hunks.entry(path.clone()).or_default();
        let reviewed = if hunks.remove(&hunk_key) {
            persist(
                self.db
                    .unset_hunk_reviewed(key, unix_path, hunk_key.to_string()),
                cx,
            );
            false
        } else {
            hunks.insert(hunk_key.clone());
            persist(
                self.db
                    .set_hunk_reviewed(key, unix_path, hunk_key.to_string()),
                cx,
            );
            true
        };
        cx.emit(ReviewSessionEvent::HunksChanged);
        cx.notify();
        reviewed
    }

    /// Records the file's current diff, which decides whether its mark counts (see
    /// [`ReviewSession`]).
    pub(crate) fn observe_digest(
        &mut self,
        path: &RepoPath,
        digest: &FileDigest,
        cx: &mut Context<Self>,
    ) {
        let was_reviewed = self.is_file_reviewed(path);
        self.content_hashes
            .insert(path.clone(), digest.content_hash);
        if !self.loaded {
            return;
        }
        if let Some(None) = self.reviewed_files.get(path) {
            self.reviewed_files
                .insert(path.clone(), Some(digest.content_hash));
            persist(
                self.db.set_file_reviewed(
                    self.key.0.to_string(),
                    path.as_unix_str().to_string(),
                    Some(digest.content_hash),
                ),
                cx,
            );
        }
        let reviewed = self.is_file_reviewed(path);
        if reviewed != was_reviewed {
            cx.emit(ReviewSessionEvent::FilesChanged(vec![(
                path.clone(),
                reviewed,
            )]));
            cx.notify();
        }
    }
}

/// The write is already queued; this only reports its failure.
fn persist(
    write: impl Future<Output = anyhow::Result<()>> + 'static,
    cx: &mut Context<ReviewSession>,
) {
    cx.spawn(async move |this, cx| {
        if let Err(error) = write.await {
            log::error!("saving reviewed marks: {error:#}");
            this.update(cx, |_, cx| {
                cx.emit(ReviewSessionEvent::Failed(
                    format!("Couldn't save the reviewed marks: {error:#}").into(),
                ))
            })
            .log_err();
        }
    })
    .detach();
}

/// A file's diff, summarized for reviewed marks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileDigest {
    /// Changes whenever any hunk changes.
    pub content_hash: u64,
    pub hunks: Vec<HunkDigest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HunkDigest {
    /// Survives edits elsewhere in the file: it hashes the hunk's content, not its position,
    /// unless another hunk of the file has the same content.
    pub key: SharedString,
    /// The hunk's range in the diff base, which identifies it on both sides of a split diff.
    pub base_range: Range<usize>,
}

impl FileDigest {
    pub(crate) fn hunk_key(&self, base_range: &Range<usize>) -> Option<&SharedString> {
        self.hunks
            .iter()
            .find(|hunk| &hunk.base_range == base_range)
            .map(|hunk| &hunk.key)
    }
}

/// FNV-1a, 64 bit (tuicr's hash): stable across runs and platforms, unlike `std`'s hashers.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

pub(crate) fn file_digest(diff: &BufferDiffSnapshot, buffer: &BufferSnapshot) -> FileDigest {
    let base_text = diff.base_text();
    let mut hunks = Vec::new();
    for hunk in
        diff.hunks_intersecting_range(Anchor::min_max_range_for_buffer(buffer.remote_id()), buffer)
    {
        let mut hash = Fnv::new();
        hash.write(b"-");
        for chunk in base_text.text_for_range(hunk.diff_base_byte_range.clone()) {
            hash.write(chunk.as_bytes());
        }
        hash.write(b"\0+");
        for chunk in buffer.text_for_range(hunk.range.clone()) {
            hash.write(chunk.as_bytes());
        }
        hash.write(b"\0");

        let old_start = base_text.offset_to_point(hunk.diff_base_byte_range.start);
        let old_end = base_text.offset_to_point(hunk.diff_base_byte_range.end);
        let old_count = line_count(old_start.row, old_end.row, old_end.column);
        let new_count = line_count(
            hunk.range.start.row,
            hunk.range.end.row,
            hunk.range.end.column,
        );
        hunks.push((
            hash.0,
            hunk.diff_base_byte_range.clone(),
            [
                old_start.row + 1,
                old_count,
                hunk.range.start.row + 1,
                new_count,
            ],
        ));
    }

    let mut content_hash = Fnv::new();
    for (hash, _, _) in &hunks {
        content_hash.write(&hash.to_le_bytes());
    }
    let hunks = hunks
        .iter()
        .map(|(hash, base_range, span)| {
            let is_repeated = hunks
                .iter()
                .filter(|(other_hash, _, _)| other_hash == hash)
                .nth(1)
                .is_some();
            let key = if is_repeated {
                let [old_start, old_count, new_start, new_count] = span;
                format!("hunk-span-v1:{hash:016x}:{old_start}:{old_count}:{new_start}:{new_count}")
            } else {
                format!("hunk-content-v1:{hash:016x}")
            };
            HunkDigest {
                key: key.into(),
                base_range: base_range.clone(),
            }
        })
        .collect();
    FileDigest {
        content_hash: content_hash.0,
        hunks,
    }
}

/// The lines a range of rows covers; a range ending in column 0 doesn't cover that row.
fn line_count(start_row: u32, end_row: u32, end_column: u32) -> u32 {
    end_row.saturating_sub(start_row) + u32::from(end_column > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer_diff::BufferDiff;
    use gpui::TestAppContext;

    fn digest(base: &str, text: &str, cx: &mut TestAppContext) -> FileDigest {
        cx.update(|cx| {
            let buffer = cx.new(|cx| language::Buffer::local(text, cx));
            let snapshot = buffer.read(cx).snapshot();
            let diff = cx.new(|cx| BufferDiff::new_with_base_text(base, &snapshot, cx));
            file_digest(&diff.read(cx).snapshot(cx), &snapshot)
        })
    }

    fn lines(range: std::ops::RangeInclusive<usize>) -> String {
        range.map(|index| format!("line {index}\n")).collect()
    }

    fn keys(digest: &FileDigest) -> Vec<&str> {
        digest.hunks.iter().map(|hunk| hunk.key.as_ref()).collect()
    }

    #[gpui::test]
    fn test_hunk_keys_follow_content(cx: &mut TestAppContext) {
        let base = lines(1..=20);
        let edited = base
            .replace("line 3\n", "LINE 3\n")
            .replace("line 15\n", "LINE 15\n");
        let original = digest(&base, &edited, cx);
        assert_eq!(original.hunks.len(), 2);
        assert!(
            keys(&original)
                .iter()
                .all(|key| key.starts_with("hunk-content-v1:"))
        );

        // Rows inserted above move the hunks but keep their keys; the file hash changes.
        let shifted = digest(&base, &format!("new\n{edited}"), cx);
        assert_eq!(shifted.hunks.len(), 3);
        for key in keys(&original) {
            assert!(keys(&shifted).contains(&key), "{key} is gone");
        }
        assert_ne!(shifted.content_hash, original.content_hash);
        assert_eq!(digest(&base, &edited, cx), original);

        // Changing one hunk changes its key only.
        let changed = digest(&base, &edited.replace("LINE 15", "Line 15"), cx);
        assert_eq!(keys(&changed)[0], keys(&original)[0]);
        assert_ne!(keys(&changed)[1], keys(&original)[1]);
        assert_ne!(changed.content_hash, original.content_hash);

        // Hunks with the same content get keys qualified by their position.
        let repeated = digest("a\nx\nb\nx\nc\n", "a\ny\nb\ny\nc\n", cx);
        assert_eq!(
            keys(&repeated),
            [
                format!("hunk-span-v1:{}:2:1:2:1", &keys(&repeated)[0][13..29]),
                format!("hunk-span-v1:{}:4:1:4:1", &keys(&repeated)[0][13..29]),
            ]
        );
    }

    #[gpui::test]
    async fn test_marks_persist_and_follow_the_diff(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let key = ReviewKey("uncommitted:/project".into());
        let path = RepoPath::new("src/lib.rs").unwrap();
        let base = lines(1..=20);
        let edited = base
            .replace("line 3\n", "LINE 3\n")
            .replace("line 15\n", "LINE 15\n");
        let original = digest(&base, &edited, cx);
        let first_hunk = original.hunks[0].key.clone();
        let second_hunk = original.hunks[1].key.clone();

        let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        cx.run_until_parked();
        session.update(cx, |session, cx| {
            assert!(session.is_loaded());
            session.mark_file(&path, original.content_hash, cx);
            session.toggle_hunk(&path, first_hunk.clone(), cx);
            session.toggle_hunk(&path, second_hunk.clone(), cx);
        });
        // Every tab of the review shares the session.
        let same = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        assert_eq!(same, session);
        let session_id = session.entity_id();
        drop((session, same));
        cx.run_until_parked();

        let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        assert_ne!(session.entity_id(), session_id);
        cx.run_until_parked();
        session.update(cx, |session, cx| {
            assert!(session.is_file_reviewed(&path));
            assert!(session.is_hunk_reviewed(&path, &first_hunk));
            assert!(session.is_hunk_reviewed(&path, &second_hunk));

            // The same diff keeps the marks.
            session.observe_digest(&path, &original, cx);
            assert!(session.is_file_reviewed(&path));

            // A changed diff makes the file unreviewed until it changes back. Hunk marks belong
            // to the hunks' content, which the digest's keys identify.
            let changed = file_digest_for_test(&original, 1);
            session.observe_digest(&path, &changed, cx);
            assert!(!session.is_file_reviewed(&path));
            session.observe_digest(&path, &original, cx);
            assert!(session.is_file_reviewed(&path));
            session.observe_digest(&path, &changed, cx);

            // Marking the changed file stores its new hash.
            session.mark_file(&path, changed.content_hash, cx);
            assert!(session.is_file_reviewed(&path));
            session.observe_digest(&path, &original, cx);
            assert!(!session.is_file_reviewed(&path));
        });
        drop(session);
        cx.run_until_parked();

        // Nothing was deleted along the way.
        let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        cx.run_until_parked();
        session.update(cx, |session, cx| {
            assert!(session.is_hunk_reviewed(&path, &first_hunk));
            assert!(session.is_hunk_reviewed(&path, &second_hunk));
            assert!(session.is_file_reviewed(&path));
            session.observe_digest(&path, &file_digest_for_test(&original, 1), cx);
            assert!(session.is_file_reviewed(&path));
            // Unmarking deletes the mark.
            session.unmark_file(&path, cx);
            assert!(!session.is_file_reviewed(&path));
        });
        drop(session);
        cx.run_until_parked();
        let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert!(!session.is_file_reviewed(&path)));
        // Other reviews have their own marks.
        let other =
            cx.update(|cx| ReviewSession::for_key(ReviewKey("uncommitted:/other".into()), cx));
        cx.run_until_parked();
        other.read_with(cx, |other, _| {
            assert!(!other.is_hunk_reviewed(&path, &first_hunk))
        });
    }

    /// `digest` with the hunk at `index` changed.
    fn file_digest_for_test(digest: &FileDigest, index: usize) -> FileDigest {
        let mut changed = digest.clone();
        changed.hunks[index].key = "hunk-content-v1:changed".into();
        changed.content_hash ^= 1;
        changed
    }
}
