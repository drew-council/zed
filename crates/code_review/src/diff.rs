use crate::target::ReviewTarget;
use anyhow::Result;
use buffer_diff::{BufferDiff, BufferDiffEvent};
use collections::{HashMap, HashSet};
use editor::{
    EditorEvent, EditorSettings, HiddenUnstagedDiffHunkRenderer, SelectionEffects,
    SplittableEditor, multibuffer_context_lines, scroll::Autoscroll,
};
use futures::{FutureExt as _, StreamExt as _, stream};
use futures_lite::future::yield_now;
use git::repository::RepoPath;
use gpui::{
    AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    WeakEntity,
};
use language::{Anchor, Buffer, Capability, OffsetRangeExt as _, language_settings::SoftWrap};
use multi_buffer::{MultiBuffer, PathKey};
use project::{
    Project, ProjectPath,
    git_store::{
        Repository,
        diff_buffer_list::{BranchDiffEvent, DiffBase, DiffBuffer, DiffBufferList},
    },
};
use settings::Settings as _;
use std::{collections::BTreeMap, sync::Arc};
use ui::{CommonAnimationExt as _, KeyBinding, prelude::*};
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{CloseActiveItem, Item as _, ItemNavHistory, Workspace};

/// Loading every changed file at once makes the first one land no sooner than the
/// last, leaving a large diff on a spinner. Throughput flattens past this point.
const MAX_CONCURRENT_BUFFER_LOADS: usize = 16;

fn diff_base(target: &ReviewTarget) -> DiffBase {
    match target {
        ReviewTarget::Uncommitted => DiffBase::Head,
    }
}

struct BufferSubscriptions {
    path_key: PathKey,
    display_buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    has_hunks: bool,
    _diff_subscription: Subscription,
}

