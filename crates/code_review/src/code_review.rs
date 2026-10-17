mod comments;
mod commits;
mod diff;
mod gitattributes;
mod line_cursor;
mod panel;
mod persistence;
mod picker;
mod reviewed;
mod session;
mod target;
mod top_block;
mod view;

use gpui::{Action, App, KeyBinding, actions};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{KeybindSource, KeymapFile, KeymapFileLoadResult};
use workspace::Workspace;

pub use diff::ReviewDiff;
pub use panel::ReviewPanel;
pub use picker::StartReviewModal;
pub use session::ReviewKey;
pub use target::{CommitRange, ReviewTarget};
pub use view::ReviewView;

actions!(
    code_review,
    [
        /// Opens a review of the uncommitted changes (staged and unstaged) in the active
        /// repository.
        ReviewUncommitted,
        /// Chooses what to review: the uncommitted changes, the current branch against its base,
        /// or recent commits (select several for a range).
        StartReview,
        /// Changes the base the current branch review compares against.
        ChangeBase,
        /// Shows or hides the commit messages at the top of a commit review.
        ToggleHeader,
        /// Toggles focus on the review panel.
        ToggleFocus,
        /// Opens the review panel and focuses it.
        FocusPanel,
        /// Focuses the diff of the active review.
        FocusDiff,
        /// Opens or closes the review panel.
        TogglePanel,
        /// Scrolls the review diff left. At the left edge, focuses the review panel instead.
        ScrollLeft,
        /// Scrolls the review diff right.
        ScrollRight,
        /// Moves to the first line of the next file in the review.
        NextFile,
        /// Moves to the first line of the previous file in the review.
        PreviousFile,
        /// Moves to the next file not marked as reviewed, wrapping around.
        NextUnreviewedFile,
        /// Moves to the previous file not marked as reviewed, wrapping around.
        PreviousUnreviewedFile,
        /// Shows only the file at the cursor, or all files again.
        ToggleSingleFileMode,
        /// Opens the file at the cursor line in a regular editor.
        OpenFileAtCursor,
        /// In the review panel: expands the selected directory, enters an expanded one, or shows
        /// the selected file in the diff.
        ExpandSelectedEntry,
        /// In the review panel: collapses the selected directory or selects the parent directory.
        CollapseSelectedEntry,
        /// In the review panel: expands or collapses the selected directory.
        ToggleSelectedDirectory,
        /// Marks the file at the cursor (or selected in the review panel) as reviewed, or not.
        /// In the diff, marking also moves to the next file.
        ToggleFileReviewed,
        /// Marks the hunk at the cursor as reviewed, or not. Reviewed hunks are drawn dimmed.
        ToggleHunkReviewed,
        /// Moves to the next hunk, skipping files folded as reviewed.
        NextHunk,
        /// Moves to the previous hunk, skipping files folded as reviewed.
        PreviousHunk,
        /// Shows only the files whose path matches a regex (case-insensitive).
        EditIncludeFilter,
        /// Hides the files whose path matches a regex (case-insensitive).
        EditExcludeFilter,
        /// Removes the include filter.
        ClearIncludeFilter,
        /// Removes the exclude filter.
        ClearExcludeFilter,
        /// Removes the include and exclude filters and shows reviewed files again.
        ClearFilters,
        /// Hides or shows the files marked as reviewed.
        ToggleHideReviewed,
        /// Shows or hides generated files (`linguist-generated` or `gitlab-generated` in
        /// `.gitattributes`), which are hidden by default.
        ToggleShowGenerated,
        /// Shows or hides vendored files (`linguist-vendored` in `.gitattributes`), which are
        /// hidden by default.
        ToggleShowVendored,
        /// Copies the name of the reviewed repository's current branch.
        CopyBranchName,
        /// Searches the review panel's files by path.
        SearchPaths,
        /// Selects the next file matching the review panel's path search.
        SelectNextMatch,
        /// Selects the previous file matching the review panel's path search.
        SelectPreviousMatch,
        /// Comments on the line at the cursor, or on the selected lines.
        AddComment,
        /// Comments on the file at the cursor.
        AddFileComment,
        /// Adds a comment on the whole review, shown at the top of the review.
        AddReviewComment,
        /// Edits the comment below the cursor line.
        EditComment,
        /// Deletes the comment below the cursor line, after asking.
        DeleteComment,
        /// Moves to the next comment, wrapping around.
        NextComment,
        /// Moves to the previous comment, wrapping around.
        PreviousComment,
        /// Saves the comment being edited and returns to the diff.
        SaveComment,
        /// Returns to the diff without saving the changes to the comment being edited. A new
        /// comment is deleted.
        DiscardCommentChanges,
        /// Changes the type of the comment being edited: none, issue, suggestion, note, praise.
        CycleCommentType,
        /// Changes the type of the comment being edited, in reverse order.
        CycleCommentTypeBackward,
    ]
);

