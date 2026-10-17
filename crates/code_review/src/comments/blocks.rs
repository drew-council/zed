use super::{
    Comment, CommentEvent, CommentId, CommentState, CommentTarget, CommentType, NewComment, Side,
    view::{CommentView, EditingState, Leave},
};
use crate::{
    diff::{ReviewDiff, review_path_key},
    line_cursor::selected_rows,
    session::{ReviewSession, ReviewSessionEvent},
};
use buffer_diff::DiffHunkStatusKind;
use collections::{HashMap, HashSet};
use editor::{
    Anchor, Bias, DisplayPoint, Editor, EditorEvent, EditorMode, SelectionEffects,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    scroll::Autoscroll,
};
use gpui::{Entity, EventEmitter, Focusable as _, PromptLevel, Subscription, WeakEntity};
use language::{
    Buffer, BufferId, LanguageRegistry, Point, ToPoint as _, language_settings::SoftWrap,
};
use multi_buffer::{
    MultiBuffer, MultiBufferRow, MultiBufferSnapshot, PathKey, RowInfo, ToPoint as _,
};
use std::{ops::RangeInclusive, sync::Arc};
use ui::prelude::*;
use util::{ResultExt as _, truncate_and_trailoff};
use workspace::{Toast, Workspace, notifications::NotificationId};

pub(crate) enum CommentBlocksEvent {
    /// A comment editor opened or closed, which the review tab's capability follows.
    EditingChanged,
}

/// The comments of one review tab: a view per comment, the blocks that show line and file
/// comments in the diff, comment editors and the review keys that act on comments. The comments
/// themselves belong to the review's [`ReviewSession`], which every tab of the review shares.
pub(crate) struct CommentBlocks {
    session: Entity<ReviewSession>,
    diff: Entity<ReviewDiff>,
    workspace: WeakEntity<Workspace>,
    language_registry: Arc<LanguageRegistry>,
    views: HashMap<CommentId, Entity<CommentView>>,
    /// The blocks showing comments on the right side of the diff.
    hosted: HashMap<CommentId, HostedBlock>,
    /// The comment `]t`/`[t` moved to, which tells comments below the same row apart while the
    /// cursor stays on it.
    selected: Option<CommentId>,
    targeted: Option<CommentId>,
    focus_guard: Option<Subscription>,
    _session_subscription: Subscription,
    _subscriptions: Vec<Subscription>,
}

struct HostedBlock {
    block: CustomBlockId,
    placement: Placement,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Placement {
    /// The row the comment belongs to: its last line, or the file's first line.
    row: Anchor,
    is_file: bool,
    block: BlockAnchor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockAnchor {
    Above(Anchor),
    Below(Anchor),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Next,
    Previous,
}

/// What `c` comments on.
enum SelectedLines {
    /// The cursor is on a reviewed file's folded header.
    FoldedFile(BufferId),
    Lines {
        side: Side,
        buffer_id: BufferId,
        /// Rows in the side's buffer.
        rows: RangeInclusive<u32>,
    },
}

impl EventEmitter<CommentBlocksEvent> for CommentBlocks {}

impl CommentBlocks {
    pub(crate) fn new(
        session: Entity<ReviewSession>,
        diff: Entity<ReviewDiff>,
        workspace: WeakEntity<Workspace>,
        language_registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let splittable = diff.read(cx).editor().clone();
        let subscriptions = vec![
            cx.subscribe_in(
                &splittable,
                window,
                |this, _, event: &EditorEvent, window, cx| match event {
                    EditorEvent::BufferRangesUpdated { .. }
                    | EditorEvent::BuffersRemoved { .. }
                    | EditorEvent::BufferFoldToggled { .. } => this.reconcile(window, cx),
                    EditorEvent::SelectionsChanged { .. } => this.update_targeted(cx),
                    _ => {}
                },
            ),
            // Text typed into a comment editor isn't lost when its tab closes.
            cx.on_release(|this, cx| this.save_open_editors(cx)),
        ];
        let session_subscription = cx.subscribe_in(&session, window, Self::handle_session_event);
        let mut this = Self {
            session,
            diff,
            workspace,
            language_registry,
            views: HashMap::default(),
            hosted: HashMap::default(),
            selected: None,
            targeted: None,
            focus_guard: None,
            _session_subscription: session_subscription,
            _subscriptions: subscriptions,
        };
        this.create_views(cx);
        this.reconcile(window, cx);
        this
    }

    /// Shows the comments of another review key (the base or the branch changed). Open editors
    /// keep their text in the session they belong to.
    pub(crate) fn set_session(
        &mut self,
        session: Entity<ReviewSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if session == self.session {
            return;
        }
        let had_focus = self.focused_comment(window, cx).is_some();
        self.save_open_editors(cx);
        let blocks = self
            .hosted
            .drain()
            .map(|(_, hosted)| hosted.block)
            .collect::<HashSet<_>>();
        self.rhs_editor(cx).update(cx, |editor, cx| {
            editor.remove_blocks(blocks, None, cx);
        });
        self.views.clear();
        self.selected = None;
        self.targeted = None;
        self.session = session;
        self._session_subscription =
            cx.subscribe_in(&self.session, window, Self::handle_session_event);
        self.create_views(cx);
        self.reconcile(window, cx);
        if had_focus {
            self.return_focus(None, window, cx);
        }
        cx.emit(CommentBlocksEvent::EditingChanged);
        cx.notify();
    }

    fn handle_session_event(
        &mut self,
        _: &Entity<ReviewSession>,
        event: &ReviewSessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ReviewSessionEvent::Comment(event) = event else {
            return;
        };
        match *event {
            CommentEvent::Added(id) => {
                self.ensure_view(id, cx);
                self.reconcile(window, cx);
            }
            CommentEvent::Changed(id) => {
                let comment = self.session.read(cx).comments().get(id).cloned();
                if let (Some(view), Some(comment)) = (self.views.get(&id), comment) {
                    view.update(cx, |view, cx| view.update_comment(&comment, cx));
                }
            }
            CommentEvent::Removed(id) => {
                self.remove_view(id, window, cx);
                self.reconcile(window, cx);
            }
        }
        cx.notify();
    }

    fn create_views(&mut self, cx: &mut Context<Self>) {
        let ids = self
            .session
            .read(cx)
            .comments()
            .iter()
            .map(|comment| comment.id)
            .collect::<Vec<_>>();
        for id in ids {
            self.ensure_view(id, cx);
        }
    }

    fn ensure_view(&mut self, id: CommentId, cx: &mut Context<Self>) {
        if self.views.contains_key(&id) {
            return;
        }
        let Some(comment) = self.session.read(cx).comments().get(id).cloned() else {
            return;
        };
        let blocks = cx.weak_entity();
        let language_registry = self.language_registry.clone();
        let view = cx.new(|cx| CommentView::new(&comment, blocks, language_registry, cx));
        self.views.insert(id, view);
    }

    fn remove_view(&mut self, id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.views.remove(&id) else {
            return;
        };
        let editing = view.update(cx, |view, cx| view.take_editing(cx));
        if let Some(editing) = editing {
            if editing.editor.focus_handle(cx).is_focused(window) {
                self.return_focus(editing.return_to.upgrade(), window, cx);
            }
            cx.emit(CommentBlocksEvent::EditingChanged);
        }
        if self.targeted == Some(id) {
            self.targeted = None;
        }
    }

    fn rhs_editor(&self, cx: &App) -> Entity<Editor> {
        self.diff.read(cx).editor().read(cx).rhs_editor().clone()
    }

    fn focused_diff_editor(&self, cx: &App) -> Entity<Editor> {
        self.diff
            .read(cx)
            .editor()
            .read(cx)
            .focused_editor()
            .clone()
    }

    pub(crate) fn is_editing(&self, cx: &App) -> bool {
        self.views
            .values()
            .any(|view| view.read(cx).editing().is_some())
    }

    /// The comment views of the review-level comments, in creation order.
    pub(crate) fn review_comment_views(&self, cx: &App) -> Vec<Entity<CommentView>> {
        self.session
            .read(cx)
            .comments()
            .iter()
            .filter(|comment| comment.target == CommentTarget::Review)
            .filter_map(|comment| self.views.get(&comment.id).cloned())
            .collect()
    }

    fn focused_comment(&self, window: &Window, cx: &App) -> Option<CommentId> {
        self.views.iter().find_map(|(id, view)| {
            let editor = view.read(cx).editor()?;
            editor.focus_handle(cx).is_focused(window).then_some(*id)
        })
    }

    /// Brings the comment blocks in line with the comments and with what the diff shows: a
    /// comment whose lines aren't in an excerpt (hidden or folded file, lines without changes
    /// nearby) has no block.
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rhs = self.rhs_editor(cx);
        let desired = {
            let editor = rhs.read(cx);
            let session = self.session.read(cx);
            session
                .comments()
                .iter()
                .filter_map(|comment| {
                    let placement = placement(comment, editor, cx)?;
                    Some((comment.id, placement))
                })
                .collect::<HashMap<_, _>>()
        };

        let mut stale = HashSet::default();
        let mut unhosted = Vec::new();
        self.hosted.retain(|id, hosted| {
            let keep = desired.get(id) == Some(&hosted.placement);
            if !keep {
                stale.insert(hosted.block);
                if !desired.contains_key(id) {
                    unhosted.push(*id);
                }
            }
            keep
        });
        let mut new_blocks = desired
            .into_iter()
            .filter(|(id, _)| !self.hosted.contains_key(id))
            .filter_map(|(id, placement)| Some((id, placement, self.views.get(&id)?.clone())))
            .collect::<Vec<_>>();
        if stale.is_empty() && new_blocks.is_empty() {
            return;
        }
        new_blocks.sort_by_key(|(id, _, _)| *id);

        let block_ids = rhs.update(cx, |editor, cx| {
            editor.remove_blocks(stale, None, cx);
            editor.insert_blocks(
                new_blocks
                    .iter()
                    .map(|(id, placement, view)| block_properties(*id, *placement, view.clone())),
                None,
                cx,
            )
        });
        for ((id, placement, _), block) in new_blocks.into_iter().zip(block_ids) {
            self.hosted.insert(id, HostedBlock { block, placement });
        }

        // An editor whose block went away can't keep the focus.
        if let Some(focused) = self.focused_comment(window, cx)
            && unhosted.contains(&focused)
        {
            let return_to = self.views.get(&focused).and_then(|view| {
                view.read(cx)
                    .editing()
                    .and_then(|editing| editing.return_to.upgrade())
            });
            self.return_focus(return_to, window, cx);
        }
        self.update_targeted(cx);
    }

    /// `c`: comments on the cursor line or the selected lines. On a folded file header, comments
    /// on the file.
    pub(crate) fn add_line_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let splittable = self.diff.read(cx).editor().read(cx);
        let editor = splittable.focused_editor().clone();
        let is_lhs = splittable.lhs_editor() == Some(&editor);
        let Some(selected) = editor.update(cx, |editor, cx| selected_lines(editor, is_lhs, cx))
        else {
            return;
        };
        collapse_selection(&editor, window, cx);
        match selected {
            SelectedLines::FoldedFile(buffer_id) => {
                let Some(path) = self.diff.read(cx).repo_path_for_buffer(buffer_id, cx) else {
                    return;
                };
                self.add_file_comment_on(path, editor, window, cx);
            }
            SelectedLines::Lines {
                side: Side::Old, ..
            } => self.show_toast(
                "Comments on the old side of the diff aren't supported yet",
                cx,
            ),
            SelectedLines::Lines {
                side: Side::New,
                buffer_id,
                rows,
            } => {
                let Some(path) = self.diff.read(cx).repo_path_for_buffer(buffer_id, cx) else {
                    return;
                };
                let Some(buffer) = editor.read(cx).buffer().read(cx).buffer(buffer_id) else {
                    return;
                };
                let snapshot = buffer.read(cx).snapshot();
                let start = Point::new(*rows.start(), 0);
                let end = Point::new(*rows.end(), snapshot.line_len(*rows.end()));
                let new_comment = NewComment {
                    target: CommentTarget::Lines {
                        path,
                        side: Side::New,
                        lines: rows.start() + 1..=rows.end() + 1,
                    },
                    line_text: Some(snapshot.text_for_range(start..end).collect()),
                    // Text typed at the start of the first line or the end of the last one
                    // stays inside the range.
                    anchor: Some(snapshot.anchor_after(start)..snapshot.anchor_before(end)),
                };
                self.add_comment(new_comment, editor, window, cx);
            }
        }
    }

