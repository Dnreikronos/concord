//! "New Group DM" dialog.
//!
//! A modal card, overlaid on the main app by the [root view](crate::ui::root),
//! that gathers an optional group name and 1–9 other participants (the creator
//! is the tenth, so the server's 2–10 bound holds), then calls
//! `POST /api/dms/group`. On success it emits [`GroupDmEvent::Created`] carrying
//! the new [`DmChannelInfo`]; the root view folds that into the DM list and
//! opens the conversation. Cancelling or clicking the backdrop emits
//! [`GroupDmEvent::Dismissed`].
//!
//! Like [`AuthView`](crate::ui::auth_view), it owns its input fields, mirrors
//! the server's validation client-side, and drives the blocking-free `reqwest`
//! calls on the shared tokio runtime ([`crate::api::runtime`]), handing results
//! back over a oneshot the GPUI task awaits. The username search is debounced so
//! a burst of keystrokes makes one request.

use std::time::Duration;

use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{h_flex, v_flex, Disableable};
use uuid::Uuid;

use concord_shared::types::{DmChannelInfo, UserSummary};
use concord_shared::validation::{validate_dm_name, DM_GROUP_MAX, DM_GROUP_MIN};

use crate::api;
use crate::auth;
use crate::ui::theme::{color, font, space};

/// Most other participants the caller may add: the group cap less the caller.
const MAX_OTHERS: usize = DM_GROUP_MAX - 1;
/// Fewest other participants needed: a group is at least the caller plus one.
const MIN_OTHERS: usize = DM_GROUP_MIN - 1;
/// Page size for the username search.
const SEARCH_LIMIT: i64 = 20;
/// How long the search box must sit idle after the last keystroke before a
/// request goes out, so a burst of typing makes one query rather than many.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Emitted as the dialog resolves.
pub enum GroupDmEvent {
    /// The group DM was created; carries the new channel for the list and chat.
    Created(DmChannelInfo),
    /// The user cancelled (button, close, or backdrop).
    Dismissed,
}