/// The diff of a review: a read-only multibuffer with one excerpt group per changed file, over
/// the real project buffers, kept in sync with the changed-file list of a [`DiffBufferList`] and
/// with each buffer's [`BufferDiff`]. Adapted from `git_ui`'s `DiffMultibuffer`.
pub struct ReviewDiff {
    multibuffer: Entity<MultiBuffer>,
    diff_buffer_list: Entity<DiffBufferList>,
    editor: Entity<SplittableEditor>,
    buffer_subscriptions: HashMap<RepoPath, BufferSubscriptions>,
    /// Single-file mode: only this file's excerpts are in the multibuffer.
    single_file: Option<PathKey>,
    focus_handle: FocusHandle,
    empty_label: SharedString,
    refresh_task: Task<Result<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ReviewDiff {
    pub fn new(
        target: &ReviewTarget,
        repo: Entity<Repository>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_store = project.read(cx).git_store().clone();
        let diff_buffer_list =
            cx.new(|cx| DiffBufferList::new(diff_base(target), git_store, Some(repo), cx));
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project,
                workspace,
                window,
                cx,
            );
            // Staged and unstaged hunks are equally under review, so none is drawn as staged,
            // and a review offers no stage/restore controls.
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenUnstagedDiffHunkRenderer)), cx);
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_read_only(true);
                // Long lines scroll horizontally instead of wrapping, like tuicr. Without an
                // override the first file's language settings would decide for the whole
                // review. The left side copies this when it is created.
                editor.set_soft_wrap_mode(SoftWrap::None, cx);
            });
            editor
        });

        let subscriptions = vec![
            cx.subscribe_in(&editor, window, Self::handle_editor_event),
            cx.subscribe_in(
                &diff_buffer_list,
                window,
                |this, _, _: &BranchDiffEvent, window, cx| this.refresh_in(window, cx),
            ),
        ];

        let mut this = Self {
            multibuffer,
            diff_buffer_list,
            editor,
            buffer_subscriptions: HashMap::default(),
            single_file: None,
            focus_handle: cx.focus_handle(),
            empty_label: target.empty_label(),
            refresh_task: Task::ready(Ok(())),
            _subscriptions: subscriptions,
        };
        this.refresh_in(window, cx);
        this
    }

    pub fn editor(&self) -> &Entity<SplittableEditor> {
        &self.editor
    }

    pub fn multibuffer(&self) -> &Entity<MultiBuffer> {
        &self.multibuffer
    }

    pub fn repo(&self, cx: &App) -> Option<Entity<Repository>> {
        self.diff_buffer_list.read(cx).repo().cloned()
    }

    fn refresh_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_task = window.spawn(cx, {
            let this = cx.weak_entity();
            async move |cx| Self::refresh(this, cx).await
        });
    }

    fn handle_editor_event(
        &mut self,
        editor: &Entity<SplittableEditor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::SelectionsChanged { local: true } => {
                if editor.focus_handle(cx).contains_focused(window, cx) {
                    cx.emit(event.clone());
                }
            }
            EditorEvent::Saved => self.refresh_in(window, cx),
            _ => {}
        }
        if editor.focus_handle(cx).contains_focused(window, cx)
            && self.multibuffer.read(cx).is_empty()
        {
            self.focus_handle.focus(window, cx)
        }
    }

    fn register_buffer(
        &mut self,
        repo_path: RepoPath,
        path_key: PathKey,
        display_buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let diff_subscription = cx.subscribe_in(&diff, window, {
            let repo_path = repo_path.clone();
            move |this, _, event, window, cx| match event {
                BufferDiffEvent::DiffChanged(_) => {
                    // Unsaved edits made in another editor don't reshape the excerpts; the
                    // refresh that follows saving does.
                    let is_dirty = this
                        .buffer_subscriptions
                        .get(&repo_path)
                        .is_some_and(|entry| entry.display_buffer.read(cx).is_dirty());
                    if !is_dirty {
                        this.sync_excerpts(&repo_path, window, cx);
                    }
                }
                BufferDiffEvent::BaseTextChanged => {}
            }
        });
        self.buffer_subscriptions.insert(
            repo_path.clone(),
            BufferSubscriptions {
                path_key,
                display_buffer,
                diff,
                has_hunks: false,
                _diff_subscription: diff_subscription,
            },
        );
        self.sync_excerpts(&repo_path, window, cx);
    }

    /// Shows the file's hunks, or removes its excerpts when it has none or is hidden.
    fn sync_excerpts(&mut self, repo_path: &RepoPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.buffer_subscriptions.get_mut(repo_path) else {
            return;
        };
        let snapshot = entry.display_buffer.read(cx).snapshot();
        let excerpt_ranges = entry
            .diff
            .read(cx)
            .snapshot(cx)
            .hunks_intersecting_range(
                Anchor::min_max_range_for_buffer(snapshot.remote_id()),
                &snapshot,
            )
            .map(|hunk| hunk.buffer_range.to_point(&snapshot))
            .collect::<Vec<_>>();
        entry.has_hunks = !excerpt_ranges.is_empty();
        let path_key = entry.path_key.clone();
        let display_buffer = entry.display_buffer.clone();
        let diff = entry.diff.clone();

        if !entry.has_hunks && self.single_file.as_ref() == Some(&path_key) {
            self.set_single_file(None, window, cx);
            return;
        }

        let is_visible = self.is_visible(&path_key);
        let was_empty = self.multibuffer.read(cx).is_empty();
        self.editor.update(cx, |editor, cx| {
            if is_visible {
                editor.update_excerpts_for_path(
                    path_key,
                    display_buffer,
                    excerpt_ranges,
                    multibuffer_context_lines(cx),
                    diff,
                    cx,
                );
            } else {
                editor.remove_excerpts_for_path(path_key, cx);
            }
            if was_empty {
                editor.rhs_editor().update(cx, |editor, cx| {
                    editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                        s.select_ranges([multi_buffer::Anchor::Min..multi_buffer::Anchor::Min])
                    });
                });
            }
        });

        let is_empty = self.multibuffer.read(cx).is_empty();
        if is_empty && self.editor.focus_handle(cx).contains_focused(window, cx) {
            self.focus_handle.focus(window, cx);
        } else if !is_empty && self.focus_handle.is_focused(window) {
            self.editor.focus_handle(cx).focus(window, cx);
        }
    }

    fn is_visible(&self, path_key: &PathKey) -> bool {
        self.single_file
            .as_ref()
            .is_none_or(|single_file| single_file == path_key)
    }

    /// The changed files in review order, including those hidden by single-file mode.
    pub(crate) fn file_order(&self) -> Vec<PathKey> {
        let mut order = self
            .buffer_subscriptions
            .values()
            .filter(|entry| entry.has_hunks)
            .map(|entry| entry.path_key.clone())
            .collect::<Vec<_>>();
        order.sort();
        order
    }

    pub(crate) fn single_file(&self) -> Option<&PathKey> {
        self.single_file.as_ref()
    }

    /// The file under the cursor of the focused side.
    pub(crate) fn path_at_cursor(&self, cx: &App) -> Option<PathKey> {
        let editor = self.editor.read(cx).focused_editor().read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let (anchor, _) = snapshot.anchor_to_buffer_anchor(editor.selections.newest_anchor().head())?;
        snapshot.path_for_buffer(anchor.buffer_id).cloned()
    }

    /// Moves the cursor to the first row of the file (showing only that file in single-file
    /// mode).
    pub(crate) fn move_to_path(&mut self, path_key: PathKey, window: &mut Window, cx: &mut Context<Self>) {
        if self.single_file.is_some() {
            self.set_single_file(Some(path_key), window, cx);
        } else {
            self.select_path_start(&path_key, window, cx);
        }
    }

    fn select_path_start(&mut self, path_key: &PathKey, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |splittable, cx| {
            // Both sides have an excerpt for every path, under the same keys.
            let focused = splittable.focused_editor().clone();
            let editors = [focused, splittable.rhs_editor().clone()];
            for editor in editors {
                let moved = editor.update(cx, |editor, cx| {
                    let Some(position) = editor.buffer().read(cx).location_for_path(path_key, cx)
                    else {
                        return false;
                    };
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::focused()),
                        window,
                        cx,
                        |selections| selections.select_ranges([position..position]),
                    );
                    true
                });
                if moved {
                    break;
                }
            }
        });
    }

    /// Enters single-file mode on `path_key`, or leaves it with `None`.
    pub(crate) fn set_single_file(
        &mut self,
        path_key: Option<PathKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.single_file = path_key.clone();
        let repo_paths = self
            .buffer_subscriptions
            .iter()
            .map(|(repo_path, entry)| (repo_path.clone(), entry.path_key.clone()))
            .collect::<Vec<_>>();
        let mut moved = false;
        if let Some(path_key) = &path_key {
            // Show the file before hiding the others: an empty multibuffer would move focus to
            // the placeholder.
            if let Some((repo_path, _)) = repo_paths.iter().find(|(_, key)| key == path_key) {
                self.sync_excerpts(repo_path, window, cx);
            }
            if self.path_at_cursor(cx).as_ref() != Some(path_key) {
                self.select_path_start(path_key, window, cx);
                moved = true;
            }
        }
        for (repo_path, key) in &repo_paths {
            if path_key.as_ref() != Some(key) {
                self.sync_excerpts(repo_path, window, cx);
            }
        }
        if !moved {
            // The scroll position may have been in a file that was just hidden.
            self.editor.update(cx, |editor, cx| {
                editor.focused_editor().update(cx, |editor, cx| {
                    editor.request_autoscroll(Autoscroll::fit(), cx)
                })
            });
        }
        cx.notify();
    }

    async fn refresh(this: WeakEntity<Self>, cx: &mut AsyncWindowContext) -> Result<()> {
        let entries = this.update(cx, |this, cx| {
            let buffers_to_load = this
                .diff_buffer_list
                .update(cx, |diff_buffer_list, cx| diff_buffer_list.load_buffers(cx));
            let mut stale_paths = this
                .multibuffer
                .read(cx)
                .snapshot(cx)
                .buffers_with_paths()
                .map(|(buffer_snapshot, path_key)| (path_key.clone(), buffer_snapshot.remote_id()))
                .collect::<HashMap<_, _>>();

            let mut entries = BTreeMap::new();
            let mut live_repo_paths = HashSet::default();
            for diff_buffer in buffers_to_load {
                live_repo_paths.insert(diff_buffer.repo_path.clone());
                let path_key = review_path_key(&diff_buffer.repo_path);
                stale_paths.remove(&path_key);
                entries.insert(path_key, diff_buffer);
            }

            let repo_path_by_display_id = this
                .buffer_subscriptions
                .iter()
                .map(|(repo_path, subscriptions)| {
                    (
                        subscriptions.display_buffer.read(cx).remote_id(),
                        repo_path.clone(),
                    )
                })
                .collect::<HashMap<_, _>>();
            this.editor.update(cx, |editor, cx| {
                for (path_key, buffer_id) in stale_paths {
                    if let Some(repo_path) = repo_path_by_display_id.get(&buffer_id) {
                        this.buffer_subscriptions.remove(repo_path);
                    }
                    editor.remove_excerpts_for_path(path_key, cx);
                }
            });
            this.buffer_subscriptions
                .retain(|repo_path, _| live_repo_paths.contains(repo_path));
            // The loads below show every file again.
            if this
                .single_file
                .as_ref()
                .is_some_and(|single_file| !entries.contains_key(single_file))
            {
                this.single_file = None;
            }

            entries
        })?;

        // Ordered, so excerpts don't shift under the reader as later files arrive.
        let mut loads = stream::iter(entries.into_iter().map(|(path_key, entry)| {
            let DiffBuffer {
                repo_path, load, ..
            } = entry;
            load.map(move |loaded| (path_key, repo_path, loaded))
        }))
        .buffered(MAX_CONCURRENT_BUFFER_LOADS);

        while let Some((path_key, repo_path, loaded)) = loads.next().await {
            // Buffered loads can already be complete when we poll them, in which
            // case this loop never awaits and starves the foreground thread.
            yield_now().await;
            if let Some(loaded) = loaded.log_err() {
                cx.update(|window, cx| {
                    this.update(cx, |this, cx| {
                        this.register_buffer(
                            repo_path,
                            path_key,
                            loaded.display_buffer,
                            loaded.diff,
                            window,
                            cx,
                        );
                    })
                    .ok();
                })?;
            }
        }
        this.update(cx, |_, cx| cx.notify())?;

        Ok(())
    }

    pub fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        let editor = self.editor.read(cx).focused_editor().read(cx);
        let focused_multibuffer = editor.buffer().read(cx);
        let position = editor.selections.newest_anchor().head();
        let focused_snapshot = focused_multibuffer.snapshot(cx);
        let (focused_anchor, _) = focused_snapshot.anchor_to_buffer_anchor(position)?;
        let path = focused_snapshot.path_for_buffer(focused_anchor.buffer_id)?;

        let multibuffer = self.multibuffer.read(cx);
        let position = multibuffer.location_for_path(path, cx)?;
        let snapshot = multibuffer.snapshot(cx);
        let (text_anchor, _) = snapshot.anchor_to_buffer_anchor(position)?;
        let buffer = multibuffer.buffer(text_anchor.buffer_id)?;
        let file = buffer.read(cx).file()?;
        Some(ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        })
    }

    pub fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    pub fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor
                .rhs_editor()
                .update(cx, |editor, cx| editor.deactivated(window, cx))
        });
    }

    pub fn navigate(
        &mut self,
        data: Arc<dyn std::any::Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor
                .rhs_editor()
                .update(cx, |editor, cx| editor.navigate(data, window, cx))
        })
    }

    pub fn set_nav_history(&mut self, nav_history: ItemNavHistory, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            })
        });
    }

    pub fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .for_each_project_item(cx, f)
    }

    /// The worktree-relative path of each excerpted buffer, in multibuffer order.
    #[cfg(test)]
    pub fn excerpt_file_paths(&self, cx: &App) -> Vec<String> {
        let multibuffer = self.multibuffer.read(cx);
        let snapshot = multibuffer.snapshot(cx);
        let mut paths = Vec::new();
        let mut last_buffer_id = None;
        for excerpt in snapshot.excerpts() {
            let buffer_id = excerpt.context.start.buffer_id;
            if last_buffer_id == Some(buffer_id) {
                continue;
            }
            last_buffer_id = Some(buffer_id);
            if let Some(file) = multibuffer
                .buffer(buffer_id)
                .and_then(|buffer| buffer.read(cx).file().cloned())
            {
                paths.push(file.path().as_unix_str().to_string());
            }
        }
        paths
    }
}

