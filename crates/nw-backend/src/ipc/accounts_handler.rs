//! IPC handler for `auth.*` and `accounts.*` ([`crate::accounts`]).
//!
//! Methods:
//!   auth.status          — {token?} → {enabled, user: UserView | null}
//!   auth.login           — {username, password} → {token, user}
//!   auth.logout          — {token} → {}
//!   auth.change_password — {token, current, new} → {} (signs the user's other sessions out)
//!   accounts.get         — {} → {enabled, users: [UserView]} (never password hashes)
//!   accounts.set_user    — {username, role, cameras?, password?} → {users} (a new user needs a password)
//!   accounts.remove_user — {username} → {users}
//!   accounts.set_enabled — {enabled} → {enabled} (needs an admin)
//!
//! The IPC server's gate ([`super::handler::RpcHandler::dispatch_gated`])
//! decides who may call which: `auth.*` is open, `accounts.*` needs an admin
//! once accounts are on.

use std::sync::Arc;

use nw_core::ipc::RpcError;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::accounts::{self, AccountsConfig, LocalUser, PasswordHash, Role, UserView};
use crate::app_state::AppState;

pub struct AccountsHandler {
    state: Arc<RwLock<AppState>>,
}

fn text<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params[key]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params(format!("Missing '{key}'")))
}

/// PBKDF2 is deliberately slow: off the async workers.
async fn hash(password: String) -> Result<PasswordHash, RpcError> {
    tokio::task::spawn_blocking(move || PasswordHash::new(&password))
        .await
        .ok()
        .flatten()
        .ok_or_else(|| RpcError::internal("Could not hash the password"))
}

