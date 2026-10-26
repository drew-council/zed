use super::{CommentBlocks, CommentTarget, SelectedLines, Side, lines_in_rows};
use crate::{AddComment, context_menu::selected_rows};
use buffer_diff::DiffHunkStatusKind;
use editor::{
    Bias, DisplayPoint, Editor, GutterRow, SelectionEffects, ToPoint as _, display_map::DisplayRow,
};
use gpui::{Entity, Focusable as _, MouseButton, WeakEntity};
use language::Point;
use multi_buffer::{MultiBufferRow, RowInfo};
use std::{
    cell::RefCell,
    ops::{Range, RangeInclusive},
    rc::Rc,
};
use ui::{IconButtonShape, Tooltip, prelude::*};
use util::ResultExt as _;

/// The rows of a drag from a "+" that a side's selection was last set to.
type DragRows = Rc<RefCell<Option<RangeInclusive<DisplayRow>>>>;

/// A "+" in the gutter of a row, GitHub's way of commenting with the mouse.
struct GutterButton {
    row: DisplayRow,
    is_lhs: bool,
    /// Why the review can't take a comment on the row, which disables the button.
    refusal: Option<SharedString>,
    /// The row is in the side's selection of several rows, which a click comments on.
    in_selection: bool,
}

/// The rows of a side's selection, and the row its cursor is on.
struct SelectionRows {
    rows: RangeInclusive<u32>,
    cursor: u32,
}

/// Draws a "+" in the gutter of one side of the diff: on the row under the pointer, and on the
/// cursor's row and the selection's last row while the side has the cursor. A click comments on
/// the row (or on the selection it is in); a drag from it selects rows, which its release
/// comments on. Clicks on a row's line number select the row (see [`select_from_line_number`]).
pub(super) fn install(editor: &Entity<Editor>, blocks: WeakEntity<CommentBlocks>, cx: &mut App) {
    let weak_editor = editor.downgrade();
    let drag_rows = DragRows::default();
    editor.update(cx, |editor, cx| {
        editor.set_gutter_row_renderer(
            Some(Rc::new(move |gutter_row, window, cx| {
                let editor = weak_editor.upgrade()?;
                follow_drag(&editor, &drag_rows, gutter_row.drag.as_ref(), window, cx);
                let button = blocks
                    .upgrade()?
                    .read(cx)
                    .gutter_button(&editor, gutter_row, cx)
                    .map(|button| render_button(button, &blocks, &editor, &drag_rows, cx));
                Some(render_row(gutter_row.row, button, &editor, &drag_rows))
            })),
            cx,
        );
    });
}

/// How far the area over a row's line number reaches from the start of the line-number column
/// (where the "+" is): past the line numbers of any length. Its click handler only takes clicks
/// on the line numbers, and lets the others through.
const LINE_NUMBER_AREA_WIDTH: Rems = Rems(6.);

/// What the review draws over a row's line number: the "+" when the row has one, in an area
/// that takes the clicks on the line number.
fn render_row(
    row: DisplayRow,
    button: Option<AnyElement>,
    editor: &Entity<Editor>,
    drag_rows: &DragRows,
) -> AnyElement {
    let editor = editor.downgrade();
    let drag_rows = drag_rows.clone();
    h_flex()
        .h_full()
        .w(LINE_NUMBER_AREA_WIDTH)
        .items_center()
        .debug_selector(move || format!("review-line-number-{}", row.0))
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            if window.default_prevented() {
                return;
            }
            let Some(editor) = editor.upgrade() else {
                return;
            };
            let on_line_number = editor
                .update(cx, |editor, cx| line_number_column(editor, window, cx))
                .is_some_and(|column| column.contains(&event.position.x));
            if !on_line_number {
                return;
            }
            // In a multibuffer, the editor opens the file at a clicked line number.
            window.prevent_default();
            cx.stop_propagation();
            select_from_line_number(&editor, row, event.modifiers.shift, &drag_rows, window, cx);
        })
        .children(button)
        .into_any_element()
}

