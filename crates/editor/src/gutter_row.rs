use std::{
    ops::{Range, RangeInclusive},
    rc::Rc,
};

use gpui::{AnyElement, App, Context, Pixels, Window};
use multi_buffer::RowInfo;

use crate::{DisplayRow, Editor};

/// A row of an editor, as a [`GutterRowRenderer`] gets it.
pub struct GutterRow<'a> {
    pub row: DisplayRow,
    pub info: &'a RowInfo,
    /// The pointer is over the row, in the gutter or in the text.
    pub hovered: bool,
    /// The rows of the drag started by [`Editor::start_gutter_row_drag`], while its mouse button
    /// is held.
    pub drag: Option<RangeInclusive<DisplayRow>>,
    /// The gutter's right padding, between the line numbers and the text, as x offsets from the
    /// element's origin.
    pub right_padding: Range<Pixels>,
}

/// Returns the element to draw over a row's line number, if any. It's called outside of any
/// update of the editor, so it may read the editor.
pub type GutterRowRenderer = Rc<dyn Fn(&GutterRow, &mut Window, &mut App) -> Option<AnyElement>>;

/// Gets the rows of a gutter row drag once its mouse button is released.
pub type GutterRowDragEnd = Box<dyn FnOnce(RangeInclusive<DisplayRow>, &mut Window, &mut App)>;

pub(crate) struct GutterRowDrag {
    start: DisplayRow,
    current: DisplayRow,
    on_end: GutterRowDragEnd,
}

impl GutterRowDrag {
    pub(crate) fn rows(&self) -> RangeInclusive<DisplayRow> {
        self.start.min(self.current)..=self.start.max(self.current)
    }
}

impl Editor {
    /// Draws the elements `renderer` returns over the line numbers of the visible rows (every row
    /// with a buffer row: deleted rows and either side of a split diff included, soft-wrapped
    /// continuations and blocks not). While a renderer is set, the editor tracks the row under
    /// the pointer.
    pub fn set_gutter_row_renderer(
        &mut self,
        renderer: Option<GutterRowRenderer>,
        cx: &mut Context<Self>,
    ) {
        self.gutter_row_renderer = renderer;
        self.hovered_gutter_row = None;
        self.gutter_row_drag = None;
        cx.notify();
    }

    /// The row under the pointer (in the gutter or in the text, not over a block), while a
    /// gutter row renderer is set.
    pub fn hovered_gutter_row(&self) -> Option<DisplayRow> {
        self.hovered_gutter_row
    }

    /// Starts a drag over the rows at `row`, for a mouse down on a gutter row renderer's element
    /// (which should prevent the default, the gutter's line selection). Until the mouse button
    /// is released, renderers get the rows from `row` to the one under the pointer as their
    /// `drag`; then `on_end` gets them.
    pub fn start_gutter_row_drag(
        &mut self,
        row: DisplayRow,
        on_end: GutterRowDragEnd,
        cx: &mut Context<Self>,
    ) {
        self.gutter_row_drag = Some(GutterRowDrag {
            start: row,
            current: row,
            on_end,
        });
        cx.notify();
    }

    pub(crate) fn update_gutter_row_hover(
        &mut self,
        hovered: Option<DisplayRow>,
        nearest_row: DisplayRow,
        cx: &mut Context<Self>,
    ) {
        let mut changed = hovered != self.hovered_gutter_row;
        self.hovered_gutter_row = hovered;
        if let Some(drag) = &mut self.gutter_row_drag
            && drag.current != nearest_row
        {
            drag.current = nearest_row;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Ends the gutter row drag, if any, at the mouse button's release.
    pub(crate) fn end_gutter_row_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.gutter_row_drag.take() else {
            return false;
        };
        let rows = drag.rows();
        let on_end = drag.on_end;
        // `on_end` may update this editor.
        window.defer(cx, move |window, cx| on_end(rows, window, cx));
        cx.notify();
        true
    }
}
