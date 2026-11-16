//! Version the memory directory with git, best effort.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> bool {
    // Own identity; never wait on signing, never run the user's hooks, accept a volume owned by another uid.
    let fixed = [
        "-c",
        "user.name=fronde",
        "-c",
        "user.email=fronde@localhost",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "safe.directory=*",
    ];
    match Command::new("git")
        .args(fixed)
        .args(args)
        .current_dir(dir)
        .output()
    {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            tracing::warn!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&o.stderr).trim()
            );
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
        let _ = crate::private::write(dir.join(".gitignore"), "*.sqlite*\n*.tmp\n");
    }
    if git(dir, &["add", "-A"]) {
        git(
            dir,
            &["commit", "-q", "--allow-empty-message", "-m", message],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(dir: &Path) -> usize {
        let out = Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }

    #[test]
    fn commits_despite_user_signing_and_hooks() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        for (k, v) in [("commit.gpgsign", "true"), ("gpg.program", "false")] {
            Command::new("git")
                .args(["config", k, v])
                .current_dir(dir.path())
                .output()
                .unwrap();
        }
        let hook = dir.path().join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "- a\n").unwrap();
        commit_all(dir.path(), "memory: add MEMORY.md");
        assert_eq!(count(dir.path()), 1);
    }
}
