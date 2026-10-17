use crate::{
    ClearExcludeFilter, ClearFilters, ClearIncludeFilter, CollapseSelectedEntry, CopyBranchName,
    EditExcludeFilter, EditIncludeFilter, ExpandSelectedEntry, FocusPanel, ReviewUncommitted,
    ReviewView, SearchPaths, SelectNextMatch, SelectPreviousMatch, ToggleFileReviewed, ToggleFocus,
    ToggleHideReviewed, TogglePanel, ToggleSelectedDirectory, ToggleShowGenerated,
    ToggleShowVendored,
    view::{PanelFile, ReviewFilters, ReviewSummary, current_review, filter_regex, focus_diff},
};
use anyhow::Result;
use collections::HashSet;
use editor::{Editor, EditorEvent, PathKey, actions::MoveToEnd};
use file_icons::FileIcons;
use git::{repository::RepoPath, status::FileStatus};
use gpui::{
    Action, Anchor, AsyncWindowContext, ClipboardItem, DismissEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, KeyContext, MouseButton, MouseDownEvent, Point,
    ScrollStrategy, Subscription, UniformListScrollHandle, WeakEntity, anchored, deferred,
    uniform_list,
};
use smallvec::SmallVec;
use std::{collections::BTreeMap, ops::Range};
use ui::{
    CommonAnimationExt as _, ContextMenu, IndentGuideColors, ListItem, ListItemSpacing,
    PopoverMenu, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::{NotificationId, NotifyTaskExt as _},
};

const REVIEW_PANEL_KEY: &str = "ReviewPanel";
const TREE_INDENT: f32 = 16.0;

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
        match review.read(cx).branch_name(cx) {
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

/// A row of the file tree.
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
}

impl TreeEntry {
    fn depth(&self) -> usize {
        match self {
            TreeEntry::Directory { depth, .. } | TreeEntry::File { depth, .. } => *depth,
        }
    }

    fn key(&self) -> EntryKey {
        match self {
            TreeEntry::Directory { path, .. } => EntryKey::Directory(path.clone()),
            TreeEntry::File { file, .. } => EntryKey::File(file.path_key.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EntryKey {
    Directory(RepoPath),
    File(PathKey),
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
        self.entries = entries;
        self.selected = match selected_key {
            Some(key) => self
                .entries
                .iter()
                .position(|entry| entry.key() == key)
                .or_else(|| {
                    self.selected
                        .map(|index| index.min(self.entries.len().saturating_sub(1)))
                })
                .filter(|_| !self.entries.is_empty()),
            None => None,
        };
        cx.notify();
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
        if index >= self.entries.len() {
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
        let index = self.selected.map_or(0, |index| index + 1);
        self.select(index, window, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = self
            .selected
            .map_or(self.entries.len().saturating_sub(1), |index| {
                index.saturating_sub(1)
            });
        self.select(index, window, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        self.select(0, window, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        self.select(self.entries.len().saturating_sub(1), window, cx);
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
            None => {}
        }
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
            None => {}
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
            editor.move_to_end(&MoveToEnd, window, cx);
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
                self.search = (!text.is_empty()).then_some(text);
                self.close_prompt(window, cx);
                self.select_match(true, window, cx);
            }
        }
    }

    /// Selects the next (or previous) file whose path contains the search, wrapping around and
    /// expanding the directories around it.
    fn select_match(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_ref().map(|search| search.to_lowercase()) else {
            return;
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
            return;
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
    }

    fn select_next_match(
        &mut self,
        _: &SelectNextMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_match(true, window, cx);
    }

    fn select_previous_match(
        &mut self,
        _: &SelectPreviousMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_match(false, window, cx);
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
            .is_some_and(|review| review.read(cx).branch_name(cx).is_some());
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
            .separator()
            .action_disabled_when(
                !has_branch,
                "Copy Branch Name",
                CopyBranchName.boxed_clone(),
            )
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
        match entry {
            TreeEntry::Directory {
                path,
                name,
                depth,
                expanded,
            } => {
                let icon = FileIcons::get_folder_icon(*expanded, path.as_std_path(), cx)
                    .map(Icon::from_path)
                    .unwrap_or_else(|| {
                        Icon::new(if *expanded {
                            IconName::FolderOpen
                        } else {
                            IconName::Folder
                        })
                    })
                    .size(IconSize::Small)
                    .color(Color::Muted);
                ListItem::new(("review-directory", index))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(*depth)
                    .indent_step_size(px(TREE_INDENT))
                    .toggle_state(selected)
                    .focused(selected && focused)
                    .start_slot(icon)
                    .child(Label::new(name.clone()).size(LabelSize::Small).truncate())
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
                let icon = FileIcons::get_icon(file.repo_path.as_std_path(), cx)
                    .map(Icon::from_path)
                    .unwrap_or_else(|| Icon::new(IconName::File))
                    .size(IconSize::Small)
                    .color(Color::Muted);
                let (letter, color) = status_letter(file.status);
                let is_deleted = file.status.is_deleted();
                ListItem::new(("review-file", index))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(*depth)
                    .indent_step_size(px(TREE_INDENT))
                    .toggle_state(selected)
                    .focused(selected && focused)
                    .start_slot(icon)
                    .child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .color(if is_deleted { Color::Disabled } else { color })
                            .when(is_deleted, Label::strikethrough)
                            .truncate(),
                    )
                    .end_slot(
                        h_flex()
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
                            .child(Label::new(letter).size(LabelSize::Small).color(color)),
                    )
                    .when(
                        file.attributes.generated || file.attributes.vendored,
                        |item| {
                            item.child(
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
        }
    }

    fn compute_indents(&self, range: Range<usize>) -> SmallVec<[usize; 64]> {
        range
            .map(|index| self.entries.get(index).map_or(0, TreeEntry::depth))
            .collect()
    }

    fn render_header(&self, review: &Entity<ReviewView>, cx: &Context<Self>) -> impl IntoElement {
        let review = review.read(cx);
        let title = review.title(cx);
        let summary = review.summary(cx);
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
            .child(
                Label::new(format!("{}/{} reviewed", summary.reviewed, summary.total))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(status, |header, status| {
                header.child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
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

impl EventEmitter<PanelEvent> for ReviewPanel {}

impl Focusable for ReviewPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let review = self.active_review();
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
                        .with_decoration(
                            ui::indent_guides(px(TREE_INDENT), IndentGuideColors::panel(cx))
                                .with_left_offset(ui::LIST_ITEM_INDENT_GUIDE_LEFT_OFFSET - px(2.))
                                .with_compute_indents_fn(cx.entity(), |this, range, _, _| {
                                    this.compute_indents(range)
                                }),
                        )
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
        add_review_panel, dispatch, excerpt_paths, init_vim_test, is_folded, is_reviewed,
        open_tree_fixture, path_at_cursor, rhs_editor,
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
        assert_eq!(status.as_deref(), Some("include ^SRC/ · 4 filtered out"));

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
        assert!(error.is_some());
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
        cx.simulate_keystrokes("g g j j space");
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
        cx.simulate_keystrokes("/");
        assert_eq!(prompt(&panel, cx), Some((".RS".into(), None)));
        cx.simulate_keystrokes("escape");
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
}
