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
        window_id: String,
    },
    /// Resize the application sidebar, not a tmux pane.
    Sidebar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragPhase {
    Down,
    Moving,
}

/// One in-flight drag: down → threshold → move → up, or Escape cancel.
#[derive(Debug, Clone)]
pub struct DragState {
    pub target: DragTarget,
    pub phase: DragPhase,
    pub start: (u16, u16),
    pub current: (u16, u16),
    /// Position already translated into tmux operations — divider drags
    /// apply live, coalescing motion since the last applied step.
    pub last_applied: (u16, u16),
}

pub const DRAG_THRESHOLD_PX: u16 = 3;

impl DragState {
    pub fn on_down(target: DragTarget, x: u16, y: u16) -> Self {
        DragState {
            target,
            phase: DragPhase::Down,
            start: (x, y),
            current: (x, y),
            last_applied: (x, y),
        }
    }

    pub fn on_move(&mut self, x: u16, y: u16) {
        self.current = (x, y);
        if self.phase == DragPhase::Down && self.past_threshold() {
            self.phase = DragPhase::Moving;
        }
    }

    pub fn past_threshold(&self) -> bool {
        let (sx, sy) = self.start;
        let (cx, cy) = self.current;
        sx.abs_diff(cx) >= DRAG_THRESHOLD_PX || sy.abs_diff(cy) >= DRAG_THRESHOLD_PX
    }

    /// Commit on mouse up. None when the drag never crossed the threshold.
    pub fn on_up(&self) -> Option<DragTarget> {
        if !self.past_threshold() {
            return None;
        }
        Some(self.target.clone())
    }

    pub fn is_active(&self) -> bool {
        self.phase == DragPhase::Moving
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
    fn each_target_variant_lifecycle() {
        for target in [
            DragTarget::Tab {
                window_id: "@0".into(),
            },
            divider(),
            DragTarget::Sidebar,
        ] {
            let mut drag = DragState::on_down(target.clone(), 0, 0);
            drag.on_move(10, 10);
            assert!(drag.is_active());
            assert_eq!(drag.on_up(), Some(target));
        }
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
