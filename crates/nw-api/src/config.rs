//! Hosted control-plane configuration.
//!
//! Every value is explicit. There is no default bind address, no default
//! database location and no built-in administrator: a deployment that does not
//! say where to listen and where to store its data does not start, and an
//! installation without a configured admin credential exposes no admin surface
//! at all.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Environment variable naming the socket address to listen on.
pub const BIND_ENV: &str = "NW_API_BIND";
/// Environment variable naming the SQLite database file.
pub const DATABASE_ENV: &str = "NW_API_DATABASE";
/// Environment variable carrying the administrator bearer token.
pub const ADMIN_TOKEN_ENV: &str = "NW_API_ADMIN_TOKEN";

/// Names the external identity provider that turns a browser credential into a
/// principal. **No provider is implemented**, so any value is a configuration
/// error: a deployment that believes it has identity must fail to start rather
/// than run with none. See [`crate::identity`].
pub const IDENTITY_PROVIDER_ENV: &str = "NW_API_IDENTITY_PROVIDER";

/// url-safe base64 ed25519 seed (32 bytes) used to sign media grants.
pub const GRANT_SIGNING_KEY_ENV: &str = "NW_API_GRANT_SIGNING_KEY";
/// How long an issued media grant is valid, in seconds.
pub const GRANT_TTL_ENV: &str = "NW_API_GRANT_TTL_SECONDS";
/// Bearer credential the SFU edge presents when it spends a grant.
pub const SFU_TOKEN_ENV: &str = "NW_API_SFU_TOKEN";
/// LiveKit server API key and secret. Both remain server-side; browser tokens
/// are separately minted, room-scoped participant credentials.
pub const LIVEKIT_API_KEY_ENV: &str = "NW_API_LIVEKIT_API_KEY";
pub const LIVEKIT_API_SECRET_ENV: &str = "NW_API_LIVEKIT_API_SECRET";
/// Opaque identifier of the SFU region; also the grant audience.
pub const SFU_REGION_ENV: &str = "NW_API_SFU_REGION";
/// HTTPS base URL a browser negotiates against (WHEP).
pub const SFU_SIGNALING_BASE_ENV: &str = "NW_API_SFU_SIGNALING_BASE";
/// Comma-separated TURN/STUN URLs offered to peers.
pub const TURN_URLS_ENV: &str = "NW_API_TURN_URLS";
/// Long-lived TURN static-auth secret. Never leaves this process: only
/// per-session credentials derived from it are handed out.
pub const TURN_SECRET_ENV: &str = "NW_API_TURN_SECRET";

/// Shortest accepted admin token. Long enough that a guessable value cannot be
/// configured by accident.
pub const MIN_ADMIN_TOKEN_LEN: usize = 32;
/// The same floor for the SFU edge credential and the TURN static secret.
pub const MIN_SECRET_LEN: usize = 32;

/// Bounds on the configured grant lifetime. The ceiling is the contract's own
/// maximum; the floor keeps a deployment from configuring grants that expire
/// before a browser can present them.
pub const MIN_GRANT_TTL_SECONDS: i64 = 1;
pub const MAX_GRANT_TTL_SECONDS: i64 = nw_cloud_protocol::media_session::MAX_GRANT_TTL_SECONDS;
pub const DEFAULT_GRANT_TTL_SECONDS: i64 = 30;

/// How much of a session must remain for renewing it to be worth doing.
///
/// A renewal's credentials are all capped at the session's own end, so a session
/// with two seconds left can only be re-granted two-second credentials: the
/// browser would spend its renewal on a grant that expires mid-handshake, and
/// the dying session would go on holding a concurrency slot it can never use.
/// Below this floor the control plane ends the session instead.
///
/// This is a floor on the *session* remainder, not on the configured grant TTL:
/// a deployment that chooses a one-second [`GRANT_TTL_ENV`] still gets
/// one-second grants for as long as its sessions are healthy.
pub const MIN_RENEWAL_WINDOW_SECONDS: i64 = 5;

