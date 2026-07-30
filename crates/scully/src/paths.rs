// Where Scully keeps its per-user files, per platform.
//
// This exists because the obvious version is wrong off Linux: falling back to
// `$HOME/.config` gives a RELATIVE path on Windows, where `HOME` is usually
// unset — so `PathBuf::from("")` silently turns into `.config/scully` under
// whatever directory the app happened to be launched from, scattering tokens
// and settings around the filesystem.
//
// The XDG variables are honoured first on every platform, not just Linux. They
// are the documented way to redirect a session, which is what the isolated
// profile used for testing relies on; respecting them everywhere keeps that
// trick working and costs nothing on platforms that don't set them.

use std::path::PathBuf;

/// Per-user configuration directory (`…/scully`), created lazily by callers.
///
///  * Linux:   `$XDG_CONFIG_HOME` or `~/.config`
///  * macOS:   `~/Library/Application Support`
///  * Windows: `%APPDATA%`
pub fn config_dir() -> PathBuf {
    if let Some(x) = env_path("XDG_CONFIG_HOME") {
        return x.join("scully");
    }
    platform_config().join("scully")
}

/// Per-user data directory (`…/scully`) — larger or less portable state than
/// config, e.g. the token fallback file.
///
///  * Linux:   `$XDG_DATA_HOME` or `~/.local/share`
///  * macOS:   `~/Library/Application Support`
///  * Windows: `%LOCALAPPDATA%`
pub fn data_dir() -> PathBuf {
    if let Some(x) = env_path("XDG_DATA_HOME") {
        return x.join("scully");
    }
    platform_data().join("scully")
}

/// A non-empty environment variable as a path. Empty is treated as unset —
/// an empty value would otherwise produce a relative path, which is the exact
/// bug this module exists to prevent.
fn env_path(key: &str) -> Option<PathBuf> {
    let v = std::env::var_os(key)?;
    if v.is_empty() {
        return None;
    }
    Some(PathBuf::from(v))
}

#[cfg(target_os = "windows")]
fn platform_config() -> PathBuf {
    env_path("APPDATA").unwrap_or_else(fallback_home)
}

#[cfg(target_os = "windows")]
fn platform_data() -> PathBuf {
    env_path("LOCALAPPDATA").or_else(|| env_path("APPDATA")).unwrap_or_else(fallback_home)
}

#[cfg(target_os = "macos")]
fn platform_config() -> PathBuf {
    home().join("Library").join("Application Support")
}

#[cfg(target_os = "macos")]
fn platform_data() -> PathBuf {
    platform_config()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_config() -> PathBuf {
    home().join(".config")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_data() -> PathBuf {
    home().join(".local").join("share")
}

/// The user's home directory. Falls back to the current directory only as an
/// absolute last resort — and as an *absolute* path, so we never write to a
/// surprise location if the process later changes directory.
#[cfg(not(target_os = "windows"))]
fn home() -> PathBuf {
    env_path("HOME").unwrap_or_else(fallback_home)
}

fn fallback_home() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_overrides_win_on_every_platform() {
        // Relied on by the isolated test profile; must work off Linux too.
        temp_env("XDG_CONFIG_HOME", Some("/somewhere/cfg"), || {
            assert_eq!(config_dir(), PathBuf::from("/somewhere/cfg").join("scully"));
        });
        temp_env("XDG_DATA_HOME", Some("/somewhere/data"), || {
            assert_eq!(data_dir(), PathBuf::from("/somewhere/data").join("scully"));
        });
    }

    #[test]
    fn an_empty_variable_is_treated_as_unset() {
        // The original bug: an empty value became a RELATIVE path, so state was
        // written under the launch directory instead of the user's profile.
        temp_env("XDG_CONFIG_HOME", Some(""), || {
            assert!(config_dir().is_absolute(), "empty XDG must not yield a relative path");
        });
    }

    #[test]
    fn resolved_directories_are_always_absolute() {
        // Whatever the platform and however sparse the environment, these are
        // paths we write user state to — a relative one is always a bug.
        temp_env("XDG_CONFIG_HOME", None, || {
            assert!(config_dir().is_absolute(), "config_dir must be absolute");
        });
        temp_env("XDG_DATA_HOME", None, || {
            assert!(data_dir().is_absolute(), "data_dir must be absolute");
        });
    }

    #[test]
    fn config_and_data_both_land_under_a_scully_directory() {
        temp_env("XDG_CONFIG_HOME", None, || {
            assert_eq!(config_dir().file_name().unwrap(), "scully");
            assert_eq!(data_dir().file_name().unwrap(), "scully");
        });
    }

    /// Set (or clear) an env var for the duration of `f`, then restore it.
    /// Tests in a crate share a process, so leaking a value here would make an
    /// unrelated test fail depending on ordering.
    fn temp_env(key: &str, value: Option<&str>, f: impl FnOnce()) {
        let old = std::env::var_os(key);
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        f();
        match old {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
