use super::*;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Barrier;

use cyclops_proto::{
    ComposerHold, MessageId, MessagePresentation, NotificationAttemptId, NotificationState,
    RecipientKey, RecipientPresentation, SessionInstanceId, TmuxPaneId, WorkspaceId,
};
use cyclops_state::StateRoot;

use crate::mailbox::{
    MailboxDirectory, MailboxIdentity, MailboxSend, MailboxService, MessageDraft, MessageStore,
};

struct NotificationScratch(PathBuf);

impl Drop for NotificationScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn notification_fixture(
    tag: &str,
) -> (
    NotificationScratch,
    Arc<StdMutex<MessageStore>>,
    NotificationContext,
    Arc<DeliveryHandle>,
    RecipientKey,
) {
    notification_fixture_with_summary(tag, None)
}

fn notification_fixture_with_summary(
    tag: &str,
    summary: Option<&str>,
) -> (
    NotificationScratch,
    Arc<StdMutex<MessageStore>>,
    NotificationContext,
    Arc<DeliveryHandle>,
    RecipientKey,
) {
    let path = cyclops_proto::scratch::scratch_dir(&format!(
        "notification-adapter-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    let root = StateRoot::open_or_create(&path).unwrap();
    let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
    let session = SessionInstanceId::from_str("00000000-0000-4000-8000-000000000002").unwrap();
    let recipient = RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%1").unwrap());
    let admin = RecipientKey::admin(workspace);
    let message_id = MessageId::new(format!("m-{tag}")).unwrap();
    let attempt_id = NotificationAttemptId::generate();
    let mut store = MessageStore::open(
        &root,
        Path::new("workspaces/current/messages.ndjson"),
        workspace,
        "boot",
    )
    .unwrap();
    store
        .accept(
            message_id.clone(),
            MessageDraft {
                kind: Kind::Msg,
                sender: admin,
                recipients: vec![recipient],
                subject: Some("Wake".into()),
                summary: summary.map(str::to_string),
                body: Some("Review the mailbox".into()),
                client_key: None,
                supersedes: None,
                presentation: MessagePresentation {
                    sender_label: "admin".into(),
                    recipient_labels: vec![RecipientPresentation {
                        recipient,
                        label: "reviewer".into(),
                    }],
                },
            },
        )
        .unwrap();
    store
        .queue_notification(message_id.clone(), recipient, attempt_id)
        .unwrap();
    let store = Arc::new(StdMutex::new(store));
    let context = NotificationContext::new(
        Arc::clone(&store),
        message_id.clone(),
        recipient,
        attempt_id,
    );
    let doorbell = cyclops_proto::render_doorbell_v1(&message_id);
    let handle = DeliveryHandle::for_notification("reviewer", "%1", 0, doorbell, context.clone());
    (NotificationScratch(path), store, context, handle, recipient)
}

#[test]
fn expected_notification_payload_is_the_single_transport_renderer() {
    let (_scratch, _store, context, _handle, _recipient) = notification_fixture("expected-payload");
    let message = context.message_line().expect("message");
    let mut record = context.current_record().expect("notification");

    assert_eq!(
        expected_notification_payload(&record, &message),
        Some(cyclops_proto::render_legacy_doorbell(&record.message_id))
    );

    record.doorbell_format = Some(cyclops_proto::DOORBELL_FORMAT_COMPACT_CLAIM);
    assert_eq!(
        expected_notification_payload(&record, &message),
        Some(cyclops_proto::render_doorbell_v1(&record.message_id))
    );

    record.doorbell_format = Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_CLAIM);
    assert_eq!(
        expected_notification_payload(&record, &message),
        Some(cyclops_proto::render_doorbell_v2(
            &record.message_id,
            record.attempt_id
        ))
    );

    record.doorbell_format = Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM);
    assert_eq!(
        expected_notification_payload(&record, &message),
        Some(cyclops_proto::render_doorbell_v3(record.attempt_id))
    );

    record.transport = NotificationTransport::DirectPayload;
    assert_eq!(expected_notification_payload(&record, &message), None);

    record.doorbell_format = None;
    assert_eq!(
        expected_notification_payload(&record, &message),
        Some(render_canonical_message_payload(&message))
    );

    record.transport = NotificationTransport::Doorbell;
    record.doorbell_format = Some(u32::MAX);
    assert_eq!(expected_notification_payload(&record, &message), None);

    record.doorbell_format = None;
    record.message_id = MessageId::new("m-different").expect("message id");
    assert_eq!(expected_notification_payload(&record, &message), None);
}

fn notification_state(
    store: &Arc<StdMutex<MessageStore>>,
    recipient: RecipientKey,
    message_id: &MessageId,
) -> cyclops_proto::NotificationRecord {
    store
        .lock()
        .unwrap()
        .projection()
        .notification(recipient, message_id)
        .cloned()
        .unwrap()
}

fn prepare_claimed_staged(
    store: &Arc<StdMutex<MessageStore>>,
    context: &NotificationContext,
    recipient: RecipientKey,
) -> cyclops_proto::NotificationRecord {
    context.record_gating().unwrap();
    context
        .record_writing(
            ProcessInstanceId::new(3999, 817_999).unwrap(),
            ProcessInstanceId::new(4000, 818_000).unwrap(),
            ProcessInstanceId::new(4242, 818_221).unwrap(),
            "codex",
            NotificationTransport::Doorbell,
            Some(cyclops_proto::DOORBELL_FORMAT_COMPACT_CLAIM),
        )
        .unwrap();
    context.record_staged().unwrap();
    let claimed = store
        .lock()
        .unwrap()
        .claim(recipient, context.message_id().clone())
        .unwrap();
    assert!(matches!(
        claimed,
        crate::mailbox::ClaimOutcome::Claimed {
            consumed_doorbell_attempt: None,
            ..
        }
    ));
    notification_state(store, recipient, context.message_id())
}

fn churn_recipient(workspace: WorkspaceId, ordinal: u128) -> RecipientKey {
    let session = SessionInstanceId::from_uuid(uuid::Uuid::from_u128(ordinal + 1)).unwrap();
    RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%1").unwrap())
}

