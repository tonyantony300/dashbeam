//! The `dashbeam` binary's library: node socket (`ipc`), auto-start
//! (`daemon`), subcommands and the terminal approver.

pub mod commands;
pub mod daemon;
pub mod ipc;
pub mod prompt;
