//! Git worktree management and project discovery.
//!
//! This crate provides the library layer for worktree lifecycle (create, delete),
//! project discovery (scanning directories for git repos), pretty naming
//! (disambiguated display names), and worktree initialization (copying files,
//! running setup commands). It has no dependency on agents, daemons, or TAM-specific
//! concepts and can be used standalone.

pub mod config;
pub mod discovery;
pub mod git;
pub mod init;
pub mod pretty;
pub mod worktree;
