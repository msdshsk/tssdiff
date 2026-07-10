/// What the viewer is comparing. Built by the CLI (or GUI) frontend and
/// consumed by `git::GitExecutor`. Shell-completion generation is a
/// frontend concern and is handled before a mode is constructed.
#[derive(Debug, Clone)]
pub enum OperationMode {
    /// Compare working directory with HEAD
    GitWorkingDirectory,
    /// Compare staged changes with HEAD
    GitCached,
    /// Compare target with working directory or HEAD
    GitDiff { target: String },
    /// Show git status with diffs
    GitStatus,
    /// Show changes introduced by a single commit
    GitCommit { commit: String },
    /// Compare two targets (refs, files, or directories)
    Compare { target1: String, target2: String },
    /// Invalid arguments
    Invalid { reason: String },
}

impl OperationMode {
    /// Check if this mode requires a git repository
    pub fn requires_git_repo(&self) -> bool {
        match self {
            OperationMode::GitWorkingDirectory
            | OperationMode::GitCached
            | OperationMode::GitDiff { .. }
            | OperationMode::GitStatus
            | OperationMode::GitCommit { .. } => true,
            OperationMode::Compare { .. } | OperationMode::Invalid { .. } => false,
        }
    }

    /// Get a description of this operation mode
    #[allow(dead_code)]
    pub fn description(&self) -> String {
        match self {
            OperationMode::GitWorkingDirectory => "Working directory changes".to_string(),
            OperationMode::GitCached => "Staged changes".to_string(),
            OperationMode::GitDiff { target } => format!("Changes from {target}"),
            OperationMode::GitStatus => "Git status with diffs".to_string(),
            OperationMode::GitCommit { commit } => format!("Changes introduced by {commit}"),
            OperationMode::Compare { target1, target2 } => {
                format!("Comparing {target1} with {target2}")
            }
            OperationMode::Invalid { reason } => format!("Invalid: {reason}"),
        }
    }
}
