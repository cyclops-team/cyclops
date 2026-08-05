//! Pure workspace model reconciled from tmux.

#![allow(dead_code)]

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

/// Session-level model for the active workspace.
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

/// One workspace row in the sidebar (a tmux session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub name: String,
    pub tab_count: usize,
    pub active: bool,
}

/// Full workspace model: sidebar sessions plus the active session's tabs.
#[derive(Debug)]
pub struct WorkspaceModel {
    pub workspaces: Vec<WorkspaceRow>,
    pub active_workspace: usize,
    pub session: SessionModel,
    /// Sidebar expanded (render state; persisted in step 13).
    pub sidebar_visible: bool,
}

impl WorkspaceModel {
    pub fn active_workspace_name(&self) -> &str {
        &self.workspaces[self.active_workspace].name
    }

    pub fn active_tab(&self) -> &TabModel {
        self.session.active_tab()
    }
}

/// One pane slot with its render rectangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSlot {
    pub pane_id: String,
    pub rect: Rect,
    pub focused: bool,
}

/// `(pane_id, cols, rows)` of every pane the tab shows. A zoomed tab shows
/// only its active pane, at the full window size.
pub fn visible_pane_dims(tab: &TabModel) -> Vec<(String, u16, u16)> {
    if tab.zoomed {
        let root = tab.layout.rect();
        return vec![(tab.active_pane.clone(), root.width, root.height)];
    }
    crate::layout::pane_dims_in_layout(&tab.layout)
}

/// Pane ids the tab shows (zoom-aware).
pub fn visible_pane_ids(tab: &TabModel) -> Vec<String> {
    visible_pane_dims(tab)
        .into_iter()
        .map(|(id, _, _)| id)
        .collect()
}
