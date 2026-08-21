use crate::{
    Chip, CommonAnimationExt, Disclosure, GradientFade, HighlightedLabel, IconButtonShape, Tooltip,
    prelude::*,
};

use gpui::{ClickEvent, Hsla, MouseButton, SharedString, WindowBackgroundAppearance};
use std::sync::Arc;

/// How many agent sessions a ticket has, and whether any of them are still live.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TicketSessionState {
    #[default]
    NeverLaunched,
    Idle {
        total: usize,
    },
    Running {
        live: usize,
        total: usize,
    },
}

/// Maps a raw Notion status string onto a theme color.
pub fn ticket_status_color(status: &str) -> Color {
    let normalized = status.to_lowercase();
    if normalized.contains("prod") {
        Color::Error
    } else if normalized.contains("waiting") {
        Color::Warning
    } else if normalized.contains("review") {
        Color::Accent
    } else if normalized.contains("progress") {
        Color::Info
    } else {
        Color::Muted
    }
}

#[derive(IntoElement, RegisterComponent)]
pub struct TicketItem {
    id: ElementId,
    title: SharedString,
    issue_id: Option<SharedString>,
    status: Option<SharedString>,
    ticket_type: Option<SharedString>,
    session_state: TicketSessionState,
    worktree_label: Option<SharedString>,
    timestamp: SharedString,
    highlight_positions: Vec<usize>,
    expandable: bool,
    expanded: bool,
    selected: bool,
    focused: bool,
    hovered: bool,
    notified: bool,
    base_bg: Option<Hsla>,
    action_slot: Option<AnyElement>,
    on_toggle: Option<Arc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    on_hover: Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>,
}

impl TicketItem {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            issue_id: None,
            status: None,
            ticket_type: None,
            session_state: TicketSessionState::default(),
            worktree_label: None,
            timestamp: "".into(),
            highlight_positions: Vec::new(),
            expandable: false,
            expanded: false,
            selected: false,
            focused: false,
            hovered: false,
            notified: false,
            base_bg: None,
            action_slot: None,
            on_toggle: None,
            on_click: None,
            on_hover: Box::new(|_, _, _| {}),
        }
    }

    /// The human-readable ticket identifier, such as `CT-1487`.
    pub fn issue_id(mut self, issue_id: impl Into<SharedString>) -> Self {
        self.issue_id = Some(issue_id.into());
        self
    }

    /// The raw Notion status string; its color is derived via [`ticket_status_color`].
    pub fn status(mut self, status: impl Into<SharedString>) -> Self {
        self.status = Some(status.into());
        self
    }

    pub fn ticket_type(mut self, ticket_type: impl Into<SharedString>) -> Self {
        self.ticket_type = Some(ticket_type.into());
        self
    }

    pub fn session_state(mut self, state: TicketSessionState) -> Self {
        self.session_state = state;
        self
    }

    pub fn worktree_label(mut self, label: impl Into<SharedString>) -> Self {
        self.worktree_label = Some(label.into());
        self
    }

    pub fn timestamp(mut self, timestamp: impl Into<SharedString>) -> Self {
        self.timestamp = timestamp.into();
        self
    }

    /// Byte offsets within the title that a fuzzy search matched.
    pub fn highlight_positions(mut self, positions: Vec<usize>) -> Self {
        self.highlight_positions = positions;
        self
    }

    pub fn expandable(mut self, expandable: bool) -> Self {
        self.expandable = expandable;
        self
    }

    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = expanded;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    pub fn notified(mut self, notified: bool) -> Self {
        self.notified = notified;
        self
    }

    pub fn base_bg(mut self, bg: Hsla) -> Self {
        self.base_bg = Some(bg);
        self
    }

    pub fn action_slot(mut self, slot: impl IntoElement) -> Self {
        self.action_slot = Some(slot.into_any_element());
        self
    }

    pub fn on_toggle(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_toggle = Some(Arc::new(handler));
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    pub fn on_hover(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Box::new(handler);
        self
    }
}

impl RenderOnce for TicketItem {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Measured up front because it needs `&mut App`, which conflicts with the
        // theme borrow held below.
        let disclosure_size = IconSize::Small.square(window, cx);

        let color = cx.theme().colors();
        // The fade gradient paints a solid color over the title to blend it into
        // the row background, but a transparent window has no opaque surface to
        // fade into, so it renders as a visible patch; truncate the title instead.
        let opaque_window =
            cx.theme().window_background_appearance() == WindowBackgroundAppearance::Opaque;

