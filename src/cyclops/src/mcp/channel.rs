//! Opt-in Claude Code Channels preview adapter.
//!
//! The daemon wakes this reader on message events. The host decides when
//! to run its next turn; writing a channel notification is not a claim.

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::thread;

use cyclops_client::{AsyncClient, ClientError, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT};
use cyclops_proto::{InboxListResult, RecipientKey};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::{write_frame, SharedOutput};

pub(super) struct Channel {
    stop: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Channel {
    pub(super) fn start(
        caller: RecipientKey,
        mut output: SharedOutput,
    ) -> Result<Self, ClientError> {
        Self::spawn(forward(
            cyclops_proto::socket_path(),
            caller,
            move |notice| {
                write_frame(&mut output, notice).map_err(|error| {
                    ClientError::Unknown(format!("channel output failed: {error}"))
                })
            },
        ))
    }

    fn spawn(
        work: impl Future<Output = Result<(), ClientError>> + Send + 'static,
    ) -> Result<Self, ClientError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ClientError::NotSent(error.to_string()))?;
        let (stop, stopped) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("cyclops-mcp-channel".into())
            .spawn(move || {
                runtime.block_on(async {
                    tokio::select! {
                        _ = stopped => {},
                        result = work => {
                            if let Err(error) = result {
                                eprintln!("cyclops channel stopped: {error}. Pending messages remain in the mailbox; use cyclops_inbox. Automatic channel reconnect is disabled.");
                            }
                        }
                    }
                });
            })
            .map_err(|error| ClientError::NotSent(error.to_string()))?;
        Ok(Self {
            stop: Some(stop),
            worker: Some(worker),
        })
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn forward(
    socket: PathBuf,
    caller: RecipientKey,
    mut send: impl FnMut(Value) -> Result<(), ClientError>,
) -> Result<(), ClientError> {
    // Subscribe before listing: an arrival in that gap is either in the
    // initial inbox or buffered on the event connection, never lost.
    let mut events = AsyncClient::connect(&socket, DEFAULT_CONNECT_TIMEOUT).await?;
    events
        .subscribe(
            json!({"kinds": ["messages.changed", "messages.route_changed"]}),
            DEFAULT_READ_TIMEOUT,
        )
        .await?;
    let mut reader = AsyncClient::connect(&socket, DEFAULT_CONNECT_TIMEOUT).await?;
    let mut seen = HashSet::new();
    loop {
        let identity = reader
            .request("agent.identity", json!({}), DEFAULT_READ_TIMEOUT)
            .await?;
        if serde_json::from_value::<RecipientKey>(identity["recipient"].clone()).ok()
            != Some(caller)
            || caller.is_admin()
        {
            return Err(ClientError::NotSent(
                "the channel no longer has its authenticated participant".into(),
            ));
        }
        let inbox = reader
            .request("inbox.list", json!({"limit": 128}), DEFAULT_READ_TIMEOUT)
            .await?;
        for notice in new_notices(inbox, &mut seen)? {
            send(notice)?;
            // Give teardown a cancellation point between bounded writes.
            tokio::task::yield_now().await;
        }
        // No timer and no passive inbox claim. A later claim/change exposes
        // the next bounded inbox page and is itself the next wake.
        events.next_event().await?;
    }
}

fn new_notices(inbox: Value, seen: &mut HashSet<String>) -> Result<Vec<Value>, ClientError> {
    let inbox: InboxListResult = serde_json::from_value(inbox)
        .map_err(|error| ClientError::Unknown(format!("invalid channel inbox: {error}")))?;
    let pending: HashSet<String> = inbox
        .entries
        .iter()
        .map(|entry| entry.message_id.to_string())
        .collect();
    seen.retain(|id| pending.contains(id));
    Ok(inbox.entries.into_iter().filter_map(|entry| {
        let message_id = entry.message_id.to_string();
        if !seen.insert(message_id.clone()) {
            return None;
        }
        let subject: String = entry.subject.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
        Some(json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {
                "content": format!("Cyclops message from {}: {}. Use cyclops_claim with message_id {} to read it; this notice is not the body.", entry.sender_label, subject, message_id),
                "meta": {"message_id": message_id, "sender": entry.sender_label}
            }
        }))
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };

    fn inbox(ids: &[&str]) -> Value {
        json!({"entries": ids.iter().map(|id| json!({
            "message_id": id, "sender_label": "reviewer", "subject": "Ready\nfor review",
            "ts": 1, "thread_root": id,
            "body": "must never be copied into a channel notice"
        })).collect::<Vec<_>>()})
    }

    #[test]
    fn notices_use_the_official_shape_without_claiming_or_exposing_bodies() {
        let mut seen = HashSet::new();
        let notices = new_notices(inbox(&["m-one"]), &mut seen).unwrap();
        assert_eq!(notices[0]["method"], "notifications/claude/channel");
        assert_eq!(notices[0]["params"]["meta"]["message_id"], "m-one");
        assert!(!notices[0].to_string().contains("must never be copied"));
        assert!(new_notices(inbox(&["m-one"]), &mut seen)
            .unwrap()
            .is_empty());
        assert_eq!(
            new_notices(inbox(&["m-one", "m-two"]), &mut seen)
                .unwrap()
                .len(),
            1
        );
        new_notices(inbox(&[]), &mut seen).unwrap();
        assert!(
            seen.is_empty(),
            "the dedup set retains only the bounded pending page"
        );
    }

    #[test]
    fn closing_stdio_cancels_the_idle_event_reader_and_joins_its_owner() {
        struct Finished(Arc<AtomicBool>);
        impl Drop for Finished {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let finished = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&finished);
        let (started, wait) = mpsc::channel();
        let channel = Channel::spawn(async move {
            let _finished = Finished(observed);
            started.send(()).unwrap();
            std::future::pending::<Result<(), ClientError>>().await
        })
        .unwrap();
        wait.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(channel);
        assert!(finished.load(Ordering::SeqCst));
    }

    #[test]
    fn unread_stdout_cannot_strand_channel_teardown() {
        let (mut output, _reader) = super::super::output::backpressured_output();
        let (started, wait) = mpsc::channel();
        let channel = Channel::spawn(async move {
            started.send(()).unwrap();
            write_frame(&mut output, json!({"notice": "pending"}))
                .map_err(|error| ClientError::Unknown(error.to_string()))
        })
        .unwrap();
        wait.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (joined, result) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            drop(channel);
            joined.send(()).unwrap();
        });
        result
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        owner.join().unwrap();
    }

    #[tokio::test]
    async fn daemon_change_events_produce_one_notice_per_pending_message_without_claims() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
        use tokio::net::UnixListener;

        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        async fn answer(
            input: &mut BufReader<OwnedReadHalf>,
            output: &mut OwnedWriteHalf,
            method: &str,
            result: Value,
        ) {
            let mut line = String::new();
            input.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], method);
            let response = json!({"id": request["id"], "result": result});
            output
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }

        let root = cyclops_proto::scratch::scratch_dir("cyc-mcp-channel-events");
        std::fs::create_dir(&root).unwrap();
        let _scratch = Scratch(root.clone());
        let socket = root.join("sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let caller: RecipientKey = serde_json::from_value(json!({
            "kind": "headless",
            "workspace_id": "00000000-0000-0000-0000-000000000001",
            "agent_instance_id": "00000000-0000-0000-0000-000000000002"
        }))
        .unwrap();
        let (notices, mut received) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(forward(socket, caller, move |notice| {
            notices.send(notice).unwrap();
            Ok(())
        }));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (events, _) = listener.accept().await.unwrap();
            let (read, mut event_output) = events.into_split();
            let mut event_input = BufReader::new(read);
            let hello = b"{\"cyclops\":\"1.2.0\",\"proto\":1,\"boot_id\":\"fixture\"}\n";
            event_output.write_all(hello).await.unwrap();
            answer(
                &mut event_input,
                &mut event_output,
                "events.subscribe",
                json!({"subscribed": true}),
            )
            .await;

            let (queries, _) = listener.accept().await.unwrap();
            let (read, mut query_output) = queries.into_split();
            let mut query_input = BufReader::new(read);
            query_output.write_all(hello).await.unwrap();
            for (index, ids) in [vec![], vec!["m-one"], vec!["m-one"], vec!["m-one", "m-two"]]
                .into_iter()
                .enumerate()
            {
                if index > 0 {
                    event_output
                        .write_all(b"{\"event\":\"messages.changed\",\"data\":{}}\n")
                        .await
                        .unwrap();
                }
                answer(
                    &mut query_input,
                    &mut query_output,
                    "agent.identity",
                    json!({"recipient": caller}),
                )
                .await;
                answer(
                    &mut query_input,
                    &mut query_output,
                    "inbox.list",
                    inbox(&ids),
                )
                .await;
                let expected = match index {
                    1 => Some("m-one"),
                    3 => Some("m-two"),
                    _ => None,
                };
                if let Some(expected) = expected {
                    let notice = received.recv().await.unwrap();
                    assert_eq!(notice["params"]["meta"]["message_id"], expected);
                }
            }
            assert!(received.try_recv().is_err());
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
    }
}
