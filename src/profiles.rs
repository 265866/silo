use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
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
            .with_context(|| format!("setting permissions on {}", path.display()))?;
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

pub fn load(config_dir: &Path) -> Result<Vec<ProfileMeta>> {
    match std::fs::read(registry_path(config_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("profiles registry is not valid JSON"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).context("reading profiles registry"),
    }
}

pub fn save(config_dir: &Path, list: &[ProfileMeta]) -> Result<()> {
    let json = serde_json::to_vec_pretty(list).context("serializing profiles")?;
    crate::vault::write_atomic(&registry_path(config_dir), &json)
}

pub fn register(config_dir: &Path, meta: ProfileMeta) -> Result<()> {
    let mut list = load(config_dir)?;
    if !list.iter().any(|p| p.id == meta.id) {
        list.push(meta);
        save(config_dir, &list)?;
    }
    Ok(())
}

pub fn rename(config_dir: &Path, id: &str, name: &str) -> Result<()> {
    let mut list = load(config_dir)?;
    if let Some(p) = list.iter_mut().find(|p| p.id == id) {
        p.name = name.to_string();
    }
    save(config_dir, &list)
}

pub fn recoverable(config_dir: &Path) -> Vec<ProfileMeta> {
    let root = config_dir.join("profiles");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let id = e.file_name().to_string_lossy().to_string();
        if path.is_dir() && !id.starts_with('.') && path.join("vault.json").exists() {
            out.push(ProfileMeta {
                name: recovered_name(out.len() + 1),
                id,
                created_at: crate::db::now_ms(),
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn recovered_name(n: usize) -> String {
    if n == 1 {
        "Recovered wallet".to_string()
    } else {
        format!("Recovered wallet {n}")
    }
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
        if !path.is_dir() {
            continue;
        }
        if let Some(id) = tombstone_id(&name) {
            if known.contains(id) {
                let live = dir_for(config_dir, id);
                if !live.exists() && std::fs::rename(&path, &live).is_ok() {
                    sync_dir(&root);
                }
            } else {
                let _ = std::fs::remove_dir_all(&path);
            }
        } else if !known.contains(name.as_ref()) && !path.join("vault.json").exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

pub fn remove(config_dir: &Path, id: &str) -> Result<()> {
    let mut list = load(config_dir)?;
    let dir = dir_for(config_dir, id);
    if dir.exists() {
        let tombstone = tombstone_dir(config_dir, id)?;
        std::fs::rename(&dir, &tombstone).with_context(|| {
            format!(
                "moving profile dir {} -> {}",
                dir.display(),
                tombstone.display()
            )
        })?;
        sync_dir(&config_dir.join("profiles"));
        list.retain(|p| p.id != id);
        if let Err(e) = save(config_dir, &list) {
            if std::fs::rename(&tombstone, &dir).is_ok() {
                sync_dir(&config_dir.join("profiles"));
            }
            return Err(e);
        }
        std::fs::remove_dir_all(&tombstone)
            .with_context(|| format!("deleting profile tombstone {}", tombstone.display()))?;
        sync_dir(&config_dir.join("profiles"));
        return Ok(());
    }
    list.retain(|p| p.id != id);
    save(config_dir, &list)
}

fn tombstone_dir(config_dir: &Path, id: &str) -> Result<PathBuf> {
    let root = config_dir.join("profiles");
    for _ in 0..16 {
        let suffix = new_id();
        let path = root.join(format!(".delete-{id}-{suffix}"));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(anyhow!("could not choose a profile tombstone path"))
}

fn tombstone_id(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(".delete-")?;
    rest.split_once('-').map(|(id, _)| id)
}

fn sync_dir(path: &Path) {
    if let Ok(dir) = std::fs::OpenOptions::new().read(true).open(path) {
        let _ = dir.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_registry_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn invalid_registry_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(registry_path(dir.path()), b"not json").unwrap();
        assert!(load(dir.path()).is_err());
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

    #[test]
    fn cleanup_restores_registered_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir_for(dir.path(), "abc");
        let tombstone = dir.path().join("profiles").join(".delete-abc-123");
        std::fs::create_dir_all(&tombstone).unwrap();
        std::fs::write(tombstone.join("vault.json"), b"{}").unwrap();
        cleanup_orphans(
            dir.path(),
            &[ProfileMeta {
                id: "abc".into(),
                name: "Wallet".into(),
                created_at: 1,
            }],
        );
        assert!(live.join("vault.json").exists());
        assert!(!tombstone.exists());
    }

    #[test]
    fn cleanup_retries_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let tombstone = dir.path().join("profiles").join(".delete-abc-123");
        std::fs::create_dir_all(&tombstone).unwrap();
        std::fs::write(tombstone.join("vault.json"), b"{}").unwrap();
        cleanup_orphans(dir.path(), &[]);
        assert!(!tombstone.exists());
    }

    #[test]
    fn register_rename_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path();
        assert!(load(cfg).unwrap().is_empty());

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
        assert_eq!(load(cfg).unwrap().len(), 1);

        rename(cfg, &id, "Treasury").unwrap();
        assert_eq!(load(cfg).unwrap()[0].name, "Treasury");

        std::fs::create_dir_all(dir_for(cfg, &id)).unwrap();
        remove(cfg, &id).unwrap();
        assert!(load(cfg).unwrap().is_empty());
        assert!(!dir_for(cfg, &id).exists());
    }
}
