//! Who is speaking: peer identity on the socket, sender attribution on a
//! message, hook peer authentication, and the state-permission contract
//! that decides which callers may report state. Every module answers the
//! same question, "is this caller who it claims to be", so they share one
//! binary and one link.
//!
//! The helper agents these tests compile with `rustc` are cached once per
//! module in distinct scratch directories, so sharing a process does not
//! race two compilations onto one output path.

// The shared rig lives beside the group directories, not inside this one.
#[path = "../common/mod.rs"]
mod common;

mod hook_peer_authentication;
mod identity;
mod sender_identity;
mod state_permission_contract;
