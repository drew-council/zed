use crate::{
    ClearExcludeFilter, ClearFilters, ClearIncludeFilter, CollapseSelectedEntry, CopyBranchName,
    CopyPullRequestUrl, EditExcludeFilter, EditIncludeFilter, ExpandSelectedEntry, FocusPanel,
    NextPullRequestInStack, OpenPullRequestInBrowser, PreviousPullRequestInStack,
    ReloadPullRequest, ReviewUncommitted, ReviewView, SearchPaths, SelectNextMatch,
    SelectPreviousMatch, StartReview, ToggleBotReviews, ToggleFileReviewed, ToggleFocus,
    ToggleHideReviewed, ToggleOutdatedThreads, TogglePanel, ToggleResolvedThreads,
    ToggleSelectedDirectory, ToggleShowGenerated, ToggleShowVendored,
    comments::{CommentId, ItemKey},
    view::{
        PanelComment, PanelCommentKey, PanelFile, ReviewFilters, ReviewSummary,
        current_pull_request, current_review, filter_regex, focus_diff,
    },
};
use anyhow::Result;
use collections::HashSet;
use editor::{
    Editor, EditorEvent, PathKey,
    actions::{MoveToEnd, SelectAll},
};
use file_icons::FileIcons;
use git::{repository::RepoPath, status::FileStatus};
use gpui::{
    Action, Anchor, AsyncWindowContext, Bounds, ClipboardItem, DismissEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, FontWeight, KeyContext, MouseButton, MouseDownEvent,
    Point, ScrollStrategy, Subscription, UniformListScrollHandle, WeakEntity, anchored, deferred,
    point, size, uniform_list,
};
use project_panel::project_panel_settings::ProjectPanelSettings;
use settings::{ProjectPanelEntrySpacing, Settings as _, ShowIndentGuides};
use smallvec::SmallVec;
use std::{collections::BTreeMap, ops::Range};
use ui::{
    Checkbox, CommonAnimationExt as _, ContextMenu, IndentGuideColors, ListItem, ListItemSpacing,
    PopoverMenu, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::{NotificationId, NotifyTaskExt as _},
};

const REVIEW_PANEL_KEY: &str = "ReviewPanel";

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &EditIncludeFilter, window, cx| {
        open_prompt(workspace, PromptKind::Include, window, cx);
    });
    workspace.register_action(|workspace, _: &EditExcludeFilter, window, cx| {
        open_prompt(workspace, PromptKind::Exclude, window, cx);
    });
    workspace.register_action(|workspace, _: &SearchPaths, window, cx| {
        open_prompt(workspace, PromptKind::Search, window, cx);
    });
    workspace.register_action(|workspace, _: &ClearIncludeFilter, window, cx| {
        update_filters(workspace, window, cx, |filters| filters.include = None);
    });
    workspace.register_action(|workspace, _: &ClearExcludeFilter, window, cx| {
        update_filters(workspace, window, cx, |filters| filters.exclude = None);
    });
    workspace.register_action(|workspace, _: &ClearFilters, window, cx| {
        update_filters(workspace, window, cx, |filters| {
            filters.include = None;
            filters.exclude = None;
            filters.hide_reviewed = false;
        });
    });
    workspace.register_action(|workspace, _: &ToggleHideReviewed, window, cx| {
        update_filters(workspace, window, cx, |filters| {
            filters.hide_reviewed = !filters.hide_reviewed
        });
    });
    workspace.register_action(|workspace, _: &ToggleShowGenerated, window, cx| {
        update_filters(workspace, window, cx, |filters| {
            filters.show_generated = !filters.show_generated
        });
    });
    workspace.register_action(|workspace, _: &ToggleShowVendored, window, cx| {
        update_filters(workspace, window, cx, |filters| {
            filters.show_vendored = !filters.show_vendored
        });
    });
    workspace.register_action(|workspace, _: &CopyBranchName, _, cx| {
        let Some(review) = current_review(workspace, cx) else {
            return;
        };
        match review.read(cx).branch_name_to_copy(cx) {
            Some(branch) => cx.write_to_clipboard(ClipboardItem::new_string(branch.to_string())),
            None => workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<CopyBranchName>(),
                    "HEAD is detached",
                )
                .autohide(),
                cx,
            ),
        }
    });
    workspace.register_action(|workspace, _: &CopyPullRequestUrl, _, cx| {
        if let Some(pull_request) = current_pull_request(workspace, cx) {
            let url = pull_request.read(cx).details().url.to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(url));
        }
    });
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<ReviewPanel>(window, cx);
    });
    workspace.register_action(|workspace, _: &FocusPanel, window, cx| {
        workspace.focus_panel::<ReviewPanel>(window, cx);
    });
    workspace.register_action(|workspace, _: &TogglePanel, window, cx| {
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
            return;
        };
        let left_dock = workspace.left_dock().read(cx);
        let is_visible = left_dock.is_open()
            && left_dock
                .visible_panel()
                .is_some_and(|visible| visible.panel_id() == panel.entity_id());
        if is_visible {
            let was_focused = panel.read(cx).focus_handle.contains_focused(window, cx);
            workspace.close_panel::<ReviewPanel>(window, cx);
            if was_focused {
                focus_diff(workspace, window, cx);
            }
        } else {
            workspace.open_panel::<ReviewPanel>(window, cx);
        }
    });
}

/// Opens the review panel with a filter or search prompt.
fn open_prompt(
    workspace: &mut Workspace,
    kind: PromptKind,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(panel) = workspace.focus_panel::<ReviewPanel>(window, cx) {
        panel.update(cx, |panel, cx| panel.start_prompt(kind, window, cx));
    }
}

