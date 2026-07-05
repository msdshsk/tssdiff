use crate::cli::OperationMode;
use crate::parser::FileDiff;
use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Hash of git's well-known empty tree, used as the diff base for root commits
const EMPTY_TREE_HASH: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// One entry of the commit history list
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub hash: String,
    pub date: String,
    pub subject: String,
}

/// Git command executor for getting diff data
pub struct GitExecutor;

impl GitExecutor {
    pub fn new() -> Self {
        Self
    }

    /// Check if we're in a git repository
    pub fn is_git_repo() -> bool {
        Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    /// Get diff output based on operation mode
    pub fn get_diff(&self, mode: &OperationMode) -> Result<String> {
        match mode {
            OperationMode::GitWorkingDirectory | OperationMode::GitStatus => {
                let mut diff = self.execute_git_diff(&["diff"])?;
                diff.push_str(&self.untracked_files_diff()?);
                Ok(diff)
            }
            OperationMode::GitCached => self.execute_git_diff(&["diff", "--cached"]),
            OperationMode::GitDiff { target } => self.execute_git_diff(&["diff", target]),
            OperationMode::GitCommit { commit } => {
                let base = self.commit_diff_base(commit);
                self.execute_git_diff(&["diff", &base, commit])
            }
            OperationMode::Compare { target1, target2 } => {
                // Check if both targets are git refs
                if self.is_git_ref(target1)? && self.is_git_ref(target2)? {
                    self.execute_git_diff(&["diff", &format!("{target1}..{target2}")])
                } else {
                    // Fall back to regular diff for files/directories
                    self.execute_regular_diff(target1, target2)
                }
            }
            OperationMode::Completions { .. } => {
                Err(anyhow!("Completions mode should not call get_diff"))
            }
            OperationMode::Invalid { reason } => Err(anyhow!("Invalid operation mode: {}", reason)),
        }
    }

    /// Get list of files that have changes
    #[allow(dead_code)]
    pub fn get_changed_files(&self, mode: &OperationMode) -> Result<Vec<String>> {
        match mode {
            OperationMode::GitWorkingDirectory => {
                self.execute_git_name_only(&["diff", "--name-only"])
            }
            OperationMode::GitCached => {
                self.execute_git_name_only(&["diff", "--cached", "--name-only"])
            }
            OperationMode::GitDiff { target } => {
                self.execute_git_name_only(&["diff", "--name-only", target])
            }
            OperationMode::GitStatus => self.execute_git_name_only(&["diff", "--name-only"]),
            OperationMode::GitCommit { commit } => {
                let base = self.commit_diff_base(commit);
                self.execute_git_name_only(&["diff", "--name-only", &base, commit])
            }
            OperationMode::Compare { target1, target2 } => {
                if self.is_git_ref(target1)? && self.is_git_ref(target2)? {
                    self.execute_git_name_only(&[
                        "diff",
                        "--name-only",
                        &format!("{target1}..{target2}"),
                    ])
                } else {
                    // For file/directory comparison, return the file paths
                    Ok(vec![target1.clone(), target2.clone()])
                }
            }
            OperationMode::Completions { .. } => Err(anyhow!(
                "Completions mode should not call get_changed_files"
            )),
            OperationMode::Invalid { reason } => Err(anyhow!("Invalid operation mode: {}", reason)),
        }
    }

    /// Get diff for a specific file
    pub fn get_file_diff(&self, mode: &OperationMode, file_path: &str) -> Result<String> {
        match mode {
            OperationMode::GitWorkingDirectory | OperationMode::GitStatus => {
                let diff = self.execute_git_diff(&["diff", "--", file_path])?;
                if diff.is_empty() {
                    // Untracked files produce no output from `git diff`
                    self.untracked_file_diff(file_path)
                } else {
                    Ok(diff)
                }
            }
            OperationMode::GitCached => {
                self.execute_git_diff(&["diff", "--cached", "--", file_path])
            }
            OperationMode::GitDiff { target } => {
                self.execute_git_diff(&["diff", target, "--", file_path])
            }
            OperationMode::GitCommit { commit } => {
                let base = self.commit_diff_base(commit);
                self.execute_git_diff(&["diff", &base, commit, "--", file_path])
            }
            OperationMode::Compare { target1, target2 } => {
                if self.is_git_ref(target1)? && self.is_git_ref(target2)? {
                    self.execute_git_diff(&[
                        "diff",
                        &format!("{target1}..{target2}"),
                        "--",
                        file_path,
                    ])
                } else {
                    // For file comparison, assume the file_path is one of the targets
                    self.execute_regular_diff(target1, target2)
                }
            }
            OperationMode::Completions { .. } => {
                Err(anyhow!("Completions mode should not call get_file_diff"))
            }
            OperationMode::Invalid { reason } => Err(anyhow!("Invalid operation mode: {}", reason)),
        }
    }

    /// Execute git diff command
    fn execute_git_diff(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .output()
            .context("Failed to execute git diff")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Git diff failed: {}", stderr));
        }

        String::from_utf8(output.stdout).context("Git diff output is not valid UTF-8")
    }

