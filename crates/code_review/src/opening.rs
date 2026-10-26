use crate::forge::{PullRequestStatus, PullRequestSummary};
use gpui::{EventEmitter, FocusHandle, Focusable};
use ui::{LoadingLabel, prelude::*};
use workspace::item::Item;

/// The tab a pull request of a stack takes while it is fetched and checked out, until its review
/// replaces it: what the stack already says about it at once, then the outcome.
pub(crate) struct OpeningReview {
    pull_request: PullRequestSummary,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
}

impl OpeningReview {
    pub(crate) fn new(pull_request: PullRequestSummary, cx: &mut Context<Self>) -> Self {
        Self {
            pull_request,
            error: None,
            focus_handle: cx.focus_handle(),
        }
    }

    #[cfg(test)]
    pub(crate) fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    pub(crate) fn fail(&mut self, error: SharedString, cx: &mut Context<Self>) {
        self.error = Some(error);
        cx.notify();
    }
}

impl EventEmitter<()> for OpeningReview {}

impl Focusable for OpeningReview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for OpeningReview {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTodo).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!(
            "PR #{}: {}",
            self.pull_request.number, self.pull_request.title
        )
        .into()
    }
}

impl Render for OpeningReview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pull_request = &self.pull_request;
        let state = match pull_request.status {
            PullRequestStatus::Open if pull_request.is_draft => "Draft",
            PullRequestStatus::Open => "Open",
            PullRequestStatus::Merged => "Merged",
            PullRequestStatus::Closed => "Closed",
        };
        let status = match &self.error {
            Some(error) => Label::new(error.clone())
                .color(Color::Error)
                .into_any_element(),
            None => LoadingLabel::new(format!("Checking out {}", pull_request.head_ref))
                .color(Color::Muted)
                .into_any_element(),
        };
        v_flex()
            .key_context("OpeningReview")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .gap_2()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(format!("#{}", pull_request.number))
                            .size(LabelSize::Large)
                            .color(Color::Muted),
                    )
                    .child(Label::new(pull_request.title.clone()).size(LabelSize::Large)),
            )
            .child(
                Label::new(format!(
                    "{state} · {} wants to merge {} into {}",
                    pull_request.author.login, pull_request.head_ref, pull_request.base_ref
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .debug_selector(|| "review-opening-status".into())
                    .child(status),
            )
    }
}
