//! Pure workspace model reconciled from tmux.

use std::collections::HashMap;

use ratatui::layout::Rect;

use crate::layout::ResolvedLayout;
use crate::runtime::PaneRuntime;

/// One tab (tmux window) in the active session.
#[derive(Debug, Clone)]
pub struct TabModel {
    pub window_id: String,
    pub index: usize,
    pub name: String,
    pub layout: ResolvedLayout,
    pub active_pane: String,
    pub zoomed: bool,
}

/// Live runtimes for panes on the visible tab only.
#[derive(Default)]
pub struct RuntimeRegistry {
    runtimes: HashMap<String, PaneRuntime>,
}

impl RuntimeRegistry {
    pub fn get(&self, pane_id: &str) -> Option<&PaneRuntime> {
        self.runtimes.get(pane_id)
    }

    pub fn get_mut(&mut self, pane_id: &str) -> Option<&mut PaneRuntime> {
        self.runtimes.get_mut(pane_id)
    }

    pub fn insert(&mut self, pane_id: String, runtime: PaneRuntime) {
        self.runtimes.insert(pane_id, runtime);
    }

    pub fn retain_visible(&mut self, visible: &[String]) {
        self.runtimes
            .retain(|id, _| visible.iter().any(|v| v == id));
    }
}

/// Session-level model for step 5 (single session; sidebar arrives step 7).
#[derive(Debug)]
pub struct SessionModel {
    pub session: String,
    pub tabs: Vec<TabModel>,
    pub active_tab: usize,
}

impl SessionModel {
    pub fn active_tab(&self) -> &TabModel {
        &self.tabs[self.active_tab]
    }
}

/// One pane slot with its render rectangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSlot {
    pub pane_id: String,
    pub rect: Rect,
    pub focused: bool,
}