/// Opens a review of the current branch: its changes since the merge base with `base`,
/// uncommitted changes included.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = code_review)]
#[serde(deny_unknown_fields)]
pub struct ReviewBranch {
    /// The ref to compare against, e.g. "origin/main". Defaults to the repository's default
    /// branch.
    #[serde(default)]
    pub base: Option<String>,
}

/// Opens a review of one commit ("abc1234", "HEAD~2") or a range of commits ("A..B": the commits
/// after A up to and including B). Without `commits`, opens the Start Review picker at the
/// recent commits.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = code_review)]
#[serde(deny_unknown_fields)]
pub struct ReviewCommits {
    #[serde(default)]
    pub commits: Option<String>,
}

// Embedded here instead of read through `settings::SettingsAssets`: rust-embed doesn't notice new
// files in release builds, and every edit to an embedded keymap would rebuild `settings` and
// everything that depends on it.
const KEYMAP: &str = include_str!("../../../assets/keymaps/code_review.json");
const VIM_KEYMAP: &str = include_str!("../../../assets/keymaps/code_review_vim.json");

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        panel::register(workspace);
        picker::register(workspace);
        view::register(workspace);
    })
    .detach();
}

/// Bindings from `assets/keymaps/code_review.json`, loaded after the base keymap.
pub fn key_bindings(cx: &App) -> Vec<KeyBinding> {
    load_keymap(
        KEYMAP,
        "keymaps/code_review.json",
        KeybindSource::Default,
        cx,
    )
}

/// Bindings from `assets/keymaps/code_review_vim.json`. They override vim's own bindings, so they
/// must be loaded right after `keymaps/vim.json`, under the same vim/helix mode condition.
pub fn vim_key_bindings(cx: &App) -> Vec<KeyBinding> {
    load_keymap(
        VIM_KEYMAP,
        "keymaps/code_review_vim.json",
        KeybindSource::Vim,
        cx,
    )
}

/// One binding that fails to load (say, an action upstream renamed) mustn't take the others with
/// it: the bindings that block insert mode in the review would go too.
fn load_keymap(content: &str, name: &str, source: KeybindSource, cx: &App) -> Vec<KeyBinding> {
    let (key_bindings, error) = parse_keymap(content, source, cx);
    if let Some(error) = error {
        log::error!("loading {name}: {error:#}");
    }
    key_bindings
}

/// The bindings that loaded, and why the others didn't.
fn parse_keymap(
    content: &str,
    source: KeybindSource,
    cx: &App,
) -> (Vec<KeyBinding>, Option<anyhow::Error>) {
    let (mut key_bindings, error) = match KeymapFile::load(content, cx) {
        KeymapFileLoadResult::Success { key_bindings } => (key_bindings, None),
        KeymapFileLoadResult::SomeFailedToLoad {
            key_bindings,
            error_message,
        } => (key_bindings, Some(anyhow::anyhow!("{error_message}"))),
        KeymapFileLoadResult::JsonParseFailure { error } => (Vec::new(), Some(error)),
    };
    for key_binding in &mut key_bindings {
        key_binding.set_meta(source.meta());
    }
    (key_bindings, error)
}

