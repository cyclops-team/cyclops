//! Scheduled and release evidence for concurrent durable mailbox acceptance.
//!
//! Four independent callers begin together. Each caller keeps one request in
//! flight and submits 32 messages to the administrator mailbox, so its own
//! submission order is defined while the cross-caller interleaving remains an
//! observed property. The administrator mailbox deliberately has no agent
//! route, which keeps notification, terminal injection, and user journeys
//! outside this workload.
//!
//! Every sample owns a fresh `Rig`: its tmux server, daemon, socket, and
//! scratch home are all isolated and torn down before the next sample. Three
//! samples keep the evidence bounded while retaining the individual timings
//! and acceptance order instead of averaging scheduling variation away.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

use common::{composer_pane, tmux_available, Rig, CAT_MANIFEST};
use cyclops_proto::{Kind, LedgerLine, MsgSendParams};
use serde_json::{json, Value};

const CALLER_COUNT: usize = 4;
const MESSAGES_PER_CALLER: usize = 32;
const SAMPLE_COUNT: usize = 3;
const FIXTURE_BODY: &str = "fixture body omitted from performance evidence";

#[derive(Debug)]
struct AcceptedMessage {
    ordinal: usize,
    message_id: String,
    request_elapsed_us: u64,
    seq: u64,
}

#[derive(Debug)]
struct CallerSample {
    caller: usize,
    accepted: Vec<AcceptedMessage>,
}

fn message_params(sample: usize, caller: usize, ordinal: usize) -> MsgSendParams {
    serde_json::from_value(json!({
        "to": ["admin"],
        "subject": "Concurrent mailbox acceptance fixture",
        "summary": "Measure durable acceptance only. Do not measure terminal effects.",
        "body": FIXTURE_BODY,
        "client_key": format!("concurrent-acceptance-{sample}-{caller}-{ordinal}"),
    }))
    .expect("fixed workload params")
}

fn as_u64(value: &Value, field: &str) -> u64 {
    value[field]
        .as_u64()
        .unwrap_or_else(|| panic!("accepted receipt has {field}: {value}"))
}

fn as_string(value: &Value, field: &str) -> String {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("accepted receipt has {field}: {value}"))
        .to_string()
}

fn run_caller(
    daemon: &cyclopsd::Daemon,
    sample: usize,
    caller: usize,
    ready: Arc<Barrier>,
    start: Arc<Barrier>,
) -> CallerSample {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("caller runtime builds");
    ready.wait();
    start.wait();

    let mut accepted = Vec::with_capacity(MESSAGES_PER_CALLER);
    for ordinal in 0..MESSAGES_PER_CALLER {
        let request_started = Instant::now();
        let receipt = runtime
            .block_on(daemon.msg_send("admin", message_params(sample, caller, ordinal)))
            .unwrap_or_else(|error| panic!("caller {caller} message {ordinal} failed: {error:?}"));
        assert_eq!(
            receipt["inserted"], true,
            "caller {caller} message {ordinal} was not a new durable acceptance"
        );
        let deliveries = receipt["deliveries"]
            .as_array()
            .expect("acceptance receipt has deliveries");
        assert_eq!(
            deliveries.len(),
            1,
            "administrator fixture has one mailbox recipient"
        );
        assert_eq!(
            deliveries[0]["notification_state"], "not_started",
            "this workload must not schedule a terminal notification"
        );
        accepted.push(AcceptedMessage {
            ordinal,
            message_id: as_string(&receipt, "msg_id"),
            request_elapsed_us: request_started
                .elapsed()
                .as_micros()
                .try_into()
                .expect("request duration fits u64"),
            seq: as_u64(&receipt, "seq"),
        });
    }
    CallerSample { caller, accepted }
}

fn workspace_message_lines(home: &std::path::Path) -> Vec<LedgerLine> {
    let workspace_id = std::fs::read_to_string(home.join("identity/workspace-id"))
        .expect("workspace identity")
        .trim()
        .to_string();
    let journal = home
        .join("workspaces")
        .join(workspace_id)
        .join("messages.ndjson");
    std::fs::read_to_string(journal)
        .expect("workspace journal")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("complete workspace journal line"))
        .filter(|line: &LedgerLine| line.kind == Kind::Msg)
        .collect()
}

