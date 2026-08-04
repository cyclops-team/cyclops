//! Mouse hit regions and routing.

#![allow(dead_code)]

use ratatui::layout::Rect;

use crate::layout::SplitDir;

/// What a click or wheel event targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitTarget {
    PaneBody { pane_id: String },
    PaneBorder { pane_id: String },
    PaneSplitRight { pane_id: String },
    PaneSplitDown { pane_id: String },
    Divider { pane_id: String, dir: SplitDir },
    Tab { index: usize },
    SidebarRow { index: usize },
    AttentionIndicator { pane_id: String },
    AppMenu,
}

/// Geometry recorded during render for cell hit-testing.
#[derive(Debug, Clone)]
pub struct PaneGeometry {
    pub pane_id: String,
    pub inner: Rect,
    pub cols: u16,
    pub rows: u16,
}

/// One recorded hit region from the last render pass.
#[derive(Debug, Clone)]
pub struct HitRegion {
    pub rect: Rect,
    pub target: HitTarget,
}

/// Regions painted during the last frame, tested on mouse events.
#[derive(Default)]
pub struct HitMap {
    regions: Vec<HitRegion>,
    pane_geometries: Vec<PaneGeometry>,
}

impl HitMap {
    pub fn clear(&mut self) {
        self.regions.clear();
        self.pane_geometries.clear();
    }

    pub fn push_geometry(&mut self, geometry: PaneGeometry) {
        self.pane_geometries.push(geometry);
    }

    pub fn pane_geometry(&self, pane_id: &str) -> Option<&PaneGeometry> {
        self.pane_geometries.iter().find(|g| g.pane_id == pane_id)
    }

    pub fn pane_geometries(&self) -> &[PaneGeometry] {
        &self.pane_geometries
    }

    pub fn push(&mut self, rect: Rect, target: HitTarget) {
        if rect.width > 0 && rect.height > 0 {
            self.regions.push(HitRegion { rect, target });
        }
    }

    pub fn hit(&self, col: u16, row: u16) -> Option<&HitTarget> {
        self.regions
            .iter()
            .rev()
            .find(|r| {
                col >= r.rect.x
                    && col < r.rect.x + r.rect.width
                    && row >= r.rect.y
                    && row < r.rect.y + r.rect.height
            })
            .map(|r| &r.target)
    }

    pub fn regions(&self) -> &[HitRegion] {
        &self.regions
    }

    /// Map terminal coordinates to a cell inside a pane body.
    pub fn cell_at(geom: &PaneGeometry, col: u16, row: u16) -> Option<crate::runtime::CellPos> {
        if col < geom.inner.x
            || col >= geom.inner.x + geom.inner.width
            || row < geom.inner.y
            || row >= geom.inner.y + geom.inner.height
        {
            return None;
        }
        Some(crate::runtime::CellPos {
            col: col - geom.inner.x,
            row: row - geom.inner.y,
        })
    }
}

/// Open menu state — app menu and context menu are mutually exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuState {
    None,
    AppMenu,
    ContextMenu { pane_id: String },
}

impl MenuState {
    pub fn close(&mut self) {
        *self = MenuState::None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_test_returns_topmost_region() {
        let mut map = HitMap::default();
        map.push(Rect::new(0, 0, 10, 10), HitTarget::Tab { index: 0 });
        map.push(
            Rect::new(2, 2, 4, 4),
            HitTarget::PaneBody {
                pane_id: "%0".into(),
            },
        );
        assert!(matches!(map.hit(3, 3), Some(HitTarget::PaneBody { .. })));
        assert!(matches!(map.hit(9, 9), Some(HitTarget::Tab { index: 0 })));
    }

    #[test]
    fn pane_body_hit_distinguishes_from_border() {
        let mut map = HitMap::default();
        map.push(
            Rect::new(0, 0, 10, 5),
            HitTarget::PaneBorder {
                pane_id: "%0".into(),
            },
        );
        map.push(
            Rect::new(1, 1, 8, 3),
            HitTarget::PaneBody {
                pane_id: "%0".into(),
            },
        );
        assert!(matches!(map.hit(5, 2), Some(HitTarget::PaneBody { .. })));
        assert!(matches!(map.hit(0, 0), Some(HitTarget::PaneBorder { .. })));
    }
}
