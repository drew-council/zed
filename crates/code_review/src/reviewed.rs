use crate::{ReviewView, ToggleFileReviewed, ToggleHunkReviewed};
use buffer_diff::{DiffHunkStatus, DiffHunkStatusKind};
use editor::{Anchor, DiffHunkRenderer, Editor, MultiBufferSnapshot, RowHighlightOptions};
use gpui::{Entity, Focusable as _, Hsla, MouseButton, WeakEntity};
use language::{BufferId, Point};
use multi_buffer::MultiBufferRow;
use std::ops::Range;
use ui::{Checkbox, ElevationIndex, KeyBinding, Tooltip, prelude::*};

struct ReviewedHunkAdded;
struct ReviewedHunkDeleted;

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

/// Hunk controls for the hunk at the cursor or under the mouse: its reviewed mark. A review has
/// no stage or restore controls, and draws staged and unstaged hunks alike.
pub(crate) struct ReviewHunkRenderer {
    pub(crate) view: WeakEntity<ReviewView>,
}

impl DiffHunkRenderer for ReviewHunkRenderer {
    fn render_hunk_controls(
        &self,
        row: u32,
        _status: &DiffHunkStatus,
        hunk_range: Range<Anchor>,
        _is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let Some(view) = self.view.upgrade() else {
            return gpui::Empty.into_any_element();
        };
        let Some((repo_path, hunk_key)) = view.read(cx).hunk_key_at(editor, hunk_range.start, cx)
        else {
            return gpui::Empty.into_any_element();
        };
        let reviewed = view
            .read(cx)
            .session()
            .read(cx)
            .is_hunk_reviewed(&repo_path, &hunk_key);
        let focus_handle = editor.focus_handle(cx);
        h_flex()
            .h(line_height)
            .mr_1()
            .px_0p5()
            .pb_1()
            .border_x_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_b_lg()
            .bg(cx.theme().colors().editor_background)
            .block_mouse_except_scroll()
            .shadow_md()
            .child(
                Button::new(
                    ("review-hunk", row as u64),
                    if reviewed {
                        "Reviewed"
                    } else {
                        "Mark Reviewed"
                    },
                )
                .when(reviewed, |button| {
                    button.start_icon(
                        Icon::new(IconName::Check)
                            .size(IconSize::Small)
                            .color(Color::Success),
                    )
                })
                .key_binding(KeyBinding::for_action_in(
                    &ToggleHunkReviewed,
                    &focus_handle,
                    cx,
                ))
                .on_click({
                    let view = view.downgrade();
                    move |_, _, cx| {
                        view.update(cx, |view, cx| {
                            view.toggle_hunk(&repo_path, hunk_key.clone(), cx)
                        })
                        .ok();
                    }
                }),
            )
            .into_any_element()
    }

    fn render_hunk_as_staged(&self, _: &DiffHunkStatus, _: &App) -> bool {
        false
    }
}

/// Draws the rows of reviewed hunks dimmed: faint versions of the diff colors replace the diff
/// backgrounds (row highlights win over them), and the gutter's diff bars keep showing the kind.
/// `rows` are multibuffer row ranges, end excluded.
pub(crate) fn paint_reviewed_hunks(
    editor: &mut Editor,
    rows: Vec<Range<u32>>,
    cx: &mut Context<Editor>,
) {
    editor.clear_row_highlights::<ReviewedHunkAdded>();
    editor.clear_row_highlights::<ReviewedHunkDeleted>();
    let buffer = editor.buffer().read(cx).snapshot(cx);
    for rows in rows {
        let mut run: Option<(u32, bool)> = None;
        for (row, row_info) in rows
            .clone()
            .zip(buffer.row_infos(MultiBufferRow(rows.start)))
        {
            let is_deleted = row_info
                .diff_status
                .is_some_and(|status| status.kind == DiffHunkStatusKind::Deleted);
            match run {
                Some((_, run_is_deleted)) if run_is_deleted == is_deleted => {}
                Some((start, run_is_deleted)) => {
                    highlight_run(editor, &buffer, start..row, run_is_deleted, cx);
                    run = Some((row, is_deleted));
                }
                None => run = Some((row, is_deleted)),
            }
        }
        if let Some((start, is_deleted)) = run {
            highlight_run(editor, &buffer, start..rows.end, is_deleted, cx);
        }
    }
    cx.notify();
}

