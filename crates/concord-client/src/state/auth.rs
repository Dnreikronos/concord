//! The authenticated session: current user, issued tokens, and login status.
//!
//! Populated when the [`crate::ui::auth_view`] emits a successful login and
//! cleared on sign-out. Holds the [`Session`] in memory only; persistent token
//! storage (keyring) is a later concern, matching [`crate::auth`].

use uuid::Uuid;

use concord_shared::types::User;

use crate::auth::Session;

/// Holds the current [`Session`], if the user is signed in.
#[derive(Default)]
pub struct AuthState {
    session: Option<Session>,
}

impl AuthState {
    /// Create signed-out state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a session is held (the login status the UI gates on).
    pub fn is_authenticated(&self) -> bool {
        self.session.is_some()
    }

    /// The current session, if signed in.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// The signed-in user, if any.
    pub fn user(&self) -> Option<&User> {
        self.session.as_ref().map(|s| &s.user)
    }

    /// The signed-in user's id, if any.
    pub fn user_id(&self) -> Option<Uuid> {
        self.session.as_ref().map(|s| s.user.id)
    }

    /// The current access token, for authenticating REST calls.
    pub fn access_token(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.access_token.as_str())
    }

    /// The current refresh token.
    pub fn refresh_token(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.refresh_token.as_str())
    }

    /// Store a freshly issued session.
    pub fn sign_in(&mut self, session: Session) {
        self.session = Some(session);
    }

    /// Drop the session, returning to signed-out.
    pub fn sign_out(&mut self) {
        self.session = None;
    }

    /// Swap in refreshed tokens, keeping the already-resolved user. No-op when
    /// signed out.
    pub fn update_tokens(&mut self, access_token: String, refresh_token: String) {
        if let Some(session) = self.session.as_mut() {
            session.access_token = access_token;
            session.refresh_token = refresh_token;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use concord_shared::types::UserStatus;

    fn sample_session() -> Session {
        let now = Utc::now();
        Session {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            user: User {
                id: Uuid::nil(),
                username: "alice".into(),
                email: Some("alice@example.com".into()),
                password_hash: None,
                avatar_url: None,
                status: UserStatus::Online,
                oauth_provider: None,
                oauth_subject: None,
                created_at: now,
                updated_at: now,
            },
        }
    }

    #[test]
    fn starts_signed_out() {
        let auth = AuthState::new();
        assert!(!auth.is_authenticated());
        assert!(auth.user().is_none());
        assert!(auth.access_token().is_none());
    }

    #[test]
    fn sign_in_exposes_user_and_tokens() {
        let mut auth = AuthState::new();
        auth.sign_in(sample_session());
        assert!(auth.is_authenticated());
        assert_eq!(auth.user().map(|u| u.username.as_str()), Some("alice"));
        assert_eq!(auth.access_token(), Some("access"));
        assert_eq!(auth.refresh_token(), Some("refresh"));
        assert_eq!(auth.user_id(), Some(Uuid::nil()));
    }

    #[test]
    fn update_tokens_keeps_user() {
        let mut auth = AuthState::new();
        auth.sign_in(sample_session());
        auth.update_tokens("new-access".into(), "new-refresh".into());
        assert_eq!(auth.access_token(), Some("new-access"));
        assert_eq!(auth.refresh_token(), Some("new-refresh"));
        assert_eq!(auth.user().map(|u| u.username.as_str()), Some("alice"));
    }

    #[test]
    fn update_tokens_noop_when_signed_out() {
        let mut auth = AuthState::new();
        auth.update_tokens("x".into(), "y".into());
        assert!(!auth.is_authenticated());
    }

    #[test]
    fn sign_out_clears_session() {
        let mut auth = AuthState::new();
        auth.sign_in(sample_session());
        auth.sign_out();
        assert!(!auth.is_authenticated());
        assert!(auth.session().is_none());
    }
}