/// Where a side's line numbers are drawn (window x coordinates, as last laid out): right-aligned
/// at the end of the gutter, before its right padding, as wide as the widest line number.
pub(super) fn line_number_column(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Range<Pixels>> {
    let origin = editor.last_bounds()?.origin.x;
    let style = editor.style(cx).clone();
    let font_id = window.text_system().resolve_font(&style.text.font());
    let font_size = style.text.font_size.to_pixels(window.rem_size());
    let snapshot = editor.snapshot(window, cx);
    let gutter = snapshot.gutter_dimensions(font_id, font_size, &style, window, cx);
    let end = origin + gutter.width - gutter.right_padding;
    let start = end - snapshot.max_line_number_width(&style, window);
    Some(start..end)
}

/// A click on a line number moves the cursor to its row (shift-click extends the selection
/// to it), and dragging from it selects rows, as on GitHub. `g space`, a header's Open File and the
/// review's context menu still open the file.
fn select_from_line_number(
    editor: &Entity<Editor>,
    row: DisplayRow,
    extend: bool,
    drag_rows: &DragRows,
    window: &mut Window,
    cx: &mut App,
) {
    window.focus(&editor.focus_handle(cx), cx);
    if extend {
        let rows = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selected = selected_rows(editor, &snapshot);
            let anchor_row = if editor.selections.newest_anchor().reversed {
                *selected.end()
            } else {
                *selected.start()
            };
            let anchor_row = snapshot
                .point_to_display_point(Point::new(anchor_row, 0), Bias::Left)
                .row();
            anchor_row.min(row)..=anchor_row.max(row)
        });
        select_display_rows(editor, rows, window, cx);
        return;
    }
    select_display_rows(editor, row..=row, window, cx);
    *drag_rows.borrow_mut() = Some(row..=row);
    // The rows follow the drag (`follow_drag`); its end only leaves them selected.
    editor.update(cx, |editor, cx| {
        editor.start_gutter_row_drag(row, Box::new(|_, _, _| {}), cx)
    });
}

impl CommentBlocks {
    fn gutter_button(
        &self,
        editor: &Entity<Editor>,
        gutter_row: &GutterRow,
        cx: &App,
    ) -> Option<GutterButton> {
        let splittable = self.diff.read(cx).editor().read(cx);
        let is_lhs = splittable.lhs_editor() == Some(editor);
        let selection =
            (splittable.focused_editor() == editor).then(|| selection_rows(editor.read(cx), cx));
        let row = gutter_row.info.multibuffer_row?.0;
        let at_selection = selection
            .as_ref()
            .is_some_and(|selection| selection.cursor == row || *selection.rows.end() == row);
        if !gutter_row.hovered && !at_selection {
            return None;
        }
        let selected_rows = selection.and_then(|selection| {
            (selection.rows.start() < selection.rows.end() && selection.rows.contains(&row))
                .then_some(selection.rows)
        });
        let in_selection = selected_rows.is_some();
        // A click there comments on the selection, so the selection's lines decide.
        let target = match selected_rows {
            Some(rows) => self.rows_target(editor.read(cx), is_lhs, rows, cx),
            None => self.row_target(is_lhs, gutter_row.info, cx),
        }?;
        Some(GutterButton {
            row: gutter_row.row,
            is_lhs,
            refusal: self.refusal(&target, cx),
            in_selection,
        })
    }

    /// The lines `rows` (multibuffer rows of a side of the diff) show, as `c` takes them.
    fn rows_target(
        &self,
        editor: &Editor,
        is_lhs: bool,
        rows: RangeInclusive<u32>,
        cx: &App,
    ) -> Option<CommentTarget> {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let SelectedLines::Lines {
            side,
            buffer_id,
            rows,
        } = lines_in_rows(&snapshot, rows, is_lhs)?
        else {
            return None;
        };
        let (path, _) = self.file_of_side_buffer(side, buffer_id, cx)?;
        Some(CommentTarget::Lines {
            path,
            side,
            lines: rows.start() + 1..=rows.end() + 1,
        })
    }

