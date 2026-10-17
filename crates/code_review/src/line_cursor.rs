use buffer_diff::DiffHunkStatusKind;
use editor::{
    Bias, DisplayPoint, Editor, EditorEvent, MultiBufferSnapshot, RowHighlightOptions,
    SplittableEditor, ToPoint as _,
    display_map::{DisplayRow, DisplaySnapshot},
};
use gpui::{Entity, EntityId, Hsla, Subscription, point};
use language::Point;
use multi_buffer::MultiBufferRow;
use std::ops::RangeInclusive;
use ui::prelude::*;

/// tuicr's horizontal scroll step, in columns.
pub(crate) const HORIZONTAL_SCROLL_COLUMNS: f64 = 4.0;

struct CursorRowContext;
struct CursorRowAdded;
struct CursorRowDeleted;

pub(crate) enum HorizontalScroll {
    Scrolled,
    AtLeftEdge,
}

/// The review tab's line cursor: Zed's caret and selections are hidden, and the rows of the
/// newest selection get a full-width highlight instead, mirrored on the other side of a split
/// diff. Vim keeps driving the (invisible) selections.
pub(crate) struct LineCursor {
    editor: Entity<SplittableEditor>,
    lhs_editor_id: Option<EntityId>,
    focused_lhs: bool,
    _subscriptions: Vec<Subscription>,
}

impl LineCursor {
    pub(crate) fn new(editor: Entity<SplittableEditor>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| match event {
                EditorEvent::SelectionsChanged { .. }
                | EditorEvent::BufferRangesUpdated { .. }
                | EditorEvent::BuffersRemoved { .. }
                | EditorEvent::BuffersEdited { .. }
                | EditorEvent::BufferFoldToggled { .. }
                | EditorEvent::BufferEdited => this.refresh(cx),
                // The side that gets the focus is only recorded while the window draws, when
                // the split editor's notifications don't reach observers: focusing the other
                // side directly (not with `ctrl-w`) would leave the highlight behind.
                EditorEvent::Focused => this.sync_split_state(cx),
                _ => {}
            }),
            cx.observe(&editor, |this, _, cx| this.sync_split_state(cx)),
        ];
        let rhs_editor = editor.read(cx).rhs_editor().clone();
        configure(&rhs_editor, cx);
        let mut this = Self {
            editor,
            lhs_editor_id: None,
            focused_lhs: false,
            _subscriptions: subscriptions,
        };
        this.sync_split_state(cx);
        this.refresh(cx);
        this
    }

    /// The left side is recreated on every split (including automatic ones when the pane gets
    /// wide enough), and the highlight moves with focus between the sides.
    fn sync_split_state(&mut self, cx: &mut Context<Self>) {
        let splittable = self.editor.read(cx);
        let rhs_editor = splittable.rhs_editor().clone();
        let lhs_editor = splittable.lhs_editor().cloned();
        let lhs_editor_id = lhs_editor.as_ref().map(|editor| editor.entity_id());
        let focused_lhs = lhs_editor
            .as_ref()
            .is_some_and(|editor| splittable.focused_editor() == editor);

        let mut changed = false;
        if lhs_editor_id != self.lhs_editor_id {
            self.lhs_editor_id = lhs_editor_id;
            if let Some(lhs_editor) = &lhs_editor {
                configure(lhs_editor, cx);
                face_cursor(&rhs_editor, lhs_editor, cx);
            }
            changed = true;
        }
        if focused_lhs != self.focused_lhs {
            self.focused_lhs = focused_lhs;
            changed = true;
        }
        if changed {
            self.refresh(cx);
        }
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let splittable = self.editor.read(cx);
        let focused = splittable.focused_editor().clone();
        let companion = if splittable.is_split() {
            if &focused == splittable.rhs_editor() {
                splittable.lhs_editor().cloned()
            } else {
                Some(splittable.rhs_editor().clone())
            }
        } else {
            None
        };

        let display_rows = focused.update(cx, |editor, cx| {
            // A rename editor turns local selections back on.
            editor.show_local_selections = false;
            let snapshot = editor.display_snapshot(cx);
            let rows = selected_rows(editor, &snapshot);
            paint_rows(editor, snapshot.buffer_snapshot(), rows.clone(), cx);
            let buffer = snapshot.buffer_snapshot();
            let end_row = *rows.end();
            let start = snapshot.point_to_display_point(Point::new(*rows.start(), 0), Bias::Left);
            let end = snapshot.point_to_display_point(
                Point::new(end_row, buffer.line_len(MultiBufferRow(end_row))),
                Bias::Left,
            );
            start.row()..=end.row()
        });

        // The sides of a split diff have aligned display rows (spacers balance them), so the
        // companion's rows are the real rows within the focused side's display rows. A row
        // facing a spacer has nothing to mirror.
        if let Some(companion) = companion {
            companion.update(cx, |editor, cx| {
                editor.show_local_selections = false;
                let snapshot = editor.display_snapshot(cx);
                match companion_rows(&snapshot, display_rows) {
                    Some(rows) => paint_rows(editor, snapshot.buffer_snapshot(), rows, cx),
                    None => {
                        clear_rows(editor);
                        cx.notify();
                    }
                }
            });
        }
    }

    /// Scrolls the focused side (and with it the other side, which shares the horizontal offset)
    /// by `columns`, negative to the left.
    pub(crate) fn scroll_horizontally(
        &self,
        columns: f64,
        window: &mut Window,
        cx: &mut App,
    ) -> HorizontalScroll {
        let editor = self.editor.read(cx).focused_editor().clone();
        editor.update(cx, |editor, cx| {
            // Reads are clamped to the widest line, so stepping from the read position never
            // accumulates scroll beyond the right edge.
            let position = editor.scroll_position(cx);
            if columns < 0. && position.x <= 0. {
                return HorizontalScroll::AtLeftEdge;
            }
            let x = (position.x + columns).max(0.);
            editor.set_scroll_position(point(x, position.y), window, cx);
            HorizontalScroll::Scrolled
        })
    }
}

