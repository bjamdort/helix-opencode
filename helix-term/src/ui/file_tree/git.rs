use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use tokio::process::Command;

/// Ordered by display priority when a directory contains multiple kinds of change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Status {
    Ignored,
    Untracked,
    Added,
    Renamed,
    Modified,
    Deleted,
    Conflict,
}

impl Status {
    pub fn marker(self) -> &'static str {
        match self {
            Self::Ignored => "!",
            Self::Untracked => "?",
            Self::Added => "A",
            Self::Renamed => "R",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Conflict => "U",
        }
    }

    pub fn scope(self) -> &'static str {
        match self {
            Self::Ignored => "ui.text.inactive",
            Self::Untracked | Self::Added => "diff.plus",
            Self::Renamed => "diff.delta.moved",
            Self::Modified => "diff.delta",
            Self::Deleted => "diff.minus",
            Self::Conflict => "diff.delta.conflict",
        }
    }

    fn from_xy(xy: &[u8]) -> Option<Self> {
        match xy {
            b"!!" => Some(Self::Ignored),
            b"??" => Some(Self::Untracked),
            b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU" => Some(Self::Conflict),
            _ if xy.contains(&b'D') => Some(Self::Deleted),
            _ if xy.contains(&b'A') || xy.contains(&b'C') => Some(Self::Added),
            _ if xy.contains(&b'R') => Some(Self::Renamed),
            _ if xy.contains(&b'M') || xy.contains(&b'T') => Some(Self::Modified),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    // Paths are relative to the sidebar root, not the repository root.
    paths: HashMap<PathBuf, Status>,
    pub files: Vec<ChangedFile>,
}

#[derive(Debug, Clone)]
pub(crate) struct ChangedFile {
    pub path: PathBuf,
    pub original: Option<PathBuf>,
    pub status: Status,
}

impl Snapshot {
    pub fn status(&self, path: &Path) -> Option<Status> {
        self.paths.get(path).copied().or_else(|| {
            // Git collapses ignored directories. Their visible descendants are ignored too.
            path.ancestors().skip(1).find_map(|parent| {
                self.paths
                    .get(parent)
                    .copied()
                    .filter(|status| *status == Status::Ignored)
            })
        })
    }

    fn insert(&mut self, path: PathBuf, status: Status) {
        if status != Status::Ignored {
            // Also color collapsed folders, including parents of files deleted on disk.
            for parent in path.ancestors().skip(1) {
                self.paths
                    .entry(parent.to_owned())
                    .and_modify(|current| *current = (*current).max(status))
                    .or_insert(status);
            }
        }
        self.paths
            .entry(path)
            .and_modify(|current| *current = (*current).max(status))
            .or_insert(status);
    }

    pub(super) fn parse(output: &[u8], prefix: &Path) -> Result<Self> {
        let mut snapshot = Self::default();
        let mut records = output.split(|byte| *byte == 0);
        while let Some(record) = records.next() {
            if record.is_empty() {
                continue;
            }
            if record.len() < 4 || record[2] != b' ' {
                bail!("invalid Git status record");
            }
            let xy = &record[..2];
            let path = path_from_bytes(&record[3..])?;
            let mut original = None;
            if let Some(status) = Status::from_xy(xy) {
                if let Ok(relative) = path.strip_prefix(prefix) {
                    snapshot.insert(relative.to_owned(), status);
                }
            }
            if xy.contains(&b'R') || xy.contains(&b'C') {
                // In porcelain -z, the destination precedes the original filename.
                let source = records
                    .next()
                    .filter(|source| !source.is_empty())
                    .context("missing Git rename source")?;
                if xy.contains(&b'R') {
                    let source = path_from_bytes(source)?;
                    if let Ok(relative) = source.strip_prefix(prefix) {
                        original = Some(relative.to_owned());
                        // The old filename no longer exists, but its parents still changed.
                        if let Some(parent) = relative.parent() {
                            snapshot.insert(parent.to_owned(), Status::Renamed);
                        }
                    }
                }
            }
            if let (Some(status), Ok(relative)) = (Status::from_xy(xy), path.strip_prefix(prefix)) {
                if status != Status::Ignored {
                    snapshot.files.push(ChangedFile {
                        path: relative.to_owned(),
                        original,
                        status,
                    });
                }
            }
        }
        Ok(snapshot)
    }

    pub async fn load(root: &Path) -> Result<Self> {
        let root = tokio::fs::canonicalize(root).await?;
        let output = run_git(&root, &["rev-parse", "--show-toplevel"]).await?;
        if !output.status.success() {
            // Normal for directories that are not inside a repository.
            return Ok(Self::default());
        }
        let repo = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
        #[cfg(windows)]
        let repo = repo.strip_suffix(b"\r").unwrap_or(repo);
        let repo = tokio::fs::canonicalize(path_from_bytes(repo)?).await?;
        let prefix = root.strip_prefix(&repo)?;
        let output = run_git(
            &root,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignored=matching",
                "--renames",
                "--ignore-submodules=none",
            ],
        )
        .await?;
        if !output.status.success() {
            bail!(
                "Git status failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Self::parse(&output.stdout, prefix)
    }
}

async fn run_git(root: &Path, args: &[&str]) -> Result<std::process::Output> {
    // Avoid index writes/locks and configured filesystem-monitor hooks. Repository
    // config can still run clean filters, so callers must check workspace trust.
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    Ok(tokio::time::timeout(Duration::from_secs(10), command.output()).await??)
}

fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes.to_vec()).into())
    }
    #[cfg(not(unix))]
    {
        Ok(OsString::from(std::str::from_utf8(bytes)?).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> Result<()> {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "user.name=Helix test",
                "-c",
                "user.email=helix@example.invalid",
                "-c",
                "core.hooksPath=",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Git test setup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[tokio::test]
    async fn reads_real_repository_and_refreshes_after_staging() -> Result<()> {
        let root = tempfile::tempdir()?;
        git(root.path(), &["init", "--quiet"])?;
        std::fs::create_dir(root.path().join("sub"))?;
        for name in ["modified", "staged", "deleted", "renamed", "clean"] {
            std::fs::write(root.path().join("sub").join(name), name)?;
        }
        std::fs::write(root.path().join(".gitignore"), "ignored/\n")?;
        git(root.path(), &["add", "."])?;
        git(root.path(), &["commit", "--quiet", "-m", "initial"])?;
        std::fs::write(root.path().join("sub/modified"), "edited")?;
        std::fs::write(root.path().join("sub/staged"), "staged edit")?;
        std::fs::write(root.path().join("sub/added"), "added")?;
        std::fs::write(root.path().join("sub/untracked"), "untracked")?;
        std::fs::remove_file(root.path().join("sub/deleted"))?;
        git(root.path(), &["mv", "sub/renamed", "sub/new name"])?;
        git(root.path(), &["add", "sub/added", "sub/staged"])?;
        std::fs::create_dir(root.path().join("ignored"))?;
        std::fs::write(root.path().join("ignored/file"), "ignored")?;

        let snapshot = Snapshot::load(root.path()).await?;
        for (path, status) in [
            ("sub/modified", Status::Modified),
            ("sub/staged", Status::Modified),
            ("sub/added", Status::Added),
            ("sub/untracked", Status::Untracked),
            ("sub/deleted", Status::Deleted),
            ("sub/new name", Status::Renamed),
            ("ignored/file", Status::Ignored),
            ("sub", Status::Deleted),
        ] {
            assert_eq!(snapshot.status(Path::new(path)), Some(status), "{path}");
        }
        assert_eq!(snapshot.status(Path::new("sub/clean")), None);
        let nested = Snapshot::load(&root.path().join("sub")).await?;
        assert_eq!(nested.status(Path::new("modified")), Some(Status::Modified));
        assert_eq!(nested.status(Path::new("")), Some(Status::Deleted));

        git(root.path(), &["add", "sub/untracked"])?;
        let refreshed = Snapshot::load(root.path()).await?;
        assert_eq!(
            refreshed.status(Path::new("sub/untracked")),
            Some(Status::Added)
        );
        Ok(())
    }

    #[tokio::test]
    async fn works_in_unborn_repositories_and_plain_directories() -> Result<()> {
        let root = tempfile::tempdir()?;
        assert!(Snapshot::load(root.path()).await?.paths.is_empty());
        git(root.path(), &["init", "--quiet"])?;
        std::fs::write(root.path().join("new"), "new")?;
        assert_eq!(
            Snapshot::load(root.path()).await?.status(Path::new("new")),
            Some(Status::Untracked)
        );
        git(root.path(), &["add", "new"])?;
        assert_eq!(
            Snapshot::load(root.path()).await?.status(Path::new("new")),
            Some(Status::Added)
        );
        Ok(())
    }

    #[tokio::test]
    async fn reads_linked_worktree_status() -> Result<()> {
        let root = tempfile::tempdir()?;
        git(root.path(), &["init", "--quiet"])?;
        std::fs::write(root.path().join("file"), "initial")?;
        git(root.path(), &["add", "."])?;
        git(root.path(), &["commit", "--quiet", "-m", "initial"])?;
        let worktree = tempfile::tempdir()?;
        git(
            root.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                worktree.path().to_str().unwrap(),
                "HEAD",
            ],
        )?;
        std::fs::write(worktree.path().join("file"), "modified")?;
        assert_eq!(
            Snapshot::load(worktree.path())
                .await?
                .status(Path::new("file")),
            Some(Status::Modified)
        );
        assert_eq!(
            Snapshot::load(root.path()).await?.status(Path::new("file")),
            None
        );
        Ok(())
    }

    #[test]
    fn parses_staged_unstaged_and_conflicted_files() -> Result<()> {
        let snapshot = Snapshot::parse(
            b"A  added\0 M edited\0M  staged\0MM both\0AM new-edited\0 D dir/deleted\0UU conflict\0AA both-added\0?? new\0!! ignored/\0",
            Path::new(""),
        )?;
        for (path, status) in [
            ("added", Status::Added),
            ("edited", Status::Modified),
            ("staged", Status::Modified),
            ("both", Status::Modified),
            ("new-edited", Status::Added),
            ("dir/deleted", Status::Deleted),
            ("dir", Status::Deleted),
            ("conflict", Status::Conflict),
            ("both-added", Status::Conflict),
            ("new", Status::Untracked),
            ("ignored/child.txt", Status::Ignored),
        ] {
            assert_eq!(snapshot.status(Path::new(path)), Some(status), "{path}");
        }
        assert_eq!(snapshot.status(Path::new("clean")), None);
        Ok(())
    }

    #[test]
    fn parses_renames_and_unusual_filenames_in_a_subdirectory() -> Result<()> {
        let snapshot = Snapshot::parse(
            b"R  sub/new name\0elsewhere/old\0?? sub/line\nbreak\0 M outside\0",
            Path::new("sub"),
        )?;
        assert_eq!(
            snapshot.status(Path::new("new name")),
            Some(Status::Renamed)
        );
        assert_eq!(
            snapshot.status(Path::new("line\nbreak")),
            Some(Status::Untracked)
        );
        assert_eq!(snapshot.status(Path::new("outside")), None);
        Ok(())
    }

    #[test]
    fn folders_use_highest_priority_and_ignored_files_do_not_dirty_parents() -> Result<()> {
        let snapshot = Snapshot::parse(
            b"?? dir/new\0 M dir/edited\0UU dir/nested/conflict\0!! clean/ignored/\0",
            Path::new(""),
        )?;
        assert_eq!(snapshot.status(Path::new("dir")), Some(Status::Conflict));
        assert_eq!(snapshot.status(Path::new("clean")), None);
        assert!(Snapshot::parse(b"R  missing-source\0", Path::new("")).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_paths() -> Result<()> {
        let snapshot = Snapshot::parse(b" M invalid-\xff\0", Path::new(""))?;
        assert_eq!(
            snapshot.status(&path_from_bytes(b"invalid-\xff")?),
            Some(Status::Modified)
        );
        Ok(())
    }
}
