use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub fn acquire_single_instance(dir: &Path) -> Result<std::fs::File> {
    let path = dir.join("silo.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another silo instance is already running")
        }
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("acquiring single-instance lock at {}", path.display()))
        }
    }
}

pub fn config_dir() -> PathBuf {
    config_dir_from(|k| std::env::var(k).ok())
}

pub fn config_dir_from(mut var: impl FnMut(&str) -> Option<String>) -> PathBuf {
    if let Some(x) = var("SILO_CONFIG_DIR") {
        return PathBuf::from(x);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = var("HOME") {
            return PathBuf::from(h).join("Library/Application Support/silo");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(a) = var("APPDATA") {
            return PathBuf::from(a).join("silo");
        }
    }
    if let Some(x) = var("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("silo");
    }
    if let Some(h) = var("HOME") {
        return PathBuf::from(h).join(".config/silo");
    }
    PathBuf::from(".silo")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(vars: &[(&str, &str)]) -> PathBuf {
        let map: HashMap<&str, &str> = vars.iter().copied().collect();
        config_dir_from(|k| map.get(k).map(|v| (*v).to_string()))
    }

    #[test]
    fn explicit_config_dir_wins() {
        assert_eq!(
            cfg(&[("SILO_CONFIG_DIR", "/tmp/silo")]),
            PathBuf::from("/tmp/silo")
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn xdg_config_home_is_used_on_unix() {
        assert_eq!(
            cfg(&[("XDG_CONFIG_HOME", "/tmp/cfg")]),
            PathBuf::from("/tmp/cfg/silo")
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn home_fallback_is_used_on_unix() {
        assert_eq!(
            cfg(&[("HOME", "/home/alice")]),
            PathBuf::from("/home/alice/.config/silo")
        );
    }
}