fn test_worker_task() -> JoinHandle<()> {
    tokio::spawn(std::future::pending())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_supervisor_restarts_without_another_enqueue() {
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recoveries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let restarted = Arc::new(Notify::new());
    let task = tokio::spawn(supervise_worker_task(
        {
            let starts = Arc::clone(&starts);
            let restarted = Arc::clone(&restarted);
            move || {
                let run = starts.fetch_add(1, Ordering::SeqCst);
                let restarted = Arc::clone(&restarted);
                tokio::spawn(async move {
                    if run == 0 {
                        panic!("simulated outer worker failure");
                    }
                    restarted.notify_one();
                    std::future::pending::<()>().await;
                })
            }
        },
        {
            let recoveries = Arc::clone(&recoveries);
            move || {
                recoveries.fetch_add(1, Ordering::SeqCst);
                true
            }
        },
        || false,
    ));

    tokio::time::timeout(Duration::from_secs(1), restarted.notified())
        .await
        .expect("supervisor starts a replacement child itself");
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(recoveries.load(Ordering::SeqCst), 1);
    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drain_waits_for_an_aborted_worker_child() {
    struct DropSignal(Arc<AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let engine = Arc::new(Engine::new());
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let spawn_engine = Arc::clone(&engine);
    let spawn_started = Arc::clone(&started);
    let spawn_dropped = Arc::clone(&dropped);
    let supervisor = tokio::spawn(supervise_worker_task(
        move || {
            let started = Arc::clone(&spawn_started);
            let marker = DropSignal(Arc::clone(&spawn_dropped));
            spawn_engine.spawn_descendant_task(async move {
                let _marker = marker;
                started.notify_one();
                std::future::pending::<()>().await;
            })
        },
        || false,
        || false,
    ));

    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("tracked worker child starts");
    supervisor.abort();
    let _ = supervisor.await;
    tokio::time::timeout(Duration::from_secs(1), engine.wait_for_descendant_tasks())
        .await
        .expect("shutdown drain observes the child cancellation");
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unexpected_clean_worker_exit_recovers_without_another_enqueue() {
    let starts = Arc::new(AtomicUsize::new(0));
    let recoveries = Arc::new(AtomicUsize::new(0));
    let restarted = Arc::new(Notify::new());
    let task = tokio::spawn(supervise_worker_task(
        {
            let starts = Arc::clone(&starts);
            let restarted = Arc::clone(&restarted);
            move || {
                let run = starts.fetch_add(1, Ordering::SeqCst);
                let restarted = Arc::clone(&restarted);
                tokio::spawn(async move {
                    if run == 0 {
                        return;
                    }
                    restarted.notify_one();
                    std::future::pending::<()>().await;
                })
            }
        },
        {
            let recoveries = Arc::clone(&recoveries);
            move || {
                recoveries.fetch_add(1, Ordering::SeqCst);
                true
            }
        },
        || false,
    ));

    tokio::time::timeout(Duration::from_secs(1), restarted.notified())
        .await
        .expect("a clean return with a published worker starts its replacement");
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(recoveries.load(Ordering::SeqCst), 1);
    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unexpected_worker_task_cancellation_recovers_without_new_traffic() {
    let starts = Arc::new(AtomicUsize::new(0));
    let recoveries = Arc::new(AtomicUsize::new(0));
    let restarted = Arc::new(Notify::new());
    let task = tokio::spawn(supervise_worker_task(
        {
            let starts = Arc::clone(&starts);
            let restarted = Arc::clone(&restarted);
            move || {
                let run = starts.fetch_add(1, Ordering::SeqCst);
                if run == 0 {
                    let child = tokio::spawn(std::future::pending::<()>());
                    child.abort();
                    return child;
                }
                let restarted = Arc::clone(&restarted);
                tokio::spawn(async move {
                    restarted.notify_one();
                    std::future::pending::<()>().await;
                })
            }
        },
        {
            let recoveries = Arc::clone(&recoveries);
            move || {
                recoveries.fetch_add(1, Ordering::SeqCst);
                true
            }
        },
        || false,
    ));

    tokio::time::timeout(Duration::from_secs(1), restarted.notified())
        .await
        .expect("an unexpected child cancellation starts its replacement");
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(recoveries.load(Ordering::SeqCst), 1);
    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_is_not_misclassified_as_a_worker_failure() {
    let engine = Arc::new(Engine::new());
    let started = Arc::new(Notify::new());
    let recoveries = Arc::new(AtomicUsize::new(0));
    let supervisor = tokio::spawn(supervise_worker_task(
        {
            let engine = Arc::clone(&engine);
            let started = Arc::clone(&started);
            move || {
                let started = Arc::clone(&started);
                engine.spawn_descendant_task(async move {
                    started.notify_one();
                    std::future::pending::<()>().await;
                })
            }
        },
        {
            let recoveries = Arc::clone(&recoveries);
            move || {
                recoveries.fetch_add(1, Ordering::SeqCst);
                true
            }
        },
        {
            let engine = Arc::clone(&engine);
            move || engine.is_stopping()
        },
    ));

    started.notified().await;
    engine.begin_stopping();
    tokio::time::timeout(Duration::from_secs(1), supervisor)
        .await
        .expect("shutdown releases the supervisor")
        .expect("supervisor exits cleanly");
    assert_eq!(recoveries.load(Ordering::SeqCst), 0);
    engine.wait_for_descendant_tasks().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_supervisor_wiring_distinguishes_retirement_from_child_loss() {
    // Normal retirement removes the exact registry entry before the child
    // returns. Drive the production supervisor and predicate together: a
    // wrong key or inverted current-worker check would recover this empty
    // worker instead of accepting its retirement.
    let retirement_root = NotificationScratch(cyclops_proto::scratch::scratch_dir(
        "notification-supervisor-retirement",
    ));
    std::fs::create_dir_all(&retirement_root.0).unwrap();
    let retirement_inner = unwritten_test_inner(&retirement_root.0);
    let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
    let retired_recipient = churn_recipient(workspace, 910);
    let retired_worker = Arc::new(Worker::new());
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let supervisor_inner = Arc::clone(&retirement_inner);
    let supervisor_worker = Arc::clone(&retired_worker);
    let task = tokio::spawn(async move {
        let _ = start_rx.await;
        notification_worker_supervisor(supervisor_inner, retired_recipient, supervisor_worker)
            .await;
        let _ = done_tx.send(());
    });
    retirement_inner
        .engine
        .notification_workers
        .lock()
        .expect("notification workers lock")
        .insert(
            retired_recipient,
            NotificationWorker {
                worker: Arc::clone(&retired_worker),
                task,
            },
        );
    start_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), done_rx)
        .await
        .expect("normal registry retirement releases the supervisor")
        .expect("supervisor completion sender stayed open");
    assert!(retirement_inner
        .engine
        .notification_workers
        .lock()
        .expect("notification workers lock")
        .is_empty());
    assert_eq!(
        retired_worker
            .state
            .lock()
            .expect("worker state lock")
            .empty_restarts,
        0,
        "normal retirement must not be recovered as worker loss"
    );

    // Cancellation of a descendant while the exact registry entry is
    // still published is unexpected. Keep a real queued notification
    // parked behind quiesce, cancel only its child, and require the
    // production supervisor to reconstruct that same durable attempt.
    let (message_scratch, store, context, _handle, recipient) =
        notification_fixture("notification-supervisor-child-loss");
    let inner_root = NotificationScratch(cyclops_proto::scratch::scratch_dir(
        "notification-supervisor-child-loss-inner",
    ));
    std::fs::create_dir_all(&inner_root.0).unwrap();
    let inner = unwritten_test_inner(&inner_root.0);
    inner.engine.paused.store(true, Ordering::SeqCst);
    let attempt = context.attempt_id();
    let handle = enqueue_notification_attempt(&inner, 0, "%1", "reviewer", context.clone(), false)
        .expect("production enqueue publishes the worker and supervisor");
    let worker = inner
        .engine
        .notification_workers
        .lock()
        .expect("notification workers lock")
        .get(&recipient)
        .map(|entry| Arc::clone(&entry.worker))
        .expect("recipient worker is registered");

    tokio::time::timeout(Duration::from_secs(1), async {
        while inner.engine.descendant_tasks.active.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the production supervisor spawned its child");
    // Hold the durable projection while the first child observes the stop.
    // Recovery increments its counter before reading that projection, so
    // this gives the test a deterministic point to lower the global stop
    // latch before the supervisor can spawn its replacement. Without this
    // ordering, a slow runner can let the replacement inherit `true`,
    // causing a second legitimate recovery and a BlockedPreWrite result.
    tokio::task::block_in_place(|| {
        let _projection = store.lock().expect("message store lock");
        inner.engine.descendant_stop.send_replace(true);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while handle.worker_recoveries.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the cancelled child reaches durable recovery"
            );
            std::thread::yield_now();
        }
        inner.engine.descendant_stop.send_replace(false);
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        while inner
            .engine
            .notification_handle(attempt)
            .is_none_or(|current| Arc::ptr_eq(&current, &handle))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unexpected child loss was classified without another enqueue");
    assert_eq!(handle.worker_recoveries.load(Ordering::SeqCst), 1);
    assert!(inner
        .engine
        .notification_worker_is_current(recipient, &worker));
    assert!(!worker.is_faulted());
    assert_eq!(context.current_record().unwrap().attempt_id, attempt);
    assert_eq!(
        context.current_record().unwrap().state,
        NotificationState::Queued
    );
    assert_eq!(
        inner
            .engine
            .notification_handle(attempt)
            .and_then(|current| current.notification.as_ref().map(|n| n.attempt_id())),
        Some(attempt),
        "recovery must keep the same durable attempt identity"
    );

    inner.engine.begin_stopping();
    for task in inner.engine.take_notification_worker_tasks() {
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown releases the production supervisor")
            .expect("production supervisor exits cleanly");
    }
    inner.engine.wait_for_descendant_tasks().await;
    drop(message_scratch);
}

#[tokio::test]
async fn stopping_cancels_a_tracked_descendant() {
    let engine = Engine::new();
    let started = Arc::new(Notify::new());
    let task_started = Arc::clone(&started);
    let task = engine.spawn_descendant_task(async move {
        task_started.notify_one();
        std::future::pending::<()>().await;
    });

    started.notified().await;
    engine.begin_stopping();

    tokio::time::timeout(Duration::from_secs(1), engine.wait_for_descendant_tasks())
        .await
        .expect("shutdown cancels tracked descendants");
    task.await.expect("tracked task exits without panic");
}

#[tokio::test]
async fn shutdown_latch_refuses_worker_creation_before_task_drain() {
    let engine = Engine::new();
    let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
    let recipient = churn_recipient(workspace, 900);
    let handle = DeliveryHandle::new("m-stopping", "worker", "%1", 0, "body".into());
    let spawned = Arc::new(AtomicBool::new(false));

    engine.begin_stopping();
    let result = engine.enqueue_notification_worker(recipient, handle, {
        let spawned = Arc::clone(&spawned);
        move |_| {
            spawned.store(true, Ordering::SeqCst);
            test_worker_task()
        }
    });

    assert_eq!(
        result.err(),
        Some(NotificationEnqueueRefusal::DaemonStopping)
    );
    assert!(!spawned.load(Ordering::SeqCst));
    assert!(engine.take_notification_worker_tasks().is_empty());
    assert!(engine.take_legacy_worker_tasks().is_empty());
}

#[tokio::test]
async fn status_worker_ownership_requires_a_live_current_or_queued_attempt() {
    let engine = Engine::new();
    let (_scratch, _store, context, handle, recipient) =
        notification_fixture("status-worker-owner");
    let attempt = context.attempt_id();
    let worker = engine
        .enqueue_notification_worker(recipient, Arc::clone(&handle), |_| test_worker_task())
        .expect("engine is running");

    assert!(engine.notification_worker_owns(recipient, attempt));
    let current = worker
        .current_or_next()
        .expect("queued handle becomes current");
    assert!(engine.notification_worker_owns(recipient, attempt));
    assert!(!engine.notification_worker_owns(recipient, NotificationAttemptId::generate()));
    worker.set_fault(CLAIMED_STAGED_SETTLEMENT_FAILED);
    assert!(
        !engine.notification_worker_owns(recipient, attempt),
        "a faulted worker cannot advertise automatic reconciliation"
    );
    assert_eq!(
        engine.notification_worker_refusal(recipient, attempt),
        Some(NotificationEnqueueRefusal::WorkerFaulted)
    );
    assert!(worker.finish(&current));
    assert!(!engine.notification_worker_owns(recipient, attempt));

    for task in engine.take_notification_worker_tasks() {
        task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_workers_retire_across_recipient_session_churn() {
    let engine = Engine::new();
    let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();

    for ordinal in 0..512 {
        let recipient = churn_recipient(workspace, ordinal);
        let handle = DeliveryHandle::new(
            &format!("m-churn-{ordinal}"),
            "worker",
            "%1",
            0,
            "payload".into(),
        );
        let worker = engine
            .enqueue_notification_worker(recipient, Arc::clone(&handle), |_| test_worker_task())
            .expect("engine is running");
        assert!(engine.notification_worker_is_current(recipient, &worker));
        let queued = worker.drain_pending();
        assert_eq!(queued.len(), 1);
        assert!(Arc::ptr_eq(&queued[0], &handle));
        assert!(engine.retire_notification_worker(recipient, &worker));
        assert!(!engine.notification_worker_is_current(recipient, &worker));
        assert!(
            engine
                .notification_workers
                .lock()
                .expect("notification workers lock")
                .is_empty(),
            "recipient churn retained an idle worker at iteration {ordinal}"
        );
    }

    assert!(engine.take_notification_worker_tasks().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_notification_supervisor_stays_faulted_without_losing_its_fifo() {
    let engine = Engine::new();
    let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
    let recipient = churn_recipient(workspace, 700);
    let first = DeliveryHandle::new("m-worker-first", "worker", "%1", 0, "first".into());
    let fail = Arc::new(Notify::new());
    let worker = engine
        .enqueue_notification_worker(recipient, Arc::clone(&first), {
            let fail = Arc::clone(&fail);
            move |_| {
                tokio::spawn(async move {
                    fail.notified().await;
                    panic!("simulated notification worker failure");
                })
            }
        })
        .expect("engine is running");
    fail.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let finished = engine
                .notification_workers
                .lock()
                .expect("notification workers lock")
                .get(&recipient)
                .expect("worker entry")
                .task
                .is_finished();
            if finished {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed worker task finishes");

    let second = DeliveryHandle::new("m-worker-second", "worker", "%1", 0, "second".into());
    let refusal = engine
        .enqueue_notification_worker(recipient, Arc::clone(&second), |_| {
            panic!("a failed supervisor must not restart on later traffic")
        })
        .err();
    assert_eq!(
        refusal,
        Some(NotificationEnqueueRefusal::WorkerSupervisorExited)
    );
    assert_eq!(
        engine.notification_worker_refusal(recipient, NotificationAttemptId::generate()),
        Some(NotificationEnqueueRefusal::WorkerSupervisorExited)
    );
    assert!(worker.current().is_none());
    assert!(
        engine
            .notification_workers
            .lock()
            .expect("notification workers lock")
            .get(&recipient)
            .expect("faulted worker entry")
            .task
            .is_finished(),
        "the next enqueue must not hide the finished supervisor"
    );
    assert_eq!(
        worker
            .state
            .lock()
            .expect("worker state lock")
            .fault
            .as_deref(),
        Some("notification worker supervisor exited")
    );
    assert_eq!(
        worker
            .state
            .lock()
            .expect("worker state lock")
            .queue
            .iter()
            .map(|handle| handle.msg_id.as_str())
            .collect::<Vec<_>>(),
        ["m-worker-first"]
    );

    let tasks = engine.take_notification_worker_tasks();
    assert_eq!(tasks.len(), 1);
    tasks[0].abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enqueue_and_idle_retirement_never_orphan_a_handle() {
    let engine = Arc::new(Engine::new());
    let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
    let runtime = tokio::runtime::Handle::current();

    for ordinal in 0..256 {
        let recipient = churn_recipient(workspace, ordinal);
        let seed = DeliveryHandle::new(
            &format!("m-seed-{ordinal}"),
            "worker",
            "%1",
            0,
            "seed".into(),
        );
        let old_worker = engine
            .enqueue_notification_worker(recipient, Arc::clone(&seed), |_| test_worker_task())
            .expect("engine is running");
        let queued = old_worker.drain_pending();
        assert_eq!(queued.len(), 1);
        assert!(Arc::ptr_eq(&queued[0], &seed));

        let next = DeliveryHandle::new(
            &format!("m-next-{ordinal}"),
            "worker",
            "%1",
            0,
            "next".into(),
        );
        let start = Arc::new(Barrier::new(3));
        let producer = tokio::task::spawn_blocking({
            let engine = Arc::clone(&engine);
            let start = Arc::clone(&start);
            let next = Arc::clone(&next);
            let runtime = runtime.clone();
            move || {
                start.wait();
                engine
                    .enqueue_notification_worker(recipient, next, move |_| {
                        runtime.spawn(std::future::pending::<()>())
                    })
                    .expect("engine is running")
            }
        });
        let retirement = tokio::task::spawn_blocking({
            let engine = Arc::clone(&engine);
            let start = Arc::clone(&start);
            let old_worker = Arc::clone(&old_worker);
            move || {
                start.wait();
                engine.retire_notification_worker(recipient, &old_worker)
            }
        });
        tokio::task::block_in_place(|| start.wait());

        let producer_worker = producer.await.unwrap();
        let retired = retirement.await.unwrap();
        let current_worker = {
            let entries = engine
                .notification_workers
                .lock()
                .expect("notification workers lock");
            Arc::clone(
                &entries
                    .get(&recipient)
                    .expect("producer leaves an active worker")
                    .worker,
            )
        };
        assert!(Arc::ptr_eq(&current_worker, &producer_worker));
        assert_eq!(
            current_worker
                .state
                .lock()
                .expect("worker state lock")
                .queue
                .iter()
                .filter(|queued| Arc::ptr_eq(queued, &next))
                .count(),
            1,
            "concurrent retirement orphaned or duplicated the enqueue"
        );
        if retired {
            assert!(!Arc::ptr_eq(&current_worker, &old_worker));
        } else {
            assert!(Arc::ptr_eq(&current_worker, &old_worker));
        }

        current_worker.drain_pending();
        assert!(engine.retire_notification_worker(recipient, &current_worker));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_enqueue_and_idle_retirement_never_orphan_a_handle() {
    let engine = Arc::new(Engine::new());

    for ordinal in 0..256 {
        let pane = PaneKey::new(ordinal, "%1");
        let seed = DeliveryHandle::new(
            &format!("m-legacy-seed-{ordinal}"),
            "worker",
            "%1",
            ordinal,
            "seed".into(),
        );
        let old_worker = engine
            .with_legacy_worker(
                pane.clone(),
                {
                    let engine = Arc::clone(&engine);
                    move |_| engine.spawn_descendant_task(std::future::pending())
                },
                |worker| {
                    worker.enqueue_back(Arc::clone(&seed));
                    Arc::clone(worker)
                },
            )
            .expect("engine is running");
        let queued = old_worker.drain_pending();
        assert_eq!(queued.len(), 1);
        assert!(Arc::ptr_eq(&queued[0], &seed));

        let next = DeliveryHandle::new(
            &format!("m-legacy-next-{ordinal}"),
            "worker",
            "%1",
            ordinal,
            "next".into(),
        );
        let start = Arc::new(Barrier::new(3));
        let producer = tokio::task::spawn_blocking({
            let engine = Arc::clone(&engine);
            let pane = pane.clone();
            let start = Arc::clone(&start);
            let next = Arc::clone(&next);
            move || {
                start.wait();
                let spawn_engine = Arc::clone(&engine);
                engine
                    .with_legacy_worker(
                        pane,
                        move |_| spawn_engine.spawn_descendant_task(std::future::pending()),
                        |worker| {
                            worker.enqueue_back(next);
                            Arc::clone(worker)
                        },
                    )
                    .expect("engine is running")
            }
        });
        let retirement = tokio::task::spawn_blocking({
            let engine = Arc::clone(&engine);
            let pane = pane.clone();
            let start = Arc::clone(&start);
            let old_worker = Arc::clone(&old_worker);
            move || {
                start.wait();
                engine.retire_legacy_worker(&pane, &old_worker)
            }
        });
        tokio::task::block_in_place(|| start.wait());

        let producer_worker = producer.await.unwrap();
        let retired = retirement.await.unwrap();
        let current_worker = {
            let entries = engine.workers.lock().expect("workers lock");
            Arc::clone(&entries.get(&pane).expect("producer leaves a worker").worker)
        };
        assert!(Arc::ptr_eq(&current_worker, &producer_worker));
        assert_eq!(
            current_worker
                .state
                .lock()
                .expect("worker state lock")
                .queue
                .iter()
                .filter(|queued| Arc::ptr_eq(queued, &next))
                .count(),
            1,
            "concurrent legacy retirement orphaned or duplicated the enqueue"
        );
        if retired {
            assert!(!Arc::ptr_eq(&current_worker, &old_worker));
        } else {
            assert!(Arc::ptr_eq(&current_worker, &old_worker));
        }

        current_worker.drain_pending();
        assert!(engine.retire_legacy_worker(&pane, &current_worker));
        assert!(!engine.legacy_worker_is_current(&pane, &current_worker));
    }

    engine.begin_stopping();
    engine.wait_for_descendant_tasks().await;
    assert!(engine.take_legacy_worker_tasks().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_worker_loop_retires_its_registry_entry_when_idle() {
    let path = cyclops_proto::scratch::scratch_dir(&format!(
        "legacy-worker-retirement-{}",
        uuid::Uuid::new_v4()
    ));
    let _scratch = NotificationScratch(path.clone());
    let inner = unwritten_test_inner(&path);
    let pane = PaneKey::new(0, "%1");
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let task_inner = Arc::clone(&inner);
    let task_pane = pane.clone();

    let worker = inner
        .engine
        .with_legacy_worker(
            pane,
            move |worker| {
                tokio::spawn(async move {
                    worker_supervisor(task_inner, task_pane, worker).await;
                    let _ = done_tx.send(());
                })
            },
            Arc::clone,
        )
        .expect("engine is running");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if inner
                .engine
                .workers
                .lock()
                .expect("workers lock")
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the idle loop removes its exact registry entry");
    tokio::time::timeout(Duration::from_secs(1), done_rx)
        .await
        .expect("registry retirement releases the supervisor")
        .expect("supervisor completion sender stayed open");
    assert_eq!(
        worker
            .state
            .lock()
            .expect("worker state lock")
            .empty_restarts,
        0,
        "normal retirement must not be recovered as worker loss"
    );

    inner.engine.begin_stopping();
    inner.engine.wait_for_descendant_tasks().await;
    assert!(inner.engine.take_legacy_worker_tasks().is_empty());
}

fn prepare_notification_receipt(context: &NotificationContext) {
    context.record_gating().unwrap();
    context
        .record_writing(
            ProcessInstanceId::new(3999, 817_999).unwrap(),
            ProcessInstanceId::new(4000, 818_000).unwrap(),
            ProcessInstanceId::new(4242, 818_221).unwrap(),
            "codex",
            NotificationTransport::Doorbell,
            None,
        )
        .unwrap();
    context.record_staged().unwrap();
    context.reserve_submit().unwrap();
    context.record_submitted().unwrap();
}

#[test]
fn notification_and_direct_handles_have_distinct_projection_owners() {
    let (_scratch, _store, _context, notification, _recipient) =
        notification_fixture("projection-owner");
    let direct = DeliveryHandle::new("m-direct-owner", "reviewer", "%1", 0, "payload".into());

    assert!(!notification.owns_session_delivery_state());
    assert!(direct.owns_session_delivery_state());
}

#[test]
fn screen_and_hook_receipts_keep_the_canonical_barrier_active() {
    for source in ["screen", "hook"] {
        let (_scratch, store, context, _handle, recipient) =
            notification_fixture(&format!("{source}-notified-barrier"));
        prepare_notification_receipt(&context);

        context.record_notified().unwrap();
        let active = store
            .lock()
            .unwrap()
            .projection()
            .active_notification_barriers();
        assert_eq!(active.len(), 1, "{source} receipt dropped the barrier");
        assert_eq!(active[0].recipient, recipient);
        assert_eq!(active[0].state, NotificationState::Notified);
    }
}

#[test]
fn an_unclaimed_reminder_can_only_select_the_content_free_exact_claim_doorbell() {
    let (_scratch, store, context, _handle, recipient) =
        notification_fixture("reminder-content-free");
    prepare_notification_receipt(&context);
    let notified = context.record_notified().unwrap();
    let reminder = {
        let mut store = store.lock().unwrap();
        store
            .retire_notification_barrier(
                notified.message_id.clone(),
                recipient,
                notified.attempt_id,
                cyclops_proto::NotificationBarrierRetirementCause::ComposerObservedClear,
                None,
            )
            .unwrap();
        store
            .queue_unclaimed_reminder(notified.attempt_id)
            .unwrap()
            .unwrap()
    };
    let reminder_context = NotificationContext::new(
        Arc::clone(&store),
        reminder.message_id.clone(),
        recipient,
        reminder.attempt_id,
    );
    let handle = DeliveryHandle::for_notification(
        "reviewer",
        "%1",
        0,
        "stale placeholder must be replaced".into(),
        reminder_context,
    );
    let manifest = Manifest::parse(
        "[agent]\nid = \"fix\"\ndisplay_name = \"Fix\"\nprocess_names = [\"cat\"]\n",
        Path::new("fix.toml"),
    )
    .unwrap();

    let selected = select_attempt_payload(&handle, &manifest, None, None).unwrap();
    assert_eq!(
        selected.bytes,
        cyclops_proto::render_doorbell_v3(reminder.attempt_id)
    );
    assert_eq!(selected.transport, Some(NotificationTransport::Doorbell));
    assert_eq!(
        selected.doorbell_format,
        Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM)
    );
    assert!(selected.capability.is_none());
    assert!(!selected.bytes.contains("Review the mailbox"));
}

#[test]
fn a_summary_notification_keeps_its_operator_preview_in_a_narrow_pane() {
    let summary = "Yahir needs three jazz songs tonight. Reply with concise recommendations.";
    let (_scratch, _store, context, handle, _recipient) =
        notification_fixture_with_summary("narrow-summary", Some(summary));
    let manifest = Manifest::parse(
        "[agent]\nid = \"fix\"\ndisplay_name = \"Fix\"\nprocess_names = [\"cat\"]\n",
        Path::new("fix.toml"),
    )
    .unwrap();

    let selected = select_attempt_payload(&handle, &manifest, None, Some(60)).unwrap();

    assert_eq!(
        selected.bytes,
        cyclops_proto::render_doorbell_v4("admin", summary, context.attempt_id())
    );
    assert_eq!(selected.transport, Some(NotificationTransport::Doorbell));
    assert_eq!(
        selected.doorbell_format,
        Some(cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM)
    );
    assert_eq!(
        selected.required_pane_width(),
        None,
        "format 4 may soft-wrap instead of dropping its human-readable summary"
    );
}

#[test]
fn mailbox_capability_proof_is_exact_and_binding_scoped() {
    let scratch = cyclops_proto::scratch::scratch_dir(&format!(
        "mailbox-capability-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&scratch).unwrap();
    let capability_file = scratch.join("SKILL.md");
    std::fs::write(&capability_file, mailbox_capability::SHIPPED_SKILL).unwrap();
    let manifest = Manifest::parse(
            &format!(
                "[agent]\nid = \"fix\"\ndisplay_name = \"Fix\"\n[messaging]\nmailbox_capability_file = {:?}\n",
                capability_file.to_string_lossy()
            ),
            Path::new("fix.toml"),
        )
        .unwrap();
    let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
    let session = SessionInstanceId::from_str("00000000-0000-4000-8000-000000000002").unwrap();
    let recipient = RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%1").unwrap());
    let agent = crate::identity::ProcId { pid: 41, birth: 90 };
    let proof = select_mailbox_capability(&manifest, recipient, agent, "fix")
        .expect("canonical skill proves capability");
    assert!(proof.recheck(recipient, agent, "fix"));
    assert!(!proof.recheck(
        recipient,
        crate::identity::ProcId { pid: 41, birth: 91 },
        "fix"
    ));
    assert!(!proof.recheck(recipient, agent, "replacement"));

    std::fs::write(&capability_file, b"older shipped skill").unwrap();
    assert!(select_mailbox_capability(&manifest, recipient, agent, "fix").is_none());
    std::fs::write(&capability_file, b"operator edited this skill").unwrap();
    assert!(!proof.recheck(recipient, agent, "fix"));
    std::fs::remove_file(&capability_file).unwrap();
    assert!(select_mailbox_capability(&manifest, recipient, agent, "fix").is_none());
    std::fs::create_dir(&capability_file).unwrap();
    assert!(select_mailbox_capability(&manifest, recipient, agent, "fix").is_none());
    std::fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn notification_prewrite_refusals_hold_without_a_legacy_terminal_state() {
    let (_scratch, store, context, notification, recipient) =
        notification_fixture("prewrite-policy");
    let direct = DeliveryHandle::new("m-direct-policy", "reviewer", "%1", 0, "payload".into());

    context.record_gating().unwrap();
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        cyclops_proto::NotificationState::Gating
    );

    for cause in ["no_such_pane", "pane_dead", "no_manifest", "blocked_quota"] {
        assert_eq!(
            workspace_prewrite_hold(&notification, cause).as_deref(),
            Some(cause)
        );
        assert_eq!(workspace_prewrite_hold(&direct, cause), None);
    }
    assert_eq!(gate_hold_action(&notification, "blocked_quota"), "wait");
    assert_eq!(gate_hold_action(&notification, "no_manifest"), "hold");
    assert_eq!(gate_hold_action(&direct, "blocked_quota"), "hold");

    assert!(workspace_prewrite_failure_is_deferred(
        &notification,
        &AttemptFailure::spool_failed()
    ));
    assert!(!workspace_prewrite_failure_is_deferred(
        &notification,
        &AttemptFailure::verify_failed()
    ));
    assert!(!workspace_prewrite_failure_is_deferred(
        &direct,
        &AttemptFailure::spool_failed()
    ));
}

#[test]
fn a_notification_bypasses_an_already_parked_legacy_worker() {
    let (_scratch, _store, _context, notification, _recipient) =
        notification_fixture("already-parked");
    let direct = DeliveryHandle::new("m-parked-direct", "reviewer", "%1", 0, "payload".into());
    let hint = Some("reset tomorrow".to_string());

    assert!(legacy_park_hint(&notification, hint.clone()).is_none());
    assert_eq!(
        legacy_park_hint(&direct, hint).as_deref(),
        Some("reset tomorrow")
    );
}

#[test]
fn a_direct_quota_park_preserves_workspace_notifications_behind_it() {
    let (_scratch, store, context, notification, recipient) =
        notification_fixture("queued-behind-park");
    let first = DeliveryHandle::new("m-parked-first", "reviewer", "%1", 0, "first".into());
    let last = DeliveryHandle::new("m-parked-last", "reviewer", "%1", 0, "last".into());

    let (direct, workspace) = split_legacy_parked_queue(vec![
        Arc::clone(&first),
        Arc::clone(&notification),
        Arc::clone(&last),
    ]);

    assert_eq!(
        direct
            .iter()
            .map(|handle| handle.msg_id.as_str())
            .collect::<Vec<_>>(),
        vec!["m-parked-first", "m-parked-last"]
    );
    assert_eq!(workspace.len(), 1);
    assert!(Arc::ptr_eq(&workspace[0], &notification));
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        cyclops_proto::NotificationState::Queued
    );
}

#[test]
fn restart_recovery_skips_workspace_messages_but_keeps_direct_deliveries() {
    let workspace_ids = HashSet::from(["m-workspace".to_string()]);

    assert!(!legacy_recovery_owns("m-workspace", &workspace_ids));
    assert!(legacy_recovery_owns("m-direct", &workspace_ids));
}

fn supersede_notification(
    store: &Arc<StdMutex<MessageStore>>,
    recipient: RecipientKey,
    message_id: &MessageId,
    replacement: &str,
) {
    store
        .lock()
        .unwrap()
        .accept(
            MessageId::new(replacement).unwrap(),
            MessageDraft {
                kind: Kind::Msg,
                sender: RecipientKey::admin(recipient.workspace_id()),
                recipients: vec![recipient],
                subject: Some("Replacement".into()),
                summary: None,
                body: None,
                client_key: None,
                supersedes: Some(message_id.clone()),
                presentation: MessagePresentation {
                    sender_label: "admin".into(),
                    recipient_labels: vec![RecipientPresentation {
                        recipient,
                        label: "reviewer".into(),
                    }],
                },
            },
        )
        .unwrap();
}

#[test]
fn retry_policy_only_retries_failures_proven_before_the_write() {
    let cases = [
        (AttemptFailure::session_detached(), "session_detached", true),
        (AttemptFailure::no_manifest(), "no_manifest", true),
        (
            AttemptFailure::pane_rebound_before_paste(),
            "pane_rebound",
            true,
        ),
        (AttemptFailure::spool_failed(), "spool_failed", true),
        (
            AttemptFailure::paste_command_unwritten(),
            "paste_command_unwritten",
            true,
        ),
        (
            AttemptFailure::composer_ownership_unproven(),
            "composer_ownership_unproven",
            false,
        ),
        (
            AttemptFailure::binding_unprovable(None),
            "binding_unprovable",
            false,
        ),
        (AttemptFailure::paste_failed(), "paste_failed", false),
        (AttemptFailure::verify_failed(), "verify_failed", false),
        (
            AttemptFailure::pane_rebound_after_paste(),
            "pane_rebound_after_paste",
            false,
        ),
        (AttemptFailure::submit_failed(), "submit_failed", false),
        (AttemptFailure::ack_timeout(), "ack_timeout", false),
        (
            AttemptFailure::notification_record_failed(),
            NOTIFICATION_RECORD_FAILED,
            false,
        ),
    ];
    for (failure, cause, retryable) in cases {
        assert_eq!(failure.cause, cause);
        assert_eq!(
            should_retry(&failure, 1, 1),
            retryable,
            "retry policy changed for {cause}"
        );
    }
    let exhausted = AttemptFailure::spool_failed();
    assert!(!should_retry(&exhausted, 2, 1));

    assert_eq!(
        AttemptFailure::verify_failed().verify_outcome,
        Some(NotificationVerifyOutcome::ambiguous())
    );
    assert_eq!(
        AttemptFailure::verify_timeout().verify_outcome,
        Some(NotificationVerifyOutcome {
            kind: NotificationVerifyFailureKind::Timeout,
            observed_composer: ComposerState::ComposerAmbiguous,
        })
    );
    assert_eq!(
        AttemptFailure::verify_mismatch(ComposerState::HumanDraft).verify_outcome,
        Some(NotificationVerifyOutcome {
            kind: NotificationVerifyFailureKind::Mismatch,
            observed_composer: ComposerState::HumanDraft,
        })
    );
    assert_eq!(
        AttemptFailure::verify_owner_missing(ComposerState::ComposerClean).verify_outcome,
        Some(NotificationVerifyOutcome {
            kind: NotificationVerifyFailureKind::OwnerMissing,
            observed_composer: ComposerState::ComposerClean,
        })
    );

    let pane_too_narrow = AttemptFailure::pane_too_narrow(NotificationPreWriteObservation {
        pane_root: Some(ProcessInstanceId::new(1, 1).unwrap()),
        selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
        binding: None,
        route_evidence: None,
        pane_width: Some(cyclops_proto::DOORBELL_V3_MIN_PANE_WIDTH - 1),
        required_pane_width: None,
        write_block: None,
    });
    assert!(!should_retry(&pane_too_narrow, 0, 3));
    assert_eq!(
        pane_too_narrow.pre_write_block.as_deref().unwrap().cause,
        NotificationPreWriteCause::WriteReadinessChanged
    );
    assert_eq!(
        pane_too_narrow
            .pre_write_block
            .as_deref()
            .and_then(|block| block.observation.as_ref())
            .and_then(|observation| observation.required_pane_width),
        Some(cyclops_proto::DOORBELL_V3_MIN_PANE_WIDTH)
    );

    // The production mapping keeps unknown injector errors conservative
    // too: they can never opt into the pre-write retry budget.
    let unknown = AttemptFailure::from_inject("future_failure".into());
    assert!(!should_retry(&unknown, 1, 1));
}

#[test]
fn capability_loss_refuses_but_a_narrow_doorbell_may_soft_wrap() {
    let scratch = NotificationScratch(cyclops_proto::scratch::scratch_dir(&format!(
        "capability-bookend-{}",
        uuid::Uuid::new_v4()
    )));
    std::fs::create_dir_all(&scratch.0).unwrap();
    let file = scratch.0.join("SKILL.md");
    std::fs::write(&file, mailbox_capability::SHIPPED_SKILL).unwrap();
    let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
    let session = SessionInstanceId::from_str("00000000-0000-4000-8000-000000000002").unwrap();
    let recipient = RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%1").unwrap());
    let agent = crate::identity::ProcId { pid: 42, birth: 7 };
    let selected = AttemptPayload {
        bytes: "doorbell".to_string(),
        transport: Some(NotificationTransport::Doorbell),
        doorbell_format: Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
        capability: Some(MailboxCapabilityProof {
            recipient,
            agent,
            manifest: "codex".to_string(),
            file: file.clone(),
            expected_digest: mailbox_capability::file_digest(&file).unwrap(),
        }),
        capability_required: true,
    };
    let binding = fusion::Binding {
        pane_root: crate::identity::ProcId { pid: 40, birth: 5 },
        leader: crate::identity::ProcId { pid: 41, birth: 6 },
        agent,
        manifest: "codex".to_string(),
    };
    let narrow = cyclops_proto::DOORBELL_V3_MIN_PANE_WIDTH - 1;

    assert_eq!(
        notification_prewrite_bookend(&selected, Some(recipient), &binding, narrow),
        None,
        "operator-visible doorbells may soft-wrap in a narrow pane"
    );
    assert_eq!(
        notification_prewrite_bookend(
            &selected,
            Some(recipient),
            &binding,
            cyclops_proto::DOORBELL_V3_MIN_PANE_WIDTH,
        ),
        None,
        "an exact current capability passes the prewrite bookend"
    );
    std::fs::write(&file, "operator edit").unwrap();
    assert_eq!(
        notification_prewrite_bookend(&selected, Some(recipient), &binding, narrow),
        Some("capability_changed".to_string())
    );
}

#[test]
fn exhausted_prewrite_failures_have_exact_recoverable_causes() {
    let cases = [
        (
            AttemptFailure::session_detached(),
            NotificationPreWriteCause::SessionUnavailable,
        ),
        (
            AttemptFailure::no_manifest(),
            NotificationPreWriteCause::ManifestUnavailable,
        ),
        (
            AttemptFailure::payload_unavailable(),
            NotificationPreWriteCause::PayloadUnavailable,
        ),
        (
            AttemptFailure::pane_rebound_before_paste(),
            NotificationPreWriteCause::WriteReadinessChanged,
        ),
        (
            AttemptFailure::spool_failed(),
            NotificationPreWriteCause::SpoolFailed,
        ),
        (
            AttemptFailure::paste_command_unwritten(),
            NotificationPreWriteCause::PasteCommandUnwritten,
        ),
        (
            AttemptFailure::composer_ownership_unproven(),
            NotificationPreWriteCause::ComposerOwnershipUnproven,
        ),
    ];

    for (failure, expected) in cases {
        assert!(!should_retry(&failure, 2, 1));
        let block = failure
            .pre_write_block
            .as_deref()
            .expect("an exhausted known pre-write failure must become recoverable");
        assert_eq!(block.cause, expected);
        assert!(block.observation.is_none());
    }

    for failure in [
        AttemptFailure::barrier_held(),
        AttemptFailure::from_inject("binding_changed".into()),
        AttemptFailure::from_inject("capability_changed".into()),
    ] {
        assert!(failure.regate_cause().is_some());
        assert!(failure.pre_write_block.is_none());
    }
}

#[test]
fn write_boundary_regates_are_bounded_per_evidence_edge() {
    let handle = DeliveryHandle::new("m-regate", "worker", "%1", 0, String::new());
    assert_eq!(
        regate_action(&handle, RegateCause::BarrierHeld),
        RegateAction::BlockPreWrite,
        "a barrier race never gets an automatic retry"
    );
    assert_eq!(
        regate_action(&handle, RegateCause::BindingChanged),
        RegateAction::ImmediateReproof
    );
    assert_eq!(
        regate_action(&handle, RegateCause::BindingChanged),
        RegateAction::BlockPreWrite,
        "unchanged binding evidence cannot spin"
    );
    assert_eq!(
        regate_action(&handle, RegateCause::CapabilityChanged),
        RegateAction::ImmediateReproof,
        "each distinct proof receives one bounded check"
    );
    assert_eq!(
        regate_action(&handle, RegateCause::CapabilityChanged),
        RegateAction::BlockPreWrite
    );

    let cumulative = handle.state.lock().unwrap().regates;
    assert_eq!(cumulative, 5);

    reset_immediate_regates(&handle);
    assert_eq!(
        regate_action(&handle, RegateCause::CapabilityChanged),
        RegateAction::ImmediateReproof,
        "a new pane edge opens one fresh proof"
    );
    assert_eq!(
        handle.state.lock().unwrap().regates,
        cumulative + 1,
        "an evidence edge never rewrites cumulative unwritten attempts"
    );
}

#[tokio::test]
async fn only_exact_pane_evidence_resets_the_regate_allowance() {
    let cancel = Notify::new();

    let (events, mut lagged) = broadcast::channel(1);
    for seq in 1..=2 {
        events
            .send(Event {
                event: "session".into(),
                data: json!({"name": format!("other-{seq}")}),
                seq: None,
            })
            .unwrap();
    }
    assert!(
        !wait_pane_change(&mut lagged, None, 0, "%1", &cancel).await,
        "lag is doubt, not exact evidence"
    );

    let mut exact = events.subscribe();
    events
        .send(Event {
            event: "readiness".into(),
            data: json!({"session_idx": 3, "pane_id": "%1"}),
            seq: None,
        })
        .unwrap();
    assert!(wait_pane_change(&mut exact, None, 3, "%1", &cancel).await);
}

#[tokio::test]
async fn closed_gate_channels_do_not_become_evidence_or_spin() {
    let (events, mut receiver) = broadcast::channel::<Event>(1);
    drop(events);
    let cancel = Notify::new();

    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            wait_pane_change(&mut receiver, None, 0, "%1", &cancel),
        )
        .await
        .is_err(),
        "a closed event channel must remain held until cancellation"
    );
}

/// Every transition the pipeline can perform must be legal in the
/// frozen state machine. If the proto table changes, this fails before
/// any integration test does.
#[test]
fn pipeline_transitions_are_legal() {
    for (from, to) in PIPELINE_TRANSITIONS {
        assert!(
            from.can_transition_to(*to),
            "pipeline performs illegal transition {from:?} -> {to:?}"
        );
    }
}

#[test]
fn payload_shape_matches_spec() {
    let p = render_payload(
        "m-3f9c2a",
        "codex",
        "Review the rate limiter",
        "please",
        false,
    );
    let lines: Vec<&str> = p.lines().collect();
    assert_eq!(
        lines[0],
        "[cyclops m-3f9c2a] FROM: codex  SUBJECT: Review the rate limiter"
    );
    assert_eq!(lines[1], "please");
    assert_eq!(
            lines[2],
            "Reply: cyclops send codex --subject \"...\" --summary \"First sentence. Second sentence.\""
        );
    assert!(
        !p.ends_with('\n'),
        "no trailing newline; submit is separate"
    );
}

#[test]
fn fyi_payload_has_no_reply_hint() {
    let p = render_payload("m-1", "codex", "heads up", "body", true);
    assert!(!p.contains("Reply:"));
}

/// Every payload ends with the terminal sentinel, whatever else the
/// envelope carries. The measured failure is that a long payload wraps
/// and pushes the leading id out of the verify region while the tail
/// stays visible, so verification needs a token at the end.
#[test]
fn payload_ends_with_the_terminal_sentinel() {
    for (fyi, from) in [(false, "codex"), (true, "codex"), (false, "admin")] {
        let p = render_payload("m-3f9c2a", from, "subject", "body", fyi);
        assert_eq!(
            p.lines().next_back(),
            Some("[cyclops:end m-3f9c2a]"),
            "fyi={fyi} from={from}"
        );
    }
}

/// A hook ACK verifies the bytes this delivery sent, or nothing.
///
/// Two bugs this pins. The first: the matcher took any prompt
/// CONTAINING a waiting delivery's id, so a later message quoting an
/// earlier one upgraded that earlier delivery on somebody else's
/// evidence. The second: matching the header and terminal sentinel
/// alone still left the body free, and the pre-submit race is
/// irreducible, so an edited body could be recorded as verified
/// against the immutable ledger message it no longer is.
#[test]
fn a_hook_ack_verifies_the_payload_or_nothing() {
    let a = render_payload("m-aaa", "codex", "ship it", "body", false);
    let b = render_payload(
        "m-bbb",
        "codex",
        "re: m-aaa",
        &format!("you said:\n{a}\nwhat now?"),
        false,
    );

    assert!(prompt_matches(&a, &a), "the delivery's own bytes verify");
    assert!(b.contains("m-aaa"), "the quoting case this exists for");
    assert!(!prompt_matches(&b, &a), "quoted text is not a claim");
    assert!(prompt_matches(&b, &b));

    // Intact header and sentinel, edited body: the framing is
    // unchanged and the content is not the message that was sent.
    let edited = a.replace("body", "body, plus a line nobody sent");
    assert!(edited.starts_with("[cyclops m-aaa]"));
    assert!(edited.ends_with(&sentinel_for("m-aaa")));
    assert!(!prompt_matches(&edited, &a), "framing is not content");

    // Content before or after the payload is content.
    assert!(!prompt_matches(&format!("note\n{a}"), &a));
    assert!(!prompt_matches(&format!("{a}\nnote"), &a));

    // Whitespace inside the body is content too.
    assert!(!prompt_matches(&a.replace("ship it", "ship  it"), &a));

    // The one allowance: the closing newline a composer submit may or
    // may not carry. One, on the hook side, and nothing else.
    assert!(prompt_matches(&format!("{a}\n"), &a));
    assert!(!prompt_matches(&format!("{a}\n\n"), &a));
    assert!(!prompt_matches(&format!("{a}  "), &a));
    assert!(!prompt_matches(&format!(" {a}"), &a));

    // Line endings are content until a probe says otherwise, and the
    // payload is never rewritten to make a match succeed: a sender
    // whose body deliberately carries CRLF must not be verified by
    // hook bytes that dropped it.
    let crlf = render_payload("m-ccc", "codex", "s", "one\r\ntwo", false);
    assert!(!prompt_matches(&crlf.replace("\r\n", "\n"), &crlf));
    assert!(!prompt_matches(&a.replace('\n', "\r\n"), &a));
}

#[test]
fn c3_duplicate_exact_dispatch_candidates_confirm_neither() {
    let agent = crate::identity::ProcId { pid: 71, birth: 3 };
    let candidate = |id: &str| {
        let handle = DeliveryHandle::new(id, "claude", "%1", 0, "same bytes".into());
        *handle.submitted_agent.lock().expect("submitted agent lock") = Some(agent);
        *handle
            .submitted_manifest
            .lock()
            .expect("submitted manifest lock") = Some("claude".into());
        {
            let mut state = handle.state.lock().expect("handle state lock");
            state.state = DeliveryState::Submitted;
            state.barrier = Some(format!("{id}-attempt"));
            state.early_ack = Some(PendingAck {
                edge_ms: 10,
                turn: None,
                evidence: PendingAckEvidence::DispatchPending,
            });
        }
        handle
    };
    let first = candidate("m-first");
    let second = candidate("m-second");

    match select_unkeyed_dispatch_candidate(
        vec![Arc::clone(&first), Arc::clone(&second)],
        agent,
        "claude",
        10,
    ) {
        UnkeyedDispatchSelection::Ambiguous(handles) => {
            assert_eq!(handles.len(), 2);
            assert!(handles.iter().any(|handle| Arc::ptr_eq(handle, &first)));
            assert!(handles.iter().any(|handle| Arc::ptr_eq(handle, &second)));
        }
        UnkeyedDispatchSelection::None | UnkeyedDispatchSelection::Unique(_, _) => {
            panic!("duplicate exact dispatch bytes selected one receipt owner")
        }
    }
}

/// The sentinel is deliberately not the reply hint: transport
/// verification must not depend on human-facing CLI copy.
#[test]
fn sentinel_is_independent_of_the_reply_hint() {
    let with_hint = render_payload("m-a", "codex", "s", "b", false);
    let without_hint = render_payload("m-a", "codex", "s", "b", true);
    assert!(with_hint.ends_with("[cyclops:end m-a]"));
    assert!(without_hint.ends_with("[cyclops:end m-a]"));
}

/// A legacy direct payload from the operator carries no reply line.
///
/// `admin` is a durable mailbox address but no pane can hold that label.
/// The mailbox claim output prints the validated `cyclops reply <id>`
/// form. This compatibility renderer preserves its older payload shape
/// and therefore omits one for admin.
#[test]
fn a_legacy_operator_payload_has_no_pane_addressed_reply_hint() {
    let p = render_payload("m-1", cyclops_proto::label::ADMIN, "ship it", "now", false);
    assert!(!p.contains("Reply:"), "{p}");
    assert_eq!(
        p, "[cyclops m-1] FROM: admin  SUBJECT: ship it\nnow\n[cyclops:end m-1]",
        "the header, the body, and the sentinel: no reply hint"
    );
    // An agent-to-agent message still gets one: those targets exist.
    let p = render_payload("m-2", "reviewer", "ship it", "now", false);
    assert!(p.contains("Reply: cyclops send reviewer"), "{p}");
}

#[test]
fn empty_body_payload_is_header_plus_hint() {
    let p = render_payload("m-1", "codex", "s", "", false);
    let lines: Vec<&str> = p.lines().collect();
    assert_eq!(lines.len(), 3, "header, hint, sentinel: no empty body line");
    assert_eq!(lines[0], "[cyclops m-1] FROM: codex  SUBJECT: s");
    assert_eq!(lines[2], "[cyclops:end m-1]");
}

#[test]
fn verify_patterns_substitute_split_and_default() {
    let m = Manifest::parse(
        r#"
[agent]
id = "x"
display_name = "x"
[injection]
verify_pattern = ["<message_id>", "Pasted text"]
"#,
        std::path::Path::new("x.toml"),
    )
    .unwrap();
    let (id, other) = verify_patterns(&m, "m-ab12");
    assert_eq!(id, vec!["m-ab12".to_string()]);
    assert_eq!(other, vec!["Pasted text".to_string()]);

    let empty = Manifest::parse(
        "[agent]\nid = \"y\"\ndisplay_name = \"y\"\n",
        std::path::Path::new("y.toml"),
    )
    .unwrap();
    let (id, other) = verify_patterns(&empty, "m-1");
    assert_eq!(id, vec!["m-1".to_string()]);
    assert!(other.is_empty());
}

/// A manifest carrying the measured composer layout: a box rule then a
/// status row, each described in plain and escaped form.
pub(super) fn sentinel_manifest() -> cyclops_manifest::Manifest {
    cyclops_manifest::Manifest::parse(
        r#"
[agent]
id = "s"
display_name = "s"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 950
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['^\x1b\[39m❯']

[injection]
composer_trailer_regex = ['^─+$', '^\s*Model \S+ · Ctx: \d+%$']
composer_trailer_regex_esc = ['^\x1b\[90m─', '^\x1b\[38;5;\d+mModel\b']
composer_trailer_required_prefix = 2
composer_prompt_regex = '^❯ (?P<content>.*)$'
composer_continuation_regex = '^(?P<content>.*)$'
"#,
        std::path::Path::new("s.toml"),
    )
    .unwrap()
}

/// The measured chrome block, escaped the way the vendor paints it.
pub(super) const CHROME: &str = "\u{1b}[90m────────\n\u{1b}[38;5;152mModel x · Ctx: 78%";

/// The failure this unit exists for: a long payload wraps, the leading
/// id scrolls out of the region, and only the tail is visible.
#[test]
fn sentinel_verifies_a_wrapped_payload_whose_id_left_the_region() {
    let m = sentinel_manifest();
    let screen = format!(
        "\u{1b}[39m❯ [cyclops m-3f9c2a] FROM: codex  SUBJECT: long\n\
             wrapped continuation line one\n\
             [cyclops:end m-3f9c2a]\n{CHROME}"
    );
    assert!(sentinel_verified(&m, &screen, "m-3f9c2a"));
}

/// Nothing may follow the token that proves nothing follows it.
///
/// The bug this pins: the sentinel row was compared after escape
/// stripping. A torn `ESC [` swallows the rest of the line and a
/// complete sequence is removed outright, so the sentinel plus any
/// trailing bytes reduced to the exact token and verified. The
/// measured row is unstyled, so the raw row itself has to be the
/// token, with only the terminal's trailing padding removed.
#[test]
fn nothing_may_follow_the_terminal_sentinel() {
    let m = sentinel_manifest();
    let want = "[cyclops:end m-3f9c2a]";
    for forged in [
        // Torn CSI: the forgiving normalizer eats the remainder.
        format!("{want}\u{1b}["),
        format!("{want}\u{1b}[38;5and a whole sentence nobody sent"),
        // Complete CSI, which normalizes away just as cleanly.
        format!("{want}\u{1b}[0m"),
        format!("{want}\u{1b}[2K"),
        format!("{want}\u{1b}[1;5H"),
        // Operating-system commands, both terminator forms.
        format!("{want}\u{1b}]8;;http://example.com\u{7}"),
        format!("{want}\u{1b}]0;title\u{1b}\\"),
        // A bare ESC, dropped silently by the forgiving version.
        format!("{want}\u{1b}"),
        // And plain content, which was always refused.
        format!("{want} plus a human sentence"),
        // Whitespace a person can type is content. Only the
        // terminal's ASCII padding is not.
        format!("{want}\t"),
        format!("{want}\u{a0}"),
        format!("{want}\u{2003}"),
        // Styling in front of the token is content on this row too:
        // the measured row is unstyled.
        format!("\u{1b}[39m{want}"),
    ] {
        let screen = format!("\u{1b}[39m❯ body\n{forged}\n{CHROME}");
        assert!(
            !sentinel_verified(&m, &screen, "m-3f9c2a"),
            "must fail closed on {forged:?}"
        );
    }

    // The measured shape still verifies: the row is exactly the token,
    // and the terminal's trailing padding is not content.
    let screen = format!("\u{1b}[39m❯ body\n{want}   \n{CHROME}");
    assert!(sentinel_verified(&m, &screen, "m-3f9c2a"));
}

/// A sentinel split by the terminal edge proves nothing about what else
/// the capture lost, so it refuses.
#[test]
fn truncated_or_wrapped_sentinel_fails_closed() {
    let m = sentinel_manifest();
    for tail in [
        "[cyclops:end m-3f9c",
        "[cyclops:end\nm-3f9c2a]",
        "cyclops:end m-3f9c2a]",
    ] {
        let screen = format!("\u{1b}[39m❯ body\n{tail}\n{CHROME}");
        assert!(
            !sentinel_verified(&m, &screen, "m-3f9c2a"),
            "must fail closed on {tail:?}"
        );
    }
}

/// Payload after the sentinel means the capture is not the whole story.
#[test]
fn payload_after_the_sentinel_fails_closed() {
    let m = sentinel_manifest();
    let screen = format!("\u{1b}[39m❯ body\n[cyclops:end m-1]\nstray text\n{CHROME}");
    assert!(!sentinel_verified(&m, &screen, "m-1"));
}

/// Two identical sentinels are an ambiguity about which row transport
/// owns, not a reason to prefer the lower one.
#[test]
fn a_duplicate_sentinel_fails_closed() {
    let m = sentinel_manifest();
    let screen = format!("\u{1b}[39m❯ body\n[cyclops:end m-1]\n[cyclops:end m-1]\n{CHROME}");
    assert!(!sentinel_verified(&m, &screen, "m-1"));
}

/// A blank row after the sentinel is composer content: the sentinel
/// was not the last thing on the row below. Filtering it away and
/// accepting the chrome underneath is how a payload gap disappears.
#[test]
fn a_blank_row_after_the_sentinel_fails_closed() {
    let m = sentinel_manifest();
    for gap in ["\n", "\n\n"] {
        let screen = format!("\u{1b}[39m❯ body\n[cyclops:end m-1]{gap}\n{CHROME}");
        assert!(
            !sentinel_verified(&m, &screen, "m-1"),
            "blank payload row must refuse: {gap:?}"
        );
    }
}

/// Leading bytes belong to the composer, so the row is not the exact
/// transport token however familiar it looks.
#[test]
fn leading_bytes_before_the_sentinel_fail_closed() {
    let m = sentinel_manifest();
    for lead in [" ", "\t", "x "] {
        let screen = format!("\u{1b}[39m❯ body\n{lead}[cyclops:end m-1]\n{CHROME}");
        assert!(
            !sentinel_verified(&m, &screen, "m-1"),
            "leading {lead:?} must refuse"
        );
    }
}

/// A capture that ends at the sentinel never saw the composer's chrome,
/// so it never saw the composer. Vacuous truth is not evidence.
#[test]
fn a_sentinel_with_nothing_after_it_fails_closed() {
    let m = sentinel_manifest();
    assert!(!sentinel_verified(
        &m,
        "\u{1b}[39m❯ body\n[cyclops:end m-1]",
        "m-1"
    ));
}

/// A sentinel that scrolled into the transcript has the composer
/// between it and the chrome, and a composer row claims no layout
/// entry. Both an empty composer and one holding other text refuse.
#[test]
fn a_transcript_echo_of_the_sentinel_never_verifies() {
    let m = sentinel_manifest();
    for composer in ["\u{1b}[39m❯ ", "\u{1b}[39m❯ something else"] {
        let screen = format!("[cyclops:end m-1]\n{composer}\n{CHROME}");
        assert!(!sentinel_verified(&m, &screen, "m-1"), "{composer:?}");
    }
}

/// Chrome-shaped prose inserted before the real chrome must not be
/// walked past: it is unstyled, so it claims no layout entry.
#[test]
fn chrome_shaped_payload_before_the_chrome_fails_closed() {
    let m = sentinel_manifest();
    for line in ["Model y · Ctx: 50%", "────────"] {
        let screen = format!("\u{1b}[39m❯ body\n[cyclops:end m-1]\n{line}\n{CHROME}");
        assert!(
            !sentinel_verified(&m, &screen, "m-1"),
            "unstyled {line:?} must not pass as chrome"
        );
    }
}

/// Order is part of the layout: the status row cannot precede the rule.
#[test]
fn chrome_out_of_measured_order_fails_closed() {
    let m = sentinel_manifest();
    let screen = "\u{1b}[39m❯ body\n[cyclops:end m-1]\n\u{1b}[38;5;152mModel x · Ctx: 78%\n\u{1b}[90m────────";
    assert!(!sentinel_verified(&m, screen, "m-1"));
}

/// Without an escaped capture the styling evidence is absent, so the
/// answer is refuse rather than guess.
#[test]
fn a_plain_capture_never_verifies_the_sentinel() {
    let m = sentinel_manifest();
    assert!(!sentinel_verified(
        &m,
        "❯ body\n[cyclops:end m-1]\n────────\nModel x · Ctx: 78%",
        "m-1"
    ));
}

/// An unmeasured vendor cannot answer the terminality question at all.
#[test]
fn an_undeclared_vendor_never_verifies_by_sentinel() {
    let bare = cyclops_manifest::Manifest::parse(
        "[agent]\nid = \"x\"\ndisplay_name = \"x\"\n",
        std::path::Path::new("x.toml"),
    )
    .unwrap();
    let screen = format!("\u{1b}[39m❯ body\n[cyclops:end m-1]\n{CHROME}");
    assert!(!sentinel_verified(&bare, &screen, "m-1"));
}

/// A visible leading id is not evidence: every one of these renders the
/// header while the tail is missing or malformed, which is what a
/// truncated paste looks like, and none may verify.
#[test]
fn a_visible_leading_id_never_verifies_without_a_sound_sentinel() {
    let m = sentinel_manifest();
    let (id, other) = verify_patterns(&m, "m-3f9c2a");
    let head = "\u{1b}[39m❯ [cyclops m-3f9c2a] FROM: codex  SUBJECT: long";
    for (name, screen) in [
        ("missing sentinel", format!("{head}\nbody\n{CHROME}")),
        (
            "truncated sentinel",
            format!("{head}\nbody\n[cyclops:end m-3f9\n{CHROME}"),
        ),
        (
            "payload after sentinel",
            format!("{head}\nbody\n[cyclops:end m-3f9c2a]\nstray\n{CHROME}"),
        ),
        (
            "no chrome at all",
            format!("{head}\nbody\n[cyclops:end m-3f9c2a]"),
        ),
    ] {
        assert_eq!(
            sentinel_representation(&m, &screen, &id, &other, "m-3f9c2a"),
            None,
            "{name} must not verify on the leading id"
        );
    }
    let ok = format!("{head}\nbody\n[cyclops:end m-3f9c2a]\n{CHROME}");
    assert_eq!(
        sentinel_representation(&m, &ok, &id, &other, "m-3f9c2a"),
        Some(StagedRepresentation::VisibleTarget)
    );
}

/// The chip proof is manifest data plus a composer pin, and it needs
/// both halves: the row must render as the vendor's chip AND sit on a
/// row a composer rule recognizes. A manifest that declares no chip
/// syntax has no chip lane at all.
#[test]
fn marker_in_composer_is_manifest_driven() {
    let m = cyclops_manifest::Manifest::parse(
        r#"
[agent]
id = "c"
display_name = "c"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 950
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['\x1b\[39m❯\x{a0}']

[injection]
composer_chip_regex = ['^\s*❯\s+\[Pasted text #\d+\]\s*$']
composer_chip_regex_esc = ['\x1b\[39m❯\x{a0}\[Pasted text #\d+\]']
composer_trailer_regex = ['^\? for shortcuts\s*$']
composer_trailer_regex_esc = ['\? for shortcuts']
composer_trailer_required_prefix = 1
"#,
        std::path::Path::new("c.toml"),
    )
    .unwrap();
    // Staged and unsubmitted: the composer row IS the chip.
    let staged = "transcript\n\u{1b}[39m❯\u{a0}[Pasted text #1]\n? for shortcuts";
    assert!(marker_in_composer(&m, staged));
    // Submitted: composer cleared, the chip only in the transcript.
    let submitted = "old [Pasted text #1]\n\u{1b}[39m❯\u{a0}\n? for shortcuts";
    assert!(!marker_in_composer(&m, submitted));
    // A manifest with no chip syntax can never pin one.
    let bare = cyclops_manifest::Manifest::parse(
        "[agent]\nid = \"x\"\ndisplay_name = \"x\"\n",
        std::path::Path::new("x.toml"),
    )
    .unwrap();
    assert!(!marker_in_composer(&bare, staged));
}

#[test]
fn bottom_window_takes_non_empty_tail() {
    let screen = "a\n\nb\nc\n   \nd\n";
    assert_eq!(bottom_window(screen, 2), "c\nd");
    assert_eq!(bottom_window(screen, 10), "a\nb\nc\nd");
}

#[test]
fn reset_hint_parses_and_stays_short() {
    let screen = "junk\n⚠ Individual quota reached. Resets in 135h57m42s.\nmore";
    assert_eq!(
        parse_reset_hint(screen).as_deref(),
        Some("resets in 135h57m42s")
    );
    assert_eq!(parse_reset_hint("no hint here"), None);
}

#[test]
fn mint_ids_are_unique_and_shaped() {
    let e = Engine::new();
    let a = e.mint_msg_id();
    let b = e.mint_msg_id();
    assert_ne!(a, b);
    assert!(a.starts_with("m-") && a.len() == 8, "{a}");
}

// -----------------------------------------------------------------
// Post-paste verification ignores stale transcript text.
// -----------------------------------------------------------------

const COMPOSER_MANIFEST: &str = r#"
[agent]
id = "c"
display_name = "c"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 1050
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['\x1b\[39m❯\x{a0}']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
composer_chip_regex = ['^\s*❯\s+\[Pasted text #\d+( \+\d+ lines)?\]\s*$']
composer_chip_regex_esc = ['\x1b\[39m❯\x{a0}\[Pasted text #\d+( \+\d+ lines)?\]']
composer_trailer_regex = ['^\? for shortcuts\s*$']
composer_trailer_regex_esc = ['\? for shortcuts']
composer_trailer_required_prefix = 1
"#;

fn composer_manifest() -> Manifest {
    Manifest::parse(COMPOSER_MANIFEST, std::path::Path::new("c.toml")).unwrap()
}

#[test]
fn stale_generic_pattern_does_not_verify() {
    let m = composer_manifest();
    let (id, other) = verify_patterns(&m, "m-new01");
    // "Pasted text" from a PREVIOUS message sits in the transcript;
    // the composer is empty. Nothing staged.
    let screen =
        "you: [Pasted text #1 +9 lines]\nassistant: done\n\u{1b}[39m❯\u{a0}\n? for shortcuts";
    assert_eq!(
        sentinel_representation(&m, screen, &id, &other, "m-new01"),
        None
    );
    // The same chip on the composer line is a recognized representation.
    let staged = "transcript\n\u{1b}[39m❯\u{a0}[Pasted text #2 +9 lines]\n? for shortcuts";
    assert_eq!(
        sentinel_representation(&m, staged, &id, &other, "m-new01"),
        Some(StagedRepresentation::CollapsedChip)
    );
    // A visible id proves the head arrived and nothing more, which is
    // also what a truncated paste looks like, so it does not verify.
    let id_anywhere = "transcript\n❯ [cyclops m-new01] hello\n? for shortcuts";
    assert_eq!(
        sentinel_representation(&m, id_anywhere, &id, &other, "m-new01"),
        None
    );
}

/// The whole inject path with a mock backend: stale transcript chips
/// fail every verification read while a chip on the composer line passes.
pub(super) struct MockInjector {
    screens: StdMutex<Vec<String>>,
    cursor: std::sync::atomic::AtomicUsize,
    pub(super) pasted: StdMutex<Vec<String>>,
    submitted: StdMutex<Vec<(String, String)>>,
    spooled: StdMutex<Option<String>>,
}

struct UnwrittenCommitInjector;

impl Injector for UnwrittenCommitInjector {
    async fn spool(&self, _payload: &str) -> Result<(), String> {
        Ok(())
    }

    async fn commit(
        &self,
        _pane_id: &str,
        on_write: &(dyn Fn() -> Result<(), String> + Sync),
    ) -> Result<(), InjectFailure> {
        on_write().map_err(InjectFailure::Other)?;
        Err(InjectFailure::PasteCommandUnwritten)
    }

    async fn discard(&self) {}

    async fn submit(&self, _pane_id: &str, _key: &str) -> Result<(), String> {
        panic!("an unwritten paste must not submit")
    }

    async fn capture_joined_escaped(&self, _pane_id: &str) -> Result<String, String> {
        panic!("an unwritten paste must not verify")
    }
}

fn unwritten_test_inner(path: &Path) -> Arc<Inner> {
    let state_root = Arc::new(StateRoot::open_or_create(path).unwrap());
    let (registry, _) = crate::registry::Registry::load(Arc::clone(&state_root));
    let workspace_id = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
    let session_identities = crate::sessionstore::SessionIdentities::open(&state_root).unwrap();
    Arc::new(Inner {
        cfg: crate::Config::defaults(path),
        force_submit: crate::ForceSubmitRuntime::new(false, 5_000),
        state_root,
        durable_record_forget_lease: StdMutex::new(None),
        state_repair: cyclops_state::RepairSummary::default(),
        workspace_id,
        session_identities: StdMutex::new(session_identities),
        mailbox: None,
        workspace_messaging: std::sync::OnceLock::new(),
        composer_recovery: Arc::new(StdMutex::new(
            crate::composer_recovery::RecoveryCoordinator::default(),
        )),
        mailbox_publication: Arc::new(StdMutex::new(())),
        unread_projection_gate: tokio::sync::Mutex::new(()),
        unread_projection_pending: StdMutex::new(HashSet::new()),
        unread_projection_wake: tokio::sync::Notify::new(),
        unread_projection_stopping: std::sync::atomic::AtomicBool::new(false),
        unread_projection_pause: StdMutex::new(None),
        chrome_repaint_pause: StdMutex::new(None),
        mailbox_publish_pause: StdMutex::new(None),
        boot_id: "b-unwritten-test".into(),
        started: std::time::Instant::now(),
        tmux_version: "test".into(),
        manifests: std::collections::BTreeMap::new(),
        manifest_dir: None,
        sessions: StdMutex::new(Vec::new()),
        session_registration: StdMutex::new(()),
        events: broadcast::channel(16).0,
        detections: StdMutex::new(HashMap::new()),
        route_evidence_generations: StdMutex::new(HashMap::new()),
        pane_observation_runtime: crate::fusion::PaneObservationRuntime::new(),
        registry: StdMutex::new(registry),
        theme: StdMutex::new(cyclops_theme::ThemeWatch::new(path)),
        hook_readings: StdMutex::new(HashMap::new()),
        hook_lifecycle: StdMutex::new(crate::hook_lifecycle::Store::new()),
        turn_ends: StdMutex::new(crate::turnkey::Ends::new()),
        argv_cache: StdMutex::new(HashMap::new()),
        engine: Engine::new(),
        ack_state: crate::ack::AckState::new(),
        hook_liveness: crate::selftest::HookLiveness::new(),
        inject_pause: StdMutex::new(None),
        name_reconcile_pause: StdMutex::new(None),
        fail_chrome_restore: AtomicBool::new(false),
        fail_next_final_binding_observation: AtomicBool::new(false),
        fail_next_admitted_binding_observation: AtomicBool::new(false),
        fail_pre_record_writing: StdMutex::new(None),
        workspace_ui: StdMutex::new(crate::workspace_ui::WorkspaceUiState::default()),
        shutdown_request: watch::channel(false).0,
        stop: watch::channel(false).1,
        extra_tasks: StdMutex::new(Vec::new()),
    })
}

fn unwritten_test_binding() -> fusion::Binding {
    fusion::Binding {
        pane_root: crate::identity::ProcId {
            pid: 3999,
            birth: 817_999,
        },
        leader: crate::identity::ProcId {
            pid: 4000,
            birth: 818_000,
        },
        agent: crate::identity::ProcId {
            pid: 4242,
            birth: 818_221,
        },
        manifest: "codex".into(),
    }
}

fn seed_unwritten_test_composer(inner: &Arc<Inner>, binding: &fusion::Binding) {
    inner.detections.lock().unwrap().insert(
        PaneKey::new(0, "%1"),
        crate::DetEntry {
            detection: Detection {
                state: AgentState::Idle,
                readings: Vec::new(),
                disagreement: false,
                decided_by: "test".into(),
                unknown_reason: None,
                stale: false,
                write_ready: true,
                write_block: None,
                composer_semantic: Some(ComposerSemantic::Clean),
            },
            binding: Some(binding.clone()),
            manifest: Some(binding.manifest.clone()),
            occupant: Some(binding.leader.pid),
            agent: Some(binding.agent),
            in_mode: false,
            quota_screen_clear: false,
            hold: ComposerHold::Clear,
            turn: None,
            hold_owner: None,
            final_submit_conflict_owner: None,
            composer: crate::ComposerProjection::default(),
            working_confirmed: false,
            since: std::time::Instant::now(),
        },
    );
}

async fn run_unwritten_attempt_arm(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    binding: &fusion::Binding,
) -> AttemptOutcome {
    let payload = handle.payload();
    let manifest = sentinel_manifest();
    let failure = inject(
        &UnwrittenCommitInjector,
        handle,
        &manifest,
        StagingTarget::ExactRow(&payload),
        &payload,
        &|| {
            latch_hold(inner, handle, binding)?;
            let mut unwritten_hold = UnwrittenHold::new(inner, handle, binding);
            if let Some(notification) = &handle.notification {
                notification
                    .record_writing(
                        ProcessInstanceId::new(binding.pane_root.pid, binding.pane_root.birth)
                            .unwrap(),
                        ProcessInstanceId::new(binding.leader.pid, binding.leader.birth).unwrap(),
                        ProcessInstanceId::new(binding.agent.pid, binding.agent.birth).unwrap(),
                        &binding.manifest,
                        NotificationTransport::Doorbell,
                        None,
                    )
                    .map_err(notification_write_cause)?;
            }
            handle.write_boundary_crossed.store(true, Ordering::SeqCst);
            unwritten_hold.commit();
            Ok(())
        },
    )
    .await
    .expect_err("test injector proves the paste command was unwritten");
    finish_attempt_delivery_inject_failure(inner, handle, binding, None, failure)
}

impl MockInjector {
    pub(super) fn new(screens: Vec<&str>) -> MockInjector {
        MockInjector {
            screens: StdMutex::new(screens.into_iter().map(String::from).collect()),
            cursor: std::sync::atomic::AtomicUsize::new(0),
            pasted: StdMutex::new(Vec::new()),
            submitted: StdMutex::new(Vec::new()),
            spooled: StdMutex::new(None),
        }
    }

    fn next_screen(&self) -> String {
        let screens = self.screens.lock().unwrap();
        let i = self.cursor.fetch_add(1, Ordering::Relaxed);
        screens[i.min(screens.len() - 1)].clone()
    }

    pub(super) fn submitted_is_empty(&self) -> bool {
        self.submitted.lock().unwrap().is_empty()
    }
}

impl Injector for MockInjector {
    async fn spool(&self, payload: &str) -> Result<(), String> {
        *self.spooled.lock().unwrap() = Some(payload.to_string());
        Ok(())
    }

    async fn discard(&self) {
        *self.spooled.lock().unwrap() = None;
    }

    async fn commit(
        &self,
        _pane_id: &str,
        on_write: &(dyn Fn() -> Result<(), String> + Sync),
    ) -> Result<(), InjectFailure> {
        on_write().map_err(InjectFailure::Other)?;
        let payload = self
            .spooled
            .lock()
            .unwrap()
            .clone()
            .expect("commit without a spooled payload");
        self.pasted.lock().unwrap().push(payload);
        Ok(())
    }
    async fn submit(&self, pane_id: &str, key: &str) -> Result<(), String> {
        self.submitted
            .lock()
            .unwrap()
            .push((pane_id.to_string(), key.to_string()));
        Ok(())
    }
    async fn capture_joined_escaped(&self, _pane_id: &str) -> Result<String, String> {
        Ok(self.next_screen())
    }
}

#[tokio::test]
async fn notification_facts_follow_real_inject_submit_and_receipt_boundaries() {
    let (_scratch, store, context, handle, recipient) = notification_fixture("boundaries");
    context.record_gating().unwrap();
    let payload = handle.payload();
    let manifest = sentinel_manifest();
    let screen = format!("\u{1b}[39m❯ {payload}\n{CHROME}");
    let injector = MockInjector::new(vec![&screen]);
    injector.spool(&payload).await.unwrap();
    inject(
        &injector,
        &handle,
        &manifest,
        StagingTarget::ExactRow(&payload),
        &payload,
        &|| {
            assert!(injector.pasted.lock().unwrap().is_empty());
            context
                .record_writing(
                    ProcessInstanceId::new(3999, 817_999).unwrap(),
                    ProcessInstanceId::new(4000, 818_000).unwrap(),
                    ProcessInstanceId::new(4242, 818_221).unwrap(),
                    "codex",
                    NotificationTransport::Doorbell,
                    None,
                )
                .map(|_| ())
                .map_err(notification_write_cause)
        },
    )
    .await
    .unwrap();
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        cyclops_proto::NotificationState::Writing
    );

    context.record_staged().unwrap();
    injector.submit("%1", "Enter").await.unwrap();
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        cyclops_proto::NotificationState::Staged,
        "send-keys success is not a receipt"
    );
    context.reserve_submit().unwrap();
    context.record_submitted().unwrap();
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        cyclops_proto::NotificationState::Submitted
    );
    context.record_notified().unwrap();
    let notified = notification_state(&store, recipient, context.message_id());
    assert_eq!(notified.state, cyclops_proto::NotificationState::Notified);
    assert_eq!(notified.binding.unwrap().manifest.as_str(), "codex");
}

#[test]
fn a_proven_unwritten_paste_restores_a_withdrawable_notification_state() {
    let (scratch, store, context, handle, recipient) =
        notification_fixture("paste-command-unwritten");
    context.record_gating().unwrap();
    context
        .record_writing(
            ProcessInstanceId::new(3999, 817_999).unwrap(),
            ProcessInstanceId::new(4000, 818_000).unwrap(),
            ProcessInstanceId::new(4242, 818_221).unwrap(),
            "codex",
            NotificationTransport::Doorbell,
            Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
        )
        .unwrap();

    assert_eq!(
        store
            .lock()
            .unwrap()
            .projection()
            .active_notification_barriers()
            .len(),
        1
    );

    let blocked = context.record_paste_command_unwritten().unwrap();

    assert_eq!(blocked.state, NotificationState::BlockedPreWrite);
    assert_eq!(
        blocked.pre_write_cause,
        Some(NotificationPreWriteCause::PasteCommandUnwritten)
    );
    assert!(blocked.state.can_withdraw_before_write());
    assert_eq!(blocked.binding, None);
    assert_eq!(blocked.doorbell_format, None);
    assert!(store
        .lock()
        .unwrap()
        .projection()
        .active_notification_barriers()
        .is_empty());
    assert_eq!(
        notification_state(&store, recipient, context.message_id()),
        blocked
    );
    let failure = AttemptFailure::paste_command_unwritten();
    assert_eq!(failure.boundary, WriteBoundary::BeforeWrite);
    assert!(!should_retry_attempt(&handle, &failure, 0, 3));
    let direct = DeliveryHandle::new(
        "m-direct-unwritten",
        "reviewer",
        "%1",
        0,
        "payload".to_string(),
    );
    assert!(should_retry_attempt(&direct, &failure, 0, 3));

    let message_id = context.message_id().clone();
    drop(handle);
    drop(context);
    drop(store);
    let root = StateRoot::open_or_create(&scratch.0).unwrap();
    let mut replayed = MessageStore::open(
        &root,
        Path::new("workspaces/current/messages.ndjson"),
        recipient.workspace_id(),
        "replay",
    )
    .unwrap();
    let replayed_record = replayed
        .projection()
        .notification(recipient, &message_id)
        .unwrap();
    assert_eq!(replayed_record.state, NotificationState::BlockedPreWrite);
    assert_eq!(
        replayed_record.pre_write_cause,
        Some(NotificationPreWriteCause::PasteCommandUnwritten)
    );
    assert!(replayed
        .projection()
        .active_notification_barriers()
        .is_empty());
    let withdrawn = replayed
        .withdraw_notification_before_write(
            RecipientKey::admin(recipient.workspace_id()),
            recipient,
            blocked.attempt_id,
        )
        .unwrap();
    assert_eq!(withdrawn.state, NotificationState::WithdrawnByOperator);
}

#[tokio::test]
async fn proven_unwritten_runs_through_attempt_and_retry_disposition() {
    let (scratch, store, context, handle, recipient) =
        notification_fixture("unwritten-production-arm");
    let inner = unwritten_test_inner(&scratch.0);
    let binding = unwritten_test_binding();
    seed_unwritten_test_composer(&inner, &binding);
    assert!(advance(
        &inner,
        &handle,
        &[DeliveryState::Queued],
        Step::to(DeliveryState::Gating),
    ));
    context.record_gating().unwrap();
    handle.state.lock().unwrap().attempts = 1;
    assert!(advance(
        &inner,
        &handle,
        &[DeliveryState::Gating],
        Step::to(DeliveryState::Pasting),
    ));

    let failure = match run_unwritten_attempt_arm(&inner, &handle, &binding).await {
        AttemptOutcome::Failed(failure) => failure,
        _ => panic!("proven unwritten paste must be an attempt failure"),
    };
    assert_eq!(failure.cause, "paste_command_unwritten");
    assert_eq!(failure.boundary, WriteBoundary::BeforeWrite);
    assert!(!handle.write_boundary_crossed.load(Ordering::SeqCst));
    assert!(handle.state.lock().unwrap().barrier.is_none());
    {
        let detection = inner.detections.lock().unwrap();
        let entry = detection.get(&PaneKey::new(0, "%1")).unwrap();
        assert_eq!(entry.hold, ComposerHold::Clear);
        assert_eq!(entry.hold_owner, None);
    }
    let corrected = notification_state(&store, recipient, context.message_id());
    assert_eq!(corrected.state, NotificationState::BlockedPreWrite);
    assert_eq!(
        corrected.pre_write_cause,
        Some(NotificationPreWriteCause::PasteCommandUnwritten)
    );
    assert!(corrected.binding.is_none());
    assert!(store
        .lock()
        .unwrap()
        .projection()
        .active_notification_barriers()
        .is_empty());
    let corrected_seq = corrected.updated_seq;
    let worker = Arc::new(Worker::new());
    assert!(!fail_attempt(&inner, &worker, &handle, &failure).await);
    assert_eq!(handle.state(), DeliveryState::Pasting);
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).updated_seq,
        corrected_seq,
        "retry disposition must not append or reopen the corrected attempt"
    );

    let (failed_scratch, failed_store, failed_context, failed_handle, failed_recipient) =
        notification_fixture("unwritten-production-arm-append-failure");
    let failed_inner = unwritten_test_inner(&failed_scratch.0);
    seed_unwritten_test_composer(&failed_inner, &binding);
    assert!(advance(
        &failed_inner,
        &failed_handle,
        &[DeliveryState::Queued],
        Step::to(DeliveryState::Gating),
    ));
    failed_context.record_gating().unwrap();
    failed_handle.state.lock().unwrap().attempts = 1;
    assert!(advance(
        &failed_inner,
        &failed_handle,
        &[DeliveryState::Gating],
        Step::to(DeliveryState::Pasting),
    ));
    failed_store
        .lock()
        .unwrap()
        .inject_next_pre_write_block_append_failure();

    let append_failure =
        match run_unwritten_attempt_arm(&failed_inner, &failed_handle, &binding).await {
            AttemptOutcome::Failed(failure) => failure,
            _ => panic!("failed correction append must remain a failed attempt"),
        };
    assert_eq!(append_failure.cause, NOTIFICATION_RECORD_FAILED);
    assert_eq!(append_failure.boundary, WriteBoundary::AfterWrite);
    assert!(failed_handle.write_boundary_crossed.load(Ordering::SeqCst));
    assert!(failed_handle.state.lock().unwrap().barrier.is_some());
    {
        let failed_detection = failed_inner.detections.lock().unwrap();
        let failed_entry = failed_detection.get(&PaneKey::new(0, "%1")).unwrap();
        assert_eq!(failed_entry.hold, ComposerHold::Staged);
        assert_eq!(
            failed_entry.hold_owner.as_deref(),
            Some(failed_handle.barrier_owner().as_str())
        );
    }
    let writing = notification_state(&failed_store, failed_recipient, failed_context.message_id());
    assert_eq!(writing.state, NotificationState::Writing);
    assert!(writing.binding.is_some());
    assert_eq!(
        failed_store
            .lock()
            .unwrap()
            .projection()
            .active_notification_barriers(),
        vec![writing]
    );

    let direct = DeliveryHandle::new(
        "m-direct-unwritten-production-arm",
        "reviewer",
        "%1",
        0,
        "payload".into(),
    );
    direct.state.lock().unwrap().attempts = 1;
    assert!(advance(
        &inner,
        &direct,
        &[DeliveryState::Queued],
        Step::to(DeliveryState::Gating),
    ));
    assert!(advance(
        &inner,
        &direct,
        &[DeliveryState::Gating],
        Step::to(DeliveryState::Pasting),
    ));
    let direct_failure = match run_unwritten_attempt_arm(&inner, &direct, &binding).await {
        AttemptOutcome::Failed(failure) => failure,
        _ => panic!("direct unwritten paste must be an attempt failure"),
    };
    let direct_worker = Worker::new();
    assert!(fail_attempt(&inner, &direct_worker, &direct, &direct_failure).await);
    assert_eq!(direct.state(), DeliveryState::RetryQueued);
}

#[test]
fn only_a_zero_byte_command_write_is_a_proven_unwritten_paste() {
    assert_eq!(
        classify_paste_buffer_failure(TmuxError::Io(std::io::Error::other("first write"))),
        InjectFailure::PasteCommandUnwritten
    );
    assert_eq!(
        classify_paste_buffer_failure(TmuxError::WriteUncertain(std::io::Error::other(
            "partial write or flush"
        ))),
        InjectFailure::Other("paste_failed".to_string())
    );
    assert_eq!(
        classify_paste_buffer_failure(TmuxError::Command("tmux refused".to_string())),
        InjectFailure::Other("paste_failed".to_string())
    );
}

#[test]
fn a_claim_after_proven_unwritten_is_not_postwrite_evidence() {
    let (_scratch, store, context, _handle, recipient) =
        notification_fixture("unwritten-claim-order");
    context.record_gating().unwrap();
    context
        .record_writing(
            ProcessInstanceId::new(3999, 817_999).unwrap(),
            ProcessInstanceId::new(4000, 818_000).unwrap(),
            ProcessInstanceId::new(4242, 818_221).unwrap(),
            "codex",
            NotificationTransport::Doorbell,
            None,
        )
        .unwrap();
    let blocked = context.record_paste_command_unwritten().unwrap();
    store
        .lock()
        .unwrap()
        .claim(recipient, context.message_id().clone())
        .unwrap();

    assert!(!store
        .lock()
        .unwrap()
        .projection()
        .exact_recipient_claimed_after_write(&blocked));
}

#[test]
fn the_unwritten_cause_requires_a_prior_writing_fact() {
    let (_scratch, store, context, _handle, recipient) =
        notification_fixture("unwritten-requires-writing");
    context.record_gating().unwrap();

    assert!(context
        .record_pre_write_block(NotificationPreWriteCause::PasteCommandUnwritten, None)
        .is_err());
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        NotificationState::Gating
    );
}

#[tokio::test]
async fn superseded_notification_aborts_inside_on_write_without_pasting() {
    let (_scratch, store, context, handle, recipient) = notification_fixture("superseded");
    context.record_gating().unwrap();
    supersede_notification(
        &store,
        recipient,
        context.message_id(),
        "m-superseded-replacement",
    );

    let manifest = sentinel_manifest();
    let payload = handle.payload();
    let screen = format!("\u{1b}[39m❯ {payload}\n{CHROME}");
    let injector = MockInjector::new(vec![&screen]);
    injector.spool(&payload).await.unwrap();
    let error = inject(
        &injector,
        &handle,
        &manifest,
        StagingTarget::ExactRow(&payload),
        &payload,
        &|| {
            context
                .ensure_current_gating()
                .map_err(notification_write_cause)
        },
    )
    .await
    .unwrap_err();

    assert_eq!(
        error,
        InjectFailure::Other(NO_LONGER_CURRENT_BEFORE_WRITE.to_string())
    );
    assert!(injector.pasted.lock().unwrap().is_empty());
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        cyclops_proto::NotificationState::Superseded
    );
}

#[test]
fn a_socket_claim_does_not_suppress_the_operator_visible_doorbell() {
    let (_scratch, store, context, _handle, recipient) = notification_fixture("claimed");
    context.record_gating().unwrap();
    let outcome = store
        .lock()
        .unwrap()
        .claim(recipient, context.message_id().clone())
        .unwrap();
    let crate::mailbox::ClaimOutcome::Claimed {
        withdrawn_attempt, ..
    } = outcome
    else {
        panic!("first claim must append a claim fact");
    };
    assert_eq!(withdrawn_attempt, None);
    context
        .ensure_current_gating()
        .expect("socket claim must not cancel the independent pane notification");
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        cyclops_proto::NotificationState::Gating
    );
}

#[tokio::test]
async fn operator_withdrawal_wakes_a_queued_attempt_before_gating() {
    let (_scratch, store, context, handle, recipient) = notification_fixture("operator-queued");
    let admin = RecipientKey::admin(recipient.workspace_id());
    store
        .lock()
        .unwrap()
        .withdraw_notification_before_write(admin, recipient, context.attempt_id())
        .unwrap();

    let engine = Engine::new();
    engine
        .notification_attempts
        .lock()
        .unwrap()
        .insert(context.attempt_id(), Arc::downgrade(&handle));
    engine.cancel_notification(context.attempt_id());
    tokio::time::timeout(Duration::from_millis(100), handle.cancel.notified())
        .await
        .expect("operator withdrawal wakes the exact queued attempt");

    assert!(matches!(
        context.record_gating(),
        Err(NotificationAdapterError::NoLongerCurrentBeforeWrite)
    ));
    assert!(!handle.write_boundary_crossed.load(Ordering::SeqCst));
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        NotificationState::WithdrawnByOperator
    );
}

#[test]
fn staged_doorbell_claim_cannot_prove_submit_or_turn_start() {
    let (_scratch, store, context, _handle, recipient) = notification_fixture("staged-claim");
    context.record_gating().unwrap();
    let writing = context
        .record_writing(
            ProcessInstanceId::new(3999, 817_999).unwrap(),
            ProcessInstanceId::new(4000, 818_000).unwrap(),
            ProcessInstanceId::new(4242, 818_221).unwrap(),
            "codex",
            NotificationTransport::Doorbell,
            None,
        )
        .unwrap();
    context.record_staged().unwrap();

    let outcome = store
        .lock()
        .unwrap()
        .claim(recipient, context.message_id().clone())
        .unwrap();
    let crate::mailbox::ClaimOutcome::Claimed {
        withdrawn_attempt,
        consumed_doorbell_attempt,
        ..
    } = outcome
    else {
        panic!("first claim must append a claim fact");
    };
    assert_eq!(withdrawn_attempt, None);
    assert_eq!(consumed_doorbell_attempt, None);

    let store = store.lock().unwrap();
    let record = store
        .projection()
        .notification(recipient, context.message_id())
        .unwrap();
    assert_eq!(record.state, cyclops_proto::NotificationState::Staged);
    assert_eq!(record.binding, writing.binding);
    let barriers = store.projection().active_notification_barriers();
    assert_eq!(barriers.len(), 1);
    assert_eq!(barriers[0].attempt_id, context.attempt_id());
    assert_eq!(barriers[0].state, cyclops_proto::NotificationState::Staged);
    drop(store);
    assert_eq!(
        context.reserve_submit().unwrap(),
        SubmitReservation::Reserved
    );
}

#[test]
fn changed_content_after_submit_reservation_becomes_one_durable_attention() {
    let (_scratch, store, context, handle, recipient) =
        notification_fixture("reserved-content-change");
    context.record_gating().unwrap();
    context
        .record_writing(
            ProcessInstanceId::new(3999, 817_999).unwrap(),
            ProcessInstanceId::new(4000, 818_000).unwrap(),
            ProcessInstanceId::new(4242, 818_221).unwrap(),
            "codex",
            NotificationTransport::Doorbell,
            None,
        )
        .unwrap();
    context.record_staged().unwrap();
    assert_eq!(
        context.reserve_submit().unwrap(),
        SubmitReservation::Reserved
    );

    let manifest = sentinel_manifest();
    let expected = handle.payload();
    let exact = format!("\u{1b}[39m❯ {expected}\n{CHROME}");
    let (id_staged, payload_at_proof) = exact_staging_proof(
        &manifest,
        &exact,
        StagingTarget::ExactRow(&expected),
        &expected,
    )
    .expect("baseline exact payload");
    let changed = format!("\u{1b}[39m❯ {expected} plus human draft\n{CHROME}");
    assert!(!exact_staging_snapshot_matches(
        &manifest,
        &changed,
        StagingTarget::ExactRow(&expected),
        &expected,
        id_staged,
        &payload_at_proof,
    ));

    context
        .record_attention(NotificationAttentionCause::VerifyFailed)
        .unwrap();
    let attention = notification_state(&store, recipient, context.message_id());
    assert_eq!(attention.state, NotificationState::AttentionRequired);
    assert_eq!(
        attention.cause,
        Some(NotificationAttentionCause::VerifyFailed)
    );
    let sequence = store.lock().unwrap().projection().last_sequence();
    let repeated = context
        .record_attention(NotificationAttentionCause::VerifyFailed)
        .unwrap();
    assert_eq!(repeated.state, NotificationState::AttentionRequired);
    assert_eq!(store.lock().unwrap().projection().last_sequence(), sequence);
}

#[tokio::test]
async fn cleared_claimed_stage_retries_only_the_settlement_fact_once() {
    let (_scratch, store, context, _handle, recipient) =
        notification_fixture("claimed-clear-retry");
    prepare_claimed_staged(&store, &context, recipient);
    let injector = MockInjector::new(Vec::new());
    injector.clear("%1", &["C-c".to_string()]).await.unwrap();
    let before_seq = store.lock().unwrap().projection().last_sequence();
    store
        .lock()
        .unwrap()
        .inject_next_claimed_staged_clear_append_failure();

    let settled = settle_claimed_staged_after_clear(&context).unwrap();
    assert_eq!(settled.state, NotificationState::WithdrawnAfterStaging);
    assert_eq!(
        store.lock().unwrap().projection().last_sequence(),
        before_seq.map(|seq| seq + 1),
        "the failed append writes nothing and the one bounded retry appends one fact"
    );
    assert!(store
        .lock()
        .unwrap()
        .projection()
        .active_notification_barriers()
        .is_empty());

    let settled_seq = store.lock().unwrap().projection().last_sequence();
    let replayed = settle_claimed_staged_after_clear(&context).unwrap();
    assert_eq!(replayed, settled);
    assert_eq!(
        store.lock().unwrap().projection().last_sequence(),
        settled_seq,
        "an already-landed settlement is discovered without a second fact"
    );
    assert_eq!(
        injector.submitted.lock().unwrap().as_slice(),
        &[("%1".to_string(), "C-c".to_string())],
        "settlement retry never repeats clear or sends Enter"
    );
    assert!(injector.pasted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn persistent_claimed_stage_settlement_failure_faults_the_exact_fifo_owner() {
    let (_scratch, store, context, handle, recipient) =
        notification_fixture("claimed-clear-persistent-failure");
    prepare_claimed_staged(&store, &context, recipient);
    let injector = MockInjector::new(Vec::new());
    injector.clear("%1", &["C-c".to_string()]).await.unwrap();
    store
        .lock()
        .unwrap()
        .inject_claimed_staged_clear_append_failures(2);

    assert!(settle_claimed_staged_after_clear(&context).is_err());
    let record = notification_state(&store, recipient, context.message_id());
    assert_eq!(record.state, NotificationState::Staged);
    assert_eq!(
        store
            .lock()
            .unwrap()
            .projection()
            .active_notification_barriers(),
        vec![record],
        "both failed appends leave the exact barrier as FIFO owner"
    );

    let worker = Arc::new(Worker::new());
    let follower = DeliveryHandle::new("m-follower", "reviewer", "%1", 0, String::new());
    worker.enqueue_back(Arc::clone(&handle));
    worker.enqueue_back(follower);
    assert!(Arc::ptr_eq(
        &worker.current_or_next().expect("current attempt"),
        &handle
    ));
    let failure = AttemptFailure::claimed_staged_settlement_failed();
    assert!(fault_notification_worker(&worker, &failure));
    let state = worker.state.lock().unwrap();
    assert_eq!(
        state.fault.as_deref(),
        Some(CLAIMED_STAGED_SETTLEMENT_FAILED)
    );
    assert!(state
        .current
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &handle)));
    assert_eq!(
        state.queue.len(),
        1,
        "the follower remains behind the fault"
    );
    drop(state);

    let engine = Engine::new();
    engine.notification_workers.lock().unwrap().insert(
        recipient,
        NotificationWorker {
            worker,
            task: test_worker_task(),
        },
    );
    let diagnostics = engine.notification_worker_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].code,
        "notification_settlement_storage_failed"
    );
    assert_eq!(diagnostics[0].notification_attempt, context.attempt_id());
    assert_eq!(
        injector.submitted.lock().unwrap().as_slice(),
        &[("%1".to_string(), "C-c".to_string())],
        "persistent storage failure does not repeat clear or send Enter"
    );
}

#[tokio::test]
async fn failed_prewrite_block_append_keeps_the_exact_fifo_owner() {
    let (_scratch, store, context, handle, recipient) =
        notification_fixture("worker-prewrite-append-failure");
    store
        .lock()
        .unwrap()
        .inject_next_pre_write_block_append_failure();

    let worker = Arc::new(Worker::new());
    let follower = DeliveryHandle::new("m-follower", "reviewer", "%1", 0, String::new());
    worker.enqueue_back(Arc::clone(&handle));
    worker.enqueue_back(Arc::clone(&follower));
    assert!(Arc::ptr_eq(
        &worker.current_or_next().expect("current attempt"),
        &handle
    ));

    let error = context
        .record_gating()
        .and_then(|_| context.record_pre_write_block(NotificationPreWriteCause::WorkerFailed, None))
        .expect_err("injected append failure reaches the Module boundary");
    worker.set_fault(format!("notification recovery failed: {error}"));
    assert!(worker.is_faulted());
    assert!(Arc::ptr_eq(
        &worker.current().expect("failed attempt remains current"),
        &handle
    ));
    assert_eq!(worker.position_of(&follower), 1);
    assert!(!handle.write_boundary_crossed.load(Ordering::SeqCst));
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        NotificationState::Gating,
        "the failed append must not invent a terminal block"
    );

    let engine = Engine::new();
    engine.notification_workers.lock().unwrap().insert(
        recipient,
        NotificationWorker {
            worker: Arc::clone(&worker),
            task: test_worker_task(),
        },
    );
    let diagnostics = engine.notification_worker_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "notification_recovery_storage_failed");
    assert_eq!(diagnostics[0].message_id, context.message_id().clone());
    assert_eq!(diagnostics[0].notification_attempt, context.attempt_id());
}

#[tokio::test]
async fn failed_readiness_block_append_keeps_the_exact_fifo_owner() {
    let (_scratch, store, context, handle, recipient) =
        notification_fixture("readiness-block-append-failure");
    context.record_gating().unwrap();
    store
        .lock()
        .unwrap()
        .inject_next_pre_write_block_append_failure();

    let worker = Arc::new(Worker::new());
    let follower = DeliveryHandle::new("m-follower", "reviewer", "%1", 0, String::new());
    worker.enqueue_back(Arc::clone(&handle));
    worker.enqueue_back(Arc::clone(&follower));
    assert!(Arc::ptr_eq(
        &worker.current_or_next().expect("current attempt"),
        &handle
    ));

    let error = context
        .record_pre_write_block(
            NotificationPreWriteCause::WriteReadinessChanged,
            Some(NotificationPreWriteObservation {
                write_block: None,
                pane_root: None,
                selected_manifest: None,
                binding: None,
                route_evidence: Some(NotificationRouteEvidenceId {
                    boot_id: "boot".into(),
                    generation: 1,
                }),
                pane_width: None,
                required_pane_width: None,
            }),
        )
        .expect_err("injected append failure reaches the Module boundary");
    worker.set_fault(format!(
        "notification pre-write block storage failed: {error}"
    ));
    assert!(worker.is_faulted());
    assert!(Arc::ptr_eq(
        &worker.current().expect("failed attempt remains current"),
        &handle
    ));
    assert_eq!(worker.position_of(&follower), 1);
    assert!(!handle.write_boundary_crossed.load(Ordering::SeqCst));
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        NotificationState::Gating,
        "the failed append must not invent a durable block"
    );

    let engine = Engine::new();
    engine.notification_workers.lock().unwrap().insert(
        recipient,
        NotificationWorker {
            worker,
            task: test_worker_task(),
        },
    );
    let diagnostics = engine.notification_worker_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "notification_prewrite_storage_failed");
    assert_eq!(diagnostics[0].notification_attempt, context.attempt_id());
}

