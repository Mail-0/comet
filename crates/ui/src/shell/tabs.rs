//! Session navigation — the horizontal tab strip is gone (wing 2026-08-10):
//! the activity sidebar IS the session list, and the titlebar names the
//! selected session (harness brand icon + title). A `+` Copilot-session button
//! lives in the titlebar's left control cluster while an existing session is
//! selected. `UiSettings.open_tabs` is legacy — no longer read or written.

use super::*;

/// The chat one step from `selected` in the sidebar `order`, wrapping at both
/// ends. Pure.
///
/// With nothing selected, cycling enters the list at the end it would have
/// wrapped to: the first row going forward, the last going back. A selection
/// that has since left the list (archived from another
/// device mid-cycle) is treated the same way rather than dead-ending.
pub(super) fn cycle_target(
    order: &[String],
    selected: Option<&str>,
    forward: bool,
) -> Option<String> {
    if order.is_empty() {
        return None;
    }
    let at = selected.and_then(|id| order.iter().position(|c| c == id));
    let next = match (at, forward) {
        (Some(at), true) => (at + 1) % order.len(),
        (Some(at), false) => (at + order.len() - 1) % order.len(),
        (None, true) => 0,
        (None, false) => order.len() - 1,
    };
    Some(order[next].clone())
}

pub(super) fn right_pane_expand_icon(expanded: bool) -> &'static str {
    if expanded {
        icons::COLLAPSE_ARROWS
    } else {
        icons::EXPAND_ARROWS
    }
}

impl Shell {
    /// Ctrl+Tab / Ctrl+Shift+Tab: step through the sidebar's Sessions list in
    /// the order it is drawn. Selection is immediate (no MRU overlay held open
    /// on the modifier) — one press, one session.
    ///
    /// Chat-scoped chrome, like the panel toggles: gpui dispatches a matched
    /// binding before any `on_key_down`, so an unscoped cycle would fire
    /// underneath Settings (yanking the user off the page mid-record, since
    /// these are the very keys the shortcuts table invites them to press) or
    /// underneath the add-space palette, stranding the overlay over a session
    /// they never picked.
    pub(super) fn cycle_session(&mut self, forward: bool, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.overlay_owns_keyboard(cx) {
            return;
        }
        // The same list `render_active_rows` draws and the jump shortcuts
        // count — one function, so neither can drift from the screen.
        let order = self.sidebar_visible_order(cx);
        let selected = self.state.read(cx).selected_chat.clone();
        if let Some(target) = cycle_target(&order, selected.as_deref(), forward) {
            self.open_chat(target, cx);
        }
    }

    /// Boot landing: the most recently active visible chat once the first
    /// chats frame has synced (manual selection wins; no chats → the
    /// empty state shows).
    pub(super) fn boot_select_chat(&mut self, cx: &mut Context<Self>) {
        let first = {
            let state = self.state.read(cx);
            if !state.chats_synced || state.selected_chat.is_some() || state.auto_selected {
                return;
            }
            state
                .overview_chats(Utc::now())
                .first()
                .map(|(_, c)| c.id.clone())
        };
        if let Some(first) = first {
            self.state
                .update(cx, |s, cx| s.select_chat(Some(first), cx));
        }
    }