fn max_consecutive_callers(order: &[usize]) -> usize {
    let mut max_run = 0;
    let mut current_run = 0;
    let mut previous = None;
    for caller in order {
        if Some(*caller) == previous {
            current_run += 1;
        } else {
            current_run = 1;
            previous = Some(*caller);
        }
        max_run = max_run.max(current_run);
    }
    max_run
}

fn latency_distribution(mut samples: Vec<u64>) -> Value {
    assert!(!samples.is_empty(), "latency distribution needs samples");
    samples.sort_unstable();
    let percentile = |numerator: usize| {
        let index = (samples.len() * numerator).div_ceil(100).saturating_sub(1);
        samples[index]
    };
    json!({
        "unit": "microseconds",
        "sample_count": samples.len(),
        "p50": percentile(50),
        "p95": percentile(95),
        "max": *samples.last().expect("nonempty latency distribution"),
    })
}

fn body_free_sample_evidence(sample: usize, callers: Vec<CallerSample>, rig: &Rig) -> Value {
    let mut by_sequence = callers
        .iter()
        .flat_map(|caller| {
            caller
                .accepted
                .iter()
                .map(move |accepted| (accepted.seq, caller.caller, accepted))
        })
        .collect::<Vec<_>>();
    by_sequence.sort_unstable_by_key(|(seq, _, _)| *seq);

    let expected_message_ids = by_sequence
        .iter()
        .map(|(_, _, accepted)| accepted.message_id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        expected_message_ids.len(),
        CALLER_COUNT * MESSAGES_PER_CALLER,
        "every concurrent acceptance must receive a distinct durable id"
    );
    let expected_sequences = by_sequence
        .iter()
        .map(|(sequence, _, _)| *sequence)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        expected_sequences.len(),
        CALLER_COUNT * MESSAGES_PER_CALLER,
        "every concurrent acceptance must receive a distinct durable sequence"
    );

    let journal_messages = workspace_message_lines(&rig.home);
    assert!(
        journal_messages
            .windows(2)
            .all(|pair| pair[0].seq < pair[1].seq),
        "message journal sequence must remain strictly increasing"
    );
    let journal_ids = journal_messages
        .iter()
        .map(|line| line.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        journal_ids, expected_message_ids,
        "the journal must retain every accepted message and no extra fixture message"
    );
    let journal_by_id = journal_messages
        .iter()
        .map(|line| (line.id.as_str(), line.seq))
        .collect::<BTreeMap<_, _>>();
    for (seq, _, accepted) in &by_sequence {
        assert_eq!(
            journal_by_id.get(accepted.message_id.as_str()),
            Some(seq),
            "receipt sequence must be the durable journal sequence"
        );
    }

    for caller in &callers {
        let actual_ordinals = caller
            .accepted
            .iter()
            .map(|accepted| accepted.ordinal)
            .collect::<Vec<_>>();
        assert_eq!(
            actual_ordinals,
            (0..MESSAGES_PER_CALLER).collect::<Vec<_>>(),
            "caller {} changed its own submission order",
            caller.caller
        );
        assert!(
            caller
                .accepted
                .windows(2)
                .all(|pair| pair[0].seq < pair[1].seq),
            "caller {} lost FIFO durable acceptance order",
            caller.caller
        );
    }

    let snapshot = rig
        .daemon
        .messages_snapshot_for_test("admin", 0)
        .expect("body-free administrator snapshot");
    assert_eq!(
        snapshot.counts.visible_messages,
        (CALLER_COUNT * MESSAGES_PER_CALLER) as u64,
        "body-free snapshot must see every accepted message"
    );

    let global_caller_order = by_sequence
        .iter()
        .map(|(_, caller, _)| *caller)
        .collect::<Vec<_>>();
    let caller_evidence = callers
        .into_iter()
        .map(|caller| {
            json!({
                "caller": caller.caller,
                "acceptance_sequences": caller.accepted.iter().map(|accepted| accepted.seq).collect::<Vec<_>>(),
                "request_elapsed_us": caller.accepted.iter().map(|accepted| accepted.request_elapsed_us).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let evidence = json!({
        "sample": sample,
        "accepted_message_count": CALLER_COUNT * MESSAGES_PER_CALLER,
        "body_free_snapshot": {
            "visible_messages": snapshot.counts.visible_messages,
            "pending_entries": snapshot.counts.pending_entries,
            "rows_returned": snapshot.rows.len(),
        },
        "fifo": {
            "per_caller_order": "strictly increasing durable sequence",
            "global_caller_interleaving": global_caller_order,
            "max_consecutive_acceptances_from_one_caller": max_consecutive_callers(&global_caller_order),
        },
        "callers": caller_evidence,
    });
    assert!(
        !serde_json::to_string(&evidence)
            .expect("body-free evidence serializes")
            .contains(FIXTURE_BODY),
        "performance evidence must not retain a message body"
    );
    evidence
}

/// Retained only in scheduled and release evidence lanes. Run directly with
/// `cargo test -p cyclopsd --test concurrent_messaging_perf -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "scheduled and release concurrent mailbox acceptance measurement"]
async fn concurrent_mailbox_acceptance_retains_fifo_evidence() {
    assert!(
        tmux_available(),
        "concurrent mailbox performance evidence requires tmux on PATH"
    );

    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for sample in 0..SAMPLE_COUNT {
        let mut rig = Rig::new(
            &format!("concurrent-messaging-perf-{sample}"),
            CAT_MANIFEST,
            &composer_pane(),
            "receipt_block_ms = 1000\n",
        )
        .await;
        rig.wait_attached(1).await;

        let ready = Arc::new(Barrier::new(CALLER_COUNT + 1));
        let start = Arc::new(Barrier::new(CALLER_COUNT + 1));
        let daemon = &rig.daemon;
        let workload_started = thread::scope(|scope| {
            let mut callers = Vec::with_capacity(CALLER_COUNT);
            for caller in 0..CALLER_COUNT {
                let ready = Arc::clone(&ready);
                let start = Arc::clone(&start);
                callers.push(scope.spawn(move || run_caller(daemon, sample, caller, ready, start)));
            }
            ready.wait();
            let started = Instant::now();
            start.wait();
            let callers = callers
                .into_iter()
                .map(|caller| caller.join().expect("caller thread succeeds"))
                .collect::<Vec<_>>();
            (started.elapsed(), callers)
        });

        let mut evidence = body_free_sample_evidence(sample, workload_started.1, &rig);
        let workload_elapsed_us: u64 = workload_started
            .0
            .as_micros()
            .try_into()
            .expect("workload duration fits u64");
        evidence["workload_elapsed_us"] = json!(workload_elapsed_us);
        samples.push(evidence);
        rig.shutdown().await;
    }

    println!(
        "CYCLOPS_CONCURRENT_MESSAGING_JSON={}",
        json!({
            "schema": 1,
            "kind": "cyclops_concurrent_mailbox_acceptance",
            "benchmark_test_build_ref": cyclops_proto::BUILD_REF,
            "cyclopsd_version": env!("CARGO_PKG_VERSION"),
            "workload": {
                "callers": CALLER_COUNT,
                "messages_per_caller": MESSAGES_PER_CALLER,
                "samples": SAMPLE_COUNT,
                "acceptance": "in-process Daemon::msg_send with resolved administrator identity",
                "concurrency": "four scoped OS threads released by a two-phase barrier; one current-thread Tokio runtime per caller",
                "ordering": "each caller sends its next message only after its prior durable acceptance",
                "recipient": "administrator mailbox without an agent route",
                "sample_isolation": "each sample owns a fresh Rig and shuts it down before the next sample",
                "excludes": [
                    "socket authentication",
                    "agent route selection",
                    "notification scheduling",
                    "terminal injection",
                    "user journey timing",
                ],
                "timing": "raw per-request and per-workload samples plus p50, p95, and max summaries",
                "bounds": "descriptive evidence only; no universal latency or interleaving bound is asserted",
            },
            "latency": {
                "workload_elapsed_us": latency_distribution(samples.iter().map(|sample| {
                    sample["workload_elapsed_us"]
                        .as_u64()
                        .expect("raw workload timing")
                }).collect()),
                "request_elapsed_us": latency_distribution(samples.iter().flat_map(|sample| {
                    sample["callers"]
                        .as_array()
                        .expect("raw caller evidence")
                        .iter()
                        .flat_map(|caller| caller["request_elapsed_us"]
                            .as_array()
                            .expect("raw request timings")
                            .iter()
                            .map(|timing| timing.as_u64().expect("positive raw request timing")))
                }).collect()),
            },
            "samples": samples,
        })
    );
}
