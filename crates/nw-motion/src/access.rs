//! Optional access codes: which NeuroWatch nodes may use the daemon, and how
//! many at once.
//!
//! Without a code file every node that authenticates is served. With one
//! (`--access-codes PATH`) a node must send one of the codes as its vendor
//! credential (Plugins → Vendor Credential… in NeuroWatch), and a code serves
//! at most `max_connections` nodes at the same time.
//!
//! The file never holds a code, only its PBKDF2-HMAC-SHA256 hash under the
//! file's random salt; `nw-motion access-codes …` writes it:
//!
//! ```json
//! {"salt": "<16 bytes hex>", "iterations": 600000, "codes": [
//!   {"name": "site-a", "hash": "<32 bytes hex>", "max_connections": 10}]}
//! ```
//!
//! A connection holds its place from `authenticate` until it ends (the
//! session's watchdog ends a silent one); discovery probes never authenticate
//! and take none. Places live in memory only: after a restart every recorder
//! connects again and is counted again.
//!
//! The file is read again every [`RELOAD_EVERY`]. A removed code, or one whose
//! code changed, revokes its connections; a lowered limit revokes the newest
//! ones above it. A file that cannot be read or is invalid keeps the list in
//! force (at start it stops the daemon: never serve everyone instead).
use ring::{digest, pbkdf2, rand::SecureRandom};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Most codes one file may hold.
pub const MAX_CODES: usize = 1024;
/// Longest code accepted, in bytes.
pub const MAX_CODE_BYTES: usize = 256;
/// Longest code name, in characters.
pub const MAX_NAME_CHARS: usize = 64;
/// PBKDF2 rounds for a new file (OWASP's figure for PBKDF2-HMAC-SHA256).
pub const DEFAULT_ITERATIONS: u32 = 600_000;
/// Fewest rounds a file may ask for.
pub const MIN_ITERATIONS: u32 = 100_000;
const MAX_ITERATIONS: u32 = 10_000_000;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// How often the file is read again.
pub const RELOAD_EVERY: Duration = Duration::from_secs(2);
/// Credentials whose hash is remembered, so a recorder reconnecting every
/// few seconds costs one PBKDF2 run, not one per attempt.
const CACHE_LIMIT: usize = 1024;

/// The code file, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeFile {
    pub salt: String,
    pub iterations: u32,
    pub codes: Vec<StoredCode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredCode {
    pub name: String,
    pub hash: String,
    #[serde(alias = "max_connection")]
    pub max_connections: usize,
}

/// A checked code file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Table {
    salt: [u8; 16],
    iterations: NonZeroU32,
    codes: Vec<Code>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Code {
    name: String,
    hash: [u8; 32],
    max_connections: usize,
}

