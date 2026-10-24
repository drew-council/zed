use std::cmp;

use collections::{HashMap, HashSet};
use gpui::{
    AbsoluteLength, AnyElement, App, AvailableSpace, Bounds, Context, DragMoveEvent, Element,
    Entity, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, Length,
    ParentElement, Pixels, StatefulInteractiveElement, Styled, TextStyleRefinement, Window, div,
    linear_color_stop, linear_gradient, point, px, size,
};
use multi_buffer::{Anchor, ExcerptBoundaryInfo};
use smallvec::smallvec;
use text::BufferId;
use theme::ActiveTheme;
use ui::{h_flex, prelude::*, v_flex};

use gpui::ContentMask;

use crate::{
    DisplayRow, Editor, EditorSnapshot, EditorStyle, FILE_HEADER_HEIGHT,
    MULTI_BUFFER_EXCERPT_HEADER_HEIGHT, RowExt, StickyHeaderExcerpt,
    display_map::Block,
    element::{EditorElement, SplitSide, header_jump_data, render_buffer_header},
    scroll::ScrollOffset,
    split::SplittableEditor,
};
use crate::{
    FocusedBlock,
    display_map::{BlockContext, BlockId, CustomBlockId, EditorMargins},
};

const RESIZE_HANDLE_WIDTH: f32 = 8.0;

#[derive(Debug, Clone)]
struct DraggedSplitHandle;

pub struct SplitEditorState {
    left_ratio: f32,
    visible_left_ratio: f32,
    cached_width: Pixels,
}

impl SplitEditorState {
    pub fn new(_cx: &mut App) -> Self {
        Self {
            left_ratio: 0.5,
            visible_left_ratio: 0.5,
            cached_width: px(0.),
        }
    }

    #[allow(clippy::misnamed_getters)]
    pub fn left_ratio(&self) -> f32 {
        self.visible_left_ratio
    }

    pub fn right_ratio(&self) -> f32 {
        1.0 - self.visible_left_ratio
    }

    fn on_drag_move(
        &mut self,
        drag_event: &DragMoveEvent<DraggedSplitHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let drag_position = drag_event.event.position;
        let bounds = drag_event.bounds;
        let bounds_width = bounds.right() - bounds.left();

        if bounds_width > px(0.) {
            self.cached_width = bounds_width;
        }

        let min_ratio = 0.1;
        let max_ratio = 0.9;

        let new_ratio = (drag_position.x - bounds.left()) / bounds_width;
        self.visible_left_ratio = new_ratio.clamp(min_ratio, max_ratio);
    }

    fn commit_ratio(&mut self) {
        self.left_ratio = self.visible_left_ratio;
    }

    fn on_double_click(&mut self) {
        self.left_ratio = 0.5;
        self.visible_left_ratio = 0.5;
    }
}

#[derive(IntoElement)]
pub struct SplitEditorView {
    splittable_editor: Entity<SplittableEditor>,
    style: EditorStyle,
    split_state: Entity<SplitEditorState>,
}

impl SplitEditorView {
    pub fn new(
        splittable_editor: Entity<SplittableEditor>,
        style: EditorStyle,
        split_state: Entity<SplitEditorState>,
    ) -> Self {
        Self {
            splittable_editor,
            style,
            split_state,
        }
    }
}

fn render_resize_handle(
    state: &Entity<SplitEditorState>,
    separator_color: Hsla,
    _window: &mut Window,
    _cx: &mut App,
) -> AnyElement {
    let state_for_click = state.clone();

    div()
        .id("split-resize-container")
        .relative()
        .h_full()
        .flex_shrink_0()
        .w(px(1.))
        .bg(separator_color)
        .child(
            div()
                .id("split-resize-handle")
                .absolute()
                .left(px(-RESIZE_HANDLE_WIDTH / 2.0))
                .w(px(RESIZE_HANDLE_WIDTH))
                .h_full()
                .cursor_col_resize()
                .block_mouse_except_scroll()
                .on_click(move |event, _, cx| {
                    if event.click_count() >= 2 {
                        state_for_click.update(cx, |state, _| {
                            state.on_double_click();
                        });
                    }
                    cx.stop_propagation();
                })
                .on_drag(DraggedSplitHandle, |_, _, _, cx| cx.new(|_| gpui::Empty)),
        )
        .into_any_element()
}

