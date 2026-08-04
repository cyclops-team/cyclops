//! Modal dialogs in the workspace UI.

/// Active modal dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// Confirm closing a pane that may host a live agent.
    ConfirmClosePane { pane_id: String },
    /// Rename the active tab; buffer holds partial input.
    RenameTab { buffer: String },
    /// Pick a folder path for a new workspace.
    NewWorkspace { buffer: String },
    /// Rename the active workspace.
    RenameWorkspace { buffer: String },
    /// Confirm closing a workspace that may host agents.
    ConfirmCloseWorkspace,
}

impl Dialog {
    pub fn confirm_close(pane_id: impl Into<String>) -> Self {
        Dialog::ConfirmClosePane {
            pane_id: pane_id.into(),
        }
    }
}
