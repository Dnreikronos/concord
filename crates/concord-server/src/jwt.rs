use std::fmt;

use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const ACCESS_TOKEN_MINUTES: i64 = 15;
const REFRESH_TOKEN_DAYS: i64 = 7;
const OAUTH_STATE_SECONDS: i64 = 300;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: Uuid,
    pub exp: i64,
    pub iat: i64,
}

#[derive(Debug)]
pub enum JwtError {
    Encode(jsonwebtoken::errors::Error),
    Decode(jsonwebtoken::errors::Error),
}

impl fmt::Display for JwtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(e) => write!(f, "jwt encode error: {e}"),
            Self::Decode(e) => write!(f, "jwt decode error: {e}"),
        }
    }
}

impl std::error::Error for JwtError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Encode(e) | Self::Decode(e) => Some(e),
        }
    }
}

pub fn encode_access_token(user_id: Uuid, secret: &str) -> Result<String, JwtError> {
    let now = Utc::now();
    let claims = Claims {
        sub: user_id,
        iat: now.timestamp(),
        exp: (now + Duration::minutes(ACCESS_TOKEN_MINUTES)).timestamp(),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(JwtError::Encode)
}

pub fn decode_access_token(token: &str, secret: &str) -> Result<Claims, JwtError> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(JwtError::Decode)?;
    Ok(data.claims)
}

pub struct RefreshToken {
    pub raw: String,
    pub hash: String,
    pub expires_at: chrono::DateTime<Utc>,
}

pub fn generate_refresh_token() -> RefreshToken {
    let mut bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    let raw = hex::encode(bytes);

    let hash = hex::encode(Sha256::digest(raw.as_bytes()));

    let expires_at = Utc::now() + Duration::days(REFRESH_TOKEN_DAYS);

    RefreshToken { raw, hash, expires_at }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OAuthStateClaims {
    pub nonce: String,
    pub exp: i64,
    pub iat: i64,
}

pub fn encode_oauth_state(secret: &str) -> Result<String, JwtError> {
    let now = Utc::now();
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let claims = OAuthStateClaims {
        nonce: hex::encode(bytes),
        iat: now.timestamp(),
        exp: (now + Duration::seconds(OAUTH_STATE_SECONDS)).timestamp(),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(JwtError::Encode)
}

pub fn decode_oauth_state(token: &str, secret: &str) -> Result<OAuthStateClaims, JwtError> {
    let data = decode::<OAuthStateClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(JwtError::Decode)?;
    Ok(data.claims)
}
