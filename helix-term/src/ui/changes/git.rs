//! Read-only local Git review; callers enforce workspace trust before invocation.
use super::*;
use crate::ui::file_tree::git::{Snapshot, Status};
use std::{ffi::OsString, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub(super) async fn files(root: &Path) -> Result<Vec<ChangeFile>> {
    let check = run(root, vec!["rev-parse".into(), "--show-toplevel".into()]).await?;
    anyhow::ensure!(check.0, "This directory is not in a Git repository");
    let snapshot = Snapshot::load(root).await?;
    Ok(snapshot
        .files
        .into_iter()
        .map(|entry| ChangeFile {
            path: entry.path,
            original: entry.original,
            status: match entry.status {
                Status::Added => "added",
                Status::Deleted => "deleted",
                Status::Renamed => "renamed",
                Status::Conflict => "conflict",
                Status::Untracked => "untracked",
                _ => "modified",
            }
            .into(),
            patch: None,
            additions: 0,
            deletions: 0,
        })
        .collect())
}

/// A commit to compare against and a short description of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BranchPoint {
    pub commit: String,
    pub label: String,
}

async fn text(root: &Path, args: &[&str]) -> Result<Option<String>> {
    let (success, output, _) = run(root, args.iter().map(Into::into).collect()).await?;
    Ok(success.then(|| String::from_utf8_lossy(&output).trim().to_owned()))
}

/// Where the current branch split from the repository's default branch:
/// `origin/HEAD` when the remote records one, otherwise `main` or `master`.
pub(crate) async fn branch_point(root: &Path) -> Result<BranchPoint> {
    let remote_default = text(
        root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await?
    .filter(|name| !name.is_empty());
    let candidates = remote_default
        .into_iter()
        .chain(["main", "master", "origin/main", "origin/master"].map(String::from));
    for candidate in candidates {
        let spec = format!("{candidate}^{{commit}}");
        if text(root, &["rev-parse", "--verify", "--quiet", &spec])
            .await?
            .is_none()
        {
            continue;
        }
        let commit = text(root, &["merge-base", "HEAD", &candidate])
            .await?
            .filter(|commit| !commit.is_empty())
            .with_context(|| {
                format!("The current branch has no common history with {candidate}")
            })?;
        let short = commit.get(..8).unwrap_or(&commit);
        return Ok(BranchPoint {
            label: format!("{candidate} ({short})"),
            commit,
        });
    }
    anyhow::bail!("No default branch found: expected origin/HEAD, main, or master")
}

/// File contents at a revision, or empty when the file did not exist there.
/// `path` is relative to `root`.
pub(crate) async fn file_at(root: &Path, revision: &str, path: &Path) -> Result<Vec<u8>> {
    validate_relative(path)?;
    let object = format!("{revision}:./{}", path.to_string_lossy());
    let exists = run(
        root,
        vec!["cat-file".into(), "-e".into(), object.clone().into()],
    )
    .await?
    .0;
    if !exists {
        return Ok(Vec::new());
    }
    let (success, output, _) = run(root, vec!["show".into(), object.into()]).await?;
    anyhow::ensure!(
        success,
        "Git could not read {} at {revision}",
        path.display()
    );
    Ok(output)
}

/// Files that differ between a commit and the working tree, plus untracked files.
pub(super) async fn branch_files(root: &Path, commit: &str) -> Result<Vec<ChangeFile>> {
    let (success, output, _) = run(
        root,
        [
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--relative",
            "--name-status",
            "-z",
            "--find-renames",
            commit,
            "--",
        ]
        .into_iter()
        .map(Into::into)
        .collect(),
    )
    .await?;
    anyhow::ensure!(success, "Git could not compare with {commit}");
    let mut files = Vec::new();
    let mut fields = output
        .split(|byte| *byte == 0)
        .map(|field| PathBuf::from(String::from_utf8_lossy(field).into_owned()));
    while let Some(status) = fields.next() {
        let status = status.to_string_lossy().into_owned();
        let Some(kind) = status.chars().next() else {
            continue;
        };
        let original = matches!(kind, 'R' | 'C').then(|| fields.next()).flatten();
        let Some(path) = fields.next() else { break };
        validate_relative(&path)?;
        files.push(ChangeFile {
            path,
            original: original.filter(|_| kind == 'R'),
            status: match kind {
                'A' | 'C' => "added",
                'D' => "deleted",
                'R' => "renamed",
                'U' => "conflict",
                _ => "modified",
            }
            .into(),
            patch: None,
            additions: 0,
            deletions: 0,
        });
    }
    let (success, output, _) = run(
        root,
        ["ls-files", "--others", "--exclude-standard", "-z"]
            .into_iter()
            .map(Into::into)
            .collect(),
    )
    .await?;
    anyhow::ensure!(success, "Git could not list untracked files");
    for path in output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        validate_relative(&path)?;
        files.push(ChangeFile {
            path,
            original: None,
            status: "untracked".into(),
            patch: None,
            additions: 0,
            deletions: 0,
        });
    }
    Ok(files)
}

