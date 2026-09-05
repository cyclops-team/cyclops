//! End-to-end journeys across several agents and several messages: the
//! three-agent handoff and the release-gate proof. These are the long
//! scenarios that exercise everything the focused binaries prove one
//! contract at a time.

// The shared rig lives beside the group directories, not inside this one.
#[path = "../common/mod.rs"]
mod common;

mod gate3_release_proof;
mod three_agent_journey;
