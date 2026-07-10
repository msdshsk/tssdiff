//! Backend-agnostic repository access for the viewers. `RepoBackend`
//! fronts either the external git CLI (`GitExecutor`, default) or the
//! built-in pure-Rust gitoxide backend (`pure-git` feature), selected
//! through `git.backend` in the config: auto / cli / pure.

use crate::config::GitBackendKind;
use crate::git::{CommitInfo, GitExecutor};
use crate::mode::OperationMode;
use crate::parser::{DiffParser, FileDiff};
use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Version string when the git CLI is on PATH, None otherwise
pub fn git_cli_version() -> Option<String> {
    let out = Command::new("git").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub enum RepoBackend {
    Cli(GitExecutor),
    // boxed: gix repositories are much larger than the unit CLI executor
    #[cfg(feature = "pure-git")]
    Pure(Box<crate::puregit::PureRepo>),
}

impl RepoBackend {
    /// Open `dir` with the requested backend. Auto prefers the git CLI
    /// and falls back to the built-in backend when git is missing.
    ///
    /// The CLI backend operates on the process working directory, so
    /// callers must chdir into `dir` (or the repo root) beforehand.
    pub fn open(dir: &Path, kind: GitBackendKind) -> Result<Self> {
        match kind {
            GitBackendKind::Cli => Self::open_cli(dir),
            GitBackendKind::Pure => Self::open_pure(dir),
            GitBackendKind::Auto => {
                if git_cli_version().is_some() {
                    Self::open_cli(dir)
                } else {
                    Self::open_pure(dir)
                }
            }
        }
    }

    fn open_cli(dir: &Path) -> Result<Self> {
        if !GitExecutor::is_git_repo() {
            return Err(anyhow!("not a git repository: {}", dir.display()));
        }
        Ok(Self::Cli(GitExecutor::new()))
    }

    #[cfg(feature = "pure-git")]
    fn open_pure(dir: &Path) -> Result<Self> {
        Ok(Self::Pure(Box::new(crate::puregit::PureRepo::open(dir)?)))
    }

    #[cfg(not(feature = "pure-git"))]
    fn open_pure(_dir: &Path) -> Result<Self> {
        Err(anyhow!(
            "this build has no pure-git backend; install git or rebuild with the pure-git feature"
        ))
    }

    /// Short backend name for status displays
    pub fn name(&self) -> &'static str {
        match self {
            Self::Cli(_) => "git",
            #[cfg(feature = "pure-git")]
            Self::Pure(_) => "gix",
        }
    }

    pub fn toplevel(&self) -> Result<PathBuf> {
        match self {
            Self::Cli(_) => GitExecutor::toplevel(),
            #[cfg(feature = "pure-git")]
            Self::Pure(repo) => Ok(repo.root().to_path_buf()),
        }
    }

    /// Current branch name, or a short detached-HEAD id
    pub fn branch(&self) -> Option<String> {
        match self {
            Self::Cli(_) => {
                let out = Command::new("git")
                    .args(["rev-parse", "--abbrev-ref", "HEAD"])
                    .output()
                    .ok()?;
                if !out.status.success() {
                    return None;
                }
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            }
            #[cfg(feature = "pure-git")]
            Self::Pure(repo) => repo.branch(),
        }
    }

    /// Files changed for the given mode, with per-file +/- line stats
    pub fn changed_files(&self, mode: &OperationMode) -> Result<Vec<FileDiff>> {
        match self {
            Self::Cli(exec) => Ok(DiffParser::parse(&exec.get_diff(mode)?)),
            #[cfg(feature = "pure-git")]
            Self::Pure(repo) => repo.changed_files(mode),
        }
    }

    /// Full old/new text of one file for the given mode
    pub fn file_versions(&self, mode: &OperationMode, file: &FileDiff) -> Result<(String, String)> {
        match self {
            Self::Cli(exec) => exec.get_file_versions(mode, file),
            #[cfg(feature = "pure-git")]
            Self::Pure(repo) => repo.file_versions(mode, &file.filename),
        }
    }

    pub fn commit_log(&self, limit: usize) -> Result<Vec<CommitInfo>> {
        match self {
            Self::Cli(exec) => exec.get_commit_log(limit),
            #[cfg(feature = "pure-git")]
            Self::Pure(repo) => repo.commit_log(limit),
        }
    }
}
