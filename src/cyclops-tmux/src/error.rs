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

    /// A control command write or flush failed after the write began. Some
    /// bytes may have reached tmux, so callers must not replay the command or
    /// describe it as definitely unwritten.
    #[error("tmux command write outcome is uncertain: {0}")]
    WriteUncertain(std::io::Error),

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

    /// The fixed reply FIFO is full. Refusing before another command task
    /// waits keeps overload bounded and leaves existing correlation intact.
    #[error("tmux control reply queue is full")]
    Busy,

    /// A layout could not be read off a session or built onto one: a
    /// window that is not a grid of rows, a zoomed pane, a session that
    /// already exists. Carries the whole sentence to show the human,
    /// because only this layer knows which window and which pane.
    #[error("{0}")]
    Layout(String),

    /// No reply within the command timeout. A command-level failure only:
    /// the connection is not torn down, and correlation stays intact
    /// because the reply slot is consumed in FIFO order when the late
    /// reply arrives. Only [`TmuxError::Disconnected`] means the transport
    /// is gone.
    #[error("tmux reply timeout for: {0}")]
    Timeout(String),
}
