use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use tempfile::NamedTempFile;
use uuid::Uuid;

/// Create a Codex-managed git worktree and return the workspace path.
///
/// The worktree is created below `$CODEX_HOME/worktrees/<id>/<repo-name>` and
/// starts from the current `HEAD` of `base_cwd`'s repository. When creating a
/// new worktree, staged, unstaged, and untracked local changes from the source
/// checkout are copied into the worktree, matching the Codex app worktree flow.
/// If `base_cwd` is a subdirectory of the repository, the returned path points
/// to the corresponding subdirectory in the new worktree.
pub fn prepare_codex_worktree(codex_home: &Path, base_cwd: Option<&Path>) -> Result<PathBuf> {
    let source_cwd = match base_cwd {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("resolve current directory")?,
    };
    let repo_root = git_stdout(&source_cwd, ["rev-parse", "--show-toplevel"])?
        .trim()
        .to_string();
    let repo_root = PathBuf::from(repo_root);
    let relative_cwd = git_stdout(&source_cwd, ["rev-parse", "--show-prefix"])?;
    let relative_cwd = relative_cwd
        .trim_end_matches(['\r', '\n'])
        .trim_end_matches('/')
        .to_string();
    let repo_name = repo_root
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| {
            anyhow!(
                "could not determine repository name for {}",
                repo_root.display()
            )
        })?;
    let repo_common_dir = git_common_dir(&repo_root)?;

    let worktree_root = codex_home.join("worktrees");
    let id = next_generated_id(&worktree_root, repo_name)?;
    let target = worktree_root.join(id).join(repo_name);

    if target.exists() {
        ensure_existing_worktree(&target, &repo_common_dir)?;
        return workspace_path(&target, &relative_cwd);
    }

    fs::create_dir_all(
        target
            .parent()
            .ok_or_else(|| anyhow!("invalid worktree target: {}", target.display()))?,
    )
    .with_context(|| format!("create parent directory for {}", target.display()))?;

    let status_before = git_stdout(&repo_root, ["status", "--porcelain"])?;
    git_status(
        &repo_root,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            target.as_os_str(),
            OsStr::new("HEAD"),
        ],
    )
    .with_context(|| format!("create git worktree at {}", target.display()))?;

    if !status_before.trim().is_empty() {
        copy_local_changes(&repo_root, &target)
            .with_context(|| format!("copy local changes into {}", target.display()))?;
    }

    workspace_path(&target, &relative_cwd)
}

fn workspace_path(target: &Path, relative_cwd: &str) -> Result<PathBuf> {
    let workspace_path = if relative_cwd.is_empty() {
        target.to_path_buf()
    } else {
        target.join(relative_cwd)
    };
    fs::create_dir_all(&workspace_path)
        .with_context(|| format!("create workspace directory {}", workspace_path.display()))?;
    Ok(workspace_path)
}

fn next_generated_id(worktree_root: &Path, repo_name: &str) -> Result<String> {
    for _ in 0..=u16::MAX {
        let id = Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or_default()
            .chars()
            .take(4)
            .collect::<String>();
        if !worktree_root.join(&id).join(repo_name).exists() {
            return Ok(id);
        }
    }
    Err(anyhow!("could not allocate a unique Codex worktree id"))
}

fn ensure_existing_worktree(path: &Path, source_common_dir: &Path) -> Result<()> {
    let output = run_git(path, ["rev-parse", "--is-inside-work-tree"])?;
    let stdout = String::from_utf8(output.stdout).context("git output was not UTF-8")?;
    if stdout.trim() != "true" {
        return Err(anyhow!(
            "existing path is not a git worktree: {}",
            path.display()
        ));
    }

    let target_common_dir = git_common_dir(path)?;
    if target_common_dir != source_common_dir {
        return Err(anyhow!(
            "existing worktree belongs to a different repository: {}",
            path.display()
        ));
    }

    Ok(())
}

fn git_common_dir(cwd: &Path) -> Result<PathBuf> {
    let common_dir = git_stdout(cwd, ["rev-parse", "--git-common-dir"])?;
    let common_dir = PathBuf::from(common_dir.trim());
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        cwd.join(common_dir)
    };
    fs::canonicalize(&common_dir)
        .with_context(|| format!("resolve git common dir {}", common_dir.display()))
}