/// Sets up a side of the review's diff (the left side on every split).
fn configure(editor: &Entity<Editor>, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        editor.show_local_selections = false;
        editor.set_forbid_horizontal_autoscroll(true);
        // The review's comment button takes the place of the gutter's hover button for
        // breakpoints and bookmarks.
        editor.set_show_breakpoints(false, cx);
        editor.set_show_bookmarks(false, cx);
        cx.notify();
    });
}

/// Puts a new left side's cursor on the row facing the right side's cursor, as the split
/// editor does whenever the right side's cursor moves. A new left side otherwise keeps its own
/// position until then (the end of the diff), and `ctrl-w h` would jump there. Changing the
/// selections directly has no selection effects, so nothing mirrors back onto the right side.
fn face_cursor(rhs_editor: &Entity<Editor>, lhs_editor: &Entity<Editor>, cx: &mut App) {
    let display_point = rhs_editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        let head = editor
            .selections
            .newest_anchor()
            .head()
            .to_point(snapshot.buffer_snapshot());
        snapshot.point_to_display_point(head, Bias::Right)
    });
    lhs_editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        let display_point = snapshot.clip_point(display_point, Bias::Right);
        let point = snapshot.display_point_to_point(display_point, Bias::Right);
        editor.selections.change_with(&snapshot, |selections| {
            selections.select_ranges([point..point])
        });
        cx.notify();
    });
}

/// The multibuffer rows spanned by the newest selection (whole lines in line-wise visual mode).
pub(crate) fn selected_rows(editor: &Editor, snapshot: &DisplaySnapshot) -> RangeInclusive<u32> {
    let selection = editor.selections.newest_adjusted(snapshot);
    let start = selection.start.row;
    let mut end = selection.end.row;
    if end > start && selection.end.column == 0 {
        end -= 1;
    }
    start..=end
}

fn companion_rows(
    snapshot: &DisplaySnapshot,
    display_rows: RangeInclusive<DisplayRow>,
) -> Option<RangeInclusive<u32>> {
    let max_row = snapshot.max_point().row();
    let mut real_rows = (display_rows.start().0..=display_rows.end().0)
        .map(DisplayRow)
        .filter(|row| *row <= max_row && !snapshot.is_block_line(*row));
    let first = real_rows.next()?;
    let last = real_rows.next_back().unwrap_or(first);
    let start = snapshot.display_point_to_point(DisplayPoint::new(first, 0), Bias::Left);
    let end = snapshot.display_point_to_point(DisplayPoint::new(last, 0), Bias::Left);
    Some(start.row..=end.row)
}

