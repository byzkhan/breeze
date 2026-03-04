use anyhow::{Context, Result, bail};
use std::process::Command;

/// A git checkpoint that can be rolled back to restore the working tree.
pub struct Checkpoint {
    /// The stash reference (SHA), or empty if the tree was clean.
    reference: String,
    /// Whether the working tree was clean when the checkpoint was created.
    is_clean: bool,
    /// The working directory for git operations.
    cwd: String,
}

impl Checkpoint {
    /// Create a checkpoint of the current working tree state.
    /// Uses `git stash create` which creates a stash commit without modifying the working tree.
    /// Returns Err if not in a git repo.
    pub fn create(cwd: &str) -> Result<Self> {
        // Verify we're in a git repo
        let status = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(cwd)
            .output()
            .context("Failed to run git")?;

        if !status.status.success() {
            bail!("Not inside a git repository");
        }

        // git stash create: creates a stash commit but doesn't store it in the reflog.
        // Returns empty string if the working tree is clean.
        // Non-zero exit = actual failure (disk full, corrupted index, etc.).
        let output = Command::new("git")
            .args(["stash", "create", "--include-untracked"])
            .current_dir(cwd)
            .output()
            .context("Failed to create git stash")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git stash create failed: {}", stderr.trim());
        }

        let reference = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let is_clean = reference.is_empty();

        Ok(Self {
            reference,
            is_clean,
            cwd: cwd.to_string(),
        })
    }

    /// Roll back the working tree to the checkpoint state.
    /// Discards all current changes and restores the stashed state.
    pub fn rollback(&self) -> Result<()> {
        // Discard all working tree changes
        let checkout = Command::new("git")
            .args(["checkout", "."])
            .current_dir(&self.cwd)
            .output()
            .context("Failed to run git checkout .")?;
        if !checkout.status.success() {
            let stderr = String::from_utf8_lossy(&checkout.stderr);
            bail!("git checkout . failed: {}", stderr.trim());
        }

        // Remove untracked files
        let clean = Command::new("git")
            .args(["clean", "-fd"])
            .current_dir(&self.cwd)
            .output()
            .context("Failed to run git clean -fd")?;
        if !clean.status.success() {
            let stderr = String::from_utf8_lossy(&clean.stderr);
            bail!("git clean -fd failed: {}", stderr.trim());
        }

        // Restore the stashed state if there was one
        if !self.is_clean {
            let output = Command::new("git")
                .args(["stash", "apply", &self.reference])
                .current_dir(&self.cwd)
                .output()
                .context("Failed to apply git stash")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("Failed to apply stash: {}", stderr);
            }
        }

        Ok(())
    }

    /// Discard the checkpoint (no-op — the stash commit will be GC'd by git).
    pub fn discard(self) {
        // Intentionally empty — consuming self prevents further use.
    }
}