impl RenderOnce for SplitEditorView {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let splittable_editor = self.splittable_editor.read(cx);

        assert!(
            splittable_editor.lhs_editor().is_some(),
            "`SplitEditorView` requires `SplittableEditor` to be in split mode"
        );

        let lhs_editor = splittable_editor.lhs_editor().unwrap().clone();
        let rhs_editor = splittable_editor.rhs_editor().clone();

        let mut lhs = EditorElement::new(&lhs_editor, self.style.clone());
        let mut rhs = EditorElement::new(&rhs_editor, self.style.clone());

        lhs.set_split_side(SplitSide::Left);
        rhs.set_split_side(SplitSide::Right);

        let left_ratio = self.split_state.read(cx).left_ratio();
        let right_ratio = self.split_state.read(cx).right_ratio();

        let separator_color = cx.theme().colors().border_variant;

        let resize_handle = render_resize_handle(&self.split_state, separator_color, window, cx);

        let state_for_drag = self.split_state.downgrade();
        let state_for_drop = self.split_state.downgrade();

        let buffer_headers = SplitBufferHeadersElement::new(
            lhs_editor.clone(),
            rhs_editor.clone(),
            self.style.clone(),
        );

        let lhs_editor_for_order = lhs_editor;
        let rhs_editor_for_order = rhs_editor;

        div()
            .id("split-editor-view-container")
            .size_full()
            .relative()
            .child(
                h_flex()
                    .with_dynamic_prepaint_order(move |_window, cx| {
                        let lhs_needs = lhs_editor_for_order.read(cx).has_autoscroll_request();
                        let rhs_needs = rhs_editor_for_order.read(cx).has_autoscroll_request();
                        match (lhs_needs, rhs_needs) {
                            (false, true) => smallvec![2, 1, 0],
                            _ => smallvec![0, 1, 2],
                        }
                    })
                    .id("split-editor-view")
                    .size_full()
                    .on_drag_move::<DraggedSplitHandle>(move |event, window, cx| {
                        state_for_drag
                            .update(cx, |state, cx| {
                                state.on_drag_move(event, window, cx);
                            })
                            .ok();
                    })
                    .on_drop::<DraggedSplitHandle>(move |_, _, cx| {
                        state_for_drop
                            .update(cx, |state, _| {
                                state.commit_ratio();
                            })
                            .ok();
                    })
                    .child(
                        div()
                            .id("split-editor-left")
                            .flex_shrink_1()
                            .min_w_0()
                            .h_full()
                            .flex_basis(DefiniteLength::Fraction(left_ratio))
                            .overflow_hidden()
                            .child(lhs),
                    )
                    .child(resize_handle)
                    .child(
                        div()
                            .id("split-editor-right")
                            .flex_shrink_1()
                            .min_w_0()
                            .h_full()
                            .flex_basis(DefiniteLength::Fraction(right_ratio))
                            .overflow_hidden()
                            .child(rhs),
                    ),
            )
            .child(buffer_headers)
    }
}

struct SplitBufferHeadersElement {
    lhs_editor: Entity<Editor>,
    rhs_editor: Entity<Editor>,
    style: EditorStyle,
    global_id: Option<GlobalElementId>,
    /// The full-width block, laid out before the two sides prepaint.
    early_full_width_block: Option<AnyElement>,
}

impl SplitBufferHeadersElement {
    fn new(lhs_editor: Entity<Editor>, rhs_editor: Entity<Editor>, style: EditorStyle) -> Self {
        Self {
            lhs_editor,
            rhs_editor,
            style,
            global_id: None,
            early_full_width_block: None,
        }
    }
}

/// Where the full-width block was laid out in the last frame.
#[derive(Clone, Copy)]
struct FullWidthBlockFrame {
    available_width: Pixels,
    visible_height: Pixels,
}

struct BufferHeaderLayout {
    element: AnyElement,
}

