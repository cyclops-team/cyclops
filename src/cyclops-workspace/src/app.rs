//! Workspace application state and event loop.
//!
//! The loop is event-armed: every visible change arms one render debounce
//! (`RENDER_DEBOUNCE`) if none is pending — arming never pushes an
//! already-armed deadline back, so a stream of events cannot starve
//! rendering. Full model reconciliation (one control-mode workspace
//! snapshot; see `crate::sync::fetch_workspace_model`) is deferred onto that
//! same deadline and coalesced through `needs_reconcile`; cheap structural
//! notifications (`%layout-change`, `%window-pane-changed`,
//! `%session-changed`) apply to the model directly without a full fetch.
//! Daemon decoration events get the identical treatment on their own
//! dedicated thread: `spawn_decoration_forwarder` arms one debounce per
//! burst instead of fetching status once per pushed event, and its
//! subscription outlives the daemon (`run_decoration_forwarder`
//! reconnects on a bounded backoff and resyncs on every connect).
//!
//! This module owns boot, the event queue, render scheduling, reconnect
//! orchestration, and top-level `App` state. It does not own tmux command
//! strings (`cyclops-tmux` and `action::route_*`), device-event decoding
//! (`input`), dialog text-editing (`dialog`), or frame composition
//! (`render`) — it calls into those and reacts to their results.

#![allow(clippy::too_many_arguments)]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::io;
use std::pin::Pin;

use crossterm::event::{
    self, Event, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use cyclops_client::{BlockingClient, ClientError, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT};
use cyclops_tmux::sizing::ClientIdentity;
use cyclops_tmux::{
    ControlClient, ControlConfig, InputCapacity, Notification, NotificationReceiver, TmuxError,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep_until, Duration, Instant};

use crate::action;
use crate::animate::{Motion, Seen, StatusInk};
use crate::bindings::load_bindings;
use crate::config::load_tmux_config;
use crate::copy;
use crate::decoration::{self, DecorationSnapshot};
use crate::dialog::{self, Dialog, DialogKeyAction};
use crate::drag::{DragState, DragTarget};
use crate::input::encode_send_keys;
use crate::input::mouse::{HitMap, HitTarget, MenuState};
use crate::input::router::{Router, RouterResult};
use crate::layout::SplitDir;
use crate::model::{pane_is_visible, RuntimeRegistry, TabModel, WorkspaceModel};
use crate::naming;
use crate::notice::NoticeState;
use crate::persist::{self, load_prefs, set_last_active, SidebarTab, WorkspacePrefs};
use crate::render::{
    paint_dialog, paint_menu, paint_messages, paint_messages_rail, paint_messages_resize_feedback,
    paint_sidebar, paint_sidebar_rail, paint_sidebar_resize_feedback, paint_tab_bar, paint_window,
    MessagesRailCue,
};
use crate::resilience::{self, LinkState};
use crate::selection::{self, SelectionState};
use crate::sync::{fetch_workspace_model, hydrate_visible_tab};
use crate::term_guard::{SynchronizedWriter, TermGuard};
use crate::theme::Paint;

mod exec;

/// At most one frame per 8 ms (~120 Hz). The timer exists only after an
/// event; idle workspaces still have no wakeups.
const RENDER_DEBOUNCE: Duration = Duration::from_millis(8);
/// How long after pane output a folder-following workspace re-reads its
/// pane's directory.
///
/// It has to be a DELAY, not a render-time check. There is no tmux
/// notification for a `cd`, so the only signal is the pane output the
/// command produces — and the first of that output is the shell echoing the
/// line the user typed, which arrives before the shell has run `chdir`.
/// MEASURED: probing on that edge reads the old path every time, and a pane
/// that then sits idle produces no further output, so the name never catches
/// up. Waiting lets the shell land first. One armed probe at a time, never
/// pushed back, so a noisy pane costs one tmux round trip per interval and
/// an idle workspace costs no wakeups at all.
const FOLDER_PROBE_DELAY: Duration = Duration::from_millis(600);
/// How long a burst of daemon decoration events (state, label, delivery
/// changes) is allowed to coalesce before `spawn_decoration_forwarder`
/// issues one status fetch. Same rule as `RENDER_DEBOUNCE`/`arm()`: armed
/// once by the first event in a burst, fired once, never pushed back by a
/// later event in the same burst. A split or a border drag can push several
/// events through cyclopsd at once; without this, each one used to cost its
/// own blocking status round trip even though only the last result before
/// the next paint was ever shown.
const DECORATION_DEBOUNCE: Duration = Duration::from_millis(30);
/// One frame's worth of ready work. Producers backpressure after this
/// instead of growing memory while rendering or tmux reconciliation waits.
const INGRESS_CAPACITY: usize = cyclops_ui::INGRESS_BATCH;
/// Payload-bearing lanes are both item-bounded and byte-bounded. Every item
/// is capped at 1 MiB before enqueue, so eight retained items hold at most
/// 8 MiB of payload per lane.
const PAYLOAD_INGRESS_CAPACITY: usize = 8;
/// Input and action answers keep priority for an interactive burst, then one
/// ready background lane must run before priority can resume.
const PRIORITY_BURST: usize = 8;
/// A send worker is serial per composer, so one answer is the full action
/// backlog the application can use.
const ACTION_CAPACITY: usize = 1;
/// One pending focus or resize edge is sufficient. Interactive keys, paste,
/// and mouse actions use the priority lanes instead.
const TERMINAL_CAPACITY: usize = 1;
/// Match the stream UI's shipped paste quarantine. Larger clipboard data is
/// refused before it enters the application queue.
const PASTE_MAX_BYTES: usize = 1 << 20;
/// Pane output uses the same byte envelope as a terminal paste. Larger
/// control-mode notifications are split without changing pane byte order.
const OUTPUT_BATCH_MAX_BYTES: usize = 1 << 20;

enum AppMsg {
    /// A send started from the composer has an answer. Carries the
    /// attempt so a receipt cannot be shown against a composer that has
    /// since been reopened or changed to a different send.
    SendFinished {
        attempt: dialog::ComposeAttempt,
        outcome: crate::daemon::SendOutcome,
    },
    Input {
        epoch: u64,
        key: KeyEvent,
    },
    Paste {
        epoch: u64,
        text: String,
    },
    PasteTooLarge {
        epoch: u64,
        bytes: usize,
    },
    Mouse {
        epoch: u64,
        mouse: MouseEvent,
    },
    /// The terminal's focus moved onto (`true`) or off (`false`) the
    /// workspace's tab. Drives the host palette only: the theme's ink and
    /// ground are handed to the terminal while the workspace is looked at
    /// and both defaults return the moment it is not, so a shell in another
    /// tab of the same window never wears the workspace's colors.
    Focus(bool),
    OutputBatch(Vec<(String, Vec<u8>)>),
    Redraw,
    Resized(u16, u16),
    Reconcile,
    SessionSwitched {
        session: String,
        name: String,
    },
    SessionRenamed {
        session: Option<String>,
        name: String,
    },
    LayoutChanged {
        window: String,
        layout: String,
        flags: Option<String>,
    },
    ActivePaneChanged {
        window: String,
        pane: String,
    },
    LinkLost,
    PanePaused {
        pane: String,
    },
    PaneContinued {
        pane: String,
    },
    DecorationChanged(DecorationSnapshot),
    /// Persistent classification of the authenticated daemon's Hello identity.
    DaemonCompatibility(cyclops_client::HelloCompatibility),
    /// The daemon subscription (re)connected. The handler resyncs what an
    /// outage loses on both sides: a restarted daemon forgot every watch
    /// ask (those live in its memory, not config.toml), and any state that
    /// changed while nothing was subscribed produced no event, so only a
    /// full refetch can replace it.
    DaemonReconnected,
    /// One daemon event, already normalized onto the shared `cyclops
    /// watch` stream vocabulary (E2) by [`spawn_decoration_forwarder`]'s
    /// reader thread. Feeds `App::record` and nothing else — decoration's
    /// own coalesced refresh is a separate message (`DecorationChanged`)
    /// on the same connection, so this must never gate or delay it.
    StreamEntry(Box<cyclops_ui::Entry>),
    StreamGap {
        why: String,
    },
    StreamReconciled(Box<crate::event_record::Bootstrap>),
    /// The daemon switched themes (`cyclops theme <name>`), forwarded by
    /// [`spawn_decoration_forwarder`]'s reader thread. Wake-only, like
    /// cyclops-ui's `UiMsg::ThemeChanged`: the handler arms the render
    /// debounce and the reload itself runs on that deadline, before draw.
    ThemeChanged,
    /// A messages snapshot response has landed from cyclopsd.
    MessagesSnapshotLoaded {
        request: cyclops_ui::RefreshRequest,
        result: cyclops_proto::MessagesSnapshotResult,
    },
    /// A messages snapshot request failed or timed out.
    MessagesSnapshotFailed {
        request: cyclops_ui::RefreshRequest,
        error: String,
    },
    /// Message detail or claim response for one frozen target.
    MessageDetailFinished {
        target: cyclops_ui::FrozenTarget,
        outcome: cyclops_ui::ActionOutcome,
    },
    /// Messages group-chat composer send outcome from background worker.
    MessagesSendFinished {
        attempt: MessagesSendAttempt,
        outcome: crate::daemon::SendOutcome,
    },
    /// Messages projection changed invalidation signal from cyclopsd.
    MessagesChanged(Option<cyclops_proto::MessagesChangedData>),
}

/// Exact composer bytes and routing held until the daemon answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagesSendAttempt {
    pub composer_revision: u64,
    pub mode: cyclops_ui::ComposerMode,
    pub caller: cyclops_proto::RecipientKey,
    pub recipient_keys: Option<Vec<cyclops_proto::RecipientKey>>,
    pub subject: String,
    pub body: String,
    pub fyi: bool,
    pub reply_to: Option<String>,
    pub client_key: String,
}

impl MessagesSendAttempt {
    fn matches(
        &self,
        composer: &cyclops_ui::ComposerState,
        composer_revision: u64,
        caller: Option<cyclops_proto::RecipientKey>,
    ) -> bool {
        composer_revision == self.composer_revision
            && caller == Some(self.caller)
            && composer.sender == Some(self.caller)
            && composer.mode.as_ref() == Some(&self.mode)
            && composer.text() == self.body
            && composer.draft.key() == Some(self.client_key.as_str())
    }
}

/// Composer identity whose uncertain result an operator chose to reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MessagesDraftIdentity {
    composer_revision: u64,
    mode: cyclops_ui::ComposerMode,
    caller: cyclops_proto::RecipientKey,
    body: String,
    client_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPaletteState {
    /// Nothing has been emitted since this process acquired or regained the surface.
    Unknown,
    /// The terminal owns both defaults (OSC 110/111 was emitted).
    Defaults,
    /// Cyclops owns both defaults with this exact theme pair.
    Theme(crate::theme::HostPalette),
}

impl MessagesDraftIdentity {
    fn current(
        composer: &cyclops_ui::ComposerState,
        composer_revision: u64,
        caller: Option<cyclops_proto::RecipientKey>,
    ) -> Option<Self> {
        Some(Self {
            composer_revision,
            mode: composer.mode.clone()?,
            caller: caller?,
            body: composer.text().to_string(),
            client_key: composer.draft.key()?.to_string(),
        })
    }

    fn matches(
        &self,
        composer: &cyclops_ui::ComposerState,
        composer_revision: u64,
        caller: Option<cyclops_proto::RecipientKey>,
    ) -> bool {
        composer_revision == self.composer_revision
            && caller == Some(self.caller)
            && composer.sender == Some(self.caller)
            && composer.mode.as_ref() == Some(&self.mode)
            && composer.text() == self.body
            && composer.draft.key() == Some(self.client_key.as_str())
    }
}

/// Work item for sending a message asynchronously from the group-chat composer.
pub(crate) struct MessagesSendTask {
    pub attempt: MessagesSendAttempt,
}

/// One detail read against the exact row the operator opened.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageDetailTask {
    row: cyclops_ui::QueueRow,
    target: cyclops_ui::FrozenTarget,
}

impl MessageDetailTask {
    fn request(&self) -> cyclops_ui::ActionRequest {
        debug_assert_eq!(&self.row.target, &self.target.target);
        match self.target.attempt {
            Some(attempt_id) => cyclops_ui::ActionRequest::OpenAttention { attempt_id },
            None => cyclops_ui::ActionRequest::OpenMessage {
                message_id: self.row.message_id.clone(),
                claim: self.row.direction == cyclops_ui::Direction::Inbound
                    && self.row.mailbox == cyclops_ui::MailboxWord::Pending,
            },
        }
    }
}

impl AppMsg {
    fn pane_input_epoch(&self) -> Option<u64> {
        match self {
            Self::Input { epoch, .. }
            | Self::Paste { epoch, .. }
            | Self::PasteTooLarge { epoch, .. }
            | Self::Mouse { epoch, .. } => Some(*epoch),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct PaneInputGate(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl PaneInputGate {
    fn new() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)))
    }

    fn stamp(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }

    fn close(&self) {
        let _ = self.0.fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |current| (current % 2 == 0).then_some(current.wrapping_add(1)),
        );
    }

    fn open(&self) {
        let _ = self.0.fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |current| (current % 2 == 1).then_some(current.wrapping_add(1)),
        );
    }

    fn accepts(&self, epoch: u64) -> bool {
        let current = self.stamp();
        current.is_multiple_of(2) && epoch == current
    }

    fn is_closed(&self) -> bool {
        self.stamp() % 2 == 1
    }
}

/// Independent bounded lanes for work that enters the application from
/// background tasks. Keeping the senders typed as one message vocabulary
/// lets the handler stay the single owner of state transitions while the
/// receivers enforce priority.
#[derive(Clone)]
struct AppSinks {
    tmux: mpsc::Sender<AppMsg>,
    stream: mpsc::Sender<AppMsg>,
    continuity: mpsc::Sender<ControlContinuityBarrier>,
}

struct ControlContinuityBarrier {
    repair: oneshot::Sender<bool>,
    cutover: oneshot::Receiver<bool>,
}

struct App {
    model: WorkspaceModel,
    runtimes: RuntimeRegistry,
    router: Router,
    paint: Paint,
    dialog: Option<Dialog>,
    /// How far the open dialog has been dragged off its resting center, in
    /// cells. Cleared by [`App::open_dialog`], so a box always opens where
    /// the eye expects it and only stays moved for as long as it is the
    /// same box. Held to what still moves the box on screen; see
    /// [`crate::render::clamp_dialog_offset`].
    dialog_offset: (i16, i16),
    /// The theme that was live when the theme picker opened; `Some` for
    /// exactly as long as the picker is. Selection moves preview the
    /// highlighted theme straight into `paint.theme`
    /// (`exec::preview_selected_theme`); closing without applying puts
    /// this back, and applying drops it, the previewed paint being the
    /// theme the daemon confirms. While set, [`refresh_theme_watch`]
    /// holds the ThemeWatch off so a reload cannot overwrite the preview.
    theme_restore: Option<cyclops_theme::Theme>,
    link_state: LinkState,
    paused_panes: HashSet<String>,
    /// The foreground/background pair last handed to the host terminal.
    /// Compared each frame so an escape goes out on ownership or theme
    /// change and on no other frame.
    window_palette: HostPaletteState,
    /// Whether the terminal's focus is on the workspace. While it is not,
    /// the draw path leaves the terminal's own palette alone
    /// (`AppMsg::Focus` hands it back), so a frame drawn for pane output
    /// arriving in an unfocused tab cannot re-paint the operator's
    /// terminal behind their back. Starts `true`: focus reporting only
    /// speaks on changes, and a workspace is launched by someone looking
    window_focused: bool,
    /// Panes deliberately collapsed to their title bar, mapped to the pre-collapse
    /// height each had before. Persisted in tmux pane option `@cyclops_pane_minimized_v1`
    /// (`v1:<height>`) so intentional minimization survives across reconnects,
    /// authority transfers, and window resizing without accidental uncrush.
    minimized: std::collections::HashMap<String, u16>,
    reconnect_attempt: usize,
    /// One-shot, set by the reconnect path: the next reconcile rehydrates
    /// every visible pane instead of only size-stale ones, because a link
    /// outage misses `%output` without moving any pane's size.
    needs_forced_hydrate: bool,
    hit_map: HitMap,
    menu: MenuState,
    /// Mouse cell, tracked while a menu or dialog is open so its rows can
    /// paint a hover highlight.
    hover: Option<(u16, u16)>,
    selection: SelectionState,
    /// Cmd+A's one-key arm: the next delete clears the focused pane's
    /// input line (`crate::input::SelectAll`).
    select_all: crate::input::SelectAll,
    drag: Option<DragState>,
    /// The one transient message slot (`crate::notice`). Its deadline
    /// joins the loop's deadline set below, so a notice clears itself on
    /// an idle workspace without a keypress and without a timer of its
    /// own.
    notice: NoticeState,
    daemon_compatibility: Option<cyclops_client::HelloCompatibility>,
    /// A Hello mismatch is durable connection state, not a transient action
    /// result. It remains visible until a later authenticated Hello replaces it.
    daemon_compatibility_notice: Option<String>,
    decoration: DecorationSnapshot,
    prefs: WorkspacePrefs,
    /// Stable session ids whose agent children are visible in the sidebar.
    expanded_workspaces: HashSet<String>,
    /// The session id the sidebar last auto-expanded for. Only a CHANGE
    /// opens a row; see [`expand_active_workspace`].
    expanded_for: Option<String>,
    /// Session ids cyclopsd has already been asked to watch. Keyed by id,
    /// not name, so a folder-following rename never asks twice; see
    /// [`crate::daemon::watch_session`].
    watched_sessions: HashSet<String>,
    /// Which tab the sidebar is showing. Seeded from prefs at boot and
    /// written back on every change, so a workspace reopens on the tab it
    /// was left on.
    sidebar_tab: SidebarTab,
    /// The sidebar's file panel. Rooted on the focused pane's directory the
    /// first time that probe answers, and then only where the operator
    /// walks it.
    files: crate::files::FileTree,
    /// The pinned browser. `files` above follows the focused agent's
    /// folder; this one stays wherever the operator last browsed it and
    /// remembers that across restarts (`prefs.files_pinned_root`).
    files_pinned: crate::files::FileTree,
    /// Which of the two the panel is showing.
    files_view: crate::files::FilesView,
    /// When the file tree is next re-read. `None` while nothing is armed;
    /// see [`arm_files_probe`], which is what keeps this off the clock when
    /// the panel is not on screen.
    files_probe_at: Option<Instant>,
    /// The next file probe should also re-root the tree on the focused
    /// pane's folder. Set at boot and whenever the operator asks for it;
    /// cleared once the probe answers, so a pane that has since gone does
    /// not leave the request armed forever.
    files_root_pending: bool,
    /// The shared `cyclops watch` stream model (E2): the same ordered,
    /// identity-stable [`cyclops_ui::Entry`] rows that surface renders,
    /// fed by [`spawn_decoration_forwarder`]'s `events.subscribe`
    /// connection so the Stream tab never opens a second one.
    ///
    /// Fed whether or not the Stream tab is showing. The record is cheap
    /// (no IO per event, and internally ring-capped) and stopping the feed
    /// while the sidebar sits on Sessions would show an empty history the
    /// moment the tab is selected mid-session; keeping it warm costs
    /// nothing a stopped feed would otherwise save.
    ///
    /// Built and fed through [`crate::event_record`]: the same replayed
    /// ledger tail, status seed, and live ordering `cyclops watch` runs,
    /// so the Stream tab and the CLI show one history rather than two that
    /// agree only on formatting.
    record: cyclops_ui::Record,
    /// The shared Cyclops watch Messages model (HumanQueue).
    messages_queue: cyclops_ui::HumanQueue,
    /// Last authenticated body-free counts for the collapsed Messages rail.
    /// Freshness is owned by `messages_gate`; these are never mutated as a
    /// second unread queue.
    messages_snapshot_counts: Option<cyclops_proto::MessagesSnapshotCounts>,
    /// Authenticated mailbox identity that produced the current snapshot.
    /// Absent means the Messages pane is read-only.
    messages_caller: Option<cyclops_proto::RecipientKey>,
    /// Open message detail view when an operator opens a message.
    messages_detail: Option<cyclops_ui::Detail>,
    /// Bounded group-chat composer state.
    messages_composer: cyclops_ui::ComposerState,
    /// Data-driven avatar registry for resolving agent/sender initials and icons.
    avatar_registry: cyclops_ui::AvatarRegistry,
    /// Startup ordering and seq dedup for `record`
    /// ([`cyclops_ui::Intake`]): live entries reaching the app before
    /// [`crate::event_record::boot`] lands its backfill buffer here, and
    /// ledger-backed duplicates arriving live during startup are dropped
    /// by seq instead of shown twice.
    intake: cyclops_ui::Intake,
    /// An oversized event invalidates the stream once. A replacement is
    /// loaded off-loop and later gaps coalesce until that result lands.
    stream_reconciling: bool,
    /// The (shape, blink) last emitted to the host terminal
    /// ([`crate::term_guard::apply_cursor_style`]), so a frame whose
    /// focused-pane cursor did not change costs no terminal write. `None`
    /// until the first visible cursor is drawn.
    cursor_style: Option<(crate::runtime::CursorShape, bool)>,
    term_size: (u16, u16),
    /// Last shared Messages-independent target pushed to the windows this
    /// workspace owns. Avoids a resize notification loop when expanded pane
    /// gutters are already at their target geometry.
    declared_client_size: Option<(u16, u16)>,
    /// Which sessions this workspace sizes, and which of their windows it
    /// has pinned. See [`WindowSizing`].
    sizing: WindowSizing,
    needs_reconcile: bool,
    /// A structural notification changed visible pane dimensions. Hydration
    /// waits for the render deadline so resize bursts collapse to one set of
    /// captures instead of blocking the input path for every intermediate.
    needs_hydrate: bool,
    paste_seq: u64,
    home: std::path::PathBuf,
    /// When the next folder probe is due. `None` means none is armed; see
    /// [`arm_folder_probe`].
    folder_probe_at: Option<Instant>,
    /// One fixed send worker. A composer can abandon an in-flight attempt,
    /// but reopening it never creates another thread.
    send_requests: Option<std::sync::mpsc::SyncSender<dialog::ComposeAttempt>>,
    /// One fixed stream replacement worker. Gap edges coalesce in its
    /// single slot while the current replacement is loading.
    stream_reconcile_requests: Option<std::sync::mpsc::SyncSender<()>>,
    /// A host resize arrived and tmux has not been told the new size yet.
    /// Drained on the render beat so a drag-resize burst costs one tmux
    /// call at the final size rather than one per intermediate size.
    repaint_resize_pending: bool,
    /// When the current resize burst is considered finished. Slid forward
    /// by every arriving resize, so a continuous drag never sends.
    repaint_resize_settle_at: Option<Instant>,
    /// Set when something decided the frame the user is looking at is not
    /// the frame the renderer thinks it wrote. Drained by [`RenderOwner`]
    /// before its next frame, which then repaints every cell.
    pub(crate) repaint_requested: bool,
    /// Whether the Messages pane has active keyboard focus.
    messages_focused: bool,
    /// Whether the Messages pane shows only the active workspace's session.
    /// The filter itself is re-derived from the live pane table every frame
    /// (`sync_messages_session_filter`); this is the operator's choice.
    messages_session_scoped: bool,
    /// Refresh gate and connection lifecycle for the Messages pane.
    messages_gate: cyclops_ui::RefreshGate,
    /// Exact failure from the last whole-snapshot RPC. Kept until another
    /// request starts or a current snapshot lands so the Messages pane never
    /// hides an authentication, routing, or transport failure behind
    /// `refresh failed`.
    messages_refresh_error: Option<String>,
    /// Async worker for Messages composer sends.
    messages_send_tx: Option<std::sync::mpsc::SyncSender<MessagesSendTask>>,
    /// Local edit generation used to reject completions for abandoned drafts.
    messages_composer_revision: u64,
    /// The only Messages composer request whose answer may change the composer.
    messages_send_in_flight: Option<MessagesSendAttempt>,
    /// Async worker for bounded messages snapshot requests.
    messages_snapshot_tx: Option<std::sync::mpsc::SyncSender<(cyclops_ui::RefreshRequest, usize)>>,
    /// Async worker for claiming/loading message detail.
    message_detail_tx: Option<mpsc::Sender<MessageDetailTask>>,
    /// The only frozen detail target whose answer is still outstanding.
    message_detail_in_flight: Option<cyclops_ui::FrozenTarget>,
    /// Exact uncertain draft whose reconciliation waits for a current snapshot.
    messages_reconcile_owed: Option<MessagesDraftIdentity>,
}

fn toggle_workspace_expanded(expanded: &mut HashSet<String>, session_id: String) -> bool {
    if expanded.remove(&session_id) {
        false
    } else {
        expanded.insert(session_id);
        true
    }
}

/// Arm the render debounce if none is pending. Never pushes an armed
/// deadline back — that would let a busy event stream starve rendering.
fn arm(debounce: &mut Option<Instant>) {
    if debounce.is_none() {
        *debounce = Some(Instant::now() + RENDER_DEBOUNCE);
    }
}

enum Wake {
    Message(Option<Box<AppMsg>>),
    InputCapacity(Result<InputCapacity, TmuxError>),
    ControlContinuityLost(ControlContinuityBarrier),
    Deadline,
}

impl Wake {
    fn message(message: Option<AppMsg>) -> Self {
        Self::Message(message.map(Box::new))
    }
}

type InputCapacityFuture =
    Pin<Box<dyn Future<Output = Result<InputCapacity, TmuxError>> + Send + 'static>>;

#[derive(Debug, PartialEq, Eq)]
struct PendingPaneInput {
    pane: String,
    keys: Vec<String>,
}

/// Retire the pane-input prefix accepted before a continuity barrier. The
/// action lane is separate and survives because it resolves against the new
/// model after reconciliation.
fn retire_pane_input_segment(
    active_pane: &str,
    pending: &mut Option<PendingPaneInput>,
    input_rx: &mut mpsc::Receiver<AppMsg>,
    paste_rx: &mut mpsc::Receiver<AppMsg>,
) -> Option<String> {
    let pending_pane = pending.take().map(|input| input.pane);
    let mut dropped_suffix = false;
    while input_rx.try_recv().is_ok() {
        dropped_suffix = true;
    }
    while paste_rx.try_recv().is_ok() {
        dropped_suffix = true;
    }
    pending_pane.or_else(|| dropped_suffix.then(|| active_pane.to_string()))
}

/// Wait for a message or the next armed deadline. The explicit due check is
/// what makes the render guarantee real: a permanently ready message queue
/// cannot keep winning a biased select after the deadline has passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngressSource {
    Priority,
    Background,
}

#[derive(Default)]
struct IngressFairness {
    priority_run: usize,
    priority_cursor: usize,
    background_cursor: usize,
}

impl IngressFairness {
    fn record(&mut self, source: IngressSource) {
        match source {
            IngressSource::Priority => {
                self.priority_run = self.priority_run.saturating_add(1);
            }
            IngressSource::Background => self.priority_run = 0,
        }
    }
}

fn try_priority_ready(
    input_rx: &mut mpsc::Receiver<AppMsg>,
    paste_rx: &mut mpsc::Receiver<AppMsg>,
    action_rx: &mut mpsc::Receiver<AppMsg>,
    cursor: &mut usize,
) -> Option<AppMsg> {
    for offset in 0..3 {
        let lane = (*cursor + offset) % 3;
        let message = match lane {
            0 => input_rx.try_recv().ok(),
            1 => paste_rx.try_recv().ok(),
            2 => action_rx.try_recv().ok(),
            _ => unreachable!(),
        };
        if message.is_some() {
            *cursor = (lane + 1) % 3;
            return message;
        }
    }
    None
}

fn try_background_ready(
    terminal_rx: &mut mpsc::Receiver<AppMsg>,
    tmux_rx: &mut mpsc::Receiver<AppMsg>,
    stream_rx: &mut mpsc::Receiver<AppMsg>,
    allow_stream: bool,
    cursor: &mut usize,
) -> Option<AppMsg> {
    for offset in 0..3 {
        let lane = (*cursor + offset) % 3;
        let message = match lane {
            0 => terminal_rx.try_recv().ok(),
            1 => tmux_rx.try_recv().ok(),
            2 if allow_stream => stream_rx.try_recv().ok(),
            2 => None,
            _ => unreachable!(),
        };
        if message.is_some() {
            *cursor = (lane + 1) % 3;
            return message;
        }
    }
    None
}

async fn next_message(
    input_rx: &mut mpsc::Receiver<AppMsg>,
    paste_rx: &mut mpsc::Receiver<AppMsg>,
    action_rx: &mut mpsc::Receiver<AppMsg>,
    terminal_rx: &mut mpsc::Receiver<AppMsg>,
    tmux_rx: &mut mpsc::Receiver<AppMsg>,
    stream_rx: &mut mpsc::Receiver<AppMsg>,
    allow_stream: bool,
    fairness: &mut IngressFairness,
) -> Option<AppMsg> {
    if fairness.priority_run >= PRIORITY_BURST {
        if let Some(message) = try_background_ready(
            terminal_rx,
            tmux_rx,
            stream_rx,
            allow_stream,
            &mut fairness.background_cursor,
        ) {
            fairness.record(IngressSource::Background);
            return Some(message);
        }
    } else if let Some(message) =
        try_priority_ready(input_rx, paste_rx, action_rx, &mut fairness.priority_cursor)
    {
        fairness.record(IngressSource::Priority);
        return Some(message);
    }

    if let Some(message) = try_background_ready(
        terminal_rx,
        tmux_rx,
        stream_rx,
        allow_stream,
        &mut fairness.background_cursor,
    ) {
        fairness.record(IngressSource::Background);
        return Some(message);
    }

    let (source, message) = tokio::select! {
        message = input_rx.recv() => (IngressSource::Priority, message),
        message = paste_rx.recv() => (IngressSource::Priority, message),
        message = action_rx.recv() => (IngressSource::Priority, message),
        message = terminal_rx.recv() => (IngressSource::Background, message),
        message = tmux_rx.recv() => (IngressSource::Background, message),
        message = stream_rx.recv(), if allow_stream => (IngressSource::Background, message),
    };
    fairness.record(source);
    message
}

async fn next_background_message(
    terminal_rx: &mut mpsc::Receiver<AppMsg>,
    tmux_rx: &mut mpsc::Receiver<AppMsg>,
    stream_rx: &mut mpsc::Receiver<AppMsg>,
    allow_stream: bool,
    cursor: &mut usize,
) -> Option<AppMsg> {
    if let Some(message) =
        try_background_ready(terminal_rx, tmux_rx, stream_rx, allow_stream, cursor)
    {
        return Some(message);
    }
    let (lane, message) = tokio::select! {
        message = terminal_rx.recv() => (0, message),
        message = tmux_rx.recv() => (1, message),
        message = stream_rx.recv(), if allow_stream => (2, message),
    };
    *cursor = (lane + 1) % 3;
    message
}

async fn next_wake(
    continuity_rx: &mut mpsc::Receiver<ControlContinuityBarrier>,
    input_rx: &mut mpsc::Receiver<AppMsg>,
    paste_rx: &mut mpsc::Receiver<AppMsg>,
    action_rx: &mut mpsc::Receiver<AppMsg>,
    terminal_rx: &mut mpsc::Receiver<AppMsg>,
    tmux_rx: &mut mpsc::Receiver<AppMsg>,
    stream_rx: &mut mpsc::Receiver<AppMsg>,
    allow_stream: bool,
    fairness: &mut IngressFairness,
    deadline: Option<Instant>,
) -> Wake {
    if let Ok(ack) = continuity_rx.try_recv() {
        return Wake::ControlContinuityLost(ack);
    }
    if deadline.is_some_and(|at| at <= Instant::now()) {
        return Wake::Deadline;
    }
    let Some(deadline) = deadline else {
        return tokio::select! {
            biased;
            ack = continuity_rx.recv() => match ack {
                Some(ack) => Wake::ControlContinuityLost(ack),
                None => Wake::Message(None),
            },
            message = next_message(
                input_rx,
                paste_rx,
                action_rx,
                terminal_rx,
                tmux_rx,
                stream_rx,
                allow_stream,
                fairness,
            ) => Wake::message(message),
        };
    };
    tokio::select! {
        biased;
        ack = continuity_rx.recv() => match ack {
            Some(ack) => Wake::ControlContinuityLost(ack),
            None => Wake::Message(None),
        },
        msg = next_message(
            input_rx,
            paste_rx,
            action_rx,
            terminal_rx,
            tmux_rx,
            stream_rx,
            allow_stream,
            fairness,
        ) => Wake::message(msg),
        _ = sleep_until(deadline) => Wake::Deadline,
    }
}

/// Wait for one held pane-input batch to gain reply capacity while the app
/// keeps consuming background state. Priority lanes are intentionally absent:
/// a later key, paste, or action cannot pass the batch already accepted from
/// the terminal. The same capacity future survives background wakes, so an
/// output flood cannot repeatedly move it to the back of the semaphore queue.
async fn next_pending_input_wake(
    continuity_rx: &mut mpsc::Receiver<ControlContinuityBarrier>,
    capacity: &mut InputCapacityFuture,
    terminal_rx: &mut mpsc::Receiver<AppMsg>,
    tmux_rx: &mut mpsc::Receiver<AppMsg>,
    stream_rx: &mut mpsc::Receiver<AppMsg>,
    allow_stream: bool,
    background_cursor: &mut usize,
    deadline: Option<Instant>,
) -> Wake {
    if let Ok(ack) = continuity_rx.try_recv() {
        return Wake::ControlContinuityLost(ack);
    }
    if deadline.is_some_and(|at| at <= Instant::now()) {
        return Wake::Deadline;
    }
    let background = next_background_message(
        terminal_rx,
        tmux_rx,
        stream_rx,
        allow_stream,
        background_cursor,
    );
    let Some(deadline) = deadline else {
        return tokio::select! {
            biased;
            ack = continuity_rx.recv() => match ack {
                Some(ack) => Wake::ControlContinuityLost(ack),
                None => Wake::Message(None),
            },
            result = capacity.as_mut() => Wake::InputCapacity(result),
            message = background => Wake::message(message),
        };
    };
    tokio::select! {
        biased;
        ack = continuity_rx.recv() => match ack {
            Some(ack) => Wake::ControlContinuityLost(ack),
            None => Wake::Message(None),
        },
        result = capacity.as_mut() => Wake::InputCapacity(result),
        message = background => Wake::message(message),
        _ = sleep_until(deadline) => Wake::Deadline,
    }
}

/// What the boot path does with the reopen decision: attach to `.0`, or
/// create it first (`new-session -A`) because the server has nothing.
fn boot_target(reopen: &persist::ReopenTarget) -> (String, bool) {
    match reopen {
        persist::ReopenTarget::LastActive { session, .. }
        | persist::ReopenTarget::DefaultWorkspace(session)
        | persist::ReopenTarget::First(session) => (session.clone(), false),
        persist::ReopenTarget::OfferCreate => (copy::DEFAULT_SESSION_NAME.to_string(), true),
    }
}

/// Run the workspace on a tty. Returns the process exit code.
pub async fn run_async() -> i32 {
    let home = cyclops_proto::cyclops_home();
    let state_root = match cyclops_state::StateRoot::open_or_create(&home) {
        Ok(root) => std::sync::Arc::new(root),
        Err(error) => {
            eprintln!("open state root {}: {error}", home.display());
            return 1;
        }
    };
    let prefs = load_prefs(&home);
    let tmux_cfg = load_tmux_config(&home);
    let socket_name = tmux_cfg.socket.clone();
    let socket = socket_name.as_deref();
    // No server yet is the same as a server with no sessions: boot one.
    let sessions = cyclops_tmux::list_sessions(socket).unwrap_or_default();
    let session_names: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
    let last_active = persist::get_last_active(&home);
    let reopen = persist::reopen_fallback(
        &session_names,
        last_active.as_ref(),
        None,
        &prefs.workspace_order,
    );
    // Creating inherits this process's cwd: `cyclops` in a project folder
    // opens a shell pane there, no preset or manual tmux required.
    let (session, create) = boot_target(&reopen);
    let mut cfg = if create {
        ControlConfig::new_session(&session).with_initial_window_name("1")
    } else {
        ControlConfig::attach(&session)
    };
    if let Some(ref sock) = socket_name {
        cfg = cfg.on_socket(sock.clone());
    }
    if let Some(path) = tmux_cfg.config_file {
        cfg = cfg.with_config_file(path);
    }
    // tmux needs a pathname for pasted text. The held root still owns
    // creation and exact-inode cleanup.
    cfg = cfg.with_state_buffer_spool(state_root, "spool");
    let control_cfg = cfg.clone();
    let (mut client, notif_rx) = match ControlClient::spawn(cfg).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let bindings = load_bindings(&home);
    let (input_tx, mut input_rx) = mpsc::channel::<AppMsg>(INGRESS_CAPACITY);
    let (paste_tx, mut paste_rx) = mpsc::channel::<AppMsg>(1);
    let (terminal_tx, mut terminal_rx) = mpsc::channel::<AppMsg>(TERMINAL_CAPACITY);
    let (action_tx, mut action_rx) = mpsc::channel::<AppMsg>(ACTION_CAPACITY);
    let (tmux_tx, mut tmux_rx) = mpsc::channel::<AppMsg>(PAYLOAD_INGRESS_CAPACITY);
    let (continuity_tx, mut continuity_rx) = mpsc::channel(1);
    let (stream_tx, mut stream_rx) = mpsc::channel::<AppMsg>(PAYLOAD_INGRESS_CAPACITY);
    let pane_input_gate = PaneInputGate::new();
    let sinks = AppSinks {
        tmux: tmux_tx,
        stream: stream_tx,
        continuity: continuity_tx,
    };

    let (send_request_tx, send_request_rx) =
        std::sync::mpsc::sync_channel::<dialog::ComposeAttempt>(1);
    let send_home = home.clone();
    let send_results = action_tx.clone();
    std::thread::spawn(move || {
        while let Ok(attempt) = send_request_rx.recv() {
            let outcome = crate::daemon::send_message(
                &send_home,
                &attempt.message.to,
                &attempt.message.subject,
                &attempt.message.body,
                &attempt.client_key,
            );
            if send_results
                .blocking_send(AppMsg::SendFinished { attempt, outcome })
                .is_err()
            {
                return;
            }
        }
    });
    let (stream_reconcile_tx, stream_reconcile_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let stream_home = home.clone();
    let stream_results = action_tx.clone();
    std::thread::spawn(move || {
        while stream_reconcile_rx.recv().is_ok() {
            let bootstrap = crate::event_record::load(&stream_home);
            if stream_results
                .blocking_send(AppMsg::StreamReconciled(Box::new(bootstrap)))
                .is_err()
            {
                return;
            }
        }
    });

    let (messages_send_tx, messages_send_rx) = std::sync::mpsc::sync_channel::<MessagesSendTask>(1);
    let msg_send_home = home.clone();
    let msg_send_results = action_tx.clone();
    std::thread::spawn(move || {
        while let Ok(task) = messages_send_rx.recv() {
            let attempt = task.attempt;
            let outcome = crate::daemon::send_message_full(
                &msg_send_home,
                crate::daemon::ExactMessageRequest {
                    recipient_keys: attempt.recipient_keys.clone(),
                    expected_caller: attempt.caller,
                    subject: &attempt.subject,
                    body: &attempt.body,
                    fyi: attempt.fyi,
                    reply_to: attempt.reply_to.clone(),
                    client_key: &attempt.client_key,
                },
            );
            if msg_send_results
                .blocking_send(AppMsg::MessagesSendFinished { attempt, outcome })
                .is_err()
            {
                return;
            }
        }
    });

    let (messages_snapshot_tx, messages_snapshot_rx) =
        std::sync::mpsc::sync_channel::<(cyclops_ui::RefreshRequest, usize)>(1);
    let msg_snapshot_home = home.clone();
    let msg_snapshot_results = action_tx.clone();
    std::thread::spawn(move || {
        while let Ok((request, limit)) = messages_snapshot_rx.recv() {
            match crate::daemon::fetch_messages_snapshot(&msg_snapshot_home, limit) {
                Ok(result) => {
                    if msg_snapshot_results
                        .blocking_send(AppMsg::MessagesSnapshotLoaded { request, result })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    if msg_snapshot_results
                        .blocking_send(AppMsg::MessagesSnapshotFailed { request, error })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });

    let (message_detail_tx, mut message_detail_rx) = mpsc::channel::<MessageDetailTask>(1);
    let msg_detail_socket = home.join(cyclops_proto::SOCK_NAME);
    let msg_detail_results = action_tx.clone();
    tokio::spawn(async move {
        while let Some(task) = message_detail_rx.recv().await {
            let target = task.target.clone();
            let outcome = cyclops_ui::perform(&msg_detail_socket, task.request()).await;
            if msg_detail_results
                .send(AppMsg::MessageDetailFinished { target, outcome })
                .await
                .is_err()
            {
                return;
            }
        }
    });

    let terminal_input_gate = pane_input_gate.clone();
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(Event::Key(k)) => {
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                let epoch = terminal_input_gate.stamp();
                if input_tx
                    .blocking_send(AppMsg::Input { epoch, key: k })
                    .is_err()
                {
                    break;
                }
            }
            Ok(Event::Mouse(m)) => {
                let epoch = terminal_input_gate.stamp();
                let moved = matches!(m.kind, MouseEventKind::Moved);
                let message = AppMsg::Mouse { epoch, mouse: m };
                let sent = if moved {
                    input_tx.try_send(message).is_ok()
                } else {
                    input_tx.blocking_send(message).is_ok()
                };
                if !sent && input_tx.is_closed() {
                    break;
                }
            }
            Ok(Event::Paste(text)) => {
                let epoch = terminal_input_gate.stamp();
                let message = if text.len() > PASTE_MAX_BYTES {
                    AppMsg::PasteTooLarge {
                        epoch,
                        bytes: text.len(),
                    }
                } else {
                    AppMsg::Paste { epoch, text }
                };
                if paste_tx.blocking_send(message).is_err() {
                    break;
                }
            }
            Ok(Event::Resize(w, h)) => {
                let _ = terminal_tx.blocking_send(AppMsg::Resized(w, h));
            }
            Ok(Event::FocusGained) => {
                let _ = terminal_tx.blocking_send(AppMsg::Focus(true));
            }
            Ok(Event::FocusLost) => {
                let _ = terminal_tx.blocking_send(AppMsg::Focus(false));
            }
            Err(_) => break,
        }
    });

    spawn_notif_forwarder(notif_rx, sinks.tmux.clone(), sinks.continuity.clone());
    spawn_decoration_forwarder(home.clone(), sinks.tmux.clone(), sinks.stream.clone());

    // Theme detection prints warnings; do it before the alternate screen
    // swallows them.
    let paint = Paint::detect();
    // Theme hot reload: a stat re-checked on the render deadline, riding
    // events that already wake the loop (never a timer; tests/guards.rs).
    // Only meaningful while color is on. A theme file edited by hand
    // repaints on the next event; the daemon's "theme" event covers
    // `cyclops theme <name>` promptly, and that trade is accepted.
    let mut theme_watch = if paint.colors_enabled() {
        Some(cyclops_theme::ThemeWatch::new(&home))
    } else {
        None
    };

    let guard = match TermGuard::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{e}");
            client.shutdown().await;
            return 1;
        }
    };

    let term_size = crossterm::terminal::size().unwrap_or((80, 24));
    let mut model = match fetch_workspace_model(&client, &session).await {
        Ok(m) => m,
        Err(e) => {
            drop(guard);
            eprintln!("{e}");
            client.shutdown().await;
            return 1;
        }
    };
    apply_workspace_order(&mut model, &prefs.workspace_order);
    // Last-active is a tmux selection, not just local paint state. Select it
    // before sizing and hydration so bytes and geometry come from the same
    // window the user sees.
    if let persist::ReopenTarget::LastActive { window_id, .. } = &reopen {
        if let Some(index) = model
            .session
            .tabs
            .iter()
            .position(|tab| tab.window_id == *window_id)
        {
            if index != model.session.active_tab {
                if let Err(error) = client.select_window(window_id).await {
                    log_err(&home, &error);
                } else {
                    model.session.active_tab = index;
                }
            }
        }
    }

    // Take ownership and record every window's original policy before any
    // size is written, so the first thing this process does to a session is
    // reversible.
    let mut sizing = WindowSizing::default();
    let adopted = adopt_windows(
        &mut sizing,
        &client,
        &model.session.session,
        &model.session.tabs,
        &home,
    )
    .await;
    let following_at_boot = adopted.newly_following;

    // Declare terminal cells only after the split topology is known. tmux
    // gets pane content cells; two-cell separator bands remain UI chrome.
    // The model carries the persisted sidebar visibility BEFORE the
    // declaration is computed, because the first frame paints from the
    // model: a workspace quit collapsed would otherwise be declared one
    // sidebar wide and painted another, and the first reconcile would
    // fight the declaration. `chrome_for` is the one geometry both read.
    apply_saved_workspace_visibility(&mut model, &prefs);
    let declared_client_size =
        declare_initial_client_size(term_size, &model, &prefs, &sizing, &client, &home).await;
    if declared_client_size.is_some() {
        if let Err(error) = recover_post_resize_geometry(&sizing, &client, &home, None).await {
            log_err(&home, &format!("boot post-resize recovery failed: {error}"));
        }
        // The resize can rebalance leaf dimensions. Re-list before
        // hydration rather than replaying captures into stale slots.
        if let Ok(resized) = fetch_workspace_model(&client, &session).await {
            // A fresh snapshot knows nothing about UI-owned preferences;
            // re-carry visibility the same way `install_reconciled_model`
            // does for every later one.
            install_reconciled_model(
                &mut model,
                resized,
                prefs.sidebar_visible,
                prefs.messages_visible,
            );
            apply_workspace_order(&mut model, &prefs.workspace_order);
        }
    }
    let mut runtimes = RuntimeRegistry::default();
    hydrate_visible_tab(&client, model.active_tab(), &mut runtimes).await;

    let mut renderer =
        match Terminal::new(CrosstermBackend::new(SynchronizedWriter::new(io::stdout())))
            .map(RenderOwner::new)
        {
            Ok(t) => t,
            Err(e) => {
                drop(guard);
                eprintln!("{e}");
                client.shutdown().await;
                return 1;
            }
        };

    let expanded_workspaces = model
        .workspaces
        .get(model.active_workspace)
        .map(|workspace| HashSet::from([workspace.session_id.clone()]))
        .unwrap_or_default();
    let minimized = model.active_tab().minimized.clone();
    let mut app = App {
        model,
        runtimes,
        router: Router::new(bindings),
        paint,
        dialog: None,
        dialog_offset: (0, 0),
        theme_restore: None,
        link_state: LinkState::Live,
        paused_panes: HashSet::new(),
        minimized,
        window_palette: HostPaletteState::Unknown,
        window_focused: true,
        select_all: crate::input::SelectAll::default(),
        reconnect_attempt: 0,
        needs_forced_hydrate: false,
        hit_map: HitMap::default(),
        menu: MenuState::None,
        hover: None,
        selection: SelectionState::default(),
        drag: None,
        notice: NoticeState::default(),
        daemon_compatibility: None,
        daemon_compatibility_notice: None,
        // Nothing to fall back to on the first frame: no answer here is
        // genuinely "nothing known yet", which is what the default says.
        decoration: decoration::fetch_decoration(&home).unwrap_or_else(|error| {
            log_err(&home, &format!("decoration bootstrap failed: {error}"));
            DecorationSnapshot::default()
        }),
        prefs: prefs.clone(),
        expanded_workspaces,
        expanded_for: None,
        watched_sessions: HashSet::new(),
        sidebar_tab: prefs.sidebar_tab,
        files: crate::files::FileTree::new(),
        files_pinned: {
            let mut pinned = crate::files::FileTree::new();
            if let Some(root) = &prefs.files_pinned_root {
                pinned.reroot(root.clone());
            }
            pinned
        },
        files_view: crate::files::FilesView::default(),
        files_probe_at: None,
        files_root_pending: true,
        record: cyclops_ui::Record::new(),
        messages_queue: cyclops_ui::HumanQueue::default(),
        messages_snapshot_counts: None,
        messages_caller: None,
        messages_detail: None,
        messages_composer: cyclops_ui::ComposerState::default(),
        avatar_registry: cyclops_ui::AvatarRegistry::default(),
        intake: cyclops_ui::Intake::new(),
        stream_reconciling: false,
        cursor_style: None,
        term_size,
        declared_client_size,
        sizing,
        needs_reconcile: false,
        needs_hydrate: false,
        paste_seq: 0,
        home,
        folder_probe_at: None,
        send_requests: Some(send_request_tx),
        stream_reconcile_requests: Some(stream_reconcile_tx),
        repaint_requested: false,
        repaint_resize_pending: false,
        repaint_resize_settle_at: None,
        messages_focused: false,
        messages_session_scoped: true,
        messages_gate: cyclops_ui::RefreshGate::new(),
        messages_refresh_error: None,
        messages_send_tx: Some(messages_send_tx),
        messages_composer_revision: 0,
        messages_send_in_flight: None,
        messages_snapshot_tx: Some(messages_snapshot_tx),
        message_detail_tx: Some(message_detail_tx),
        message_detail_in_flight: None,
        messages_reconcile_owed: None,
    };
    // Bare `cyclops` can boot a session config.toml never mentions, so the
    // very first frame is already a frame the daemon may not be watching
    // for. Ask before drawing it.
    ensure_sessions_watched(&mut app);
    app.decoration = decoration::fetch_decoration(&app.home).unwrap_or_else(|error| {
        log_err(&app.home, &format!("decoration refresh failed: {error}"));
        DecorationSnapshot::default()
    });
    if app.model.messages_visible {
        app.messages_gate.mark_dirty();
        pump_messages_refresh(&mut app);
    }
    // The subscription at the top of this function is already queuing live
    // entries on the app channel; boot's backfill-then-seed lands before
    // the loop below drains them, which is exactly the order the intake
    // contract wants (crate::event_record's doc).
    if let Some(warning) = crate::event_record::boot(&mut app.record, &mut app.intake, &app.home) {
        app.notice.show(warning, Instant::now());
    }
    if following_at_boot {
        app.notice
            .show(copy::SIZING_FOLLOWER.to_string(), Instant::now());
    }

    let mut debounce: Option<Instant> = None;
    let mut reconnect_deadline: Option<Instant> = None;
    let mut ingress_fairness = IngressFairness::default();
    let mut pending_input: Option<PendingPaneInput> = None;
    let mut input_capacity: Option<InputCapacityFuture> = None;
    let mut pane_input_barrier_target: Option<String> = None;
    let mut pane_input_notice_shown = false;
    // The animation clock (`crate::animate`) sits with the loop's other
    // deadlines rather than in `App`: nothing outside this loop and `draw`
    // reads it, and its wakeups are scheduled here the same one-shot way
    // the render debounce is.
    let mut motion = Motion::new(app.prefs.motion && motion_capable(&app.paint));
    let mut detached = false;
    if let Err(error) = renderer.frame(&mut app, &mut motion, Instant::now()) {
        log_err(&app.home, &error);
    }
    while !detached {
        // Every iteration, because everything that turns the file panel on
        // is somewhere else: the sidebar reopening, the tab going back to
        // Sessions, the menu toggle, the first frame needing a folder at
        // all. Arming from each of those was four places to forget one; the
        // call is a no-op when a probe is already armed or the panel is not
        // on screen, so asking every time is both cheaper to reason about
        // and the only version that cannot go stale.
        arm_files_probe(&mut app);
        let next_deadline = soonest([
            debounce,
            reconnect_deadline,
            app.folder_probe_at,
            app.files_probe_at,
            app.notice.deadline(),
            motion.deadline(),
            // The resize settle window is a one-shot deadline like every
            // other entry here. Each arriving resize REPLACES it, so a
            // drag slides one deadline rather than queueing many, and the
            // loop wakes once when the burst has stopped. It must never
            // be answered by re-arming the render beat: that would be a
            // deadline rescheduling itself, which is the definition of a
            // poll and is exactly what INVARIANTS forbids.
            app.repaint_resize_settle_at,
        ]);
        if pending_input.is_some() && input_capacity.is_none() {
            input_capacity = Some(Box::pin(client.reserve_input_capacity()));
        } else if pending_input.is_none() {
            input_capacity = None;
        }
        let wake = if let Some(capacity) = input_capacity.as_mut() {
            next_pending_input_wake(
                &mut continuity_rx,
                capacity,
                &mut terminal_rx,
                &mut tmux_rx,
                &mut stream_rx,
                !app.stream_reconciling,
                &mut ingress_fairness.background_cursor,
                next_deadline,
            )
            .await
        } else {
            next_wake(
                &mut continuity_rx,
                &mut input_rx,
                &mut paste_rx,
                &mut action_rx,
                &mut terminal_rx,
                &mut tmux_rx,
                &mut stream_rx,
                !app.stream_reconciling,
                &mut ingress_fairness,
                next_deadline,
            )
            .await
        };
        match wake {
            Wake::ControlContinuityLost(barrier) => {
                pane_input_gate.close();
                pane_input_notice_shown = false;
                input_capacity = None;
                let active_pane = app.model.active_tab().active_pane.clone();
                pane_input_barrier_target = Some(active_pane.clone());
                let before_repair = retire_pane_input_segment(
                    &active_pane,
                    &mut pending_input,
                    &mut input_rx,
                    &mut paste_rx,
                );
                while tmux_rx.try_recv().is_ok() {}
                app.needs_forced_hydrate = true;
                // Byte continuity was lost, and the host surface may have lost
                // frame continuity with it.
                app.repaint_requested = true;
                app.hit_map.clear();
                let repair = reconcile(&mut app, &client).await;
                let during_repair = retire_pane_input_segment(
                    &active_pane,
                    &mut pending_input,
                    &mut input_rx,
                    &mut paste_rx,
                );
                let repair = finish_control_cutover(repair, barrier).await;
                let after_cutover = retire_pane_input_segment(
                    &active_pane,
                    &mut pending_input,
                    &mut input_rx,
                    &mut paste_rx,
                );
                if let Some(pane) = before_repair.or(during_repair).or(after_cutover) {
                    show_pane_input_not_sent(
                        &mut app,
                        &pane,
                        &TmuxError::Protocol(copy::CONTROL_STREAM_GAP.into()),
                        &mut debounce,
                    );
                    pane_input_notice_shown = true;
                }
                if repair.is_ok() {
                    pane_input_gate.open();
                }
                settle_control_continuity_repair(&mut app, repair, &mut reconnect_deadline);
                arm(&mut debounce);
            }
            Wake::Message(msg) => {
                if let Some(epoch) = msg.as_deref().and_then(AppMsg::pane_input_epoch) {
                    if !pane_input_gate.accepts(epoch) {
                        if !pane_input_notice_shown {
                            let pane = pane_input_barrier_target
                                .as_deref()
                                .unwrap_or_else(|| app.model.active_tab().active_pane.as_str())
                                .to_string();
                            show_pane_input_not_sent(
                                &mut app,
                                &pane,
                                &TmuxError::Protocol(copy::CONTROL_STREAM_GAP.into()),
                                &mut debounce,
                            );
                            pane_input_notice_shown = true;
                        }
                        continue;
                    }
                }
                if !handle_app_msg(
                    msg.map(|message| *message),
                    &mut app,
                    &mut client,
                    &mut debounce,
                    &mut reconnect_deadline,
                    &mut detached,
                    &mut pending_input,
                )
                .await
                {
                    break;
                }
            }
            Wake::InputCapacity(result) => {
                input_capacity = None;
                let pending = pending_input
                    .take()
                    .expect("capacity wake belongs to one pending input");
                match result {
                    Ok(capacity) => {
                        let keys: Vec<&str> = pending.keys.iter().map(String::as_str).collect();
                        if let Err(error) = client
                            .send_keys_unconfirmed_reserved(&pending.pane, &keys, capacity)
                            .await
                        {
                            let outcome =
                                pane_input_outcome(pending.pane, pending.keys, Err(error));
                            apply_pane_input_outcome(
                                outcome,
                                &mut app,
                                &mut pending_input,
                                &mut detached,
                                &mut debounce,
                            );
                        }
                    }
                    Err(error) => {
                        show_pane_input_not_sent(&mut app, &pending.pane, &error, &mut debounce);
                    }
                }
            }
            Wake::Deadline => {
                let now = Instant::now();
                // A timed-out notice is gone from the next frame, whichever
                // deadline that frame ends up riding on. Checked before the
                // render deadline so the two collapse into one draw.
                let notice_expired = app.notice.expire(now);
                // The highlight and the notice are one confirmation with
                // two halves, so they leave together: the selection that
                // was copied has done its job once the words announcing
                // it are gone, and a highlight nobody cleared reads as
                // state the pane is still in.
                if notice_expired {
                    clear_selection(&mut app);
                }
                // Retire finished fades and arm the next frame. Before the
                // draw below so a motion frame and a render deadline that
                // came due together collapse into one draw.
                let motion_frame = motion.tick(now);
                if apply_settled_resize(&mut app, &client, now).await {
                    arm(&mut debounce);
                }
                if debounce.is_some_and(|deadline| deadline <= now) {
                    debounce = None;
                    let resize_applied = match apply_live_divider(&mut app, &client).await {
                        Ok(applied) => applied,
                        Err(e) => {
                            log_err(&app.home, &e);
                            false
                        }
                    };
                    // The sidebar's own drag, on the same beat as a pane
                    // divider's.
                    apply_live_sidebar(&mut app, &client).await;
                    if app.needs_reconcile {
                        app.needs_reconcile = false;
                        if let Err(e) = reconcile(&mut app, &client).await {
                            log_err(&app.home, &e);
                        }
                    } else if app.needs_hydrate && !resize_applied {
                        app.needs_hydrate = false;
                        hydrate_visible_tab(&client, app.model.active_tab(), &mut app.runtimes)
                            .await;
                    }
                    // A theme edit, or a `cyclops theme <name>`, applies on
                    // this render. A reload the engine refused leaves the
                    // colors alone and hands back a line for workspace.log:
                    // stderr is under the alternate screen here.
                    if let Some(watch) = theme_watch.as_mut() {
                        refresh_theme_watch(&mut app, watch);
                    }
                    if let Err(error) = renderer.frame(&mut app, &mut motion, now) {
                        log_err(&app.home, &error);
                    }
                } else if notice_expired || motion_frame {
                    // Nothing else is due: the expiry or the fade is the
                    // only reason this frame exists, and it owes exactly
                    // one.
                    if let Err(error) = renderer.frame(&mut app, &mut motion, now) {
                        log_err(&app.home, &error);
                    }
                }
                if app.folder_probe_at.is_some_and(|due| due <= now) {
                    app.folder_probe_at = None;
                    if let Err(e) = follow_workspace_folder(&mut app, &client).await {
                        log_err(&app.home, &e);
                    }
                }
                if app.files_probe_at.is_some_and(|due| due <= now) {
                    app.files_probe_at = None;
                    // Only a change earns a frame. The poll runs once a
                    // second and answers "nothing moved" nearly every time;
                    // redrawing on each of those would be a workspace that
                    // repaints forever over a folder nobody touched.
                    if probe_files(&mut app, &client).await {
                        if let Err(error) = renderer.frame(&mut app, &mut motion, Instant::now()) {
                            log_err(&app.home, &error);
                        }
                    }
                }
                if reconnect_deadline.is_some_and(|deadline| deadline <= now) {
                    reconnect_deadline = None;
                    let reconnected = handle_reconnect(
                        &mut app,
                        &mut client,
                        &control_cfg,
                        &sinks,
                        &mut reconnect_deadline,
                    )
                    .await;
                    if reconnected.is_ok()
                        && app.link_state == LinkState::Live
                        && pane_input_gate.is_closed()
                    {
                        let target = pane_input_barrier_target
                            .as_deref()
                            .unwrap_or_else(|| app.model.active_tab().active_pane.as_str())
                            .to_string();
                        if retire_pane_input_segment(
                            &target,
                            &mut pending_input,
                            &mut input_rx,
                            &mut paste_rx,
                        )
                        .is_some()
                            && !pane_input_notice_shown
                        {
                            show_pane_input_not_sent(
                                &mut app,
                                &target,
                                &TmuxError::Protocol(copy::CONTROL_STREAM_GAP.into()),
                                &mut debounce,
                            );
                            pane_input_notice_shown = true;
                        }
                        pane_input_gate.open();
                    }
                    // A fresh instant, not this wake's: reconnecting awaits
                    // the server, so `now` is stale by the time this frame
                    // is composed and the clock would date its fades to
                    // before the work.
                    if let Err(error) = renderer.frame(&mut app, &mut motion, Instant::now()) {
                        log_err(&app.home, &error);
                    }
                }
            }
        }
    }

    drop(renderer);
    drop(guard);
    // Before the link goes, hand back every window this workspace pinned.
    // A manual size is window state and outlives the process that set it,
    // so this is the difference between quitting and leaving the operator's
    // sessions frozen at whatever size this workspace happened to be.
    restore_owned_sizing(&mut app.sizing, &client, &app.home).await;
    client.shutdown().await;
    if detached {
        eprintln!("{}", copy::DETACHED);
    } else if app.link_state == LinkState::ServerGone {
        eprintln!("{}", copy::SERVER_GONE_OFFER);
    }
    0
}

/// The render-deadline half of theme hot reload: re-stat the selection,
/// adopt a change into the live paint, log what a refused reload had to
/// say. Skipped whole while the theme picker is open (`App::theme_restore`
/// is `Some`): the preview owns the paint until the picker closes, and a
/// stamp the watch never polled is still a pending change, so the first
/// refresh after the close adopts whatever happened while browsing. That
/// is how the watch resumes ownership unconditionally.
///
/// A running fade needs nothing here, and the absence is a decision.
/// `crate::animate` stores time and endpoints as scalars, never a color, so
/// the next frame resolves both ends of every blend through the new theme
/// and the fade lands on the new colors instead of chasing one the theme
/// dropped.
fn refresh_theme_watch(app: &mut App, watch: &mut cyclops_theme::ThemeWatch) {
    if app.theme_restore.is_some() {
        return;
    }
    if watch.refresh() {
        app.paint.theme = watch.theme().clone();
    }
    for warning in watch.take_warnings() {
        log_err(&app.home, &format!("theme: {warning}"));
    }
}

async fn reconcile_control_ingress(
    rx: &mut NotificationReceiver,
    continuity_tx: &mpsc::Sender<ControlContinuityBarrier>,
) -> bool {
    let epoch = rx.hold_continuity();
    let (repair_tx, repair_rx) = oneshot::channel();
    let (cutover_tx, cutover_rx) = oneshot::channel();
    if continuity_tx
        .send(ControlContinuityBarrier {
            repair: repair_tx,
            cutover: cutover_rx,
        })
        .await
        .is_err()
    {
        return false;
    }
    match repair_rx.await {
        Ok(true) => {
            let complete = rx.resume_after_reconcile(epoch);
            let _ = cutover_tx.send(complete);
            complete
        }
        Ok(false) | Err(_) => {
            let _ = cutover_tx.send(false);
            false
        }
    }
}

async fn forward_notification_message(tmux_tx: &mpsc::Sender<AppMsg>, message: AppMsg) -> bool {
    tmux_tx.send(message).await.is_ok()
}

fn spawn_notif_forwarder(
    mut rx: NotificationReceiver,
    tmux_tx: mpsc::Sender<AppMsg>,
    continuity_tx: mpsc::Sender<ControlContinuityBarrier>,
) {
    tokio::spawn(async move {
        let mut pending = None;
        loop {
            let notification = match pending.take() {
                Some(notification) => notification,
                None => match rx.recv().await {
                    Some(notification) => notification,
                    None => {
                        let _ = tmux_tx.send(AppMsg::LinkLost).await;
                        break;
                    }
                },
            };
            let notification = match notification {
                Notification::Output { pane, data }
                | Notification::ExtendedOutput { pane, data, .. } => {
                    if data.len() > OUTPUT_BATCH_MAX_BYTES {
                        for chunk in data.chunks(OUTPUT_BATCH_MAX_BYTES) {
                            if !forward_notification_message(
                                &tmux_tx,
                                AppMsg::OutputBatch(vec![(pane.clone(), chunk.to_vec())]),
                            )
                            .await
                            {
                                return;
                            }
                        }
                        continue;
                    }
                    let mut output = Vec::new();
                    let mut output_bytes = data.len();
                    push_output(&mut output, pane, data);
                    while let Ok(next) = rx.try_recv() {
                        match next {
                            Notification::Output { pane, data }
                            | Notification::ExtendedOutput { pane, data, .. } => {
                                if output_bytes.saturating_add(data.len()) > OUTPUT_BATCH_MAX_BYTES
                                {
                                    pending = Some(Notification::Output { pane, data });
                                    break;
                                }
                                output_bytes += data.len();
                                push_output(&mut output, pane, data)
                            }
                            other => {
                                pending = Some(other);
                                break;
                            }
                        }
                    }
                    if matches!(&pending, Some(Notification::ContinuityLost)) {
                        pending = None;
                        if !reconcile_control_ingress(&mut rx, &continuity_tx).await {
                            return;
                        }
                        continue;
                    }
                    if !forward_notification_message(&tmux_tx, AppMsg::OutputBatch(output)).await {
                        return;
                    }
                    continue;
                }
                other => other,
            };
            let message = match notification {
                Notification::ContinuityLost => {
                    if !reconcile_control_ingress(&mut rx, &continuity_tx).await {
                        return;
                    }
                    None
                }
                Notification::Exit { .. } => {
                    let _ = tmux_tx.send(AppMsg::LinkLost).await;
                    break;
                }
                other => structural_message(other),
            };
            if let Some(message) = message {
                if !forward_notification_message(&tmux_tx, message).await {
                    return;
                }
            }
        }
    });
}

/// The app message a structural notification becomes, or `None` when it
/// carries nothing this loop acts on.
///
/// Free standing so the routing can be exercised directly. A notification
/// that silently stops reaching the loop is invisible until a user notices
/// the workspace ignoring something, and one of these arms is load bearing
/// for a correctness property rather than for a redraw.
fn structural_message(notification: Notification) -> Option<AppMsg> {
    match notification {
        Notification::LayoutChange { window, rest } => {
            let mut fields = rest.split_whitespace();
            let layout = fields.next().unwrap_or("").to_string();
            // rest is "layout visible-layout flags"; the flags field
            // carries the zoom marker.
            let flags = fields.nth(1).map(str::to_string);
            Some(AppMsg::LayoutChanged {
                window,
                layout,
                flags,
            })
        }
        Notification::WindowPaneChanged { window, pane } => {
            Some(AppMsg::ActivePaneChanged { window, pane })
        }
        Notification::SessionChanged { session, name } => {
            Some(AppMsg::SessionSwitched { session, name })
        }
        Notification::SessionRenamed { session, name } => {
            Some(AppMsg::SessionRenamed { session, name })
        }
        Notification::WindowAdd { .. }
        | Notification::WindowClose { .. }
        | Notification::WindowRenamed { .. }
        | Notification::SessionsChanged => Some(AppMsg::Reconcile),
        // A client leaving is the edge that can make a sizing owner dead.
        // Without this, a workspace following a session whose owner just
        // quit would keep rendering inside a dead workspace's geometry
        // until something unrelated happened to reconcile. The reconcile
        // path is what re-reads the mark, finds it names a client the
        // server no longer has, and takes the session over.
        Notification::ClientDetached { .. } => Some(AppMsg::Reconcile),
        Notification::Pause { pane } => Some(AppMsg::PanePaused { pane }),
        Notification::Continue { pane } => Some(AppMsg::PaneContinued { pane }),
        _ => None,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What the blocking reader thread below hands to the coalescing loop: one
/// signal per subscription line that carried an `"event"` field, or one
/// signal when the connection ends.
pub enum DecorationSignal {
    Event,
    Closed,
}

/// Why [`coalesce_decoration_signals`] returned: the daemon connection
/// ended (the caller reports offline), or the refresh sink is gone (the
/// app itself is shutting down, nothing left to tell).
#[derive(Debug, PartialEq, Eq)]
pub enum CoalesceEnd {
    Closed,
    SinkGone,
}

/// The decoration burst rule, runnable on its own: a burst of signals
/// coalesces into ONE `refresh` call on a deadline armed by the burst's
/// FIRST signal — later signals never push it back (rule 9's arm-once
/// shape, `recv_timeout` instead of `tokio::select!` because this loop
/// deliberately lives on a blocking thread). `refresh` returns false when
/// its sink is gone, which ends the loop the way a lost app channel ends
/// the production forwarder.
///
/// Public so the performance contract can drive a measured burst through
/// the real loop (src/cyclops-workspace/tests/perf_contract.rs) instead
/// of a re-implementation that could drift; production behavior is
/// unchanged, `spawn_decoration_forwarder` calls exactly this.
pub fn coalesce_decoration_signals(
    sig_rx: std::sync::mpsc::Receiver<DecorationSignal>,
    debounce: Duration,
    mut refresh: impl FnMut() -> bool,
) -> CoalesceEnd {
    // One-shot deadline armed by the first signal in a burst; `None`
    // means idle, exactly like `debounce` in the main loop.
    let mut deadline: Option<std::time::Instant> = None;
    loop {
        let signal = match deadline {
            None => sig_rx.recv().ok(),
            Some(at) => {
                let wait = at.saturating_duration_since(std::time::Instant::now());
                match sig_rx.recv_timeout(wait) {
                    Ok(signal) => Some(signal),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        deadline = None;
                        if !refresh() {
                            return CoalesceEnd::SinkGone;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
                }
            }
        };
        match signal {
            Some(DecorationSignal::Event) => {
                // Arm once per burst; a signal that arrives while
                // already armed must never push the deadline back.
                if deadline.is_none() {
                    deadline = Some(std::time::Instant::now() + debounce);
                }
            }
            Some(DecorationSignal::Closed) | None => return CoalesceEnd::Closed,
        }
    }
}

/// Event-driven daemon decoration updates, and (E2) the shared `cyclops
/// watch` stream feed. The subscription itself never polls. A split or a
/// border drag pushes several state/label/delivery events through
/// cyclopsd at once; a dedicated reader thread turns each subscription
/// line into one [`DecorationSignal`] on a single-slot channel, and
/// [`subscribe_decoration_once`] coalesces a burst of them into ONE status
/// fetch instead of one per event — the same arm-once, never-push-back
/// rule `arm()` and `RENDER_DEBOUNCE` use for rendering
/// (`DECORATION_DEBOUNCE`), just built on
/// `std::sync::mpsc::Receiver::recv_timeout` instead of `tokio::select!`
/// because this connection is deliberately blocking IO on its own thread,
/// away from the input loop.
///
/// That same reader thread also normalizes each event line into a
/// [`cyclops_ui::Entry`] and sends it straight to the app as
/// [`AppMsg::StreamEntry`], bypassing the coalescing above entirely.
/// Feeding the record is in-memory normalization with no IO of its own —
/// unlike a status fetch, there is nothing to debounce — so folding it
/// into the burst logic would only add latency the decoration path does
/// not have today. This is the ONE connection both concerns share; see
/// the module doc and `docs/development/INVARIANTS.md` rule 9.
///
/// The subscription outlives the daemon: [`run_decoration_forwarder`]
/// reconnects on a bounded backoff, so a daemon restart or a boot-order
/// race costs an outage, never the thread.
fn spawn_decoration_forwarder(
    home: std::path::PathBuf,
    control_tx: mpsc::Sender<AppMsg>,
    stream_tx: mpsc::Sender<AppMsg>,
) {
    std::thread::spawn(move || {
        run_decoration_forwarder(&home, &control_tx, &stream_tx, resilience::reconnect_delay);
    });
}

/// Why one subscription connection ended, deciding what the reconnect
/// loop does next.
#[derive(Debug, PartialEq, Eq)]
enum SubscribeEnd {
    /// Nothing answered on the socket, or the handshake failed.
    ConnectFailed,
    /// A live subscription ended: the daemon went away mid-run.
    Closed,
    /// The app channel is gone: the workspace itself is shutting down.
    SinkGone,
}

/// The forwarder's whole life: subscribe, forward until the connection
/// ends, reconnect on a bounded backoff, repeat. Unlike the tmux link
/// there is no give-up state: the workspace works without its daemon and
/// the daemon restarts routinely (upgrades, crashes), so the subscription
/// must still be waiting whenever it returns; a dead thread here is a
/// status indicator that only moves on structural reconciles. The backoff
/// sleeps are reconnects only, never state polls (the same bound the
/// daemon's own watcher states); while connected the loop rides
/// subscription events alone, so rule 9 holds.
///
/// Offline is reported once per outage, and only after the chain has
/// failed [`resilience::RECONNECT_CAP`] times: an outage shorter than the
/// chain keeps the last decoration on screen instead of un-naming every
/// agent for a restart the user never noticed (doubt vs. news, the
/// refresh closure's rule). The empty snapshot paints the sidebar's
/// "cyclopsd offline" line and drops the watch record so recovery
/// re-asks; one workspace.log line says the same. `delay_for` is injected
/// so tests can run the chain in milliseconds; production passes
/// [`resilience::reconnect_delay`], which clamps at its last entry.
fn run_decoration_forwarder(
    home: &std::path::Path,
    control_tx: &mpsc::Sender<AppMsg>,
    stream_tx: &mpsc::Sender<AppMsg>,
    delay_for: impl Fn(usize) -> std::time::Duration,
) {
    let mut attempt = 0usize;
    let mut reported_offline = false;
    loop {
        match subscribe_decoration_once(home, control_tx, stream_tx) {
            SubscribeEnd::SinkGone => return,
            // A connection existed, so this outage starts its own chain.
            SubscribeEnd::Closed => {
                attempt = 0;
                reported_offline = false;
            }
            SubscribeEnd::ConnectFailed => attempt = attempt.saturating_add(1),
        }
        if !resilience::may_retry(attempt) && !reported_offline {
            reported_offline = true;
            if control_tx
                .blocking_send(AppMsg::DecorationChanged(DecorationSnapshot::default()))
                .is_err()
            {
                return;
            }
            log_err(
                home,
                &format!(
                    "cyclopsd is not answering after {} reconnect attempts; \
                     agent decoration is offline until it returns (still retrying)",
                    resilience::RECONNECT_CAP
                ),
            );
        }
        // The workspace shutting down closes the channel; a retry loop
        // with nobody left to tell stops instead of probing a dead socket.
        if control_tx.is_closed() || stream_tx.is_closed() {
            return;
        }
        // `attempt` counts completed failures; the sleep indexes the retry
        // about to run, so a cold boot's chain starts at RECONNECT_ATTEMPT_1
        // exactly like a chain that follows a live close.
        std::thread::sleep(delay_for(attempt.saturating_sub(1)));
    }
}

/// One subscription connection, end to end: the Hello-first handshake,
/// the `events.subscribe` request, a resync ask, then the reader thread
/// and the coalescing loop until the connection or the app ends. See
/// [`spawn_decoration_forwarder`] for what the reader and the coalescer
/// each carry.
fn subscribe_decoration_once(
    home: &std::path::Path,
    control_tx: &mpsc::Sender<AppMsg>,
    stream_tx: &mpsc::Sender<AppMsg>,
) -> SubscribeEnd {
    let socket = home.join(cyclops_proto::SOCK_NAME);
    let mut client =
        match BlockingClient::connect_path(socket, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT) {
            Ok(client) => client,
            Err(ClientError::NotRunning(_)) => return SubscribeEnd::ConnectFailed,
            Err(error) => {
                log_err(
                    home,
                    &format!("cyclopsd event subscription could not connect: {error}"),
                );
                return SubscribeEnd::ConnectFailed;
            }
        };
    if stream_tx
        .blocking_send(AppMsg::DaemonCompatibility(client.hello_compatibility()))
        .is_err()
    {
        return SubscribeEnd::SinkGone;
    }
    if let Err(error) = client.subscribe(serde_json::json!({})) {
        log_err(
            home,
            &format!("cyclopsd did not establish the event subscription: {error}"),
        );
        return SubscribeEnd::ConnectFailed;
    }
    // Subscriptions are idle by design. Request deadlines protect the
    // handshake above; after acknowledgement, EOF or a malformed frame is the
    // evidence of a gap, not the absence of traffic for five seconds.
    client.clear_read_timeout();
    // Subscribed. Ask the app to resync before any event arrives on this
    // connection: everything that changed while nothing was subscribed
    // produced no event, so without this a state flip during the outage
    // stays on screen as stale.
    // The prior connection reports its gap on this same FIFO before it
    // releases the reconnect loop. Keeping the new acknowledgement here
    // prevents a delayed old gap from invalidating this connection.
    if stream_tx.blocking_send(AppMsg::DaemonReconnected).is_err() {
        return SubscribeEnd::SinkGone;
    }

    let (sig_tx, sig_rx) = std::sync::mpsc::sync_channel::<DecorationSignal>(1);
    let stream_tx = stream_tx.clone();
    std::thread::spawn(move || loop {
        let ev = match client.next_event() {
            Ok(frame) => frame.event,
            Err(error) => {
                let why = match &error {
                    ClientError::Gap(cause) if cause == "the connection closed" => {
                        "daemon event subscription closed".to_string()
                    }
                    ClientError::Gap(cause) if cause.starts_with("malformed event frame: ") => {
                        cause.replacen(
                            "malformed event frame: ",
                            "malformed daemon event JSON: ",
                            1,
                        )
                    }
                    _ => error.cause(),
                };
                report_stream_gap(&stream_tx, &sig_tx, why);
                return;
            }
        };
        // E2: normalize this same line onto the shared stream model
        // before the decoration signal below. Cheap (no IO) and
        // per-event on purpose. It must never wait for or extend the
        // coalescing deadline that follows. An unreadable event closes
        // this subscription so the app reconciles before reconnecting.
        // A theme reload is not a fact about the record; the CLI stream
        // drops it the same way (`cyclops_ui`'s own subscribe loop). It
        // still becomes a wake-only `ThemeChanged`. Every other vocabulary,
        // known or not, becomes an entry.
        if ev.event == "messages.changed" {
            let data = serde_json::from_value::<cyclops_proto::MessagesChangedData>(ev.data).ok();
            if stream_tx
                .blocking_send(AppMsg::MessagesChanged(data))
                .is_err()
            {
                return;
            }
        } else if ev.event != "theme" {
            let entry = cyclops_ui::Entry::from_event(&ev, now_ms());
            if stream_tx
                .blocking_send(AppMsg::StreamEntry(Box::new(entry)))
                .is_err()
            {
                return;
            }
        } else if stream_tx.blocking_send(AppMsg::ThemeChanged).is_err() {
            return;
        }
        if !signal_decoration_event(&sig_tx) {
            return;
        }
    });

    let end = coalesce_decoration_signals(sig_rx, DECORATION_DEBOUNCE, || {
        // A refused or timed-out status call is doubt about this
        // instant, not news about the roster: the subscription is
        // still up, so the next burst asks again. Dropping it keeps
        // the last known decoration on screen instead of un-naming
        // every agent for a frame. A daemon that is really gone ends
        // the read loop above; whether that becomes "offline" on
        // screen is the reconnect chain's call in
        // [`run_decoration_forwarder`], the one place it is reported.
        match decoration::fetch_decoration(home) {
            Ok(snapshot) => control_tx
                .blocking_send(AppMsg::DecorationChanged(snapshot))
                .is_ok(),
            Err(error) => {
                log_err(home, &format!("decoration refresh failed: {error}"));
                true
            }
        }
    });
    match end {
        CoalesceEnd::Closed => SubscribeEnd::Closed,
        CoalesceEnd::SinkGone => SubscribeEnd::SinkGone,
    }
}

fn report_stream_gap(
    stream_tx: &mpsc::Sender<AppMsg>,
    sig_tx: &std::sync::mpsc::SyncSender<DecorationSignal>,
    why: impl Into<String>,
) {
    let _ = stream_tx.blocking_send(AppMsg::StreamGap { why: why.into() });
    let _ = sig_tx.send(DecorationSignal::Closed);
}

/// One pending edge is enough to arm the coalescer. Further edges before it
/// drains carry no additional state and are intentionally collapsed.
fn signal_decoration_event(tx: &std::sync::mpsc::SyncSender<DecorationSignal>) -> bool {
    match tx.try_send(DecorationSignal::Event) {
        Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => true,
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
    }
}

/// Coalesce only adjacent output for the same pane. Keeping separate entries
/// across a pane switch preserves the control stream's global order.
fn push_output(output: &mut Vec<(String, Vec<u8>)>, pane: String, bytes: Vec<u8>) {
    if let Some((id, pending)) = output
        .last_mut()
        .filter(|(id, _)| id.as_str() == pane.as_str())
    {
        debug_assert_eq!(id, &pane);
        pending.extend(bytes);
    } else {
        output.push((pane, bytes));
    }
}

fn schedule_reconnect(app: &mut App, reconnect_deadline: &mut Option<Instant>) {
    if !resilience::may_retry(app.reconnect_attempt) {
        app.link_state = LinkState::ServerGone;
        return;
    }
    app.link_state = LinkState::Reconnecting {
        attempt: app.reconnect_attempt,
    };
    let delay = resilience::reconnect_delay(app.reconnect_attempt);
    *reconnect_deadline = Some(Instant::now() + delay);
}

fn settle_control_continuity_repair(
    app: &mut App,
    repair: Result<(), TmuxError>,
    reconnect_deadline: &mut Option<Instant>,
) -> bool {
    match repair {
        Ok(()) => {
            app.needs_reconcile = false;
            true
        }
        Err(error) => {
            log_err(&app.home, &error);
            app.needs_reconcile = true;
            schedule_reconnect(app, reconnect_deadline);
            false
        }
    }
}

async fn finish_control_cutover(
    repair: Result<(), TmuxError>,
    barrier: ControlContinuityBarrier,
) -> Result<(), TmuxError> {
    let snapshot_complete = repair.is_ok();
    let cutover_complete = if barrier.repair.send(snapshot_complete).is_ok() {
        barrier.cutover.await.unwrap_or(false)
    } else {
        false
    };
    match repair {
        Err(error) => Err(error),
        Ok(()) if cutover_complete => Ok(()),
        Ok(()) => Err(TmuxError::Protocol(
            "control events crossed the hydration cutover".into(),
        )),
    }
}

fn prepare_forced_hydration(app: &mut App) {
    // Pause and Continue are stream edges, not snapshot fields. Once an
    // edge was lost, no retained pause can be trusted. Forced capture
    // rebuilds the display without that transient gate.
    app.paused_panes.clear();
}

async fn handle_reconnect(
    app: &mut App,
    client: &mut ControlClient,
    cfg: &ControlConfig,
    sinks: &AppSinks,
    reconnect_deadline: &mut Option<Instant>,
) -> Result<(), cyclops_tmux::TmuxError> {
    client.shutdown().await;
    let cfg = reconnect_config(cfg, &app.model.session.session);
    match ControlClient::spawn(cfg).await {
        Ok((new_client, rx)) => {
            *client = new_client;
            spawn_notif_forwarder(rx, sinks.tmux.clone(), sinks.continuity.clone());
            app.declared_client_size = None;
            // A reconnect is a different tmux client with a different
            // identity, so every mark this workspace left names a client
            // that no longer exists. Move them onto the new identity here,
            // at the seam, rather than letting the sessions this process is
            // not currently displaying sit pinned with nobody owning them.
            rekey_ownership(&mut app.sizing, client, &app.home).await;
            resize_client(app, client).await;
            // The gap this flag exists for: %output missed while the link
            // was down, with no size change to mark any pane stale.
            app.needs_forced_hydrate = true;
            // Byte continuity was lost, and the host surface may have lost
            // frame continuity with it.
            app.repaint_requested = true;
            if let Err(error) = reconcile(app, client).await {
                app.reconnect_attempt += 1;
                schedule_reconnect(app, reconnect_deadline);
                return Err(error);
            }
            app.link_state = LinkState::Live;
            app.reconnect_attempt = 0;
        }
        Err(_) => {
            app.reconnect_attempt += 1;
            if resilience::may_retry(app.reconnect_attempt) {
                schedule_reconnect(app, reconnect_deadline);
            } else {
                app.link_state = LinkState::ServerGone;
                let _ = sinks.tmux.send(AppMsg::Redraw).await;
            }
        }
    }
    Ok(())
}

/// The chrome split one frame of `model` under `prefs` composes to.
///
/// The one place chrome geometry is derived, called by boot (to declare the
/// tmux client size) and by every frame (to paint). Both must read the same
/// inputs or the declaration and the paint disagree by the sidebar's width
/// — the collapsed-at-boot case, and the bug class of 626ec09.
fn chrome_for(
    area: Rect,
    model: &WorkspaceModel,
    prefs: &WorkspacePrefs,
) -> crate::render::ChromeAreas {
    crate::render::chrome_areas_for(
        area,
        model.sidebar_visible,
        prefs.sidebar_width.max(crate::render::SIDEBAR_MIN_WIDTH),
        prefs.tab_bar_visible,
        model.messages_visible,
        prefs.messages_width.max(crate::render::MESSAGES_MIN_WIDTH),
    )
}

/// Shared tmux target for boot and every later size declaration.
fn desired_tmux_size(area: Rect, model: &WorkspaceModel, prefs: &WorkspacePrefs) -> (u16, u16) {
    crate::render::tmux_client_size(
        chrome_for(area, model, prefs).tmux_sizing_canvas(),
        model.active_tab(),
    )
}

/// Perform the first shared tmux size declaration from persisted chrome.
///
/// Boot and its nested-tmux regression both enter through this seam, so the
/// initial write cannot silently drift from the Messages-independent target
/// used by every later [`resize_client`] call.
async fn declare_initial_client_size(
    term_size: (u16, u16),
    model: &WorkspaceModel,
    prefs: &WorkspacePrefs,
    sizing: &WindowSizing,
    client: &ControlClient,
    home: &std::path::Path,
) -> Option<(u16, u16)> {
    let size = desired_tmux_size(Rect::new(0, 0, term_size.0, term_size.1), model, prefs);
    if !declarable(size) {
        return None;
    }
    let canvas =
        chrome_for(Rect::new(0, 0, term_size.0, term_size.1), model, prefs).tmux_sizing_canvas();
    let outcome = size_owned_windows(sizing, client, canvas, &model.session.tabs, home).await;
    if outcome.all_succeeded() {
        Some(size)
    } else {
        None
    }
}

/// Whether the workspace may animate (`crate::animate`). Capability first,
/// then intent: the first two are not preferences, they are "there is
/// nothing to fade".
///
/// 1. Colors off, which `Paint::detect` collapses `NO_COLOR` and a non-tty
///    stdout into. Every token then resolves to the same empty style, so
///    both endpoints of every blend are identical and a fade would be a
///    no-op with a timer behind it.
/// 2. No truecolor. An interpolated color resolves to the nearest 256-cube
///    entry, and the whole dim-to-accent path collapses to four or five of
///    them, so an eight-frame fade shows four steps. Banding is worse than
///    a snap. Note the conservatism: `Paint::detect` tests `COLORTERM ==
///    "truecolor"` exactly, so a terminal advertising `24bit` gets no
///    motion. That is existing detection and it fails in the safe
///    direction.
/// 3. `CYCLOPS_MOTION`, when it parses. Anything else is ignored rather
///    than treated as off, the way `load_prefs` treats a value it cannot
///    read.
/// 4. Default on. Contingent on what animates, not on the mechanism:
///    nothing translates, scales or scrolls, nothing repeats, and the
///    fastest fade completes in 120ms. A moving rectangle flips this
///    default in the same commit that adds it.
///
/// The `[workspace] motion` preference sits between 3 and 4 and is applied
/// separately, in `draw`, through `Motion::set_preference`. It is not read
/// here because this function answers "can this terminal fade" and the
/// preference answers "should it", and the two must not be able to
/// re-enable each other: a preference that could switch motion back on for
/// a terminal without truecolor would paint the banding this rejects.
fn motion_capable(paint: &Paint) -> bool {
    if !paint.colors_enabled() {
        return false;
    }
    if !paint.truecolor {
        return false;
    }
    match std::env::var("CYCLOPS_MOTION")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "0" | "false" | "off" => false,
        "1" | "true" | "on" => true,
        _ => true,
    }
}

/// What this frame shows, as the animation clock's diff input.
///
/// `&App`, never `&mut`, and the borrow is the guard: `observe` arms from
/// the draw path, so a draw that wrote something this function reads back
/// would let the clock sustain itself with no event behind it, which is the
/// one way this design could break rule 9 silently.
fn observed(app: &App) -> Seen {
    let tab = app.model.active_tab();
    let mut status: Vec<(String, StatusInk)> = Vec::new();
    let mut note = |pane_id: &str, dec: &decoration::PaneDecoration| {
        if status.iter().any(|(id, _)| id == pane_id) {
            return;
        }
        if let Some(ink) = status_ink(dec) {
            status.push((pane_id.to_string(), ink));
        }
    };
    // The pane titles this tab paints (`render::canvas`).
    for pane_id in crate::layout::pane_ids_in_layout(&tab.layout) {
        if let Some(dec) = app.decoration.pane(&pane_id) {
            note(&pane_id, dec);
        }
    }
    // Plus the sidebar's agent rows, which can name a pane in a tab this
    // window is not showing (`render::sidebar`). Same key space, so an
    // agent on screen twice settles once. Rows come from the function the
    // sidebar itself calls, so the two cannot disagree about which rows
    // exist.
    for workspace in &app.model.workspaces {
        if !app.expanded_workspaces.contains(&workspace.session_id) {
            continue;
        }
        for dec in app
            .decoration
            .agent_rows_for_window_ids(&workspace.window_ids, &app.prefs.agent_order)
        {
            note(&dec.pane_id, dec);
        }
    }
    Seen::new(Some(tab.active_pane.clone()), status, app.notice.deadline())
}

/// The semantic source of a pane's status ink, or `None` for a pane with no
/// status cell to paint. Never a resolved color: the painter resolves both
/// endpoints through the live theme on every frame, which is what makes a
/// theme change mid-fade land on the new colors.
fn status_ink(dec: &decoration::PaneDecoration) -> Option<StatusInk> {
    if dec.needs_attention {
        return Some(StatusInk::Eye);
    }
    DecorationSnapshot::primary_status(dec).map(|status| StatusInk::State(status.color_state))
}

/// A workspace can switch or rename sessions after boot. Reconnection must
/// follow the model's current target, never the name captured at startup.
fn reconnect_config(base: &ControlConfig, session: &str) -> ControlConfig {
    let mut cfg = base.clone();
    cfg.session = session.to_string();
    cfg
}

impl App {
    fn daemon_compatibility_marker(&self) -> Option<&'static str> {
        match self.daemon_compatibility.as_ref()? {
            cyclops_client::HelloCompatibility::Current { .. } => None,
            cyclops_client::HelloCompatibility::Mismatch { .. } => Some("daemon mismatch"),
            cyclops_client::HelloCompatibility::UnverifiedDaemon { .. } => {
                Some("daemon unverified")
            }
        }
    }

    /// Put a dialog on screen at its resting center.
    ///
    /// Every open goes through here so the drag offset is cleared with it.
    /// A box that reopened where the last one was dragged to would look
    /// misplaced rather than moved: the operator moved that dialog, not
    /// this one.
    pub(crate) fn open_dialog(&mut self, dialog: Dialog) {
        self.dialog = Some(dialog);
        self.dialog_offset = (0, 0);
    }

    fn is_visible_pane(&self, pane: &str) -> bool {
        pane_is_visible(self.model.active_tab(), pane)
    }

    /// The file browser the panel is showing. Everything the operator does
    /// to "the file panel" — clicks, keys, the wheel, painting — goes
    /// through these two, so a view switch swaps all of it at once.
    fn files_tree(&self) -> &crate::files::FileTree {
        match self.files_view {
            crate::files::FilesView::Agent => &self.files,
            crate::files::FilesView::Pinned => &self.files_pinned,
        }
    }

    fn files_tree_mut(&mut self) -> &mut crate::files::FileTree {
        match self.files_view {
            crate::files::FilesView::Agent => &mut self.files,
            crate::files::FilesView::Pinned => &mut self.files_pinned,
        }
    }

    fn chrome(&self, area: Rect) -> crate::render::ChromeAreas {
        chrome_for(area, &self.model, &self.prefs)
    }

    fn persist_active(&self) {
        let tab = self.model.active_tab();
        set_last_active(&self.home, &self.model.session.session, &tab.window_id);
    }

    /// Write the prefs, logging a failure instead of surfacing it: prefs
    /// are comfort, and no preference is worth interrupting the operator's
    /// session over. Every save goes through here so that trade-off is
    /// decided once.
    /// A chrome surface appeared, vanished, or changed the space it owns.
    ///
    /// The cells the previous layout occupied still hold its glyphs until
    /// something writes them, and a diff frame writes only what the new
    /// layout believes changed, so a collapsed sidebar or Messages pane can
    /// leave its own contents behind. Named rather than folded into
    /// `resize_client` because telling tmux a new size and repainting a
    /// surface are different jobs. Messages visibility and width mutations
    /// change only local geometry, while shared-sizing mutations may also
    /// call `resize_client`.
    pub(crate) fn layout_changed(&mut self) {
        self.repaint_requested = true;
    }

    fn save_prefs_or_log(&self) {
        if let Err(error) = persist::save_prefs(&self.home, &self.prefs) {
            log_err(&self.home, &error);
        }
    }

    fn open_menu(&mut self, menu: MenuState) {
        self.menu = menu;
        self.hover = None;
        self.hit_map.clear_menu_items();
    }

    fn close_menu(&mut self) {
        self.menu.close();
        self.hover = None;
        self.hit_map.clear_menu_items();
    }
}

/// Append one error line to `<home>/workspace.log`. The alternate screen
/// owns stderr while the workspace runs; printing there corrupts the frame.
fn log_err(home: &std::path::Path, err: &dyn std::fmt::Display) {
    use std::io::Write;
    if let Ok(root) = cyclops_state::StateRoot::open_or_create(home) {
        let Ok(mut f) = root.open_append(std::path::Path::new("workspace.log")) else {
            return;
        };
        let _ = writeln!(f, "{} {err}", chrono_stamp());
    }
}

fn chrono_stamp() -> String {
    // Seconds since the epoch; enough to correlate with daemon logs.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("[{now}]")
}

enum InputOutcome {
    Detached,
    Redraw,
    NoRedraw,
    Pending(PendingPaneInput),
    /// The first write failed before the event entered the pending-input
    /// state. It carries target context for one visible notice, but no key
    /// bytes, so reconnect cannot replay an event with an unknown outcome.
    NotSent {
        pane: String,
        error: TmuxError,
    },
    /// The write began but did not complete. No key bytes are retained and
    /// reconnect never replays them, but the notice does not claim that tmux
    /// received nothing.
    Uncertain {
        pane: String,
        error: TmuxError,
    },
}

fn show_pane_input_not_sent(
    app: &mut App,
    pane: &str,
    error: &TmuxError,
    debounce: &mut Option<Instant>,
) {
    log_err(&app.home, error);
    app.notice
        .show(copy::pane_input_not_sent(pane, error), Instant::now());
    arm(debounce);
}

fn show_pane_input_uncertain(
    app: &mut App,
    pane: &str,
    error: &TmuxError,
    debounce: &mut Option<Instant>,
) {
    log_err(&app.home, error);
    app.notice
        .show(copy::pane_input_uncertain(pane, error), Instant::now());
    arm(debounce);
}

fn apply_pane_input_outcome(
    outcome: InputOutcome,
    app: &mut App,
    pending_input: &mut Option<PendingPaneInput>,
    detached: &mut bool,
    debounce: &mut Option<Instant>,
) {
    match outcome {
        InputOutcome::Detached => *detached = true,
        InputOutcome::Redraw => arm(debounce),
        InputOutcome::NoRedraw => {}
        InputOutcome::Pending(pending) => {
            debug_assert!(pending_input.is_none());
            *pending_input = Some(pending);
        }
        InputOutcome::NotSent { pane, error } => {
            show_pane_input_not_sent(app, &pane, &error, debounce);
        }
        InputOutcome::Uncertain { pane, error } => {
            show_pane_input_uncertain(app, &pane, &error, debounce);
        }
    }
}

/// The smallest grid worth declaring to tmux. Below this the terminal is
/// nearly all chrome, and declaring the leftover sliver would reshape
/// every pane in the session to fit it. Boot and every later resize must
/// apply the same floor: if they disagree, a terminal declarable at boot
/// stops being declarable on the first resize, or the reverse, and the
/// panes are painted for a size tmux was never told about.
const MIN_DECLARABLE_SIZE: (u16, u16) = (10, 3);

fn declarable(size: (u16, u16)) -> bool {
    size.0 >= MIN_DECLARABLE_SIZE.0 && size.1 >= MIN_DECLARABLE_SIZE.1
}

/// Which sessions this workspace sizes, and what it owes them back.
///
/// A window's size is its panes' size, so sizing is not a viewer's private
/// business: it reshapes every agent running in that session. Exactly one
/// workspace per session therefore writes sizes, and the rest render inside
/// whatever it chose. `sizing.rs` holds the tmux side and the measurements
/// behind it; this holds what one process remembers.
///
/// Ownership is per session and lasts for the life of the process, not for
/// the life of a view. A workspace that navigates from a session keeps one
/// connection and one identity, so it vanishes from that session's client
/// list while remaining alive; re-electing on that would hand a session to
/// whoever glanced at it next and would put its windows back while its
/// owner was still using them.
#[derive(Debug, Default)]
pub struct WindowSizing {
    /// This connection's identity, read once. A reconnect is a new client
    /// and therefore a new identity, so this is dropped with the old link.
    pub identity: Option<ClientIdentity>,
    /// Sessions owned, each with what this workspace holds in it. Ordered
    /// so a restore visits them the same way twice.
    pub owned: BTreeMap<String, OwnedSession>,
    /// Sessions found already owned by a live workspace. Kept so a follower
    /// asks tmux once rather than on every reconcile.
    pub following: BTreeSet<String>,
}

/// What this workspace holds in one session it owns.
#[derive(Debug, Default)]
pub struct OwnedSession {
    /// Windows this workspace pinned, and therefore must put back.
    pub pinned: BTreeSet<String>,
    /// Windows carrying a record this version cannot read.
    ///
    /// Never pinned by this workspace and never changed by it, and yet the
    /// reason the session stays owned. A window already on `manual` with an
    /// unreadable record is exactly the state that cannot recover on its
    /// own, and releasing the mark over it is what strands it: no policy
    /// applies, no owner exists, and no later workspace can tell what it
    /// was. Holding the mark keeps it visibly somebody's problem.
    pub blocked: BTreeSet<String>,
}

impl OwnedSession {
    /// Whether this session may be handed back. A window whose original is
    /// unknowable is not a window that can be put back.
    fn releasable(&self) -> bool {
        self.blocked.is_empty()
    }
}

impl WindowSizing {
    fn owns(&self, session: &str) -> bool {
        self.owned.contains_key(session)
    }

    pub(crate) fn has_window_authority(&self, session: &str, window_id: &str) -> bool {
        self.owned
            .get(session)
            .is_some_and(|o| o.pinned.contains(window_id))
    }
}

/// This connection's identity, read once and remembered.
async fn sizing_identity(
    sizing: &mut WindowSizing,
    client: &ControlClient,
    home: &std::path::Path,
) -> Option<ClientIdentity> {
    if let Some(identity) = &sizing.identity {
        return Some(identity.clone());
    }
    match client.client_identity().await {
        Ok(identity) => {
            sizing.identity = Some(identity.clone());
            Some(identity)
        }
        Err(error) => {
            log_err(home, &error);
            None
        }
    }
}

/// Whether this workspace sizes `session`, claiming it when nobody live
/// does.
///
/// Fails closed everywhere: an unreadable mark, an unreadable client list,
/// or a lost race all answer false, and a workspace that answers false
/// writes no sizes at all. The cost of a wrong false is that a session
/// keeps the size it already had; the cost of a wrong true is two
/// workspaces fighting over every pane in it.
async fn owns_session(
    sizing: &mut WindowSizing,
    client: &ControlClient,
    session: &str,
    home: &std::path::Path,
) -> bool {
    if sizing.owns(session) {
        return true;
    }
    let Some(identity) = sizing_identity(sizing, client, home).await else {
        return false;
    };
    let marker = identity.marker();
    let held = match client.window_driver(session).await {
        Ok(held) => held,
        Err(error) => {
            log_err(home, &error);
            return false;
        }
    };
    let won = match held {
        // Nobody has it. The claim is create-only, so a race is decided by
        // tmux rather than by who read first.
        None => client.claim_window_driver(session, &marker).await,
        // Already ours: a reconcile after we claimed, not a new election.
        Some(held) if held == marker => Ok(true),
        Some(held) => {
            // Server-wide, never this session's client list. An owner that
            // navigated to another session is absent from this session's
            // list while still alive and still sizing these windows
            // (F76, M12); testing liveness there would steal the session
            // out from under a live workspace.
            let live = match client.server_client_markers().await {
                Ok(live) => live,
                Err(error) => {
                    log_err(home, &error);
                    return false;
                }
            };
            if live.contains(&held) {
                // A live owner. Follow it, and say so once.
                sizing.following.insert(session.to_string());
                return false;
            }
            client
                .take_over_window_driver(session, &held, &marker)
                .await
        }
    };
    match won {
        Ok(true) => {
            sizing.following.remove(session);
            sizing.owned.entry(session.to_string()).or_default();
            true
        }
        Ok(false) => {
            sizing.following.insert(session.to_string());
            false
        }
        Err(error) => {
            log_err(home, &error);
            false
        }
    }
}

/// Record what each displayed window's sizing policy was, then take it off
/// every policy so only this workspace moves it.
///
/// The order is the whole point and it is not an implementation detail: a
/// capture without a pin restores to what is already there, while a pin
/// without a capture loses the window's original policy permanently. A
/// window that fails stays unowned so the next reconcile retries it.
/// What one adoption pass changed, for the caller that has to react to it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Adopted {
    /// This call was the one that found another workspace owns the session,
    /// so exactly one notice is shown for it.
    newly_following: bool,
    /// At least one window was pinned that was not pinned before, so it is
    /// carrying whatever size it had rather than this workspace's canvas.
    took_a_window: bool,
    /// True when this client transitioned from follower to authoritative sizing owner.
    authority_transferred: bool,
}

/// Take ownership of a session's displayed windows: record what each one's
/// sizing policy was, then take it off every policy so only this workspace
/// moves it.
async fn adopt_windows(
    sizing: &mut WindowSizing,
    client: &ControlClient,
    session: &str,
    tabs: &[TabModel],
    home: &std::path::Path,
) -> Adopted {
    let followed_before = sizing.following.contains(session);
    if !owns_session(sizing, client, session, home).await {
        return Adopted {
            newly_following: !followed_before && sizing.following.contains(session),
            took_a_window: false,
            authority_transferred: false,
        };
    }
    // A window that has been closed is not owned any more, and there is
    // nothing left to restore on it. Dropping it here keeps the exit path
    // from asking tmux about windows that no longer exist. Re-adopting one
    // that only looked absent is safe: the capture is create-only, so its
    // original survives a second pass.
    let displayed: BTreeSet<String> = tabs.iter().map(|tab| tab.window_id.clone()).collect();
    if let Some(owned) = sizing.owned.get_mut(session) {
        owned
            .pinned
            .retain(|window_id| displayed.contains(window_id));
        owned
            .blocked
            .retain(|window_id| displayed.contains(window_id));
    }
    let owned = sizing.owned.entry(session.to_string()).or_default();
    // Blocked windows are deliberately not excluded here. They are cheap to
    // re-read, this workspace never pinned them, and if the record they
    // carry is ever repaired the next pass adopts them properly instead of
    // ignoring them for the life of the process.
    let fresh: Vec<String> = unpinned_windows(tabs, &owned.pinned)
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut took_a_window = false;
    for window_id in fresh {
        match client.capture_prior_window_size(&window_id).await {
            Ok(cyclops_tmux::Captured::Record(_)) => {}
            Ok(cyclops_tmux::Captured::Malformed) => {
                // Not pinned, not written to, and not forgotten. Forgetting
                // it is what used to release the session's mark over a
                // window that was already pinned and unreadable, which is
                // the one state nothing recovers from.
                let owned = sizing.owned.entry(session.to_string()).or_default();
                if owned.blocked.insert(window_id.clone()) {
                    log_err(
                        home,
                        &format!(
                            "{window_id}: sizing record unreadable, so this workspace will not \
                             size it and will not release {session}. Inspect it with: tmux \
                             show-options -w -t {window_id} @cyclops_prior_window_size"
                        ),
                    );
                }
                continue;
            }
            Err(error) => {
                log_err(home, &error);
                continue;
            }
        }
        match client.pin_window_size_manual(&window_id).await {
            Ok(()) => {
                sizing
                    .owned
                    .entry(session.to_string())
                    .or_default()
                    .pinned
                    .insert(window_id);
                took_a_window = true;
            }
            Err(error) => log_err(home, &error),
        }
    }
    Adopted {
        newly_following: false,
        took_a_window,
        authority_transferred: followed_before,
    }
}

/// Result of resizing execution across pinned windows in a session.
#[derive(Debug, Default)]
pub(crate) struct SizingOutcome {
    pub succeeded: BTreeSet<String>,
    pub failed: BTreeMap<String, cyclops_tmux::TmuxError>,
}

impl SizingOutcome {
    pub fn all_succeeded(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Push per-window topology-derived target sizes to every window this workspace owns,
/// in every session it owns. Returns exact per-window successes and failures.
async fn size_owned_windows(
    sizing: &WindowSizing,
    client: &ControlClient,
    canvas: Rect,
    tabs: &[TabModel],
    home: &std::path::Path,
) -> SizingOutcome {
    let mut outcome = SizingOutcome::default();
    for owned in sizing.owned.values() {
        for window_id in &owned.pinned {
            let target_size = if let Some(tab) = tabs.iter().find(|t| &t.window_id == window_id) {
                crate::render::window_target_size_for_layout(canvas, &tab.layout, tab.zoomed)
            } else {
                let inner = crate::render::pane_canvas(canvas);
                (inner.width, inner.height)
            };

            if !declarable(target_size) {
                continue;
            }

            match client
                .resize_window(window_id, target_size.0, target_size.1)
                .await
            {
                Ok(()) => {
                    outcome.succeeded.insert(window_id.clone());
                }
                Err(error) => {
                    log_err(home, &error);
                    outcome.failed.insert(window_id.clone(), error);
                }
            }
        }
    }
    outcome
}

/// Put every window this workspace pinned back on the policy it was found
/// with, then stop owning its sessions.
///
/// Restores before releasing, in that order: a marker cleared first would
/// let another workspace claim the session and adopt windows that still
/// carry this one's pin, which is how a `manual` nobody chose becomes
/// permanent.
async fn restore_owned_sizing(
    sizing: &mut WindowSizing,
    client: &ControlClient,
    home: &std::path::Path,
) {
    let Some(marker) = sizing.identity.as_ref().map(ClientIdentity::marker) else {
        // No identity means nothing can be proved to be ours, and putting
        // windows back on a guess would undo whoever does own them.
        sizing.owned.clear();
        return;
    };
    for (session, owned) in std::mem::take(&mut sizing.owned) {
        // Ownership is re-checked here, not assumed from the map. A
        // workspace can lose a session between claiming it and quitting:
        // its link dropped, a follower found the mark stale and took over,
        // and that follower is now the one those windows belong to.
        // Restoring them here would take a live workspace's session out
        // from under it, so the exact marker has to still be this one's.
        match client.window_driver(&session).await {
            Ok(Some(held)) if held == marker => {}
            Ok(_) => continue,
            Err(error) => {
                log_err(home, &error);
                continue;
            }
        }
        // Whether this session was fully handed back. It starts false when a
        // window here carries a record nobody can read, since such a window
        // was never pinned by this workspace and is exactly why the session
        // may not be released.
        let mut handed_back = owned.releasable();
        for window_id in &owned.pinned {
            match client.restore_window_size(window_id).await {
                Ok(cyclops_tmux::Restored::Malformed) => {
                    // The record of what this window was cannot be read, so
                    // the original policy is unknowable. Nothing was
                    // changed, and nothing here will change it: choosing a
                    // policy would invent state the operator never set, and
                    // clearing the record would destroy the only evidence
                    // of what the window originally was. The window stays
                    // pinned and this workspace stays its owner, which is
                    // visibly wrong and fully recoverable.
                    handed_back = false;
                    log_err(
                        home,
                        &format!(
                            "{window_id}: sizing record unreadable, so the original policy is \
                             unknown. The window is left on manual and still owned. Inspect it \
                             with: tmux show-options -w -t {window_id} @cyclops_prior_window_size"
                        ),
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    // A restore that failed leaves the window exactly as
                    // this workspace pinned it: on `manual`, with its record
                    // still attached. Releasing the mark over that is the
                    // same orphaning as the unreadable case, reached through
                    // a transient tmux failure instead: no policy applies,
                    // no client can resize it, and no owner is named for
                    // anyone to blame. The link may be down or the command
                    // may have timed out, and either way this session was
                    // not handed back.
                    handed_back = false;
                    log_err(home, &error);
                }
            }
        }
        if !handed_back {
            // Keeping the mark is the point: a pinned window with no owner
            // is the one state nothing can recover from on its own.
            continue;
        }
        if let Err(error) = client.release_window_driver(&session).await {
            log_err(home, &error);
        }
    }
}

/// Move every session this workspace owns onto the identity of a new
/// connection.
///
/// A reconnect replaces the tmux client, so `client_name:client_created`
/// changes while the process lives on. The marks left behind name a client
/// that no longer exists, which is exactly what a follower watches for, so
/// this is a race with a real other party rather than bookkeeping: between
/// the old client dying and this running, a follower may have taken a
/// session legitimately.
///
/// Each session is therefore moved with one compare-and-set from the exact
/// old marker to the exact new one, and ownership is kept only where that
/// won. A session lost in the gap is dropped from the map entirely, so
/// nothing here resizes it and the exit path will not put it back: it
/// belongs to the workspace that won it.
async fn rekey_ownership(
    sizing: &mut WindowSizing,
    client: &ControlClient,
    home: &std::path::Path,
) {
    sizing.following.clear();
    let Some(previous) = sizing.identity.take() else {
        // Nothing was ever claimed under a proven identity.
        sizing.owned.clear();
        return;
    };
    let stale = previous.marker();
    let Some(identity) = sizing_identity(sizing, client, home).await else {
        // Without a new identity this workspace cannot prove it owns
        // anything, so it claims nothing rather than writing sizes it
        // cannot defend.
        sizing.owned.clear();
        return;
    };
    let marker = identity.marker();
    for session in sizing.owned.keys().cloned().collect::<Vec<_>>() {
        let kept = client
            .take_over_window_driver(&session, &stale, &marker)
            .await;
        match kept {
            Ok(true) => {}
            Ok(false) => {
                sizing.owned.remove(&session);
            }
            Err(error) => {
                log_err(home, &error);
                sizing.owned.remove(&session);
            }
        }
    }
}

async fn resize_client(app: &mut App, client: &ControlClient) -> SizingOutcome {
    let (w, h) = app.term_size;
    let canvas = app.chrome(Rect::new(0, 0, w, h)).tmux_sizing_canvas();
    let size = crate::render::tmux_client_size(canvas, app.model.active_tab());
    if !declarable(size) {
        return SizingOutcome::default();
    }
    let any_window_diverged = app.sizing.owned.values().any(|owned| {
        owned.pinned.iter().any(|window_id| {
            if let Some(tab) = app
                .model
                .session
                .tabs
                .iter()
                .find(|t| &t.window_id == window_id)
            {
                let target =
                    crate::render::window_target_size_for_layout(canvas, &tab.layout, tab.zoomed);
                let rect = tab.layout.rect();
                rect.width != target.0 || rect.height != target.1
            } else {
                true
            }
        })
    });
    if app.declared_client_size == Some(size) && !any_window_diverged {
        return SizingOutcome::default();
    }
    let outcome = size_owned_windows(
        &app.sizing,
        client,
        canvas,
        &app.model.session.tabs,
        &app.home,
    )
    .await;
    if outcome.all_succeeded() {
        app.declared_client_size = Some(size);
    }
    run_post_resize_recovery(app, client).await;
    outcome
}

/// Shared post-resize recovery helper:
/// 1. Reconciles every exact successfully resized window in every owned session.
/// 2. Always fetches a fresh post-resize snapshot before any pane decision.
/// 3. For each window in owned[session].pinned, revalidates live driver marker before mutating.
/// 4. Panes with deliberate minimization provenance (`Minimized { original_height }`)
///    must remain collapsed at 1 row after tmux automatic reflow on window resize.
/// 5. Panes with `None` provenance that are 1-row high fail closed: they are NOT modified
///    (manual resize is preserved; no auto-uncrush of unknown intent) and surface an explicit banner.
/// 6. Panes with malformed provenance (`Malformed(bad)`) fail closed: surface visible notice,
///    log error, leave option evidence untouched.
/// 7. Fails visibly on errors, logs errors, and retains retry state (`needs_reconcile = true`).
pub async fn recover_post_resize_geometry(
    sizing: &WindowSizing,
    client: &ControlClient,
    home: &std::path::Path,
    mut notice: Option<&mut NoticeState>,
) -> Result<bool, TmuxError> {
    let identity = client.client_identity().await?;
    let my_marker = identity.marker();

    let owned_sessions: Vec<(String, Vec<String>)> = sizing
        .owned
        .iter()
        .map(|(sess, owned)| (sess.clone(), owned.pinned.iter().cloned().collect()))
        .collect();

    if owned_sessions.is_empty() {
        return Ok(false);
    }

    let snapshot = client.workspace_snapshot().await?;
    let mut any_modified = false;

    for (session, pinned_windows) in owned_sessions {
        let current_driver = client.window_driver(&session).await?;
        if current_driver.as_ref() != Some(&my_marker) {
            continue;
        }

        let Some(snap_session) = snapshot.sessions.iter().find(|s| s.name == session) else {
            continue;
        };

        for window_id in pinned_windows {
            let Some(snap_window) = snap_session.windows.iter().find(|w| w.id == window_id) else {
                continue;
            };

            for pane in &snap_window.panes {
                match &pane.minimization {
                    cyclops_tmux::PaneMinimizationProvenance::Minimized { .. } => {
                        if pane.height > crate::render::MINIMIZED_ROWS as u32 {
                            if let Err(e) = client
                                .resize_pane_height(&pane.id, crate::render::MINIMIZED_ROWS)
                                .await
                            {
                                log_err(
                                    home,
                                    &format!(
                                        "failed to re-collapse minimized pane {}: {e}",
                                        pane.id
                                    ),
                                );
                                if let Some(ref mut n) = notice {
                                    n.show(
                                        format!("error: failed to re-collapse pane {}", pane.id),
                                        Instant::now(),
                                    );
                                }
                                return Err(e);
                            }
                            any_modified = true;
                        }
                    }
                    cyclops_tmux::PaneMinimizationProvenance::Malformed(bad) => {
                        if pane.height <= crate::render::MINIMIZED_ROWS as u32 {
                            log_err(
                                home,
                                &format!(
                                    "{}: malformed minimization provenance ({bad}), refusing recovery",
                                    pane.id
                                ),
                            );
                            if let Some(ref mut n) = notice {
                                n.show(
                                    format!(
                                        "warning: pane {} has malformed minimization record ({bad}); manual recovery required",
                                        pane.id
                                    ),
                                    Instant::now(),
                                );
                            }
                        }
                    }
                    cyclops_tmux::PaneMinimizationProvenance::None => {
                        if pane.height <= crate::render::MINIMIZED_ROWS as u32 {
                            // Fail closed on unknown intent: do not uncrush without positive provenance.
                            if let Some(ref mut n) = notice {
                                n.show(
                                    format!(
                                        "pane {} is 1 row high (unknown provenance); manual resize required to uncrush",
                                        pane.id
                                    ),
                                    Instant::now(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(any_modified)
}

async fn run_post_resize_recovery(app: &mut App, client: &ControlClient) {
    if let Err(error) =
        recover_post_resize_geometry(&app.sizing, client, &app.home, Some(&mut app.notice)).await
    {
        log_err(
            &app.home,
            &format!("post-resize geometry recovery failed: {error}"),
        );
        app.needs_reconcile = true;
    }
}

/// The tab windows not yet pinned to the sizing policy, in tab order.
fn unpinned_windows<'a>(tabs: &'a [TabModel], pinned: &BTreeSet<String>) -> Vec<&'a str> {
    tabs.iter()
        .filter(|tab| !pinned.contains(&tab.window_id))
        .map(|tab| tab.window_id.as_str())
        .collect()
}

/// Apply a `%layout-change` notification directly. Returns false when the
/// model cannot absorb it and a full reconcile is needed.
fn apply_layout_change(app: &mut App, window: &str, layout: &str, flags: Option<&str>) -> bool {
    let Ok(node) = crate::layout::parse_layout(layout) else {
        return false;
    };
    let Some(resolved) = crate::layout::resolve_layout(&node, &[]) else {
        return false;
    };
    let Some(zoomed) = flags.map(|f| f.contains('Z')) else {
        // Older tmux without the flags field: zoom state is unknowable
        // from the notification alone.
        return false;
    };
    let Some(tab) = app
        .model
        .session
        .tabs
        .iter_mut()
        .find(|t| t.window_id == window)
    else {
        return false;
    };
    tab.layout = resolved;
    tab.zoomed = zoomed;
    let ids = crate::layout::pane_ids_in_layout(&tab.layout);
    if !ids.iter().any(|id| id == &tab.active_pane) {
        if let Some(first) = ids.first() {
            tab.active_pane = first.clone();
        }
    }
    true
}

/// Whether a rename notification belongs to this control client's session.
/// A missing id is the legacy attached-session-only notification shape.
fn rename_targets_active_session(active: Option<&str>, renamed: Option<&str>) -> bool {
    renamed.is_none_or(|id| Some(id) == active)
}

fn finish_compose_send(
    dialog: Option<&mut Dialog>,
    completed: dialog::ComposeAttempt,
    outcome: crate::daemon::SendOutcome,
) {
    let Some(Dialog::Compose {
        buffer,
        status,
        send,
    }) = dialog
    else {
        return;
    };
    if send.attempt() != Some(&completed) {
        return;
    }

    match outcome {
        crate::daemon::SendOutcome::Accepted(receipt) => {
            let sent_draft_is_unchanged =
                dialog::parse_compose(buffer).ok().as_ref() == Some(&completed.message);
            *status = Some(copy::compose_sent(&receipt));
            *send = dialog::ComposeSendState::Ready;
            if sent_draft_is_unchanged {
                *buffer = format!("@{} ", completed.message.to);
            }
        }
        crate::daemon::SendOutcome::NotSent(cause) => {
            *status = Some(copy::compose_rejected(&completed.message.to, &cause));
            *send = dialog::ComposeSendState::Ready;
        }
        crate::daemon::SendOutcome::Rejected(refusal) => {
            *status = Some(copy::compose_rejected(
                &completed.message.to,
                &refusal.message,
            ));
            *send = dialog::ComposeSendState::Ready;
        }
        crate::daemon::SendOutcome::Unknown(cause) => {
            *status = Some(copy::compose_unknown(&completed.message.to, &cause));
            *send = dialog::ComposeSendState::Retryable(completed);
        }
    }
}

fn queue_messages_send(app: &mut App, attempt: MessagesSendAttempt) {
    if !app.messages_gate.may_mutate() {
        app.messages_composer.record_not_sent(
            "message state is not current; wait for the daemon connection to recover".into(),
        );
        return;
    }
    if app.messages_caller != Some(attempt.caller) {
        app.messages_composer.record_not_sent(
            "sender identity is not current; reopen the workspace after updating Cyclops".into(),
        );
        return;
    }
    if app.messages_send_in_flight.is_some() {
        app.messages_composer
            .record_not_sent("another message send is still in progress".into());
        return;
    }
    let Some(tx) = &app.messages_send_tx else {
        app.messages_composer
            .record_not_sent("the message send worker is unavailable".into());
        return;
    };
    match tx.try_send(MessagesSendTask {
        attempt: attempt.clone(),
    }) {
        Ok(()) => {
            app.messages_send_in_flight = Some(attempt);
            app.messages_composer.stage =
                Some(cyclops_ui::Stage::Acting(cyclops_ui::Action::Reply));
        }
        Err(std::sync::mpsc::TrySendError::Full(_)) => app
            .messages_composer
            .record_not_sent("the bounded message send lane is busy".into()),
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => app
            .messages_composer
            .record_not_sent("the message send worker stopped".into()),
    }
}

fn process_generation_refusal(refusal: &crate::daemon::DaemonRefusal) -> bool {
    refusal.code == "denied"
}

fn finish_messages_send(
    app: &mut App,
    attempt: MessagesSendAttempt,
    outcome: crate::daemon::SendOutcome,
) {
    if app.messages_send_in_flight.as_ref() != Some(&attempt) {
        return;
    }
    app.messages_send_in_flight = None;

    let current_composer = attempt.matches(
        &app.messages_composer,
        app.messages_composer_revision,
        app.messages_caller,
    );
    if !current_composer {
        if matches!(
            app.messages_composer.stage,
            Some(cyclops_ui::Stage::Acting(_))
        ) {
            app.messages_composer.clear_stage();
        }
        let notice = match &outcome {
            crate::daemon::SendOutcome::Accepted(receipt) => receipt.clone(),
            crate::daemon::SendOutcome::NotSent(why) => {
                format!("Earlier draft was not sent: {why}")
            }
            crate::daemon::SendOutcome::Rejected(refusal) => {
                format!(
                    "Earlier draft was refused ({}): {}",
                    refusal.code, refusal.message
                )
            }
            crate::daemon::SendOutcome::Unknown(why) => {
                format!("Earlier draft outcome is unknown: {why}")
            }
        };
        app.notice.show(notice, Instant::now());
        app.messages_gate.mark_dirty();
        pump_messages_refresh(app);
        return;
    }

    match outcome {
        crate::daemon::SendOutcome::Accepted(receipt) => {
            app.messages_composer.draft.set("");
            app.messages_composer.mode = None;
            app.messages_composer.focused = false;
            app.messages_composer.clear_stage();
            app.notice.show(receipt, Instant::now());
        }
        crate::daemon::SendOutcome::NotSent(why) => {
            app.messages_composer.record_not_sent(why);
        }
        crate::daemon::SendOutcome::Rejected(refusal) => {
            let why = if process_generation_refusal(&refusal) {
                app.messages_caller = None;
                format!(
                    "sender identity changed; nothing was accepted. Reopen this workspace after updating Cyclops: {}",
                    refusal.message
                )
            } else {
                format!("{}: {}", refusal.code, refusal.message)
            };
            app.messages_composer.record_not_sent(why);
        }
        crate::daemon::SendOutcome::Unknown(why) => {
            app.messages_composer.record_uncertain(why);
        }
    }
    app.messages_gate.mark_dirty();
    pump_messages_refresh(app);
}

fn finish_messages_reconcile(app: &mut App) {
    let Some(reconciled) = app.messages_reconcile_owed.take() else {
        return;
    };
    if !reconciled.matches(
        &app.messages_composer,
        app.messages_composer_revision,
        app.messages_caller,
    ) {
        return;
    }
    app.messages_composer.reconcile_stage();
    app.notice.show(
        "Message state refreshed; an exact-key retry is now available",
        Instant::now(),
    );
}

fn messages_composer_changed(app: &mut App) {
    app.messages_composer_revision = app.messages_composer_revision.wrapping_add(1);
    app.messages_composer.clear_stage();
    app.messages_reconcile_owed = None;
}

fn queue_message_detail(
    app: &mut App,
    row: cyclops_ui::QueueRow,
    target: cyclops_ui::FrozenTarget,
) {
    debug_assert_eq!(&row.target, &target.target);
    let mut detail = cyclops_ui::Detail::open(&row, target.watermark);
    if !app.messages_gate.may_mutate() {
        detail.not_sent(
            None,
            "message state is not current; wait for the daemon connection to recover",
        );
        app.messages_detail = Some(detail);
        return;
    }
    if app.messages_caller.is_none() {
        detail.not_sent(
            None,
            "authenticated mailbox identity is unavailable; update Cyclops and reopen this workspace",
        );
        app.messages_detail = Some(detail);
        return;
    }
    if app.message_detail_in_flight.is_some() {
        detail.not_sent(None, "another message detail request is still in progress");
        app.messages_detail = Some(detail);
        return;
    }
    let Some(tx) = &app.message_detail_tx else {
        detail.not_sent(None, "the message detail worker is unavailable");
        app.messages_detail = Some(detail);
        return;
    };
    match tx.try_send(MessageDetailTask {
        row,
        target: target.clone(),
    }) {
        Ok(()) => app.message_detail_in_flight = Some(target),
        Err(mpsc::error::TrySendError::Full(_)) => {
            detail.not_sent(None, "the bounded message detail lane is busy")
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            detail.not_sent(None, "the message detail worker stopped")
        }
    }
    app.messages_detail = Some(detail);
}

fn finish_message_detail(
    app: &mut App,
    target: cyclops_ui::FrozenTarget,
    outcome: cyclops_ui::ActionOutcome,
) {
    if app.message_detail_in_flight.as_ref() != Some(&target) {
        return;
    }
    app.message_detail_in_flight = None;
    let Some(detail) = app
        .messages_detail
        .as_mut()
        .filter(|detail| detail.target() == &target)
    else {
        return;
    };
    let notice = match outcome {
        cyclops_ui::ActionOutcome::Opened(loaded) => {
            detail.loaded_ok(*loaded);
            "Message detail loaded".to_string()
        }
        cyclops_ui::ActionOutcome::Done(note) => {
            let why = format!("unexpected detail mutation result: {note}");
            detail.failed(None, why.clone());
            why
        }
        cyclops_ui::ActionOutcome::Refused { code, message } => {
            detail.refused(None, &code, message.clone());
            format!("Detail refused ({code}): {message}")
        }
        cyclops_ui::ActionOutcome::NotSent(why) => {
            detail.not_sent(None, why.clone());
            format!("Detail not sent: {why}")
        }
        cyclops_ui::ActionOutcome::Uncertain(why) => {
            detail.uncertain(None, why.clone());
            format!("Detail outcome unknown: {why}")
        }
    };
    app.notice.show(notice, Instant::now());
    app.messages_gate.mark_dirty();
    pump_messages_refresh(app);
}

/// Request current message state without inventing connection evidence.
fn request_messages_snapshot(app: &mut App) {
    match app.messages_gate.link() {
        cyclops_ui::Link::Connected => {
            app.messages_refresh_error = None;
            app.messages_gate.mark_dirty();
            pump_messages_refresh(app);
        }
        cyclops_ui::Link::Lost => {
            app.messages_gate.reconnecting();
        }
        cyclops_ui::Link::Connecting => {}
    }
}

/// Pump the Messages pane refresh gate, issuing a snapshot fetch if one is
/// owed and none is in flight.
fn pump_messages_refresh(app: &mut App) {
    if let Some(req) = app.messages_gate.begin() {
        let sent = if let Some(tx) = &app.messages_snapshot_tx {
            tx.try_send((req, 128)).is_ok()
        } else {
            false
        };
        if !sent {
            app.messages_gate.finish_failure(req);
        }
    }
}

/// Install one authenticated body-free snapshot for both the Messages pane
/// and its collapsed rail. The refresh gate rejects replies made stale by a
/// newer invalidation, so retaining these counts never invents current state.
fn install_messages_snapshot(
    app: &mut App,
    request: cyclops_ui::RefreshRequest,
    result: cyclops_proto::MessagesSnapshotResult,
) -> bool {
    if !app.messages_gate.finish_snapshot(request, &result) {
        return false;
    }
    app.messages_refresh_error = None;
    app.messages_caller = result.caller;
    app.messages_snapshot_counts = Some(result.counts);
    let snapshot = apply_messages_presentation_cutoff(
        cyclops_ui::messages::rows_from_snapshot(&result),
        app.prefs.messages_cleared_through_seq,
    );
    app.messages_queue.replace(snapshot);
    finish_messages_reconcile(app);
    true
}

/// Handle one app message. Returns false when the channel closed.
async fn handle_app_msg(
    msg: Option<AppMsg>,
    app: &mut App,
    client: &mut ControlClient,
    debounce: &mut Option<Instant>,
    reconnect_deadline: &mut Option<Instant>,
    detached: &mut bool,
    pending_input: &mut Option<PendingPaneInput>,
) -> bool {
    let Some(msg) = msg else {
        return false;
    };
    match msg {
        // The composer's send came back. Show what happened and let the
        // operator type another one; closing the dialog for them would
        // take the receipt off the screen at the moment it arrived.
        //
        // The attempt key is checked against the open composer because the
        // dialog may have been closed and reopened while the send was in
        // flight. A stale receipt must not rewrite a newer draft.
        AppMsg::SendFinished { attempt, outcome } => {
            finish_compose_send(app.dialog.as_mut(), attempt, outcome);
            arm(debounce);
        }
        AppMsg::Redraw => arm(debounce),
        AppMsg::Focus(focused) => {
            app.window_focused = focused;
            if focused {
                // Forget what the terminal was last told so the next draw
                // re-emits the theme's ground even though it has not
                // changed since focus left.
                app.window_palette = HostPaletteState::Unknown;
                // Another program owned this surface while focus was away
                // and may have written over it.
                app.repaint_requested = true;
                arm(debounce);
            } else {
                // Immediately, not on a frame: an unfocused workspace may
                // not draw again until something happens in it, and the
                // operator is looking at their own shell right now.
                crate::term_guard::yield_window_palette();
                app.window_palette = HostPaletteState::Defaults;
                // The button, if held, is let go somewhere this app will
                // never hear about.
                if settle_lost_release(app, client).await {
                    arm(debounce);
                }
            }
        }
        AppMsg::Resized(w, h) => {
            app.term_size = (w, h);
            app.hit_map.clear();
            // Bookkeeping only. A drag delivers a resize per host frame,
            // and answering each one would cost a `resize-pane` round trip
            // that reflows every agent's TUI plus a full repaint that
            // clears and rewrites the surface. Both belong to the burst
            // rather than to its events, so this records the latest size,
            // drops geometry that described the old one, and slides the
            // one-shot settle deadline. `apply_settled_resize` owns the
            // single resize and the single repaint when it expires.
            app.repaint_resize_pending = true;
            app.repaint_resize_settle_at = Some(Instant::now() + RESIZE_SETTLE);
        }
        AppMsg::Reconcile => {
            app.needs_reconcile = true;
            app.hit_map.clear();
            arm(debounce);
        }
        AppMsg::SessionSwitched { session, name } => {
            if let Some(index) = app
                .model
                .workspaces
                .iter()
                .position(|workspace| workspace.session_id == session)
            {
                app.model.active_workspace = index;
            }
            app.model.session.session = name;
            app.needs_reconcile = true;
            app.hit_map.clear();
            arm(debounce);
        }
        AppMsg::SessionRenamed { session, name } => {
            let renamed_index = session
                .as_deref()
                .and_then(|session_id| {
                    app.model
                        .workspaces
                        .iter()
                        .position(|workspace| workspace.session_id == session_id)
                })
                .or_else(|| session.is_none().then_some(app.model.active_workspace));
            if let Some(index) = renamed_index {
                if let Some(workspace) = app.model.workspaces.get_mut(index) {
                    let old_name = std::mem::replace(&mut workspace.name, name.clone());
                    if persist::migrate_order_entry(
                        &mut app.prefs.workspace_order,
                        &old_name,
                        &name,
                    ) {
                        app.save_prefs_or_log();
                    }
                }
            }
            let active_session = app
                .model
                .workspaces
                .get(app.model.active_workspace)
                .map(|workspace| workspace.session_id.as_str());
            // The legacy one-field notification was documented for the
            // attached session. Current tmux identifies every renamed
            // session, including background ones, by stable id (F37).
            if rename_targets_active_session(active_session, session.as_deref()) {
                app.model.session.session = name;
            }
            app.needs_reconcile = true;
            app.hit_map.clear();
            arm(debounce);
        }
        AppMsg::LayoutChanged {
            window,
            layout,
            flags,
        } => {
            if apply_layout_change(app, &window, &layout, flags.as_deref()) {
                if app.model.active_tab().window_id == window {
                    resize_client(app, client).await;
                    app.needs_hydrate = true;
                    app.hit_map.clear();
                    // tmux moved a seam under us, so the cells the old
                    // split owned are not the cells the new one does.
                    app.layout_changed();
                }
            } else {
                app.needs_reconcile = true;
                app.hit_map.clear();
            }
            arm(debounce);
        }
        AppMsg::ActivePaneChanged { window, pane } => {
            let known = app
                .model
                .session
                .tabs
                .iter_mut()
                .find(|t| t.window_id == window)
                .map(|t| t.active_pane = pane)
                .is_some();
            if !known {
                app.needs_reconcile = true;
            }
            arm(debounce);
        }
        AppMsg::OutputBatch(output) => {
            let mut changed = false;
            for (pane, bytes) in output {
                if app.is_visible_pane(&pane) {
                    if let Some(rt) = app.runtimes.get_mut(&pane) {
                        rt.feed(&bytes);
                        changed = true;
                    }
                }
            }
            if changed {
                arm(debounce);
                arm_folder_probe(app);
            }
        }
        AppMsg::LinkLost => {
            if let Some(pending) = pending_input.take() {
                app.notice.show(
                    copy::pane_input_not_sent(&pending.pane, &TmuxError::Disconnected),
                    Instant::now(),
                );
            }
            app.reconnect_attempt = 0;
            schedule_reconnect(app, reconnect_deadline);
            arm(debounce);
        }
        AppMsg::PanePaused { pane } => {
            app.paused_panes.insert(pane);
            arm(debounce);
        }
        AppMsg::PaneContinued { pane } => {
            if app.paused_panes.remove(&pane) {
                if app.is_visible_pane(&pane) {
                    // Rehydrate: paused output was dropped, continuity is gone.
                    app.runtimes.retain_visible(&[]);
                    app.needs_hydrate = true;
                }
                arm(debounce);
            }
        }
        AppMsg::DecorationChanged(snapshot) => {
            apply_decoration_snapshot(app, snapshot);
            // Route chrome changes invalidate the detailed open view. The
            // collapsed cue contains only durable snapshot counts, so it does
            // not turn every pane-state repaint into a message fetch or a
            // false uncertainty marker. Reopening explicitly refreshes the
            // detailed projection before enabling actions.
            if app.model.messages_visible {
                app.messages_gate.mark_dirty();
                pump_messages_refresh(app);
            }
            arm(debounce);
        }
        AppMsg::DaemonCompatibility(compatibility) => {
            app.daemon_compatibility_notice = copy::daemon_compatibility_notice(&compatibility);
            app.daemon_compatibility = Some(compatibility);
            arm(debounce);
        }
        AppMsg::DaemonReconnected => {
            apply_daemon_reconnected(app);
            arm(debounce);
        }
        // E2: per-event, not coalesced with the decoration burst above —
        // feeding the record is cheap and in-memory, so it runs on every
        // event rather than waiting for that debounce (see
        // `spawn_decoration_forwarder`'s doc). `Record::live` also moves
        // the record's own attention register by the one rule in
        // `cyclops_proto::attention`, the same rule `app.decoration`'s
        // register answers to; neither side recomputes it. The intake
        // between here and the record drops ledger-backed entries the
        // boot-time tail already replayed (crate::event_record).
        AppMsg::StreamEntry(entry) => {
            crate::event_record::live(&mut app.record, &mut app.intake, *entry);
            arm(debounce);
        }
        AppMsg::MessagesChanged(changed) => {
            if app.messages_gate.link() != cyclops_ui::Link::Connected {
                app.messages_gate.connected();
            }
            match changed {
                Some(data) => {
                    app.messages_gate.messages_changed(&data);
                }
                None => {
                    app.messages_gate.mark_dirty();
                }
            }
            pump_messages_refresh(app);
        }
        AppMsg::StreamGap { why } => {
            app.messages_gate.disconnected();
            app.messages_caller = None;
            if app.stream_reconciling {
                return true;
            }
            app.stream_reconciling = true;
            app.notice.show(copy::stream_stale(&why), Instant::now());
            let queued =
                app.stream_reconcile_requests
                    .as_ref()
                    .is_some_and(|tx| match tx.try_send(()) {
                        Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => true,
                        Err(std::sync::mpsc::TrySendError::Disconnected(())) => false,
                    });
            if !queued {
                let bootstrap = crate::event_record::load(&app.home);
                let mut record = cyclops_ui::Record::new();
                let mut intake = cyclops_ui::Intake::new();
                let warning = crate::event_record::install(&mut record, &mut intake, bootstrap);
                app.record = record;
                app.intake = intake;
                app.stream_reconciling = false;
                app.notice.show(
                    warning.unwrap_or_else(|| copy::STREAM_RECONCILED.to_string()),
                    Instant::now(),
                );
            }
            arm(debounce);
        }
        AppMsg::StreamReconciled(bootstrap) => {
            let mut record = cyclops_ui::Record::new();
            let mut intake = cyclops_ui::Intake::new();
            let warning = crate::event_record::install(&mut record, &mut intake, *bootstrap);
            app.record = record;
            app.intake = intake;
            app.stream_reconciling = false;
            app.notice.show(
                warning.unwrap_or_else(|| copy::STREAM_RECONCILED.to_string()),
                Instant::now(),
            );
            arm(debounce);
        }
        // Wake-only: the reload itself runs on the render deadline this
        // arms (the theme_watch refresh in `run_async`).
        AppMsg::ThemeChanged => arm(debounce),
        AppMsg::MessagesSnapshotLoaded { request, result } => {
            install_messages_snapshot(app, request, result);
            pump_messages_refresh(app);
            arm(debounce);
        }
        AppMsg::MessagesSnapshotFailed { request, error } => {
            if app.messages_gate.finish_snapshot_failure(request) {
                app.messages_refresh_error = Some(error);
                app.messages_caller = None;
            }
            pump_messages_refresh(app);
            arm(debounce);
        }
        AppMsg::MessageDetailFinished { target, outcome } => {
            finish_message_detail(app, target, outcome);
            arm(debounce);
        }
        AppMsg::MessagesSendFinished { attempt, outcome } => {
            finish_messages_send(app, attempt, outcome);
            arm(debounce);
        }
        AppMsg::Mouse { mouse, .. } => {
            // Bare motion only matters while a menu or dialog shows hover
            // highlights — or over the sidebar's create button, the one
            // piece of resting chrome that answers the mouse. Everywhere
            // else it must not wake the renderer.
            if matches!(mouse.kind, MouseEventKind::Moved)
                && !app.menu.is_open()
                && app.dialog.is_none()
                // Bare motion while something is still picked up is the
                // release this app never saw; `handle_mouse` settles it.
                && app.drag.is_none()
                && !app.selection.is_dragging()
                && !crate::input::mouse::motion_touches_hover_button(
                    &app.hit_map,
                    app.hover,
                    mouse.column,
                    mouse.row,
                )
            {
                return true;
            }
            if matches!(mouse.kind, MouseEventKind::Moved)
                && app.hover == Some((mouse.column, mouse.row))
            {
                return true;
            }
            if let Err(e) = handle_mouse(app, client, mouse, detached).await {
                log_err(&app.home, &e);
            }
            arm(debounce);
        }
        AppMsg::PasteTooLarge { bytes, .. } => {
            app.notice
                .show(copy::paste_too_large(bytes), Instant::now());
            arm(debounce);
        }
        AppMsg::Paste { text, .. } => {
            if app.link_state == LinkState::ServerGone {
                return true;
            }
            if app.menu.is_open() {
                app.close_menu();
                arm(debounce);
                return true;
            }
            if app.dialog.is_some() {
                if dialog::append_dialog_text(app.dialog.as_mut(), &text) {
                    arm(debounce);
                }
                return true;
            }
            if app.model.messages_visible && app.messages_focused && app.messages_composer.focused {
                for ch in text.chars() {
                    app.messages_composer.push_char(ch);
                }
                arm(debounce);
                return true;
            }
            if let Err(e) = paste_into_focused_pane(app, client, text.as_bytes()).await {
                log_err(&app.home, &e);
            }
        }
        AppMsg::Input { key, .. } => {
            if app.link_state == LinkState::ServerGone {
                return true;
            }
            if app.menu.is_open() {
                // Any key dismisses an open menu and is consumed by it.
                app.close_menu();
                arm(debounce);
                return true;
            }
            if crate::input::escape_cancels_visual_state(
                key.code,
                app.selection.active_pane().is_some(),
                app.drag.is_some(),
            ) {
                clear_selection(app);
                cancel_drag(app);
                // Escape belongs to the chrome operation it just cancelled;
                // do not leak it into the child TUI as a second action.
                arm(debounce);
                return true;
            }
            match handle_key(app, client, key).await {
                Ok(outcome) => {
                    apply_pane_input_outcome(outcome, app, pending_input, detached, debounce)
                }
                Err(e) => log_err(&app.home, &e),
            }
        }
    }
    true
}

#[cfg(test)]
mod compose_send_tests {
    use super::*;

    fn composer(buffer: &str) -> Dialog {
        Dialog::Compose {
            buffer: buffer.to_string(),
            status: None,
            send: dialog::ComposeSendState::Ready,
        }
    }

    #[test]
    fn escape_after_unknown_keeps_the_key_for_an_exact_retry() {
        let mut dialog = composer("@reviewer ship it");
        let message = dialog::parse_compose("@reviewer ship it").expect("message");
        let first = dialog::begin_compose_send(Some(&mut dialog), message.clone(), || {
            "stable-key".to_string()
        })
        .expect("first attempt");
        finish_compose_send(
            Some(&mut dialog),
            first.clone(),
            crate::daemon::SendOutcome::Unknown("connection closed".to_string()),
        );
        let Dialog::Compose { status, send, .. } = &dialog else {
            unreachable!()
        };
        assert!(status
            .as_deref()
            .is_some_and(|text| text.starts_with("acceptance unknown for reviewer:")));
        assert_eq!(
            send.attempt().map(|attempt| attempt.client_key.as_str()),
            Some("stable-key")
        );

        assert_eq!(
            dialog::request_compose_cancel(&mut dialog),
            dialog::ComposeCancel::KeepOpen
        );
        let Dialog::Compose { send, .. } = &dialog else {
            unreachable!()
        };
        assert!(matches!(
            send,
            dialog::ComposeSendState::ConfirmAbandon {
                resume: dialog::ComposeResume::Retryable,
                ..
            }
        ));
        assert_eq!(
            dialog::request_compose_cancel(&mut dialog),
            dialog::ComposeCancel::KeepOpen
        );

        let retry = dialog::begin_compose_send(Some(&mut dialog), message, || {
            panic!("an exact retry must not generate another key")
        })
        .expect("retry attempt");
        assert_eq!(retry.client_key, first.client_key);

        finish_compose_send(
            Some(&mut dialog),
            retry,
            crate::daemon::SendOutcome::Accepted("already accepted m-original".to_string()),
        );
        let Dialog::Compose {
            buffer,
            status,
            send,
        } = dialog
        else {
            unreachable!()
        };
        assert_eq!(buffer, "@reviewer ");
        assert_eq!(status.as_deref(), Some("already accepted m-original"));
        assert_eq!(send, dialog::ComposeSendState::Ready);
    }

    #[test]
    fn editing_or_rejection_starts_a_new_attempt() {
        let mut dialog = composer("@reviewer ship it");
        let original = dialog::parse_compose("@reviewer ship it").expect("message");
        let first = dialog::begin_compose_send(Some(&mut dialog), original, || "key-1".into())
            .expect("first attempt");
        finish_compose_send(
            Some(&mut dialog),
            first.clone(),
            crate::daemon::SendOutcome::Unknown("connection closed".to_string()),
        );

        let Dialog::Compose { buffer, .. } = &mut dialog else {
            unreachable!()
        };
        *buffer = "@reviewer ship the corrected patch".to_string();
        let edited = dialog::parse_compose(buffer).expect("edited message");
        let second =
            dialog::begin_compose_send(Some(&mut dialog), edited.clone(), || "key-2".into())
                .expect("edited attempt");
        assert_ne!(second.client_key, first.client_key);

        finish_compose_send(
            Some(&mut dialog),
            second,
            crate::daemon::SendOutcome::Rejected(crate::daemon::DaemonRefusal::new(
                "no_such_target",
                "recipient is unavailable",
            )),
        );
        let Dialog::Compose {
            buffer,
            status,
            send,
            ..
        } = &dialog
        else {
            unreachable!()
        };
        assert_eq!(buffer, "@reviewer ship the corrected patch");
        assert_eq!(
            status.as_deref(),
            Some("not accepted for reviewer: recipient is unavailable")
        );
        assert_eq!(*send, dialog::ComposeSendState::Ready);
        let third = dialog::begin_compose_send(Some(&mut dialog), edited, || "key-3".into())
            .expect("attempt after rejection");
        assert_eq!(third.client_key, "key-3");
    }
}

/// Finish a pickup whose release the workspace never saw.
///
/// A button let go outside the terminal, or while focus was elsewhere,
/// sends no Up here. The next event that can arrive is proof it happened:
/// bare motion is reported only with no button held, a fresh press cannot
/// begin while the old one is still down, and a window losing focus takes
/// the button's release with it. Left in place, the stale pickup finished
/// on the operator's NEXT click instead: the divider it held was dragged
/// from wherever the pointer last was to wherever that click landed (the
/// pane "randomly" resizing), and a selection still being dragged kept the
/// wheel swallowed over every other pane (scrolling "locked") until a click
/// happened to land on the right one.
///
/// Nothing is applied here. A divider already applied every step it made
/// while the button was held; the sidebar and Messages pane widths a drag
/// previewed stay where the operator left them and are saved, as a release
/// would have saved them; a selection ends where it stood and is copied,
/// the same as a release over the pane. Returns whether anything was
/// settled, which is a frame's worth of change.
async fn settle_lost_release(app: &mut App, client: &ControlClient) -> bool {
    let mut settled = false;
    if let Some(drag) = app.drag.take() {
        settled = true;
        if drag.is_active() {
            match drag.target {
                DragTarget::Sidebar => {
                    app.save_prefs_or_log();
                    // Same commit as a release the app actually saw, so
                    // the same topology epoch: the panel is keeping the
                    // width the preview left it at.
                    app.layout_changed();
                    resize_client(app, client).await;
                }
                DragTarget::Messages => {
                    app.save_prefs_or_log();
                    app.layout_changed();
                    resize_client(app, client).await;
                }
                DragTarget::SidebarSplit => {
                    app.save_prefs_or_log();
                    app.layout_changed();
                }
                _ => {}
            }
        }
    }
    if app.selection.is_dragging() {
        settled = true;
        if let Some(pane) = app.selection.finish_drag() {
            copy_selection(app, &pane);
        }
    }
    settled
}

fn cancel_drag(app: &mut App) {
    if let Some(drag) = app.drag.take() {
        if let Some(width) = crate::render::sidebar_width_on_cancel(&drag, app.term_size.0) {
            // Sidebar motion is only visual until mouse-up, so Escape can
            // restore the start without a compensating tmux resize.
            app.prefs.sidebar_width = width;
        }
        if let Some(width) = crate::render::messages_width_on_cancel(&drag, app.term_size.0) {
            // The Messages divider follows the same preview contract as
            // the sidebar: Escape restores the width from mouse-down.
            app.prefs.messages_width = width;
        }
        // A cancelled chrome drag snaps a panel back to where it started,
        // which vacates every column the preview had taken.
        if matches!(
            drag.target,
            DragTarget::Sidebar | DragTarget::Messages | DragTarget::SidebarSplit
        ) {
            app.layout_changed();
        }
    }
}

/// The minimal live-model context [`action::route_binding`] and
/// [`action::route_menu_item`] need, built fresh for each event so nothing
/// here can go stale.
fn route_context(app: &App) -> action::RouteContext<'_> {
    action::RouteContext {
        tabs: &app.model.session.tabs,
        active_tab: app.model.session.active_tab,
        active_pane: &app.model.active_tab().active_pane,
        session: &app.model.session.session,
        workspaces: &app.model.workspaces,
        active_workspace: app.model.active_workspace,
    }
}

/// Apply the executor's outcome. `redraw` is not one of its fields: every
/// call site below already arms the render debounce unconditionally after
/// dispatching a resolved action (the same thing keyboard, mouse, and dialog
/// handling did before this task), so a redundant flag would just be a
/// field nobody reads. `detach` is read at each call site directly, since
/// the two callers that can reach it (`handle_key`, and the menu-item
/// branch of `handle_mouse`) each have their own way of ending the loop.
fn apply_outcome(app: &mut App, outcome: exec::Outcome) {
    if outcome.persist {
        app.save_prefs_or_log();
    }
    if outcome.reconcile {
        app.needs_reconcile = true;
    }
}

/// Handle one resolved mouse event. This owns hit-testing, drag-state
/// mechanics, selection, and menu/dialog overlay handling only; every
/// workspace mutation resolves an [`Action`] (via `action`'s routing
/// functions) and runs through [`exec::execute`] — see the module docs.
async fn handle_mouse(
    app: &mut App,
    client: &ControlClient,
    mouse: MouseEvent,
    detached: &mut bool,
) -> Result<(), cyclops_tmux::TmuxError> {
    let col = mouse.column;
    let row = mouse.row;
    // Hover feeds the highlight on menu rows and dialog buttons; the
    // arm-side filter drops motion when neither is open.
    if matches!(mouse.kind, MouseEventKind::Moved) {
        app.hover = Some((col, row));
        settle_lost_release(app, client).await;
        return Ok(());
    }
    // An open dialog owns the mouse: its buttons respond, nothing else.
    if app.dialog.is_some() {
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
            let max_scroll = keybind_scroll_limit(app);
            // A picker notch moves the selection one row; the keybinds
            // card's moves its viewport three. The dialog says which list
            // the wheel is over.
            let rows = app.dialog.as_ref().map_or(1, dialog::dialog_wheel_rows);
            scroll_dialog(app, if up { -rows } else { rows }, max_scroll);
            // Same landing the arrow keys make: a theme goes live for the
            // next render, a sound row takes the check.
            exec::settings_cursor_moved(app, false);
            return Ok(());
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A press ends any previous pickup, whether or not it
                // starts a new one: a button released outside the terminal
                // never arrives here as an Up, and a card still holding the
                // pointer would then follow the next drag anywhere.
                drop_dialog_drag(app);
                match app.hit_map.hit(col, row) {
                    Some(HitTarget::DialogConfirm) => dialog_confirm(app, client).await?,
                    Some(HitTarget::DialogCancel) => dialog_cancel(app),
                    // Picking the card up. Nothing moves until the pointer
                    // does; the drags below carry it.
                    Some(HitTarget::DialogTitleBar) => {
                        app.drag = Some(DragState::on_down(DragTarget::Dialog, col, row));
                    }
                    Some(HitTarget::SettingsSection { section }) => {
                        let section = *section;
                        if let Some(open) = app.dialog.as_mut() {
                            dialog::show_settings_section(open, section);
                        }
                    }
                    // The mouse's half of the arrows: the row goes under
                    // the cursor and, for a theme, live for the next
                    // render; a sound row takes the check and plays on
                    // every click. Applying stays with Enter and the
                    // button.
                    Some(HitTarget::SettingsRow { index }) => {
                        let index = *index;
                        if let Some(open) = app.dialog.as_mut() {
                            dialog::select_settings_row(open, index);
                        }
                        // A view row is a switch, not a preview: the click
                        // that lands on it flips it, exactly as Enter would.
                        if matches!(
                            app.dialog,
                            Some(Dialog::Settings {
                                section: dialog::SettingsSection::View
                                    | dialog::SettingsSection::Delivery,
                                ..
                            })
                        ) {
                            dialog_confirm(app, client).await?;
                        } else {
                            exec::settings_cursor_moved(app, true);
                        }
                    }
                    _ => {}
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => carry_dialog_drag(app, col, row),
            MouseEventKind::Up(MouseButton::Left) => {
                carry_dialog_drag(app, col, row);
                drop_dialog_drag(app);
            }
            _ => {}
        }
        return Ok(());
    }
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            if app.menu.is_open() {
                return Ok(());
            }
            // Mid-drag, the wheel stays live over the pane being selected
            // so a selection can grow past one screen: exec's local scroll
            // moves the viewport and re-extends the selection under the
            // pointer. Every other target stays inert until the button
            // lifts, which is what the old whole-screen gate meant.
            if let Some(dragging) = app.selection.dragging_pane().map(str::to_string) {
                let over_dragging = matches!(
                    app.hit_map.hit(col, row),
                    Some(HitTarget::PaneBody { pane_id }) if *pane_id == dragging
                );
                if !over_dragging {
                    return Ok(());
                }
            }
            if let Some(target) = app.hit_map.hit(col, row).cloned() {
                let direction = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    action::ScrollDirection::Up
                } else {
                    action::ScrollDirection::Down
                };
                // The file panel scrolls its own list. It is the only
                // sidebar surface with more rows than it shows and a
                // pointer already over them, so the wheel belongs to it
                // here rather than to whatever is behind the sidebar.
                if matches!(
                    target,
                    HitTarget::FileRow { .. }
                        | HitTarget::FileDisclosure { .. }
                        | HitTarget::FileUp
                        | HitTarget::FileRoot
                        | HitTarget::FileBack
                        | HitTarget::FileForward
                ) {
                    let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                        -3
                    } else {
                        3
                    };
                    let rows = files_panel_rows(app);
                    app.files_tree_mut().scroll_by(delta, rows);
                    return Ok(());
                }
                // Only a `PaneBody` hit has a pane to resolve a cell
                // against; every other scrollable target (none exist
                // today) would just carry `None` through unchanged.
                let at = match &target {
                    HitTarget::PaneBody { pane_id } => app
                        .hit_map
                        .pane_geometry(pane_id)
                        .and_then(|geom| HitMap::cell_at(geom, col, row)),
                    _ => None,
                };
                if let Some(action) = action::route_mouse_scroll(&target, direction, at) {
                    let outcome = exec::execute(app, client, action).await?;
                    apply_outcome(app, outcome);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            let Some(target) = app.hit_map.hit(col, row).cloned() else {
                app.close_menu();
                return Ok(());
            };
            match &target {
                HitTarget::PaneBody { pane_id }
                | HitTarget::PaneFrame { pane_id }
                | HitTarget::PaneGrip { pane_id } => {
                    if let Some(action) = action::route_mouse_click(&target, MouseButton::Right) {
                        let outcome = exec::execute(app, client, action).await?;
                        apply_outcome(app, outcome);
                    }
                    app.open_menu(MenuState::ContextMenu {
                        pane_id: pane_id.clone(),
                        at: (col, row),
                    });
                }
                HitTarget::Tab { window_id } => {
                    app.open_menu(MenuState::TabMenu {
                        window_id: window_id.clone(),
                        at: (col, row),
                    });
                }
                HitTarget::SidebarRow { session, .. } => {
                    app.open_menu(MenuState::WorkspaceMenu {
                        session: session.clone(),
                        at: (col, row),
                    });
                }
                HitTarget::SidebarAgent { pane_id, .. } => {
                    if let Some(action) = action::route_mouse_click(&target, MouseButton::Right) {
                        let outcome = exec::execute(app, client, action).await?;
                        apply_outcome(app, outcome);
                    }
                    app.open_menu(MenuState::ContextMenu {
                        pane_id: pane_id.clone(),
                        at: (col, row),
                    });
                }
                _ => app.close_menu(),
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // A press ends any previous pickup before it can start a new
            // one: the button cannot go down again while it is still down,
            // so a drag still held here lost its release. Same guard the
            // dialog branch has; without it a divider drag that ended
            // outside the terminal finished on this click instead, with
            // the whole distance between the two as its resize.
            settle_lost_release(app, client).await;
            let Some(target) = app.hit_map.hit(col, row).cloned() else {
                app.close_menu();
                clear_selection(app);
                return Ok(());
            };
            match &target {
                HitTarget::MenuItem { action } => {
                    let action = *action;
                    let menu = std::mem::replace(&mut app.menu, MenuState::None);
                    app.hover = None;
                    app.hit_map.clear_menu_items();
                    let resolved = {
                        let ctx = route_context(app);
                        crate::action::route_menu_item(&menu, action, &ctx)
                    };
                    if let Some(resolved) = resolved {
                        let outcome = exec::execute(app, client, resolved).await?;
                        apply_outcome(app, outcome);
                        if outcome.detach {
                            *detached = true;
                        }
                    }
                    return Ok(());
                }
                HitTarget::PaneBody { pane_id } => {
                    app.close_menu();
                    app.messages_focused = false;
                    let pane_id = pane_id.clone();
                    let hit = HitTarget::PaneBody {
                        pane_id: pane_id.clone(),
                    };
                    let clicks = app.selection.register_click(&hit, col, row);
                    // Copied out so the hit-map borrow ends before the
                    // arms below take `app` mutably.
                    let picked = app.hit_map.pane_geometry(&pane_id).and_then(|geom| {
                        crate::input::mouse::HitMap::cell_at(geom, col, row)
                            .map(|cell| (cell, geom.inner.width))
                    });
                    if let Some((cell, width)) = picked {
                        match clicks {
                            2 => {
                                clear_selection(app);
                                if let Some(rt) = app.runtimes.get_mut(&pane_id) {
                                    let row_text = rt.row_text(cell.row);
                                    let (from, to) = crate::selection::word_range(&row_text, cell);
                                    rt.anchor_selection(from, to);
                                    app.selection.set_active(pane_id.clone());
                                }
                                copy_active_selection(app);
                            }
                            3 => {
                                clear_selection(app);
                                if let Some(rt) = app.runtimes.get_mut(&pane_id) {
                                    rt.anchor_selection(
                                        crate::runtime::CellPos {
                                            col: 0,
                                            row: cell.row,
                                        },
                                        crate::runtime::CellPos {
                                            col: width.saturating_sub(1),
                                            row: cell.row,
                                        },
                                    );
                                    app.selection.set_active(pane_id.clone());
                                }
                                copy_active_selection(app);
                            }
                            _ => {
                                clear_selection(app);
                                app.selection.press(pane_id.clone(), cell);
                            }
                        }
                    }
                }
                HitTarget::PaneFrame { pane_id } => {
                    // A frame click focuses, nothing more; the swap pickup
                    // lives on the corner grip, so the seam cells a frame
                    // shares with a sibling stay resize handles.
                    app.close_menu();
                    clear_selection(app);
                    // Except that a pane's top border is also the seam
                    // between it and the pane above, and the title strip
                    // painted along it had taken the whole row: a stacked
                    // pane could only be resized from its far edge. Press
                    // it and the seam moves; release without moving and it
                    // is the focus click it has always been.
                    if let Some((seam, dir)) = app.hit_map.divider_at(col, row) {
                        app.drag = Some(DragState::on_down(
                            DragTarget::Divider {
                                pane_id: seam.to_string(),
                                dir,
                                focus_on_click: Some(pane_id.clone()),
                            },
                            col,
                            row,
                        ));
                        return Ok(());
                    }
                }
                HitTarget::PaneMinimize { pane_id } => {
                    app.close_menu();
                    let outcome = exec::execute(
                        app,
                        client,
                        action::Action::ToggleMinimizePane {
                            pane_id: pane_id.clone(),
                        },
                    )
                    .await?;
                    apply_outcome(app, outcome);
                    return Ok(());
                }
                HitTarget::PaneGrip { pane_id } => {
                    app.close_menu();
                    clear_selection(app);
                    // Down on the grip starts a possible swap drag; a
                    // below-threshold release focuses the pane, same as a
                    // frame click. The body is untouched; dragging there
                    // is text selection.
                    app.drag = Some(DragState::on_down(
                        DragTarget::Pane {
                            pane_id: pane_id.clone(),
                        },
                        col,
                        row,
                    ));
                    return Ok(());
                }
                HitTarget::PaneSplitRight { .. } | HitTarget::PaneSplitDown { .. } => {
                    app.close_menu();
                }
                HitTarget::Divider { pane_id, dir } => {
                    clear_selection(app);
                    app.drag = Some(DragState::on_down(
                        DragTarget::Divider {
                            pane_id: pane_id.clone(),
                            dir: *dir,
                            // Bare gutter: no pane was pressed, so a
                            // release that never moved has nothing to
                            // focus.
                            focus_on_click: None,
                        },
                        col,
                        row,
                    ));
                    return Ok(());
                }
                HitTarget::Tab { window_id } => {
                    app.close_menu();
                    clear_selection(app);
                    // Down starts a possible reorder drag; a below-threshold
                    // release selects the tab instead.
                    app.drag = Some(DragState::on_down(
                        DragTarget::Tab {
                            window_id: window_id.clone(),
                        },
                        col,
                        row,
                    ));
                    return Ok(());
                }
                HitTarget::NewTabButton
                | HitTarget::ComposeButton
                | HitTarget::NewWorkspaceButton
                | HitTarget::SidebarTab { .. }
                | HitTarget::SidebarToggle
                | HitTarget::MessagesToggle
                | HitTarget::MessagesAction(_)
                | HitTarget::AttentionIndicator { .. } => {
                    app.close_menu();
                }
                HitTarget::SidebarRow {
                    session_id,
                    session,
                } => {
                    app.close_menu();
                    clear_selection(app);
                    app.drag = Some(DragState::on_down(
                        DragTarget::Workspace {
                            session_id: session_id.clone(),
                            session: session.clone(),
                        },
                        col,
                        row,
                    ));
                    return Ok(());
                }
                HitTarget::SidebarDisclosure { session_id } => {
                    toggle_workspace_expanded(&mut app.expanded_workspaces, session_id.clone());
                    return Ok(());
                }
                HitTarget::SidebarAgent {
                    workspace_id,
                    pane_id,
                    order_key,
                } => {
                    app.close_menu();
                    clear_selection(app);
                    app.drag = Some(DragState::on_down(
                        DragTarget::Agent {
                            workspace_id: workspace_id.clone(),
                            pane_id: pane_id.clone(),
                            order_key: order_key.clone(),
                        },
                        col,
                        row,
                    ));
                    return Ok(());
                }
                HitTarget::SidebarDivider => {
                    app.close_menu();
                    clear_selection(app);
                    app.drag = Some(DragState::on_down(DragTarget::Sidebar, col, row));
                    return Ok(());
                }
                HitTarget::MessagesDivider => {
                    app.close_menu();
                    clear_selection(app);
                    app.messages_focused = true;
                    app.drag = Some(DragState::on_down(DragTarget::Messages, col, row));
                    return Ok(());
                }
                HitTarget::SidebarSplit => {
                    app.close_menu();
                    clear_selection(app);
                    app.drag = Some(DragState::on_down(DragTarget::SidebarSplit, col, row));
                    return Ok(());
                }
                HitTarget::FileUp => {
                    app.close_menu();
                    if let Some(parent) = app.files_tree().parent() {
                        app.files_tree_mut().reroot(parent);
                        remember_pinned_root(app);
                    }
                    return Ok(());
                }
                HitTarget::FileRoot => {
                    app.close_menu();
                    match app.files_view {
                        // Back to where the work is. Asking the pane where
                        // it is costs a tmux round trip, so this only
                        // records the request; the loop's next pass arms
                        // the probe that answers it.
                        crate::files::FilesView::Agent => app.files_root_pending = true,
                        // Back to the folder the operator pinned. Saved
                        // state, no probe to wait for.
                        crate::files::FilesView::Pinned => {
                            if let Some(root) = app.prefs.files_pinned_root.clone() {
                                app.files_pinned.reroot(root);
                            }
                        }
                    }
                    return Ok(());
                }
                HitTarget::FilesViewToggle => {
                    app.close_menu();
                    // The outgoing view surrenders the keyboard: its
                    // cursor would otherwise sit armed and silently
                    // swallow bare keys the moment the operator toggles
                    // back, a mode nobody re-entered on purpose.
                    app.files_tree_mut().release_cursor();
                    app.files_view = app.files_view.other();
                    // The probe follows the panel; a fresh view may need
                    // its first root or a refresh right away.
                    arm_files_probe(app);
                    return Ok(());
                }
                HitTarget::FileBack => {
                    app.close_menu();
                    app.files_tree_mut().go_back();
                    remember_pinned_root(app);
                    return Ok(());
                }
                HitTarget::FileForward => {
                    app.close_menu();
                    app.files_tree_mut().go_forward();
                    remember_pinned_root(app);
                    return Ok(());
                }
                // The chevron column: open the folder where it sits, which
                // is what the whole row used to do.
                HitTarget::FileDisclosure { path } => {
                    app.close_menu();
                    let path = std::path::PathBuf::from(path.clone());
                    app.files_tree_mut().toggle(&path);
                    return Ok(());
                }
                // The rest of a folder's row walks into it. The panel is
                // narrow, so browsing one folder at a time beats nesting
                // everything under one root and running out of columns.
                HitTarget::FileRow { path, is_dir, .. } if *is_dir => {
                    app.close_menu();
                    let path = std::path::PathBuf::from(path.clone());
                    app.files_tree_mut().reroot(path);
                    remember_pinned_root(app);
                    return Ok(());
                }
                HitTarget::FileRow { reference, .. } => {
                    app.close_menu();
                    let reference = reference.clone();
                    let outcome =
                        exec::execute(app, client, action::Action::InsertFileRef { reference })
                            .await?;
                    apply_outcome(app, outcome);
                    return Ok(());
                }
                HitTarget::AppMenu => {
                    if app.menu == MenuState::AppMenu {
                        app.close_menu();
                    } else {
                        app.open_menu(MenuState::AppMenu);
                    }
                    return Ok(());
                }
                // Dialog rows only ever reach the mouse handler's own
                // dialog-is-open branch, which returns before this match.
                HitTarget::DialogConfirm
                | HitTarget::DialogCancel
                | HitTarget::DialogTitleBar
                | HitTarget::SettingsSection { .. }
                | HitTarget::SettingsRow { .. } => return Ok(()),
            }
            if let Some(action) = action::route_mouse_click(&target, MouseButton::Left) {
                let outcome = exec::execute(app, client, action).await?;
                apply_outcome(app, outcome);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.drag.is_some() {
                if let Some(drag) = app.drag.as_mut() {
                    drag.on_move(col, row);
                }
                if app.drag.as_ref().is_some_and(|drag| {
                    drag.is_active() && matches!(&drag.target, DragTarget::Sidebar)
                }) {
                    app.prefs.sidebar_width =
                        crate::render::sidebar_width_for_column(col, app.term_size.0);
                }
                if app.drag.as_ref().is_some_and(|drag| {
                    drag.is_active() && matches!(&drag.target, DragTarget::Messages)
                }) {
                    app.prefs.messages_width =
                        crate::render::messages_width_for_column(col, app.term_size.0);
                }
                if app.drag.as_ref().is_some_and(|drag| {
                    drag.is_active() && matches!(&drag.target, DragTarget::SidebarSplit)
                }) {
                    app.prefs.files_rows = files_rows_for_row(app, row);
                }
            } else if let Some(anchor) = app.selection.anchor_pane().map(str::to_string) {
                if let Some(geom) = app.hit_map.pane_geometry(&anchor) {
                    if let Some(cell) = crate::input::mouse::HitMap::cell_at(geom, col, row) {
                        let step = app.selection.drag_to(&anchor, cell);
                        if let Some(rt) = app.runtimes.get_mut(&anchor) {
                            match step {
                                crate::selection::DragStep::Begin { start, now } => {
                                    rt.begin_selection(start);
                                    rt.extend_selection(now);
                                }
                                crate::selection::DragStep::Extend { now } => {
                                    rt.extend_selection(now);
                                }
                                crate::selection::DragStep::None => {}
                            }
                        }
                    }
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(drag) = app.drag.as_mut() {
                drag.on_move(col, row);
            }
            let sidebar_drag = app.drag.as_ref().is_some_and(|drag| {
                drag.is_active() && matches!(&drag.target, DragTarget::Sidebar)
            });
            if sidebar_drag {
                app.prefs.sidebar_width =
                    crate::render::sidebar_width_for_column(col, app.term_size.0);
                app.save_prefs_or_log();
                // The panel settled at a new width, so the columns it gave
                // up or took belong to a different surface now.
                app.layout_changed();
                resize_client(app, client).await;
            }
            let messages_drag = app.drag.as_ref().is_some_and(|drag| {
                drag.is_active() && matches!(&drag.target, DragTarget::Messages)
            });
            if messages_drag {
                app.prefs.messages_width =
                    crate::render::messages_width_for_column(col, app.term_size.0);
                app.save_prefs_or_log();
                app.layout_changed();
                resize_client(app, client).await;
            }
            let split_drag = app.drag.as_ref().is_some_and(|drag| {
                drag.is_active() && matches!(&drag.target, DragTarget::SidebarSplit)
            });
            if split_drag {
                app.prefs.files_rows = files_rows_for_row(app, row);
                app.layout_changed();
                // No `resize_client`: this seam is inside the sidebar, so
                // no column changed hands and no pane reflows.
                app.save_prefs_or_log();
            }
            apply_live_divider(app, client).await?;
            if let Some(drag) = app.drag.take() {
                let crossed_threshold = drag.on_up().is_some();
                if crossed_threshold {
                    commit_drag_drop(app, client, &drag.target, col, row).await?;
                } else if let Some(action) = action::route_drag_click(&drag.target) {
                    let outcome = exec::execute(app, client, action).await?;
                    apply_outcome(app, outcome);
                }
            } else if app.selection.is_dragging() {
                if let Some(pane) = app.selection.finish_drag() {
                    copy_selection(app, &pane);
                }
            } else {
                let _ = app.selection.finish_drag();
            }
        }
        _ => {}
    }
    Ok(())
}

/// Carry an in-flight dialog drag to the pointer.
///
/// The offset accumulates from the last position the drag was applied at,
/// not from where it started, because the offset is clamped to what still
/// moves the box: measured from the start, a pointer that ran past the
/// screen edge would build up travel the clamp threw away, and the box
/// would not come back until the pointer had undone all of it.
fn carry_dialog_drag(app: &mut App, col: u16, row: u16) {
    let Some(drag) = app.drag.as_mut() else {
        return;
    };
    if !matches!(drag.target, DragTarget::Dialog) {
        return;
    }
    drag.on_move(col, row);
    let (from_col, from_row) = drag.last_applied;
    drag.last_applied = (col, row);
    let wanted = (
        app.dialog_offset.0.saturating_add(travel(from_col, col)),
        app.dialog_offset.1.saturating_add(travel(from_row, row)),
    );
    let area = Rect::new(0, 0, app.term_size.0, app.term_size.1);
    app.dialog_offset = match app.dialog.as_ref() {
        Some(dialog) => crate::render::clamp_dialog_offset(dialog, area, wanted),
        None => (0, 0),
    };
}

/// How many list rows the file panel is showing right now, for the wheel
/// to clamp against.
///
/// Read from the hit map the last frame pushed rather than recomputed from
/// the layout: what the wheel scrolls is the list the operator is looking
/// at, and a second derivation of the same number is a second thing that
/// can disagree with the paint.
fn files_panel_rows(app: &App) -> usize {
    app.hit_map
        .regions()
        .iter()
        .filter(|region| matches!(region.target, HitTarget::FileRow { .. }))
        .count()
}

/// The file panel's row count for a seam dragged to `row`.
///
/// Counted from the footer up, which is the same direction the preference
/// is stored in, so dragging the seam up grows the file panel. The paint
/// clamps against both panels' minimums, so an out-of-range answer here is
/// bounded rather than wrong; clamping to zero is only to keep the
/// unsigned arithmetic honest when the pointer runs past the footer.
fn files_rows_for_row(app: &App, row: u16) -> u16 {
    let areas = app.chrome(Rect::new(0, 0, app.term_size.0, app.term_size.1));
    let Some(sidebar) = areas.sidebar else {
        return app.prefs.files_rows;
    };
    // The same bottom the paint measures the seam from. Computing it here
    // instead put the seam one row above the pointer for the whole drag,
    // because the paint had given a row to the footer rule and this had not.
    let bottom = crate::render::sidebar_body_bottom(sidebar);
    bottom.saturating_sub(row).saturating_sub(1)
}

/// Whether this key is the prefix itself (`Ctrl+B`).
///
/// The file panel's key gate lets it through untouched so every chord keeps
/// working while the cursor is in the panel.
fn is_prefix_key(key: &KeyEvent) -> bool {
    key.code == crossterm::event::KeyCode::Char('b')
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Keys the file panel takes while it holds the cursor.
///
/// `None` means "not mine": the key falls through to the router, which is
/// how the prefix chords keep working from inside the panel. Everything
/// else is swallowed rather than forwarded to the pane. A half-mode where
/// arrows move a highlighted row but letters land in an agent's prompt is
/// worse than losing the keystrokes.
async fn handle_files_key(
    app: &mut App,
    client: &ControlClient,
    key: KeyEvent,
) -> Result<Option<InputOutcome>, cyclops_tmux::TmuxError> {
    use crossterm::event::KeyCode;

    match key.code {
        // Esc hands the keyboard back. It is the same key that cancels a
        // selection or a chrome drag, so those are cleared on the way in
        // (see `Action::FocusFiles`) and Esc has one owner at a time.
        KeyCode::Esc => {
            app.files_tree_mut().release_cursor();
            Ok(Some(InputOutcome::Redraw))
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.files_tree_mut().move_cursor(-1);
            Ok(Some(InputOutcome::Redraw))
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.files_tree_mut().move_cursor(1);
            Ok(Some(InputOutcome::Redraw))
        }
        // Left climbs out, right walks in: the two directions the panel
        // already navigates by mouse, on the keys that mean them.
        KeyCode::Left | KeyCode::Char('h') => {
            if let Some(parent) = app.files_tree().parent() {
                app.files_tree_mut().reroot(parent);
                remember_pinned_root(app);
            }
            Ok(Some(InputOutcome::Redraw))
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
            let Some(row) = app.files_tree().cursor_row() else {
                return Ok(Some(InputOutcome::Redraw));
            };
            match row.kind {
                crate::files::RowKind::Dir { .. } => {
                    let path = row.path.clone();
                    app.files_tree_mut().reroot(path);
                    remember_pinned_root(app);
                    Ok(Some(InputOutcome::Redraw))
                }
                crate::files::RowKind::File => {
                    // Same act the mouse performs, through the same
                    // action, so the two devices cannot drift apart.
                    let reference = app.files_tree().reference(&row.path.clone());
                    let outcome =
                        exec::execute(app, client, action::Action::InsertFileRef { reference })
                            .await?;
                    apply_outcome(app, outcome);
                    // The cursor stays: inserting several files in a row is
                    // the normal case, unlike walking into a folder.
                    Ok(Some(InputOutcome::Redraw))
                }
                // A truncation notice is not a row anything acts on.
                crate::files::RowKind::Truncated { .. } => Ok(Some(InputOutcome::Redraw)),
            }
        }
        _ => Ok(Some(InputOutcome::Redraw)),
    }
}

/// Keys the Messages pane and group-chat composer take while active.
async fn handle_messages_key(
    app: &mut App,
    key: KeyEvent,
) -> Result<Option<InputOutcome>, cyclops_tmux::TmuxError> {
    use crossterm::event::{KeyCode, KeyModifiers};

    if !app.model.messages_visible {
        return Ok(None);
    }

    if matches!(key.code, KeyCode::Char('r'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && (app.messages_focused || app.messages_refresh_error.is_some())
    {
        if matches!(
            app.messages_composer.stage,
            Some(cyclops_ui::Stage::Uncertain { .. })
        ) {
            app.messages_reconcile_owed = MessagesDraftIdentity::current(
                &app.messages_composer,
                app.messages_composer_revision,
                app.messages_caller,
            );
        }
        request_messages_snapshot(app);
        app.notice.show("Refreshing messages", Instant::now());
        return Ok(Some(InputOutcome::Redraw));
    }

    if !app.messages_focused {
        return Ok(None);
    }

    // If the composer is focused and active, it handles typing and sending:
    if app.messages_composer.focused && app.messages_composer.mode.is_some() {
        match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if app.messages_composer.push_char(c) {
                    messages_composer_changed(app);
                }
                return Ok(Some(InputOutcome::Redraw));
            }
            KeyCode::Backspace => {
                let before = app.messages_composer.text().len();
                app.messages_composer.backspace();
                if app.messages_composer.text().len() != before {
                    messages_composer_changed(app);
                }
                return Ok(Some(InputOutcome::Redraw));
            }
            KeyCode::Esc => {
                app.messages_composer.focused = false;
                app.messages_composer.mode = None;
                messages_composer_changed(app);
                return Ok(Some(InputOutcome::Redraw));
            }
            KeyCode::Enter => {
                // If the composer is in an Uncertain stage, send write may have already
                // completed at the daemon. Require explicit reconciliation before retry.
                if matches!(
                    app.messages_composer.stage,
                    Some(cyclops_ui::Stage::Uncertain { .. })
                ) {
                    app.notice.show(
                        "Send outcome unconfirmed by daemon: press Ctrl+R to reconcile or verify snapshot before retry",
                        Instant::now(),
                    );
                    return Ok(Some(InputOutcome::Redraw));
                }

                let text = app.messages_composer.text().to_string();
                if text.trim().is_empty() {
                    return Ok(Some(InputOutcome::NoRedraw));
                }
                let key_mint = format!("ws-msg-{}", uuid::Uuid::new_v4());
                let client_key = app.messages_composer.key_for_send(|| key_mint);
                let mode = app.messages_composer.mode.clone().unwrap();

                // Revalidate exact RecipientKeys against current live mailbox routes; never retarget by label
                let route = match mode.revalidate_routes(&app.decoration.mailbox_routes) {
                    Ok(res) => res,
                    Err(why) => {
                        app.messages_composer.record_not_sent(why.clone());
                        app.notice.show(format!("Refused: {why}"), Instant::now());
                        return Ok(Some(InputOutcome::Redraw));
                    }
                };
                let Some(caller) = app.messages_caller else {
                    app.messages_composer.record_not_sent(
                        "authenticated sender is unavailable; update Cyclops and reopen this workspace"
                            .into(),
                    );
                    return Ok(Some(InputOutcome::Redraw));
                };
                if app.messages_composer.sender != Some(caller) {
                    app.messages_composer.record_not_sent(
                        "the authenticated sender changed; close and reopen this composer".into(),
                    );
                    return Ok(Some(InputOutcome::Redraw));
                }

                queue_messages_send(
                    app,
                    MessagesSendAttempt {
                        composer_revision: app.messages_composer_revision,
                        mode,
                        caller,
                        recipient_keys: route.recipient_keys,
                        subject: route.subject,
                        body: text,
                        fyi: route.fyi,
                        reply_to: route.reply_to,
                        client_key,
                    },
                );
                return Ok(Some(InputOutcome::Redraw));
            }
            _ => return Ok(Some(InputOutcome::Redraw)),
        }
    }

    // When the composer is not focused, handle navigation and shortcut actions:
    match key.code {
        KeyCode::Enter => {
            if app.messages_detail.is_some() {
                return Ok(Some(InputOutcome::NoRedraw));
            }
            if let (Some(row), Some(target)) = (
                app.messages_queue.selected().cloned(),
                app.messages_queue.freeze(),
            ) {
                queue_message_detail(app, row, target);
                return Ok(Some(InputOutcome::Redraw));
            }
        }
        KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('q') => {
            if app.messages_detail.is_some() {
                app.messages_detail = None;
                return Ok(Some(InputOutcome::Redraw));
            }
            // Return keyboard focus to active terminal pane
            app.messages_focused = false;
            return Ok(Some(InputOutcome::Redraw));
        }
        KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(row) = app.messages_queue.selected().cloned() {
                messages_composer_changed(app);
                app.messages_composer = cyclops_ui::ComposerState::new_reply(
                    row.message_id,
                    row.sender,
                    row.sender_label,
                    row.subject,
                );
                app.messages_composer.bind_sender(app.messages_caller);
                return Ok(Some(InputOutcome::Redraw));
            }
        }
        KeyCode::Char('a') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let routes = &app.decoration.mailbox_routes;
            if routes.is_empty() {
                app.messages_composer.record_not_sent(
                    "no reachable mailbox routes available for announcement".into(),
                );
                app.notice
                    .show("Refused: no reachable mailbox routes", Instant::now());
                return Ok(Some(InputOutcome::Redraw));
            }
            // `@all` means everyone the Messages pane is showing: narrowed
            // to one session, an announcement stays inside that session.
            let session = app.messages_queue.session_filter();
            let recipients: Vec<(cyclops_proto::RecipientKey, String)> = routes
                .iter()
                .filter(|r| {
                    session.is_none_or(|filter| {
                        r.recipient
                            .pane_id()
                            .is_some_and(|pane| filter.panes.contains(&pane.to_string()))
                    })
                })
                .map(|r| (r.recipient, r.label.clone()))
                .collect();
            if recipients.is_empty() {
                app.messages_composer
                    .record_not_sent("no mailbox routes in this session".into());
                app.notice
                    .show("Refused: no mailbox routes in this session", Instant::now());
                return Ok(Some(InputOutcome::Redraw));
            }
            messages_composer_changed(app);
            app.messages_composer = cyclops_ui::ComposerState::new_announce(recipients);
            app.messages_composer.bind_sender(app.messages_caller);
            return Ok(Some(InputOutcome::Redraw));
        }
        KeyCode::Char('j') | KeyCode::Down if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(detail) = app.messages_detail.as_mut() {
                detail.scroll_by(1);
            } else {
                app.messages_queue.select_next();
            }
            return Ok(Some(InputOutcome::Redraw));
        }
        KeyCode::Char('k') | KeyCode::Up if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(detail) = app.messages_detail.as_mut() {
                detail.scroll_by(-1);
            } else {
                app.messages_queue.select_previous();
            }
            return Ok(Some(InputOutcome::Redraw));
        }
        KeyCode::Char('s') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let next = match app.messages_queue.scope() {
                cyclops_ui::Scope::Work => cyclops_ui::Scope::Inbox,
                cyclops_ui::Scope::Inbox => cyclops_ui::Scope::Outbound,
                cyclops_ui::Scope::Outbound => cyclops_ui::Scope::All,
                cyclops_ui::Scope::All => cyclops_ui::Scope::Work,
            };
            app.messages_queue.set_scope(next);
            return Ok(Some(InputOutcome::Redraw));
        }
        KeyCode::Char('c') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let through = app.messages_queue.watermark();
            app.prefs.messages_cleared_through_seq =
                app.prefs.messages_cleared_through_seq.max(through);
            app.messages_queue.replace(cyclops_ui::Snapshot {
                watermark: through,
                rows: Vec::new(),
            });
            app.messages_detail = None;
            app.save_prefs_or_log();
            app.notice.show(
                "Messages cleared from this view; durable history was preserved",
                Instant::now(),
            );
            return Ok(Some(InputOutcome::Redraw));
        }
        KeyCode::Char('t') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.messages_session_scoped = !app.messages_session_scoped;
            sync_messages_session_filter(app);
            return Ok(Some(InputOutcome::Redraw));
        }
        _ => {}
    }

    Ok(None)
}

fn apply_messages_presentation_cutoff(
    mut snapshot: cyclops_ui::Snapshot,
    cleared_through_seq: u64,
) -> cyclops_ui::Snapshot {
    snapshot.rows.retain(|row| row.seq > cleared_through_seq);
    snapshot
}

/// The session filter the Messages pane should apply right now: the active
/// workspace's name and the panes linked into its windows, or none when the
/// operator asked for every session.
///
/// Derived, never stored: tmux panes join and leave a session while the
/// Messages pane is open, and a stored pane set would go stale the moment
/// one did. The queue ignores a filter equal to its current one, so deriving
/// it every frame costs a small set build and no view rebuild.
fn messages_session_filter(app: &App) -> Option<cyclops_ui::SessionFilter> {
    if !app.messages_session_scoped {
        return None;
    }
    let workspace = app.model.workspaces.get(app.model.active_workspace)?;
    let panes = app
        .decoration
        .panes
        .values()
        .filter(|pane| workspace.window_ids.contains(&pane.window_id))
        .map(|pane| pane.pane_id.clone());
    Some(cyclops_ui::SessionFilter::new(
        workspace.name.clone(),
        panes,
    ))
}

fn sync_messages_session_filter(app: &mut App) {
    let filter = messages_session_filter(app);
    app.messages_queue.set_session_filter(filter);
}

/// One axis of pointer travel as a signed cell count.
fn travel(from: u16, to: u16) -> i16 {
    i16::try_from(i32::from(to) - i32::from(from)).unwrap_or(0)
}

/// Put the card down. Leaves every other drag alone: the dialog branch is
/// the only place a dialog drag is created, and it must not swallow a pane
/// or sidebar drag that a dialog opened on top of.
fn drop_dialog_drag(app: &mut App) {
    if app
        .drag
        .as_ref()
        .is_some_and(|drag| matches!(drag.target, DragTarget::Dialog))
    {
        app.drag = None;
    }
}

/// Commit a drag that crossed its move threshold: resolve the drop hit
/// target under the release point through `action`'s routing, and execute
/// whatever it resolves to. Only [`DragTarget::Pane`], `Tab`, `Workspace`,
/// and `Agent` resolve here: a divider applies live during motion
/// ([`apply_live_divider`]), a sidebar-width drag already applied and
/// persisted above, and a dialog drag never reaches this function at all
/// (an open dialog returns from its own branch of `handle_mouse`).
///
/// `Workspace` is the one variant that does NOT resolve through a dropped-
/// on hit target: [`resolve_workspace_slot_drop`] recomputes the exact
/// slot [`crate::drag::slot_for_row`] would preview at the release point,
/// so the drop always matches the live insertion rule the user watched
/// track their pointer — never a second, possibly disagreeing, resolution.
async fn commit_drag_drop(
    app: &mut App,
    client: &ControlClient,
    picked_up: &DragTarget,
    col: u16,
    row: u16,
) -> Result<(), cyclops_tmux::TmuxError> {
    let drop = app.hit_map.hit(col, row).cloned();
    let action = match picked_up {
        DragTarget::Pane { pane_id } => {
            drop.and_then(|drop| action::resolve_pane_drop(pane_id, &drop))
        }
        DragTarget::Tab { window_id } => {
            drop.and_then(|drop| action::resolve_tab_drop(window_id, &drop))
        }
        DragTarget::Workspace { session_id, .. } => {
            let sidebar = app
                .chrome(Rect::new(0, 0, app.term_size.0, app.term_size.1))
                .sidebar;
            resolve_workspace_slot_drop(&app.hit_map, sidebar, session_id, col, row)
        }
        DragTarget::Agent {
            workspace_id,
            order_key,
            ..
        } => {
            let order = agent_order_for_workspace(app, workspace_id);
            drop.and_then(|drop| action::resolve_agent_drop(workspace_id, order_key, &drop, &order))
        }
        // These apply themselves as the pointer moves and have nothing
        // left to resolve against whatever is under the release.
        DragTarget::Divider { .. }
        | DragTarget::Sidebar
        | DragTarget::Messages
        | DragTarget::SidebarSplit
        | DragTarget::Dialog => None,
    };
    let Some(action) = action else {
        return Ok(());
    };
    let outcome = exec::execute(app, client, action).await?;
    apply_outcome(app, outcome);
    Ok(())
}

/// Resolve a workspace-row drag release into the exact [`Action`] its live
/// preview showed. `None` — leave the order exactly as it was, dispatch
/// nothing — covers every case that is not a real move:
///
/// - the release point is outside the sidebar entirely (no rule was
///   showing there to honor);
/// - the sidebar is not visible at all (`sidebar` is `None`);
/// - the previewed slot is one of the two boundaries touching the dragged
///   row's own position (dropping it back where it started); or
/// - the dragged workspace has vanished from the model mid-drag (closed by
///   another client) — a stale drop, not a move.
///
/// This is a pure function of the last painted frame's hit rects and the
/// release point — no tmux call, nothing async — so both the rule's
/// destination and this function answer the identical question the
/// identical way; see [`crate::drag::slot_for_row`] and
/// [`crate::drag::insertion_for_slot`].
fn resolve_workspace_slot_drop(
    hit_map: &crate::input::mouse::HitMap,
    sidebar: Option<Rect>,
    session_id: &str,
    col: u16,
    row: u16,
) -> Option<action::Action> {
    let sidebar = sidebar?;
    if !sidebar.contains(ratatui::layout::Position::from((col, row))) {
        return None;
    }
    let blocks = hit_map.workspace_blocks();
    let slot = crate::drag::slot_for_row(&blocks, row);
    let insertion = crate::drag::insertion_for_slot(&blocks, session_id, slot)?;
    Some(action::Action::ReorderWorkspace {
        session_id: session_id.to_string(),
        insertion,
    })
}

/// The sidebar agent order for one workspace, keyed the same way
/// [`crate::decoration::DecorationSnapshot::agent_order_key`] does — the
/// context [`action::resolve_agent_drop`] needs to turn a drop into a
/// stable-id insertion.
fn agent_order_for_workspace(app: &App, workspace_id: &str) -> Vec<String> {
    app.model
        .workspaces
        .iter()
        .find(|workspace| workspace.session_id == workspace_id)
        .map(|workspace| {
            app.decoration
                .agent_rows_for_window_ids(&workspace.window_ids, &app.prefs.agent_order)
                .into_iter()
                .map(DecorationSnapshot::agent_order_key)
                .collect()
        })
        .unwrap_or_default()
}

fn copy_active_selection(app: &mut App) {
    let Some(pane) = app.selection.active_pane().map(str::to_string) else {
        return;
    };
    copy_selection(app, &pane);
}

/// Copy one finished selection: take the text, write the clipboard, say
/// what landed.
///
/// The clipboard write is the only step with an effect outside this
/// process, and it can never report back: OSC 52 is fire and forget and a
/// native tool's exit code says nothing about what the terminal did with
/// it. So the confirmation is built from what was extracted, and the two
/// halves either side of the write are what tests drive
/// ([`selection_text`] and [`announce_copy`]), rather than a test putting
/// its fixture on the machine's real clipboard.
fn copy_selection(app: &mut App, pane_id: &str) {
    let Some(text) = selection_text(app, pane_id) else {
        // An empty pick posts no notice, and the notice's expiry is what
        // normally clears the highlight state. Left active with nothing
        // to expire it, that state would eat the operator's next Escape,
        // which for an agent pane is an interrupt that never lands.
        clear_selection(app);
        return;
    };
    selection::copy_to_clipboard(&text);
    announce_copy(app, &text);
}

/// The text a pane's selection takes. `None` when the pane is gone or
/// the pick came back empty. An empty pick copies nothing, so it must not
/// claim to have copied anything either.
fn selection_text(app: &mut App, pane_id: &str) -> Option<String> {
    let runtime = app.runtimes.get(pane_id)?;
    selection::SelectionState::extract(runtime).filter(|text| !text.is_empty())
}

/// Forget the selection everywhere it lives: this state machine and the
/// owning pane's runtime, whose engine holds the geometry. Every clear
/// goes through here, because state cleared without the runtime leaves a
/// highlight nothing owns and no later event unpaints.
fn clear_selection(app: &mut App) {
    if let Some(pane) = app.selection.take_active() {
        if let Some(rt) = app.runtimes.get_mut(&pane) {
            rt.clear_selection();
        }
    }
    app.selection.clear();
}

/// Say what a copy took, on the notice line.
fn announce_copy(app: &mut App, text: &str) {
    app.notice.show(copy::copied(text), Instant::now());
}

/// The divider drag's pending motion since the last applied step, if any:
/// `(pane_id, axis, signed delta)`. A read-only helper so the borrow it
/// takes on `app.drag` ends before [`apply_live_divider`] needs `&mut App`
/// to run the executor.
fn pending_divider_resize(app: &App) -> Option<(String, SplitDir, i32)> {
    let drag = app.drag.as_ref()?;
    if !drag.is_active() {
        return None;
    }
    let DragTarget::Divider { pane_id, dir, .. } = drag.target.clone() else {
        return None;
    };
    let delta = match dir {
        SplitDir::Horizontal => drag.current.0 as i32 - drag.last_applied.0 as i32,
        SplitDir::Vertical => drag.current.1 as i32 - drag.last_applied.1 as i32,
    };
    Some((pane_id, dir, delta))
}

/// Apply divider motion since the last applied step as a resize-pane call.
/// tmux's `%layout-change` answers reconcile the model — the drag itself
/// never writes geometry.
async fn apply_live_divider(
    app: &mut App,
    client: &ControlClient,
) -> Result<bool, cyclops_tmux::TmuxError> {
    let Some((pane_id, axis, delta)) = pending_divider_resize(app) else {
        return Ok(false);
    };
    let Some(action) = action::resolve_pane_resize(&pane_id, axis, delta) else {
        return Ok(false);
    };
    exec::execute(app, client, action).await?;
    if let Some(drag) = app.drag.as_mut() {
        drag.last_applied = drag.current;
    }
    Ok(true)
}

/// Hand the pane canvas its new width while a sidebar drag is still
/// running, rather than only when it ends.
///
/// The sidebar's own rectangle follows the pointer on every motion event,
/// but the panes inside the canvas are laid out by tmux and do not move
/// until tmux is told the client changed size. Told only on release, the
/// columns the sidebar gave up sat as empty gutter for the whole drag:
/// the pane canvas is grounded in panel color first, so what the operator
/// saw was a widening dead strip down the right edge that snapped shut
/// when they let go. The geometry was never wrong — the sidebar's width,
/// the declared grid, the margins and the gap overhead account for every
/// column of the terminal at every width — the panes were simply still
/// the size they had been told to be.
///
/// This rides the render debounce, which is exactly where
/// [`apply_live_divider`] puts the same problem for pane dividers, and
/// [`resize_client`]'s own `declared_client_size` check collapses a burst
/// of motion into one tmux call per column actually crossed. A resize per
/// motion event would instead reflow every agent's TUI dozens of times a
/// second, which is the cost that put this on mouse-up to begin with.
async fn apply_live_sidebar(app: &mut App, client: &ControlClient) {
    let dragging = app
        .drag
        .as_ref()
        .is_some_and(|drag| drag.is_active() && matches!(drag.target, DragTarget::Sidebar));
    if dragging {
        resize_client(app, client).await;
    }
}

/// How often the file panel re-reads what it is showing.
///
/// A poll rather than a filesystem watch, which is the same call the theme
/// reload and the folder-follow already make. It costs one `read_dir` per
/// OPEN directory and answers "nothing moved" with a single integer
/// comparison ([`crate::files::FileTree::refresh`]), so a second is far
/// more often than it needs to be and still cheap. A watch would mean a
/// new dependency with a platform backend per OS, in a binary whose build
/// time is already something the operator notices.
const FILES_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Arm the next file-panel poll, unless one is already armed or the panel
/// is not on screen to poll for.
///
/// Three ways to be off screen, and all three have to gate this or the
/// loop wakes once a second forever to read a folder nobody is looking at:
/// the sidebar collapsed, the sidebar showing the event stream, and the
/// panel itself toggled shut. `files_root_pending` overrides them, because
/// that request is a tmux round trip the panel needs answered before it
/// can show anything at all, and it clears itself once it lands.
fn arm_files_probe(app: &mut App) {
    if app.files_probe_at.is_some() {
        return;
    }
    let showing = app.model.sidebar_visible
        && app.sidebar_tab == SidebarTab::Sessions
        && app.prefs.files_rows > 0;
    if showing || app.files_root_pending {
        app.files_probe_at = Some(Instant::now() + FILES_PROBE_INTERVAL);
    }
}

/// Re-read the file panel, and keep the agent view where the agent is.
///
/// Returns whether anything a reader would see moved, which is the only
/// thing that earns a redraw. Nothing here is fatal: a tmux probe that
/// fails leaves the request armed for the next poll, and an unreadable
/// folder simply lists as empty.
async fn probe_files(app: &mut App, client: &ControlClient) -> bool {
    let mut changed = false;
    let pane = app.model.active_tab().active_pane.clone();
    if let Ok(cwd) = client.display(&pane, "#{pane_current_path}").await {
        let cwd = cwd.trim();
        if !cwd.is_empty() {
            // The agent view follows the agent. An answer that differs
            // from the anchor means the agent itself moved — focus changed
            // panes, or the pane cd'd — and the view goes where it went.
            // An answer matching the anchor is the agent standing still,
            // so browsing the operator did inside the view is left alone.
            // (A cd has no tmux notification to subscribe to; this rides
            // the same once-a-second probe the panel already runs.)
            let moved = app.files.anchor() != std::path::Path::new(cwd);
            if app.files_root_pending || moved {
                app.files_root_pending = false;
                let before = app.files.root().to_path_buf();
                // Anchor and root together, and only here: the anchor is
                // what references are written from, so it follows the pane
                // rather than the browsing that happens after this.
                app.files.anchor_at(cwd);
                app.files.reroot(cwd);
                changed |= app.files.root() != before;
            }
            // References from the pinned view are sent to the same agent,
            // so its anchor tracks the same folder. Its ROOT does not: the
            // one exception is a pinned view that has never been anywhere,
            // which starts at the agent's folder so it is browsable at all
            // (browsing it afterwards is what pins it, and what persists:
            // `remember_pinned_root`).
            app.files_pinned.anchor_at(cwd);
            if !app.files_pinned.has_root() {
                app.files_pinned.reroot(cwd);
            }
        }
    }
    changed | app.files_tree_mut().refresh()
}

/// A browse in the pinned view is what "pin it" means: the folder the
/// operator lands on becomes the saved pinned root. Called after every
/// user navigation; the agent view saves nothing (following is its whole
/// contract), and an unchanged root writes nothing.
fn remember_pinned_root(app: &mut App) {
    if app.files_view != crate::files::FilesView::Pinned || !app.files_pinned.has_root() {
        return;
    }
    let root = Some(app.files_pinned.root().to_string_lossy().into_owned());
    if app.prefs.files_pinned_root != root {
        app.prefs.files_pinned_root = root;
        app.save_prefs_or_log();
    }
}

/// Arm one delayed folder probe, if the active workspace follows its folder
/// and none is armed already.
///
/// Never pushes an armed probe back — the same rule [`arm`] follows for the
/// render deadline, and for the same reason: a pane producing a steady
/// stream of output would otherwise postpone the probe forever. Workspaces
/// that do not follow a folder arm nothing, so they add no wakeups.
fn arm_folder_probe(app: &mut App) {
    if app.folder_probe_at.is_some() {
        return;
    }
    let follows = app
        .model
        .workspaces
        .get(app.model.active_workspace)
        .is_some_and(|workspace| app.prefs.folder_tracked.contains(&workspace.session_id));
    if follows {
        app.folder_probe_at = Some(Instant::now() + FOLDER_PROBE_DELAY);
    }
}

/// Keep a folder-following workspace's name on its pane's directory.
///
/// A `cd` produces no tmux notification, so there is nothing to subscribe
/// to; what a `cd` DOES produce is pane output, and pane output is what
/// arms the render deadline. Riding that deadline means the check happens
/// exactly when the screen changed and never on a clock, rate-limited so a
/// noisy pane cannot turn every frame into a tmux round trip.
async fn follow_workspace_folder(
    app: &mut App,
    client: &ControlClient,
) -> Result<(), cyclops_tmux::TmuxError> {
    let Some(workspace) = app.model.workspaces.get(app.model.active_workspace) else {
        return Ok(());
    };
    if !app.prefs.folder_tracked.contains(&workspace.session_id) {
        return Ok(());
    }
    let session_id = workspace.session_id.clone();
    let current_name = workspace.name.clone();

    let pane = app.model.active_tab().active_pane.clone();
    let Ok(cwd) = client.display(&pane, "#{pane_current_path}").await else {
        // A transient tmux failure here must never be fatal — the next
        // probe tries again.
        return Ok(());
    };

    let taken: Vec<String> = app
        .model
        .workspaces
        .iter()
        .filter(|w| w.session_id != session_id)
        .map(|w| w.name.clone())
        .collect();
    let Some(next) = naming::folder_rename(&current_name, cwd.trim(), &taken) else {
        return Ok(());
    };

    client.rename_session(&current_name, &next).await?;

    // The model addresses the active session BY NAME; skip this and the
    // next reconcile queries a session that no longer exists. The sidebar
    // row carries that same name, and the reconcile that refreshes it is a
    // deadline away — leaving the stale name there would let the next probe
    // rename a target tmux no longer knows.
    if app.model.session.session == current_name {
        app.model.session.session = next.clone();
    }
    if let Some(row) = app.model.workspaces.get_mut(app.model.active_workspace) {
        row.name = next.clone();
    }
    if persist::migrate_order_entry(&mut app.prefs.workspace_order, &current_name, &next) {
        app.save_prefs_or_log();
    }
    app.needs_reconcile = true;
    Ok(())
}

/// Apply the open dialog's action (Enter or its confirm button).
/// Route the open dialog's confirmation to a terminal [`Action`] and run it
/// through the executor. A dialog whose buffer resolves to nothing (a blank
/// rename, or the read-only keybinds card) still
/// dismisses on Enter — it just does so here instead of inside an executor
/// arm, since there is no action to hand it.
async fn dialog_confirm(
    app: &mut App,
    client: &ControlClient,
) -> Result<(), cyclops_tmux::TmuxError> {
    if handle_compose_confirm(app) {
        return Ok(());
    }
    let Some(dialog) = app.dialog.clone() else {
        return Ok(());
    };
    let Some(action) = action::route_dialog_confirm(&dialog) else {
        // Dismissing without an action is a cancel: same close path, so a
        // theme picker with nothing to apply also restores its paint.
        dialog_cancel(app);
        return Ok(());
    };
    let outcome = exec::execute(app, client, action).await?;
    apply_outcome(app, outcome);
    Ok(())
}

fn handle_compose_confirm(app: &mut App) -> bool {
    let Some(Dialog::Compose { send, .. }) = app.dialog.as_ref() else {
        return false;
    };
    if send.is_sending() {
        return true;
    }
    if send.is_confirming_abandon() {
        app.dialog = None;
        app.hover = None;
        return true;
    }
    false
}

fn dialog_cancel(app: &mut App) {
    if let Some(open) = app.dialog.as_mut() {
        if dialog::request_compose_cancel(open) == dialog::ComposeCancel::KeepOpen {
            app.hover = None;
            return;
        }
    }
    // Close-without-apply: the theme picker previews over the live paint,
    // so the theme that was live when it opened goes back. `None` for
    // every other dialog, and for an applied picker (the apply drops it).
    if let Some(theme) = app.theme_restore.take() {
        app.paint.theme = theme;
    }
    app.dialog = None;
    app.hover = None;
}

/// Paste one outer-terminal bracketed paste into the focused pane in two
/// tmux commands, regardless of payload length. On failure after load, the
/// server-global buffer is removed best-effort so pasted text cannot linger.
async fn paste_into_focused_pane(
    app: &mut App,
    client: &ControlClient,
    bytes: &[u8],
) -> Result<(), cyclops_tmux::TmuxError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let pane_id = app.model.active_tab().active_pane.clone();
    if pane_id.is_empty() {
        return Ok(());
    }
    // Paste is pane input just as surely as a keypress. Keep the viewport
    // contract provider-neutral: an operator pasting at a live prompt must
    // see that prompt and the resulting output, not remain pinned in the
    // pane's local history while the bytes land below the fold.
    snap_pane_to_tail(app, &pane_id);
    let buffer = format!("cyclops-workspace-{}-{}", std::process::id(), app.paste_seq);
    app.paste_seq = app.paste_seq.checked_add(1).ok_or_else(|| {
        cyclops_tmux::TmuxError::Protocol("workspace paste sequence overflow".into())
    })?;
    client.load_buffer(&buffer, bytes).await?;
    if let Err(error) = client.paste_buffer(&buffer, &pane_id, true, true).await {
        let _ = client.delete_buffer(&buffer).await;
        return Err(error);
    }
    Ok(())
}

/// How far the keybinds card can scroll on this terminal: its rows past
/// the list the card has room for. Zero for every other dialog, and for
/// no dialog.
fn keybind_scroll_limit(app: &App) -> u16 {
    let Some(Dialog::Keybinds { rows, .. }) = app.dialog.as_ref() else {
        return 0;
    };
    crate::render::keybind_max_scroll(
        rows.len(),
        Rect::new(0, 0, app.term_size.0, app.term_size.1),
    )
}

/// Move whichever list an open dialog scrolls `delta` rows: the keybinds
/// card's viewport within `max_scroll`, or the settings card's cursor.
/// One place, so the key path and the wheel path cannot disagree about
/// which list they are on.
fn scroll_dialog(app: &mut App, delta: i16, max_scroll: u16) {
    match app.dialog.as_mut() {
        Some(Dialog::Keybinds { scroll, .. }) => {
            *scroll = dialog::move_keybind_scroll(*scroll, delta, max_scroll);
        }
        Some(open @ Dialog::Settings { .. }) => dialog::move_settings_selection(open, delta),
        _ => {}
    }
}

/// Install one decoration snapshot: the offline bookkeeping and the
/// agent-order migration every arrival path shares (the forwarder's
/// coalesced refresh, its offline report, and the reconnect resync).
fn apply_decoration_snapshot(app: &mut App, snapshot: DecorationSnapshot) {
    // A daemon that went away forgot every session it was asked to
    // watch: those live in memory, not in config.toml. Dropping the
    // record here is what makes the next reconcile ask again.
    if !snapshot.online {
        app.watched_sessions.clear();
    }
    if persist::migrate_agent_order_entries(&mut app.prefs.agent_order, &app.decoration, &snapshot)
    {
        app.save_prefs_or_log();
    }
    // Before the swap, while both snapshots are in hand: the cue is about
    // the difference between them (`crate::sound` says which differences).
    if app.prefs.sound_notifs
        && crate::sound::background_state_changed(
            &app.decoration,
            &snapshot,
            &app.model.active_tab().active_pane,
            app.window_focused,
        )
    {
        crate::sound::play(&app.home, &app.prefs.sound);
    }
    app.decoration = snapshot;
}

/// The daemon subscription (re)connected: pull back what the outage lost
/// on both sides, without waiting for a structural reconcile. The watch
/// record is forgotten first because a restarted daemon has no pane table
/// for UI-created sessions until each is re-asked (asking for a session
/// it already watches returns the existing slot, so a reconnect that was
/// only a socket blip costs nothing); the asks land before the fetch so
/// the snapshot already covers them. The fetch then replaces the whole
/// snapshot, because a state that flipped while nothing was subscribed
/// produced no event and would otherwise stay on screen as stale.
fn resync_daemon_state(app: &mut App) {
    app.watched_sessions.clear();
    ensure_sessions_watched(app);
    match decoration::fetch_decoration(&app.home) {
        Ok(snapshot) => apply_decoration_snapshot(app, snapshot),
        Err(error) => log_err(&app.home, &format!("decoration resync failed: {error}")),
    }
}

/// Rebuild every daemon-owned projection after a new subscription connects.
///
/// A fresh workspace process has no prior Messages snapshot to retain, so the
/// collapsed rail must request the same authenticated body-free projection as
/// an already-running workspace recovering from a socket gap.
fn apply_daemon_reconnected(app: &mut App) {
    resync_daemon_state(app);
    app.messages_gate.connected();
    app.messages_gate.mark_dirty();
    pump_messages_refresh(app);
}

async fn reconcile(app: &mut App, client: &ControlClient) -> Result<(), cyclops_tmux::TmuxError> {
    let session = app.model.session.session.clone();
    let mut model = fetch_workspace_model(client, &session).await?;
    apply_workspace_order(&mut model, &app.prefs.workspace_order);
    install_reconciled_model(
        &mut app.model,
        model,
        app.prefs.sidebar_visible,
        app.prefs.messages_visible,
    );

    let session = app.model.session.session.clone();
    let tabs = app.model.session.tabs.clone();
    let adopted = adopt_windows(&mut app.sizing, client, &session, &tabs, &app.home).await;
    if adopted.newly_following {
        app.notice
            .show(copy::SIZING_FOLLOWER.to_string(), Instant::now());
    }
    if adopted.authority_transferred || adopted.took_a_window {
        // A window pinned just now is holding whatever size it had before
        // this workspace touched it, and the canvas may not have moved, so
        // the unchanged-canvas guard in `resize_client` would skip it and
        // leave a new tab laid out at the wrong size.
        app.declared_client_size = None;
    }
    resize_client(app, client).await;

    // Fetch fresh model after resize and recovery so dimensions and
    // provenance are up-to-date with live tmux state before hydration and rendering.
    let mut fresh_model = fetch_workspace_model(client, &session).await?;
    apply_workspace_order(&mut fresh_model, &app.prefs.workspace_order);
    install_reconciled_model(
        &mut app.model,
        fresh_model,
        app.prefs.sidebar_visible,
        app.prefs.messages_visible,
    );

    app.minimized = app.model.active_tab().minimized.clone();
    // An authoritative replacement: panes may have appeared, closed,
    // moved window, or changed proportion since the frame on screen.
    app.layout_changed();
    expand_active_workspace(
        &app.model.workspaces,
        app.model.active_workspace,
        &mut app.expanded_for,
        &mut app.expanded_workspaces,
    );
    // Before the snapshot, not after: a session the daemon starts watching
    // now is one this same reconcile can already show agents for.
    ensure_sessions_watched(app);
    // Keep what the last answer said when this one does not arrive: a
    // reconcile that cannot reach the daemon knows nothing new about the
    // roster, and blanking it would un-name every agent on screen.
    match decoration::fetch_decoration(&app.home) {
        Ok(snapshot) => app.decoration = snapshot,
        Err(error) => log_err(&app.home, &format!("decoration reconcile failed: {error}")),
    }
    app.persist_active();
    // Forced only when continuity was actually lost: a control-mode
    // reconnect missed %output while the layout stood still, so the size
    // check would leave same-sized panes showing stale content and stale
    // VT modes until something resized them (the scroll bug an operator
    // could only fix by resizing). Every OTHER reconcile keeps the size
    // gate — this path also runs on %window-renamed, which tmux's
    // automatic-rename fires for every command a shell pane runs, and a
    // forced hydrate there would snap scrolled viewports to the tail once
    // a second.
    if std::mem::take(&mut app.needs_forced_hydrate) {
        prepare_forced_hydration(app);
        crate::sync::hydrate_visible_tab_forced(client, app.model.active_tab(), &mut app.runtimes)
            .await?;
    } else {
        hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await;
    }
    app.needs_hydrate = false;
    Ok(())
}

/// Make sure cyclopsd is watching every workspace on screen.
///
/// The sidebar's whole agent hierarchy comes from the daemon's pane table,
/// and the daemon only builds one for sessions it watches. Workspaces this
/// UI creates are not in config.toml, so without this their rows would show
/// nothing no matter what was running in them.
///
/// One ask per session id, ever. See [`crate::daemon::watch_session`] for
/// why a rename must not produce a second ask.
fn ensure_sessions_watched(app: &mut App) {
    for workspace in &app.model.workspaces {
        if app.watched_sessions.contains(&workspace.session_id) {
            continue;
        }
        match crate::daemon::watch_session(&app.home, &workspace.name) {
            Ok(()) => {
                app.watched_sessions.insert(workspace.session_id.clone());
            }
            // A daemon that is down is a normal state for this UI: the rest
            // of the workspace works without it. Leaving the id unrecorded
            // is what makes the next reconcile try again.
            Err(error) => log_err(&app.home, &error),
        }
    }
}

/// Open the active workspace in the sidebar when it becomes the active one.
///
/// Agent rows are children of an expanded workspace, so a workspace you just
/// created or switched to would otherwise hide the agents it was opened for.
/// Only the CHANGE opens it: re-expanding on every reconcile would leave the
/// disclosure triangle unable to close the row you are actually looking at.
fn expand_active_workspace(
    workspaces: &[crate::model::WorkspaceRow],
    active: usize,
    expanded_for: &mut Option<String>,
    expanded: &mut HashSet<String>,
) {
    let Some(row) = workspaces.get(active) else {
        return;
    };
    if expanded_for.as_deref() == Some(row.session_id.as_str()) {
        return;
    }
    *expanded_for = Some(row.session_id.clone());
    expanded.insert(row.session_id.clone());
}

/// Apply the persisted visual order without changing tmux session identity.
/// Rows absent from the preference append in tmux's deterministic order.
fn apply_workspace_order(model: &mut WorkspaceModel, order: &[String]) {
    let active_id = model
        .workspaces
        .get(model.active_workspace)
        .map(|workspace| workspace.session_id.clone());
    let mut remaining = std::mem::take(&mut model.workspaces);
    let mut ordered = Vec::with_capacity(remaining.len());
    for name in order {
        if let Some(index) = remaining
            .iter()
            .position(|workspace| &workspace.name == name)
        {
            ordered.push(remaining.remove(index));
        }
    }
    ordered.extend(remaining);
    model.workspaces = ordered;
    model.active_workspace = active_id
        .and_then(|id| {
            model
                .workspaces
                .iter()
                .position(|workspace| workspace.session_id == id)
        })
        .or_else(|| {
            model
                .workspaces
                .iter()
                .position(|workspace| workspace.name == model.session.session)
        })
        .unwrap_or(0);
}

/// Replace local structure with one authoritative tmux snapshot while
/// retaining only UI-owned preferences. In particular, the snapshot's
/// active window wins: preserving the old window when it still exists
/// would leave a newly created or externally selected tab invisible.
fn install_reconciled_model(
    current: &mut WorkspaceModel,
    mut fresh: WorkspaceModel,
    sidebar_visible: bool,
    messages_visible: bool,
) {
    // The one place a whole model is replaced by a fresh snapshot, so it
    // is the one place that knows the layout on screen may no longer be
    // the layout that was drawn. Panes may have appeared, closed, moved
    // window, or changed proportion, and a diff frame writes only what
    // the new model believes changed.
    fresh.sidebar_visible = sidebar_visible;
    fresh.messages_visible = messages_visible;
    *current = fresh;
}

/// Restore the two visibility choices owned by workspace preferences.
///
/// Boot uses this before its first geometry declaration, so a workspace that
/// was detached with the Messages pane collapsed does not briefly reserve or
/// paint the expanded rail when it is attached again.
fn apply_saved_workspace_visibility(model: &mut WorkspaceModel, prefs: &WorkspacePrefs) {
    model.sidebar_visible = prefs.sidebar_visible;
    model.messages_visible = prefs.messages_visible;
}

/// Forward one already-routed input event, retaining its exact target and
/// key batch when the bounded reply FIFO is temporarily full.
/// A key typed into a pane means the operator is at its prompt, so the
/// viewport returns to the live tail first, the way every terminal
/// emulator does. Without this a pane the wheel had left a few lines back
/// stayed there: new output kept the view pinned to the history it was
/// reading, the prompt the keys went to was below the fold, and the pane
/// read as "locked" until the operator scrolled all the way down by hand.
/// Returns whether the viewport moved.
fn snap_pane_to_tail(app: &mut App, pane: &str) -> bool {
    app.runtimes
        .get_mut(pane)
        .is_some_and(|runtime| runtime.scroll_to_tail())
}

/// A snapped viewport is a visible change even when the keys themselves
/// earned no frame.
fn redraw_if_snapped(outcome: InputOutcome, snapped: bool) -> InputOutcome {
    if snapped && matches!(outcome, InputOutcome::NoRedraw) {
        InputOutcome::Redraw
    } else {
        outcome
    }
}

async fn forward_pane_input(
    client: &ControlClient,
    pane: String,
    keys: Vec<String>,
) -> InputOutcome {
    let borrowed: Vec<&str> = keys.iter().map(String::as_str).collect();
    let result = client.send_keys_unconfirmed(&pane, &borrowed).await;
    pane_input_outcome(pane, keys, result)
}

fn pane_input_outcome(
    pane: String,
    keys: Vec<String>,
    result: Result<(), TmuxError>,
) -> InputOutcome {
    match result {
        Ok(()) => InputOutcome::NoRedraw,
        Err(TmuxError::Busy) => InputOutcome::Pending(PendingPaneInput { pane, keys }),
        Err(error @ TmuxError::WriteUncertain(_)) => InputOutcome::Uncertain { pane, error },
        Err(error) => InputOutcome::NotSent { pane, error },
    }
}

/// Route one key to an [`Action`] (via [`action::route_binding`]) and run
/// it through the executor. Key passthrough to the focused pane is NOT an
/// action and stays on its own fast path below, unconfirmed and untouched.
async fn handle_key(
    app: &mut App,
    client: &ControlClient,
    key: KeyEvent,
) -> Result<InputOutcome, cyclops_tmux::TmuxError> {
    if app.dialog.is_some() {
        return handle_dialog_key(app, client, key).await;
    }
    // The file panel holds the keyboard only while its cursor exists, and
    // only a chord creates that cursor. That is the whole reason bare
    // arrows still reach the focused pane: with no cursor this branch is
    // not taken, nothing about the router changes, and every shell and
    // agent keeps its history recall and its menus.
    //
    // Ctrl+B is handed on rather than eaten. A mode that swallowed the
    // prefix could not detach, could not switch tabs, and could not
    // collapse the very panel the cursor sits in, which makes it a trap
    // rather than a mode.
    if app.files_tree().cursor().is_some() && !is_prefix_key(&key) && !app.router.prefix_armed() {
        if let Some(outcome) = handle_files_key(app, client, key).await? {
            return Ok(outcome);
        }
    }
    if app.model.messages_visible && !is_prefix_key(&key) && !app.router.prefix_armed() {
        if let Some(outcome) = handle_messages_key(app, key).await? {
            return Ok(outcome);
        }
    }
    match app.router.route(key) {
        RouterResult::PrefixArmed => Ok(InputOutcome::NoRedraw),
        RouterResult::Consumed => Ok(InputOutcome::NoRedraw),
        RouterResult::Action(binding) => {
            let resolved = {
                let ctx = route_context(app);
                // Shift on the chord upgrades directional focus to a pane
                // swap (Ctrl+B Shift+Arrow). Only a prefix chord can carry
                // the upgrade: the router matches prefix chords by key
                // code alone, so Shift is unread information there, while
                // a direct chord matched its modifiers exactly and an
                // explicit shift+arrow focus binding must stay focus.
                let prefix_shift = key.modifiers.contains(KeyModifiers::SHIFT)
                    && matches!(
                        app.router.chord(binding),
                        Some(crate::bindings::BindingChord::Prefix(_))
                    );
                if prefix_shift {
                    action::route_binding_shifted(binding, &ctx)
                } else {
                    action::route_binding(binding, &ctx)
                }
            };
            let Some(resolved) = resolved else {
                return Ok(InputOutcome::NoRedraw);
            };
            let outcome = exec::execute(app, client, resolved).await?;
            apply_outcome(app, outcome);
            Ok(if outcome.detach {
                InputOutcome::Detached
            } else {
                InputOutcome::Redraw
            })
        }
        RouterResult::PassThrough(key) => {
            let pane = app.model.active_tab().active_pane.clone();
            if pane.is_empty() {
                return Ok(InputOutcome::NoRedraw);
            }
            match app.select_all.on_key(&pane, &key) {
                crate::input::SelectAllOutcome::Armed => return Ok(InputOutcome::NoRedraw),
                crate::input::SelectAllOutcome::ClearLine => {
                    // Cursor to end first, so kill-to-start takes the
                    // whole line no matter where the cursor sat.
                    let snapped = snap_pane_to_tail(app, &pane);
                    let outcome = forward_pane_input(
                        client,
                        pane,
                        vec!["C-e".to_string(), "C-u".to_string()],
                    )
                    .await;
                    return Ok(redraw_if_snapped(outcome, snapped));
                }
                crate::input::SelectAllOutcome::Forward => {}
            }
            let encoded = encode_send_keys(&key);
            if !encoded.is_empty() {
                let snapped = snap_pane_to_tail(app, &pane);
                let outcome = forward_pane_input(client, pane, encoded).await;
                return Ok(redraw_if_snapped(outcome, snapped));
            }
            Ok(InputOutcome::NoRedraw)
        }
    }
}

async fn handle_dialog_key(
    app: &mut App,
    client: &ControlClient,
    key: KeyEvent,
) -> Result<InputOutcome, cyclops_tmux::TmuxError> {
    let Some(dialog) = app.dialog.as_ref() else {
        return Ok(InputOutcome::NoRedraw);
    };
    let action = dialog::dialog_key_action(dialog, &key);
    let max_scroll = keybind_scroll_limit(app);
    match action {
        DialogKeyAction::Cancel => {
            dialog_cancel(app);
            if key.code == crossterm::event::KeyCode::Esc {
                clear_selection(app);
                cancel_drag(app);
            }
        }
        DialogKeyAction::Confirm => dialog_confirm(app, client).await?,
        DialogKeyAction::Backspace => {
            if let Some(buffer) = app.dialog.as_mut().and_then(dialog::dialog_buffer_mut) {
                buffer.pop();
            }
            if let Some(Dialog::NamePane { error, .. }) = app.dialog.as_mut() {
                *error = None;
            }
        }
        DialogKeyAction::Append(c) => {
            let mut encoded = [0; 4];
            dialog::append_dialog_text(app.dialog.as_mut(), c.encode_utf8(&mut encoded));
        }
        // Straight onto the buffer rather than through `append_dialog_text`:
        // that one filters exactly the character being asked for here.
        DialogKeyAction::Newline => {
            if let Some(buffer) = app.dialog.as_mut().and_then(dialog::dialog_buffer_mut) {
                buffer.push('\n');
            }
        }
        DialogKeyAction::Scroll(delta) => {
            scroll_dialog(app, delta, max_scroll);
            // The row the arrows land on goes live, or takes the check,
            // for the next render (a no-op for every other dialog).
            exec::settings_cursor_moved(app, false);
        }
        DialogKeyAction::SwitchSection(delta) => {
            if let Some(open) = app.dialog.as_mut() {
                dialog::switch_settings_section(open, delta);
            }
        }
        DialogKeyAction::Adjust(delta) => {
            let changed = app
                .dialog
                .as_mut()
                .is_some_and(|open| dialog::adjust_force_submit_delay(open, delta));
            if changed {
                let setting = app.dialog.as_ref().and_then(|open| match open {
                    Dialog::Settings { delivery, .. } => {
                        Some((delivery.enabled, delivery.delay_seconds))
                    }
                    _ => None,
                });
                if let Some((enabled, delay_seconds)) = setting {
                    let outcome = exec::execute(
                        app,
                        client,
                        action::Action::ApplyForceSubmitSettings {
                            enabled,
                            delay_seconds,
                        },
                    )
                    .await?;
                    apply_outcome(app, outcome);
                }
            }
        }
        DialogKeyAction::ScrollStart | DialogKeyAction::ScrollEnd => {
            let to_end = action == DialogKeyAction::ScrollEnd;
            match app.dialog.as_mut() {
                Some(Dialog::Keybinds { scroll, .. }) => {
                    *scroll = if to_end { max_scroll } else { 0 };
                }
                Some(open @ Dialog::Settings { .. }) => {
                    dialog::jump_settings_selection(open, to_end);
                }
                _ => {}
            }
            exec::settings_cursor_moved(app, false);
        }
        DialogKeyAction::Ignore => {}
    }
    Ok(if action == DialogKeyAction::Ignore {
        InputOutcome::NoRedraw
    } else {
        InputOutcome::Redraw
    })
}

/// Compose and write one frame.
///
/// `now` is this wake's instant, shared with the animation clock so the
/// factors a frame paints with and the deadline it arms come from the same
/// reading. `observe` runs first and is the only place an animation starts;
/// see `crate::animate` for why arming is a diff rather than a call at each
/// site.
/// How long a host resize must stop arriving before tmux is told.
///
/// A pointer drag on a window edge delivers a resize per frame the host
/// renders, roughly every 16ms at 60Hz, so anything at or below that
/// settles mid-drag and sends again. Three of those is the smallest
/// window that cannot, and 50ms is imperceptible once the drag stops.
/// Not a poll: it only decides whether the beat that already runs may
/// send yet.
const RESIZE_SETTLE: Duration = Duration::from_millis(50);

/// Answer a resize burst that has stopped moving: one tmux call and one
/// repaint, at the size the drag ended on.
///
/// Answered by its own one-shot deadline rather than by the render beat,
/// and nothing re-arms it: each arriving resize replaces the deadline and
/// expiry clears it, so a drag of any length costs one wake, one
/// `resize-pane`, and one full repaint however many events the host sent.
/// Returns whether it acted, which is the caller's cue to draw the frame
/// the repaint asked for.
async fn apply_settled_resize(app: &mut App, client: &ControlClient, now: Instant) -> bool {
    if !resize_due(
        app.repaint_resize_pending,
        app.repaint_resize_settle_at,
        now,
    ) {
        return false;
    }
    app.repaint_resize_pending = false;
    app.repaint_resize_settle_at = None;
    // The host reflowed what it kept, so cells the old layout owned may
    // still hold its glyphs. One epoch for the whole burst.
    app.layout_changed();
    resize_client(app, client).await;
    true
}

/// The soonest of the loop's one-shot deadlines, or none when the loop has
/// nothing to wake for.
///
/// Named so the selection can be exercised: every entry is a one-shot that
/// its own event replaces, and answering one clears it. Nothing here may
/// be re-armed by the wake it caused, which is what keeps an idle
/// workspace genuinely idle.
fn soonest<const N: usize>(candidates: [Option<Instant>; N]) -> Option<Instant> {
    candidates.into_iter().flatten().min()
}

/// Whether a pending resize has stopped moving and may go to tmux.
///
/// Pure so the coalescing rule can be exercised against an explicit clock
/// rather than a sleep: a burst re-arms `settle_at`, and only the first
/// beat after the burst stops answers true.
fn resize_due(pending: bool, settle_at: Option<Instant>, now: Instant) -> bool {
    pending && settle_at.is_some_and(|at| now >= at)
}

/// The one owner of the host terminal surface.
///
/// Ratatui draws by diffing against the frame it believes it last wrote.
/// That belief is only true if every write reached the terminal, and the
/// workspace used to discard the `io::Result` at every draw site, so a
/// failed or partial write left the remembered baseline ahead of the
/// pixels the user could see. Nothing ever reconciled the difference, so
/// corrupted cells stayed corrupted until something else happened to
/// rewrite them. That is the whole mechanism behind garbled text that
/// persists.
///
/// This owner makes the repair edge-driven and explicit. A repaint is
/// requested at the moments frame continuity is known to break, and the
/// next frame then invalidates the baseline and writes every cell once.
/// There is no periodic clear and no redraw loop: an idle workspace still
/// writes nothing.
///
/// It deliberately owns nothing else. It does not know about panes,
/// messages, vendors, or the daemon, and a repaint changes no application
/// state.
struct RenderOwner<B: Backend> {
    terminal: Terminal<B>,
    /// The next frame must write every cell rather than a diff. Set here
    /// on a failed write, and requested from anywhere else through
    /// `App::repaint_requested`, so there is one way to ask and one place
    /// that decides.
    repaint_pending: bool,
}

impl<B: Backend> RenderOwner<B> {
    /// Starts pending: the first frame of a session has no baseline worth
    /// diffing against.
    fn new(terminal: Terminal<B>) -> Self {
        Self {
            terminal,
            repaint_pending: true,
        }
    }

    /// Draw one frame, repainting in full when an epoch is pending.
    ///
    /// Every failure takes the same exit, and that is the point of the
    /// split below: invalidating the baseline can fail exactly like
    /// writing the frame can, and an early return from the clear would
    /// leave a hit map describing a frame that was never delivered. So
    /// `paint` is allowed to fail anywhere and `frame` owns what a failure
    /// means: drop the geometry, because a click resolved against it would
    /// answer for pixels nobody saw, and re-arm the epoch, because the
    /// surface is now in a state this renderer cannot describe.
    fn frame(&mut self, app: &mut App, motion: &mut Motion, now: Instant) -> Result<(), B::Error> {
        if std::mem::take(&mut app.repaint_requested) {
            self.repaint_pending = true;
        }
        match self.paint(app, motion, now) {
            Ok(()) => Ok(()),
            Err(error) => {
                app.hit_map.clear();
                self.repaint_pending = true;
                Err(error)
            }
        }
    }

    /// The two writes one frame makes, in order. The epoch clears only
    /// once it has actually been delivered, so an invalidation that failed
    /// is still pending for the next frame.
    ///
    /// Invalidating through `resize` rather than the obvious
    /// `Terminal::clear` is the load-bearing detail here, and it is a
    /// measured one. Ratatui answers `clear` by first calling
    /// `Backend::get_cursor_position`, which the crossterm backend serves
    /// by writing `ESC[6n` and then BLOCKING the caller on stdin until the
    /// terminal answers. This workspace already owns stdin in its own
    /// event reader, and both sides take crossterm's one internal reader.
    /// The reader is always polling, so it consumes the reply as an event
    /// and the query waits out its full timeout, and since a failed frame
    /// re-arms the epoch the next frame asks again: 2s per attempt, with
    /// the pane in the alternate screen showing nothing. MEASURED on tmux
    /// 3.4, 3.6a and next-3.8 alike (F75): the terminal-restoration e2e
    /// burned its whole 15s budget on every one of them.
    ///
    /// A repaint therefore never reads from the terminal. Resizing to the
    /// size the terminal already has reaches both effects a repaint needs
    /// through writes alone: `clear_viewport` clears the host surface and
    /// resets the diff baseline, so the frame below writes every cell.
    /// `size` is an ioctl, not a query on the wire.
    fn paint(&mut self, app: &mut App, motion: &mut Motion, now: Instant) -> Result<(), B::Error> {
        if self.repaint_pending {
            let area = self.terminal.size()?.into();
            self.terminal.resize(area)?;
            self.repaint_pending = false;
        }
        draw(&mut self.terminal, app, motion, now)
    }
}

fn messages_rail_cue(app: &App) -> Option<MessagesRailCue> {
    app.messages_snapshot_counts.map(|counts| MessagesRailCue {
        work_messages: counts.work_messages,
        attention_entries: counts.open_attention_entries,
        current: app.messages_gate.may_mutate(),
    })
}

fn draw<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    motion: &mut Motion,
    now: Instant,
) -> Result<(), B::Error> {
    // The preference is read here rather than pushed from the exec arm
    // because the clock is a local of the loop, not a field of `App`. One
    // read per frame is cheap, it cannot desynchronise from what the menu
    // just wrote, and it also covers a `config.toml` edited under a
    // running workspace.
    // Repaint the host terminal's defaults when the theme's ink or ground
    // changes. The ground fills the window padding around the grid; the ink
    // keeps unstyled host text readable against it.
    // Here rather than at each theme site because the paint changes through
    // three of them: boot, the ThemeWatch reload, and the picker's live
    // preview. One comparison catches all three and emits nothing on the
    // frames between.
    // Only while focus is here: a frame drawn for output arriving in an
    // unfocused tab must not restyle the terminal the operator is using
    // for something else (`AppMsg::Focus` hands both defaults back on leave
    // and clears `window_palette` so return reapplies it).
    let palette = app
        .paint
        .host_palette_rgb()
        .map_or(HostPaletteState::Defaults, HostPaletteState::Theme);
    if app.window_focused && palette != app.window_palette {
        match palette {
            HostPaletteState::Theme(palette) => {
                crate::term_guard::apply_window_palette(palette.fg, palette.bg);
            }
            HostPaletteState::Defaults => crate::term_guard::yield_window_palette(),
            HostPaletteState::Unknown => unreachable!("desired palette is always known"),
        }
        app.window_palette = palette;
    }
    motion.set_preference(app.prefs.motion, motion_capable(&app.paint));
    motion.observe(observed(app), now);
    app.hit_map.clear();
    // Before the frame, not during it: the Messages pane's session filter
    // follows the live pane table, and the queue takes it by value.
    sync_messages_session_filter(app);
    let mut shown_cursor: Option<crate::render::HostCursor> = None;
    // The whole call, write and flush included: what the slow-terminal
    // latch measures is the cost of putting a frame on the wire, not the
    // composition the 570us guard already bounds.
    let started = std::time::Instant::now();
    terminal
        .draw(|f| {
            let areas = app.chrome(f.area());
            if let Some(sidebar) = areas.sidebar {
                paint_sidebar(
                    &app.model.workspaces,
                    app.model.active_workspace,
                    &app.model.active_tab().active_pane,
                    &app.expanded_workspaces,
                    &app.prefs.agent_order,
                    app.sidebar_tab,
                    &app.record,
                    match app.files_view {
                        crate::files::FilesView::Agent => &mut app.files,
                        crate::files::FilesView::Pinned => &mut app.files_pinned,
                    },
                    app.files_view,
                    app.prefs.files_rows,
                    sidebar,
                    f.buffer_mut(),
                    &app.paint,
                    &mut app.hit_map,
                    &app.decoration,
                    app.hover,
                    app.drag.as_ref(),
                );
                if app.decoration.online {
                    if let Some(marker) = app.daemon_compatibility_marker() {
                        crate::render::paint_daemon_status(
                            sidebar,
                            app.sidebar_tab,
                            marker,
                            f.buffer_mut(),
                            &app.paint,
                        );
                    }
                }
            }
            if let Some(rail) = areas.rail {
                let daemon_warning = app.daemon_compatibility_marker().is_some();
                paint_sidebar_rail(
                    rail,
                    f.buffer_mut(),
                    &app.paint,
                    &mut app.hit_map,
                    daemon_warning,
                    app.hover,
                );
            }
            if let Some(messages) = areas.messages {
                let pane_manifests: std::collections::HashMap<String, String> = app
                    .decoration
                    .panes
                    .iter()
                    .filter_map(|(id, p)| p.manifest.as_ref().map(|m| (id.clone(), m.clone())))
                    .collect();
                let refresh_status = app
                    .messages_refresh_error
                    .as_ref()
                    .map(|error| format!("refresh failed: {error} · Ctrl+R to retry"));
                let visible_notice = app
                    .notice
                    .text()
                    .or(app.daemon_compatibility_notice.as_deref());
                let link_status = refresh_status.as_deref().or_else(|| {
                    (app.messages_gate.link() == cyclops_ui::Link::Lost)
                        .then_some("daemon reconnecting")
                        .or(visible_notice)
                });
                paint_messages(
                    &app.messages_queue,
                    app.messages_detail.as_ref(),
                    Some(&app.messages_composer),
                    &app.avatar_registry,
                    Some(&app.decoration.mailbox_routes),
                    Some(&pane_manifests),
                    link_status,
                    app.messages_refresh_error.is_some(),
                    app.messages_focused,
                    messages,
                    f.buffer_mut(),
                    &app.paint,
                    &mut app.hit_map,
                    app.hover,
                );
            }
            if let Some(messages_rail) = areas.messages_rail {
                let cue = messages_rail_cue(app);
                paint_messages_rail(
                    messages_rail,
                    f.buffer_mut(),
                    &app.paint,
                    &mut app.hit_map,
                    cue,
                    app.hover,
                );
            }
            paint_tab_bar(
                &app.model.session.tabs,
                app.model.session.active_tab,
                areas.tab_bar,
                f.buffer_mut(),
                &app.paint,
                &mut app.hit_map,
                &app.decoration,
                app.hover,
            );
            let tab = app.model.active_tab();
            let visible_notice = app
                .notice
                .text()
                .or(app.daemon_compatibility_notice.as_deref());
            let mut ctx = crate::render::WindowPaintCtx {
                link: app.link_state,
                paused: &app.paused_panes,
                hits: &mut app.hit_map,
                decoration: &app.decoration,
                selection: app.selection.active_pane(),
                drag: app.drag.as_ref(),
                notice: visible_notice,
                minimized: &app.minimized,
                cursor: None,
                // Where every fade this frame stands. `none()` while motion
                // is off, and a fade that has finished reads as its
                // endpoint, so the painters below need no motion branch of
                // their own.
                motion: crate::animate::MotionFrame::new(motion, now),
            };
            paint_window(
                tab,
                &app.runtimes,
                areas.canvas,
                f.buffer_mut(),
                &app.paint,
                &mut ctx,
            );
            let cursor = ctx.cursor;
            // The pane canvas's own left border is the resize handle now
            // (`render::sidebar`'s `SIDEBAR_GRAB_WIDTH` doc has the
            // history). Pushed here, after `paint_window`'s own
            // `PaneFrame` hits, this wins the column from the pane frame
            // beneath it, so grabbing the border the operator can actually
            // see resizes the sidebar instead of focusing whatever pane
            // sits behind it. Known and accepted: the leftmost cell of any
            // pane frame or divider that lands on this column answers as
            // the sidebar's resize handle instead of pane focus/resize for
            // its own row — one column, traded on purpose so there is a
            // visible line to grab instead of a hidden one to hunt for.
            if areas.sidebar.is_some() && areas.canvas.width > 0 && areas.canvas.height > 0 {
                let divider = Rect::new(areas.canvas.x, areas.canvas.y, 1, areas.canvas.height);
                app.hit_map.push(divider, HitTarget::SidebarDivider);
                paint_sidebar_resize_feedback(
                    f.buffer_mut(),
                    divider,
                    &app.paint,
                    app.hover,
                    app.drag.as_ref(),
                );
            }
            if let Some(messages) = areas.messages {
                if areas.canvas.width > 0 && areas.canvas.height > 0 {
                    let divider = Rect::new(messages.x, messages.y, 1, messages.height);
                    paint_messages_resize_feedback(
                        f.buffer_mut(),
                        divider,
                        &app.paint,
                        app.hover,
                        app.drag.as_ref(),
                    );
                }
            }
            // Menus paint after panes so their hit regions shadow them.
            paint_menu(
                &app.menu,
                f.area(),
                f.buffer_mut(),
                &app.paint,
                &mut app.hit_map,
                app.hover,
                crate::render::MenuChecks {
                    messages: app.model.messages_visible,
                },
            );
            if let Some(dialog) = &app.dialog {
                paint_dialog(
                    dialog,
                    f.area(),
                    f.buffer_mut(),
                    &app.paint,
                    &mut app.hit_map,
                    app.hover,
                    app.dialog_offset,
                );
            } else if !app.menu.is_open() {
                if let Some(hc) = cursor {
                    f.set_cursor_position((hc.x, hc.y));
                    shown_cursor = Some(hc);
                }
            }
        })
        .map(|_| ())?;
    // DECSCUSR is terminal state, not frame content: Ratatui diffs cells
    // and knows nothing of cursor shape, so the pane's requested shape is
    // emitted here, once per change rather than once per frame. While the
    // cursor is hidden its shape cannot show, so nothing is emitted and
    // the last emission stands until a visible cursor differs.
    if let Some(hc) = shown_cursor {
        let style = (hc.shape, hc.blink);
        if app.cursor_style != Some(style) {
            crate::term_guard::apply_cursor_style(hc.shape, hc.blink);
            app.cursor_style = Some(style);
        }
    }
    // A terminal that writes frames slower than this app draws them is
    // spending the operator's responsiveness on decoration. The latch is
    // one way, so this line is the only explanation it will ever give.
    if motion.note_frame(started.elapsed()) {
        log_err(
            &app.home,
            &"motion: off, this terminal writes frames slower than it draws them",
        );
    }
    Ok(())
}

/// Sync entry: run the async workspace loop.
pub fn run() -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("can't start workspace runtime: {e}");
            return 1;
        }
    };
    rt.block_on(run_async())
}

/// Non-tty entry: print help hint and exit 0.
pub fn print_help_and_exit() -> i32 {
    println!("{}", copy::HELP_HINT);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    trait FixtureParse: Sized {
        type Error;

        fn parse(value: &str) -> Result<Self, Self::Error>;
    }

    impl FixtureParse for cyclops_proto::RecipientKey {
        type Error = cyclops_proto::IdentityError;

        fn parse(value: &str) -> Result<Self, Self::Error> {
            value.parse()
        }
    }

    impl FixtureParse for cyclops_proto::MessageId {
        type Error = cyclops_proto::MailboxTypeError;

        fn parse(value: &str) -> Result<Self, Self::Error> {
            cyclops_proto::MessageId::new(value)
        }
    }

    #[test]
    fn workspace_log_refuses_a_link_without_touching_its_target() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let home = cyclops_proto::scratch::scratch_dir("workspace-log-link-home");
        let external = cyclops_proto::scratch::scratch_dir("workspace-log-link-external");
        for path in [&home, &external] {
            let _ = std::fs::remove_dir_all(path);
            std::fs::create_dir_all(path).unwrap();
        }
        let target = external.join("workspace.log");
        std::fs::write(&target, b"external\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, home.join("workspace.log")).unwrap();

        log_err(&home, &"new error");

        assert_eq!(std::fs::read(&target).unwrap(), b"external\n");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&external);
    }

    /// A minimal `App` for tests that only need model/prefs/drag state, not
    /// a live pane runtime — mirrors `exec::tests::test_app`, which is
    /// private to that module and so cannot be reused directly here.
    /// Bare arrows keep reaching the focused pane, and only stop when the
    /// operator has explicitly handed the keyboard to the file panel.
    ///
    /// This is the promise the whole design of the focus mode exists to
    /// keep. Every shell and every agent CLI in every pane uses bare
    /// arrows for history and menus, and `docs/guides/workspace-ui.md`
    /// states that unbound keys pass through. Taking them globally would
    /// break all of it, invisibly, in a way no other test here would
    /// notice.
    ///
    /// Written against the router rather than through tmux, because what
    /// is being pinned is the routing decision: with no cursor, an arrow
    /// must come back as `PassThrough` and reach `send_keys`.
    #[test]
    fn bare_arrows_reach_the_pane_until_the_file_panel_is_given_the_keyboard() {
        use crossterm::event::KeyCode;

        let mut router = Router::new(crate::bindings::default_bindings());
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());

        assert!(
            matches!(router.route(up), RouterResult::PassThrough(_)),
            "a bare arrow is nobody's binding, so it belongs to the pane"
        );
        // And it still encodes as a real arrow for tmux to deliver.
        assert_eq!(crate::input::encode_send_keys(&up), vec!["Up".to_string()]);

        // The gate in `handle_key` is a cursor test and nothing else, so
        // the panel can only take keys after `take_cursor`.
        let mut tree = crate::files::FileTree::new();
        assert_eq!(
            tree.cursor(),
            None,
            "a fresh panel does not hold the keyboard"
        );
        tree.take_cursor();
        assert_eq!(
            tree.cursor(),
            None,
            "and an empty panel still does not: there is no row to sit on"
        );

        // The prefix itself is never swallowed by the panel, or a mode
        // becomes a trap with no way to detach or switch tabs.
        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(is_prefix_key(&ctrl_b));
        assert!(!is_prefix_key(&up));
    }

    use ratatui::backend::TestBackend;

    /// The repair the whole slice exists for: a failed write must not
    /// leave the renderer believing it painted the frame the user is
    /// looking at, and it must not leave hit geometry describing pixels
    /// nobody saw. A backend that fails once, then succeeds, proves both
    /// halves in one pass.
    #[derive(Debug)]
    struct FlakyBackend {
        inner: TestBackend,
        fail_next_flush: bool,
        /// Fails the full-surface invalidation at `clear_region`, before
        /// Ratatui resets its remembered buffer.
        fail_next_clear: bool,
        /// Cells handed to the backend across every frame. Ratatui writes
        /// only what differs from its remembered buffer, so this is the
        /// observable that separates a repaint from a diff: an epoch
        /// rewrites the surface, an unchanged frame writes nothing.
        cells_written: usize,
        /// How many times this backend was asked where the cursor is. On
        /// the real crossterm backend that question is a blocking read on
        /// stdin, which this workspace cannot answer while its own event
        /// reader owns stdin, so the contract is that it is never asked.
        cursor_queries: usize,
    }

    /// `TestBackend` cannot fail, so a wrapper is the only way to exercise
    /// the path that matters. Delegation is mechanical; the one behaviour
    /// under test is the single failing flush.
    impl Backend for FlakyBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            let mut counted = 0usize;
            let content = content.inspect(|_| counted += 1);
            let result = self.inner.draw(content).map_err(|e| match e {});
            self.cells_written += counted;
            result
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            self.inner.hide_cursor().map_err(|e| match e {})
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.inner.show_cursor().map_err(|e| match e {})
        }
        fn get_cursor_position(&mut self) -> io::Result<ratatui::layout::Position> {
            self.cursor_queries += 1;
            self.inner.get_cursor_position().map_err(|e| match e {})
        }
        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> io::Result<()> {
            self.inner
                .set_cursor_position(position)
                .map_err(|e| match e {})
        }
        fn clear(&mut self) -> io::Result<()> {
            self.inner.clear().map_err(|e| match e {})
        }
        fn clear_region(&mut self, region: ratatui::backend::ClearType) -> io::Result<()> {
            if std::mem::take(&mut self.fail_next_clear) {
                return Err(io::Error::other("host clear failed"));
            }
            self.inner.clear_region(region).map_err(|e| match e {})
        }
        fn size(&self) -> io::Result<ratatui::layout::Size> {
            self.inner.size().map_err(|e| match e {})
        }
        fn window_size(&mut self) -> io::Result<ratatui::backend::WindowSize> {
            self.inner.window_size().map_err(|e| match e {})
        }
        fn flush(&mut self) -> io::Result<()> {
            if std::mem::take(&mut self.fail_next_flush) {
                return Err(io::Error::other("host write failed"));
            }
            self.inner.flush().map_err(|e| match e {})
        }
    }

    #[test]
    fn a_failed_frame_rearms_a_full_repaint_and_drops_hit_geometry() {
        let backend = FlakyBackend {
            inner: TestBackend::new(80, 24),
            fail_next_flush: true,
            fail_next_clear: false,
            cells_written: 0,
            cursor_queries: 0,
        };
        let mut renderer = RenderOwner::new(Terminal::new(backend).expect("terminal"));
        let mut app = test_app(
            one_pane_model(),
            cyclops_proto::scratch::scratch_dir("render-owner-failed-frame"),
        );
        let mut motion = Motion::new(false);

        // The boot frame is pending by construction and this one fails.
        assert!(renderer.repaint_pending, "a fresh surface has no baseline");
        let first = renderer.frame(&mut app, &mut motion, Instant::now());
        assert!(first.is_err(), "the backend was told to fail this write");
        assert!(
            renderer.repaint_pending,
            "a frame that did not reach the terminal must re-arm a full repaint"
        );
        assert!(
            app.hit_map.hit(0, 0).is_none(),
            "hit geometry outlived the frame it described"
        );

        // The next frame succeeds and clears the epoch.
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("second frame writes");
        assert!(
            !renderer.repaint_pending,
            "a delivered frame leaves nothing pending"
        );
    }

    /// The other half of the same contract: invalidating the baseline can
    /// fail exactly like writing the frame can, and it must take the same
    /// exit. An early return from the clear would leave a hit map
    /// describing a frame that was never delivered and would clear the
    /// epoch that had not actually happened.
    #[test]
    fn a_failed_clear_takes_the_same_exit_as_a_failed_write() {
        let mut renderer = RenderOwner::new(
            Terminal::new(FlakyBackend {
                inner: ratatui::backend::TestBackend::new(60, 20),
                fail_next_flush: false,
                fail_next_clear: true,
                cells_written: 0,
                cursor_queries: 0,
            })
            .expect("terminal"),
        );
        let home = cyclops_proto::scratch::scratch_dir("render-owner-failed-clear");
        let mut app = test_app(one_pane_model(), home.clone());
        let mut motion = Motion::new(false);

        assert!(renderer.repaint_pending, "boot has no baseline");
        // Seed geometry that a click could resolve against, so the
        // assertion below is about the drop rather than about an empty
        // map that was never populated.
        app.hit_map.push(
            Rect::new(0, 0, 4, 1),
            HitTarget::PaneBody {
                pane_id: "%0".to_string(),
            },
        );
        assert!(
            app.hit_map.hit(0, 0).is_some(),
            "the seeded hit target must exist before the failure"
        );
        let first = renderer.frame(&mut app, &mut motion, Instant::now());
        assert!(
            first.is_err(),
            "the backend was told to fail inside the invalidation"
        );
        assert!(
            renderer.repaint_pending,
            "a clear that failed left the epoch spent"
        );
        assert!(
            app.hit_map.hit(0, 0).is_none(),
            "hit geometry outlived a frame that was never delivered"
        );

        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("the next frame clears and draws");
        assert!(!renderer.repaint_pending);
        assert!(
            renderer.terminal.backend().cells_written > 0,
            "the recovered frame wrote nothing"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// A repaint may not ask the terminal a question.
    ///
    /// `Terminal::clear` is the call this renderer is supposed to want,
    /// and it is the one it must not make: ratatui serves it by first
    /// calling `Backend::get_cursor_position`, which crossterm answers by
    /// writing `ESC[6n` and blocking on stdin for the reply. The workspace
    /// runs its own always-polling reader on stdin, so the reply is
    /// consumed as an event and the query waits out its timeout instead of
    /// repairing anything. MEASURED on tmux 3.4, 3.6a and next-3.8 alike
    /// (F75): the terminal-restoration e2e burned its whole 15s budget on
    /// every one of them, and this test is the cheap guard that does not
    /// need a tmux server to catch it.
    ///
    /// This pins the property rather than the spelling. Any future
    /// invalidation is free to change how it clears, and is not free to
    /// start reading from the terminal to do it.
    #[test]
    fn a_repaint_never_asks_the_terminal_where_the_cursor_is() {
        let mut renderer = RenderOwner::new(
            Terminal::new(FlakyBackend {
                inner: ratatui::backend::TestBackend::new(60, 20),
                fail_next_flush: false,
                fail_next_clear: false,
                cells_written: 0,
                cursor_queries: 0,
            })
            .expect("terminal"),
        );
        let home = cyclops_proto::scratch::scratch_dir("render-owner-no-cursor-query");
        let mut app = test_app(one_pane_model(), home.clone());
        let mut motion = Motion::new(false);

        // The boot epoch: the one that hung, and the one every session
        // pays before the user sees anything at all.
        assert!(renderer.repaint_pending, "boot has no baseline");
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("the boot frame paints");
        assert_eq!(
            renderer.terminal.backend().cursor_queries,
            0,
            "the boot repaint queried the terminal for the cursor position"
        );

        // And every later epoch: a resize settling, a reconnect, a focus
        // regain, `Ctrl+B r`. They all arrive through this one flag.
        app.repaint_requested = true;
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("the requested repaint paints");
        assert!(
            !renderer.repaint_pending,
            "the requested epoch was not spent"
        );
        assert_eq!(
            renderer.terminal.backend().cursor_queries,
            0,
            "a requested repaint queried the terminal for the cursor position"
        );

        // A plain diff frame has no excuse either.
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("the ordinary frame paints");
        assert_eq!(
            renderer.terminal.backend().cursor_queries,
            0,
            "an ordinary frame queried the terminal for the cursor position"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// Anything may ask for a repaint through `App`, and the renderer
    /// drains that request exactly once.
    #[test]
    fn a_requested_repaint_is_consumed_by_the_next_frame() {
        let mut renderer = RenderOwner::new(
            Terminal::new(FlakyBackend {
                inner: TestBackend::new(40, 12),
                fail_next_flush: false,
                fail_next_clear: false,
                cells_written: 0,
                cursor_queries: 0,
            })
            .expect("terminal"),
        );
        let mut app = test_app(
            one_pane_model(),
            cyclops_proto::scratch::scratch_dir("render-owner-requested"),
        );
        let mut motion = Motion::new(false);
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("boot frame");

        app.repaint_requested = true;
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("repaint frame");
        assert!(
            !app.repaint_requested,
            "the request must be drained, not left to repaint every frame"
        );
        assert!(!renderer.repaint_pending);
        let after_repaint = renderer.terminal.backend().cells_written;
        assert!(
            after_repaint > 0,
            "the repaint frame wrote nothing to the surface"
        );

        // Nothing changed, so an ordinary frame is a diff and writes
        // nothing. That is what makes the epoch meaningful: without one,
        // a surface the host corrupted would never be rewritten.
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("diff frame");
        assert_eq!(
            renderer.terminal.backend().cells_written,
            after_repaint,
            "an unchanged frame must write no cells"
        );

        // And asking again rewrites the surface.
        app.repaint_requested = true;
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("second repaint");
        assert!(
            renderer.terminal.backend().cells_written > after_repaint,
            "a requested repaint must rewrite the surface"
        );
    }

    /// The whole resize contract, driven through the real event handler
    /// and the real settle seam rather than by mutating fields: a drag
    /// costs zero tmux calls and zero full repaints while it is moving,
    /// then exactly one of each at the size it ended on.
    ///
    /// A restore that fails keeps the session too.
    ///
    /// The third door into the same forbidden state. The unreadable record
    /// is handled, and the window this workspace never pinned is handled,
    /// but a restore that simply errors, a dropped link, a timed-out
    /// command, used to be logged and stepped over, and the mark was
    /// released anyway. What that leaves is a window still on `manual`
    /// with its record still attached and nobody named as its owner, which
    /// is the same orphaning reached through a transient failure.
    #[tokio::test]
    async fn a_failed_restore_keeps_the_session_owned() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-failed-restore");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "120",
            "-y",
            "40",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-failed-restore-home");

        let mut sizing = WindowSizing::default();
        let tabs = one_pane_model().session.tabs;
        adopt_windows(&mut sizing, &client, "s", &tabs, &home).await;
        assert!(sizing.owns("s"));
        let marker = sizing.identity.as_ref().expect("identity").marker();

        // A record that reads correctly and cannot be applied: the encoding
        // is exactly what this workspace writes, and the policy inside it is
        // one tmux will reject. The restore therefore fails at the real
        // write, on a link that is otherwise perfectly healthy, so the
        // release below is a decision rather than a second casualty of a
        // dead client.
        server.run_ok(&[
            "set-option",
            "-w",
            "-t",
            "@0",
            "@cyclops_prior_window_size",
            "explicit:not-a-policy",
        ]);

        restore_owned_sizing(&mut sizing, &client, &home).await;

        // The window really did stay pinned: this is the state the mark has
        // to keep naming an owner for.
        let policy = server.run(&["show-options", "-w", "-t", "@0", "-qv", "window-size"]);
        assert_eq!(
            String::from_utf8_lossy(&policy.stdout).trim(),
            "manual",
            "the fixture wants a window that failed to come off manual"
        );
        let mark = server.run(&["show-options", "-t", "s", "-qv", "@cyclops_window_driver"]);
        assert_eq!(
            String::from_utf8_lossy(&mark.stdout).trim(),
            marker,
            "the mark was released while a window could not be put back"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// A window already carrying an unreadable record keeps its session
    /// owned, even though this workspace never pinned it.
    ///
    /// This is the hole the first cut left, end to end. Adoption used to
    /// treat an unreadable record as an error, log it, and move on, so the
    /// window was never owned; quitting then found a session with nothing
    /// to restore and released the mark. What was left behind was `manual`
    /// plus an unreadable record plus no owner, which is the one state
    /// nothing can recover from: no policy applies, no client can resize
    /// it, and no later workspace can learn what it was.
    #[tokio::test]
    async fn a_window_with_an_unreadable_record_keeps_its_session_owned() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-pre-malformed");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "120",
            "-y",
            "40",
            "/bin/sh",
        ]);
        // A dead workspace's leftovers: pinned, with a record nobody can
        // read, and a mark naming a client that no longer exists.
        let garbage = "written-by-something-else";
        server.run_ok(&["set-option", "-w", "-t", "@0", "window-size", "manual"]);
        server.run_ok(&[
            "set-option",
            "-w",
            "-t",
            "@0",
            "@cyclops_prior_window_size",
            garbage,
        ]);
        server.run_ok(&[
            "set-option",
            "-t",
            "s",
            "@cyclops_window_driver",
            "client-999999:1700000000",
        ]);

        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-pre-malformed-home");

        let mut sizing = WindowSizing::default();
        let tabs = one_pane_model().session.tabs;
        let adopted = adopt_windows(&mut sizing, &client, "s", &tabs, &home).await;

        // It takes the session over from the dead owner, and it does not
        // pin or claim to have taken the window.
        assert!(sizing.owns("s"), "the stale session was not taken over");
        assert!(
            !adopted.took_a_window,
            "an unreadable window was reported as taken"
        );
        let owned = sizing.owned.get("s").expect("owned session");
        assert!(owned.pinned.is_empty(), "an unreadable window was pinned");
        assert_eq!(
            owned.blocked.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["@0"],
            "the unreadable window was forgotten instead of blocking release"
        );

        let marker = sizing.identity.as_ref().expect("identity").marker();
        restore_owned_sizing(&mut sizing, &client, &home).await;

        // Everything the operator needs is still exactly where it was.
        let policy = server.run(&["show-options", "-w", "-t", "@0", "-qv", "window-size"]);
        assert_eq!(
            String::from_utf8_lossy(&policy.stdout).trim(),
            "manual",
            "quitting moved the window off manual on a guess"
        );
        let record = server.run(&[
            "show-options",
            "-w",
            "-t",
            "@0",
            "-qv",
            "@cyclops_prior_window_size",
        ]);
        assert_eq!(
            String::from_utf8_lossy(&record.stdout).trim(),
            garbage,
            "quitting destroyed the only evidence of the original"
        );
        let mark = server.run(&["show-options", "-t", "s", "-qv", "@cyclops_window_driver"]);
        assert_eq!(
            String::from_utf8_lossy(&mark.stdout).trim(),
            marker,
            "quitting released a session whose window is still pinned and unreadable"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// Quitting does not release a session it could not put back.
    ///
    /// If the record of what a window was cannot be read, the original is
    /// unknowable, and the exit path must not paper over that. Releasing
    /// the mark would leave a window pinned to `manual` with no owner,
    /// which no client can resize and nothing repairs on its own. So the
    /// mark stays, the pin stays, and the record stays as evidence.
    #[tokio::test]
    async fn quitting_keeps_a_session_whose_record_it_cannot_read() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-malformed-exit");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "120",
            "-y",
            "40",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-malformed-exit-home");

        let mut sizing = WindowSizing::default();
        let tabs = one_pane_model().session.tabs;
        adopt_windows(&mut sizing, &client, "s", &tabs, &home).await;
        assert!(sizing.owns("s"));
        let marker = sizing.identity.as_ref().expect("identity").marker();

        let garbage = "written-by-something-else";
        server.run_ok(&[
            "set-option",
            "-w",
            "-t",
            "@0",
            "@cyclops_prior_window_size",
            garbage,
        ]);

        restore_owned_sizing(&mut sizing, &client, &home).await;

        let policy = server.run(&["show-options", "-w", "-t", "@0", "-qv", "window-size"]);
        assert_eq!(
            String::from_utf8_lossy(&policy.stdout).trim(),
            "manual",
            "quitting moved the window off manual on a guess"
        );
        let record = server.run(&[
            "show-options",
            "-w",
            "-t",
            "@0",
            "-qv",
            "@cyclops_prior_window_size",
        ]);
        assert_eq!(
            String::from_utf8_lossy(&record.stdout).trim(),
            garbage,
            "quitting destroyed the only evidence of the original"
        );
        let mark = server.run(&["show-options", "-t", "s", "-qv", "@cyclops_window_driver"]);
        assert_eq!(
            String::from_utf8_lossy(&mark.stdout).trim(),
            marker,
            "quitting released a session whose window is still pinned"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// A client leaving has to reach the loop, because that is the only
    /// edge that tells a follower its owner died.
    ///
    /// Nothing else in a quiet workspace would notice: no layout changed,
    /// no window opened, no pane produced output. Without this arm a
    /// follower renders inside a dead workspace's geometry indefinitely,
    /// and the takeover that exists to fix that never runs.
    #[test]
    fn a_client_leaving_reaches_the_loop_so_a_dead_owner_can_be_replaced() {
        assert!(matches!(
            structural_message(cyclops_tmux::Notification::ClientDetached {
                client: "client-1".into()
            }),
            Some(AppMsg::Reconcile)
        ));
    }

    /// A workspace that navigated away is still alive, and a follower
    /// arriving at the session it left must not treat it as dead.
    ///
    /// This is the failure the A to B to A test could not catch, because
    /// that test had no rival in it. The owner claims A and navigates to B,
    /// which takes it out of A's client list while leaving it running and
    /// still sizing A's windows. A follower attaching to A then reads a
    /// marker naming a client it cannot see in A, and if liveness were
    /// asked of A's client list it would call that stale and steal the
    /// session, putting two writers on the same windows. Liveness is a
    /// question about the server, so it is asked of the server.
    #[tokio::test]
    async fn a_follower_cannot_steal_from_an_owner_that_navigated_away() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-navigated-owner");
        for name in ["alpha", "beta"] {
            server.run_ok(&[
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                "120",
                "-y",
                "40",
                "/bin/sh",
            ]);
        }
        let attach = |session: &str| {
            ControlConfig::attach(session)
                .on_socket(server.socket().to_string())
                .with_config_file("/dev/null")
        };
        let home = cyclops_proto::scratch::scratch_dir("workspace-navigated-owner-home");

        // The owner claims alpha, then navigates to beta and stays alive.
        let (owner, _rx) = ControlClient::spawn(attach("alpha")).await.expect("owner");
        let mut owner_sizing = WindowSizing::default();
        assert!(owns_session(&mut owner_sizing, &owner, "alpha", &home).await);
        let owner_marker = owner_sizing.identity.as_ref().expect("identity").marker();
        owner
            .command("switch-client -t 'beta'")
            .await
            .expect("navigate");

        // The hazard, stated as a fact rather than assumed: alpha's own
        // client list no longer names the owner, while the server does.
        assert!(
            !owner
                .session_client_markers("alpha")
                .await
                .expect("alpha viewers")
                .contains(&owner_marker),
            "the fixture wants the owner displaying beta"
        );
        assert!(
            owner
                .server_client_markers()
                .await
                .expect("server clients")
                .contains(&owner_marker),
            "the owner must still be alive on the server"
        );

        // A second workspace arrives at alpha and asks whether it owns it.
        let (rival, _rx2) = ControlClient::spawn(attach("alpha")).await.expect("rival");
        let mut rival_sizing = WindowSizing::default();
        let took = owns_session(&mut rival_sizing, &rival, "alpha", &home).await;
        assert!(
            !took,
            "a follower stole a session from an owner that had merely navigated away"
        );
        assert!(!rival_sizing.owns("alpha"));
        assert_eq!(
            rival.window_driver("alpha").await.expect("readback"),
            Some(owner_marker),
            "alpha's owner was replaced by a workspace that only looked at it"
        );
        assert!(
            rival_sizing.following.contains("alpha"),
            "the follower did not record that it follows alpha"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// A tab opened later is pinned AND reported, because pinning alone
    /// leaves it holding whatever size it had.
    ///
    /// The canvas does not change when a tab opens, and `resize_client`
    /// skips an unchanged canvas, so without the report a new tab would be
    /// taken off every sizing policy and then never told what size to be.
    /// The same pass must also stay quiet about windows it already owns,
    /// since re-pinning every reconcile would be a write per snapshot.
    #[tokio::test]
    async fn adopting_reports_only_windows_it_actually_took() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-adopt-report");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "120",
            "-y",
            "40",
            "/bin/sh",
        ]);
        // `s:` and not `s`: without the colon tmux reads the target as a
        // WINDOW, resolves it to the session's current window, and tries to
        // create at that index, which fails with "index 0 in use" depending
        // on base-index and what is already there. The colon names the
        // session and appends at the next free index. MEASURED: the bare
        // form failed on CI's ubuntu and tmux-head runners while passing
        // locally, which is what an ambiguous target looks like.
        server.run_ok(&["new-window", "-t", "s:", "/bin/sh"]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-adopt-report-home");

        let mut sizing = WindowSizing::default();
        let boot_tabs = one_pane_model().session.tabs;
        let first = adopt_windows(&mut sizing, &client, "s", &boot_tabs, &home).await;
        assert_eq!(
            first,
            Adopted {
                newly_following: false,
                took_a_window: true,
                authority_transferred: false,
            },
            "the boot window was not reported as taken"
        );

        let again = adopt_windows(&mut sizing, &client, "s", &boot_tabs, &home).await;
        assert_eq!(
            again,
            Adopted::default(),
            "a window already owned was taken a second time"
        );

        // A tab opens. The canvas has not moved, so this report is the only
        // thing that will cause the new window to be sized at all.
        let mut two_tabs = boot_tabs.clone();
        let mut second = two_tabs[0].clone();
        second.window_id = "@1".into();
        two_tabs.push(second);
        let opened = adopt_windows(&mut sizing, &client, "s", &two_tabs, &home).await;
        assert!(
            opened.took_a_window,
            "a tab opened later was pinned without being reported"
        );
        assert_eq!(
            sizing.owned.get("s").map(|owned| owned.pinned.len()),
            Some(2),
            "the new tab was not recorded as owned"
        );

        // And a tab that closes stops being owned, so the exit path does
        // not ask tmux about a window that is gone.
        let closed = adopt_windows(&mut sizing, &client, "s", &boot_tabs, &home).await;
        assert_eq!(closed, Adopted::default());
        assert_eq!(
            sizing.owned.get("s").map(|owned| owned.pinned.len()),
            Some(1),
            "a closed window stayed owned"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// A reconnect is a new tmux client, so a workspace that owned sessions
    /// under its old identity has to move them onto the new one or leave
    /// them pinned with nobody owning them.
    ///
    /// The sessions this process is not currently displaying are the ones
    /// that matter: nothing else in the loop revisits them, so a seam that
    /// only re-elected the active session would strand the rest at `manual`
    /// past this workspace's own exit.
    #[tokio::test]
    async fn a_reconnect_moves_every_owned_session_onto_the_new_identity() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-rekey");
        for name in ["shown", "background"] {
            server.run_ok(&[
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                "120",
                "-y",
                "40",
                "/bin/sh",
            ]);
        }
        let cfg = ControlConfig::attach("shown")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let home = cyclops_proto::scratch::scratch_dir("workspace-rekey-home");
        let (first, _rx) = ControlClient::spawn(cfg.clone()).await.expect("attach");

        let mut sizing = WindowSizing::default();
        for session in ["shown", "background"] {
            assert!(
                owns_session(&mut sizing, &first, session, &home).await,
                "an unowned session must be claimable"
            );
            sizing
                .owned
                .entry(session.to_string())
                .or_default()
                .pinned
                .insert("@0".to_string());
        }
        let old_marker = sizing.identity.as_ref().expect("identity").marker();

        // The reconnect: the old client goes, a new one arrives, and the
        // marks in tmux still name the client that just died.
        first.shutdown().await;
        let (second, _rx2) = ControlClient::spawn(cfg).await.expect("reattach");
        rekey_ownership(&mut sizing, &second, &home).await;

        let new_marker = sizing.identity.as_ref().expect("identity").marker();
        assert_ne!(new_marker, old_marker, "a reconnect must change identity");
        for session in ["shown", "background"] {
            assert!(
                sizing.owns(session),
                "{session} was dropped by its own reconnect"
            );
            assert_eq!(
                second.window_driver(session).await.expect("readback"),
                Some(new_marker.clone()),
                "{session} still names the dead client"
            );
        }
        let _ = std::fs::remove_dir_all(home);
    }

    /// A follower that legitimately takes a session during the reconnect
    /// gap keeps it, and the reconnecting workspace neither resizes it nor
    /// puts it back on its way out.
    ///
    /// The gap is real: between the old client dying and the new one
    /// re-keying, the mark names a client that is genuinely gone, which is
    /// exactly the condition a follower is entitled to act on. Losing that
    /// race is not an error, and the losing workspace must behave as though
    /// it never owned the session, because the winner is now using it.
    #[tokio::test]
    async fn a_follower_that_wins_the_reconnect_gap_keeps_the_session() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-rekey-lost");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "120",
            "-y",
            "40",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let home = cyclops_proto::scratch::scratch_dir("workspace-rekey-lost-home");
        let (first, _rx) = ControlClient::spawn(cfg.clone()).await.expect("attach");

        let mut sizing = WindowSizing::default();
        assert!(owns_session(&mut sizing, &first, "s", &home).await);
        let tabs = one_pane_model().session.tabs;
        adopt_windows(&mut sizing, &first, "s", &tabs, &home).await;
        assert!(sizing.owns("s"));
        let old_marker = sizing.identity.as_ref().expect("identity").marker();

        // The link drops, and a second workspace finds the mark stale and
        // takes the session while this one is away.
        first.shutdown().await;
        let (rival, _rx2) = ControlClient::spawn(cfg.clone()).await.expect("rival");
        let rival_marker = rival.client_identity().await.expect("identity").marker();
        assert!(
            rival
                .take_over_window_driver("s", &old_marker, &rival_marker)
                .await
                .expect("takeover"),
            "a stale mark must be takeable"
        );

        // Now this workspace reconnects and tries to move its ownership.
        let (second, _rx3) = ControlClient::spawn(cfg).await.expect("reattach");
        rekey_ownership(&mut sizing, &second, &home).await;
        assert!(
            !sizing.owns("s"),
            "the session was kept after losing it to a live workspace"
        );
        assert_eq!(
            second.window_driver("s").await.expect("readback"),
            Some(rival_marker.clone()),
            "the reconnect stole the session back"
        );

        // And the exit path leaves the winner's session exactly as it is.
        restore_owned_sizing(&mut sizing, &second, &home).await;
        assert_eq!(
            second.window_driver("s").await.expect("readback"),
            Some(rival_marker),
            "quitting released a session this workspace no longer owned"
        );
        let out = server.run(&["show-options", "-w", "-t", "@0", "-qv", "window-size"]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "manual",
            "quitting unpinned a window the winning workspace is using"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// `declared_client_size` is the observable for the tmux call, because
    /// only a successful `resize_client` sets it.
    #[tokio::test]
    async fn a_resize_burst_costs_one_call_and_one_repaint_at_the_final_size() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-resize-burst");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "120",
            "-y",
            "40",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (mut client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-resize-burst-home");
        let mut app = test_app(one_pane_model(), home.clone());
        app.model.session.session = "s".to_string();
        app.term_size = (120, 40);
        app.repaint_requested = false;
        assert!(app.declared_client_size.is_none(), "nothing declared yet");

        let mut debounce = None;
        let mut reconnect_deadline = None;
        let mut detached = false;
        let mut pending_input = None;
        let mut previous_settle: Option<Instant> = None;

        for size in [(118u16, 40u16), (112, 40), (104, 40), (96, 40)] {
            handle_app_msg(
                Some(AppMsg::Resized(size.0, size.1)),
                &mut app,
                &mut client,
                &mut debounce,
                &mut reconnect_deadline,
                &mut detached,
                &mut pending_input,
            )
            .await;

            assert_eq!(app.term_size, size, "the latest size must win");
            assert!(
                !app.repaint_requested,
                "an event inside the burst asked for a full repaint"
            );
            assert!(
                app.declared_client_size.is_none(),
                "an event inside the burst sent tmux an intermediate size"
            );
            let settle = app
                .repaint_resize_settle_at
                .expect("a resize arms the settle deadline");
            if let Some(previous) = previous_settle {
                assert!(settle > previous, "the deadline must slide, not queue");
            }
            previous_settle = Some(settle);
            assert_eq!(
                soonest([debounce, app.repaint_resize_settle_at]),
                app.repaint_resize_settle_at,
                "the settle deadline must be what the loop wakes for"
            );
        }

        let settle = previous_settle.expect("armed");
        assert!(
            !apply_settled_resize(&mut app, &client, settle - Duration::from_millis(1)).await,
            "the burst was answered before it settled"
        );
        assert!(app.declared_client_size.is_none());
        assert!(!app.repaint_requested);

        assert!(
            apply_settled_resize(&mut app, &client, settle).await,
            "the settled burst was not answered"
        );
        assert!(app.repaint_requested, "the burst asked for no repaint");
        let declared = app
            .declared_client_size
            .expect("the settled burst told tmux nothing");
        assert!(
            declared.0 <= 96,
            "tmux was told a size from inside the burst: {declared:?}"
        );
        assert_eq!(
            app.repaint_resize_settle_at, None,
            "the deadline outlived it"
        );

        app.repaint_requested = false;
        assert!(
            !apply_settled_resize(&mut app, &client, settle + RESIZE_SETTLE).await,
            "a drained burst answered twice"
        );
        assert!(!app.repaint_requested);
        assert_eq!(app.declared_client_size, Some(declared));

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
    }

    fn test_app(model: WorkspaceModel, home: std::path::PathBuf) -> App {
        App {
            model,
            runtimes: RuntimeRegistry::default(),
            router: Router::new(crate::bindings::default_bindings()),
            paint: Paint::for_test(),
            dialog: None,
            dialog_offset: (0, 0),
            theme_restore: None,
            link_state: LinkState::Live,
            paused_panes: HashSet::new(),
            minimized: std::collections::HashMap::new(),
            window_palette: HostPaletteState::Unknown,
            window_focused: true,
            select_all: crate::input::SelectAll::default(),
            reconnect_attempt: 0,
            needs_forced_hydrate: false,
            hit_map: HitMap::default(),
            menu: MenuState::None,
            hover: None,
            selection: SelectionState::default(),
            drag: None,
            notice: NoticeState::default(),
            daemon_compatibility: None,
            daemon_compatibility_notice: None,
            decoration: DecorationSnapshot::default(),
            prefs: WorkspacePrefs::default(),
            expanded_workspaces: HashSet::new(),
            expanded_for: None,
            watched_sessions: HashSet::new(),
            sidebar_tab: SidebarTab::default(),
            files: crate::files::FileTree::new(),
            files_pinned: crate::files::FileTree::new(),
            files_view: crate::files::FilesView::default(),
            files_probe_at: None,
            files_root_pending: true,
            record: cyclops_ui::Record::new(),
            messages_queue: cyclops_ui::HumanQueue::default(),
            messages_snapshot_counts: None,
            messages_caller: None,
            messages_detail: None,
            messages_composer: cyclops_ui::ComposerState::default(),
            avatar_registry: cyclops_ui::AvatarRegistry::default(),
            intake: cyclops_ui::Intake::new(),
            stream_reconciling: false,
            cursor_style: None,
            term_size: (40, 12),
            declared_client_size: None,
            sizing: WindowSizing::default(),
            needs_reconcile: false,
            needs_hydrate: false,
            paste_seq: 0,
            home,
            folder_probe_at: None,
            send_requests: None,
            stream_reconcile_requests: None,
            repaint_requested: false,
            repaint_resize_pending: false,
            repaint_resize_settle_at: None,
            messages_focused: false,
            messages_session_scoped: true,
            messages_gate: cyclops_ui::RefreshGate::new(),
            messages_refresh_error: None,
            messages_send_tx: None,
            messages_composer_revision: 0,
            messages_send_in_flight: None,
            messages_snapshot_tx: None,
            message_detail_tx: None,
            message_detail_in_flight: None,
            messages_reconcile_owed: None,
        }
    }

    /// A one-pane model for tests that need an `App` but never reach
    /// tmux, so the ids are inert.
    fn one_pane_model() -> WorkspaceModel {
        let tab = crate::model::TabModel {
            window_id: "@0".into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: "%0".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%0".into(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
            minimization_provenance: std::collections::HashMap::new(),
        };
        WorkspaceModel {
            workspaces: vec![crate::model::WorkspaceRow {
                session_id: "$0".into(),
                name: "s".into(),
                tab_count: 1,
                window_ids: vec!["@0".into()],
            }],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "s".into(),
                tabs: vec![tab],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        }
    }

    fn messages_pane_model() -> WorkspaceModel {
        let tab = crate::model::TabModel {
            window_id: "@0".into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Split {
                dir: crate::layout::SplitDir::Horizontal,
                x: 0,
                y: 0,
                width: 90,
                height: 20,
                children: vec![
                    crate::layout::ResolvedLayout::Leaf {
                        pane_id: "%0".into(),
                        x: 0,
                        y: 0,
                        width: 60,
                        height: 20,
                    },
                    crate::layout::ResolvedLayout::Leaf {
                        pane_id: "%1".into(),
                        x: 61,
                        y: 0,
                        width: 29,
                        height: 20,
                    },
                ],
            },
            active_pane: "%1".into(),
            zoomed: false,
            minimized: Default::default(),
            minimization_provenance: Default::default(),
        };
        WorkspaceModel {
            workspaces: vec![crate::model::WorkspaceRow {
                session_id: "$0".into(),
                name: "s".into(),
                tab_count: 1,
                window_ids: vec!["@0".into()],
            }],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "s".into(),
                tabs: vec![tab],
                active_tab: 0,
            },
            sidebar_visible: false,
            messages_visible: false,
        }
    }

    #[test]
    fn any_pane_input_attempt_returns_the_viewport_to_its_live_tail() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-input-snaps-tail");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create scratch home");
        let mut app = test_app(one_pane_model(), home.clone());
        let mut runtime = crate::runtime::PaneRuntime::new(40, 6);
        for line in 0..40 {
            runtime.feed(format!("line {line}\r\n").as_bytes());
        }
        runtime.scroll(-8);
        assert!(!runtime.at_tail(), "fixture begins in scrollback");
        app.runtimes.insert("%0".into(), runtime);

        assert!(snap_pane_to_tail(&mut app, "%0"));
        assert!(app.runtimes.get("%0").expect("runtime").at_tail());
        assert!(
            !snap_pane_to_tail(&mut app, "%0"),
            "a second key or paste at the tail has no viewport work"
        );
        assert!(
            !snap_pane_to_tail(&mut app, "%missing"),
            "an event cannot move a runtime it does not own"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    fn messages_snapshot(
        workspace_seq: u64,
        work_messages: u64,
        attention_entries: u64,
    ) -> cyclops_proto::MessagesSnapshotResult {
        let workspace_id = "00000000-0000-0000-0000-000000000001"
            .parse()
            .expect("workspace id");
        cyclops_proto::MessagesSnapshotResult {
            workspace_id,
            caller: Some(cyclops_proto::RecipientKey::admin(workspace_id)),
            workspace_seq,
            counts: cyclops_proto::MessagesSnapshotCounts {
                visible_messages: work_messages,
                returned_messages: 0,
                inbox_messages: work_messages,
                outbound_messages: 0,
                work_messages,
                active_messages: work_messages,
                settled_messages: 0,
                pending_entries: work_messages,
                claimed_entries: 0,
                open_attention_entries: attention_entries,
            },
            rows: Vec::new(),
            mailbox_attention: Vec::new(),
        }
    }

    fn current_messages_gate(app: &mut App) {
        let snapshot = messages_snapshot(1, 0, 0);
        app.messages_gate.connected();
        app.messages_gate.mark_dirty();
        let request = app.messages_gate.begin().expect("snapshot request");
        assert!(app.messages_gate.finish_snapshot(request, &snapshot));
        app.messages_caller = snapshot.caller;
        assert!(app.messages_gate.may_mutate());
    }

    #[test]
    fn a_hidden_messages_invalidation_fetches_without_opening_the_pane() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-hidden-messages-cue");
        let mut app = test_app(one_pane_model(), home.clone());
        assert!(!app.model.messages_visible, "the fixture starts collapsed");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        app.messages_snapshot_tx = Some(tx);
        app.messages_gate.connected();

        let workspace_id = "00000000-0000-0000-0000-000000000001"
            .parse()
            .expect("workspace id");
        app.messages_gate
            .messages_changed(&cyclops_proto::MessagesChangedData {
                workspace_id,
                workspace_seq: 7,
                changed: [cyclops_proto::MessagesChangedArea::Messages]
                    .into_iter()
                    .collect(),
            });
        pump_messages_refresh(&mut app);

        let (request, bound) = rx
            .try_recv()
            .expect("the body-free invalidation must fetch an authorized snapshot");
        assert_eq!(bound, 128);
        assert!(install_messages_snapshot(
            &mut app,
            request,
            messages_snapshot(7, 3, 1),
        ));
        assert_eq!(
            app.messages_snapshot_counts,
            Some(cyclops_proto::MessagesSnapshotCounts {
                visible_messages: 3,
                returned_messages: 0,
                inbox_messages: 3,
                outbound_messages: 0,
                work_messages: 3,
                active_messages: 3,
                settled_messages: 0,
                pending_entries: 3,
                claimed_entries: 0,
                open_attention_entries: 1,
            })
        );
        assert!(
            app.messages_gate.may_mutate(),
            "the installed cue is current"
        );
        assert!(
            !app.model.messages_visible,
            "an arrival forced the Messages pane open"
        );
        let accepted_counts = app.messages_snapshot_counts;
        app.messages_gate.mark_dirty();
        assert_eq!(
            app.messages_snapshot_counts, accepted_counts,
            "uncertainty retains the last authenticated body-free counts"
        );
        assert!(
            !app.messages_gate.may_mutate(),
            "the retained cue must be labeled stale"
        );

        app.messages_gate.disconnected();
        app.messages_gate.connected();
        pump_messages_refresh(&mut app);
        let (reconnect_request, reconnect_bound) = rx
            .try_recv()
            .expect("a hidden reconnect must rebuild the authorized cue");
        assert_eq!(reconnect_bound, 128);
        assert!(install_messages_snapshot(
            &mut app,
            reconnect_request,
            messages_snapshot(8, 2, 0),
        ));
        assert_eq!(
            app.messages_snapshot_counts
                .expect("reconnected counts")
                .work_messages,
            2
        );
        assert!(app.messages_gate.may_mutate());
        assert!(
            !app.model.messages_visible,
            "reconnecting forced the Messages pane open"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// Protects the body-free cue across a real control-client detach and
    /// reattach, a fresh `App`, saved visibility restoration, and the actual
    /// `AppMsg::DaemonReconnected` dispatch. This becomes obsolete when a
    /// full-binary reattachment journey asserts the same snapshot, cue, and
    /// visibility contract without broadening its failure meaning.
    #[tokio::test]
    async fn a_hidden_workspace_rebuilds_the_collapsed_cue_after_reattach() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-messages-reattach");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let home = cyclops_proto::scratch::scratch_dir("workspace-reattached-messages-cue");

        struct ScratchOwner(std::path::PathBuf);
        impl Drop for ScratchOwner {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _ = std::fs::remove_dir_all(&home);
        let _scratch = ScratchOwner(home.clone());

        let (first_client, _first_rx) = ControlClient::spawn(cfg.clone()).await.expect("attach");
        let mut first = test_app(one_pane_model(), home.clone());
        first.prefs.messages_visible = false;
        first.prefs.messages_width = 41;
        persist::save_prefs(&home, &first.prefs).expect("save collapsed visibility");
        current_messages_gate(&mut first);
        first.messages_snapshot_counts = Some(messages_snapshot(4, 5, 1).counts);
        assert!(first.messages_gate.may_mutate());
        first_client.shutdown().await;
        drop(first);

        let saved = load_prefs(&home);
        assert_eq!(saved.messages_width, 41, "reattachment did not load prefs");
        let mut reattached_model = one_pane_model();
        apply_saved_workspace_visibility(&mut reattached_model, &saved);
        let mut reattached = test_app(reattached_model, home.clone());
        reattached.prefs = saved;
        assert!(
            !reattached.model.messages_visible,
            "the fresh workspace preserves the collapsed visibility choice"
        );
        assert!(
            reattached.messages_snapshot_counts.is_none(),
            "a fresh App cannot reuse another instance's projection"
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        reattached.messages_snapshot_tx = Some(tx);

        let (mut reattached_client, _reattached_rx) =
            ControlClient::spawn(cfg).await.expect("reattach");
        let mut debounce = None;
        let mut reconnect_deadline = None;
        let mut detached = false;
        let mut pending_input = None;
        assert!(
            handle_app_msg(
                Some(AppMsg::DaemonReconnected),
                &mut reattached,
                &mut reattached_client,
                &mut debounce,
                &mut reconnect_deadline,
                &mut detached,
                &mut pending_input,
            )
            .await,
            "the reconnect event closed the app channel"
        );
        assert!(debounce.is_some(), "the reconnect did not schedule a frame");

        let (request, bound) = rx
            .try_recv()
            .expect("reattachment must request the authenticated body-free snapshot");
        assert_eq!(bound, 128);
        assert!(install_messages_snapshot(
            &mut reattached,
            request,
            messages_snapshot(9, 2, 1),
        ));
        let counts = reattached
            .messages_snapshot_counts
            .expect("reattached collapsed cue");
        assert_eq!(counts.work_messages, 2);
        assert_eq!(counts.open_attention_entries, 1);
        assert!(reattached.messages_gate.may_mutate());
        assert_eq!(
            messages_rail_cue(&reattached),
            Some(MessagesRailCue {
                work_messages: 2,
                attention_entries: 1,
                current: true,
            })
        );
        assert!(
            !reattached.model.messages_visible,
            "reattachment forced the Messages pane open"
        );
        reattached_client.shutdown().await;
    }

    fn messages_test_caller() -> cyclops_proto::RecipientKey {
        cyclops_proto::RecipientKey::admin(
            "00000000-0000-0000-0000-000000000001"
                .parse()
                .expect("workspace id"),
        )
    }

    fn messages_attempt(
        composer_revision: u64,
        composer: &mut cyclops_ui::ComposerState,
    ) -> MessagesSendAttempt {
        composer.set_text("ship the patch");
        let client_key = composer.key_for_send(|| "stable-client-key".to_string());
        MessagesSendAttempt {
            composer_revision,
            mode: composer.mode.clone().expect("composer mode"),
            caller: messages_test_caller(),
            recipient_keys: Some(vec![
                "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1"
                    .parse()
                    .expect("recipient"),
            ]),
            subject: "Direct Message".into(),
            body: composer.text().to_string(),
            fyi: false,
            reply_to: None,
            client_key,
        }
    }

    #[test]
    fn a_messages_presentation_cutoff_keeps_only_later_sequences() {
        let row = |seq| cyclops_ui::QueueRow {
            seq,
            ..cyclops_ui::QueueRow::default()
        };
        let filtered = apply_messages_presentation_cutoff(
            cyclops_ui::Snapshot {
                watermark: 9,
                rows: vec![row(4), row(6), row(9)],
            },
            6,
        );

        assert_eq!(filtered.watermark, 9);
        assert_eq!(
            filtered.rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![9]
        );
    }

    #[tokio::test]
    async fn c_clears_the_messages_view_and_persists_its_watermark() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let home = cyclops_proto::scratch::scratch_dir("workspace-messages-clear-view");
        let _ = std::fs::remove_dir_all(&home);
        let mut app = test_app(one_pane_model(), home.clone());
        app.model.messages_visible = true;
        app.messages_focused = true;
        app.messages_queue.replace(cyclops_ui::Snapshot {
            watermark: 17,
            rows: vec![cyclops_ui::QueueRow {
                seq: 11,
                ..cyclops_ui::QueueRow::default()
            }],
        });

        let outcome = handle_messages_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        )
        .await
        .expect("key handling");

        assert!(matches!(outcome, Some(InputOutcome::Redraw)));
        assert_eq!(app.messages_queue.watermark(), 17);
        assert_eq!(app.messages_queue.counts().total, 0);
        assert_eq!(app.prefs.messages_cleared_through_seq, 17);
        assert_eq!(persist::load_prefs(&home).messages_cleared_through_seq, 17);
        let _ = std::fs::remove_dir_all(home);
    }

    fn direct_composer() -> cyclops_ui::ComposerState {
        let recipient = cyclops_proto::RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .expect("recipient");
        let mut composer = cyclops_ui::ComposerState::new_direct(recipient, "claudex".into());
        composer.bind_sender(Some(messages_test_caller()));
        composer
    }

    #[test]
    fn stale_send_completion_cannot_clear_a_newer_draft() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-messages-stale-send");
        let mut app = test_app(one_pane_model(), home.clone());
        current_messages_gate(&mut app);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        app.messages_send_tx = Some(tx);
        app.messages_composer = direct_composer();
        let attempt = messages_attempt(app.messages_composer_revision, &mut app.messages_composer);

        queue_messages_send(&mut app, attempt.clone());
        assert_eq!(
            rx.try_recv().expect("queued request").attempt,
            attempt,
            "the worker receives the exact bytes and client key"
        );
        assert!(app.messages_composer.push_char('!'));
        messages_composer_changed(&mut app);
        app.messages_composer.backspace();
        messages_composer_changed(&mut app);
        finish_messages_send(
            &mut app,
            attempt,
            crate::daemon::SendOutcome::Accepted("accepted m-old".into()),
        );

        assert_eq!(app.messages_composer.text(), "ship the patch");
        assert!(app.messages_composer.mode.is_some());
        assert!(app.messages_send_in_flight.is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn stale_reconcile_cannot_unlock_a_newer_uncertain_draft() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-messages-stale-reconcile");
        let mut app = test_app(one_pane_model(), home.clone());
        current_messages_gate(&mut app);
        app.messages_composer = direct_composer();
        let _ = messages_attempt(app.messages_composer_revision, &mut app.messages_composer);
        app.messages_composer
            .record_uncertain("old send outcome unknown".into());
        app.messages_reconcile_owed = MessagesDraftIdentity::current(
            &app.messages_composer,
            app.messages_composer_revision,
            app.messages_caller,
        );

        let mut newer = direct_composer();
        newer.set_text("new exact bytes");
        let _ = newer.key_for_send(|| "new-client-key".to_string());
        newer.record_uncertain("new send outcome unknown".into());
        app.messages_composer = newer;
        finish_messages_reconcile(&mut app);

        assert!(matches!(
            app.messages_composer.stage,
            Some(cyclops_ui::Stage::Uncertain { .. })
        ));
        assert!(app.messages_reconcile_owed.is_none());

        app.messages_reconcile_owed = MessagesDraftIdentity::current(
            &app.messages_composer,
            app.messages_composer_revision,
            app.messages_caller,
        );
        finish_messages_reconcile(&mut app);
        assert!(app.messages_composer.stage.is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn full_or_closed_send_lane_is_explicitly_not_sent() {
        for closed in [false, true] {
            let home = cyclops_proto::scratch::scratch_dir(if closed {
                "workspace-messages-send-closed"
            } else {
                "workspace-messages-send-full"
            });
            let mut app = test_app(one_pane_model(), home.clone());
            current_messages_gate(&mut app);
            app.messages_composer = direct_composer();
            let attempt =
                messages_attempt(app.messages_composer_revision, &mut app.messages_composer);
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            if closed {
                drop(rx);
            } else {
                tx.try_send(MessagesSendTask {
                    attempt: attempt.clone(),
                })
                .expect("fill lane");
            }
            app.messages_send_tx = Some(tx);

            queue_messages_send(&mut app, attempt.clone());

            assert!(matches!(
                app.messages_composer.stage,
                Some(cyclops_ui::Stage::NotSent { .. })
            ));
            assert_eq!(app.messages_composer.text(), attempt.body);
            assert_eq!(
                app.messages_composer.draft.key(),
                Some(attempt.client_key.as_str())
            );
            assert!(app.messages_send_in_flight.is_none());
            let _ = std::fs::remove_dir_all(home);
        }
    }

    #[test]
    fn process_generation_refusal_preserves_draft_and_invalidates_routes() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-messages-send-denied");
        let mut app = test_app(one_pane_model(), home.clone());
        current_messages_gate(&mut app);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        app.messages_send_tx = Some(tx);
        app.messages_composer = direct_composer();
        let attempt = messages_attempt(app.messages_composer_revision, &mut app.messages_composer);
        queue_messages_send(&mut app, attempt.clone());
        let _ = rx.try_recv().expect("queued request");

        finish_messages_send(
            &mut app,
            attempt.clone(),
            crate::daemon::SendOutcome::Rejected(crate::daemon::DaemonRefusal::new(
                "denied",
                "the process that opened this connection is no longer the one on it",
            )),
        );

        assert_eq!(app.messages_composer.text(), attempt.body);
        assert_eq!(
            app.messages_composer.draft.key(),
            Some(attempt.client_key.as_str())
        );
        let Some(cyclops_ui::Stage::NotSent { why, .. }) = &app.messages_composer.stage else {
            panic!("a denied sender must be known not sent");
        };
        assert!(why.contains("sender identity changed"), "{why}");
        assert!(!app.messages_gate.may_mutate());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn detail_results_are_bound_to_the_frozen_row() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-messages-detail-target");
        let mut app = test_app(one_pane_model(), home.clone());
        current_messages_gate(&mut app);
        let (tx, mut rx) = mpsc::channel(1);
        app.message_detail_tx = Some(tx);
        let row = cyclops_ui::QueueRow::default();
        let target = cyclops_ui::FrozenTarget {
            target: row.target.clone(),
            attempt: row.attention,
            watermark: 7,
        };
        queue_message_detail(&mut app, row.clone(), target.clone());
        let task = rx.try_recv().expect("detail request");
        assert_eq!(task.row, row);
        assert_eq!(task.target, target);

        let mut newer = cyclops_ui::QueueRow::default();
        newer.message_id = cyclops_proto::MessageId::parse("m-0000000000000002").unwrap();
        newer.target = cyclops_ui::QueueTarget::new(newer.message_id.clone(), newer.recipient);
        let newer_detail = cyclops_ui::Detail::open(&newer, 8);
        let newer_target = newer_detail.target().clone();
        app.messages_detail = Some(newer_detail);

        finish_message_detail(
            &mut app,
            target,
            cyclops_ui::ActionOutcome::Opened(Box::default()),
        );

        assert_eq!(
            app.messages_detail.as_ref().unwrap().target(),
            &newer_target,
            "an old answer cannot replace a newly opened detail"
        );
        assert!(app.message_detail_in_flight.is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn closed_detail_lane_leaves_an_explicit_terminal_state() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-messages-detail-closed");
        let mut app = test_app(one_pane_model(), home.clone());
        current_messages_gate(&mut app);
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        app.message_detail_tx = Some(tx);
        let row = cyclops_ui::QueueRow::default();
        let target = cyclops_ui::FrozenTarget {
            target: row.target.clone(),
            attempt: row.attention,
            watermark: 4,
        };

        queue_message_detail(&mut app, row, target);

        assert!(matches!(
            app.messages_detail.as_ref().map(cyclops_ui::Detail::stage),
            Some(cyclops_ui::Stage::NotSent { .. })
        ));
        assert!(app.message_detail_in_flight.is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn operator_refresh_waits_for_connection_evidence() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-messages-reconnect-proof");
        let mut app = test_app(one_pane_model(), home.clone());
        app.messages_gate.disconnected();

        request_messages_snapshot(&mut app);

        assert_eq!(app.messages_gate.link(), cyclops_ui::Link::Connecting);
        assert!(!app.messages_gate.may_mutate());
        request_messages_snapshot(&mut app);
        assert_eq!(app.messages_gate.link(), cyclops_ui::Link::Connecting);
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn ctrl_r_retries_a_failed_snapshot_without_messages_pane_focus() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let home = cyclops_proto::scratch::scratch_dir("workspace-messages-retry");
        let mut model = one_pane_model();
        model.messages_visible = true;
        let mut app = test_app(model, home.clone());
        app.messages_gate.connected();
        let failed = app.messages_gate.begin().expect("initial snapshot");
        assert!(app.messages_gate.finish_snapshot_failure(failed));
        app.messages_refresh_error = Some("old daemon closed the socket".into());
        app.messages_focused = false;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        app.messages_snapshot_tx = Some(tx);

        let outcome = handle_messages_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        )
        .await
        .expect("key handling");

        assert!(matches!(outcome, Some(InputOutcome::Redraw)));
        assert!(app.messages_refresh_error.is_none());
        assert!(app.messages_gate.is_fetching());
        let (_, limit) = rx.try_recv().expect("retry request");
        assert_eq!(limit, 128);
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn clicking_the_messages_chevron_collapses_the_messages_pane() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};
        use ratatui::backend::TestBackend;

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-messages-toggle");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-messages-toggle-home");
        let mut model = one_pane_model();
        model.messages_visible = true;
        let mut app = test_app(model, home.clone());
        app.prefs.messages_visible = true;
        app.term_size = (80, 24);
        app.window_focused = false;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        let mut motion = Motion::new(false);
        draw(&mut terminal, &mut app, &mut motion, Instant::now()).expect("full frame");
        let (column, row) = (0..80u16)
            .flat_map(|column| (0..24u16).map(move |row| (column, row)))
            .find(|&(column, row)| {
                matches!(
                    app.hit_map.hit(column, row),
                    Some(HitTarget::MessagesToggle)
                )
            })
            .expect("messages chevron hit target");
        let mut detached = false;

        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Down(MouseButton::Left), column, row),
            &mut detached,
        )
        .await
        .expect("click");

        assert!(!app.model.messages_visible);
        assert!(!app.prefs.messages_visible);
        assert!(!app.messages_focused);
        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
    }

    /// Every chrome topology change must ask for a repaint. A diff frame
    /// writes only what the new layout believes changed, so a surface that
    /// vanished leaves its own glyphs behind: the sidebar's rows, the
    /// Messages pane's rows, the tab strip. This is the epoch that was
    /// missing when the owner first landed.
    #[tokio::test]
    async fn every_layout_topology_change_requests_one_repaint() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-topology-epoch");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-topology-epoch-home");
        let mut app = test_app(one_pane_model(), home.clone());
        app.term_size = (80, 24);

        let tab = app.model.active_tab().window_id.clone();
        for action in [
            crate::action::Action::ToggleSidebar,
            crate::action::Action::ToggleMessages,
            crate::action::Action::ToggleTabBar,
            crate::action::Action::ToggleFiles,
            crate::action::Action::SelectTab {
                window_id: tab.clone(),
            },
            crate::action::Action::NewTab { name: None },
            crate::action::Action::CloseTab { window_id: tab },
        ] {
            app.repaint_requested = false;
            // A tab action may legitimately fail against a rig session
            // that has already lost the window; the epoch is the contract
            // under test, not the tmux outcome.
            let _ = exec::execute(&mut app, &client, action.clone()).await;
            assert!(
                app.repaint_requested,
                "{action:?} changed the layout without asking for a repaint"
            );
        }

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
    }

    /// Render one app at one size through its own owner, and hand back the
    /// buffer. The comparison the resize and bleed tests make is only
    /// meaningful against a frame that never saw the other state.
    fn clean_frame(app: &mut App, cols: u16, rows: u16) -> ratatui::buffer::Buffer {
        let mut renderer = RenderOwner::new(
            Terminal::new(FlakyBackend {
                inner: ratatui::backend::TestBackend::new(cols, rows),
                fail_next_flush: false,
                fail_next_clear: false,
                cells_written: 0,
                cursor_queries: 0,
            })
            .expect("terminal"),
        );
        let mut motion = Motion::new(false);
        renderer
            .frame(app, &mut motion, Instant::now())
            .expect("clean frame");
        renderer.terminal.backend().inner.buffer().clone()
    }

    /// Shrinking then growing must land on exactly the frame a workspace
    /// that opened at the final size would have drawn. A diff frame has no
    /// reason to rewrite cells the smaller layout never touched, so without
    /// the resize epoch the grown frame keeps whatever the host left there.
    #[test]
    fn a_resize_down_then_up_matches_a_clean_render_at_the_final_size() {
        let home = cyclops_proto::scratch::scratch_dir("render-owner-resize-roundtrip");
        let mut motion = Motion::new(false);
        let mut app = test_app(one_pane_model(), home.clone());
        app.term_size = (80, 24);
        let mut renderer = RenderOwner::new(
            Terminal::new(FlakyBackend {
                inner: ratatui::backend::TestBackend::new(80, 24),
                fail_next_flush: false,
                fail_next_clear: false,
                cells_written: 0,
                cursor_queries: 0,
            })
            .expect("terminal"),
        );
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("first frame");

        for (cols, rows) in [(48u16, 14u16), (80, 24)] {
            renderer.terminal.backend_mut().inner.resize(cols, rows);
            app.term_size = (cols, rows);
            // What `AppMsg::Resized` does, without the tmux round trip.
            app.repaint_requested = true;
            renderer
                .frame(&mut app, &mut motion, Instant::now())
                .expect("resized frame");
        }

        let mut fresh = test_app(one_pane_model(), home.clone());
        fresh.term_size = (80, 24);
        assert_eq!(
            renderer.terminal.backend().inner.buffer(),
            &clean_frame(&mut fresh, 80, 24),
            "the grown frame kept cells from the smaller layout"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// Focus regain asks for a repaint, and asks exactly once: another
    /// program owned the surface while focus was away, and the request is
    /// drained by the frame that answers it rather than repainting forever.
    #[tokio::test]
    async fn focus_regain_requests_exactly_one_repaint() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-focus-epoch");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (mut client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-focus-epoch-home");
        let mut app = test_app(one_pane_model(), home.clone());
        app.term_size = (80, 24);
        app.repaint_requested = false;
        let mut debounce = None;
        let mut reconnect_deadline = None;
        let mut detached = false;
        let mut pending_input = None;

        handle_app_msg(
            Some(AppMsg::Focus(true)),
            &mut app,
            &mut client,
            &mut debounce,
            &mut reconnect_deadline,
            &mut detached,
            &mut pending_input,
        )
        .await;
        assert!(app.repaint_requested, "focus regain must ask for a repaint");

        // One frame answers it and nothing repaints after that. The
        // reconnect epoch (`handle_reconnect`) shares this same single
        // drain, which is what makes "exactly one" true for both.
        let mut renderer = RenderOwner::new(
            Terminal::new(FlakyBackend {
                inner: ratatui::backend::TestBackend::new(80, 24),
                fail_next_flush: false,
                fail_next_clear: false,
                cells_written: 0,
                cursor_queries: 0,
            })
            .expect("terminal"),
        );
        let mut motion = Motion::new(false);
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("frame");
        assert!(!app.repaint_requested, "the request outlived its frame");
        let after = renderer.terminal.backend().cells_written;
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("frame");
        assert_eq!(
            renderer.terminal.backend().cells_written,
            after,
            "focus regain repainted more than once"
        );

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
    }

    /// A reconnect is a frame-continuity break as much as a byte one: the
    /// old control client is gone, the model is rebuilt, and the surface
    /// on screen was drawn against a connection that no longer exists.
    /// This drives the real `handle_reconnect` rather than asserting a
    /// comment, and pins that it asks exactly once.
    #[tokio::test]
    async fn a_reconnect_requests_exactly_one_repaint() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-reconnect-epoch");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (mut client, _rx) = ControlClient::spawn(cfg.clone()).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-reconnect-epoch-home");
        let mut app = test_app(one_pane_model(), home.clone());
        app.model.session.session = "s".to_string();
        app.term_size = (80, 24);
        app.repaint_requested = false;

        let (tmux_tx, _tmux_rx) = mpsc::channel(8);
        let (stream_tx, _stream_rx) = mpsc::channel(8);
        let (continuity_tx, _continuity_rx) = mpsc::channel(1);
        let sinks = AppSinks {
            tmux: tmux_tx,
            stream: stream_tx,
            continuity: continuity_tx,
        };
        let mut deadline = None;

        handle_reconnect(&mut app, &mut client, &cfg, &sinks, &mut deadline)
            .await
            .expect("reconnect to the live rig");
        assert!(app.repaint_requested, "a reconnect must ask for a repaint");

        let mut renderer = RenderOwner::new(
            Terminal::new(FlakyBackend {
                inner: ratatui::backend::TestBackend::new(80, 24),
                fail_next_flush: false,
                fail_next_clear: false,
                cells_written: 0,
                cursor_queries: 0,
            })
            .expect("terminal"),
        );
        let mut motion = Motion::new(false);
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("frame");
        assert!(!app.repaint_requested, "the request outlived its frame");
        let after = renderer.terminal.backend().cells_written;
        renderer
            .frame(&mut app, &mut motion, Instant::now())
            .expect("frame");
        assert_eq!(
            renderer.terminal.backend().cells_written,
            after,
            "a reconnect repainted more than once"
        );

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
    }

    /// The repair is a repaint and nothing else. It must not close a
    /// dialog, move a panel, change a preference, or touch the layout,
    /// because an operator reaches for it when the screen is wrong and not
    /// when they want the workspace rearranged.
    #[tokio::test]
    async fn the_repaint_action_repairs_the_surface_without_touching_state() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-redraw-action");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-redraw-action-home");
        let mut app = test_app(one_pane_model(), home.clone());
        app.term_size = (80, 24);
        let before = (
            app.model.sidebar_visible,
            app.model.messages_visible,
            app.prefs.clone(),
            app.term_size,
            app.sidebar_tab,
        );

        let outcome = exec::execute(&mut app, &client, crate::action::Action::RequestRedraw)
            .await
            .expect("redraw");

        assert!(app.repaint_requested, "the action asked for nothing");
        assert!(!outcome.persist, "a repaint is not a preference change");
        assert!(!outcome.reconcile, "a repaint is not a model change");
        assert!(!outcome.detach);
        assert_eq!(
            (
                app.model.sidebar_visible,
                app.model.messages_visible,
                app.prefs.clone(),
                app.term_size,
                app.sidebar_tab,
            ),
            before,
            "the repaint mutated application state"
        );

        // The menu row reaches the same action as the chord.
        let ctx = route_context(&app);
        assert_eq!(
            crate::action::route_menu_item(
                &MenuState::AppMenu,
                crate::bindings::BindingAction::Redraw,
                &ctx,
            ),
            Some(crate::action::Action::RequestRedraw),
            "the app-menu Redraw row must reach the same action as Ctrl+B r"
        );

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
    }

    /// The Messages pane's own no-stale-glyph equality, the mirror of the
    /// sidebar's. A collapsed Messages pane gives columns back on the other
    /// edge, and a diff frame has no reason to rewrite them.
    #[test]
    fn collapsing_the_messages_pane_leaves_no_cell_of_it_behind() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-collapse-messages-pane");
        let mut motion = Motion::new(false);
        let mut collapsed = test_app(one_pane_model(), home.clone());
        collapsed.term_size = (100, 24);
        collapsed.model.messages_visible = true;
        let mut renderer = RenderOwner::new(
            Terminal::new(FlakyBackend {
                inner: ratatui::backend::TestBackend::new(100, 24),
                fail_next_flush: false,
                fail_next_clear: false,
                cells_written: 0,
                cursor_queries: 0,
            })
            .expect("terminal"),
        );
        renderer
            .frame(&mut collapsed, &mut motion, Instant::now())
            .expect("frame with the Messages pane");
        collapsed.model.messages_visible = false;
        collapsed.layout_changed();
        renderer
            .frame(&mut collapsed, &mut motion, Instant::now())
            .expect("frame after the collapse");

        let mut clean = test_app(one_pane_model(), home.clone());
        clean.term_size = (100, 24);
        clean.model.messages_visible = false;
        assert_eq!(
            renderer.terminal.backend().inner.buffer(),
            &clean_frame(&mut clean, 100, 24),
            "the collapsed frame kept cells the Messages pane had drawn"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// The Messages pane owns a bordered region beside the agent grid.
    /// Opening it reflows both agent cards into the remaining canvas, and
    /// closing it reclaims that reserved width and restores the prior grid.
    /// A narrow client must keep the selected trailing pane on screen even
    /// when it cannot resize the shared tmux window.
    #[test]
    fn messages_pane_is_bordered_and_never_hides_the_selected_agent() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-messages-pane");
        let mut app = test_app(messages_pane_model(), home.clone());
        app.term_size = (96, 24);

        let _closed = clean_frame(&mut app, 96, 24);
        let closed_left = app
            .hit_map
            .pane_geometry("%0")
            .expect("closed left pane")
            .inner;
        let closed_right = app
            .hit_map
            .pane_geometry("%1")
            .expect("closed selected pane")
            .inner;

        app.model.messages_visible = true;
        app.messages_focused = true;
        app.layout_changed();
        let opened = clean_frame(&mut app, 96, 24);
        let messages = app
            .chrome(Rect::new(0, 0, 96, 24))
            .messages
            .expect("Messages pane rectangle");
        let opened_left = app
            .hit_map
            .pane_geometry("%0")
            .expect("open left pane")
            .inner;
        let opened_right = app
            .hit_map
            .pane_geometry("%1")
            .expect("open selected pane")
            .inner;
        assert!(opened_left.right() <= messages.x);
        assert!(opened_right.right() <= messages.x);
        assert!(opened_left.width < closed_left.width);
        assert!(opened_right.width < closed_right.width);
        assert!(
            (i32::from(opened_right.width) * 60 - i32::from(opened_left.width) * 29).abs() <= 60,
            "both cards must reflow, not clip only the trailing card"
        );
        assert_eq!(opened[(messages.x, messages.y)].symbol(), "╔");
        assert_eq!(opened[(messages.right() - 1, messages.y)].symbol(), "╗");
        assert_eq!(opened[(messages.x, messages.bottom() - 1)].symbol(), "╚");
        assert_eq!(
            opened[(messages.right() - 1, messages.bottom() - 1)].symbol(),
            "╝"
        );

        app.model.messages_visible = false;
        app.messages_focused = false;
        app.layout_changed();
        let _closed_again = clean_frame(&mut app, 96, 24);
        assert_eq!(
            app.hit_map
                .pane_geometry("%0")
                .expect("restored left pane")
                .inner,
            closed_left
        );
        assert_eq!(
            app.hit_map
                .pane_geometry("%1")
                .expect("restored selected pane")
                .inner,
            closed_right
        );

        app.model.messages_visible = true;
        app.messages_focused = true;
        app.term_size = (60, 24);
        app.layout_changed();
        let _narrow = clean_frame(&mut app, 60, 24);
        assert!(
            app.hit_map.pane_geometry("%1").is_some(),
            "narrow Messages view hid the selected agent"
        );

        let _ = std::fs::remove_dir_all(home);
    }

    fn nested_tmux_model(
        server: &cyclops_testrig::TmuxServer,
        session: &str,
    ) -> (WorkspaceModel, String) {
        let output = server.run(&["list-windows", "-t", session, "-F", "#{window_id}"]);
        let window_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let source = nested_tmux_layout(server, &window_id);
        let node = crate::layout::parse_layout(&source).expect("parse nested layout");
        let layout = crate::layout::resolve_layout(&node, &[]).expect("resolve nested layout");
        let output = server.run(&[
            "list-panes",
            "-t",
            session,
            "-f",
            "#{pane_active}",
            "-F",
            "#{pane_id}",
        ]);
        let active_pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let mut model = messages_pane_model();
        model.session.session = session.into();
        model.session.tabs[0].window_id = window_id.clone();
        model.session.tabs[0].layout = layout;
        model.session.tabs[0].active_pane = active_pane;
        model.workspaces[0].name = session.into();
        model.workspaces[0].window_ids = vec![window_id.clone()];
        (model, window_id)
    }

    fn nested_tmux_layout(server: &cyclops_testrig::TmuxServer, window_id: &str) -> String {
        let output = server.run(&["display-message", "-p", "-t", window_id, "#{window_layout}"]);
        assert!(
            output.status.success(),
            "read nested layout: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn start_nested_tmux(server: &cyclops_testrig::TmuxServer, session: &str) {
        server.run_ok(&[
            "new-session",
            "-d",
            "-x",
            "100",
            "-y",
            "30",
            "-s",
            session,
            "/bin/sh",
        ]);
        server.run_ok(&["split-window", "-h", "-l", "30", "-t", session, "/bin/sh"]);
        server.run_ok(&["split-window", "-v", "-l", "5", "-t", session, "/bin/sh"]);
    }

    async fn persisted_boot_sizing_observation(
        tag: &str,
        messages_visible: bool,
    ) -> ((u16, u16), String, (u16, u16)) {
        use cyclops_testrig::TmuxServer;
        use cyclops_tmux::{ControlClient, ControlConfig};

        let server = TmuxServer::new(tag);
        start_nested_tmux(&server, "s");
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let (mut model, window_id) = nested_tmux_model(&server, "s");
        model.sidebar_visible = false;
        model.messages_visible = messages_visible;
        let prefs = WorkspacePrefs {
            sidebar_visible: false,
            messages_visible,
            ..WorkspacePrefs::default()
        };
        let home = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create scratch home");
        let mut sizing = WindowSizing::default();
        let adopted = adopt_windows(&mut sizing, &client, "s", &model.session.tabs, &home).await;
        assert!(adopted.took_a_window, "boot fixture must own its window");

        // Cold boot must size child PTYs to the same visible canvas that the
        // first frame paints, including persisted Messages visibility.
        let painted_target = crate::render::tmux_client_size(
            chrome_for(Rect::new(0, 0, 100, 30), &model, &prefs).canvas,
            model.active_tab(),
        );
        // Enter through the exact production cold-boot calculation and
        // write, after persisted visibility has been installed on the model.
        let declared =
            declare_initial_client_size((100, 30), &model, &prefs, &sizing, &client, &home)
                .await
                .expect("fixture has a declarable cold-boot target");
        let layout = nested_tmux_layout(&server, &window_id);
        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
        (declared, layout, painted_target)
    }

    #[tokio::test]
    async fn persisted_open_cold_boot_sizes_tmux_to_the_visible_canvas() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let closed = persisted_boot_sizing_observation("boot-messages-closed", false).await;
        let open = persisted_boot_sizing_observation("boot-messages-open", true).await;
        assert_eq!(
            closed.0, closed.2,
            "closed cold boot diverged from its painted canvas"
        );
        assert_eq!(
            open.0, open.2,
            "persisted-open cold boot diverged from its painted canvas"
        );
        assert!(
            open.0 .0 < closed.0 .0,
            "persisted-open Messages must narrow the child PTY to its visible canvas"
        );
        assert_ne!(
            open.1, closed.1,
            "persisted-open cold boot did not reflow the nested tmux layout"
        );
    }

    #[tokio::test]
    async fn reconcile_with_open_resized_messages_reflows_nested_tmux_layout() {
        use cyclops_testrig::TmuxServer;
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !cyclops_testrig::tmux_available() {
            return;
        }
        let server = TmuxServer::new("reconcile-open-messages");
        start_nested_tmux(&server, "s");
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let (mut model, window_id) = nested_tmux_model(&server, "s");
        model.sidebar_visible = false;
        model.messages_visible = false;
        let home = cyclops_proto::scratch::scratch_dir("reconcile-open-messages-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create scratch home");
        let mut app = test_app(model, home.clone());
        app.term_size = (100, 30);
        app.prefs.sidebar_visible = false;
        app.prefs.messages_visible = false;

        reconcile(&mut app, &client)
            .await
            .expect("closed reconcile");
        let closed_size = app.declared_client_size.expect("closed size declared");
        let closed_layout = nested_tmux_layout(&server, &window_id);

        app.prefs.messages_visible = true;
        app.prefs.messages_width = 41;
        // Reconnect clears this cache before its generic reconcile. Force
        // that lifecycle path so equality cannot pass only by skipping it.
        app.declared_client_size = None;
        reconcile(&mut app, &client)
            .await
            .expect("open Messages reconcile");
        assert!(app.model.messages_visible);
        let open_size = app
            .declared_client_size
            .expect("open Messages size declared");
        assert!(
            open_size.0 < closed_size.0,
            "open Messages did not narrow the child PTY to the visible canvas"
        );
        assert_ne!(
            nested_tmux_layout(&server, &window_id),
            closed_layout,
            "generic reconcile did not reflow nested tmux for Messages chrome"
        );

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
    }

    async fn nested_layout_after_messages_width_drag(
        lost_release: bool,
    ) -> (String, String, Option<(u16, u16)>, u16, u16) {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        assert!(tmux_available(), "tmux checked by the calling test");
        let tag = if lost_release {
            "app-messages-drag-lost"
        } else {
            "app-messages-drag-up"
        };
        let server = TmuxServer::new(tag);
        server.run_ok(&[
            "new-session",
            "-d",
            "-x",
            "100",
            "-y",
            "30",
            "-s",
            "s",
            "/bin/sh",
        ]);
        server.run_ok(&["split-window", "-h", "-l", "30", "-t", "s", "/bin/sh"]);
        server.run_ok(&["split-window", "-v", "-l", "5", "-t", "s", "/bin/sh"]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");

        let output = server.run(&["list-windows", "-t", "s", "-F", "#{window_id}"]);
        let window_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let read_layout = || {
            let output = server.run(&[
                "display-message",
                "-p",
                "-t",
                &window_id,
                "#{window_layout}",
            ]);
            assert!(
                output.status.success(),
                "read nested layout: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        let source = read_layout();
        let node = crate::layout::parse_layout(&source).expect("parse nested layout");
        let layout = crate::layout::resolve_layout(&node, &[]).expect("resolve nested layout");
        let output = server.run(&[
            "list-panes",
            "-t",
            "s",
            "-f",
            "#{pane_active}",
            "-F",
            "#{pane_id}",
        ]);
        let active_pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let mut model = messages_pane_model();
        model.session.session = "s".into();
        model.session.tabs[0].window_id = window_id.clone();
        model.session.tabs[0].layout = layout;
        model.session.tabs[0].active_pane = active_pane;
        model.workspaces[0].name = "s".into();
        model.workspaces[0].window_ids = vec![window_id.clone()];
        model.messages_visible = true;

        let home = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create scratch home");
        let mut app = test_app(model, home.clone());
        app.term_size = (100, 30);
        app.prefs.sidebar_visible = false;
        app.prefs.messages_visible = true;
        let adopted = adopt_windows(
            &mut app.sizing,
            &client,
            "s",
            &app.model.session.tabs,
            &home,
        )
        .await;
        assert!(adopted.took_a_window, "test app must own the nested window");
        let before = read_layout();

        let _frame = clean_frame(&mut app, 100, 30);
        let (divider_col, divider_row) = (0..100u16)
            .flat_map(|column| (0..30u16).map(move |row| (column, row)))
            .find(|&(column, row)| {
                matches!(
                    app.hit_map.hit(column, row),
                    Some(HitTarget::MessagesDivider)
                )
            })
            .expect("painted Messages pane divider");
        let width_before = app.prefs.messages_width;
        let dragged_col = divider_col.saturating_sub(8);
        let mut detached = false;
        handle_mouse(
            &mut app,
            &client,
            mouse_at(
                MouseEventKind::Down(MouseButton::Left),
                divider_col,
                divider_row,
            ),
            &mut detached,
        )
        .await
        .expect("start Messages pane width drag");
        handle_mouse(
            &mut app,
            &client,
            mouse_at(
                MouseEventKind::Drag(MouseButton::Left),
                dragged_col,
                divider_row,
            ),
            &mut detached,
        )
        .await
        .expect("preview Messages pane width");
        let final_kind = if lost_release {
            MouseEventKind::Moved
        } else {
            MouseEventKind::Up(MouseButton::Left)
        };
        handle_mouse(
            &mut app,
            &client,
            mouse_at(final_kind, dragged_col, divider_row),
            &mut detached,
        )
        .await
        .expect("settle Messages pane width drag");

        let result = (
            before,
            read_layout(),
            app.declared_client_size,
            width_before,
            app.prefs.messages_width,
        );
        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
        result
    }

    #[tokio::test]
    async fn messages_width_drag_release_paths_resize_nested_tmux_layout() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        for lost_release in [false, true] {
            let (before, after, declared, width_before, width_after) =
                nested_layout_after_messages_width_drag(lost_release).await;
            assert_ne!(
                width_after, width_before,
                "fixture must commit a different local Messages pane width"
            );
            assert!(
                declared.is_some(),
                "Messages pane width settlement must declare the visible tmux size"
            );
            assert_ne!(
                after, before,
                "Messages pane width settlement did not reshape the shared nested layout; \
                 lost_release={lost_release}"
            );
        }
    }

    /// A cancelled chrome drag snaps the panel back to the width it had at
    /// mouse-down, vacating every column the preview had taken, so it is a
    /// topology change like any other.
    #[test]
    fn cancelling_a_chrome_drag_requests_a_repaint() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-drag-cancel-epoch");
        for target in [
            DragTarget::Sidebar,
            DragTarget::Messages,
            DragTarget::SidebarSplit,
        ] {
            let mut app = test_app(one_pane_model(), home.clone());
            app.term_size = (100, 24);
            app.repaint_requested = false;
            let mut drag = DragState::on_down(target.clone(), 10, 10);
            drag.on_move(40, 10);
            app.drag = Some(drag);

            cancel_drag(&mut app);

            assert!(
                app.repaint_requested,
                "cancelling {target:?} vacated columns without asking for a repaint"
            );
            assert!(app.drag.is_none());
        }
        let _ = std::fs::remove_dir_all(home);
    }

    /// A reconcile replaces the whole model from a fresh snapshot: panes
    /// may have appeared, closed, moved window, or changed proportion
    /// since the frame on screen, and a diff frame writes only what the
    /// new model believes changed.
    #[tokio::test]
    async fn an_authoritative_model_replacement_requests_a_repaint() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("workspace-reconcile-epoch");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("workspace-reconcile-epoch-home");
        let mut app = test_app(one_pane_model(), home.clone());
        app.model.session.session = "s".to_string();
        app.term_size = (80, 24);
        app.repaint_requested = false;

        reconcile(&mut app, &client).await.expect("reconcile");
        assert!(
            app.repaint_requested,
            "an authoritative model replacement must ask for a repaint"
        );

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
    }

    /// Collapsing a surface must leave the frame identical to one rendered
    /// from a workspace that started collapsed. Anything else is a stale
    /// cell the diff had no reason to touch.
    #[test]
    fn collapsing_the_sidebar_leaves_no_cell_of_it_behind() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-collapse-clean");
        let mut motion = Motion::new(false);

        // One workspace opens with the sidebar, then collapses it.
        let mut collapsed = test_app(one_pane_model(), home.clone());
        collapsed.term_size = (80, 24);
        collapsed.model.sidebar_visible = true;
        let mut renderer = RenderOwner::new(
            Terminal::new(FlakyBackend {
                inner: ratatui::backend::TestBackend::new(80, 24),
                fail_next_flush: false,
                fail_next_clear: false,
                cells_written: 0,
                cursor_queries: 0,
            })
            .expect("terminal"),
        );
        renderer
            .frame(&mut collapsed, &mut motion, Instant::now())
            .expect("frame with the sidebar");
        collapsed.model.sidebar_visible = false;
        collapsed.layout_changed();
        renderer
            .frame(&mut collapsed, &mut motion, Instant::now())
            .expect("frame after the collapse");

        // Another opens already collapsed, and never had a sidebar to
        // leave behind.
        let mut clean = test_app(one_pane_model(), home.clone());
        clean.term_size = (80, 24);
        clean.model.sidebar_visible = false;
        let mut clean_renderer = RenderOwner::new(
            Terminal::new(FlakyBackend {
                inner: ratatui::backend::TestBackend::new(80, 24),
                fail_next_flush: false,
                fail_next_clear: false,
                cells_written: 0,
                cursor_queries: 0,
            })
            .expect("terminal"),
        );
        clean_renderer
            .frame(&mut clean, &mut motion, Instant::now())
            .expect("clean collapsed frame");

        assert_eq!(
            renderer.terminal.backend().inner.buffer(),
            clean_renderer.terminal.backend().inner.buffer(),
            "the collapsed frame kept cells the sidebar had drawn"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn help_exits_zero_message() {
        assert_eq!(print_help_and_exit(), 0);
    }

    #[test]
    fn windows_pin_once_and_a_failed_pin_stays_eligible() {
        let tab = |id: &str| crate::model::TabModel {
            window_id: id.into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: "%0".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%0".into(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
            minimization_provenance: std::collections::HashMap::new(),
        };
        let tabs = vec![tab("@0"), tab("@1")];
        let mut pinned = BTreeSet::new();
        assert_eq!(unpinned_windows(&tabs, &pinned), vec!["@0", "@1"]);
        // Only a recorded pin drops out; a window whose pin failed is not
        // recorded and stays eligible for the next reconcile.
        pinned.insert("@0".to_string());
        assert_eq!(unpinned_windows(&tabs, &pinned), vec!["@1"]);
        pinned.insert("@1".to_string());
        assert!(unpinned_windows(&tabs, &pinned).is_empty());
    }

    #[test]
    fn empty_server_boots_a_fresh_default_session() {
        let (session, create) = boot_target(&persist::ReopenTarget::OfferCreate);
        assert_eq!(session, copy::DEFAULT_SESSION_NAME);
        assert!(create, "nothing to attach to must create, not exit");
    }

    #[test]
    fn existing_session_attaches_without_creating() {
        let reopen = persist::ReopenTarget::First("proj".into());
        assert_eq!(boot_target(&reopen), ("proj".into(), false));
    }

    #[test]
    fn arm_never_pushes_a_pending_deadline_back() {
        let mut debounce = None;
        arm(&mut debounce);
        let first = debounce.expect("armed");
        // A later event must not extend the pending deadline — that shape
        // starves rendering under a steady event stream.
        arm(&mut debounce);
        assert_eq!(debounce, Some(first));
    }

    #[tokio::test]
    async fn due_render_deadline_beats_a_ready_message_queue() {
        let (input_tx, mut input_rx) = mpsc::channel(4);
        let (_paste_tx, mut paste_rx) = mpsc::channel(1);
        let (_action_tx, mut action_rx) = mpsc::channel(4);
        let (_terminal_tx, mut terminal_rx) = mpsc::channel(4);
        let (_tmux_tx, mut tmux_rx) = mpsc::channel(4);
        let (_stream_tx, mut stream_rx) = mpsc::channel(4);
        let (_continuity_tx, mut continuity_rx) = mpsc::channel(1);
        let mut fairness = IngressFairness::default();
        input_tx
            .send(AppMsg::Redraw)
            .await
            .expect("queue stays open");
        let due = Instant::now() - Duration::from_millis(1);

        assert!(matches!(
            next_wake(
                &mut continuity_rx,
                &mut input_rx,
                &mut paste_rx,
                &mut action_rx,
                &mut terminal_rx,
                &mut tmux_rx,
                &mut stream_rx,
                true,
                &mut fairness,
                Some(due),
            )
            .await,
            Wake::Deadline
        ));
    }

    #[tokio::test]
    async fn a_control_gap_preempts_ready_user_input() {
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let (_paste_tx, mut paste_rx) = mpsc::channel(1);
        let (_action_tx, mut action_rx) = mpsc::channel(1);
        let (_terminal_tx, mut terminal_rx) = mpsc::channel(1);
        let (_tmux_tx, mut tmux_rx) = mpsc::channel(1);
        let (_stream_tx, mut stream_rx) = mpsc::channel(1);
        let (continuity_tx, mut continuity_rx) = mpsc::channel(1);
        let mut fairness = IngressFairness::default();
        input_tx.try_send(AppMsg::Redraw).expect("input is ready");
        let (repair, _repair_done) = oneshot::channel();
        let (_cutover_done, cutover) = oneshot::channel();
        assert!(
            continuity_tx
                .try_send(ControlContinuityBarrier { repair, cutover })
                .is_ok(),
            "continuity barrier is ready"
        );

        assert!(matches!(
            next_wake(
                &mut continuity_rx,
                &mut input_rx,
                &mut paste_rx,
                &mut action_rx,
                &mut terminal_rx,
                &mut tmux_rx,
                &mut stream_rx,
                true,
                &mut fairness,
                None,
            )
            .await,
            Wake::ControlContinuityLost(_)
        ));
        assert!(matches!(input_rx.try_recv(), Ok(AppMsg::Redraw)));
    }

    #[test]
    fn a_failed_forced_hydrate_keeps_the_app_barrier_closed() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-continuity-hydrate");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create scratch home");
        let mut app = test_app(one_pane_model(), home.clone());
        app.paused_panes.insert("%0".to_string());

        // The Continue edge was in the dropped suffix. Preparing the
        // authoritative capture must not preserve its earlier Pause.
        prepare_forced_hydration(&mut app);
        assert!(app.paused_panes.is_empty());

        let mut reconnect_deadline = None;
        let acknowledged = settle_control_continuity_repair(
            &mut app,
            Err(TmuxError::Protocol("forced pane capture failed".into())),
            &mut reconnect_deadline,
        );
        assert!(
            !acknowledged,
            "an incomplete capture cannot open the barrier"
        );
        assert!(app.needs_reconcile);
        assert!(matches!(
            app.link_state,
            LinkState::Reconnecting { attempt: 0 }
        ));
        assert!(reconnect_deadline.is_some());
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn an_event_after_capture_keeps_the_app_barrier_closed() {
        let (repair, repair_rx) = oneshot::channel::<bool>();
        let (cutover_tx, cutover) = oneshot::channel();
        let source = tokio::spawn(async move {
            assert!(repair_rx.await.expect("app reports snapshot result"));
            cutover_tx
                .send(false)
                .expect("event after capture rejects cutover");
        });

        let result =
            finish_control_cutover(Ok(()), ControlContinuityBarrier { repair, cutover }).await;
        assert!(
            matches!(result, Err(TmuxError::Protocol(message)) if message.contains("cutover")),
            "snapshot success alone cannot acknowledge continuity"
        );
        source.await.expect("source cutover task exits");
    }

    #[tokio::test]
    async fn input_arriving_during_cutover_wait_keeps_the_closed_epoch() {
        let gate = PaneInputGate::new();
        gate.close();
        let source_gate = gate.clone();
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let (repair, repair_rx) = oneshot::channel::<bool>();
        let (cutover_tx, cutover) = oneshot::channel();
        let source = tokio::spawn(async move {
            assert!(repair_rx.await.expect("app reports snapshot result"));
            input_tx
                .send(AppMsg::Input {
                    epoch: source_gate.stamp(),
                    key: KeyEvent::new(crossterm::event::KeyCode::Char('x'), KeyModifiers::empty()),
                })
                .await
                .expect("input arrives before source cutover");
            cutover_tx.send(true).expect("source completes cutover");
        });

        finish_control_cutover(Ok(()), ControlContinuityBarrier { repair, cutover })
            .await
            .expect("snapshot and source cutover both succeed");
        gate.open();
        let input = input_rx.try_recv().expect("cutover input was queued");
        assert!(
            !gate.accepts(input.pane_input_epoch().expect("pane input epoch")),
            "input accepted during repair cannot target the reconciled model"
        );
        source.await.expect("source cutover task exits");
    }

    #[tokio::test]
    async fn pending_pane_input_drains_background_without_consuming_later_actions() {
        let (input_tx, mut input_rx) = mpsc::channel(4);
        let (paste_tx, mut paste_rx) = mpsc::channel(1);
        let (action_tx, mut action_rx) = mpsc::channel(4);
        let (terminal_tx, mut terminal_rx) = mpsc::channel(4);
        let (tmux_tx, mut tmux_rx) = mpsc::channel(4);
        let (stream_tx, mut stream_rx) = mpsc::channel(4);
        let (_continuity_tx, mut continuity_rx) = mpsc::channel(1);
        let mut background_cursor = 0;
        let mut capacity: InputCapacityFuture = Box::pin(std::future::pending());

        input_tx
            .try_send(AppMsg::Input {
                epoch: 0,
                key: KeyEvent::new(crossterm::event::KeyCode::Char('b'), KeyModifiers::empty()),
            })
            .expect("input lane accepts a later key");
        input_tx
            .try_send(AppMsg::Mouse {
                epoch: 0,
                mouse: MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 1,
                    row: 1,
                    modifiers: KeyModifiers::empty(),
                },
            })
            .expect("input lane accepts a later mouse action");
        paste_tx
            .try_send(AppMsg::Paste {
                epoch: 0,
                text: "later paste".to_string(),
            })
            .expect("paste lane accepts later text");
        action_tx
            .try_send(AppMsg::ThemeChanged)
            .expect("action lane accepts a later completion");
        tmux_tx
            .try_send(AppMsg::Redraw)
            .expect("tmux lane carries background state");
        terminal_tx
            .try_send(AppMsg::Focus(true))
            .expect("terminal lane carries focus state");
        stream_tx
            .try_send(AppMsg::ThemeChanged)
            .expect("stream lane carries daemon state");

        for expected in ["focus", "tmux", "stream"] {
            let wake = next_pending_input_wake(
                &mut continuity_rx,
                &mut capacity,
                &mut terminal_rx,
                &mut tmux_rx,
                &mut stream_rx,
                true,
                &mut background_cursor,
                None,
            )
            .await;
            let Wake::Message(Some(message)) = wake else {
                panic!("pending input returned a non-message wake");
            };
            match (expected, *message) {
                ("focus", AppMsg::Focus(true))
                | ("tmux", AppMsg::Redraw)
                | ("stream", AppMsg::ThemeChanged) => {}
                _ => panic!("pending input drained background lanes out of order"),
            }
        }
        assert!(matches!(input_rx.try_recv(), Ok(AppMsg::Input { .. })));
        assert!(matches!(input_rx.try_recv(), Ok(AppMsg::Mouse { .. })));
        assert!(matches!(paste_rx.try_recv(), Ok(AppMsg::Paste { .. })));
        assert!(matches!(action_rx.try_recv(), Ok(AppMsg::ThemeChanged)));
    }

    #[tokio::test]
    async fn a_control_gap_retires_the_whole_pre_reconcile_input_segment() {
        let (input_tx, mut input_rx) = mpsc::channel(4);
        let (paste_tx, mut paste_rx) = mpsc::channel(1);
        let mut pending = Some(PendingPaneInput {
            pane: "%7".to_string(),
            keys: vec!["a".to_string()],
        });
        for key in ['b', 'c'] {
            input_tx
                .try_send(AppMsg::Input {
                    epoch: 0,
                    key: KeyEvent::new(crossterm::event::KeyCode::Char(key), KeyModifiers::empty()),
                })
                .expect("later key queues behind held input");
        }
        paste_tx
            .try_send(AppMsg::Paste {
                epoch: 0,
                text: "later paste".to_string(),
            })
            .expect("later paste queues behind held input");

        assert_eq!(
            retire_pane_input_segment("%9", &mut pending, &mut input_rx, &mut paste_rx),
            Some("%7".to_string()),
            "one notice names the target of the first held batch"
        );
        assert!(pending.is_none());
        assert!(input_rx.try_recv().is_err(), "queued keys are retired");
        assert!(paste_rx.try_recv().is_err(), "queued paste is retired");
    }

    #[tokio::test]
    async fn input_arriving_during_repair_is_quarantined_before_cutover() {
        let (input_tx, mut input_rx) = mpsc::channel(2);
        let (paste_tx, mut paste_rx) = mpsc::channel(1);
        let mut pending = None;
        assert!(
            retire_pane_input_segment("%3", &mut pending, &mut input_rx, &mut paste_rx).is_none(),
            "the prefix starts empty"
        );

        input_tx
            .try_send(AppMsg::Input {
                epoch: 0,
                key: KeyEvent::new(crossterm::event::KeyCode::Char('x'), KeyModifiers::empty()),
            })
            .expect("key arrives while capture is awaited");
        paste_tx
            .try_send(AppMsg::Paste {
                epoch: 0,
                text: "during repair".to_string(),
            })
            .expect("paste arrives while capture is awaited");

        assert_eq!(
            retire_pane_input_segment("%3", &mut pending, &mut input_rx, &mut paste_rx),
            Some("%3".to_string())
        );
        assert!(input_rx.try_recv().is_err());
        assert!(paste_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn disconnected_capacity_wins_without_replaying_a_later_message() {
        let (_terminal_tx, mut terminal_rx) = mpsc::channel(4);
        let (tmux_tx, mut tmux_rx) = mpsc::channel(4);
        let (_stream_tx, mut stream_rx) = mpsc::channel(4);
        let (_continuity_tx, mut continuity_rx) = mpsc::channel(1);
        let mut background_cursor = 0;
        let mut capacity: InputCapacityFuture = Box::pin(async { Err(TmuxError::Disconnected) });
        tmux_tx
            .try_send(AppMsg::Redraw)
            .expect("tmux lane accepts a ready message");

        assert!(matches!(
            next_pending_input_wake(
                &mut continuity_rx,
                &mut capacity,
                &mut terminal_rx,
                &mut tmux_rx,
                &mut stream_rx,
                true,
                &mut background_cursor,
                None,
            )
            .await,
            Wake::InputCapacity(Err(TmuxError::Disconnected))
        ));
        assert!(matches!(tmux_rx.try_recv(), Ok(AppMsg::Redraw)));
    }

    #[test]
    fn held_input_keeps_the_original_pane_and_exact_keys() {
        let held = PendingPaneInput {
            pane: "%7".to_string(),
            keys: vec!["C-e".to_string(), "C-u".to_string()],
        };
        assert_eq!(held.pane, "%7");
        assert_eq!(held.keys, ["C-e", "C-u"]);
    }

    #[test]
    fn a_close_before_the_first_write_is_visible_and_never_replayable() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-input-disconnected");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create scratch home");
        let mut app = test_app(one_pane_model(), home.clone());
        let mut pending = None;
        let mut detached = false;
        let mut debounce = None;

        let outcome = pane_input_outcome(
            "%7".to_string(),
            vec!["CYCLOPS_INPUT_MUST_NOT_REPLAY".to_string()],
            Err(TmuxError::Disconnected),
        );
        apply_pane_input_outcome(
            outcome,
            &mut app,
            &mut pending,
            &mut detached,
            &mut debounce,
        );

        assert_eq!(
            app.notice.text(),
            Some("input was not sent to %7: tmux control connection closed")
        );
        assert!(debounce.is_some(), "the visible notice earns one frame");
        assert!(pending.is_none(), "failed key bytes are not retained");
        assert!(!detached);

        // A later link-loss event only inspects PendingPaneInput. Since the
        // initial failure stores no key bytes there, reconnect has nothing
        // it can replay and cannot produce a second target notice.
        assert!(pending.take().is_none());
        assert_eq!(
            app.notice.text(),
            Some("input was not sent to %7: tmux control connection closed")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_write_failure_is_uncertain_visible_and_never_replayable() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-input-uncertain");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create scratch home");
        let mut app = test_app(one_pane_model(), home.clone());
        let mut pending = None;
        let mut detached = false;
        let mut debounce = None;
        let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "flush failed");

        let outcome = pane_input_outcome(
            "%7".to_string(),
            vec!["CYCLOPS_INPUT_MAY_HAVE_LANDED".to_string()],
            Err(TmuxError::WriteUncertain(error)),
        );
        apply_pane_input_outcome(
            outcome,
            &mut app,
            &mut pending,
            &mut detached,
            &mut debounce,
        );

        let notice = app.notice.text().expect("uncertain write is visible");
        assert!(notice.contains("input may have reached %7"));
        assert!(notice.contains("will not be replayed"));
        assert!(!notice.contains("input was not sent"));
        assert!(pending.is_none(), "uncertain key bytes are not retained");
        assert!(debounce.is_some());
        assert!(!detached);
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn a_ready_input_precedes_a_full_stream_lane() {
        let (input_tx, mut input_rx) = mpsc::channel(4);
        let (_paste_tx, mut paste_rx) = mpsc::channel(1);
        let (_action_tx, mut action_rx) = mpsc::channel(4);
        let (_terminal_tx, mut terminal_rx) = mpsc::channel(4);
        let (_tmux_tx, mut tmux_rx) = mpsc::channel(4);
        let (stream_tx, mut stream_rx) = mpsc::channel(4);
        let mut fairness = IngressFairness::default();

        for _ in 0..4 {
            stream_tx.try_send(AppMsg::ThemeChanged).unwrap();
        }
        assert!(stream_tx.try_send(AppMsg::ThemeChanged).is_err());
        input_tx.try_send(AppMsg::Redraw).unwrap();

        assert!(matches!(
            next_message(
                &mut input_rx,
                &mut paste_rx,
                &mut action_rx,
                &mut terminal_rx,
                &mut tmux_rx,
                &mut stream_rx,
                true,
                &mut fairness,
            )
            .await,
            Some(AppMsg::Redraw)
        ));
        assert!(matches!(stream_rx.try_recv(), Ok(AppMsg::ThemeChanged)));
    }

    #[tokio::test]
    async fn reconciliation_blocks_stream_entries_without_blocking_actions() {
        let (_input_tx, mut input_rx) = mpsc::channel(4);
        let (_paste_tx, mut paste_rx) = mpsc::channel(1);
        let (action_tx, mut action_rx) = mpsc::channel(4);
        let (_terminal_tx, mut terminal_rx) = mpsc::channel(4);
        let (_tmux_tx, mut tmux_rx) = mpsc::channel(4);
        let (stream_tx, mut stream_rx) = mpsc::channel(4);
        let mut fairness = IngressFairness::default();

        stream_tx.try_send(AppMsg::ThemeChanged).unwrap();
        action_tx.try_send(AppMsg::Redraw).unwrap();

        assert!(matches!(
            next_message(
                &mut input_rx,
                &mut paste_rx,
                &mut action_rx,
                &mut terminal_rx,
                &mut tmux_rx,
                &mut stream_rx,
                false,
                &mut fairness,
            )
            .await,
            Some(AppMsg::Redraw)
        ));
        assert!(matches!(stream_rx.try_recv(), Ok(AppMsg::ThemeChanged)));
    }

    #[tokio::test]
    async fn a_priority_burst_yields_to_ready_background_work() {
        let (input_tx, mut input_rx) = mpsc::channel(PRIORITY_BURST + 1);
        let (_paste_tx, mut paste_rx) = mpsc::channel(1);
        let (_action_tx, mut action_rx) = mpsc::channel(1);
        let (_terminal_tx, mut terminal_rx) = mpsc::channel(1);
        let (_tmux_tx, mut tmux_rx) = mpsc::channel(1);
        let (stream_tx, mut stream_rx) = mpsc::channel(1);
        let mut fairness = IngressFairness::default();
        for _ in 0..=PRIORITY_BURST {
            input_tx.try_send(AppMsg::Redraw).unwrap();
        }
        stream_tx.try_send(AppMsg::ThemeChanged).unwrap();

        for _ in 0..PRIORITY_BURST {
            assert!(matches!(
                next_message(
                    &mut input_rx,
                    &mut paste_rx,
                    &mut action_rx,
                    &mut terminal_rx,
                    &mut tmux_rx,
                    &mut stream_rx,
                    true,
                    &mut fairness,
                )
                .await,
                Some(AppMsg::Redraw)
            ));
        }
        assert!(matches!(
            next_message(
                &mut input_rx,
                &mut paste_rx,
                &mut action_rx,
                &mut terminal_rx,
                &mut tmux_rx,
                &mut stream_rx,
                true,
                &mut fairness,
            )
            .await,
            Some(AppMsg::ThemeChanged)
        ));
    }

    #[test]
    fn decoration_edges_coalesce_in_the_single_pending_slot() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        assert!(signal_decoration_event(&tx));
        assert!(signal_decoration_event(&tx));
        assert!(matches!(rx.try_recv(), Ok(DecorationSignal::Event)));
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(rx);
        assert!(!signal_decoration_event(&tx));
    }

    /// L1: a burst of daemon events must collapse to exactly one status
    /// fetch, the same coalescing guarantee `arm()`/`RENDER_DEBOUNCE` give
    /// rendering. A fake cyclopsd accepts the forwarder's persistent
    /// `events.subscribe` connection, pushes five event lines back to back
    /// with no delay (a split or a border drag's burst), then counts every
    /// LATER connection — each one is exactly one coalesced status fetch.
    #[test]
    fn a_burst_of_decoration_events_produces_one_refresh() {
        use std::io::{BufRead, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let home = cyclops_proto::scratch::scratch_dir("workspace-decoration-burst");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let listener =
            UnixListener::bind(home.join(cyclops_proto::SOCK_NAME)).expect("bind fake daemon");

        let fetches = Arc::new(AtomicUsize::new(0));
        let fetches_for_server = fetches.clone();
        std::thread::spawn(move || {
            // 1. The persistent subscribe connection: hello, read the
            //    subscribe request, then a burst of five event lines with
            //    no delay between them.
            let (stream, _) = listener.accept().expect("subscribe connection");
            let mut reader = std::io::BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut line = String::new();
            reader.read_line(&mut line).expect("subscribe request");
            reader
                .get_mut()
                .write_all(b"{\"id\":1,\"result\":{\"subscribed\":true}}\n")
                .expect("subscribe acknowledgement");
            for _ in 0..5 {
                let _ = reader
                    .get_mut()
                    .write_all(b"{\"event\":\"state\",\"data\":{}}\n");
            }

            // 2. Every later connection is one coalesced status fetch:
            //    answer it and count it.
            for stream in listener.incoming().flatten() {
                fetches_for_server.fetch_add(1, Ordering::SeqCst);
                let mut reader = std::io::BufReader::new(stream);
                let _ = reader
                    .get_mut()
                    .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n");
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let body = serde_json::json!({
                    "id": 1,
                    "result": {
                        "daemon_version": "0.1.0",
                        "proto": 1,
                        "boot_id": "b",
                        "uptime_ms": 0,
                        "tmux_version": "3.4",
                        "sessions": [],
                    },
                });
                let mut out = serde_json::to_vec(&body).expect("encode status");
                out.push(b'\n');
                let _ = reader.get_mut().write_all(&out);
            }
        });

        let (control_tx, mut control_rx) = mpsc::channel(INGRESS_CAPACITY);
        let (stream_tx, _stream_rx) = mpsc::channel(INGRESS_CAPACITY);
        spawn_decoration_forwarder(home.clone(), control_tx, stream_tx);

        // 3. Wait for the coalesced refresh to land, then confirm it stays
        //    at exactly one — bounded polling in test code only (rule 9's
        //    documented exception), never in the forwarder itself.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while fetches.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(DECORATION_DEBOUNCE * 5);
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "a burst of five daemon events must collapse to exactly one status fetch"
        );

        let mut changed = 0;
        while let Ok(msg) = control_rx.try_recv() {
            if matches!(msg, AppMsg::DecorationChanged(_)) {
                changed += 1;
            }
        }
        assert_eq!(
            changed, 1,
            "exactly one DecorationChanged should reach the app per coalesced burst"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A daemon "theme" event must reach the app as the wake-only
    /// `ThemeChanged`, never as a stream entry: the record mirrors
    /// `cyclops watch`, which drops theme switches the same way.
    #[test]
    fn a_theme_event_wakes_the_app_without_entering_the_record() {
        use std::io::{BufRead, Write};
        use std::os::unix::net::UnixListener;

        let home = cyclops_proto::scratch::scratch_dir("workspace-theme-event");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let listener =
            UnixListener::bind(home.join(cyclops_proto::SOCK_NAME)).expect("bind fake daemon");

        std::thread::spawn(move || {
            // The persistent subscribe connection: hello, read the
            // subscribe request, then one theme event. Later connections
            // (the coalesced status fetch) are dropped unanswered; the
            // refresh closure treats that as doubt and sends nothing.
            let (stream, _) = listener.accept().expect("subscribe connection");
            let mut reader = std::io::BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut line = String::new();
            reader.read_line(&mut line).expect("subscribe request");
            reader
                .get_mut()
                .write_all(b"{\"id\":1,\"result\":{\"subscribed\":true}}\n")
                .expect("subscribe acknowledgement");
            let _ = reader
                .get_mut()
                .write_all(b"{\"event\":\"theme\",\"data\":{\"name\":\"aurora\"}}\n");
            for stream in listener.incoming().flatten() {
                drop(stream);
            }
        });

        let (control_tx, _control_rx) = mpsc::channel(INGRESS_CAPACITY);
        let (stream_tx, mut stream_rx) = mpsc::channel(INGRESS_CAPACITY);
        spawn_decoration_forwarder(home.clone(), control_tx, stream_tx);

        // Bounded polling in test code only (rule 9's documented
        // exception), never in the forwarder itself.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut woke = 0;
        let mut entries = 0;
        while woke == 0 && std::time::Instant::now() < deadline {
            while let Ok(msg) = stream_rx.try_recv() {
                match msg {
                    AppMsg::ThemeChanged => woke += 1,
                    AppMsg::StreamEntry(_) => entries += 1,
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(woke, 1, "a theme event must wake the app exactly once");
        assert_eq!(entries, 0, "a theme event must never become a stream entry");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A fake cyclopsd for subscription-lifecycle tests: serves any
    /// number of sequential connections on the home socket, unlike the
    /// single-connection fakes above. `events.subscribe` connections are
    /// held open ([`FakeDaemon::push_event`] writes to them); `status`
    /// answers with one working agent in pane %0 of session "s";
    /// `session.watch` answers ok and records the asked name. Dropping
    /// the handle is a daemon death: the socket file goes away and every
    /// held subscription closes.
    struct FakeDaemon {
        home: std::path::PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        subs: std::sync::Arc<std::sync::Mutex<Vec<std::os::unix::net::UnixStream>>>,
        watches: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeDaemon {
        fn spawn(home: &std::path::Path) -> FakeDaemon {
            use std::io::{BufRead, Write};
            use std::os::unix::net::UnixListener;
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Arc, Mutex};

            let socket = home.join(cyclops_proto::SOCK_NAME);
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).expect("bind fake daemon");
            // Nonblocking accept so a stopped daemon's thread can notice
            // the stop flag; bounded test-side polling only (rule 9's
            // documented exception).
            listener.set_nonblocking(true).expect("nonblocking accept");
            let stop = Arc::new(AtomicBool::new(false));
            let subs = Arc::new(Mutex::new(Vec::new()));
            let watches = Arc::new(Mutex::new(Vec::new()));
            let stop_bg = Arc::clone(&stop);
            let subs_bg = Arc::clone(&subs);
            let watches_bg = Arc::clone(&watches);
            std::thread::spawn(move || {
                while !stop_bg.load(Ordering::SeqCst) {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                    };
                    // BSD-derived accepts can inherit nonblocking; every
                    // connection below is plain blocking line IO.
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    let mut reader = std::io::BufReader::new(stream);
                    if reader
                        .get_mut()
                        .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                        .is_err()
                    {
                        continue;
                    }
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    match request["method"].as_str() {
                        Some("events.subscribe") => {
                            let mut stream = reader.into_inner();
                            let _ =
                                stream.write_all(b"{\"id\":1,\"result\":{\"subscribed\":true}}\n");
                            subs_bg.lock().expect("subs").push(stream);
                        }
                        Some("session.watch") => {
                            if let Some(name) = request["params"]["session"].as_str() {
                                watches_bg.lock().expect("watches").push(name.to_string());
                            }
                            let _ = reader.get_mut().write_all(b"{\"id\":1,\"result\":{}}\n");
                        }
                        Some("status") => {
                            let body = serde_json::json!({
                                "id": 1,
                                "result": {
                                    "daemon_version": "0.1.0",
                                    "proto": 1,
                                    "boot_id": "b",
                                    "uptime_ms": 0,
                                    "tmux_version": "3.4",
                                    "sessions": [{
                                        "name": "s",
                                        "attached": true,
                                        "panes": [{
                                            "pane_id": "%0",
                                            "window_id": "@0",
                                            "window_name": "1",
                                            "agent": "reviewer",
                                            "title": "",
                                            "current_command": "sh",
                                            "dead": false,
                                            "in_mode": false,
                                            "width": 80,
                                            "height": 24,
                                            "state": "working",
                                        }],
                                    }],
                                },
                            });
                            let mut out = serde_json::to_vec(&body).expect("encode status");
                            out.push(b'\n');
                            let _ = reader.get_mut().write_all(&out);
                        }
                        _ => {}
                    }
                }
            });
            FakeDaemon {
                home: home.to_path_buf(),
                stop,
                subs,
                watches,
            }
        }

        fn push_event(&self, line: &[u8]) {
            use std::io::Write;
            for stream in self.subs.lock().expect("subs").iter_mut() {
                let _ = stream.write_all(line);
            }
        }

        fn watched(&self) -> Vec<String> {
            self.watches.lock().expect("watches").clone()
        }
    }

    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;
            self.stop.store(true, Ordering::SeqCst);
            // What a daemon death looks like from outside: connects start
            // failing and the live subscriptions EOF.
            let _ = std::fs::remove_file(self.home.join(cyclops_proto::SOCK_NAME));
            self.subs.lock().expect("subs").clear();
        }
    }

    /// Bounded wait for one matching app message. Test-side polling only
    /// (rule 9's documented exception); the forwarder itself stays
    /// event-armed.
    fn wait_for_msg(
        rx: &mut mpsc::Receiver<AppMsg>,
        what: &str,
        mut matching: impl FnMut(&AppMsg) -> bool,
    ) -> AppMsg {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(msg) if matching(&msg) => return msg,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("gave up waiting for {what}");
    }

    /// The regression the reconnect loop exists for: the one-shot
    /// forwarder died silently on a boot-order race or a daemon restart,
    /// and decoration then refreshed only inside structural reconciles
    /// (the stale status indicator the operator alt-tabbed to fix). The
    /// subscription must survive both a daemon that is not up yet and a
    /// daemon that dies and returns, and events on the new connection
    /// must still become refreshes.
    #[test]
    fn the_decoration_subscription_survives_boot_races_and_daemon_restarts() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-dec-restart");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");

        let (control_tx, mut control_rx) = mpsc::channel(INGRESS_CAPACITY);
        let (stream_tx, mut stream_rx) = mpsc::channel(INGRESS_CAPACITY);
        let home_bg = home.clone();
        // Millisecond backoff so the whole lifecycle runs in test time;
        // production wires resilience::reconnect_delay.
        std::thread::spawn(move || {
            run_decoration_forwarder(&home_bg, &control_tx, &stream_tx, |_| {
                Duration::from_millis(5)
            });
        });

        // Boot race: nothing is listening yet. The forwarder must still
        // be trying, not dead, when the daemon finally binds.
        std::thread::sleep(Duration::from_millis(40));
        let daemon = FakeDaemon::spawn(&home);
        wait_for_msg(
            &mut stream_rx,
            "the resync ask after the boot race",
            |msg| matches!(msg, AppMsg::DaemonReconnected),
        );

        // Restart: the live connection drops; before the fix the thread
        // ended here, permanently.
        drop(daemon);
        let daemon = FakeDaemon::spawn(&home);
        wait_for_msg(&mut stream_rx, "the gap before the restart", |msg| {
            matches!(msg, AppMsg::StreamGap { .. })
        });
        wait_for_msg(&mut stream_rx, "the resync ask after the restart", |msg| {
            matches!(msg, AppMsg::DaemonReconnected)
        });

        // A state event on the new subscription still becomes a coalesced
        // online refresh. The push can race the daemon registering the
        // fresh subscription, so it is re-sent on a bounded schedule;
        // one landing is proof.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut refreshed = false;
        'push: while std::time::Instant::now() < deadline {
            daemon.push_event(b"{\"event\":\"state\",\"data\":{}}\n");
            let pause = std::time::Instant::now() + Duration::from_millis(100);
            while std::time::Instant::now() < pause {
                match control_rx.try_recv() {
                    Ok(AppMsg::DecorationChanged(snapshot)) if snapshot.online => {
                        refreshed = true;
                        break 'push;
                    }
                    Ok(_) => {}
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        }
        assert!(
            refreshed,
            "a state event after the restart must still produce a fresh online refresh"
        );

        drop(daemon);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A daemon that stays gone is reported once per outage: after the
    /// backoff chain fails `RECONNECT_CAP` times the app gets one empty
    /// offline snapshot (the sidebar's "cyclopsd offline" line) and
    /// workspace.log says why, while the loop keeps retrying quietly. A
    /// daemon that finally answers is resynced like any reconnect, so
    /// the report is a state, not a surrender.
    #[test]
    fn a_daemon_that_stays_gone_is_reported_offline_once_then_recovered() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-dec-offline");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");

        let (control_tx, mut control_rx) = mpsc::channel(INGRESS_CAPACITY);
        let (stream_tx, mut stream_rx) = mpsc::channel(INGRESS_CAPACITY);
        let home_bg = home.clone();
        std::thread::spawn(move || {
            run_decoration_forwarder(&home_bg, &control_tx, &stream_tx, |_| {
                Duration::from_millis(2)
            });
        });

        let offline = wait_for_msg(&mut control_rx, "the offline report", |msg| {
            matches!(msg, AppMsg::DecorationChanged(_))
        });
        let AppMsg::DecorationChanged(snapshot) = offline else {
            unreachable!("wait_for_msg matched DecorationChanged");
        };
        assert!(!snapshot.online, "the report is the empty offline snapshot");
        let log_deadline = std::time::Instant::now() + Duration::from_secs(2);
        let log_path = home.join("workspace.log");
        while std::time::Instant::now() < log_deadline
            && !std::fs::read_to_string(&log_path)
                .unwrap_or_default()
                .contains("cyclopsd is not answering")
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            std::fs::read_to_string(&log_path)
                .unwrap_or_default()
                .contains("cyclopsd is not answering"),
            "the outage must leave one honest line in workspace.log"
        );

        // Once per outage: many more failed chain turns add nothing.
        std::thread::sleep(Duration::from_millis(60));
        while let Ok(msg) = control_rx.try_recv() {
            assert!(
                !matches!(msg, AppMsg::DecorationChanged(_)),
                "offline must be reported once, not once per retry"
            );
        }

        // The chain never gave up: a daemon that appears now is found.
        let daemon = FakeDaemon::spawn(&home);
        wait_for_msg(&mut stream_rx, "the resync ask after recovery", |msg| {
            matches!(msg, AppMsg::DaemonReconnected)
        });

        drop(daemon);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// What the resync ask does when it lands: the stale watch record is
    /// dropped and re-asked (a restarted daemon has no watch table), and
    /// the snapshot is replaced whole, so a state that flipped while
    /// nothing was subscribed cannot stay on screen as its old word.
    #[test]
    fn a_reconnect_resync_reasks_watches_and_replaces_the_snapshot() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-dec-resync");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let daemon = FakeDaemon::spawn(&home);

        let mut app = test_app(one_pane_model(), home.clone());
        // Watched under the daemon that died, and showing its last word.
        app.watched_sessions.insert("$0".into());
        app.decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        app.decoration.panes.insert(
            "%0".into(),
            crate::decoration::PaneDecoration {
                pane_id: "%0".into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: None,
                manifest_display_name: None,
                state: cyclops_proto::AgentState::Idle,
                needs_attention: false,
            },
        );

        resync_daemon_state(&mut app);

        assert_eq!(
            daemon.watched(),
            vec!["s".to_string()],
            "the on-screen workspace must be re-asked"
        );
        assert!(
            app.watched_sessions.contains("$0"),
            "the fresh ask is recorded again"
        );
        assert!(app.decoration.online);
        assert_eq!(
            app.decoration.pane("%0").expect("the resynced agent").state,
            cyclops_proto::AgentState::Working,
            "the state change the outage swallowed must be on screen"
        );

        drop(daemon);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The dead strip down the right edge, stated as arithmetic.
    ///
    /// Dragging the sidebar narrower widens the pane canvas immediately,
    /// because the sidebar's rectangle is the app's own. The grid tmux was
    /// told about does not change until something tells it, so between
    /// those two facts sits a run of columns the canvas owns and no pane
    /// fills. This measures that gap directly: it is zero before the drag,
    /// grows with every column crossed, and is what `apply_live_sidebar`
    /// exists to keep at zero.
    ///
    /// The geometry itself is not at fault and this pins that too: at
    /// every width the sidebar, the declared grid, the canvas margins and
    /// the layout's gap overhead account for every column of the terminal.
    #[test]
    fn narrowing_the_sidebar_strands_canvas_columns_until_tmux_is_told() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-sidebar-live");
        let mut app = test_app(one_pane_model(), home.clone());
        app.term_size = (120, 30);
        let full = Rect::new(0, 0, app.term_size.0, app.term_size.1);

        let declared_for = |app: &App| {
            crate::render::tmux_client_size(app.chrome(full).canvas, app.model.active_tab())
        };

        // Settled: what tmux was told is what the canvas wants.
        let start = 30u16;
        app.prefs.sidebar_width = start;
        app.declared_client_size = Some(declared_for(&app));
        assert_eq!(stranded_columns(&app, full), 0);

        // Now drag the edge left, the way a pointer walks it, without
        // telling tmux. Every column the sidebar gives up is a column the
        // canvas has and no pane covers.
        //
        // The floor comes from `clamp_sidebar_width`, not a number written
        // here: it has already moved once, and a test that restated it
        // would measure the clamp rather than the strip.
        let floor = crate::render::clamp_sidebar_width(0, full.width);
        assert!(floor < start, "the drag has somewhere to go");
        for width in (floor..start).rev() {
            app.prefs.sidebar_width = width;
            assert_eq!(
                stranded_columns(&app, full),
                i32::from(start - width),
                "width {width}: the strip is exactly the columns not handed over"
            );
        }

        // Telling tmux is what closes it, which is all the fix does.
        app.declared_client_size = Some(declared_for(&app));
        assert_eq!(stranded_columns(&app, full), 0);

        // And the geometry never loses a column of its own: at every width
        // the panel, the grid, the margins and the gaps add up to the
        // terminal.
        let tab = app.model.active_tab().clone();
        let (gap_w, _) = crate::layout::layout_gap_overhead(&tab.layout, crate::render::PANE_GAPS);
        for want in 10..=50u16 {
            let width = crate::render::clamp_sidebar_width(want, full.width);
            app.prefs.sidebar_width = width;
            let (grid, _) = declared_for(&app);
            let messages_rail = app
                .chrome(full)
                .messages_rail
                .expect("closed Messages rail")
                .width;
            assert_eq!(
                i32::from(width)
                    + i32::from(grid)
                    + 2 * i32::from(crate::render::PANE_MARGIN)
                    + i32::from(gap_w)
                    + i32::from(messages_rail),
                i32::from(full.width),
                "width {width} loses a column somewhere"
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Canvas columns no pane can be covering: what the canvas is now
    /// versus the grid tmux was last told to lay out.
    fn stranded_columns(app: &App, full: Rect) -> i32 {
        let want = crate::render::tmux_client_size(app.chrome(full).canvas, app.model.active_tab());
        let told = app.declared_client_size.unwrap_or(want);
        i32::from(want.0) - i32::from(told.0)
    }

    /// A dialog drag accumulates from the last position it was applied at,
    /// and gives back the travel it spent against the screen edge.
    ///
    /// Measured from the drag's start instead, a pointer that ran off the
    /// edge would keep banking travel the clamp discards, and the box would
    /// sit still until the pointer had walked all of it back. This is the
    /// bug the test is here to keep out.
    #[test]
    fn a_dialog_drag_does_not_bank_travel_it_spent_against_the_edge() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-dialog-drag");
        let mut app = test_app(one_pane_model(), home.clone());
        app.term_size = (80, 24);
        app.open_dialog(Dialog::confirm_close("%0"));

        let start = (40u16, 12u16);
        app.drag = Some(DragState::on_down(DragTarget::Dialog, start.0, start.1));
        carry_dialog_drag(&mut app, start.0 + 4, start.1 + 3);
        assert_eq!(app.dialog_offset, (4, 3), "the box follows the pointer");

        // Off the right edge and back. The clamp caps the offset on the way
        // out; on the way back the box has to move immediately.
        carry_dialog_drag(&mut app, 79, start.1 + 3);
        let pinned = app.dialog_offset;
        let area = Rect::new(0, 0, app.term_size.0, app.term_size.1);
        let dialog = app.dialog.clone().expect("still open");
        assert_eq!(
            pinned,
            crate::render::clamp_dialog_offset(&dialog, area, (i16::MAX, 3)),
            "past the edge the offset is the clamp, not the travel"
        );
        carry_dialog_drag(&mut app, 78, start.1 + 3);
        assert_eq!(
            app.dialog_offset.0,
            pinned.0 - 1,
            "one cell back is one cell of movement"
        );

        // A fresh dialog opens where the eye expects it, not where the last
        // one was left.
        app.open_dialog(Dialog::confirm_close("%0"));
        assert_eq!(app.dialog_offset, (0, 0));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn escape_during_send_requires_explicit_abandon_before_dropping_the_key() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-compose-cancel");
        let mut app = test_app(one_pane_model(), home.clone());
        let old_attempt = dialog::ComposeAttempt {
            message: dialog::parse_compose("@reviewer ship it").expect("message"),
            client_key: "stable-key".into(),
        };
        app.dialog = Some(Dialog::Compose {
            buffer: "@reviewer ship it".into(),
            status: Some("sending to reviewer…".into()),
            send: dialog::ComposeSendState::Sending(old_attempt.clone()),
        });

        assert!(handle_compose_confirm(&mut app));
        assert!(matches!(
            app.dialog.as_ref(),
            Some(Dialog::Compose {
                send: dialog::ComposeSendState::Sending(_),
                ..
            })
        ));

        dialog_cancel(&mut app);

        let Some(Dialog::Compose { buffer, send, .. }) = app.dialog.as_ref() else {
            panic!("Esc must not discard the composer");
        };
        assert_eq!(buffer, "@reviewer ship it");
        assert!(send.is_confirming_abandon());
        assert_eq!(
            send.attempt().map(|attempt| attempt.client_key.as_str()),
            Some("stable-key")
        );

        dialog_cancel(&mut app);
        let Some(Dialog::Compose { buffer, send, .. }) = app.dialog.as_ref() else {
            panic!("cancelling abandon must restore the composer");
        };
        assert_eq!(buffer, "@reviewer ship it");
        assert!(send.is_sending());
        assert_eq!(
            send.attempt().map(|attempt| attempt.client_key.as_str()),
            Some("stable-key")
        );

        dialog_cancel(&mut app);
        assert!(handle_compose_confirm(&mut app));
        assert!(app.dialog.is_none(), "explicit abandon closes the composer");

        app.open_dialog(Dialog::Compose {
            buffer: "@reviewer ship it".into(),
            status: None,
            send: dialog::ComposeSendState::Ready,
        });
        finish_compose_send(
            app.dialog.as_mut(),
            old_attempt,
            crate::daemon::SendOutcome::Accepted("accepted m-old".into()),
        );
        let Some(Dialog::Compose {
            buffer,
            status,
            send,
        }) = app.dialog.as_ref()
        else {
            panic!("the reopened composer must remain open");
        };
        assert_eq!(buffer, "@reviewer ship it");
        assert!(status.is_none());
        assert_eq!(*send, dialog::ComposeSendState::Ready);
        let _ = std::fs::remove_dir_all(home);
    }

    /// Esc (or the Cancel button: both land in `dialog_cancel`) puts back
    /// the paint that was live when the theme picker opened.
    #[test]
    fn closing_the_picker_without_applying_restores_the_original_paint() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-picker-restore");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("themes")).expect("themes dir");
        std::fs::write(
            home.join("themes/solar.toml"),
            "name = \"solar\"\n[surface]\ndim = \"#222222\"\n",
        )
        .expect("write theme");
        let mut app = test_app(one_pane_model(), home.clone());
        let dim = cyclops_theme::tokens::SURFACE_DIM;
        let original = app.paint.theme.resolve(dim).rgb;
        // What ShowSettings does: keep the live paint beside the picker.
        app.theme_restore = Some(app.paint.theme.clone());
        app.dialog = Some(Dialog::Settings {
            section: dialog::SettingsSection::Theme,
            themes: dialog::ThemePicker {
                names: vec!["solar".into()],
                selected: 0,
                active: None,
                notice: None,
            },
            view: dialog::ViewSwitches::new(true, true),
            sound: dialog::SoundPicker::new(false, vec!["system".into()], "system"),
            delivery: dialog::ForceSubmitPicker::new(false, 5),
        });
        exec::preview_selected_theme(&mut app);
        assert_ne!(app.paint.theme.resolve(dim).rgb, original, "previewed");

        dialog_cancel(&mut app);

        assert_eq!(
            app.paint.theme.resolve(dim).rgb,
            original,
            "cancel restores the paint the picker opened over"
        );
        assert!(app.dialog.is_none());
        assert!(
            app.theme_restore.is_none(),
            "the watch owns the paint again"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The interleaving trap: the ThemeWatch refresh runs on the render
    /// deadline, BEFORE draw, so a theme change landing while the picker
    /// previews would repaint over the preview. While the picker is open
    /// the refresh is skipped whole; the stamps it never polled are still
    /// pending changes, so the first refresh after close adopts them and
    /// the watch owns the paint again.
    #[test]
    fn a_watch_refresh_never_overwrites_an_open_preview() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-preview-watch");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("themes")).expect("themes dir");
        std::fs::write(
            home.join("themes/dark.toml"),
            "[surface]\ndim = \"#111111\"\n",
        )
        .expect("write dark");
        std::fs::write(
            home.join("themes/solar.toml"),
            "[surface]\ndim = \"#222222\"\n",
        )
        .expect("write solar");
        std::fs::write(home.join("config.toml"), "theme = \"dark\"\n").expect("config");
        let mut watch = cyclops_theme::ThemeWatch::with_env(None, &home);
        let mut app = test_app(one_pane_model(), home.clone());
        app.paint.theme = watch.theme().clone();
        let dim = cyclops_theme::tokens::SURFACE_DIM;

        app.theme_restore = Some(app.paint.theme.clone());
        app.dialog = Some(Dialog::Settings {
            section: dialog::SettingsSection::Theme,
            themes: dialog::ThemePicker {
                names: vec!["dark".into(), "solar".into()],
                selected: 1,
                active: Some(0),
                notice: None,
            },
            view: dialog::ViewSwitches::new(true, true),
            sound: dialog::SoundPicker::new(false, vec!["system".into()], "system"),
            delivery: dialog::ForceSubmitPicker::new(false, 5),
        });
        exec::preview_selected_theme(&mut app);
        assert_eq!(app.paint.theme.resolve(dim).rgb, (0x22, 0x22, 0x22));

        // The watched file moves mid-browse (longer, so the stamp moves
        // regardless of mtime granularity). The deadline refresh must not
        // repaint.
        std::fs::write(
            home.join("themes/dark.toml"),
            "name = \"dark\"\n[surface]\ndim = \"#333333\"\n",
        )
        .expect("edit dark");
        refresh_theme_watch(&mut app, &mut watch);
        assert_eq!(
            app.paint.theme.resolve(dim).rgb,
            (0x22, 0x22, 0x22),
            "the preview survives the refresh"
        );

        // Close without applying: the original comes back first, then the
        // watch resumes ownership and adopts the edit it was held off from.
        dialog_cancel(&mut app);
        assert_eq!(
            app.paint.theme.resolve(dim).rgb,
            (0x11, 0x11, 0x11),
            "Esc restores what was live at open"
        );
        refresh_theme_watch(&mut app, &mut watch);
        assert_eq!(
            app.paint.theme.resolve(dim).rgb,
            (0x33, 0x33, 0x33),
            "the first refresh after close adopts what happened while browsing"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn output_coalescing_preserves_global_pane_order() {
        let mut output = Vec::new();
        push_output(&mut output, "%0".into(), b"ab".to_vec());
        push_output(&mut output, "%1".into(), b"x".to_vec());
        push_output(&mut output, "%0".into(), b"cd".to_vec());

        assert_eq!(
            output,
            vec![
                ("%0".to_string(), b"ab".to_vec()),
                ("%1".to_string(), b"x".to_vec()),
                ("%0".to_string(), b"cd".to_vec())
            ]
        );
    }

    #[tokio::test]
    async fn oversized_output_is_split_without_changing_its_bytes() {
        let (notification_tx, notification_rx) = mpsc::channel(1);
        let (tmux_tx, mut tmux_rx) = mpsc::channel(3);
        let (continuity_tx, _continuity_rx) = mpsc::channel(1);
        spawn_notif_forwarder(
            NotificationReceiver::from_bounded(notification_rx),
            tmux_tx,
            continuity_tx,
        );

        let bytes = vec![b'x'; OUTPUT_BATCH_MAX_BYTES + 17];
        notification_tx
            .send(Notification::Output {
                pane: "%1".into(),
                data: bytes.clone(),
            })
            .await
            .unwrap();
        drop(notification_tx);

        let mut landed = Vec::new();
        for expected_len in [OUTPUT_BATCH_MAX_BYTES, 17] {
            let Some(AppMsg::OutputBatch(batch)) = tmux_rx.recv().await else {
                panic!("output forwarder ended before both chunks landed");
            };
            assert_eq!(batch.len(), 1);
            assert_eq!(batch[0].0, "%1");
            assert_eq!(batch[0].1.len(), expected_len);
            landed.extend_from_slice(&batch[0].1);
        }
        assert_eq!(landed, bytes);
    }

    #[tokio::test]
    async fn a_closed_notification_stream_requests_reconnect() {
        let (notification_tx, notification_rx) = mpsc::channel(1);
        let (tmux_tx, mut tmux_rx) = mpsc::channel(1);
        let (continuity_tx, _continuity_rx) = mpsc::channel(1);
        spawn_notif_forwarder(
            NotificationReceiver::from_bounded(notification_rx),
            tmux_tx,
            continuity_tx,
        );

        drop(notification_tx);
        assert!(matches!(tmux_rx.recv().await, Some(AppMsg::LinkLost)));
    }

    #[tokio::test]
    async fn a_control_gap_blocks_the_suffix_until_authoritative_resync() {
        let (notification_tx, notification_rx) = mpsc::channel(2);
        let (tmux_tx, mut tmux_rx) = mpsc::channel(1);
        let (continuity_tx, mut continuity_rx) = mpsc::channel(1);
        spawn_notif_forwarder(
            NotificationReceiver::from_bounded(notification_rx),
            tmux_tx,
            continuity_tx,
        );

        notification_tx
            .send(Notification::ContinuityLost)
            .await
            .expect("notification channel stays open");
        notification_tx
            .send(Notification::Output {
                pane: "%0".into(),
                data: b"stale suffix".to_vec(),
            })
            .await
            .expect("suffix queues behind the marker");

        let barrier = continuity_rx.recv().await.expect("barrier reaches app");
        assert!(
            tmux_rx.try_recv().is_err(),
            "barrier bypasses ordinary lane"
        );
        barrier
            .repair
            .send(true)
            .expect("app confirms authoritative snapshot");
        assert!(
            barrier.cutover.await.expect("source answers cutover"),
            "no event crossed the snapshot boundary"
        );
        tokio::task::yield_now().await;
        assert!(tmux_rx.try_recv().is_err(), "stale suffix is discarded");
    }

    #[tokio::test]
    async fn a_full_app_hop_awaits_capacity_deterministically_and_preserves_order() {
        let (tmux_tx, mut tmux_rx) = mpsc::channel(1);
        tmux_tx
            .try_send(AppMsg::OutputBatch(vec![("%0".into(), b"first".to_vec())]))
            .expect("fill the cap-1 app queue");

        let second_bytes = b"second".to_vec();
        let send_fut = forward_notification_message(
            &tmux_tx,
            AppMsg::OutputBatch(vec![("%0".into(), second_bytes.clone())]),
        );
        tokio::pin!(send_fut);

        // Biased select proves send_fut is Pending while queue is full.
        tokio::select! {
            biased;
            _ = &mut send_fut => panic!("send_fut completed while app queue was full"),
            _ = tokio::task::yield_now() => {}
        }

        // Drain the first message from the cap-1 app queue.
        let Some(AppMsg::OutputBatch(first)) = tmux_rx.recv().await else {
            panic!("expected OutputBatch for first message");
        };
        assert_eq!(first, vec![("%0".into(), b"first".to_vec())]);

        // Awaiting future succeeds now that capacity opened.
        assert!(
            send_fut.await,
            "send unblocks and succeeds once capacity opens"
        );

        // Second message arrives byte-identical with order preserved.
        let Some(AppMsg::OutputBatch(second)) = tmux_rx.recv().await else {
            panic!("expected OutputBatch for second message");
        };
        assert_eq!(second, vec![("%0".into(), second_bytes)]);
    }

    #[tokio::test]
    async fn structural_notifications_do_not_overtake_pane_output() {
        let (notification_tx, notification_rx) = mpsc::channel(3);
        let (tmux_tx, mut tmux_rx) = mpsc::channel(4);
        let (continuity_tx, _continuity_rx) = mpsc::channel(1);
        spawn_notif_forwarder(
            NotificationReceiver::from_bounded(notification_rx),
            tmux_tx,
            continuity_tx,
        );
        notification_tx
            .send(Notification::Output {
                pane: "%0".into(),
                data: b"before".to_vec(),
            })
            .await
            .unwrap();
        notification_tx
            .send(Notification::LayoutChange {
                window: "@0".into(),
                rest: "layout".into(),
            })
            .await
            .unwrap();
        notification_tx
            .send(Notification::Output {
                pane: "%0".into(),
                data: b"after".to_vec(),
            })
            .await
            .unwrap();
        drop(notification_tx);

        assert!(matches!(tmux_rx.recv().await, Some(AppMsg::OutputBatch(_))));
        assert!(matches!(
            tmux_rx.recv().await,
            Some(AppMsg::LayoutChanged { .. })
        ));
        assert!(matches!(tmux_rx.recv().await, Some(AppMsg::OutputBatch(_))));
    }

    #[test]
    fn background_session_rename_does_not_switch_the_attached_session() {
        assert!(!rename_targets_active_session(Some("$0"), Some("$1")));
        assert!(rename_targets_active_session(Some("$0"), Some("$0")));
        assert!(rename_targets_active_session(Some("$0"), None));
    }

    #[test]
    fn reconnect_uses_the_workspace_selected_after_boot() {
        let boot = ControlConfig::attach("alpha").on_socket("test-socket");
        let reconnect = reconnect_config(&boot, "beta");

        assert_eq!(reconnect.session, "beta");
        assert_eq!(reconnect.socket_name, boot.socket_name);
    }

    /// Boot sizing and the first frame must derive from the same visible
    /// chrome for every persisted visibility combination.
    #[test]
    fn boot_declares_the_geometry_the_first_frame_paints_for_every_chrome_combination() {
        let area = Rect::new(0, 0, 200, 50);
        for sidebar_visible in [true, false] {
            for tab_bar_visible in [true, false] {
                let mut closed_size: Option<(u16, u16)> = None;
                for messages_visible in [false, true] {
                    let prefs = WorkspacePrefs {
                        sidebar_visible,
                        tab_bar_visible,
                        messages_visible,
                        ..WorkspacePrefs::default()
                    };
                    let what = format!(
                        "sidebar {sidebar_visible}, tab bar {tab_bar_visible}, \
                         Messages {messages_visible}"
                    );

                    // What `run_async` does before it declares: persisted
                    // visibility lands on the model, then boot sizes from it.
                    let mut model = one_pane_model();
                    model.sidebar_visible = prefs.sidebar_visible;
                    model.messages_visible = prefs.messages_visible;
                    let boot = chrome_for(area, &model, &prefs);
                    let declared = desired_tmux_size(area, &model, &prefs);
                    if messages_visible {
                        let closed = closed_size.expect("closed size measured first");
                        assert!(
                            declared.0 < closed.0,
                            "{what}: open Messages did not narrow the visible tmux target"
                        );
                    } else {
                        closed_size = Some(declared);
                    }

                    let mut app = test_app(
                        model,
                        cyclops_proto::scratch::scratch_dir("app-boot-chrome"),
                    );
                    app.prefs = prefs;
                    let painted = app.chrome(area);

                    assert_eq!(painted, boot, "{what}: the first frame moved the chrome");
                    assert_eq!(
                        desired_tmux_size(area, &app.model, &app.prefs),
                        declared,
                        "{what}: boot and runtime sizing geometry drifted"
                    );

                    // And each flag really does change paint geometry, or
                    // the agreement above would be agreement about nothing.
                    let expected_left = if sidebar_visible {
                        crate::render::SIDEBAR_DEFAULT_WIDTH
                    } else {
                        1
                    };
                    assert_eq!(painted.canvas.x, expected_left, "{what}: canvas left edge");
                    assert_eq!(
                        painted.sidebar.is_some(),
                        sidebar_visible,
                        "{what}: sidebar"
                    );
                    assert_eq!(
                        painted.rail.is_some(),
                        !sidebar_visible,
                        "{what}: collapsing leaves a rail, never nothing"
                    );
                    assert_eq!(
                        painted.messages.is_some(),
                        messages_visible,
                        "{what}: Messages"
                    );
                    assert_eq!(
                        painted.messages_rail.is_some(),
                        !messages_visible,
                        "{what}: closed Messages leaves its rail"
                    );
                    assert_eq!(
                        painted.tab_bar.height,
                        u16::from(tab_bar_visible),
                        "{what}: the strip's row"
                    );
                    assert_eq!(
                        painted.canvas.y,
                        u16::from(tab_bar_visible),
                        "{what}: the canvas takes the row the strip does not"
                    );
                }
            }
        }
    }

    /// A copy says what it took, and says it for a while without anyone
    /// touching a key. The text comes from a real pane runtime through the
    /// real extraction path; only the clipboard write itself is left out,
    /// so a test never puts its fixture on the machine's real clipboard.
    #[test]
    fn a_copy_announces_what_it_took_and_the_notice_expires_on_its_own() {
        let mut app = test_app(
            one_pane_model(),
            cyclops_proto::scratch::scratch_dir("app-copy-notice"),
        );
        let mut runtime = crate::runtime::PaneRuntime::new(20, 3);
        runtime.feed(
            b"cargo test
",
        );
        app.runtimes.insert("%0".into(), runtime);
        if let Some(rt) = app.runtimes.get_mut("%0") {
            rt.anchor_selection(
                crate::runtime::CellPos { col: 0, row: 0 },
                crate::runtime::CellPos { col: 19, row: 0 },
            );
        }
        app.selection.set_active("%0".into());

        let text = selection_text(&mut app, "%0").expect("the row has text");
        assert_eq!(text.trim_end(), "cargo test");
        announce_copy(&mut app, &text);

        assert_eq!(
            app.notice.text(),
            Some(copy::copied(&text).as_str()),
            "the notice has to name what landed, not just that something did"
        );
        assert!(
            app.notice.text().is_some_and(|said| said.contains("10")),
            "and the count is the selection's own: {:?}",
            app.notice.text()
        );

        // It goes away on its own deadline: no keypress, no timer of its
        // own, just the instant the loop already wakes for.
        let due = app.notice.deadline().expect("a live notice arms a wakeup");
        assert!(!app.notice.expire(due - Duration::from_millis(1)));
        assert!(app.notice.expire(due), "the deadline clears it");
        assert_eq!(app.notice.text(), None);
        assert_eq!(app.notice.deadline(), None, "and stops waking the loop");
    }

    /// An empty pick copies nothing, so it must not claim to have copied
    /// anything: no notice, no wakeup.
    #[test]
    fn an_empty_selection_says_nothing() {
        let mut app = test_app(
            one_pane_model(),
            cyclops_proto::scratch::scratch_dir("app-copy-notice-empty"),
        );
        app.runtimes
            .insert("%0".into(), crate::runtime::PaneRuntime::new(20, 3));
        if let Some(rt) = app.runtimes.get_mut("%0") {
            rt.anchor_selection(
                crate::runtime::CellPos { col: 0, row: 0 },
                crate::runtime::CellPos { col: 19, row: 0 },
            );
        }
        app.selection.set_active("%0".into());

        assert_eq!(selection_text(&mut app, "%0"), None);
        copy_selection(&mut app, "%0");
        assert_eq!(app.notice.text(), None);
        assert_eq!(app.notice.deadline(), None);
    }

    #[test]
    fn workspace_disclosure_click_toggles_both_directions() {
        let mut expanded = HashSet::new();
        assert!(toggle_workspace_expanded(&mut expanded, "$0".into()));
        assert!(expanded.contains("$0"));
        assert!(!toggle_workspace_expanded(&mut expanded, "$0".into()));
        assert!(!expanded.contains("$0"));
    }

    // -- Cancelling a workspace-row drag (Escape, or any other
    // `cancel_drag` path) must leave the model order and prefs exactly as
    // they were: `cancel_drag` only ever restores the sidebar WIDTH for a
    // `DragTarget::Sidebar` drag, and a `Workspace` drag never touched the
    // model to begin with — nothing here is undone because nothing was
    // ever applied while the drag was live. --
    #[test]
    fn cancelling_a_workspace_reorder_drag_leaves_order_and_prefs_untouched() {
        let row = |id: &str, name: &str| crate::model::WorkspaceRow {
            session_id: id.into(),
            name: name.into(),
            tab_count: 1,
            window_ids: Vec::new(),
        };
        let tab = crate::model::TabModel {
            window_id: "@0".into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: "%0".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%0".into(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
            minimization_provenance: std::collections::HashMap::new(),
        };
        let model = WorkspaceModel {
            workspaces: vec![row("$a", "a"), row("$b", "b"), row("$c", "c")],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "a".into(),
                tabs: vec![tab],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        };
        let mut app = test_app(
            model,
            cyclops_proto::scratch::scratch_dir("cancel-workspace-drag"),
        );
        let orders_before = (
            app.model
                .workspaces
                .iter()
                .map(|w| w.session_id.clone())
                .collect::<Vec<_>>(),
            app.prefs.workspace_order.clone(),
        );
        let mut drag = DragState::on_down(
            DragTarget::Workspace {
                session_id: "$c".into(),
                session: "c".into(),
            },
            5,
            5,
        );
        drag.on_move(5, 3);
        assert!(drag.is_active(), "past the 1-cell sidebar row threshold");
        app.drag = Some(drag);

        cancel_drag(&mut app);

        assert!(app.drag.is_none(), "cancel always clears the drag");
        assert_eq!(
            (
                app.model
                    .workspaces
                    .iter()
                    .map(|w| w.session_id.clone())
                    .collect::<Vec<_>>(),
                app.prefs.workspace_order.clone(),
            ),
            orders_before,
            "cancelling must not reorder the model or touch prefs"
        );
    }

    #[test]
    fn reconciled_model_follows_tmuxs_new_active_window() {
        let tab = |id: &str, pane: &str| crate::model::TabModel {
            window_id: id.into(),
            name: id.into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: pane.into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: pane.into(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
            minimization_provenance: std::collections::HashMap::new(),
        };
        let mut current = WorkspaceModel {
            workspaces: vec![],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "alpha".into(),
                tabs: vec![tab("@1", "%1")],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        };
        let fresh = WorkspaceModel {
            workspaces: vec![],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "alpha".into(),
                tabs: vec![tab("@1", "%1"), tab("@2", "%2")],
                active_tab: 1,
            },
            sidebar_visible: true,
            messages_visible: false,
        };

        install_reconciled_model(&mut current, fresh, false, false);

        assert_eq!(current.active_tab().window_id, "@2");
        assert!(!current.sidebar_visible);
        assert!(!current.messages_visible);
    }

    #[test]
    fn persisted_workspace_order_keeps_the_active_identity() {
        let tab = crate::model::TabModel {
            window_id: "@1".into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: "%1".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%1".into(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
            minimization_provenance: std::collections::HashMap::new(),
        };
        let row = |id: &str, name: &str| crate::model::WorkspaceRow {
            session_id: id.into(),
            name: name.into(),
            tab_count: 1,
            window_ids: vec!["@1".into()],
        };
        let mut model = WorkspaceModel {
            workspaces: vec![row("$0", "alpha"), row("$1", "beta")],
            active_workspace: 1,
            session: crate::model::SessionModel {
                session: "beta".into(),
                tabs: vec![tab],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        };

        apply_workspace_order(&mut model, &["beta".into(), "alpha".into()]);
        assert_eq!(model.workspaces[0].name, "beta");
        assert_eq!(model.active_workspace, 0);
        assert_eq!(model.workspaces[model.active_workspace].session_id, "$1");
    }

    #[test]
    fn switching_workspaces_opens_the_new_one_but_respects_a_manual_collapse() {
        let row = |id: &str| crate::model::WorkspaceRow {
            session_id: id.into(),
            name: id.into(),
            tab_count: 1,
            window_ids: vec!["@1".into()],
        };
        let rows = vec![row("$0"), row("$1")];
        let mut expanded_for = None;
        let mut expanded = HashSet::new();

        expand_active_workspace(&rows, 0, &mut expanded_for, &mut expanded);
        assert!(expanded.contains("$0"), "the active workspace opens");

        // Collapsing the row you are looking at has to stick, so a reconcile
        // that does not change the active workspace must not reopen it.
        toggle_workspace_expanded(&mut expanded, "$0".into());
        expand_active_workspace(&rows, 0, &mut expanded_for, &mut expanded);
        assert!(!expanded.contains("$0"), "a manual collapse survives");

        // Switching is a change, and the workspace you switch to has to show
        // its agents without a second click.
        expand_active_workspace(&rows, 1, &mut expanded_for, &mut expanded);
        assert!(expanded.contains("$1"), "the workspace switched to opens");
    }

    // -- `resolve_workspace_slot_drop`: the drop must match exactly what
    // the live rule would have previewed at the same point, computed the
    // same way (`crate::drag::slot_for_row` + `insertion_for_slot`), never
    // re-derived from whatever hit target happens to sit under the
    // release. --

    fn sidebar_rows(rows: &[(&str, &str, u16)]) -> HitMap {
        let mut hits = HitMap::default();
        for (session_id, session, y) in rows {
            hits.push(
                Rect::new(2, *y, 18, 1),
                HitTarget::SidebarRow {
                    session_id: session_id.to_string(),
                    session: session.to_string(),
                },
            );
        }
        hits
    }

    #[test]
    fn a_valid_drop_resolves_the_previewed_insertion() {
        let hits = sidebar_rows(&[("$a", "a", 3), ("$b", "b", 4), ("$c", "c", 5)]);
        let sidebar = Some(Rect::new(0, 0, 22, 10));

        // $c, picked up and released on $a's row, previews "before $a" —
        // the same slot a pointer at row 3 would have shown all along.
        assert_eq!(
            resolve_workspace_slot_drop(&hits, sidebar, "$c", 5, 3),
            Some(action::Action::ReorderWorkspace {
                session_id: "$c".into(),
                insertion: action::Insertion::Before("$a".into()),
            })
        );
    }

    #[test]
    fn a_release_outside_the_sidebar_resolves_nothing() {
        let hits = sidebar_rows(&[("$a", "a", 3), ("$b", "b", 4)]);
        let sidebar = Some(Rect::new(0, 0, 22, 10));

        // Column 30 is past the sidebar's right edge: released over the
        // pane canvas, not the sidebar, even though the row lines up with
        // a workspace row.
        assert_eq!(
            resolve_workspace_slot_drop(&hits, sidebar, "$a", 30, 3),
            None
        );
        // No sidebar painted at all (hidden).
        assert_eq!(resolve_workspace_slot_drop(&hits, None, "$a", 5, 3), None);
    }

    #[test]
    fn a_drop_that_does_not_move_the_row_resolves_nothing() {
        let hits = sidebar_rows(&[("$a", "a", 3), ("$b", "b", 4)]);
        let sidebar = Some(Rect::new(0, 0, 22, 10));

        // Releasing $a back on its own row previews "before $a" — its own
        // position, not a move.
        assert_eq!(
            resolve_workspace_slot_drop(&hits, sidebar, "$a", 5, 3),
            None
        );
    }

    #[test]
    fn a_stale_drop_against_a_vanished_workspace_resolves_nothing() {
        let hits = sidebar_rows(&[("$a", "a", 3), ("$b", "b", 4)]);
        let sidebar = Some(Rect::new(0, 0, 22, 10));

        assert_eq!(
            resolve_workspace_slot_drop(&hits, sidebar, "$gone", 5, 3),
            None
        );
    }

    // -- End to end through `commit_drag_drop`: a valid drop dispatches
    // exactly one `ReorderWorkspace` and persists exactly once. Executor
    // tests (`app::exec::tests`) already cover what the dispatched action
    // itself does; this proves the resolution-to-dispatch wiring. --

    #[tokio::test]
    async fn a_valid_workspace_drop_dispatches_one_reorder_and_persists_once() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-workspace-drop");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");

        let home = cyclops_proto::scratch::scratch_dir("app-workspace-drop-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");

        let row = |id: &str, name: &str| crate::model::WorkspaceRow {
            session_id: id.into(),
            name: name.into(),
            tab_count: 1,
            window_ids: Vec::new(),
        };
        let tab = crate::model::TabModel {
            window_id: "@0".into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: "%0".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%0".into(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
            minimization_provenance: std::collections::HashMap::new(),
        };
        let model = WorkspaceModel {
            workspaces: vec![row("$a", "a"), row("$b", "b"), row("$c", "c")],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "a".into(),
                tabs: vec![tab],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        };
        let mut app = test_app(model, home.clone());

        // Fixture rects standing in for "the last frame actually painted" —
        // narrower than proving `paint_sidebar` itself (that's render.rs's
        // job), just enough to exercise `commit_drag_drop`'s wiring.
        let sidebar = app
            .chrome(Rect::new(0, 0, app.term_size.0, app.term_size.1))
            .sidebar
            .expect("sidebar visible");
        app.hit_map.push(
            Rect::new(sidebar.x + 2, 3, 10, 1),
            HitTarget::SidebarRow {
                session_id: "$a".into(),
                session: "a".into(),
            },
        );
        app.hit_map.push(
            Rect::new(sidebar.x + 2, 4, 10, 1),
            HitTarget::SidebarRow {
                session_id: "$b".into(),
                session: "b".into(),
            },
        );
        app.hit_map.push(
            Rect::new(sidebar.x + 2, 5, 10, 1),
            HitTarget::SidebarRow {
                session_id: "$c".into(),
                session: "c".into(),
            },
        );
        assert!(
            sidebar.contains(ratatui::layout::Position::from((sidebar.x + 2, 3))),
            "fixture rows must sit inside the real sidebar rect"
        );

        // $c is picked up and released on $a's row.
        commit_drag_drop(
            &mut app,
            &client,
            &DragTarget::Workspace {
                session_id: "$c".into(),
                session: "c".into(),
            },
            sidebar.x + 2,
            3,
        )
        .await
        .expect("commit");

        assert_eq!(
            app.model
                .workspaces
                .iter()
                .map(|w| w.session_id.clone())
                .collect::<Vec<_>>(),
            vec!["$c".to_string(), "$a".to_string(), "$b".to_string()],
        );
        assert_eq!(
            app.prefs.workspace_order,
            vec!["c".to_string(), "a".to_string(), "b".to_string()]
        );
        let reloaded = crate::persist::load_prefs(&home);
        assert_eq!(
            reloaded.workspace_order, app.prefs.workspace_order,
            "the promised persist must have actually round-tripped"
        );

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    // -- A pane-grip drag released on another pane dispatches one swap
    // through the executor. What the swap itself does to tmux is
    // `app::exec::tests`' job; this proves the resolution-to-dispatch
    // wiring against the painted hit rects. --

    #[tokio::test]
    async fn a_pane_drop_on_another_pane_dispatches_one_swap() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-pane-drop");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        server.run_ok(&["split-window", "-h", "-t", "s"]);
        let out = server.run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_left}"]);
        let before: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut fields = line.split_whitespace();
                (
                    fields.next().unwrap_or_default().to_string(),
                    fields.next().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let (dragged, target) = (before[0].0.clone(), before[1].0.clone());
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");

        let model = WorkspaceModel {
            workspaces: vec![crate::model::WorkspaceRow {
                session_id: "$0".into(),
                name: "s".into(),
                tab_count: 1,
                window_ids: vec!["@0".into()],
            }],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "s".into(),
                tabs: vec![crate::model::TabModel {
                    window_id: "@0".into(),
                    name: "1".into(),
                    layout: crate::layout::ResolvedLayout::Leaf {
                        pane_id: dragged.clone(),
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    active_pane: dragged.clone(),
                    zoomed: false,
                    minimized: std::collections::HashMap::new(),
                    minimization_provenance: std::collections::HashMap::new(),
                }],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        };
        let mut app = test_app(
            model,
            cyclops_proto::scratch::scratch_dir("app-pane-drop-home"),
        );

        // Fixture rects standing in for the last painted frame: the dragged
        // pane on the left, the drop target's body on the right.
        app.hit_map.push(
            Rect::new(0, 2, 10, 5),
            HitTarget::PaneBody {
                pane_id: dragged.clone(),
            },
        );
        app.hit_map.push(
            Rect::new(12, 2, 10, 5),
            HitTarget::PaneBody {
                pane_id: target.clone(),
            },
        );

        commit_drag_drop(
            &mut app,
            &client,
            &DragTarget::Pane {
                pane_id: dragged.clone(),
            },
            13,
            3,
        )
        .await
        .expect("commit");

        assert!(app.needs_reconcile, "a swap is structural");
        let out = server.run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_left}"]);
        let after = String::from_utf8_lossy(&out.stdout);
        assert!(
            after
                .lines()
                .any(|line| line == format!("{dragged} {}", before[1].1)),
            "the dragged pane must take the target's slot, got {after}"
        );
        assert!(
            after
                .lines()
                .any(|line| line == format!("{target} {}", before[0].1)),
            "the target pane must take the dragged pane's slot, got {after}"
        );
        client.shutdown().await;
    }

    // -- The corner grip owns the swap pickup; every other frame cell is a
    // focus click; the stacked seam resizes. The hit map comes from a real
    // `paint_window` pass so these tests press the exact cells a user
    // sees. --

    /// A stacked two-pane app (top over bottom, the bottom labeled like an
    /// adopted agent pane) whose hit map the real render pass painted at
    /// the 40x12 test terminal size: pane rects (1,1,38,4) and (1,7,38,3),
    /// seam rows 5 and 6 with the bottom pane's title strip on row 6, and
    /// corner grips at (39,5) and (39,10).
    fn stacked_app_with_painted_hits(top: &str, bottom: &str, home: std::path::PathBuf) -> App {
        use crate::decoration::PaneDecoration;

        let leaf = |pane_id: &str, y: u16, height: u16| crate::layout::ResolvedLayout::Leaf {
            pane_id: pane_id.into(),
            x: 0,
            y,
            width: 38,
            height,
        };
        let model = WorkspaceModel {
            workspaces: vec![crate::model::WorkspaceRow {
                session_id: "$0".into(),
                name: "s".into(),
                tab_count: 1,
                window_ids: vec!["@0".into()],
            }],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "s".into(),
                tabs: vec![crate::model::TabModel {
                    window_id: "@0".into(),
                    name: "1".into(),
                    layout: crate::layout::ResolvedLayout::Split {
                        dir: SplitDir::Vertical,
                        x: 0,
                        y: 0,
                        width: 38,
                        height: 8,
                        children: vec![leaf(top, 0, 4), leaf(bottom, 5, 3)],
                    },
                    active_pane: bottom.into(),
                    zoomed: false,
                    minimized: std::collections::HashMap::new(),
                    minimization_provenance: std::collections::HashMap::new(),
                }],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        };
        let mut app = test_app(model, home);
        app.sizing.owned.insert(
            "s".into(),
            OwnedSession {
                pinned: std::collections::BTreeSet::from(["@0".into()]),
                blocked: std::collections::BTreeSet::new(),
            },
        );
        app.decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        app.decoration.panes.insert(
            bottom.into(),
            PaneDecoration {
                pane_id: bottom.into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: None,
                manifest_display_name: None,
                state: cyclops_proto::AgentState::Idle,
                needs_attention: false,
            },
        );

        let tab = app.model.active_tab().clone();
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| {
            let mut ctx = crate::render::WindowPaintCtx {
                link: LinkState::Live,
                paused: &app.paused_panes,
                hits: &mut app.hit_map,
                decoration: &app.decoration,
                selection: None,
                drag: None,
                notice: None,
                minimized: &std::collections::HashMap::new(),
                cursor: None,
                motion: crate::animate::MotionFrame::none(),
            };
            paint_window(
                &tab,
                &app.runtimes,
                frame.area(),
                frame.buffer_mut(),
                &app.paint,
                &mut ctx,
            );
        })
        .unwrap();
        app
    }

    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    /// The drag-grip regression: a drag starting on the seam between a top
    /// and bottom pane must resize the panes and never swap them, even
    /// with the bottom pane labeled. This is the grab the operator
    /// reported as "just grabs and does not resize".
    #[tokio::test]
    async fn a_drag_from_the_stacked_seam_resizes_and_never_swaps() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-seam-resize");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        server.run_ok(&["split-window", "-v", "-t", "s"]);
        let heights = |server: &TmuxServer| -> Vec<(String, u16)> {
            String::from_utf8_lossy(
                &server
                    .run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_height}"])
                    .stdout,
            )
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((fields.next()?.to_string(), fields.next()?.parse().ok()?))
            })
            .collect()
        };
        let before = heights(&server);
        assert_eq!(before.len(), 2);
        let (top, bottom) = (before[0].0.clone(), before[1].0.clone());
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let id = client.client_identity().await.expect("identity");
        client
            .claim_window_driver("s", &id.marker())
            .await
            .expect("claim");
        let mut app = stacked_app_with_painted_hits(
            &top,
            &bottom,
            cyclops_proto::scratch::scratch_dir("app-seam-resize-home"),
        );
        app.sizing.identity = Some(id);
        let mut detached = false;

        // Row A of the seam, on the columns the title strip shadowed.
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Down(MouseButton::Left), 20, 5),
            &mut detached,
        )
        .await
        .expect("down");
        assert!(
            matches!(
                app.drag.as_ref().map(|drag| &drag.target),
                Some(DragTarget::Divider {
                    dir: SplitDir::Vertical,
                    ..
                })
            ),
            "the seam must pick up a resize, never a pane"
        );
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Drag(MouseButton::Left), 20, 8),
            &mut detached,
        )
        .await
        .expect("drag");
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Up(MouseButton::Left), 20, 8),
            &mut detached,
        )
        .await
        .expect("up");

        assert!(app.drag.is_none());
        assert!(
            !app.needs_reconcile,
            "a resize applies live; a swap would have asked to reconcile"
        );
        let after = heights(&server);
        assert_eq!(
            after.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec![top.as_str(), bottom.as_str()],
            "no swap: both panes keep their slots"
        );
        assert_eq!(
            after[0].1,
            before[0].1 + 3,
            "the top pane must grow by the dragged rows"
        );
        client.shutdown().await;
    }

    /// The resize the operator reported as random: a divider drag whose
    /// release the workspace never saw (let go outside the terminal) used
    /// to finish on the next click anywhere, resizing by the whole distance
    /// from where the pointer had been to where that click landed. Bare
    /// motion proves the button is up and settles the drag; so does a fresh
    /// press. Either way the next click is only a click.
    #[tokio::test]
    async fn a_release_the_workspace_never_saw_is_settled_before_the_next_click() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-lost-release");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        server.run_ok(&["split-window", "-v", "-t", "s"]);
        let heights = |server: &TmuxServer| -> Vec<(String, u16)> {
            String::from_utf8_lossy(
                &server
                    .run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_height}"])
                    .stdout,
            )
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((fields.next()?.to_string(), fields.next()?.parse().ok()?))
            })
            .collect()
        };
        let before = heights(&server);
        assert_eq!(before.len(), 2);
        let (top, bottom) = (before[0].0.clone(), before[1].0.clone());
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let id = client.client_identity().await.expect("identity");
        client
            .claim_window_driver("s", &id.marker())
            .await
            .expect("claim");
        let mut app = stacked_app_with_painted_hits(
            &top,
            &bottom,
            cyclops_proto::scratch::scratch_dir("app-lost-release-home"),
        );
        app.sizing.identity = Some(id);
        let mut detached = false;
        let body_row = (0..12)
            .find(|&y| {
                matches!(
                    app.hit_map.hit(20, y),
                    Some(HitTarget::PaneBody { pane_id }) if *pane_id == bottom
                )
            })
            .expect("the bottom pane paints a body");

        // One resize, applied while the button was held, exactly as the
        // render debounce applies it.
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
        ] {
            let row = if matches!(kind, MouseEventKind::Down(_)) {
                5
            } else {
                8
            };
            handle_mouse(&mut app, &client, mouse_at(kind, 20, row), &mut detached)
                .await
                .expect("press and drag");
        }
        apply_live_divider(&mut app, &client)
            .await
            .expect("the held drag applies");
        let resized = heights(&server);
        assert_eq!(resized[0].1, before[0].1 + 3, "the drag resized once");

        // The button goes up somewhere this app never hears about. The
        // first bare motion is the proof.
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Moved, 20, 2),
            &mut detached,
        )
        .await
        .expect("motion");
        assert!(app.drag.is_none(), "bare motion settles the lost release");

        // A click on the other pane is only a click.
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            handle_mouse(
                &mut app,
                &client,
                mouse_at(kind, 20, body_row),
                &mut detached,
            )
            .await
            .expect("click");
        }
        assert_eq!(heights(&server), resized, "the click resized nothing");
        assert!(app.drag.is_none());

        // A terminal that reports no bare motion: the next press itself
        // is the proof, and still resizes nothing.
        // The hit map is the frame painted before the resize, so the seam
        // is still on row 5 there; tmux resolves the resize by pane, not
        // by row, so that is the seam to press.
        for (kind, row) in [
            (MouseEventKind::Down(MouseButton::Left), 5),
            (MouseEventKind::Drag(MouseButton::Left), 9),
        ] {
            handle_mouse(&mut app, &client, mouse_at(kind, 20, row), &mut detached)
                .await
                .expect("press and drag");
        }
        assert!(app.drag.is_some(), "a drag is in flight");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            handle_mouse(&mut app, &client, mouse_at(kind, 20, 2), &mut detached)
                .await
                .expect("click");
        }
        assert!(app.drag.is_none(), "the press settled the lost release");
        assert_eq!(
            heights(&server),
            resized,
            "the unapplied motion of a lost drag is never applied by a click"
        );
        client.shutdown().await;
    }

    /// The frame never picks a pane UP. The labeled title strip used to
    /// start the swap, so a click and a twitch on a pane's own title
    /// rearranged the workspace; the swap pickup is the corner grip now and
    /// nothing else.
    ///
    /// What the title strip may start is a resize, because the row it is
    /// painted on is also the seam between this pane and the one above, and
    /// the strip had taken every cell of it. That is a different drag: the
    /// panes keep their slots and only the boundary moves. A release that
    /// never moved is still the focus click it always was.
    #[tokio::test]
    async fn a_left_down_on_the_frame_focuses_without_picking_the_pane_up() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-frame-focus");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        server.run_ok(&["split-window", "-v", "-t", "s"]);
        let out = server.run(&["list-panes", "-t", "s", "-F", "#{pane_id}"]);
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        let (top, bottom) = (ids[0].clone(), ids[1].clone());
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let mut app = stacked_app_with_painted_hits(
            &top,
            &bottom,
            cyclops_proto::scratch::scratch_dir("app-frame-focus-home"),
        );
        let mut detached = false;

        // The labeled bottom pane's title strip: the old swap pickup, and
        // the seam this pane shares with the one above it.
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Down(MouseButton::Left), 20, 6),
            &mut detached,
        )
        .await
        .expect("down");
        match app.drag.as_ref().map(|drag| &drag.target) {
            Some(DragTarget::Divider { focus_on_click, .. }) => assert_eq!(
                focus_on_click.as_deref(),
                Some(bottom.as_str()),
                "a release that never moved has to focus the pane pressed"
            ),
            other => panic!("a frame cell must never pick a pane up, got {other:?}"),
        }
        // Released without moving: the seam stays put and the pane focuses.
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Up(MouseButton::Left), 20, 6),
            &mut detached,
        )
        .await
        .expect("up");
        assert!(app.drag.is_none());
        assert_eq!(app.model.active_tab().active_pane, bottom);

        // The top pane's own top border has no pane above it, so there is
        // no seam there and the press is a plain focus click.
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Down(MouseButton::Left), 20, 0),
            &mut detached,
        )
        .await
        .expect("down");
        assert!(app.drag.is_none());
        assert_eq!(app.model.active_tab().active_pane, top);
        let active = server.run(&["display-message", "-p", "-t", "s", "#{pane_id}"]);
        assert_eq!(String::from_utf8_lossy(&active.stdout).trim(), top);
        client.shutdown().await;
    }

    /// The one frame cell that still picks a pane up: a drag from the
    /// bottom-right corner grip dropped on another pane swaps the two,
    /// exactly as the frame drag used to.
    #[tokio::test]
    async fn a_grip_drag_dropped_on_another_pane_swaps_them() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-grip-swap");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        server.run_ok(&["split-window", "-v", "-t", "s"]);
        let positions = |server: &TmuxServer| -> Vec<(String, String)> {
            String::from_utf8_lossy(
                &server
                    .run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_top}"])
                    .stdout,
            )
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((fields.next()?.to_string(), fields.next()?.to_string()))
            })
            .collect()
        };
        let before = positions(&server);
        let (top, bottom) = (before[0].0.clone(), before[1].0.clone());
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let mut app = stacked_app_with_painted_hits(
            &top,
            &bottom,
            cyclops_proto::scratch::scratch_dir("app-grip-swap-home"),
        );
        let mut detached = false;

        // Down on the bottom pane's grip, drop on the top pane's body.
        // The grip sits in the control row on the pane's TOP border now,
        // beside [|] and [-], so its cell is found from the hit map rather
        // than written here.
        let (grip_x, grip_y) = (0..40u16)
            .flat_map(|x| (0..12u16).map(move |y| (x, y)))
            .find(|&(x, y)| {
                matches!(
                    app.hit_map.hit(x, y),
                    Some(HitTarget::PaneGrip { pane_id }) if pane_id == &bottom
                )
            })
            .expect("the bottom pane has a grip");
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Down(MouseButton::Left), grip_x, grip_y),
            &mut detached,
        )
        .await
        .expect("down");
        assert!(
            matches!(
                app.drag.as_ref().map(|drag| &drag.target),
                Some(DragTarget::Pane { pane_id }) if pane_id == &bottom
            ),
            "the grip must pick its pane up for a swap"
        );
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Drag(MouseButton::Left), 20, 3),
            &mut detached,
        )
        .await
        .expect("drag");
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Up(MouseButton::Left), 20, 3),
            &mut detached,
        )
        .await
        .expect("up");

        assert!(app.needs_reconcile, "a swap is structural");
        let after = positions(&server);
        assert!(
            after
                .iter()
                .any(|(id, at)| id == &bottom && at == &before[0].1),
            "the dragged pane must take the top slot, got {after:?}"
        );
        assert!(
            after
                .iter()
                .any(|(id, at)| id == &top && at == &before[1].1),
            "the top pane must take the dragged pane's slot, got {after:?}"
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn authoritative_owner_converges_when_tmux_window_resized_externally() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("owner-stale-cache-reconcile");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "120",
            "-y",
            "40",
            "/bin/sh",
        ]);

        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let home = cyclops_proto::scratch::scratch_dir("owner-stale-cache-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create home");

        let (mut model, window_id) = (one_pane_model(), "@0".to_string());
        model.session.session = "s".to_string();
        model.session.tabs[0].window_id = window_id.clone();
        model.sidebar_visible = false;
        model.messages_visible = false;

        let mut app = test_app(model, home.clone());
        app.term_size = (120, 40);
        app.prefs.sidebar_visible = false;
        app.prefs.messages_visible = false;

        // 1. Initial reconcile adopts window and declares size.
        reconcile(&mut app, &client)
            .await
            .expect("initial reconcile");
        let desired_size = app.declared_client_size.expect("declared size");
        assert_eq!(desired_size, (116, 37));

        // Verify tmux window is at target size.
        let out = server.run(&[
            "display",
            "-p",
            "-t",
            &window_id,
            "#{window_width}x#{window_height}",
        ]);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "116x37");

        // 2. External event resizes tmux window to 80x24 (unexplained slack introduced).
        server.run_ok(&["resize-window", "-t", &window_id, "-x", "80", "-y", "24"]);
        let out = server.run(&[
            "display",
            "-p",
            "-t",
            &window_id,
            "#{window_width}x#{window_height}",
        ]);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "80x24");

        // 3. Trigger reconcile as the authoritative owner.
        reconcile(&mut app, &client)
            .await
            .expect("recovery reconcile");

        // 4. Verification: The authoritative owner must converge both live tmux and in-memory model geometry.
        let out = server.run(&[
            "display",
            "-p",
            "-t",
            &window_id,
            "#{window_width}x#{window_height}",
        ]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "116x37",
            "authoritative owner failed to converge tmux window geometry after external resize"
        );
        let active_rect = app.model.active_tab().layout.rect();
        assert_eq!(
            (active_rect.width, active_rect.height),
            (116, 37),
            "in-memory app.model layout must reflect post-resize geometry immediately after single reconcile"
        );

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
    }
}