/// Everything needed to hand a peer to a media edge.
///
/// All of it or none of it: a half-configured edge would let the service issue
/// grants nobody can spend. There is no default and no local stand-in — the
/// product choice behind these values is unresolved
/// (`docs/adr/0002-sfu-and-turn-selection.md`).
#[derive(Clone, PartialEq, Eq)]
pub struct MediaEdgeConfig {
    /// ed25519 signing seed for grants.
    pub grant_signing_seed: [u8; 32],
    pub grant_ttl_seconds: i64,
    pub sfu_token: String,
    pub livekit_api_key: String,
    pub livekit_api_secret: String,
    pub sfu_region: String,
    pub sfu_signaling_base: String,
    pub turn_urls: Vec<String>,
    pub turn_secret: String,
}

/// Hand-written so that a configuration dump — or a `tracing` field, or a panic
/// message — cannot print the signing seed, the edge credential or the TURN
/// static secret.
impl std::fmt::Debug for MediaEdgeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaEdgeConfig")
            .field("grant_signing_seed", &"<redacted>")
            .field("grant_ttl_seconds", &self.grant_ttl_seconds)
            .field("sfu_token", &"<redacted>")
            .field("livekit_api_key", &"<redacted>")
            .field("livekit_api_secret", &"<redacted>")
            .field("sfu_region", &self.sfu_region)
            .field("sfu_signaling_base", &self.sfu_signaling_base)
            .field("turn_urls", &self.turn_urls)
            .field("turn_secret", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{0} is required; there is no default")]
    Missing(&'static str),
    #[error("{name} is not usable: {reason}")]
    Invalid { name: &'static str, reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiConfig {
    pub bind: SocketAddr,
    pub database: PathBuf,
    /// When `None` the admin routes are not mounted.
    pub admin_token: Option<String>,
    /// When `None` the media routes are mounted but answer
    /// `503 media_edge_unavailable`.
    pub media_edge: Option<MediaEdgeConfig>,
}

impl ApiConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Build the configuration from an arbitrary variable source. Empty values
    /// are treated as unset so that `VAR=` cannot silently mean "default".
    pub fn from_vars(get: impl Fn(&'static str) -> Option<String>) -> Result<Self, ConfigError> {
        let read = |name: &'static str| -> Option<String> {
            get(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let raw_bind = read(BIND_ENV).ok_or(ConfigError::Missing(BIND_ENV))?;
        let bind: SocketAddr = raw_bind.parse().map_err(|error| ConfigError::Invalid {
            name: BIND_ENV,
            reason: format!("{raw_bind} is not a socket address: {error}"),
        })?;
        let database = PathBuf::from(read(DATABASE_ENV).ok_or(ConfigError::Missing(DATABASE_ENV))?);
        if database.as_os_str() == ":memory:" {
            return Err(ConfigError::Invalid {
                name: DATABASE_ENV,
                reason: "an in-memory database would lose every tenant on restart".into(),
            });
        }
        let admin_token = read(ADMIN_TOKEN_ENV);
        if let Some(token) = &admin_token {
            if token.len() < MIN_ADMIN_TOKEN_LEN {
                return Err(ConfigError::Invalid {
                    name: ADMIN_TOKEN_ENV,
                    reason: format!("the token must be at least {MIN_ADMIN_TOKEN_LEN} characters"),
                });
            }
        }
        if let Some(named) = read(IDENTITY_PROVIDER_ENV) {
            // Named but unimplemented. Starting anyway would mean a deployment
            // that thinks it has identity and has none.
            return Err(ConfigError::Invalid {
                name: IDENTITY_PROVIDER_ENV,
                reason: format!(
                    "no identity provider is implemented, so {named:?} cannot be honoured; \
                     a host must inject a PrincipalVerifier through Server::bind_with"
                ),
            });
        }
        let media_edge = Self::media_edge_from(&read)?;
        Ok(Self {
            bind,
            database,
            admin_token,
            media_edge,
        })
    }

    /// Read the media edge as one unit. Absent entirely is fine; partially
    /// present is a configuration error naming the first missing variable.
    fn media_edge_from(
        read: &impl Fn(&'static str) -> Option<String>,
    ) -> Result<Option<MediaEdgeConfig>, ConfigError> {
        const REQUIRED: [&str; 8] = [
            GRANT_SIGNING_KEY_ENV,
            SFU_TOKEN_ENV,
            SFU_REGION_ENV,
            SFU_SIGNALING_BASE_ENV,
            TURN_URLS_ENV,
            LIVEKIT_API_KEY_ENV,
            LIVEKIT_API_SECRET_ENV,
            TURN_SECRET_ENV,
        ];
        let present = REQUIRED.iter().filter(|name| read(name).is_some()).count();
        if present == 0 {
            return Ok(None);
        }
        let require = |name: &'static str| read(name).ok_or(ConfigError::Missing(name));

        let raw_seed = require(GRANT_SIGNING_KEY_ENV)?;
        let seed = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            raw_seed.as_bytes(),
        )
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or(ConfigError::Invalid {
            name: GRANT_SIGNING_KEY_ENV,
            reason: "expected 32 bytes of url-safe base64 without padding".into(),
        })?;

        let grant_ttl_seconds = match read(GRANT_TTL_ENV) {
            Some(raw) => raw.parse::<i64>().map_err(|_| ConfigError::Invalid {
                name: GRANT_TTL_ENV,
                reason: "expected a whole number of seconds".into(),
            })?,
            None => DEFAULT_GRANT_TTL_SECONDS,
        };
        if !(MIN_GRANT_TTL_SECONDS..=MAX_GRANT_TTL_SECONDS).contains(&grant_ttl_seconds) {
            return Err(ConfigError::Invalid {
                name: GRANT_TTL_ENV,
                reason: format!(
                    "must be between {MIN_GRANT_TTL_SECONDS} and {MAX_GRANT_TTL_SECONDS} seconds"
                ),
            });
        }

        let sfu_token = require(SFU_TOKEN_ENV)?;
        let livekit_api_key = require(LIVEKIT_API_KEY_ENV)?;
        let livekit_api_secret = require(LIVEKIT_API_SECRET_ENV)?;
        let turn_secret = require(TURN_SECRET_ENV)?;
        for (name, secret) in [
            (SFU_TOKEN_ENV, &sfu_token),
            (LIVEKIT_API_SECRET_ENV, &livekit_api_secret),
            (TURN_SECRET_ENV, &turn_secret),
        ] {
            if secret.len() < MIN_SECRET_LEN {
                return Err(ConfigError::Invalid {
                    name,
                    reason: format!("must be at least {MIN_SECRET_LEN} characters"),
                });
            }
        }

        let sfu_region = require(SFU_REGION_ENV)?;
        let sfu_signaling_base = require(SFU_SIGNALING_BASE_ENV)?;
        let turn_urls: Vec<String> = require(TURN_URLS_ENV)?
            .split(',')
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty())
            .collect();

        // Validate the endpoint against the shared contract now, at startup,
        // rather than discovering at grant time that we would be handing a
        // browser a LAN address.
        let endpoint = nw_cloud_protocol::media_session::SfuEndpoint {
            region_id: sfu_region.clone(),
            signaling_url: sfu_signaling_base.clone(),
            ice_servers: vec![nw_cloud_protocol::media_session::IceServer {
                urls: turn_urls.clone(),
                username: None,
                credential: None,
                credential_ttl_seconds: None,
            }],
        };
        endpoint.validate().map_err(|error| ConfigError::Invalid {
            name: SFU_SIGNALING_BASE_ENV,
            reason: format!("the media edge is not a usable public endpoint: {error}"),
        })?;

        Ok(Some(MediaEdgeConfig {
            grant_signing_seed: seed,
            grant_ttl_seconds,
            sfu_token,
            livekit_api_key,
            livekit_api_secret,
            sfu_region,
            sfu_signaling_base,
            turn_urls,
            turn_secret,
        }))
    }
}
