//! Local accounts: who may use this recorder through its IPC, and how much.
//!
//! Off by default: the IPC is then open to its callers as before (the Unix
//! socket is owner-only; a TCP endpoint needs this user's token). Turned on
//! (`accounts.enabled`), every request needs a session from `auth.login`,
//! except a few status reads (the tray's) and the sign-in itself, and each
//! user's role and cameras limit what they may do and see ([`access`]).
//!
//! Roles: a **viewer** watches live video and recordings and reviews events;
//! an **operator** also steers cameras (PTZ), bookmarks, exports clips and
//! starts or stops recording; an **admin** changes settings, cameras, alerts
//! and accounts. A user may be limited to some cameras (not admins).
//!
//! Passwords are stored as PBKDF2-HMAC-SHA256 hashes with a random salt.
//! These accounts guard the recorder's IPC (the desktop app, a TCP endpoint),
//! not the files of the operating-system user it runs as: that user can still
//! edit the configuration, e.g. to turn accounts off after losing the last
//! admin password.

pub mod access;

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use ring::rand::SecureRandom as _;
use serde::{Deserialize, Serialize};

/// PBKDF2-HMAC-SHA256 rounds for new hashes (OWASP's figure for this PRF).
/// A stored hash with fewer is replaced when its user next signs in.
pub const ITERATIONS: u32 = 600_000;
const SALT_BYTES: usize = 16;
const HASH_BYTES: usize = 32;
pub const MIN_PASSWORD: usize = 8;
pub const MAX_USERS: usize = 50;
/// A session ends after this long without a request.
pub const SESSION_IDLE: Duration = Duration::from_secs(12 * 60 * 60);
/// Wrong passwords in a row before a name must wait.
const FAILURES: u32 = 5;
/// The first wait; each further wrong password doubles it, up to
/// [`MAX_LOCKOUT`].
const LOCKOUT: Duration = Duration::from_secs(30);
const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Failures of names nobody has share one counter, so the table never holds
/// more than the users plus this.
const UNKNOWN_NAMES: &str = "";

