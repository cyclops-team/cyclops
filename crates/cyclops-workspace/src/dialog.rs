//! Modal dialogs in the workspace UI.
//!
//! Every dialog carries its own target (a pane, window or session id), so
//! a dialog opened from a right-click on a background tab acts on that
//! tab, not on whatever is active by the time the user confirms.

/// Active modal dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// Confirm closing a pane that may host a live agent.
    ConfirmClosePane { pane_id: String },
    /// Name a new tab before it is created; blank uses the next number.
    NewTab { buffer: String },
    /// Assign the pane's Cyclops identity and message address.
    NamePane {
        pane_id: String,
        buffer: String,
        error: Option<String>,
    },
    /// Rename one tab; buffer holds the edited name.
    RenameTab { window_id: String, buffer: String },
    /// Confirm closing a whole tab (kills every pane in it).
    ConfirmCloseTab { window_id: String },
    /// Rename one workspace (tmux session).
    RenameWorkspace { session: String, buffer: String },
    /// Confirm closing a workspace that may host agents.
    ConfirmCloseWorkspace { session: String },
    /// Read-only, scrollable reference generated from the active bindings.
    Keybinds {
        scroll: u16,
        rows: Vec<crate::bindings::BindingHelp>,
    },
}

impl Dialog {
    pub fn confirm_close(pane_id: impl Into<String>) -> Self {
        Dialog::ConfirmClosePane {
            pane_id: pane_id.into(),
        }
    }

    /// Whether the dialog takes typed input (vs a yes/no confirm).
    pub fn has_input(&self) -> bool {
        matches!(
            self,
            Dialog::NewTab { .. }
                | Dialog::NamePane { .. }
                | Dialog::RenameTab { .. }
                | Dialog::RenameWorkspace { .. }
        )
    }
}
