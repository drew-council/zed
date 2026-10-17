use crate::{
    CollapseSelectedEntry, ExpandSelectedEntry, FocusPanel, ReviewUncommitted, ReviewView,
    ToggleFocus, TogglePanel, ToggleSelectedDirectory, diff::ReviewFile, view::focus_diff,
};
use anyhow::Result;
use collections::HashSet;
use editor::{EditorEvent, PathKey};
use file_icons::FileIcons;
use git::{repository::RepoPath, status::FileStatus};
use gpui::{
    Action, AsyncWindowContext, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    KeyContext, ScrollStrategy, Subscription, UniformListScrollHandle, WeakEntity, uniform_list,
};
use smallvec::SmallVec;
use std::{collections::BTreeMap, ops::Range};
use ui::{
    CommonAnimationExt as _, IndentGuideColors, ListItem, ListItemSpacing, Tooltip, prelude::*,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const REVIEW_PANEL_KEY: &str = "ReviewPanel";
const TREE_INDENT: f32 = 16.0;

pub(crate) fn register(workspace: &mut Workspace) {
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
        file: ReviewFile,
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
    files: Vec<ReviewFile>,
}

/// The review panel in the left dock: the file tree of the active review tab.
pub struct ReviewPanel {
    focus_handle: FocusHandle,
    active_review: Option<WeakEntity<ReviewView>>,
    /// The visible rows: collapsed directories hide their descendants.
    entries: Vec<TreeEntry>,
    /// Directories are expanded unless collapsed here, per review tab.
    collapsed: HashSet<(EntityId, RepoPath)>,
    selected: Option<usize>,
    scroll_handle: UniformListScrollHandle,
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
            focus_handle: cx.focus_handle(),
            active_review: None,
            entries: Vec::new(),
            collapsed: HashSet::default(),
            selected: None,
            scroll_handle: UniformListScrollHandle::new(),
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
                    cx.subscribe_in(review, window, |this, _, event: &EditorEvent, window, cx| {
                        if let EditorEvent::SelectionsChanged { .. } = event
                            && !this.focus_handle.contains_focused(window, cx)
                        {
                            this.select_cursor_file(cx);
                        }
                    }),
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

    fn files(&self, cx: &App) -> Vec<ReviewFile> {
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
                let path = RepoPath::new(&directory).ok();
                node = node.children.entry(name.clone()).or_insert_with(|| TreeNode {
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

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
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
            self.reveal_in_diff(&review, path_key, cx);
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
            let repo_path = self.files(cx).into_iter().find(|file| file.path_key == path_key)?.repo_path;
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

    fn reveal_in_diff(&self, review: &Entity<ReviewView>, path_key: PathKey, cx: &mut App) {
        let Some(window_handle) = cx.active_window() else {
            return;
        };
        window_handle
            .update(cx, |_, window, cx| {
                review.update(cx, |review, cx| review.reveal_file(path_key, window, cx));
            })
            .ok();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let index = self.selected.map_or(0, |index| index + 1);
        self.select(index, cx);
    }

    fn select_previous(&mut self, _: &menu::SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let index = self
            .selected
            .map_or(self.entries.len().saturating_sub(1), |index| index.saturating_sub(1));
        self.select(index, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.entries.len().saturating_sub(1), cx);
    }

    /// Enter: a file opens in the diff, which takes focus; a directory expands or collapses.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected_entry().cloned() {
            Some(TreeEntry::File { file, .. }) => self.open_file(file.path_key, true, window, cx),
            Some(TreeEntry::Directory { path, .. }) => self.toggle_directory(path, cx),
            None => {}
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(crate::FocusDiff.boxed_clone(), cx);
    }

    /// Moves the diff to the file; `focus` also moves focus to the diff.
    fn open_file(&mut self, path_key: PathKey, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
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
                    self.select(index + 1, cx);
                }
            }
            Some(TreeEntry::File { file, .. }) => {
                self.open_file(file.path_key, false, window, cx)
            }
            None => {}
        }
    }

    /// `h`: collapses an expanded directory, otherwise selects the parent directory.
    fn collapse_selected_entry(
        &mut self,
        _: &CollapseSelectedEntry,
        _: &mut Window,
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
        let parent = self.entries[..index]
            .iter()
            .rposition(|entry| matches!(entry, TreeEntry::Directory { .. }) && entry.depth() < depth);
        if let Some(parent) = parent {
            self.select(parent, cx);
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
                        if let Some(TreeEntry::Directory { path, .. }) = this.entries.get(index).cloned() {
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
                            .child(Label::new(letter).size(LabelSize::Small).color(color)),
                    )
                    .tooltip({
                        let path = SharedString::from(file.repo_path.as_unix_str().to_string());
                        move |_, cx| Tooltip::simple(path.clone(), cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        window.focus(&this.focus_handle, cx);
                        this.selected = Some(index);
                        if let Some(TreeEntry::File { file, .. }) = this.entries.get(index).cloned() {
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
        let total = self
            .entries
            .iter()
            .filter(|entry| matches!(entry, TreeEntry::File { .. }))
            .count();
        v_flex()
            .px_2()
            .py_1()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new(title).truncate())
            .child(
                Label::new(format!(
                    "{total} changed file{}",
                    if total == 1 { "" } else { "s" }
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
    }

    fn render_empty_state(&self, review: Option<&Entity<ReviewView>>, cx: &Context<Self>) -> AnyElement {
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
    /// status letter after files. The selected row starts with `>`.
    pub(crate) fn rows_for_test(&self) -> Vec<String> {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let marker = if self.selected == Some(index) { ">" } else { "" };
                let indent = "  ".repeat(entry.depth());
                match entry {
                    TreeEntry::Directory { name, expanded, .. } => {
                        let collapsed = if *expanded { "" } else { " ▸" };
                        format!("{marker}{indent}{name}/{collapsed}")
                    }
                    TreeEntry::File { file, name, .. } => {
                        let (letter, _) = status_letter(file.status);
                        format!("{marker}{indent}{name} {letter}")
                    }
                }
            })
            .collect()
    }
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

impl EventEmitter<PanelEvent> for ReviewPanel {}

impl Focusable for ReviewPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let review = self.active_review();
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("ReviewPanel");
        key_context.add("not_editing");

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
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .when_some(review.as_ref(), |panel, review| {
                panel.child(self.render_header(review, cx))
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
        add_review_panel, init_vim_test, open_tree_fixture, path_at_cursor, rhs_editor,
    };
    use gpui::{TestAppContext, VisualTestContext};
    use settings::DiffViewStyle;

    fn rows(panel: &Entity<ReviewPanel>, cx: &mut VisualTestContext) -> Vec<String> {
        panel.read_with(cx, |panel, _| panel.rows_for_test())
    }

    #[gpui::test]
    async fn test_panel_tree(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_tree_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        let editor = rhs_editor(&review, cx);
        cx.run_until_parked();

        // Directories first, single-child chains compacted, in the diff's order.
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
                "  x.js A",
                "Cargo.lock M",
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
}