/// How long a name that failed `count` times in a row must wait.
fn lockout_for(count: u32) -> Duration {
    if count < FAILURES {
        return Duration::ZERO;
    }
    let doublings = (count - FAILURES).min(10);
    LOCKOUT.saturating_mul(1 << doublings).min(MAX_LOCKOUT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordHash {
    pub salt: String,
    pub hash: String,
    pub iterations: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalUser {
    pub username: String,
    pub role: Role,
    /// Cameras this user may use; empty: every camera. Admins always have all.
    #[serde(default)]
    pub cameras: Vec<String>,
    pub password: PasswordHash,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub users: Vec<LocalUser>,
}

/// A user as clients see them: never the password hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UserView {
    pub username: String,
    pub role: Role,
    pub cameras: Vec<String>,
}

impl From<&LocalUser> for UserView {
    fn from(user: &LocalUser) -> Self {
        Self {
            username: user.username.clone(),
            role: user.role,
            cameras: if user.role == Role::Admin {
                Vec::new()
            } else {
                user.cameras.clone()
            },
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}

fn derive(password: &str, salt: &[u8], iterations: u32) -> [u8; HASH_BYTES] {
    let mut out = [0u8; HASH_BYTES];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(iterations.max(1)).unwrap_or(NonZeroU32::MIN),
        salt,
        password.as_bytes(),
        &mut out,
    );
    out
}

pub fn random_bytes<const N: usize>() -> Option<[u8; N]> {
    let mut bytes = [0u8; N];
    ring::rand::SystemRandom::new().fill(&mut bytes).ok()?;
    Some(bytes)
}

impl PasswordHash {
    pub fn new(password: &str) -> Option<Self> {
        let salt: [u8; SALT_BYTES] = random_bytes()?;
        Some(Self {
            salt: b64(&salt),
            hash: b64(&derive(password, &salt, ITERATIONS)),
            iterations: ITERATIONS,
        })
    }

    /// Whether this hash has fewer rounds than new ones get.
    pub fn needs_upgrade(&self) -> bool {
        self.iterations < ITERATIONS
    }

    /// A hash of a password nobody knows, to check a name that does not
    /// exist against, so an unknown name costs the same time as a wrong
    /// password.
    fn decoy() -> &'static Self {
        static DECOY: std::sync::OnceLock<PasswordHash> = std::sync::OnceLock::new();
        DECOY.get_or_init(|| {
            let salt = [0x5a_u8; SALT_BYTES];
            Self {
                salt: b64(&salt),
                hash: b64(&derive("decoy-not-a-password", &salt, ITERATIONS)),
                iterations: ITERATIONS,
            }
        })
    }

    /// Constant-time check of `password` against the stored hash.
    pub fn verify(&self, password: &str) -> bool {
        let engine = base64::engine::general_purpose::STANDARD_NO_PAD;
        let (Ok(salt), Ok(hash)) = (engine.decode(&self.salt), engine.decode(&self.hash)) else {
            return false;
        };
        ring::pbkdf2::verify(
            ring::pbkdf2::PBKDF2_HMAC_SHA256,
            NonZeroU32::new(self.iterations.max(1)).unwrap_or(NonZeroU32::MIN),
            &salt,
            password.as_bytes(),
            &hash,
        )
        .is_ok()
    }
}

pub fn valid_username(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn password_problem(password: &str) -> Option<String> {
    let length = password.chars().count();
    if length < MIN_PASSWORD {
        return Some(format!(
            "use a password of at least {MIN_PASSWORD} characters"
        ));
    }
    if length > 256 || password.chars().any(char::is_control) {
        return Some("the password is too long or has control characters".into());
    }
    None
}

impl AccountsConfig {
    pub fn user(&self, username: &str) -> Option<&LocalUser> {
        self.users
            .iter()
            .find(|u| u.username.eq_ignore_ascii_case(username))
    }

    fn admins(&self) -> usize {
        self.users.iter().filter(|u| u.role == Role::Admin).count()
    }

    /// Why these accounts cannot be used, or `Ok`.
    pub fn validate(&self) -> Result<(), String> {
        if self.users.len() > MAX_USERS {
            return Err(format!("at most {MAX_USERS} users"));
        }
        let mut names = std::collections::HashSet::new();
        for user in &self.users {
            if !valid_username(&user.username) || !names.insert(user.username.to_ascii_lowercase())
            {
                return Err(format!(
                    "user name '{}' is invalid or taken (1–32 letters, digits, . _ -)",
                    user.username
                ));
            }
        }
        if self.enabled && self.admins() == 0 {
            return Err("sign-in needs at least one admin, or nobody could change settings".into());
        }
        Ok(())
    }
}

/// A signed-in user, as the IPC gate sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub username: String,
    pub role: Role,
    /// Empty: every camera.
    pub cameras: Vec<String>,
}

impl Session {
    pub fn of(user: &LocalUser) -> Self {
        let view = UserView::from(user);
        Self {
            username: view.username,
            role: view.role,
            cameras: view.cameras,
        }
    }

    pub fn sees(&self, camera_id: &str) -> bool {
        self.cameras.is_empty() || self.cameras.iter().any(|c| c == camera_id)
    }
}

struct Live {
    username: String,
    last_seen: Instant,
}

/// Sessions and sign-in throttling (in memory: a restart signs everyone out).
#[derive(Default)]
pub struct Sessions {
    live: Mutex<HashMap<String, Live>>,
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl Sessions {
    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Live>> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Sign in; a new session token, or why not.
    pub fn login(
        &self,
        accounts: &AccountsConfig,
        username: &str,
        password: &str,
    ) -> Result<(String, Session), String> {
        // Failures count against a real user's name; every unknown name
        // shares one counter, so spraying names cannot grow the table.
        let known = accounts.user(username);
        let key = match known {
            Some(user) => user.username.to_ascii_lowercase(),
            None => UNKNOWN_NAMES.to_owned(),
        };
        {
            let failures = self.failures.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((count, at)) = failures.get(&key) {
                let wait = lockout_for(*count);
                if let Some(left) = wait.checked_sub(at.elapsed()).filter(|d| !d.is_zero()) {
                    return Err(format!(
                        "too many wrong passwords; wait {} seconds and try again",
                        left.as_secs().max(1)
                    ));
                }
            }
        }
        // Unknown names are checked against a decoy, so the time taken does
        // not tell a missing name from a wrong password.
        let user = match known {
            Some(user) => user.password.verify(password).then_some(user),
            None => {
                let _ = PasswordHash::decoy().verify(password);
                None
            }
        };
        let Some(user) = user else {
            let mut failures = self.failures.lock().unwrap_or_else(|p| p.into_inner());
            let entry = failures.entry(key).or_insert((0, Instant::now()));
            // A name left alone for twice its longest wait starts again.
            if entry.1.elapsed() >= MAX_LOCKOUT * 2 {
                *entry = (0, Instant::now());
            }
            entry.0 = entry.0.saturating_add(1);
            entry.1 = Instant::now();
            tracing::warn!(username, "Wrong user name or password");
            return Err("wrong user name or password".into());
        };
        self.failures
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
        let token = random_bytes::<32>()
            .map(|bytes| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
            .ok_or("could not start a session")?;
        self.live().insert(
            token.clone(),
            Live {
                username: user.username.clone(),
                last_seen: Instant::now(),
            },
        );
        tracing::info!(username = %user.username, role = user.role.label(), "Signed in");
        Ok((token, Session::of(user)))
    }

    /// The session behind `token`, if it is current (its user as configured now).
    pub fn session(&self, accounts: &AccountsConfig, token: &str) -> Option<Session> {
        let mut live = self.live();
        let entry = live.get_mut(token)?;
        if entry.last_seen.elapsed() > SESSION_IDLE {
            live.remove(token);
            return None;
        }
        let Some(user) = accounts.user(&entry.username) else {
            live.remove(token);
            return None;
        };
        entry.last_seen = Instant::now();
        Some(Session::of(user))
    }

    pub fn logout(&self, token: &str) {
        self.live().remove(token);
    }

    /// End every session of `username` (removed, or its password changed).
    pub fn end_user(&self, username: &str) {
        self.live()
            .retain(|_, live| !live.username.eq_ignore_ascii_case(username));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str, role: Role, password: &str) -> LocalUser {
        LocalUser {
            username: name.into(),
            role,
            cameras: vec!["cam-1".into()],
            password: PasswordHash::new(password).unwrap(),
        }
    }

    #[test]
    fn passwords_are_salted_hashes_that_verify_only_the_right_password() {
        let a = PasswordHash::new("correct horse").unwrap();
        let b = PasswordHash::new("correct horse").unwrap();
        assert_ne!(a.hash, b.hash, "a random salt per password");
        assert!(a.verify("correct horse"));
        assert!(!a.verify("correct horsE") && !a.verify(""));
        assert!(!a.hash.contains("horse"));
        assert_eq!(
            password_problem("short"),
            Some("use a password of at least 8 characters".into())
        );
        assert_eq!(password_problem("long enough"), None);
    }

    #[test]
    fn accounts_need_unique_names_and_an_admin_once_turned_on() {
        let mut accounts = AccountsConfig {
            enabled: true,
            users: vec![user("ana", Role::Viewer, "password1")],
        };
        assert!(accounts.validate().is_err(), "no admin");
        accounts.users.push(user("Boss", Role::Admin, "password2"));
        assert!(accounts.validate().is_ok());
        accounts.users.push(user("boss", Role::Viewer, "password3"));
        assert!(accounts.validate().is_err(), "names are case-insensitive");
        accounts.users.pop();
        accounts
            .users
            .push(user("bad name!", Role::Viewer, "password3"));
        assert!(accounts.validate().is_err());
    }

    #[test]
    fn sign_in_gives_a_session_slows_guessing_and_follows_the_user() {
        let mut accounts = AccountsConfig {
            enabled: true,
            users: vec![
                user("ana", Role::Operator, "password1"),
                user("root", Role::Admin, "password2"),
            ],
        };
        let sessions = Sessions::default();
        let (token, session) = sessions.login(&accounts, "ANA", "password1").unwrap();
        assert_eq!(
            (session.role, session.cameras.clone()),
            (Role::Operator, vec!["cam-1".to_owned()])
        );
        assert!(session.sees("cam-1") && !session.sees("cam-2"));
        assert_eq!(sessions.session(&accounts, &token), Some(session));
        assert!(sessions.session(&accounts, "forged").is_none());
        let (admin_token, admin) = sessions.login(&accounts, "root", "password2").unwrap();
        assert!(admin.cameras.is_empty(), "admins see every camera");
        // Guessing: five wrong passwords lock the name for a while.
        for _ in 0..5 {
            assert!(sessions.login(&accounts, "root", "nope").is_err());
        }
        assert!(sessions
            .login(&accounts, "root", "password2")
            .unwrap_err()
            .contains("wait"));
        // A changed role applies to live sessions; a removed user loses them.
        accounts.users[0].role = Role::Viewer;
        assert_eq!(
            sessions.session(&accounts, &token).unwrap().role,
            Role::Viewer
        );
        accounts.users.remove(0);
        assert!(sessions.session(&accounts, &token).is_none());
        sessions.end_user("ROOT");
        assert!(sessions.session(&accounts, &admin_token).is_none());
    }

    #[test]
    fn the_wait_grows_with_each_wrong_password_up_to_a_limit() {
        assert_eq!(lockout_for(4), Duration::ZERO);
        assert_eq!(lockout_for(5), Duration::from_secs(30));
        assert_eq!(lockout_for(6), Duration::from_secs(60));
        assert_eq!(lockout_for(7), Duration::from_secs(120));
        assert_eq!(lockout_for(1_000_000), MAX_LOCKOUT);
    }

    #[test]
    fn names_nobody_has_cannot_grow_the_failure_table_and_cost_a_hash() {
        let accounts = AccountsConfig {
            enabled: true,
            users: vec![user("ana", Role::Operator, "password1")],
        };
        let sessions = Sessions::default();
        let started = Instant::now();
        let _ = sessions.login(&accounts, "nobody", "whatever1");
        let unknown = started.elapsed();
        let started = Instant::now();
        let _ = sessions.login(&accounts, "ana", "whatever1");
        let wrong = started.elapsed();
        assert!(
            unknown * 4 > wrong,
            "an unknown name ran no hash: {unknown:?} against {wrong:?}"
        );
        for n in 0..20 {
            let _ = sessions.login(&accounts, &format!("ghost{n}"), "whatever1");
        }
        let table = sessions.failures.lock().unwrap().len();
        assert!(table <= 2, "table holds {table} names");
    }

    #[test]
    fn old_hashes_are_marked_for_an_upgrade_and_still_verify() {
        let salt = [7_u8; SALT_BYTES];
        let old = PasswordHash {
            salt: b64(&salt),
            hash: b64(&derive("password-old", &salt, 210_000)),
            iterations: 210_000,
        };
        assert!(old.verify("password-old") && old.needs_upgrade());
        let fresh = PasswordHash::new("password-new").unwrap();
        assert_eq!(fresh.iterations, 600_000);
        assert!(!fresh.needs_upgrade());
    }
}
