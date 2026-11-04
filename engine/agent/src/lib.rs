//! Headless DashBeam agent: data dir, config, node lock and the `AgentService`.

pub mod approver;
mod cancel;
pub mod config;
pub mod devices;
pub mod events;
pub mod inbound;
pub mod lock;
pub mod metadata;
mod outbound;
pub mod paths;
pub mod registry;
pub mod service;
mod tickets;

pub use service::{AgentOptions, AgentService};
