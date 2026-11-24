//! Filesystem locations and environment variable names (contracts §0).
//!
//! Every `*_with` function takes an env getter so callers and tests can
//! resolve paths without mutating the process environment.

use std::path::{Path, PathBuf};

pub const ENV_STATE_DIR: &str = "BATON_STATE_DIR";
pub const ENV_SOCKET: &str = "BATON_SOCKET";
pub const ENV_CONFIG: &str = "BATON_CONFIG";
pub const ENV_BIND: &str = "BATON_BIND";
pub const ENV_LOG: &str = "BATON_LOG";
pub const ENV_DISABLE: &str = "BATON_DISABLE";

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
            .join("baton"),
        None => std::env::temp_dir().join("baton"),
    }
}

pub fn socket_path_with(get: EnvGet<'_>) -> PathBuf {
    match get(ENV_SOCKET) {
        Some(sock) => PathBuf::from(sock),
        None => state_dir_with(get).join("batond.sock"),
    }
}

pub fn config_path_with(get: EnvGet<'_>) -> PathBuf {
    if let Some(cfg) = get(ENV_CONFIG) {
        return PathBuf::from(cfg);
    }
    if let Some(xdg) = get("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("baton").join("config.toml");
    }
    match get("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".config")
            .join("baton")
            .join("config.toml"),
        None => std::env::temp_dir().join("baton").join("config.toml"),
    }
}

/// `$BATON_STATE_DIR` or `~/.local/state/baton/`.
pub fn state_dir() -> PathBuf {
    state_dir_with(&env_get)
}
/// `$BATON_SOCKET` or `<state>/batond.sock`.
pub fn socket_path() -> PathBuf {
    socket_path_with(&env_get)
}
/// `$BATON_CONFIG` or `~/.config/baton/config.toml`.
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
/// `<state>/batond.pid` — exclusive daemon lock.
pub fn pid_path() -> PathBuf {
    state_dir().join("batond.pid")
}
/// `<state>/batond.log`.
pub fn log_path() -> PathBuf {
    state_dir().join("batond.log")
}
/// `<state>/spawn.lock` — emit's daemon-autostart rate limiter (mtime based).
pub fn spawn_lock_path() -> PathBuf {
    state_dir().join("spawn.lock")
}
/// `<git_root>/.baton/`.
pub fn project_dir(git_root: &Path) -> PathBuf {
    git_root.join(".baton")
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
            PathBuf::from("/Users/x/.local/state/baton")
        );
        assert_eq!(
            socket_path_with(&g),
            PathBuf::from("/Users/x/.local/state/baton/batond.sock")
        );
        assert_eq!(
            config_path_with(&g),
            PathBuf::from("/Users/x/.config/baton/config.toml")
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
        assert_eq!(socket_path_with(&g), PathBuf::from("/tmp/s/batond.sock"));
        assert_eq!(config_path_with(&g), PathBuf::from("/tmp/c.toml"));
        let g2 = getter(&[(ENV_SOCKET, "/tmp/x.sock"), ("XDG_CONFIG_HOME", "/xdg")]);
        assert_eq!(socket_path_with(&g2), PathBuf::from("/tmp/x.sock"));
        assert_eq!(
            config_path_with(&g2),
            PathBuf::from("/xdg/baton/config.toml")
        );
    }

    #[test]
    fn project_dir_is_dot_baton() {
        assert_eq!(
            project_dir(Path::new("/repo")),
            PathBuf::from("/repo/.baton")
        );
    }
}
