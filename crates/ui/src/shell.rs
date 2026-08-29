use gpui::{
    AnyElement, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, WindowControlArea, div,
    prelude::FluentBuilder as _, px,
};
use keiki_api::Client;
use keiki_model::{AgentStatus, AgentSummary, sort_agents};

use crate::app_menus;
use crate::icons::{self, icon};
use crate::theme::Theme;

pub struct Shell {
    _client: Client,
    agents: Vec<AgentSummary>,
    selected_agent_id: Option<String>,
    focus_handle: FocusHandle,
}

impl Shell {
    pub fn new(api_base_url: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            _client: Client::new(api_base_url),
            agents: Vec::new(),
            selected_agent_id: None,
            focus_handle,
        }
    }

    fn select_agent(&mut self, agent_id: String, cx: &mut Context<Self>) {
        self.selected_agent_id = Some(agent_id);
        cx.notify();
    }

    fn render_sidebar(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut agents = self.agents.clone();
        sort_agents(&mut agents);
        let agent_rows = agents.into_iter().map(|agent| {
            let id = agent.id.clone();
            let selected = self.selected_agent_id.as_deref() == Some(agent.id.as_str());
            div()
                .id(SharedString::from(format!("agent-{}", agent.id)))
                .h(px(44.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .rounded(px(8.0))
                .cursor_pointer()
                .when(selected, |row| row.bg(theme.glass_hover()))
                .hover(|row| row.bg(theme.glass_hover()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_agent(id.clone(), cx);
                }))
                .child(
                    div()
                        .size(px(8.0))
                        .rounded_full()
                        .bg(status_color(agent.status, theme)),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text)
                        .child(SharedString::from(agent.name)),
                )
        });

        div()
            .w(px(256.0))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.glass())
            .child(
                div()
                    .h(px(44.0))
                    .px(px(16.0))
                    .when(cfg!(target_os = "macos"), |header| header.pl(px(80.0)))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .window_control_area(WindowControlArea::Drag)
                    .child(icon(icons::BOT).size(px(17.0)).text_color(theme.text))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("Keiki"),
                    ),
            )
            .child(
                div()
                    .h(px(40.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text_muted)
                            .child("AGENTS"),
                    )
                    .child(
                        div()
                            .size(px(26.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .text_color(theme.text_muted)
                            .child(
                                icon(icons::PLUS)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(8.0))
                    .children(agent_rows)
                    .when(self.agents.is_empty(), |list| {
                        list.child(
                            div()
                                .px(px(8.0))
                                .py(px(12.0))
                                .text_size(crate::typography::ui_rems(12.0))
                                .line_height(px(18.0))
                                .text_color(theme.text_muted)
                                .child("Agent sync is not connected yet."),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_content(&self, theme: &Theme) -> AnyElement {
        let title = self
            .selected_agent_id
            .as_ref()
            .and_then(|selected| self.agents.iter().find(|agent| &agent.id == selected))
            .map(|agent| agent.name.as_str())
            .unwrap_or("Conversations");
        div()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .child(
                div()
                    .h(px(44.0))
                    .px(px(16.0))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .window_control_area(WindowControlArea::Drag)
                    .text_size(crate::typography::ui_rems(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(title.to_owned())),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .w(px(360.0))
                            .flex()
                            .flex_col()
                            .items_center()
                            .text_center()
                            .child(
                                div()
                                    .size(px(48.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(14.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.surface_card)
                                    .child(
                                        icon(icons::CHAT_ROUND_LINE)
                                            .size(px(22.0))
                                            .text_color(theme.text_muted),
                                    ),
                            )
                            .child(
                                div()
                                    .mt(px(16.0))
                                    .text_size(crate::typography::ui_rems(15.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("No conversation selected"),
                            )
                            .child(
                                div()
                                    .mt(px(6.0))
                                    .text_size(crate::typography::ui_rems(13.0))
                                    .line_height(px(19.0))
                                    .text_color(theme.text_muted)
                                    .child(
                                        "Connect your Keiki account to load agents and start live conversations.",
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        div()
            .size_full()
            .flex()
            .flex_row()
            .overflow_hidden()
            .bg(theme.bg)
            .text_color(theme.text)
            .font_family(theme.font_sans.clone())
            .track_focus(&self.focus_handle)
            .on_action(|_: &app_menus::Minimize, window, _| {
                window.minimize_window();
            })
            .on_action(|_: &app_menus::Zoom, window, _| {
                window.zoom_window();
            })
            .on_action(|_: &app_menus::CloseWindow, window, _| {
                window.remove_window();
            })
            .child(self.render_sidebar(&theme, cx))
            .child(self.render_content(&theme))
    }
}

fn status_color(status: AgentStatus, theme: &Theme) -> gpui::Hsla {
    match status {
        AgentStatus::NeedsAttention => theme.danger,
        AgentStatus::Running => theme.accent,
        AgentStatus::Idle => theme.text_muted,
        AgentStatus::Offline => theme.border,
    }
}
