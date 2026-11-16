//! Version the memory directory with git, best effort.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> bool {
    match Command::new("git").args(["-c", "user.name=jarvis", "-c", "user.email=jarvis@localhost"]).args(args).current_dir(dir).output() {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            tracing::warn!("git {:?} failed: {}", args, String::from_utf8_lossy(&o.stderr).trim());
            false
        }
        Err(e) => {
            tracing::warn!("git unavailable: {e}");
            false
        }
    }
}

/// Commit every change in `dir`, creating the repository on first use. Never fails the caller.
pub fn commit_all(dir: &Path, message: &str) {
    if !dir.join(".git").exists() {
        if !git(dir, &["init", "-q"]) {
            return;
        }
        let _ = std::fs::write(dir.join(".gitignore"), "*.sqlite*\n*.tmp\n");
    }
    if git(dir, &["add", "-A"]) {
        git(dir, &["commit", "-q", "--allow-empty-message", "-m", message]);
    }
}
