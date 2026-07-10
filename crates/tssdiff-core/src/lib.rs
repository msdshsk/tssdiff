//! UI-agnostic core of tssdiff: git access, diff parsing and alignment,
//! syntax highlighting, configuration, and agent feedback. Consumed by
//! the TUI (`tssdiff`) and the desktop GUI (`tssdiff-gui`).

pub mod agent;
pub mod config;
pub mod diff;
pub mod git;
pub mod highlight;
pub mod icons;
pub mod mode;
pub mod parser;
pub mod persistence;
pub mod side_by_side;
pub mod theme;
pub mod tree;