    /// The line a row of a side shows: on the left side and on the removed rows of a unified
    /// diff, a line of the old file.
    fn row_target(&self, is_lhs: bool, info: &RowInfo, cx: &App) -> Option<CommentTarget> {
        let is_removed = info
            .diff_status
            .is_some_and(|status| status.kind == DiffHunkStatusKind::Deleted);
        let side = if is_lhs || is_removed {
            Side::Old
        } else {
            Side::New
        };
        let (path, _) = self.file_of_side_buffer(side, info.buffer_id?, cx)?;
        let line = info.buffer_row? + 1;
        Some(CommentTarget::Lines {
            path,
            side,
            lines: line..=line,
        })
    }

    /// Why the review can't take a comment on `target`, if it can't.
    pub(crate) fn refusal(&self, target: &CommentTarget, cx: &App) -> Option<SharedString> {
        self.check(target, cx).err()
    }

    /// A "+" was pressed: the cursor goes to its row (unless the row is in the selection, which
    /// a click comments on), and a drag over rows begins.
    fn start_gutter_comment(
        &mut self,
        editor: &Entity<Editor>,
        row: DisplayRow,
        drag_rows: &DragRows,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&editor.focus_handle(cx), cx);
        let in_selection = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let rows = selected_rows(editor, &snapshot);
            let row = snapshot
                .display_point_to_point(DisplayPoint::new(row, 0), Bias::Left)
                .row;
            rows.start() < rows.end() && rows.contains(&row)
        });
        if !in_selection {
            select_display_rows(editor, row..=row, window, cx);
        }
        *drag_rows.borrow_mut() = Some(row..=row);
        let blocks = cx.weak_entity();
        let weak_editor = editor.downgrade();
        editor.update(cx, |editor, cx| {
            editor.start_gutter_row_drag(
                row,
                Box::new(move |rows, window, cx| {
                    let Some(editor) = weak_editor.upgrade() else {
                        return;
                    };
                    blocks
                        .update(cx, |blocks, cx| {
                            blocks.finish_gutter_comment(editor, rows, in_selection, window, cx)
                        })
                        .log_err();
                }),
                cx,
            );
        });
    }

    /// The "+" was released: a click comments on its row (or on the selection it is in), a drag
    /// on the rows it went over.
    fn finish_gutter_comment(
        &mut self,
        editor: Entity<Editor>,
        rows: RangeInclusive<DisplayRow>,
        in_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            if in_selection && rows.start() == rows.end() {
                return selected_rows(editor, &snapshot);
            }
            let buffer_row = |row: DisplayRow| {
                snapshot
                    .display_point_to_point(DisplayPoint::new(row, 0), Bias::Left)
                    .row
            };
            buffer_row(*rows.start())..=buffer_row(*rows.end())
        });
        self.add_line_comment_on(editor, Some(rows), window, cx);
    }

    /// The refusal a "+" on `row` (a display row of `editor`) would show, if it showed.
    #[cfg(test)]
    pub(crate) fn gutter_refusal(
        &self,
        editor: &Entity<Editor>,
        row: u32,
        cx: &mut App,
    ) -> Option<SharedString> {
        let is_lhs = self.lhs_editor(cx).as_ref() == Some(editor);
        let info = editor.update(cx, |editor, cx| {
            editor
                .display_snapshot(cx)
                .row_infos(DisplayRow(row))
                .next()
        })?;
        let target = self.row_target(is_lhs, &info, cx)?;
        self.refusal(&target, cx)
    }
}

fn selection_rows(editor: &Editor, cx: &App) -> SelectionRows {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let selection = editor.selections.newest_anchor();
    let start = selection.start.to_point(&snapshot);
    let end = selection.end.to_point(&snapshot);
    // A selection of whole lines ends at the start of the next one.
    let last = if end.row > start.row && end.column == 0 {
        end.row - 1
    } else {
        end.row
    };
    SelectionRows {
        rows: start.row..=last,
        cursor: if selection.reversed { start.row } else { last },
    }
}