/// Changes the filters of the active review (or the one the panel shows).
fn update_filters(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    update: impl FnOnce(&mut ReviewFilters),
) {
    let Some(review) = current_review(workspace, cx) else {
        return;
    };
    review.update(cx, |review, cx| {
        let mut filters = review.filters().clone();
        update(&mut filters);
        review.set_filters(filters, window, cx);
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptKind {
    Include,
    Exclude,
    Search,
}

/// The one-line input above the tree for a filter pattern or a path search.
struct Prompt {
    kind: PromptKind,
    editor: Entity<Editor>,
    error: Option<SharedString>,
}

/// A row of the panel: the file tree, then the review's comments.
#[derive(Clone, Debug)]
enum TreeEntry {
    Directory {
        path: RepoPath,
        /// Single-child directory chains are compacted into one row ("src/a/b").
        name: SharedString,
        depth: usize,
        expanded: bool,
    },
    File {
        file: PanelFile,
        name: SharedString,
        depth: usize,
    },
    /// The heading of the comment list, which selection skips.
    CommentsHeader {
        count: usize,
    },
    Comment(PanelComment),
}

impl TreeEntry {
    fn depth(&self) -> usize {
        match self {
            TreeEntry::Directory { depth, .. } | TreeEntry::File { depth, .. } => *depth,
            TreeEntry::CommentsHeader { .. } | TreeEntry::Comment(_) => 0,
        }
    }

    fn key(&self) -> EntryKey {
        match self {
            TreeEntry::Directory { path, .. } => EntryKey::Directory(path.clone()),
            TreeEntry::File { file, .. } => EntryKey::File(file.path_key.clone()),
            TreeEntry::CommentsHeader { .. } => EntryKey::CommentsHeader,
            TreeEntry::Comment(comment) => EntryKey::Comment(comment.key.clone()),
        }
    }

    fn is_selectable(&self) -> bool {
        !matches!(self, TreeEntry::CommentsHeader { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EntryKey {
    Directory(RepoPath),
    File(PathKey),
    CommentsHeader,
    Comment(PanelCommentKey),
}

#[derive(Default)]
struct TreeNode {
    name: SharedString,
    path: Option<RepoPath>,
    children: BTreeMap<SharedString, TreeNode>,
    files: Vec<PanelFile>,
}

/// The review panel in the left dock: the file tree of the active review tab.
pub struct ReviewPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    active_review: Option<WeakEntity<ReviewView>>,
    /// The visible rows: collapsed directories hide their descendants.
    entries: Vec<TreeEntry>,
    /// Directories are expanded unless collapsed here, per review tab.
    collapsed: HashSet<(EntityId, RepoPath)>,
    selected: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    prompt: Option<Prompt>,
    /// The last path search, repeated by `n` and `N`.
    search: Option<String>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    _review_subscriptions: Vec<Subscription>,
    _workspace_subscription: Option<Subscription>,
}

impl ReviewPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        })
    }

    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_subscription = workspace.weak_handle().upgrade().map(|workspace| {
            cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &workspace::Event, window, cx| {
                    this.handle_workspace_event(workspace, event, window, cx)
                },
            )
        });
        let active_review = workspace
            .active_item_as::<ReviewView>(cx)
            .or_else(|| workspace.items_of_type::<ReviewView>(cx).next());
        let mut this = Self {
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            active_review: None,
            entries: Vec::new(),
            collapsed: HashSet::default(),
            selected: None,
            scroll_handle: UniformListScrollHandle::new(),
            prompt: None,
            search: None,
            context_menu: None,
            _review_subscriptions: Vec::new(),
            _workspace_subscription: workspace_subscription,
        };
        this.set_active_review(active_review, window, cx);
        this
    }

    fn set_active_review(
        &mut self,
        review: Option<Entity<ReviewView>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._review_subscriptions = review
            .as_ref()
            .map(|review| {
                vec![
                    cx.observe(review, |this, _, cx| this.rebuild(cx)),
                    cx.subscribe_in(
                        review,
                        window,
                        |this, _, event: &EditorEvent, window, cx| {
                            if let EditorEvent::SelectionsChanged { .. } = event
                                && !this.focus_handle.contains_focused(window, cx)
                            {
                                this.select_cursor_file(cx);
                            }
                        },
                    ),
                ]
            })
            .unwrap_or_default();
        self.active_review = review.map(|review| review.downgrade());
        self.selected = None;
        self.rebuild(cx);
        self.select_cursor_file(cx);
    }

    /// The review the panel shows: the active review tab, or the last active one while another
    /// kind of tab is active.
    pub fn active_review(&self) -> Option<Entity<ReviewView>> {
        self.active_review.as_ref()?.upgrade()
    }

    fn handle_workspace_event(
        &mut self,
        workspace: &Entity<Workspace>,
        event: &workspace::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            workspace::Event::ActiveItemChanged => {
                if let Some(review) = workspace.read(cx).active_item_as::<ReviewView>(cx)
                    && self.active_review() != Some(review.clone())
                {
                    self.set_active_review(Some(review), window, cx);
                }
            }
            workspace::Event::ItemRemoved { item_id } => {
                if self
                    .active_review
                    .as_ref()
                    .is_some_and(|review| review.entity_id() == *item_id)
                {
                    let next_review = workspace
                        .read(cx)
                        .items_of_type::<ReviewView>(cx)
                        .find(|review| review.entity_id() != *item_id);
                    self.set_active_review(next_review, window, cx);
                }
            }
            _ => {}
        }
    }

    fn files(&self, cx: &App) -> Vec<PanelFile> {
        self.active_review()
            .map(|review| review.read(cx).panel_files(cx))
            .unwrap_or_default()
    }

    /// Rebuilds the rows from the review's files, keeping the selected row (or its position when
    /// it disappeared).
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let selected_key = self.selected_entry().map(TreeEntry::key);
        let review_id = self.active_review.as_ref().map(|review| review.entity_id());
        let mut root = TreeNode::default();
        for file in self.files(cx) {
            let components = file.repo_path.components().collect::<Vec<_>>();
            let mut node = &mut root;
            let mut directory = String::new();
            for component in &components[..components.len().saturating_sub(1)] {
                if !directory.is_empty() {
                    directory.push('/');
                }
                directory.push_str(component);
                let name = SharedString::from(component.to_string());
                let path = RepoPath::new(&directory).log_err();
                node = node
                    .children
                    .entry(name.clone())
                    .or_insert_with(|| TreeNode {
                        name,
                        path,
                        ..TreeNode::default()
                    });
            }
            node.files.push(file);
        }

        let mut entries = Vec::new();
        self.flatten(&root, 0, review_id, &mut entries);
        let comments = self
            .active_review()
            .map(|review| review.read(cx).panel_comments(cx))
            .unwrap_or_default();
        if !comments.is_empty() {
            entries.push(TreeEntry::CommentsHeader {
                count: comments.len(),
            });
            entries.extend(comments.into_iter().map(TreeEntry::Comment));
        }
        self.entries = entries;
        self.selected = match selected_key {
            Some(key) => self
                .entries
                .iter()
                .position(|entry| entry.key() == key)
                .or_else(|| self.nearest_selectable(self.selected?)),
            None => None,
        };
        cx.notify();
    }

    /// The selectable row closest to `index` (the earlier one of two as close), for a selection
    /// whose row went away.
    fn nearest_selectable(&self, index: usize) -> Option<usize> {
        let index = index.min(self.entries.len().checked_sub(1)?);
        (0..self.entries.len()).find_map(|distance| {
            [index.checked_sub(distance), index.checked_add(distance)]
                .into_iter()
                .flatten()
                .find(|candidate| self.is_selectable(*candidate))
        })
    }

    fn flatten(
        &self,
        node: &TreeNode,
        depth: usize,
        review_id: Option<EntityId>,
        entries: &mut Vec<TreeEntry>,
    ) {
        for child in node.children.values() {
            let mut terminal = child;
            let mut name = child.name.to_string();
            while terminal.files.is_empty() && terminal.children.len() == 1 {
                let Some(only_child) = terminal.children.values().next() else {
                    break;
                };
                name.push('/');
                name.push_str(&only_child.name);
                terminal = only_child;
            }
            let Some(path) = terminal.path.clone() else {
                continue;
            };
            let expanded = review_id
                .is_none_or(|review_id| !self.collapsed.contains(&(review_id, path.clone())));
            entries.push(TreeEntry::Directory {
                path,
                name: name.into(),
                depth,
                expanded,
            });
            if expanded {
                self.flatten(terminal, depth + 1, review_id, entries);
            }
        }
        for file in &node.files {
            let name = file
                .repo_path
                .file_name()
                .map(|name| SharedString::from(name.to_string()))
                .unwrap_or_else(|| file.repo_path.as_unix_str().to_string().into());
            entries.push(TreeEntry::File {
                file: file.clone(),
                name,
                depth,
            });
        }
    }

    fn selected_entry(&self) -> Option<&TreeEntry> {
        self.entries.get(self.selected?)
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .entries
            .get(index)
            .is_none_or(|entry| !entry.is_selectable())
        {
            return;
        }
        self.selected = Some(index);
        self.scroll_handle
            .scroll_to_item(index, ScrollStrategy::Center);
        // In single-file mode the diff shows only the selected file, so it follows the selection.
        if let Some(TreeEntry::File { file, .. }) = self.entries.get(index)
            && let Some(review) = self.active_review()
            && review.read(cx).diff().read(cx).single_file().is_some()
        {
            let path_key = file.path_key.clone();
            review.update(cx, |review, cx| review.reveal_file(path_key, window, cx));
        }
        cx.notify();
    }

    /// Selects the row of the file under the diff's cursor.
    fn select_cursor_file(&mut self, cx: &mut Context<Self>) {
        let Some(review) = self.active_review() else {
            return;
        };
        let Some(path_key) = review.read(cx).diff().read(cx).path_at_cursor(cx) else {
            return;
        };
        let index = self.entries.iter().position(
            |entry| matches!(entry, TreeEntry::File { file, .. } if file.path_key == path_key),
        );
        // A file inside a collapsed directory selects that directory.
        let index = index.or_else(|| {
            let repo_path = self
                .files(cx)
                .into_iter()
                .find(|file| file.path_key == path_key)?
                .repo_path;
            self.entries.iter().position(|entry| {
                matches!(entry, TreeEntry::Directory { path, expanded: false, .. }
                    if repo_path.starts_with(path))
            })
        });
        if let Some(index) = index
            && self.selected != Some(index)
        {
            self.selected = Some(index);
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Center);
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        let start = self.selected.map_or(0, |index| index + 1);
        if let Some(index) = (start..self.entries.len()).find(|index| self.is_selectable(*index)) {
            self.select(index, window, cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let end = self.selected.unwrap_or(self.entries.len());
        if let Some(index) = (0..end).rev().find(|index| self.is_selectable(*index)) {
            self.select(index, window, cx);
        }
    }

    fn select_first(&mut self, _: &menu::SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = (0..self.entries.len()).find(|index| self.is_selectable(*index)) {
            self.select(index, window, cx);
        }
    }

    fn select_last(&mut self, _: &menu::SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = (0..self.entries.len())
            .rev()
            .find(|index| self.is_selectable(*index))
        {
            self.select(index, window, cx);
        }
    }

    fn is_selectable(&self, index: usize) -> bool {
        self.entries
            .get(index)
            .is_some_and(TreeEntry::is_selectable)
    }

    /// Enter: a file opens in the diff, which takes focus; a directory expands or collapses.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt.is_some() {
            self.confirm_prompt(window, cx);
            return;
        }
        match self.selected_entry().cloned() {
            Some(TreeEntry::File { file, .. }) => self.open_file(file.path_key, true, window, cx),
            Some(TreeEntry::Directory { path, .. }) => self.toggle_directory(path, cx),
            Some(TreeEntry::Comment(comment)) => self.open_comment(&comment, window, cx),
            Some(TreeEntry::CommentsHeader { .. }) | None => {}
        }
    }

    /// Like `]t`: the diff moves to a line or file comment, or a thread, and takes the focus. A
    /// review comment opens for editing.
    fn open_comment(
        &mut self,
        comment: &PanelComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.active_review() else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&review, true, false, window, cx)
            });
        }
        let blocks = review.read(cx).comments().clone();
        if comment.submitted {
            self.show_toast(
                "This comment was submitted to GitHub: its thread shows in the diff",
                cx,
            );
            return;
        }
        if comment.is_review
            && let ItemKey::Comment(id) = comment.item
        {
            blocks.update(cx, |blocks, cx| blocks.edit(id, window, cx));
            return;
        }
        let landed = blocks.update(cx, |blocks, cx| blocks.land_on(&comment.item, window, cx));
        if !landed {
            self.show_toast("This comment's lines aren't in the diff", cx);
        }
    }

    /// A comment row's edit button: the diff moves to the comment and its editor opens.
    fn edit_comment(&mut self, id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.active_review() else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&review, true, false, window, cx)
            });
        }
        let blocks = review.read(cx).comments().clone();
        if !blocks.update(cx, |blocks, cx| blocks.land_and_edit(id, window, cx)) {
            self.show_toast("This comment's lines aren't in the diff", cx);
        }
    }

    /// A comment row's delete button: deletes the comment after asking, like `dd`.
    fn delete_comment(&mut self, id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.active_review() else {
            return;
        };
        let blocks = review.read(cx).comments().clone();
        blocks.update(cx, |blocks, cx| blocks.confirm_delete(id, window, cx));
    }

    /// A file row's checkbox: toggles the file's reviewed mark, without selecting the row.
    fn toggle_file_at(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(TreeEntry::File { file, .. }) = self.entries.get(index).cloned() else {
            return;
        };
        if let Some(review) = self.active_review() {
            review.update(cx, |review, cx| review.toggle_file(&file.repo_path, cx));
        }
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

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt.is_some() {
            self.close_prompt(window, cx);
        } else {
            window.dispatch_action(crate::FocusDiff.boxed_clone(), cx);
        }
    }

    /// Moves the diff to the file; `focus` also moves focus to the diff.
    fn open_file(
        &mut self,
        path_key: PathKey,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.active_review() else {
            return;
        };
        review.update(cx, |review, cx| review.reveal_file(path_key, window, cx));
        if focus {
            window.dispatch_action(crate::FocusDiff.boxed_clone(), cx);
        }
    }

    fn toggle_directory(&mut self, path: RepoPath, cx: &mut Context<Self>) {
        let Some(review_id) = self.active_review.as_ref().map(|review| review.entity_id()) else {
            return;
        };
        let key = (review_id, path);
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.rebuild(cx);
    }

    /// `l`: expands a collapsed directory, enters an expanded one, and shows a file in the diff
    /// while keeping focus here.
    fn expand_selected_entry(
        &mut self,
        _: &ExpandSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selected_entry().cloned() {
            Some(TreeEntry::Directory {
                path,
                expanded: false,
                ..
            }) => self.toggle_directory(path, cx),
            Some(TreeEntry::Directory { expanded: true, .. }) => {
                if let Some(index) = self.selected {
                    self.select(index + 1, window, cx);
                }
            }
            Some(TreeEntry::File { file, .. }) => self.open_file(file.path_key, false, window, cx),
            Some(TreeEntry::CommentsHeader { .. } | TreeEntry::Comment(_)) | None => {}
        }
    }

    /// `h`: collapses an expanded directory, otherwise selects the parent directory.
    fn collapse_selected_entry(
        &mut self,
        _: &CollapseSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.selected else {
            return;
        };
        if let Some(TreeEntry::Directory {
            path,
            expanded: true,
            ..
        }) = self.entries.get(index).cloned()
        {
            self.toggle_directory(path, cx);
            return;
        }
        let depth = self.entries.get(index).map_or(0, TreeEntry::depth);
        if depth == 0 {
            return;
        }
        let parent = self.entries.get(..index).and_then(|entries| {
            entries.iter().rposition(|entry| {
                matches!(entry, TreeEntry::Directory { .. }) && entry.depth() < depth
            })
        });
        if let Some(parent) = parent {
            self.select(parent, window, cx);
        }
    }

    fn start_prompt(&mut self, kind: PromptKind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.active_review() else {
            return;
        };
        let filters = review.read(cx).filters();
        let text = match kind {
            PromptKind::Include => filters
                .include
                .as_ref()
                .map(|regex| regex.as_str().to_string()),
            PromptKind::Exclude => filters
                .exclude
                .as_ref()
                .map(|regex| regex.as_str().to_string()),
            PromptKind::Search => self.search.clone(),
        }
        .unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                match kind {
                    PromptKind::Include | PromptKind::Exclude => "Regex (empty clears)",
                    PromptKind::Search => "Part of a path",
                },
                window,
                cx,
            );
            editor.set_text(text, window, cx);
            // Typing replaces the last search; a filter's pattern is usually refined.
            match kind {
                PromptKind::Search => editor.select_all(&SelectAll, window, cx),
                PromptKind::Include | PromptKind::Exclude => {
                    editor.move_to_end(&MoveToEnd, window, cx)
                }
            }
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.prompt = Some(Prompt {
            kind,
            editor,
            error: None,
        });
        cx.notify();
    }

    fn close_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    /// Enter in the prompt: applies the pattern (an invalid one keeps the prompt open with the
    /// error) or runs the search.
    fn confirm_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        let text = prompt.editor.read(cx).text(cx);
        match prompt.kind {
            PromptKind::Include | PromptKind::Exclude => {
                let regex = match filter_regex(&text) {
                    Ok(regex) => regex,
                    Err(error) => {
                        prompt.error = Some(error.to_string().into());
                        cx.notify();
                        return;
                    }
                };
                let kind = prompt.kind;
                if let Some(review) = self.active_review() {
                    review.update(cx, |review, cx| {
                        let mut filters = review.filters().clone();
                        match kind {
                            PromptKind::Include => filters.include = regex,
                            _ => filters.exclude = regex,
                        }
                        review.set_filters(filters, window, cx);
                    });
                }
                self.close_prompt(window, cx);
            }
            PromptKind::Search => {
                if !text.is_empty() && !self.has_match(&text, cx) {
                    if let Some(prompt) = &mut self.prompt {
                        prompt.error =
                            Some(format!("No file path contains \u{201c}{text}\u{201d}").into());
                    }
                    cx.notify();
                    return;
                }
                self.search = (!text.is_empty()).then_some(text);
                self.close_prompt(window, cx);
                self.select_match(true, window, cx);
            }
        }
    }

    fn has_match(&self, search: &str, cx: &App) -> bool {
        let search = search.to_lowercase();
        self.files(cx).iter().any(|file| {
            file.repo_path
                .as_unix_str()
                .to_lowercase()
                .contains(&search)
        })
    }

    /// Selects the next (or previous) file whose path contains the search, wrapping around and
    /// expanding the directories around it. Returns whether a file matches.
    fn select_match(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(search) = self.search.as_ref().map(|search| search.to_lowercase()) else {
            return false;
        };
        let files = self.files(cx);
        let matches = files
            .iter()
            .filter(|file| {
                file.repo_path
                    .as_unix_str()
                    .to_lowercase()
                    .contains(&search)
            })
            .collect::<Vec<_>>();
        let current = match self.selected_entry() {
            Some(TreeEntry::File { file, .. }) => Some(file.path_key.clone()),
            _ => None,
        };
        let target = if forward {
            matches
                .iter()
                .find(|file| {
                    current
                        .as_ref()
                        .is_none_or(|current| &file.path_key > current)
                })
                .or_else(|| matches.first())
        } else {
            matches
                .iter()
                .rev()
                .find(|file| {
                    current
                        .as_ref()
                        .is_none_or(|current| &file.path_key < current)
                })
                .or_else(|| matches.last())
        };
        let Some(target) = target.map(|file| (*file).clone()) else {
            return false;
        };
        if let Some(review_id) = self.active_review.as_ref().map(|review| review.entity_id()) {
            self.collapsed.retain(|(id, directory)| {
                *id != review_id || !target.repo_path.starts_with(directory)
            });
        }
        self.rebuild(cx);
        if let Some(index) = self.entries.iter().position(
            |entry| matches!(entry, TreeEntry::File { file, .. } if file.path_key == target.path_key),
        ) {
            self.select(index, window, cx);
        }
        true
    }

    fn select_next_match(
        &mut self,
        _: &SelectNextMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_match_or_say_none(true, window, cx);
    }

    fn select_previous_match(
        &mut self,
        _: &SelectPreviousMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_match_or_say_none(false, window, cx);
    }

    /// `n`/`N`: the files may have changed since the search.
    fn select_match_or_say_none(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search.is_some() && !self.select_match(forward, window, cx) {
            self.show_toast("No file path contains the search", cx);
        }
    }

    fn cancel_editor(
        &mut self,
        _: &editor::actions::Cancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.prompt.is_some() {
            self.close_prompt(window, cx);
        } else {
            cx.propagate();
        }
    }

    /// `r`: toggles the selected file's mark, without moving on (unlike `r` in the diff).
    fn toggle_file_reviewed(
        &mut self,
        _: &ToggleFileReviewed,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(TreeEntry::File { file, .. }) = self.selected_entry().cloned() else {
            return;
        };
        if let Some(review) = self.active_review() {
            review.update(cx, |review, cx| review.toggle_file(&file.repo_path, cx));
        }
    }

    fn toggle_selected_directory(
        &mut self,
        _: &ToggleSelectedDirectory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(TreeEntry::Directory { path, .. }) = self.selected_entry().cloned() {
            self.toggle_directory(path, cx);
        }
    }

    /// The menu of a right-clicked row (`file` for a file row) or of the header's button.
    fn build_menu(
        &self,
        file: Option<PanelFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let review = self.active_review();
        let filters = review
            .as_ref()
            .map(|review| review.read(cx).filters().clone())
            .unwrap_or_default();
        let has_branch = review
            .as_ref()
            .is_some_and(|review| review.read(cx).branch_name_to_copy(cx).is_some());
        let thread_filters = review.as_ref().and_then(|review| {
            review
                .read(cx)
                .pull_request()
                .map(|pull_request| pull_request.read(cx).filters())
        });
        let stack = review.as_ref().and_then(|review| {
            review
                .read(cx)
                .pull_request()
                .and_then(|pull_request| pull_request.read(cx).stack_position())
        });
        let project_path = file
            .as_ref()
            .zip(review.as_ref())
            .and_then(|(file, review)| review.read(cx).project_path(&file.repo_path, cx))
            .filter(|_| file.as_ref().is_some_and(|file| !file.status.is_deleted()));
        let workspace = self.workspace.clone();
        let focus_handle = self.focus_handle.clone();
        ContextMenu::build(window, cx, move |mut menu, _, _| {
            menu = menu.context(focus_handle);
            if let Some(file) = file {
                let path = file.repo_path.as_unix_str().to_string();
                menu = menu
                    .action(
                        if file.reviewed {
                            "Mark as Not Reviewed"
                        } else {
                            "Mark as Reviewed"
                        },
                        ToggleFileReviewed.boxed_clone(),
                    )
                    .entry("Open File", None, move |window, cx| {
                        if let Some(project_path) = project_path.clone()
                            && let Some(open) = workspace
                                .update(cx, |workspace, cx| {
                                    workspace.open_path(project_path, None, true, window, cx)
                                })
                                .log_err()
                        {
                            open.detach_and_notify_err(workspace.clone(), window, cx);
                        }
                    })
                    .entry("Copy Relative Path", None, move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                    })
                    .separator();
            }
            menu.toggleable_entry(
                "Hide Reviewed Files",
                filters.hide_reviewed,
                IconPosition::Start,
                Some(ToggleHideReviewed.boxed_clone()),
                |window, cx| window.dispatch_action(ToggleHideReviewed.boxed_clone(), cx),
            )
            .toggleable_entry(
                "Show Generated Files",
                filters.show_generated,
                IconPosition::Start,
                Some(ToggleShowGenerated.boxed_clone()),
                |window, cx| window.dispatch_action(ToggleShowGenerated.boxed_clone(), cx),
            )
            .toggleable_entry(
                "Show Vendored Files",
                filters.show_vendored,
                IconPosition::Start,
                Some(ToggleShowVendored.boxed_clone()),
                |window, cx| window.dispatch_action(ToggleShowVendored.boxed_clone(), cx),
            )
            .separator()
            .action("Include Filter…", EditIncludeFilter.boxed_clone())
            .action("Exclude Filter…", EditExcludeFilter.boxed_clone())
            .action("Search Paths…", SearchPaths.boxed_clone())
            .action_disabled_when(
                filters.include.is_none() && filters.exclude.is_none() && !filters.hide_reviewed,
                "Clear Filters",
                ClearFilters.boxed_clone(),
            )
            .when_some(thread_filters, |menu, thread_filters| {
                menu.separator()
                    .toggleable_entry(
                        "Show Resolved Threads",
                        thread_filters.show_resolved,
                        IconPosition::Start,
                        Some(ToggleResolvedThreads.boxed_clone()),
                        |window, cx| {
                            window.dispatch_action(ToggleResolvedThreads.boxed_clone(), cx)
                        },
                    )
                    .toggleable_entry(
                        "Show Outdated Threads",
                        thread_filters.show_outdated,
                        IconPosition::Start,
                        Some(ToggleOutdatedThreads.boxed_clone()),
                        |window, cx| {
                            window.dispatch_action(ToggleOutdatedThreads.boxed_clone(), cx)
                        },
                    )
                    .toggleable_entry(
                        "Show Bot Reviews",
                        thread_filters.show_bots,
                        IconPosition::Start,
                        Some(ToggleBotReviews.boxed_clone()),
                        |window, cx| window.dispatch_action(ToggleBotReviews.boxed_clone(), cx),
                    )
                    .when_some(stack, |menu, stack| {
                        menu.action_disabled_when(
                            stack.below.is_none(),
                            "Previous Pull Request in Stack",
                            PreviousPullRequestInStack.boxed_clone(),
                        )
                        .action_disabled_when(
                            stack.above.is_none(),
                            "Next Pull Request in Stack",
                            NextPullRequestInStack.boxed_clone(),
                        )
                    })
                    .action("Reload Pull Request", ReloadPullRequest.boxed_clone())
                    .action(
                        "Open Pull Request in Browser",
                        OpenPullRequestInBrowser.boxed_clone(),
                    )
            })
            .separator()
            .action_disabled_when(
                !has_branch,
                "Copy Branch Name",
                CopyBranchName.boxed_clone(),
            )
            .when(thread_filters.is_some(), |menu| {
                menu.action("Copy Pull Request URL", CopyPullRequestUrl.boxed_clone())
            })
        })
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        file: Option<PanelFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = self.build_menu(file, window, cx);
        let menu_focus = menu.focus_handle(cx);
        window.defer(cx, move |window, cx| window.focus(&menu_focus, cx));
        let subscription =
            cx.subscribe_in(&menu, window, |this, menu, _: &DismissEvent, window, cx| {
                if menu.focus_handle(cx).contains_focused(window, cx) {
                    window.focus(&this.focus_handle, cx);
                }
                this.context_menu = None;
                cx.notify();
            });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn render_entry(
        &self,
        index: usize,
        entry: &TreeEntry,
        focused: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let selected = self.selected == Some(index);
        // The tree follows the project panel's settings and sizes, so both trees look alike.
        let settings = *ProjectPanelSettings::get_global(cx);
        let spacing = match settings.entry_spacing {
            ProjectPanelEntrySpacing::Comfortable => ListItemSpacing::Dense,
            ProjectPanelEntrySpacing::Standard => ListItemSpacing::ExtraDense,
        };
        match entry {
            TreeEntry::Directory {
                path,
                name,
                depth,
                expanded,
            } => {
                let indicators = FileIcons::get_folder_indicators(
                    settings.folder_indicator,
                    *expanded,
                    path.as_std_path(),
                    cx,
                );
                ListItem::new(("review-directory", index))
                    .spacing(spacing)
                    .indent_level(*depth)
                    .indent_step_size(px(settings.indent_size))
                    .toggle_state(selected)
                    .focused(selected && focused)
                    .start_slot(render_tree_icons(
                        indicators.chevron,
                        indicators.icon,
                        false,
                    ))
                    .child(render_tree_label(
                        Label::new(name.clone())
                            .when(settings.bold_folder_labels, |label| {
                                label.weight(FontWeight::SEMIBOLD)
                            })
                            .truncate(),
                    ))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        window.focus(&this.focus_handle, cx);
                        this.selected = Some(index);
                        if let Some(TreeEntry::Directory { path, .. }) =
                            this.entries.get(index).cloned()
                        {
                            this.toggle_directory(path, cx);
                        }
                    }))
                    .into_any_element()
            }
            TreeEntry::File { file, name, depth } => {
                let icon = settings
                    .file_icons
                    .then(|| FileIcons::get_icon(file.repo_path.as_std_path(), cx))
                    .flatten();
                let reserves_chevron_slot = settings.folder_indicator.shows_chevron()
                    && settings.folder_indicator.shows_icon();
                let (letter, color) = status_letter(file.status);
                let is_deleted = file.status.is_deleted();
                let path = file.repo_path.as_unix_str().to_string();
                let status_label = || Label::new(letter).size(LabelSize::Small).color(color);
                // On hover, the ✓ gives way to a checkbox that marks the file without selecting
                // the row or taking the focus.
                let reviewed_checkbox = div()
                    .debug_selector({
                        let path = path.clone();
                        move || format!("review-panel-reviewed-{path}")
                    })
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .child(
                        Checkbox::new(("review-panel-reviewed", index), file.reviewed.into())
                            .tooltip(Tooltip::for_action_title(
                                if file.reviewed {
                                    "Mark as Not Reviewed"
                                } else {
                                    "Mark as Reviewed"
                                },
                                &ToggleFileReviewed,
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_file_at(index, cx);
                            })),
                    );
                ListItem::new(("review-file", index))
                    .spacing(spacing)
                    .indent_level(*depth)
                    .indent_step_size(px(settings.indent_size))
                    .toggle_state(selected)
                    .focused(selected && focused)
                    .start_slot(render_tree_icons(None, icon, reserves_chevron_slot))
                    .child(
                        render_tree_label(
                            Label::new(name.clone())
                                .color(if is_deleted { Color::Disabled } else { color })
                                .when(is_deleted, Label::strikethrough)
                                .truncate(),
                        )
                        .when(
                            file.attributes.generated || file.attributes.vendored,
                            |name| {
                                name.child(
                                    Label::new(if file.attributes.generated {
                                        "(generated)"
                                    } else {
                                        "(vendored)"
                                    })
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                )
                            },
                        )
                        .debug_selector({
                            let path = path.clone();
                            move || format!("review-panel-file-name-{path}")
                        }),
                    )
                    .end_slot(
                        h_flex()
                            .debug_selector(move || format!("review-panel-file-status-{path}"))
                            .gap_1()
                            .child(
                                h_flex()
                                    .child(
                                        Icon::new(IconName::Check)
                                            .size(IconSize::Small)
                                            .color(Color::Success),
                                    )
                                    .when(!file.reviewed, |slot| slot.invisible()),
                            )
                            .child(status_label()),
                    )
                    .end_slot_on_hover(
                        h_flex()
                            .gap_1()
                            .child(reviewed_checkbox)
                            .child(status_label()),
                    )
                    .tooltip({
                        let path = SharedString::from(file.repo_path.as_unix_str().to_string());
                        move |_, cx| Tooltip::simple(path.clone(), cx)
                    })
                    .on_secondary_mouse_down(cx.listener(
                        move |this, event: &MouseDownEvent, window, cx| {
                            this.selected = Some(index);
                            let file = match this.entries.get(index) {
                                Some(TreeEntry::File { file, .. }) => Some(file.clone()),
                                _ => None,
                            };
                            this.deploy_context_menu(event.position, file, window, cx);
                            cx.stop_propagation();
                        },
                    ))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        window.focus(&this.focus_handle, cx);
                        this.selected = Some(index);
                        if let Some(TreeEntry::File { file, .. }) = this.entries.get(index).cloned()
                        {
                            this.open_file(file.path_key, false, window, cx);
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            TreeEntry::CommentsHeader { count } => h_flex()
                .id(("review-comments-header", index))
                .px_2()
                .pt_2()
                .pb_0p5()
                .gap_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Label::new(format!("Comments ({count})"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            TreeEntry::Comment(comment) => {
                let muted = comment.outdated || comment.submitted || comment.remote;
                let state = if comment.remote && comment.resolved {
                    Some(("resolved", Color::Muted))
                } else if comment.outdated {
                    Some(("outdated", Color::Warning))
                } else if comment.submitted {
                    Some(("submitted", Color::Muted))
                } else {
                    None
                };
                ListItem::new(("review-comment", index))
                    .spacing(spacing)
                    .toggle_state(selected)
                    .focused(selected && focused)
                    .start_slot(match comment.comment_type {
                        Some(comment_type) => Label::new(comment_type.label())
                            .size(LabelSize::Small)
                            .color(comment_type.color())
                            .buffer_font(cx)
                            .into_any_element(),
                        None => Icon::new(if comment.remote {
                            IconName::Github
                        } else {
                            IconName::Chat
                        })
                        .color(Color::Muted)
                        .into_any_element(),
                    })
                    .child(
                        h_flex()
                            .h_6()
                            .w_full()
                            .min_w_0()
                            .gap_1()
                            // Where the comment is matters more than how it starts: the first
                            // line gets what the location leaves.
                            .child(
                                div()
                                    .flex_none()
                                    .max_w_full()
                                    .debug_selector(|| {
                                        format!("review-comment-location: {}", comment.first_line)
                                    })
                                    .child(
                                        Label::new(comment.location.clone())
                                            .color(if muted {
                                                Color::Muted
                                            } else {
                                                Color::Default
                                            })
                                            .truncate(),
                                    ),
                            )
                            .child(
                                div().flex_1().min_w_0().child(
                                    Label::new(comment.first_line.clone())
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                            ),
                    )
                    .when_some(state, |item, (state, color)| {
                        item.end_slot(Label::new(state).size(LabelSize::Small).color(color))
                    })
                    .when_some(
                        match comment.key {
                            PanelCommentKey::Comment(id)
                                if !comment.remote && !comment.submitted =>
                            {
                                Some(id)
                            }
                            PanelCommentKey::Comment(_) | PanelCommentKey::Thread(_) => None,
                        },
                        |item, id| {
                            item.end_slot_on_hover(
                                h_flex()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .debug_selector(move || {
                                                format!("review-panel-edit-comment-{}", id.0)
                                            })
                                            .child(
                                                IconButton::new(
                                                    ("review-panel-edit-comment", index),
                                                    IconName::Pencil,
                                                )
                                                .icon_size(IconSize::Small)
                                                .tooltip(Tooltip::text("Edit Comment"))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.edit_comment(id, window, cx)
                                                    },
                                                )),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .debug_selector(move || {
                                                format!("review-panel-delete-comment-{}", id.0)
                                            })
                                            .child(
                                                IconButton::new(
                                                    ("review-panel-delete-comment", index),
                                                    IconName::Trash,
                                                )
                                                .icon_size(IconSize::Small)
                                                .tooltip(Tooltip::text("Delete Comment"))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.delete_comment(id, window, cx)
                                                    },
                                                )),
                                            ),
                                    ),
                            )
                        },
                    )
                    .tooltip({
                        let location = if comment.remote {
                            SharedString::from(format!("{} (on GitHub)", comment.location))
                        } else {
                            comment.location.clone()
                        };
                        move |_, cx| Tooltip::simple(location.clone(), cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected = Some(index);
                        if let Some(TreeEntry::Comment(comment)) = this.entries.get(index).cloned()
                        {
                            this.open_comment(&comment, window, cx);
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
        }
    }

    fn compute_indents(&self, range: Range<usize>) -> SmallVec<[usize; 64]> {
        range
            .map(|index| self.entries.get(index).map_or(0, TreeEntry::depth))
            .collect()
    }

    fn render_header(&self, review: &Entity<ReviewView>, cx: &Context<Self>) -> impl IntoElement {
        let review = review.read(cx);
        let title = review.title_with_repository(cx);
        let summary = review.summary(cx);
        let stack = review
            .pull_request()
            .and_then(|pull_request| pull_request.read(cx).stack_position());
        let status = status_line(
            review.filters(),
            &summary,
            review.diff().read(cx).single_file().is_some(),
        );
        let panel = cx.weak_entity();
        v_flex()
            .px_2()
            .py_1()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_1()
                    .child(div().flex_1().min_w_0().child(Label::new(title).truncate()))
                    .child(
                        IconButton::new("review-panel-start-review", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Start Review…"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(StartReview.boxed_clone(), cx)
                            }),
                    )
                    .child(
                        PopoverMenu::new("review-panel-menu")
                            .trigger_with_tooltip(
                                IconButton::new("review-panel-menu-button", IconName::Ellipsis)
                                    .icon_size(IconSize::Small),
                                Tooltip::text("Filters and Actions"),
                            )
                            .anchor(Anchor::TopRight)
                            .menu(move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.build_menu(None, window, cx))
                                    .log_err()
                            }),
                    ),
            )
            .when_some(stack, |header, stack| {
                let stack_button = |id: &'static str,
                                    icon: IconName,
                                    title: &'static str,
                                    neighbor: Option<u64>,
                                    action: Box<dyn Action>| {
                    IconButton::new(id, icon)
                        .icon_size(IconSize::XSmall)
                        .disabled(neighbor.is_none())
                        .tooltip(Tooltip::text(match neighbor {
                            Some(number) => format!("{title}: #{number}"),
                            None => format!("{title}: none open"),
                        }))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(action.boxed_clone(), cx)
                        })
                };
                header.child(
                    h_flex()
                        .debug_selector(|| "review-panel-stack".into())
                        .gap_1()
                        .child(
                            Label::new(format!(
                                "Stack #{} · {} of {}",
                                stack.stack,
                                stack.position + 1,
                                stack.count
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .child(stack_button(
                            "review-panel-previous-in-stack",
                            IconName::ArrowLeft,
                            "Previous Pull Request in Stack",
                            stack.below,
                            PreviousPullRequestInStack.boxed_clone(),
                        ))
                        .child(stack_button(
                            "review-panel-next-in-stack",
                            IconName::ArrowRight,
                            "Next Pull Request in Stack",
                            stack.above,
                            NextPullRequestInStack.boxed_clone(),
                        )),
                )
            })
            .child(
                Label::new(format!("{}/{} reviewed", summary.reviewed, summary.total))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            // It wraps: it says what hides files, and has no other place to be read in full.
            .when_some(status, |header, status| {
                header.child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
    }

    fn render_empty_state(
        &self,
        review: Option<&Entity<ReviewView>>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let content = match review {
            None => v_flex()
                .gap_2()
                .items_center()
                .child(Label::new("No review open").color(Color::Muted))
                .child(
                    Button::new("review-panel-start", "Review Uncommitted Changes").on_click(
                        |_, window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx),
                    ),
                )
                .child(
                    Button::new("review-panel-start-review", "Start Review…").on_click(
                        |_, window, cx| window.dispatch_action(StartReview.boxed_clone(), cx),
                    ),
                )
                .into_any_element(),
            Some(review) if !review.read(cx).diff().read(cx).files(cx).is_empty() => {
                let review = review.read(cx);
                let summary = review.summary(cx);
                let filters = review.filters();
                v_flex()
                    .gap_2()
                    .items_center()
                    .child(Label::new(review.diff().read(cx).empty_label(cx)).color(Color::Muted))
                    .when(
                        filters.include.is_some() || filters.exclude.is_some(),
                        |element| {
                            element.child(
                                Button::new("review-panel-clear-filters", "Clear Filters")
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(ClearFilters.boxed_clone(), cx)
                                    }),
                            )
                        },
                    )
                    .when(summary.reviewed_hidden > 0, |element| {
                        element.child(
                            Button::new("review-panel-show-reviewed", "Show Reviewed Files")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(ToggleHideReviewed.boxed_clone(), cx)
                                }),
                        )
                    })
                    .when(summary.generated_hidden > 0, |element| {
                        element.child(
                            Button::new("review-panel-show-generated", "Show Generated Files")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(ToggleShowGenerated.boxed_clone(), cx)
                                }),
                        )
                    })
                    .when(summary.vendored_hidden > 0, |element| {
                        element.child(
                            Button::new("review-panel-show-vendored", "Show Vendored Files")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(ToggleShowVendored.boxed_clone(), cx)
                                }),
                        )
                    })
                    .into_any_element()
            }
            Some(review) => {
                let diff = review.read(cx).diff().read(cx);
                if diff.is_loading(cx) {
                    Icon::new(IconName::LoadCircle)
                        .color(Color::Accent)
                        .with_rotate_animation(3)
                        .into_any_element()
                } else {
                    Label::new(diff.empty_label(cx))
                        .color(Color::Muted)
                        .into_any_element()
                }
            }
        };
        v_flex()
            .flex_1()
            .size_full()
            .p_2()
            .items_center()
            .justify_center()
            .child(content)
            .into_any_element()
    }
}

