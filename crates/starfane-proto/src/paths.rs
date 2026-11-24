//! Filesystem locations and environment variable names (contracts §0).
//!
//! Every `*_with` function takes an env getter so callers and tests can
//! resolve paths without mutating the process environment.

use std::path::{Path, PathBuf};

pub const ENV_STATE_DIR: &str = "STARFANE_STATE_DIR";
pub const ENV_SOCKET: &str = "STARFANE_SOCKET";
pub const ENV_CONFIG: &str = "STARFANE_CONFIG";
pub const ENV_BIND: &str = "STARFANE_BIND";
pub const ENV_LOG: &str = "STARFANE_LOG";
pub const ENV_DISABLE: &str = "STARFANE_DISABLE";

/// Env getter signature used by the `*_with` resolvers.
pub type EnvGet<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Process environment getter; empty values count as unset.
pub fn env_get(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

pub fn state_dir_with(get: EnvGet<'_>) -> PathBuf {
    if let Some(dir) = get(ENV_STATE_DIR) {
        return PathBuf::from(dir);
    }
    match get("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("starfane"),
        None => std::env::temp_dir().join("starfane"),
    }
}

pub fn socket_path_with(get: EnvGet<'_>) -> PathBuf {
    match get(ENV_SOCKET) {
        Some(sock) => PathBuf::from(sock),
        None => state_dir_with(get).join("starfaned.sock"),
    }
}

pub fn config_path_with(get: EnvGet<'_>) -> PathBuf {
    if let Some(cfg) = get(ENV_CONFIG) {
        return PathBuf::from(cfg);
    }
    if let Some(xdg) = get("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("starfane").join("config.toml");
    }
    match get("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".config")
            .join("starfane")
            .join("config.toml"),
        None => std::env::temp_dir().join("starfane").join("config.toml"),
    }
}

/// `$STARFANE_STATE_DIR` or `~/.local/state/starfane/`.
pub fn state_dir() -> PathBuf {
    state_dir_with(&env_get)
}
/// `$STARFANE_SOCKET` or `<state>/starfaned.sock`.
pub fn socket_path() -> PathBuf {
    socket_path_with(&env_get)
}
/// `$STARFANE_CONFIG` or `~/.config/starfane/config.toml`.
pub fn config_path() -> PathBuf {
    config_path_with(&env_get)
}
/// `<state>/spool/` — emit fallback JSONL files.
pub fn spool_dir() -> PathBuf {
    state_dir().join("spool")
}
/// `<state>/events/` — `YYYY-MM-DD.jsonl` normalized event log.
pub fn events_dir() -> PathBuf {
    state_dir().join("events")
}
/// `<state>/state.json` — atomic snapshot.
pub fn snapshot_path() -> PathBuf {
    state_dir().join("state.json")
}
/// `<state>/backups/` — install/uninstall backups.
pub fn backups_dir() -> PathBuf {
    state_dir().join("backups")
}
/// `<state>/bind/` — pending L4 spawn bind tokens.
pub fn bind_dir() -> PathBuf {
    state_dir().join("bind")
}
/// `<state>/web.token` (mode 0600).
pub fn web_token_path() -> PathBuf {
    state_dir().join("web.token")
}
/// `<state>/starfaned.pid` — exclusive daemon lock.
pub fn pid_path() -> PathBuf {
    state_dir().join("starfaned.pid")
}
/// `<state>/starfaned.log`.
pub fn log_path() -> PathBuf {
    state_dir().join("starfaned.log")
}
/// `<state>/spawn.lock` — emit's daemon-autostart rate limiter (mtime based).
pub fn spawn_lock_path() -> PathBuf {
    state_dir().join("spawn.lock")
}
/// `<git_root>/.starfane/`.
pub fn project_dir(git_root: &Path) -> PathBuf {
    git_root.join(".starfane")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn getter(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn defaults_under_home() {
        let g = getter(&[("HOME", "/Users/x")]);
        assert_eq!(
            state_dir_with(&g),
            PathBuf::from("/Users/x/.local/state/starfane")
        );
        assert_eq!(
            socket_path_with(&g),
            PathBuf::from("/Users/x/.local/state/starfane/starfaned.sock")
        );
        assert_eq!(
            config_path_with(&g),
            PathBuf::from("/Users/x/.config/starfane/config.toml")
        );
    }

    #[test]
    fn overrides_win() {
        let g = getter(&[
            ("HOME", "/Users/x"),
            (ENV_STATE_DIR, "/tmp/s"),
            (ENV_CONFIG, "/tmp/c.toml"),
        ]);
        assert_eq!(state_dir_with(&g), PathBuf::from("/tmp/s"));
        assert_eq!(socket_path_with(&g), PathBuf::from("/tmp/s/starfaned.sock"));
        assert_eq!(config_path_with(&g), PathBuf::from("/tmp/c.toml"));
        let g2 = getter(&[(ENV_SOCKET, "/tmp/x.sock"), ("XDG_CONFIG_HOME", "/xdg")]);
        assert_eq!(socket_path_with(&g2), PathBuf::from("/tmp/x.sock"));
        assert_eq!(
            config_path_with(&g2),
            PathBuf::from("/xdg/starfane/config.toml")
        );
    }

    #[test]
    fn project_dir_is_dot_starfane() {
        assert_eq!(
            project_dir(Path::new("/repo")),
            PathBuf::from("/repo/.starfane")
        );
    }
}