impl CodeFile {
    /// An empty file with a fresh salt.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            salt: nw_analytics_sdk::identity::hex(&random::<16>()?),
            iterations: DEFAULT_ITERATIONS,
            codes: vec![],
        })
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let file: Self = serde_json::from_str(text).map_err(|error| {
            if text.trim_start().starts_with('[') {
                "codes are stored hashed: write the file with `nw-motion access-codes add`".into()
            } else {
                error.to_string()
            }
        })?;
        file.table()?;
        Ok(file)
    }

    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let text = String::from_utf8(read_bounded(path)?)
            .map_err(|_| anyhow::anyhow!("{} is not UTF-8", path.display()))?;
        Self::parse(&text).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
    }

    /// Replace the file in one step, readable by its owner only.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)? + "\n";
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    }

    /// Add `name` with `code`, or give an existing name a new code and limit.
    pub fn add(&mut self, name: &str, code: &str, max_connections: usize) -> Result<(), String> {
        let table = self.table()?;
        check_code(code)?;
        let hash = derive(&table.salt, table.iterations, code.as_bytes());
        if let Some(other) = table
            .codes
            .iter()
            .find(|c| c.hash == hash && c.name != name)
        {
            return Err(format!("that code is already in use as {:?}", other.name));
        }
        let stored = StoredCode {
            name: name.into(),
            hash: nw_analytics_sdk::identity::hex(&hash),
            max_connections,
        };
        // Checked as a whole before it replaces the file's list.
        let mut next = self.clone();
        match next.codes.iter_mut().find(|c| c.name == name) {
            Some(existing) => *existing = stored,
            None => next.codes.push(stored),
        }
        next.table()?;
        *self = next;
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.codes.len();
        self.codes.retain(|c| c.name != name);
        self.codes.len() != before
    }

    pub fn set_limit(&mut self, name: &str, max_connections: usize) -> bool {
        self.codes
            .iter_mut()
            .find(|c| c.name == name)
            .map(|c| c.max_connections = max_connections)
            .is_some()
    }

    fn table(&self) -> Result<Table, String> {
        let salt = nw_analytics_sdk::identity::unhex::<16>(&self.salt)
            .ok_or("salt must be 16 bytes of hex")?;
        if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&self.iterations) {
            return Err(format!(
                "iterations must be {MIN_ITERATIONS}–{MAX_ITERATIONS}"
            ));
        }
        if self.codes.len() > MAX_CODES {
            return Err(format!("at most {MAX_CODES} codes"));
        }
        let mut codes: Vec<Code> = Vec::with_capacity(self.codes.len());
        for stored in &self.codes {
            let name = stored.name.as_str();
            if name.trim().is_empty() || name.chars().count() > MAX_NAME_CHARS {
                return Err(format!("code names are 1–{MAX_NAME_CHARS} characters"));
            }
            let hash = nw_analytics_sdk::identity::unhex::<32>(&stored.hash)
                .ok_or_else(|| format!("{name:?}: hash must be 32 bytes of hex"))?;
            if codes.iter().any(|c| c.name == name) {
                return Err(format!("{name:?} appears twice"));
            }
            if codes.iter().any(|c| c.hash == hash) {
                return Err(format!("{name:?} has the same code as another entry"));
            }
            codes.push(Code {
                name: name.into(),
                hash,
                max_connections: stored.max_connections,
            });
        }
        Ok(Table {
            salt,
            iterations: NonZeroU32::new(self.iterations).ok_or("iterations must not be 0")?,
            codes,
        })
    }
}

/// A new random code to hand to a recorder: 128 bits, as hex.
pub fn new_code() -> io::Result<String> {
    Ok(nw_analytics_sdk::identity::hex(&random::<16>()?))
}

/// What a code must look like.
pub fn check_code(code: &str) -> Result<(), String> {
    if code.len() < 8 || code.len() > MAX_CODE_BYTES || code.trim() != code {
        return Err(format!(
            "a code is 8–{MAX_CODE_BYTES} bytes, without leading or trailing spaces"
        ));
    }
    Ok(())
}

/// Why a node was turned away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// No code, or one that is not in the file.
    Code,
    /// The code already serves its `max_connections` nodes.
    Full { max_connections: usize },
}

/// The daemon's codes and who holds a place under each.
#[derive(Debug, Default)]
pub struct Access {
    /// The code file (`None`: no list, every node is served).
    path: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    table: Option<Table>,
    /// Connections by code name, oldest first.
    holders: HashMap<String, Vec<Holder>>,
    next_id: u64,
    /// SHA-256 of a credential → its PBKDF2 hash under the current salt.
    cache: HashMap<[u8; 32], [u8; 32]>,
    /// The file's bytes as last applied, and as last refused.
    applied: Vec<u8>,
    refused: Option<Vec<u8>>,
}

#[derive(Debug)]
struct Holder {
    id: u64,
    revoked: Arc<AtomicBool>,
}

/// A connection's place under its code, given back when dropped.
#[derive(Debug)]
pub struct Admission {
    access: Arc<Access>,
    held: Option<(String, u64)>,
    revoked: Arc<AtomicBool>,
}