#[test]
fn readiness_block_persistence_records_route_baseline_and_reopens_once() {
    let path = cyclops_proto::scratch::scratch_dir(&format!(
        "readiness-route-baseline-{}",
        uuid::Uuid::new_v4()
    ));
    let _scratch = NotificationScratch(path.clone());
    let root = StateRoot::open_or_create(&path).unwrap();
    let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
    let session = SessionInstanceId::from_str("00000000-0000-4000-8000-000000000002").unwrap();
    let recipient = RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%1").unwrap());
    let directory = || {
        MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: recipient,
                label: "reviewer".into(),
            }],
        )
        .unwrap()
    };
    let store = MessageStore::open(
        &root,
        Path::new("workspaces/current/messages.ndjson"),
        workspace,
        "boot",
    )
    .unwrap();
    let service = MailboxService::new(directory(), store);
    let message = service
        .send(
            service.admin(),
            MailboxSend {
                addresses: vec!["reviewer".into()],
                recipient_keys: None,
                subject: "Wake".into(),
                summary: None,
                body: "Review the mailbox".into(),
                fyi: false,
                client_key: None,
                supersedes: None,
            },
        )
        .unwrap();
    let queued = service
        .prepare_oldest_notification(recipient)
        .unwrap()
        .unwrap();
    let context = NotificationContext::new(
        service.store_handle(),
        message.message_id,
        recipient,
        queued.attempt_id,
    );
    context.record_gating().unwrap();
    let route_evidence = |generation| NotificationRouteEvidenceId {
        boot_id: "boot".into(),
        generation,
    };
    let baseline = route_evidence(7);
    let blocked = context
        .record_pre_write_block(
            NotificationPreWriteCause::WriteReadinessChanged,
            Some(NotificationPreWriteObservation {
                write_block: None,
                pane_root: None,
                selected_manifest: None,
                binding: None,
                route_evidence: Some(baseline.clone()),
                pane_width: None,
                required_pane_width: None,
            }),
        )
        .expect("readiness block");
    let stored = blocked
        .pre_write_observation
        .as_ref()
        .expect("route baseline");
    assert_eq!(stored.route_evidence.as_ref(), Some(&baseline));
    assert!(stored.pane_root.is_none());
    assert!(stored.selected_manifest.is_none());
    assert!(stored.binding.is_none());
    assert!(stored.pane_width.is_none());
    assert!(stored.required_pane_width.is_none());

    let message_id = context.message_id().clone();
    drop(context);
    drop(service);
    let store = MessageStore::open(
        &root,
        Path::new("workspaces/current/messages.ndjson"),
        workspace,
        "boot",
    )
    .unwrap();
    let service = MailboxService::new(directory(), store);
    let replay_store = service.store_handle();
    let replayed = replay_store
        .lock()
        .unwrap()
        .projection()
        .notification(recipient, &message_id)
        .cloned()
        .expect("replayed readiness block");
    assert_eq!(
        replayed
            .pre_write_observation
            .as_ref()
            .and_then(|observation| observation.route_evidence.as_ref()),
        Some(&baseline)
    );

    let pane_root = ProcessInstanceId::new(3999, 817_999).unwrap();
    let manifest = NotificationManifestId::new("codex").unwrap();
    let binding = NotificationBinding {
        recipient,
        pane_root: Some(pane_root),
        leader: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
        agent: ProcessInstanceId::new(4242, 818_221).unwrap(),
        manifest: manifest.clone(),
    };
    let observation = |generation| NotificationPreWriteObservation {
        write_block: None,
        pane_root: Some(pane_root),
        selected_manifest: Some(manifest.clone()),
        binding: Some(binding.clone()),
        route_evidence: Some(route_evidence(generation)),
        pane_width: None,
        required_pane_width: None,
    };
    let lines_before = service.journal_lines().unwrap().len();
    assert!(service
        .reopen_oldest_notification_after_route_evidence(recipient, observation(7), true)
        .unwrap()
        .is_none());
    assert!(service
        .reopen_oldest_notification_after_route_evidence(recipient, observation(6), true)
        .unwrap()
        .is_none());
    assert_eq!(service.journal_lines().unwrap().len(), lines_before);
    assert!(service
        .reopen_oldest_notification_after_route_evidence(recipient, observation(8), false)
        .unwrap()
        .is_none());

    let reopened = service
        .reopen_oldest_notification_after_route_evidence(recipient, observation(8), true)
        .unwrap()
        .expect("later ready route reopens");
    assert_eq!(reopened.attempt_id, queued.attempt_id);
    assert_eq!(reopened.state, NotificationState::Gating);
    assert_eq!(reopened.pre_write_reopen_count, 1);

    let reopened_context = NotificationContext::new(
        service.store_handle(),
        message_id,
        recipient,
        queued.attempt_id,
    );
    let blocked_again = reopened_context
        .record_pre_write_block(
            NotificationPreWriteCause::WriteReadinessChanged,
            Some(NotificationPreWriteObservation {
                write_block: None,
                pane_root: None,
                selected_manifest: None,
                binding: None,
                route_evidence: Some(route_evidence(8)),
                pane_width: None,
                required_pane_width: None,
            }),
        )
        .expect("second readiness block");
    assert_eq!(blocked_again.pre_write_reopen_count, 1);
    let lines_before_repeat = service.journal_lines().unwrap().len();
    assert!(service
        .reopen_oldest_notification_after_route_evidence(recipient, observation(9), true)
        .unwrap()
        .is_none());
    assert_eq!(service.journal_lines().unwrap().len(), lines_before_repeat);
}

