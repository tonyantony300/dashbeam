//! Headless DashBeam agent: data dir, config, node lock and the `AgentService`.

pub mod approver;
pub mod config;
pub mod devices;
pub mod events;
pub mod lock;
pub mod paths;
pub mod registry;
pub mod service;

pub use service::{AgentOptions, AgentService};