impl Admission {
    /// The code was removed or changed, or its limit lowered below this
    /// connection: end it.
    pub fn revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// The name of the code it holds a place under.
    pub fn code_name(&self) -> Option<&str> {
        self.held.as_ref().map(|(name, _)| name.as_str())
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        let Some((name, id)) = &self.held else {
            return;
        };
        if let Ok(mut state) = self.access.state.lock() {
            if let Some(holders) = state.holders.get_mut(name) {
                holders.retain(|holder| holder.id != *id);
            }
        }
    }
}

impl Access {
    /// Serve only nodes with a code from the file at `path`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let access = Self {
            path: Some(path.to_owned()),
            state: Mutex::default(),
        };
        let bytes = read_bounded(path)?;
        let table = parse_bytes(&bytes).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        if let Ok(mut state) = access.state.lock() {
            state.table = Some(table);
            state.applied = bytes;
        }
        Ok(access)
    }

    /// Whether a code file is in force.
    pub fn restricted(&self) -> bool {
        self.path.is_some()
    }

    /// Read the file again every [`RELOAD_EVERY`] for the daemon's life.
    pub fn watch(self: &Arc<Self>) -> io::Result<()> {
        if self.path.is_none() {
            return Ok(());
        }
        let access = Arc::clone(self);
        std::thread::Builder::new()
            .name("access-codes".into())
            .spawn(move || loop {
                std::thread::sleep(RELOAD_EVERY);
                access.reload();
            })
            .map(|_| ())
    }

    /// Apply the file if it changed. Returns whether a new list took effect.
    pub fn reload(&self) -> bool {
        let Some(path) = &self.path else {
            return false;
        };
        let read = read_bounded(path);
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let (bytes, problem) = match read {
            Ok(bytes) if bytes == state.applied => return false,
            Ok(bytes) => match parse_bytes(&bytes) {
                Ok(table) => {
                    state.apply(table);
                    state.applied = bytes;
                    state.refused = None;
                    tracing::info!(path = %path.display(), "Access codes reloaded");
                    return true;
                }
                Err(problem) => (bytes, problem),
            },
            Err(error) => (vec![], error.to_string()),
        };
        // Said once per version of the file, not every few seconds.
        if state.refused.as_ref() != Some(&bytes) {
            tracing::warn!(path = %path.display(), %problem, "Access codes not reloaded; the previous list stays in force");
        }
        state.refused = Some(bytes);
        false
    }

    /// Admit a node that sent `credential`, taking a place under its code.
    pub fn admit(self: &Arc<Self>, credential: Option<&str>) -> Result<Admission, Denied> {
        let credential = credential.unwrap_or("");
        let key: [u8; 32] = digest::digest(&digest::SHA256, credential.as_bytes())
            .as_ref()
            .try_into()
            .map_err(|_| Denied::Code)?;
        // PBKDF2 runs without the lock (it takes a moment); the list may
        // change meanwhile, so the hash is checked against the salt it used.
        for _ in 0..3 {
            let (salt, iterations, cached) = {
                let state = self.state.lock().map_err(|_| Denied::Code)?;
                let Some(table) = &state.table else {
                    return Ok(Admission {
                        access: Arc::clone(self),
                        held: None,
                        revoked: Arc::default(),
                    });
                };
                (table.salt, table.iterations, state.cache.get(&key).copied())
            };
            let hash = match cached {
                Some(hash) => hash,
                None if credential.len() > MAX_CODE_BYTES => return Err(Denied::Code),
                None => derive(&salt, iterations, credential.as_bytes()),
            };
            let mut state = self.state.lock().map_err(|_| Denied::Code)?;
            let state = &mut *state;
            let Some(table) = &state.table else {
                continue;
            };
            if table.salt != salt || table.iterations != iterations {
                continue;
            }
            if state.cache.len() >= CACHE_LIMIT {
                state.cache.clear();
            }
            state.cache.insert(key, hash);
            // Every code is compared, each in constant time.
            let mut found = None;
            for code in &table.codes {
                if same(&code.hash, &hash) {
                    found = Some(code);
                }
            }
            let code = found.ok_or(Denied::Code)?;
            let holders = state.holders.entry(code.name.clone()).or_default();
            if holders.len() >= code.max_connections {
                return Err(Denied::Full {
                    max_connections: code.max_connections,
                });
            }
            state.next_id += 1;
            let revoked = Arc::new(AtomicBool::new(false));
            holders.push(Holder {
                id: state.next_id,
                revoked: Arc::clone(&revoked),
            });
            return Ok(Admission {
                access: Arc::clone(self),
                held: Some((code.name.clone(), state.next_id)),
                revoked,
            });
        }
        Err(Denied::Code)
    }

    /// Connections holding a place under `name` now.
    pub fn holding(&self, name: &str) -> usize {
        self.state
            .lock()
            .map_or(0, |state| state.holders.get(name).map_or(0, Vec::len))
    }
}

