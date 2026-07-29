// Session token storage.
//
// §3.1 says to keep the token in the platform keychain. On Linux that is the
// Secret Service, which is normally provided by gnome-keyring or (on KDE)
// KWallet — but it genuinely may be absent on a minimal desktop, and a client
// that hard-fails there cannot log in at all.
//
// So: Secret Service first, and a 0600 file under XDG_DATA_HOME as an explicit,
// reported fallback. The fallback is a real security downgrade — a plaintext
// 30-day session token on disk — so [`Storage::describe`] exists to tell the
// user which one is in use rather than degrading silently.

use std::io::Write;
use std::path::PathBuf;

const SERVICE: &str = "scully";
/// Pre-rename identifiers, read as fallbacks so an existing login survives the
/// app being renamed. Never written to.
const LEGACY_SERVICE: &str = "lurker-desktop";
const LEGACY_DIR: &str = "lurker-desktop";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    Keyring,
    /// Plaintext file, mode 0600.
    File,
}

impl Storage {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Keyring => "system keyring",
            Self::File => "a 0600 file (no Secret Service available)",
        }
    }
}

/// Where the token for one account on one server lives.
///
/// Keyed by server URL so several instances can be signed in at once.
pub struct Credentials {
    account: String,
}

impl Credentials {
    pub fn for_server(base: &url::Url, username: &str) -> Self {
        Self { account: format!("{username}@{base}") }
    }

    fn file_path(&self) -> PathBuf {
        let dir = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
            })
            .join("scully");
        let digest: String = self
            .account
            .bytes()
            .fold(5381u64, |h, b| h.wrapping_mul(33) ^ b as u64)
            .to_string();
        dir.join(format!("token-{digest}"))
    }

    pub fn store(&self, token: &str) -> Storage {
        if let Ok(entry) = keyring::Entry::new(SERVICE, &self.account) {
            if entry.set_password(token).is_ok() {
                return Storage::Keyring;
            }
        }
        let path = self.file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::File::create(&path) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
            }
            let _ = f.write_all(token.as_bytes());
        }
        Storage::File
    }

    pub fn load(&self) -> Option<String> {
        for service in [SERVICE, LEGACY_SERVICE] {
            if let Ok(entry) = keyring::Entry::new(service, &self.account) {
                if let Ok(token) = entry.get_password() {
                    return Some(token);
                }
            }
        }
        if let Ok(token) = std::fs::read_to_string(self.file_path()) {
            return Some(token.trim().to_string());
        }
        // Legacy file location, from before the rename.
        let legacy = self
            .file_path()
            .to_string_lossy()
            .replace("/scully/", &format!("/{LEGACY_DIR}/"));
        std::fs::read_to_string(legacy).ok().map(|s| s.trim().to_string())
    }

    /// Remove the stored token. Called on any `401`, which §3.4 defines as
    /// meaning the session is dead.
    pub fn clear(&self) {
        if let Ok(entry) = keyring::Entry::new(SERVICE, &self.account) {
            let _ = entry.delete_credential();
        }
        let _ = std::fs::remove_file(self.file_path());
    }
}

/// The last account used, so the app can resume without asking again.
pub fn remember_last(base: &url::Url, username: &str) {
    let path = last_account_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{base}\n{username}\n"));
}

pub fn last_account() -> Option<(url::Url, String)> {
    let raw = std::fs::read_to_string(last_account_path())
        .or_else(|_| {
            // Legacy location, from before the rename.
            let legacy = last_account_path()
                .to_string_lossy()
                .replace("/scully/", &format!("/{LEGACY_DIR}/"));
            std::fs::read_to_string(legacy)
        })
        .ok()?;
    let mut lines = raw.lines();
    let base = url::Url::parse(lines.next()?).ok()?;
    let user = lines.next()?.to_string();
    Some((base, user))
}

fn last_account_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("scully")
        .join("last-account")
}
