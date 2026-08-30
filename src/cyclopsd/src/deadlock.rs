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
        let route =
            match crate::messaging_runtime::notification_route(inner, service, record.recipient) {
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
        candidates.push(DeadlockCandidate {
            message_id: record.message_id,
            notification_attempt: record.attempt_id,
            recipient: record.recipient,
            recipient_label: route.label,
            pane_id: route.pane_id,
            pane_pid: route.row.pane_pid,
        });
    }
    diagnostics_for_candidates(candidates, ProcessSnapshot::read)
}

struct DeadlockCandidate {
    message_id: cyclops_proto::MessageId,
    notification_attempt: cyclops_proto::NotificationAttemptId,
    recipient: cyclops_proto::RecipientKey,
    recipient_label: String,
    pane_id: String,
    pane_pid: i32,
}

fn diagnostics_for_candidates(
    candidates: Vec<DeadlockCandidate>,
    read_processes: impl FnOnce() -> Option<ProcessSnapshot>,
) -> Vec<StatusDiagnostic> {
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut seen_attempts = HashSet::new();
    let mut diagnostics = Vec::new();
    if let Some(processes) = read_processes() {
        for candidate in candidates {
            if seen_attempts.insert(candidate.notification_attempt)
                && processes.pane_runs_watch(candidate.pane_pid)
            {
                diagnostics.push(StatusDiagnostic {
                    code: "deadlock_risk".into(),
                    message_id: candidate.message_id,
                    notification_attempt: candidate.notification_attempt,
                    recipient: candidate.recipient,
                    recipient_label: candidate.recipient_label,
                    pane_id: candidate.pane_id,
                });
            }
        }
    }

    diagnostics
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
    use std::str::FromStr;

    use cyclops_proto::{MessageId, NotificationAttemptId, RecipientKey, WorkspaceId};

    fn candidate() -> DeadlockCandidate {
        DeadlockCandidate {
            message_id: MessageId::new("m-deadlock").unwrap(),
            notification_attempt: NotificationAttemptId::generate(),
            recipient: RecipientKey::admin(
                WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap(),
            ),
            recipient_label: "agent".into(),
            pane_id: "%1".into(),
            pane_pid: 100,
        }
    }

    #[test]
    fn empty_candidates_skip_the_process_snapshot_read() {
        let diagnostics = diagnostics_for_candidates(Vec::new(), || {
            panic!("process snapshot must not be read without candidates")
        });

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn nonempty_candidates_keep_process_snapshot_diagnostics() {
        let mut processes = ProcessSnapshot::default();
        processes.foreground_group_by_pid.insert(100, 220);
        processes.watch_groups.insert(220);

        let diagnostics = diagnostics_for_candidates(vec![candidate()], || Some(processes));

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "deadlock_risk");
        assert_eq!(diagnostics[0].pane_id, "%1");
    }

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
