//! Volatile last-active workspace/tab state for the workspace UI.

use std::collections::HashMap;
use std::sync::Mutex;

use cyclops_proto::{WorkspaceUiGetResult, WorkspaceUiSetParams};

/// Daemon-side last-active map: session name → window id.
#[derive(Debug, Default)]
pub struct WorkspaceUiState {
    pub last_active_session: Option<String>,
    pub last_active_window: HashMap<String, String>,
}

impl WorkspaceUiState {
    pub fn get(&self) -> WorkspaceUiGetResult {
        let session = self.last_active_session.clone();
        let window_id = session
            .as_ref()
            .and_then(|s| self.last_active_window.get(s).cloned());
        WorkspaceUiGetResult {
            last_active_session: session,
            last_active_window: window_id,
        }
    }

    pub fn set(&mut self, params: &WorkspaceUiSetParams) {
        self.last_active_session = Some(params.session.clone());
        self.last_active_window
            .insert(params.session.clone(), params.window_id.clone());
    }
}

pub fn workspace_ui_get(state: &Mutex<WorkspaceUiState>) -> WorkspaceUiGetResult {
    state.lock().map(|s| s.get()).unwrap_or_default()
}

pub fn workspace_ui_set(state: &Mutex<WorkspaceUiState>, params: &WorkspaceUiSetParams) {
    if let Ok(mut guard) = state.lock() {
        guard.set(params);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_last_active() {
        let state = Mutex::new(WorkspaceUiState::default());
        workspace_ui_set(
            &state,
            &WorkspaceUiSetParams {
                session: "main".into(),
                window_id: "@2".into(),
                protocol_version: 1,
            },
        );
        let got = workspace_ui_get(&state);
        assert_eq!(got.last_active_session.as_deref(), Some("main"));
        assert_eq!(got.last_active_window.as_deref(), Some("@2"));
    }
}
