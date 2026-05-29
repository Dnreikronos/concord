//! Login / register screen.
//!
//! Renders a single card that toggles between a login and a register form,
//! drives the `/api/auth/*` calls, and surfaces OAuth buttons that hand off to
//! the system browser. On a successful authentication it emits
//! [`AuthEvent::Authenticated`] carrying the [`Session`]; the root view listens
//! for that and swaps to the main app.
//!
//! GPUI runs its own (non-tokio) executor, so the blocking-free `reqwest` calls
//! are driven on a dedicated tokio runtime and their results handed back over a
//! oneshot channel that the GPUI task awaits.

use std::sync::OnceLock;

use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{h_flex, v_flex, Disableable};

use concord_shared::types::OAuthProvider;
use concord_shared::validation::{validate_email, validate_password, validate_username};

use crate::auth::{self, Session};
use crate::ui::theme::{color, font, space};

/// Shared tokio runtime used to drive the auth HTTP calls off GPUI's executor.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Runtime::new().expect("failed to start tokio runtime for auth requests")
    })
}

/// Open `url` in the user's default browser, best effort.
fn open_in_browser(url: &str) {
    use std::process::Command;
    let result = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else {
        Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to open browser for OAuth");
    }
}

/// Which form the card is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Login,
    Register,
}

/// Transient feedback shown beneath the inputs.
enum Status {
    Idle,
    Submitting,
    Error(SharedString),
    Info(SharedString),
}

/// Emitted when the user successfully authenticates.
pub enum AuthEvent {
    Authenticated(Session),
}

/// The login / register card.
pub struct AuthView {
    mode: Mode,
    username: Entity<InputState>,
    email: Entity<InputState>,
    password: Entity<InputState>,
    status: Status,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AuthEvent> for AuthView {}

impl AuthView {
    /// Build the view, creating the three input fields.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let username = cx.new(|cx| InputState::new(window, cx).placeholder("Username"));
        let email = cx.new(|cx| InputState::new(window, cx).placeholder("you@example.com"));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Password")
        });

        // Pressing Enter in any field submits the form.
        let subscriptions = vec![
            cx.subscribe_in(&username, window, Self::on_field_event),
            cx.subscribe_in(&email, window, Self::on_field_event),
            cx.subscribe_in(&password, window, Self::on_field_event),
        ];

        Self {
            mode: Mode::Login,
            username,
            email,
            password,
            status: Status::Idle,
            _subscriptions: subscriptions,
        }
    }

    fn on_field_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            self.submit(cx);
        }
    }

    /// Switch between the login and register forms, clearing any feedback.
    fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            Mode::Login => Mode::Register,
            Mode::Register => Mode::Login,
        };
        self.status = Status::Idle;
        cx.notify();
    }

    /// Open the system browser to begin the OAuth flow for `provider`.
    fn start_oauth(&mut self, provider: OAuthProvider, cx: &mut Context<Self>) {
        open_in_browser(&auth::oauth_url(&auth::api_base_url(), provider));
        self.status = Status::Info("Continue signing in from your browser.".into());
        cx.notify();
    }

    /// Validate the inputs, fire the request, and react to the result.
    fn submit(&mut self, cx: &mut Context<Self>) {
        if matches!(self.status, Status::Submitting) {
            return;
        }

        let email = self.email.read(cx).value().trim().to_string();
        let password = self.password.read(cx).value().to_string();
        let username = self.username.read(cx).value().trim().to_string();
        let mode = self.mode;

        // Client-side validation mirrors the server's own checks so obvious
        // mistakes never leave the machine.
        let check = (|| -> Result<(), String> {
            validate_email(&email).map_err(|e| e.to_string())?;
            validate_password(&password).map_err(|e| e.to_string())?;
            if mode == Mode::Register {
                validate_username(&username).map_err(|e| e.to_string())?;
            }
            Ok(())
        })();
        if let Err(message) = check {
            self.status = Status::Error(message.into());
            cx.notify();
            return;
        }

        self.status = Status::Submitting;
        cx.notify();

        let base = auth::api_base_url();
        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime().spawn(async move {
            let result = match mode {
                Mode::Login => auth::login(&base, &email, &password).await,
                Mode::Register => auth::register(&base, &username, &email, &password).await,
            };
            let _ = tx.send(result);
        });

        cx.spawn(async move |this, cx| {
            let outcome = rx.await;
            let _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(session)) => {
                        this.status = Status::Idle;
                        cx.emit(AuthEvent::Authenticated(session));
                    }
                    Ok(Err(err)) => {
                        this.status = Status::Error(err.to_string().into());
                    }
                    Err(_canceled) => {
                        this.status = Status::Error("the request was cancelled".into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A labelled input field. `mask_toggle` adds the show/hide control used by
    /// the password field.
    fn field(
        label: impl Into<SharedString>,
        state: &Entity<InputState>,
        mask_toggle: bool,
    ) -> impl IntoElement {
        let mut input = Input::new(state).w_full();
        if mask_toggle {
            input = input.mask_toggle();
        }
        v_flex()
            .gap(px(space::XS))
            .child(
                div()
                    .text_size(px(font::SM))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color::text_muted())
                    .child(label.into()),
            )
            .child(input)
    }

    /// The status / error line, if there is anything to show.
    fn status_line(&self) -> Option<AnyElement> {
        let (message, tone) = match &self.status {
            Status::Error(m) => (m.clone(), color::danger()),
            Status::Info(m) => (m.clone(), color::text_muted()),
            Status::Idle | Status::Submitting => return None,
        };
        Some(
            div()
                .text_size(px(font::SM))
                .text_color(tone)
                .child(message)
                .into_any_element(),
        )
    }

    /// A horizontal "OR" divider between the form and the OAuth buttons.
    fn divider() -> impl IntoElement {
        h_flex()
            .w_full()
            .items_center()
            .gap(px(space::SM))
            .child(div().h(px(1.0)).flex_1().bg(color::border()))
            .child(
                div()
                    .text_size(px(font::SM))
                    .text_color(color::text_faint())
                    .child("OR"),
            )
            .child(div().h(px(1.0)).flex_1().bg(color::border()))
    }

    fn oauth_button(
        &self,
        id: &'static str,
        label: &'static str,
        provider: OAuthProvider,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        Button::new(id)
            .label(label)
            .outline()
            .w_full()
            .on_click(cx.listener(move |this, _, _, cx| this.start_oauth(provider, cx)))
    }

    fn mode_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (prompt, action) = match self.mode {
            Mode::Login => ("Need an account?", "Register"),
            Mode::Register => ("Already have an account?", "Log In"),
        };
        h_flex()
            .w_full()
            .justify_center()
            .items_center()
            .gap(px(space::XS))
            .child(
                div()
                    .text_size(px(font::SM))
                    .text_color(color::text_muted())
                    .child(prompt),
            )
            .child(
                Button::new("auth-toggle")
                    .label(action)
                    .link()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_mode(cx))),
            )
    }
}

