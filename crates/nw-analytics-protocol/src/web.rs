//! Plugin HTTP ingress, split into bounded control messages. Cookies and
//! recorder credentials never travel through this interface.
use crate::messages::CameraInfo;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebUi {
    pub entry: String,
    pub title: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserContext {
    pub user: String,
    pub role: String,
    pub cameras: Vec<CameraInfo>,
    pub plugin_key: String,
    pub expires_at: i64,
    pub nonce: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedUser {
    pub context: UserContext,
    pub signature: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub path: String,
    pub method: String,
    #[serde(default)]
    pub content_type: String,
    #[serde(default)]
    pub body_b64: String,
    pub user: SignedUser,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Read {
    pub token: String,
    pub offset: usize,
    pub user: SignedUser,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub token: String,
    pub status: u16,
    pub content_type: String,
    pub size: usize,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub offset: usize,
    pub data_b64: String,
    pub eof: bool,
}
pub const MAX_BODY: usize = 4 * 1024 * 1024;
pub const CHUNK: usize = 48 * 1024;