fn highlight_run(
    editor: &mut Editor,
    buffer: &MultiBufferSnapshot,
    rows: Range<u32>,
    is_deleted: bool,
    cx: &mut Context<Editor>,
) {
    let start = buffer.anchor_before(Point::new(rows.start, 0));
    // A highlight ending at column 0 leaves that row out.
    let end = if rows.end <= buffer.max_row().0 {
        buffer.anchor_before(Point::new(rows.end, 0))
    } else {
        Anchor::Max
    };
    let options = RowHighlightOptions {
        autoscroll: false,
        include_gutter: false,
    };
    if is_deleted {
        editor.highlight_rows::<ReviewedHunkDeleted>(
            start..end,
            reviewed_deleted_color,
            options,
            cx,
        );
    } else {
        editor.highlight_rows::<ReviewedHunkAdded>(start..end, reviewed_added_color, options, cx);
    }
}

fn reviewed_added_color(cx: &App) -> Hsla {
    let colors = cx.theme().colors();
    colors
        .editor_background
        .blend(colors.editor_diff_hunk_added_background.opacity(0.4))
}

fn reviewed_deleted_color(cx: &App) -> Hsla {
    let colors = cx.theme().colors();
    colors
        .editor_background
        .blend(colors.editor_diff_hunk_deleted_background.opacity(0.4))
}

