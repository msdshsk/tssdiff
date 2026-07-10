use clap::{Parser, Subcommand};
use tssdiff_core::config::IconMode;
use tssdiff_core::mode::OperationMode;

#[derive(Parser)]
#[command(name = "tssdiff")]
#[command(about = "A read-only TUI diff viewer with history browsing and side-by-side panes")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Git refs, files, or directories to compare
    #[arg(value_name = "REF_OR_PATH")]
    pub targets: Vec<String>,

    /// Show staged changes (equivalent to git diff --cached)
    #[arg(long, short)]
    pub cached: bool,

    /// Show changes in working directory (default)
    #[arg(long, short)]
    pub worktree: bool,

    /// Show changed files as a flat list instead of a tree
    #[arg(long, short = 'f')]
    pub flat: bool,

    /// Launch the desktop GUI (tssdiff-gui) instead of the TUI
    #[arg(long)]
    pub gui: bool,

    /// Icon set override (ascii recommended for plain xterm)
    #[arg(long, value_enum)]
    pub icons: Option<IconMode>,

    /// Configuration file path
    #[arg(long, value_name = "FILE")]
    pub config: Option<String>,

    /// Verbose output
    #[arg(long, short)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Compare git refs, files, or directories
    Diff {
        /// First target (branch, commit, file, or directory)
        target1: String,
        /// Second target (branch, commit, file, or directory)
        target2: Option<String>,
        /// Show staged changes
        #[arg(long)]
        cached: bool,
    },
    /// Show current git status with diffs
    Status,
    /// Show changes introduced by a specific commit (like git show)
    Show {
        /// Commit to review (e.g. HEAD, a1b2c3d, v1.0.0)
        commit: String,
    },
    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

impl Cli {
    pub fn parse_args() -> Self {
        Cli::parse()
    }

    /// Determine the operation mode based on arguments
    pub fn get_operation_mode(&self) -> OperationMode {
        if let Some(command) = &self.command {
            match command {
                Commands::Diff {
                    target1,
                    target2,
                    cached,
                } => {
                    if *cached {
                        OperationMode::GitCached
                    } else if let Some(target2) = target2 {
                        // Two targets: could be refs, files, or directories
                        OperationMode::Compare {
                            target1: target1.clone(),
                            target2: target2.clone(),
                        }
                    } else {
                        // One target: compare with working directory or HEAD
                        OperationMode::GitDiff {
                            target: target1.clone(),
                        }
                    }
                }
                Commands::Status => OperationMode::GitStatus,
                Commands::Show { commit } => OperationMode::GitCommit {
                    commit: commit.clone(),
                },
                // Completions never reach mode dispatch; main handles them first
                Commands::Completions { .. } => OperationMode::Invalid {
                    reason: "completions are handled before mode dispatch".to_string(),
                },
            }
        } else if self.cached {
            OperationMode::GitCached
        } else if self.targets.is_empty() {
            // No arguments: show working directory changes
            OperationMode::GitWorkingDirectory
        } else if self.targets.len() == 1 {
            // One target: compare with working directory or HEAD
            OperationMode::GitDiff {
                target: self.targets[0].clone(),
            }
        } else if self.targets.len() == 2 {
            // Two targets: compare them
            OperationMode::Compare {
                target1: self.targets[0].clone(),
                target2: self.targets[1].clone(),
            }
        } else {
            // Too many arguments
            OperationMode::Invalid {
                reason: "Too many arguments provided".to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_args_gives_working_directory() {
        let cli = Cli {
            command: None,
            targets: vec![],
            cached: false,
            worktree: false,
            flat: false,
            gui: false,
            icons: None,
            config: None,
            verbose: false,
        };

        match cli.get_operation_mode() {
            OperationMode::GitWorkingDirectory => (),
            _ => panic!("Expected GitWorkingDirectory mode"),
        }
    }

    #[test]
    fn test_cached_flag() {
        let cli = Cli {
            command: None,
            targets: vec![],
            cached: true,
            worktree: false,
            flat: false,
            gui: false,
            icons: None,
            config: None,
            verbose: false,
        };

        match cli.get_operation_mode() {
            OperationMode::GitCached => (),
            _ => panic!("Expected GitCached mode"),
        }
    }

    #[test]
    fn test_single_target() {
        let cli = Cli {
            command: None,
            targets: vec!["branch1".to_string()],
            cached: false,
            worktree: false,
            flat: false,
            gui: false,
            icons: None,
            config: None,
            verbose: false,
        };

        match cli.get_operation_mode() {
            OperationMode::GitDiff { target } => assert_eq!(target, "branch1"),
            _ => panic!("Expected GitDiff mode"),
        }
    }

    #[test]
    fn test_show_subcommand() {
        let cli = Cli {
            command: Some(Commands::Show {
                commit: "HEAD".to_string(),
            }),
            targets: vec![],
            cached: false,
            worktree: false,
            flat: false,
            gui: false,
            icons: None,
            config: None,
            verbose: false,
        };

        match cli.get_operation_mode() {
            OperationMode::GitCommit { commit } => assert_eq!(commit, "HEAD"),
            _ => panic!("Expected GitCommit mode"),
        }
    }

    #[test]
    fn test_two_targets() {
        let cli = Cli {
            command: None,
            targets: vec!["branch1".to_string(), "branch2".to_string()],
            cached: false,
            worktree: false,
            flat: false,
            gui: false,
            icons: None,
            config: None,
            verbose: false,
        };

        match cli.get_operation_mode() {
            OperationMode::Compare { target1, target2 } => {
                assert_eq!(target1, "branch1");
                assert_eq!(target2, "branch2");
            }
            _ => panic!("Expected Compare mode"),
        }
    }
}