#[test]
fn a_socket_claim_does_not_retire_the_operator_notification_prewrite_block() {
    let (_scratch, store, context, handle, recipient) =
        notification_fixture("claim-wins-prewrite-block");
    context.record_gating().unwrap();
    store
        .lock()
        .unwrap()
        .claim(recipient, context.message_id().clone())
        .unwrap();

    let worker = Arc::new(Worker::new());
    worker.enqueue_back(Arc::clone(&handle));
    assert!(Arc::ptr_eq(
        &worker.current_or_next().expect("current attempt"),
        &handle
    ));

    let record = context.record_pre_write_block(
        NotificationPreWriteCause::WriteReadinessChanged,
        Some(NotificationPreWriteObservation {
            write_block: None,
            pane_root: None,
            selected_manifest: None,
            binding: None,
            route_evidence: Some(NotificationRouteEvidenceId {
                boot_id: "boot".into(),
                generation: 1,
            }),
            pane_width: None,
            required_pane_width: None,
        }),
    );
    assert!(record.is_ok());
    assert!(!worker.is_faulted());
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        NotificationState::BlockedPreWrite
    );
    assert!(Arc::ptr_eq(
        &worker
            .current_or_next()
            .expect("notification remains the FIFO owner"),
        &handle
    ));
}

#[tokio::test]
async fn clean_restart_recovery_settles_without_clear_or_enter() {
    let (scratch, store, context, handle, recipient) =
        notification_fixture("claimed-clean-restart");
    let staged = prepare_claimed_staged(&store, &context, recipient);
    let message_id = context.message_id().clone();
    let attempt_id = context.attempt_id();
    let binding = staged.binding.clone();
    drop(handle);
    drop(context);
    drop(store);

    let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
    let root = StateRoot::open_or_create(&scratch.0).unwrap();
    let reopened = MessageStore::open(
        &root,
        Path::new("workspaces/current/messages.ndjson"),
        workspace,
        "boot-recovered",
    )
    .unwrap();
    let store = Arc::new(StdMutex::new(reopened));
    let recovered = store
        .lock()
        .unwrap()
        .projection()
        .claimed_notification_barrier(recipient)
        .cloned()
        .expect("restart retains the claimed staged attempt");
    assert_eq!(recovered.attempt_id, attempt_id);
    assert_eq!(recovered.binding, binding);

    let manifest = Manifest::parse(
        r#"
[agent]
id = "codex"
display_name = "codex"

[[rule]]
id = "composer_clean"
state = "idle"
composer_semantic = "clean"
priority = 1000
region = "bottom_non_empty_lines(3)"
line_regex = ['^❯$']
line_regex_esc = ['^❯$']

[injection]
composer_trailer_regex = ['^status$']
composer_trailer_regex_esc = ['^status$']
composer_trailer_required_prefix = 1
composer_prompt_regex = '^❯(?P<content>.*)$'
composer_continuation_regex = '^  (?P<content>.*)$'
unstyled_composer_proof = 'structural_trailer'
"#,
        Path::new("clean.toml"),
    )
    .unwrap();
    let capture = "transcript\n❯\nstatus";
    let expected = cyclops_proto::render_doorbell_v1(&message_id);
    assert_eq!(
        classify_claimed_staged_composer(
            &manifest,
            capture,
            StagingTarget::ExactRow(&expected),
            &expected,
        ),
        ClaimedStagedComposer::Clean
    );
    assert_eq!(
        claimed_staged_action(
            ClaimedStagedComposer::Clean,
            ClaimedStagedReconciliation::Recovered(ClaimedNotificationBarrier::Staged),
        ),
        ClaimedStagedAction::SettleOnly
    );

    let injector = MockInjector::new(vec![capture]);
    let observed = injector.capture_joined_escaped("%1").await.unwrap();
    assert_eq!(observed, capture);
    let context = NotificationContext::new(store.clone(), message_id, recipient, attempt_id);
    let settled = settle_claimed_staged_after_clear(&context).unwrap();
    assert_eq!(settled.state, NotificationState::WithdrawnAfterStaging);
    assert!(injector.submitted.lock().unwrap().is_empty());
    assert!(injector.pasted.lock().unwrap().is_empty());
    assert!(store
        .lock()
        .unwrap()
        .projection()
        .active_notification_barriers()
        .is_empty());

    let unsupported = Manifest::parse(
        r#"
[agent]
id = "unsupported"
display_name = "unsupported"
[[rule]]
id = "composer_clean"
state = "idle"
composer_semantic = "clean"
priority = 1000
region = "bottom_non_empty_lines(1)"
line_regex = ['^❯$']
line_regex_esc = ['^❯$']
"#,
        Path::new("unsupported.toml"),
    )
    .unwrap();
    assert!(!clean_composer_proof(&unsupported, "❯"));
    assert!(!clean_composer_proof(&manifest, "transcript\n❯"));
    assert!(clean_composer_proof(
        &manifest,
        "❯\ntranscript echo\n❯\nstatus"
    ));
    assert!(!clean_composer_proof(
        &manifest,
        "❯\ntranscript echo\n❯\nstatus\nunexpected"
    ));
    assert!(!clean_composer_proof(&manifest, "❯human draft\nstatus"));

    let chip_manifest = composer_manifest();
    let chip = "transcript\n\u{1b}[39m❯\u{a0}[Pasted text #1]\n? for shortcuts";
    assert_eq!(
        exact_composer_content_for_state(
            &chip_manifest,
            chip,
            AgentState::Idle,
            Some(ComposerSemantic::Clean),
        ),
        ComposerContentProof::Hidden
    );
    assert!(!clean_composer_proof(&chip_manifest, chip));
}