/// Shows a drag from a "+" as the side's selection, once the editor
/// that calls the renderer is drawn.
fn follow_drag(
    editor: &Entity<Editor>,
    drag_rows: &DragRows,
    drag: Option<&RangeInclusive<DisplayRow>>,
    window: &mut Window,
    cx: &mut App,
) {
    if drag_rows.borrow().as_ref() == drag {
        return;
    }
    *drag_rows.borrow_mut() = drag.cloned();
    let Some(rows) = drag.cloned() else {
        return;
    };
    let editor = editor.downgrade();
    window.defer(cx, move |window, cx| {
        if let Some(editor) = editor.upgrade() {
            select_display_rows(&editor, rows, window, cx);
        }
    });
}

/// Selects whole rows, as dragging over line numbers does; a single row gets the cursor.
fn select_display_rows(
    editor: &Entity<Editor>,
    rows: RangeInclusive<DisplayRow>,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        let buffer_row = |row: DisplayRow| {
            snapshot
                .display_point_to_point(DisplayPoint::new(row, 0), Bias::Left)
                .row
        };
        let (start, end) = (buffer_row(*rows.start()), buffer_row(*rows.end()));
        let buffer = snapshot.buffer_snapshot();
        let range = if start == end {
            Point::new(start, 0)..Point::new(start, 0)
        } else if end < buffer.max_row().0 {
            Point::new(start, 0)..Point::new(end + 1, 0)
        } else {
            Point::new(start, 0)..Point::new(end, buffer.line_len(MultiBufferRow(end)))
        };
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([range])
        });
    });
}

fn render_button(
    button: GutterButton,
    blocks: &WeakEntity<CommentBlocks>,
    editor: &Entity<Editor>,
    drag_rows: &DragRows,
    cx: &App,
) -> AnyElement {
    let GutterButton {
        row,
        is_lhs,
        refusal,
        in_selection,
    } = button;
    let refused = refusal.is_some();
    let selector = format!(
        "review-gutter-comment{}-{}-{}",
        if refused { "-refused" } else { "" },
        if is_lhs { "lhs" } else { "rhs" },
        row.0
    );
    let focus_handle = editor.focus_handle(cx);
    let colors = cx.theme().colors();
    let tooltip = move |_: &mut Window, cx: &mut App| match &refusal {
        Some(reason) => Tooltip::simple(reason.clone(), cx),
        None => Tooltip::with_meta_in(
            if in_selection {
                "Comment on the Selected Lines"
            } else {
                "Add a Comment"
            },
            Some(&AddComment),
            "Drag to comment on several lines",
            &focus_handle,
            cx,
        ),
    };
    let icon = IconButton::new(("review-gutter-comment", row.0 as usize), IconName::Plus)
        .shape(IconButtonShape::Square)
        .icon_size(IconSize::XSmall)
        .map(|icon| {
            if refused {
                icon.style(ButtonStyle::Subtle).disabled(true)
            } else {
                icon.style(ButtonStyle::Transparent)
                    .icon_color(Color::Custom(colors.editor_background))
            }
        })
        .tooltip(tooltip);
    let blocks = blocks.clone();
    let editor = editor.downgrade();
    let drag_rows = drag_rows.clone();
    div()
        .debug_selector(move || selector)
        .rounded_sm()
        // GitHub's solid "+", which stands out from the line numbers.
        .when(!refused, |button| button.bg(colors.text_accent))
        .child(icon)
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            // Shift-click extends the selection to the row, as on a line number.
            if event.modifiers.shift {
                return;
            }
            // Neither the gutter's own line selection nor an excerpt's expand button below (on
            // the first and last rows of excerpts) takes the click.
            window.prevent_default();
            cx.stop_propagation();
            if refused {
                return;
            }
            let Some(editor) = editor.upgrade() else {
                return;
            };
            blocks
                .update(cx, |blocks, cx| {
                    blocks.start_gutter_comment(&editor, row, &drag_rows, window, cx)
                })
                .log_err();
        })
        .into_any_element()
}