fn clear_rows(editor: &mut Editor) {
    editor.clear_row_highlights::<CursorRowContext>();
    editor.clear_row_highlights::<CursorRowAdded>();
    editor.clear_row_highlights::<CursorRowDeleted>();
}

/// Row highlights replace the diff background of their rows, so added and deleted rows get
/// their own cursor colors that keep the diff color visible underneath.
fn paint_rows(
    editor: &mut Editor,
    buffer: &MultiBufferSnapshot,
    rows: RangeInclusive<u32>,
    cx: &mut Context<Editor>,
) {
    clear_rows(editor);
    let max_row = buffer.max_row().0;
    let start_row = (*rows.start()).min(max_row);
    let end_row = (*rows.end()).min(max_row);

    let mut run: Option<(u32, Option<DiffHunkStatusKind>)> = None;
    let mut last_row = start_row;
    for (row, row_info) in (start_row..=end_row).zip(buffer.row_infos(MultiBufferRow(start_row))) {
        let kind = row_info.diff_status.map(|status| status.kind);
        match run {
            Some((_, run_kind)) if run_kind == kind => {}
            Some((run_start, run_kind)) => {
                highlight_run(editor, buffer, run_start..=row - 1, run_kind, cx);
                run = Some((row, kind));
            }
            None => run = Some((row, kind)),
        }
        last_row = row;
    }
    if let Some((run_start, run_kind)) = run {
        highlight_run(editor, buffer, run_start..=last_row, run_kind, cx);
    }
    cx.notify();
}

fn highlight_run(
    editor: &mut Editor,
    buffer: &MultiBufferSnapshot,
    rows: RangeInclusive<u32>,
    kind: Option<DiffHunkStatusKind>,
    cx: &mut Context<Editor>,
) {
    let start = buffer.anchor_before(Point::new(*rows.start(), 0));
    let end_row = *rows.end();
    let end = buffer.anchor_before(Point::new(
        end_row,
        buffer.line_len(MultiBufferRow(end_row)),
    ));
    // An empty last row ends at column 0, which would exclude it; reaching into the next row
    // instead would also paint the spacers and blocks before that row.
    let options = RowHighlightOptions {
        autoscroll: false,
        include_gutter: true,
        include_end_row: true,
    };
    match kind {
        Some(DiffHunkStatusKind::Added) => {
            editor.highlight_rows::<CursorRowAdded>(start..end, added_row_color, options, cx)
        }
        Some(DiffHunkStatusKind::Deleted) => {
            editor.highlight_rows::<CursorRowDeleted>(start..end, deleted_row_color, options, cx)
        }
        Some(DiffHunkStatusKind::Modified) | None => {
            editor.highlight_rows::<CursorRowContext>(start..end, context_row_color, options, cx)
        }
    }
}

fn cursor_overlay(cx: &App) -> Hsla {
    cx.theme().players().local().selection
}

fn context_row_color(cx: &App) -> Hsla {
    cx.theme()
        .colors()
        .editor_background
        .blend(cursor_overlay(cx))
}

fn added_row_color(cx: &App) -> Hsla {
    let colors = cx.theme().colors();
    colors
        .editor_background
        .blend(colors.editor_diff_hunk_added_background)
        .blend(cursor_overlay(cx))
}

fn deleted_row_color(cx: &App) -> Hsla {
    let colors = cx.theme().colors();
    colors
        .editor_background
        .blend(colors.editor_diff_hunk_deleted_background)
        .blend(cursor_overlay(cx))
}