fn copy_local_changes(repo_root: &Path, target: &Path) -> Result<()> {
    let staged_patch = run_git(repo_root, ["diff", "--binary", "--cached"])?.stdout;
    let unstaged_patch = run_git(repo_root, ["diff", "--binary"])?.stdout;

    if !staged_patch.is_empty() {
        let patch = write_patch_file(&staged_patch)?;
        git_status(
            target,
            [
                OsStr::new("apply"),
                OsStr::new("--index"),
                OsStr::new("--binary"),
                patch.path().as_os_str(),
            ],
        )?;
    }

    if !unstaged_patch.is_empty() {
        let patch = write_patch_file(&unstaged_patch)?;
        git_status(
            target,
            [
                OsStr::new("apply"),
                OsStr::new("--binary"),
                patch.path().as_os_str(),
            ],
        )?;
    }

    let untracked = run_git(
        repo_root,
        ["ls-files", "--others", "--exclude-standard", "-z"],
    )?
    .stdout;
    for relative in untracked
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let relative = std::str::from_utf8(relative).context("untracked path was not UTF-8")?;
        let source = repo_root.join(relative);
        let destination = target.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
        fs::copy(&source, &destination)
            .with_context(|| format!("copy {} to {}", source.display(), destination.display()))?;
    }

    Ok(())
}

fn write_patch_file(bytes: &[u8]) -> Result<NamedTempFile> {
    let mut patch = NamedTempFile::new().context("create temporary patch file")?;
    patch
        .write_all(bytes)
        .context("write temporary patch file")?;
    Ok(patch)
}

fn git_stdout<const N: usize, S>(cwd: &Path, args: [S; N]) -> Result<String>
where
    S: AsRef<OsStr>,
{
    let output = run_git(cwd, args)?;
    String::from_utf8(output.stdout).context("git output was not UTF-8")
}

fn git_status<const N: usize, S>(cwd: &Path, args: [S; N]) -> Result<()>
where
    S: AsRef<OsStr>,
{
    run_git(cwd, args).map(|_| ())
}

fn run_git<const N: usize, S>(cwd: &Path, args: [S; N]) -> Result<Output>
where
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .args(args.iter().map(AsRef::as_ref))
        .current_dir(cwd)
        .output()
        .with_context(|| format!("run git in {}", cwd.display()))?;
    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(anyhow!(
        "git failed in {} with status {}: {}",
        cwd.display(),
        output.status,
        stderr.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn creates_detached_worktree_and_copies_local_changes() -> Result<()> {
        let repo = TempDir::new()?;
        git_status(repo.path(), ["init"])?;
        fs::write(repo.path().join("tracked.txt"), "base\n")?;
        git_status(repo.path(), ["add", "tracked.txt"])?;
        git_status(
            repo.path(),
            [
                "-c",
                "user.name=Codex Test",
                "-c",
                "user.email=codex@example.com",
                "commit",
                "-m",
                "initial",
            ],
        )?;

        fs::write(repo.path().join("tracked.txt"), "staged\n")?;
        git_status(repo.path(), ["add", "tracked.txt"])?;
        fs::write(repo.path().join("tracked.txt"), "unstaged\n")?;
        fs::write(repo.path().join("untracked.txt"), "new\n")?;

        let codex_home = TempDir::new()?;
        let worktree = prepare_codex_worktree(codex_home.path(), Some(repo.path()))?;

        assert!(worktree.starts_with(codex_home.path().join("worktrees")));
        assert!(worktree_id(&worktree).is_some_and(is_four_hex_chars));
        assert_eq!(git_stdout(&worktree, ["branch", "--show-current"])?, "");
        assert_eq!(
            fs::read_to_string(worktree.join("tracked.txt"))?,
            "unstaged\n"
        );
        assert_eq!(fs::read_to_string(worktree.join("untracked.txt"))?, "new\n");
        assert_eq!(
            git_stdout(&worktree, ["diff", "--cached", "--name-only"])?.trim(),
            "tracked.txt"
        );

        Ok(())
    }

    #[test]
    fn preserves_repository_relative_cwd() -> Result<()> {
        let repo = TempDir::new()?;
        git_status(repo.path(), ["init"])?;
        fs::create_dir_all(repo.path().join("nested"))?;
        fs::write(repo.path().join("nested").join("tracked.txt"), "base\n")?;
        git_status(repo.path(), ["add", "nested/tracked.txt"])?;
        git_status(
            repo.path(),
            [
                "-c",
                "user.name=Codex Test",
                "-c",
                "user.email=codex@example.com",
                "commit",
                "-m",
                "initial",
            ],
        )?;

        let codex_home = TempDir::new()?;
        let workspace =
            prepare_codex_worktree(codex_home.path(), Some(&repo.path().join("nested")))?;

        assert_eq!(
            workspace.file_name().and_then(OsStr::to_str),
            Some("nested")
        );
        let worktree_root = workspace
            .parent()
            .ok_or_else(|| anyhow!("workspace path should have a parent"))?;
        assert!(worktree_id(worktree_root).is_some_and(is_four_hex_chars));
        assert_eq!(git_stdout(&workspace, ["branch", "--show-current"])?, "");
        assert_eq!(fs::read_to_string(workspace.join("tracked.txt"))?, "base\n");

        Ok(())
    }

    fn worktree_id(path: &Path) -> Option<&str> {
        path.parent()?.file_name()?.to_str()
    }

    fn is_four_hex_chars(value: &str) -> bool {
        value.len() == 4 && value.chars().all(|ch| ch.is_ascii_hexdigit())
    }
}
