mod gutter;

use super::{
    Comment, CommentEvent, CommentId, CommentTarget, CommentType, NewComment, Side,
    remote::{RemoteThreadContent, RemoteThreadView},
    view::{CommentView, EditingState, Leave},
};
use crate::{
    diff::{ReviewDiff, file_header_row, file_start_autoscroll, review_path_key},
    forge::LinesRefusal,
    line_cursor::selected_rows,
    session::{ReviewSession, ReviewSessionEvent},
};
use buffer_diff::DiffHunkStatusKind;
use collections::{HashMap, HashSet};
use command_palette_hooks::GlobalCommandPaletteInterceptor;
use editor::{
    Anchor, Bias, DisplayPoint, Editor, EditorEvent, EditorMode, SelectionEffects,
    display_map::{
        BlockContext, BlockId, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
        DisplayRow,
    },
    scroll::{Autoscroll, ScrollAnchor},
};
use futures::FutureExt as _;
use git::repository::RepoPath;
use gpui::{
    AnyView, DispatchPhase, Entity, EntityId, EventEmitter, Focusable as _, Global, PromptLevel,
    Subscription, WeakEntity,
};
use language::{
    Buffer, BufferId, BufferSnapshot, LanguageRegistry, Point, ToOffset as _, ToPoint as _,
    language_settings::SoftWrap,
};
use multi_buffer::{
    MultiBuffer, MultiBufferRow, MultiBufferSnapshot, PathKey, RowInfo, ToPoint as _,
};
use std::{
    any::TypeId,
    ops::{Range, RangeInclusive},
    sync::Arc,
};
use ui::prelude::*;
use util::{ResultExt as _, truncate_and_trailoff};
use workspace::{Toast, Workspace, notifications::NotificationId};

/// Refuses a comment the review tab can't take, saying why.
pub(crate) type CommentGate = Arc<dyn Fn(&CommentTarget, &App) -> Result<(), SharedString>>;

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
    /// The pull request's threads this tab shows (pull request reviews only), with their anchors
    /// in this tab's buffers.
    remote: HashMap<RemoteKey, RemoteBlock>,
    /// The blocks showing line and file comments, and threads, in the diff.
    hosted: HashMap<ItemKey, HostedBlock>,
    anchors: TabAnchors,
    /// The anchors of the sessions this tab showed before (another base or branch), by session,
    /// for when it switches back: they followed the edits made meanwhile.
    parked_anchors: HashMap<EntityId, TabAnchors>,
    /// The left side of a split diff is recreated on every split (also when the pane gets wide
    /// enough) and dropped on unsplit; its blocks go with it.
    lhs_editor_id: Option<EntityId>,
    /// The comment or thread `]t`/`[t` moved to, which tells those below the same row apart while
    /// the cursor stays on it.
    selected: Option<ItemKey>,
    targeted: Option<ItemKey>,
    /// Refuses comments a pull request review can't take (lines outside GitHub's diff, or lines
    /// this tab's checkout numbers differently). Without one, the review's own diff decides
    /// ([`Self::check`]).
    gate: Option<CommentGate>,
    focus_guard: Option<Subscription>,
    /// The block showing the review's overview at the top of the right side, with the
    /// review-level comments in it, while it has something to show.
    overview_block: Option<CustomBlockId>,
    /// A review-level comment opened before the overview got its block.
    reveal_overview_pending: bool,
    _session_subscription: Subscription,
    _subscriptions: Vec<Subscription>,
}

/// What a block of the diff shows: a comment of the review, or threads of the pull request on
/// GitHub. Threads come first where both are at the same place.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ItemKey {
    Remote(RemoteKey),
    Comment(CommentId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum RemoteKey {
    Thread(SharedString),
    /// A file's outdated and moved threads, in one block at the file.
    Moved(RepoPath),
}

/// Threads of the pull request at one place of the diff, as the review tab is told to show
/// them: one thread below its lines or at its file, or a file's outdated and moved threads at the
/// file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RemoteItem {
    pub key: RemoteKey,
    /// Lines (numbered in the checked-out files) or a file.
    pub target: CommentTarget,
    pub title: Option<SharedString>,
    pub threads: Vec<RemoteThreadContent>,
}

struct RemoteBlock {
    target: CommentTarget,
    view: Entity<RemoteThreadView>,
    /// A line thread's lines in this tab's buffer of their side, anchored when its file is first
    /// shown, so that they follow edits.
    anchor: Option<Range<language::Anchor>>,
    /// Blocks at the same place are in this order.
    order: usize,
}

/// Comments come after the threads at the same place, in creation order.
const COMMENT_BLOCK_PRIORITY: usize = 1 << 20;

fn comment_block_priority(id: CommentId) -> usize {
    usize::try_from(id.0)
        .unwrap_or(usize::MAX)
        .saturating_add(COMMENT_BLOCK_PRIORITY)
}

/// The side of the diff a block is in. Old-side comments are on the left side of a split diff
/// and on the removed rows of a unified one; everything else is on the right side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Host {
    Rhs,
    Lhs,
}

/// Where this tab's line comments are. Anchors are per tab rather than per comment: a review of
/// the same thing in another project (another window) has other buffers.
#[derive(Default)]
struct TabAnchors {
    lines: HashMap<CommentId, LineAnchor>,
    /// Comments whose lines were deleted, or whose buffer was replaced. They are anchored on
    /// their line numbers again, and flagged outdated unless those lines still have their text
    /// (an outdated one is no longer outdated when they do).
    lost: HashSet<CommentId>,
}

/// A line comment's lines in this tab's buffer of their side: the file's buffer, or its diff
/// base for old lines.
struct LineAnchor {
    range: Range<language::Anchor>,
    /// The lines (1-based) and their text as this tab last saw them. A tab only reports its own
    /// moves: tabs whose buffers differ (unsaved edits in another window) would otherwise undo
    /// each other's.
    lines: RangeInclusive<u32>,
    text: String,
    /// Where the lines of a comment that this tab flagged outdated because they were deleted
    /// used to be: the comment's text coming back there (an undo, a stash popped) makes it
    /// current again. Its anchor on the line numbers moves away from that place when the lines
    /// are put back right before it.
    deleted_from: Option<Range<language::Anchor>>,
    /// Kept open, so that a file that leaves the review and comes back (a stash and its pop in
    /// an uncommitted review) is shown with this buffer again, whose edits the range followed
    /// meanwhile, rather than with a new one, where only the line numbers would be left.
    _buffer: Entity<Buffer>,
}

struct HostedBlock {
    host: Host,
    editor: WeakEntity<Editor>,
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
    /// The cursor is on a reviewed file's folded header (`BufferId` is that side's buffer).
    FoldedFile(BufferId),
    Lines {
        side: Side,
        buffer_id: BufferId,
        /// Rows in the side's buffer.
        rows: RangeInclusive<u32>,
    },
}

/// The cursor's row on each side of the diff (a side's row is `None` when the cursor faces a
/// spacer there, or when the diff isn't split).
struct CursorRows {
    focused: Host,
    rhs: Option<u32>,
    lhs: Option<u32>,
}

impl CursorRows {
    fn row(&self, host: Host) -> Option<u32> {
        match host {
            Host::Rhs => self.rhs,
            Host::Lhs => self.lhs,
        }
    }
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
        let lhs_editor_id = splittable.read(cx).lhs_editor().map(|lhs| lhs.entity_id());
        let subscriptions = vec![
            cx.subscribe_in(
                &splittable,
                window,
                |this, _, event: &EditorEvent, window, cx| match event {
                    EditorEvent::BufferRangesUpdated { .. }
                    | EditorEvent::BuffersRemoved { .. }
                    | EditorEvent::BufferFoldToggled { .. }
                    | EditorEvent::BufferEdited
                    | EditorEvent::BuffersEdited { .. } => this.reconcile(window, cx),
                    EditorEvent::SelectionsChanged { .. } => this.update_targeted(cx),
                    _ => {}
                },
            ),
            cx.observe_in(&splittable, window, |this, splittable, window, cx| {
                let lhs_editor = splittable.read(cx).lhs_editor().cloned();
                let lhs_editor_id = lhs_editor.as_ref().map(|lhs| lhs.entity_id());
                if lhs_editor_id != this.lhs_editor_id {
                    this.lhs_editor_id = lhs_editor_id;
                    if let Some(lhs_editor) = &lhs_editor {
                        gutter::install(lhs_editor, cx.weak_entity(), cx);
                    }
                    this.reconcile(window, cx);
                }
            }),
            // Text typed into a comment editor isn't lost when its tab closes, and positions that
            // followed edits are saved.
            cx.on_release(|this, cx| {
                this.save_open_editors(cx);
                this.session.update(cx, |session, cx| session.flush(cx));
            }),
            cx.on_app_quit(|this, cx| {
                this.save_open_editors(cx);
                this.session.update(cx, |session, cx| {
                    session.flush(cx);
                    session.saved()
                })
            }),
        ];
        let session_subscription = cx.subscribe_in(&session, window, Self::handle_session_event);
        let sides = [
            Some(splittable.read(cx).rhs_editor().clone()),
            splittable.read(cx).lhs_editor().cloned(),
        ];
        for editor in sides.iter().flatten() {
            gutter::install(editor, cx.weak_entity(), cx);
        }
        let mut this = Self {
            session,
            diff,
            workspace,
            language_registry,
            views: HashMap::default(),
            remote: HashMap::default(),
            hosted: HashMap::default(),
            anchors: TabAnchors::default(),
            parked_anchors: HashMap::default(),
            lhs_editor_id,
            selected: None,
            targeted: None,
            gate: None,
            focus_guard: None,
            overview_block: None,
            reveal_overview_pending: false,
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
        self.session.update(cx, |session, cx| session.flush(cx));
        self.remove_all_blocks(cx);
        self.views.clear();
        self.selected = None;
        self.targeted = None;
        let anchors = self
            .parked_anchors
            .remove(&session.entity_id())
            .unwrap_or_default();
        let previous = std::mem::replace(&mut self.session, session);
        self.parked_anchors.insert(
            previous.entity_id(),
            std::mem::replace(&mut self.anchors, anchors),
        );
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

    pub(crate) fn set_gate(&mut self, gate: Option<CommentGate>) {
        self.gate = gate;
    }

    /// Whether the review takes a comment on `target`: a pull request's by its gate (GitHub's
    /// rules), a local review's when its lines are in one of the diff's hunks.
    pub(crate) fn check(&self, target: &CommentTarget, cx: &App) -> Result<(), SharedString> {
        if let Some(gate) = &self.gate {
            return gate(target, cx);
        }
        let CommentTarget::Lines { path, side, lines } = target else {
            return Ok(());
        };
        self.diff
            .read(cx)
            .check_lines(path, *side, lines, cx)
            .map_err(|refusal| {
                match refusal {
                    LinesRefusal::Outside => {
                        "Comments go on lines of the diff's hunks: changed lines and the 3 lines \
                         around them"
                    }
                    LinesRefusal::StartOutside => {
                        "The selection starts outside the diff's hunks: comments go on changed \
                         lines and the 3 lines around them"
                    }
                    LinesRefusal::EndOutside => {
                        "The selection ends outside the diff's hunks: comments go on changed \
                         lines and the 3 lines around them"
                    }
                    LinesRefusal::AcrossHunks => {
                        "The selection spans two hunks of the diff: a comment's lines must be in \
                         one hunk"
                    }
                    LinesRefusal::Reversed => {
                        "The selection's start comes after its end in the diff"
                    }
                }
                .into()
            })
    }

    fn handle_session_event(
        &mut self,
        _: &Entity<ReviewSession>,
        event: &ReviewSessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ReviewSessionEvent::Loaded => {
                self.create_views(cx);
                self.reconcile(window, cx);
            }
            ReviewSessionEvent::Comment(CommentEvent::Added(id)) => {
                self.ensure_view(*id, cx);
                self.reconcile(window, cx);
            }
            ReviewSessionEvent::Comment(CommentEvent::Changed(id)) => {
                // Submitted from another tab: this tab's editor can't change it any more.
                if self
                    .session
                    .read(cx)
                    .comments()
                    .get(*id)
                    .is_some_and(|comment| comment.is_submitted())
                {
                    self.discard_editor(*id, window, cx);
                }
                self.update_views(&[*id], cx);
                // Another tab's new comment shows once it has text.
                self.reconcile(window, cx);
            }
            // Where a comment is in this tab is up to this tab's anchors, which follow its
            // buffers' edits.
            ReviewSessionEvent::Comment(CommentEvent::Moved(ids)) => self.update_views(ids, cx),
            ReviewSessionEvent::Comment(CommentEvent::Removed(id)) => {
                self.remove_view(*id, window, cx);
                self.anchors.lines.remove(id);
                self.anchors.lost.remove(id);
                self.reconcile(window, cx);
            }
            ReviewSessionEvent::FilesChanged(_)
            | ReviewSessionEvent::HunksChanged
            | ReviewSessionEvent::Failed(_) => return,
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

    fn update_views(&self, ids: &[CommentId], cx: &mut Context<Self>) {
        for id in ids {
            let comment = self.session.read(cx).comments().get(*id).cloned();
            if let (Some(view), Some(comment)) = (self.views.get(id), comment) {
                view.update(cx, |view, cx| view.update_comment(&comment, cx));
            }
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
                self.return_focus_from(Some(id), editing.return_to.upgrade(), window, cx);
            }
            cx.emit(CommentBlocksEvent::EditingChanged);
        }
        if self.targeted == Some(ItemKey::Comment(id)) {
            self.targeted = None;
        }
    }

    /// Closes the comment's editor without saving it.
    fn discard_editor(&mut self, id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.views.get(&id).cloned() else {
            return;
        };
        let Some(editing) = view.update(cx, |view, cx| view.take_editing(cx)) else {
            return;
        };
        if editing.editor.focus_handle(cx).contains_focused(window, cx) {
            self.return_focus_from(Some(id), editing.return_to.upgrade(), window, cx);
        }
        cx.emit(CommentBlocksEvent::EditingChanged);
    }

    /// Shows the pull request's threads (`items`) instead of those shown so far.
    pub(crate) fn set_remote_items(
        &mut self,
        items: Vec<RemoteItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut previous = std::mem::take(&mut self.remote);
        let mut changed = Vec::new();
        for (order, item) in items.into_iter().enumerate() {
            let block = match previous.remove(&item.key) {
                Some(mut block) => {
                    if block.view.update(cx, |view, cx| {
                        view.set_threads(item.title, item.threads, cx)
                    }) {
                        changed.push(item.key.clone());
                    }
                    if block.target != item.target {
                        block.target = item.target;
                        block.anchor = None;
                    }
                    block.order = order;
                    block
                }
                None => {
                    changed.push(item.key.clone());
                    let language_registry = self.language_registry.clone();
                    let view = cx.new(|cx| {
                        RemoteThreadView::new(item.title, item.threads, language_registry, cx)
                    });
                    RemoteBlock {
                        target: item.target,
                        view,
                        anchor: None,
                        order,
                    }
                }
            };
            self.remote.insert(item.key, block);
        }
        if self.selected.as_ref().is_some_and(
            |key| matches!(key, ItemKey::Remote(remote) if !self.remote.contains_key(remote)),
        ) {
            self.selected = None;
        }
        self.reconcile(window, cx);
        self.keep_file_start_in_view(&changed, cx);
        cx.notify();
    }

    /// Threads of a file that show up (or change) above its first row while the cursor is on
    /// that row (a reload after a submit, say) are shown with the file's header at the top, as
    /// when moving to the file: wherever the view's top was, they could be under the sticky
    /// header.
    fn keep_file_start_in_view(&self, changed: &[RemoteKey], cx: &mut App) {
        for key in changed {
            let Some(hosted) = self.hosted.get(&ItemKey::Remote(key.clone())) else {
                continue;
            };
            if hosted.placement.is_file
                && let Some(editor) = hosted.editor.upgrade()
                && editor.update(cx, |editor, cx| {
                    show_file_start(editor, hosted.placement.row, cx)
                })
            {
                return;
            }
        }
    }

    /// After a scroll that follows the cursor (vim's `gg`, a reload of the diff) onto a file's
    /// first row with the file's comments or threads above it, the file's header goes to the
    /// top: the scroll only keeps the cursor's row in view, which leaves them under the sticky
    /// header.
    pub(crate) fn show_file_start_at_cursor(&self, cx: &mut App) {
        for hosted in self.hosted.values() {
            if hosted.placement.is_file
                && let Some(editor) = hosted.editor.upgrade()
                && editor.update(cx, |editor, cx| {
                    show_file_start(editor, hosted.placement.row, cx)
                })
            {
                return;
            }
        }
        // The overview sits above the first file like its file blocks (tuicr's `gg`: the first
        // file's header at the top, the overview just above it).
        if self.overview_block.is_some() {
            self.focused_diff_editor(cx).update(cx, |editor, cx| {
                let first_row = editor
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .anchor_before(Point::zero());
                show_file_start(editor, first_row, cx);
            });
        }
    }

    /// The overview's block, hosted by the review tab while the overview has something to show.
    pub(crate) fn set_overview_block(
        &mut self,
        block: Option<CustomBlockId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overview_block = block;
        if block.is_some() && std::mem::take(&mut self.reveal_overview_pending) {
            self.reveal_overview(window, cx);
        }
    }

    /// Scrolls the overview (with the review-level comments at its bottom) into view: all of it
    /// when it fits, else its bottom at the bottom of the view. A comment editor opening in it
    /// changes its height once rendered, so it's scrolled again after the next frame.
    fn reveal_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.overview_block else {
            self.reveal_overview_pending = true;
            return;
        };
        let rhs = self.rhs_editor(cx);
        rhs.update(cx, |editor, cx| {
            scroll_to_overview(editor, block, window, cx)
        });
        let rhs = rhs.downgrade();
        window.on_next_frame(move |window, cx| {
            rhs.update(cx, |editor, cx| {
                scroll_to_overview(editor, block, window, cx)
            })
            .log_err();
        });
    }

