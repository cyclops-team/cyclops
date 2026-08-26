//! Pure workspace model reconciled from tmux.

use std::collections::HashMap;

use ratatui::layout::Rect;

use crate::layout::ResolvedLayout;
use crate::runtime::PaneRuntime;

/// One tab (tmux window) in the active session.
#[derive(Debug, Clone)]
pub struct TabModel {
    pub window_id: String,
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
    /// The tab everything renders and routes against.
    ///
    /// Total, not indexed: `active_tab` can go stale between a
    /// `%window-close` and the reconcile that repairs it, and this getter
    /// sits on the `%output` hot path (`is_visible_pane`), so one closing
    /// window in a churning session must not take the workspace down. A
    /// stale index answers with the last tab, and an empty list (a
    /// session snapshot caught mid-teardown) answers with a blank
    /// placeholder tab; either paints wrong for at most one frame before
    /// reconcile lands, which beats both panics.
    pub fn active_tab(&self) -> &TabModel {
        static EMPTY: std::sync::LazyLock<TabModel> = std::sync::LazyLock::new(|| TabModel {
            window_id: String::new(),
            name: String::new(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: String::new(),
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            active_pane: String::new(),
            zoomed: false,
        });
        self.tabs
            .get(self.active_tab)
            .or_else(|| self.tabs.last())
            .unwrap_or(&EMPTY)
    }
}

/// One workspace row in the sidebar (a tmux session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    /// Stable tmux identity, retained across session renames.
    pub session_id: String,
    pub name: String,
    pub tab_count: usize,
    /// Stable window ids currently linked into this session. Sidebar agent
    /// membership follows these ids, so a session rename cannot orphan rows.
    pub window_ids: Vec<String>,
}

/// Full workspace model: sidebar sessions plus the active session's tabs.
#[derive(Debug)]
pub struct WorkspaceModel {
    pub workspaces: Vec<WorkspaceRow>,
    pub active_workspace: usize,
    pub session: SessionModel,
    /// Sidebar expanded (render state; persisted in step 13).
    pub sidebar_visible: bool,
    /// Messages drawer expanded on the right edge.
    pub messages_visible: bool,
}

impl WorkspaceModel {
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

/// Whether `pane_id` is visible on this tab, including zoom semantics.
pub fn pane_is_visible(tab: &TabModel, pane_id: &str) -> bool {
    if tab.zoomed {
        tab.active_pane == pane_id
    } else {
        crate::layout::layout_contains_pane(&tab.layout, pane_id)
    }
}