#[cfg(test)]
impl ReviewPanel {
    /// The rows as text: two spaces per level, `/` after directories (`▸` when collapsed), the
    /// status letter (and `✓` when reviewed) after files. The selected row starts with `>`.
    pub(crate) fn rows_for_test(&self) -> Vec<String> {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let marker = if self.selected == Some(index) {
                    ">"
                } else {
                    ""
                };
                let indent = "  ".repeat(entry.depth());
                match entry {
                    TreeEntry::Directory { name, expanded, .. } => {
                        let collapsed = if *expanded { "" } else { " ▸" };
                        format!("{marker}{indent}{name}/{collapsed}")
                    }
                    TreeEntry::File { file, name, .. } => {
                        let (letter, _) = status_letter(file.status);
                        let reviewed = if file.reviewed { " ✓" } else { "" };
                        let kind = if file.attributes.generated {
                            " (generated)"
                        } else if file.attributes.vendored {
                            " (vendored)"
                        } else {
                            ""
                        };
                        format!("{marker}{indent}{name}{kind} {letter}{reviewed}")
                    }
                    TreeEntry::CommentsHeader { count } => format!("{marker}Comments ({count})"),
                    TreeEntry::Comment(comment) => {
                        let comment_type = comment
                            .comment_type
                            .map(|comment_type| format!("[{}] ", comment_type.label()))
                            .unwrap_or_default();
                        let state = if comment.outdated {
                            " (outdated)"
                        } else if comment.submitted {
                            " (submitted)"
                        } else {
                            ""
                        };
                        format!(
                            "{marker}{comment_type}{} {}{state}",
                            comment.location, comment.first_line
                        )
                    }
                }
            })
            .collect()
    }
}