#[test]
fn claimed_stage_policy_clears_only_exact_bytes_and_refuses_human_input() {
    let manifest = sentinel_manifest();
    let message_id = MessageId::new("m-claimed-policy").unwrap();
    let expected = cyclops_proto::render_doorbell_v1(&message_id);
    let exact = format!("\u{1b}[39m❯ {expected}\n{CHROME}");
    let human = format!("\u{1b}[39m❯ {expected} plus my draft\n{CHROME}");

    assert_eq!(
        classify_claimed_staged_composer(
            &manifest,
            &exact,
            StagingTarget::ExactRow(&expected),
            &expected,
        ),
        ClaimedStagedComposer::ExactDoorbell
    );
    assert_eq!(
        claimed_staged_action(
            ClaimedStagedComposer::ExactDoorbell,
            ClaimedStagedReconciliation::Recovered(ClaimedNotificationBarrier::Staged),
        ),
        ClaimedStagedAction::ClearThenSettle
    );
    assert_eq!(
        classify_claimed_staged_composer(
            &manifest,
            &human,
            StagingTarget::ExactRow(&expected),
            &expected,
        ),
        ClaimedStagedComposer::Ambiguous
    );
    assert_eq!(
        claimed_staged_action(
            ClaimedStagedComposer::Ambiguous,
            ClaimedStagedReconciliation::Recovered(ClaimedNotificationBarrier::Staged),
        ),
        ClaimedStagedAction::Refuse
    );
}

#[tokio::test]
async fn claim_after_submit_reservation_waits_for_submit_success() {
    let (_scratch, store, context, handle, recipient) =
        notification_fixture("claim-after-reservation");
    context.record_gating().unwrap();
    context
        .record_writing(
            ProcessInstanceId::new(3999, 817_999).unwrap(),
            ProcessInstanceId::new(4000, 818_000).unwrap(),
            ProcessInstanceId::new(4242, 818_221).unwrap(),
            "codex",
            NotificationTransport::Doorbell,
            None,
        )
        .unwrap();
    context.record_staged().unwrap();
    assert_eq!(
        context.reserve_submit().unwrap(),
        SubmitReservation::Reserved
    );

    let first = store
        .lock()
        .unwrap()
        .claim(recipient, context.message_id().clone())
        .unwrap();
    let crate::mailbox::ClaimOutcome::Claimed {
        entry: first_entry,
        message: first_message,
        withdrawn_attempt,
        consumed_doorbell_attempt,
        ..
    } = first
    else {
        panic!("first claim must append one claim fact");
    };
    assert_eq!(first_entry.message_id, *context.message_id());
    assert_eq!(first_message.message_id, *context.message_id());
    assert_eq!(withdrawn_attempt, None);
    assert_eq!(consumed_doorbell_attempt, None);
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        NotificationState::Submitting,
        "a socket claim cannot prove that Enter was sent"
    );

    let sequence_before_reclaim = store.lock().unwrap().projection().last_sequence();
    let repeated = store
        .lock()
        .unwrap()
        .claim(recipient, context.message_id().clone())
        .unwrap();
    let crate::mailbox::ClaimOutcome::AlreadyClaimed {
        entry: repeated_entry,
        message: repeated_message,
        withdrawn_attempt,
        consumed_doorbell_attempt,
        ..
    } = repeated
    else {
        panic!("repeat claim must return the original mailbox task");
    };
    assert_eq!(repeated_entry.message_id, first_entry.message_id);
    assert_eq!(repeated_message.message_id, first_message.message_id);
    assert_eq!(withdrawn_attempt, None);
    assert_eq!(consumed_doorbell_attempt, None);
    assert_eq!(
        store.lock().unwrap().projection().last_sequence(),
        sequence_before_reclaim,
        "reclaim must not append a second task"
    );
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        NotificationState::Submitting
    );

    let doorbell = cyclops_proto::render_doorbell_v1(context.message_id());
    assert_eq!(handle.payload(), doorbell);
    let injector = MockInjector::new(Vec::new());
    let turn = crate::turnkey::TurnKey::for_test(&["session-1", "turn-1"]);
    {
        let mut state = handle.state.lock().unwrap();
        state.state = DeliveryState::Staged;
        state.early_ack = Some(PendingAck {
            edge_ms: 91,
            turn: Some(turn.clone()),
            evidence: PendingAckEvidence::Receipt,
        });
    }
    injector.submit(&handle.pane_id, "Enter").await.unwrap();
    assert_eq!(
        injector.submitted.lock().unwrap().as_slice(),
        &[(handle.pane_id.clone(), "Enter".to_string())],
        "the reserved terminal key submits the same attempt's doorbell"
    );
    context.record_submitted().unwrap();
    handle.state.lock().unwrap().state = DeliveryState::Submitted;
    let early = take_accepted_early_ack(&handle).expect("hook receipt survives to worker");
    assert!(context.settle_submitted_claim().unwrap());
    let step = early_ack_step(early);
    assert_eq!(step.next, DeliveryState::DeliveredVerified);
    assert_eq!(step.cause, Some("hook_ack"));
    assert_eq!(step.verified_by, Some(VerifiedBy::Hook));
    assert_eq!(step.turn_edge_ms, Some(91));
    assert_eq!(step.turn, Some(turn));
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        NotificationState::Notified
    );
}

#[test]
fn notified_state_without_the_exact_claim_does_not_settle_a_claim_race() {
    let (_scratch, _store, context, _handle, _recipient) =
        notification_fixture("notified-without-claim");
    prepare_notification_receipt(&context);
    context.record_notified().unwrap();
    assert!(
        !context.settle_submitted_claim().unwrap(),
        "Notified proves receipt, not an exact mailbox claim"
    );
}

#[test]
fn submitted_doorbell_claim_names_the_attempt_that_consumed_it() {
    let (_scratch, store, context, _handle, recipient) = notification_fixture("submitted-claim");
    prepare_notification_receipt(&context);

    let first = store
        .lock()
        .unwrap()
        .claim(recipient, context.message_id().clone())
        .unwrap();
    let crate::mailbox::ClaimOutcome::Claimed {
        withdrawn_attempt,
        consumed_doorbell_attempt,
        ..
    } = first
    else {
        panic!("first claim must append a claim fact");
    };
    assert_eq!(withdrawn_attempt, None);
    assert_eq!(consumed_doorbell_attempt, Some(context.attempt_id()));
    let record = notification_state(&store, recipient, context.message_id());
    assert_eq!(record.state, cyclops_proto::NotificationState::Notified);
    assert_eq!(record.cause, None);
    assert!(store
        .lock()
        .unwrap()
        .projection()
        .open_alarms_for_message(context.message_id())
        .is_empty());

    let late = context
        .record_attention(NotificationAttentionCause::ReceiptOccupantChanged)
        .unwrap_err();
    assert!(matches!(
        late,
        NotificationAdapterError::TerminalConflict(cyclops_proto::NotificationState::Notified)
    ));
    assert!(store
        .lock()
        .unwrap()
        .projection()
        .open_alarms_for_message(context.message_id())
        .is_empty());

    let repeated = store
        .lock()
        .unwrap()
        .claim(recipient, context.message_id().clone())
        .unwrap();
    let crate::mailbox::ClaimOutcome::AlreadyClaimed {
        consumed_doorbell_attempt,
        ..
    } = repeated
    else {
        panic!("repeat claim must be idempotent");
    };
    assert_eq!(consumed_doorbell_attempt, Some(context.attempt_id()));
}

#[test]
fn claimed_doorbell_still_enters_the_operator_notification_gate() {
    let (_scratch, store, context, _handle, recipient) =
        notification_fixture("withdrawn-before-gate");
    let outcome = store
        .lock()
        .unwrap()
        .claim(recipient, context.message_id().clone())
        .unwrap();
    let crate::mailbox::ClaimOutcome::Claimed {
        withdrawn_attempt, ..
    } = outcome
    else {
        panic!("first claim must append a claim fact");
    };
    assert_eq!(withdrawn_attempt, None);

    context
        .record_gating()
        .expect("claim and operator-visible pane notification are independent");
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        cyclops_proto::NotificationState::Gating
    );
}

#[test]
fn notification_gate_admission_is_idempotent_for_worker_reentry() {
    let (_scratch, store, context, _handle, recipient) = notification_fixture("gate-reentry");

    let first = context.record_gating().unwrap();
    let second = context.record_gating().unwrap();

    assert_eq!(first, second);
    assert_eq!(
        notification_state(&store, recipient, context.message_id()).state,
        cyclops_proto::NotificationState::Gating
    );
}

#[test]
fn notification_faults_map_to_the_closed_attention_taxonomy() {
    for (cause, expected) in [
        ("paste_failed", NotificationAttentionCause::PasteFailed),
        ("verify_failed", NotificationAttentionCause::VerifyFailed),
        (
            "pane_rebound_after_paste",
            NotificationAttentionCause::PaneReboundAfterPaste,
        ),
        ("submit_failed", NotificationAttentionCause::SubmitFailed),
        (
            "receipt_occupant_changed",
            NotificationAttentionCause::ReceiptOccupantChanged,
        ),
        ("ack_timeout", NotificationAttentionCause::AckTimeout),
        (
            NOTIFICATION_RECORD_FAILED,
            NotificationAttentionCause::TransportOutcomeUnknown,
        ),
    ] {
        assert_eq!(notification_attention_cause(cause), expected, "{cause}");
    }
}

#[tokio::test(start_paused = true)]
async fn failed_staging_proof_records_attention_without_retrying_the_paste() {
    let (_scratch, store, context, handle, recipient) = notification_fixture("verify-fault");
    context.record_gating().unwrap();
    let manifest = sentinel_manifest();
    let injector = MockInjector::new(vec!["transcript\n❯\n? for shortcuts"]);
    let payload = handle.payload();
    injector.spool(&payload).await.unwrap();
    let result = inject(
        &injector,
        &handle,
        &manifest,
        StagingTarget::ExactRow(&payload),
        &payload,
        &|| {
            context
                .record_writing(
                    ProcessInstanceId::new(3999, 817_999).unwrap(),
                    ProcessInstanceId::new(4000, 818_000).unwrap(),
                    ProcessInstanceId::new(4242, 818_221).unwrap(),
                    "codex",
                    NotificationTransport::Doorbell,
                    None,
                )
                .map(|_| ())
                .map_err(notification_write_cause)
        },
    )
    .await;
    let (capture, verified, _) =
        result.expect("unverified staging succeeds for one unverified submit");
    assert!(!verified);
    assert_eq!(capture, "transcript\n❯\n? for shortcuts");
    assert_eq!(injector.pasted.lock().unwrap().len(), 1);

    context
        .record_attention(notification_attention_cause("verify_failed"))
        .unwrap();
    let attention = notification_state(&store, recipient, context.message_id());
    assert_eq!(
        attention.state,
        cyclops_proto::NotificationState::AttentionRequired
    );
    assert_eq!(
        attention.cause,
        Some(NotificationAttentionCause::VerifyFailed)
    );
    assert!(attention.binding.is_some());
}

