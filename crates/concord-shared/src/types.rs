use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserStatus {
    Online,
    Idle,
    Dnd,
    #[default]
    Offline,
}

impl fmt::Display for UserStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Online => "online",
            Self::Idle => "idle",
            Self::Dnd => "dnd",
            Self::Offline => "offline",
        };
        f.write_str(s)
    }
}

impl FromStr for UserStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "online" => Ok(Self::Online),
            "idle" => Ok(Self::Idle),
            "dnd" => Ok(Self::Dnd),
            "offline" => Ok(Self::Offline),
            other => Err(format!("unknown user status: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthProvider {
    Google,
    Github,
}

impl fmt::Display for OAuthProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Google => "google",
            Self::Github => "github",
        };
        f.write_str(s)
    }
}

impl FromStr for OAuthProvider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "google" => Ok(Self::Google),
            "github" => Ok(Self::Github),
            other => Err(format!("unknown oauth provider: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip)]
    pub password_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    pub status: UserStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_provider: Option<OAuthProvider>,
    #[serde(skip)]
    pub oauth_subject: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub id: Uuid,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    pub owner_id: Uuid,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    Admin,
    Member,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMember {
    pub server_id: Uuid,
    pub user_id: Uuid,
    pub role: MemberRole,
    pub joined_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelCategory {
    pub id: Uuid,
    pub server_id: Uuid,
    pub name: String,
    pub position: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelType {
    Text,
    Voice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: Uuid,
    pub server_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_id: Option<Uuid>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub channel_type: ChannelType,
    pub position: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub channel_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_id: Option<Uuid>,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DmChannel {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub is_group: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DmMember {
    pub dm_channel_id: Uuid,
    pub user_id: Uuid,
    pub joined_at: DateTime<Utc>,
}
