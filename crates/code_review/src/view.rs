use crate::{
    FocusDiff, FocusPanel, NextFile, NextUnreviewedFile, OpenFileAtCursor, PreviousFile,
    PreviousUnreviewedFile, ReviewBranch, ReviewCommits, ReviewPanel, ReviewUncommitted,
    ScrollLeft, ScrollRight, ChangeBase, ToggleHeader, ToggleSingleFileMode,
    commits::{TOP_BLOCK_COMMIT_LIMIT, first_parent_chain, resolve_commit_spec, validate_ref},
    diff::{ReviewDiff, ReviewDiffEvent, ReviewFile},
    line_cursor::{HORIZONTAL_SCROLL_COLUMNS, HorizontalScroll, LineCursor},
    picker::{StartReviewFocus, StartReviewModal},
    target::{ReviewTarget, parse_commit_spec},
    top_block::{CommitsState, ReviewTopBlock, TopBlockContent},
};
use editor::{
    Editor, EditorEvent, PathKey, SelectionEffects, SplittableEditor,
    actions::{OpenExcerpts, ToggleSoftWrap},
    scroll::Autoscroll,
};
use git_ui::branch_picker::{self, SelectBranchCallback};
use gpui::{
    Action as _, AnyEntity, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    WeakEntity,
};
use language::{Capability, Point, language_settings::SoftWrap};
use project::{
    Project, ProjectPath,
    git_store::{Repository, RepositoryEvent},
};
use std::{
    any::{Any, TypeId},
    sync::Arc,
};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    ItemNavHistory, Toast, Workspace,
    item::{Item, ItemEvent},
    notifications::{NotificationId, NotifyTaskExt as _},
    searchable::SearchableItemHandle,
};

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ReviewUncommitted, window, cx| {
        ReviewView::deploy(workspace, ReviewTarget::Uncommitted, window, cx);
    });
    workspace.register_action(|workspace, action: &ReviewBranch, window, cx| {
        review_branch(workspace, action.base.clone(), window, cx);
    });
    workspace.register_action(|workspace, action: &ReviewCommits, window, cx| {
        match &action.commits {
            Some(commits) => review_commits(workspace, commits, window, cx),
            None => StartReviewModal::toggle(workspace, StartReviewFocus::Commits, window, cx),
        }
    });
    workspace.register_action(|workspace, _: &FocusDiff, window, cx| {
        focus_diff(workspace, window, cx);
    });
}

/// The repository reviews act on, or a toast saying there is none.
pub(crate) fn active_repository(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> Option<Entity<Repository>> {
    let repo = workspace.project().read(cx).active_repository(cx);
    if repo.is_none() {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<ReviewView>(),
                "Code review needs a git repository",
            )
            .autohide(),
            cx,
        );
    }
    repo
}

fn review_branch(
    workspace: &mut Workspace,
    base: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repo) = active_repository(workspace, cx) else {
        return;
    };
    let workspace_handle = cx.weak_entity();
    let task = match base {
        Some(base) => {
            let base = SharedString::from(base);
            let validation = validate_ref(&repo, base.clone(), cx);
            window.spawn(cx, {
                let workspace_handle = workspace_handle.clone();
                async move |cx| {
                    validation.await?;
                    workspace_handle.update_in(cx, |workspace, window, cx| {
                        ReviewView::deploy_in_repo(
                            workspace,
                            ReviewTarget::Branch { base },
                            repo,
                            window,
                            cx,
                        );
                    })
                }
            })
        }
        None => {
            let default_branch = repo.update(cx, |repo, _| repo.default_branch(true));
            window.spawn(cx, {
                let workspace_handle = workspace_handle.clone();
                async move |cx| {
                    let base = default_branch.await??;
                    workspace_handle.update_in(cx, |workspace, window, cx| match base {
                        Some(base) => {
                            ReviewView::deploy_in_repo(
                                workspace,
                                ReviewTarget::Branch { base },
                                repo,
                                window,
                                cx,
                            );
                        }
                        None => pick_base_for_new_review(workspace, repo, None, window, cx),
                    })
                }
            })
        }
    };
    task.detach_and_notify_err(workspace_handle, window, cx);
}

fn review_commits(
    workspace: &mut Workspace,
    commits: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repo) = active_repository(workspace, cx) else {
        return;
    };
    let workspace_handle = cx.weak_entity();
    let resolve = parse_commit_spec(commits).map(|spec| resolve_commit_spec(&repo, spec, cx));
    let task = window.spawn(cx, {
        let workspace_handle = workspace_handle.clone();
        async move |cx| {
            let range = resolve?.await?;
            workspace_handle.update_in(cx, |workspace, window, cx| {
                ReviewView::deploy_in_repo(
                    workspace,
                    ReviewTarget::Commits(range),
                    repo,
                    window,
                    cx,
                );
            })
        }
    });
    task.detach_and_notify_err(workspace_handle, window, cx);
}

/// Opens Zed's branch picker; the chosen branch becomes the base of a new branch review.
pub(crate) fn pick_base_for_new_review(
    workspace: &mut Workspace,
    repo: Entity<Repository>,
    selected_base: Option<SharedString>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = workspace.weak_handle();
    let on_select: SelectBranchCallback = Arc::new({
        let workspace_handle = workspace_handle.clone();
        let repo = repo.clone();
        move |branch, window, cx| {
            let base = SharedString::from(branch.name().to_owned());
            workspace_handle
                .update(cx, |workspace, cx| {
                    ReviewView::deploy_in_repo(
                        workspace,
                        ReviewTarget::Branch { base },
                        repo.clone(),
                        window,
                        cx,
                    );
                })
                .log_err();
        }
    });
    workspace.toggle_modal(window, cx, |window, cx| {
        branch_picker::select_modal(
            workspace_handle,
            Some(repo),
            selected_base,
            on_select,
            window,
            cx,
        )
    });
}

