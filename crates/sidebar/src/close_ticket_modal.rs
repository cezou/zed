use std::path::PathBuf;

use agent_ui::ticket_metadata_store::{self, WorktreeWorkStatus};
use gpui::{DismissEvent, EventEmitter, FocusHandle, Focusable, Styled, Task};
use ui::{
    App, Button, ButtonCommon, ButtonStyle, Checkbox, Clickable, Color, Context, DynamicSpacing,
    Headline, HeadlineSize, InteractiveElement, IntoElement, Label, LabelCommon, LabelSize,
    ParentElement, Render, SharedString, StyledExt, ToggleState, Window, div, h_flex, prelude::*,
    v_flex,
};
use workspace::ModalView;

/// The confirmation shown before a ticket's worktree is deleted.
///
/// A Zed modal rather than `window.prompt`, whose Windows backend is a native
/// message box: it renders outside the window, in the OS' style, and can only
/// carry the branch choice as a third button.
pub struct CloseTicketModal {
    title: SharedString,
    detail: SharedString,
    /// The branch the worktree is on. `Some` puts the "delete it too" checkbox
    /// on the modal; `None` is for a confirmation with no branch to offer (the
    /// force-remove retry).
    branch: Option<SharedString>,
    delete_branch: bool,
    confirm_label: SharedString,
    /// What the worktree still holds, once the background check answers. The
    /// modal opens without waiting for it, and the warning appears underneath
    /// when it lands.
    unsaved_work: Option<WorktreeWorkStatus>,
    focus_handle: FocusHandle,
    on_confirm: Option<Box<dyn FnOnce(bool, &mut Window, &mut App) + 'static>>,
    _work_status_task: Task<()>,
}

impl EventEmitter<DismissEvent> for CloseTicketModal {}
impl ModalView for CloseTicketModal {}

impl Focusable for CloseTicketModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl CloseTicketModal {
    pub fn new(
        title: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        branch: Option<SharedString>,
        confirm_label: impl Into<SharedString>,
        worktree_path: Option<PathBuf>,
        on_confirm: impl FnOnce(bool, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let work_status_task = match worktree_path {
            Some(worktree_path) => cx.spawn(async move |this, cx| {
                let work = cx
                    .background_spawn(async move {
                        ticket_metadata_store::worktree_work_status(&worktree_path).await
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if work.has_unsaved_work() {
                        this.unsaved_work = Some(work);
                        cx.notify();
                    }
                })
                .ok();
            }),
            None => Task::ready(()),
        };

        Self {
            title: title.into(),
            detail: detail.into(),
            // Pre-checked: a ticket being closed is work that is done with, and
            // its branch is what keeps showing up in every branch list after.
            delete_branch: branch.is_some(),
            branch,
            confirm_label: confirm_label.into(),
            unsaved_work: None,
            focus_handle: cx.focus_handle(),
            on_confirm: Some(Box::new(on_confirm)),
            _work_status_task: work_status_task,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(on_confirm) = self.on_confirm.take() {
            on_confirm(self.delete_branch, window, cx);
        }
        cx.emit(DismissEvent);
    }
}

impl Render for CloseTicketModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let confirm_label = self.confirm_label.clone();
        v_flex()
            .key_context("CloseTicketModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w_96()
            .child(
                h_flex()
                    .px(DynamicSpacing::Base12.rems(cx))
                    .pt(DynamicSpacing::Base08.rems(cx))
                    .pb(DynamicSpacing::Base04.rems(cx))
                    .child(Headline::new(self.title.clone()).size(HeadlineSize::XSmall)),
            )
            .child(
                div()
                    .px(DynamicSpacing::Base12.rems(cx))
                    .pb_2()
                    .child(Label::new(self.detail.clone()).color(Color::Muted)),
            )
            .when_some(self.unsaved_work, |this, work| {
                this.child(
                    div().px(DynamicSpacing::Base12.rems(cx)).pb_2().child(
                        Label::new(format!(
                            "It still holds {} uncommitted file(s) and {} unpushed commit(s); \
                             deleting the branch as well loses them.",
                            work.dirty_files, work.unpushed_commits
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                    ),
                )
            })
            .when_some(self.branch.clone(), |this, branch| {
                this.child(
                    h_flex().px(DynamicSpacing::Base12.rems(cx)).pb_2().child(
                        Checkbox::new("delete-branch", self.delete_branch.into())
                            .label(format!("Delete the branch {branch} too"))
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                                this.delete_branch = *state == ToggleState::Selected;
                                cx.notify();
                            })),
                    ),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .px(DynamicSpacing::Base12.rems(cx))
                    .pb(DynamicSpacing::Base08.rems(cx))
                    .child(Button::new("cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("confirm", confirm_label)
                            .style(ButtonStyle::Tinted(ui::TintColor::Error))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            })),
                    ),
            )
    }
}
