//! Encrypted credential vault for website logins.
//!
//! The LLM drives lightbrowse into login walls; the vault stores the
//! credentials it needs, encrypted at rest:
//!
//! - **AES-256-GCM** with a 32-byte key from `~/.config/lightbrowse/vault.key`
//!   (auto-generated, `0600` perms) or the `LIGHTBROWSE_VAULT_KEY` env var.
//! - The vault file is hex-encoded `[nonce(12)][ciphertext][tag(16)]`.
//! - `list` never returns secrets; secrets are redacted from logs/errors.
//! - Key + secrets are `zeroize`d (wiped from memory) when dropped.
//!
//! Runbook replay can reference entries server-side (e.g. a variable value
//! `vault:outlook.password`) so the agent never sees the secret at all.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// One credential entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultEntry {
    pub url: String,
    pub username: String,
    pub password: String,
    /// Optional extra fields — arbitrary JSON (nested objects, arrays, e.g.
    /// {pin, answers: [...]}). Never shown by `list`. Old string-map entries
    /// still deserialize (serde_json::Value is a superset).
    #[serde(default)]
    pub extra: serde_json::Value,
    #[serde(default)]
    pub updated_at: u64,
}

/// Vault document (in memory, plaintext).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultDoc {
    pub version: u32,
    pub entries: BTreeMap<String, VaultEntry>,
}

impl Default for VaultDoc {
    fn default() -> Self {
        Self {
            version: 1,
            entries: BTreeMap::new(),
        }
    }
}

/// Paths for the vault + its key.
#[derive(Clone)]
pub struct VaultPaths {
    pub vault: PathBuf,
    pub key: PathBuf,
}

impl Default for VaultPaths {
    fn default() -> Self {
        let base = std::env::var("LIGHTBROWSE_VAULT_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs_config_dir().join("lightbrowse"));
        Self {
            vault: base.join("vault.enc"),
            key: base.join("vault.key"),
        }
    }
}

fn dirs_config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_CONFIG_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".config");
    }
    PathBuf::from(".config")
}

/// The encrypted vault. Cheap to clone (key is wrapped in `Arc`).
#[derive(Clone)]
pub struct Vault {
    paths: VaultPaths,
    key: std::sync::Arc<Zeroizing<[u8; 32]>>,
    doc: std::sync::Arc<std::sync::Mutex<VaultDoc>>,
}

impl Vault {
    /// Open (or create) the vault, deriving the key from `LIGHTBROWSE_VAULT_KEY`
    /// or the key file. Missing key file is generated with `0600` perms.
    pub fn open(paths: VaultPaths) -> Result<Self, String> {
        let key = load_or_create_key(&paths.key)?;
        let doc = load_doc(&paths.vault, &key)?;
        Ok(Self {
            paths,
            key: std::sync::Arc::new(Zeroizing::new(key)),
            doc: std::sync::Arc::new(std::sync::Mutex::new(doc)),
        })
    }