#[cfg(test)]
pub(crate) fn reviewed_rows_for_test(
    editor: &Entity<Editor>,
    cx: &mut gpui::VisualTestContext,
) -> Vec<u32> {
    use editor::ToPoint as _;
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let mut rows = editor
            .highlighted_rows::<ReviewedHunkAdded>(cx)
            .chain(editor.highlighted_rows::<ReviewedHunkDeleted>(cx))
            .flat_map(|(range, _)| {
                let start = range.start.to_point(&snapshot).row;
                let end = range.end.to_point(&snapshot);
                let end = if range.end.is_max() {
                    end.row + 1
                } else {
                    end.row
                };
                start..end
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows.dedup();
        rows
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ReviewUncommitted,
        test_support::{
            active_review, add_review_panel, cursor_line, dispatch, init_vim_test, is_folded,
            is_reviewed, lhs_editor, move_to_row_with_text, open_review_fixture, path_at_cursor,
            rhs_editor, row_text,
        },
    };
    use fs::Fs as _;
    use gpui::{TestAppContext, VisualTestContext};
    use settings::DiffViewStyle;
    use std::any::TypeId;
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
        fs.insert_file(
            path!("/project/a.rs"),
            a.replace("line 20\n", "LINE 20\n").into_bytes(),
        )
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
            [
                "added.txt",
                "long.txt",
                "modified.txt",
                "modified.txt",
                "added.txt",
                "long.txt"
            ]
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

    fn row_texts(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<String> {
        reviewed_rows_for_test(editor, cx)
            .into_iter()
            .map(|row| row_text(editor, row, cx))
            .collect()
    }

    #[gpui::test]
    async fn test_reviewed_hunks_are_dimmed(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Unified);
        let (fs, workspace, review, cx) = open_review_fixture(cx).await;
        let editor = rhs_editor(&review, cx);

        // Both the deleted and the added rows of the hunk.
        let row = move_to_row_with_text(&editor, "LINE 2", cx);
        cx.simulate_keystrokes("shift-r");
        assert_eq!(row_texts(&editor, cx), ["line 2", "LINE 2"]);
        assert_eq!(cursor_line(&editor, cx), "LINE 2");
        // The line cursor stays on top of the dimmed rows.
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                let highlights = editor.highlighted_display_rows(window, cx);
                let cursor_highlight = highlights
                    .values()
                    .filter(|highlight| {
                        highlight.type_id != Some(TypeId::of::<ReviewedHunkAdded>())
                            && highlight.type_id != Some(TypeId::of::<ReviewedHunkDeleted>())
                    })
                    .count();
                assert_eq!(cursor_highlight, 1, "row {row}");
            })
        });

        // The deleted row toggles it too; on a context row nothing happens but a toast.
        move_to_row_with_text(&editor, "line 2", cx);
        cx.simulate_keystrokes("shift-r");
        assert!(row_texts(&editor, cx).is_empty());
        cx.simulate_keystrokes("shift-r");
        assert_eq!(row_texts(&editor, cx), ["line 2", "LINE 2"]);
        move_to_row_with_text(&editor, "line 3", cx);
        cx.simulate_keystrokes("shift-r");
        assert_eq!(row_texts(&editor, cx), ["line 2", "LINE 2"]);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.notification_ids().len(), 1)
        });

        move_to_row_with_text(&editor, "LINE 35", cx);
        cx.simulate_keystrokes("shift-r");
        assert_eq!(
            row_texts(&editor, cx),
            ["line 2", "LINE 2", "line 35", "LINE 35"]
        );

        // A new hunk elsewhere keeps both marks; a changed hunk loses its own.
        let a = fs.load(path!("/project/a.rs").as_ref()).await.unwrap();
        let a = a.replace("line 20\n", "LINE 20\n");
        fs.insert_file(path!("/project/a.rs"), a.clone().into_bytes())
            .await;
        cx.run_until_parked();
        assert_eq!(
            row_texts(&editor, cx),
            ["line 2", "LINE 2", "line 35", "LINE 35"]
        );
        fs.insert_file(
            path!("/project/a.rs"),
            a.replace("LINE 35", "Line 35").into_bytes(),
        )
        .await;
        cx.run_until_parked();
        assert_eq!(row_texts(&editor, cx), ["line 2", "LINE 2"]);

        // Marks survive a reopened review.
        dispatch(workspace::CloseActiveItem::default(), cx);
        drop(review);
        cx.run_until_parked();
        dispatch(ReviewUncommitted, cx);
        let review = active_review(&workspace, cx);
        cx.run_until_parked();
        let editor = rhs_editor(&review, cx);
        assert_eq!(row_texts(&editor, cx), ["line 2", "LINE 2"]);
    }

    #[gpui::test]
    async fn test_reviewed_hunks_in_split_view(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Split);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let lhs = lhs_editor(&review, cx);

        move_to_row_with_text(&rhs, "LINE 2", cx);
        cx.simulate_keystrokes("shift-r");
        assert_eq!(row_texts(&rhs, cx), ["LINE 2"]);
        assert_eq!(row_texts(&lhs, cx), ["line 2"]);

        // A deletion is marked from the left side.
        cx.simulate_keystrokes("ctrl-w h");
        move_to_row_with_text(&lhs, "gone 1", cx);
        cx.simulate_keystrokes("shift-r");
        assert_eq!(row_texts(&lhs, cx), ["line 2", "gone 1", "gone 2"]);

        // The marks are repainted on a new left side.
        cx.simulate_keystrokes("ctrl-w l");
        for _ in 0..2 {
            cx.update(|window, cx| {
                let focus_handle = rhs.focus_handle(cx);
                focus_handle.dispatch_action(&editor::ToggleSplitDiff, window, cx);
            });
            cx.run_until_parked();
        }
        let new_lhs = lhs_editor(&review, cx);
        assert_ne!(new_lhs, lhs);
        assert_eq!(row_texts(&new_lhs, cx), ["line 2", "gone 1", "gone 2"]);
        assert_eq!(row_texts(&rhs, cx), ["LINE 2"]);
    }

    #[gpui::test]
    async fn test_folded_header_keeps_normal_mode_when_refocused(cx: &mut TestAppContext) {
        init_vim_test(cx, DiffViewStyle::Split);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        add_review_panel(&workspace, cx);
        let editor = rhs_editor(&review, cx);
        // The cursor stays on the last file's folded header.
        cx.simulate_keystrokes("] f ] f ] f ] f r");
        assert!(is_folded(&review, "modified.txt", cx));

        // Vim would enter visual mode here: the folded file is one big "character".
        cx.simulate_keystrokes("; h ; l");
        cx.update(|window, cx| assert!(editor.focus_handle(cx).is_focused(window)));
        editor.read_with(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let selection = editor.selections.newest_anchor();
            assert!(selection.start.cmp(&selection.end, &snapshot).is_eq());
        });
        // A folded file's hunks aren't marked from its header.
        cx.simulate_keystrokes("shift-r");
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.notification_ids().len(), 1)
        });
        cx.simulate_keystrokes("r");
        assert!(!is_reviewed(&review, "modified.txt", cx));
        assert_eq!(cursor_line(&editor, cx), "ONE");
        assert!(row_texts(&editor, cx).is_empty());
    }
}