/// Focuses the active review tab, else the review the panel shows, else any open review.
pub(crate) fn focus_diff(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let review = workspace
        .active_item_as::<ReviewView>(cx)
        .or_else(|| {
            workspace
                .panel::<ReviewPanel>(cx)
                .and_then(|panel| panel.read(cx).active_review())
        })
        .or_else(|| workspace.items_of_type::<ReviewView>(cx).next());
    if let Some(review) = review {
        workspace.activate_item(&review, true, true, window, cx);
    }
}

fn find_review(
    workspace: &Workspace,
    target: &ReviewTarget,
    repo: &Entity<Repository>,
    cx: &App,
) -> Option<Entity<ReviewView>> {
    workspace
        .items_of_type::<ReviewView>(cx)
        .find(|view| view.read(cx).reviews(target, repo))
}

fn branch_name(repo: &Entity<Repository>, cx: &App) -> SharedString {
    repo.read(cx)
        .branch
        .as_ref()
        .map(|branch| SharedString::from(branch.name().to_owned()))
        .unwrap_or_else(|| "HEAD".into())
}

/// Vim keeps a pending count after actions that aren't its own, where it would multiply the next
/// motion. The review's own actions don't take counts.
fn discard_vim_count(window: &mut Window, cx: &mut App) {
    if let Some(action) = cx.build_action("vim::ClearOperators", None).log_err() {
        window.dispatch_action(action, cx);
    }
}