impl AccountsHandler {
    pub fn new(state: Arc<RwLock<AppState>>) -> Self {
        Self { state }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "auth.status" => {
                let st = self.state.read().await;
                let user = params["token"].as_str().and_then(|token| {
                    st.sessions.session(&st.config.accounts, token).map(|session| {
                        json!({"username": session.username, "role": session.role, "cameras": session.cameras})
                    })
                });
                Ok(json!({"enabled": st.config.accounts.enabled, "user": user}))
            }
            "auth.login" => self.login(params).await,
            "auth.logout" => {
                if let Some(token) = params["token"].as_str() {
                    Arc::clone(&self.state.read().await.sessions).logout(token);
                }
                Ok(json!({}))
            }
            "auth.change_password" => self.change_password(params).await,
            "accounts.get" => {
                let st = self.state.read().await;
                Ok(Self::view(&st.config.accounts))
            }
            "accounts.set_user" => self.set_user(params).await,
            "accounts.remove_user" => {
                let username = text(params, "username")?.to_owned();
                self.change(|accounts| {
                    let before = accounts.users.len();
                    accounts
                        .users
                        .retain(|u| !u.username.eq_ignore_ascii_case(&username));
                    if accounts.users.len() == before {
                        return Err("user not found".into());
                    }
                    Ok(())
                })
                .await?;
                Arc::clone(&self.state.read().await.sessions).end_user(&username);
                Ok(Self::view(&self.state.read().await.config.accounts))
            }
            "accounts.set_enabled" => {
                let enabled = params["enabled"]
                    .as_bool()
                    .ok_or_else(|| RpcError::invalid_params("Missing 'enabled'"))?;
                self.change(|accounts| {
                    accounts.enabled = enabled;
                    Ok(())
                })
                .await?;
                tracing::info!(enabled, "Sign-in requirement changed");
                Ok(json!({"enabled": enabled}))
            }
            _ => Err(RpcError::method_not_found()),
        }
    }

    fn view(accounts: &AccountsConfig) -> Value {
        let users: Vec<UserView> = accounts.users.iter().map(UserView::from).collect();
        json!({"enabled": accounts.enabled, "users": users})
    }

    /// Apply `edit` to a copy of the accounts; keep it only if valid and saved.
    async fn change(
        &self,
        edit: impl FnOnce(&mut AccountsConfig) -> Result<(), String>,
    ) -> Result<(), RpcError> {
        let mut st = self.state.write().await;
        let mut next = st.config.accounts.clone();
        edit(&mut next).map_err(RpcError::invalid_params)?;
        next.validate().map_err(RpcError::invalid_params)?;
        let previous = std::mem::replace(&mut st.config.accounts, next);
        if let Err(error) = st.save_config() {
            st.config.accounts = previous;
            return Err(RpcError::internal(error.to_string()));
        }
        Ok(())
    }

    async fn login(&self, params: &Value) -> Result<Value, RpcError> {
        let username = text(params, "username")?.to_owned();
        let password = text(params, "password")?.to_owned();
        let (accounts, sessions) = {
            let st = self.state.read().await;
            if !st.config.accounts.enabled {
                return Err(RpcError::invalid_params("Sign-in is not turned on"));
            }
            (st.config.accounts.clone(), Arc::clone(&st.sessions))
        };
        let (token, session) =
            tokio::task::spawn_blocking(move || sessions.login(&accounts, &username, &password))
                .await
                .map_err(|e| RpcError::internal(e.to_string()))?
                .map_err(RpcError::invalid_params)?;
        Ok(json!({"token": token, "user": {
            "username": session.username, "role": session.role, "cameras": session.cameras
        }}))
    }

    async fn change_password(&self, params: &Value) -> Result<Value, RpcError> {
        let token = text(params, "token")?.to_owned();
        let current = text(params, "current")?.to_owned();
        let new = text(params, "new")?.to_owned();
        if let Some(problem) = accounts::password_problem(&new) {
            return Err(RpcError::invalid_params(problem));
        }
        let (session, stored) = {
            let st = self.state.read().await;
            let session = st
                .sessions
                .session(&st.config.accounts, &token)
                .ok_or_else(|| RpcError::invalid_params("Sign in again first"))?;
            let stored = st
                .config
                .accounts
                .user(&session.username)
                .map(|u| u.password.clone())
                .ok_or_else(|| RpcError::invalid_params("Sign in again first"))?;
            (session, stored)
        };
        let ok = tokio::task::spawn_blocking(move || stored.verify(&current))
            .await
            .unwrap_or(false);
        if !ok {
            return Err(RpcError::invalid_params("The current password is wrong"));
        }
        let password = hash(new).await?;
        let username = session.username.clone();
        self.change(move |accounts| {
            let user = accounts
                .users
                .iter_mut()
                .find(|u| u.username.eq_ignore_ascii_case(&username))
                .ok_or("user not found")?;
            user.password = password;
            Ok(())
        })
        .await?;
        let sessions = Arc::clone(&self.state.read().await.sessions);
        sessions.end_user(&session.username);
        tracing::info!(username = %session.username, "Password changed");
        Ok(json!({}))
    }

    async fn set_user(&self, params: &Value) -> Result<Value, RpcError> {
        let username = text(params, "username")?.trim().to_owned();
        let role: Role = serde_json::from_value(params["role"].clone())
            .map_err(|_| RpcError::invalid_params("Choose a role: viewer, operator or admin"))?;
        let cameras: Vec<String> = match &params["cameras"] {
            Value::Null => Vec::new(),
            value => serde_json::from_value(value.clone())
                .map_err(|_| RpcError::invalid_params("'cameras' must be a list of camera ids"))?,
        };
        let password = match params["password"].as_str() {
            Some(password) if !password.is_empty() => {
                if let Some(problem) = accounts::password_problem(password) {
                    return Err(RpcError::invalid_params(problem));
                }
                Some(hash(password.to_owned()).await?)
            }
            _ => None,
        };
        let changed_password = password.is_some();
        let name = username.clone();
        self.change(move |accounts| {
            match accounts
                .users
                .iter_mut()
                .find(|u| u.username.eq_ignore_ascii_case(&name))
            {
                Some(user) => {
                    user.role = role;
                    user.cameras = cameras;
                    if let Some(password) = password {
                        user.password = password;
                    }
                }
                None => accounts.users.push(LocalUser {
                    username: name,
                    role,
                    cameras,
                    password: password.ok_or("a new user needs a password")?,
                }),
            }
            Ok(())
        })
        .await?;
        if changed_password {
            Arc::clone(&self.state.read().await.sessions).end_user(&username);
        }
        tracing::info!(username = %username, role = role.label(), "Account saved");
        Ok(Self::view(&self.state.read().await.config.accounts))
    }
}