struct SplitBufferHeadersPrepaintState {
    content_bounds: Bounds<Pixels>,
    full_width_block: Option<AnyElement>,
    sticky_header: Option<AnyElement>,
    non_sticky_headers: Vec<BufferHeaderLayout>,
}

impl IntoElement for SplitBufferHeadersElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SplitBufferHeadersElement {
    type RequestLayoutState = ();
    type PrepaintState = SplitBufferHeadersPrepaintState;

    fn id(&self) -> Option<gpui::ElementId> {
        Some("split-buffer-headers".into())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = gpui::Style::default();
        style.position = gpui::Position::Absolute;
        style.inset.top = DefiniteLength::Fraction(0.0).into();
        style.inset.left = DefiniteLength::Fraction(0.0).into();
        style.size.width = Length::Definite(DefiniteLength::Fraction(1.0));
        style.size.height = Length::Definite(DefiniteLength::Fraction(1.0));
        let layout_id = window.request_layout(style, [], cx);
        self.global_id = id.cloned();
        self.layout_full_width_block_early(window, cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return SplitBufferHeadersPrepaintState {
                content_bounds: bounds,
                full_width_block: None,
                sticky_header: None,
                non_sticky_headers: Vec::new(),
            };
        }

        let rem_size = self.rem_size();
        let text_style = TextStyleRefinement {
            font_size: Some(self.style.text.font_size),
            line_height: Some(self.style.text.line_height),
            ..Default::default()
        };

        window.with_rem_size(rem_size, |window| {
            window.with_text_style(Some(text_style), |window| {
                Self::prepaint_inner(self, bounds, window, cx)
            })
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let rem_size = self.rem_size();
        let text_style = TextStyleRefinement {
            font_size: Some(self.style.text.font_size),
            line_height: Some(self.style.text.line_height),
            ..Default::default()
        };

        window.with_rem_size(rem_size, |window| {
            window.with_text_style(Some(text_style), |window| {
                window.with_content_mask(
                    Some(ContentMask {
                        bounds: prepaint.content_bounds,
                    }),
                    |window| {
                        if let Some(full_width_block) = &mut prepaint.full_width_block {
                            full_width_block.paint(window, cx);
                        }

                        for header_layout in &mut prepaint.non_sticky_headers {
                            header_layout.element.paint(window, cx);
                        }

                        if let Some(mut sticky_header) = prepaint.sticky_header.take() {
                            sticky_header.paint(window, cx);
                        }
                    },
                );
            });
        });
    }
}

impl SplitBufferHeadersElement {
    fn rem_size(&self) -> Option<Pixels> {
        match self.style.text.font_size {
            AbsoluteLength::Pixels(pixels) => {
                let rem_size_scale = {
                    let default_font_size_scale = 14. / ui::BASE_REM_SIZE_IN_PX;
                    let default_font_size_delta = 1. - default_font_size_scale;
                    1. + default_font_size_delta
                };

                Some(pixels * rem_size_scale)
            }
            AbsoluteLength::Rems(rems) => Some(rems.to_pixels(ui::BASE_REM_SIZE_IN_PX.into())),
        }
    }

    fn prepaint_inner(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> SplitBufferHeadersPrepaintState {
        let line_height = window.line_height();

        let snapshot = self
            .rhs_editor
            .update(cx, |editor, cx| editor.snapshot(window, cx));
        let scroll_position = snapshot.scroll_position();

        // Compute right margin to avoid overlapping the scrollbar
        let content_bounds = self.content_bounds(bounds, cx);
        let available_width = (content_bounds.size.width
            - self.rhs_editor.read(cx).last_right_margin())
        .max(Pixels::ZERO);

        let visible_height_in_lines = content_bounds.size.height / line_height;
        let max_row = snapshot.max_point().row();
        let start_row = cmp::min(DisplayRow(scroll_position.y.floor() as u32), max_row);
        let end_row = cmp::min(
            (scroll_position.y + visible_height_in_lines as f64).ceil() as u32,
            max_row.next_row().0,
        );
        let end_row = DisplayRow(end_row);

        let full_width_block = self.build_full_width_block(
            &snapshot,
            scroll_position,
            content_bounds,
            available_width,
            line_height,
            start_row..end_row,
            window,
            cx,
        );

        let (selected_buffer_ids, latest_selection_anchors) =
            self.compute_selection_info(&snapshot, cx);

        let sticky_header = if snapshot.buffer_snapshot().show_headers() {
            snapshot
                .sticky_header_excerpt(scroll_position.y)
                .map(|sticky_excerpt| {
                    self.build_sticky_header(
                        sticky_excerpt,
                        &snapshot,
                        scroll_position,
                        content_bounds,
                        available_width,
                        line_height,
                        &selected_buffer_ids,
                        &latest_selection_anchors,
                        start_row,
                        end_row,
                        window,
                        cx,
                    )
                })
        } else {
            None
        };

        let sticky_header_excerpt_id = snapshot
            .sticky_header_excerpt(scroll_position.y)
            .map(|e| e.excerpt);

        let non_sticky_headers = self.build_non_sticky_headers(
            &snapshot,
            scroll_position,
            content_bounds,
            available_width,
            line_height,
            start_row,
            end_row,
            &selected_buffer_ids,
            &latest_selection_anchors,
            sticky_header_excerpt_id,
            window,
            cx,
        );

        SplitBufferHeadersPrepaintState {
            content_bounds,
            full_width_block,
            sticky_header,
            non_sticky_headers,
        }
    }

    fn content_bounds(&self, bounds: Bounds<Pixels>, cx: &App) -> Bounds<Pixels> {
        // Left hand side and right hand side horizontal scrollbars are
        // independent, so we clip the bottom if either is visible.
        let horizontal_scrollbar_height =
            (self.lhs_editor.read(cx).last_horizontal_scrollbar_visible()
                || self.rhs_editor.read(cx).last_horizontal_scrollbar_visible())
            .then_some(self.style.scrollbar_width)
            .unwrap_or(Pixels::ZERO);

        Bounds::new(
            bounds.origin,
            size(
                bounds.size.width,
                (bounds.size.height - horizontal_scrollbar_height).max(Pixels::ZERO),
            ),
        )
    }

    fn compute_selection_info(
        &self,
        snapshot: &EditorSnapshot,
        cx: &App,
    ) -> (HashSet<BufferId>, HashMap<BufferId, Anchor>) {
        let editor = self.rhs_editor.read(cx);
        let all_selections = editor
            .selections
            .all::<crate::Point>(&snapshot.display_snapshot);
        let all_anchor_selections = editor.selections.all_anchors(&snapshot.display_snapshot);

        let mut selected_buffer_ids = HashSet::default();
        for selection in &all_selections {
            for buffer_id in snapshot
                .buffer_snapshot()
                .buffer_ids_for_range(selection.range())
            {
                selected_buffer_ids.insert(buffer_id);
            }
        }

        let mut anchors_by_buffer: HashMap<BufferId, (usize, Anchor)> = HashMap::default();
        for selection in all_anchor_selections.iter() {
            let head = selection.head();
            if let Some((text_anchor, _)) = snapshot.buffer_snapshot().anchor_to_buffer_anchor(head)
            {
                anchors_by_buffer
                    .entry(text_anchor.buffer_id)
                    .and_modify(|(latest_id, latest_anchor)| {
                        if selection.id > *latest_id {
                            *latest_id = selection.id;
                            *latest_anchor = head;
                        }
                    })
                    .or_insert((selection.id, head));
            }
        }
        let latest_selection_anchors = anchors_by_buffer
            .into_iter()
            .map(|(buffer_id, (_, anchor))| (buffer_id, anchor))
            .collect();

        (selected_buffer_ids, latest_selection_anchors)
    }

    fn build_sticky_header(
        &self,
        StickyHeaderExcerpt { excerpt }: StickyHeaderExcerpt<'_>,
        snapshot: &EditorSnapshot,
        scroll_position: gpui::Point<ScrollOffset>,
        bounds: Bounds<Pixels>,
        available_width: Pixels,
        line_height: Pixels,
        selected_buffer_ids: &HashSet<BufferId>,
        latest_selection_anchors: &HashMap<BufferId, Anchor>,
        start_row: DisplayRow,
        end_row: DisplayRow,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let jump_data = header_jump_data(
            snapshot,
            DisplayRow(scroll_position.y as u32),
            FILE_HEADER_HEIGHT + MULTI_BUFFER_EXCERPT_HEADER_HEIGHT,
            excerpt,
            latest_selection_anchors,
        );

        let editor_bg_color = cx.theme().colors().editor_background;
        let selected = selected_buffer_ids.contains(&excerpt.buffer_id());

        let mut header = v_flex()
            .id("sticky-buffer-header")
            .w(available_width)
            .relative()
            .child(
                div()
                    .w(available_width)
                    .h(FILE_HEADER_HEIGHT as f32 * line_height)
                    .bg(linear_gradient(
                        0.,
                        linear_color_stop(editor_bg_color.opacity(0.), 0.),
                        linear_color_stop(editor_bg_color, 0.6),
                    ))
                    .absolute()
                    .top_0(),
            )
            .child(
                render_buffer_header(
                    &self.rhs_editor,
                    excerpt,
                    false,
                    selected,
                    true,
                    jump_data,
                    window,
                    cx,
                )
                .into_any_element(),
            )
            .into_any_element();

        let mut origin = bounds.origin;

        for (block_row, block) in snapshot.blocks_in_range(start_row..end_row) {
            if !block.is_buffer_header() {
                continue;
            }

            if block_row.0 <= scroll_position.y as u32 {
                continue;
            }

            let max_row = block_row.0.saturating_sub(FILE_HEADER_HEIGHT);
            let offset = scroll_position.y - max_row as f64;

            if offset > 0.0 {
                origin.y -= Pixels::from(offset * f64::from(line_height));
            }
            break;
        }

        let available_size = size(
            AvailableSpace::Definite(available_width),
            AvailableSpace::MinContent,
        );

        Self::prepaint_header(&mut header, origin, available_size, bounds, window, cx);

        header
    }

    fn build_non_sticky_headers(
        &self,
        snapshot: &EditorSnapshot,
        scroll_position: gpui::Point<ScrollOffset>,
        bounds: Bounds<Pixels>,
        available_width: Pixels,
        line_height: Pixels,
        start_row: DisplayRow,
        end_row: DisplayRow,
        selected_buffer_ids: &HashSet<BufferId>,
        latest_selection_anchors: &HashMap<BufferId, Anchor>,
        sticky_header: Option<&ExcerptBoundaryInfo>,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<BufferHeaderLayout> {
        let mut headers = Vec::new();

        for (block_row, block) in snapshot.blocks_in_range(start_row..end_row) {
            let (excerpt, is_folded) = match block {
                Block::BufferHeader { excerpt, .. } => {
                    if sticky_header == Some(excerpt) {
                        continue;
                    }
                    (excerpt, false)
                }
                Block::FoldedBuffer { first_excerpt, .. } => (first_excerpt, true),
                // ExcerptBoundary is just a separator line, not a buffer header
                Block::ExcerptBoundary { .. } | Block::Custom(_) | Block::Spacer { .. } => continue,
            };

            let selected = selected_buffer_ids.contains(&excerpt.buffer_id());
            let jump_data = header_jump_data(
                snapshot,
                block_row,
                block.height(),
                excerpt,
                latest_selection_anchors,
            );

            let mut header = render_buffer_header(
                &self.rhs_editor,
                excerpt,
                is_folded,
                selected,
                false,
                jump_data,
                window,
                cx,
            )
            .into_any_element();

            let y_offset = (block_row.0 as f64 - scroll_position.y) * f64::from(line_height);
            let origin = point(bounds.origin.x, bounds.origin.y + Pixels::from(y_offset));

            let available_size = size(
                AvailableSpace::Definite(available_width),
                AvailableSpace::MinContent,
            );

            Self::prepaint_header(&mut header, origin, available_size, bounds, window, cx);

            headers.push(BufferHeaderLayout { element: header });
        }

        headers
    }

    /// The right side's full-width block (see [`Editor::set_full_width_block`]): drawn here,
    /// over both sides, while it is in view or holds the focus (below the view then, as the
    /// editor does with a focused block, so that the focus stays), and measured at this width.
    fn build_full_width_block(
        &mut self,
        snapshot: &EditorSnapshot,
        scroll_position: gpui::Point<ScrollOffset>,
        bounds: Bounds<Pixels>,
        available_width: Pixels,
        line_height: Pixels,
        rows: std::ops::Range<DisplayRow>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let early_element = self.early_full_width_block.take();
        let custom_block_id = self.rhs_editor.read(cx).full_width_block?;
        if let Some(global_id) = &self.global_id {
            let frame = FullWidthBlockFrame {
                available_width,
                visible_height: bounds.size.height,
            };
            window.with_element_state(global_id, |_: Option<Option<FullWidthBlockFrame>>, _| {
                ((), Some(frame))
            });
        }
        let block_id = BlockId::Custom(custom_block_id);
        // The right side, prepainted already, left its focused block for the next frame,
        // unless it is this one.
        let focused_block = self.rhs_editor.update(cx, |editor, _| {
            let focused_block = editor.take_focused_block();
            match focused_block {
                Some(focused_block) if focused_block.id == block_id => Some(focused_block),
                Some(focused_block) => {
                    editor.set_focused_block(focused_block);
                    None
                }
                None => None,
            }
        });
        let visible_row = snapshot
            .blocks_in_range(rows)
            .find(|(_, block)| block.id() == block_id)
            .map(|(row, _)| row);
        let has_focus = focused_block
            .and_then(|focused_block| focused_block.focus_handle.upgrade())
            .is_some_and(|focus_handle| focus_handle.contains_focused(window, cx));
        if visible_row.is_none() && !has_focus {
            return None;
        }
        let block = snapshot.block_for_id(block_id)?;
        let Block::Custom(custom) = &block else {
            return None;
        };

        let mut element = match early_element {
            Some(element) => element,
            None => {
                self.render_full_width_block(&block, available_width, line_height, window, cx)?
            }
        };
        let height_in_rows =
            Self::measure_full_width_block(&mut element, available_width, line_height, window, cx);
        if custom.height != Some(height_in_rows) {
            self.rhs_editor.update(cx, |editor, cx| {
                editor.resize_blocks(
                    HashMap::from_iter([(custom_block_id, height_in_rows)]),
                    None,
                    cx,
                )
            });
            // Both sides are prepainted already, with the block's previous height, and a
            // notification during a draw doesn't schedule another one: without a new frame,
            // the rows would only move at the next input event.
            window.request_animation_frame();
        }

        let origin = match visible_row {
            Some(row) => point(
                bounds.origin.x,
                bounds.origin.y
                    + Pixels::from((row.as_f64() - scroll_position.y) * f64::from(line_height)),
            ),
            None => point(bounds.origin.x, bounds.bottom()),
        };
        let available_size = size(
            AvailableSpace::Definite(available_width),
            AvailableSpace::Definite(height_in_rows as f32 * line_height),
        );
        let focus_handle = window.with_content_mask(Some(ContentMask { bounds }), |window| {
            element.prepaint_as_root(origin, available_size, window, cx)
        });
        if let Some(focus_handle) = focus_handle {
            self.rhs_editor.update(cx, |editor, _| {
                editor.set_focused_block(FocusedBlock {
                    id: block_id,
                    focus_handle: focus_handle.downgrade(),
                })
            });
        }
        Some(element)
    }

    /// Lays out the full-width block before the two sides prepaint (this element prepaints
    /// after them), at the last frame's width, when it is in view at the current scroll
    /// position: a change of its height (a section collapsed, a line typed into it) then
    /// moves this frame's rows instead of the next frame's.
    fn layout_full_width_block_early(&mut self, window: &mut Window, cx: &mut App) {
        let Some(global_id) = self.global_id.clone() else {
            return;
        };
        let Some(custom_block_id) = self.rhs_editor.read(cx).full_width_block else {
            return;
        };
        let Some(frame) = window.with_element_state(
            &global_id,
            |frame: Option<Option<FullWidthBlockFrame>>, _| {
                let frame = frame.flatten();
                (frame, frame)
            },
        ) else {
            return;
        };
        if frame.available_width <= Pixels::ZERO {
            return;
        }

        let rem_size = self.rem_size();
        let text_style = TextStyleRefinement {
            font_size: Some(self.style.text.font_size),
            line_height: Some(self.style.text.line_height),
            ..Default::default()
        };
        let element = window.with_rem_size(rem_size, |window| {
            window.with_text_style(Some(text_style), |window| {
                let line_height = window.line_height();
                let snapshot = self
                    .rhs_editor
                    .update(cx, |editor, cx| editor.snapshot(window, cx));
                let scroll_top = snapshot.scroll_position().y;
                let max_row = snapshot.max_point().row();
                let start_row = cmp::min(DisplayRow(scroll_top.floor() as u32), max_row);
                let end_row = cmp::min(
                    (scroll_top + f64::from(frame.visible_height / line_height)).ceil() as u32,
                    max_row.next_row().0,
                );
                let block_id = BlockId::Custom(custom_block_id);
                snapshot
                    .blocks_in_range(start_row..DisplayRow(end_row))
                    .find(|(_, block)| block.id() == block_id)?;
                let block = snapshot.block_for_id(block_id)?;
                let Block::Custom(custom) = &block else {
                    return None;
                };
                let mut element = self.render_full_width_block(
                    &block,
                    frame.available_width,
                    line_height,
                    window,
                    cx,
                )?;
                let height_in_rows = Self::measure_full_width_block(
                    &mut element,
                    frame.available_width,
                    line_height,
                    window,
                    cx,
                );
                if custom.height != Some(height_in_rows) {
                    self.rhs_editor.update(cx, |editor, cx| {
                        editor.resize_blocks(
                            HashMap::from_iter([(custom_block_id, height_in_rows)]),
                            None,
                            cx,
                        )
                    });
                }
                Some(element)
            })
        });
        self.early_full_width_block = element;
    }

    fn render_full_width_block(
        &self,
        block: &Block,
        available_width: Pixels,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let Block::Custom(custom) = block else {
            return None;
        };
        let rem_size = window.rem_size();
        let font_id = window.text_system().resolve_font(&self.style.text.font());
        let font_size = self.style.text.font_size.to_pixels(rem_size);
        let em_width = window
            .text_system()
            .em_width(font_id, font_size)
            .unwrap_or_default();
        let margins = EditorMargins::default();
        Some(
            div()
                .size_full()
                .child(custom.render(&mut BlockContext {
                    window,
                    app: cx,
                    anchor_x: Pixels::ZERO,
                    max_width: available_width,
                    margins: &margins,
                    em_width,
                    line_height,
                    block_id: block.id(),
                    height: custom.height.unwrap_or(1),
                    selected: false,
                    editor_style: &self.style,
                    indent_guide_padding: Pixels::ZERO,
                }))
                .into_any_element(),
        )
    }

    /// The block's height in rows, measured like the editor measures blocks: whole rows, at
    /// least one.
    fn measure_full_width_block(
        element: &mut AnyElement,
        available_width: Pixels,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> u32 {
        let content_size = element.layout_as_root(
            size(
                AvailableSpace::Definite(available_width),
                AvailableSpace::MinContent,
            ),
            window,
            cx,
        );
        ((content_size.height / line_height).ceil() as u32).max(1)
    }

    fn prepaint_header(
        header: &mut AnyElement,
        origin: gpui::Point<Pixels>,
        available_size: gpui::Size<AvailableSpace>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            header.prepaint_as_root(origin, available_size, window, cx);
        });
    }
}

impl Editor {
    /// When this editor is the right side of a split diff, draws its custom block `block`
    /// across both sides instead of on the right side only (over the left side's spacer for
    /// it), measured at that width: a block above the start of the multibuffer
    /// (`BlockPlacement::Above(Anchor::Min)`) as the document's header, say. Unsplit, and in
    /// other editors, the block is drawn as usual.
    pub fn set_full_width_block(&mut self, block: Option<CustomBlockId>, cx: &mut Context<Self>) {
        self.full_width_block = block;
        cx.notify();
    }
}
