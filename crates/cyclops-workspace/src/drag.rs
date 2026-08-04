#![allow(dead_code)]

//! Drag state machine for dividers, tabs, and pane drops.

use crate::layout::SplitDir;

/// What a drag operation targets once it crosses the movement threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragTarget {
    Divider {
        pane_id: String,
        dir: SplitDir,
    },
    Tab {
        index: usize,
    },
    TabToWorkspace {
        tab_index: usize,
        workspace_index: usize,
    },
    Pane {
        pane_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragPhase {
    Down,
    Threshold,
    Moving,
}

/// One in-flight drag: down → threshold → move → up, or Escape cancel.
#[derive(Debug, Clone)]
pub struct DragState {
    pub target: DragTarget,
    pub phase: DragPhase,
    pub start: (u16, u16),
    pub current: (u16, u16),
}

pub const DRAG_THRESHOLD_PX: u16 = 3;

impl DragState {
    pub fn on_down(target: DragTarget, x: u16, y: u16) -> Self {
        DragState {
            target,
            phase: DragPhase::Down,
            start: (x, y),
            current: (x, y),
        }
    }

    pub fn on_move(&mut self, x: u16, y: u16) {
        self.current = (x, y);
        if self.phase == DragPhase::Down && self.past_threshold() {
            self.phase = DragPhase::Threshold;
        }
        if self.phase == DragPhase::Threshold || self.phase == DragPhase::Moving {
            self.phase = DragPhase::Moving;
        }
    }

    pub fn past_threshold(&self) -> bool {
        let (sx, sy) = self.start;
        let (cx, cy) = self.current;
        sx.abs_diff(cx) >= DRAG_THRESHOLD_PX || sy.abs_diff(cy) >= DRAG_THRESHOLD_PX
    }

    /// Commit on mouse up. None when the drag never crossed the threshold.
    pub fn on_up(&mut self) -> Option<DragTarget> {
        if !self.past_threshold() {
            *self = Self::on_down(self.target.clone(), self.start.0, self.start.1);
            return None;
        }
        Some(self.target.clone())
    }

    pub fn cancel(&mut self) {
        self.phase = DragPhase::Down;
        self.current = self.start;
    }

    pub fn is_active(&self) -> bool {
        matches!(self.phase, DragPhase::Threshold | DragPhase::Moving)
    }

    /// Resize steps coalesced from a divider drag delta.
    pub fn divider_resize_steps(&self, dir: SplitDir) -> i32 {
        if !self.is_active() {
            return 0;
        }
        let delta = match dir {
            SplitDir::Horizontal => self.current.0 as i32 - self.start.0 as i32,
            SplitDir::Vertical => self.current.1 as i32 - self.start.1 as i32,
        };
        delta.signum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn divider() -> DragTarget {
        DragTarget::Divider {
            pane_id: "%0".into(),
            dir: SplitDir::Horizontal,
        }
    }

    #[test]
    fn down_to_up_without_threshold_commits_nothing() {
        let mut drag = DragState::on_down(divider(), 10, 10);
        drag.on_move(11, 10);
        assert!(!drag.past_threshold());
        assert!(drag.on_up().is_none());
    }

    #[test]
    fn threshold_then_move_then_up() {
        let mut drag = DragState::on_down(divider(), 0, 0);
        drag.on_move(5, 0);
        assert_eq!(drag.phase, DragPhase::Moving);
        let committed = drag.on_up().expect("commit");
        assert!(matches!(committed, DragTarget::Divider { .. }));
    }

    #[test]
    fn escape_cancels() {
        let mut drag = DragState::on_down(DragTarget::Tab { index: 1 }, 0, 0);
        drag.on_move(10, 0);
        drag.cancel();
        assert!(!drag.is_active());
    }

    #[test]
    fn each_target_variant_lifecycle() {
        for target in [
            DragTarget::Tab { index: 0 },
            DragTarget::Pane {
                pane_id: "%1".into(),
            },
            DragTarget::TabToWorkspace {
                tab_index: 0,
                workspace_index: 1,
            },
            divider(),
        ] {
            let mut drag = DragState::on_down(target.clone(), 0, 0);
            drag.on_move(10, 10);
            assert!(drag.is_active());
            assert_eq!(drag.on_up(), Some(target));
        }
    }

    #[test]
    fn divider_resize_sign_follows_motion() {
        let mut drag = DragState::on_down(
            DragTarget::Divider {
                pane_id: "%0".into(),
                dir: SplitDir::Horizontal,
            },
            10,
            5,
        );
        drag.on_move(20, 5);
        assert_eq!(drag.divider_resize_steps(SplitDir::Horizontal), 1);
        drag.start = (20, 5);
        drag.on_move(5, 5);
        assert_eq!(drag.divider_resize_steps(SplitDir::Horizontal), -1);
    }

    #[tokio::test]
    async fn divider_drag_resize_converges_on_rig() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::ControlClient;

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("drag-divider");
        server.run_ok(&["new-session", "-d", "-s", "d", "/bin/sh"]);
        server.run_ok(&["split-window", "-h", "-t", "d"]);
        let cfg = cyclops_tmux::ControlConfig::attach("d")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let out = server.run(&["list-panes", "-t", "d", "-F", "#{pane_id}"]);
        let pane = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("%0")
            .trim()
            .to_string();
        crate::intent::resize_divider(&client, &pane, SplitDir::Horizontal, 3)
            .await
            .expect("resize");
        let sizes: Vec<String> = String::from_utf8_lossy(
            &server
                .run(&["list-panes", "-t", "d", "-F", "#{pane_width}"])
                .stdout,
        )
        .lines()
        .map(str::to_string)
        .collect();
        assert_eq!(sizes.len(), 2);
        assert_ne!(
            sizes[0], sizes[1],
            "divider resize should change pane widths"
        );
        client.shutdown().await;
    }
}
