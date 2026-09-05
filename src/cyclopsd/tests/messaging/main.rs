//! The durable mailbox and what surrounds it: acceptance and scheduling
//! (`messaging_coordinator`), the unread projection, body privacy on the
//! pane, history, the name and theme chrome the daemon paints, and the
//! shipped manifests. One binary because every module reads or writes the
//! workspace journal and asserts what the daemon publishes about it.
//!
//! One binary instead of one per module: each integration binary links
//! the whole daemon, and the modules were spending more time linking than
//! running. Rig tags are unique across the modules, so their scratch homes
//! cannot collide inside one process.

// The shared rig lives beside the group directories, not inside this one.
#[path = "../common/mod.rs"]
mod common;

mod body_privacy;
mod m2_history;
mod m4_name;
mod m5_theme;
mod m6_manifests;
mod messaging_coordinator;
mod stage1_unread_projection;