#[cfg(test)]
/// The highlighted cursor rows of `editor`, with the kind of row they were painted as.
pub(crate) fn cursor_rows(
    editor: &Entity<Editor>,
    cx: &mut gpui::VisualTestContext,
) -> Vec<(u32, &'static str)> {
    use editor::ToPoint as _;
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let mut rows = Vec::new();
        let highlights: [(&'static str, Vec<std::ops::Range<editor::Anchor>>); 3] = [
            (
                "context",
                editor
                    .highlighted_rows::<CursorRowContext>(cx)
                    .map(|(range, _)| range)
                    .collect(),
            ),
            (
                "added",
                editor
                    .highlighted_rows::<CursorRowAdded>(cx)
                    .map(|(range, _)| range)
                    .collect(),
            ),
            (
                "deleted",
                editor
                    .highlighted_rows::<CursorRowDeleted>(cx)
                    .map(|(range, _)| range)
                    .collect(),
            ),
        ];
        for (kind, ranges) in highlights {
            for range in ranges {
                let start = range.start.to_point(&snapshot).row;
                let end_row = range.end.to_point(&snapshot).row;
                rows.extend((start..=end_row).map(|row| (row, kind)));
            }
        }
        rows.sort();
        rows
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        add_review_panel, cursor_line, cursor_point, lhs_editor, move_to_row_with_text,
        open_review_fixture, rhs_editor, row_text, splittable,
    };
    use editor::{SelectionEffects, ToggleSplitDiff};
    use gpui::{Focusable as _, TestAppContext, VisualTestContext};
    use settings::DiffViewStyle;

    fn init(cx: &mut TestAppContext, diff_view_style: DiffViewStyle) {
        crate::test_support::init_vim_test(cx, diff_view_style);
    }

    fn scroll_x(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> f64 {
        editor.update(cx, |editor, cx| editor.scroll_position(cx).x)
    }

    /// The display rows the cursor highlight paints, and the display row of the cursor's line
    /// on that side.
    fn painted_rows(
        editor: &Entity<Editor>,
        row: u32,
        cx: &mut VisualTestContext,
    ) -> (Vec<DisplayRow>, DisplayRow) {
        use std::any::TypeId;
        let cursor_types = [
            TypeId::of::<CursorRowContext>(),
            TypeId::of::<CursorRowAdded>(),
            TypeId::of::<CursorRowDeleted>(),
        ];
        editor.update_in(cx, |editor, window, cx| {
            let painted = editor
                .highlighted_display_rows(window, cx)
                .into_iter()
                .filter(|(_, highlight)| {
                    highlight
                        .type_id
                        .is_some_and(|type_id| cursor_types.contains(&type_id))
                })
                .map(|(display_row, _)| display_row)
                .collect();
            let snapshot = editor.display_snapshot(cx);
            let cursor_row = snapshot
                .point_to_display_point(Point::new(row, 0), Bias::Left)
                .row();
            (painted, cursor_row)
        })
    }

    #[gpui::test]
    async fn test_empty_row_above_spacers_highlights_only_itself(cx: &mut TestAppContext) {
        use crate::test_support::{dot_git, open_workspace};
        use project::FakeFs;
        use util::path;

        init(cx, DiffViewStyle::Split);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            serde_json::json!({
                ".git": {},
                "a.txt": "one\n\nnew 1\nnew 2\ntwo\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(dot_git(), &[("a.txt", "one\n\ntwo\n".into())]);
        let (workspace, cx) = open_workspace(fs, cx).await;
        crate::test_support::dispatch(crate::ReviewUncommitted, cx);
        let review = crate::test_support::active_review(&workspace, cx);
        cx.focus(&review);
        cx.run_until_parked();
        let rhs = rhs_editor(&review, cx);
        let lhs = lhs_editor(&review, cx);

        // On the right side the empty row is followed by the added rows; on the left side by
        // the spacers facing them, which the highlight must not cover.
        let rhs_row = move_to_row_with_text(&rhs, "", cx);
        let (painted, cursor_row) = painted_rows(&rhs, rhs_row, cx);
        assert_eq!(painted, [cursor_row]);
        let lhs_rows = cursor_rows(&lhs, cx);
        assert_eq!(lhs_rows.len(), 1);
        let (painted, cursor_row) = painted_rows(&lhs, lhs_rows[0].0, cx);
        assert_eq!(painted, [cursor_row]);

        cx.simulate_keystrokes("ctrl-w h");
        let lhs_row = move_to_row_with_text(&lhs, "", cx);
        let (painted, cursor_row) = painted_rows(&lhs, lhs_row, cx);
        assert_eq!(painted, [cursor_row]);

        // A line-wise selection from the empty row on covers the spacers it spans.
        cx.simulate_keystrokes("shift-v j");
        let (painted, cursor_row) = painted_rows(&lhs, lhs_row, cx);
        assert_eq!(painted.first(), Some(&cursor_row));
        assert!(painted.len() > 2, "{painted:?}");
    }

    #[gpui::test]
    async fn test_line_cursor_highlights_row_without_caret(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let editor = rhs_editor(&review, cx);

        editor.read_with(cx, |editor, _| assert!(!editor.show_local_selections));
        assert_eq!(cursor_point(&editor, cx), Point::new(0, 0));
        assert_eq!(cursor_rows(&editor, cx), [(0, "context")]);

        cx.simulate_keystrokes("j");
        assert_eq!(cursor_rows(&editor, cx), [(1, "deleted")]);
        assert_eq!(row_text(&editor, 1, cx), "line 2");
        cx.simulate_keystrokes("j");
        assert_eq!(cursor_rows(&editor, cx), [(2, "added")]);
        assert_eq!(row_text(&editor, 2, cx), "LINE 2");
        cx.simulate_keystrokes("k k");
        assert_eq!(cursor_rows(&editor, cx), [(0, "context")]);
    }

    #[gpui::test]
    async fn test_character_motions_do_nothing(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let editor = rhs_editor(&review, cx);

        let row = move_to_row_with_text(&editor, "line 4", cx);
        for keystrokes in [
            "w",
            "b",
            "shift-w",
            "shift-e",
            "$",
            "^",
            "space",
            "backspace",
            "(",
            ")",
            "%",
            "f",
            "t",
            "; ,",
            "end",
        ] {
            cx.simulate_keystrokes(keystrokes);
            assert_eq!(
                cursor_point(&editor, cx),
                Point::new(row, 0),
                "after {keystrokes:?}"
            );
            assert_eq!(cursor_rows(&editor, cx), [(row, "context")]);
        }

        // Editing keys are stopped by the read-only diff (they may still move the cursor).
        let text = editor.read_with(cx, |editor, cx| editor.text(cx));
        for keystrokes in [
            "x", "shift-x", "p", "shift-p", "u", "d d", "shift-d", "shift-j", "~", "> >", "< <",
        ] {
            cx.simulate_keystrokes(keystrokes);
            editor.read_with(cx, |editor, cx| assert_eq!(editor.text(cx), text));
        }
        move_to_row_with_text(&editor, "line 4", cx);

        // Counts (including a 0 digit) and `0` still work.
        cx.simulate_keystrokes("1 0 j");
        assert_eq!(cursor_point(&editor, cx).row, row + 10);
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(row, 3)..Point::new(row, 3)])
            });
        });
        cx.simulate_keystrokes("0");
        assert_eq!(cursor_point(&editor, cx), Point::new(row, 0));
    }

    #[gpui::test]
    async fn test_insert_keys_are_noops(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let editor = rhs_editor(&review, cx);
        let text = editor.read_with(cx, |editor, cx| editor.text(cx));

        // `r` and `shift-r` are the review's reviewed marks, `c` and `shift-c` add comments and
        // `i` edits the comment below the cursor line (there is none).
        for keystrokes in [
            "i", "a", "shift-i", "shift-a", "o", "shift-o", "s", "shift-s", "g i", "insert",
        ] {
            let row = cursor_point(&editor, cx).row;
            cx.simulate_keystrokes(keystrokes);
            // Still in normal mode: `j` moves the cursor instead of typing.
            cx.simulate_keystrokes("j");
            assert_eq!(
                cursor_point(&editor, cx).row,
                row + 1,
                "after {keystrokes:?}"
            );
        }
        editor.read_with(cx, |editor, cx| assert_eq!(editor.text(cx), text));
    }

    #[gpui::test]
    async fn test_visual_mode_is_line_wise(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Unified);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let editor = rhs_editor(&review, cx);

        for start_visual in ["v", "shift-v", "ctrl-v"] {
            cx.simulate_keystrokes(start_visual);
            cx.simulate_keystrokes("j j");
            editor.read_with(cx, |editor, _| assert!(editor.selections.line_mode()));
            assert_eq!(
                cursor_rows(&editor, cx),
                [(0, "context"), (1, "deleted"), (2, "added")],
                "after {start_visual:?}"
            );

            cx.simulate_keystrokes("escape");
            assert_eq!(cursor_rows(&editor, cx), [(2, "added")]);
            cx.simulate_keystrokes("k k");
        }

        cx.simulate_keystrokes("v j v");
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert!(!editor.selections.line_mode());
            assert!(editor.selections.newest::<Point>(&snapshot).is_empty());
        });
        assert_eq!(cursor_rows(&editor, cx), [(1, "deleted")]);
    }

    #[gpui::test]
    async fn test_split_mirrors_cursor_row(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Split);
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let splittable = splittable(&review, cx);
        assert!(splittable.read_with(cx, |editor, _| editor.is_split()));
        let rhs = rhs_editor(&review, cx);
        let lhs = lhs_editor(&review, cx);
        lhs.read_with(cx, |editor, _| assert!(!editor.show_local_selections));

        // A modified row faces its old version.
        let rhs_row = move_to_row_with_text(&rhs, "LINE 2", cx);
        assert_eq!(cursor_rows(&rhs, cx), [(rhs_row, "added")]);
        let lhs_rows = cursor_rows(&lhs, cx);
        assert_eq!(lhs_rows.len(), 1);
        assert_eq!(row_text(&lhs, lhs_rows[0].0, cx), "line 2");
        assert_eq!(lhs_rows[0].1, "deleted");

        // A pure addition faces a spacer.
        let rhs_row = move_to_row_with_text(&rhs, "four", cx);
        assert_eq!(cursor_rows(&rhs, cx), [(rhs_row, "added")]);
        assert_eq!(cursor_rows(&lhs, cx), []);

        // Moving to the left side moves the highlight with it.
        move_to_row_with_text(&rhs, "two", cx);
        cx.simulate_keystrokes("ctrl-w h");
        assert!(splittable.read_with(cx, |editor, _| editor.focused_editor() == &lhs));
        let lhs_rows = cursor_rows(&lhs, cx);
        assert_eq!(lhs_rows.len(), 1);
        assert_eq!(row_text(&lhs, lhs_rows[0].0, cx), "two");
        cx.simulate_keystrokes("k");
        let lhs_rows = cursor_rows(&lhs, cx);
        assert_eq!(row_text(&lhs, lhs_rows[0].0, cx), "one");
        assert_eq!(lhs_rows[0].1, "deleted");
        let rhs_rows = cursor_rows(&rhs, cx);
        assert_eq!(rhs_rows.len(), 1);
        assert_eq!(row_text(&rhs, rhs_rows[0].0, cx), "ONE");

        // So does focusing a side directly, as moving to a comment does (on a row facing a
        // spacer, where the right side's cursor can't follow).
        cx.simulate_keystrokes("ctrl-w l");
        move_to_row_with_text(&rhs, "three", cx);
        move_to_row_with_text(&lhs, "gone 2", cx);
        cx.update(|window, cx| window.focus(&lhs.focus_handle(cx), cx));
        cx.run_until_parked();
        let lhs_rows = cursor_rows(&lhs, cx);
        assert_eq!(lhs_rows.len(), 1);
        assert_eq!(row_text(&lhs, lhs_rows[0].0, cx), "gone 2");
        move_to_row_with_text(&rhs, "three", cx);
        cx.update(|window, cx| window.focus(&rhs.focus_handle(cx), cx));
        cx.run_until_parked();
        let rhs_rows = cursor_rows(&rhs, cx);
        assert_eq!(rhs_rows.len(), 1);
        assert_eq!(row_text(&rhs, rhs_rows[0].0, cx), "three");

        // The left side is recreated when the diff is split again.
        let rhs_row = move_to_row_with_text(&rhs, "three", cx);
        for _ in 0..2 {
            cx.update(|window, cx| {
                let focus_handle = rhs.focus_handle(cx);
                focus_handle.dispatch_action(&ToggleSplitDiff, window, cx);
            });
            cx.run_until_parked();
        }
        let new_lhs = lhs_editor(&review, cx);
        assert_ne!(new_lhs, lhs);
        new_lhs.read_with(cx, |editor, _| assert!(!editor.show_local_selections));
        assert_eq!(cursor_rows(&rhs, cx), [(rhs_row, "context")]);
        let lhs_rows = cursor_rows(&new_lhs, cx);
        assert_eq!(lhs_rows.len(), 1);
        assert_eq!(row_text(&new_lhs, lhs_rows[0].0, cx), "three");

        // Its own cursor starts on the facing row too, so `ctrl-w h` before any motion stays
        // there (V7.X2).
        let scroll_y = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.update(cx, |editor, cx| editor.scroll_position(cx).y)
        };
        let scroll_top = scroll_y(&rhs, cx);
        cx.simulate_keystrokes("ctrl-w h");
        assert!(splittable.read_with(cx, |editor, _| editor.focused_editor() == &new_lhs));
        assert_eq!(cursor_line(&new_lhs, cx), "three");
        assert_eq!(scroll_y(&new_lhs, cx), scroll_top);
        assert_eq!(cursor_point(&rhs, cx).row, rhs_row);

        // Likewise after the pane got too narrow for a split diff and wide again.
        cx.simulate_keystrokes("ctrl-w l");
        let wide = cx.update(|window, _| window.bounds().size);
        cx.simulate_resize(gpui::size(px(300.), wide.height));
        cx.run_until_parked();
        cx.simulate_resize(wide);
        cx.run_until_parked();
        let resized_lhs = lhs_editor(&review, cx);
        assert_ne!(resized_lhs, new_lhs);
        let scroll_top = scroll_y(&rhs, cx);
        cx.simulate_keystrokes("ctrl-w h");
        assert!(splittable.read_with(cx, |editor, _| editor.focused_editor() == &resized_lhs));
        assert_eq!(cursor_line(&resized_lhs, cx), "three");
        assert_eq!(scroll_y(&resized_lhs, cx), scroll_top);
        assert_eq!(cursor_point(&rhs, cx).row, rhs_row);
    }

    #[gpui::test]
    async fn test_horizontal_scroll(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Split);
        let (_fs, workspace, review, cx) = open_review_fixture(cx).await;
        let panel = add_review_panel(&workspace, cx);
        let rhs = rhs_editor(&review, cx);
        let lhs = lhs_editor(&review, cx);
        let panel_focused = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| panel.read(cx).focus_handle(cx).contains_focused(window, cx))
        };

        let row = move_to_row_with_text(&rhs, &"y".repeat(300), cx);
        cx.simulate_keystrokes("l");
        assert_eq!(scroll_x(&rhs, cx), 4.);
        assert_eq!(scroll_x(&lhs, cx), 4.);
        cx.simulate_keystrokes("right l");
        assert_eq!(scroll_x(&rhs, cx), 12.);

        // Vertical motions keep the horizontal scroll.
        cx.simulate_keystrokes("k j k");
        assert_eq!(scroll_x(&rhs, cx), 12.);
        assert_eq!(cursor_point(&rhs, cx).row, row - 1);

        // Counts don't apply and don't leak into the next motion.
        cx.simulate_keystrokes("3 l j");
        assert_eq!(scroll_x(&rhs, cx), 16.);
        assert_eq!(cursor_point(&rhs, cx).row, row);

        cx.simulate_keystrokes("h left h h");
        assert_eq!(scroll_x(&rhs, cx), 0.);
        assert!(!panel_focused(cx));

        // At the left edge, `h` focuses the panel, except in visual mode.
        cx.simulate_keystrokes("shift-v h");
        assert!(!panel_focused(cx));
        cx.simulate_keystrokes("escape h");
        assert!(panel_focused(cx));
    }

    #[gpui::test]
    async fn test_long_lines_scroll_instead_of_wrapping(cx: &mut TestAppContext) {
        init(cx, DiffViewStyle::Split);
        cx.update(|cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.all_languages.defaults.soft_wrap =
                        Some(language::language_settings::SoftWrap::EditorWidth);
                });
            });
        });
        let (_fs, _workspace, review, cx) = open_review_fixture(cx).await;
        let rhs = rhs_editor(&review, cx);
        let lhs = lhs_editor(&review, cx);
        let wraps = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |editor, cx| {
                !matches!(editor.soft_wrap_mode(cx), editor::SoftWrap::None)
            })
        };
        assert!(!wraps(&rhs, cx));
        assert!(!wraps(&lhs, cx));

        cx.update(|window, cx| {
            window.dispatch_action(Box::new(editor::actions::ToggleSoftWrap), cx)
        });
        cx.run_until_parked();
        assert!(wraps(&rhs, cx));
        assert!(wraps(&lhs, cx));

        cx.simulate_keystrokes("ctrl-w h");
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(editor::actions::ToggleSoftWrap), cx)
        });
        cx.run_until_parked();
        assert!(!wraps(&rhs, cx));
        assert!(!wraps(&lhs, cx));
    }
}