/// The review tab: a read-only diff of a [`ReviewTarget`] in one repository.
pub struct ReviewView {
    target: ReviewTarget,
    repo: Entity<Repository>,
    diff: Entity<ReviewDiff>,
    top_block: Entity<ReviewTopBlock>,
    line_cursor: Entity<LineCursor>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    _load_commits_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ReviewView {
    /// Activates the open review of `target` in the active repository, or opens a new one.
    pub fn deploy(
        workspace: &mut Workspace,
        target: ReviewTarget,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Self>> {
        let repo = active_repository(workspace, cx)?;
        Some(Self::deploy_in_repo(workspace, target, repo, window, cx))
    }

    /// Activates the open review of `target` in `repo`, or opens a new one. Reviews of other
    /// targets stay open in their own tabs.
    pub fn deploy_in_repo(
        workspace: &mut Workspace,
        target: ReviewTarget,
        repo: Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        if let Some(existing) = find_review(workspace, &target, &repo, cx) {
            workspace.activate_item(&existing, true, true, window, cx);
            return existing;
        }

        let project = workspace.project().clone();
        let workspace_handle = cx.entity();
        let view = cx.new(|cx| Self::new(target, repo, project, workspace_handle, window, cx));
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    }

    fn new(
        target: ReviewTarget,
        repo: Entity<Repository>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let diff = cx.new(|cx| {
            ReviewDiff::new(
                &target,
                repo.clone(),
                project.clone(),
                workspace.clone(),
                window,
                cx,
            )
        });
        let content = match &target {
            ReviewTarget::Uncommitted => TopBlockContent::None,
            ReviewTarget::Branch { base } => TopBlockContent::Branch {
                branch: branch_name(&repo, cx),
                base: base.clone(),
            },
            ReviewTarget::Commits(range) => TopBlockContent::Commits {
                range: *range,
                commits: CommitsState::Loading,
            },
        };
        let view = cx.weak_entity();
        let top_block = cx.new(|cx| ReviewTopBlock::new(content, view, diff.clone(), cx));
        let load_commits_task = match &target {
            ReviewTarget::Commits(range) => {
                let chain = first_parent_chain(
                    &repo,
                    Some(range.head),
                    range.base,
                    TOP_BLOCK_COMMIT_LIMIT,
                    cx,
                );
                let language_registry = project.read(cx).languages().clone();
                cx.spawn(async move |this, cx| {
                    let chain = chain.await;
                    if let Err(error) = &chain {
                        log::error!("loading the commits of a review: {error:#}");
                    }
                    this.update(cx, |this, cx| {
                        this.top_block.update(cx, |top_block, cx| {
                            top_block.set_commits(chain, language_registry, cx)
                        });
                        cx.emit(EditorEvent::TitleChanged);
                        cx.notify();
                    })
                    .ok();
                })
            }
            ReviewTarget::Uncommitted | ReviewTarget::Branch { .. } => Task::ready(()),
        };

        let subscriptions = vec![
            cx.subscribe(&diff, |_, _, event: &EditorEvent, cx| cx.emit(event.clone())),
            cx.subscribe(&diff, |_, _, _: &ReviewDiffEvent, cx| cx.notify()),
            // The review panel shows the diff's loading state.
            cx.observe(&diff, |_, _, cx| cx.notify()),
            // A branch review is titled after the checked-out branch.
            cx.subscribe(&repo, |this, repo, event: &RepositoryEvent, cx| {
                if let (RepositoryEvent::HeadChanged, ReviewTarget::Branch { base }) =
                    (event, &this.target)
                {
                    let branch = branch_name(&repo, cx);
                    let base = base.clone();
                    this.top_block
                        .update(cx, |top_block, cx| top_block.set_branch(branch, base, cx));
                    cx.emit(EditorEvent::TitleChanged);
                    cx.notify();
                }
            }),
        ];
        let line_cursor = cx.new(|cx| LineCursor::new(diff.read(cx).editor().clone(), cx));
        Self {
            target,
            repo,
            diff,
            top_block,
            line_cursor,
            project,
            workspace: workspace.downgrade(),
            _load_commits_task: load_commits_task,
            _subscriptions: subscriptions,
        }
    }

    pub fn target(&self) -> &ReviewTarget {
        &self.target
    }

    pub fn diff(&self) -> &Entity<ReviewDiff> {
        &self.diff
    }

    #[cfg(test)]
    pub(crate) fn top_block(&self) -> &Entity<ReviewTopBlock> {
        &self.top_block
    }

    pub fn title(&self, cx: &App) -> SharedString {
        match &self.target {
            ReviewTarget::Uncommitted => "Uncommitted changes".into(),
            ReviewTarget::Branch { base } => {
                format!("{} vs {base}", branch_name(&self.repo, cx)).into()
            }
            ReviewTarget::Commits(range) => self
                .top_block
                .read(cx)
                .commits_title()
                .unwrap_or_else(|| range.head.display_short().into()),
        }
    }

    /// The files the review panel lists, in review order.
    pub(crate) fn panel_files(&self, cx: &App) -> Vec<ReviewFile> {
        self.diff.read(cx).files(cx)
    }

    /// Moves the diff to the first row of the file (showing only that file in single-file mode).
    pub(crate) fn reveal_file(&mut self, path_key: PathKey, window: &mut Window, cx: &mut Context<Self>) {
        self.diff
            .update(cx, |diff, cx| diff.move_to_path(path_key, window, cx));
    }

    /// Opens Zed's branch picker to change the base of a branch review. A branch that another
    /// tab already reviews against activates that tab instead.
    pub(crate) fn open_base_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ReviewTarget::Branch { base } = &self.target else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let workspace_handle = self.workspace.clone();
        let view = cx.weak_entity();
        let repo = self.repo.clone();
        let on_select: SelectBranchCallback = Arc::new({
            let workspace_handle = workspace_handle.clone();
            move |branch, window, cx| {
                let base = SharedString::from(branch.name().to_owned());
                let Some(view) = view.upgrade() else {
                    return;
                };
                workspace_handle
                    .update(cx, |workspace, cx| {
                        Self::select_base(view, base, workspace, window, cx)
                    })
                    .log_err();
            }
        });
        let selected_base = Some(base.clone());
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                branch_picker::select_modal(
                    workspace_handle,
                    Some(repo),
                    selected_base,
                    on_select,
                    window,
                    cx,
                )
            })
        });
    }

    /// The base picker's choice for `view`: another tab that already reviews the branch against
    /// `base` is activated, otherwise `view` changes its base.
    pub(crate) fn select_base(
        view: Entity<Self>,
        base: SharedString,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let repo = view.read(cx).repo.clone();
        let target = ReviewTarget::Branch { base: base.clone() };
        match find_review(workspace, &target, &repo, cx) {
            Some(existing) if existing != view => {
                workspace.activate_item(&existing, true, true, window, cx);
            }
            _ => view.update(cx, |view, cx| view.change_base(base, cx)),
        }
    }

    /// Reviews the branch against `base` instead, in this tab.
    pub(crate) fn change_base(&mut self, base: SharedString, cx: &mut Context<Self>) {
        let target = ReviewTarget::Branch { base: base.clone() };
        if self.target == target {
            return;
        }
        self.target = target;
        self.diff
            .update(cx, |diff, cx| diff.set_branch_base(base.clone(), cx));
        let branch = branch_name(&self.repo, cx);
        self.top_block
            .update(cx, |top_block, cx| top_block.set_branch(branch, base, cx));
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    fn change_base_action(&mut self, _: &ChangeBase, window: &mut Window, cx: &mut Context<Self>) {
        self.open_base_picker(window, cx);
    }

    fn toggle_header(&mut self, _: &ToggleHeader, _: &mut Window, cx: &mut Context<Self>) {
        self.top_block
            .update(cx, |top_block, cx| top_block.toggle_expanded(cx));
    }

    fn scroll_left(&mut self, _: &ScrollLeft, window: &mut Window, cx: &mut Context<Self>) {
        let scroll = self.line_cursor.update(cx, |line_cursor, cx| {
            line_cursor.scroll_horizontally(-HORIZONTAL_SCROLL_COLUMNS, window, cx)
        });
        // In visual mode `h` only scrolls, so the selection isn't lost to a focus change.
        if matches!(scroll, HorizontalScroll::AtLeftEdge) && !self.has_visual_selection(cx) {
            window.dispatch_action(FocusPanel.boxed_clone(), cx);
        }
        discard_vim_count(window, cx);
    }

    fn scroll_right(&mut self, _: &ScrollRight, window: &mut Window, cx: &mut Context<Self>) {
        self.line_cursor.update(cx, |line_cursor, cx| {
            line_cursor.scroll_horizontally(HORIZONTAL_SCROLL_COLUMNS, window, cx)
        });
        discard_vim_count(window, cx);
    }

    fn next_file(&mut self, _: &NextFile, window: &mut Window, cx: &mut Context<Self>) {
        // No wrapping, like tuicr: the last file is a stop.
        self.move_to_file(window, cx, |order, current| match current {
            Some(current) => order.iter().find(|key| *key > current).cloned(),
            None => order.first().cloned(),
        });
    }

    fn previous_file(&mut self, _: &PreviousFile, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to_file(window, cx, |order, current| {
            let current = current?;
            order.iter().rev().find(|key| *key < current).cloned()
        });
    }

    fn next_unreviewed_file(
        &mut self,
        _: &NextUnreviewedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to_unreviewed_file(window, cx, |order, current| {
            let after = current.and_then(|current| order.iter().find(|key| *key > current));
            after
                .or_else(|| order.iter().find(|key| Some(*key) != current))
                .cloned()
        });
    }

    fn previous_unreviewed_file(
        &mut self,
        _: &PreviousUnreviewedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to_unreviewed_file(window, cx, |order, current| {
            let before = current.and_then(|current| order.iter().rev().find(|key| *key < current));
            before
                .or_else(|| order.iter().rev().find(|key| Some(*key) != current))
                .cloned()
        });
    }

    /// Unlike plain file navigation, this wraps around: it finds the remaining work from anywhere.
    fn move_to_unreviewed_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: impl FnOnce(&[PathKey], Option<&PathKey>) -> Option<PathKey>,
    ) {
        self.move_to_file(window, cx, |order, current| {
            let unreviewed = order
                .iter()
                .filter(|key| !Self::is_file_reviewed(key))
                .cloned()
                .collect::<Vec<_>>();
            target(&unreviewed, current)
        });
    }

    // Reviewed marks arrive with the review panel's file tree (milestone 4).
    fn is_file_reviewed(_path_key: &PathKey) -> bool {
        false
    }

    fn move_to_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: impl FnOnce(&[PathKey], Option<&PathKey>) -> Option<PathKey>,
    ) {
        self.diff.update(cx, |diff, cx| {
            let order = diff.file_order();
            let current = diff.path_at_cursor(cx);
            if let Some(target) = target(&order, current.as_ref()) {
                diff.move_to_path(target, window, cx);
            }
        });
        discard_vim_count(window, cx);
    }

    fn toggle_single_file_mode(
        &mut self,
        _: &ToggleSingleFileMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            let single_file = if diff.single_file().is_some() {
                None
            } else {
                diff.path_at_cursor(cx)
                    .or_else(|| diff.file_order().first().cloned())
            };
            diff.set_single_file(single_file, window, cx);
        });
        discard_vim_count(window, cx);
    }

    fn open_file_at_cursor(
        &mut self,
        _: &OpenFileAtCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.diff.read(cx).is_snapshot(cx) {
            self.open_working_tree_file_at_cursor(window, cx);
        } else {
            let editor = self.diff.read(cx).editor().read(cx).focused_editor().clone();
            editor.update(cx, |editor, cx| editor.open_excerpts(&OpenExcerpts, window, cx));
        }
        discard_vim_count(window, cx);
    }

    /// A snapshot's buffers are historic, so the file opens as it is in the working tree, at the
    /// same row as far as it still exists.
    fn open_working_tree_file_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path_key, row)) = self
            .diff
            .update(cx, |diff, cx| diff.path_and_row_at_cursor(cx))
        else {
            return;
        };
        let Some(project_path) = self
            .diff
            .read(cx)
            .repo_path_for(&path_key)
            .and_then(|repo_path| self.repo.read(cx).repo_path_to_project_path(&repo_path, cx))
        else {
            return;
        };
        let workspace = self.workspace.clone();
        // Opening activates another item, which calls back into this one.
        window
            .spawn(cx, async move |cx| {
                let item = workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.open_path(project_path, None, true, window, cx)
                    })?
                    .await?;
                if let Some(editor) = item.downcast::<Editor>() {
                    editor.update_in(cx, |editor, window, cx| {
                        let max_row = editor.buffer().read(cx).snapshot(cx).max_point().row;
                        let point = Point::new(row.min(max_row), 0);
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |selections| selections.select_ranges([point..point]),
                        );
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }

    /// Both sides switch between wrapping and not wrapping together. Zed's own toggle would fall
    /// back to the language settings first, which may not change anything.
    fn toggle_soft_wrap(&mut self, _: &ToggleSoftWrap, _: &mut Window, cx: &mut Context<Self>) {
        let editor = self.diff.read(cx).editor().clone();
        let wraps = !matches!(
            editor.read(cx).rhs_editor().read(cx).soft_wrap_mode(cx),
            editor::SoftWrap::None
        );
        let mode = if wraps {
            SoftWrap::None
        } else {
            SoftWrap::EditorWidth
        };
        editor.update(cx, |editor, cx| {
            editor.update_editors(cx, |editor, cx| editor.set_soft_wrap_mode(mode, cx))
        });
        cx.stop_propagation();
    }

    fn has_visual_selection(&self, cx: &mut App) -> bool {
        let editor = self.diff.read(cx).editor().read(cx).focused_editor().clone();
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            !editor
                .selections
                .newest::<Point>(&snapshot)
                .is_empty()
        })
    }

    fn reviews(&self, target: &ReviewTarget, repo: &Entity<Repository>) -> bool {
        self.target.is_same_review(target) && self.repo.entity_id() == repo.entity_id()
    }
}

