//! The right pane's Conversations surface: every agent-to-agent thread of the
//! selected agent in one list — "[orb] Asker ↔ Agent [orb]" with the run
//! state — each row with Go to (select the thread) and End (block the asker
//! so the loop stops at its next ask).

use super::*;

struct PeerThreadRow {
    chat_id: String,
    title: String,
    peer: crate::keiki::PeerConversation,
    target_state: keiki_model::AvatarState,
    ago: String,
    ended: bool,
}

impl Shell {
    pub(super) fn render_peer_threads_surface(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = chrono::Utc::now();
        let rows: Vec<PeerThreadRow> = {
            let state = self.state.read(cx);
            let Some(space) = state.selected_space_row() else {
                return self.peer_threads_empty("Select an agent to see its conversations", &theme);
            };
            // Threads this agent is on either end of: the ones others opened
            // on it (its own chats) and the ones it opened on other agents
            // (chats of those agents' spaces, keyed by the asker id).
            let mut chats: Vec<(&zeron_proto::Chat, crate::keiki::PeerConversation)> = state
                .visible_chats()
                .filter_map(|chat| Some((chat, crate::keiki::peer_conversation(&chat.id)?)))
                .filter(|(chat, peer)| {
                    chat.space_id.as_deref() == Some(space.id.as_str())
                        || crate::keiki::agent_id(&peer.source_agent_id) == space.id
                })
                .collect();
            chats.sort_by_key(|(chat, _)| std::cmp::Reverse(chat.last_message_at));
            chats
                .into_iter()
                .map(|(chat, peer)| {
                    let status = state.display_status_for(chat, now);
                    let ago = chat
                        .last_message_at
                        .map(|at| zeron_proto::view::format_time_ago(at, now))
                        .unwrap_or_default();
                    let target = state
                        .space_for_chat(chat)
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "Agent".to_string());
                    PeerThreadRow {
                        chat_id: chat.id.clone(),
                        title: format!("{} ↔ {}", state.chat_title(chat), target),
                        peer,
                        target_state: crate::avatars::avatar_state(status),
                        ago,
                        ended: crate::keiki::peer_thread_ended(state, &chat.id),
                    }
                })
                .collect()
        };
        if rows.is_empty() {
            return self.peer_threads_empty("No agent-to-agent conversations yet", &theme);
        }
        let selected = self.state.read(cx).selected_chat.clone();
        let mut list = div().w_full().flex().flex_col().gap(px(6.0)).p(px(12.0));
        for (
            ix,
            PeerThreadRow {
                chat_id,
                title,
                peer,
                target_state,
                ago,
                ended,
            },
        ) in rows.into_iter().enumerate()
        {
            let is_selected = selected.as_deref() == Some(chat_id.as_str());
            let working = matches!(
                target_state,
                keiki_model::AvatarState::Thinking | keiki_model::AvatarState::Running
            );
            let source_orb = self.avatar_element(
                &peer.source_agent_id,
                format!("peer-thread-source-{ix}").into(),
                keiki_model::AvatarState::Idle,
                14.0,
                &theme,
                cx,
            );
            let target_orb = self.avatar_element(
                &peer.target_agent_id,
                format!("peer-thread-target-{ix}").into(),
                target_state,
                14.0,
                &theme,
                cx,
            );
            let state_label = if ended {
                "Ended"
            } else if working {
                "Working"
            } else {
                ""
            };
            let goto_id = chat_id.clone();
            let end_id = chat_id.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("peer-thread-{ix}")))
                    .w_full()
                    .px(px(12.0))
                    .py(px(10.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(if is_selected {
                        theme.border_strong
                    } else {
                        theme.border
                    })
                    .bg(crate::theme::ink(0.02))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .when(ended, |el| el.opacity(0.6))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(source_orb)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_size(crate::typography::ui_rems(13.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(title)),
                            )
                            .child(target_orb),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(
                                        [state_label, ago.as_str()]
                                            .iter()
                                            .filter(|s| !s.is_empty())
                                            .copied()
                                            .collect::<Vec<_>>()
                                            .join(" · "),
                                    )),
                            )
                            .child(
                                self.peer_thread_button(
                                    format!("peer-thread-goto-{ix}"),
                                    "Go to",
                                    false,
                                    &theme,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        let target = Some(goto_id.clone());
                                        this.state.update(cx, |s, cx| s.select_chat(target, cx));
                                    },
                                )),
                            )
                            .when(!ended, |el| {
                                el.child(
                                    self.peer_thread_button(
                                        format!("peer-thread-end-{ix}"),
                                        "End",
                                        true,
                                        &theme,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            crate::keiki::end_peer_thread(
                                                this.state.clone(),
                                                end_id.clone(),
                                                cx,
                                            );
                                        },
                                    )),
                                )
                            }),
                    ),
            );
        }
        div()
            .id("peer-threads-list")
            .size_full()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }

    fn peer_thread_button(
        &self,
        id: String,
        label: &'static str,
        danger: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        let text = if danger {
            theme.danger
        } else {
            theme.text_muted
        };
        div()
            .id(SharedString::from(id))
            .h(px(24.0))
            .px(px(10.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(text)
            .hover(|s| s.bg(crate::theme::ink(0.05)))
            .child(SharedString::from(label))
    }

    fn peer_threads_empty(&self, message: &'static str, theme: &Theme) -> AnyElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.0))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .child(SharedString::from(message))
            .into_any_element()
    }
}
