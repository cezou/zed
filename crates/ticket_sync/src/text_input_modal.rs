use editor::Editor;
use gpui::{AppContext as _, DismissEvent, Entity, EventEmitter, Focusable, Styled};
use ui::{
    App, Button, Clickable, Context, DynamicSpacing, Headline, HeadlineSize, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StyledExt, Window, div, h_flex, v_flex,
};
use workspace::ModalView;

/// A minimal single-line text-input modal, for the handful of "ask the user
/// for one string" flows the tickets panel needs (Notion token, page id,
/// worktree name) that don't warrant a filterable `Picker`. Modeled on
/// `git_ui::askpass_modal::AskPassModal`.
pub struct TextInputModal {
    title: SharedString,
    editor: Entity<Editor>,
    on_confirm: Option<Box<dyn FnOnce(String, &mut Window, &mut App) + 'static>>,
}

impl EventEmitter<DismissEvent> for TextInputModal {}
impl ModalView for TextInputModal {}

impl Focusable for TextInputModal {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl TextInputModal {
    pub fn new(
        title: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        default_text: Option<SharedString>,
        masked: bool,
        on_confirm: impl FnOnce(String, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = placeholder.into();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(placeholder.as_ref(), window, cx);
            editor.set_masked(masked, cx);
            if let Some(default_text) = default_text {
                editor.set_text(default_text, window, cx);
            }
            editor
        });
        Self {
            title: title.into(),
            editor,
            on_confirm: Some(Box::new(on_confirm)),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(on_confirm) = self.on_confirm.take() {
            let text = self.editor.read(cx).text(cx);
            on_confirm(text, window, cx);
        }
        cx.emit(DismissEvent);
    }
}

impl Render for TextInputModal {
    fn render(&mut self, _window: &mut Window, cx: &mut ui::Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("TextInputModal")
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
                    .py_2()
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .px(DynamicSpacing::Base12.rems(cx))
                    .pb(DynamicSpacing::Base08.rems(cx))
                    .child(Button::new("cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(Button::new("confirm", "Confirm").on_click(
                        cx.listener(|this, _, window, cx| this.confirm(&menu::Confirm, window, cx)),
                    )),
            )
    }
}