/// A whole-file patch against `HEAD`, or against `base` for branch review.
pub(super) async fn patch(root: &Path, file: &ChangeFile, base: Option<&str>) -> Result<String> {
    validate_relative(&file.path)?;
    let head = run(
        root,
        vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            "HEAD".into(),
        ],
    )
    .await?
    .0;
    let mut args: Vec<OsString> = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-color-moved",
        // Whole-file context: the review shows each change within its file.
        "--unified=1000000",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let no_index = file.status == "untracked" || !head;
    if no_index {
        anyhow::ensure!(
            !root.join(&file.path).is_dir(),
            "Cannot diff an uncommitted submodule as a regular file"
        );
        args.extend([
            "--no-index".into(),
            "--".into(),
            "/dev/null".into(),
            root.join(&file.path).into_os_string(),
        ]);
    } else {
        args.extend([
            "--find-renames".into(),
            base.unwrap_or("HEAD").into(),
            "--".into(),
        ]);
        if let Some(original) = &file.original {
            validate_relative(original)?;
            args.push(original.clone().into_os_string());
        }
        args.push(file.path.clone().into_os_string());
    }
    let (success, output, code) = run(root, args).await?;
    anyhow::ensure!(
        success || no_index && code == Some(1) && !output.is_empty(),
        "Git diff failed for {}",
        file.path.display()
    );
    let patch = String::from_utf8_lossy(&output).into_owned();
    Ok(if patch.is_empty() {
        "No net change against HEAD. The index and working tree may still differ.\n".into()
    } else {
        patch
    })
}

