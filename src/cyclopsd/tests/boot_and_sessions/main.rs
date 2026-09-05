//! Boot and session ownership: the first socket hello, session adoption
//! and watching, pane arrival and departure, the eye across a daemon
//! restart, and the rig's own teardown wiring. Every module here boots the
//! daemon against an isolated tmux server and asserts what it sees before
//! any message is sent.
//!
//! One binary instead of one per module: each integration binary links
//! the whole daemon, and the modules were spending more time linking than
//! running. The modules share one process, so a test that mutates
//! process-global state stays out; `scratch_override` keeps its own binary
//! for that reason.

// The shared rig lives beside the group directories, not inside this one.
#[path = "../common/mod.rs"]
mod common;

mod demos_lib;
mod lifecycle;
mod m0;
mod pane_lifecycle;
mod restart_eye;
mod session_watch;
mod teardown;