/// A refusal at the write boundary stops the write and costs no
/// transport budget.
///
/// The callback is the last thing between a proof and the pane taking
/// the payload, and it is where the barrier is claimed and the pane's
/// binding is compared again. Both of the things it can refuse for,
/// somebody else holding the composer and the pane becoming another
/// program, are the world moving rather than transport failing:
/// nothing was written, so the delivery goes back to the gate instead
/// of spending a retry or summoning a human.
#[tokio::test]
async fn a_refused_write_boundary_never_pastes_and_never_spends_budget() {
    let m = composer_manifest();
    let handle = DeliveryHandle::new("m-x", "worker", "%1", 0, "payload".into());
    for cause in ["barrier_held", "binding_changed", "capability_changed"] {
        let mock = MockInjector::new(vec!["transcript\n\u{1b}[39m❯\u{a0}\n? for shortcuts"]);
        let payload = handle.payload();
        mock.spool(&payload).await.expect("spool");
        assert_eq!(
            inject(
                &mock,
                &handle,
                &m,
                StagingTarget::Sentinel(&handle.msg_id),
                &payload,
                &|| Err(cause.to_string())
            )
            .await,
            Err(InjectFailure::Other(cause.to_string()))
        );
        assert!(
            mock.pasted.lock().unwrap().is_empty(),
            "{cause} still reached the pane"
        );

        let failure = AttemptFailure::from_inject(cause.to_string());
        assert_eq!(
            failure.boundary,
            WriteBoundary::BeforeWrite,
            "{cause} must not be treated as possibly-written"
        );
        assert!(
            failure.regate_cause().is_some(),
            "{cause} belongs back at the gate, not in the retry budget"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn inject_rejects_stale_and_hidden_staging() {
    let m = composer_manifest();
    let handle = DeliveryHandle::new("m-new01", "worker", "%1", 0, "payload".into());

    let stale = "you: [Pasted text #1 +9 lines]\nold turn\n\u{1b}[39m❯\u{a0}\n? for shortcuts";
    let mock = MockInjector::new(vec![stale]);
    let payload = handle.payload();
    mock.spool(&payload).await.expect("spool");
    assert_eq!(
        inject(
            &mock,
            &handle,
            &m,
            StagingTarget::Sentinel(&handle.msg_id),
            &payload,
            &|| Ok(())
        )
        .await,
        Err(InjectFailure::Other("verify_failed".to_string()))
    );
    assert_eq!(mock.pasted.lock().unwrap().len(), 1, "payload was pasted");

    let staged = "transcript\n\u{1b}[39m❯\u{a0}[Pasted text #2 +9 lines]\n? for shortcuts";
    let mock = MockInjector::new(vec![stale, staged]);
    mock.spool(&payload).await.expect("spool");
    assert_eq!(
        inject(
            &mock,
            &handle,
            &m,
            StagingTarget::Sentinel(&handle.msg_id),
            &payload,
            &|| Ok(()),
        )
        .await,
        Err(InjectFailure::Other("verify_failed".to_string()))
    );
    assert_eq!(mock.pasted.lock().unwrap().len(), 1, "payload was pasted");
    assert!(mock.submitted_is_empty());
}

/// The shipped codex manifest, parsed as data for the two tests below:
/// its only composer discriminators are `line_regex_esc` clauses, on
/// purpose (a plain capture cannot tell its ghost text from typed
/// text).
fn codex_manifest() -> Manifest {
    let m = Manifest::parse(
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../resources/manifests/codex.toml"
        )),
        std::path::Path::new("codex.toml"),
    )
    .expect("shipped codex manifest parses");
    assert!(m.has_escaped_rules(), "codex discriminates by SGR");
    m
}

/// Lines as codex-cli 0.147.0 actually draws them, captured from a
/// live pane on 2026-08-17 (the full screens are
/// cyclops-manifest/tests/fixtures/codex_pasted_chip_*). The chip is
/// COLORED and the transcript glyph is bold-DIM where the composer's
/// is bold-only; both facts decide the assertions below, and an
/// invented approximation of either passed while the real thing
/// failed.
const CODEX_COMPOSER_CHIP: &str =
    "\u{1b}[1m›\u{1b}[0m \u{1b}[38;5;6m[Pasted Content 2828 chars]\u{1b}[39m";
const CODEX_COMPOSER_GHOST: &str = "\u{1b}[1m›\u{1b}[0m \u{1b}[2mSummarize recent commits\u{1b}[0m";
/// The measured composer trailer below that chip, from the same
/// capture: a blank row, then the model row. Both are painted every
/// time, and they are what proves the chip is the last thing in the
/// composer rather than merely present in it.
const CODEX_TRAILER: &str = "\n\n  \u{1b}[38;2;246;226;183mgpt-5.6-sol high\u{1b}[39m · /tmp/x";
const CODEX_TRANSCRIPT_LINE: &str =
    "\u{1b}[1;2m›  \u{1b}[0m[cyclops m-diag01] FROM: tester  SUBJECT: verify chip rendering";

/// The field failure this fixes, pinned on the shipped manifest and
/// real captures: a message long enough to collapse renders as a
/// "[Pasted Content N chars]" chip that hides the id, so the generic
/// "Pasted" tier is the only staging evidence left. That tier
/// pins the marker to the composer line, which for codex is
/// recognizable only in an escaped capture. Every verify re-read
/// failed, verify_before_submit withheld Enter, and the payload sat
/// staged in the recipient's composer behind "outcome unknown".
#[test]
fn codex_collapsed_paste_verifies_through_the_escaped_composer_line() {
    let m = codex_manifest();
    let (id, other) = verify_patterns(&m, "m-jean01");

    let staged = format!("transcript above\n{CODEX_COMPOSER_CHIP}{CODEX_TRAILER}");
    assert_eq!(
        sentinel_representation(&m, &staged, &id, &other, "m-x"),
        Some(StagedRepresentation::CollapsedChip)
    );

    // A chip in the TRANSCRIPT (bold-dim glyph) over an empty
    // composer: an earlier message, already submitted. Nothing staged.
    let stale = format!(
        "\u{1b}[1;2m›  \u{1b}[0m[Pasted Content 900 chars]\n{CODEX_COMPOSER_GHOST}{CODEX_TRAILER}"
    );
    assert_eq!(
        sentinel_representation(&m, &stale, &id, &other, "m-x"),
        None
    );

    // A short message renders literally: the id proves it anywhere in
    // the region, chip or no chip.
    // A short message renders literally, so its sentinel is on screen
    // and is what verifies it: the id alone never does. The status row
    // below it must arrive PAINTED, which is what separates the real
    // chrome from prose shaped like it.
    // Codex paints a blank separator between the composer and status.
    let literal = format!(
            "{CODEX_TRANSCRIPT_LINE}\n\u{1b}[1m›\u{1b}[0m [cyclops m-jean01] hello\n[cyclops:end m-jean01]\n\n  \u{1b}[38;2;246;226;183mgpt-5.6-sol high\u{1b}[39m · /tmp/x"
        );
    assert_eq!(
        sentinel_representation(&m, &literal, &id, &other, "m-jean01"),
        Some(StagedRepresentation::VisibleTarget)
    );
}

/// A collapsed chip confirms representation, not exact composer bytes.
/// The inject path must withhold Enter even when the escaped capture
/// proves the chip belongs to the active composer.
#[tokio::test(start_paused = true)]
async fn inject_refuses_codex_collapse_without_exact_ownership() {
    let m = codex_manifest();
    let handle = DeliveryHandle::new("m-jean01", "codex", "%1", 0, "payload".into());
    let staged = format!("transcript above\n{CODEX_COMPOSER_CHIP}{CODEX_TRAILER}");
    let mock = MockInjector::new(vec![staged.as_str()]);
    let payload = handle.payload();
    mock.spool(&payload).await.expect("spool");
    assert_eq!(
        inject(
            &mock,
            &handle,
            &m,
            StagingTarget::Sentinel(&handle.msg_id),
            &payload,
            &|| Ok(()),
        )
        .await,
        Err(InjectFailure::Other("verify_failed".to_string()))
    );
    assert_eq!(mock.pasted.lock().unwrap().len(), 1);
    assert!(mock.submitted_is_empty());
}

// -----------------------------------------------------------------
// Tier-2 evidence (fix D) and the detach-aware clock (fix E)
// -----------------------------------------------------------------

#[test]
fn tier2_changed_window_alone_needs_the_id_staged() {
    // A changed window with no staged id is a redraw, not delivery
    // evidence (this returned true before fix D).
    assert!(!tier2_evidence(
        AckEvidence::Receipt,
        true,
        false,
        false,
        false
    ));
    assert!(tier2_evidence(
        AckEvidence::Receipt,
        true,
        true,
        false,
        false
    ));
    assert!(tier2_evidence(
        AckEvidence::Receipt,
        false,
        false,
        true,
        false
    ));
    // Output activity is no longer evidence on its own: %output names
    // a pane and its bytes, never the process that wrote them, so a
    // replacement occupant's noise could otherwise resolve a delivery
    // it never received. It survives as a cue to look.
    assert!(!tier2_evidence(
        AckEvidence::Receipt,
        false,
        false,
        false,
        true
    ));
    // Marker gone but nothing else moved: not evidence.
    assert!(!tier2_evidence(
        AckEvidence::Receipt,
        false,
        true,
        false,
        false
    ));
    assert!(
        !tier2_evidence(AckEvidence::Dispatch, true, true, true, true),
        "a dispatch candidate needs exact visual acceptance"
    );
}

#[test]
fn working_evidence_must_follow_submit_for_the_exact_session_and_binding() {
    let handle = DeliveryHandle::new("m-state-evidence", "worker", "%1", 3, "payload".into());
    let agent = crate::identity::ProcId {
        pid: 4242,
        birth: 818_221,
    };
    *handle.submitted_agent.lock().unwrap() = Some(agent);
    *handle.submitted_manifest.lock().unwrap() = Some("fix".into());
    handle.submitted_at_ms.store(1_000, Ordering::SeqCst);

    let event = |session_idx: usize, observed_at_ms: u64, source_birth: u64, confirmed: bool| {
        Ok(Event {
            event: "state".into(),
            data: json!({
                "pane_id": "%1",
                "session_idx": session_idx,
                "state": "working",
                "source_pid": 4242,
                "source_birth": source_birth,
                "source_manifest": "fix",
                "observed_at_ms": observed_at_ms,
                "working_confirmed": confirmed,
            }),
            seq: None,
        })
    };

    assert!(!track_state_event(
        &event(3, 999, agent.birth, true),
        &handle
    ));
    assert!(!track_state_event(
        &event(2, 1_000, agent.birth, true),
        &handle
    ));
    assert!(!track_state_event(
        &event(3, 1_000, agent.birth + 1, true),
        &handle
    ));
    assert!(!track_state_event(
        &event(3, 1_000, agent.birth, false),
        &handle
    ));
    assert!(track_state_event(
        &event(3, 1_000, agent.birth, true),
        &handle
    ));

    // The composed pane wait uses the same gate on its fresh live
    // receiver. A confirmed Working edge from an earlier submit or a
    // replacement process must not turn a later Idle into success.
    assert!(!wait_working_event_is_eligible(
        &event(3, 999, agent.birth, true).expect("event"),
        Some(&handle)
    ));
    assert!(!wait_working_event_is_eligible(
        &event(3, 1_000, agent.birth + 1, true).expect("event"),
        Some(&handle)
    ));
    assert!(wait_working_event_is_eligible(
        &event(3, 1_000, agent.birth, true).expect("event"),
        Some(&handle)
    ));

    // Standalone waits do not inherit a delivery identity; their outer
    // loop has already selected the requested pane and session.
    assert!(wait_working_event_is_eligible(
        &event(2, 999, agent.birth + 1, true).expect("event"),
        None
    ));
}

#[test]
fn buffered_working_evidence_survives_a_screen_checkpoint_race() {
    // Model the receipt boundary precisely: Enter subscribed first, the
    // watcher published a matching Working fact, and a screen checkpoint
    // is ready before the normal receipt loop reads that fact. The
    // checkpoint must not erase the turn evidence needed by `turn_ended`.
    let handle = DeliveryHandle::new("m-buffered-working", "worker", "%1", 3, "payload".into());
    let agent = crate::identity::ProcId {
        pid: 4242,
        birth: 818_221,
    };
    *handle.submitted_agent.lock().unwrap() = Some(agent);
    *handle.submitted_manifest.lock().unwrap() = Some("fix".into());
    handle.submitted_at_ms.store(1_000, Ordering::SeqCst);
    let (events, mut turn_events) = broadcast::channel(4);
    events
        .send(Event {
            event: "state".into(),
            data: json!({
                "pane_id": "%1",
                "session_idx": 3,
                "state": "working",
                "source_pid": 4242,
                "source_birth": agent.birth,
                "source_manifest": "fix",
                "observed_at_ms": 1_000,
                "working_confirmed": true,
            }),
            seq: None,
        })
        .expect("working event has a receiver");

    assert!(record_buffered_working_evidence(&mut turn_events, &handle));
    assert!(handle.working_seen.load(Ordering::SeqCst));
    assert!(!record_buffered_working_evidence(&mut turn_events, &handle));
}

#[test]
fn unobservable_evidence_freezes_instead_of_expiring() {
    // The detach race: the watcher is cleared before its lifecycle
    // event is broadcast, so a checkpoint's evidence pass cannot look.
    // Before the fix an expired clock returned Timeout here and the
    // retry double-pasted a delivery that may have landed.
    assert_eq!(
        checkpoint_step(Evidence::Unobservable, true),
        CheckpointStep::Freeze
    );
    assert_eq!(
        checkpoint_step(Evidence::Unobservable, false),
        CheckpointStep::Freeze
    );
    // Expiry stands only on a pass that looked and saw nothing.
    assert_eq!(
        checkpoint_step(Evidence::Absent, true),
        CheckpointStep::Expire
    );
    assert_eq!(
        checkpoint_step(Evidence::Absent, false),
        CheckpointStep::Wait
    );
    assert_eq!(
        checkpoint_step(Evidence::Confirmed, true),
        CheckpointStep::Deliver
    );
}

#[test]
fn receipt_refresh_freezes_only_for_unobservable_safety_facts() {
    let detection = |state, stale, write_block: Option<&str>| Detection {
        state,
        readings: Vec::new(),
        disagreement: false,
        decided_by: "test".into(),
        unknown_reason: None,
        stale,
        write_ready: write_block.is_none(),
        write_block: write_block.map(str::to_string),
        composer_semantic: None,
    };
    let clean = detection(AgentState::Idle, false, None);
    let stale = detection(AgentState::Idle, true, Some("stale_screen_evidence"));
    let mode = detection(AgentState::Idle, false, Some("pane_in_mode"));
    let unprovable = detection(AgentState::Idle, false, Some("occupant_unprovable"));
    let human_draft = detection(AgentState::IdleWithInput, false, Some("not_idle"));
    let dead = detection(AgentState::Dead, false, Some("not_idle"));

    assert_eq!(
        receipt_refresh_step(false, Some(&clean), false),
        ReceiptRefresh::Freeze
    );
    assert_eq!(
        receipt_refresh_step(true, None, false),
        ReceiptRefresh::Rebound,
        "a live watcher with no submitted pane is a rebound"
    );
    for frozen in [&stale, &mode, &unprovable] {
        assert_eq!(
            receipt_refresh_step(true, Some(frozen), false),
            ReceiptRefresh::Freeze
        );
    }
    assert_eq!(
        receipt_refresh_step(true, Some(&human_draft), false),
        ReceiptRefresh::Observe,
        "observable input must not stop the receipt clock"
    );
    assert_eq!(
        receipt_refresh_step(true, Some(&dead), false),
        ReceiptRefresh::Rebound
    );
    assert_eq!(
        receipt_refresh_step(false, None, true),
        ReceiptRefresh::Resolved,
        "a receipt committed during refresh wins over transport loss"
    );
}

#[test]
fn notification_liveness_protects_only_positive_human_input() {
    let detection = Detection {
        state: AgentState::Idle,
        readings: vec![cyclops_proto::SensorReading {
            sensor: cyclops_proto::Sensor::Screen,
            state: AgentState::Idle,
            rule: "runtime_idle".to_string(),
            ts: 1,
        }],
        disagreement: false,
        decided_by: "runtime_idle".to_string(),
        unknown_reason: None,
        stale: false,
        write_ready: false,
        write_block: Some("composer_hold".to_string()),
        composer_semantic: Some(ComposerSemantic::HumanInput),
    };

    assert!(
        !unproven_composer_is_eligible(&detection),
        "a lower-priority composer rule already proved human input"
    );
    assert!(unproven_composer_is_eligible(&Detection {
        composer_semantic: None,
        write_block: Some("no_write_safe_composer_evidence".into()),
        ..detection.clone()
    }));
    assert!(unproven_composer_is_eligible(&Detection {
        state: AgentState::Working,
        composer_semantic: Some(ComposerSemantic::Ambiguous),
        write_block: Some("no_write_safe_composer_evidence".into()),
        ..detection.clone()
    }));
    assert!(!unproven_composer_is_eligible(&Detection {
        composer_semantic: Some(ComposerSemantic::HumanInput),
        ..detection
    }));
}

#[test]
fn pane_change_rechecks_and_resumes_a_frozen_receipt_clock() {
    let row = |dead| cyclops_tmux::PaneRow {
        pane_id: "%1".into(),
        window_id: "@1".into(),
        window_name: "test".into(),
        title: "test".into(),
        dead,
        in_mode: false,
        current_command: "agent".into(),
        width: 80,
        height: 24,
        active: true,
        pane_pid: 41,
    };
    let changed = Ok(PaneEvent::PaneChanged {
        id: "%1".into(),
        changed: Vec::new(),
        row: row(false),
    });
    let dead = Ok(PaneEvent::PaneChanged {
        id: "%1".into(),
        changed: Vec::new(),
        row: row(true),
    });
    let output = Ok(PaneEvent::OutputActivity {
        pane_id: "%1".into(),
        ts: 1,
    });

    assert_eq!(
        receipt_pane_step(&changed, "%1", true),
        ReceiptPaneStep::Recheck
    );
    assert_eq!(
        receipt_pane_step(&changed, "%1", false),
        ReceiptPaneStep::Recheck
    );
    assert_eq!(
        receipt_pane_step(&dead, "%1", true),
        ReceiptPaneStep::Rebound
    );
    assert_eq!(
        receipt_pane_step(&output, "%1", true),
        ReceiptPaneStep::Recheck
    );
    assert_eq!(
        receipt_pane_step(&output, "%1", false),
        ReceiptPaneStep::Ignore
    );
    assert_eq!(
        receipt_pane_step(&Ok(PaneEvent::Disconnected), "%1", true),
        ReceiptPaneStep::Freeze
    );
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Every target a clock hands out, in milliseconds from the submit it
/// was built on, paired with whether it is the hook-phase end.
///
/// The ACK ladder is a sequence, and asserting it one `next_target` at
/// a time hides the shape and makes a moved rung read as an unrelated
/// number. Reading it as offsets also makes the arithmetic checkable
/// by eye: 1500 is the hook window, 250/750/1500/3000/5000 are the
/// checkpoints, 5000 is the give-up deadline.
fn timeline(mut c: AckClock, submit_at: Instant) -> Vec<(u64, bool)> {
    let mut out = Vec::new();
    // Bounded so a clock that stops advancing fails as a wrong
    // timeline instead of hanging the suite.
    for _ in 0..12 {
        let Some((at, hook_end)) = c.next_target() else {
            break;
        };
        out.push(((at - submit_at).as_millis() as u64, hook_end));
        if hook_end {
            c.end_hook_phase(at);
        } else if c.expired(at) {
            break;
        } else {
            c.advance_checkpoint();
        }
    }
    out
}

/// The clock reads no wall time after it is built.
///
/// Everything it hands out is derived from the submit instant it was
/// given, so two clocks built ten minutes apart must produce the same
/// timeline. This is the property that keeps the assertions above from
/// being load-sensitive, and it is worth stating once rather than
/// leaving every reader to re-derive it from `next_target`.
#[tokio::test(start_paused = true)]
async fn the_ack_timeline_does_not_depend_on_when_the_clock_was_built() {
    let early = Instant::now();
    let late = early + ms(600_000);
    assert_eq!(
        timeline(AckClock::new(early, Some(ms(1500))), early),
        timeline(AckClock::new(late, Some(ms(1500))), late)
    );
    assert_eq!(
        timeline(AckClock::new(early, None), early),
        timeline(AckClock::new(late, None), late)
    );
}

/// Instants are asserted as offsets from the submit the clock was
/// built on, never as wall-clock values: nothing in `AckClock` reads
/// the clock after construction (proved by
/// `the_ack_timeline_does_not_depend_on_when_the_clock_was_built`), so
/// every number below is arithmetic and none of it is a race.
#[tokio::test(start_paused = true)]
async fn ack_clock_freezes_across_detach_and_extends_deadlines() {
    let t0 = Instant::now();
    let at = |c: &AckClock| {
        c.next_target()
            .map(|(t, hook)| ((t - t0).as_millis() as u64, hook))
    };
    let mut c = AckClock::new(t0, Some(ms(1500)));
    assert_eq!(at(&c), Some((1500, true)));

    // Detach at +200ms: the clock stops firing entirely.
    c.freeze(t0 + ms(200));
    assert!(c.frozen());
    assert_eq!(c.next_target(), None);
    assert!(!c.expired(t0 + ms(60_000)), "a frozen clock never expires");
    // A second freeze keeps the first freeze instant.
    c.freeze(t0 + ms(300));

    // Reattach at +6200ms: 6s of outage extend every deadline.
    c.unfreeze(t0 + ms(6200));
    assert_eq!(at(&c), Some((7500, true)));
    c.end_hook_phase(t0 + ms(7500));
    // Checkpoints shifted by 6s. The ones the hook phase covered
    // (250/750/1500 -> 6250/6750/7500) are dropped and replaced by one
    // pass now, so tier 2 opens with a look instead of a wait.
    assert_eq!(at(&c), Some((7500, false)));
    c.advance_checkpoint();
    assert_eq!(at(&c), Some((9000, false)));
    c.advance_checkpoint();
    assert_eq!(at(&c), Some((11_000, false)));
    c.advance_checkpoint();
    // Past the checkpoints the final deadline is also shifted.
    assert_eq!(at(&c), Some((11_000, false)));
    assert!(!c.expired(t0 + ms(10_999)));
    assert!(c.expired(t0 + ms(11_000)));
}

/// A screen-tier agent has no hook window, so the ladder is the
/// checkpoints and nothing else.
#[tokio::test(start_paused = true)]
async fn ack_clock_without_hook_window_goes_straight_to_checkpoints() {
    let t0 = Instant::now();
    assert_eq!(
        timeline(AckClock::new(t0, None), t0),
        vec![
            (250, false),
            (750, false),
            (1500, false),
            (3000, false),
            (5000, false),
        ]
    );
}

/// The shipped numbers, and the hole the receipt fell through.
///
/// ack_timeout_ms is 1500 and every manifest for a real CLI declares a
/// hook, so a pane whose hooks are not wired spends the whole window
/// waiting for an ACK that never comes. When it closes, the screen has
/// held the evidence since the submit, and the second entry here is
/// the look that reads it.
///
/// That entry is the fix. Without it the timeline ran
/// [(1500, hook), (3000, ...)]: a second and a half in which tier 2
/// had opened and nothing looked, with receipt_block_ms (2500)
/// expiring inside it. A 1.5s gap between the first two entries is
/// exactly that defect coming back.
#[tokio::test(start_paused = true)]
async fn tier2_opens_the_moment_the_hook_window_closes() {
    let t0 = Instant::now();
    assert_eq!(
        timeline(AckClock::new(t0, Some(ms(1500))), t0),
        vec![(1500, true), (1500, false), (3000, false), (5000, false),]
    );
}

/// Queued is a claim about the pane: nothing has been typed into it.
#[test]
fn a_receipt_calls_a_delivery_queued_only_before_the_paste() {
    use DeliveryState::*;
    for s in [Queued, Gating, RetryQueued] {
        assert!(receipt_is_queued(s), "{s:?} is waiting on the recipient");
    }
    for s in [Pasting, Staged, Submitted] {
        assert!(
            !receipt_is_queued(s),
            "{s:?} has the payload in the pane and may not report queued"
        );
    }
    // Resolved states never reach the question.
    for s in [
        DeliveredVerified,
        DeliveredUnverified,
        AttentionRequired,
        ParkedBlockedQuota,
    ] {
        assert!(receipt_resolved(s), "{s:?}");
    }
}

#[test]
fn only_the_position_zero_head_can_recover_a_hold_token() {
    assert_eq!(
        held_by_for_position(None, None, Some("working".into())),
        None
    );
    assert_eq!(
        held_by_for_position(Some(1), None, Some("working".into())),
        None
    );
    assert_eq!(
        held_by_for_position(Some(0), None, Some("working".into())),
        Some("working".into())
    );
    assert_eq!(
        held_by_for_position(Some(0), Some("blocked".into()), Some("working".into())),
        Some("blocked".into())
    );
}

// -----------------------------------------------------------------
// Decline TOCTOU (fix G: modal must still match before the confirm)
// -----------------------------------------------------------------

#[test]
fn modal_match_is_rechecked_by_rule_id() {
    let m = Manifest::parse(
        r#"
[agent]
id = "x"
display_name = "x"

[[rule]]
id = "update_modal"
state = "blocked_modal"
priority = 1300
region = "bottom_non_empty_lines(8)"
contains = ["FAKE-UPDATE-AVAILABLE"]
decline_keys = ["3", "Enter"]
auto_dismiss = true

[[rule]]
id = "other_modal"
state = "blocked_modal"
priority = 1200
region = "bottom_non_empty_lines(8)"
contains = ["OTHER-DIALOG"]
"#,
        std::path::Path::new("x.toml"),
    )
    .unwrap();
    assert!(modal_still_matches(
        &m,
        "t",
        "text\nFAKE-UPDATE-AVAILABLE\nmore",
        "update_modal"
    ));
    // Dialog vanished: never send the confirming key.
    assert!(!modal_still_matches(
        &m,
        "t",
        "plain shell output",
        "update_modal"
    ));
    // A DIFFERENT dialog appeared: the confirm belongs to nobody.
    assert!(!modal_still_matches(
        &m,
        "t",
        "OTHER-DIALOG",
        "update_modal"
    ));
}

// -----------------------------------------------------------------
// Buffer hygiene (fix G: delete-buffer after a failed paste)
// -----------------------------------------------------------------

/// Real tmux on an isolated -L socket: when paste-buffer fails after
/// load-buffer succeeded, the loaded buffer must not linger
/// server-global with the payload in it.
#[tokio::test]
async fn paste_failure_deletes_the_loaded_buffer() {
    if !cyclops_testrig::tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let pid = std::process::id();
    // The rig owns the server: teardown kills it AND unlinks its socket,
    // and runs on unwind, which a kill at the end of the body does not.
    let tmux = cyclops_testrig::TmuxServer::new("dubuf");
    let spool = cyclops_proto::scratch::scratch_dir("cyc-dubuf-spool");
    let cfg = cyclops_tmux::ControlConfig::new_session("dubuf")
        .on_socket(tmux.socket())
        .with_config_file("/dev/null")
        .with_buffer_spool_dir(&spool);
    let (client, _rx) = ControlClient::spawn(cfg).await.expect("tmux spawns");
    let client = Arc::new(client);
    let injector = TmuxInjector {
        client: Arc::clone(&client),
        buffer: format!("cyc-{pid}-t"),
    };
    // %9999 does not exist: load-buffer succeeds, paste-buffer fails.
    injector.spool("secret payload").await.expect("spool");
    let err = injector.commit("%9999", &|| Ok(())).await.unwrap_err();
    assert_eq!(err, InjectFailure::Other("paste_failed".to_string()));
    let buffers = client.command("list-buffers").await.unwrap_or_default();
    assert!(
        buffers.iter().all(|l| !l.contains(&injector.buffer)),
        "buffer lingered after failed paste: {buffers:?}"
    );
    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&spool);
}

#[cfg(test)]
mod chip_proof {
    use super::*;

    fn chip_manifest() -> Manifest {
        Manifest::parse(
            r#"
[agent]
id = "c"
display_name = "c"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 950
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['\x1b\[39m❯\x{a0}']

[injection]
composer_chip_regex = ['^\s*❯\s+\[Pasted text #\d+( \+\d+ lines)?\]\s*$']
composer_chip_regex_esc = ['\x1b\[39m❯\x{a0}\[Pasted text #\d+( \+\d+ lines)?\]']
composer_trailer_regex = ['^\? for shortcuts\s*$']
composer_trailer_regex_esc = ['\? for shortcuts']
composer_trailer_required_prefix = 1
"#,
            std::path::Path::new("c.toml"),
        )
        .unwrap()
    }

    /// The chip is the alternate proof of a whole staged payload, so it
    /// has to be the whole row. Anything else on the row means the row is
    /// not the chip, and the payload around it is unaccounted for.
    #[test]
    fn a_chip_with_text_around_it_is_not_a_chip() {
        let m = chip_manifest();
        let good = "\u{1b}[39m❯\u{a0}[Pasted text #1 +8 lines]\n? for shortcuts";
        assert!(marker_in_composer(&m, good), "the measured row must pass");

        for bad in [
            "\u{1b}[39m❯\u{a0}[Pasted text #1 +8 lines] and then some",
            "\u{1b}[39m❯\u{a0}see [Pasted text #1 +8 lines]",
        ] {
            let screen = format!("{bad}\n? for shortcuts");
            assert!(
                !marker_in_composer(&m, &screen),
                "payload beside the chip must refuse: {bad:?}"
            );
        }

        // And a line typed UNDER the chip is payload nobody accounted
        // for: the chip is still exact, but it is no longer the last
        // thing in the composer.
        let after = "\u{1b}[39m❯\u{a0}[Pasted text #1 +8 lines]\nand then some\n? for shortcuts";
        assert!(
            !marker_in_composer(&m, after),
            "a row under the chip must refuse"
        );
    }

    /// The exact collision that made the old substring test unsafe: a
    /// message whose SUBJECT contains the word verified a paste whose
    /// sentinel had never arrived, and the truncated payload submitted
    /// itself.
    #[test]
    fn a_subject_containing_the_chip_words_never_verifies() {
        let m = chip_manifest();
        for row in [
            "\u{1b}[39m❯\u{a0}[cyclops m-1] FROM: codex  SUBJECT: Pasted text handling",
            "\u{1b}[39m❯\u{a0}[cyclops m-1] FROM: codex  SUBJECT: Pasted",
        ] {
            let screen = format!("{row}\n? for shortcuts");
            assert!(
                !marker_in_composer(&m, &screen),
                "a subject is not a chip: {row:?}"
            );
        }
    }

    /// A chip that scrolled into the transcript is not the composer's.
    #[test]
    fn a_transcript_echo_of_a_chip_never_verifies() {
        let m = chip_manifest();
        let echo = "you: [Pasted text #1 +8 lines]\n\u{1b}[39m❯\u{a0}\n? for shortcuts";
        assert!(!marker_in_composer(&m, echo));
    }

    /// Without an escaped capture the styling half cannot be checked.
    #[test]
    fn a_plain_capture_never_proves_a_chip() {
        let m = chip_manifest();
        assert!(!marker_in_composer(
            &m,
            "❯ [Pasted text #1 +8 lines]\n? for shortcuts"
        ));
    }
}

#[cfg(test)]
mod shipped_chip_proof {
    use super::tests::{sentinel_manifest, MockInjector, CHROME};
    use super::*;
    use cyclops_proto::MessageId;

    fn claude() -> Manifest {
        Manifest::parse(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/claude.toml"
            )),
            std::path::Path::new("claude.toml"),
        )
        .expect("shipped claude manifest parses")
    }

    fn codex() -> Manifest {
        Manifest::parse(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/codex.toml"
            )),
            std::path::Path::new("codex.toml"),
        )
        .expect("shipped codex manifest parses")
    }

    /// Codex paints a blank row between the composer and its model status
    /// row, so a raw-wrapped sentinel's suffix starts with a row the
    /// layout has to describe. It did not, and every raw-wrapped codex
    /// delivery refused: correct fail-closed behaviour, and a whole
    /// vendor lane with no sentinel path.
    ///
    /// The chrome here is verbatim from a real capture. The composer row
    /// is synthetic, so this proves only the declared trailer layout.
    #[test]
    fn a_codex_raw_wrap_verifies_through_its_measured_blank_separator() {
        let real = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/codex_pasted_chip_esc.txt"
        ));
        let mut rows: Vec<String> = real.split('\n').map(str::to_string).collect();
        let chip = rows
            .iter()
            .position(|r| r.contains("[Pasted Content"))
            .expect("the real capture's composer row");
        // A raw wrap puts the payload in the composer instead of a chip,
        // and the sentinel is its last row.
        rows[chip] = "\u{1b}[1m›\u{1b}[0m the last line of the body".to_string();
        rows.insert(chip + 1, sentinel_for("m-9f1"));
        let screen = rows.join("\n");
        assert!(
            sentinel_verified(&codex(), &screen, "m-9f1"),
            "the shipped codex layout still refuses its own chrome:\n{screen}"
        );

        // The blank row is declared, not ignored. A SECOND blank is a row
        // the layout does not describe, which is what a truncated capture
        // looks like, and it still refuses.
        let mut extra = rows.clone();
        extra.insert(chip + 2, String::new());
        assert!(
            !sentinel_verified(&codex(), &extra.join("\n"), "m-9f1"),
            "an undeclared blank row was accepted"
        );
    }

    /// The shipped manifest against real captures, through the production
    /// proof rather than an inline fixture shaped to suit it.
    ///
    /// An inline manifest proves the algorithm; it cannot prove that the
    /// patterns Cyclops actually ships match the screens Claude actually
    /// draws. Those are different claims, and only the second one is
    /// about delivering a message to a real agent.
    #[test]
    fn the_shipped_claude_chip_verifies_and_its_echo_does_not() {
        let m = claude();
        let staged = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_pasted_chip_esc.txt"
        ));
        assert!(
            marker_in_composer(&m, staged),
            "the shipped chip row must prove a staged paste"
        );

        // The prompt-echo capture is the same CLI with no chip on the
        // composer: whatever else is on screen, nothing there is a staged
        // payload, and claiming otherwise would submit on a redraw.
        let echo = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_prompt_echo_esc.txt"
        ));
        assert!(
            !marker_in_composer(&m, echo),
            "an echo with no composer chip must not verify"
        );

        // The plain sibling of that capture is what a manifest without
        // escaped rules would be handed. The chip proof needs the styling
        // half, so this refuses too, and the two fixtures are now both
        // load-bearing rather than one of them sitting unreferenced.
        let echo_plain = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_prompt_echo_plain.txt"
        ));
        assert!(!marker_in_composer(&m, echo_plain));
    }

    /// The halves disagreeing is the case an OR cannot survive: the chip
    /// row renders exactly as the vendor draws it, the escaped composer
    /// clause holds, and the plain one does not. Under "plain OR escaped"
    /// the escaped half alone would carry the proof; under the manifest's
    /// own semantics both are required and this refuses.
    #[test]
    fn a_row_that_satisfies_only_the_escaped_clause_refuses() {
        let m = Manifest::parse(
            r#"
[agent]
id = "d"
display_name = "d"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 950
region = "bottom_non_empty_lines(6)"
line_regex = ['^NEVER-MATCHES-THIS-ROW$']
line_regex_esc = ['\x1b\[39m❯\x{a0}']

[injection]
composer_chip_regex = ['^\s*❯\s+\[Pasted text #\d+\]\s*$']
composer_chip_regex_esc = ['\x1b\[39m❯\x{a0}\[Pasted text #\d+\]']
composer_trailer_regex = ['^\? for shortcuts\s*$']
composer_trailer_regex_esc = ['\? for shortcuts']
composer_trailer_required_prefix = 1
"#,
            std::path::Path::new("d.toml"),
        )
        .unwrap();
        let row = "\u{1b}[39m❯\u{a0}[Pasted text #1]";
        assert!(
            !marker_in_composer(&m, row),
            "one satisfied clause is not the rule the manifest wrote"
        );
    }

    /// Both shipped clauses have to carry the proof, so breaking either
    /// one must make production refuse.
    ///
    /// This is the half of the contract a passing test cannot show on its
    /// own: with an OR, the escaped clause alone kept the proof alive and
    /// the vendor's plain pattern could rot untouched for as long as
    /// nobody looked. Each half is broken here in turn, against the same
    /// real capture, and each break must be fatal.
    #[test]
    fn breaking_either_shipped_clause_refuses_the_chip() {
        let staged = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_pasted_chip_esc.txt"
        ));
        assert!(marker_in_composer(&claude(), staged), "baseline");

        let shipped = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../resources/manifests/claude.toml"
        ));
        for (half, broken) in [
            (
                "plain",
                shipped.replace(
                    "line_regex = ['^\\s*❯\\s+\\S']",
                    "line_regex = ['^NEVER-MATCHES-THIS-ROW$']",
                ),
            ),
            (
                "escaped",
                shipped.replace(
                    "line_regex_esc = ['\\x1b\\[39m❯\\x{a0}[^\\x1b]']",
                    "line_regex_esc = ['NEVER-MATCHES-THIS-ROW']",
                ),
            ),
        ] {
            assert_ne!(broken, shipped, "the {half} clause moved; update this test");
            let m = Manifest::parse(&broken, std::path::Path::new("claude.toml"))
                .expect("broken manifest still parses");
            assert!(
                !marker_in_composer(&m, staged),
                "breaking the shipped {half} clause must refuse the chip"
            );
        }
    }

    #[test]
    fn compact_doorbell_verifies_as_one_exact_row_at_narrow_width() {
        let m = sentinel_manifest();
        let msg_id = MessageId::new("m-0123456789abcdef0123456789abcdef")
            .expect("valid generated message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);
        let screen = format!("\u{1b}[39m❯ {doorbell}\n{CHROME}");

        assert!(2 + doorbell.chars().count() <= 60);
        assert!(
            exact_row_verified(&m, &screen, &doorbell),
            "exact doorbell in composer must verify"
        );
        assert_eq!(
            staged_representation(&m, &screen, StagingTarget::ExactRow(&doorbell)),
            Some(StagedRepresentation::VisibleTarget),
            "target helper must identify visible exact-row staging"
        );
    }

    #[test]
    fn compact_doorbell_verifies_across_claudes_measured_status_widths() {
        let manifest = claude();
        let msg_id = MessageId::new("m-0123456789abcdef0123456789abcdef")
            .expect("valid generated message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);
        let statuses = [
            (60, "\u{1b}[39m  \u{1b}[38;5;174mOpus 5 (1M context)\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m~/projects/agentic_dev/cy…\u{1b}[39m"),
            (80, "\u{1b}[39m  \u{1b}[38;5;174mOpus 5 (1M context)\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m~/projects/agentic_dev/cyclops-worktrees/mess…\u{1b}[39m"),
            (100, "\u{1b}[39m  \u{1b}[38;5;174mOpus 5 (1M context)\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m~/projects/agentic_dev/cyclops-worktrees/messaging-integration\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;72m…\u{1b}[39m"),
            (125, "\u{1b}[39m  \u{1b}[38;5;174mOpus 5 (1M context)\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m~/projects/agentic_dev/cyclops-worktrees/messaging-integration\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;72mCtx: 95%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;181m5h: 93%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;181m7d: …\u{1b}[39m"),
        ];

        for (width, status) in statuses {
            let rule = "─".repeat(width);
            let screen = format!("\u{1b}[39m❯\u{a0}{doorbell}\n\u{1b}[38;5;244m{rule}\n{status}");
            assert!(
                exact_row_verified(&manifest, &screen, &doorbell),
                "the measured {width}-column Claude trailer must preserve exact proof"
            );
            assert_eq!(
                exact_composer_content_from_joined_capture(&manifest, &screen),
                ComposerContentProof::Visible(doorbell.clone()),
                "attention extraction must agree at {width} columns"
            );
        }
    }

    #[test]
    fn tier2_refuses_while_the_exact_doorbell_row_remains() {
        let manifest = sentinel_manifest();
        let msg_id = MessageId::new("m-3f9c2a").unwrap();
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);
        let changed_chrome = format!(
            "\u{1b}[39m❯ {doorbell}\n\u{1b}[90m────────\n\u{1b}[38;5;152mModel y · Ctx: 77%"
        );

        let still_present = staging_target_still_present(
            &manifest,
            &changed_chrome,
            StagingTarget::ExactRow(&doorbell),
        );
        assert!(still_present);
        let confirmed =
            !still_present && tier2_evidence(AckEvidence::Receipt, true, true, false, false);
        assert!(
            !confirmed,
            "changed chrome cannot turn a staged doorbell into a receipt"
        );
    }

    #[test]
    fn shipped_composers_verify_one_line_notifications_across_visual_wraps() {
        let shipped = |id: &str| {
            let body = match id {
                "codex" => include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../resources/manifests/codex.toml"
                )),
                "claude" => include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../resources/manifests/claude.toml"
                )),
                "agy" => include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../resources/manifests/agy.toml"
                )),
                _ => unreachable!("unknown shipped manifest"),
            };
            Manifest::parse(body, std::path::Path::new(id)).expect("shipped manifest parses")
        };
        let attempt =
            NotificationAttemptId::parse("att-01234567-89ab-4def-8123-456789abcdef").unwrap();
        let expected = cyclops_proto::render_doorbell_v4(
            "implementer",
            "Cable-carrier rounding was restored to whole sections. Review the fix and regression tests.",
            attempt,
        );
        let parts = [
            "[cyclops from implementer] Cable-carrier rounding was restored",
            "to whole sections. Review the fix and regression tests. | cyclops",
            "inbox claim m-att_ASNFZ4mrTe-BI0VniavN7w",
        ];
        assert_eq!(parts.join(" "), expected);
        let padded = |part: &str, width: usize| format!("{part:<width$}");

        let captures = [
            (
                shipped("codex"),
                format!(
                    "\x1b[1m›\x1b[0m {}\n  {}\n  {}\n\n\x1b[38;2;246;226;183mgpt-5.6-sol xhigh · ~/work · Workspace",
                    padded(parts[0], 94), padded(parts[1], 94), parts[2]
                ),
            ),
            (
                shipped("claude"),
                format!(
                    "\x1b[39m❯\u{a0}{}\n  {}\n  {}\n\x1b[38;5;244m{}\n\x1b[39m  \x1b[38;5;174mSonnet 5 · low · ~ · Ctx: 95% · 1000K window",
                    padded(parts[0], 94), padded(parts[1], 94), parts[2], "─".repeat(96)
                ),
            ),
            (
                shipped("agy"),
                format!(
                    "\x1b[94m>\x1b[39m {}\n  {}\n  {}\n\x1b[90m{}\n\x1b[38;2;174;198;207mGemini 3.7 Flash · High · /tmp/work · Full · Ctx:",
                    padded(parts[0], 94), padded(parts[1], 96), padded(parts[2], 96), "─".repeat(96)
                ),
            ),
        ];

        for (manifest, capture) in captures {
            assert_eq!(
                exact_staging_proof(
                    &manifest,
                    &capture,
                    StagingTarget::ExactRow(&expected),
                    &expected,
                ),
                Some((true, expected.clone())),
                "{} must preserve exact ownership across its visual composer wraps",
                manifest.agent.id
            );

            let changed = capture.replacen(parts[1], &format!("{} changed", parts[1]), 1);
            assert!(
                exact_staging_proof(
                    &manifest,
                    &changed,
                    StagingTarget::ExactRow(&expected),
                    &expected,
                )
                .is_none(),
                "{} must reject changed wrapped content",
                manifest.agent.id
            );

            for suffix in ["\t", "\u{a0}"] {
                let changed = capture.replacen(parts[1], &format!("{}{suffix}", parts[1]), 1);
                assert!(
                    exact_staging_proof(
                        &manifest,
                        &changed,
                        StagingTarget::ExactRow(&expected),
                        &expected,
                    )
                    .is_none(),
                    "{} must not normalize non-ASCII-space input",
                    manifest.agent.id
                );
            }
        }
    }

    #[test]
    fn shipped_composers_verify_the_observed_sixty_column_notification_wraps() {
        let shipped = |id: &str| {
            let body = match id {
                "claude" => include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../resources/manifests/claude.toml"
                )),
                "agy" => include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../resources/manifests/agy.toml"
                )),
                _ => unreachable!("unknown shipped manifest"),
            };
            Manifest::parse(body, std::path::Path::new(id)).expect("shipped manifest parses")
        };
        let attempt =
            NotificationAttemptId::parse("att-485beb62-3287-47b9-9a5d-5f7303e91e54").unwrap();
        let expected = cyclops_proto::render_doorbell_v4(
            "chatty",
            "Codex is checking that Cyclops messaging works. Please acknowledge receipt when you see this.",
            attempt,
        );
        let parts = [
            "[cyclops from chatty] Codex is checking that Cyclops",
            "messaging works. Please acknowledge receipt when you see",
            "this. | cyclops inbox claim",
            "m-att_SFvrYjKHR7maXV9zA-keVA",
        ];
        assert_eq!(parts.join(" "), expected);
        let padded = |part: &str, width: usize| format!("{part:<width$}");
        let captures = [
            (
                shipped("claude"),
                format!(
                    "\x1b[39m❯\u{a0}{}\n  {}\n  {}\n  {}\n\x1b[38;5;244m{}\n\x1b[39m  \x1b[38;5;174mSonnet 5 · low · ~ · 5h: 100% · 7d: 1% · 1000K window\n  \x1b[38;5;174m⏵⏵ bypass permissions on (shift+tab to cycle)\n                                                       \x1b[38;5;75m\x1b]8;id=test;https://example.invalid/session\x1b\\/rc\x1b]8;;\x1b\\",
                    padded(parts[0], 58),
                    padded(parts[1], 58),
                    padded(parts[2], 58),
                    padded(parts[3], 58),
                    "─".repeat(60),
                ),
            ),
            (
                shipped("agy"),
                format!(
                    "\x1b[94m>\x1b[39m {}\n  {}\n  {}\n  {}\n\x1b[90m{}\n\x1b[38;2;174;198;207mGemini 3.7 Flash · High · ~ · Full · Ctx:",
                    padded(parts[0], 58),
                    padded(parts[1], 60),
                    padded(parts[2], 60),
                    padded(parts[3], 60),
                    "─".repeat(60),
                ),
            ),
        ];

        for (manifest, capture) in captures {
            assert_eq!(
                exact_staging_proof(
                    &manifest,
                    &capture,
                    StagingTarget::ExactRow(&expected),
                    &expected,
                ),
                Some((true, expected.clone())),
                "{} must verify the observed 60-column wrap",
                manifest.agent.id,
            );
        }
    }

    /// AGY 1.1.23 paints a two-column gutter before every application-owned
    /// continuation row. The gutter is terminal chrome, not part of the
    /// original single-line doorbell, so it must not turn an exact staged
    /// notification into an ambiguous human draft.
    #[test]
    fn agy_indented_wrapped_doorbell_reaches_the_submit_gate() {
        let manifest = Manifest::parse(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/agy.toml"
            )),
            std::path::Path::new("agy"),
        )
        .expect("shipped AGY manifest parses");
        let attempt =
            NotificationAttemptId::parse("att-01234567-89ab-4def-8123-456789abcdef").unwrap();
        let expected = cyclops_proto::render_doorbell_v4(
            "implementer",
            "Check the exact wrapped doorbell before submitting it to the recipient.",
            attempt,
        );
        let parts = [
            "[cyclops from implementer] Check the exact wrapped doorbell before",
            "submitting it to the recipient. | cyclops inbox claim",
            "m-att_ASNFZ4mrTe-BI0VniavN7w",
        ];
        assert_eq!(parts.join(" "), expected);
        let capture = format!(
            "\x1b[94m>\x1b[39m {}\n  {}\n  {}\n\x1b[90m{}\n\x1b[38;2;174;198;207mGemini 3.7 Flash · High · ~ · Full · Ctx:",
            parts[0],
            parts[1],
            parts[2],
            "─".repeat(80),
        );

        assert_eq!(
            exact_staging_proof(
                &manifest,
                &capture,
                StagingTarget::ExactRow(&expected),
                &expected,
            ),
            Some((true, expected.clone())),
            "the measured AGY continuation gutter is renderer chrome, not user input",
        );

        let legacy_source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../resources/manifests/agy.toml"
        ))
        .replacen(
            "region = \"bottom_non_empty_lines(8)\"",
            "region = \"bottom_non_empty_lines(5)\"",
            1,
        )
        .replacen(
            "On 1.1.23, a 70-column pane can wrap a staged doorbell across the prompt and\n\
             three unstyled continuation rows. The divider and status chrome below make\n\
             this an eight-row bottom window. The extra rows are needed only to include the\n\
             styled prompt; they do not relax the escaped discriminator.\n\n\
             The empty composer still renders exactly 'ESC[94m>ESC[39m' with no ghost or\n\
             suggestion text. If a future version paints one, this rule fails closed as\n\
             input until that representation is measured.\"\"\"\n\
             evidence = \"MEASURED: active staged doorbell ESC[94m>ESC[39m text and transcript echo ESC[1mESC[34m> text (2026-08-26, agy 1.1.21); a 4-row wrapped active doorbell at 70 columns needs the 8-row window (2026-09-02, agy 1.1.23); empty composer has no ghost text (2026-08-20, agy 1.1.13)\"",
            "On 1.1.23, a narrow pane wrapped a staged doorbell across the prompt and two\n\
             unstyled continuation rows. The divider and status row below make this a\n\
             five-row bottom window. The extra row is needed only to include the styled\n\
             prompt; it does not relax the escaped discriminator.\n\n\
             The empty composer still renders exactly 'ESC[94m>ESC[39m' with no ghost or\n\
             suggestion text. If a future version paints one, this rule fails closed as\n\
             input until that representation is measured.\"\"\"\n\
             evidence = \"MEASURED: active staged doorbell ESC[94m>ESC[39m text and transcript echo ESC[1mESC[34m> text (2026-08-26, agy 1.1.21); a 3-row wrapped active doorbell needs the 5-row window (2026-09-01, agy 1.1.23); empty composer has no ghost text (2026-08-20, agy 1.1.13)\"",
            1,
        )
        .replacen(
            "# followed by one separator space and the exact compact doorbell. AGY 1.1.23\n\
             # paints exactly two ASCII gutter columns before every wrapped continuation\n\
             # row. They are renderer chrome, not message bytes, so the content capture\n\
             # begins after them. The styled trailer still proves the active composer\n\
             # boundary before Cyclops can submit. AGY 1.1.22 may leave the Ctx value empty\n\
             # and paints the model name with truecolor instead of the earlier 256-color\n\
             # palette; both measured shapes remain chrome-only.",
            "# followed by one separator space and the exact compact doorbell. Joined\n\
             # continuation rows carry no trusted chrome here, so preserve every byte and\n\
             # let the exact payload comparison decide. The styled trailer still proves the\n\
             # active composer boundary before Cyclops can submit. AGY 1.1.22 may leave the\n\
             # Ctx value empty and paints the model name with truecolor instead of the\n\
             # earlier 256-color palette; both measured shapes remain chrome-only.",
            1,
        )
        .replacen(
            "composer_continuation_regex = '^  (?P<content>.*)$'",
            "composer_continuation_regex = '^(?P<content>.*)$'",
            1,
        );
        let legacy_manifest = Manifest::parse(&legacy_source, std::path::Path::new("agy.toml"))
            .expect("the previous shipped AGY manifest parses");
        assert_eq!(
            legacy_manifest.source_digest(),
            LEGACY_AGY_PRE_GUTTER_MANIFEST_SHA256,
            "the compatibility case is the exact historical shipped source",
        );
        assert_eq!(
            exact_staging_proof(
                &legacy_manifest,
                &capture,
                StagingTarget::ExactRow(&expected),
                &expected,
            ),
            Some((true, expected.clone())),
            "an unedited old AGY seed still ignores its measured renderer gutter",
        );

        let customized_source = legacy_source.replacen(
            "display_name = \"Antigravity CLI\"",
            "display_name = \"Antigravity CLI (operator-customized)\"",
            1,
        );
        let customized_manifest =
            Manifest::parse(&customized_source, std::path::Path::new("agy.toml"))
                .expect("an operator-customized AGY manifest parses");
        assert_ne!(
            customized_manifest.source_digest(),
            LEGACY_AGY_PRE_GUTTER_MANIFEST_SHA256,
            "a changed manifest must not inherit the historic seed exception",
        );
        assert!(
            exact_staging_proof(
                &customized_manifest,
                &capture,
                StagingTarget::ExactRow(&expected),
                &expected,
            )
            .is_none(),
            "an operator-customized generic continuation rule must retain every captured byte",
        );

        let with_user_space = capture.replacen(
            &format!("\n  {}", parts[1]),
            &format!("\n    {}", parts[1]),
            1,
        );
        assert!(
            exact_staging_proof(
                &legacy_manifest,
                &with_user_space,
                StagingTarget::ExactRow(&expected),
                &expected,
            )
            .is_none(),
            "legacy compatibility must not strip a third leading input space",
        );
        assert!(
            exact_staging_proof(
                &manifest,
                &with_user_space,
                StagingTarget::ExactRow(&expected),
                &expected,
            )
            .is_none(),
            "the current extractor keeps deliberate spaces after its two-cell gutter",
        );

        let changed = capture.replacen(parts[1], "changed continuation", 1);
        assert!(
            exact_staging_proof(
                &manifest,
                &changed,
                StagingTarget::ExactRow(&expected),
                &expected,
            )
            .is_none(),
            "normalizing the gutter must not normalize changed message content",
        );
    }

    #[test]
    fn collapsed_chip_is_representation_evidence_but_never_exact_ownership() {
        let m = claude();
        let staged = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_pasted_chip_esc.txt"
        ));
        let msg_id = MessageId::new("m-3f9c2a").expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);

        assert_eq!(
            staged_representation(&m, staged, StagingTarget::ExactRow(&doorbell)),
            Some(StagedRepresentation::CollapsedChip),
            "the manifest still recognizes the vendor's staged representation"
        );
        assert_eq!(
            exact_composer_content_from_joined_capture(&m, staged),
            ComposerContentProof::Hidden
        );
        assert!(
            exact_staging_proof(&m, staged, StagingTarget::ExactRow(&doorbell), &doorbell)
                .is_none(),
            "hidden bytes cannot authorize a submit key"
        );
    }

    #[tokio::test]
    async fn collapsed_chip_stops_the_delivery_before_submit() {
        let manifest = claude();
        let staged = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_pasted_chip_esc.txt"
        ));
        let handle = DeliveryHandle::new("m-chip-no-submit", "claude", "%1", 0, String::new());
        let message_id = MessageId::new(&handle.msg_id).expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&message_id);
        let injector = MockInjector::new(vec![staged]);
        injector.spool(&doorbell).await.expect("spool");

        let result = inject(
            &injector,
            &handle,
            &manifest,
            StagingTarget::ExactRow(&doorbell),
            &doorbell,
            &|| Ok(()),
        )
        .await;

        assert_eq!(
            result.unwrap_err(),
            InjectFailure::Other("verify_failed".to_string())
        );
        assert_eq!(injector.pasted.lock().unwrap().as_slice(), &[doorbell]);
        assert!(
            injector.submitted_is_empty(),
            "representation evidence must not reach the submit key"
        );
    }

    #[test]
    fn human_draft_in_composer_refuses_verification() {
        let m = sentinel_manifest();
        let msg_id = MessageId::new("m-3f9c2a").expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);

        // Case 1: Human typed draft before the doorbell row on the same line
        let screen_before = format!("\u{1b}[39m❯ draft prefix text {doorbell}\n{CHROME}");
        assert!(
            !exact_row_verified(&m, &screen_before, &doorbell),
            "draft text before doorbell must refuse"
        );

        // Case 2: Human typed draft after the doorbell row on the same line
        let screen_after = format!("\u{1b}[39m❯ {doorbell} draft suffix text\n{CHROME}");
        assert!(
            !exact_row_verified(&m, &screen_after, &doorbell),
            "draft text after doorbell must refuse"
        );

        // Case 3: Human draft on a row between doorbell and chrome
        let screen_multi = format!("\u{1b}[39m❯ {doorbell}\nhuman second line\n{CHROME}");
        assert!(
            !exact_row_verified(&m, &screen_multi, &doorbell),
            "multiline human draft below doorbell must refuse"
        );

        // Case 4: Separate human draft row before the doorbell row and chrome after it (adversarial capture)
        let screen_draft_above = format!(
            "\u{1b}[39m❯ my unfinished thought\n\
             {doorbell}\n\
             {CHROME}"
        );
        assert!(
            !exact_row_verified(&m, &screen_draft_above, &doorbell),
            "separate human draft row before doorbell must refuse"
        );
        assert_eq!(
            staged_representation(&m, &screen_draft_above, StagingTarget::ExactRow(&doorbell)),
            None,
            "adversarial draft above doorbell must return None"
        );

        // Case 5: Prompt on draft row above and prompt on doorbell row below
        let screen_two_prompts = format!(
            "\u{1b}[39m❯ my unfinished thought\n\
             \u{1b}[39m❯ {doorbell}\n\
             {CHROME}"
        );
        assert!(
            !exact_row_verified(&m, &screen_two_prompts, &doorbell),
            "two prompt rows in composer must refuse"
        );
    }

    #[test]
    fn exact_composer_diff_extracts_only_the_active_prompt() {
        let manifest = sentinel_manifest();
        let message_id = MessageId::new("m-3f9c2a").unwrap();
        let doorbell = cyclops_proto::render_doorbell_v1(&message_id);
        let screen = format!(
            "\u{1b}[1;2m❯ old transcript prompt\u{1b}[0m\n\
             \u{1b}[39m❯ {doorbell} trailing human input\n{CHROME}"
        );

        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, &screen),
            ComposerContentProof::Visible(format!("{doorbell} trailing human input"))
        );
    }

    #[test]
    fn exact_composer_diff_refuses_two_active_prompts() {
        let manifest = sentinel_manifest();
        let screen = format!(
            "\u{1b}[39m❯ first staged row\n\
             \u{1b}[39m❯ second staged row\n{CHROME}"
        );

        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, &screen),
            ComposerContentProof::Unprovable
        );
    }

    #[test]
    fn modal_dialog_blocking_composer_refuses_verification() {
        let m = sentinel_manifest();
        let msg_id = MessageId::new("m-3f9c2a").expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);

        // Screen where a modal dialog is present instead of the composer trailer chrome
        let screen_modal = format!(
            "\u{1b}[39m❯ {doorbell}\n\
             \u{1b}[31m[Modal: Do you trust this folder? (y/n)]\u{1b}[0m\n\
             \u{1b}[90m[Press Enter to confirm, Esc to cancel]\u{1b}[0m"
        );
        assert!(
            !exact_row_verified(&m, &screen_modal, &doorbell),
            "modal dialog blocking composer must refuse staging verification"
        );
        assert_eq!(
            staged_representation(&m, &screen_modal, StagingTarget::ExactRow(&doorbell)),
            None
        );
    }

    #[tokio::test]
    async fn inject_verifies_exact_row_target() {
        let m = sentinel_manifest();
        let handle = DeliveryHandle::new("m-exact01", "worker", "%1", 0, "payload".into());
        let msg_id = MessageId::new(&handle.msg_id).expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);
        let screen = format!("\u{1b}[39m❯ {doorbell}\n{CHROME}");
        let mock = MockInjector::new(vec![screen.as_str()]);
        mock.spool(&doorbell).await.expect("spool");
        let (window, id_staged, proof) = inject(
            &mock,
            &handle,
            &m,
            StagingTarget::ExactRow(&doorbell),
            &doorbell,
            &|| Ok(()),
        )
        .await
        .expect("exact row inject must succeed");
        assert!(id_staged);
        assert!(proof.contains(&doorbell));
        assert!(window.contains(&doorbell));
    }

    #[test]
    fn exact_row_changed_before_submit_recheck_withholds_enter() {
        let m = sentinel_manifest();
        let msg_id = MessageId::new("m-3f9c2a").expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);

        let initial_screen = format!("\u{1b}[39m❯ {doorbell}\n{CHROME}");
        let initial = exact_staging_proof(
            &m,
            &initial_screen,
            StagingTarget::ExactRow(&doorbell),
            &doorbell,
        )
        .expect("initial exact proof");

        // Case 1: Human typed additional text after the doorbell before Enter is sent
        let changed_after = format!("\u{1b}[39m❯ {doorbell} append edit\n{CHROME}");
        let recheck = exact_staging_proof(
            &m,
            &changed_after,
            StagingTarget::ExactRow(&doorbell),
            &doorbell,
        );
        let would_submit = recheck.as_ref() == Some(&initial);
        assert!(
            !would_submit,
            "recheck must detect changed text after doorbell and withhold enter"
        );

        // Case 2: Human inserted a draft row above the doorbell before Enter is sent
        let changed_above = format!("\u{1b}[39m❯ draft line\n{doorbell}\n{CHROME}");
        let recheck_above = exact_staging_proof(
            &m,
            &changed_above,
            StagingTarget::ExactRow(&doorbell),
            &doorbell,
        );
        let would_submit_above = recheck_above.as_ref() == Some(&initial);
        assert!(
            !would_submit_above,
            "recheck must detect draft row above doorbell and withhold enter"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn exact_staging_recheck_waits_out_a_partial_repaint_but_not_a_draft() {
        let manifest = sentinel_manifest();
        let message_id = MessageId::new("m-3f9c2a").expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&message_id);
        let exact = format!("\u{1b}[39m❯ {doorbell}\n{CHROME}");
        let (id_staged, payload_at_proof) = exact_staging_proof(
            &manifest,
            &exact,
            StagingTarget::ExactRow(&doorbell),
            &doorbell,
        )
        .expect("baseline exact proof");

        // A terminal repaint can expose only the cleared prompt for one
        // capture. A later complete frame with the same exact bytes is safe
        // to use; this helper only reads and never widens the proof.
        let partial_repaint = "\u{1b}[39m❯\n";
        let repainting = MockInjector::new(vec![partial_repaint, exact.as_str()]);
        assert_eq!(
            recheck_exact_staging_snapshot(
                &repainting,
                "%1",
                &manifest,
                StagingTarget::ExactRow(&doorbell),
                &doorbell,
                id_staged,
                &payload_at_proof,
            )
            .await,
            Ok(exact.clone()),
            "a partial redraw must not manufacture verify_failed after an exact paste proof"
        );

        // A stable human edit never becomes the earlier exact doorbell, so
        // the bounded re-read still refuses to send Enter.
        let human_draft = format!("\u{1b}[39m❯ {doorbell} human edit\n{CHROME}");
        let edited = MockInjector::new(vec![human_draft.as_str(); VERIFY_DELAYS_MS.len()]);
        assert_eq!(
            recheck_exact_staging_snapshot(
                &edited,
                "%1",
                &manifest,
                StagingTarget::ExactRow(&doorbell),
                &doorbell,
                id_staged,
                &payload_at_proof,
            )
            .await,
            Err(ExactStagingRecheck::Mismatch),
            "a durable human edit must still withhold Enter"
        );
    }

    #[test]
    fn submit_binding_rejects_reused_pid_generations() {
        let proven = fusion::Binding {
            pane_root: crate::identity::ProcId { pid: 39, birth: 79 },
            leader: crate::identity::ProcId { pid: 40, birth: 80 },
            agent: crate::identity::ProcId { pid: 41, birth: 81 },
            manifest: "claude".to_string(),
        };
        assert!(binding_is_exact(Some(&proven), &proven));

        let reused_pane_root = fusion::Binding {
            pane_root: crate::identity::ProcId { pid: 39, birth: 80 },
            ..proven.clone()
        };
        assert!(!binding_is_exact(Some(&reused_pane_root), &proven));

        let reused_leader = fusion::Binding {
            leader: crate::identity::ProcId { pid: 40, birth: 82 },
            ..proven.clone()
        };
        assert!(!binding_is_exact(Some(&reused_leader), &proven));

        let replaced_agent = fusion::Binding {
            agent: crate::identity::ProcId { pid: 41, birth: 83 },
            ..proven.clone()
        };
        assert!(!binding_is_exact(Some(&replaced_agent), &proven));

        let replaced_manifest = fusion::Binding {
            manifest: "codex".to_string(),
            ..proven.clone()
        };
        assert!(!binding_is_exact(Some(&replaced_manifest), &proven));
        assert!(!binding_is_exact(None, &proven));
    }
}