    /// Execute git command to get file names only
    #[allow(dead_code)]
    fn execute_git_name_only(&self, args: &[&str]) -> Result<Vec<String>> {
        let output = Command::new("git")
            .args(args)
            .output()
            .context("Failed to execute git diff --name-only")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Git diff --name-only failed: {}", stderr));
        }

        let stdout = String::from_utf8(output.stdout).context("Git output is not valid UTF-8")?;

        Ok(stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| line.to_string())
            .collect())
    }

    /// Execute git diff --no-index for non-git files/directories.
    /// Unlike plain `diff -u`, this emits `diff --git` headers that
    /// DiffParser expects, and requires no external diff binary.
    fn execute_regular_diff(&self, file1: &str, file2: &str) -> Result<String> {
        Self::no_index_diff(file1, file2, None)
    }

    /// Run `git diff --no-index` between two paths, optionally from a
    /// specific working directory (for repo-root-relative paths)
    fn no_index_diff(path1: &str, path2: &str, cwd: Option<&Path>) -> Result<String> {
        // Forward slashes keep the generated headers unquoted on Windows
        let target1 = path1.replace('\\', "/");
        let target2 = path2.replace('\\', "/");

        let mut cmd = Command::new("git");
        cmd.args(["diff", "--no-index", "--", &target1, &target2]);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let output = cmd
            .output()
            .context("Failed to execute git diff --no-index")?;

        // git diff --no-index exits with 1 when the targets differ
        match output.status.code() {
            Some(0) | Some(1) => {
                String::from_utf8(output.stdout).context("Git diff output is not valid UTF-8")
            }
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(anyhow!("Git diff --no-index failed: {}", stderr))
            }
        }
    }

    /// Old and new contents of a changed file, for side-by-side display.
    /// Sides that do not exist (created/deleted files) come back as empty
    /// strings; binary content surfaces as Err so callers can fall back
    /// to the unified view.
    pub fn get_file_versions(
        &self,
        mode: &OperationMode,
        file_diff: &FileDiff,
    ) -> Result<(String, String)> {
        let old_rel = Self::side_path(file_diff.old_path.as_deref(), &file_diff.filename, "a/");
        let new_rel = Self::side_path(file_diff.new_path.as_deref(), &file_diff.filename, "b/");

        match mode {
            OperationMode::GitWorkingDirectory | OperationMode::GitStatus => Ok((
                self.blob_for_side(old_rel, ":")?,
                Self::worktree_for_side(new_rel)?,
            )),
            OperationMode::GitCached => Ok((
                self.blob_for_side(old_rel, "HEAD:")?,
                self.blob_for_side(new_rel, ":")?,
            )),
            OperationMode::GitDiff { target } => Ok((
                self.blob_for_side(old_rel, &format!("{target}:"))?,
                Self::worktree_for_side(new_rel)?,
            )),
            OperationMode::GitCommit { commit } => {
                let base = self.commit_diff_base(commit);
                Ok((
                    self.blob_for_side(old_rel, &format!("{base}:"))?,
                    self.blob_for_side(new_rel, &format!("{commit}:"))?,
                ))
            }
            OperationMode::Compare { target1, target2 } => {
                if self.is_git_ref(target1)? && self.is_git_ref(target2)? {
                    Ok((
                        self.blob_for_side(old_rel, &format!("{target1}:"))?,
                        self.blob_for_side(new_rel, &format!("{target2}:"))?,
                    ))
                } else {
                    // File/directory compare: parsed paths resolve from the
                    // invocation directory, not a repository root
                    Ok((
                        Self::read_text_or_empty(old_rel)?,
                        Self::read_text_or_empty(new_rel)?,
                    ))
                }
            }
            OperationMode::Completions { .. } | OperationMode::Invalid { .. } => {
                Err(anyhow!("Operation mode has no file versions"))
            }
        }
    }

    /// Repo-relative path for one diff side; None for /dev/null (no file)
    fn side_path<'a>(path: Option<&'a str>, fallback: &'a str, prefix: &str) -> Option<&'a str> {
        match path {
            Some("/dev/null") => None,
            Some(p) => Some(p.strip_prefix(prefix).unwrap_or(p)),
            None => Some(fallback),
        }
    }

    fn blob_for_side(&self, side: Option<&str>, rev_prefix: &str) -> Result<String> {
        match side {
            Some(path) => self.show_blob_or_empty(&format!("{rev_prefix}{path}")),
            None => Ok(String::new()),
        }
    }

    fn worktree_for_side(side: Option<&str>) -> Result<String> {
        match side {
            Some(path) => {
                let root = Self::toplevel()?;
                Self::read_text_or_empty(root.join(path).to_str())
            }
            None => Ok(String::new()),
        }
    }

    /// Blob content at `<rev>:<path>`; empty when the path is absent in
    /// that revision (created or deleted file)
    fn show_blob_or_empty(&self, spec: &str) -> Result<String> {
        let output = Command::new("git")
            .args(["show", spec])
            .output()
            .context("Failed to execute git show")?;

        if !output.status.success() {
            return Ok(String::new());
        }

        String::from_utf8(output.stdout).context("File content is not valid UTF-8")
    }

    /// File content from disk; empty when the file does not exist
    fn read_text_or_empty(path: Option<&str>) -> Result<String> {
        let Some(path) = path else {
            return Ok(String::new());
        };
        match std::fs::read(path) {
            Ok(bytes) => String::from_utf8(bytes).context("File content is not valid UTF-8"),
            Err(_) => Ok(String::new()),
        }
    }

    /// Recent commit history, newest first
    pub fn get_commit_log(&self, limit: usize) -> Result<Vec<CommitInfo>> {
        Self::get_commit_log_in(Path::new("."), limit)
    }

    fn get_commit_log_in(dir: &Path, limit: usize) -> Result<Vec<CommitInfo>> {
        let output = Command::new("git")
            .args([
                "log",
                &format!("-n{limit}"),
                "--date=format:%Y-%m-%d %H:%M",
                "--pretty=format:%h%x09%ad%x09%s",
            ])
            .current_dir(dir)
            .output()
            .context("Failed to execute git log")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("git log failed: {}", stderr));
        }

        let stdout = String::from_utf8(output.stdout).context("Git output is not valid UTF-8")?;
        Ok(stdout
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                Some(CommitInfo {
                    hash: parts.next()?.to_string(),
                    date: parts.next()?.to_string(),
                    subject: parts.next().unwrap_or("").to_string(),
                })
            })
            .collect())
    }

    /// Commit header, message, and diffstat (no patch) for previews
    pub fn get_commit_summary(&self, commit: &str) -> Result<String> {
        let output = Command::new("git")
            .args([
                "show",
                "--stat",
                "--color=never",
                "--date=format:%Y-%m-%d %H:%M",
                commit,
            ])
            .output()
            .context("Failed to execute git show")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("git show failed: {}", stderr));
        }

        String::from_utf8(output.stdout).context("Git output is not valid UTF-8")
    }

    /// Short working-tree status for the history preview
    pub fn get_status_summary(&self) -> Result<String> {
        let output = Command::new("git")
            .args(["status", "--short", "--branch"])
            .output()
            .context("Failed to execute git status")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("git status failed: {}", stderr));
        }

        String::from_utf8(output.stdout).context("Git output is not valid UTF-8")
    }

    /// Diff base for reviewing a single commit: its first parent, or the
    /// empty tree for a root commit
    fn commit_diff_base(&self, commit: &str) -> String {
        let parent = format!("{commit}^");
        let has_parent = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &parent])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);

        if has_parent {
            parent
        } else {
            EMPTY_TREE_HASH.to_string()
        }
    }

    /// Repository root of the current working directory
    fn toplevel() -> Result<PathBuf> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("Failed to locate repository root")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Not in a git repository: {}", stderr));
        }

        let path =
            String::from_utf8(output.stdout).context("Repository root is not valid UTF-8")?;
        Ok(PathBuf::from(path.trim()))
    }

    /// Render every untracked file as an all-added diff, since plain
    /// `git diff` omits files that were never added to the index
    fn untracked_files_diff(&self) -> Result<String> {
        Self::untracked_files_diff_in(&Self::toplevel()?)
    }

    fn untracked_files_diff_in(repo_root: &Path) -> Result<String> {
        let mut result = String::new();
        for file in Self::list_untracked_in(repo_root)? {
            result.push_str(&Self::no_index_diff("/dev/null", &file, Some(repo_root))?);
        }
        Ok(result)
    }

    /// Untracked files (recursive, .gitignore respected), relative to repo_root
    fn list_untracked_in(repo_root: &Path) -> Result<Vec<String>> {
        let output = Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(repo_root)
            .output()
            .context("Failed to list untracked files")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("git ls-files failed: {}", stderr));
        }

        let stdout = String::from_utf8(output.stdout).context("Git output is not valid UTF-8")?;
        Ok(stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| line.to_string())
            .collect())
    }

    /// All-added diff for a single untracked file; empty if the file is tracked
    fn untracked_file_diff(&self, file_path: &str) -> Result<String> {
        let repo_root = Self::toplevel()?;

        let tracked = Command::new("git")
            .args(["ls-files", "--error-unmatch", "--", file_path])
            .current_dir(&repo_root)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if tracked {
            return Ok(String::new());
        }

        Self::no_index_diff("/dev/null", file_path, Some(&repo_root))
    }

    /// Check if a string is a valid git ref
    fn is_git_ref(&self, ref_name: &str) -> Result<bool> {
        // First check if it's a file or directory path
        if Path::new(ref_name).exists() {
            return Ok(false);
        }

        // Check if git can resolve it as a ref
        let output = Command::new("git")
            .args(["rev-parse", "--verify", ref_name])
            .output()
            .context("Failed to check git ref")?;

        Ok(output.status.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_executor_creation() {
        let _executor = GitExecutor::new();
        // Just test that we can create it without panicking
    }

    #[test]
    fn test_execute_regular_diff_produces_git_format() {
        let dir = std::env::temp_dir().join(format!("ftdv_diff_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file1 = dir.join("file1.txt");
        let file2 = dir.join("file2.txt");
        std::fs::write(&file1, "test1\n").unwrap();
        std::fs::write(&file2, "test2\n").unwrap();

        let executor = GitExecutor::new();
        let output = executor
            .execute_regular_diff(file1.to_str().unwrap(), file2.to_str().unwrap())
            .unwrap();

        // Output must carry git-format headers so DiffParser can split files
        assert!(output.starts_with("diff --git"), "got: {output}");
        let diffs = crate::parser::DiffParser::parse(&output);
        assert_eq!(diffs.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_execute_regular_diff_identical_files() {
        let dir = std::env::temp_dir().join(format!("ftdv_same_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file1 = dir.join("same1.txt");
        let file2 = dir.join("same2.txt");
        std::fs::write(&file1, "same\n").unwrap();
        std::fs::write(&file2, "same\n").unwrap();

        let executor = GitExecutor::new();
        let output = executor
            .execute_regular_diff(file1.to_str().unwrap(), file2.to_str().unwrap())
            .unwrap();

        assert!(output.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_untracked_files_diff_in_git_format() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path();
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(init.status.success());

        std::fs::create_dir(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub").join("new.txt"), "added line\n").unwrap();

        let untracked = GitExecutor::list_untracked_in(repo).unwrap();
        assert_eq!(untracked, vec!["sub/new.txt".to_string()]);

        let diff = GitExecutor::untracked_files_diff_in(repo).unwrap();
        assert!(diff.contains("diff --git"), "got: {diff}");
        assert!(diff.contains("+added line"), "got: {diff}");

        let parsed = crate::parser::DiffParser::parse(&diff);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].filename, "sub/new.txt");
        assert_eq!(parsed[0].added_lines, 1);
    }

    #[test]
    fn test_get_commit_log_in_parses_entries() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path();
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "user.email", "t@t"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "first commit"]);
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        run(&["commit", "-aqm", "second commit"]);

        let log = GitExecutor::get_commit_log_in(repo, 10).unwrap();
        assert_eq!(log.len(), 2);
        // Newest first
        assert_eq!(log[0].subject, "second commit");
        assert_eq!(log[1].subject, "first commit");
        assert!(!log[0].hash.is_empty());
        assert!(log[0].date.contains('-'), "got date: {}", log[0].date);
    }

    #[test]
    fn test_is_git_repo() {
        // This test will pass if run in a git repository
        // In a non-git directory, it should return false
        let result = GitExecutor::is_git_repo();
        // We can't assert a specific value since it depends on test environment
        // Just ensure it returns a boolean without panicking
        let _is_boolean = matches!(result, true | false);
    }
}
