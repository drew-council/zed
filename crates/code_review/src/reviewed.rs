use crate::{ReviewView, ToggleFileReviewed};
use gpui::{Entity, MouseButton};
use language::BufferId;
use ui::{Checkbox, ElevationIndex, Tooltip, prelude::*};

/// The reviewed checkbox in a file header (also on folded headers, so reviewed files can be
/// unmarked from there).
pub(crate) fn render_file_checkbox(
    view: &Entity<ReviewView>,
    buffer_id: BufferId,
    cx: &App,
) -> Option<AnyElement> {
    let review = view.read(cx);
    let repo_path = review.diff().read(cx).repo_path_for_buffer(buffer_id, cx)?;
    let reviewed = review.session().read(cx).is_file_reviewed(&repo_path);
    let checkbox = Checkbox::new(("review-file", buffer_id.to_proto()), reviewed.into())
        .fill()
        .elevation(ElevationIndex::Surface)
        .tooltip(Tooltip::for_action_title(
            if reviewed {
                "Mark as Not Reviewed"
            } else {
                "Mark as Reviewed"
            },
            &ToggleFileReviewed,
        ))
        .on_click({
            let view = view.downgrade();
            move |_, _, cx| {
                view.update(cx, |view, cx| view.toggle_file(&repo_path, cx))
                    .ok();
                cx.stop_propagation();
            }
        });
    Some(
        h_flex()
            .id("review-file-checkbox")
            .child(checkbox)
            // Keeps the header's own click handling (folding, opening the file) out of it.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use crate::{
        ReviewUncommitted,
        test_support::{
            active_review, add_review_panel, cursor_line, dispatch, init_vim_test, is_folded,
            is_reviewed, lhs_editor, open_review_fixture, path_at_cursor, rhs_editor,
        },
    };
    use fs::Fs as _;
    use gpui::{Focusable as _, TestAppContext};
    use settings::DiffViewStyle;
    use util::path;

    #[gpui::test]
    async fn test_toggle_file_reviewed_folds_and_advances(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let editor = rhs_editor(&review, cx);
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));

        cx.simulate_keystrokes("r");
        assert!(is_reviewed(&review, "a.rs", cx));
        assert!(is_folded(&review, "a.rs", cx));
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("added.txt"));
        assert_eq!(cursor_line(&editor, cx), "new 1");
        // The header (folded or not) has the checkbox.
        let buffer_ids = review.read_with(cx, |review, cx| {
            let diff = review.diff().read(cx);
            diff.files(cx)
                .iter()
                .map(|file| diff.buffer_id_for(&file.path_key, cx).unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(buffer_ids.len(), 5);
        for buffer_id in buffer_ids {
            cx.update(|_, cx| {
                assert!(super::render_file_checkbox(&review, buffer_id, cx).is_some());
            });
        }

        // On the folded header, `r` unmarks the file and lands on its first row.
        cx.simulate_keystrokes("[ f");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));
        cx.simulate_keystrokes("r");
        assert!(!is_reviewed(&review, "a.rs", cx));
        assert!(!is_folded(&review, "a.rs", cx));
        assert_eq!(cursor_line(&editor, cx), "line 1");

        // The last file has nothing to advance to: the cursor stays on its folded header.
        cx.simulate_keystrokes("] f ] f ] f ] f");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("modified.txt"));
        cx.simulate_keystrokes("r");
        assert!(is_folded(&review, "modified.txt", cx));
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("modified.txt"));
        cx.simulate_keystrokes("r");
        assert!(!is_folded(&review, "modified.txt", cx));
        assert_eq!(cursor_line(&editor, cx), "one");
    }

    #[gpui::test]
    async fn test_toggle_file_reviewed_in_split_view(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Split);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let lhs = lhs_editor(&review, cx);

        cx.simulate_keystrokes("r");
        assert!(is_folded(&review, "a.rs", cx));
        lhs.read_with(cx, |lhs, cx| assert_eq!(lhs.folded_buffers(cx).len(), 1));
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("added.txt"));

        // From the left side too.
        cx.simulate_keystrokes("ctrl-w h");
        cx.simulate_keystrokes("] f");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("deleted.txt"));
        cx.simulate_keystrokes("r");
        assert!(is_folded(&review, "deleted.txt", cx));
        lhs.read_with(cx, |lhs, cx| assert_eq!(lhs.folded_buffers(cx).len(), 2));
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("long.txt"));
    }

    #[gpui::test]
    async fn test_reviewed_files_persist_and_invalidate(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (fs, workspace, review, cx) = open_review_fixture(cx).await;
        // a.rs, then modified.txt.
        cx.simulate_keystrokes("r ] f ] f ] f r");
        assert!(is_reviewed(&review, "a.rs", cx));
        assert!(is_reviewed(&review, "modified.txt", cx));
        let session_id = review.read_with(cx, |review, _| review.session().entity_id());

        dispatch(workspace::CloseActiveItem::default(), cx);
        drop(review);
        cx.run_until_parked();
        dispatch(ReviewUncommitted, cx);
        let review = active_review(&workspace, cx);
        cx.run_until_parked();
        assert_ne!(
            review.read_with(cx, |review, _| review.session().entity_id()),
            session_id
        );
        assert!(is_reviewed(&review, "a.rs", cx));
        assert!(is_folded(&review, "a.rs", cx));
        assert!(is_reviewed(&review, "modified.txt", cx));
        assert!(is_folded(&review, "modified.txt", cx));
        assert!(!is_folded(&review, "added.txt", cx));

        // A change to a reviewed file's diff clears its mark and unfolds it; other files keep
        // theirs.
        let a = fs.load(path!("/project/a.rs").as_ref()).await.unwrap();
        fs.insert_file(path!("/project/a.rs"), a.replace("line 20\n", "LINE 20\n").into_bytes())
            .await;
        cx.run_until_parked();
        assert!(!is_reviewed(&review, "a.rs", cx));
        assert!(!is_folded(&review, "a.rs", cx));
        assert!(is_reviewed(&review, "modified.txt", cx));
        assert!(is_folded(&review, "modified.txt", cx));
    }

    #[gpui::test]
    async fn test_unreviewed_and_hunk_navigation(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let editor = rhs_editor(&review, cx);
        // a.rs and deleted.txt.
        cx.simulate_keystrokes("r ] f r");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("long.txt"));

        cx.simulate_keystrokes("] u");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("modified.txt"));
        cx.simulate_keystrokes("] u");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("added.txt"));
        cx.simulate_keystrokes("[ u");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("modified.txt"));

        // `] c` / `[ c` never stop inside a folded file, and wrap.
        cx.simulate_keystrokes("g g");
        let mut visited = Vec::new();
        for _ in 0..6 {
            cx.simulate_keystrokes("] c");
            visited.push(path_at_cursor(&review, cx).unwrap());
        }
        assert_eq!(
            visited,
            ["added.txt", "long.txt", "modified.txt", "modified.txt", "added.txt", "long.txt"]
        );
        cx.simulate_keystrokes("[ c [ c");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("modified.txt"));
        assert_eq!(cursor_line(&editor, cx), "four");

        // Single-file mode shows a reviewed file unfolded; leaving it folds the file again.
        cx.simulate_keystrokes("g g");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));
        cx.simulate_keystrokes("; f");
        assert!(!is_folded(&review, "a.rs", cx));
        cx.simulate_keystrokes("] f");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("added.txt"));
        cx.simulate_keystrokes("] f");
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("deleted.txt"));
        assert!(!is_folded(&review, "deleted.txt", cx));
        cx.simulate_keystrokes("; f");
        assert!(is_folded(&review, "deleted.txt", cx));
        assert!(is_folded(&review, "a.rs", cx));
    }

    #[gpui::test]
    async fn test_panel_toggles_selected_file(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        let editor = rhs_editor(&review, cx);
        cx.simulate_keystrokes("; h j r");
        assert!(is_reviewed(&review, "added.txt", cx));
        assert!(is_folded(&review, "added.txt", cx));
        // The panel doesn't move on, and the diff's cursor stays.
        assert_eq!(path_at_cursor(&review, cx).as_deref(), Some("a.rs"));
        assert_eq!(cursor_line(&editor, cx), "line 1");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.rows_for_test()[1], ">added.txt A ✓");
        });
        cx.update(|window, cx| assert!(panel.focus_handle(cx).contains_focused(window, cx)));
        cx.simulate_keystrokes("r");
        assert!(!is_reviewed(&review, "added.txt", cx));
        assert!(!is_folded(&review, "added.txt", cx));
    }
}
