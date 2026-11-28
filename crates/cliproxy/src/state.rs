//! The server's state folder: `$WRITABLE_PATH`, else `cpa` in the user config folder. It
//! holds discovery state and, when native routing does not run, the activity store.
use std::path::{Path, PathBuf};

pub fn root() -> Option<PathBuf> {
    let trimmed = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    if let Some(w) = trimmed("WRITABLE_PATH").or_else(|| trimmed("writable_path")) {
        return Some(PathBuf::from(w));
    }
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let config_dir = if cfg!(target_os = "macos") {
        env("HOME").map(|h| Path::new(&h).join("Library/Application Support"))
    } else if cfg!(windows) {
        env("AppData").map(PathBuf::from)
    } else {
        env("XDG_CONFIG_HOME")
            .filter(|d| Path::new(d).is_absolute())
            .map(PathBuf::from)
            .or_else(|| env("HOME").map(|h| Path::new(&h).join(".config")))
    };
    config_dir.map(|d| d.join("cpa"))
}
