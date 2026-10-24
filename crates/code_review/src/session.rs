use crate::{
    comments::{
        Comment, CommentEvent, CommentId, CommentState, CommentStore, CommentTarget, CommentType,
        NewComment,
    },
    forge::{DIFF_CONTEXT_LINES, Hunk},
    persistence::{CodeReviewDb, StoredReview},
};
use anyhow::Context as _;
use buffer_diff::BufferDiffSnapshot;
use collections::{HashMap, HashSet};
use futures::channel::oneshot;
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
/// after the first review has loaded.
const FORGET_REVIEWS_AFTER_DAYS: i64 = 90;

/// Identifies a review across tabs and restarts (DESIGN §2.6): the repository plus what is
/// reviewed, never a head SHA, so new commits or pushes keep the review's state.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReviewKey(pub SharedString);

/// The open sessions by key, so every tab of a review shares one.
#[derive(Default)]
struct ReviewSessions(HashMap<ReviewKey, WeakEntity<ReviewSession>>);

impl Global for ReviewSessions {}

/// How a review ended, for whoever waits for it (`zed --review --wait`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// The review was posted to the pull request; `url` is the posted review's.
    Submitted { url: String },
    /// A local review was finished: its comments as Markdown (`None` without comments).
    Finished { markdown: Option<String> },
}

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
    /// Line comments loaded from the database whose lines haven't been compared with their file
    /// yet: the first tab that shows the file does, once per run, and flags them outdated when
    /// the text changed. Other tabs (maybe another project's buffers with other unsaved edits)
    /// take the result as is.
    unchecked: HashSet<CommentId>,
    /// Who waits for the review to be submitted or finished.
    outcome_waiters: Vec<oneshot::Sender<ReviewOutcome>>,
    /// The tabs showing the session now. A tab also keeps the sessions it showed before a key
    /// switch (to switch back): those don't count, so the review counts as closed once no tab
    /// shows it.
    shown_by: usize,
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
        let prune = !cx.has_global::<PrunedOldReviews>();
        if prune {
            cx.set_global(PrunedOldReviews);
        }
        let load = db.open_review(key.0.to_string());
        let load_task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| match result {
                Ok(stored) => {
                    // Only now, so that the review being opened (whose load marked it opened)
                    // is never the one forgotten.
                    if prune {
                        let prune = this.db.prune(FORGET_REVIEWS_AFTER_DAYS * 24 * 60 * 60);
                        cx.background_spawn(async move {
                            prune.await.context("forgetting old reviews")
                        })
                        .detach_and_log_err(cx);
                    }
                    this.apply_stored_review(stored, cx)
                }
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
            unchecked: HashSet::default(),
            outcome_waiters: Vec::new(),
            shown_by: 0,
            _load_task: load_task,
        }
    }

    fn apply_stored_review(&mut self, stored: StoredReview, cx: &mut Context<Self>) {
        for (path, content_hash) in stored.files {
            match RepoPath::new(&path) {
                Ok(repo_path) => {
                    self.reviewed_files.insert(repo_path, content_hash);
                }
                Err(error) => log::warn!("skipping reviewed file {path:?}: {error:#}"),
            }
        }
        for (path, hunk_key) in stored.hunks {
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
        let max_stored_id = stored.comments.iter().map(|(id, _)| *id).max();
        let comments = stored
            .comments
            .into_iter()
            .filter_map(|(id, json)| {
                Comment::from_json(CommentId(id), &json)
                    .with_context(|| format!("skipping stored comment {id}"))
                    .log_err()
            })
            .collect::<Vec<_>>();
        self.unchecked.extend(
            comments
                .iter()
                .filter(|comment| matches!(comment.target, CommentTarget::Lines { .. }))
                .map(|comment| comment.id),
        );
        self.comments.restore(comments, max_stored_id);
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

    /// Saves the comment's text, which is trimmed first. A submitted comment can't change: it
    /// is on GitHub as it was.
    pub(crate) fn set_comment_body(&mut self, id: CommentId, body: &str, cx: &mut Context<Self>) {
        let Some(comment) = self
            .comments
            .get_mut(id)
            .filter(|comment| !comment.is_submitted())
        else {
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
        let Some(comment) = self
            .comments
            .get_mut(id)
            .filter(|comment| !comment.is_submitted())
        else {
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
        if self
            .comments
            .get(id)
            .is_some_and(|comment| comment.is_submitted())
        {
            return;
        }
        if self.comments.remove(id).is_some() {
            self.moved_comments.remove(&id);
            self.unchecked.remove(&id);
            persist(
                self.db.delete_comment(self.key.0.to_string(), id.0),
                "Couldn't delete the comment",
                cx,
            );
            cx.emit(ReviewSessionEvent::Comment(CommentEvent::Removed(id)));
            cx.notify();
        }
    }

    /// Records where line comments' lines are now, and their text unless the comment is
    /// outdated (its text is what the comment was about). Saved later.
    pub(crate) fn set_comment_positions(
        &mut self,
        positions: Vec<(CommentId, RangeInclusive<u32>, String)>,
        cx: &mut Context<Self>,
    ) {
        let mut moved = Vec::new();
        for (id, lines, line_text) in positions {
            let Some(comment) = self.comments.get_mut(id) else {
                continue;
            };
            let is_outdated = comment.is_outdated();
            let CommentTarget::Lines {
                lines: comment_lines,
                ..
            } = &mut comment.target
            else {
                continue;
            };
            let mut changed = false;
            if *comment_lines != lines {
                *comment_lines = lines;
                changed = true;
            }
            if !is_outdated && comment.line_text.as_deref() != Some(line_text.as_str()) {
                comment.line_text = Some(line_text);
                changed = true;
            }
            if changed {
                self.moved_comments.insert(id);
                moved.push(id);
            }
        }
        if !moved.is_empty() {
            cx.emit(ReviewSessionEvent::Comment(CommentEvent::Moved(moved)));
            cx.notify();
        }
    }

    pub(crate) fn needs_check(&self, id: CommentId) -> bool {
        self.unchecked.contains(&id)
    }

    /// Records whether a line comment's lines still have the text it was about. A comment whose
    /// text changed is outdated (stored) until its lines have that text again (a stash popped,
    /// an undo) or it is deleted.
    pub(crate) fn check_comment(&mut self, id: CommentId, outdated: bool, cx: &mut Context<Self>) {
        self.unchecked.remove(&id);
        let Some(comment) = self.comments.get_mut(id) else {
            return;
        };
        let state = match (outdated, &comment.state) {
            (true, CommentState::Draft) => CommentState::Outdated,
            (false, CommentState::Outdated) => CommentState::Draft,
            _ => return,
        };
        comment.state = state;
        self.save_comment(id, cx);
        cx.emit(ReviewSessionEvent::Comment(CommentEvent::Changed(id)));
        cx.notify();
    }

    /// Records that the comments were posted to GitHub as part of the review `review_id`: they
    /// can't change any more, and the pull request's threads show them from now on.
    pub(crate) fn mark_submitted(
        &mut self,
        ids: &[CommentId],
        review_id: SharedString,
        cx: &mut Context<Self>,
    ) {
        for id in ids {
            let Some(comment) = self.comments.get_mut(*id) else {
                continue;
            };
            comment.state = CommentState::Submitted {
                review_id: review_id.clone(),
            };
            self.save_comment(*id, cx);
            cx.emit(ReviewSessionEvent::Comment(CommentEvent::Changed(*id)));
        }
        cx.notify();
    }

    /// Resolves once the review is submitted or finished; canceled when no tab shows the review
    /// any more (see [`Self::hide`]).
    pub(crate) fn wait_for_outcome(&mut self) -> oneshot::Receiver<ReviewOutcome> {
        let (sender, receiver) = oneshot::channel();
        self.outcome_waiters.retain(|waiter| !waiter.is_canceled());
        self.outcome_waiters.push(sender);
        receiver
    }

    pub(crate) fn has_outcome_waiters(&mut self) -> bool {
        self.outcome_waiters.retain(|waiter| !waiter.is_canceled());
        !self.outcome_waiters.is_empty()
    }

    pub(crate) fn resolve_outcome(&mut self, outcome: ReviewOutcome) {
        for waiter in self.outcome_waiters.drain(..) {
            // A waiter that went away doesn't want it any more.
            waiter.send(outcome.clone()).ok();
        }
    }

    /// A tab shows the session.
    pub(crate) fn show(&mut self) {
        self.shown_by += 1;
    }

    /// A tab stopped showing the session: it closed, or it switched to `successor` (its review
    /// key changed: another base, another branch). Once no tab shows the session, its waiters
    /// wait for the review the last tab switched to, or hear that the review was closed.
    pub(crate) fn hide(&mut self, successor: Option<&mut ReviewSession>) {
        self.shown_by = self.shown_by.saturating_sub(1);
        if self.shown_by > 0 {
            return;
        }
        let waiters = self
            .outcome_waiters
            .drain(..)
            .filter(|waiter| !waiter.is_canceled());
        match successor {
            Some(successor) => successor.outcome_waiters.extend(waiters),
            // Dropping a sender cancels its waiter.
            None => waiters.for_each(drop),
        }
    }

    #[cfg(test)]
    pub(crate) fn shown_by(&self) -> usize {
        self.shown_by
    }

    pub(crate) fn flush(&mut self, cx: &mut Context<Self>) {
        for id in std::mem::take(&mut self.moved_comments) {
            self.save_comment(id, cx);
        }
        // A review used for longer than the pruning age without being reopened is kept.
        persist(
            self.db.touch_review(self.key.0.to_string()),
            "Couldn't save the review",
            cx,
        );
    }

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

/// The hunks of a file's diff as `git diff` shows them: each change with its unchanged lines
/// around it. Lines are 1-based, old ones in the diff's base.
pub(crate) fn diff_hunks(diff: &BufferDiffSnapshot, buffer: &BufferSnapshot) -> Vec<Hunk> {
    let base_text = diff.base_text();
    let changes = diff
        .hunks_intersecting_range(Anchor::min_max_range_for_buffer(buffer.remote_id()), buffer)
        .map(|hunk| {
            let old_start = base_text.offset_to_point(hunk.diff_base_byte_range.start);
            let old_end = base_text.offset_to_point(hunk.diff_base_byte_range.end);
            let old_count = line_count(old_start.row, old_end.row, old_end.column);
            let new_count = line_count(
                hunk.range.start.row,
                hunk.range.end.row,
                hunk.range.end.column,
            );
            let old_start = old_start.row + 1;
            let new_start = hunk.range.start.row + 1;
            (
                old_start..old_start + old_count,
                new_start..new_start + new_count,
            )
        })
        .collect::<Vec<_>>();
    let lines_of = |text: &BufferSnapshot| {
        let end = text.max_point();
        line_count(0, end.row, end.column)
    };
    Hunk::with_context(
        &changes,
        DIFF_CONTEXT_LINES,
        lines_of(base_text),
        lines_of(buffer),
    )
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
                },
                cx,
            );
            // Positions are stored on the next change, or when flushed.
            session.set_comment_positions(vec![(second, 10..=11, "line 10\nline 11".into())], cx);
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
            // Their lines are compared with their files once those are shown.
            assert!(session.needs_check(second.id));
        });
        // Ids continue after the stored ones.
        let next = add_line_comment(&session, "next", cx);
        assert!(next.0 > second.0);

        // An outdated comment stays outdated, and keeps the text it was about.
        session.update(cx, |session, cx| {
            session.check_comment(first, true, cx);
            assert!(!session.needs_check(first));
            session.set_comment_positions(vec![(first, 1..=1, "changed".into())], cx);
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

        // Until its lines have that text again.
        session.update(cx, |session, cx| {
            session.check_comment(first, false, cx);
            assert_eq!(
                session.comments().get(first).unwrap().state,
                CommentState::Draft
            );
            session.flush(cx);
        });
        drop(session);
        cx.run_until_parked();
        let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(
                session.comments().get(first).unwrap().state,
                CommentState::Draft
            );
        });

        // Other reviews have their own comments.
        let other =
            cx.update(|cx| ReviewSession::for_key(ReviewKey("uncommitted:/other".into()), cx));
        cx.run_until_parked();
        assert!(comment_bodies(&other, cx).is_empty());
    }

    #[gpui::test]
    async fn test_submitted_comments_stay_submitted(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let key = ReviewKey("pr:github.com/o/r#1".into());
        let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        cx.run_until_parked();
        let submitted = add_line_comment(&session, "sent", cx);
        let draft = add_line_comment(&session, "kept", cx);
        session.update(cx, |session, cx| {
            session.mark_submitted(&[submitted], "42".into(), cx)
        });
        drop(session);
        cx.run_until_parked();

        let session = cx.update(|cx| ReviewSession::for_key(key, cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            let comments = session.comments();
            assert_eq!(
                comments.get(submitted).unwrap().state,
                CommentState::Submitted {
                    review_id: "42".into()
                }
            );
            assert_eq!(comments.get(draft).unwrap().state, CommentState::Draft);
        });
    }

    #[gpui::test]
    async fn test_old_reviews_are_forgotten(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let old_key = ReviewKey("uncommitted:/old".into());
        let recent_key = ReviewKey("uncommitted:/recent".into());
        let first_key = ReviewKey("uncommitted:/first".into());
        let used_key = ReviewKey("uncommitted:/used".into());
        for key in [&old_key, &recent_key, &first_key, &used_key] {
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
        for (key, age) in [(&old_key, 91), (&recent_key, 89), (&first_key, 91)] {
            db.set_last_opened_at(key.0.to_string(), now - days(age))
                .await
                .unwrap();
        }
        // A review open all along counts as opened whenever it is written to.
        let used = cx.update(|cx| ReviewSession::for_key(used_key.clone(), cx));
        cx.run_until_parked();
        db.set_last_opened_at(used_key.0.to_string(), now - days(91))
            .await
            .unwrap();
        add_line_comment(&used, "still used", cx);
        cx.run_until_parked();

        // The next run of Zed forgets the old reviews once its first review has loaded, which
        // is never one of them.
        cx.update(|cx| cx.remove_global::<PrunedOldReviews>());
        let first = cx.update(|cx| ReviewSession::for_key(first_key.clone(), cx));
        cx.run_until_parked();
        let recent = cx.update(|cx| ReviewSession::for_key(recent_key.clone(), cx));
        let old = cx.update(|cx| ReviewSession::for_key(old_key.clone(), cx));
        cx.run_until_parked();
        assert_eq!(comment_bodies(&first, cx).len(), 1);
        assert_eq!(comment_bodies(&recent, cx).len(), 1);
        assert!(comment_bodies(&old, cx).is_empty());
        let path = RepoPath::new("src/lib.rs").unwrap();
        first.read_with(cx, |first, _| assert!(first.is_file_reviewed(&path)));
        recent.read_with(cx, |recent, _| assert!(recent.is_file_reviewed(&path)));
        old.read_with(cx, |old, _| assert!(!old.is_file_reviewed(&path)));
        let used_rows = db.open_review(used_key.0.to_string()).await.unwrap();
        assert_eq!(used_rows.comments.len(), 2);
        assert_eq!(used_rows.files.len(), 1);
    }

    #[gpui::test]
    async fn test_unreadable_comments_are_kept(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let key = ReviewKey("uncommitted:/project".into());
        let db = cx.update(|cx| CodeReviewDb::global(cx));
        // A newer version of Zed wrote a comment this one can't read, with the largest id.
        let readable = r#"{"target":{"kind":"review"},"body":"readable"}"#;
        let unreadable = r#"{"target":{"kind":"thread","id":"t1"},"body":"from the future"}"#;
        db.save_comment(key.0.to_string(), 2, readable.into())
            .await
            .unwrap();
        db.save_comment(key.0.to_string(), 7, unreadable.into())
            .await
            .unwrap();

        let session = cx.update(|cx| ReviewSession::for_key(key.clone(), cx));
        cx.run_until_parked();
        assert_eq!(comment_bodies(&session, cx), [(2, "readable".to_string())]);
        let new = add_line_comment(&session, "new", cx);
        assert_eq!(new, CommentId(8));
        drop(session);
        cx.run_until_parked();

        let stored = db.open_review(key.0.to_string()).await.unwrap();
        assert_eq!(
            stored
                .comments
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            [2, 7, 8]
        );
        assert!(stored.comments.contains(&(7, unreadable.to_string())));
    }

    /// `digest` with the hunk at `index` changed.
    fn file_digest_for_test(digest: &FileDigest, index: usize) -> FileDigest {
        let mut changed = digest.clone();
        changed.hunks[index].key = "hunk-content-v1:changed".into();
        changed.content_hash ^= 1;
        changed
    }
}