    /// The pull request's threads this tab shows, in the order they were given.
    pub(crate) fn remote_items(
        &self,
    ) -> impl Iterator<Item = (&RemoteKey, &CommentTarget, &Entity<RemoteThreadView>)> {
        let mut items = self.remote.iter().collect::<Vec<_>>();
        items.sort_by_key(|(_, block)| block.order);
        items
            .into_iter()
            .map(|(key, block)| (key, &block.target, &block.view))
    }

    fn rhs_editor(&self, cx: &App) -> Entity<Editor> {
        self.diff.read(cx).editor().read(cx).rhs_editor().clone()
    }

    fn lhs_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.diff.read(cx).editor().read(cx).lhs_editor().cloned()
    }

    fn host_editor(&self, host: Host, cx: &App) -> Option<Entity<Editor>> {
        match host {
            Host::Rhs => Some(self.rhs_editor(cx)),
            Host::Lhs => self.lhs_editor(cx),
        }
    }

    fn focused_diff_editor(&self, cx: &App) -> Entity<Editor> {
        self.diff
            .read(cx)
            .editor()
            .read(cx)
            .focused_editor()
            .clone()
    }

    /// The text of the comment editors open in this tab, as leaving them would save it.
    pub(crate) fn unsaved_bodies(&self, cx: &App) -> HashMap<CommentId, String> {
        self.views
            .iter()
            .filter_map(|(id, view)| {
                let text = view.read(cx).editor()?.read(cx).text(cx);
                let text = text.trim();
                (!text.is_empty()).then(|| (*id, text.to_string()))
            })
            .collect()
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
            .filter(|comment| self.is_shown(comment, cx))
            .filter_map(|comment| self.views.get(&comment.id).cloned())
            .collect()
    }

    /// Comments start empty and are saved with their first text, so an empty comment is being
    /// written in some tab of the review. The other tabs don't show it.
    pub(crate) fn is_listed(&self, comment: &Comment, cx: &App) -> bool {
        !comment.body.is_empty() || self.is_editing_comment(comment.id, cx)
    }

    /// Whether the comment shows in the diff (or in the top block). Submitted comments are the
    /// pull request's threads now, which show instead.
    fn is_shown(&self, comment: &Comment, cx: &App) -> bool {
        self.is_listed(comment, cx) && !comment.is_submitted()
    }

    fn is_editing_comment(&self, id: CommentId, cx: &App) -> bool {
        self.views
            .get(&id)
            .is_some_and(|view| view.read(cx).editing().is_some())
    }

    fn focused_comment(&self, window: &Window, cx: &App) -> Option<CommentId> {
        self.views.iter().find_map(|(id, view)| {
            let editor = view.read(cx).editor()?;
            editor.focus_handle(cx).is_focused(window).then_some(*id)
        })
    }

    fn item_target(&self, key: &ItemKey, cx: &App) -> Option<CommentTarget> {
        match key {
            ItemKey::Comment(id) => Some(self.session.read(cx).comments().get(*id)?.target.clone()),
            ItemKey::Remote(key) => Some(self.remote.get(key)?.target.clone()),
        }
    }

    fn item_view(&self, key: &ItemKey) -> Option<AnyView> {
        match key {
            ItemKey::Comment(id) => Some(self.views.get(id)?.clone().into()),
            ItemKey::Remote(key) => Some(self.remote.get(key)?.view.clone().into()),
        }
    }

    fn remove_all_blocks(&mut self, cx: &mut App) {
        let mut blocks_by_editor = HashMap::<EntityId, (WeakEntity<Editor>, HashSet<_>)>::default();
        for (_, hosted) in self.hosted.drain() {
            blocks_by_editor
                .entry(hosted.editor.entity_id())
                .or_insert_with(|| (hosted.editor.clone(), HashSet::default()))
                .1
                .insert(hosted.block);
        }
        for (editor, blocks) in blocks_by_editor.into_values() {
            // A left side that was dropped took its blocks with it.
            editor
                .update(cx, |editor, cx| editor.remove_blocks(blocks, None, cx))
                .ok();
        }
    }

    /// Brings the comment blocks in line with the comments and with what the diff shows: a
    /// comment whose lines aren't in an excerpt (hidden or folded file, lines without changes
    /// nearby) has no block. The anchors are brought up to date first.
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_anchors(cx);

        let rhs = self.rhs_editor(cx);
        let lhs = self.lhs_editor(cx);
        let desired = {
            let session = self.session.read(cx);
            let diff = self.diff.read(cx);
            let hosts = Hosts::new(&rhs, lhs.as_ref(), cx);
            let comments = session
                .comments()
                .iter()
                .filter(|comment| self.is_shown(comment, cx))
                .filter_map(|comment| {
                    let anchor = self
                        .anchors
                        .lines
                        .get(&comment.id)
                        .map(|anchor| &anchor.range);
                    let (host, placement) = placement(&comment.target, anchor, &hosts, diff, cx)?;
                    Some((ItemKey::Comment(comment.id), (host, placement)))
                });
            let remote = self.remote.iter().filter_map(|(key, block)| {
                let (host, placement) =
                    placement(&block.target, block.anchor.as_ref(), &hosts, diff, cx)?;
                Some((ItemKey::Remote(key.clone()), (host, placement)))
            });
            comments.chain(remote).collect::<HashMap<_, _>>()
        };
        let current_editor = |host: Host| match host {
            Host::Rhs => Some(rhs.entity_id()),
            Host::Lhs => lhs.as_ref().map(|lhs| lhs.entity_id()),
        };

        let mut stale = HashMap::<EntityId, (WeakEntity<Editor>, HashSet<_>)>::default();
        let mut unhosted = Vec::new();
        self.hosted.retain(|id, hosted| {
            let keep = desired.get(id) == Some(&(hosted.host, hosted.placement))
                && current_editor(hosted.host) == Some(hosted.editor.entity_id());
            if !keep {
                stale
                    .entry(hosted.editor.entity_id())
                    .or_insert_with(|| (hosted.editor.clone(), HashSet::default()))
                    .1
                    .insert(hosted.block);
                if let ItemKey::Comment(id) = id
                    && !desired.contains_key(&ItemKey::Comment(*id))
                {
                    unhosted.push(*id);
                }
            }
            keep
        });
        let mut new_blocks = desired
            .into_iter()
            .filter(|(key, _)| !self.hosted.contains_key(key))
            .filter_map(|(key, (host, placement))| {
                let priority = match &key {
                    ItemKey::Comment(id) => comment_block_priority(*id),
                    ItemKey::Remote(remote) => self.remote.get(remote)?.order,
                };
                Some((
                    host,
                    key.clone(),
                    placement,
                    self.item_view(&key)?,
                    priority,
                ))
            })
            .collect::<Vec<_>>();
        if stale.is_empty() && new_blocks.is_empty() {
            return;
        }
        new_blocks.sort_by(|(host_a, key_a, ..), (host_b, key_b, ..)| {
            (host_a, key_a).cmp(&(host_b, key_b))
        });

        for (editor, blocks) in stale.into_values() {
            // A left side that was dropped took its blocks with it.
            editor
                .update(cx, |editor, cx| editor.remove_blocks(blocks, None, cx))
                .ok();
        }
        for host in [Host::Rhs, Host::Lhs] {
            let blocks = new_blocks
                .iter()
                .filter(|(block_host, ..)| *block_host == host)
                .collect::<Vec<_>>();
            if blocks.is_empty() {
                continue;
            }
            let Some(editor) = self.host_editor(host, cx) else {
                continue;
            };
            let block_ids = editor.update(cx, |editor, cx| {
                let scroll_position = editor.scroll_position(cx);
                let block_ids = editor.insert_blocks(
                    blocks.iter().map(|(_, _, placement, view, priority)| {
                        block_properties(*placement, view.clone(), *priority)
                    }),
                    None,
                    cx,
                );
                if scroll_position.y == 0. {
                    anchor_scroll_at_start(editor, scroll_position.x, window, cx);
                }
                block_ids
            });
            for ((_, key, placement, _, _), block) in blocks.into_iter().zip(block_ids) {
                self.hosted.insert(
                    key.clone(),
                    HostedBlock {
                        host,
                        editor: editor.downgrade(),
                        block,
                        placement: *placement,
                    },
                );
            }
        }

        // A comment being edited whose block went away (its file was folded, hidden, or lost its
        // changes) is left, keeping its text: it couldn't be reached again.
        for id in unhosted {
            if self.is_editing_comment(id, cx) {
                self.close_editor(id, Leave::Keep, window, cx);
            }
        }
        self.update_targeted(cx);
    }

    /// Anchors the line comments whose file is shown and that have no anchor in this tab yet
    /// (the lines of comments loaded from the database are compared with the file then),
    /// notices lines that were deleted, and records where the comments' lines are now.
    fn update_anchors(&mut self, cx: &mut Context<Self>) {
        let mut checked = Vec::new();
        let mut moves = Vec::new();
        {
            let diff = self.diff.read(cx);
            let session = self.session.read(cx);
            let comments = session.comments();
            let anchors = &mut self.anchors;
            anchors.lines.retain(|id, _| comments.get(*id).is_some());
            anchors.lost.retain(|id| comments.get(*id).is_some());
            let mut snapshots = HashMap::<EntityId, BufferSnapshot>::default();
            for comment in comments.iter() {
                let id = comment.id;
                let CommentTarget::Lines { path, side, lines } = &comment.target else {
                    continue;
                };
                let Some(buffer_entity) = side_buffer(diff, path, *side, cx) else {
                    continue;
                };
                let buffer = snapshots
                    .entry(buffer_entity.entity_id())
                    .or_insert_with(|| buffer_entity.read(cx).snapshot());
                // As it will be once this update's checks are applied.
                let mut is_outdated = comment.is_outdated();
                let mut deleted_from = None;

                if let Some(anchor) = anchors.lines.get(&id) {
                    let start = &anchor.range.start;
                    let end = &anchor.range.end;
                    // The buffer was replaced: the file left the review and came back, or its
                    // diff base or snapshot was rebuilt.
                    let replaced = !buffer.can_resolve(start) || !buffer.can_resolve(end);
                    // Deleting the lines collapses both ends onto the deletion (text inserted
                    // there later, as a stash pop does, goes after them). An empty line has
                    // nothing to delete.
                    let deleted = !replaced
                        && !anchor.text.is_empty()
                        && start.to_offset(buffer) >= end.to_offset(buffer);
                    if deleted {
                        // Lines deleted again while outdated: the first place is the one that
                        // had the comment's text.
                        deleted_from = Some(
                            anchor
                                .deleted_from
                                .clone()
                                .unwrap_or_else(|| anchor.range.clone()),
                        );
                    }
                    if replaced || deleted {
                        anchors.lines.remove(&id);
                        anchors.lost.insert(id);
                    }
                }

                if !anchors.lines.contains_key(&id) {
                    let max_row = buffer.max_point().row;
                    let last_row = lines.end().saturating_sub(1);
                    let start_row = lines.start().saturating_sub(1).min(max_row);
                    let end_row = last_row.min(max_row).max(start_row);
                    let (range, text) = line_anchor_and_text(buffer, start_row..=end_row);
                    let outdated = last_row > max_row
                        || comment
                            .line_text
                            .as_ref()
                            .is_some_and(|line_text| *line_text != text);
                    let was_lost = anchors.lost.remove(&id);
                    // Other tabs anchor a checked comment on its lines without comparing, but an
                    // outdated one whose lines have its text again (changed back while no tab
                    // showed them) is current again.
                    if session.needs_check(id) || was_lost || (is_outdated && !outdated) {
                        checked.push((id, outdated));
                        is_outdated = outdated && !comment.is_submitted();
                    }
                    // As stored, so that a position that had to be clamped is recorded below.
                    anchors.lines.insert(
                        id,
                        LineAnchor {
                            range,
                            lines: lines.clone(),
                            text: comment.line_text.clone().unwrap_or_default(),
                            deleted_from: deleted_from.filter(|_| is_outdated),
                            _buffer: buffer_entity.clone(),
                        },
                    );
                }

                let Some(anchor) = anchors.lines.get_mut(&id) else {
                    continue;
                };
                if !is_outdated {
                    anchor.deleted_from = None;
                } else if let Some((range, line_text)) = anchor
                    .deleted_from
                    .as_ref()
                    .zip(comment.line_text.as_deref())
                {
                    if !buffer.can_resolve(&range.start) || !buffer.can_resolve(&range.end) {
                        anchor.deleted_from = None;
                    } else if let Some(restored) = restored_lines(buffer, range, line_text) {
                        anchor.range = restored;
                        anchor.deleted_from = None;
                        checked.push((id, false));
                        is_outdated = false;
                    }
                }
                // Typing on an empty commented line moves the range's start past its end; the
                // block stays below the end.
                let end_row = anchor.range.end.to_point(buffer).row;
                let start_row = anchor.range.start.to_point(buffer).row.min(end_row);
                let (_, text) = line_anchor_and_text(buffer, start_row..=end_row);
                let lines = start_row + 1..=end_row + 1;
                if anchor.lines != lines || anchor.text != text {
                    // This tab's lines got the text the outdated comment was about back. Only a
                    // change seen in this tab counts: another window whose buffer never lost
                    // the text doesn't overrule the tab that saw it go.
                    if is_outdated
                        && anchor.text != text
                        && comment.line_text.as_deref() == Some(text.as_str())
                    {
                        anchor.deleted_from = None;
                        checked.push((id, false));
                    }
                    anchor.lines = lines.clone();
                    anchor.text = text.clone();
                    moves.push((id, lines, text));
                }
            }

            // Threads are anchored on their line numbers once, then follow the edits like
            // comments do; lines past the end of the local file aren't shown.
            for block in self.remote.values_mut() {
                let CommentTarget::Lines { path, side, lines } = &block.target else {
                    continue;
                };
                let Some(buffer) = side_buffer(diff, path, *side, cx) else {
                    continue;
                };
                let buffer = snapshots
                    .entry(buffer.entity_id())
                    .or_insert_with(|| buffer.read(cx).snapshot());
                if block.anchor.as_ref().is_some_and(|anchor| {
                    !buffer.can_resolve(&anchor.start) || !buffer.can_resolve(&anchor.end)
                }) {
                    block.anchor = None;
                }
                if block.anchor.is_none() {
                    let end_row = lines.end().saturating_sub(1);
                    if end_row > buffer.max_point().row {
                        continue;
                    }
                    let start_row = lines.start().saturating_sub(1).min(end_row);
                    block.anchor = Some(line_anchor_and_text(buffer, start_row..=end_row).0);
                }
            }
        }
        if checked.is_empty() && moves.is_empty() {
            return;
        }
        self.session.update(cx, |session, cx| {
            for (id, outdated) in checked {
                session.check_comment(id, outdated, cx);
            }
            session.set_comment_positions(moves, cx);
        });
    }

    /// `c`: comments on the cursor line or the selected lines. On a folded file header, comments
    /// on the file.
    pub(crate) fn add_line_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.focused_diff_editor(cx);
        self.add_line_comment_on(editor, None, window, cx);
    }

    /// Comments on `rows` (multibuffer rows) of one side of the diff, or on its selection, and
    /// leaves the cursor on the last of them (or where the selection's head was). The side is
    /// passed in: a side focused directly only becomes the split editor's focused one on the
    /// next frame.
    pub(crate) fn add_line_comment_on(
        &mut self,
        editor: Entity<Editor>,
        rows: Option<RangeInclusive<u32>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected) = self.selected_lines(&editor, rows.clone(), cx) else {
            return;
        };
        match rows {
            Some(rows) => move_cursor_to_row(&editor, *rows.end(), window, cx),
            None => collapse_selection(&editor, window, cx),
        }
        match selected {
            SelectedLines::FoldedFile(buffer_id) => {
                let Some(path) = self.path_of_buffer(buffer_id, cx) else {
                    return;
                };
                self.add_file_comment_on(path, editor, window, cx);
            }
            SelectedLines::Lines {
                side,
                buffer_id,
                rows,
            } => {
                let Some((path, buffer)) = self.file_of_side_buffer(side, buffer_id, cx) else {
                    return;
                };
                let snapshot = buffer.read(cx).snapshot();
                let (range, text) = line_anchor_and_text(&snapshot, rows.clone());
                let lines = rows.start() + 1..=rows.end() + 1;
                let new_comment = NewComment {
                    target: CommentTarget::Lines {
                        path,
                        side,
                        lines: lines.clone(),
                    },
                    line_text: Some(text.clone()),
                };
                let anchor = LineAnchor {
                    range,
                    lines,
                    text,
                    deleted_from: None,
                    _buffer: buffer,
                };
                self.add_comment(new_comment, Some(anchor), editor, window, cx);
            }
        }
    }

    /// What `c` comments on in a side of the diff: `rows` (multibuffer rows), or the selection.
    fn selected_lines(
        &self,
        editor: &Entity<Editor>,
        rows: Option<RangeInclusive<u32>>,
        cx: &mut App,
    ) -> Option<SelectedLines> {
        let splittable = self.diff.read(cx).editor().read(cx);
        let rhs = splittable.rhs_editor().clone();
        let is_lhs = splittable.lhs_editor() == Some(editor);
        let diff = self.diff.clone();
        editor.update(cx, |editor, cx| {
            selected_lines(editor, rows, is_lhs, &rhs, &diff, cx)
        })
    }

    /// The file a buffer of either side of the diff shows.
    fn path_of_buffer(&self, buffer_id: BufferId, cx: &App) -> Option<RepoPath> {
        let diff = self.diff.read(cx);
        diff.repo_path_for_buffer(buffer_id, cx)
            .or_else(|| diff.base_buffer(buffer_id, cx).map(|(path, _)| path))
    }

    /// What `c` would comment on in a side of the diff: its selected lines (or the cursor's), or
    /// the file of a folded header.
    pub(crate) fn line_comment_target(
        &self,
        editor: &Entity<Editor>,
        cx: &mut App,
    ) -> Option<CommentTarget> {
        match self.selected_lines(editor, None, cx)? {
            SelectedLines::FoldedFile(buffer_id) => Some(CommentTarget::File {
                path: self.path_of_buffer(buffer_id, cx)?,
            }),
            SelectedLines::Lines {
                side,
                buffer_id,
                rows,
            } => {
                let (path, _) = self.file_of_side_buffer(side, buffer_id, cx)?;
                Some(CommentTarget::Lines {
                    path,
                    side,
                    lines: rows.start() + 1..=rows.end() + 1,
                })
            }
        }
    }

    /// The file a buffer of one side of the diff shows, and that buffer.
    fn file_of_side_buffer(
        &self,
        side: Side,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<(RepoPath, Entity<Buffer>)> {
        let diff = self.diff.read(cx);
        match side {
            Side::New => {
                let path = diff.repo_path_for_buffer(buffer_id, cx)?;
                let buffer = diff.display_buffer(&review_path_key(&path))?.clone();
                Some((path, buffer))
            }
            Side::Old => diff.base_buffer(buffer_id, cx),
        }
    }

    /// Visual `y`: the text of the selected lines, of the side `c` would comment on, and how many
    /// lines that is. Leaves visual mode on the selection's first row, like a yank.
    pub(crate) fn selected_text(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(String, usize)> {
        let editor = self.focused_diff_editor(cx);
        let selected = self.selected_lines(&editor, None, cx);
        collapse_selection_to_start(&editor, window, cx);
        let SelectedLines::Lines {
            side,
            buffer_id,
            rows,
        } = selected?
        else {
            return None;
        };
        let (_, buffer) = self.file_of_side_buffer(side, buffer_id, cx)?;
        let (_, text) = line_anchor_and_text(&buffer.read(cx).snapshot(), rows.clone());
        Some((text, rows.count()))
    }

    /// A comment being written for the first time has no text yet.
    pub(crate) fn body_at_cursor(&self, cx: &mut App) -> Option<String> {
        let key = self.item_at_cursor(cx)?;
        self.item_body(&key, cx)
    }

    /// The text of a comment, or of a thread's first comment.
    pub(crate) fn item_body(&self, key: &ItemKey, cx: &App) -> Option<String> {
        let body = match key {
            ItemKey::Comment(id) => self.session.read(cx).comments().get(*id)?.body.clone(),
            ItemKey::Remote(key) => {
                let view = self.remote.get(key)?.view.read(cx);
                view.threads().next()?.comments.first()?.body.to_string()
            }
        };
        (!body.is_empty()).then_some(body)
    }

    /// Whether a comment of the review was submitted (it can't change any more).
    pub(crate) fn is_submitted(&self, id: CommentId, cx: &App) -> bool {
        self.session
            .read(cx)
            .comments()
            .get(id)
            .is_some_and(|comment| comment.is_submitted())
    }

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

    /// A file header's comment button: comments on that file, wherever the cursor is.
    pub(crate) fn add_file_comment_for(
        &mut self,
        path: RepoPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.focused_diff_editor(cx);
        collapse_selection(&editor, window, cx);
        self.add_file_comment_on(path, editor, window, cx);
    }

    /// The review context menu's "Comment on File", from a side of the diff.
    pub(crate) fn add_file_comment_from(
        &mut self,
        path: RepoPath,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        collapse_selection(&editor, window, cx);
        self.add_file_comment_on(path, editor, window, cx);
    }

    fn add_file_comment_on(
        &mut self,
        path: RepoPath,
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
        };
        self.add_comment(new_comment, None, return_to, window, cx);
    }

    /// `;c`: adds a comment on the whole review, at the top of the tab.
    pub(crate) fn add_review_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.focused_diff_editor(cx);
        collapse_selection(&editor, window, cx);
        let new_comment = NewComment {
            target: CommentTarget::Review,
            line_text: None,
        };
        self.add_comment(new_comment, None, editor, window, cx);
    }

    /// Every comment is made here.
    fn add_comment(
        &mut self,
        new_comment: NewComment,
        anchor: Option<LineAnchor>,
        return_to: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(reason) = self.check(&new_comment.target, cx) {
            self.show_toast(reason, cx);
            return;
        }
        let Some(id) = self
            .session
            .update(cx, |session, cx| session.add_comment(new_comment, cx))
        else {
            self.show_toast("The review's comments haven't loaded yet", cx);
            return;
        };
        if let Some(anchor) = anchor {
            self.anchors.lines.insert(id, anchor);
        }
        // The session's event comes later; the editor opens now.
        self.ensure_view(id, cx);
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

    /// Edits a comment (its edit button, the review panel).
    pub(crate) fn edit(&mut self, id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_edit(id, false, None, window, cx);
    }

    /// Edits a comment, returning to `editor` (a side of the diff) when done.
    pub(crate) fn edit_from(
        &mut self,
        id: CommentId,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_edit(id, false, Some(editor), window, cx);
    }

    /// The review panel's edit button: the diff moves to the comment (showing its file), then
    /// its editor opens. A comment the diff doesn't show (its file is hidden) isn't edited.
    pub(crate) fn land_and_edit(
        &mut self,
        id: CommentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = ItemKey::Comment(id);
        if self
            .session
            .read(cx)
            .comments()
            .get(id)
            .is_some_and(|comment| comment.target == CommentTarget::Review)
        {
            self.edit(id, window, cx);
            return true;
        }
        if !self.land_on(&key, window, cx) {
            return false;
        }
        let editor = self
            .hosted
            .get(&key)
            .and_then(|hosted| hosted.editor.upgrade());
        self.begin_edit(id, false, editor, window, cx);
        true
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
        if let Some(editor) = view.read(cx).editor().cloned() {
            if let Some(return_to) = return_to {
                view.update(cx, |view, _| view.set_return_to(return_to.downgrade()));
            }
            window.focus(&editor.focus_handle(cx), cx);
            self.reveal(id, window, cx);
            return;
        }
        let Some(comment) = self.session.read(cx).comments().get(id) else {
            return;
        };
        if comment.is_submitted() {
            self.show_toast("This comment was submitted and can't be changed", cx);
            return;
        }
        let body = comment.body.clone();
        let return_to = return_to
            .unwrap_or_else(|| self.focused_diff_editor(cx))
            .downgrade();
        let vim_save = vim_save_action_type(self.workspace.clone(), cx);
        let blocks = cx.weak_entity();
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
            // Vim's `:w` with a range and a file name (`:%w file`) writes the lines through the
            // editor's project, which comment editors don't have (vim panics), and `:w!` or
            // `:'<,'>w` wouldn't leave the comment. Every `:w` leaves it, keeping the text.
            if let Some(vim_save) = vim_save {
                editor
                    .register_action_renderer(move |_, window, _| {
                        let blocks = blocks.clone();
                        window.on_action(vim_save, move |_, phase, window, cx| {
                            if phase == DispatchPhase::Capture {
                                cx.stop_propagation();
                                blocks
                                    .update(cx, |blocks, cx| {
                                        blocks.leave(id, Leave::Keep, window, cx)
                                    })
                                    .log_err();
                            }
                        });
                    })
                    .detach();
            }
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
        // A new comment, still empty, only gets its block once it is being edited.
        self.reconcile(window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        self.reveal(id, window, cx);
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

    /// Scrolls the diff so that the comment's block (or the overview, for a review-level
    /// comment) shows.
    fn reveal(&mut self, id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .session
            .read(cx)
            .comments()
            .get(id)
            .is_some_and(|comment| comment.target == CommentTarget::Review)
        {
            self.reveal_overview(window, cx);
            return;
        }
        let Some(hosted) = self.hosted.get(&ItemKey::Comment(id)) else {
            return;
        };
        let Some(editor) = hosted.editor.upgrade() else {
            return;
        };
        let placement = hosted.placement;
        editor.update(cx, |editor, cx| {
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
        self.close_editor(id, leave, window, cx);
    }

    /// Like [`Self::leave`], but the diff only takes the focus from the comment's editor.
    fn close_editor(
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
        // Before saving: a new comment left empty is deleted, and its block with it.
        if editing.editor.focus_handle(cx).contains_focused(window, cx) {
            self.return_focus_from(Some(id), editing.return_to.upgrade(), window, cx);
        }
        save_text(&self.session, id, editing.is_new, &text, leave, cx);
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
    pub(crate) fn save_open_editors(&mut self, cx: &mut App) {
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
    /// is in normal mode there. When that editor is gone (the left side was recreated or
    /// dropped since), the side showing the comment gets the focus, on the comment's row.
    fn return_focus(
        &mut self,
        return_to: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.return_focus_from(None, return_to, window, cx);
    }

    fn return_focus_from(
        &mut self,
        comment: Option<CommentId>,
        return_to: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rhs = self.rhs_editor(cx);
        let lhs = self.lhs_editor(cx);
        let return_to = return_to.filter(|editor| *editor == rhs || Some(editor) == lhs.as_ref());
        if let Some(editor) = return_to {
            collapse_selection(&editor, window, cx);
            self.focus_diff_editor(&editor, window, cx);
            return;
        }
        let hosted = comment
            .and_then(|id| self.hosted.get(&ItemKey::Comment(id)))
            .and_then(|hosted| Some((hosted.editor.upgrade()?, hosted.placement.row)));
        match hosted {
            Some((editor, row)) => {
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let point = Point::new(row.to_point(&snapshot).row, 0);
                    editor.change_selections(
                        SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| selections.select_ranges([point..point]),
                    );
                });
                self.focus_diff_editor(&editor, window, cx);
            }
            None => {
                let editor = self.focused_diff_editor(cx);
                collapse_selection(&editor, window, cx);
                self.focus_diff_editor(&editor, window, cx);
            }
        }
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
        // runs after the editor's own and takes the focus back. It holds the handle weakly: a
        // left side dropped before that would otherwise stay focusable, unrendered.
        self.focus_guard = Some(cx.on_focus(&focus_handle, window, {
            let focus_handle = focus_handle.downgrade();
            move |this, window, cx| {
                this.focus_guard = None;
                if let Some(focus_handle) = focus_handle.upgrade() {
                    window.focus(&focus_handle, cx);
                }
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
        self.set_type(id, comment_type, cx);
    }

    /// The type chosen in a comment editor's type menu.
    pub(crate) fn set_type(
        &mut self,
        id: CommentId,
        comment_type: Option<CommentType>,
        cx: &mut Context<Self>,
    ) {
        self.session.update(cx, |session, cx| {
            session.set_comment_type(id, comment_type, cx)
        });
    }

    /// `dd`: deletes the comment below the cursor line, after asking.
    pub(crate) fn delete_comment_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.comment_at_cursor(cx) {
            Some(id) => self.confirm_delete(id, window, cx),
            None if self.item_at_cursor(cx).is_some() => {
                self.show_toast("Threads on GitHub can't be changed in the review", cx)
            }
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
        if comment.is_submitted() {
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
        // The comment of this session: the review key may change while the prompt is open, and
        // ids are per session.
        let session = self.session.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            if answer.await.ok() == Some(0) {
                session
                    .update(cx, |session, cx| session.delete_comment(id, cx))
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
            let has_comments = !self.remote.is_empty()
                || self
                    .session
                    .read(cx)
                    .comments()
                    .iter()
                    .any(|comment| self.is_shown(comment, cx));
            self.show_toast(
                if has_comments {
                    "No comments in the diff"
                } else {
                    "No comments"
                },
                cx,
            );
            return;
        }
        let order = self.diff.read(cx).file_order();
        let focused = self.focused_diff_editor(cx);
        let current = self
            .items_at_cursor(&focused, cx)
            .and_then(|at_cursor| self.selected.clone().filter(|key| at_cursor.contains(key)))
            .and_then(|key| items.iter().find(|item| item.5 == key).cloned());
        let target = match &current {
            Some(current) => match direction {
                Direction::Next => items.iter().find(|item| *item > current),
                Direction::Previous => items.iter().rev().find(|item| *item < current),
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
                        .find(|(file_index, row, ..)| (*file_index, *row) >= position),
                    (Direction::Previous, Some(position)) => items
                        .iter()
                        .rev()
                        .find(|(file_index, row, ..)| (*file_index, *row) < position),
                    (_, None) => None,
                }
            }
        };
        let target = match direction {
            Direction::Next => target.or(items.first()),
            Direction::Previous => target.or(items.last()),
        };
        if let Some((.., key)) = target.cloned() {
            self.land_on(&key, window, cx);
        }
    }

    /// The line and file comments and the threads in the order of the diff: by file, file
    /// comments first, then by the new file's line (old lines at the new line they face, by
    /// their old line), then in the order their blocks show at one place (threads in GitHub's
    /// order, then comments). Comments in files the filters hide, and comments on lines the
    /// diff doesn't show, are left out.
    fn comment_order(&self, cx: &App) -> Vec<(usize, i64, Side, u32, usize, ItemKey)> {
        let diff = self.diff.read(cx);
        let order = diff.file_order();
        let rhs = self.rhs_editor(cx);
        let lhs = self.lhs_editor(cx);
        let hosts = Hosts::new(&rhs, lhs.as_ref(), cx);
        let position = |target: &CommentTarget, anchor: Option<&Range<language::Anchor>>| {
            let path_key = review_path_key(target.path()?);
            let file_index = order.iter().position(|key| key == &path_key)?;
            let (row, side, side_row) = match target {
                CommentTarget::File { .. } => (-1, Side::Old, 0),
                CommentTarget::Lines { side, .. } => {
                    let anchor = anchor?;
                    // Files hidden by single-file mode have no excerpts to check. Folded
                    // (reviewed) files count: landing on a comment unfolds its file.
                    if diff.is_shown(&path_key) {
                        line_row_in_excerpts(target, anchor, &hosts, diff, cx)?;
                    }
                    let (buffer, file_diff) = diff.file_diff(&path_key)?;
                    let buffer = buffer.read(cx).snapshot();
                    // An anchor into a buffer that was replaced since (the file left the
                    // review and came back) can't be resolved.
                    let (row, side_row) = match side {
                        Side::New => {
                            if !buffer.can_resolve(&anchor.end) {
                                return None;
                            }
                            let row = anchor.end.to_point(&buffer).row;
                            (row, row)
                        }
                        Side::Old => {
                            let file_diff = file_diff.read(cx).snapshot(cx);
                            if !file_diff.base_text().can_resolve(&anchor.end) {
                                return None;
                            }
                            let point = anchor.end.to_point(file_diff.base_text());
                            let row = file_diff
                                .base_text_point_to_buffer_point(Point::new(point.row, 0), &buffer)
                                .row;
                            (row, point.row)
                        }
                    };
                    (i64::from(row), *side, side_row)
                }
                CommentTarget::Review => return None,
            };
            Some((file_index, row, side, side_row))
        };
        let comments = self
            .session
            .read(cx)
            .comments()
            .iter()
            .filter(|comment| self.is_shown(comment, cx))
            .filter_map(|comment| {
                let anchor = self
                    .anchors
                    .lines
                    .get(&comment.id)
                    .map(|anchor| &anchor.range);
                let (file_index, row, side, side_row) = position(&comment.target, anchor)?;
                Some((
                    file_index,
                    row,
                    side,
                    side_row,
                    comment_block_priority(comment.id),
                    ItemKey::Comment(comment.id),
                ))
            });
        let remote = self.remote.iter().filter_map(|(key, block)| {
            let (file_index, row, side, side_row) = position(&block.target, block.anchor.as_ref())?;
            Some((
                file_index,
                row,
                side,
                side_row,
                block.order,
                ItemKey::Remote(key.clone()),
            ))
        });
        let mut items = comments.chain(remote).collect::<Vec<_>>();
        items.sort();
        items
    }

    /// Moves the cursor to the row the comment's block is attached to, showing its file first,
    /// and focuses that side of the diff. Returns whether the comment is in the diff.
    pub(crate) fn land_on(
        &mut self,
        key: &ItemKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(path_key) = self
            .item_target(key, cx)
            .and_then(|target| target.path().map(review_path_key))
        else {
            return false;
        };
        self.show_file(&path_key, window, cx);
        self.reconcile(window, cx);
        let Some(hosted) = self.hosted.get(key) else {
            return false;
        };
        let Some(editor) = hosted.editor.upgrade() else {
            return false;
        };
        let anchor = hosted.placement.row;
        let is_file = hosted.placement.is_file;
        editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let point = Point::new(anchor.to_point(&snapshot).row, 0);
            // A file's comments and threads sit between its header and its first row, where the
            // sticky header would cover them.
            let autoscroll = is_file
                .then(|| file_start_autoscroll(editor, anchor, cx))
                .flatten()
                .unwrap_or_else(Autoscroll::center);
            editor.change_selections(
                SelectionEffects::scroll(autoscroll),
                window,
                cx,
                |selections| selections.select_ranges([point..point]),
            );
        });
        self.focus_diff_editor(&editor, window, cx);
        self.selected = Some(key.clone());
        self.update_targeted(cx);
        true
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
    /// comment below the cursor row (on the focused side first), else the file comment above it.
    /// Threads on the cursor row don't count.
    fn comment_at_cursor(&self, cx: &mut App) -> Option<CommentId> {
        let editor = self.focused_diff_editor(cx);
        self.comment_at_cursor_of(&editor, cx)
    }

    /// [`Self::comment_at_cursor`] with the cursor of a given side of the diff, which the split
    /// editor only takes as its focused side on the next frame after focusing it.
    pub(crate) fn comment_at_cursor_of(
        &self,
        editor: &Entity<Editor>,
        cx: &mut App,
    ) -> Option<CommentId> {
        let at_cursor = self.items_at_cursor(editor, cx)?;
        let comment = |key: &ItemKey| match key {
            ItemKey::Comment(id) => Some(*id),
            ItemKey::Remote(_) => None,
        };
        self.selected
            .as_ref()
            .filter(|key| at_cursor.contains(key))
            .and_then(comment)
            .or_else(|| at_cursor.iter().find_map(comment))
    }

    /// The comment or thread at the cursor row: the one `]t`/`[t` moved to, else the review's
    /// own comment there (which `i` edits), else a thread.
    fn item_at_cursor(&self, cx: &mut App) -> Option<ItemKey> {
        let editor = self.focused_diff_editor(cx);
        self.item_at_cursor_of(&editor, cx)
    }

    pub(crate) fn item_at_cursor_of(
        &self,
        editor: &Entity<Editor>,
        cx: &mut App,
    ) -> Option<ItemKey> {
        let at_cursor = self.items_at_cursor(editor, cx)?;
        self.selected
            .clone()
            .filter(|key| at_cursor.contains(key))
            .or_else(|| {
                at_cursor
                    .iter()
                    .find(|key| matches!(key, ItemKey::Comment(_)))
                    .cloned()
            })
            .or_else(|| at_cursor.first().cloned())
    }

    /// The comments and threads shown at the cursor row: line ones before file ones, the focused
    /// side's first.
    fn items_at_cursor(&self, editor: &Entity<Editor>, cx: &mut App) -> Option<Vec<ItemKey>> {
        let rows = self.cursor_rows(editor, cx)?;
        let mut at_cursor = Vec::new();
        for (key, hosted) in &self.hosted {
            let Some(cursor_row) = rows.row(hosted.host) else {
                continue;
            };
            let Some(editor) = hosted.editor.upgrade() else {
                continue;
            };
            let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
            if hosted.placement.row.to_point(&snapshot).row == cursor_row {
                at_cursor.push((hosted.placement.is_file, hosted.host != rows.focused, key));
            }
        }
        if at_cursor.is_empty() {
            return None;
        }
        at_cursor.sort();
        Some(
            at_cursor
                .into_iter()
                .map(|(_, _, key)| key.clone())
                .collect(),
        )
    }

    /// The cursor's row on the focused side, and the row facing it on the other side of a split
    /// diff (spacers keep the sides' rows facing each other).
    fn cursor_rows(&self, focused: &Entity<Editor>, cx: &mut App) -> Option<CursorRows> {
        let focused_host = if self.lhs_editor(cx).as_ref() == Some(focused) {
            Host::Lhs
        } else {
            Host::Rhs
        };
        let row = {
            let editor = focused.read(cx);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            editor
                .selections
                .newest_anchor()
                .head()
                .to_point(&snapshot)
                .row
        };
        let other = match focused_host {
            Host::Rhs => self.lhs_editor(cx),
            Host::Lhs => Some(self.rhs_editor(cx)),
        };
        let facing = other.and_then(|other| facing_row(focused, row, &other, cx));
        Some(match focused_host {
            Host::Rhs => CursorRows {
                focused: Host::Rhs,
                rhs: Some(row),
                lhs: facing,
            },
            Host::Lhs => CursorRows {
                focused: Host::Lhs,
                rhs: facing,
                lhs: Some(row),
            },
        })
    }

    /// The file at the cursor and the cursor's row in the new file (the row after them on removed
    /// lines, the facing row on the left side of a split diff).
    fn cursor_position(&self, cx: &mut App) -> Option<(PathKey, u32)> {
        let focused = self.focused_diff_editor(cx);
        let editor = focused.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let row = editor
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
            .row;
        let anchor = snapshot.anchor_before(Point::new(row, 0));
        let (text_anchor, buffer) = snapshot.anchor_to_buffer_anchor(anchor)?;
        let path_key = snapshot.path_for_buffer(text_anchor.buffer_id)?.clone();
        let point = text_anchor.to_point(buffer);
        let diff = self.diff.read(cx);
        let (display_buffer, file_diff) = diff.file_diff(&path_key)?;
        let display_buffer = display_buffer.read(cx).snapshot();
        if text_anchor.buffer_id == display_buffer.remote_id() {
            return Some((path_key, point.row));
        }
        let row = file_diff
            .read(cx)
            .snapshot(cx)
            .base_text_point_to_buffer_point(point, &display_buffer)
            .row;
        Some((path_key, row))
    }

    fn update_targeted(&mut self, cx: &mut Context<Self>) {
        let targeted = self.item_at_cursor(cx);
        if targeted == self.targeted {
            return;
        }
        for (key, is_targeted) in [(self.targeted.clone(), false), (targeted.clone(), true)] {
            match key {
                Some(ItemKey::Comment(id)) => {
                    if let Some(view) = self.views.get(&id) {
                        view.update(cx, |view, cx| view.set_targeted(is_targeted, cx));
                    }
                }
                Some(ItemKey::Remote(key)) => {
                    if let Some(block) = self.remote.get(&key) {
                        block
                            .view
                            .update(cx, |view, cx| view.set_targeted(is_targeted, cx));
                    }
                }
                None => {}
            }
        }
        self.targeted = targeted;
    }

    fn show_toast(&self, message: impl Into<SharedString>, cx: &mut App) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<Self>(), message.into().to_string())
                        .autohide(),
                    cx,
                )
            })
            .log_err();
    }

    #[cfg(test)]
    pub(crate) fn view(&self, id: CommentId) -> Option<&Entity<CommentView>> {
        self.views.get(&id)
    }

    /// Each hosted comment's block row, and whether it is on the left side.
    #[cfg(test)]
    pub(crate) fn hosted_rows_on_sides(&self, cx: &mut App) -> Vec<(CommentId, bool, u32)> {
        let mut rows = self
            .hosted
            .iter()
            .filter_map(|(key, hosted)| match key {
                ItemKey::Comment(id) => Some((id, hosted)),
                ItemKey::Remote(_) => None,
            })
            .map(|(id, hosted)| {
                let block = hosted.block;
                let row = hosted
                    .editor
                    .update(cx, |editor, cx| editor.row_for_block(block, cx))
                    .ok()
                    .flatten();
                (
                    *id,
                    hosted.host == Host::Lhs,
                    row.map_or(u32::MAX, |row| row.0),
                )
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    }

    /// Each hosted thread block's row, and whether it is on the left side.
    #[cfg(test)]
    pub(crate) fn hosted_remote_rows(&self, cx: &mut App) -> Vec<(RemoteKey, bool, u32)> {
        let mut rows = self
            .hosted
            .iter()
            .filter_map(|(key, hosted)| match key {
                ItemKey::Remote(key) => Some((key, hosted)),
                ItemKey::Comment(_) => None,
            })
            .map(|(key, hosted)| {
                let block = hosted.block;
                let row = hosted
                    .editor
                    .update(cx, |editor, cx| editor.row_for_block(block, cx))
                    .ok()
                    .flatten();
                (
                    key.clone(),
                    hosted.host == Host::Lhs,
                    row.map_or(u32::MAX, |row| row.0),
                )
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    }

    #[cfg(test)]
    pub(crate) fn remote_view(&self, key: &RemoteKey) -> Option<&Entity<RemoteThreadView>> {
        self.remote.get(key).map(|block| &block.view)
    }

    /// Each hosted comment's block row (either side).
    #[cfg(test)]
    pub(crate) fn hosted_rows(&self, cx: &mut App) -> Vec<(CommentId, u32)> {
        self.hosted_rows_on_sides(cx)
            .into_iter()
            .map(|(id, _, row)| (id, row))
            .collect()
    }
}

/// The two sides of the diff, read once per pass over the comments.
struct Hosts<'a> {
    rhs: &'a Editor,
    rhs_snapshot: MultiBufferSnapshot,
    lhs_snapshot: Option<MultiBufferSnapshot>,
}

impl<'a> Hosts<'a> {
    fn new(rhs: &'a Entity<Editor>, lhs: Option<&Entity<Editor>>, cx: &'a App) -> Self {
        let rhs = rhs.read(cx);
        Self {
            rhs,
            rhs_snapshot: rhs.buffer().read(cx).snapshot(cx),
            lhs_snapshot: lhs.map(|lhs| lhs.read(cx).buffer().read(cx).snapshot(cx)),
        }
    }

    fn snapshot(&self, host: Host) -> Option<&MultiBufferSnapshot> {
        match host {
            Host::Rhs => Some(&self.rhs_snapshot),
            Host::Lhs => self.lhs_snapshot.as_ref(),
        }
    }
}

/// The buffer a line comment's side shows: the file's buffer, or its diff base.
fn side_buffer<'a>(
    diff: &'a ReviewDiff,
    path: &RepoPath,
    side: Side,
    cx: &'a App,
) -> Option<&'a Entity<Buffer>> {
    let (buffer, file_diff) = diff.file_diff(&review_path_key(path))?;
    match side {
        Side::New => Some(buffer),
        // A file without a diff base (an added one) has no old lines yet.
        Side::Old => {
            let file_diff = file_diff.read(cx);
            file_diff
                .base_text_exists()
                .then(|| file_diff.base_text_buffer())
        }
    }
}

/// Vim's `:w` action, `vim::VimSave`, is private, so its type is taken from an instance: the
/// command palette's interceptor, which vim installs while it is enabled, makes one for `w`.
fn vim_save_action_type(workspace: WeakEntity<Workspace>, cx: &mut App) -> Option<TypeId> {
    struct VimSaveAction(TypeId);
    impl Global for VimSaveAction {}

    if let Some(action) = cx.try_global::<VimSaveAction>() {
        return Some(action.0);
    }
    let intercepted = GlobalCommandPaletteInterceptor::intercept("w", workspace, cx)?;
    let type_id = intercepted
        .now_or_never()?
        .results
        .iter()
        .find(|item| item.action.name() == "vim::VimSave")?
        .action
        .as_any()
        .type_id();
    cx.set_global(VimSaveAction(type_id));
    Some(type_id)
}

/// The anchor range of whole rows of a buffer, and their text. Text typed at the start of the
/// first row or at the end of the last one lands outside the range, so a line typed below the
/// last row doesn't become part of the comment.
fn line_anchor_and_text(
    buffer: &BufferSnapshot,
    rows: RangeInclusive<u32>,
) -> (std::ops::Range<language::Anchor>, String) {
    let start = Point::new(*rows.start(), 0);
    let end = Point::new(*rows.end(), buffer.line_len(*rows.end()));
    (
        buffer.anchor_after(start)..buffer.anchor_before(end),
        buffer.text_for_range(start..end).collect(),
    )
}

/// The lines starting where deleted lines were (`deleted_from`, collapsed by the deletion, or
/// whole again after an undo), when they have `line_text` again.
fn restored_lines(
    buffer: &BufferSnapshot,
    deleted_from: &Range<language::Anchor>,
    line_text: &str,
) -> Option<Range<language::Anchor>> {
    let start = deleted_from
        .start
        .to_offset(buffer)
        .min(deleted_from.end.to_offset(buffer));
    let start_row = buffer.offset_to_point(start).row;
    let line_count = u32::try_from(line_text.split('\n').count()).ok()?;
    let end_row = start_row.checked_add(line_count.saturating_sub(1))?;
    if end_row > buffer.max_point().row {
        return None;
    }
    let (range, text) = line_anchor_and_text(buffer, start_row..=end_row);
    (text == line_text).then_some(range)
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

/// Puts the file's header at the top of the view when the cursor is on the file's first row
/// (`row`, which the file's blocks sit above) and the header isn't there already. Returns whether
/// it scrolls.
fn show_file_start(editor: &mut Editor, row: Anchor, cx: &mut Context<Editor>) -> bool {
    // The very top shows the overview, or the first file's header already: nothing is under a
    // sticky header there.
    if editor.scroll_position(cx).y == 0. {
        return false;
    }
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let cursor_row = editor
        .selections
        .newest_anchor()
        .head()
        .to_point(&snapshot)
        .row;
    if cursor_row != row.to_point(&snapshot).row {
        return false;
    }
    let Some((header_row, row)) = file_header_row(editor, row, cx) else {
        return false;
    };
    if editor.scroll_position(cx).y == f64::from(header_row.0) {
        return false;
    }
    editor.request_autoscroll(
        Autoscroll::top_relative(f64::from(row.0 - header_row.0)),
        cx,
    );
    true
}

/// Keeps a view scrolled to the very top there. Zed anchors it on the first file's first row
/// (the file's header above that row is a block), so blocks above the row (a file's comments and
/// threads, and their heights, measured once rendered) would push the header and themselves
/// above the view's top, under the sticky header. The start of the multibuffer stays the top.
pub(crate) fn keep_scroll_at_start(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let position = editor.scroll_position(cx);
    if position.y == 0.
        && editor
            .scroll_manager
            .shared_scroll_anchor(cx)
            .scroll_anchor
            .anchor
            != Anchor::Min
    {
        anchor_scroll_at_start(editor, position.x, window, cx);
    }
}

/// Shows the overview block at the top of the right side: from its top when it fits in the
/// view, else with its bottom at the bottom of the view.
fn scroll_to_overview(
    editor: &mut Editor,
    block: CustomBlockId,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let Some(visible_lines) = editor.visible_line_count() else {
        return;
    };
    let snapshot = editor.display_snapshot(cx);
    let Some(height) = snapshot
        .blocks_in_range(DisplayRow(0)..DisplayRow(1))
        .find(|(_, overview)| overview.id() == BlockId::Custom(block))
        .map(|(_, overview)| overview.height())
    else {
        return;
    };
    let scroll_x = editor.scroll_position(cx).x;
    let top = f64::from(height) - visible_lines.floor();
    if top <= 0. {
        anchor_scroll_at_start(editor, scroll_x, window, cx);
    } else {
        editor.set_scroll_position(gpui::point(scroll_x, top), window, cx);
    }
}

/// After `r` on a file at the very top of the review: the cursor moved on to the next file and
/// the reviewed file folded to its header. Following the cursor would put the next file's header
/// at the top and the folded header out of view, so a view whose cursor row still fits (with the
/// scroll margin below it) stays at the top.
pub(crate) fn stay_at_start(editor: &mut Editor, cx: &mut Context<Editor>) {
    const ROWS_BELOW_CURSOR: u32 = 4;
    let Some(visible_lines) = editor.visible_line_count() else {
        return;
    };
    let snapshot = editor.display_snapshot(cx);
    let cursor = editor
        .selections
        .newest_anchor()
        .head()
        .to_point(snapshot.buffer_snapshot());
    let row = snapshot.point_to_display_point(cursor, Bias::Left).row();
    if row.0 + ROWS_BELOW_CURSOR <= visible_lines as u32 {
        editor.request_autoscroll(Autoscroll::top_relative(f64::from(row.0)), cx);
    }
}

fn anchor_scroll_at_start(
    editor: &mut Editor,
    scroll_x: f64,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    editor.set_scroll_anchor(
        ScrollAnchor {
            offset: gpui::point(scroll_x, 0.),
            anchor: Anchor::Min,
        },
        window,
        cx,
    );
}

/// Where the comment's block goes, if its lines (or file) are shown: line comments on the old
/// side on the left side of a split diff (or below the removed row of a unified one),
/// everything else on the right side.
fn placement(
    target: &CommentTarget,
    anchor: Option<&Range<language::Anchor>>,
    hosts: &Hosts,
    diff: &ReviewDiff,
    cx: &App,
) -> Option<(Host, Placement)> {
    match target {
        CommentTarget::Review => None,
        CommentTarget::Lines { .. } => {
            let (host, row) = line_row_anchor(target, anchor?, hosts, diff, cx)?;
            Some((
                host,
                Placement {
                    row,
                    is_file: false,
                    block: block_below_row(row, hosts.snapshot(host)?),
                },
            ))
        }
        CommentTarget::File { path } => {
            let anchor = hosts
                .rhs
                .buffer()
                .read(cx)
                .location_for_path(&review_path_key(path), cx)?;
            let (text_anchor, _) = hosts.rhs_snapshot.anchor_to_buffer_anchor(anchor)?;
            if hosts.rhs.is_buffer_folded(text_anchor.buffer_id, cx) {
                return None;
            }
            Some((
                Host::Rhs,
                Placement {
                    row: anchor,
                    is_file: true,
                    block: BlockAnchor::Above(anchor),
                },
            ))
        }
    }
}

/// The side showing a line comment's last line (`anchor` is where its lines are) and that row's
/// anchor there, while the row is shown (in an excerpt, in a file that isn't folded).
fn line_row_anchor(
    target: &CommentTarget,
    anchor: &Range<language::Anchor>,
    hosts: &Hosts,
    diff: &ReviewDiff,
    cx: &App,
) -> Option<(Host, Anchor)> {
    let path = target.path()?;
    let buffer_id = diff.buffer_id_for(&review_path_key(path), cx)?;
    // Folding the right side folds the left side too.
    if hosts.rhs.is_buffer_folded(buffer_id, cx) {
        return None;
    }
    line_row_in_excerpts(target, anchor, hosts, diff, cx)
}

/// Like [`line_row_anchor`], also while the file is folded: its excerpts are still there.
fn line_row_in_excerpts(
    target: &CommentTarget,
    anchor: &Range<language::Anchor>,
    hosts: &Hosts,
    diff: &ReviewDiff,
    cx: &App,
) -> Option<(Host, Anchor)> {
    let CommentTarget::Lines { path, side, .. } = target else {
        return None;
    };
    let buffer_id = diff.buffer_id_for(&review_path_key(path), cx)?;
    match (side, &hosts.lhs_snapshot) {
        (Side::New, _) => Some((Host::Rhs, hosts.rhs_snapshot.anchor_in_excerpt(anchor.end)?)),
        // The left side shows the diff base.
        (Side::Old, Some(lhs_snapshot)) => {
            Some((Host::Lhs, lhs_snapshot.anchor_in_excerpt(anchor.end)?))
        }
        (Side::Old, None) => Some((
            Host::Rhs,
            old_row_in_unified_diff(anchor.end, buffer_id, &hosts.rhs_snapshot)?,
        )),
    }
}

/// The row of a unified diff showing a row of the diff base: a removed row, or the unchanged row
/// it is in the new file.
fn old_row_in_unified_diff(
    base_anchor: language::Anchor,
    buffer_id: BufferId,
    snapshot: &MultiBufferSnapshot,
) -> Option<Anchor> {
    let buffer = snapshot.buffer_for_id(buffer_id)?;
    let file_diff = snapshot.diff_for_buffer_id(buffer_id)?;
    let base_text = file_diff.base_text();
    if !base_text.can_resolve(&base_anchor) {
        return None;
    }
    let row_start = Point::new(base_anchor.to_point(base_text).row, 0);
    let offset = row_start.to_offset(base_text);
    let removed_in = file_diff
        .hunks_intersecting_base_text_range(offset..offset, buffer)
        .find(|hunk| {
            hunk.diff_base_byte_range.start <= offset && offset < hunk.diff_base_byte_range.end
        });
    match removed_in {
        // Removed rows sit where the hunk starts in the new file, which is how the multibuffer
        // anchors them too.
        Some(hunk) => Some(
            snapshot
                .anchor_in_excerpt(buffer.anchor_before(hunk.range.start))?
                .with_diff_base_anchor(base_text.anchor_before(row_start)),
        ),
        // Right-biased: after a pure deletion the removed rows sit at the same place in the new
        // file, and a left-biased anchor would resolve to the first of them.
        None => {
            let point = file_diff.base_text_point_to_buffer_point(row_start, buffer);
            snapshot.anchor_in_excerpt(buffer.anchor_after(point))
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
    placement: Placement,
    view: AnyView,
    priority: usize,
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
        priority,
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

/// The row of `to` facing the row `row` of `from`, the other side of a split diff, unless a
/// spacer faces it.
fn facing_row(from: &Entity<Editor>, row: u32, to: &Entity<Editor>, cx: &mut App) -> Option<u32> {
    let display_row = from.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        snapshot
            .point_to_display_point(Point::new(row, 0), Bias::Left)
            .row()
    });
    to.update(cx, |editor, cx| {
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

/// The lines `c` comments on (or visual `y` copies), on the side of the diff `editor` shows:
/// `rows` (multibuffer rows), or the selection's ([`lines_in_rows`]).
fn selected_lines(
    editor: &mut Editor,
    rows: Option<RangeInclusive<u32>>,
    is_lhs: bool,
    rhs: &Entity<Editor>,
    diff: &Entity<ReviewDiff>,
    cx: &mut Context<Editor>,
) -> Option<SelectedLines> {
    let display_snapshot = editor.display_snapshot(cx);
    let diff = diff.read(cx);
    let snapshot = display_snapshot.buffer_snapshot();
    // The anchor, not the resolved selection: on a folded header that spans the whole file.
    let head = match &rows {
        Some(rows) => snapshot.anchor_before(Point::new(*rows.end(), 0)),
        None => editor.selections.newest_anchor().head(),
    };
    let (head_anchor, _) = snapshot.anchor_to_buffer_anchor(head)?;
    // Folds live on the right side, by the file's buffer; the left side shows the diff bases.
    let is_folded = if is_lhs {
        diff.base_buffer(head_anchor.buffer_id, cx)
            .and_then(|(path, _)| diff.buffer_id_for(&review_path_key(&path), cx))
            .is_some_and(|buffer_id| rhs.read(cx).is_buffer_folded(buffer_id, cx))
    } else {
        editor.is_buffer_folded(head_anchor.buffer_id, cx)
    };
    if is_folded {
        return Some(SelectedLines::FoldedFile(head_anchor.buffer_id));
    }
    let rows = rows.unwrap_or_else(|| selected_rows(editor, &display_snapshot));
    lines_in_rows(snapshot, rows, is_lhs)
}

/// The lines of one file that `rows` (multibuffer rows of a side of the diff) cover: those of
/// the file at the last row. A selection over the removed and added lines of a unified diff
/// takes the added ones.
fn lines_in_rows(
    snapshot: &MultiBufferSnapshot,
    rows: RangeInclusive<u32>,
    is_lhs: bool,
) -> Option<SelectedLines> {
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

/// Puts the cursor (an empty selection, so vim is in normal mode) at the start of a multibuffer
/// row, without scrolling.
fn move_cursor_to_row(editor: &Entity<Editor>, row: u32, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        let point = Point::new(row, 0);
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([point..point])
        });
    });
}

/// Collapses the selection onto its first row's start, where vim leaves the cursor after a
/// yank.
fn collapse_selection_to_start(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let start = editor.selections.newest_anchor().start.to_point(&snapshot);
        let point = Point::new(start.row, 0);
        editor.change_selections(
            SelectionEffects::scroll(Autoscroll::fit()),
            window,
            cx,
            |selections| selections.select_ranges([point..point]),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ReviewView,
        test_support::{
            active_review, add_review_panel, cursor_line, cursor_point, dispatch, excerpt_paths,
            init_vim_test, is_folded, lhs_editor, move_to_row_with_text, open_review_fixture,
            path_at_cursor, review_count, rhs_editor, row_text, splittable,
        },
    };
    use fs::Fs as _;
    use git::repository::RepoPath;
    use gpui::{Action as _, TestAppContext, VisualTestContext};
    use settings::DiffViewStyle;
    use std::path::Path;
    use util::path;
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

    fn old_lines(path: &str, lines: RangeInclusive<u32>) -> CommentTarget {
        CommentTarget::Lines {
            path: RepoPath::new(path).unwrap(),
            side: Side::Old,
            lines,
        }
    }

    /// Each hosted comment's block row, and whether it is on the left side.
    fn hosted_on_sides(
        review: &Entity<ReviewView>,
        cx: &mut VisualTestContext,
    ) -> Vec<(CommentId, bool, u32)> {
        let blocks = blocks(review, cx);
        cx.update(|_, cx| blocks.update(cx, |blocks, cx| blocks.hosted_rows_on_sides(cx)))
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
            "escape : w ! enter",
            "escape v : w enter",
            "escape : % w space x enter",
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

        // Removed lines alone are commented on as old lines.
        move_to_row_with_text(&rhs, "gone 1", cx);
        cx.simulate_keystrokes("v j c");
        cx.simulate_input("removed");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&rhs, cx);
        assert_eq!(
            comments(&review, cx)[2],
            (old_lines("deleted.txt", 1..=2), "removed".to_string(), None)
        );
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

        // The line comments of a reviewed (folded) file are visited too, unfolding the file
        // (V3.15).
        move_to_row_with_text(&rhs, "line 1", cx);
        cx.simulate_keystrokes("r");
        assert!(is_folded(&review, "a.rs", cx));
        let a_rs = review.read_with(cx, |review, cx| {
            let diff = review.diff().read(cx);
            diff.buffer_id_for(&review_path_key(&RepoPath::new("a.rs").unwrap()), cx)
                .unwrap()
        });
        for (start, keys, body) in [
            ("new 1", "[ t", "line 36"),
            ("new 1", "shift-m", "line 36"),
            ("three", "] t ] t", "line 3"),
            ("three", "m m", "line 3"),
        ] {
            rhs.update(cx, |editor, cx| editor.fold_buffer(a_rs, cx));
            cx.run_until_parked();
            move_to_row_with_text(&rhs, start, cx);
            cx.simulate_keystrokes(keys);
            assert_eq!(targeted(cx).as_deref(), Some(body), "after {keys}");
            assert_eq!(cursor_line(&rhs, cx), body, "after {keys}");
            assert!(!is_folded(&review, "a.rs", cx), "after {keys}");
            assert_back_in_diff(&rhs, cx);
        }
        assert!(review.read_with(cx, |review, cx| {
            review
                .session()
                .read(cx)
                .is_file_reviewed(&RepoPath::new("a.rs").unwrap())
        }));
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

        // The left side comments on the old lines, in a block on the left side.
        cx.simulate_keystrokes("ctrl-w h");
        let lhs = lhs_editor(&review, cx);
        assert!(splittable(&review, cx).read_with(cx, |editor, _| editor.focused_editor() == &lhs));
        cx.simulate_keystrokes("c");
        cx.simulate_input("old two");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&lhs, cx);
        assert_eq!(
            comments(&review, cx)[3],
            (old_lines("a.rs", 2..=2), "old two".to_string(), None)
        );
        // Below the line, after the spacer facing the right side's block below the same row.
        let old_comment = comment_ids(&review, cx)[3];
        let lhs_row = move_to_row_with_text(&lhs, "line 2", cx);
        let below_line = display_row(&lhs, lhs_row, cx) + 1;
        let hosted = hosted_on_sides(&review, cx);
        assert!(
            hosted
                .iter()
                .any(|(id, on_lhs, row)| *id == old_comment && *on_lhs && *row > below_line),
            "{hosted:?}"
        );
        // `i` on the left side edits the comment of the focused side first, then the one facing
        // the cursor row.
        cx.simulate_keystrokes("i");
        assert_eq!(text(&open_editor(&review, cx).unwrap(), cx), "old two");
        cx.simulate_keystrokes("escape escape");
        assert_back_in_diff(&lhs, cx);
        move_to_row_with_text(&lhs, "line 3", cx);
        cx.simulate_keystrokes("i");
        assert_eq!(text(&open_editor(&review, cx).unwrap(), cx), "line 3");
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

    #[gpui::test]
    async fn test_old_side_comments_in_unified_view(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        // `c` on a removed row comments on the old line, in a block below that row.
        let row = move_to_row_with_text(&rhs, "line 2", cx);
        cx.simulate_keystrokes("c");
        assert_eq!(
            vim_mode(&open_editor(&review, cx).unwrap(), cx).as_deref(),
            Some("insert")
        );
        cx.simulate_input("old");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&rhs, cx);
        let old_comment = comment_ids(&review, cx)[0];
        assert_eq!(
            comments(&review, cx),
            [(old_lines("a.rs", 2..=2), "old".to_string(), None)]
        );
        assert_eq!(
            hosted_rows(&review, cx),
            [(old_comment, display_row(&rhs, row, cx) + 1)]
        );
        blocks(&review, cx).read_with(cx, |blocks, cx| {
            let view = blocks.view(old_comment).unwrap().read(cx);
            assert_eq!(view.location().as_deref(), Some("Old line 2"));
        });

        // The added line replacing it gets its own comment.
        move_to_row_with_text(&rhs, "LINE 2", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("new");
        cx.simulate_keystrokes("ctrl-enter");
        let new_comment = comment_ids(&review, cx)[1];

        // Old lines come first where both sides are at the same place.
        let targeted = |cx: &mut VisualTestContext| {
            let blocks = blocks(&review, cx);
            cx.update(|_, cx| blocks.update(cx, |blocks, cx| blocks.comment_at_cursor(cx)))
        };
        move_to_row_with_text(&rhs, "line 1", cx);
        cx.simulate_keystrokes("] t");
        assert_eq!(targeted(cx), Some(old_comment));
        assert_eq!(cursor_line(&rhs, cx), "line 2");
        cx.simulate_keystrokes("] t");
        assert_eq!(targeted(cx), Some(new_comment));
        cx.simulate_keystrokes("[ t");
        assert_eq!(targeted(cx), Some(old_comment));

        // `i` edits it; `dd` deletes it.
        cx.simulate_keystrokes("i");
        assert_eq!(text(&open_editor(&review, cx).unwrap(), cx), "old");
        cx.simulate_keystrokes("escape escape");
        cx.simulate_keystrokes("d d");
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert_eq!(comment_ids(&review, cx), [new_comment]);

        // A whole deleted file.
        move_to_row_with_text(&rhs, "gone 2", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("gone");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            comments(&review, cx)[1],
            (old_lines("deleted.txt", 2..=2), "gone".to_string(), None)
        );
    }

    #[gpui::test]
    async fn test_old_side_comments_follow_split_toggles(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Split);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let on_sides = |cx: &mut VisualTestContext| {
            hosted_on_sides(&review, cx)
                .into_iter()
                .map(|(_, on_lhs, _)| on_lhs)
                .collect::<Vec<_>>()
        };

        move_to_row_with_text(&rhs, "LINE 2", cx);
        cx.simulate_keystrokes("ctrl-w h");
        let lhs = lhs_editor(&review, cx);
        move_to_row_with_text(&lhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("mid-edit");
        let editor = open_editor(&review, cx).unwrap();
        assert_eq!(on_sides(cx), [true]);

        // Unsplitting moves the block below the row in the unified diff, and splitting again
        // into the new left side. The editor stays open with its text, and keeps the focus.
        for (split, on_lhs) in [(false, false), (true, true), (false, false), (true, true)] {
            cx.update(|window, cx| {
                editor
                    .focus_handle(cx)
                    .dispatch_action(&editor::ToggleSplitDiff, window, cx)
            });
            cx.run_until_parked();
            assert_eq!(
                splittable(&review, cx).read_with(cx, |editor, _| editor.is_split()),
                split
            );
            assert_eq!(on_sides(cx), [on_lhs], "split: {split}");
            assert_eq!(open_editor(&review, cx), Some(editor.clone()));
            assert_eq!(text(&editor, cx), "mid-edit");
            assert!(is_focused(&editor, cx), "split: {split}");
        }
        assert_ne!(lhs_editor(&review, cx), lhs);

        // So does a pane too narrow for a split diff, and wide again.
        let wide = cx.update(|window, _| window.bounds().size);
        cx.simulate_resize(gpui::size(px(300.), wide.height));
        cx.run_until_parked();
        assert!(!splittable(&review, cx).read_with(cx, |editor, _| editor.is_split()));
        assert_eq!(on_sides(cx), [false]);
        let unified_row = move_to_row_with_text(&rhs, "line 3", cx);
        assert_eq!(
            hosted_rows(&review, cx)[0].1,
            display_row(&rhs, unified_row, cx) + 1
        );
        cx.simulate_resize(wide);
        cx.run_until_parked();
        assert!(splittable(&review, cx).read_with(cx, |editor, _| editor.is_split()));
        assert_eq!(on_sides(cx), [true]);
        assert_eq!(text(&editor, cx), "mid-edit");

        // The left side the comment was opened from is gone: the new one gets the focus, on the
        // comment's row.
        cx.focus(&editor);
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            comments(&review, cx),
            [(old_lines("a.rs", 3..=3), "mid-edit".to_string(), None)]
        );
        let new_lhs = lhs_editor(&review, cx);
        assert_back_in_diff(&new_lhs, cx);
        assert_eq!(cursor_line(&new_lhs, cx), "line 3");

        // Hidden and folded files hide the left side's blocks too.
        cx.simulate_keystrokes("] f ; f");
        assert_eq!(excerpt_paths(&review, cx), ["added.txt"]);
        assert!(hosted_on_sides(&review, cx).is_empty());
        cx.simulate_keystrokes("; f");
        assert_eq!(on_sides(cx), [true]);
        let buffer_id = review.read_with(cx, |review, cx| {
            let diff = review.diff().read(cx);
            diff.buffer_id_for(&review_path_key(&RepoPath::new("a.rs").unwrap()), cx)
                .unwrap()
        });
        rhs.update(cx, |editor, cx| editor.fold_buffer(buffer_id, cx));
        cx.run_until_parked();
        assert!(hosted_on_sides(&review, cx).is_empty());
        rhs.update(cx, |editor, cx| editor.unfold_buffer(buffer_id, cx));
        cx.run_until_parked();
        assert_eq!(on_sides(cx), [true]);
    }

    /// A line break in a comment is a line break in its card, as on GitHub (V3.02).
    #[gpui::test]
    async fn test_comment_lines_render_as_lines(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let card_height = |line: usize, body: &str, cx: &mut VisualTestContext| {
            move_to_row_with_text(&rhs, &format!("line {line}"), cx);
            cx.simulate_keystrokes("c");
            cx.simulate_input(body);
            cx.simulate_keystrokes("ctrl-enter");
            cx.run_until_parked();
            let id = *comment_ids(&review, cx).last().unwrap();
            let block_row = hosted_rows(&review, cx)
                .into_iter()
                .find(|(hosted, _)| *hosted == id)
                .unwrap()
                .1;
            let next_row = move_to_row_with_text(&rhs, &format!("line {}", line + 1), cx);
            display_row(&rhs, next_row, cx) - block_row
        };
        let one_line = card_height(3, "one", cx);
        let three_lines = card_height(36, "one\ntwo\nthree", cx);
        assert_eq!(three_lines, one_line + 2);
    }

    /// In a unified diff, an old-side comment on the unchanged row right after a pure deletion
    /// sits below that row, not below the removed row facing the same place in the new file,
    /// and `]t` visits it after the removed row's comment (V7.X3).
    #[gpui::test]
    async fn test_old_side_comment_after_a_pure_deletion(cx: &mut TestAppContext) {
        use crate::test_support::{dot_git, open_workspace};
        use project::FakeFs;

        init(cx, DiffViewStyle::Split);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            serde_json::json!({ ".git": {}, "a.txt": "one\nthree\nfour\n" }),
        )
        .await;
        fs.set_head_and_index_for_repo(dot_git(), &[("a.txt", "one\ntwo\nthree\nfour\n".into())]);
        let (workspace, cx) = open_workspace(fs, cx).await;
        dispatch(crate::ReviewUncommitted, cx);
        let review = active_review(&workspace, cx);
        cx.focus(&review);
        cx.run_until_parked();
        let rhs = rhs_editor(&review, cx);

        cx.simulate_keystrokes("ctrl-w h");
        let lhs = lhs_editor(&review, cx);
        for text in ["three", "two"] {
            move_to_row_with_text(&lhs, text, cx);
            cx.simulate_keystrokes("c");
            cx.simulate_input(text);
            cx.simulate_keystrokes("ctrl-enter");
        }
        assert_eq!(
            comments(&review, cx),
            [
                (old_lines("a.txt", 3..=3), "three".to_string(), None),
                (old_lines("a.txt", 2..=2), "two".to_string(), None),
            ]
        );
        let ids = comment_ids(&review, cx);

        cx.update(|window, cx| {
            lhs.focus_handle(cx)
                .dispatch_action(&editor::ToggleSplitDiff, window, cx)
        });
        cx.run_until_parked();
        assert!(!splittable(&review, cx).read_with(cx, |editor, _| editor.is_split()));
        let three_row = move_to_row_with_text(&rhs, "three", cx);
        let two_row = move_to_row_with_text(&rhs, "two", cx);
        let mut hosted = hosted_rows(&review, cx);
        hosted.sort();
        assert_eq!(
            hosted,
            [
                (ids[0], display_row(&rhs, three_row, cx) + 1),
                (ids[1], display_row(&rhs, two_row, cx) + 1),
            ]
        );

        let targeted = |cx: &mut VisualTestContext| {
            let blocks = blocks(&review, cx);
            cx.update(|_, cx| blocks.update(cx, |blocks, cx| blocks.comment_at_cursor(cx)))
        };
        cx.simulate_keystrokes("g g ] t");
        assert_eq!(targeted(cx), Some(ids[1]));
        assert_eq!(cursor_line(&rhs, cx), "two");
        cx.simulate_keystrokes("] t");
        assert_eq!(targeted(cx), Some(ids[0]));
        assert_eq!(cursor_line(&rhs, cx), "three");
        cx.simulate_keystrokes("[ t");
        assert_eq!(targeted(cx), Some(ids[1]));
    }

    /// A review opened from outside its pane (the panel here, or the palette) gets the focus
    /// while it is still loading, so the pane remembers the diff's loading placeholder as the
    /// tab's focus. Closing a split clone focuses that pane again, which must end in the diff
    /// (V2.15+; the fold the validation saw is incidental).
    async fn close_split_clone(fold: bool, cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        cx.simulate_keystrokes(": q enter");
        cx.run_until_parked();
        assert_eq!(review_count(&workspace, cx), 0);
        drop(review);
        cx.focus(&panel);
        dispatch(crate::ReviewUncommitted, cx);
        let review = active_review(&workspace, cx);
        let rhs = rhs_editor(&review, cx);
        assert!(is_focused(&rhs, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let split = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_and_clone(pane, workspace::SplitDirection::Right, window, cx)
        });
        cx.run_until_parked();
        split.await.unwrap();
        let clone = active_review(&workspace, cx);
        assert_ne!(clone, review);
        assert!(is_focused(&rhs_editor(&clone, cx), cx));

        if fold {
            cx.simulate_keystrokes("g g r");
            assert!(is_folded(&review, "a.rs", cx));
        }
        cx.simulate_keystrokes(": q enter");
        cx.run_until_parked();
        assert_eq!(active_review(&workspace, cx), review);
        assert!(is_focused(&rhs, cx), "fold: {fold}");
        assert_eq!(
            vim_mode(&rhs, cx).as_deref(),
            Some("normal"),
            "fold: {fold}"
        );
        let before = cursor_point(&rhs, cx);
        cx.simulate_keystrokes("j");
        assert_ne!(cursor_point(&rhs, cx), before, "fold: {fold}");
    }

    #[gpui::test]
    async fn test_closing_a_split_clone_keeps_focus(cx: &mut TestAppContext) {
        close_split_clone(false, cx).await;
    }

    #[gpui::test]
    async fn test_closing_a_split_clone_after_a_fold_keeps_focus(cx: &mut TestAppContext) {
        close_split_clone(true, cx).await;
    }

    /// Unsplitting while the left side has the focus (a pane too narrow for a split diff, or
    /// `ToggleSplitDiff`) drops that editor: the right side takes the focus (V7.X1).
    #[gpui::test]
    async fn test_dropping_the_focused_left_side_focuses_the_right_side(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Split);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let is_split = |cx: &mut VisualTestContext| {
            splittable(&review, cx).read_with(cx, |editor, _| editor.is_split())
        };
        let assert_keys_work = |cx: &mut VisualTestContext| {
            assert_back_in_diff(&rhs, cx);
            let before = cursor_point(&rhs, cx);
            cx.simulate_keystrokes("j");
            assert_ne!(cursor_point(&rhs, cx), before);
            cx.simulate_keystrokes("k");
        };

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("ctrl-w h");
        assert!(is_focused(&lhs_editor(&review, cx), cx));
        let wide = cx.update(|window, _| window.bounds().size);
        cx.simulate_resize(gpui::size(px(300.), wide.height));
        cx.run_until_parked();
        assert!(!is_split(cx));
        assert_keys_work(cx);

        cx.simulate_resize(wide);
        cx.run_until_parked();
        assert!(is_split(cx));
        cx.simulate_keystrokes("ctrl-w h");
        let lhs = lhs_editor(&review, cx);
        assert!(is_focused(&lhs, cx));
        cx.update(|window, cx| {
            lhs.focus_handle(cx)
                .dispatch_action(&editor::ToggleSplitDiff, window, cx)
        });
        cx.run_until_parked();
        assert!(!is_split(cx));
        assert_keys_work(cx);
    }

    #[gpui::test]
    async fn test_line_numbers_follow_edits(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("v j c");
        cx.simulate_input("range");
        cx.simulate_keystrokes("ctrl-enter");
        let id = comment_ids(&review, cx)[0];
        let label = |cx: &mut VisualTestContext| {
            blocks(&review, cx).read_with(cx, |blocks, cx| {
                blocks.view(id).unwrap().read(cx).location()
            })
        };
        assert_eq!(label(cx).as_deref(), Some("Lines 3–4"));

        // Lines added above the comment (in another editor, say) move it.
        let buffer = review.read_with(cx, |review, cx| {
            review
                .diff()
                .read(cx)
                .display_buffer(&review_path_key(&RepoPath::new("a.rs").unwrap()))
                .unwrap()
                .clone()
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "new 1\nnew 2\n")], None, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            comments(&review, cx),
            [(lines("a.rs", 5..=6), "range".to_string(), None)]
        );
        assert_eq!(label(cx).as_deref(), Some("Lines 5–6"));
        // Editing the commented lines keeps their text current.
        buffer.update(cx, |buffer, cx| {
            let offset = buffer.text().find("line 3").unwrap();
            buffer.edit([(offset..offset + 4, "LINE")], None, cx)
        });
        cx.run_until_parked();
        let line_text = review.read_with(cx, |review, cx| {
            review
                .session()
                .read(cx)
                .comments()
                .get(id)
                .unwrap()
                .line_text
                .clone()
        });
        assert_eq!(line_text.as_deref(), Some("LINE 3\nline 4"));
    }

    /// The comment's lines, and whether it is outdated.
    fn line_state(
        review: &Entity<ReviewView>,
        id: CommentId,
        cx: &VisualTestContext,
    ) -> (CommentTarget, bool) {
        review.read_with(cx, |review, cx| {
            let comment = review.session().read(cx).comments().get(id).unwrap();
            (comment.target.clone(), comment.is_outdated())
        })
    }

    fn display_buffer(
        review: &Entity<ReviewView>,
        path: &str,
        cx: &VisualTestContext,
    ) -> Entity<Buffer> {
        review.read_with(cx, |review, cx| {
            review
                .diff()
                .read(cx)
                .display_buffer(&review_path_key(&RepoPath::new(path).unwrap()))
                .unwrap()
                .clone()
        })
    }

    #[gpui::test]
    async fn test_deleted_lines_make_comments_outdated(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("v j c");
        cx.simulate_input("range");
        cx.simulate_keystrokes("ctrl-enter");
        let id = comment_ids(&review, cx)[0];
        let buffer = display_buffer(&review, "a.rs", cx);
        let commented = "line 3\nline 4\n";
        let offset_of = |buffer: &Buffer| buffer.text().find(commented).unwrap();

        // The lines are deleted and put back before the review notices (a stash and a pop): the
        // comment stays on both lines.
        buffer.update(cx, |buffer, cx| {
            let offset = offset_of(buffer);
            buffer.edit([(offset..offset + commented.len(), "")], None, cx);
            buffer.edit([(offset..offset, commented)], None, cx);
        });
        cx.run_until_parked();
        assert_eq!(line_state(&review, id, cx), (lines("a.rs", 3..=4), false));
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "new\n")], None, cx));
        cx.run_until_parked();
        assert_eq!(line_state(&review, id, cx), (lines("a.rs", 4..=5), false));

        // Deleting them makes the comment outdated, on the lines with its numbers, rather than
        // moving it onto the next line.
        buffer.update(cx, |buffer, cx| {
            let offset = offset_of(buffer);
            buffer.edit([(offset..offset + commented.len(), "")], None, cx);
        });
        cx.run_until_parked();
        assert_eq!(line_state(&review, id, cx), (lines("a.rs", 4..=5), true));
        review.read_with(cx, |review, cx| {
            let comment = review.session().read(cx).comments().get(id).unwrap();
            assert_eq!(comment.line_text.as_deref(), Some("line 3\nline 4"));
        });
        blocks(&review, cx).read_with(cx, |blocks, cx| {
            assert!(blocks.view(id).unwrap().read(cx).is_outdated());
        });

        // A single commented line too.
        move_to_row_with_text(&rhs, "line 1", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("single");
        cx.simulate_keystrokes("ctrl-enter");
        let single = comment_ids(&review, cx)[1];
        assert_eq!(
            line_state(&review, single, cx),
            (lines("a.rs", 2..=2), false)
        );
        buffer.update(cx, |buffer, cx| {
            let offset = buffer.text().find("line 1\n").unwrap();
            buffer.edit([(offset..offset + "line 1\n".len(), "")], None, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            line_state(&review, single, cx),
            (lines("a.rs", 2..=2), true)
        );
    }

    #[gpui::test]
    async fn test_outdated_comments_come_back_with_their_text(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("v j c");
        cx.simulate_input("range");
        cx.simulate_keystrokes("ctrl-enter");
        move_to_row_with_text(&rhs, "line 1", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("single");
        cx.simulate_keystrokes("ctrl-enter");
        let [range, single] = comment_ids(&review, cx)[..] else {
            panic!("two comments expected");
        };
        let buffer = display_buffer(&review, "a.rs", cx);
        // Each edit undoes on its own.
        buffer.update(cx, |buffer, _| {
            buffer.set_group_interval(std::time::Duration::ZERO)
        });
        let commented = "line 3\nline 4\n";
        let offset_of = |buffer: &Buffer, text: &str| buffer.text().find(text).unwrap();
        let line_text = |cx: &VisualTestContext| {
            review.read_with(cx, |review, cx| {
                let session = review.session().read(cx);
                session.comments().get(range).unwrap().line_text.clone()
            })
        };

        // A stash takes the lines away, its pop puts them back where they were: the comment
        // is current again, on its lines (not below them, where its anchor on the line numbers
        // went).
        let offset = buffer.read_with(cx, |buffer, _| offset_of(buffer, commented));
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(offset..offset + commented.len(), "")], None, cx)
        });
        cx.run_until_parked();
        assert_eq!(line_state(&review, range, cx), (lines("a.rs", 3..=4), true));
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(offset..offset, commented)], None, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            line_state(&review, range, cx),
            (lines("a.rs", 3..=4), false)
        );
        assert_eq!(line_text(cx).as_deref(), Some("line 3\nline 4"));
        blocks(&review, cx).read_with(cx, |blocks, cx| {
            assert!(!blocks.view(range).unwrap().read(cx).is_outdated());
        });

        // The text coming back somewhere else doesn't count.
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(offset..offset + commented.len(), "")], None, cx)
        });
        cx.run_until_parked();
        assert_eq!(line_state(&review, range, cx), (lines("a.rs", 3..=4), true));
        buffer.update(cx, |buffer, cx| {
            let end = buffer.len();
            buffer.edit([(end..end, commented)], None, cx)
        });
        cx.run_until_parked();
        assert_eq!(line_state(&review, range, cx), (lines("a.rs", 3..=4), true));
        assert_eq!(line_text(cx).as_deref(), Some("line 3\nline 4"));

        // Nor does other text at the place; an undo of the deletion does.
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(offset..offset, "other\n")], None, cx)
        });
        cx.run_until_parked();
        assert!(line_state(&review, range, cx).1);
        buffer.update(cx, |buffer, cx| {
            buffer.undo(cx);
            buffer.undo(cx);
            buffer.undo(cx);
        });
        cx.run_until_parked();
        buffer.read_with(cx, |buffer, _| {
            assert_eq!(offset_of(buffer, commented), offset);
        });
        assert_eq!(
            line_state(&review, range, cx),
            (lines("a.rs", 3..=4), false)
        );

        // A replaced line, and the line put back as it was.
        let original = "line 1";
        let offset = buffer.read_with(cx, |buffer, _| offset_of(buffer, "line 1\n"));
        let replace = |text: &str, length: usize, cx: &mut VisualTestContext| {
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(offset..offset + length, text)], None, cx)
            });
            cx.run_until_parked();
        };
        replace("changed 1", original.len(), cx);
        assert_eq!(
            line_state(&review, single, cx),
            (lines("a.rs", 1..=1), true)
        );
        replace(original, "changed 1".len(), cx);
        assert_eq!(
            line_state(&review, single, cx),
            (lines("a.rs", 1..=1), false)
        );
    }

    /// A stash and its pop in an uncommitted review: the file leaves the review and comes back.
    #[gpui::test]
    async fn test_comments_survive_a_stash_and_pop(cx: &mut TestAppContext) {
        use crate::test_support::{dot_git, line, open_workspace};
        use project::FakeFs;

        init(cx, DiffViewStyle::Unified);
        let head: String = (1..=20).map(line).collect();
        let working = head
            .replace(&line(10), "line 10\nadded A\nadded B\n")
            .replace(&line(15), "line FIFTEEN\n");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            serde_json::json!({ ".git": {}, "x.rs": working.clone() }),
        )
        .await;
        fs.set_head_and_index_for_repo(dot_git(), &[("x.rs", head.clone())]);
        let (workspace, cx) = open_workspace(fs.clone(), cx).await;
        crate::test_support::dispatch(crate::ReviewUncommitted, cx);
        let review = crate::test_support::active_review(&workspace, cx);
        cx.focus(&review);
        cx.run_until_parked();
        let rhs = rhs_editor(&review, cx);
        for (row_text, body) in [("added A", "added"), ("line FIFTEEN", "fifteen")] {
            move_to_row_with_text(&rhs, row_text, cx);
            cx.simulate_keystrokes("c");
            cx.simulate_input(body);
            cx.simulate_keystrokes("ctrl-enter");
        }
        let [added, fifteen] = comment_ids(&review, cx)[..] else {
            panic!("two comments expected");
        };
        assert_eq!(
            line_state(&review, added, cx),
            (lines("x.rs", 11..=11), false)
        );
        assert_eq!(
            line_state(&review, fifteen, cx),
            (lines("x.rs", 17..=17), false)
        );

        // The stash: the file has no changes left and leaves the review.
        fs.insert_file(path!("/project/x.rs"), head.into_bytes())
            .await;
        cx.run_until_parked();
        assert_eq!(hosted_rows(&review, cx).len(), 0);

        // The pop: it comes back, with its comments on their lines and current.
        fs.insert_file(path!("/project/x.rs"), working.into_bytes())
            .await;
        cx.run_until_parked();
        assert_eq!(
            line_state(&review, added, cx),
            (lines("x.rs", 11..=11), false)
        );
        assert_eq!(
            line_state(&review, fifteen, cx),
            (lines("x.rs", 17..=17), false)
        );
        assert_eq!(hosted_rows(&review, cx).len(), 2);
    }

    /// Two projects on one repository (its root and a subfolder, in two windows) review the
    /// same thing, so they share the review's comments, but each has its own buffers.
    #[gpui::test]
    async fn test_two_projects_review_the_same_repository(cx: &mut TestAppContext) {
        use crate::test_support::{dot_git, line};
        use project::{FakeFs, Project};
        use workspace::MultiWorkspace;

        init(cx, DiffViewStyle::Unified);
        let head: String = (1..=10).map(line).collect();
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            serde_json::json!({
                ".git": {},
                "sub": { "x.rs": head.replace(&line(5), "LINE 5\n") },
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(dot_git(), &[("sub/x.rs", head)]);

        let mut windows = Vec::new();
        for root in [path!("/project"), path!("/project/sub")] {
            let project = Project::test(fs.clone(), [Path::new(root)], cx).await;
            let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
            let workspace = window
                .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
                .unwrap();
            windows.push((workspace, VisualTestContext::from_window(window.into(), cx)));
        }
        let [(workspace_a, cx_a), (workspace_b, cx_b)] = &mut windows[..] else {
            unreachable!()
        };
        let open_review = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.focus(workspace);
            cx.update(|window, cx| {
                window.dispatch_action(crate::ReviewUncommitted.boxed_clone(), cx)
            });
            cx.run_until_parked();
            let review = workspace.update(cx, |workspace, cx| {
                workspace.active_item_as::<ReviewView>(cx).unwrap()
            });
            cx.focus(&review);
            cx.run_until_parked();
            review
        };
        let review_a = open_review(workspace_a, cx_a);
        let review_b = open_review(workspace_b, cx_b);
        let key = |review: &Entity<ReviewView>, cx: &VisualTestContext| {
            review.read_with(cx, |review, cx| review.session().read(cx).key().clone())
        };
        assert_eq!(key(&review_a, cx_a), key(&review_b, cx_b));

        let rhs_a = rhs_editor(&review_a, cx_a);
        move_to_row_with_text(&rhs_a, "line 4", cx_a);
        cx_a.simulate_keystrokes("c");
        cx_a.simulate_input("shared");
        cx_a.simulate_keystrokes("ctrl-enter");
        cx_b.run_until_parked();
        let id = comment_ids(&review_a, cx_a)[0];
        assert_eq!(hosted_rows(&review_b, cx_b).len(), 1);
        assert_eq!(
            line_state(&review_a, id, cx_a),
            (lines("sub/x.rs", 4..=4), false)
        );

        // The commented line deleted in one window outdates the comment; the other window,
        // whose buffer still has the line, leaves it outdated. Putting the line back makes it
        // current again, in both.
        let buffer_a = display_buffer(&review_a, "sub/x.rs", cx_a);
        let deleted = "line 4\n";
        let offset = buffer_a.read_with(cx_a, |buffer, _| buffer.text().find(deleted).unwrap());
        buffer_a.update(cx_a, |buffer, cx| {
            buffer.edit([(offset..offset + deleted.len(), "")], None, cx)
        });
        cx_a.run_until_parked();
        cx_b.run_until_parked();
        cx_a.run_until_parked();
        assert!(line_state(&review_a, id, cx_a).1);
        assert!(line_state(&review_b, id, cx_b).1);
        buffer_a.update(cx_a, |buffer, cx| {
            buffer.edit([(offset..offset, deleted)], None, cx)
        });
        cx_a.run_until_parked();
        cx_b.run_until_parked();
        cx_a.run_until_parked();
        assert_eq!(
            line_state(&review_b, id, cx_b),
            (lines("sub/x.rs", 4..=4), false)
        );

        // An unsaved edit in one window moves the comment there; the other window's buffer
        // doesn't have it and doesn't move the comment back.
        let buffer_b = display_buffer(&review_b, "sub/x.rs", cx_b);
        assert_ne!(buffer_a.entity_id(), buffer_b.entity_id());
        buffer_a.update(cx_a, |buffer, cx| buffer.edit([(0..0, "new\n")], None, cx));
        cx_a.run_until_parked();
        assert_eq!(
            line_state(&review_b, id, cx_b),
            (lines("sub/x.rs", 5..=5), false)
        );
        buffer_b.update(cx_b, |buffer, cx| {
            let end = buffer.len();
            buffer.edit([(end..end, "tail\n")], None, cx)
        });
        cx_b.run_until_parked();
        assert_eq!(
            line_state(&review_b, id, cx_b),
            (lines("sub/x.rs", 5..=5), false)
        );
        assert_eq!(hosted_rows(&review_a, cx_a).len(), 1);
        assert_eq!(hosted_rows(&review_b, cx_b).len(), 1);

        // Both reopen the stored comment: the window whose buffer has the edit checks it first,
        // and the other one doesn't flag it outdated for its own buffer's text.
        for (workspace, review, cx) in [
            (&*workspace_a, &review_a, &mut *cx_a),
            (&*workspace_b, &review_b, &mut *cx_b),
        ] {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_item_by_id(
                        review.entity_id(),
                        workspace::SaveIntent::Skip,
                        window,
                        cx,
                    )
                    .detach_and_log_err(cx)
                })
            });
            cx.run_until_parked();
        }
        drop((review_a, review_b, rhs_a));
        let review_a = open_review(workspace_a, cx_a);
        let review_b = open_review(workspace_b, cx_b);
        let id = comment_ids(&review_a, cx_a)[0];
        assert_eq!(
            line_state(&review_a, id, cx_a),
            (lines("sub/x.rs", 5..=5), false)
        );
        assert_eq!(hosted_rows(&review_a, cx_a).len(), 1);
        assert_eq!(hosted_rows(&review_b, cx_b).len(), 1);

        drop((buffer_a, buffer_b));
    }

    #[gpui::test]
    async fn test_comments_come_back_after_reopening(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (fs, workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        for (row_text, keys, body) in [
            ("line 3", "c", "kept"),
            ("line 4", "c", "changed"),
            ("line 2", "c", "old"),
            ("line 3", "shift-c", "file"),
            ("line 3", "; c", "overall"),
        ] {
            move_to_row_with_text(&rhs, row_text, cx);
            cx.simulate_keystrokes(keys);
            cx.simulate_input(body);
            cx.simulate_keystrokes("escape tab ctrl-enter");
        }
        // Still being written when the tab closes.
        move_to_row_with_text(&rhs, "line 36", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("open draft");
        let expected = comments(&review, cx)
            .into_iter()
            .take(5)
            .map(|(target, body, _)| (target, body))
            .chain([(lines("a.rs", 36..=36), "open draft".to_string())])
            .collect::<Vec<_>>();

        drop(rhs);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.close_item_by_id(review.entity_id(), workspace::SaveIntent::Skip, window, cx)
                    .detach_and_log_err(cx)
            })
        });
        drop(review);
        cx.run_until_parked();

        // One commented line changes while the review is closed.
        let text = fs.load(Path::new(path!("/project/a.rs"))).await.unwrap();
        fs.insert_file(
            path!("/project/a.rs"),
            text.replace("line 4\n", "line four\n").into_bytes(),
        )
        .await;
        cx.run_until_parked();

        cx.update(|window, cx| window.dispatch_action(crate::ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();
        let review = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ReviewView>(cx).unwrap()
        });
        cx.run_until_parked();
        let restored = comments(&review, cx);
        assert_eq!(
            restored
                .iter()
                .map(|(target, body, _)| (target.clone(), body.clone()))
                .collect::<Vec<_>>(),
            expected
        );
        assert!(
            restored[..5]
                .iter()
                .all(|(_, _, comment_type)| *comment_type == Some(CommentType::Issue))
        );
        let states = review.read_with(cx, |review, cx| {
            review
                .session()
                .read(cx)
                .comments()
                .iter()
                .map(|comment| (comment.body.clone(), comment.is_outdated()))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            states,
            [
                ("kept".to_string(), false),
                ("changed".to_string(), true),
                ("old".to_string(), false),
                ("file".to_string(), false),
                ("overall".to_string(), false),
                ("open draft".to_string(), false),
            ]
        );
        // The outdated comment is still in the diff, with its badge, and still navigable.
        let changed = comment_ids(&review, cx)[1];
        assert_eq!(hosted_rows(&review, cx).len(), 5);
        blocks(&review, cx).read_with(cx, |blocks, cx| {
            assert!(blocks.view(changed).unwrap().read(cx).is_outdated());
        });
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("] t ] t");
        assert_eq!(cursor_line(&rhs, cx), "line four");
        drop(rhs);

        let close = |review: Entity<ReviewView>, cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_item_by_id(
                        review.entity_id(),
                        workspace::SaveIntent::Skip,
                        window,
                        cx,
                    )
                    .detach_and_log_err(cx)
                })
            });
            drop(review);
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        };
        let reopen = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                window.dispatch_action(crate::ReviewUncommitted.boxed_clone(), cx)
            });
            cx.run_until_parked();
            let review = workspace.update(cx, |workspace, cx| {
                workspace.active_item_as::<ReviewView>(cx).unwrap()
            });
            cx.run_until_parked();
            review
        };

        // The line changes back while the review is closed (a stash popped then): the comment
        // is current again.
        close(review, cx);
        fs.insert_file(path!("/project/a.rs"), text.clone().into_bytes())
            .await;
        cx.run_until_parked();
        let review = reopen(cx);
        assert_eq!(
            line_state(&review, changed, cx),
            (lines("a.rs", 4..=4), false)
        );

        // So does editing the line back as it was while the review shows it.
        close(review, cx);
        fs.insert_file(
            path!("/project/a.rs"),
            text.replace("line 4\n", "line four\n").into_bytes(),
        )
        .await;
        cx.run_until_parked();
        let review = reopen(cx);
        assert_eq!(
            line_state(&review, changed, cx),
            (lines("a.rs", 4..=4), true)
        );
        let buffer = display_buffer(&review, "a.rs", cx);
        buffer.update(cx, |buffer, cx| {
            let offset = buffer.text().find("line four").unwrap() + "line ".len();
            buffer.edit([(offset..offset + "four".len(), "4")], None, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            line_state(&review, changed, cx),
            (lines("a.rs", 4..=4), false)
        );
        blocks(&review, cx).read_with(cx, |blocks, cx| {
            assert!(!blocks.view(changed).unwrap().read(cx).is_outdated());
        });
    }

    #[gpui::test]
    async fn test_editing_comment_whose_block_goes_away_is_left(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let buffer_id = review.read_with(cx, |review, cx| {
            let diff = review.diff().read(cx);
            diff.buffer_id_for(&review_path_key(&RepoPath::new("a.rs").unwrap()), cx)
                .unwrap()
        });
        let is_editing = |cx: &VisualTestContext| {
            review.read_with(cx, |review, cx| review.comments().read(cx).is_editing(cx))
        };

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("typed");
        rhs.update(cx, |editor, cx| editor.fold_buffer(buffer_id, cx));
        cx.run_until_parked();
        // Its text is kept, and the tab is read-only again.
        assert!(!is_editing(cx));
        assert_eq!(
            comments(&review, cx),
            [(lines("a.rs", 3..=3), "typed".to_string(), None)]
        );
        assert_eq!(
            review.read_with(cx, |review, cx| review.capability(cx)),
            language::Capability::ReadOnly
        );

        // A new comment left empty that way is deleted.
        rhs.update(cx, |editor, cx| editor.unfold_buffer(buffer_id, cx));
        cx.run_until_parked();
        move_to_row_with_text(&rhs, "line 4", cx);
        cx.simulate_keystrokes("c");
        rhs.update(cx, |editor, cx| editor.fold_buffer(buffer_id, cx));
        cx.run_until_parked();
        assert!(!is_editing(cx));
        assert_eq!(comments(&review, cx).len(), 1);
    }

    #[gpui::test]
    async fn test_other_tabs_show_new_comments_once_saved(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let clone = review.update_in(cx, |review, window, cx| {
            review.clone_on_split(None, window, cx)
        });
        let other = clone.await.unwrap();
        cx.run_until_parked();

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        assert_eq!(hosted_rows(&review, cx).len(), 1);
        assert!(hosted_rows(&other, cx).is_empty());
        cx.simulate_input("shared");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(hosted_rows(&other, cx).len(), 1);
        let id = comment_ids(&review, cx)[0];
        blocks(&other, cx).read_with(cx, |blocks, cx| {
            let view = blocks.view(id).unwrap().read(cx);
            assert_eq!(view.markdown_source(cx), "shared");
        });
    }

    #[gpui::test]
    async fn test_copy_review_comment_and_lines(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let clipboard = |cx: &mut VisualTestContext| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        };

        cx.simulate_keystrokes("y");
        assert_eq!(clipboard(cx), "");
        for (row_text, keys, body, types) in [
            ("line 3", "c", "fix this\nplease", "tab"),
            ("gone 1", "c", "why removed", ""),
            ("line 4", "; c", "Looks good", "tab tab tab tab"),
        ] {
            move_to_row_with_text(&rhs, row_text, cx);
            cx.simulate_keystrokes(keys);
            cx.simulate_input(body);
            cx.simulate_keystrokes("escape");
            if !types.is_empty() {
                cx.simulate_keystrokes(types);
            }
            cx.simulate_keystrokes("ctrl-enter");
        }

        move_to_row_with_text(&rhs, "line 1", cx);
        cx.simulate_keystrokes("y");
        assert_eq!(
            clipboard(cx),
            "\
I reviewed your code and have the following comments. Please address them.

Reviewing staged + unstaged changes

Comment types: ISSUE (problems to fix), PRAISE (positive feedback)

## Local tuicr Comments

1. **[PRAISE]** `Review Comment (scope: staged + unstaged changes)` - Looks good
2. **[ISSUE]** `a.rs:3` - fix this
   please
3. `deleted.txt:~1` - why removed
"
        );
        // Nothing is left pending: `j` moves one line.
        cx.simulate_keystrokes("j");
        assert_eq!(cursor_line(&rhs, cx), "line 2");

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("shift-y");
        assert_eq!(clipboard(cx), "fix this\nplease");
        move_to_row_with_text(&rhs, "line 5", cx);
        cx.simulate_keystrokes("shift-y");
        assert_eq!(clipboard(cx), "fix this\nplease");

        // The export has what open comment editors say.
        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("i escape shift-g shift-a");
        cx.simulate_input(" now");
        crate::test_support::dispatch(crate::CopyReviewAsMarkdown, cx);
        assert!(clipboard(cx).contains("`a.rs:3` - fix this\n   please now\n"));
        cx.simulate_keystrokes("escape escape");
        // A comment still being written has no text to copy.
        move_to_row_with_text(&rhs, "line 1", cx);
        cx.simulate_keystrokes("c");
        cx.focus(&rhs);
        cx.run_until_parked();
        let before = clipboard(cx);
        cx.simulate_keystrokes("shift-y");
        assert_eq!(clipboard(cx), before);

        // Visual `y` copies the lines and leaves visual mode.
        move_to_row_with_text(&rhs, "line 4", cx);
        cx.simulate_keystrokes("v j y");
        assert_eq!(clipboard(cx), "line 4\nline 5");
        assert_back_in_diff(&rhs, cx);
        // Like vim's yank, it leaves the cursor at the selection's start.
        assert_eq!(cursor_line(&rhs, cx), "line 4");
        move_to_row_with_text(&rhs, "line 5", cx);
        cx.simulate_keystrokes("shift-v k y");
        assert_eq!(clipboard(cx), "line 4\nline 5");
        assert_eq!(cursor_line(&rhs, cx), "line 4");
        // Over removed and added lines, the added lines (and unchanged ones) count.
        move_to_row_with_text(&rhs, "line 1", cx);
        cx.simulate_keystrokes("shift-v j j y");
        assert_eq!(clipboard(cx), "line 1\nLINE 2");
        assert_eq!(cursor_line(&rhs, cx), "line 1");
        move_to_row_with_text(&rhs, "gone 1", cx);
        cx.simulate_keystrokes("v j y");
        assert_eq!(clipboard(cx), "gone 1\ngone 2");
    }

    #[gpui::test]
    async fn test_enter_types_a_new_line(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c o n e enter t w o");
        let editor = open_editor(&review, cx).unwrap();
        assert_eq!(text(&editor, cx), "one\ntwo");
        // In normal mode, `enter` is vim's.
        cx.simulate_keystrokes("escape k enter");
        assert_eq!(text(&editor, cx), "one\ntwo");
        assert_eq!(cursor_point(&editor, cx).row, 1);
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(bodies(&review, cx), ["one\ntwo"]);
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
    }

    /// The gutter's "+" on a display row of one side, as last drawn.
    fn plus_button(
        is_lhs: bool,
        row: u32,
        refused: bool,
        cx: &mut VisualTestContext,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        let selector = format!(
            "review-gutter-comment{}-{}-{row}",
            if refused { "-refused" } else { "" },
            if is_lhs { "lhs" } else { "rhs" },
        );
        cx.debug_bounds(selector.leak())
    }

    fn line_height(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> gpui::Pixels {
        editor.update_in(cx, |editor, window, cx| {
            editor
                .style(cx)
                .text
                .line_height_in_pixels(window.rem_size())
        })
    }

    /// A point in the text of `editor`'s display row `row`, found from a "+" drawn on display row
    /// `reference_row` (the sides of a split diff have the same display rows).
    fn text_position(
        editor: &Entity<Editor>,
        row: u32,
        reference: gpui::Bounds<gpui::Pixels>,
        reference_row: u32,
        cx: &mut VisualTestContext,
    ) -> gpui::Point<gpui::Pixels> {
        let line_height = line_height(editor, cx);
        let bounds = editor.read_with(cx, |editor, _| *editor.last_bounds().unwrap());
        gpui::point(
            bounds.center().x,
            reference.center().y + line_height * (row as f32 - reference_row as f32),
        )
    }

    /// The "+" of the row the cursor of `editor` is on, and that display row.
    fn cursor_plus_button(
        editor: &Entity<Editor>,
        is_lhs: bool,
        cx: &mut VisualTestContext,
    ) -> (gpui::Bounds<gpui::Pixels>, u32) {
        let row = display_row(editor, cursor_point(editor, cx).row, cx);
        draw(cx);
        let bounds = plus_button(is_lhs, row, false, cx).expect("the cursor row has a \"+\"");
        (bounds, row)
    }

    /// A local review takes line comments inside the hunks `git diff` shows (changed lines and
    /// the 3 lines around them), like tuicr: a range of unchanged lines outside them, or across
    /// two hunks, is refused with a toast, and the "+" at such a selection is disabled.
    #[gpui::test]
    async fn test_line_comments_stay_in_the_diffs_hunks(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let blocks = blocks(&review, cx);
        let check = |target: CommentTarget, cx: &mut VisualTestContext| {
            blocks.read_with(cx, |blocks, cx| blocks.check(&target, cx))
        };
        let toasts = |cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                let ids = workspace.notification_ids();
                for id in &ids {
                    workspace.dismiss_notification(id, cx);
                }
                ids.len()
            })
        };
        let across: Result<(), SharedString> = Err(
            "The selection spans two hunks of the diff: a comment's lines must be in one hunk"
                .into(),
        );

        // a.rs: lines 2 and 35 of 40 changed, so its hunks are lines 1–5 and 32–38.
        assert_eq!(check(lines("a.rs", 1..=5), cx), Ok(()));
        assert_eq!(check(lines("a.rs", 4..=5), cx), Ok(()));
        assert_eq!(check(old_lines("a.rs", 1..=5), cx), Ok(()));
        assert_eq!(check(lines("a.rs", 32..=38), cx), Ok(()));
        assert_eq!(
            check(lines("a.rs", 10..=12), cx),
            Err(
                "Comments go on lines of the diff's hunks: changed lines and the 3 lines around \
                 them"
                    .into()
            )
        );
        assert_eq!(
            check(lines("a.rs", 4..=7), cx),
            Err(
                "The selection ends outside the diff's hunks: comments go on changed lines and \
                 the 3 lines around them"
                    .into()
            )
        );
        assert_eq!(
            check(lines("a.rs", 30..=33), cx),
            Err(
                "The selection starts outside the diff's hunks: comments go on changed lines and \
                 the 3 lines around them"
                    .into()
            )
        );
        assert_eq!(check(lines("a.rs", 3..=33), cx), across);
        assert_eq!(check(lines("added.txt", 1..=2), cx), Ok(()));
        assert_eq!(check(old_lines("deleted.txt", 1..=2), cx), Ok(()));
        assert_eq!(
            check(
                CommentTarget::File {
                    path: RepoPath::new("a.rs").unwrap()
                },
                cx
            ),
            Ok(())
        );

        // A visual selection from the first hunk into the second.
        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("shift-v");
        for _ in 0..20 {
            if cursor_line(&rhs, cx) == "line 34" {
                break;
            }
            cx.simulate_keystrokes("j");
        }
        assert_eq!(cursor_line(&rhs, cx), "line 34");
        let end = display_row(&rhs, cursor_point(&rhs, cx).row, cx);
        draw(cx);
        assert!(plus_button(false, end, true, cx).is_some());
        assert!(plus_button(false, end, false, cx).is_none());
        cx.simulate_keystrokes("c");
        cx.run_until_parked();
        assert!(open_editor(&review, cx).is_none());
        assert!(comment_ids(&review, cx).is_empty());
        assert_eq!(toasts(cx), 1);

        // Unchanged lines inside a hunk take the comment.
        move_to_row_with_text(&rhs, "line 33", cx);
        cx.simulate_keystrokes("shift-v j");
        let end = display_row(&rhs, cursor_point(&rhs, cx).row, cx);
        draw(cx);
        assert!(plus_button(false, end, false, cx).is_some());
        cx.simulate_keystrokes("c");
        cx.run_until_parked();
        assert!(open_editor(&review, cx).is_some());
        cx.simulate_input("context");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            comments(&review, cx),
            [(lines("a.rs", 33..=34), "context".to_string(), None)]
        );
        assert_eq!(toasts(cx), 0);
    }

    /// A click on a line number moves the line cursor there instead of opening the file (a
    /// multibuffer's own behaviour); a drag from it selects rows, a shift-click extends the
    /// selection to it. Neither comments.
    #[gpui::test]
    async fn test_line_number_clicks_select_rows(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Split);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let lhs = lhs_editor(&review, cx);
        let none = gpui::Modifiers::none();
        let item_count = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| workspace.items(cx).count())
        };
        let items = item_count(cx);
        let line_number = |editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext| {
            draw(cx);
            let column = editor
                .update_in(cx, |editor, window, cx| {
                    gutter::line_number_column(editor, window, cx)
                })
                .unwrap();
            gpui::point(
                column.end - gpui::px(2.),
                crate::test_support::row_center(editor, row, cx).y,
            )
        };
        let selection = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                crate::line_cursor::selected_rows(editor, &snapshot)
            })
        };

        let line_3 = move_to_row_with_text(&rhs, "line 3", cx);
        let position = line_number(&rhs, line_3 + 1, cx);
        cx.simulate_click(position, none);
        cx.run_until_parked();
        assert_eq!(cursor_line(&rhs, cx), "line 4");
        assert_eq!(vim_mode(&rhs, cx).as_deref(), Some("normal"));
        assert_eq!(item_count(cx), items);

        // The left side's line numbers too.
        let row = move_to_row_with_text(&lhs, "line 1", cx);
        let position = line_number(&lhs, row + 2, cx);
        cx.simulate_click(position, none);
        cx.run_until_parked();
        assert!(is_focused(&lhs, cx));
        assert_eq!(cursor_line(&lhs, cx), "line 3");
        assert_eq!(item_count(cx), items);

        // A drag over line numbers selects the rows.
        let from = line_number(&rhs, line_3 - 1, cx);
        let to = line_number(&rhs, line_3 + 1, cx);
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, none);
        cx.simulate_mouse_move(to, Some(gpui::MouseButton::Left), none);
        draw(cx);
        cx.run_until_parked();
        cx.simulate_mouse_up(to, gpui::MouseButton::Left, none);
        cx.run_until_parked();
        assert_eq!(selection(&rhs, cx), line_3 - 1..=line_3 + 1);
        assert_eq!(vim_mode(&rhs, cx).as_deref(), Some("visual"));
        assert!(comment_ids(&review, cx).is_empty());

        // Shift-click extends the selection.
        move_to_row_with_text(&rhs, "LINE 2", cx);
        let shift = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let position = line_number(&rhs, line_3 + 1, cx);
        cx.simulate_click(position, shift);
        cx.run_until_parked();
        assert_eq!(selection(&rhs, cx), line_3 - 1..=line_3 + 1);
        assert_eq!(item_count(cx), items);
    }

    /// GitHub's "+": on the hovered row and the cursor row; a click comments on its row (the old
    /// line on a removed row of a unified diff), and clicking a row's text moves the cursor
    /// there.
    #[gpui::test]
    async fn test_gutter_button_comments_on_its_row(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let none = gpui::Modifiers::none();

        let line_3 = move_to_row_with_text(&rhs, "line 3", cx);
        let (cursor_plus, line_3_row) = cursor_plus_button(&rhs, false, cx);
        let line_4_row = display_row(&rhs, line_3 + 1, cx);
        assert!(plus_button(false, line_4_row, false, cx).is_none());
        let line_4_text = text_position(&rhs, line_4_row, cursor_plus, line_3_row, cx);
        cx.simulate_mouse_move(line_4_text, None, none);
        draw(cx);
        let plus = plus_button(false, line_4_row, false, cx).expect("the hovered row has a \"+\"");
        assert!(plus_button(false, line_3_row, false, cx).is_some());

        cx.simulate_click(plus.center(), none);
        cx.run_until_parked();
        let editor = open_editor(&review, cx).expect("a comment editor opens");
        assert!(is_focused(&editor, cx));
        assert_eq!(vim_mode(&editor, cx).as_deref(), Some("insert"));
        cx.simulate_input("four");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&rhs, cx);
        assert_eq!(cursor_line(&rhs, cx), "line 4");
        assert_eq!(
            comments(&review, cx),
            [(lines("a.rs", 4..=4), "four".to_string(), None)]
        );

        // On a removed row, the old line.
        let removed = move_to_row_with_text(&rhs, "line 2", cx);
        let (plus, _) = cursor_plus_button(&rhs, false, cx);
        cx.simulate_click(plus.center(), none);
        cx.run_until_parked();
        cx.simulate_input("old");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(cursor_point(&rhs, cx).row, removed);
        assert_eq!(
            comments(&review, cx)[1],
            (old_lines("a.rs", 2..=2), "old".to_string(), None)
        );

        // A click on a row's text puts the line cursor there.
        let (plus, row) = cursor_plus_button(&rhs, false, cx);
        let line_1_row = display_row(&rhs, removed - 1, cx);
        let position = text_position(&rhs, line_1_row, plus, row, cx);
        cx.simulate_click(position, none);
        cx.run_until_parked();
        assert_eq!(cursor_line(&rhs, cx), "line 1");
        assert_back_in_diff(&rhs, cx);
        assert_eq!(
            crate::line_cursor::cursor_rows(&rhs, cx)
                .into_iter()
                .map(|(row, _)| row)
                .collect::<Vec<_>>(),
            [removed - 1]
        );
    }

    /// Pressing a "+" and dragging over rows selects them (the line cursor paints them), and the
    /// release comments on them; a "+" in a selection comments on all of it, also on a
    /// selection made by shift-clicking; one outside it comments on its own row.
    #[gpui::test]
    async fn test_gutter_button_comments_on_rows(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let none = gpui::Modifiers::none();
        let painted_rows = |cx: &mut VisualTestContext| {
            crate::line_cursor::cursor_rows(&rhs, cx)
                .into_iter()
                .map(|(row, _)| row_text(&rhs, row, cx))
                .collect::<Vec<_>>()
        };

        move_to_row_with_text(&rhs, "LINE 2", cx);
        let (plus, _) = cursor_plus_button(&rhs, false, cx);
        let line_height = line_height(&rhs, cx);
        cx.simulate_mouse_down(plus.center(), gpui::MouseButton::Left, none);
        let below = gpui::point(plus.center().x, plus.center().y + line_height * 2.);
        cx.simulate_mouse_move(below, gpui::MouseButton::Left, none);
        draw(cx);
        cx.run_until_parked();
        assert_eq!(painted_rows(cx), ["LINE 2", "line 3", "line 4"]);
        assert!(open_editor(&review, cx).is_none());
        cx.simulate_mouse_up(below, gpui::MouseButton::Left, none);
        cx.run_until_parked();
        let editor = open_editor(&review, cx).expect("a comment editor opens");
        assert_eq!(vim_mode(&editor, cx).as_deref(), Some("insert"));
        cx.simulate_input("range");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&rhs, cx);
        assert_eq!(cursor_line(&rhs, cx), "line 4");
        assert_eq!(
            comments(&review, cx),
            [(lines("a.rs", 2..=4), "range".to_string(), None)]
        );

        // A drag upwards, over the removed row: the added and unchanged lines count, as for `c`.
        move_to_row_with_text(&rhs, "LINE 2", cx);
        let (plus, _) = cursor_plus_button(&rhs, false, cx);
        let above = gpui::point(plus.center().x, plus.center().y - line_height * 2.);
        cx.simulate_mouse_down(plus.center(), gpui::MouseButton::Left, none);
        cx.simulate_mouse_move(above, gpui::MouseButton::Left, none);
        draw(cx);
        cx.run_until_parked();
        assert_eq!(painted_rows(cx), ["line 1", "line 2", "LINE 2"]);
        cx.simulate_mouse_up(above, gpui::MouseButton::Left, none);
        cx.run_until_parked();
        cx.simulate_input("up");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            comments(&review, cx)[1],
            (lines("a.rs", 1..=2), "up".to_string(), None)
        );
        assert_eq!(cursor_line(&rhs, cx), "LINE 2");

        // Shift-clicking a row's text selects up to it; the "+" at the selection's end comments
        // on all of it, and so does the one of a hovered row inside it.
        let line_3 = move_to_row_with_text(&rhs, "line 3", cx);
        for hover_inside in [false, true] {
            move_to_row_with_text(&rhs, "line 3", cx);
            let (plus, row) = cursor_plus_button(&rhs, false, cx);
            let line_5_row = display_row(&rhs, line_3 + 2, cx);
            let position = text_position(&rhs, line_5_row, plus, row, cx);
            cx.simulate_click(position, gpui::Modifiers::shift());
            draw(cx);
            assert_eq!(painted_rows(cx), ["line 3", "line 4", "line 5"]);
            let plus = if hover_inside {
                let line_4_row = display_row(&rhs, line_3 + 1, cx);
                let position = text_position(&rhs, line_4_row, plus, row, cx);
                cx.simulate_mouse_move(position, None, none);
                draw(cx);
                plus_button(false, line_4_row, false, cx).unwrap()
            } else {
                plus_button(false, line_5_row, false, cx).expect("the selection's end has a \"+\"")
            };
            cx.simulate_click(plus.center(), none);
            cx.run_until_parked();
            cx.simulate_input("selected");
            cx.simulate_keystrokes("ctrl-enter");
            assert_back_in_diff(&rhs, cx);
            assert_eq!(cursor_line(&rhs, cx), "line 5");
            assert_eq!(
                comments(&review, cx).last().unwrap(),
                &(lines("a.rs", 3..=5), "selected".to_string(), None)
            );
            // Line 5 ends the excerpt, whose expand button the "+" covers.
            assert_ne!(row_text(&rhs, line_3 + 3, cx), "line 6");
        }

        // A "+" outside the selection comments on its row, and the selection is gone.
        move_to_row_with_text(&rhs, "line 4", cx);
        cx.simulate_keystrokes("shift-v j");
        let (plus, row) = cursor_plus_button(&rhs, false, cx);
        let line_1_row = display_row(&rhs, line_3 - 3, cx);
        let position = text_position(&rhs, line_1_row, plus, row, cx);
        cx.simulate_mouse_move(position, None, none);
        draw(cx);
        let plus = plus_button(false, line_1_row, false, cx).unwrap();
        cx.simulate_click(plus.center(), none);
        cx.run_until_parked();
        cx.simulate_input("one");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&rhs, cx);
        assert_eq!(
            comments(&review, cx).last().unwrap(),
            &(lines("a.rs", 1..=1), "one".to_string(), None)
        );
    }

    /// Both sides of a split diff have a "+": the left one comments on old lines.
    #[gpui::test]
    async fn test_gutter_button_in_split_view(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Split);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let lhs = lhs_editor(&review, cx);
        let none = gpui::Modifiers::none();

        let line_3 = move_to_row_with_text(&rhs, "line 3", cx);
        let (plus, row) = cursor_plus_button(&rhs, false, cx);
        // The left side shows the cursor row, but its "+" only while hovered.
        assert!(plus_button(true, row, false, cx).is_none());
        let old_line_2 = move_to_row_with_text(&lhs, "line 2", cx);
        cx.focus(&rhs);
        move_to_row_with_text(&rhs, "line 3", cx);
        let old_line_2_row = display_row(&lhs, old_line_2, cx);
        let position = text_position(&lhs, old_line_2_row, plus, row, cx);
        cx.simulate_mouse_move(position, None, none);
        draw(cx);
        assert!(plus_button(false, old_line_2_row, false, cx).is_none());
        let lhs_plus = plus_button(true, old_line_2_row, false, cx).expect("the left side has one");
        cx.simulate_click(lhs_plus.center(), none);
        cx.run_until_parked();
        let editor = open_editor(&review, cx).expect("a comment editor opens");
        assert_eq!(vim_mode(&editor, cx).as_deref(), Some("insert"));
        cx.simulate_input("old two");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&lhs, cx);
        assert_eq!(cursor_line(&lhs, cx), "line 2");
        assert_eq!(
            comments(&review, cx),
            [(old_lines("a.rs", 2..=2), "old two".to_string(), None)]
        );
        assert!(
            hosted_on_sides(&review, cx)[0].1,
            "the block is on the left side"
        );

        // The right side's, for the new line.
        let (plus, row) = cursor_plus_button(&lhs, true, cx);
        let line_3_row = display_row(&rhs, line_3, cx);
        let position = text_position(&rhs, line_3_row, plus, row, cx);
        cx.simulate_mouse_move(position, None, none);
        draw(cx);
        let rhs_plus = plus_button(false, line_3_row, false, cx).unwrap();
        cx.simulate_click(rhs_plus.center(), none);
        cx.run_until_parked();
        cx.simulate_input("new three");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&rhs, cx);
        assert_eq!(
            comments(&review, cx)[1],
            (lines("a.rs", 3..=3), "new three".to_string(), None)
        );
    }

    /// The gutter's breakpoint and bookmark button doesn't show in the review.
    #[gpui::test]
    async fn test_no_breakpoint_button_in_the_gutter(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Split);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        move_to_row_with_text(&rhs, "line 3", cx);
        let (plus, row) = cursor_plus_button(&rhs, false, cx);

        // Over the line number of the next row, for longer than the button's delay.
        let position = gpui::point(
            plus.center().x + gpui::px(8.),
            plus.center().y + line_height(&rhs, cx),
        );
        cx.simulate_mouse_move(position, None, gpui::Modifiers::none());
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(500));
        cx.run_until_parked();
        draw(cx);
        assert!(plus_button(false, row + 1, false, cx).is_some());
        assert!(cx.debug_bounds("ICON-DebugBreakpoint").is_none());
        assert!(cx.debug_bounds("ICON-Bookmark").is_none());
    }

    fn click_drawn(selector: &'static str, cx: &mut VisualTestContext) {
        draw(cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} isn't drawn"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    /// The comment editor's footer: Save, Cancel (a new comment goes away, an edit is undone),
    /// the type menu and, for a saved comment, Delete (after asking, like `dd`).
    #[gpui::test]
    async fn test_comment_editor_footer(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("saved");
        draw(cx);
        assert!(cx.debug_bounds("review-comment-delete").is_none());
        // The Save button's key.
        let editor = open_editor(&review, cx).unwrap();
        let binding = cx.update(|window, cx| {
            window
                .highest_precedence_binding_for_action_in(
                    &crate::SaveComment,
                    &editor.focus_handle(cx),
                )
                .map(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .map(|keystroke| keystroke.unparse())
                        .collect::<Vec<_>>()
                })
        });
        assert_eq!(binding, Some(vec!["ctrl-enter".to_string()]));
        click_drawn("review-comment-save", cx);
        assert!(open_editor(&review, cx).is_none());
        assert_back_in_diff(&rhs, cx);
        assert_eq!(bodies(&review, cx), ["saved"]);

        move_to_row_with_text(&rhs, "line 4", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("gone");
        click_drawn("review-comment-cancel", cx);
        assert!(open_editor(&review, cx).is_none());
        assert_back_in_diff(&rhs, cx);
        assert_eq!(bodies(&review, cx), ["saved"]);

        move_to_row_with_text(&rhs, "line 3", cx);
        cx.simulate_keystrokes("i");
        cx.simulate_input("changed ");
        click_drawn("review-comment-cancel", cx);
        assert!(open_editor(&review, cx).is_none());
        assert_eq!(bodies(&review, cx), ["saved"]);

        cx.simulate_keystrokes("i");
        let editor = open_editor(&review, cx).unwrap();
        click_drawn("review-comment-type", cx);
        click_drawn("MENU_ITEM-Issue", cx);
        assert_eq!(
            comments(&review, cx)[0].2,
            Some(CommentType::Issue),
            "the menu sets the type"
        );
        assert_eq!(open_editor(&review, cx), Some(editor));
        click_drawn("review-comment-type", cx);
        click_drawn("MENU_ITEM-No Type", cx);
        assert_eq!(comments(&review, cx)[0].2, None);

        click_drawn("review-comment-delete", cx);
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(bodies(&review, cx), ["saved"]);
        click_drawn("review-comment-delete", cx);
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(comments(&review, cx).is_empty());
        assert!(open_editor(&review, cx).is_none());
        assert_back_in_diff(&rhs, cx);
    }

    async fn check_file_comment_button(diff_view_style: DiffViewStyle, cx: &mut TestAppContext) {
        init(cx, diff_view_style);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);

        // Wherever the cursor is.
        move_to_row_with_text(&rhs, "new 1", cx);
        click_drawn("review-file-comment-a.rs", cx);
        let editor = open_editor(&review, cx).expect("a comment editor opens");
        assert_eq!(vim_mode(&editor, cx).as_deref(), Some("insert"));
        cx.simulate_input("whole file");
        cx.simulate_keystrokes("ctrl-enter");
        assert_back_in_diff(&rhs, cx);
        assert_eq!(
            comments(&review, cx),
            [(
                CommentTarget::File {
                    path: RepoPath::new("a.rs").unwrap()
                },
                "whole file".to_string(),
                None
            )]
        );

        // A reviewed file's folded header has the button too; the file unfolds for the comment.
        move_to_row_with_text(&rhs, "line 1", cx);
        cx.simulate_keystrokes("r");
        cx.run_until_parked();
        assert!(is_folded(&review, "a.rs", cx));
        rhs.update_in(cx, |editor, window, cx| {
            editor.set_scroll_position(gpui::point(0., 0.), window, cx)
        });
        click_drawn("review-file-comment-a.rs", cx);
        assert!(!is_folded(&review, "a.rs", cx));
        cx.simulate_input("again");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(bodies(&review, cx), ["whole file", "again"]);
    }

    #[gpui::test]
    async fn test_file_comment_button(cx: &mut TestAppContext) {
        check_file_comment_button(DiffViewStyle::Unified, cx).await;
    }

    #[gpui::test]
    async fn test_file_comment_button_in_split_view(cx: &mut TestAppContext) {
        check_file_comment_button(DiffViewStyle::Split, cx).await;
    }
}
