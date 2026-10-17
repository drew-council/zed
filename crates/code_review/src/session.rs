use crate::{
    comments::{
        Comment, CommentEvent, CommentId, CommentState, CommentStore, CommentTarget, CommentType,
        NewComment,
    },
    persistence::{CodeReviewDb, StoredReview},
};
use anyhow::Context as _;
use buffer_diff::BufferDiffSnapshot;
use collections::{HashMap, HashSet};
use git::repository::RepoPath;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task, TaskExt as _,
    WeakEntity,
};
use language::{Anchor, BufferSnapshot};
use std::{
    future::Future,
    ops::{Range, RangeInclusive},
};
use util::ResultExt as _;

/// Reviews not opened for this long are forgotten (marks and comments), once per run of Zed,
/// when the first review opens.
const FORGET_REVIEWS_AFTER_DAYS: i64 = 90;

/// Identifies a review across tabs and restarts (DESIGN §2.6): the repository plus what is
/// reviewed, never a head SHA, so new commits or pushes keep the review's state.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReviewKey(pub SharedString);

/// The open sessions by key, so every tab of a review shares one.
#[derive(Default)]
struct ReviewSessions(HashMap<ReviewKey, WeakEntity<ReviewSession>>);

impl Global for ReviewSessions {}

/// Set once old reviews have been pruned in this run.
struct PrunedOldReviews;

impl Global for PrunedOldReviews {}

