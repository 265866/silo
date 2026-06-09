#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Persistent,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NonPersistent,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    PersistenceUnknown,
}

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("clipboard is unavailable on this system")]
    Unavailable,
    #[error("clipboard is empty")]
    Empty,
    #[error("not valid base58")]
    NotBase58,
    #[error("address is {0} bytes, expected 32")]
    WrongLength(usize),
    #[error("clipboard error: {0}")]
    Backend(String),
}

impl From<arboard::Error> for ClipboardError {
    fn from(e: arboard::Error) -> Self {
        match e {
            arboard::Error::ClipboardNotSupported => ClipboardError::Unavailable,
            arboard::Error::ContentNotAvailable => ClipboardError::Empty,
            other => ClipboardError::Backend(other.to_string()),
        }
    }
}

#[cfg(target_os = "linux")]
const CLIP_DAEMON_ARG: &str = "__silo_clip_daemon";

#[cfg(target_os = "linux")]
pub fn maybe_run_clip_daemon() {
    use std::io::Read;
    let mut args = std::env::args();
    let _ = args.next();
    if args.next().as_deref() == Some(CLIP_DAEMON_ARG) {
        let mut text = String::new();
        if std::io::stdin().read_to_string(&mut text).is_ok()
            && let Ok(mut cb) = arboard::Clipboard::new()
        {
            use arboard::SetExtLinux;
            let _ = cb.set().wait().text(text);
        }
        std::process::exit(0);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn maybe_run_clip_daemon() {}

#[cfg(target_os = "linux")]
fn wayland_without_data_control() -> bool {
    if std::env::var("XDG_SESSION_TYPE").unwrap_or_default() != "wayland" {
        return false;
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();
    desktop.contains("gnome")
}

#[cfg(target_os = "linux")]
fn spawn_detached(command: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            match libc::fork() {
                -1 => return Err(std::io::Error::last_os_error()),
                0 => {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                _ => libc::_exit(0),
            }
            Ok(())
        });
    }
    command.spawn()
}

#[derive(Clone, Default)]
pub struct ClipboardManager;

impl ClipboardManager {
    pub fn new() -> Self {
        ClipboardManager
    }

    pub fn copy(&self, text: &str) -> Result<CopyOutcome, ClipboardError> {
        #[cfg(target_os = "linux")]
        {
            self.copy_linux_persistent(text)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut cb = arboard::Clipboard::new()?;
            cb.set_text(text.to_owned())?;
            Ok(CopyOutcome::Persistent)
        }
    }

    #[cfg(target_os = "linux")]
    fn copy_linux_persistent(&self, text: &str) -> Result<CopyOutcome, ClipboardError> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        if wayland_without_data_control() {
            let mut cb = arboard::Clipboard::new()?;
            cb.set_text(text.to_owned())?;
            return Ok(CopyOutcome::NonPersistent);
        }

        let exe = std::env::current_exe().map_err(|e| ClipboardError::Backend(e.to_string()))?;
        let mut command = Command::new(exe);
        command
            .arg(CLIP_DAEMON_ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match spawn_detached(&mut command) {
            Ok(mut child) => {
                let wrote = match child.stdin.take() {
                    Some(mut stdin) => {
                        let ok = stdin.write_all(text.as_bytes()).is_ok() && stdin.flush().is_ok();
                        drop(stdin);
                        ok
                    }
                    None => false,
                };
                let _ = child.wait();
                if wrote {
                    Ok(CopyOutcome::PersistenceUnknown)
                } else {
                    let mut cb = arboard::Clipboard::new()?;
                    cb.set_text(text.to_owned())?;
                    Ok(CopyOutcome::NonPersistent)
                }
            }
            Err(_) => {
                let mut cb = arboard::Clipboard::new()?;
                cb.set_text(text.to_owned())?;
                Ok(CopyOutcome::NonPersistent)
            }
        }
    }

    pub fn paste(&self) -> Result<String, ClipboardError> {
        let mut cb = arboard::Clipboard::new()?;
        Ok(cb.get_text()?)
    }
}

pub fn validate_solana_pubkey(raw: &str) -> Result<String, ClipboardError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(ClipboardError::Empty);
    }
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|_| ClipboardError::NotBase58)?;
    if bytes.len() != 32 {
        return Err(ClipboardError::WrongLength(bytes.len()));
    }
    Ok(s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk";

    #[test]
    fn validates_good_address() {
        assert_eq!(validate_solana_pubkey(VALID).unwrap(), VALID);
        assert_eq!(
            validate_solana_pubkey(&format!("  {VALID}  ")).unwrap(),
            VALID
        );
    }

    #[test]
    fn rejects_bad_addresses() {
        assert!(matches!(
            validate_solana_pubkey(""),
            Err(ClipboardError::Empty)
        ));
        assert!(matches!(
            validate_solana_pubkey("0OIl-not-base58"),
            Err(ClipboardError::NotBase58)
        ));
        assert!(matches!(
            validate_solana_pubkey("abc"),
            Err(ClipboardError::WrongLength(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn spawn_detached_reparents_to_init() {
        use std::io::Read;
        use std::process::{Command, Stdio};

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 0.3; ps -o ppid= -p $$")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = spawn_detached(&mut command).expect("spawn detached helper");
        let intermediate_pid = child.id();
        let mut stdout = child.stdout.take().expect("piped stdout");
        let status = child.wait().expect("reap intermediate fork");
        assert!(status.success(), "intermediate fork should exit cleanly");

        let mut out = String::new();
        stdout
            .read_to_string(&mut out)
            .expect("read grandchild ppid");
        let ppid: u32 = out.trim().parse().expect("ppid is numeric");
        assert_ne!(
            ppid,
            std::process::id(),
            "detached daemon must not remain a child of silo"
        );
        assert_ne!(
            ppid, intermediate_pid,
            "detached daemon must outlive the intermediate fork"
        );
    }
}
