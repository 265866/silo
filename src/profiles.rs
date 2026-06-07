use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileMeta {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

fn registry_path(config_dir: &Path) -> PathBuf {
    config_dir.join("profiles.json")
}

pub fn dir_for(config_dir: &Path, id: &str) -> PathBuf {
    config_dir.join("profiles").join(id)
}

pub fn db_path(config_dir: &Path, id: &str) -> PathBuf {
    dir_for(config_dir, id).join("silo.db")
}

pub fn vault_path(config_dir: &Path, id: &str) -> PathBuf {
    dir_for(config_dir, id).join("vault.json")
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting private permissions on {}", path.display()))?;
    }
    Ok(())
}

pub fn new_id() -> String {
    let mut b = [0u8; 8];
    crate::crypto::random_bytes(&mut b);
    let mut s = String::with_capacity(16);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

pub fn load(config_dir: &Path) -> Vec<ProfileMeta> {
    match std::fs::read(registry_path(config_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save(config_dir: &Path, list: &[ProfileMeta]) -> Result<()> {
    let json = serde_json::to_vec_pretty(list).context("serializing profiles")?;
    crate::vault::write_atomic(&registry_path(config_dir), &json)
}

pub fn register(config_dir: &Path, meta: ProfileMeta) -> Result<()> {
    let mut list = load(config_dir);
    if !list.iter().any(|p| p.id == meta.id) {
        list.push(meta);
        save(config_dir, &list)?;
    }
    Ok(())
}

pub fn rename(config_dir: &Path, id: &str, name: &str) -> Result<()> {
    let mut list = load(config_dir);
    if let Some(p) = list.iter_mut().find(|p| p.id == id) {
        p.name = name.to_string();
    }
    save(config_dir, &list)
}

pub fn cleanup_orphans(config_dir: &Path, registry: &[ProfileMeta]) {
    let root = config_dir.join("profiles");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let known: std::collections::HashSet<&str> = registry.iter().map(|p| p.id.as_str()).collect();
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() && !known.contains(name.as_ref()) && !path.join("vault.json").exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

pub fn remove(config_dir: &Path, id: &str) -> Result<()> {
    let mut list = load(config_dir);
    list.retain(|p| p.id != id);
    save(config_dir, &list)?;
    let dir = dir_for(config_dir, id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("deleting profile dir {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_rename_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path();
        assert!(load(cfg).is_empty());

        let id = new_id();
        assert_eq!(id.len(), 16);
        register(
            cfg,
            ProfileMeta {
                id: id.clone(),
                name: "Wallet 1".into(),
                created_at: 1,
            },
        )
        .unwrap();
        register(
            cfg,
            ProfileMeta {
                id: id.clone(),
                name: "dup".into(),
                created_at: 2,
            },
        )
        .unwrap();
        assert_eq!(load(cfg).len(), 1);

        rename(cfg, &id, "Treasury").unwrap();
        assert_eq!(load(cfg)[0].name, "Treasury");

        std::fs::create_dir_all(dir_for(cfg, &id)).unwrap();
        remove(cfg, &id).unwrap();
        assert!(load(cfg).is_empty());
        assert!(!dir_for(cfg, &id).exists());
    }

    #[cfg(unix)]
    #[test]
    fn profile_dir_mode_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles").join("abc");
        ensure_private_dir(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