async fn run(root: &Path, args: Vec<OsString>) -> Result<(bool, Vec<u8>, Option<i32>)> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args([
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("Cannot start Git")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    tokio::time::timeout(Duration::from_secs(20), async move {
        let mut bytes = Vec::new();
        let mut errors = Vec::new();
        let mut stdout = stdout.take(LIMIT + 1);
        let mut stderr = stderr.take(8192);
        let (out, err) = tokio::join!(
            stdout.read_to_end(&mut bytes),
            stderr.read_to_end(&mut errors),
        );
        out?;
        err?;
        if bytes.len() as u64 > LIMIT {
            child.kill().await?;
            anyhow::bail!("Git output exceeds the 2 MiB review limit");
        }
        let status = child.wait().await?;
        Ok((status.success(), bytes, status.code()))
    })
    .await
    .context("Git review timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(root: &Path, args: &[&str]) -> Result<()> {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "user.name=Helix test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "core.hooksPath=",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Fixture Git: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[tokio::test]
    async fn lists_only_changed_files_and_reads_staged_unstaged_deleted_renamed_and_untracked_patches(
    ) -> Result<()> {
        let root = tempfile::tempdir()?;
        setup(root.path(), &["init", "--quiet"])?;
        for name in ["modified", "staged", "deleted", "old", "clean"] {
            std::fs::write(root.path().join(name), format!("initial {name}\n"))?;
        }
        std::fs::write(root.path().join(".gitignore"), "ignored\n")?;
        setup(root.path(), &["add", "."])?;
        setup(root.path(), &["commit", "--quiet", "-m", "initial"])?;
        std::fs::write(root.path().join("modified"), "working change\n")?;
        std::fs::write(root.path().join("staged"), "staged change\n")?;
        setup(root.path(), &["add", "staged"])?;
        std::fs::remove_file(root.path().join("deleted"))?;
        setup(root.path(), &["mv", "old", "new name"])?;
        std::fs::write(root.path().join("untracked"), "untracked change\n")?;
        std::fs::write(root.path().join("ignored"), "ignored\n")?;
        let entries = files(root.path()).await?;
        assert_eq!(entries.len(), 5);
        assert!(!entries
            .iter()
            .any(|file| file.path == Path::new("clean") || file.path == Path::new("ignored")));
        for (name, expected) in [
            ("modified", "+working change"),
            ("staged", "+staged change"),
            ("deleted", "-initial deleted"),
            ("new name", "rename from old"),
            ("untracked", "+untracked change"),
        ] {
            let file = entries
                .iter()
                .find(|file| file.path == Path::new(name))
                .unwrap();
            let content = patch(root.path(), file, None).await?;
            assert!(content.contains(expected), "{name}: {content}");
        }
        let status_before = std::process::Command::new("git")
            .current_dir(root.path())
            .args(["status", "--porcelain=v1", "-z"])
            .output()?
            .stdout;
        for file in &entries {
            patch(root.path(), file, None).await?;
        }
        let status_after = std::process::Command::new("git")
            .current_dir(root.path())
            .args(["status", "--porcelain=v1", "-z"])
            .output()?
            .stdout;
        assert_eq!(
            status_before, status_after,
            "Review must not stage, revert, or write files"
        );
        Ok(())
    }

    #[tokio::test]
    async fn unborn_repositories_and_literal_filenames_work_without_creating_a_commit() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        setup(root.path(), &["init", "--quiet"])?;
        std::fs::write(root.path().join(":(glob)* odd.txt"), "literal file\n")?;
        std::fs::write(root.path().join("staged"), "staged before first commit\n")?;
        setup(root.path(), &["add", "staged"])?;
        let entries = files(root.path()).await?;
        assert_eq!(entries.len(), 2);
        for file in entries {
            assert!(patch(root.path(), &file, None).await?.contains('+'));
        }
        assert!(!std::process::Command::new("git")
            .current_dir(root.path())
            .args(["rev-parse", "--verify", "HEAD"])
            .output()?
            .status
            .success());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_diff_reads_the_link_not_its_external_target() -> Result<()> {
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        setup(root.path(), &["init", "--quiet"])?;
        std::fs::write(outside.path().join("secret"), "OUTSIDE_CONTENT\n")?;
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("link"))?;
        let entries = files(root.path()).await?;
        let content = patch(root.path(), &entries[0], None).await?;
        assert!(content.contains("120000"));
        assert!(!content.contains("OUTSIDE_CONTENT"));
        Ok(())
    }

    #[tokio::test]
    async fn branch_review_compares_with_the_branch_point_including_uncommitted_files() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        setup(root.path(), &["init", "--quiet", "--initial-branch=main"])?;
        std::fs::create_dir(root.path().join("src"))?;
        std::fs::write(root.path().join("src/lib.rs"), "base\n")?;
        std::fs::write(root.path().join("gone.rs"), "gone\n")?;
        setup(root.path(), &["add", "."])?;
        setup(root.path(), &["commit", "--quiet", "-m", "base"])?;
        let base = text(root.path(), &["rev-parse", "HEAD"]).await?.unwrap();
        setup(root.path(), &["checkout", "--quiet", "-b", "feature"])?;
        std::fs::write(root.path().join("src/lib.rs"), "committed\n")?;
        setup(root.path(), &["commit", "--quiet", "-am", "feature"])?;
        // A later commit on main does not move the branch point.
        setup(root.path(), &["checkout", "--quiet", "main"])?;
        std::fs::write(root.path().join("main.rs"), "main\n")?;
        setup(root.path(), &["add", "main.rs"])?;
        setup(root.path(), &["commit", "--quiet", "-m", "main"])?;
        setup(root.path(), &["checkout", "--quiet", "feature"])?;
        std::fs::write(root.path().join("src/lib.rs"), "committed\nuncommitted\n")?;
        std::fs::remove_file(root.path().join("gone.rs"))?;
        std::fs::write(root.path().join("new.rs"), "new\n")?;

        let point = branch_point(root.path()).await?;
        assert_eq!(point.commit, base);
        assert!(point.label.starts_with("main ("), "{}", point.label);
        let mut files = branch_files(root.path(), &point.commit).await?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let summary: Vec<_> = files
            .iter()
            .map(|file| {
                (
                    file.path.to_string_lossy().into_owned(),
                    file.status.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("gone.rs".to_owned(), "deleted"),
                ("new.rs".to_owned(), "untracked"),
                ("src/lib.rs".to_owned(), "modified"),
            ]
        );
        assert_eq!(
            file_at(root.path(), &point.commit, Path::new("src/lib.rs")).await?,
            b"base\n"
        );
        assert!(file_at(root.path(), &point.commit, Path::new("new.rs"))
            .await?
            .is_empty());
        assert!(file_at(root.path(), &point.commit, Path::new("../x"))
            .await
            .is_err());
        let sub = root.path().join("src");
        assert_eq!(
            file_at(&sub, &point.commit, Path::new("lib.rs")).await?,
            b"base\n"
        );
        let relative = branch_files(&sub, &point.commit).await?;
        assert_eq!(relative.len(), 1);
        assert_eq!(relative[0].path, Path::new("lib.rs"));
        let patch = patch(root.path(), &files[2], Some(&point.commit)).await?;
        assert!(
            patch.contains("-base") && patch.contains("+uncommitted"),
            "{patch}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn plain_directories_report_an_error_instead_of_a_false_clean_repository() -> Result<()> {
        let root = tempfile::tempdir()?;
        assert!(files(root.path()).await.is_err());
        Ok(())
    }
}