    /// Open a session from the sidebar: select it, the main area follows.
    pub(super) fn open_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.state
            .update(cx, |s, cx| s.select_chat(Some(chat_id), cx));
        cx.notify();
    }

    /// Create a device-local Copilot session through the same workspace
    /// `CreateChat` mutation used by the composer.
    pub(super) fn new_copilot_chat(&mut self, cx: &mut Context<Self>) {
        let (has_credentials, engine, device_id) = {
            let state = self.state.read(cx);
            (
                state.keiki_token.is_some(),
                state.engine().cloned(),
                state
                    .local_device_id
                    .clone()
                    .unwrap_or_else(|| "local".to_string()),
            )
        };
        if !has_credentials {
            self.set_sidebar_notice("Sign in to Keiki to use Copilot");
            cx.notify();
            return;
        }
        let Some(engine) = engine else {
            self.set_sidebar_notice("Engine not connected");
            cx.notify();
            return;
        };
        let chat_id = uuid::Uuid::new_v4().to_string();
        self.route = Route::Chat;
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            let config = zeron_proto::ChatConfig {
                harness: zeron_proto::HarnessId::Copilot,
                model: Some("copilot".into()),
                reasoning: None,
                model_options: Default::default(),
                sandbox: zeron_proto::SandboxLevel::ReadOnly,
            };
            let params = serde_json::json!({
                "op": "createChat",
                "chatId": chat_id,
                "deviceId": device_id,
                "config": config,
            });
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => shell.open_chat(chat_id, cx),
                    Err(error) => {
                        shell.set_sidebar_notice(format!("Couldn't open Copilot: {error}"))
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// `+` in the titlebar and Ctrl+N create a fresh Copilot session.
    pub(super) fn open_new_session(&mut self, cx: &mut Context<Self>) {
        self.new_copilot_chat(cx);
    }

    /// The unified titlebar in chat mode:
    /// `[Copilot +] [harness icon + session title] … [toggle-changes]`.
    /// Replaces the tab strip; inherits its titlebar duties (drag region,
    /// animated left inset, the toggle-changes button on git projects).
    pub(super) fn render_session_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        // Nothing selected leaves the title empty; the empty state owns its
        // content while the bar keeps its height,
        // drag region, and buttons. A session appends its target as a muted
        // "project @ device" tag right of the title (the composer footer no
        // longer carries it).
        let (title, target, harness, on_canvas): (
            SharedString,
            Option<SharedString>,
            Option<zeron_proto::HarnessId>,
            bool,
        ) = {
            let state = self.state.read(cx);
            match state.selected_chat_row() {
                Some(chat) => {
                    let folder = chat
                        .space_id
                        .as_deref()
                        .and_then(|id| state.space_row(id))
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "~".to_string());
                    let device = state
                        .device_name(&chat.device_id)
                        .unwrap_or("Unknown device");
                    (
                        SharedString::from(transcript::single_line(
                            &chat.title.clone().unwrap_or_else(|| "New session".into()),
                        )),
                        Some(SharedString::from(format!("{folder} @ {device}"))),
                        chat.config.as_ref().map(|c| c.harness),
                        false,
                    )
                }
                None => (SharedString::from(""), None, None, true),
            }
        };
        let conversation_status = if on_canvas {
            None
        } else {
            self.state.read(cx).keiki_conversation().cloned()
        };
        let keiki_controls = conversation_status.as_ref().map(|conversation| {
            (
                crate::keiki::takeover_live(conversation),
                conversation.pending,
                self.composer.read(cx).has_steer_text(cx),
            )
        });

        // The titlebar `+` creates a Copilot session in the sidebar folder.
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let plus_inset = TITLEBAR_ACTION_SLOT_WIDTH * self.titlebar_plus_alpha(cx);

        // Same glide as the old strip: content starts at the inset card's
        // left edge while the sidebar is open, and slides toward the control
        // cluster as it collapses.
        let content_left =
            (sidebar_now + Theme::SPACE_LG).max(self.title_bar_content_start() + plus_inset);

        // Trailing titlebar section. With the changes pane open this is the
        // PANE'S HEADER — a strip exactly as wide as the pane carrying its
        // controls (scope dropdown, ref selector, fold-all from the Changes
        // entity; expand + close shell-side). It lives up here because the
        // titlebar overlay owns this band's hit-testing: controls mounted in
        // the pane itself would sit under the drag region and never see a
        // click. Closed, it is just the stable open/close toggle. Hidden when
        // no session is selected — nothing to diff yet.
        let takeover = !on_canvas && self.right_pane_open(cx) && self.right_pane_expanded;
        // In takeover the title hides and the strip owns the whole band, so
        // the row's left inset pulls back to the sidebar seam — the title
        // inset would push the scope dropdown off the pane's own left gutter
        // (user report: misaligned dead space). With the sidebar COLLAPSED
        // the seam is the window edge, where the traffic lights + nav
        // cluster overlay lives — the strip must still clear it, but only
        // just: `title_bar_content_start` carries the identity-group margin the
        // strip doesn't want (it brings its own 8px pad), and doubling up
        // read as a hole after the `+` (user report).
        let row_left = if takeover {
            // The surface tabs must LEFT-ALIGN with the pane's own rows (the
            // diff options and stats strip carry an 8px box gutter off the
            // seam — user report: rows started at different insets). The
            // strip's width is capped to `avail`, which subtracts the row's
            // 8px child gap — pulling row_left 8 LEFT of the seam cancels
            // that, so the uncapped strip starts exactly at the seam and its
            // own 8px pad lands the first chip on the pane gutter. The
            // window-control cluster still wins while the sidebar is
            // collapsed (the chips clear it instead of underlapping).
            let cluster_end =
                self.title_bar_content_start() - TITLEBAR_IDENTITY_GAP + plus_inset - 14.0;
            (sidebar_now - 8.0).max(cluster_end)
        } else {
            content_left
        };
        let trailing: Option<gpui::AnyElement> = if on_canvas {
            None
        } else {
            let right_open = self.right_pane_open(cx);
            let mut controls = div()
                .id("right-titlebar-controls")
                .flex_none()
                .h_full()
                .flex()
                .flex_row()
                .items_center();
            if !takeover && let Some((takeover_live, pending, has_steer_text)) = keiki_controls {
                let selected_chat = self.state.read(cx).selected_chat.clone();
                let takeover_pending = pending
                    == Some(crate::keiki::KeikiConversationPending::Takeover)
                    || pending == Some(crate::keiki::KeikiConversationPending::HandBack);
                let takeover_label = if takeover_live {
                    "Hand back"
                } else {
                    "Take over"
                };
                let takeover_text = if takeover_pending {
                    theme.text_muted
                } else {
                    motion::hover_blend("keiki-titlebar-takeover", theme.text_muted, theme.text)
                };
                let takeover_background = if takeover_pending {
                    gpui::transparent_black()
                } else {
                    motion::hover_blend(
                        "keiki-titlebar-takeover",
                        gpui::transparent_black(),
                        theme.glass_hover(),
                    )
                };
                let takeover_button = div()
                    .id("keiki-titlebar-takeover")
                    .h(px(28.0))
                    .px(px(8.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .line_height(px(16.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(takeover_text)
                    .bg(takeover_background)
                    .rounded(px(6.0))
                    .when(takeover_pending, |el| el.opacity(0.35))
                    .when(!takeover_pending, |el| {
                        el.cursor_pointer()
                            .on_hover(motion::hover_listener("keiki-titlebar-takeover"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.state.read(cx).selected_chat == selected_chat
                                    && let Some(conversation) =
                                        this.state.read(cx).keiki_conversation()
                                {
                                    if crate::keiki::takeover_live(conversation) {
                                        crate::keiki::hand_back(this.state.clone(), cx);
                                    } else {
                                        crate::keiki::take_over(this.state.clone(), cx);
                                    }
                                }
                            }))
                    })
                    .child(takeover_label);
                let steer_pending = pending == Some(crate::keiki::KeikiConversationPending::Steer);
                let steer_disabled = !has_steer_text || steer_pending;
                let steer_text = if steer_disabled {
                    theme.text_muted
                } else {
                    motion::hover_blend("keiki-titlebar-steer", theme.text_muted, theme.text)
                };
                let steer_background = if steer_disabled {
                    gpui::transparent_black()
                } else {
                    motion::hover_blend(
                        "keiki-titlebar-steer",
                        gpui::transparent_black(),
                        theme.glass_hover(),
                    )
                };
                let steer_button = div()
                    .id("keiki-titlebar-steer")
                    .h(px(28.0))
                    .px(px(8.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .line_height(px(16.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(steer_text)
                    .bg(steer_background)
                    .rounded(px(6.0))
                    .when(steer_disabled, |el| {
                        el.opacity(if steer_pending { 0.35 } else { 0.55 })
                    })
                    .when(!steer_pending, |el| {
                        el.cursor_pointer()
                            .on_hover(motion::hover_listener("keiki-titlebar-steer"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.composer.read(cx).has_steer_text(cx) {
                                    this.composer.update(cx, |composer, cx| {
                                        composer.steer_keiki(cx);
                                    });
                                } else {
                                    let handle = this.composer.read(cx).focus_handle(cx);
                                    window.focus(&handle, cx);
                                }
                            }))
                    })
                    .child("Steer");
                controls = controls.child(takeover_button).child(steer_button);
            }
            if right_open {
                let right_now = self.eval_tween(self.right_tween, self.right_target(cx));
                let pr = self.titlebar_right_pad(TITLEBAR_ACTION_EDGE_INSET);
                // The row's own left padding is part of its content box: a strip
                // wider than what's left after it overflows and clips at the right
                // edge (flex_none never shrinks) — cap to the available width. The
                // row's 8px child gaps sit OUTSIDE the strip's width (one before
                // the strip in takeover, two with the title row present): without
                // budgeting them the capped strip overflows by exactly one gap and
                // the buttons slide right on expand (user report).
                let gap_budget = if takeover { 8.0 } else { 16.0 };
                let avail = self.viewport_width - row_left - pr - gap_budget;
                // The right pane's SURFACE TABS (t3 RightPanelTabs) — the diff
                // options that used to live here moved into the pane's own
                // second row; expand stays in this band (user request).
                let tabs = self.render_right_tab_strip(cx);
                // The toggle is the fixed right-edge anchor, like the left
                // sidebar control. Only the tabs + expand section reveals to
                // its left; including the toggle in this animated width
                // compressed both icons into the same clipped box at open.
                let animated_width = ((right_now - pr).min(avail) - 28.0).max(0.0);
                controls = controls.child(
                    div()
                        .w(px(animated_width))
                        .h_full()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .overflow_hidden()
                        // 8 + the trigger's own 8px pad = the pane's 16px
                        // text gutter. The 4px right padding is the stable
                        // gap before the fixed toggle.
                        .pl(px(8.0))
                        .pr(px(4.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .overflow_hidden()
                                .child(tabs),
                        )
                        .child(header_icon_button(
                            "expand-changes",
                            right_pane_expand_icon(self.right_pane_expanded),
                            &theme,
                            cx.listener(|this, _, _, cx| this.toggle_right_pane_expand(cx)),
                        )),
                );
            }
            // Keep the trigger mounted at one fixed position while the pane
            // controls reveal to its left.
            Some(
                controls
                    .child(header_icon_button(
                        "toggle-changes",
                        icons::SIDEBAR_MINIMALISTIC,
                        &theme,
                        cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                    ))
                    .into_any_element(),
            )
        };

        let inner = div()
            .size_full()
            .flex()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(8.0))
            .pl(px(row_left))
            .pr(px(self.titlebar_right_pad(TITLEBAR_ACTION_EDGE_INSET)))
            // In panel takeover the header strip spans the whole band — the
            // title would sit UNDER it (both flex_none, the row overflows and
            // paint order stacks them), so it hides for the duration.
            .when(!takeover, |el| {
                el.child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .when_some(
                            harness.map(crate::pickers::harness_brand_icon),
                            |el, (path, tint)| {
                                el.child(
                                    icon(path)
                                        .size(px(14.0))
                                        .flex_none()
                                        .text_color(tint.unwrap_or(theme.text_muted)),
                                )
                            },
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(crate::typography::ui_rems(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if on_canvas {
                                    theme.text_muted.opacity(0.7)
                                } else {
                                    theme.text.opacity(0.85)
                                })
                                .child(title),
                        )
                        .when_some(target, |el, target| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(theme.text_muted.opacity(0.5))
                                    .child(target),
                            )
                        })
                        .when_some(conversation_status, |el, conversation| {
                            let takeover = conversation.takeover.as_ref().and_then(|takeover| {
                                crate::keiki::parse_timestamp(&takeover.expires_at).map(|expires| {
                                    let seconds =
                                        (expires - chrono::Utc::now()).num_seconds().max(0);
                                    let remaining = if seconds < 60 {
                                        format!("{seconds}s")
                                    } else {
                                        format!("{}m", seconds / 60)
                                    };
                                    format!("Taken over · {remaining} remaining")
                                })
                            });
                            el.when_some(takeover, |el, label| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(theme.warning)
                                        .child(SharedString::from(label)),
                                )
                            })
                            .when(conversation.blocked, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(theme.danger)
                                        .child(SharedString::from("Blocked")),
                                )
                            })
                        }),
                )
            })
            .child(div().flex_1())
            .children(trailing);

        // The unified window titlebar: full-width on the glass shell, ABOVE
        // the inset card. No bottom border — the card's own hairline is the
        // separation; the glass gutter shows between.
        let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
        self.titlebar_drag_region("chat-titlebar", bar, cx)
            .into_any_element()
    }
}

