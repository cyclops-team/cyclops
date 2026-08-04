//! Mouse hit regions and routing.

use ratatui::layout::Rect;

/// What a click or wheel event targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitTarget {
    PaneBody { pane_id: String },
    PaneSplitRight { pane_id: String },
    PaneSplitDown { pane_id: String },
    Tab { index: usize },
    SidebarRow { index: usize },
    AppMenu,
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
}

impl HitMap {
    pub fn clear(&mut self) {
        self.regions.clear();
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
        map.push(Rect::new(2, 2, 4, 4), HitTarget::PaneBody {
            pane_id: "%0".into(),
        });
        assert!(matches!(
            map.hit(3, 3),
            Some(HitTarget::PaneBody { .. })
        ));
        assert!(matches!(map.hit(9, 9), Some(HitTarget::Tab { index: 0 })));
    }

    #[test]
    fn menus_are_mutually_exclusive_type() {
        let mut menu = MenuState::AppMenu;
        menu.close();
        assert_eq!(menu, MenuState::None);
    }
}