/// Describes the active filters and what they hide, when anything applies.
fn status_line(
    filters: &ReviewFilters,
    summary: &ReviewSummary,
    single_file: bool,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(include) = &filters.include {
        parts.push(format!("include {}", include.as_str()));
    }
    if let Some(exclude) = &filters.exclude {
        parts.push(format!("exclude {}", exclude.as_str()));
    }
    if summary.filtered_out > 0 {
        parts.push(format!("{} filtered out", summary.filtered_out));
    }
    if summary.generated_hidden > 0 {
        parts.push(format!("{} generated hidden", summary.generated_hidden));
    }
    if summary.vendored_hidden > 0 {
        parts.push(format!("{} vendored hidden", summary.vendored_hidden));
    }
    if summary.reviewed_hidden > 0 {
        parts.push(format!("{} reviewed hidden", summary.reviewed_hidden));
    }
    if single_file {
        parts.push("single file".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The status letter of a file row and its color.
fn status_letter(status: FileStatus) -> (&'static str, Color) {
    if status.is_conflicted() {
        ("U", Color::VersionControlConflict)
    } else if status.is_created() {
        ("A", Color::VersionControlAdded)
    } else if status.is_deleted() {
        ("D", Color::VersionControlDeleted)
    } else {
        ("M", Color::VersionControlModified)
    }
}

fn render_prompt(prompt: &Prompt, cx: &App) -> impl IntoElement {
    let label = match prompt.kind {
        PromptKind::Include => "Include",
        PromptKind::Exclude => "Exclude",
        PromptKind::Search => "Search",
    };
    v_flex()
        .px_2()
        .py_1()
        .gap_0p5()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            h_flex()
                .gap_2()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .child(div().flex_1().child(prompt.editor.clone())),
        )
        .when_some(prompt.error.clone(), |element, error| {
            element.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
        })
}

/// A tree row's chevron and icon, laid out as the project panel lays out its rows.
fn render_tree_icons(
    chevron: Option<SharedString>,
    icon: Option<SharedString>,
    reserves_chevron_slot: bool,
) -> Div {
    let spacer = || {
        h_flex()
            .size(IconSize::default().rems())
            .invisible()
            .flex_none()
    };
    let has_chevron = chevron.is_some();
    h_flex()
        .flex_none()
        .gap_0p5()
        .map(|icons| match chevron {
            Some(chevron) => icons.child(Icon::from_path(chevron).color(Color::Muted)),
            None if reserves_chevron_slot => icons.child(spacer()),
            None => icons,
        })
        .map(|icons| match icon {
            Some(icon) => icons.child(Icon::from_path(icon).color(Color::Muted)),
            // The chevron already fills the icon's slot.
            None if has_chevron => icons,
            None => icons.child(spacer()),
        })
}

/// A tree row's name, as tall as a project panel row's. Its zero width and hidden overflow keep
/// a long name out of the row's minimum width, so `ListItem` cuts the name off instead of
/// pushing the row's status out of the panel.
fn render_tree_label(label: Label) -> Div {
    h_flex()
        .h_6()
        .w_0()
        .flex_grow_1()
        .overflow_hidden()
        .gap_1()
        .child(div().min_w_0().child(label))
}

/// The project panel's indent guides: at its icon column, and short of the rows' edges.
fn render_indent_guides(
    params: ui::RenderIndentGuideParams,
) -> SmallVec<[ui::RenderedIndentGuide; 12]> {
    const PADDING_Y: Pixels = px(4.);
    params
        .indent_guides
        .into_iter()
        .map(|layout| {
            let padding = if layout.continues_offscreen {
                px(0.)
            } else {
                PADDING_Y
            };
            ui::RenderedIndentGuide {
                bounds: Bounds::new(
                    point(
                        layout.offset.x * params.indent_size
                            + ui::LIST_ITEM_INDENT_GUIDE_LEFT_OFFSET,
                        layout.offset.y * params.item_height + padding,
                    ),
                    size(px(1.), layout.length * params.item_height - padding * 2.),
                ),
                layout,
                is_active: false,
                hitbox: None,
            }
        })
        .collect()
}

impl EventEmitter<PanelEvent> for ReviewPanel {}

impl Focusable for ReviewPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let review = self.active_review();
        let project_panel_settings = ProjectPanelSettings::get_global(cx);
        let indent_size = project_panel_settings.indent_size;
        let show_indent_guides =
            project_panel_settings.indent_guides.show == ShowIndentGuides::Always;
        let editing = self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.editor.focus_handle(cx).is_focused(window));
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("ReviewPanel");
        key_context.add(if editing { "editing" } else { "not_editing" });

        v_flex()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::expand_selected_entry))
            .on_action(cx.listener(Self::collapse_selected_entry))
            .on_action(cx.listener(Self::toggle_selected_directory))
            .on_action(cx.listener(Self::toggle_file_reviewed))
            .on_action(cx.listener(Self::select_next_match))
            .on_action(cx.listener(Self::select_previous_match))
            .on_action(cx.listener(Self::cancel_editor))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .when_some(review.as_ref(), |panel, review| {
                panel.child(self.render_header(review, cx))
            })
            .when_some(self.prompt.as_ref(), |panel, prompt| {
                panel.child(render_prompt(prompt, cx))
            })
            .map(|panel| {
                if self.entries.is_empty() {
                    panel.child(self.render_empty_state(review.as_ref(), cx))
                } else {
                    panel.child(
                        uniform_list(
                            "review-panel-entries",
                            self.entries.len(),
                            cx.processor(|this, range: Range<usize>, window, cx| {
                                let focused = this.focus_handle.contains_focused(window, cx);
                                range
                                    .filter_map(|index| {
                                        let entry = this.entries.get(index)?;
                                        Some(this.render_entry(index, entry, focused, cx))
                                    })
                                    .collect()
                            }),
                        )
                        .when(show_indent_guides, |list| {
                            list.with_decoration(
                                ui::indent_guides(px(indent_size), IndentGuideColors::panel(cx))
                                    .with_compute_indents_fn(cx.entity(), |this, range, _, _| {
                                        this.compute_indents(range)
                                    })
                                    .with_render_fn(cx.entity(), |_, params, _, _| {
                                        render_indent_guides(params)
                                    }),
                            )
                        })
                        .flex_1()
                        .size_full()
                        .track_scroll(&self.scroll_handle),
                    )
                }
            })
            .when(review.is_some(), |panel| {
                panel.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.deploy_context_menu(event.position, None, window, cx)
                    }),
                )
            })
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Panel for ReviewPanel {
    fn persistent_name() -> &'static str {
        "Review Panel"
    }

    fn panel_key() -> &'static str {
        REVIEW_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    // Docks move panels by re-reading `position` when settings change, and there is no
    // `code_review` setting to persist another position yet.
    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Left
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Review Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        add_review_panel, click_selector, dispatch, excerpt_paths, init_vim_test, is_folded,
        is_reviewed, move_to_row_with_text, open_tree_fixture, path_at_cursor, redraw, rhs_editor,
    };
    use gpui::{TestAppContext, VisualTestContext};
    use settings::DiffViewStyle;

    fn rows(panel: &Entity<ReviewPanel>, cx: &mut VisualTestContext) -> Vec<String> {
        panel.read_with(cx, |panel, _| panel.rows_for_test())
    }

    /// The rows without the selection marker or indentation.
    fn names(panel: &Entity<ReviewPanel>, cx: &mut VisualTestContext) -> Vec<String> {
        rows(panel, cx)
            .into_iter()
            .map(|row| row.trim_start_matches('>').trim_start().to_string())
            .collect()
    }

    fn selected(panel: &Entity<ReviewPanel>, cx: &mut VisualTestContext) -> String {
        rows(panel, cx)
            .into_iter()
            .find_map(|row| Some(row.strip_prefix('>')?.trim_start().to_string()))
            .unwrap_or_default()
    }

    fn prompt(
        panel: &Entity<ReviewPanel>,
        cx: &mut VisualTestContext,
    ) -> Option<(String, Option<SharedString>)> {
        panel.read_with(cx, |panel, cx| {
            let prompt = panel.prompt.as_ref()?;
            Some((prompt.editor.read(cx).text(cx), prompt.error.clone()))
        })
    }

    fn header(panel: &Entity<ReviewPanel>, cx: &mut VisualTestContext) -> (String, Option<String>) {
        panel.read_with(cx, |panel, cx| {
            let review = panel.active_review().unwrap();
            let review = review.read(cx);
            let summary = review.summary(cx);
            let single_file = review.diff().read(cx).single_file().is_some();
            (
                format!("{}/{} reviewed", summary.reviewed, summary.total),
                status_line(review.filters(), &summary, single_file),
            )
        })
    }

    /// Hovering a file row swaps its ✓ for a checkbox, which marks the file without selecting
    /// the row or taking the focus; hovering a comment row shows its edit and delete buttons.
    #[gpui::test]
    async fn test_panel_hover_controls(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_tree_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_panel::<ReviewPanel>(window, cx)
        });
        cx.focus(&review);
        cx.run_until_parked();
        let rhs = rhs_editor(&review, cx);
        let diff_focused = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| rhs.read(cx).focus_handle(cx).is_focused(window))
        };
        let hover = |selector: &'static str, cx: &mut VisualTestContext| {
            redraw(cx);
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} isn't drawn"));
            cx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::none());
            redraw(cx);
        };
        let selected_before = selected(&panel, cx);

        // Not shown until the row is hovered.
        redraw(cx);
        assert!(
            cx.debug_bounds("review-panel-reviewed-src/lib.rs")
                .is_none()
        );
        hover("review-panel-file-status-src/lib.rs", cx);
        click_selector("review-panel-reviewed-src/lib.rs", cx);
        assert!(is_reviewed(&review, "src/lib.rs", cx));
        assert!(is_folded(&review, "src/lib.rs", cx));
        assert_eq!(selected(&panel, cx), selected_before);
        assert!(diff_focused(cx));
        hover("review-panel-reviewed-src/lib.rs", cx);
        click_selector("review-panel-reviewed-src/lib.rs", cx);
        assert!(!is_reviewed(&review, "src/lib.rs", cx));

        move_to_row_with_text(&rhs, "LINE 2", cx);
        cx.simulate_keystrokes("c");
        cx.simulate_input("two");
        cx.simulate_keystrokes("ctrl-enter");
        let id = review.read_with(cx, |review, cx| {
            review
                .session()
                .read(cx)
                .comments()
                .iter()
                .next()
                .unwrap()
                .id
        });
        let is_editing = |cx: &mut VisualTestContext| {
            review.read_with(cx, |review, cx| review.comments().read(cx).is_editing(cx))
        };
        // The comment's line scrolls away; the edit button brings it back, editing.
        cx.simulate_keystrokes("G");
        hover("review-comment-location: two", cx);
        click_selector(format!("review-panel-edit-comment-{}", id.0).leak(), cx);
        assert!(is_editing(cx));
        cx.simulate_input("too ");
        cx.simulate_keystrokes("ctrl-enter");
        assert!(!is_editing(cx));
        let bodies = |cx: &mut VisualTestContext| {
            review.read_with(cx, |review, cx| {
                review
                    .session()
                    .read(cx)
                    .comments()
                    .iter()
                    .map(|comment| comment.body.clone())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(bodies(cx), ["too two"]);

        hover("review-comment-location: too two", cx);
        click_selector(format!("review-panel-delete-comment-{}", id.0).leak(), cx);
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(bodies(cx).is_empty());
    }

    #[gpui::test]
    async fn test_panel_selection_shows_the_file_in_single_file_mode(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_tree_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        cx.run_until_parked();
        cx.simulate_keystrokes("; f");
        assert_eq!(excerpt_paths(&review, cx), ["docs/old.md"]);

        cx.simulate_keystrokes("; h j j j");
        assert_eq!(selected(&panel, cx), "c.rs A");
        assert_eq!(excerpt_paths(&review, cx), ["src/a/b/c.rs"]);
        cx.simulate_keystrokes("G");
        assert_eq!(excerpt_paths(&review, cx), ["z.txt"]);
        cx.simulate_keystrokes("g g j");
        assert_eq!(excerpt_paths(&review, cx), ["docs/old.md"]);
    }

    #[gpui::test]
    async fn test_panel_tree(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_tree_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        let editor = rhs_editor(&review, cx);
        cx.run_until_parked();

        // Directories first, single-child chains compacted, in the diff's order. Generated and
        // vendored files are hidden by default.
        assert_eq!(
            rows(&panel, cx),
            [
                "docs/",
                ">  old.md D",
                "src/",
                "  a/b/",
                "    c.rs A",
                "  lib.rs M",
                "  mod_b.rs M",
                "z.txt M",
            ]
        );
        dispatch(crate::ToggleShowGenerated, cx);
        dispatch(crate::ToggleShowVendored, cx);
        assert_eq!(
            rows(&panel, cx),
            [
                "docs/",
                ">  old.md D",
                "src/",
                "  a/b/",
                "    c.rs A",
                "  lib.rs M",
                "  mod_b.rs M",
                "vendor/",
                "  x.js (vendored) A",
                "Cargo.lock (generated) M",
                "z.txt M",
            ]
        );
        let excerpts = crate::test_support::excerpt_paths(&review, cx);
        assert_eq!(
            excerpts,
            [
                "docs/old.md",
                "src/a/b/c.rs",
                "src/lib.rs",
                "src/mod_b.rs",
                "vendor/x.js",
                "Cargo.lock",
                "z.txt"
            ]
        );

        let panel_focused = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| panel.read(cx).focus_handle(cx).contains_focused(window, cx))
        };
        cx.simulate_keystrokes("; h");
        assert!(panel_focused(cx));

        // `space` toggles a directory; `l` enters an expanded one.
        cx.simulate_keystrokes("j space");
        assert_eq!(rows(&panel, cx)[2..4], [">src/ ▸", "vendor/"]);
        cx.simulate_keystrokes("space l l");
        assert_eq!(rows(&panel, cx)[4], ">    c.rs A");

        // `l` on a file shows it in the diff and keeps the focus here.
        cx.simulate_keystrokes("l");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("src/a/b/c.rs"));
        assert!(panel_focused(cx));

        // `h` selects the parent directory, then collapses it.
        cx.simulate_keystrokes("h");
        assert_eq!(rows(&panel, cx)[3], ">  a/b/");
        cx.simulate_keystrokes("h");
        assert_eq!(rows(&panel, cx)[3], ">  a/b/ ▸");
        assert_eq!(rows(&panel, cx)[4], "  lib.rs M");

        // Enter on a file moves the diff there and focuses it.
        cx.simulate_keystrokes("j enter");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("src/lib.rs"));
        cx.update(|window, cx| assert!(editor.focus_handle(cx).is_focused(window)));

        // The selection follows the diff's cursor.
        cx.simulate_keystrokes("] f");
        assert_eq!(rows(&panel, cx)[5], ">  mod_b.rs M");
        // A file in a collapsed directory selects the directory.
        cx.simulate_keystrokes("[ f [ f");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("src/a/b/c.rs"));
        assert_eq!(rows(&panel, cx)[3], ">  a/b/ ▸");

        cx.simulate_keystrokes("; h shift-g");
        assert_eq!(rows(&panel, cx).last().unwrap(), ">z.txt M");
        cx.simulate_keystrokes("g g");
        assert_eq!(rows(&panel, cx)[0], ">docs/");
        cx.simulate_keystrokes("2 j");
        assert_eq!(rows(&panel, cx)[2], ">src/");
    }

    #[gpui::test]
    async fn test_filters_and_search(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_tree_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        let all_files = excerpt_paths(&review, cx);
        let panel_focused = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| panel.read(cx).focus_handle(cx).contains_focused(window, cx))
        };

        // Include (case-insensitive) hides the rest in the diff and the panel, and moves the
        // cursor off a hidden file.
        cx.simulate_keystrokes("; h i");
        assert_eq!(prompt(&panel, cx), Some((String::new(), None)));
        cx.simulate_input("^SRC/");
        cx.simulate_keystrokes("enter");
        assert_eq!(prompt(&panel, cx), None);
        assert!(panel_focused(cx));
        assert_eq!(
            excerpt_paths(&review, cx),
            ["src/a/b/c.rs", "src/lib.rs", "src/mod_b.rs"]
        );
        assert_eq!(
            names(&panel, cx),
            ["src/", "a/b/", "c.rs A", "lib.rs M", "mod_b.rs M"]
        );
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("src/a/b/c.rs"));
        let (count, status) = header(&panel, cx);
        assert_eq!(count, "0/3 reviewed");
        // Generated and vendored files count as such, not as filtered out (V2.08).
        assert_eq!(
            status.as_deref(),
            Some("include ^SRC/ · 2 filtered out · 1 generated hidden · 1 vendored hidden")
        );

        // File navigation stays among the shown files.
        cx.simulate_keystrokes("; l ] f ] f ] f");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("src/mod_b.rs"));

        // Exclude composes with include.
        cx.simulate_keystrokes("; h e");
        cx.simulate_input("mod_");
        cx.simulate_keystrokes("enter");
        assert_eq!(excerpt_paths(&review, cx), ["src/a/b/c.rs", "src/lib.rs"]);
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("src/a/b/c.rs"));

        // Prompts start with the current pattern; an invalid one keeps the prompt open.
        cx.simulate_keystrokes("i");
        assert_eq!(prompt(&panel, cx), Some(("^SRC/".into(), None)));
        cx.simulate_keystrokes("escape");
        assert_eq!(prompt(&panel, cx), None);
        assert!(panel_focused(cx));
        cx.simulate_keystrokes("e");
        cx.simulate_input("(");
        cx.simulate_keystrokes("enter");
        let (text, error) = prompt(&panel, cx).unwrap();
        assert_eq!(text, "mod_(");
        assert_eq!(error.as_deref(), Some("Invalid regex: unclosed group"));
        cx.simulate_keystrokes("escape");
        assert_eq!(excerpt_paths(&review, cx), ["src/a/b/c.rs", "src/lib.rs"]);

        // `I` and `E` clear the patterns.
        cx.simulate_keystrokes("shift-i shift-e");
        assert_eq!(excerpt_paths(&review, cx), all_files);
        assert_eq!(
            header(&panel, cx).1.as_deref(),
            Some("1 generated hidden · 1 vendored hidden")
        );

        // Path search expands collapsed directories, and `n`/`N` wrap.
        cx.simulate_keystrokes("g g j j");
        assert_eq!(selected(&panel, cx), "src/");
        // `; h` in the panel stays there, and doesn't collapse the directory as `h` would.
        cx.simulate_keystrokes("; h");
        assert_eq!(selected(&panel, cx), "src/");
        assert!(panel_focused(cx));
        cx.simulate_keystrokes("space");
        assert_eq!(selected(&panel, cx), "src/ ▸");
        cx.simulate_keystrokes("/");
        cx.simulate_input(".RS");
        cx.simulate_keystrokes("enter");
        assert_eq!(selected(&panel, cx), "c.rs A");
        cx.simulate_keystrokes("n");
        assert_eq!(selected(&panel, cx), "lib.rs M");
        cx.simulate_keystrokes("n n");
        assert_eq!(selected(&panel, cx), "c.rs A");
        cx.simulate_keystrokes("shift-n");
        assert_eq!(selected(&panel, cx), "mod_b.rs M");
        // The prompt starts with the last search, selected: typing replaces it.
        cx.simulate_keystrokes("/");
        assert_eq!(prompt(&panel, cx), Some((".RS".into(), None)));
        cx.simulate_input("lib");
        cx.simulate_keystrokes("enter");
        assert_eq!(prompt(&panel, cx), None);
        assert_eq!(selected(&panel, cx), "lib.rs M");
        // A search nothing matches keeps the prompt open, and says so.
        cx.simulate_keystrokes("/");
        cx.simulate_input("nothing");
        cx.simulate_keystrokes("enter");
        let (text, error) = prompt(&panel, cx).unwrap();
        assert_eq!(text, "nothing");
        assert_eq!(
            error.as_deref(),
            Some("No file path contains \u{201c}nothing\u{201d}")
        );
        cx.simulate_keystrokes("escape");
        assert_eq!(selected(&panel, cx), "lib.rs M");

        // With everything hidden, the generated and vendored files (hidden anyway) aren't
        // counted among the changed files.
        cx.simulate_keystrokes("i");
        cx.simulate_input("nothing matches");
        cx.simulate_keystrokes("enter");
        let empty_label = |cx: &mut VisualTestContext| {
            review.read_with(cx, |review, cx| {
                review.diff().read(cx).empty_label(cx).to_string()
            })
        };
        assert_eq!(empty_label(cx), "All 5 changed files are hidden");
        dispatch(crate::ToggleShowGenerated, cx);
        assert_eq!(empty_label(cx), "All 6 changed files are hidden");
    }

    #[gpui::test]
    async fn test_hide_reviewed(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_tree_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        let all_files = excerpt_paths(&review, cx);
        dispatch(crate::ToggleHideReviewed, cx);

        // Marking hides the file and moves on.
        cx.simulate_keystrokes("r");
        assert!(is_reviewed(&review, "docs/old.md", cx));
        assert!(!excerpt_paths(&review, cx).contains(&"docs/old.md".to_string()));
        assert!(!names(&panel, cx).contains(&"old.md D ✓".to_string()));
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("src/a/b/c.rs"));
        let total = all_files.len();
        assert_eq!(header(&panel, cx).0, format!("1/{total} reviewed"));
        assert_eq!(
            header(&panel, cx).1.as_deref(),
            Some("1 generated hidden · 1 vendored hidden · 1 reviewed hidden")
        );

        // On the last file the cursor wraps around to the first shown file.
        let last = all_files.last().unwrap().clone();
        for _ in 0..all_files.len() {
            cx.simulate_keystrokes("] f");
        }
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some(last.as_str()));
        cx.simulate_keystrokes("r");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("src/a/b/c.rs"));

        // Everything reviewed: the diff and the panel say so.
        for _ in 0..all_files.len() {
            cx.simulate_keystrokes("r");
        }
        assert_eq!(excerpt_paths(&review, cx), Vec::<String>::new());
        assert_eq!(
            review.read_with(cx, |review, cx| review.diff().read(cx).empty_label(cx)),
            "All files reviewed"
        );
        assert!(names(&panel, cx).is_empty());

        // Showing reviewed files again brings them back folded.
        dispatch(crate::ToggleHideReviewed, cx);
        assert_eq!(excerpt_paths(&review, cx), all_files);
        assert!(is_folded(&review, "src/lib.rs", cx));
    }

    /// `]t` from one side of a split diff onto the other side's item in another file selects
    /// that file (R2: from an old line on the left side, the panel stayed on the first file).
    #[gpui::test]
    async fn test_panel_follows_comment_jumps_across_sides(cx: &mut TestAppContext) {
        use crate::test_support::{lhs_editor, move_to_row_with_text, open_review_fixture};

        init_vim_test(cx, DiffViewStyle::Split);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        let rhs = rhs_editor(&review, cx);
        let lhs = lhs_editor(&review, cx);
        let comment = |cx: &mut VisualTestContext, text: &str| {
            cx.simulate_keystrokes("c");
            cx.simulate_input(text);
            cx.simulate_keystrokes("ctrl-enter");
        };
        move_to_row_with_text(&rhs, "three", cx);
        comment(cx, "new side");
        cx.simulate_keystrokes("ctrl-w h");
        move_to_row_with_text(&lhs, "gone 1", cx);
        comment(cx, "old side");

        move_to_row_with_text(&lhs, "line 2", cx);
        assert_eq!(selected(&panel, cx), "a.rs M");
        cx.simulate_keystrokes("] t");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("deleted.txt"));
        assert_eq!(selected(&panel, cx), "deleted.txt D");
        // From an old line on the left side onto the right side of the next file, and around
        // onto the left side again.
        cx.simulate_keystrokes("] t");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("modified.txt"));
        assert_eq!(selected(&panel, cx), "modified.txt M");
        cx.simulate_keystrokes("] t");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("deleted.txt"));
        assert_eq!(selected(&panel, cx), "deleted.txt D");
    }

    #[gpui::test]
    async fn test_panel_follows_active_review(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_tree_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_review()),
            Some(review.clone())
        );

        // A split shares the reviewed marks but has its own filters.
        dispatch(workspace::SplitRight::default(), cx);
        let other = crate::test_support::active_review(&workspace, cx);
        assert_ne!(other, review);
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_review()),
            Some(other.clone())
        );
        cx.focus(&other);
        cx.simulate_keystrokes("r");
        assert!(is_reviewed(&review, "docs/old.md", cx));
        assert!(is_folded(&review, "docs/old.md", cx));
        cx.simulate_keystrokes("; h i");
        cx.simulate_input("lib");
        cx.simulate_keystrokes("enter");
        assert_eq!(excerpt_paths(&other, cx), ["src/lib.rs"]);
        assert_eq!(names(&panel, cx), ["src/", "lib.rs M"]);
        assert_eq!(excerpt_paths(&review, cx).len(), 5);

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&review, true, true, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_review()),
            Some(review.clone())
        );
        assert_eq!(names(&panel, cx)[1], "old.md D ✓");

        // Closing every review empties the panel.
        for _ in 0..2 {
            dispatch(workspace::CloseActiveItem::default(), cx);
            if let Some(review) = workspace.read_with(cx, |workspace, cx| {
                workspace.items_of_type::<ReviewView>(cx).next()
            }) {
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.activate_item(&review, true, true, window, cx)
                });
                cx.run_until_parked();
            }
        }
        assert_eq!(panel.read_with(cx, |panel, _| panel.active_review()), None);
        assert!(rows(&panel, cx).is_empty());
    }

    #[gpui::test]
    async fn test_generated_and_vendored_files(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (fs, workspace, review, cx) = open_tree_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        let shown = [
            "docs/old.md",
            "src/a/b/c.rs",
            "src/lib.rs",
            "src/mod_b.rs",
            "z.txt",
        ];
        assert_eq!(excerpt_paths(&review, cx), shown);
        assert_eq!(header(&panel, cx).0, "0/5 reviewed");
        assert_eq!(
            header(&panel, cx).1.as_deref(),
            Some("1 generated hidden · 1 vendored hidden")
        );

        dispatch(crate::ToggleShowGenerated, cx);
        assert!(excerpt_paths(&review, cx).contains(&"Cargo.lock".to_string()));
        assert!(names(&panel, cx).contains(&"Cargo.lock (generated) M".to_string()));
        assert_eq!(header(&panel, cx).1.as_deref(), Some("1 vendored hidden"));
        dispatch(crate::ToggleShowGenerated, cx);
        assert_eq!(excerpt_paths(&review, cx), shown);

        // Attributes are read again when the file list changes.
        fs.insert_file(
            util::path!("/project/.gitattributes"),
            "Cargo.lock linguist-generated\n".into(),
        )
        .await;
        cx.run_until_parked();
        assert_eq!(
            excerpt_paths(&review, cx),
            [
                "docs/old.md",
                "src/a/b/c.rs",
                "src/lib.rs",
                "src/mod_b.rs",
                "vendor/x.js",
                ".gitattributes",
                "z.txt"
            ]
        );
        assert_eq!(header(&panel, cx).1.as_deref(), Some("1 generated hidden"));
    }

    #[gpui::test]
    async fn test_context_menu_and_copy_actions(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (fs, workspace, _review, cx) = open_tree_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        fs.set_branch_name(crate::test_support::dot_git(), Some("feature/review"));
        cx.run_until_parked();

        dispatch(crate::CopyBranchName, cx);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("feature/review")
        );

        cx.simulate_keystrokes("; h");
        panel.update_in(cx, |panel, window, cx| {
            let file = panel.files(cx).into_iter().next();
            panel.deploy_context_menu(Point::default(), file, window, cx);
        });
        cx.run_until_parked();
        let menu = panel.read_with(cx, |panel, _| {
            panel.context_menu.as_ref().map(|(menu, _, _)| menu.clone())
        });
        let menu = menu.unwrap();
        cx.update(|window, cx| assert!(menu.focus_handle(cx).contains_focused(window, cx)));
        dispatch(menu::Cancel, cx);
        assert!(panel.read_with(cx, |panel, _| panel.context_menu.is_none()));
        cx.update(|window, cx| assert!(panel.focus_handle(cx).contains_focused(window, cx)));
    }

    #[gpui::test]
    async fn test_panel_lists_comments(cx: &mut TestAppContext) {
        use crate::test_support::{cursor_line, move_to_row_with_text, open_review_fixture};

        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        let editor = rhs_editor(&review, cx);
        cx.run_until_parked();
        let comment_rows = |cx: &mut VisualTestContext| {
            let rows = rows(&panel, cx);
            let start = rows
                .iter()
                .position(|row| row.trim_start_matches('>').starts_with("Comments"));
            start.map_or(Vec::new(), |start| rows[start..].to_vec())
        };
        assert!(comment_rows(cx).is_empty());

        for (row_text, keys, body) in [
            ("ONE", "shift-c", "file note"),
            ("line 3", "c", "first line\nsecond line"),
            ("gone 1", "c", "removed"),
            ("line 4", "; c", "overall"),
        ] {
            move_to_row_with_text(&editor, row_text, cx);
            cx.simulate_keystrokes(keys);
            cx.simulate_input(body);
            cx.simulate_keystrokes("escape");
            if body == "removed" {
                cx.simulate_keystrokes("tab");
            }
            cx.simulate_keystrokes("ctrl-enter");
        }
        // Review comments first, then in the review's file order.
        assert_eq!(
            comment_rows(cx),
            [
                "Comments (4)",
                "Review overall",
                "a.rs:3 first line",
                "[ISSUE] deleted.txt:~1 removed",
                "modified.txt file note",
            ]
        );

        // Enter on a comment moves the diff there, like `]t`.
        cx.simulate_keystrokes("; h shift-g");
        assert_eq!(selected(&panel, cx), "modified.txt file note");
        cx.simulate_keystrokes("k enter");
        assert!(cx.update(|window, cx| editor.focus_handle(cx).is_focused(window)));
        assert_eq!(cursor_line(&editor, cx), "gone 1");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("deleted.txt"));

        // A review comment opens for editing.
        cx.simulate_keystrokes("; h shift-g k k k");
        assert_eq!(selected(&panel, cx), "Review overall");
        // The heading isn't selectable.
        cx.simulate_keystrokes("k");
        assert_ne!(selected(&panel, cx), "Comments (4)");
        cx.simulate_keystrokes("j enter");
        let open = review.read_with(cx, |review, cx| review.comments().read(cx).is_editing(cx));
        assert!(open);
        cx.simulate_keystrokes("escape escape");

        // Deleting a comment removes its row.
        move_to_row_with_text(&editor, "line 3", cx);
        cx.simulate_keystrokes("d d");
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert_eq!(comment_rows(cx).len(), 4);
        assert_eq!(comment_rows(cx)[0], "Comments (3)");

        // When the selected row goes away, the selection moves to a row next to it, never to
        // the heading.
        dispatch(crate::ToggleHideReviewed, cx);
        let last_file = names(&panel, cx)
            .iter()
            .position(|name| name.starts_with("modified.txt"))
            .unwrap();
        cx.simulate_keystrokes("; h g g");
        for _ in 0..last_file {
            cx.simulate_keystrokes("j");
        }
        assert!(selected(&panel, cx).starts_with("modified.txt"));
        cx.simulate_keystrokes("r");
        assert!(is_reviewed(&review, "modified.txt", cx));
        assert!(selected(&panel, cx).starts_with("long.txt"));
    }

    /// A comment's location shows in full however short its first line is (V3.17).
    #[gpui::test]
    async fn test_comment_location_isnt_cut_off(cx: &mut TestAppContext) {
        use crate::test_support::{move_to_row_with_text, open_review_fixture};

        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        add_review_panel(&workspace, cx);
        let editor = rhs_editor(&review, cx);
        for body in ["x", "a first line too long for the panel, so it is cut off"] {
            move_to_row_with_text(&editor, "line 3", cx);
            cx.simulate_keystrokes("c");
            cx.simulate_input(body);
            cx.simulate_keystrokes("ctrl-enter");
        }
        cx.simulate_keystrokes("; h");
        cx.run_until_parked();
        let short = cx.debug_bounds("review-comment-location: x").unwrap();
        let long = cx
            .debug_bounds(
                "review-comment-location: a first line too long for the panel, so it is cut off",
            )
            .unwrap();
        assert!(short.size.width > px(0.));
        assert_eq!(short.size.width, long.size.width);
    }

    /// The tree is indented and spaced by the project panel's settings, so it looks like the
    /// project panel's tree.
    #[gpui::test]
    async fn test_tree_follows_project_panel_settings(cx: &mut TestAppContext) {
        use gpui::BorrowAppContext as _;
        use settings::SettingsStore;

        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, _review, cx) = open_tree_fixture(cx).await;
        add_review_panel(&workspace, cx);
        cx.simulate_keystrokes("; h");
        cx.run_until_parked();
        let name_bounds =
            |selector: &'static str, cx: &mut VisualTestContext| cx.debug_bounds(selector).unwrap();
        let indent = |cx: &mut VisualTestContext| {
            name_bounds("review-panel-file-name-src/lib.rs", cx)
                .origin
                .x
                - name_bounds("review-panel-file-name-z.txt", cx).origin.x
        };
        let row_height = |cx: &mut VisualTestContext| {
            name_bounds("review-panel-file-name-src/mod_b.rs", cx)
                .origin
                .y
                - name_bounds("review-panel-file-name-src/lib.rs", cx)
                    .origin
                    .y
        };
        assert_eq!(indent(cx), px(20.));
        let comfortable_row_height = row_height(cx);

        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let project_panel = settings.project_panel.get_or_insert_default();
                    project_panel.indent_size = Some(settings::PixelSetting(32.));
                    project_panel.entry_spacing =
                        Some(settings::ProjectPanelEntrySpacing::Standard);
                });
            });
        });
        redraw(cx);
        assert_eq!(indent(cx), px(32.));
        assert!(row_height(cx) < comfortable_row_height);
    }

    /// A long name deep in the tree is cut off; it doesn't push its row's status letter out of
    /// the panel.
    #[gpui::test]
    async fn test_long_names_keep_the_status_in_view(cx: &mut TestAppContext) {
        use crate::test_support::{dot_git, open_workspace};
        use project::FakeFs;
        use serde_json::json;
        use util::path;

        init_vim_test(cx, DiffViewStyle::Unified);
        let fs = FakeFs::new(cx.executor());
        let long_path = "one/two/three/four/five/a_name_far_too_long_for_the_review_panel.txt";
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "short.txt": "ONE\n",
                "one": { "two": { "three": { "four": { "five": {
                    "a_name_far_too_long_for_the_review_panel.txt": "ONE\n",
                } } } } },
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            dot_git(),
            &[("short.txt", "one\n".into()), (long_path, "one\n".into())],
        );
        let (workspace, cx) = open_workspace(fs.clone(), cx).await;
        add_review_panel(&workspace, cx);
        dispatch(ReviewUncommitted, cx);
        cx.run_until_parked();
        redraw(cx);

        let short = cx
            .debug_bounds("review-panel-file-status-short.txt")
            .unwrap();
        let long = cx
            .debug_bounds(
                "review-panel-file-status-one/two/three/four/five/a_name_far_too_long_for_the_review_panel.txt",
            )
            .unwrap();
        assert_eq!(long.right(), short.right());
    }

    /// Opening a review shows the panel in the left dock, whichever panel the dock showed and
    /// whether it was open, and keeps the focus in the diff. Switching between review tabs and
    /// refreshes leave a dock the user closed closed.
    #[gpui::test]
    async fn test_opening_a_review_shows_the_panel(cx: &mut TestAppContext) {
        use crate::{
            ReviewBranch,
            test_support::{
                active_review, cursor_point, dot_git, insert_commit_ref, open_workspace,
                review_count,
            },
        };
        use project::FakeFs;
        use serde_json::json;
        use util::path;
        use workspace::{dock::test::TestPanel, pane::ActivatePreviousItem};

        init_vim_test(cx, DiffViewStyle::Unified);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ ".git": {}, "a.txt": "ONE\nTWO\nthree\n" }),
        )
        .await;
        fs.set_head_and_index_for_repo(dot_git(), &[("a.txt", "one\ntwo\nthree\n".into())]);
        fs.set_merge_base_content_for_repo(dot_git(), &[("a.txt", "one\ntwo\nthree\n".into())]);
        insert_commit_ref(&fs, "main", "1111111111111111111111111111111111111111");
        let (workspace, cx) = open_workspace(fs.clone(), cx).await;
        // Another panel of the left dock (the project panel, say) is its active one, and the
        // dock is closed.
        let other_panel = workspace.update_in(cx, |workspace, window, cx| {
            let other_panel = cx.new(|cx| TestPanel::new(DockPosition::Left, 0, cx));
            workspace.add_panel(other_panel.clone(), window, cx);
            workspace.open_panel::<TestPanel>(window, cx);
            workspace.close_panel::<TestPanel>(window, cx);
            other_panel
        });
        let panel = add_review_panel(&workspace, cx);
        let shown_panel = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let left_dock = workspace.left_dock().read(cx);
                left_dock.visible_panel().map(|panel| panel.panel_id())
            })
        };
        let assert_diff_focused = |review: &Entity<ReviewView>, cx: &mut VisualTestContext| {
            let editor = rhs_editor(review, cx);
            cx.update(|window, cx| assert!(editor.focus_handle(cx).is_focused(window)));
            let row = cursor_point(&editor, cx).row;
            cx.simulate_keystrokes("j");
            assert_ne!(cursor_point(&editor, cx).row, row);
            cx.simulate_keystrokes("k");
        };
        assert_eq!(shown_panel(cx), None);

        dispatch(ReviewUncommitted, cx);
        let uncommitted = active_review(&workspace, cx);
        assert_eq!(shown_panel(cx), Some(panel.entity_id()));
        assert_diff_focused(&uncommitted, cx);

        // The dock shows the other panel.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_panel::<TestPanel>(window, cx)
        });
        assert_eq!(shown_panel(cx), Some(other_panel.entity_id()));
        dispatch(
            ReviewBranch {
                base: Some("main".into()),
            },
            cx,
        );
        let branch = active_review(&workspace, cx);
        assert_ne!(branch, uncommitted);
        assert_eq!(shown_panel(cx), Some(panel.entity_id()));
        assert_diff_focused(&branch, cx);

        // The user closes the dock.
        cx.simulate_keystrokes("; e");
        assert_eq!(shown_panel(cx), None);
        dispatch(ActivatePreviousItem::default(), cx);
        assert_eq!(active_review(&workspace, cx), uncommitted);
        assert_eq!(shown_panel(cx), None);
        dispatch(ActivatePreviousItem::default(), cx);
        assert_eq!(active_review(&workspace, cx), branch);
        assert_eq!(shown_panel(cx), None);
        fs.set_head_and_index_for_repo(dot_git(), &[("a.txt", "one\nTWO\nthree\n".into())]);
        cx.run_until_parked();
        assert_eq!(shown_panel(cx), None);
        assert_diff_focused(&branch, cx);

        // Opening a review that is already open shows the panel too.
        dispatch(ReviewUncommitted, cx);
        assert_eq!(review_count(&workspace, cx), 2);
        assert_eq!(active_review(&workspace, cx), uncommitted);
        assert_eq!(shown_panel(cx), Some(panel.entity_id()));
        assert_diff_focused(&uncommitted, cx);
    }
}
