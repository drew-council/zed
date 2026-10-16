mod panel;

use anyhow::Context as _;
use gpui::{App, KeyBinding, actions};
use settings::{KeybindSource, KeymapFile, KeymapFileLoadResult};
use util::ResultExt as _;
use workspace::Workspace;

pub use panel::ReviewPanel;

actions!(
    code_review,
    [
        /// Toggles focus on the review panel.
        ToggleFocus,
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
    cx.update(|cx| {
        let store = settings::SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        init(cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Focusable as _, TestAppContext, VisualContext as _};
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
            let panel = cx.new(ReviewPanel::new);
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
    fn test_keymaps_load_without_errors(cx: &mut TestAppContext) {
        cx.update(|cx| {
            parse_keymap(KEYMAP, KeybindSource::Default, cx).unwrap();
            let vim_bindings = parse_keymap(VIM_KEYMAP, KeybindSource::Vim, cx).unwrap();
            assert!(
                vim_bindings
                    .iter()
                    .any(|binding| binding.action().name() == "code_review::ToggleFocus")
            );
        });
    }
}