impl State {
    /// Take a new list into force, revoking places it no longer grants.
    fn apply(&mut self, table: Table) {
        if self.table.as_ref().map_or(true, |old| {
            old.salt != table.salt || old.iterations != table.iterations
        }) {
            self.cache.clear();
        }
        let old = self.table.take();
        for (name, holders) in &mut self.holders {
            let before = old
                .as_ref()
                .and_then(|t| t.codes.iter().find(|c| &c.name == name));
            let now = table.codes.iter().find(|c| &c.name == name);
            let keep = match (before, now) {
                (Some(before), Some(now)) if before.hash == now.hash => now.max_connections,
                _ => 0,
            };
            if holders.len() > keep {
                tracing::info!(code = %name, revoked = holders.len() - keep, "Access code changed; closing its connections over the new list");
                for holder in holders.drain(keep..) {
                    holder.revoked.store(true, Ordering::Release);
                }
            }
        }
        self.holders.retain(|_, holders| !holders.is_empty());
        self.table = Some(table);
    }
}

fn parse_bytes(bytes: &[u8]) -> Result<Table, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8".to_owned())?;
    CodeFile::parse(text)?.table()
}

fn read_bounded(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = vec![];
    std::fs::File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::other("the code file is larger than 1 MiB"));
    }
    Ok(bytes)
}

fn derive(salt: &[u8; 16], iterations: NonZeroU32, code: &[u8]) -> [u8; 32] {
    let mut hash = [0; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        code,
        &mut hash,
    );
    hash
}

fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("no system randomness"))?;
    Ok(bytes)
}

fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn file(codes: &[(&str, &str, usize)]) -> (TempFile, CodeFile) {
        let mut file = CodeFile::new().unwrap();
        file.iterations = MIN_ITERATIONS;
        for (name, code, max) in codes {
            file.add(name, code, *max).unwrap();
        }
        let path =
            std::env::temp_dir().join(format!("nw-motion-codes-{}.json", new_code().unwrap()));
        file.write(&path).unwrap();
        (TempFile(path), file)
    }

    fn access(path: &Path) -> Arc<Access> {
        Arc::new(Access::load(path).unwrap())
    }

    #[test]
    fn without_a_file_every_node_is_served() {
        let access = Arc::new(Access::default());
        assert!(!access.restricted());
        let held: Vec<_> = (0..100).map(|_| access.admit(None).unwrap()).collect();
        assert_eq!(held.len(), 100);
        assert!(!held[0].revoked());
    }

    #[test]
    fn the_file_holds_salted_hashes_never_codes() {
        let (path, stored) = file(&[("site-a", "alpha-secret", 1)]);
        let text = std::fs::read_to_string(&path.0).unwrap();
        assert!(!text.contains("alpha-secret"));
        let hash = &stored.codes[0].hash;
        assert_eq!(hash.len(), 64);
        // The same code under another file's salt hashes differently.
        let (_other, other) = file(&[("site-a", "alpha-secret", 1)]);
        assert_ne!(&other.codes[0].hash, hash);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path.0).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_code_serves_up_to_its_limit_and_frees_places_on_disconnect() {
        let (path, _) = file(&[("site-a", "alpha-secret", 2)]);
        let access = access(&path.0);
        let first = access.admit(Some("alpha-secret")).unwrap();
        assert_eq!(first.code_name(), Some("site-a"));
        let _second = access.admit(Some("alpha-secret")).unwrap();
        assert_eq!(
            access.admit(Some("alpha-secret")).unwrap_err(),
            Denied::Full { max_connections: 2 }
        );
        drop(first);
        assert_eq!(access.holding("site-a"), 1);
        assert!(access.admit(Some("alpha-secret")).is_ok());
    }

    #[test]
    fn unknown_or_missing_codes_are_refused() {
        let (path, _) = file(&[("site-a", "alpha-secret", 1)]);
        let access = access(&path.0);
        for wrong in [Some("alpha-secre"), Some("alpha-secrett"), Some(""), None] {
            assert_eq!(access.admit(wrong).unwrap_err(), Denied::Code);
        }
        assert_eq!(
            access.admit(Some(&"x".repeat(5000))).unwrap_err(),
            Denied::Code
        );
    }

    #[test]
    fn edits_take_effect_without_a_restart() {
        let (path, mut stored) =
            file(&[("site-a", "alpha-secret", 3), ("site-b", "beta-secret", 1)]);
        let access = access(&path.0);
        let a: Vec<_> = (0..3)
            .map(|_| access.admit(Some("alpha-secret")).unwrap())
            .collect();
        let b = access.admit(Some("beta-secret")).unwrap();
        assert!(!access.reload(), "unchanged file");

        // Lower site-a to one: the two newest go; remove site-b: it goes.
        stored.set_limit("site-a", 1);
        stored.remove("site-b");
        stored.write(&path.0).unwrap();
        assert!(access.reload());
        assert!(!a[0].revoked());
        assert!(a[1].revoked() && a[2].revoked() && b.revoked());
        assert_eq!(access.holding("site-a"), 1);
        assert_eq!(access.admit(Some("beta-secret")).unwrap_err(), Denied::Code);

        // A new code for site-a: the old one stops working, its holder goes.
        stored.add("site-a", "gamma-secret", 1).unwrap();
        stored.write(&path.0).unwrap();
        assert!(access.reload());
        assert!(a[0].revoked());
        assert_eq!(
            access.admit(Some("alpha-secret")).unwrap_err(),
            Denied::Code
        );
        let _c = access.admit(Some("gamma-secret")).unwrap();
        drop(a);
        assert_eq!(
            access.holding("site-a"),
            1,
            "revoked places are not counted twice"
        );
    }

    #[test]
    fn a_broken_or_missing_file_keeps_the_list_in_force() {
        let (path, _) = file(&[("site-a", "alpha-secret", 1)]);
        let access = access(&path.0);
        std::fs::write(&path.0, "{not json").unwrap();
        assert!(!access.reload());
        std::fs::remove_file(&path.0).unwrap();
        assert!(!access.reload());
        assert_eq!(access.admit(Some("nope-nope")).unwrap_err(), Denied::Code);
        assert!(access.admit(Some("alpha-secret")).is_ok());
    }

    #[test]
    fn files_are_checked() {
        let mut stored = CodeFile::new().unwrap();
        stored.iterations = MIN_ITERATIONS;
        stored.add("a", "alpha-secret", 1).unwrap();
        assert!(
            stored.add("b", "alpha-secret", 1).is_err(),
            "one code, two names"
        );
        assert!(stored.add("c", "short", 1).is_err());
        assert!(stored.add("", "charlie-secret", 1).is_err());
        assert!(CodeFile::parse(r#"[{"code":"a","max_connections":1}]"#)
            .unwrap_err()
            .contains("hashed"));
        let mut weak = stored.clone();
        weak.iterations = 1000;
        assert!(CodeFile::parse(&serde_json::to_string(&weak).unwrap()).is_err());
        let mut twice = stored.clone();
        twice.codes.push(twice.codes[0].clone());
        assert!(CodeFile::parse(&serde_json::to_string(&twice).unwrap()).is_err());
        let text = serde_json::to_string(&stored).unwrap();
        assert_eq!(CodeFile::parse(&text).unwrap(), stored);
        assert!(CodeFile::parse(&text.replacen('{', r#"{"extra":1,"#, 1)).is_err());
    }
}