        let sidebar_base_bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));
        let raw_bg = self.base_bg.unwrap_or(sidebar_base_bg);
        let running = matches!(self.session_state, TicketSessionState::Running { .. });
        let apparent_bg = {
            let bg = color.background.blend(raw_bg);
            if running {
                bg.blend(cx.theme().status().success.opacity(0.08))
            } else {
                bg
            }
        };

        let base_bg = if self.selected {
            apparent_bg.blend(color.element_active)
        } else {
            apparent_bg
        };

        let hover_color = color
            .element_active
            .blend(color.element_background.opacity(0.2));
        let hover_bg = apparent_bg.blend(hover_color);

        let gradient_overlay = GradientFade::new(base_bg, hover_bg, hover_bg)
            .width(px(64.0))
            .right(px(-10.0))
            .gradient_stop(0.7)
            .group_name("ticket-item");

        let separator_color = Color::Custom(color.text_muted.opacity(0.4));
        let dot_separator = || {
            Label::new("•")
                .size(LabelSize::Small)
                .color(separator_color)
        };

        let toggle_spacer = || div().w(disclosure_size).h(disclosure_size).flex_none();
        let toggle = if self.expandable {
            Disclosure::new(format!("ticket-toggle-{}", self.id), self.expanded)
                .shape(IconButtonShape::Square)
                .when_some(self.on_toggle, |this, on_toggle| {
                    this.on_toggle_expanded(on_toggle)
                })
                .into_any_element()
        } else {
            toggle_spacer().into_any_element()
        };

        let icon_slot = || h_flex().size_4().flex_none().justify_center();
        let session_state = self.session_state;
        let session_glyph = match session_state {
            TicketSessionState::Running { .. } => Icon::new(IconName::LoadCircle)
                .size(IconSize::Small)
                .color(Color::Success)
                // Keyed on the row id so sibling rows do not share a caller-location id.
                .with_keyed_rotate_animation(format!("ticket-spinner-{}", self.id), 2)
                .into_any_element(),
            _ if self.notified => Icon::new(IconName::Circle)
                .size(IconSize::Small)
                .color(Color::Accent)
                .into_any_element(),
            TicketSessionState::Idle { .. } => Icon::new(IconName::Terminal)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            TicketSessionState::NeverLaunched => Icon::new(IconName::Circle)
                .size(IconSize::Small)
                .color(Color::Custom(color.icon_muted.opacity(0.4)))
                .into_any_element(),
        };

        let session_tooltip: SharedString = match session_state {
            TicketSessionState::NeverLaunched => "No agent session yet".into(),
            TicketSessionState::Idle { total: 1 } => "1 session".into(),
            TicketSessionState::Idle { total } => format!("{total} sessions").into(),
            TicketSessionState::Running { live, total } => {
                format!("{live} of {total} sessions running").into()
            }
        };

        let session_icon = icon_slot()
            .id(SharedString::from(format!("ticket-session-{}", self.id)))
            .child(session_glyph)
            .tooltip(Tooltip::text(session_tooltip));

        let title_label = if self.highlight_positions.is_empty() {
            Label::new(self.title)
                .when(!opaque_window, |label| label.truncate())
                .into_any_element()
        } else {
            HighlightedLabel::new(self.title, self.highlight_positions)
                .when(!opaque_window, |label| label.truncate())
                .into_any_element()
        };

        let has_timestamp = !self.timestamp.is_empty();
        let has_leading_metadata = self.issue_id.is_some()
            || self.status.is_some()
            || self.ticket_type.is_some()
            || self.worktree_label.is_some();
        let has_metadata = has_leading_metadata || has_timestamp;

        let timestamp = self.timestamp;
        let metadata = h_flex()
            .min_w_0()
            .gap_1()
            .when_some(self.issue_id, |this, issue_id| {
                this.child(
                    h_flex()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(color.text_accent.opacity(0.1))
                        .child(
                            Label::new(issue_id)
                                .size(LabelSize::XSmall)
                                .color(Color::Accent)
                                .buffer_font(cx),
                        ),
                )
            })
            .when_some(self.status, |this, status| {
                let status_color = ticket_status_color(&status);
                this.child(
                    Chip::new(status)
                        .label_color(status_color)
                        .label_size(LabelSize::XSmall),
                )
            })
            .when_some(self.ticket_type, |this, ticket_type| {
                this.child(
                    Chip::new(ticket_type)
                        .label_color(Color::Muted)
                        .label_size(LabelSize::XSmall),
                )
            })
            .when_some(self.worktree_label, |this, label| {
                this.child(
                    h_flex()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            Icon::new(IconName::GitBranch)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(label)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
            })
            .when(has_timestamp, |this| {
                this.when(has_leading_metadata, |this| this.child(dot_separator()))
                    .child(
                        Label::new(timestamp)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
            });

        v_flex()
            .id(self.id.clone())
            .cursor_pointer()
            .group("ticket-item")
            .relative()
            .flex_shrink_0()
            .overflow_hidden()
            .w_full()
            .py_1()
            .px_1p5()
            .when(self.selected, |this| this.bg(color.element_active))
            .border_1()
            .border_color(gpui::transparent_black())
            .when(self.focused, |this| this.border_color(color.border_focused))
            .hover(|this| this.bg(hover_color))
            .on_hover(self.on_hover)
            .child(
                h_flex()
                    .min_w_0()
                    .w_full()
                    .h_6()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .id("content")
                            .min_w_0()
                            .flex_1()
                            .gap_1p5()
                            .child(toggle)
                            .child(session_icon)
                            .child(title_label),
                    )
                    .when(opaque_window, |this| this.child(gradient_overlay))
                    .when(self.hovered, |this| {
                        this.when_some(self.action_slot, |this, slot| {
                            this.child(
                                h_flex()
                                    .relative()
                                    .pr_1p5()
                                    .when(opaque_window, |this| {
                                        this.child(
                                            GradientFade::new(base_bg, hover_bg, hover_bg)
                                                .width(px(120.0))
                                                .right(px(8.))
                                                .gradient_stop(0.90)
                                                .group_name("ticket-item"),
                                        )
                                    })
                                    .child(slot)
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    }),
                            )
                        })
                    }),
            )
            .when(has_metadata, |this| {
                this.child(
                    h_flex()
                        .min_w_0()
                        .gap_1p5()
                        .child(toggle_spacer())
                        .child(icon_slot())
                        .child(metadata),
                )
            })
            .when_some(self.on_click, |this, on_click| this.on_click(on_click))
    }
}

