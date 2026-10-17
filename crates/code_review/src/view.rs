use crate::{
    FocusDiff, FocusPanel, ReviewPanel, ReviewUncommitted, ScrollLeft, ScrollRight,
    diff::ReviewDiff,
    line_cursor::{HORIZONTAL_SCROLL_COLUMNS, HorizontalScroll, LineCursor},
    target::ReviewTarget,
};
use editor::{Editor, EditorEvent, SplittableEditor};
use gpui::{
    Action as _, AnyEntity, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    WeakEntity,
};
use language::{Capability, Point};
use project::{Project, ProjectPath, git_store::Repository};
use std::{
    any::{Any, TypeId},
    sync::Arc,
};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    ItemNavHistory, Toast, Workspace,
    item::{Item, ItemEvent},
    notifications::NotificationId,
    searchable::SearchableItemHandle,
};

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ReviewUncommitted, window, cx| {
        ReviewView::deploy(workspace, ReviewTarget::Uncommitted, window, cx);
    });
    workspace.register_action(|workspace, _: &FocusDiff, window, cx| {
        focus_diff(workspace, window, cx);
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
    diff: Entity<ReviewDiff>,
    line_cursor: Entity<LineCursor>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    _diff_subscription: Subscription,
}

impl ReviewView {
    /// Activates the open review of `target` in the active repository, or opens a new one.
    pub fn deploy(
        workspace: &mut Workspace,
        target: ReviewTarget,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Self>> {
        let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<Self>(),
                    "Code review needs a git repository",
                )
                .autohide(),
                cx,
            );
            return None;
        };

        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|view| view.read(cx).reviews(&target, &repo, cx));
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return Some(existing);
        }

        let project = workspace.project().clone();
        let workspace_handle = cx.entity();
        let view = cx.new(|cx| Self::new(target, repo, project, workspace_handle, window, cx));
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        Some(view)
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
                repo,
                project.clone(),
                workspace.clone(),
                window,
                cx,
            )
        });
        let diff_subscription = cx.subscribe(&diff, |_, _, event: &EditorEvent, cx| {
            cx.emit(event.clone())
        });
        let line_cursor = cx.new(|cx| LineCursor::new(diff.read(cx).editor().clone(), cx));
        Self {
            target,
            diff,
            line_cursor,
            project,
            workspace: workspace.downgrade(),
            _diff_subscription: diff_subscription,
        }
    }

    pub fn target(&self) -> &ReviewTarget {
        &self.target
    }

    pub fn diff(&self) -> &Entity<ReviewDiff> {
        &self.diff
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

    fn reviews(&self, target: &ReviewTarget, repo: &Entity<Repository>, cx: &App) -> bool {
        self.target == *target
            && self
                .diff
                .read(cx)
                .repo(cx)
                .is_some_and(|current| current.entity_id() == repo.entity_id())
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
        div()
            .key_context("CodeReview")
            .on_action(cx.listener(Self::scroll_left))
            .on_action(cx.listener(Self::scroll_right))
            .size_full()
            .child(self.diff.clone())
    }
}

impl Item for ReviewView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTodo).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Review: {}", self.target.title()).into()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.tab_content_text(0, cx))
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
        let Some((workspace, repo)) = self.workspace.upgrade().zip(self.diff.read(cx).repo(cx))
        else {
            return Task::ready(None);
        };
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
}