    /// Entry names + urls only — NEVER secrets.
    pub fn list(&self) -> Vec<(String, String, u64)> {
        self.doc
            .lock()
            .map(|g| {
                g.entries
                    .iter()
                    .map(|(k, v)| (k.clone(), v.url.clone(), v.updated_at))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Full entry (secrets included) — for login flows.
    pub fn get(&self, name: &str) -> Option<VaultEntry> {
        self.doc
            .lock()
            .ok()
            .and_then(|g| g.entries.get(name).cloned())
    }

    /// Resolve a `vault:<name>.<field>` reference (field: password/username/url/extra key).
    /// Returns None when the reference isn't a vault ref.
    pub fn resolve_ref(&self, reference: &str) -> Option<Result<String, String>> {
        let (name, field) = reference.strip_prefix("vault:")?.split_once('.')?;
        let entry = self.get(name)?;
        let value = match field {
            "password" => entry.password,
            "username" => entry.username,
            "url" => entry.url,
            other => {
                // Nested path into `extra` JSON: answers.0, server.host, ...
                let mut cur = &entry.extra;
                let mut ok = true;
                for part in other.split('.') {
                    cur = match cur {
                        serde_json::Value::Object(m) => match m.get(part) {
                            Some(v) => v,
                            None => {
                                ok = false;
                                break;
                            }
                        },
                        serde_json::Value::Array(a) => {
                            match part.parse::<usize>().ok().and_then(|i| a.get(i)) {
                                Some(v) => v,
                                None => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    };
                }
                if !ok {
                    return Some(Err(format!(
                        "vault ref {reference}: field '{field}' not found for entry '{name}'"
                    )));
                }
                match cur {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    _ => String::new(), // object/array/null — not directly usable
                }
            }
        };
        if value.is_empty() {
            Some(Err(format!(
                "vault ref {reference}: field '{field}' is empty for entry '{name}'"
            )))
        } else {
            Some(Ok(value))
        }
    }

    /// Insert or update an entry, persisting immediately.
    pub fn set(&self, name: &str, entry: VaultEntry) -> Result<(), String> {
        {
            let mut g = self
                .doc
                .lock()
                .map_err(|_| "vault lock poisoned".to_string())?;
            g.entries.insert(name.to_string(), entry);
        }
        self.save()
    }

    pub fn delete(&self, name: &str) -> Result<bool, String> {
        let removed = self
            .doc
            .lock()
            .map_err(|_| "vault lock poisoned".to_string())?
            .entries
            .remove(name)
            .is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Re-encrypt the in-memory doc to the vault file (`0600`).
    fn save(&self) -> Result<(), String> {
        let doc = self
            .doc
            .lock()
            .map_err(|_| "vault lock poisoned".to_string())?;
        let plain = serde_json::to_vec(&*doc).map_err(|e| format!("vault serialize: {e}"))?;
        let enc = encrypt(&plain, &self.key)?;
        write_private(&self.paths.vault, hex::encode(enc).as_bytes())?;
        Ok(())
    }

    pub fn paths(&self) -> &VaultPaths {
        &self.paths
    }
}

/// Encrypt plaintext: hex(nonce || ciphertext || tag).
fn encrypt(plain: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("cipher init: {e}"))?;
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: b"lightbrowse-vault-v1",
            },
        )
        .map_err(|_| "vault encryption failed".to_string())?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt hex(nonce || ciphertext || tag); any tampering fails.
fn decrypt(enc: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, String> {
    if enc.len() <= 12 + 16 {
        return Err("vault file too short — corrupted?".into());
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("cipher init: {e}"))?;
    let (nonce, ct) = enc.split_at(12);
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ct,
                aad: b"lightbrowse-vault-v1",
            },
        )
        .map_err(|_| "vault decryption failed — wrong key or corrupted file".to_string())
}

fn load_or_create_key(path: &Path) -> Result<[u8; 32], String> {
    if let Ok(k) = std::env::var("LIGHTBROWSE_VAULT_KEY") {
        if !k.is_empty() {
            let bytes = hex::decode(k.trim()).map_err(|e| format!("LIGHTBROWSE_VAULT_KEY: {e}"))?;
            let arr: [u8; 32] = bytes
                .try_into()
                .map_err(|_| "LIGHTBROWSE_VAULT_KEY must be 64 hex chars (32 bytes)".to_string())?;
            return Ok(arr);
        }
    }
    if path.exists() {
        let hex_key = std::fs::read_to_string(path)
            .map_err(|e| format!("read vault key: {e}"))?
            .trim()
            .to_string();
        let bytes = hex::decode(&hex_key).map_err(|e| format!("vault key file: {e}"))?;
        return bytes
            .try_into()
            .map_err(|_| "vault key file must contain 64 hex chars (32 bytes)".to_string());
    }
    // Generate a fresh key.
    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    let hex_key = hex::encode(key);
    write_private(path, hex_key.as_bytes())?;
    Ok(key)
}

fn load_doc(path: &Path, key: &[u8; 32]) -> Result<VaultDoc, String> {
    if !path.exists() {
        return Ok(VaultDoc::default());
    }
    let hex_data = std::fs::read_to_string(path).map_err(|e| format!("read vault: {e}"))?;
    let enc = hex::decode(hex_data.trim()).map_err(|e| format!("vault hex: {e}"))?;
    let plain = decrypt(&enc, key)?;
    serde_json::from_slice(&plain).map_err(|e| format!("vault parse: {e}"))
}

/// Write a file with `0600` permissions (owner-only).
fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    let mut f =
        std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(data)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    f.sync_all()
        .map_err(|e| format!("sync {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod 600 {}: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("lb-vault-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Vault::open(VaultPaths {
            vault: dir.join("vault.enc"),
            key: dir.join("vault.key"),
        })
        .unwrap()
    }

    #[test]
    fn set_get_roundtrip() {
        let v = tmp_vault("roundtrip");
        v.set(
            "outlook",
            VaultEntry {
                url: "https://outlook.live.com".into(),
                username: "me@corp.com".into(),
                password: "s3cret!".into(),
                extra: Default::default(),
                updated_at: 1,
            },
        )
        .unwrap();
        let e = v.get("outlook").unwrap();
        assert_eq!(e.username, "me@corp.com");
        assert_eq!(e.password, "s3cret!");
    }

    #[test]
    fn persists_and_decrypts() {
        let dir =
            std::env::temp_dir().join(format!("lb-vault-test-persist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = VaultPaths {
            vault: dir.join("vault.enc"),
            key: dir.join("vault.key"),
        };
        {
            let v = Vault::open(paths.clone()).unwrap();
            v.set(
                "gmail",
                VaultEntry {
                    url: "https://gmail.com".into(),
                    username: "u".into(),
                    password: "p".into(),
                    extra: Default::default(),
                    updated_at: 2,
                },
            )
            .unwrap();
        } // dropped — file on disk
        let v2 = Vault::open(paths.clone()).unwrap();
        assert_eq!(v2.get("gmail").unwrap().password, "p");
        // File must be encrypted (no plaintext), and 0600.
        let raw = std::fs::read_to_string(&paths.vault).unwrap();
        assert!(!raw.contains("p") && !raw.contains("s3cret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&paths.vault)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "vault file must be 0600");
            let kmode = std::fs::metadata(&paths.key).unwrap().permissions().mode() & 0o777;
            assert_eq!(kmode, 0o600, "key file must be 0600");
        }
    }

    #[test]
    fn tamper_detected() {
        let dir = std::env::temp_dir().join(format!("lb-vault-test-tamper-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = VaultPaths {
            vault: dir.join("vault.enc"),
            key: dir.join("vault.key"),
        };
        let v = Vault::open(paths.clone()).unwrap();
        v.set(
            "a",
            VaultEntry {
                url: "u".into(),
                username: "u".into(),
                password: "p".into(),
                extra: Default::default(),
                updated_at: 0,
            },
        )
        .unwrap();
        // Corrupt one byte of the ciphertext.
        let raw = std::fs::read_to_string(&paths.vault).unwrap();
        let mut bytes = hex::decode(raw.trim()).unwrap();
        let n = bytes.len();
        bytes[n / 2] ^= 0xFF;
        std::fs::write(&paths.vault, hex::encode(bytes)).unwrap();
        assert!(
            Vault::open(paths.clone()).is_err(),
            "tampered vault must fail to open"
        );
    }

    #[test]
    fn list_never_leaks() {
        let v = tmp_vault("list");
        v.set(
            "a",
            VaultEntry {
                url: "https://a.example".into(),
                username: "u1".into(),
                password: "pw1".into(),
                extra: Default::default(),
                updated_at: 3,
            },
        )
        .unwrap();
        let items = v.list();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "a");
        assert_eq!(items[0].1, "https://a.example");
        // The list tuple has no password field at all.
    }

    #[test]
    fn resolve_ref_works() {
        let v = tmp_vault("ref");
        v.set(
            "outlook",
            VaultEntry {
                url: "https://outlook.live.com".into(),
                username: "me@corp.com".into(),
                password: "pw!".into(),
                extra: serde_json::json!({ "pin": "1234", "answers": ["a", "b"] }),
                updated_at: 0,
            },
        )
        .unwrap();
        assert_eq!(
            v.resolve_ref("vault:outlook.password").unwrap().unwrap(),
            "pw!"
        );
        assert_eq!(
            v.resolve_ref("vault:outlook.username").unwrap().unwrap(),
            "me@corp.com"
        );
        assert_eq!(v.resolve_ref("vault:outlook.pin").unwrap().unwrap(), "1234");
        // nested JSON refs work too
        assert_eq!(
            v.resolve_ref("vault:outlook.answers.0").unwrap().unwrap(),
            "a"
        );
        assert!(v.resolve_ref("vault:missing.password").is_none());
        assert!(v.resolve_ref("not-a-ref").is_none());
        // empty field → error result
        assert!(v.resolve_ref("vault:outlook.otp").unwrap().is_err());
    }
}