#[cfg(test)]
mod cycle_tests {
    use super::*;

    fn order(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn steps_forward_and_back_through_the_list() {
        let list = order(&["a", "b", "c"]);
        assert_eq!(cycle_target(&list, Some("a"), true).as_deref(), Some("b"));
        assert_eq!(cycle_target(&list, Some("b"), true).as_deref(), Some("c"));
        assert_eq!(cycle_target(&list, Some("c"), false).as_deref(), Some("b"));
        assert_eq!(cycle_target(&list, Some("b"), false).as_deref(), Some("a"));
    }

    #[test]
    fn wraps_at_both_ends() {
        let list = order(&["a", "b", "c"]);
        assert_eq!(cycle_target(&list, Some("c"), true).as_deref(), Some("a"));
        assert_eq!(cycle_target(&list, Some("a"), false).as_deref(), Some("c"));
    }

    #[test]
    fn a_single_session_cycles_to_itself() {
        // Not a no-op by accident: with one row both directions must resolve,
        // so the shortcut never looks broken by dead-ending on `None`.
        let list = order(&["only"]);
        assert_eq!(
            cycle_target(&list, Some("only"), true).as_deref(),
            Some("only")
        );
        assert_eq!(
            cycle_target(&list, Some("only"), false).as_deref(),
            Some("only")
        );
    }

    #[test]
    fn no_selection_enters_the_list_from_the_matching_end() {
        let list = order(&["a", "b", "c"]);
        assert_eq!(cycle_target(&list, None, true).as_deref(), Some("a"));
        assert_eq!(cycle_target(&list, None, false).as_deref(), Some("c"));
        assert_eq!(
            cycle_target(&list, Some("gone"), true).as_deref(),
            Some("a")
        );
        assert_eq!(
            cycle_target(&list, Some("gone"), false).as_deref(),
            Some("c")
        );
    }

    #[test]
    fn an_empty_list_has_nothing_to_select() {
        assert_eq!(cycle_target(&[], None, true), None);
        assert_eq!(cycle_target(&[], Some("a"), true), None);
    }

    // Cycling walks the rows the sidebar is drawing, not every chat: that
    // guarantee is structural now — `cycle_session` reads the same
    // `AppState::sidebar_chats` the sidebar and the jump shortcuts read, and
    // `jump_slots_count_the_rows_the_sidebar_draws` (state.rs) covers the
    // space-filter behaviour for all of them.
}
