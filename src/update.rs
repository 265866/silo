use std::path::{Path, PathBuf};

pub const REPO: &str = "265866/silo";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CHECK_INTERVAL_SECS: u64 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    Installer,
    Cargo,
    Manual,
}

impl InstallMethod {
    pub fn detect() -> Self {
        detect_from(
            std::env::current_exe().ok(),
            dist_receipt_path().is_some_and(|p| p.exists()),
            cargo_bin_dir(),
        )
    }

    pub fn upgrade_command(self) -> &'static str {
        match self {
            InstallMethod::Homebrew => "brew upgrade 265866/silo/silo",
            InstallMethod::Installer => "re-run the silo install script",
            InstallMethod::Cargo => {
                "cargo install --locked --git https://github.com/265866/silo --force"
            }
            InstallMethod::Manual => "download from github.com/265866/silo/releases/latest",
        }
    }
}

fn detect_from(
    exe: Option<PathBuf>,
    receipt_present: bool,
    cargo_bin: Option<PathBuf>,
) -> InstallMethod {
    if let Some(exe) = exe {
        let exe = exe.canonicalize().unwrap_or(exe);
        if is_homebrew_path(&exe) {
            return InstallMethod::Homebrew;
        }
        if receipt_present {
            return InstallMethod::Installer;
        }
        if let Some(bin) = cargo_bin {
            let bin = bin.canonicalize().unwrap_or(bin);
            if exe.starts_with(&bin) {
                return InstallMethod::Cargo;
            }
        }
    } else if receipt_present {
        return InstallMethod::Installer;
    }
    InstallMethod::Manual
}

fn is_homebrew_path(exe: &Path) -> bool {
    if exe.components().any(|c| c.as_os_str() == "Cellar") {
        return true;
    }
    exe.to_string_lossy().contains("/linuxbrew/")
}

fn cargo_bin_dir() -> Option<PathBuf> {
    cargo_bin_dir_from(|k| std::env::var_os(k))
}

fn cargo_bin_dir_from(mut var: impl FnMut(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(home) = var("CARGO_HOME") {
        return Some(PathBuf::from(home).join("bin"));
    }
    #[cfg(windows)]
    let home = var("USERPROFILE");
    #[cfg(not(windows))]
    let home = var("HOME");
    home.map(|h| PathBuf::from(h).join(".cargo").join("bin"))
}

fn dist_receipt_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|a| PathBuf::from(a).join("silo").join("silo-receipt.json"))
    }
    #[cfg(not(windows))]
    {
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(x).join("silo").join("silo-receipt.json"));
        }
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join("silo")
                .join("silo-receipt.json")
        })
    }
}

pub fn releases_api_url() -> String {
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_semver_with_and_without_v() {
        assert_eq!(parse_version("v0.1.7"), Some((0, 1, 7)));
        assert_eq!(parse_version("0.1.7"), Some((0, 1, 7)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("0.1.7-rc.1"), Some((0, 1, 7)));
        assert_eq!(parse_version("not-a-version"), None);
    }

    #[test]
    fn newer_compares_numerically() {
        assert!(is_newer("0.1.8", "0.1.7"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.7", "0.1.7"));
        assert!(!is_newer("0.1.6", "0.1.7"));
    }

    #[test]
    fn newer_handles_double_digit_components() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
    }

    #[test]
    fn detects_homebrew_from_cellar_path() {
        let m = detect_from(
            Some(PathBuf::from("/opt/homebrew/Cellar/silo/0.1.7/bin/silo")),
            false,
            None,
        );
        assert_eq!(m, InstallMethod::Homebrew);
    }

    #[test]
    fn receipt_means_installer_even_under_cargo_bin() {
        let m = detect_from(
            Some(PathBuf::from("/home/u/.cargo/bin/silo")),
            true,
            Some(PathBuf::from("/home/u/.cargo/bin")),
        );
        assert_eq!(m, InstallMethod::Installer);
    }

    #[test]
    fn cargo_bin_without_receipt_is_cargo() {
        let m = detect_from(
            Some(PathBuf::from("/home/u/.cargo/bin/silo")),
            false,
            Some(PathBuf::from("/home/u/.cargo/bin")),
        );
        assert_eq!(m, InstallMethod::Cargo);
    }

    fn cargo_bin_from_env(vars: &[(&str, &str)]) -> Option<PathBuf> {
        cargo_bin_dir_from(|key| {
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).into())
        })
    }

    #[test]
    fn cargo_home_overrides_home_and_userprofile() {
        assert_eq!(
            cargo_bin_from_env(&[
                ("CARGO_HOME", "/custom/cargo"),
                ("HOME", "/home/u"),
                ("USERPROFILE", "C:/Users/u"),
            ]),
            Some(PathBuf::from("/custom/cargo/bin"))
        );
    }

    #[test]
    fn cargo_home_works_without_a_home_directory() {
        assert_eq!(
            cargo_bin_from_env(&[("CARGO_HOME", "/custom/cargo")]),
            Some(PathBuf::from("/custom/cargo/bin"))
        );
    }

    #[test]
    fn missing_cargo_and_home_directories_leave_install_manual() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_bin = cargo_bin_from_env(&[]);
        assert_eq!(cargo_bin, None);
        assert_eq!(
            detect_from(Some(dir.path().join("silo")), false, cargo_bin),
            InstallMethod::Manual
        );
    }

    #[cfg(windows)]
    #[test]
    fn userprofile_overrides_home_on_windows() {
        assert_eq!(
            cargo_bin_from_env(&[("USERPROFILE", "C:/Users/u"), ("HOME", "C:/other")]),
            Some(PathBuf::from("C:/Users/u/.cargo/bin"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn home_alone_is_not_used_on_windows() {
        assert_eq!(cargo_bin_from_env(&[("HOME", "C:/other")]), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn home_is_used_instead_of_userprofile_off_windows() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".cargo/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("silo");
        std::fs::write(&exe, b"").unwrap();
        for userprofile in [None, Some("/other")] {
            let cargo_bin = cargo_bin_dir_from(|key| match key {
                "HOME" => Some(home.path().as_os_str().to_owned()),
                "USERPROFILE" => userprofile.map(Into::into),
                _ => None,
            });
            assert_eq!(cargo_bin.as_ref(), Some(&bin));
            assert_eq!(
                detect_from(Some(exe.clone()), false, cargo_bin),
                InstallMethod::Cargo
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn userprofile_alone_is_not_used_off_windows() {
        assert_eq!(cargo_bin_from_env(&[("USERPROFILE", "/other")]), None);
    }

    #[cfg(windows)]
    #[test]
    fn userprofile_only_detects_cargo_and_upgrade_command() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".cargo/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("silo.exe");
        std::fs::write(&exe, b"").unwrap();
        let cargo_bin = cargo_bin_dir_from(|key| match key {
            "USERPROFILE" => Some(home.path().as_os_str().to_owned()),
            _ => None,
        });
        assert_eq!(cargo_bin.as_ref(), Some(&bin));
        let method = detect_from(Some(exe), false, cargo_bin);
        assert_eq!(method, InstallMethod::Cargo);
        assert_eq!(
            method.upgrade_command(),
            "cargo install --locked --git https://github.com/265866/silo --force"
        );
    }

    #[test]
    fn unknown_paths_are_manual() {
        let m = detect_from(
            Some(PathBuf::from("/usr/local/bin/silo")),
            false,
            Some(PathBuf::from("/home/u/.cargo/bin")),
        );
        assert_eq!(m, InstallMethod::Manual);
    }
}
