//! Pure-Rust git backend built on gitoxide (`gix`). Read-only: it
//! covers what the viewers need (changed-file lists, blob and worktree
//! content, history) so tssdiff can run without a git installation.
//! Write operations (stage/commit in the TUI) stay on the CLI backend.

use crate::git::CommitInfo;
use crate::mode::OperationMode;
use crate::parser::FileDiff;
use crate::side_by_side::{self, RowKind};
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub struct PureRepo {
    repo: gix::Repository,
    root: PathBuf,
}

impl PureRepo {
    pub fn open(dir: &Path) -> Result<Self> {
        let repo = gix::discover(dir)
            .with_context(|| format!("not a git repository: {}", dir.display()))?;
        let root = repo
            .workdir()
            .ok_or_else(|| anyhow!("bare repositories are not supported"))?
            .to_path_buf();
        Ok(Self { repo, root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Current branch name, or a short id when HEAD is detached
    pub fn branch(&self) -> Option<String> {
        if let Ok(Some(name)) = self.repo.head_name() {
            return Some(name.shorten().to_string());
        }
        self.repo
            .head_id()
            .ok()
            .map(|id| id.shorten_or_id().to_string())
    }

    /// Files changed for the given mode, stats computed through the
    /// same line alignment the viewer renders
    pub fn changed_files(&self, mode: &OperationMode) -> Result<Vec<FileDiff>> {
        let paths = match mode {
            OperationMode::GitWorkingDirectory | OperationMode::GitStatus => {
                self.status_paths(true)?
            }
            OperationMode::GitCached => self.status_paths(false)?,
            OperationMode::GitCommit { commit } => self.commit_paths(commit)?,
            _ => {
                return Err(anyhow!(
                    "this comparison is not supported by the gix backend"
                ));
            }
        };

        let mut files = Vec::new();
        for path in paths {
            let (old_bytes, new_bytes) = self.side_bytes(mode, &path)?;
            let binary = is_binary(&old_bytes) || is_binary(&new_bytes);
            let (added_lines, removed_lines) = if binary {
                (0, 0)
            } else {
                match (String::from_utf8(old_bytes), String::from_utf8(new_bytes)) {
                    (Ok(old_text), Ok(new_text)) => line_stats(&old_text, &new_text),
                    _ => (0, 0),
                }
            };
            files.push(FileDiff {
                filename: path.clone(),
                old_path: Some(format!("a/{path}")),
                new_path: Some(format!("b/{path}")),
                content: String::new(),
                added_lines,
                removed_lines,
                diff_key: None,
            });
        }
        Ok(files)
    }

    /// Full old/new text of one file for the given mode
    pub fn file_versions(&self, mode: &OperationMode, path: &str) -> Result<(String, String)> {
        let (old_bytes, new_bytes) = self.side_bytes(mode, path)?;
        Ok((to_text(old_bytes, path)?, to_text(new_bytes, path)?))
    }

    pub fn commit_log(&self, limit: usize) -> Result<Vec<CommitInfo>> {
        let Ok(head_id) = self.repo.head_id() else {
            // repository without commits
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for info in self
            .repo
            .rev_walk(Some(head_id.detach()))
            .all()?
            .take(limit)
        {
            let info = info?;
            let commit = self
                .repo
                .find_object(info.id)?
                .try_into_commit()
                .map_err(|_| anyhow!("history walk yielded a non-commit object"))?;
            let time = commit.time()?;
            out.push(CommitInfo {
                hash: info.id.to_hex_with_len(7).to_string(),
                date: format_time(time.seconds, time.offset),
                subject: commit.message()?.summary().to_string(),
                parents: commit
                    .parent_ids()
                    .map(|id| id.detach().to_hex_with_len(7).to_string())
                    .collect(),
            });
        }
        Ok(out)
    }

    /// Raw bytes of both sides of `path` for the given mode; an absent
    /// side (added/deleted file) is an empty buffer
    fn side_bytes(&self, mode: &OperationMode, path: &str) -> Result<(Vec<u8>, Vec<u8>)> {
        match mode {
            OperationMode::GitWorkingDirectory | OperationMode::GitStatus => Ok((
                self.blob_bytes(&format!("HEAD:{path}")),
                self.worktree_bytes(path)?,
            )),
            OperationMode::GitCached => Ok((
                self.blob_bytes(&format!("HEAD:{path}")),
                self.index_bytes(path)?,
            )),
            OperationMode::GitCommit { commit } => Ok((
                // first parent; missing on root commits -> empty side
                self.blob_bytes(&format!("{commit}^:{path}")),
                self.blob_bytes(&format!("{commit}:{path}")),
            )),
            _ => Err(anyhow!(
                "this comparison is not supported by the gix backend"
            )),
        }
    }

    /// Blob content addressed by a `rev:path` spec; empty when the
    /// revision or the path within it does not exist
    fn blob_bytes(&self, spec: &str) -> Vec<u8> {
        let Ok(id) = self.repo.rev_parse_single(gix::bstr::BStr::new(spec)) else {
            return Vec::new();
        };
        match self.repo.find_object(id) {
            Ok(obj) => obj.detach().data,
            Err(_) => Vec::new(),
        }
    }

    fn worktree_bytes(&self, path: &str) -> Result<Vec<u8>> {
        match std::fs::read(self.root.join(path)) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("failed to read {path}")),
        }
    }

    /// Stage-0 index entry content; empty when the path is not staged
    fn index_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let index = self.repo.index_or_empty()?;
        let Some(entry) = index.entry_by_path(path.into()) else {
            return Ok(Vec::new());
        };
        Ok(self
            .repo
            .find_object(entry.id)
            .map(|obj| obj.detach().data)
            .unwrap_or_default())
    }

    /// Paths that differ from HEAD: staged changes always, plus
    /// worktree changes and untracked files when `include_worktree`
    fn status_paths(&self, include_worktree: bool) -> Result<BTreeSet<String>> {
        let platform = self
            .repo
            .status(gix::progress::Discard)?
            .untracked_files(gix::status::UntrackedFiles::Files);
        let mut out = BTreeSet::new();
        for item in platform.into_iter(None)? {
            let item = item?;
            match item {
                gix::status::Item::TreeIndex(change) => {
                    out.insert(change.location().to_string());
                }
                gix::status::Item::IndexWorktree(change) if include_worktree => {
                    use gix::status::index_worktree::Item;
                    match change {
                        Item::Modification { rela_path, .. } => {
                            out.insert(rela_path.to_string());
                        }
                        Item::DirectoryContents { entry, .. } => {
                            out.insert(entry.rela_path.to_string());
                        }
                        Item::Rewrite { dirwalk_entry, .. } => {
                            out.insert(dirwalk_entry.rela_path.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// Paths changed by a commit relative to its first parent (or the
    /// empty tree for root commits)
    fn commit_paths(&self, commit: &str) -> Result<BTreeSet<String>> {
        let commit_id = self
            .repo
            .rev_parse_single(gix::bstr::BStr::new(commit))
            .with_context(|| format!("unknown revision: {commit}"))?;
        let commit_obj = self
            .repo
            .find_object(commit_id)?
            .peel_to_kind(gix::object::Kind::Commit)?
            .into_commit();
        let tree = commit_obj.tree()?;
        let parent_tree = match commit_obj.parent_ids().next() {
            Some(parent) => parent.object()?.into_commit().tree()?,
            None => self.repo.empty_tree(),
        };
        let changes = self
            .repo
            .diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)?;
        Ok(changes
            .iter()
            .map(|change| change.location().to_string())
            .collect())
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn to_text(bytes: Vec<u8>, path: &str) -> Result<String> {
    if is_binary(&bytes) {
        return Err(anyhow!("{path} is not valid UTF-8 (binary file)"));
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("{path} is not valid UTF-8"))
}

/// +/- line counts through the viewer's own alignment (modified rows
/// count on both sides, matching git's numstat for typical edits)
fn line_stats(old_text: &str, new_text: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for row in side_by_side::align(old_text, new_text) {
        match row.kind {
            RowKind::Added => added += 1,
            RowKind::Removed => removed += 1,
            RowKind::Modified => {
                added += 1;
                removed += 1;
            }
            RowKind::Context => {}
        }
    }
    (added, removed)
}

/// "YYYY-MM-DD HH:MM" in the commit author's local offset, matching
/// the CLI backend's `--date=format:%Y-%m-%d %H:%M`
fn format_time(seconds: i64, offset_seconds: i32) -> String {
    let t = seconds + i64::from(offset_seconds);
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// Howard Hinnant's civil_from_days: gregorian date from days since epoch
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().to_path_buf();
        run(&repo, &["init", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.com"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(repo.join("b.txt"), "alpha\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-m", "initial commit"]);
        (temp, repo)
    }

    #[test]
    fn test_worktree_changes_with_untracked() {
        let (_temp, repo) = init_repo();
        std::fs::write(repo.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
        std::fs::write(repo.join("new.txt"), "x\ny\n").unwrap();

        let pure = PureRepo::open(&repo).unwrap();
        let files = pure
            .changed_files(&OperationMode::GitWorkingDirectory)
            .unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.filename.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "new.txt"]);

        let a = &files[0];
        assert_eq!((a.added_lines, a.removed_lines), (2, 1)); // TWO modified + four added
        let new = &files[1];
        assert_eq!((new.added_lines, new.removed_lines), (2, 0));
    }

    #[test]
    fn test_staged_changes_only() {
        let (_temp, repo) = init_repo();
        std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\nstaged\n").unwrap();
        run(&repo, &["add", "a.txt"]);
        std::fs::write(repo.join("b.txt"), "alpha\nworktree-only\n").unwrap();

        let pure = PureRepo::open(&repo).unwrap();
        let files = pure.changed_files(&OperationMode::GitCached).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.filename.as_str()).collect();
        assert_eq!(names, vec!["a.txt"]);

        // staged side comes from the index, not the worktree
        let (old, new) = pure
            .file_versions(&OperationMode::GitCached, "a.txt")
            .unwrap();
        assert_eq!(old, "one\ntwo\nthree\n");
        assert_eq!(new, "one\ntwo\nthree\nstaged\n");
    }

    #[test]
    fn test_commit_mode_and_root_commit() {
        let (_temp, repo) = init_repo();
        std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\nmore\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-m", "second commit"]);

        let pure = PureRepo::open(&repo).unwrap();
        let log = pure.commit_log(10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "second commit");
        assert_eq!(log[0].hash.len(), 7);
        assert!(log[0].date.len() == 16, "date format: {}", log[0].date);

        let files = pure
            .changed_files(&OperationMode::GitCommit {
                commit: log[0].hash.clone(),
            })
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "a.txt");
        assert_eq!((files[0].added_lines, files[0].removed_lines), (1, 0));

        // root commit diffs against the empty tree
        let files = pure
            .changed_files(&OperationMode::GitCommit {
                commit: log[1].hash.clone(),
            })
            .unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.filename.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
        let (old, new) = pure
            .file_versions(
                &OperationMode::GitCommit {
                    commit: log[1].hash.clone(),
                },
                "b.txt",
            )
            .unwrap();
        assert_eq!(old, "");
        assert_eq!(new, "alpha\n");
    }

    #[test]
    fn test_binary_files_are_flagged() {
        let (_temp, repo) = init_repo();
        std::fs::write(repo.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();

        let pure = PureRepo::open(&repo).unwrap();
        let files = pure
            .changed_files(&OperationMode::GitWorkingDirectory)
            .unwrap();
        let bin = files.iter().find(|f| f.filename == "blob.bin").unwrap();
        assert_eq!((bin.added_lines, bin.removed_lines), (0, 0));

        let err = pure
            .file_versions(&OperationMode::GitWorkingDirectory, "blob.bin")
            .unwrap_err();
        assert!(err.to_string().contains("UTF-8"));
    }

    #[test]
    fn test_branch_name() {
        let (_temp, repo) = init_repo();
        let pure = PureRepo::open(&repo).unwrap();
        assert_eq!(pure.branch().as_deref(), Some("main"));
    }

    #[test]
    fn test_format_time() {
        // 2026-07-10 14:30:00 UTC, +09:00 offset
        assert_eq!(format_time(1_783_693_800, 9 * 3600), "2026-07-10 23:30");
        assert_eq!(format_time(0, 0), "1970-01-01 00:00");
    }
}
