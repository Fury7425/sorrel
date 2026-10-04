//! Per-turn checkpoints as hidden git refs in a shadow repository.
//!
//! Each working folder gets its own git dir under the app's data dir, with its
//! own index, so the user's folder and any repository inside it are never
//! touched. A snapshot is `add -A` + `write-tree` + `commit-tree`, kept alive by
//! a ref under `refs/sorrel/`. Undo checks a snapshot back out.

use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io,
    path::{Path, PathBuf},
    process::Command,
};

/// Never snapshotted, on top of the folder's own `.gitignore` files.
const EXCLUDE: &str = "node_modules/\ntarget/\n.venv/\n__pycache__/\n.DS_Store\n";

pub struct Shadow {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

impl Shadow {
    pub fn new(data_dir: &Path, work_tree: &Path) -> Shadow {
        let mut hasher = DefaultHasher::new();
        work_tree.hash(&mut hasher);
        Shadow {
            git_dir: data_dir
                .join("checkpoints")
                .join(format!("{:016x}.git", hasher.finish())),
            work_tree: work_tree.to_owned(),
        }
    }

    fn git(&self, args: &[&str]) -> io::Result<String> {
        let mut cmd = Command::new("git");
        cmd.args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.quotepath=off",
            "-c",
            "commit.gpgsign=false",
        ])
        .arg("--git-dir")
        .arg(&self.git_dir)
        .arg("--work-tree")
        .arg(&self.work_tree)
        .args(args)
        .current_dir(&self.work_tree)
        .env("GIT_INDEX_FILE", self.git_dir.join("sorrel-index"))
        .env("GIT_AUTHOR_NAME", "Sorrel")
        .env("GIT_AUTHOR_EMAIL", "checkpoints@sorrel.invalid")
        .env("GIT_COMMITTER_NAME", "Sorrel")
        .env("GIT_COMMITTER_EMAIL", "checkpoints@sorrel.invalid");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let output = cmd.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::other(format!(
                "git {}: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn init(&self) -> io::Result<()> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        fs::create_dir_all(&self.git_dir)?;
        self.git(&["init", "--quiet"])?;
        let info = self.git_dir.join("info");
        fs::create_dir_all(&info)?;
        fs::write(info.join("exclude"), EXCLUDE)
    }

    /// Records the folder as it is now under `refs/sorrel/<label>`.
    pub fn snapshot(&self, label: &str) -> io::Result<String> {
        self.init()?;
        self.git(&["add", "--all", "--", "."])?;
        let tree = self.git(&["write-tree"])?;
        let commit = self.git(&["commit-tree", &tree, "-m", label])?;
        self.git(&["update-ref", &format!("refs/sorrel/{label}"), &commit])?;
        Ok(commit)
    }

    /// Puts the folder back to `commit`. The current state is snapshotted
    /// first, so a restore can itself be undone.
    pub fn restore(&self, commit: &str, label: &str) -> io::Result<()> {
        let current = self.snapshot(label)?;
        let added = self.git(&[
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=A",
            commit,
            &current,
        ])?;
        for file in added.lines().filter(|line| !line.is_empty()) {
            let _ = fs::remove_file(self.work_tree.join(file));
        }
        self.git(&["read-tree", commit])?;
        self.git(&["checkout-index", "--all", "--force"])?;
        Ok(())
    }

    pub fn diff(&self, from: &str, to: &str) -> io::Result<String> {
        self.git(&["diff", "--no-color", "--stat", "--patch", from, to])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_brings_back_edits_and_removes_new_files() {
        if Command::new("git").arg("--version").output().is_err() {
            return; // no git on this machine
        }
        let root = std::env::temp_dir().join(format!("sorrel-ckpt-{}", std::process::id()));
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("a.txt"), "one").unwrap();
        let shadow = Shadow::new(&root.join("data"), &work);

        let before = shadow.snapshot("t/1/before").unwrap();
        fs::write(work.join("a.txt"), "two").unwrap();
        fs::write(work.join("new.txt"), "x").unwrap();
        let after = shadow.snapshot("t/1/after").unwrap();
        assert!(shadow.diff(&before, &after).unwrap().contains("new.txt"));

        shadow.restore(&before, "t/1/restore").unwrap();
        assert_eq!(fs::read_to_string(work.join("a.txt")).unwrap(), "one");
        assert!(!work.join("new.txt").exists());
        // No .git appeared in the user's folder.
        assert!(!work.join(".git").exists());
    }
}