/// The "New Group DM" modal.
pub struct GroupDmDialog {
    /// Access token captured when the dialog opened; used for the search and
    /// create calls. A short-lived dialog needn't track token refreshes.
    token: String,
    name_input: Entity<InputState>,
    search_input: Entity<InputState>,
    /// Bumped on every keystroke; only the latest debounce/search task whose
    /// captured value still matches goes on to touch the results.
    search_seq: u64,
    /// Latest search matches, with anyone already selected filtered out.
    results: Vec<UserSummary>,
    /// True while a search request is in flight (after the debounce).
    searching: bool,
    /// Chosen recipients, in the order they were added (the caller is implicit).
    selected: Vec<UserSummary>,
    /// True while the create request is in flight.
    creating: bool,
    /// The most recent error to surface beneath the inputs, if any.
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<GroupDmEvent> for GroupDmDialog {}

impl GroupDmDialog {
    /// Build the dialog, focusing the search field so the user can type at once.
    pub fn new(token: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Group name (optional)"));
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search by username"));
        search_input.update(cx, |state, cx| state.focus(window, cx));

        let subscriptions = vec![
            cx.subscribe_in(&search_input, window, Self::on_search_event),
            cx.subscribe_in(&name_input, window, Self::on_name_event),
        ];

        // Offer people to choose from before the user types: once the entity is
        // live, run an initial blank-query search to populate the results list.
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| this.on_search_change(cx));
        })
        .detach();

        Self {
            token,
            name_input,
            search_input,
            search_seq: 0,
            results: Vec::new(),
            searching: false,
            selected: Vec::new(),
            creating: false,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn on_search_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => self.on_search_change(cx),
            // Enter adds the top match, the fast path for "search, hit enter".
            InputEvent::PressEnter { .. } => {
                if let Some(first) = self.results.first().cloned() {
                    self.select(first, cx);
                }
            }
            _ => {}
        }
    }

    fn on_name_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            self.create(cx);
        }
    }

    /// React to a keystroke in the search box (and the initial open): (re)arm
    /// the debounce timer and, once it elapses unsuperseded, run the query. A
    /// blank box lists candidates rather than clearing, so the picker is useful
    /// before anything is typed.
    fn on_search_change(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().trim().to_string();
        self.search_seq = self.search_seq.wrapping_add(1);
        let seq = self.search_seq;

        self.searching = true;
        cx.notify();

        let token = self.token.clone();
        let base = auth::api_base_url();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            // A later keystroke supersedes this one; bail before the request.
            let current = this.update(cx, |this, _| this.search_seq == seq);
            if !matches!(current, Ok(true)) {
                return;
            }

            let (tx, rx) = tokio::sync::oneshot::channel();
            api::runtime().spawn(async move {
                let _ = tx.send(api::search_users(&base, &token, &query, SEARCH_LIMIT).await);
            });
            let outcome = rx.await;

            let _ = this.update(cx, |this, cx| {
                // Drop a response the next keystroke has already obsoleted.
                if this.search_seq != seq {
                    return;
                }
                this.searching = false;
                match outcome {
                    Ok(Ok(users)) => this.results = this.without_selected(users),
                    Ok(Err(err)) => {
                        tracing::warn!(error = %err, "user search failed");
                        this.results.clear();
                    }
                    Err(_canceled) => this.results.clear(),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Keep only users not already chosen, so the results never offer a repeat.
    fn without_selected(&self, users: Vec<UserSummary>) -> Vec<UserSummary> {
        users
            .into_iter()
            .filter(|u| !self.selected.iter().any(|s| s.id == u.id))
            .collect()
    }

    /// Add a user to the selection (no-op past the cap or for a duplicate),
    /// dropping them from the visible results.
    fn select(&mut self, user: UserSummary, cx: &mut Context<Self>) {
        if self.selected.len() >= MAX_OTHERS || self.selected.iter().any(|s| s.id == user.id) {
            return;
        }
        self.results.retain(|u| u.id != user.id);
        self.selected.push(user);
        self.error = None;
        cx.notify();
    }

    /// Drop a user from the selection.
    fn deselect(&mut self, user_id: Uuid, cx: &mut Context<Self>) {
        self.selected.retain(|u| u.id != user_id);
        cx.notify();
    }

    /// Validate and fire the create request. A missing recipient or a bad name
    /// is reported inline without a round-trip; the server is the authority on
    /// everything else and its error surfaces the same way.
    fn create(&mut self, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        if self.selected.len() < MIN_OTHERS {
            self.error = Some("Add at least one other person.".into());
            cx.notify();
            return;
        }

        let raw_name = self.name_input.read(cx).value().trim().to_string();
        let name = if raw_name.is_empty() {
            None
        } else {
            if let Err(err) = validate_dm_name(&raw_name) {
                self.error = Some(err.to_string().into());
                cx.notify();
                return;
            }
            Some(raw_name)
        };

        let recipient_ids: Vec<Uuid> = self.selected.iter().map(|u| u.id).collect();
        self.creating = true;
        self.error = None;
        cx.notify();

        let token = self.token.clone();
        let base = auth::api_base_url();
        let (tx, rx) = tokio::sync::oneshot::channel();
        api::runtime().spawn(async move {
            let _ = tx.send(api::create_group_dm(&base, &token, &recipient_ids, name.as_deref()).await);
        });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await;
            let _ = this.update(cx, |this, cx| {
                this.creating = false;
                match outcome {
                    Ok(Ok(info)) => cx.emit(GroupDmEvent::Created(info)),
                    Ok(Err(err)) => this.error = Some(err.to_string().into()),
                    Err(_canceled) => this.error = Some("the request was cancelled".into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Emit a dismissal so the root view can tear the dialog down.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(GroupDmEvent::Dismissed);
    }

    /// A labelled section: a muted caption above its content.
    fn field(label: &'static str, content: impl IntoElement) -> impl IntoElement {
        v_flex()
            .gap(px(space::XS))
            .child(
                div()
                    .text_size(px(font::SM))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color::text_muted())
                    .child(label),
            )
            .child(content)
    }

    /// The chosen-recipient pills, or `None` when nothing is selected yet.
    fn selected_chips(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.selected.is_empty() {
            return None;
        }
        let mut row = h_flex().w_full().flex_wrap().gap(px(space::XS));
        for user in self.selected.clone() {
            let id = user.id;
            row = row.child(
                h_flex()
                    .id(SharedString::from(format!("chip-{id}")))
                    .items_center()
                    .gap(px(space::XS))
                    .pl(px(space::SM))
                    .pr(px(space::XS))
                    .py(px(2.0))
                    .rounded(px(space::SM))
                    .bg(color::accent())
                    .text_color(color::interactive_active())
                    .text_size(px(font::SM))
                    .child(SharedString::from(user.username))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(16.0))
                            .rounded_full()
                            .text_size(px(font::MD))
                            .cursor_pointer()
                            .hover(|s| s.bg(color::accent_hover()))
                            .child("×"),
                    )
                    .cursor_pointer()
                    .hover(|s| s.bg(color::accent_hover()))
                    .on_click(cx.listener(move |this, _, _, cx| this.deselect(id, cx))),
            );
        }
        Some(row)
    }

    /// The search-results area: matching users as clickable rows, or a status
    /// line (searching / no matches) once a query has been typed.
    fn results_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex()
            .id("group-dm-results")
            .w_full()
            .max_h(px(220.0))
            .overflow_y_scroll()
            .gap(px(2.0));

        if self.results.is_empty() {
            let notice = if self.searching {
                "Searching…"
            } else if self.search_input.read(cx).value().trim().is_empty() {
                "No people to add yet."
            } else {
                "No matches."
            };
            return list.child(
                div()
                    .px(px(space::SM))
                    .py(px(space::SM))
                    .text_size(px(font::SM))
                    .text_color(color::text_muted())
                    .child(notice),
            );
        }

        let at_cap = self.selected.len() >= MAX_OTHERS;
        for user in self.results.clone() {
            let id = user.id;
            let initial = first_initial(&user.username);
            let mut row = h_flex()
                .id(SharedString::from(format!("result-{id}")))
                .w_full()
                .items_center()
                .gap(px(space::SM))
                .px(px(space::SM))
                .py(px(space::XS))
                .rounded(px(space::XS))
                .child(
                    div()
                        .size(px(28.0))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(color::elevated())
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(font::SM))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color::text())
                        .child(initial),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(font::MD))
                        .text_color(color::text())
                        .child(SharedString::from(user.username.clone())),
                );
            // Past the cap the rows read as disabled and don't respond.
            if at_cap {
                row = row.opacity(0.5);
            } else {
                row = row
                    .cursor_pointer()
                    .hover(|s| s.bg(color::hover()))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(user.clone(), cx)));
            }
            list = list.child(row);
        }
        list
    }

    /// The footer: the live selected-count and the cancel / create buttons.
    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.selected.len();
        let can_create = (MIN_OTHERS..=MAX_OTHERS).contains(&count) && !self.creating;

        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(font::SM))
                    .text_color(color::text_faint())
                    .child(SharedString::from(format!("{count}/{MAX_OTHERS} selected"))),
            )
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .child(
                        Button::new("group-dm-cancel")
                            .label("Cancel")
                            .ghost()
                            .disabled(self.creating)
                            .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
                    )
                    .child(
                        Button::new("group-dm-create")
                            .label("Create")
                            .primary()
                            .loading(self.creating)
                            .disabled(!can_create)
                            .on_click(cx.listener(|this, _, _, cx| this.create(cx))),
                    ),
            )
    }
}