/// The reviewed marks and comments of one review, loaded from and saved to [`CodeReviewDb`].
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
    comments: CommentStore,
    /// Comments whose lines moved (or whose text changed) since they were last saved. They are
    /// saved with the comment's next change, and when the review's tabs close or Zed quits.
    moved_comments: HashSet<CommentId>,
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
    Comment(CommentEvent),
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
        if !cx.has_global::<PrunedOldReviews>() {
            cx.set_global(PrunedOldReviews);
            let prune = db.prune(FORGET_REVIEWS_AFTER_DAYS * 24 * 60 * 60);
            cx.background_spawn(async move { prune.await.context("forgetting old reviews") })
                .detach_and_log_err(cx);
        }
        let load = db.open_review(key.0.to_string());
        let load_task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| match result {
                Ok(stored) => this.apply_stored_review(stored, cx),
                // Marks and comments stay unchangeable, so that nothing overwrites the stored
                // ones.
                Err(error) => {
                    log::error!("loading a review: {error:#}");
                    cx.emit(ReviewSessionEvent::Failed(
                        format!("Couldn't load the review's marks and comments: {error:#}").into(),
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
            comments: CommentStore::default(),
            moved_comments: HashSet::default(),
            _load_task: load_task,
        }
    }

    fn apply_stored_review(&mut self, stored: StoredReview, cx: &mut Context<Self>) {
        let marks = stored;
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
        self.comments
            .restore(marks.comments.into_iter().filter_map(|(id, json)| {
                Comment::from_json(CommentId(id), &json)
                    .with_context(|| format!("skipping stored comment {id}"))
                    .log_err()
            }));
        self.loaded = true;
        cx.emit(ReviewSessionEvent::Loaded);
        cx.notify();
    }

    pub(crate) fn key(&self) -> &ReviewKey {
        &self.key
    }

    /// Marks can't change and comments can't be added until the stored ones are loaded (which
    /// takes milliseconds), or the load would overwrite them.
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
            MARKS_NOT_SAVED,
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
            MARKS_NOT_SAVED,
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
                MARKS_NOT_SAVED,
                cx,
            );
            false
        } else {
            hunks.insert(hunk_key.clone());
            persist(
                self.db
                    .set_hunk_reviewed(key, unix_path, hunk_key.to_string()),
                MARKS_NOT_SAVED,
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
                MARKS_NOT_SAVED,
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

impl ReviewSession {
    pub(crate) fn comments(&self) -> &CommentStore {
        &self.comments
    }

    /// Adds a comment, which is saved once it has text. `None` until the stored comments are
    /// loaded.
    pub(crate) fn add_comment(
        &mut self,
        new_comment: NewComment,
        cx: &mut Context<Self>,
    ) -> Option<CommentId> {
        if !self.loaded {
            return None;
        }
        let id = self.comments.insert(new_comment);
        cx.emit(ReviewSessionEvent::Comment(CommentEvent::Added(id)));
        cx.notify();
        Some(id)
    }

    /// Saves the comment's text, which is trimmed first.
    pub(crate) fn set_comment_body(&mut self, id: CommentId, body: &str, cx: &mut Context<Self>) {
        let Some(comment) = self.comments.get_mut(id) else {
            return;
        };
        let body = body.trim();
        if comment.body == body {
            return;
        }
        comment.body = body.to_string();
        self.save_comment(id, cx);
        cx.emit(ReviewSessionEvent::Comment(CommentEvent::Changed(id)));
        cx.notify();
    }

    pub(crate) fn set_comment_type(
        &mut self,
        id: CommentId,
        comment_type: Option<CommentType>,
        cx: &mut Context<Self>,
    ) {
        let Some(comment) = self.comments.get_mut(id) else {
            return;
        };
        if comment.comment_type == comment_type {
            return;
        }
        comment.comment_type = comment_type;
        self.save_comment(id, cx);
        cx.emit(ReviewSessionEvent::Comment(CommentEvent::Changed(id)));
        cx.notify();
    }

    pub(crate) fn delete_comment(&mut self, id: CommentId, cx: &mut Context<Self>) {
        if self.comments.remove(id).is_some() {
            self.moved_comments.remove(&id);
            persist(
                self.db.delete_comment(self.key.0.to_string(), id.0),
                "Couldn't delete the comment",
                cx,
            );
            cx.emit(ReviewSessionEvent::Comment(CommentEvent::Removed(id)));
            cx.notify();
        }
    }

    /// Records where a line comment's anchor is now: its lines, and their text unless the
    /// comment is outdated (whose text is what the comment was about). Saved later.
    pub(crate) fn set_comment_position(
        &mut self,
        id: CommentId,
        lines: RangeInclusive<u32>,
        line_text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(comment) = self.comments.get_mut(id) else {
            return;
        };
        let CommentTarget::Lines {
            lines: comment_lines,
            ..
        } = &mut comment.target
        else {
            return;
        };
        let mut changed = false;
        if *comment_lines != lines {
            *comment_lines = lines;
            changed = true;
        }
        if !comment.is_outdated() && comment.line_text.as_deref() != Some(line_text.as_str()) {
            comment.line_text = Some(line_text);
            changed = true;
        }
        if changed {
            self.moved_comments.insert(id);
            cx.emit(ReviewSessionEvent::Comment(CommentEvent::Changed(id)));
            cx.notify();
        }
    }

    /// Gives a comment loaded from the database its anchor in its file, flagging it outdated
    /// when its lines' text changed while the review was closed.
    pub(crate) fn resolve_comment(
        &mut self,
        id: CommentId,
        anchor: Range<Anchor>,
        outdated: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(comment) = self.comments.get_mut(id) else {
            return;
        };
        comment.anchor = Some(anchor);
        if outdated && comment.state == CommentState::Draft {
            comment.state = CommentState::Outdated;
            self.save_comment(id, cx);
        }
        cx.emit(ReviewSessionEvent::Comment(CommentEvent::Changed(id)));
        cx.notify();
    }

    /// Forgets a comment's anchor, whose buffer was replaced (the file's diff base was reloaded
    /// as another buffer, or its snapshot rebuilt); it is resolved again by its lines.
    pub(crate) fn unresolve_comment(&mut self, id: CommentId) {
        if let Some(comment) = self.comments.get_mut(id) {
            comment.anchor = None;
        }
    }

    /// Saves the comments whose position changed since they were last saved.
    pub(crate) fn flush(&mut self, cx: &mut Context<Self>) {
        for id in std::mem::take(&mut self.moved_comments) {
            self.save_comment(id, cx);
        }
    }

    /// Completes once every write queued so far has.
    pub(crate) fn saved(&self) -> impl Future<Output = ()> + use<> {
        let done = self.db.sync();
        async move {
            done.await.log_err();
        }
    }

    /// Comments without text aren't saved: a new comment starts empty, and is deleted if it is
    /// left so.
    fn save_comment(&mut self, id: CommentId, cx: &mut Context<Self>) {
        let Some(comment) = self.comments.get(id) else {
            return;
        };
        if comment.body.is_empty() {
            return;
        }
        self.moved_comments.remove(&id);
        match comment.to_json() {
            Ok(json) => persist(
                self.db.save_comment(self.key.0.to_string(), id.0, json),
                "Couldn't save the comment",
                cx,
            ),
            Err(error) => log::error!("serializing comment {id:?}: {error:#}"),
        }
    }
}

const MARKS_NOT_SAVED: &str = "Couldn't save the reviewed marks";

/// The write is already queued; this only reports its failure.
fn persist(
    write: impl Future<Output = anyhow::Result<()>> + 'static,
    failure: &'static str,
    cx: &mut Context<ReviewSession>,
) {
    cx.spawn(async move |this, cx| {
        if let Err(error) = write.await {
            log::error!("{failure}: {error:#}");
            this.update(cx, |_, cx| {
                cx.emit(ReviewSessionEvent::Failed(
                    format!("{failure}: {error:#}").into(),
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

    fn comment_bodies(session: &Entity<ReviewSession>, cx: &TestAppContext) -> Vec<(u64, String)> {
        session.read_with(cx, |session, _| {
            session
                .comments()
                .iter()
                .map(|comment| (comment.id.0, comment.body.clone()))
                .collect()
        })
    }

    fn add_line_comment(
        session: &Entity<ReviewSession>,
        body: &str,
        cx: &mut TestAppContext,
    ) -> CommentId {
        session.update(cx, |session, cx| {
            let id = session
                .add_comment(
                    NewComment {
                        target: CommentTarget::Lines {
                            path: RepoPath::new("src/lib.rs").unwrap(),
                            side: crate::comments::Side::New,
                            lines: 3..=4,
                        },
                        line_text: Some("line 3\nline 4".into()),
                        anchor: None,
                    },
                    cx,
                )
                .unwrap();
            session.set_comment_body(id, body, cx);
            id
        })
    }

    #[gpui::test]
    async fn test_comments_persist(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let key = ReviewKey("uncommitted:/project".into());
        let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        // Nothing can be added before the stored comments are loaded.
        session.update(cx, |session, cx| {
            let new_comment = NewComment {
                target: CommentTarget::Review,
                line_text: None,
                anchor: None,
            };
            assert!(session.add_comment(new_comment, cx).is_none());
        });
        cx.run_until_parked();

        let first = add_line_comment(&session, "  first  ", cx);
        let second = add_line_comment(&session, "second", cx);
        let deleted = add_line_comment(&session, "deleted", cx);
        session.update(cx, |session, cx| {
            session.set_comment_type(first, Some(CommentType::Issue), cx);
            session.delete_comment(deleted, cx);
            // A comment without text isn't stored.
            session.add_comment(
                NewComment {
                    target: CommentTarget::Review,
                    line_text: None,
                    anchor: None,
                },
                cx,
            );
            // Positions are stored on the next change, or when flushed.
            session.set_comment_position(second, 10..=11, "line 10\nline 11".into(), cx);
            session.flush(cx);
        });
        drop(session);
        cx.run_until_parked();

        let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        cx.run_until_parked();
        assert_eq!(
            comment_bodies(&session, cx),
            [
                (first.0, "first".to_string()),
                (second.0, "second".to_string())
            ]
        );
        session.read_with(cx, |session, _| {
            let first = session.comments().get(first).unwrap();
            assert_eq!(first.comment_type, Some(CommentType::Issue));
            assert_eq!(first.state, CommentState::Draft);
            let second = session.comments().get(second).unwrap();
            assert_eq!(
                second.target,
                CommentTarget::Lines {
                    path: RepoPath::new("src/lib.rs").unwrap(),
                    side: crate::comments::Side::New,
                    lines: 10..=11,
                }
            );
            assert_eq!(second.line_text.as_deref(), Some("line 10\nline 11"));
            assert!(second.anchor.is_none());
        });
        // Ids continue after the stored ones.
        let next = add_line_comment(&session, "next", cx);
        assert!(next.0 > second.0);

        // An outdated comment stays outdated, and keeps the text it was about.
        session.update(cx, |session, cx| {
            let anchor =
                language::Anchor::min_max_range_for_buffer(language::BufferId::new(1).unwrap());
            session.resolve_comment(first, anchor, true, cx);
            session.set_comment_position(first, 1..=1, "changed".into(), cx);
            session.flush(cx);
        });
        drop(session);
        cx.run_until_parked();
        let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            let first = session.comments().get(first).unwrap();
            assert_eq!(first.state, CommentState::Outdated);
            assert_eq!(first.line_text.as_deref(), Some("line 3\nline 4"));
        });

        // Other reviews have their own comments.
        let other =
            cx.update(|cx| ReviewSession::for_key(ReviewKey("uncommitted:/other".into()), cx));
        cx.run_until_parked();
        assert!(comment_bodies(&other, cx).is_empty());
    }

    #[gpui::test]
    async fn test_old_reviews_are_forgotten(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let old_key = ReviewKey("uncommitted:/old".into());
        let recent_key = ReviewKey("uncommitted:/recent".into());
        for key in [&old_key, &recent_key] {
            let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
            cx.run_until_parked();
            add_line_comment(&session, "kept?", cx);
            session.update(cx, |session, cx| {
                session.mark_file(&RepoPath::new("src/lib.rs").unwrap(), 1, cx)
            });
            drop(session);
            cx.run_until_parked();
        }
        let db = cx.update(|cx| CodeReviewDb::global(cx));
        let days = |days: i64| days * 24 * 60 * 60;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        db.set_last_opened_at(old_key.0.to_string(), now - days(91))
            .await
            .unwrap();
        db.set_last_opened_at(recent_key.0.to_string(), now - days(89))
            .await
            .unwrap();

        // The next run of Zed forgets the old review when it opens its first review.
        cx.update(|cx| cx.remove_global::<PrunedOldReviews>());
        let recent = cx.update(|cx| ReviewSession::for_key(recent_key.clone(), cx));
        let old = cx.update(|cx| ReviewSession::for_key(old_key.clone(), cx));
        cx.run_until_parked();
        assert_eq!(comment_bodies(&recent, cx).len(), 1);
        assert!(comment_bodies(&old, cx).is_empty());
        let path = RepoPath::new("src/lib.rs").unwrap();
        recent.read_with(cx, |recent, _| assert!(recent.is_file_reviewed(&path)));
        old.read_with(cx, |old, _| assert!(!old.is_file_reviewed(&path)));
    }

    /// `digest` with the hunk at `index` changed.
    fn file_digest_for_test(digest: &FileDigest, index: usize) -> FileDigest {
        let mut changed = digest.clone();
        changed.hunks[index].key = "hunk-content-v1:changed".into();
        changed.content_hash ^= 1;
        changed
    }
}