#[cfg(test)]
pub(crate) fn init_test(cx: &mut gpui::TestAppContext) {
    use gpui::BorrowAppContext as _;

    cx.update(|cx| {
        // A fresh database per test, so stored review state doesn't leak between tests.
        cx.set_global(db::AppDatabase::test_new());
        let store = settings::SettingsStore::test(cx);
        cx.set_global(store);
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.editor.diff_view_style = Some(settings::DiffViewStyle::Unified);
            });
        });
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        init(cx);
    });
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use askpass::AskPassDelegate;
    use editor::{Editor, SelectionEffects, SplittableEditor};
    use git::repository::{CommitData, CommitOptions};
    use gpui::{
        Action, AppContext as _, BorrowAppContext as _, Entity, TestAppContext, VisualContext as _,
        VisualTestContext,
    };
    use language::Point;
    use multi_buffer::MultiBufferRow;
    use project::{FakeFs, Project, git_store::Repository};
    use serde_json::json;
    use settings::{DiffViewStyle, SettingsStore};
    use std::{path::Path, sync::Arc};
    use util::path;
    use workspace::MultiWorkspace;

    /// `init_test` plus vim mode and the keymaps in the order `zed` loads them (minus the base
    /// keymap overlay). Vim attaches to
    /// editors when they are created, so this must run before the review opens.
    pub(crate) fn init_vim_test(cx: &mut TestAppContext, diff_view_style: DiffViewStyle) {
        init_test(cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.vim_mode = Some(true);
                    settings.editor.diff_view_style = Some(diff_view_style);
                });
            });
            vim::init(cx);
            let load_asset = |asset: &str, source: KeybindSource, cx: &mut App| {
                let mut key_bindings =
                    KeymapFile::load_asset_allow_partial_failure(asset, cx).unwrap();
                for key_binding in &mut key_bindings {
                    key_binding.set_meta(source.meta());
                }
                cx.bind_keys(key_bindings);
            };
            load_asset("keymaps/default-linux.json", KeybindSource::Default, cx);
            cx.bind_keys(key_bindings(cx));
            load_asset("keymaps/vim.json", KeybindSource::Vim, cx);
            cx.bind_keys(vim_key_bindings(cx));
        });
    }

    pub(crate) fn add_review_panel(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<ReviewPanel> {
        workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| ReviewPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        })
    }

    pub(crate) fn splittable(
        review: &Entity<ReviewView>,
        cx: &VisualTestContext,
    ) -> Entity<SplittableEditor> {
        review.read_with(cx, |review, cx| review.diff().read(cx).editor().clone())
    }

    pub(crate) fn rhs_editor(
        review: &Entity<ReviewView>,
        cx: &VisualTestContext,
    ) -> Entity<Editor> {
        splittable(review, cx).read_with(cx, |editor, _| editor.rhs_editor().clone())
    }

    pub(crate) fn lhs_editor(
        review: &Entity<ReviewView>,
        cx: &VisualTestContext,
    ) -> Entity<Editor> {
        splittable(review, cx).read_with(cx, |editor, _| editor.lhs_editor().unwrap().clone())
    }

    pub(crate) fn cursor_point(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        })
    }

    pub(crate) fn row_text(
        editor: &Entity<Editor>,
        row: u32,
        cx: &mut VisualTestContext,
    ) -> String {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let end = Point::new(row, snapshot.line_len(MultiBufferRow(row)));
            snapshot.text_for_range(Point::new(row, 0)..end).collect()
        })
    }

    pub(crate) fn move_to_row_with_text(
        editor: &Entity<Editor>,
        text: &str,
        cx: &mut VisualTestContext,
    ) -> u32 {
        let row = editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            (0..=snapshot.max_row().0)
                .find(|row| {
                    let end = Point::new(*row, snapshot.line_len(MultiBufferRow(*row)));
                    snapshot
                        .text_for_range(Point::new(*row, 0)..end)
                        .collect::<String>()
                        == text
                })
                .unwrap()
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)])
            });
        });
        cx.run_until_parked();
        row
    }

    pub(crate) fn cursor_line(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
        let row = cursor_point(editor, cx).row;
        row_text(editor, row, cx)
    }

    /// The repository path of the file under the cursor.
    pub(crate) fn path_at_cursor(
        review: &Entity<ReviewView>,
        cx: &mut VisualTestContext,
    ) -> Option<String> {
        review.read_with(cx, |review, cx| {
            let diff = review.diff().read(cx);
            let path_key = diff.path_at_cursor(cx)?;
            Some(diff.repo_path_for(&path_key)?.as_unix_str().to_string())
        })
    }

    /// Whether the file's buffer is folded on the right side of the diff.
    pub(crate) fn is_folded(
        review: &Entity<ReviewView>,
        path: &str,
        cx: &mut VisualTestContext,
    ) -> bool {
        review.read_with(cx, |review, cx| {
            let diff = review.diff().read(cx);
            let repo_path = git::repository::RepoPath::new(path).unwrap();
            let buffer_id = diff
                .buffer_id_for(&crate::diff::review_path_key(&repo_path), cx)
                .unwrap();
            diff.editor()
                .read(cx)
                .rhs_editor()
                .read(cx)
                .is_buffer_folded(buffer_id, cx)
        })
    }

    pub(crate) fn is_reviewed(
        review: &Entity<ReviewView>,
        path: &str,
        cx: &mut VisualTestContext,
    ) -> bool {
        review.read_with(cx, |review, cx| {
            let repo_path = git::repository::RepoPath::new(path).unwrap();
            review.session().read(cx).is_file_reviewed(&repo_path)
        })
    }

    pub(crate) fn line(index: usize) -> String {
        format!("line {index}\n")
    }

    pub(crate) const C1: &str = "1111111111111111111111111111111111111111";
    pub(crate) const C2: &str = "2222222222222222222222222222222222222222";
    pub(crate) const C3: &str = "3333333333333333333333333333333333333333";

    pub(crate) fn dot_git() -> &'static Path {
        Path::new(path!("/project/.git"))
    }

    /// Makes `name` resolve to the commit `sha` in the fake repository, which answers
    /// `revparse_batch` from its refs only.
    pub(crate) fn insert_commit_ref(fs: &FakeFs, name: &str, sha: &str) {
        fs.with_git_state(dot_git(), true, |state| {
            state.refs.insert(name.into(), sha.into());
            state.refs.insert(format!("{name}^{{commit}}"), sha.into());
        })
        .unwrap();
    }

    pub(crate) fn oid(sha: &str) -> git::Oid {
        sha.parse().unwrap()
    }

    pub(crate) fn commit_data(sha: &str, parent: Option<&str>, message: &str) -> CommitData {
        CommitData {
            sha: oid(sha),
            parents: parent.map(oid).into_iter().collect(),
            author_name: "Ada".into(),
            author_email: "ada@example.com".into(),
            commit_timestamp: 1_700_000_000,
            subject: message
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
                .into(),
            message: message.to_string().into(),
        }
    }

    /// A workspace over `/project`, focused.
    pub(crate) async fn open_workspace(
        fs: Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();
        cx.focus(&workspace);
        (workspace, cx)
    }

    pub(crate) fn repository(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<Repository> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.project().read(cx).active_repository(cx).unwrap()
        })
    }

    /// Makes `sha` with `contents` a historic commit of the fake repository, which only keeps
    /// the contents of the HEADs it had before each commit.
    pub(crate) async fn record_commit(
        fs: &FakeFs,
        repo: &Entity<Repository>,
        contents: &[(&str, String)],
        sha: &str,
        cx: &mut VisualTestContext,
    ) {
        fs.set_head_for_repo(dot_git(), contents, sha);
        cx.run_until_parked();
        let committed = repo.update(cx, |repo, cx| {
            repo.commit(
                "next".into(),
                None,
                CommitOptions {
                    allow_empty: true,
                    ..Default::default()
                },
                AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
                cx,
            )
        });
        cx.run_until_parked();
        committed.await.unwrap().unwrap();
    }

    pub(crate) fn dispatch(action: impl Action, cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.dispatch_action(action.boxed_clone(), cx));
        cx.run_until_parked();
    }

    pub(crate) fn active_review(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<ReviewView> {
        workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ReviewView>(cx).unwrap()
        })
    }

    pub(crate) fn review_count(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
        workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<ReviewView>(cx).count()
        })
    }

    pub(crate) fn tab_title(review: &Entity<ReviewView>, cx: &mut VisualTestContext) -> String {
        use workspace::item::Item as _;
        review.read_with(cx, |review, cx| review.tab_content_text(0, cx).to_string())
    }

    pub(crate) fn excerpt_paths(
        review: &Entity<ReviewView>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        review.read_with(cx, |review, cx| {
            review.diff().read(cx).excerpt_file_paths(cx)
        })
    }

    /// A repository whose uncommitted changes are, in review order: `a.rs` (two hunks far apart),
    /// `added.txt`, `deleted.txt`, `long.txt` (a 300-column line) and `modified.txt` (a
    /// modification plus a pure addition, which faces a spacer in split view).
    pub(crate) async fn open_review_fixture(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<Workspace>,
        Entity<ReviewView>,
        &mut VisualTestContext,
    ) {
        let head_a: String = (1..=40).map(line).collect();
        let worktree_a = head_a
            .replace(&line(2), "LINE 2\n")
            .replace(&line(35), "LINE 35\n");
        let head_long = format!("short\n{}\n", "x".repeat(300));
        let worktree_long = format!("short\n{}\n", "y".repeat(300));

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.rs": worktree_a,
                "added.txt": "new 1\nnew 2\n",
                "long.txt": worktree_long,
                "modified.txt": "ONE\ntwo\nthree\nfour\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[
                ("a.rs", head_a),
                ("deleted.txt", "gone 1\ngone 2\n".into()),
                ("long.txt", head_long),
                ("modified.txt", "one\ntwo\nthree\n".into()),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
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
        cx.focus(&review);
        cx.run_until_parked();
        (fs, workspace, review, cx)
    }

    /// The HEAD contents of [`open_tree_fixture`]'s repository.
    pub(crate) fn tree_fixture_head() -> Vec<(&'static str, String)> {
        let lib: String = (1..=30).map(line).collect();
        vec![
            (".gitattributes", TREE_FIXTURE_ATTRIBUTES.into()),
            ("Cargo.lock", "lock 1\nlock 2\n".into()),
            ("docs/old.md", "old\n".into()),
            ("src/lib.rs", lib),
            ("src/mod_b.rs", "b 1\nb 2\n".into()),
            ("z.txt", "one\ntwo\nthree\n".into()),
        ]
    }

    pub(crate) const TREE_FIXTURE_ATTRIBUTES: &str =
        "Cargo.lock linguist-generated\nvendor/** linguist-vendored\n";

    /// A repository with nested directories, a generated and a vendored file (per its
    /// `.gitattributes`) and a file with two hunks (`src/lib.rs`). Review order:
    /// `docs/old.md` (deleted), `src/a/b/c.rs` (added), `src/lib.rs`, `src/mod_b.rs`,
    /// `vendor/x.js` (added, vendored), `Cargo.lock` (generated), `z.txt`.
    pub(crate) async fn open_tree_fixture(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<Workspace>,
        Entity<ReviewView>,
        &mut VisualTestContext,
    ) {
        let lib = (1..=30)
            .map(line)
            .collect::<String>()
            .replace(&line(2), "LINE 2\n")
            .replace(&line(25), "LINE 25\n");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                ".gitattributes": TREE_FIXTURE_ATTRIBUTES,
                "Cargo.lock": "lock 1\nlock 2\nlock 3\n",
                "src": {
                    "a": { "b": { "c.rs": "fn c() {}\n" } },
                    "lib.rs": lib,
                    "mod_b.rs": "b 1\nB 2\n",
                },
                "vendor": { "x.js": "x();\n" },
                "z.txt": "one\nTWO\nthree\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(dot_git(), &tree_fixture_head());
        let (workspace, cx) = open_workspace(fs.clone(), cx).await;
        cx.update(|window, cx| window.dispatch_action(ReviewUncommitted.boxed_clone(), cx));
        cx.run_until_parked();
        let review = active_review(&workspace, cx);
        cx.focus(&review);
        cx.run_until_parked();
        (fs, workspace, review, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AppContext as _, Focusable as _, TestAppContext, VisualContext as _, VisualTestContext,
    };
    use project::{FakeFs, Project};
    use workspace::MultiWorkspace;

    #[ctor::ctor(unsafe)]
    fn init_logger() {
        zlog::init_test();
    }

    #[gpui::test]
    async fn test_toggle_focus_moves_focus_to_review_panel_and_back(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| ReviewPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        cx.focus(&workspace);
        cx.update(|window, cx| window.dispatch_action(Box::new(ToggleFocus), cx));
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            assert!(workspace.left_dock().read(cx).is_open());
            assert!(panel.read(cx).focus_handle(cx).contains_focused(window, cx));
        });

        cx.update(|window, cx| window.dispatch_action(Box::new(ToggleFocus), cx));
        cx.run_until_parked();
        workspace.update_in(cx, |_workspace, window, cx| {
            assert!(!panel.read(cx).focus_handle(cx).contains_focused(window, cx));
        });
    }

    #[gpui::test]
    async fn test_panel_keys(cx: &mut TestAppContext) {
        test_support::init_vim_test(cx, settings::DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = test_support::open_review_fixture(cx).await;
        let panel = test_support::add_review_panel(&workspace, cx);
        let editor = review.read_with(cx, |review, cx| {
            review
                .diff()
                .read(cx)
                .editor()
                .read(cx)
                .rhs_editor()
                .clone()
        });
        let panel_focused = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| panel.read(cx).focus_handle(cx).contains_focused(window, cx))
        };
        let editor_focused = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| editor.read(cx).focus_handle(cx).is_focused(window))
        };
        let dock_open = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| workspace.left_dock().read(cx).is_open())
        };

        // The panel follows the review tab.
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.active_review(), Some(review.clone()))
        });

        cx.simulate_keystrokes("; e");
        assert!(dock_open(cx));
        assert!(editor_focused(cx));
        cx.simulate_keystrokes("; e");
        assert!(!dock_open(cx));
        assert!(editor_focused(cx));

        cx.simulate_keystrokes("; h");
        assert!(dock_open(cx));
        assert!(panel_focused(cx));
        cx.simulate_keystrokes("; l");
        assert!(editor_focused(cx));

        cx.simulate_keystrokes("; h");
        assert!(panel_focused(cx));
        cx.simulate_keystrokes("; e");
        assert!(!dock_open(cx));
        assert!(editor_focused(cx));

        // Closing the review tab empties the panel.
        cx.update(|window, cx| {
            window.dispatch_action(workspace::CloseActiveItem::default().boxed_clone(), cx)
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| assert_eq!(panel.active_review(), None));
    }

    #[gpui::test]
    fn test_keymaps_load_without_errors(cx: &mut TestAppContext) {
        // Unknown action names are errors, so vim's actions must be registered.
        test_support::init_vim_test(cx, settings::DiffViewStyle::Unified);
        cx.update(|cx| {
            let (_, error) = parse_keymap(KEYMAP, KeybindSource::Default, cx);
            assert!(error.is_none(), "{error:?}");
            let (vim_bindings, error) = parse_keymap(VIM_KEYMAP, KeybindSource::Vim, cx);
            assert!(error.is_none(), "{error:?}");
            for action in [
                "code_review::FocusPanel",
                "code_review::AddComment",
                "code_review::AddFileComment",
                "code_review::AddReviewComment",
                "code_review::EditComment",
                "code_review::DeleteComment",
                "code_review::NextComment",
                "code_review::PreviousComment",
                "code_review::SaveComment",
                "code_review::CycleCommentType",
                "code_review::CycleCommentTypeBackward",
            ] {
                assert!(
                    vim_bindings
                        .iter()
                        .any(|binding| binding.action().name() == action),
                    "{action} isn't bound"
                );
            }
        });
    }

    #[gpui::test]
    fn test_a_bad_binding_keeps_the_others(cx: &mut TestAppContext) {
        test_support::init_vim_test(cx, settings::DiffViewStyle::Unified);
        cx.update(|cx| {
            let keymap = r#"[
                {
                    "context": "CodeReview > Editor",
                    "bindings": { "] f": "code_review::NextFile", "] q": "vim::NoSuchAction" }
                }
            ]"#;
            let (bindings, error) = parse_keymap(keymap, KeybindSource::Vim, cx);
            assert!(error.is_some());
            assert_eq!(bindings.len(), 1);
            assert_eq!(bindings[0].action().name(), "code_review::NextFile");
        });
    }
}