    /// `C`: comments on the file at the cursor.
    pub(crate) fn add_file_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let diff = self.diff.read(cx);
        let Some(path) = diff
            .path_at_cursor(cx)
            .and_then(|path_key| diff.repo_path_for(&path_key))
        else {
            return;
        };
        let editor = self.focused_diff_editor(cx);
        collapse_selection(&editor, window, cx);
        self.add_file_comment_on(path, editor, window, cx);
    }

    fn add_file_comment_on(
        &mut self,
        path: git::repository::RepoPath,
        return_to: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A reviewed file is folded to its header, which would hide the comment.
        if let Some(buffer_id) = self
            .diff
            .read(cx)
            .buffer_id_for(&review_path_key(&path), cx)
        {
            self.rhs_editor(cx).update(cx, |editor, cx| {
                if editor.is_buffer_folded(buffer_id, cx) {
                    editor.unfold_buffer(buffer_id, cx);
                }
            });
        }
        let new_comment = NewComment {
            target: CommentTarget::File { path },
            line_text: None,
            anchor: None,
        };
        self.add_comment(new_comment, return_to, window, cx);
    }

    /// `;c`: adds a comment on the whole review, at the top of the tab.
    pub(crate) fn add_review_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.focused_diff_editor(cx);
        collapse_selection(&editor, window, cx);
        let new_comment = NewComment {
            target: CommentTarget::Review,
            line_text: None,
            anchor: None,
        };
        self.add_comment(new_comment, editor, window, cx);
    }

    fn add_comment(
        &mut self,
        new_comment: NewComment,
        return_to: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self
            .session
            .update(cx, |session, cx| session.add_comment(new_comment, cx));
        // The session's event comes later; the editor opens now.
        self.ensure_view(id, cx);
        self.reconcile(window, cx);
        self.begin_edit(id, true, Some(return_to), window, cx);
        cx.notify();
    }

    /// `i` / `Enter`: edits the comment below the cursor line. Returns whether there is one.
    pub(crate) fn edit_comment_at_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.comment_at_cursor(cx) else {
            return false;
        };
        let return_to = self.focused_diff_editor(cx);
        self.begin_edit(id, false, Some(return_to), window, cx);
        true
    }

    /// Edits a comment (its edit button).
    pub(crate) fn edit(&mut self, id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_edit(id, false, None, window, cx);
    }

    fn begin_edit(
        &mut self,
        id: CommentId,
        is_new: bool,
        return_to: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.views.get(&id).cloned() else {
            return;
        };
        if let Some(editor) = view.read(cx).editor() {
            window.focus(&editor.focus_handle(cx), cx);
            self.reveal(id, cx);
            return;
        }
        let Some(comment) = self.session.read(cx).comments().get(id) else {
            return;
        };
        if matches!(comment.state, CommentState::Submitted { .. }) {
            self.show_toast("This comment was submitted and can't be changed", cx);
            return;
        }
        let body = comment.body.clone();
        let return_to = return_to
            .unwrap_or_else(|| self.focused_diff_editor(cx))
            .downgrade();
        let editor = cx.new(|cx| {
            // Created with its text, so that undo can't empty it.
            let buffer = cx.new(|cx| Buffer::local(body, cx));
            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let mut editor = Editor::new(
                EditorMode::AutoHeight {
                    min_lines: 1,
                    max_lines: None,
                },
                buffer,
                None,
                window,
                cx,
            );
            // Vim attaches to editors right after they are created, and only to those that use
            // modal editing by then.
            editor.set_use_modal_editing(true);
            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
            editor.set_placeholder_text("Leave a comment…", window, cx);
            editor
        });
        let subscription = cx.subscribe_in(
            &editor,
            window,
            move |this, editor, event: &EditorEvent, window, cx| {
                if matches!(event, EditorEvent::Focused) {
                    this.comment_editor_focused(id, editor, window, cx);
                }
            },
        );
        view.update(cx, |view, cx| {
            view.start_editing(
                EditingState {
                    editor: editor.clone(),
                    return_to,
                    is_new,
                    enter_insert_mode: true,
                    _subscription: subscription,
                },
                cx,
            )
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.reveal(id, cx);
        cx.emit(CommentBlocksEvent::EditingChanged);
        cx.notify();
    }

    fn comment_editor_focused(
        &mut self,
        id: CommentId,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.views.get(&id) else {
            return;
        };
        if view.update(cx, |view, _| view.take_enter_insert_mode())
            && let Some(action) = cx.build_action("vim::SwitchToInsertMode", None).log_err()
        {
            editor
                .focus_handle(cx)
                .dispatch_action(&*action, window, cx);
        }
    }

    /// Scrolls the diff so that the comment's block shows.
    fn reveal(&self, id: CommentId, cx: &mut Context<Self>) {
        let Some(placement) = self.hosted.get(&id).map(|hosted| hosted.placement) else {
            return;
        };
        self.rhs_editor(cx).update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let autoscroll = match placement.block {
                BlockAnchor::Below(anchor) => {
                    let row_below = anchor.to_point(&snapshot).row + 1;
                    if row_below > snapshot.max_point().row {
                        Autoscroll::center().for_anchor(anchor)
                    } else {
                        Autoscroll::fit()
                            .for_anchor(snapshot.anchor_before(Point::new(row_below, 0)))
                    }
                }
                // The file header is sticky, and would cover a block fitted to the top.
                BlockAnchor::Above(anchor) if placement.is_file => {
                    Autoscroll::center().for_anchor(anchor)
                }
                BlockAnchor::Above(anchor) => Autoscroll::fit().for_anchor(anchor),
            };
            editor.request_autoscroll(autoscroll, cx);
        });
    }

    /// Closes the comment's editor, saving or discarding its text, and focuses the diff again.
    pub(crate) fn leave(
        &mut self,
        id: CommentId,
        leave: Leave,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.views.get(&id).cloned() else {
            return;
        };
        let Some(editing) = view.update(cx, |view, cx| view.take_editing(cx)) else {
            return;
        };
        let text = editing.editor.read(cx).text(cx);
        save_text(&self.session, id, editing.is_new, &text, leave, cx);
        self.return_focus(editing.return_to.upgrade(), window, cx);
        cx.emit(CommentBlocksEvent::EditingChanged);
        cx.notify();
    }

    /// `:w` on the review tab: leaves the focused comment editor, keeping its text.
    pub(crate) fn save_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.focused_comment(window, cx) {
            self.leave(id, Leave::Keep, window, cx);
        }
    }

    /// Keeps the text of every open comment editor, which closes.
    fn save_open_editors(&mut self, cx: &mut App) {
        let mut closed = false;
        for (id, view) in &self.views {
            let Some(editing) = view.update(cx, |view, cx| view.take_editing(cx)) else {
                continue;
            };
            let text = editing.editor.read(cx).text(cx);
            save_text(&self.session, *id, editing.is_new, &text, Leave::Keep, cx);
            closed = true;
        }
        if closed {
            log::debug!("saved the open comment editors of a review tab");
        }
    }

    /// Focuses the diff editor a comment was opened from, with an empty selection, so that vim
    /// is in normal mode there.
    fn return_focus(
        &mut self,
        return_to: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = return_to.unwrap_or_else(|| self.focused_diff_editor(cx));
        collapse_selection(&editor, window, cx);
        self.focus_diff_editor(&editor, window, cx);
    }

    fn focus_diff_editor(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus_handle = editor.focus_handle(cx);
        if focus_handle.is_focused(window) {
            return;
        }
        window.focus(&focus_handle, cx);
        // When a comment editor lost the focus to a modal (the command palette for `:w`) or to
        // the other side of the diff, the diff editor remembers it as its last focused
        // descendant and focuses it again as soon as it gets the focus itself. This listener
        // runs after the editor's own and takes the focus back.
        self.focus_guard = Some(cx.on_focus(&focus_handle, window, {
            let focus_handle = focus_handle.clone();
            move |this, window, cx| {
                this.focus_guard = None;
                window.focus(&focus_handle, cx);
            }
        }));
    }

    pub(crate) fn cycle_type(&mut self, id: CommentId, forward: bool, cx: &mut Context<Self>) {
        let Some(current) = self
            .session
            .read(cx)
            .comments()
            .get(id)
            .map(|comment| comment.comment_type)
        else {
            return;
        };
        let comment_type = CommentType::cycle(current, forward);
        self.session.update(cx, |session, cx| {
            session.set_comment_type(id, comment_type, cx)
        });
    }

    /// `dd`: deletes the comment below the cursor line, after asking.
    pub(crate) fn delete_comment_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.comment_at_cursor(cx) {
            Some(id) => self.confirm_delete(id, window, cx),
            None => self.show_toast("No comment at the cursor", cx),
        }
    }

    pub(crate) fn confirm_delete(
        &mut self,
        id: CommentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(comment) = self.session.read(cx).comments().get(id) else {
            return;
        };
        if matches!(comment.state, CommentState::Submitted { .. }) {
            self.show_toast("This comment was submitted and can't be deleted", cx);
            return;
        }
        let first_line = comment
            .body
            .lines()
            .next()
            .map(|line| truncate_and_trailoff(line, 80));
        let answer = window.prompt(
            PromptLevel::Warning,
            "Delete this comment?",
            first_line.as_deref(),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() == Some(0) {
                this.update(cx, |this, cx| {
                    this.session
                        .update(cx, |session, cx| session.delete_comment(id, cx))
                })
                .log_err();
            }
        })
        .detach();
    }

    /// `]t` / `[t`: moves to the next or previous line or file comment, wrapping around.
    /// Review-level comments aren't in the diff.
    pub(crate) fn go_to_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.go_to_comment(Direction::Next, window, cx);
    }

    pub(crate) fn go_to_previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.go_to_comment(Direction::Previous, window, cx);
    }

    fn go_to_comment(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.comment_order(cx);
        if items.is_empty() {
            self.show_toast("No comments", cx);
            return;
        }
        let order = self.diff.read(cx).file_order();
        let current = self
            .comments_at_cursor(cx)
            .and_then(|(_, at_cursor)| self.selected.filter(|id| at_cursor.contains(id)))
            .and_then(|id| items.iter().find(|item| item.2 == id).copied());
        let target = match current {
            Some(current) => match direction {
                Direction::Next => items.iter().find(|item| **item > current),
                Direction::Previous => items.iter().rev().find(|item| **item < current),
            },
            None => {
                // A comment sits below its row, so the comment on the cursor row is the next one.
                let position = self.cursor_position(cx).and_then(|(path_key, row)| {
                    let file_index = order.iter().position(|key| key == &path_key)?;
                    Some((file_index, i64::from(row)))
                });
                match (direction, position) {
                    (Direction::Next, Some(position)) => items
                        .iter()
                        .find(|(file_index, row, _)| (*file_index, *row) >= position),
                    (Direction::Previous, Some(position)) => items
                        .iter()
                        .rev()
                        .find(|(file_index, row, _)| (*file_index, *row) < position),
                    (_, None) => None,
                }
            }
        };
        let target = match direction {
            Direction::Next => target.or(items.first()),
            Direction::Previous => target.or(items.last()),
        };
        if let Some((_, _, id)) = target.copied() {
            self.land_on(id, window, cx);
        }
    }

    /// The line and file comments in the order of the diff: by file, file comments first, then
    /// by line. Comments in files the filters hide, and comments on lines the diff doesn't show,
    /// are left out.
    fn comment_order(&self, cx: &App) -> Vec<(usize, i64, CommentId)> {
        let diff = self.diff.read(cx);
        let order = diff.file_order();
        let snapshot = self.rhs_editor(cx).read(cx).buffer().read(cx).snapshot(cx);
        let mut items = self
            .session
            .read(cx)
            .comments()
            .iter()
            .filter_map(|comment| {
                let path_key = review_path_key(comment.target.path()?);
                let file_index = order.iter().position(|key| key == &path_key)?;
                let row = match &comment.target {
                    CommentTarget::File { .. } => -1,
                    CommentTarget::Lines {
                        side: Side::New, ..
                    } => {
                        let anchor = comment.anchor.as_ref()?;
                        // Files hidden by single-file mode have no excerpts to check.
                        if diff.is_shown(&path_key) {
                            snapshot.anchor_in_excerpt(anchor.end)?;
                        }
                        let buffer = diff.display_buffer(&path_key)?.read(cx);
                        i64::from(anchor.end.to_point(&buffer.snapshot()).row)
                    }
                    CommentTarget::Lines {
                        side: Side::Old, ..
                    }
                    | CommentTarget::Review => return None,
                };
                Some((file_index, row, comment.id))
            })
            .collect::<Vec<_>>();
        items.sort();
        items
    }

    /// Moves the cursor to the row the comment's block is attached to, showing its file first.
    fn land_on(&mut self, id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path_key) = self
            .session
            .read(cx)
            .comments()
            .get(id)
            .and_then(|comment| comment.target.path())
            .map(review_path_key)
        else {
            return;
        };
        self.show_file(&path_key, window, cx);
        self.reconcile(window, cx);
        let Some(anchor) = self.hosted.get(&id).map(|hosted| hosted.placement.row) else {
            return;
        };
        let rhs = self.rhs_editor(cx);
        rhs.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let point = Point::new(anchor.to_point(&snapshot).row, 0);
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_ranges([point..point]),
            );
        });
        self.focus_diff_editor(&rhs, window, cx);
        self.selected = Some(id);
        self.update_targeted(cx);
    }

    /// Single-file mode switches to the file, and a reviewed file opens.
    fn show_file(&mut self, path_key: &PathKey, window: &mut Window, cx: &mut Context<Self>) {
        self.diff.update(cx, |diff, cx| {
            if diff
                .single_file()
                .is_some_and(|single_file| single_file != path_key)
            {
                diff.move_to_path(path_key.clone(), window, cx);
            }
        });
        if let Some(buffer_id) = self.diff.read(cx).buffer_id_for(path_key, cx) {
            self.rhs_editor(cx).update(cx, |editor, cx| {
                if editor.is_buffer_folded(buffer_id, cx) {
                    editor.unfold_buffer(buffer_id, cx);
                }
            });
        }
    }

    /// The comment `i`, `Enter` and `dd` act on: the one `]t`/`[t` moved to, else the first line
    /// comment below the cursor row, else the file comment above it.
    fn comment_at_cursor(&self, cx: &mut App) -> Option<CommentId> {
        let (first, at_cursor) = self.comments_at_cursor(cx)?;
        Some(
            self.selected
                .filter(|id| at_cursor.contains(id))
                .unwrap_or(first),
        )
    }

    /// The comments shown at the cursor row: the first one `i` would act on, and all of them.
    fn comments_at_cursor(&self, cx: &mut App) -> Option<(CommentId, Vec<CommentId>)> {
        let row = self.rhs_cursor_row(cx)?;
        let snapshot = self.rhs_editor(cx).read(cx).buffer().read(cx).snapshot(cx);
        let mut at_cursor = self
            .hosted
            .iter()
            .filter(|(_, hosted)| hosted.placement.row.to_point(&snapshot).row == row)
            .map(|(id, hosted)| (hosted.placement.is_file, *id))
            .collect::<Vec<_>>();
        at_cursor.sort();
        let (_, first) = *at_cursor.first()?;
        Some((first, at_cursor.into_iter().map(|(_, id)| id).collect()))
    }

    /// The cursor's row on the right side of the diff. The left side's rows face the right
    /// side's (spacers balance them), except where they face a spacer.
    fn rhs_cursor_row(&self, cx: &mut App) -> Option<u32> {
        let rhs = self.rhs_editor(cx);
        let focused = self.focused_diff_editor(cx);
        if focused == rhs {
            let editor = rhs.read(cx);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            return Some(
                editor
                    .selections
                    .newest_anchor()
                    .head()
                    .to_point(&snapshot)
                    .row,
            );
        }
        let display_row = focused.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let head = editor
                .selections
                .newest_anchor()
                .head()
                .to_point(snapshot.buffer_snapshot());
            snapshot.point_to_display_point(head, Bias::Left).row()
        });
        rhs.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            if display_row > snapshot.max_point().row() || snapshot.is_block_line(display_row) {
                return None;
            }
            Some(
                snapshot
                    .display_point_to_point(DisplayPoint::new(display_row, 0), Bias::Left)
                    .row,
            )
        })
    }

    /// The file at the cursor and the cursor's row in its buffer (the row after them on removed
    /// lines).
    fn cursor_position(&self, cx: &mut App) -> Option<(PathKey, u32)> {
        let row = self.rhs_cursor_row(cx)?;
        let rhs = self.rhs_editor(cx);
        let snapshot = rhs.read(cx).buffer().read(cx).snapshot(cx);
        let anchor = snapshot.anchor_before(Point::new(row, 0));
        let (text_anchor, buffer) = snapshot.anchor_to_buffer_anchor(anchor)?;
        let path_key = snapshot.path_for_buffer(text_anchor.buffer_id)?.clone();
        Some((path_key, text_anchor.to_point(buffer).row))
    }

    fn update_targeted(&mut self, cx: &mut Context<Self>) {
        let targeted = self.comment_at_cursor(cx);
        if targeted == self.targeted {
            return;
        }
        for (id, targeted) in [(self.targeted, false), (targeted, true)] {
            if let Some(view) = id.and_then(|id| self.views.get(&id)) {
                view.update(cx, |view, cx| view.set_targeted(targeted, cx));
            }
        }
        self.targeted = targeted;
    }

    fn show_toast(&self, message: &'static str, cx: &mut App) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<Self>(), message).autohide(),
                    cx,
                )
            })
            .log_err();
    }

    #[cfg(test)]
    pub(crate) fn view(&self, id: CommentId) -> Option<&Entity<CommentView>> {
        self.views.get(&id)
    }

    /// Each hosted comment's block row on the right side.
    #[cfg(test)]
    pub(crate) fn hosted_rows(&self, cx: &mut App) -> Vec<(CommentId, u32)> {
        let rhs = self.rhs_editor(cx);
        let mut rows = self
            .hosted
            .iter()
            .map(|(id, hosted)| {
                let block = hosted.block;
                let row = rhs.update(cx, |editor, cx| editor.row_for_block(block, cx));
                (*id, row.map_or(u32::MAX, |row| row.0))
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    }
}

/// Saves a left editor's text: trimmed, and only when not empty. A new comment that is
/// discarded, or left empty, is deleted.
fn save_text(
    session: &Entity<ReviewSession>,
    id: CommentId,
    is_new: bool,
    text: &str,
    leave: Leave,
    cx: &mut App,
) {
    let is_empty = text.trim().is_empty();
    session.update(cx, |session, cx| match leave {
        Leave::Discard if is_new => session.delete_comment(id, cx),
        Leave::Discard => {}
        Leave::Keep if is_empty && is_new => session.delete_comment(id, cx),
        // Emptying a saved comment doesn't delete it: `dd` does.
        Leave::Keep if is_empty => {}
        Leave::Keep => session.set_comment_body(id, text, cx),
    });
}

/// Where the comment's block goes in the right side of the diff, if its lines (or file) are
/// shown there.
fn placement(comment: &Comment, editor: &Editor, cx: &App) -> Option<Placement> {
    let multibuffer = editor.buffer().read(cx);
    let snapshot = multibuffer.snapshot(cx);
    match &comment.target {
        CommentTarget::Review => None,
        CommentTarget::Lines {
            side: Side::New, ..
        } => {
            let anchor = comment.anchor.as_ref()?;
            if editor.is_buffer_folded(anchor.end.buffer_id, cx) {
                return None;
            }
            let row = snapshot.anchor_in_excerpt(anchor.end)?;
            Some(Placement {
                row,
                is_file: false,
                block: block_below_row(row, &snapshot),
            })
        }
        CommentTarget::Lines {
            side: Side::Old, ..
        } => None,
        CommentTarget::File { path } => {
            let anchor = multibuffer.location_for_path(&review_path_key(path), cx)?;
            let (text_anchor, _) = snapshot.anchor_to_buffer_anchor(anchor)?;
            if editor.is_buffer_folded(text_anchor.buffer_id, cx) {
                return None;
            }
            Some(Placement {
                row: anchor,
                is_file: true,
                block: BlockAnchor::Above(anchor),
            })
        }
    }
}

/// Where a block goes to sit right below the row `row`: below it, or above the next row of the
/// same excerpt, which looks the same. A split diff puts a spacer facing the block on the other
/// side, and mirrors a block below an unchanged row as a spacer above the facing row, which
/// would misalign the rows; a block above the next unchanged row (or the next hunk) is mirrored
/// above the row facing that one. Changed rows keep their block below them: the other side
/// mirrors it below the start of the hunk.
fn block_below_row(row: Anchor, snapshot: &MultiBufferSnapshot) -> BlockAnchor {
    let point = row.to_point(snapshot);
    let mut row_infos = snapshot.row_infos(MultiBufferRow(point.row));
    if let (Some(current), Some(next)) = (row_infos.next(), row_infos.next())
        && current.diff_status.is_none()
        && next.buffer_id == current.buffer_id
        && next.buffer_row.is_some()
        && next.buffer_row == current.buffer_row.map(|row| row + 1)
    {
        return BlockAnchor::Above(snapshot.anchor_before(Point::new(point.row + 1, 0)));
    }
    BlockAnchor::Below(row)
}

fn block_properties(
    id: CommentId,
    placement: Placement,
    view: Entity<CommentView>,
) -> BlockProperties<Anchor> {
    BlockProperties {
        placement: match placement.block {
            BlockAnchor::Above(anchor) => BlockPlacement::Above(anchor),
            BlockAnchor::Below(anchor) => BlockPlacement::Below(anchor),
        },
        // Blocks take the height of their content once rendered.
        height: Some(3),
        // Not moved by the diff's horizontal scroll.
        style: BlockStyle::Sticky,
        priority: usize::try_from(id.0).unwrap_or(usize::MAX),
        render: Arc::new(move |cx: &mut BlockContext| {
            // Opaque, so that a line cursor highlight reaching past an empty row doesn't show
            // through.
            div()
                .w_full()
                .pl(cx.margins.gutter.full_width())
                .pr_4()
                .py_1()
                .bg(cx.theme().colors().editor_background)
                .block_mouse_except_scroll()
                .child(view.clone())
                .into_any_element()
        }),
    }
}

/// The lines `c` comments on, on the side of the diff `editor` shows. A selection over the
/// removed and added lines of a unified diff comments on the added ones.
fn selected_lines(
    editor: &mut Editor,
    is_lhs: bool,
    cx: &mut Context<Editor>,
) -> Option<SelectedLines> {
    let display_snapshot = editor.display_snapshot(cx);
    let snapshot = display_snapshot.buffer_snapshot();
    // The anchor, not the resolved selection: on a folded header that spans the whole file.
    let head = editor.selections.newest_anchor().head();
    let (head_anchor, _) = snapshot.anchor_to_buffer_anchor(head)?;
    if editor.is_buffer_folded(head_anchor.buffer_id, cx) {
        return Some(SelectedLines::FoldedFile(head_anchor.buffer_id));
    }
    let rows = selected_rows(editor, &display_snapshot);
    let row_infos = snapshot
        .row_infos(MultiBufferRow(*rows.start()))
        .take(rows.clone().count())
        .collect::<Vec<_>>();
    let last = row_infos.last()?;
    let is_removed = |row_info: &RowInfo| {
        row_info
            .diff_status
            .is_some_and(|status| status.kind == DiffHunkStatusKind::Deleted)
    };
    let rows_of = |buffer_id: BufferId, removed: bool| {
        let rows = row_infos
            .iter()
            .filter(|row_info| {
                row_info.buffer_id == Some(buffer_id) && (is_lhs || is_removed(row_info) == removed)
            })
            .filter_map(|row_info| row_info.buffer_row)
            .collect::<Vec<_>>();
        Some(*rows.iter().min()?..=*rows.iter().max()?)
    };
    // The left side shows the old file; on the right side, removed lines show the old file's
    // rows (and buffer).
    if !is_lhs {
        let (anchor, _) =
            snapshot.anchor_to_buffer_anchor(snapshot.anchor_before(Point::new(*rows.end(), 0)))?;
        if let Some(rows) = rows_of(anchor.buffer_id, false) {
            return Some(SelectedLines::Lines {
                side: Side::New,
                buffer_id: anchor.buffer_id,
                rows,
            });
        }
    }
    let buffer_id = last.buffer_id?;
    Some(SelectedLines::Lines {
        side: Side::Old,
        buffer_id,
        rows: rows_of(buffer_id, true)?,
    })
}

/// Collapses the selection onto the cursor row's start, which takes vim out of visual mode.
fn collapse_selection(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let head = editor.selections.newest_anchor().head().to_point(&snapshot);
        let point = Point::new(head.row, 0);
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([point..point])
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ReviewView,
        test_support::{
            cursor_line, cursor_point, init_vim_test, lhs_editor, move_to_row_with_text,
            open_review_fixture, path_at_cursor, rhs_editor, splittable,
        },
    };
    use git::repository::RepoPath;
    use gpui::{TestAppContext, VisualTestContext};
    use settings::DiffViewStyle;
    use workspace::item::Item as _;

    fn init(cx: &mut TestAppContext, diff_view_style: DiffViewStyle) {
        init_vim_test(cx, diff_view_style);
        cx.update(command_palette::init);
    }

    fn blocks(review: &Entity<ReviewView>, cx: &VisualTestContext) -> Entity<CommentBlocks> {
        review.read_with(cx, |review, _| review.comments().clone())
    }

    /// Each comment's target, body and type, in creation order.
    fn comments(
        review: &Entity<ReviewView>,
        cx: &VisualTestContext,
    ) -> Vec<(CommentTarget, String, Option<CommentType>)> {
        review.read_with(cx, |review, cx| {
            review
                .session()
                .read(cx)
                .comments()
                .iter()
                .map(|comment| {
                    (
                        comment.target.clone(),
                        comment.body.clone(),
                        comment.comment_type,
                    )
                })
                .collect()
        })
    }

    fn bodies(review: &Entity<ReviewView>, cx: &VisualTestContext) -> Vec<String> {
        comments(review, cx)
            .into_iter()
            .map(|(_, body, _)| body)
            .collect()
    }

    fn comment_ids(review: &Entity<ReviewView>, cx: &VisualTestContext) -> Vec<CommentId> {
        review.read_with(cx, |review, cx| {
            review
                .session()
                .read(cx)
                .comments()
                .iter()
                .map(|comment| comment.id)
                .collect()
        })
    }

    fn lines(path: &str, lines: RangeInclusive<u32>) -> CommentTarget {
        CommentTarget::Lines {
            path: RepoPath::new(path).unwrap(),
            side: Side::New,
            lines,
        }
    }

    /// The open comment editor, if any.
    fn open_editor(review: &Entity<ReviewView>, cx: &VisualTestContext) -> Option<Entity<Editor>> {
        blocks(review, cx).read_with(cx, |blocks, cx| {
            blocks
                .views
                .values()
                .find_map(|view| view.read(cx).editor().cloned())
        })
    }

    fn is_focused(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| editor.focus_handle(cx).is_focused(window))
    }

    fn vim_mode(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Option<String> {
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor
                    .key_context(window, cx)
                    .get("vim_mode")
                    .map(|mode| mode.to_string())
            })
        })
    }

    fn text(editor: &Entity<Editor>, cx: &VisualTestContext) -> String {
        editor.read_with(cx, |editor, cx| editor.text(cx))
    }

    fn hosted_rows(
        review: &Entity<ReviewView>,
        cx: &mut VisualTestContext,
    ) -> Vec<(CommentId, u32)> {
        let blocks = blocks(review, cx);
        cx.update(|_, cx| blocks.update(cx, |blocks, cx| blocks.hosted_rows(cx)))
    }

    /// The display row of `editor`'s row `row`.
    fn display_row(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) -> u32 {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            snapshot
                .point_to_display_point(Point::new(row, 0), Bias::Left)
                .row()
                .0
        })
    }

    /// The diff editor is focused, in normal mode, with an empty selection.
    #[track_caller]
    fn assert_back_in_diff(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
        assert!(is_focused(editor, cx), "the diff isn't focused");
        assert_eq!(vim_mode(editor, cx).as_deref(), Some("normal"));
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert!(editor.selections.newest::<Point>(&snapshot).is_empty());
            assert!(!editor.selections.line_mode());
        });
    }

    #[gpui::test]
    async fn test_line_comment_opens_in_insert_mode(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        let row = move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        let editor = open_editor(&review, cx).unwrap();
        assert!(is_focused(&editor, cx));
        assert_eq!(vim_mode(&editor, cx).as_deref(), Some("insert"));
        // The block sits right below the commented line.
        let id = comment_ids(&review, cx)[0];
        assert_eq!(
            hosted_rows(&review, cx),
            [(id, display_row(&rhs, row, cx) + 1)]
        );

        cx.simulate_keystrokes("h i");
        assert_eq!(text(&editor, cx), "hi");
        cx.simulate_keystrokes("escape");
        assert!(is_focused(&editor, cx));
        assert_eq!(vim_mode(&editor, cx).as_deref(), Some("normal"));
        cx.simulate_keystrokes("escape");
        assert!(open_editor(&review, cx).is_none());
        assert_back_in_diff(&rhs, cx);
        assert_eq!(
            comments(&review, cx),
            [(lines("a.rs", 3..=3), "hi".to_string(), None)]
        );
        // The comment renders as markdown and stays below its line.
        blocks(&review, cx).read_with(cx, |blocks, cx| {
            let view = blocks.view(id).unwrap().read(cx);
            assert!(view.editing().is_none());
            assert!(view.is_targeted());
        });
        assert_eq!(
            hosted_rows(&review, cx),
            [(id, display_row(&rhs, row, cx) + 1)]
        );

        cx.simulate_keystrokes("j");
        assert_eq!(cursor_line(&rhs, cx), "line 4");
        assert_eq!(cursor_point(&rhs, cx).row, row + 1);
    }

    #[gpui::test]
    async fn test_leave_keys_keep_text(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        let leave_keys = [
            "ctrl-enter",
            "ctrl-s",
            "escape ctrl-enter",
            "escape : w enter",
            "escape : w q enter",
            "escape : x enter",
            "escape shift-z shift-z",
            "escape : q enter",
            "escape ctrl-w q",
            "escape escape",
        ];
        for (index, keys) in leave_keys.iter().enumerate() {
            move_to_row_with_text(&rhs, "line 3", cx);
            cx.simulate_keystrokes("c");
            cx.simulate_input(&format!("note {index}"));
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            assert!(open_editor(&review, cx).is_none(), "{keys}");
            assert_back_in_diff(&rhs, cx);
            assert_eq!(
                bodies(&review, cx).last().map(String::as_str),
                Some(format!("note {index}").as_str()),
                "{keys}"
            );
            cx.simulate_keystrokes("j");
            assert_eq!(cursor_line(&rhs, cx), "line 4", "{keys}");
        }
        assert_eq!(bodies(&review, cx).len(), leave_keys.len());
        // The review tab stayed open.
        assert!(review.read_with(cx, |review, cx| !review.is_dirty(cx)));
    }

    #[gpui::test]
    async fn test_force_quit_discards(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        // A new comment is deleted.
        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("gone");
        cx.simulate_keystrokes("escape : q ! enter");
        cx.run_until_parked();
        assert!(comments(&review, cx).is_empty());
        assert_back_in_diff(&rhs, cx);

        // So is a new one left empty.
        cx.simulate_keystrokes("c escape escape");
        assert!(comments(&review, cx).is_empty());
        assert_back_in_diff(&rhs, cx);

        // An existing comment keeps its text.
        cx.simulate_keystrokes("c");
        cx.simulate_input("kept");
        cx.simulate_keystrokes("ctrl-enter");
        for keys in [
            "escape : q ! enter",
            "escape shift-z shift-q",
            "escape d d escape",
        ] {
            cx.simulate_keystrokes("i");
            let editor = open_editor(&review, cx).unwrap();
            cx.simulate_input("changed ");
            assert_eq!(text(&editor, cx), "changed kept");
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            assert_eq!(bodies(&review, cx), ["kept"], "{keys}");
            assert_back_in_diff(&rhs, cx);
        }

        // The palette's Discard Comment Changes.
        cx.simulate_keystrokes("i");
        cx.simulate_input("changed ");
        cx.update(|window, cx| window.dispatch_action(Box::new(crate::DiscardCommentChanges), cx));
        cx.run_until_parked();
        assert_eq!(bodies(&review, cx), ["kept"]);
        assert_back_in_diff(&rhs, cx);
    }

    #[gpui::test]
    async fn test_review_item_capability(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let capability = |cx: &VisualTestContext| {
            review.read_with(cx, |review, cx| {
                (
                    review.capability(cx),
                    review.can_save(cx),
                    review.is_dirty(cx),
                )
            })
        };
        assert_eq!(
            capability(cx),
            (language::Capability::ReadOnly, false, false)
        );

        // `:w` on the diff itself does nothing.
        cx.simulate_keystrokes(": w enter");
        cx.run_until_parked();
        assert!(comments(&review, cx).is_empty());
        assert_back_in_diff(&rhs, cx);

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("x");
        assert_eq!(
            capability(cx),
            (language::Capability::ReadWrite, true, false)
        );
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            capability(cx),
            (language::Capability::ReadOnly, false, false)
        );
    }

    #[gpui::test]
    async fn test_range_comment_from_visual(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        let row = move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("v j c");
        // The diff left visual mode, and the cursor is on the last line.
        assert_eq!(cursor_point(&rhs, cx), Point::new(row + 1, 0));
        rhs.read_with(cx, |editor, _| assert!(!editor.selections.line_mode()));
        cx.simulate_input("range");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&rhs, cx);
        let id = comment_ids(&review, cx)[0];
        assert_eq!(
            comments(&review, cx),
            [(lines("a.rs", 3..=4), "range".to_string(), None)]
        );
        assert_eq!(
            hosted_rows(&review, cx),
            [(id, display_row(&rhs, row + 1, cx) + 1)]
        );

        // Over removed and added lines, the added lines count.
        move_to_row_with_text(&rhs, "line 1", cx);
        cx.simulate_keystrokes("shift-v j j c");
        cx.simulate_input("mixed");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            comments(&review, cx)[1],
            (lines("a.rs", 1..=2), "mixed".to_string(), None)
        );

        // Removed lines alone can't be commented on yet.
        move_to_row_with_text(&rhs, "gone 1", cx);
        cx.simulate_keystrokes("c");
        assert!(open_editor(&review, cx).is_none());
        assert_eq!(comments(&review, cx).len(), 2);
        assert_back_in_diff(&rhs, cx);
    }

    #[gpui::test]
    async fn test_file_and_review_comments(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        move_to_row_with_text(&rhs, "line 36", cx);
        cx.simulate_keystrokes("shift-c");
        let editor = open_editor(&review, cx).unwrap();
        assert!(is_focused(&editor, cx));
        assert_eq!(vim_mode(&editor, cx).as_deref(), Some("insert"));
        cx.simulate_input("file note");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&rhs, cx);
        assert_eq!(cursor_line(&rhs, cx), "line 36");
        // The block sits above the file's first line, below its header.
        let file_comment = comment_ids(&review, cx)[0];
        let first_row = display_row(&rhs, 0, cx);
        let block_row = hosted_rows(&review, cx)[0].1;
        assert!(block_row < first_row, "{block_row} {first_row}");
        assert_eq!(
            comments(&review, cx),
            [(
                CommentTarget::File {
                    path: RepoPath::new("a.rs").unwrap()
                },
                "file note".to_string(),
                None
            )]
        );

        cx.simulate_keystrokes("; c");
        let editor = open_editor(&review, cx).unwrap();
        assert!(is_focused(&editor, cx));
        assert_eq!(vim_mode(&editor, cx).as_deref(), Some("insert"));
        let top_block_comments =
            blocks(&review, cx).read_with(cx, |blocks, cx| blocks.review_comment_views(cx).len());
        assert_eq!(top_block_comments, 1);
        cx.simulate_input("overall");
        cx.simulate_keystrokes("escape escape");
        assert_back_in_diff(&rhs, cx);
        assert_eq!(
            comments(&review, cx)[1],
            (CommentTarget::Review, "overall".to_string(), None)
        );
        // Review-level comments have no block in the diff.
        assert_eq!(
            hosted_rows(&review, cx)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            [file_comment]
        );

        // The file comment is the one at the file's first line.
        move_to_row_with_text(&rhs, "line 1", cx);
        cx.simulate_keystrokes("i");
        let editor = open_editor(&review, cx).unwrap();
        assert_eq!(text(&editor, cx), "file note");
        cx.simulate_keystrokes("escape escape");
    }

    #[gpui::test]
    async fn test_comment_types_cycle(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let comment_type = |cx: &VisualTestContext| comments(&review, cx)[0].2;

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("typed");
        cx.simulate_keystrokes("escape");
        for expected in [
            Some(CommentType::Issue),
            Some(CommentType::Suggestion),
            Some(CommentType::Note),
            Some(CommentType::Praise),
            None,
        ] {
            cx.simulate_keystrokes("tab");
            assert_eq!(comment_type(cx), expected);
        }
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(comment_type(cx), Some(CommentType::Praise));

        // In insert mode, `tab` is text.
        let editor = open_editor(&review, cx).unwrap();
        cx.simulate_keystrokes("shift-a tab");
        assert_ne!(text(&editor, cx), "typed");
        assert!(text(&editor, cx).starts_with("typed"));
        assert_eq!(comment_type(cx), Some(CommentType::Praise));
        // Saving trims the text.
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            comments(&review, cx),
            [(
                lines("a.rs", 3..=3),
                "typed".to_string(),
                Some(CommentType::Praise)
            )]
        );

        // The type changes at once, so discarding the text keeps it.
        cx.simulate_keystrokes("i");
        cx.simulate_input("more ");
        cx.simulate_keystrokes("escape tab tab : q ! enter");
        cx.run_until_parked();
        assert_eq!(
            comments(&review, cx),
            [(
                lines("a.rs", 3..=3),
                "typed".to_string(),
                Some(CommentType::Issue)
            )]
        );
    }

    #[gpui::test]
    async fn test_edit_and_delete_at_cursor(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("first");
        cx.simulate_keystrokes("ctrl-enter");

        // `i` edits with the cursor at the start, in insert mode.
        cx.simulate_keystrokes("i");
        let editor = open_editor(&review, cx).unwrap();
        assert_eq!(vim_mode(&editor, cx).as_deref(), Some("insert"));
        cx.simulate_input("A ");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(bodies(&review, cx), ["A first"]);

        cx.simulate_keystrokes("enter");
        assert!(open_editor(&review, cx).is_some());
        cx.simulate_keystrokes("escape escape");
        assert_back_in_diff(&rhs, cx);

        // Without a comment, `enter` moves down and `i` does nothing.
        cx.simulate_keystrokes("j enter");
        assert_eq!(cursor_line(&rhs, cx), "line 5");
        cx.simulate_keystrokes("i");
        assert!(open_editor(&review, cx).is_none());
        assert_back_in_diff(&rhs, cx);
        cx.simulate_keystrokes("d d");
        assert!(!cx.has_pending_prompt());

        // `dd` asks first.
        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("d d");
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(bodies(&review, cx), ["A first"]);
        cx.simulate_keystrokes("d d");
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(comments(&review, cx).is_empty());
        assert!(hosted_rows(&review, cx).is_empty());
        assert_eq!(cursor_line(&rhs, cx), "line 3");
    }

    #[gpui::test]
    async fn test_comment_navigation(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let add = |row_text: &str, keys: &str, cx: &mut VisualTestContext| {
            move_to_row_with_text(&rhs, row_text, cx);
            cx.simulate_keystrokes(keys);
            cx.simulate_input(row_text);
            cx.simulate_keystrokes("ctrl-enter");
        };
        add("line 36", "c", cx);
        add("line 3", "c", cx);
        add("three", "c", cx);
        add("two", "shift-c", cx);
        add("line 4", "; c", cx);
        let targeted = |cx: &mut VisualTestContext| {
            let blocks = blocks(&review, cx);
            cx.update(|_, cx| {
                blocks.update(cx, |blocks, cx| {
                    let id = blocks.comment_at_cursor(cx)?;
                    let comment = blocks.session.read(cx).comments().get(id)?;
                    Some(comment.body.clone())
                })
            })
        };

        move_to_row_with_text(&rhs, "line 1", cx);
        for (keys, body, path) in [
            ("] t", "line 3", "a.rs"),
            ("] t", "line 36", "a.rs"),
            // File comments come first in their file.
            ("m", "two", "modified.txt"),
            ("] t", "three", "modified.txt"),
            // Wraps around; review-level comments aren't in the diff.
            ("] t", "line 3", "a.rs"),
            ("[ t", "three", "modified.txt"),
            ("shift-m", "two", "modified.txt"),
            ("[ t", "line 36", "a.rs"),
        ] {
            cx.simulate_keystrokes(keys);
            assert_eq!(targeted(cx).as_deref(), Some(body), "after {keys}");
            assert_eq!(
                path_at_cursor(&review, cx).as_deref(),
                Some(path),
                "after {keys}"
            );
            assert_back_in_diff(&rhs, cx);
        }
        assert_eq!(cursor_line(&rhs, cx), "line 36");

        // From a plain row, the next comment is the first one below it.
        move_to_row_with_text(&rhs, "line 4", cx);
        cx.simulate_keystrokes("] t");
        assert_eq!(targeted(cx).as_deref(), Some("line 36"));
        move_to_row_with_text(&rhs, "line 4", cx);
        cx.simulate_keystrokes("[ t");
        assert_eq!(targeted(cx).as_deref(), Some("line 3"));

        // Single-file mode switches files.
        cx.simulate_keystrokes("; f");
        cx.simulate_keystrokes("] t ] t");
        assert_eq!(targeted(cx).as_deref(), Some("two"));
        assert_eq!(
            review.read_with(cx, |review, cx| review
                .diff()
                .read(cx)
                .excerpt_file_paths(cx)),
            ["modified.txt"]
        );

        // The comment moved to isn't at the cursor once other files show above it.
        cx.simulate_keystrokes("; f g g");
        assert_eq!(targeted(cx), None);
        cx.simulate_keystrokes("j");
        assert_eq!(targeted(cx), None);
    }

    #[gpui::test]
    async fn test_typing_in_a_comment_isnt_taken_by_review_keys(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        let row = move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        let editor = open_editor(&review, cx).unwrap();
        cx.simulate_keystrokes("h l ; r c ] space i a e w b shift-c shift-r");
        assert_eq!(text(&editor, cx), "hl;rc] iaewbCR");
        cx.simulate_keystrokes("tab");
        assert!(text(&editor, cx).len() > "hl;rc] iaewbCR".len());
        assert!(is_focused(&editor, cx));
        assert_eq!(comments(&review, cx).len(), 1);
        assert_eq!(
            rhs.update(cx, |editor, cx| editor.scroll_position(cx).x),
            0.
        );

        // Normal-mode keys move within the comment.
        cx.simulate_keystrokes("escape 0 l l x");
        assert!(text(&editor, cx).starts_with("hlrc]"));
        assert!(is_focused(&editor, cx));
        assert_eq!(cursor_point(&rhs, cx).row, row);

        // Soft wrap toggles in the comment, not in the diff.
        let wraps = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |editor, cx| {
                !matches!(editor.soft_wrap_mode(cx), editor::SoftWrap::None)
            })
        };
        assert!(wraps(&editor, cx));
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(editor::actions::ToggleSoftWrap), cx)
        });
        cx.run_until_parked();
        assert!(!wraps(&rhs, cx));
        assert!(!wraps(&editor, cx));
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(editor::actions::ToggleSoftWrap), cx)
        });
        cx.run_until_parked();
        assert!(wraps(&rhs, cx));
    }

    #[gpui::test]
    async fn test_comments_in_split_view(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Split);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let lhs = lhs_editor(&review, cx);

        let row = move_to_row_with_text(&rhs, "LINE 2", cx);
        cx.simulate_keystrokes("c");
        let editor = open_editor(&review, cx).unwrap();
        cx.simulate_input("split");
        // The left side gets a spacer facing the block.
        let block_row = display_row(&rhs, row, cx) + 1;
        assert_eq!(hosted_rows(&review, cx)[0].1, block_row);
        assert!(lhs.update(cx, |editor, cx| {
            editor
                .display_snapshot(cx)
                .is_block_line(editor::display_map::DisplayRow(block_row))
        }));

        // The open editor survives the left side being recreated.
        for _ in 0..2 {
            cx.update(|window, cx| {
                editor
                    .focus_handle(cx)
                    .dispatch_action(&editor::ToggleSplitDiff, window, cx)
            });
            cx.run_until_parked();
        }
        assert_ne!(lhs_editor(&review, cx), lhs);
        assert_eq!(open_editor(&review, cx), Some(editor.clone()));
        assert_eq!(text(&editor, cx), "split");
        assert_eq!(hosted_rows(&review, cx).len(), 1);
        cx.focus(&editor);
        cx.simulate_keystrokes("escape escape");
        assert_back_in_diff(&rhs, cx);
        assert_eq!(bodies(&review, cx), ["split"]);

        // Blocks below unchanged rows keep the rows facing each other.
        for row_text in ["line 3", "line 4"] {
            move_to_row_with_text(&rhs, row_text, cx);
            cx.simulate_keystrokes("c");
            cx.simulate_input(row_text);
            cx.simulate_keystrokes("ctrl-enter");
        }
        for row_text in ["LINE 2", "line 3", "line 4", "line 5"] {
            let rhs_row = move_to_row_with_text(&rhs, row_text, cx);
            let lhs_text = if row_text == "LINE 2" {
                "line 2"
            } else {
                row_text
            };
            let lhs_row = move_to_row_with_text(&lhs_editor(&review, cx), lhs_text, cx);
            assert_eq!(
                display_row(&rhs, rhs_row, cx),
                display_row(&lhs_editor(&review, cx), lhs_row, cx),
                "{row_text}"
            );
        }
        move_to_row_with_text(&rhs, "LINE 2", cx);

        // Old lines can't be commented on yet.
        cx.simulate_keystrokes("ctrl-w h");
        let lhs = lhs_editor(&review, cx);
        assert!(splittable(&review, cx).read_with(cx, |editor, _| editor.focused_editor() == &lhs));
        cx.simulate_keystrokes("c");
        assert!(open_editor(&review, cx).is_none());
        assert_eq!(bodies(&review, cx), ["split", "line 3", "line 4"]);
        // `i` on the left side edits the comment facing the cursor row.
        move_to_row_with_text(&lhs, "line 2", cx);
        cx.simulate_keystrokes("i");
        assert!(open_editor(&review, cx).is_some());
        cx.simulate_keystrokes("escape escape");
        assert_back_in_diff(&lhs, cx);
    }

    #[gpui::test]
    async fn test_blocks_follow_hidden_and_folded_files(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("a");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(hosted_rows(&review, cx).len(), 1);

        // Single-file mode on another file hides the comment.
        cx.simulate_keystrokes("] f ; f");
        assert!(hosted_rows(&review, cx).is_empty());
        cx.simulate_keystrokes("; f");
        assert_eq!(hosted_rows(&review, cx).len(), 1);

        // So does folding its file; `C` on the folded header opens it again.
        let buffer_id = review.read_with(cx, |review, cx| {
            let diff = review.diff().read(cx);
            diff.buffer_id_for(&review_path_key(&RepoPath::new("a.rs").unwrap()), cx)
                .unwrap()
        });
        rhs.update(cx, |editor, cx| editor.fold_buffer(buffer_id, cx));
        cx.run_until_parked();
        assert!(hosted_rows(&review, cx).is_empty());
        move_to_row_with_text(&rhs, "new 1", cx);
        cx.simulate_keystrokes("[ f");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));
        cx.simulate_keystrokes("shift-c");
        assert!(!rhs.read_with(cx, |editor, cx| editor.is_buffer_folded(buffer_id, cx)));
        cx.simulate_input("b");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(hosted_rows(&review, cx).len(), 2);
    }
}
