mod diff;
mod line_cursor;
mod panel;
mod target;
mod view;

use anyhow::Context as _;
use gpui::{App, KeyBinding, actions};
use settings::{KeybindSource, KeymapFile, KeymapFileLoadResult};
use util::ResultExt as _;
use workspace::Workspace;

pub use diff::ReviewDiff;
pub use panel::ReviewPanel;
pub use target::ReviewTarget;
pub use view::ReviewView;

actions!(
    code_review,
    [
        /// Opens a review of the uncommitted changes (staged and unstaged) in the active
        /// repository.
        ReviewUncommitted,
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
    ]
);

// Embedded here instead of read through `settings::SettingsAssets`: rust-embed doesn't notice new
// files in release builds, and every edit to an embedded keymap would rebuild `settings` and
// everything that depends on it.
const KEYMAP: &str = include_str!("../../../assets/keymaps/code_review.json");
const VIM_KEYMAP: &str = include_str!("../../../assets/keymaps/code_review_vim.json");

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        panel::register(workspace);
        view::register(workspace);
    })
    .detach();
}

/// Bindings from `assets/keymaps/code_review.json`, loaded after the base keymap.
pub fn key_bindings(cx: &App) -> Vec<KeyBinding> {
    parse_keymap(KEYMAP, KeybindSource::Default, cx)
        .context("loading keymaps/code_review.json")
        .log_err()
        .unwrap_or_default()
}

/// Bindings from `assets/keymaps/code_review_vim.json`. They override vim's own bindings, so they
/// must be loaded right after `keymaps/vim.json`, under the same vim/helix mode condition.
pub fn vim_key_bindings(cx: &App) -> Vec<KeyBinding> {
    parse_keymap(VIM_KEYMAP, KeybindSource::Vim, cx)
        .context("loading keymaps/code_review_vim.json")
        .log_err()
        .unwrap_or_default()
}

fn parse_keymap(content: &str, source: KeybindSource, cx: &App) -> anyhow::Result<Vec<KeyBinding>> {
    match KeymapFile::load(content, cx) {
        KeymapFileLoadResult::Success { mut key_bindings } => {
            for key_binding in &mut key_bindings {
                key_binding.set_meta(source.meta());
            }
            Ok(key_bindings)
        }
        KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => {
            anyhow::bail!("{error_message}")
        }
        KeymapFileLoadResult::JsonParseFailure { error } => Err(error),
    }
}

#[cfg(test)]
pub(crate) fn init_test(cx: &mut gpui::TestAppContext) {
    use gpui::BorrowAppContext as _;

    cx.update(|cx| {
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
    use gpui::{
        Action as _, AppContext as _, BorrowAppContext as _, Entity, TestAppContext,
        VisualContext as _, VisualTestContext,
    };
    use editor::{Editor, SelectionEffects, SplittableEditor};
    use language::Point;
    use multi_buffer::MultiBufferRow;
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::{DiffViewStyle, SettingsStore};
    use std::{path::Path, sync::Arc};
    use util::path;
    use workspace::MultiWorkspace;

    /// `init_test` plus vim mode and the keymaps in the order `zed` loads them. Vim attaches to
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
            for (asset, source) in [
                ("keymaps/default-linux.json", KeybindSource::Default),
                ("keymaps/vim.json", KeybindSource::Vim),
            ] {
                let mut key_bindings =
                    KeymapFile::load_asset_allow_partial_failure(asset, cx).unwrap();
                for key_binding in &mut key_bindings {
                    key_binding.set_meta(source.meta());
                }
                cx.bind_keys(key_bindings);
            }
            cx.bind_keys(parse_keymap(VIM_KEYMAP, KeybindSource::Vim, cx).unwrap());
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

    pub(crate) fn splittable(review: &Entity<ReviewView>, cx: &VisualTestContext) -> Entity<SplittableEditor> {
        review.read_with(cx, |review, cx| review.diff().read(cx).editor().clone())
    }

    pub(crate) fn rhs_editor(review: &Entity<ReviewView>, cx: &VisualTestContext) -> Entity<Editor> {
        splittable(review, cx).read_with(cx, |editor, _| editor.rhs_editor().clone())
    }

    pub(crate) fn lhs_editor(review: &Entity<ReviewView>, cx: &VisualTestContext) -> Entity<Editor> {
        splittable(review, cx).read_with(cx, |editor, _| editor.lhs_editor().unwrap().clone())
    }

    pub(crate) fn cursor_point(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        })
    }

    pub(crate) fn row_text(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) -> String {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let end = Point::new(row, snapshot.line_len(MultiBufferRow(row)));
            snapshot.text_for_range(Point::new(row, 0)..end).collect()
        })
    }

    pub(crate) fn move_to_row_with_text(editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext) -> u32 {
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

    /// The worktree path of the file under the cursor.
    pub(crate) fn path_at_cursor(
        review: &Entity<ReviewView>,
        cx: &mut VisualTestContext,
    ) -> Option<String> {
        review.read_with(cx, |review, cx| {
            review
                .diff()
                .read(cx)
                .path_at_cursor(cx)
                .map(|path_key| path_key.path.as_unix_str().to_string())
        })
    }

    pub(crate) fn line(index: usize) -> String {
        format!("line {index}\n")
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Action as _, AppContext as _, Focusable as _, TestAppContext, VisualContext as _,
        VisualTestContext,
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
            review.diff().read(cx).editor().read(cx).rhs_editor().clone()
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
        panel.read_with(cx, |panel, _| assert_eq!(panel.active_review(), Some(review.clone())));

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
        // Unknown action names fail the whole keymap, so vim's actions must be registered.
        test_support::init_vim_test(cx, settings::DiffViewStyle::Unified);
        cx.update(|cx| {
            parse_keymap(KEYMAP, KeybindSource::Default, cx).unwrap();
            let vim_bindings = parse_keymap(VIM_KEYMAP, KeybindSource::Vim, cx).unwrap();
            assert!(
                vim_bindings
                    .iter()
                    .any(|binding| binding.action().name() == "code_review::FocusPanel")
            );
        });
    }
}
