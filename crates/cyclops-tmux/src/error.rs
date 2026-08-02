//! One error type for the whole adapter.

use thiserror::Error;

/// Everything that can go wrong talking to tmux.
#[derive(Debug, Error)]
pub enum TmuxError {
    /// The tmux binary failed to start, or died during the attach handshake.
    /// Carries stderr text when tmux produced any.
    #[error("tmux spawn failed: {0}")]
    Spawn(String),

    /// Plain IO on the control pipes.
    #[error("tmux io: {0}")]
    Io(#[from] std::io::Error),

    /// tmux answered a command with a %error block. Carries the error text.
    #[error("tmux command error: {0}")]
    Command(String),

    /// We refused to put something on the wire (for example a command
    /// containing a newline), or the stream broke an assumption we hold.
    #[error("tmux protocol: {0}")]
    Protocol(String),

    /// The control connection is gone. Pending and future commands fail with
    /// this until the owner reconnects.
    #[error("tmux control connection closed")]
    Disconnected,

    /// No reply within the command timeout. A command-level failure only:
    /// the connection is not torn down, and correlation stays intact
    /// because the reply slot is consumed in FIFO order when the late
    /// reply arrives. Only [`TmuxError::Disconnected`] means the transport
    /// is gone.
    #[error("tmux reply timeout for: {0}")]
    Timeout(String),
}
