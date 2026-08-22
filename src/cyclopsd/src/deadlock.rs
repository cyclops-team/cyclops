//! Content-free diagnosis for a foreground receive cycle.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use cyclops_proto::{AgentState, StatusDiagnostic};
use tracing::warn;

use crate::Inner;

/// Report a notification that is gated by the same foreground `cyclops
/// watch` process waiting to observe it. Durable route matching happens
/// before process inspection, so a reused pane id cannot inherit the warning.
pub(crate) fn status_diagnostics(inner: &Inner) -> Vec<StatusDiagnostic> {
    let Some(service) = inner.mailbox.as_ref() else {
        return Vec::new();
    };
    let records = match service.gating_notifications() {
        Ok(records) => records,
        Err(error) => {
            warn!(%error, "deadlock diagnostic could not read notification state");
            return Vec::new();
        }
    };

    let mut candidates = Vec::new();
    for record in records {
        let route = match crate::messaging::notification_route(inner, service, record.recipient) {
            Ok(Some(route)) => route,
            Ok(None) => continue,
            Err(error) => {
                warn!(%error, "deadlock diagnostic could not resolve notification route");
                continue;
            }
        };
        if inner.cached_state(route.session_idx, &route.pane_id) != AgentState::Working {
            continue;
        }
        candidates.push((record, route));
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    let Some(processes) = ProcessSnapshot::read() else {
        return Vec::new();
    };
    let mut seen_attempts = HashSet::new();
    candidates
        .into_iter()
        .filter(|(record, _)| seen_attempts.insert(record.attempt_id))
        .filter(|(_, route)| processes.pane_runs_watch(route.row.pane_pid))
        .map(|(record, route)| StatusDiagnostic {
            code: "deadlock_risk".into(),
            message_id: record.message_id,
            notification_attempt: record.attempt_id,
            recipient: record.recipient,
            recipient_label: route.label,
            pane_id: route.pane_id,
        })
        .collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ProcessSnapshot {
    foreground_group_by_pid: HashMap<i32, i32>,
    watch_groups: HashSet<i32>,
}

impl ProcessSnapshot {
    fn read() -> Option<Self> {
        let output = Command::new("ps")
            .args(["-axo", "pid=,pgid=,tpgid=,comm=,args="])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| Self::parse(&String::from_utf8_lossy(&output.stdout)))
    }

    fn parse(output: &str) -> Self {
        let mut snapshot = Self::default();
        for line in output.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 5 {
                continue;
            }
            let (Ok(pid), Ok(group), Ok(foreground_group)) = (
                fields[0].parse::<i32>(),
                fields[1].parse::<i32>(),
                fields[2].parse::<i32>(),
            ) else {
                continue;
            };
            if foreground_group > 0 {
                snapshot
                    .foreground_group_by_pid
                    .insert(pid, foreground_group);
            }
            // macOS truncates `comm` to the display column width. The first
            // argv token preserves the executable path, so accept either
            // kernel spelling but still require an exact basename and verb.
            let command_name = Path::new(fields[3])
                .file_name()
                .and_then(|name| name.to_str());
            let argv_name = Path::new(fields[4])
                .file_name()
                .and_then(|name| name.to_str());
            if (command_name == Some("cyclops") || argv_name == Some("cyclops"))
                && fields.get(5) == Some(&"watch")
            {
                snapshot.watch_groups.insert(group);
            }
        }
        snapshot
    }

    fn pane_runs_watch(&self, pane_pid: i32) -> bool {
        self.foreground_group_by_pid
            .get(&pane_pid)
            .is_some_and(|group| self.watch_groups.contains(group))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_snapshot_requires_an_exact_watch_process_in_the_foreground_group() {
        let snapshot = ProcessSnapshot::parse(
            " 100 100 220 /bin/zsh -zsh\n\
             220 220 220 /opt/bin/cyc /opt/bin/cyclops watch --from gemini\n\
             300 300 300 /opt/bin/cyclops /opt/bin/cyclops status --note watch\n\
             400 400 400 /bin/sh /bin/sh -c echo cyclops watch\n",
        );

        assert!(snapshot.pane_runs_watch(100));
        assert!(!snapshot.pane_runs_watch(300));
        assert!(!snapshot.pane_runs_watch(400));
    }

    #[test]
    fn a_watch_in_another_foreground_group_does_not_match_the_pane() {
        let snapshot = ProcessSnapshot::parse(
            " 100 100 500 /bin/zsh -zsh\n\
             220 220 220 cyclops cyclops watch --to codex\n",
        );

        assert!(!snapshot.pane_runs_watch(100));
    }

    #[test]
    fn missing_foreground_process_evidence_fails_closed() {
        let snapshot = ProcessSnapshot::parse(
            " 100 100 -1 /bin/zsh -zsh\n\
             220 220 220 cyclops cyclops watch --from gemini\n",
        );

        assert!(!snapshot.pane_runs_watch(100));
    }
}
