//! Headless DashBeam agent: data dir, config, node lock and the `AgentService`.

pub mod approver;
mod cancel;
pub mod config;
pub mod devices;
pub mod events;
pub mod inbound;
pub mod lock;
pub mod messages;
pub mod metadata;
mod outbound;
mod pairing;
pub mod paths;
pub mod registry;
pub mod service;
pub mod tempstore;
mod tickets;

pub use service::{AgentOptions, AgentService};