impl EventEmitter<EditorEvent> for ReviewDiff {}

impl Focusable for ReviewDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.multibuffer.read(cx).is_empty() {
            self.focus_handle.clone()
        } else {
            self.editor.focus_handle(cx)
        }
    }
}

impl Render for ReviewDiff {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.multibuffer.read(cx).is_empty();
        let is_loading = !self.refresh_task.is_ready();

        div()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .when(is_empty && is_loading, |el| {
                let rems = TextSize::Large.rems(cx);
                el.child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::Custom(rems))
                        .color(Color::Accent)
                        .with_rotate_animation(3),
                )
            })
            .when(is_empty && !is_loading, |el| {
                let focus_handle = self.focus_handle.clone();
                el.child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_around()
                                .child(Label::new(self.empty_label.clone())),
                        )
                        .child(
                            h_flex().justify_around().mt_1().child(
                                Button::new("review-close-button", "Close")
                                    .key_binding(KeyBinding::for_action_in(
                                        &CloseActiveItem::default(),
                                        &focus_handle,
                                        cx,
                                    ))
                                    .on_click(move |_, window, cx| {
                                        window.focus(&focus_handle, cx);
                                        window.dispatch_action(
                                            Box::new(CloseActiveItem::default()),
                                            cx,
                                        );
                                    }),
                            ),
                        ),
                )
            })
            .when(!is_empty, |el| el.child(self.editor.clone()))
    }
}

/// Orders files like Zed's git panel tree view (directories before files at each level), which
/// the review panel's file tree will mirror. The path only orders and identifies excerpts;
/// headers show the buffer's own path.
fn review_path_key(repo_path: &RepoPath) -> PathKey {
    PathKey::with_sort_prefix(0, tree_sort_path(repo_path))
}

/// Builds a synthetic path whose natural component-wise ordering reproduces a
/// folder-first tree order. Each directory component is prefixed with a NUL
/// byte, which can never appear in a real path component and sorts before every
/// printable character, so at each level directories sort before files.
fn tree_sort_path(repo_path: &RelPath) -> Arc<RelPath> {
    let components: Vec<&str> = repo_path.components().collect();
    if components.len() <= 1 {
        return repo_path.into_arc();
    }
    let last = components.len() - 1;
    let mut synthetic = String::new();
    for (index, component) in components.into_iter().enumerate() {
        if index > 0 {
            synthetic.push('/');
        }
        if index < last {
            synthetic.push('\0');
        }
        synthetic.push_str(component);
    }
    RelPath::from_unix_str(&synthetic)
        .map(|path| path.into_arc())
        .unwrap_or_else(|_| repo_path.into_arc())
}
