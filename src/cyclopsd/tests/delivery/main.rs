//! The write boundary: the delivery gate's three checks, `agent.wait`,
//! hook receipts, the write-readiness matrix over the shipped manifests,
//! and a live agent receiving through the mailbox. Every module decides
//! whether and when a row may reach a pane, which is the contract the
//! daemon must never get wrong.
//!
//! One binary instead of one per module: each integration binary links
//! the whole daemon, and the modules were spending more time linking than
//! running. Rig tags are unique across the modules, so their scratch homes
//! cannot collide inside one process.

// The shared rig lives beside the group directories, not inside this one.
#[path = "../common/mod.rs"]
mod common;

mod gate;
mod live_use_receive;
mod m2_hooks;
mod wait;
mod write_readiness_matrix;
