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

    pub fn upgrade_hint(self) -> &'static str {
        match self {
            InstallMethod::Homebrew => "Run: brew upgrade 265866/silo/silo",
            InstallMethod::Installer => "Re-run the silo install script",
            InstallMethod::Cargo => {
                "Run: cargo install --locked --git https://github.com/265866/silo --force"
            }
            InstallMethod::Manual => {
                "Download the latest from github.com/265866/silo/releases/latest"
            }
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
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        return Some(PathBuf::from(home).join("bin"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo").join("bin"))
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

pub fn changelog_url(latest: &str) -> String {
    format!("https://github.com/{REPO}/compare/v{CURRENT_VERSION}...v{latest}")
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
    fn changelog_points_at_compare_range() {
        let url = changelog_url("9.9.9");
        assert!(url.starts_with("https://github.com/265866/silo/compare/v"));
        assert!(url.ends_with("...v9.9.9"));
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