impl Component for TicketItem {
    fn scope() -> ComponentScope {
        ComponentScope::Agent
    }

    fn description() -> &'static str {
        "A row representing a Notion ticket in a list, showing its title, agent session \
        state, and metadata such as issue id, status, type, worktree and timestamp."
    }

    fn preview(_window: &mut Window, cx: &mut App) -> AnyElement {
        let color = cx.theme().colors();
        let bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        let container = || {
            v_flex()
                .w_72()
                .border_1()
                .border_color(color.border_variant)
                .bg(bg)
        };

        let ticket_item_examples = vec![
            single_example(
                "Never Launched",
                container()
                    .child(
                        TicketItem::new("tk-1", "Fix fare grid pagination")
                            .issue_id("CT-1487")
                            .status("In Progress")
                            .timestamp("15m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Idle Sessions (Collapsed)",
                container()
                    .child(
                        TicketItem::new("tk-2", "Add competitor filter to tracking view")
                            .issue_id("CT-1492")
                            .status("In Review")
                            .session_state(TicketSessionState::Idle { total: 3 })
                            .expandable(true)
                            .worktree_label("ct-1492-competitor-filter")
                            .timestamp("2h"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Running (Expanded)",
                container()
                    .child(
                        TicketItem::new("tk-3", "Refactor pricing recommendation service")
                            .issue_id("CT-1500")
                            .status("In Progress")
                            .session_state(TicketSessionState::Running { live: 1, total: 2 })
                            .expandable(true)
                            .expanded(true)
                            .worktree_label("ct-1500-pricing-refactor")
                            .timestamp("3m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Long Title (truncation)",
                container()
                    .child(
                        TicketItem::new(
                            "tk-4",
                            "Investigate why the overnight tracking job silently drops fares \
                            for multi-leg journeys on weekends",
                        )
                        .issue_id("CT-1503")
                        .status("Waiting for Input")
                        .timestamp("1d"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "All Metadata",
                container()
                    .child(
                        TicketItem::new("tk-5", "Hotfix stale prod cache")
                            .issue_id("CT-1511")
                            .status("In Prod")
                            .ticket_type("Bug")
                            .session_state(TicketSessionState::Running { live: 2, total: 4 })
                            .expandable(true)
                            .expanded(true)
                            .worktree_label("ct-1511-hotfix")
                            .timestamp("5m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Search Highlights",
                container()
                    .child(
                        TicketItem::new("tk-6", "Cache invalidation on fare import")
                            .issue_id("CT-1520")
                            .highlight_positions(vec![0, 1, 2, 3, 4])
                            .status("Backlog")
                            .timestamp("2w"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Focused and Notified",
                container()
                    .child(
                        TicketItem::new("tk-7", "Review tariff grid export")
                            .issue_id("CT-1525")
                            .status("In Review")
                            .session_state(TicketSessionState::Idle { total: 1 })
                            .notified(true)
                            .focused(true)
                            .timestamp("4h"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Action Slot",
                container()
                    .child(
                        TicketItem::new("tk-8", "Hover to see action button")
                            .issue_id("CT-1530")
                            .status("In Progress")
                            .hovered(true)
                            .timestamp("6h")
                            .action_slot(
                                IconButton::new("launch", IconName::PlayOutlined)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted),
                            ),
                    )
                    .into_any_element(),
            ),
        ];

        example_group(ticket_item_examples)
            .vertical()
            .into_any_element()
    }
}