impl EventEmitter<EditorEvent> for ReviewView {}

impl Focusable for ReviewView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff.read(cx).focus_handle(cx)
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_branch_review = matches!(self.target, ReviewTarget::Branch { .. });
        let has_top_block = self.top_block.read(cx).has_content();
        v_flex()
            .key_context("CodeReview")
            .capture_action(cx.listener(Self::toggle_soft_wrap))
            .on_action(cx.listener(Self::scroll_left))
            .on_action(cx.listener(Self::scroll_right))
            .on_action(cx.listener(Self::next_file))
            .on_action(cx.listener(Self::previous_file))
            .on_action(cx.listener(Self::next_unreviewed_file))
            .on_action(cx.listener(Self::previous_unreviewed_file))
            .on_action(cx.listener(Self::toggle_single_file_mode))
            .on_action(cx.listener(Self::open_file_at_cursor))
            .when(is_branch_review, |view| {
                view.on_action(cx.listener(Self::change_base_action))
            })
            .when(has_top_block, |view| {
                view.on_action(cx.listener(Self::toggle_header))
            })
            .size_full()
            .child(self.top_block.clone())
            .child(div().flex_1().min_h_0().child(self.diff.clone()))
    }
}

impl Item for ReviewView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTodo).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        format!("Review: {}", self.title(cx)).into()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let title = self.tab_content_text(0, cx);
        Some(if self.diff.read(cx).is_snapshot(cx) {
            format!("{title}\nRead-only snapshot (no language server)").into()
        } else {
            title
        })
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diff
            .update(cx, |diff, cx| diff.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.diff
            .update(cx, |diff, cx| diff.navigate(data, window, cx))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.diff.read(cx).for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.diff.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.set_nav_history(nav_history, cx));
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let repo = self.repo.clone();
        let target = self.target.clone();
        let project = self.project.clone();
        Task::ready(Some(
            cx.new(|cx| Self::new(target, repo, project, workspace, window, cx)),
        ))
    }

    fn capability(&self, _cx: &App) -> Capability {
        Capability::ReadOnly
    }

    fn as_searchable(&self, _: &Entity<Self>, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.diff.read(cx).editor().clone()))
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(
                self.diff
                    .read(cx)
                    .editor()
                    .read(cx)
                    .rhs_editor()
                    .clone()
                    .into(),
            )
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.diff.read(cx).editor().clone().into())
        } else if type_id == TypeId::of::<ReviewDiff>() {
            Some(self.diff.clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            diff.added_to_workspace(workspace, window, cx)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_test_context::assert_state_with_diff;
    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;
    use settings::{DiffViewStyle, SettingsStore};
    use std::path::Path;
    use unindent::Unindent as _;
    use util::path;
    use workspace::MultiWorkspace;

    #[gpui::test]
    async fn test_review_uncommitted_shows_head_to_working_tree(cx: &mut TestAppContext) {
        crate::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "added.txt": "new file\n",
                "modified.txt": "one\nTWO\nthree\n",
                "staged.txt": "alpha\nbeta\n",
                "src": {
                    "nested.rs": "fn a() {}\nfn b() {}\n",
                },
            }),
        )
        .await;
        let dot_git = Path::new(path!("/project/.git"));
        fs.set_head_for_repo(
            dot_git,
            &[
                ("deleted.txt", "gone\n".into()),
                ("modified.txt", "one\ntwo\nthree\n".into()),
                ("staged.txt", "alpha\n".into()),
                ("src/nested.rs", "fn a() {}\n".into()),
            ],
            "deadbeef",
        );
        fs.set_index_for_repo(
            dot_git,
            &[
                ("deleted.txt", "gone\n".into()),
                ("modified.txt", "one\ntwo\nthree\n".into()),
                ("staged.txt", "alpha\nbeta\n".into()),
                ("src/nested.rs", "fn a() {}\n".into()),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();

        let review = workspace.update(cx, |workspace, cx| {
            let active_item = workspace.active_item(cx).unwrap();
            assert_eq!(
                active_item.tab_content_text(0, cx),
                "Review: Uncommitted changes"
            );
            workspace.active_item_as::<ReviewView>(cx).unwrap()
        });
        let diff = review.read_with(cx, |review, _| review.diff().clone());
        let editor = diff.read_with(cx, |diff, cx| diff.editor().read(cx).rhs_editor().clone());

        // Staged and unstaged changes, untracked files and deletions, in tree order.
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff.excerpt_file_paths(cx)),
            [
                "src/nested.rs",
                "added.txt",
                "deleted.txt",
                "modified.txt",
                "staged.txt"
            ]
        );
        assert_state_with_diff(
            &editor,
            cx,
            &"
                  ˇfn a() {}
                + fn b() {}

                + new file

                - gone

                  one
                - two
                + TWO
                  three

                  alpha
                + beta
            "
            .unindent(),
        );

        // The review is read-only, but the project buffers behind it stay writable.
        editor.update_in(cx, |editor, window, cx| {
            assert!(editor.read_only(cx));
            assert!(editor.buffer().read(cx).read_only());
            editor.insert("typed", window, cx);
        });
        let modified_buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/modified.txt"), cx)
            })
            .await
            .unwrap();
        modified_buffer.read_with(cx, |buffer, _| {
            assert_eq!(buffer.capability(), Capability::ReadWrite);
            assert_eq!(buffer.text(), "one\nTWO\nthree\n");
            assert!(!buffer.is_dirty());
        });

        // Reviewing the same target again reuses the open tab.
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();
        workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ReviewView>(cx).count(), 1);
            assert_eq!(workspace.active_item_as::<ReviewView>(cx).unwrap(), review);
        });

        // The diff follows changes to the working tree.
        fs.insert_file(path!("/project/later.txt"), "later\n".into())
            .await;
        fs.insert_file(path!("/project/modified.txt"), "one\ntwo\nthree\n".into())
            .await;
        cx.run_until_parked();
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff.excerpt_file_paths(cx)),
            [
                "src/nested.rs",
                "added.txt",
                "deleted.txt",
                "later.txt",
                "staged.txt"
            ]
        );
    }

    #[gpui::test]
    async fn test_review_follows_split_diff_view_style(cx: &mut TestAppContext) {
        crate::init_test(cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Split);
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "modified.txt": "one\nTWO\nthree\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("modified.txt", "one\ntwo\nthree\n".into())],
        );

        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();

        let review = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ReviewView>(cx).unwrap()
        });
        review.read_with(cx, |review, cx| {
            let editor = review.diff().read(cx).editor().read(cx);
            assert!(editor.is_split());
            assert!(editor.rhs_editor().read(cx).read_only(cx));
            assert!(editor.lhs_editor().unwrap().read(cx).read_only(cx));
        });
    }

    #[gpui::test]
    async fn test_review_uncommitted_without_repository_opens_nothing(cx: &mut TestAppContext) {
        crate::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "file.txt": "text\n" }))
            .await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();

        workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ReviewView>(cx).count(), 0);
            assert!(workspace.active_item(cx).is_none());
        });
    }

    mod targets {
        use super::*;
        use crate::{
            ReviewBranch, ReviewCommits, ToggleHeader,
            diff::DiffSource,
            target::CommitRange,
            test_support::{
                C1, C2, C3, active_review, commit_data, dispatch, dot_git, excerpt_paths, oid,
                open_workspace, repository, review_count, tab_title,
            },
        };
        use gpui::VisualTestContext;
        use project::git_store::diff_buffer_list::DiffBase;

        /// Error toasts share one id, so each check dismisses them.
        fn take_notifications(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
            workspace.update(cx, |workspace, cx| {
                let ids = workspace.notification_ids();
                for id in &ids {
                    workspace.dismiss_notification(id, cx);
                }
                ids.len()
            })
        }

        fn diff_base(review: &Entity<ReviewView>, cx: &mut VisualTestContext) -> DiffBase {
            review.read_with(cx, |review, cx| match review.diff().read(cx).source() {
                DiffSource::BufferList(list) => list.read(cx).diff_base().clone(),
                DiffSource::Commits(_) => panic!("not a branch review"),
            })
        }

        /// A `feature` branch whose merge base with `origin/main` differs from the working tree
        /// in committed, uncommitted and untracked ways.
        async fn branch_fixture(
            cx: &mut TestAppContext,
        ) -> (Arc<FakeFs>, Entity<Workspace>, &mut VisualTestContext) {
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(
                path!("/project"),
                json!({
                    ".git": {},
                    "both.txt": "a\nB\nc\nD\n",
                    "committed.txt": "one\nTWO\nthree\n",
                    "untracked.txt": "new\n",
                }),
            )
            .await;
            fs.set_merge_base_content_for_repo(
                dot_git(),
                &[
                    ("both.txt", "a\nb\nc\nd\n".into()),
                    ("committed.txt", "one\ntwo\nthree\n".into()),
                    ("deleted.txt", "gone\n".into()),
                ],
            );
            fs.set_head_and_index_for_repo(
                dot_git(),
                &[
                    ("both.txt", "a\nB\nc\nd\n".into()),
                    ("committed.txt", "one\nTWO\nthree\n".into()),
                ],
            );
            fs.set_branch_name(dot_git(), Some("feature"));
            fs.with_git_state(dot_git(), true, |state| {
                state.refs.insert("origin/main".into(), C1.into());
                state.refs.insert("main".into(), C2.into());
            })
            .unwrap();
            let (workspace, cx) = open_workspace(fs.clone(), cx).await;
            (fs, workspace, cx)
        }

        #[gpui::test]
        async fn test_review_branch_shows_merge_base_to_working_tree(cx: &mut TestAppContext) {
            crate::init_test(cx);
            let (_fs, workspace, cx) = branch_fixture(cx).await;

            dispatch(ReviewBranch::default(), cx);
            let review = active_review(&workspace, cx);
            assert_eq!(tab_title(&review, cx), "Review: feature vs origin/main");
            assert_eq!(
                review.read_with(cx, |review, _| review.target().clone()),
                ReviewTarget::Branch {
                    base: "origin/main".into()
                }
            );
            // Committed, uncommitted and untracked changes since the merge base, and deletions.
            assert_eq!(
                excerpt_paths(&review, cx),
                ["both.txt", "committed.txt", "deleted.txt", "untracked.txt"]
            );
            let editor = review.read_with(cx, |review, cx| {
                review.diff().read(cx).editor().read(cx).rhs_editor().clone()
            });
            assert_state_with_diff(
                &editor,
                cx,
                &"
                      ˇa
                    - b
                    + B
                      c
                    - d
                    + D

                      one
                    - two
                    + TWO
                      three

                    - gone

                    + new
                "
                .unindent(),
            );
            review.read_with(cx, |review, cx| {
                assert!(matches!(
                    review.top_block().read(cx).content(),
                    TopBlockContent::Branch { branch, base }
                        if branch == "feature" && base == "origin/main"
                ));
            });

            // The same review, with the default base or the same base given explicitly.
            dispatch(ReviewBranch::default(), cx);
            dispatch(
                ReviewBranch {
                    base: Some("origin/main".into()),
                },
                cx,
            );
            assert_eq!(review_count(&workspace, cx), 1);
            assert_eq!(active_review(&workspace, cx), review);

            // An unknown base is an error, not an empty review.
            assert_eq!(take_notifications(&workspace, cx), 0);
            dispatch(
                ReviewBranch {
                    base: Some("nope".into()),
                },
                cx,
            );
            assert_eq!(review_count(&workspace, cx), 1);
            assert_eq!(take_notifications(&workspace, cx), 1);

            dispatch(
                ReviewBranch {
                    base: Some("main".into()),
                },
                cx,
            );
            assert_eq!(review_count(&workspace, cx), 2);
            let main_review = active_review(&workspace, cx);
            assert_eq!(tab_title(&main_review, cx), "Review: feature vs main");
        }

        #[gpui::test]
        async fn test_change_base_in_place_and_reuse(cx: &mut TestAppContext) {
            crate::init_test(cx);
            let (_fs, workspace, cx) = branch_fixture(cx).await;
            dispatch(ReviewBranch::default(), cx);
            let review = active_review(&workspace, cx);

            // Change Base opens Zed's branch picker.
            cx.focus(&review);
            dispatch(crate::ChangeBase, cx);
            workspace.read_with(cx, |workspace, cx| {
                assert!(
                    workspace
                        .active_modal::<git_ui::branch_picker::BranchList>(cx)
                        .is_some()
                );
            });
            dispatch(menu::Cancel, cx);

            workspace.update_in(cx, |workspace, window, cx| {
                ReviewView::select_base(review.clone(), "main".into(), workspace, window, cx)
            });
            cx.run_until_parked();
            assert_eq!(
                review.read_with(cx, |review, _| review.target().clone()),
                ReviewTarget::Branch { base: "main".into() }
            );
            assert_eq!(tab_title(&review, cx), "Review: feature vs main");
            assert_eq!(
                diff_base(&review, cx),
                DiffBase::Merge {
                    base_ref: "main".into()
                }
            );
            assert_eq!(
                excerpt_paths(&review, cx),
                ["both.txt", "committed.txt", "deleted.txt", "untracked.txt"]
            );

            // Choosing a base another tab already reviews against activates that tab.
            let repo = repository(&workspace, cx);
            let other = workspace.update_in(cx, |workspace, window, cx| {
                ReviewView::deploy_in_repo(
                    workspace,
                    ReviewTarget::Branch {
                        base: "origin/main".into(),
                    },
                    repo,
                    window,
                    cx,
                )
            });
            cx.run_until_parked();
            assert_ne!(other, review);
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.activate_item(&review, true, true, window, cx);
                ReviewView::select_base(
                    review.clone(),
                    "origin/main".into(),
                    workspace,
                    window,
                    cx,
                )
            });
            cx.run_until_parked();
            assert_eq!(active_review(&workspace, cx), other);
            assert_eq!(
                review.read_with(cx, |review, _| review.target().clone()),
                ReviewTarget::Branch { base: "main".into() }
            );
            assert_eq!(review_count(&workspace, cx), 2);
        }

        #[gpui::test]
        async fn test_commit_review_reuse_titles_and_top_block(cx: &mut TestAppContext) {
            crate::init_test(cx);
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(
                path!("/project"),
                json!({ ".git": {}, "a.txt": "three\n" }),
            )
            .await;
            fs.set_merge_base_content_for_repo(dot_git(), &[("a.txt", "one\n".into())]);
            fs.set_head_and_index_for_repo(dot_git(), &[("a.txt", "three\n".into())]);
            fs.with_git_state(dot_git(), true, |state| {
                state.refs.insert("HEAD".into(), C3.into());
            })
            .unwrap();
            fs.set_commit_data(
                dot_git(),
                [
                    (commit_data(C1, None, "Start"), false),
                    (commit_data(C2, Some(C1), "Write two\n\nThe second line."), false),
                    (
                        commit_data(C3, Some(C2), "Write three, which is a long subject"),
                        false,
                    ),
                ],
            );
            let (workspace, cx) = open_workspace(fs, cx).await;

            let range = format!("{C1}..{C3}");
            dispatch(
                ReviewCommits {
                    commits: Some(range.clone()),
                },
                cx,
            );
            let review = active_review(&workspace, cx);
            assert_eq!(
                review.read_with(cx, |review, _| review.target().clone()),
                ReviewTarget::Commits(CommitRange {
                    base: Some(oid(C1)),
                    head: oid(C3)
                })
            );
            assert_eq!(tab_title(&review, cx), "Review: 2 commits 2222222–3333333");
            review.read_with(cx, |review, cx| {
                let top_block = review.top_block().read(cx);
                let TopBlockContent::Commits {
                    commits: CommitsState::Loaded { commits, truncated },
                    ..
                } = top_block.content()
                else {
                    panic!("commits didn't load");
                };
                assert!(!truncated);
                let subjects = commits
                    .iter()
                    .map(|commit| commit.data.subject.to_string())
                    .collect::<Vec<_>>();
                assert_eq!(
                    subjects,
                    ["Write three, which is a long subject", "Write two"]
                );
                assert!(commits[0].body.is_none());
                assert_eq!(
                    commits[1].body.as_ref().unwrap().read(cx).source(),
                    "The second line."
                );
                assert!(!top_block.is_expanded());
            });
            cx.focus(&review);
            dispatch(ToggleHeader, cx);
            assert!(review.read_with(cx, |review, cx| review.top_block().read(cx).is_expanded()));

            dispatch(
                ReviewCommits {
                    commits: Some(range),
                },
                cx,
            );
            assert_eq!(review_count(&workspace, cx), 1);

            dispatch(
                ReviewCommits {
                    commits: Some(C3.into()),
                },
                cx,
            );
            assert_eq!(review_count(&workspace, cx), 2);
            let single = active_review(&workspace, cx);
            assert_eq!(
                single.read_with(cx, |review, _| review.target().clone()),
                ReviewTarget::Commits(CommitRange {
                    base: Some(oid(C2)),
                    head: oid(C3)
                })
            );
            assert_eq!(tab_title(&single, cx), "Review: 3333333 Write three, which i…");

            // Typos are errors.
            assert_eq!(take_notifications(&workspace, cx), 0);
            for commits in ["nope..HEAD", "a b"] {
                dispatch(
                    ReviewCommits {
                        commits: Some(commits.into()),
                    },
                    cx,
                );
                assert_eq!(take_notifications(&workspace, cx), 1, "{commits:?}");
            }
            assert_eq!(review_count(&workspace, cx), 2);
        }
    }

    mod navigation {
        use super::*;
        use crate::test_support::{
            cursor_line, cursor_point, init_vim_test, lhs_editor, move_to_row_with_text,
            open_review_fixture, path_at_cursor, rhs_editor, splittable,
        };
        use fs::Fs as _;
        use gpui::VisualTestContext;

        fn excerpt_paths(review: &Entity<ReviewView>, cx: &mut VisualTestContext) -> Vec<String> {
            review.read_with(cx, |review, cx| review.diff().read(cx).excerpt_file_paths(cx))
        }

        #[gpui::test]
        async fn test_file_navigation(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let editor = rhs_editor(&review, cx);
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));

            for (keystrokes, path, line) in [
                ("] f", "added.txt", "new 1"),
                ("}", "deleted.txt", "gone 1"),
                ("] f", "long.txt", "short"),
                ("] f", "modified.txt", "one"),
                // The last file is a stop.
                ("] f", "modified.txt", "one"),
                ("[ f", "long.txt", "short"),
                ("{", "deleted.txt", "gone 1"),
                ("[ f", "added.txt", "new 1"),
                ("[ f", "a.rs", "line 1"),
                ("[ f", "a.rs", "line 1"),
            ] {
                cx.simulate_keystrokes(keystrokes);
                assert_eq!(
                    path_at_cursor(&review, cx).as_deref(),
                    Some(path),
                    "after {keystrokes:?}"
                );
                assert_eq!(cursor_line(&editor, cx), line, "after {keystrokes:?}");
            }

            // `[ f` goes to the previous file, not to the start of the current one.
            move_to_row_with_text(&editor, "line 36", cx);
            cx.simulate_keystrokes("[ f");
            assert_eq!(cursor_line(&editor, cx), "line 36");
            cx.simulate_keystrokes("] f");
            assert_eq!(cursor_line(&editor, cx), "new 1");

            // Counts are discarded rather than applied to the next motion.
            cx.simulate_keystrokes("3 ] f");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("deleted.txt"));
            let row = cursor_point(&editor, cx).row;
            cx.simulate_keystrokes("j");
            assert_eq!(cursor_point(&editor, cx).row, row + 1);
            assert_eq!(cursor_line(&editor, cx), "gone 2");
        }

        #[gpui::test]
        async fn test_file_navigation_on_the_old_side(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Split);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let lhs = lhs_editor(&review, cx);
            cx.simulate_keystrokes("ctrl-w h");
            assert!(splittable(&review, cx).read_with(cx, |editor, _| editor.focused_editor() == &lhs));

            cx.simulate_keystrokes("] f");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("added.txt"));
            cx.simulate_keystrokes("] f");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("deleted.txt"));
            assert_eq!(cursor_line(&lhs, cx), "gone 1");
            cx.simulate_keystrokes("[ f [ f");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));
            assert_eq!(cursor_line(&lhs, cx), "line 1");
        }

        #[gpui::test]
        async fn test_unreviewed_file_navigation_wraps(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;

            cx.simulate_keystrokes("[ u");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("modified.txt"));
            cx.simulate_keystrokes("] u");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));
            cx.simulate_keystrokes("] u");
            assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("added.txt"));
        }

        #[gpui::test]
        async fn test_single_file_mode(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Unified);
            let (fs, _workspace, review, cx) = open_review_fixture(cx).await;
            let editor = rhs_editor(&review, cx);
            let all_files = ["a.rs", "added.txt", "deleted.txt", "long.txt", "modified.txt"];

            cx.simulate_keystrokes("] f j");
            cx.simulate_keystrokes("; f");
            assert_eq!(excerpt_paths(&review, cx), ["added.txt"]);
            assert_eq!(cursor_line(&editor, cx), "new 2");

            cx.simulate_keystrokes("] f");
            assert_eq!(excerpt_paths(&review, cx), ["deleted.txt"]);
            assert_eq!(cursor_line(&editor, cx), "gone 1");
            cx.update(|window, cx| assert!(editor.focus_handle(cx).is_focused(window)));
            cx.simulate_keystrokes("[ f [ f");
            assert_eq!(excerpt_paths(&review, cx), ["a.rs"]);
            assert_eq!(cursor_line(&editor, cx), "line 1");
            cx.simulate_keystrokes("] f");

            cx.simulate_keystrokes("; f");
            assert_eq!(excerpt_paths(&review, cx), all_files);
            assert_eq!(cursor_line(&editor, cx), "new 1");

            // Files that change while in single-file mode stay hidden.
            cx.simulate_keystrokes("; f");
            fs.insert_file(path!("/project/later.txt"), "later\n".into())
                .await;
            cx.run_until_parked();
            assert_eq!(excerpt_paths(&review, cx), ["added.txt"]);
            cx.simulate_keystrokes("] f ] f");
            assert_eq!(excerpt_paths(&review, cx), ["later.txt"]);
            cx.simulate_keystrokes("[ f [ f");
            assert_eq!(excerpt_paths(&review, cx), ["added.txt"]);

            // When the shown file has no changes left, every file is shown again.
            fs.remove_file(path!("/project/added.txt").as_ref(), Default::default())
                .await
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                excerpt_paths(&review, cx),
                ["a.rs", "deleted.txt", "later.txt", "long.txt", "modified.txt"]
            );
            cx.update(|window, cx| assert!(editor.focus_handle(cx).is_focused(window)));
        }

        #[gpui::test]
        async fn test_open_file_at_cursor(cx: &mut TestAppContext) {
            init_vim_test(cx, DiffViewStyle::Unified);
            let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
            let editor = rhs_editor(&review, cx);

            move_to_row_with_text(&editor, "three", cx);
            cx.simulate_keystrokes("e");
            cx.run_until_parked();
            workspace.update(cx, |workspace, cx| {
                assert!(workspace.active_item_as::<ReviewView>(cx).is_none());
                assert_eq!(workspace.items_of_type::<ReviewView>(cx).count(), 1);
                let opened = workspace.active_item_as::<Editor>(cx).unwrap();
                let opened = opened.read(cx);
                let buffer = opened.buffer().read(cx).as_singleton().unwrap();
                assert_eq!(
                    buffer.read(cx).file().unwrap().path().as_unix_str(),
                    "modified.txt"
                );
            });
            let opened = workspace.update(cx, |workspace, cx| {
                workspace.active_item_as::<Editor>(cx).unwrap()
            });
            assert_eq!(cursor_point(&opened, cx).row, 2);
        }
    }
}