#[cfg(test)]
mod composer_content_proof {
    use super::*;

    const PROBE_BODY: &str = "This is a deliberately long composer-only probe that wraps across physical terminal rows without being submitted to any model, and it contains punctuation: [] {} <> ! ? plus Unicode λ 漢字.";
    const CLAUDE_TRAILER: &str = "\u{1b}[38;5;244m────────────────────────────────────────────────────────────────────────────────\n\u{1b}[39m  \u{1b}[38;5;174mOpus 5 (1M context)\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m/tmp/project\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;181m5h: 92%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;181m7d: 36%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;180m1000K window\u{1b}[39m";

    fn decoded_fixture(hex: &str) -> String {
        let compact: String = hex
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        assert_eq!(compact.len() % 2, 0, "fixture has a partial byte");
        let bytes = compact
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let pair = std::str::from_utf8(pair).expect("hex is ASCII");
                u8::from_str_radix(pair, 16).expect("fixture contains hex")
            })
            .collect();
        String::from_utf8(bytes).expect("capture is UTF-8")
    }

    fn shipped(id: &str) -> Manifest {
        let source = match id {
            "claude" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/claude.toml"
            )),
            "codex" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/codex.toml"
            )),
            "agy" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/agy.toml"
            )),
            "cursor" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/cursor.toml"
            )),
            _ => panic!("unknown shipped manifest {id}"),
        };
        Manifest::parse(source, std::path::Path::new(id)).expect("shipped manifest parses")
    }

    fn claude_capture(payload: &str) -> String {
        let mut rows = payload.lines();
        let first = rows.next().expect("payload has an envelope");
        let mut screen = format!("\u{1b}[39m❯\u{a0}{first}");
        for row in rows {
            screen.push_str("\n  ");
            screen.push_str(row);
        }
        screen.push('\n');
        screen.push_str(CLAUDE_TRAILER);
        screen
    }

    #[test]
    fn current_raw_captures_extract_the_rebuilt_payload() {
        let expected = render_payload("m-wrapprobe", "test", "exact wrap probe", PROBE_BODY, true);
        for (vendor, capture) in [
            (
                "claude",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../cyclops-manifest/tests/fixtures/claude_raw_composer_2_1_239_esc.txt"
                ))
                .to_string(),
            ),
            (
                "codex",
                decoded_fixture(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../cyclops-manifest/tests/fixtures/codex_raw_composer_0_149_0_esc.hex"
                ))),
            ),
        ] {
            assert_eq!(
                composer_content_from_joined_capture(&shipped(vendor), &capture, "m-wrapprobe"),
                ComposerContentProof::Visible(expected.clone()),
                "{vendor} did not reconstruct the rendered payload"
            );
        }
    }

    /// AGY 1.1.21 keeps a compact doorbell visible as one prompt row. The
    /// styled rule and status rows bind that row to the active composer.
    #[test]
    fn agy_1_1_21_exact_doorbell_reaches_the_submit_gate() {
        let manifest = shipped("agy");
        let message_id = MessageId::new("m-0123456789abcdef0123456789abcdef")
            .expect("valid generated message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&message_id);
        let capture = format!(
            "\u{1b}[1m\u{1b}[34m> an earlier submitted prompt\u{1b}[0m\n\
             \u{1b}[94m>\u{1b}[39m {doorbell}\n\
             \u{1b}[90m────────────────────────────────────────────────────────────────────────────────\n\
             \u{1b}[38;5;152mGemini 3.7 Flash\u{1b}[38;5;251m · \u{1b}[38;5;217mHigh\u{1b}[38;5;251m · \u{1b}[38;5;151m~\u{1b}[38;5;251m · \u{1b}[38;5;182mFull\u{1b}[38;5;251m · \u{1b}[38;5;151mCtx: 100%\u{1b}[38;5;251m · 44% 5h, 74% wk · \u{1b}[38;5;215m(0K / 1048K)"
        );

        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, &capture),
            ComposerContentProof::Visible(doorbell.clone())
        );
        assert_eq!(
            exact_staging_proof(
                &manifest,
                &capture,
                StagingTarget::ExactRow(&doorbell),
                &doorbell,
            ),
            Some((true, doorbell))
        );
    }

    /// MEASURED 2026-08-28 on AGY 1.1.22. The model status row changed from
    /// a 256-color prefix to truecolor and may leave the context value empty.
    /// Both rows are still vendor chrome, so an exact format 3 doorbell must
    /// remain provable without accepting any additional composer content.
    #[test]
    fn agy_1_1_22_truecolor_empty_ctx_format_3_reaches_the_submit_gate() {
        let manifest = shipped("agy");
        let attempt_id =
            NotificationAttemptId::parse("att-01234567-89ab-4def-8123-456789abcdef").unwrap();
        let doorbell = cyclops_proto::render_doorbell_v3(attempt_id);
        let capture = format!(
            "\u{1b}[94m>\u{1b}[39m {doorbell}\n\
             \u{1b}[90m───────────────────────────────────────────────────────────────────────────────\n\
             \u{1b}[38;2;174;198;207mGemini 3.7 Flash\u{1b}[38;2;200;200;200m · \u{1b}[38;2;255;179;186mHigh\u{1b}[38;2;200;200;200m · \u{1b}[38;2;168;230;163m/tmp/release\u{1b}[38;2;200;200;200m · \u{1b}[38;2;203;170;203mFull\u{1b}[38;2;200;200;200m · \u{1b}[38;2;168;230;163mCtx:"
        );

        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, &capture),
            ComposerContentProof::Visible(doorbell.clone())
        );
        assert_eq!(
            exact_staging_proof(
                &manifest,
                &capture,
                StagingTarget::ExactRow(&doorbell),
                &doorbell,
            ),
            Some((true, doorbell))
        );
    }

    /// MEASURED 2026-08-28 on Claude Code 2.1.251. The `/rc` shortcut is
    /// right-aligned after the ordinary model status fields. It remains
    /// vendor chrome and must not make an exact Format 3 doorbell ambiguous.
    #[test]
    fn claude_2_1_251_rc_status_format_3_reaches_the_submit_gate() {
        let manifest = shipped("claude");
        let attempt_id =
            NotificationAttemptId::parse("att-01234567-89ab-4def-8123-456789abcdef").unwrap();
        let doorbell = cyclops_proto::render_doorbell_v3(attempt_id);
        let capture = format!(
            "\u{1b}[39m❯\u{a0}{doorbell}\n\
             \u{1b}[38;5;244m───────────────────────────────────────────────────────────────────────────────\n\
             \u{1b}[39m  \u{1b}[38;5;174mSonnet 5\u{1b}[38;5;246m · \u{1b}[38;5;216mlow\u{1b}[38;5;246m · ~ · \u{1b}[38;5;72mCtx: 95%\u{1b}[38;5;246m · 5h: 100% · 7d: 1% · \u{1b}[38;5;180m1000K window\u{1b}[38;5;246m · \u{1b}[38;5;180m52K used            /rc"
        );

        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, &capture),
            ComposerContentProof::Visible(doorbell.clone())
        );
        assert_eq!(
            exact_staging_proof(
                &manifest,
                &capture,
                StagingTarget::ExactRow(&doorbell),
                &doorbell,
            ),
            Some((true, doorbell))
        );
    }

    #[test]
    fn current_raw_claude_capture_is_safe_to_submit() {
        let capture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_raw_composer_2_1_239_esc.txt"
        ));
        assert!(sentinel_verified(
            &shipped("claude"),
            capture,
            "m-wrapprobe"
        ));
    }

    #[test]
    fn claude_current_fable_empty_composer_is_visible_across_box_palette() {
        let capture = concat!(
            "\u{1b}[38;5;244m────────────────────────────────────────────────\n",
            "\u{1b}[39m❯\u{a0}                                                \n",
            "\u{1b}[38;5;244m────────────────────────────────────────────────\n",
            "\u{1b}[39m  \u{1b}[38;5;174mFable 5\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m~/projects/agentic_dev/clops\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;72mCtx: 58%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;181m5h: 94%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;181m7d: 75%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;180m1000K window\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;180m423K used\n",
            "\u{1b}[39m  \u{1b}[38;5;210m⏵⏵ bypass permissions on\u{1b}[38;5;246m (shift+tab to cycle) · ← 1 agent",
        );

        for capture in [
            capture.to_string(),
            capture.replace("\u{1b}[38;5;244m─", "\u{1b}[38;5;116m─"),
        ] {
            assert_eq!(
                composer_content_for_projection_from_joined_capture(&shipped("claude"), &capture),
                ComposerContentProof::Visible(String::new())
            );

            let unexpected = format!("{capture}\nunexpected text");
            assert_eq!(
                composer_content_for_projection_from_joined_capture(
                    &shipped("claude"),
                    &unexpected,
                ),
                ComposerContentProof::Unprovable
            );
        }
    }

    #[test]
    fn claude_2_1_243_no_color_visible_sentinel_uses_structural_proof() {
        let capture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_staged_no_color_2_1_243.txt"
        ));
        let manifest = shipped("claude");
        assert!(
            !capture.contains('\u{1b}'),
            "fixture unexpectedly contains SGR"
        );
        assert_eq!(
            staged_representation(&manifest, capture, StagingTarget::Sentinel("m-no-color")),
            Some(StagedRepresentation::VisibleTarget)
        );

        let doorbell_id = MessageId::new("m-no-color").expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&doorbell_id);
        let doorbell_capture = format!(
            "❯\u{a0}{doorbell}\n\
             ────────────────────────────────────────────────────────────────\n\
               Haiku 4.5 · low · ~ · Ctx: 76% · 200K window · 47K used\n\
               ⏵⏵ bypass permissions on (shift+tab to cycle)"
        );
        assert_eq!(
            staged_representation(
                &manifest,
                &doorbell_capture,
                StagingTarget::ExactRow(&doorbell)
            ),
            Some(StagedRepresentation::VisibleTarget)
        );
        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, &doorbell_capture),
            ComposerContentProof::Visible(doorbell.clone())
        );

        let doorbell_after_echo = format!(
            "❯\u{a0}previous submitted prompt\n\
             prior answer\n\
             {doorbell_capture}"
        );
        assert_eq!(
            staged_representation(
                &manifest,
                &doorbell_after_echo,
                StagingTarget::ExactRow(&doorbell)
            ),
            Some(StagedRepresentation::VisibleTarget)
        );
        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, &doorbell_after_echo),
            ComposerContentProof::Visible(doorbell)
        );

        for invalid in [
            capture.replace(
                "  [cyclops:end m-no-color]\n",
                "  [cyclops:end m-no-color]\n  typed after the sentinel\n",
            ),
            capture.replace(
                "  [cyclops:end m-no-color]",
                "  [cyclops:end m-no-color] trailing",
            ),
            capture.replace(
                "  [cyclops:end m-no-color]\n",
                "  [cyclops:end m-no-color]\n  [cyclops:end m-no-color]\n",
            ),
            capture.replace(
                "  [cyclops:end m-no-color]\n",
                concat!(
                    "  [cyclops:end m-no-color]\n",
                    "────────────────────────────────────────────────────────────────\n",
                    "❯\u{a0}\n",
                ),
            ),
        ] {
            assert!(!sentinel_verified(&manifest, &invalid, "m-no-color"));
        }
    }

    #[test]
    fn codex_0_149_1_doorbell_has_exact_visible_ownership() {
        let capture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/codex_staged_0_149_1_esc.txt"
        ));
        let manifest = shipped("codex");
        let doorbell =
            "cyclops inbox claim m-4c0cdcbf9cb04cf983ef2c6aa206eac9 #c:xvXB2rLoTC2SpbRj5fnDFA";

        assert_eq!(
            staged_representation(&manifest, capture, StagingTarget::ExactRow(doorbell)),
            Some(StagedRepresentation::VisibleTarget)
        );
        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, capture),
            ComposerContentProof::Visible(doorbell.to_string())
        );

        for ambiguous in [
            capture.replace(doorbell, &format!("human draft {doorbell}")),
            capture.replace(doorbell, &format!("{doorbell} unexpected")),
        ] {
            let ComposerContentProof::Visible(content) =
                exact_composer_content_from_joined_capture(&manifest, &ambiguous)
            else {
                panic!("the occupied composer must remain visible as human input")
            };
            assert_ne!(content, doorbell);
            assert!(exact_staging_proof(
                &manifest,
                &ambiguous,
                StagingTarget::ExactRow(doorbell),
                doorbell
            )
            .is_none());
        }
    }

    #[test]
    fn codex_0_149_1_fast_status_keeps_exact_visible_ownership() {
        let doorbell = "cyclops inbox claim m-att_LMRvMkzHQzixuOJNYwT8Qw";
        let capture = concat!(
            "\x1b[48;2;30;30;30m\n",
            "\x1b[1m›\x1b[0m\x1b[48;2;30;30;30m ",
            "cyclops inbox claim m-att_LMRvMkzHQzixuOJNYwT8Qw\n",
            "\n",
            "\x1b[49m  \x1b[38;2;246;226;183mgpt-5.6-sol high fast",
            "\x1b[2m\x1b[39m · \x1b[0m",
            "\x1b[38;2;171;223;167m/private/tmp/cyclops-release-final",
            "\x1b[2m\x1b[39m · \x1b[0m",
            "\x1b[38;2;200;169;238mWorkspace\x1b[39m\n",
        );
        let manifest = shipped("codex");

        assert_eq!(
            exact_staging_proof(
                &manifest,
                capture,
                StagingTarget::ExactRow(doorbell),
                doorbell,
            ),
            Some((true, doorbell.to_string()))
        );
    }

    /// Derived from the measured 0.149.1 prompt and trailer rows. The live
    /// 187-column capture remains release evidence; this minimized fixture
    /// proves format 3 stays exact at the supported 80-column layout.
    #[test]
    fn codex_0_149_1_format_3_is_exact_in_a_derived_80_column_capture() {
        let capture = decoded_fixture(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/codex_doorbell_v3_derived_80_esc.hex"
        )));
        let manifest = shipped("codex");
        let attempt_id =
            NotificationAttemptId::parse("att-c6f5c1da-b2e8-4c2d-92a5-b463e5f9c314").unwrap();
        let doorbell = cyclops_proto::render_doorbell_v3(attempt_id);

        for row in cyclops_manifest::strip_csi(&capture).lines() {
            assert!(row.chars().count() <= 80, "derived row exceeds 80 columns");
        }
        assert_eq!(
            exact_staging_proof(
                &manifest,
                &capture,
                StagingTarget::ExactRow(&doorbell),
                &doorbell,
            ),
            Some((true, doorbell))
        );
    }

    #[test]
    fn codex_0_149_1_no_color_doorbell_uses_structural_trailer() {
        let capture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/codex_staged_no_color_0_149_1.txt"
        ));
        let manifest = shipped("codex");
        let doorbell =
            "cyclops inbox claim m-cfb2ad82c11a484cb617733220308231 #c:RN31a7Y4SsKK6gxgZ0xUKg";

        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, capture),
            ComposerContentProof::Visible(doorbell.to_string())
        );
        assert_eq!(
            exact_staging_proof(
                &manifest,
                capture,
                StagingTarget::ExactRow(doorbell),
                doorbell
            ),
            Some((true, doorbell.to_string()))
        );

        let followed = format!("{capture}\nunexpected text");
        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, &followed),
            ComposerContentProof::Unprovable
        );
        assert!(exact_staging_proof(
            &manifest,
            &followed,
            StagingTarget::ExactRow(doorbell),
            doorbell
        )
        .is_none());

        let prompt_row = capture.lines().next().unwrap();
        let status_row = capture.lines().last().unwrap();
        let empty_prompt = "\u{1b}[1m›\u{1b}[0m ";
        let invalid = [
            capture.replace(doorbell, &format!("human draft {doorbell}")),
            capture.replace(doorbell, &format!("{doorbell} unexpected")),
            capture.replacen("\n\n", "\n  unexpected continuation\n\n", 1),
            format!("{prompt_row}\n{capture}"),
            format!("{prompt_row}\nprior answer\n{empty_prompt}\n\n{status_row}"),
            capture.replace(status_row, "Allow command? [y/N]"),
        ];
        for screen in invalid {
            assert!(
                exact_staging_proof(
                    &manifest,
                    &screen,
                    StagingTarget::ExactRow(doorbell),
                    doorbell
                )
                .is_none(),
                "ambiguous no-color composer content must fail closed"
            );
        }
    }

    #[test]
    fn no_color_trailer_does_not_hide_content_after_the_sentinel() {
        let capture = concat!(
            "❯\u{a0}[cyclops m-no-color] FROM: cyclopsd  SUBJECT: hook self-test\n",
            "  [cyclops:end m-no-color]\n",
            "  typed after the sentinel\n",
            "────────────────────────────────────────────────────────────────\n",
            "  Sonnet 5 · xhigh · ~ · 5h: 98% · 7d: 91% · 1000K window\n",
        );
        assert!(!sentinel_verified(
            &shipped("claude"),
            capture,
            "m-no-color"
        ));

        let forged_boundary = concat!(
            "❯\u{a0}[cyclops m-no-color] FROM: cyclopsd  SUBJECT: hook self-test\n",
            "  [cyclops:end m-no-color]\n",
            "  ──────────────────────────────────────────────────────────────\n",
            "  Sonnet 5 · xhigh · ~ · 5h: 98% · 7d: 91% · 1000K window\n",
        );
        assert!(!sentinel_verified(
            &shipped("claude"),
            forged_boundary,
            "m-no-color"
        ));
    }

    #[test]
    fn joined_capture_preserves_payload_trailing_spaces() {
        let expected = render_payload("m-space", "test", "spaces", "body", true);
        let edited_capture = claude_capture(&expected).replace(
            "  body\n  [cyclops:end m-space]",
            "  body \n  [cyclops:end m-space]",
        );
        let edited_payload = expected.replace("body\n", "body \n");
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &edited_capture, "m-space"),
            ComposerContentProof::Visible(edited_payload)
        );

        let edited_sentinel = claude_capture(&expected)
            .replace("  [cyclops:end m-space]\n", "  [cyclops:end m-space] \n");
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &edited_sentinel, "m-space"),
            ComposerContentProof::Unprovable
        );
    }

    #[test]
    fn direct_staging_requires_the_reconstructed_payload_bytes() {
        let manifest = shipped("claude");
        let expected = render_payload("m-exact", "test", "subject", "body", true);
        let exact = claude_capture(&expected);
        assert_eq!(
            exact_staging_proof(
                &manifest,
                &exact,
                StagingTarget::Sentinel("m-exact"),
                &expected,
            ),
            Some((true, expected.clone()))
        );

        let prefixed = exact.replace(
            "  body\n  [cyclops:end m-exact]",
            "  human draft\n  body\n  [cyclops:end m-exact]",
        );
        assert!(exact_staging_proof(
            &manifest,
            &prefixed,
            StagingTarget::Sentinel("m-exact"),
            &expected,
        )
        .is_none());

        let trailing = exact.replace("  [cyclops:end m-exact]\n", "  [cyclops:end m-exact] \n");
        assert!(exact_staging_proof(
            &manifest,
            &trailing,
            StagingTarget::Sentinel("m-exact"),
            &expected,
        )
        .is_none());
    }

    #[test]
    fn codex_hex_fixtures_preserve_measured_trailing_cells() {
        let raw = decoded_fixture(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/codex_raw_composer_0_149_0_esc.hex"
        )));
        let collapsed = decoded_fixture(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/codex_collapsed_chip_0_149_0_esc.hex"
        )));
        let raw_rows: Vec<&str> = raw.lines().collect();
        let collapsed_rows: Vec<&str> = collapsed.lines().collect();
        assert!(raw_rows[0].ends_with(' '));
        assert_eq!(raw_rows[4], " ");
        assert!(collapsed_rows[0].ends_with(' '));
        assert!(collapsed_rows[2].ends_with(' '));
    }

    #[test]
    fn prompt_may_be_outside_the_sentinel_search_window() {
        let body = (0..24)
            .map(|line| format!("body line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let expected = render_payload("m-long", "test", "long", &body, true);
        let capture = claude_capture(&expected);
        let prompt_at = composer_rows(&capture)
            .iter()
            .position(|(_, plain)| plain.starts_with("❯"))
            .expect("prompt row");
        assert!(
            composer_rows(&capture).len() - prompt_at > VERIFY_REGION,
            "fixture did not put the prompt outside the search window"
        );
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &capture, "m-long"),
            ComposerContentProof::Visible(expected)
        );
    }

    #[test]
    fn terminal_composer_wins_over_a_prior_transcript_echo() {
        let expected = render_payload("m-echo", "test", "current", "new body", true);
        let active = claude_capture(&expected);
        let echoed = format!(
            "\u{1b}[38;5;239m\u{1b}[48;5;237m❯ \u{1b}[38;5;231m[cyclops m-echo] FROM: test  SUBJECT: old\u{1b}[39m\n  old body\n  [cyclops:end m-echo]\nassistant response\n{active}"
        );
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &echoed, "m-echo"),
            ComposerContentProof::Visible(expected),
            "the unique terminal trailer must bind the active composer"
        );

        let empty_composer = format!(
            "\u{1b}[38;5;239m\u{1b}[48;5;237m❯ \u{1b}[38;5;231m[cyclops m-echo] FROM: test  SUBJECT: old\u{1b}[39m\n  old body\n  [cyclops:end m-echo]\nassistant response\n\u{1b}[39m❯\u{a0}\n{CLAUDE_TRAILER}"
        );
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &empty_composer, "m-echo"),
            ComposerContentProof::Unprovable
        );
    }

    #[test]
    fn repeated_sentinel_and_trailing_content_refuse() {
        let repeated = render_payload(
            "m-repeat",
            "test",
            "quoted sentinel",
            "before\n[cyclops:end m-repeat]\nafter",
            true,
        );
        assert_eq!(
            composer_content_from_joined_capture(
                &shipped("claude"),
                &claude_capture(&repeated),
                "m-repeat"
            ),
            ComposerContentProof::Unprovable
        );

        let expected = render_payload("m-trail", "test", "trailing", "body", true);
        let capture = claude_capture(&expected).replace(
            "  [cyclops:end m-trail]\n\u{1b}[38;5;244m",
            "  [cyclops:end m-trail]\n  human addition\n\u{1b}[38;5;244m",
        );
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &capture, "m-trail"),
            ComposerContentProof::Unprovable
        );

        let distant_body = std::iter::once("[cyclops:end m-distant]".to_string())
            .chain((0..20).map(|line| format!("body line {line:02}")))
            .collect::<Vec<_>>()
            .join("\n");
        let distant = render_payload(
            "m-distant",
            "test",
            "distant duplicate",
            &distant_body,
            true,
        );
        let distant_capture = claude_capture(&distant);
        let rows = joined_composer_rows(&distant_capture);
        let first_duplicate = rows
            .iter()
            .position(|(_, plain)| plain == "  [cyclops:end m-distant]")
            .expect("body sentinel");
        assert!(
            first_duplicate < rows.len().saturating_sub(VERIFY_REGION),
            "duplicate remained inside the bounded search window"
        );
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &distant_capture, "m-distant"),
            ComposerContentProof::Unprovable
        );
    }

    #[test]
    fn payload_chrome_shapes_spaces_and_blank_lines_are_preserved() {
        let body = "  indented\n\n────────────────────────────────\nOpus 5 · xhigh · /tmp/x · 5h: 1% · 7d: 2% · 1000K window";
        let expected = render_payload("m-shape", "test", "shapes", body, false);
        assert_eq!(
            composer_content_from_joined_capture(
                &shipped("claude"),
                &claude_capture(&expected),
                "m-shape"
            ),
            ComposerContentProof::Visible(expected)
        );
    }

    #[test]
    fn collapsed_chips_are_hidden_and_unmeasured_vendors_are_unsupported() {
        for (vendor, capture) in [
            (
                "claude",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../cyclops-manifest/tests/fixtures/claude_collapsed_chip_2_1_239_esc.txt"
                ))
                .to_string(),
            ),
            (
                "codex",
                decoded_fixture(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../cyclops-manifest/tests/fixtures/codex_collapsed_chip_0_149_0_esc.hex"
                ))),
            ),
        ] {
            assert_eq!(
                composer_content_from_joined_capture(&shipped(vendor), &capture, "m-hidden"),
                ComposerContentProof::Hidden,
                "{vendor} chip bytes were treated as visible"
            );
        }
        assert_eq!(
            composer_content_from_joined_capture(&shipped("cursor"), "anything", "m-unsupported"),
            ComposerContentProof::Unsupported
        );
    }
}

#[tokio::test]
async fn delivery_engine_spawns_injector_for_buffer() {
    let tmux = cyclops_testrig::TmuxServer::new("enginetest");
    let spool = cyclops_proto::scratch::scratch_dir("cyc-enginetest-spool");
    let cfg = cyclops_tmux::ControlConfig::new_session("enginetest")
        .on_socket(tmux.socket())
        .with_config_file("/dev/null")
        .with_buffer_spool_dir(&spool);
    let (client, _rx) = ControlClient::spawn(cfg).await.expect("tmux spawns");
    let engine = DeliveryEngine::new(Arc::new(client));
    let injector = engine.injector("cyc-test-buf");
    assert_eq!(injector.buffer, "cyc-test-buf");
}