impl Render for AuthView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let registering = self.mode == Mode::Register;
        let submitting = matches!(self.status, Status::Submitting);
        let (title, subtitle) = if registering {
            ("Create an account", "Join the conversation on Concord.")
        } else {
            ("Welcome back", "We're glad to see you again.")
        };
        let submit_label = if registering { "Register" } else { "Log In" };

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(color::chat())
            .font_family(font::FAMILY)
            .child(
                v_flex()
                    .w(px(400.0))
                    .gap(px(space::MD))
                    .p(px(space::XL))
                    .rounded(px(space::SM))
                    .bg(color::sidebar())
                    .child(
                        v_flex()
                            .gap(px(space::XS))
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(font::TITLE))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(color::text())
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(font::SM))
                                    .text_color(color::text_muted())
                                    .child(subtitle),
                            ),
                    )
                    .children(registering.then(|| Self::field("Username", &self.username, false)))
                    .child(Self::field("Email", &self.email, false))
                    .child(Self::field("Password", &self.password, true))
                    .children(self.status_line())
                    .child(
                        Button::new("auth-submit")
                            .label(submit_label)
                            .primary()
                            .w_full()
                            .loading(submitting)
                            .disabled(submitting)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    )
                    .child(Self::divider())
                    .child(
                        h_flex()
                            .w_full()
                            .gap(px(space::SM))
                            .child(self.oauth_button(
                                "oauth-github",
                                "GitHub",
                                OAuthProvider::Github,
                                cx,
                            ))
                            .child(self.oauth_button(
                                "oauth-google",
                                "Google",
                                OAuthProvider::Google,
                                cx,
                            )),
                    )
                    .child(self.mode_toggle(cx)),
            )
    }
}
