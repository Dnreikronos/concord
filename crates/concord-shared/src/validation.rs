use std::fmt;

// Limits mirror the CHECK constraints in the SQL schema.
pub const USERNAME_MIN: usize = 3;
pub const USERNAME_MAX: usize = 32;
pub const SERVER_NAME_MIN: usize = 1;
pub const SERVER_NAME_MAX: usize = 100;
pub const CHANNEL_NAME_MIN: usize = 1;
pub const CHANNEL_NAME_MAX: usize = 100;
pub const CATEGORY_NAME_MIN: usize = 1;
pub const CATEGORY_NAME_MAX: usize = 100;
pub const MESSAGE_CONTENT_MIN: usize = 1;
pub const MESSAGE_CONTENT_MAX: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    TooShort { field: &'static str, min: usize },
    TooLong { field: &'static str, max: usize },
    InvalidEmail,
    InvalidChars { field: &'static str },
    BlankContent { field: &'static str },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { field, min } => {
                write!(f, "{field} must be at least {min} characters")
            }
            Self::TooLong { field, max } => {
                write!(f, "{field} must be at most {max} characters")
            }
            Self::InvalidEmail => write!(f, "invalid email address"),
            Self::InvalidChars { field } => {
                write!(f, "{field} contains invalid characters")
            }
            Self::BlankContent { field } => {
                write!(f, "{field} must not be blank")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

pub fn validate_username(s: &str) -> Result<(), ValidationError> {
    let len = s.chars().count();
    if len < USERNAME_MIN {
        return Err(ValidationError::TooShort { field: "username", min: USERNAME_MIN });
    }
    if len > USERNAME_MAX {
        return Err(ValidationError::TooLong { field: "username", max: USERNAME_MAX });
    }
    if s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ValidationError::InvalidChars { field: "username" });
    }
    Ok(())
}

pub fn validate_email(s: &str) -> Result<(), ValidationError> {
    let (local, domain) = s.rsplit_once('@').ok_or(ValidationError::InvalidEmail)?;

    if local.is_empty() || domain.len() < 3 {
        return Err(ValidationError::InvalidEmail);
    }

    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return Err(ValidationError::InvalidEmail);
    }

    if !domain.contains('.') {
        return Err(ValidationError::InvalidEmail);
    }

    let tld = domain.rsplit_once('.').map(|(_, tld)| tld).unwrap_or("");
    if tld.len() < 2 {
        return Err(ValidationError::InvalidEmail);
    }

    Ok(())
}

pub fn validate_message_content(s: &str) -> Result<(), ValidationError> {
    if s.trim().is_empty() {
        return Err(ValidationError::BlankContent { field: "message content" });
    }
    let len = s.chars().count();
    if len > MESSAGE_CONTENT_MAX {
        return Err(ValidationError::TooLong { field: "message content", max: MESSAGE_CONTENT_MAX });
    }
    Ok(())
}

pub fn validate_server_name(s: &str) -> Result<(), ValidationError> {
    validate_name(s, "server name", SERVER_NAME_MIN, SERVER_NAME_MAX)
}

pub fn validate_channel_name(s: &str) -> Result<(), ValidationError> {
    validate_name(s, "channel name", CHANNEL_NAME_MIN, CHANNEL_NAME_MAX)
}

pub fn validate_category_name(s: &str) -> Result<(), ValidationError> {
    validate_name(s, "category name", CATEGORY_NAME_MIN, CATEGORY_NAME_MAX)
}

fn validate_name(
    s: &str,
    field: &'static str,
    min: usize,
    max: usize,
) -> Result<(), ValidationError> {
    if s.trim().is_empty() {
        return Err(ValidationError::BlankContent { field });
    }
    let len = s.chars().count();
    if len < min {
        return Err(ValidationError::TooShort { field, min });
    }
    if len > max {
        return Err(ValidationError::TooLong { field, max });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_length_boundaries() {
        assert!(validate_username("ab").is_err());
        assert!(validate_username("abc").is_ok());
        assert!(validate_username(&"a".repeat(32)).is_ok());
        assert!(validate_username(&"a".repeat(33)).is_err());
    }

    #[test]
    fn email_validation() {
        assert!(validate_email("user@example.com").is_ok());
        assert!(validate_email("a@b.co").is_ok());
        assert!(validate_email("a@b.c").is_err());
        assert!(validate_email("user@.com").is_err());
        assert!(validate_email("user@com.").is_err());
        assert!(validate_email("user@do..com").is_err());
        assert!(validate_email("noatsign").is_err());
        assert!(validate_email("@missing-local.com").is_err());
        assert!(validate_email("missing-domain@").is_err());
        assert!(validate_email("no-dot@domain").is_err());
    }

    #[test]
    fn username_rejects_invalid_chars() {
        assert!(validate_username("good_name").is_ok());
        assert!(validate_username("has space").is_err());
        assert!(validate_username("has\tnewline").is_err());
        assert!(validate_username("has\0null").is_err());
    }

    #[test]
    fn message_content_validation() {
        assert!(validate_message_content("hello").is_ok());
        assert!(validate_message_content("   ").is_err());
        assert!(validate_message_content("").is_err());
        assert!(validate_message_content(&"x".repeat(4000)).is_ok());
        assert!(validate_message_content(&"x".repeat(4001)).is_err());
    }

    #[test]
    fn server_name_validation() {
        assert!(validate_server_name("My Server").is_ok());
        assert!(validate_server_name("").is_err());
        assert!(validate_server_name("   ").is_err());
        assert!(validate_server_name(&"a".repeat(101)).is_err());
    }
}
