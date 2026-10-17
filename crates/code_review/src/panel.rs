use crate::{FocusPanel, ReviewView, ToggleFocus, TogglePanel, view::focus_diff};
use anyhow::Result;
use gpui::{
    Action, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    WeakEntity,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const REVIEW_PANEL_KEY: &str = "ReviewPanel";

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<ReviewPanel>(window, cx);
    });
    workspace.register_action(|workspace, _: &FocusPanel, window, cx| {
        workspace.focus_panel::<ReviewPanel>(window, cx);
    });
    workspace.register_action(|workspace, _: &TogglePanel, window, cx| {
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
            return;
        };
        let left_dock = workspace.left_dock().read(cx);
        let is_visible = left_dock.is_open()
            && left_dock
                .visible_panel()
                .is_some_and(|visible| visible.panel_id() == panel.entity_id());
        if is_visible {
            let was_focused = panel.read(cx).focus_handle.contains_focused(window, cx);
            workspace.close_panel::<ReviewPanel>(window, cx);
            if was_focused {
                focus_diff(workspace, window, cx);
            }
        } else {
            workspace.open_panel::<ReviewPanel>(window, cx);
        }
    });
}

/// The review panel in the left dock. It follows the active review tab; the file tree and
/// comment list will live here.
pub struct ReviewPanel {
    focus_handle: FocusHandle,
    active_review: Option<WeakEntity<ReviewView>>,
    _workspace_subscription: Option<Subscription>,
}

impl ReviewPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        })
    }

    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_subscription = workspace.weak_handle().upgrade().map(|workspace| {
            cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &workspace::Event, _window, cx| {
                    this.handle_workspace_event(workspace, event, cx)
                },
            )
        });
        let active_review = workspace
            .active_item_as::<ReviewView>(cx)
            .or_else(|| workspace.items_of_type::<ReviewView>(cx).next());
        Self {
            focus_handle: cx.focus_handle(),
            active_review: active_review.map(|review| review.downgrade()),
            _workspace_subscription: workspace_subscription,
        }
    }

    /// The review the panel shows: the active review tab, or the last active one while another
    /// kind of tab is active.
    pub fn active_review(&self) -> Option<Entity<ReviewView>> {
        self.active_review.as_ref()?.upgrade()
    }

    fn handle_workspace_event(
        &mut self,
        workspace: &Entity<Workspace>,
        event: &workspace::Event,
        cx: &mut Context<Self>,
    ) {
        match event {
            workspace::Event::ActiveItemChanged => {
                if let Some(review) = workspace.read(cx).active_item_as::<ReviewView>(cx)
                    && self.active_review() != Some(review.clone())
                {
                    self.active_review = Some(review.downgrade());
                    cx.notify();
                }
            }
            workspace::Event::ItemRemoved { item_id } => {
                if self
                    .active_review
                    .as_ref()
                    .is_some_and(|review| review.entity_id() == *item_id)
                {
                    self.active_review = workspace
                        .read(cx)
                        .items_of_type::<ReviewView>(cx)
                        .find(|review| review.entity_id() != *item_id)
                        .map(|review| review.downgrade());
                    cx.notify();
                }
            }
            _ => {}
        }
    }
}

impl EventEmitter<PanelEvent> for ReviewPanel {}

impl Focusable for ReviewPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self
            .active_review()
            .map(|review| review.read(cx).target().title());
        v_flex()
            .key_context("ReviewPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .map(|panel| match title {
                Some(title) => panel.child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(Label::new(title)),
                ),
                None => panel
                    .items_center()
                    .justify_center()
                    .child(Label::new("No review open").color(Color::Muted)),
            })
    }
}

impl Panel for ReviewPanel {
    fn persistent_name() -> &'static str {
        "Review Panel"
    }

    fn panel_key() -> &'static str {
        REVIEW_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    // Docks move panels by re-reading `position` when settings change, and there is no
    // `code_review` setting to persist another position yet.
    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Left
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Review Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        4
    }
}