impl Render for GroupDmDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let card = v_flex()
            .id("group-dm-card")
            .w(px(440.0))
            .gap(px(space::MD))
            .p(px(space::XL))
            .rounded(px(space::SM))
            .bg(color::sidebar())
            // Clicks on the card itself must not reach the backdrop's dismiss.
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(font::LG))
                            .font_weight(FontWeight::BOLD)
                            .text_color(color::text())
                            .child("New Group DM"),
                    )
                    .child(
                        div()
                            .id("group-dm-close")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(24.0))
                            .rounded(px(space::XS))
                            .text_size(px(font::LG))
                            .text_color(color::text_muted())
                            .cursor_pointer()
                            .hover(|s| s.bg(color::hover()).text_color(color::text()))
                            .child("×")
                            .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
                    ),
            )
            .child(Self::field("Group name", Input::new(&self.name_input).w_full()))
            .children(self.selected_chips(cx))
            .child(Self::field("Add people", Input::new(&self.search_input).w_full()))
            .child(self.results_area(cx))
            .children(self.error.clone().map(|message| {
                div()
                    .text_size(px(font::SM))
                    .text_color(color::danger())
                    .child(message)
            }))
            .child(self.footer(cx));

        div()
            .id("group-dm-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            // Capture all pointer events so the app behind the modal is inert.
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0.0, 0.0, 0.0, 0.5))
            .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx)))
            .child(card)
    }
}

/// The uppercase first alphanumeric character of a username, for the avatar
/// placeholder; falls back to `?` for an empty or symbol-only name.
fn first_initial(name: &str) -> SharedString {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".to_string())
        .into()
}

#[cfg(test)]
mod tests {
    // Import specifics rather than `use super::*`: the latter re-globs `gpui::*`
    // into this module, which blows the recursion limit when the `#[test]`
    // harness expands (see the other gui modules' test mods).
    use super::{first_initial, MAX_OTHERS, MIN_OTHERS};

    #[test]
    fn participant_bounds_leave_room_for_the_caller() {
        // The caller is the implicit tenth member, so 1–9 others fit a 2–10 group.
        assert_eq!(MIN_OTHERS, 1);
        assert_eq!(MAX_OTHERS, 9);
    }

    #[test]
    fn first_initial_uppercases_first_alphanumeric() {
        assert_eq!(first_initial("bob").as_ref(), "B");
        assert_eq!(first_initial("_neo").as_ref(), "N");
        assert_eq!(first_initial("99bottles").as_ref(), "9");
        assert_eq!(first_initial("").as_ref(), "?");
        assert_eq!(first_initial("___").as_ref(), "?");
    }
}
