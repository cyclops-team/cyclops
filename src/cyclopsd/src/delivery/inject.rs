//! Payload injection, tmux buffer spooling, Enter submission, and composer verification.

use super::*;

// ---------------------------------------------------------------------------
// Inject and verify
// ---------------------------------------------------------------------------

/// How payload bytes reach an agent and how the backend reads them back.
/// The gate, verify, and acknowledgment layers call through this seam only,
/// so a headless protocol
/// backend slots in per agent without touching them. [`TmuxInjector`] is
/// the terminal implementation. Errors are the short cause codes retry
/// accounting records.
pub(crate) trait Injector {
    /// Put the payload somewhere the pane can take it, WITHOUT the pane
    /// taking it.
    ///
    /// Separate from `commit` because spooling costs a control round
    /// trip, and any round trip is time a person can type in. Done here,
    /// that time is before the final proof rather than after it: the
    /// capture that admits the write is then the last thing to happen
    /// before the write. Spooling touches no pane and is freely
    /// retryable.
    async fn spool(&self, payload: &str) -> Result<(), String>;

    /// Hand the spooled payload to the pane's composer, without
    /// submitting.
    ///
    /// `on_write` runs immediately before the pane is asked to take it,
    /// which arms the conservative write boundary: everything before it is
    /// provably retryable, and everything from it onward may have left text
    /// in somebody's composer. Only a transport result proving that the
    /// command pipe accepted zero bytes can correct that boundary.
    ///
    /// It can FAIL, and then nothing is written. The barrier it installs
    /// is what stops the next delivery pasting over this one, so a paste
    /// that went ahead without it would create exactly the state the
    /// barrier exists to prevent, with nothing recording that it is
    /// there.
    async fn commit(
        &self,
        pane_id: &str,
        on_write: &(dyn Fn() -> Result<(), String> + Sync),
    ) -> Result<(), InjectFailure>;

    /// Drop a spooled payload the attempt is not going to write.
    async fn discard(&self);
    /// Press the submit key.
    async fn submit(&self, pane_id: &str, key: &str) -> Result<(), String>;
    /// Read one escaped snapshot with tmux physical wraps joined. Exact
    /// composer extraction compares these logical rows with the bytes that
    /// were spooled.
    async fn capture_joined_escaped(&self, pane_id: &str) -> Result<String, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InjectFailure {
    /// The tmux command pipe accepted no byte of the paste command.
    PasteCommandUnwritten,
    /// Every other refusal or ambiguous outcome keeps its existing cause.
    Other(String),
}

impl From<String> for InjectFailure {
    fn from(cause: String) -> Self {
        Self::Other(cause)
    }
}

pub(crate) fn classify_paste_buffer_failure(error: TmuxError) -> InjectFailure {
    match error {
        TmuxError::Io(_) => InjectFailure::PasteCommandUnwritten,
        _ => InjectFailure::Other("paste_failed".to_string()),
    }
}

/// The tmux paste path: load-buffer through the adapter's private spool
/// (0600 file under the 0700 cyclops home, never the shared temp dir) into
/// a per-delivery unique buffer, paste-buffer -p (bracketed when the app
/// opted in), and -d so the buffer does not linger server-global. Submit uses
/// send-keys.
pub(crate) struct TmuxInjector {
    pub(crate) client: Arc<ControlClient>,
    /// Per-delivery unique buffer name.
    pub(crate) buffer: String,
}

impl Injector for TmuxInjector {
    async fn spool(&self, payload: &str) -> Result<(), String> {
        if let Err(e) = self
            .client
            .load_buffer(&self.buffer, payload.as_bytes())
            .await
        {
            warn!(buffer = %self.buffer, error = %e, "load-buffer failed");
            // Loading the private spool buffer happens before tmux is asked
            // to write to the pane, regardless of how the load command
            // failed. It is therefore safe to retry under the bounded
            // pre-write budget. A paste-buffer failure stays ambiguous unless
            // the command pipe proves that it accepted zero command bytes.
            return Err("spool_failed".to_string());
        }
        Ok(())
    }

    async fn discard(&self) {
        let _ = self.client.delete_buffer(&self.buffer).await;
    }

    async fn commit(
        &self,
        pane_id: &str,
        on_write: &(dyn Fn() -> Result<(), String> + Sync),
    ) -> Result<(), InjectFailure> {
        // The write boundary. Spooling is behind us and provably touched
        // no pane; the next call may put text in somebody's composer. Every
        // outcome except an exact zero-byte command failure is ambiguous
        // about whether it did. Whatever this hook installs has to be
        // installed BEFORE the await, not after
        // it returns, or an outcome that leaves a payload behind can be
        // acted on by another delivery first.
        //
        // A hook that cannot install it stops the write. Nothing has been
        // pasted at this point, so refusing is the cheap direction: the
        // buffer is dropped and the delivery retries under the pre-write
        // budget.
        if let Err(cause) = on_write() {
            let _ = self.client.delete_buffer(&self.buffer).await;
            return Err(InjectFailure::Other(cause));
        }
        if let Err(e) = self
            .client
            .paste_buffer(&self.buffer, pane_id, true, true)
            .await
        {
            warn!(buffer = %self.buffer, error = %e, "paste-buffer failed");
            // paste-buffer -d never ran, so the loaded buffer would linger
            // server-global with the payload in it. Best effort: the buffer
            // dies with the server either way.
            let _ = self.client.delete_buffer(&self.buffer).await;
            return Err(classify_paste_buffer_failure(e));
        }
        Ok(())
    }

    async fn submit(&self, pane_id: &str, key: &str) -> Result<(), String> {
        self.client.send_keys(pane_id, &[key]).await.map_err(|e| {
            warn!(error = %e, "submit key failed");
            "submit_failed".to_string()
        })
    }

    async fn capture_joined_escaped(&self, pane_id: &str) -> Result<String, String> {
        self.client
            .capture_pane_joined_escaped(pane_id)
            .await
            .map_err(|e| e.to_string())
    }
}

/// The expected staged target to verify in the active composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagingTarget<'a> {
    /// Single-line payload matching exact expected terminal composer row.
    ExactRow(&'a str),
}

/// Result of extracting one active composer's visible payload.
///
/// `Hidden` is distinct from `Unprovable`: a collapsed chip is positive
/// evidence that bytes exist, but the screen cannot reveal which bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum ComposerContentProof {
    Visible(String),
    Hidden,
    Unsupported,
    Unprovable,
}

/// Paste the payload and verify the composer staged it. Returns the
/// composer-window snapshot the screen ACK tier compares against, plus
/// whether an id-carrying pattern proved the staging (feeds the tier-2
/// evidence rules).
///
/// Composer verification is the gate because bracketed-paste
/// degradation is not observable up front through tmux 3.6a.
pub(crate) async fn inject<I: Injector>(
    injector: &I,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    target: StagingTarget<'_>,
    expected_payload: &str,
    on_write: &(dyn Fn() -> Result<(), String> + Sync),
) -> Result<(String, bool, String), InjectFailure> {
    injector.commit(&handle.pane_id, on_write).await?;
    // The capture flavor follows the manifest's composer discriminators:
    // esc rules need the SGR-escaped grid or they fail closed, and a
    // composer that collapses a long paste into a chip hides the bytes.
    // That representation can identify a staged-input condition, but it
    // cannot prove exact Cyclops ownership or authorize Enter.
    let mut last_delay = 0;
    for delay in VERIFY_DELAYS_MS {
        if delay > last_delay {
            tokio::time::sleep(Duration::from_millis(delay - last_delay)).await;
        }
        last_delay = delay;
        let capture = injector.capture_joined_escaped(&handle.pane_id).await;
        match capture {
            Ok(screen) => {
                if let Some((id_staged, payload_proof)) =
                    exact_staging_proof(manifest, &screen, target, expected_payload)
                {
                    // The comparison window is de-escaped text either way,
                    // so SGR churn (a blink, a focus change) can never fake
                    // a "changed composer" for the ACK tier.
                    return Ok((
                        bottom_window(&strip_csi(&screen), COMPOSER_WINDOW),
                        id_staged,
                        payload_proof,
                    ));
                }
            }
            Err(e) => debug!(error = %e, "verify capture failed"),
        }
    }
    // Staging was unobservable. The doorbell proceeds to submit once and
    // finishes as delivered_unverified.
    let capture = injector
        .capture_joined_escaped(&handle.pane_id)
        .await
        .unwrap_or_default();
    Ok((
        bottom_window(&strip_csi(&capture), COMPOSER_WINDOW),
        false,
        String::new(),
    ))
}

/// What representation is visible in the active composer.
///
/// A visible target is still only structural evidence until the extracted
/// composer bytes match the expected payload. A collapsed chip proves only
/// that the vendor drew a chip. It cannot prove ownership or authorize Enter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StagedRepresentation {
    VisibleTarget,
    CollapsedChip,
}

pub(crate) fn staged_representation(
    manifest: &Manifest,
    screen: &str,
    target: StagingTarget<'_>,
) -> Option<StagedRepresentation> {
    let StagingTarget::ExactRow(expected_row) = target;
    if exact_row_verified(manifest, screen, expected_row) {
        return Some(StagedRepresentation::VisibleTarget);
    }
    match exact_composer_content_from_joined_capture(manifest, screen) {
        ComposerContentProof::Visible(content)
            if content.contains('\n')
                && visible_single_line_payload_matches(&content, expected_row) =>
        {
            Some(StagedRepresentation::VisibleTarget)
        }
        ComposerContentProof::Hidden => Some(StagedRepresentation::CollapsedChip),
        ComposerContentProof::Visible(_)
        | ComposerContentProof::Unsupported
        | ComposerContentProof::Unprovable => None,
    }
}

/// The proven staged rows, when the sentinel path validates: every visible
/// row through the unique exact sentinel, and nothing after it.
///
/// The boundary comes from the proof rather than from pattern matching,
/// which matters because a payload row can read exactly like chrome. If
/// rows were dropped merely for looking like a status row, a human could
/// edit one and the comparison would never see it.
/// Remove the terminal's own right padding, and nothing else.
///
/// `capture-pane` pads every row out to the pane width with ASCII spaces,
/// and that padding is the one trailing thing on a row that is not
/// composer content. Rust's `trim_end` removes tabs, non-breaking spaces
/// and every other Unicode space as well, each of which a person can put
/// in a composer, so using it before an exact comparison would let a
/// sentinel followed by a tab read as exact.
pub(crate) fn unpad(row: &str) -> &str {
    row.trim_end_matches(' ')
}

/// The screen as physical rows, in order, with the terminal's own right
/// padding removed and the blank grid below the last content dropped.
///
/// Each row is kept in both forms: raw, and with escape sequences
/// removed. A blank row BETWEEN content survives, because it is composer
/// content and it means whatever sits above it was not the last thing on
/// the screen.
pub(crate) fn composer_rows(screen: &str) -> Vec<(&str, String)> {
    let mut rows: Vec<(&str, String)> = screen
        .lines()
        .map(|raw| (unpad(raw), unpad(&strip_csi(raw)).to_string()))
        .collect();
    while rows.last().is_some_and(|(_, plain)| plain.is_empty()) {
        rows.pop();
    }
    rows
}

/// Rows from `capture-pane -J`, which already omits unused grid cells.
///
/// Unlike the regular capture, `-J` preserves spaces that occupy terminal
/// cells. Those spaces may be composer content, so exact extraction keeps
/// them and drops only empty rows below the visible grid.
pub(crate) fn joined_composer_rows(screen: &str) -> Vec<(&str, String)> {
    let mut rows: Vec<(&str, String)> = screen.lines().map(|raw| (raw, strip_csi(raw))).collect();
    while rows.last().is_some_and(|(raw, _)| raw.is_empty()) {
        rows.pop();
    }
    rows
}

/// Do the vendor's declared trailer rows follow, in order, with nothing
/// else after them?
///
/// This is what makes a staging proof TERMINAL. Finding what was staged
/// says only that it is on the screen somewhere; proving that only the
/// vendor's own chrome follows it is what says the composer holds that
/// and nothing more. Without it, a line a person typed underneath rides
/// along and the submit key sends both.
///
/// Shared by both proofs deliberately. A visible payload and a collapsed
/// one are two ways of recognizing the same staged text, and a second
/// copy of this rule would be a second place for terminality to rot.
pub(crate) fn trailer_follows(manifest: &Manifest, suffix: &[(&str, String)]) -> bool {
    let layout = &manifest.composer_trailers;
    let layout_esc = &manifest.composer_trailers_esc;
    let required = manifest.injection.composer_trailer_required_prefix;
    // Unmeasured layout cannot answer the question.
    if layout.is_empty() || layout_esc.len() != layout.len() {
        return false;
    }
    if required == 0 || required > layout.len() {
        return false;
    }
    // Chrome always follows a real composer, and never more rows than the
    // layout declares.
    if suffix.len() < required || suffix.len() > layout.len() {
        return false;
    }
    let structural_unstyled = suffix.iter().all(|(raw, plain)| *raw == plain)
        && matches!(
            manifest.injection.unstyled_composer_proof,
            Some(cyclops_manifest::UnstyledComposerProof::StructuralTrailer)
        );
    if structural_unstyled {
        let Some((_, first)) = suffix.first() else {
            return false;
        };
        let belongs_to_composer = manifest
            .composer_prompt
            .as_ref()
            .is_some_and(|pattern| captured_content(pattern, first).is_some())
            || manifest
                .composer_continuation
                .as_ref()
                .is_some_and(|pattern| captured_content(pattern, first).is_some());
        if belongs_to_composer {
            return false;
        }
    }
    // Full span on the plain row, generically: a manifest that forgot an
    // anchor would otherwise accept trailing payload on a chrome row, and
    // no vendor should be able to weaken terminality by omission. The
    // escaped half supplies the style evidence, where a partial match is
    // meaningful because SGR runs surround the text.
    let matches = |i: usize, raw: &str, plain: &str| {
        whole_row(&layout[i], plain) && (layout_esc[i].is_match(raw) || structural_unstyled)
    };
    for (i, (raw, plain)) in suffix.iter().enumerate().take(required) {
        if !matches(i, raw, plain) {
            return false;
        }
    }
    // Later declared rows may be absent, but never reordered, and an
    // undeclared row claims nothing and refuses.
    let mut next = required;
    for (raw, plain) in &suffix[required..] {
        let mut claimed = false;
        while next < layout.len() {
            let i = next;
            next += 1;
            if matches(i, raw, plain) {
                claimed = true;
                break;
            }
        }
        if !claimed {
            return false;
        }
    }
    true
}

/// The proven staged row, when an exact single-line composer row validates:
/// the unique terminal composer row matching the expected text (with optional
/// vendor prompt prefix verified against the manifest), followed directly by
/// the vendor's declared composer trailer chrome.
pub(crate) fn exact_row_proof(
    manifest: &Manifest,
    screen: &str,
    expected_row: &str,
) -> Option<String> {
    let want = unpad(expected_row);
    if want.is_empty() {
        return None;
    }
    let rows = composer_rows(screen);
    let start = rows.len().saturating_sub(VERIFY_REGION);
    let window = &rows[start..];

    let idle_rules: Vec<_> = manifest
        .rules
        .iter()
        .filter(|rule| rule.state == AgentState::IdleWithInput)
        .collect();
    let structural_unstyled = manifest.injection.unstyled_composer_proof
        == Some(UnstyledComposerProof::StructuralTrailer)
        && !screen.contains('\u{1b}');

    let hits: Vec<usize> = window
        .iter()
        .enumerate()
        .filter(|(_, (raw, plain))| {
            let p = unpad(plain);
            let r = unpad(raw);
            if idle_rules.is_empty() {
                return p == want || r == want;
            }
            if structural_unstyled
                && raw == plain
                && manifest
                    .composer_prompt
                    .as_ref()
                    .and_then(|prompt| captured_content(prompt, plain))
                    == Some(want)
            {
                return true;
            }
            if let Some(prefix) = p.strip_suffix(want) {
                let is_prompt_prefix = prefix
                    .chars()
                    .all(|c| !c.is_alphanumeric() && !c.is_control());
                if is_prompt_prefix && idle_rules.iter().any(|rule| rule.matches_row(plain, raw)) {
                    return true;
                }
            }
            if let Some(prefix) = r.strip_suffix(want) {
                let is_prompt_prefix = prefix
                    .chars()
                    .all(|c| !c.is_alphanumeric() && !c.is_control());
                if is_prompt_prefix && idle_rules.iter().any(|rule| rule.matches_row(plain, raw)) {
                    return true;
                }
            }
            false
        })
        .map(|(i, _)| i)
        .filter(|at| trailer_follows(manifest, &window[at + 1..]))
        .collect();

    let [at] = hits[..] else {
        return None;
    };

    if window[..at]
        .iter()
        .any(|(raw, plain)| idle_rules.iter().any(|rule| rule.matches_row(plain, raw)))
    {
        return None;
    }

    Some(window[at].1.trim().to_string())
}

pub(crate) fn exact_row_verified(manifest: &Manifest, screen: &str, expected_row: &str) -> bool {
    exact_row_proof(manifest, screen, expected_row).is_some()
}

/// Last `n` non-empty lines of a capture, top-down, joined.
pub(crate) fn bottom_window(screen: &str, n: usize) -> String {
    let mut lines: Vec<&str> = screen
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(n)
        .collect();
    lines.reverse();
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// ACK tiers
// ---------------------------------------------------------------------------

/// Prove that the active composer contains the exact bytes selected for this
/// attempt. Visible payloads are reconstructed from joined logical rows and
/// compared byte for byte. A collapsed chip proves only that the vendor drew a
/// chip. Its hidden bytes cannot prove ownership and can never authorize a
/// submit key.
pub(crate) fn exact_staging_proof(
    manifest: &Manifest,
    screen: &str,
    target: StagingTarget<'_>,
    expected_payload: &str,
) -> Option<(bool, String)> {
    if staged_representation(manifest, screen, target) != Some(StagedRepresentation::VisibleTarget)
    {
        return None;
    }
    match exact_composer_content_from_joined_capture(manifest, screen) {
        ComposerContentProof::Visible(content)
            if visible_single_line_payload_matches(&content, expected_payload) =>
        {
            Some((true, expected_payload.to_string()))
        }
        ComposerContentProof::Visible(_)
        | ComposerContentProof::Hidden
        | ComposerContentProof::Unsupported
        | ComposerContentProof::Unprovable => None,
    }
}

/// Match one exact single-line payload after a terminal application has drawn
/// it over several visual composer rows.
///
/// Codex, Claude, and AGY wrap at word boundaries themselves, so tmux `-J`
/// cannot join those rows. They also repaint the unused suffix of each visual
/// composer row with ASCII spaces. Those renderer-owned suffix cells and the
/// one ASCII separator consumed at a wrap boundary are ignored. No other byte
/// may be added, removed, or reordered.
pub(crate) fn visible_single_line_payload_matches(visible: &str, expected: &str) -> bool {
    if visible == expected {
        return true;
    }
    if expected.contains('\n') || !visible.contains('\n') {
        return false;
    }

    let parts: Vec<&str> = visible.split('\n').collect();
    let mut offsets = vec![0usize];
    for (at, part) in parts.iter().enumerate() {
        let part = part.trim_end_matches(' ');
        if part.is_empty() {
            return false;
        }
        let mut next = Vec::with_capacity(offsets.len() * 2);
        for offset in offsets {
            let Some(remaining) = expected.get(offset..) else {
                continue;
            };
            let Some(remaining) = remaining.strip_prefix(part) else {
                continue;
            };
            let end = expected.len() - remaining.len();
            next.push(end);
            if at + 1 < parts.len() && expected.as_bytes().get(end) == Some(&b' ') {
                next.push(end + 1);
            }
        }
        next.sort_unstable();
        next.dedup();
        offsets = next;
        if offsets.is_empty() {
            return false;
        }
    }
    offsets.binary_search(&expected.len()).is_ok()
}

/// Closed screen-representation outcomes for the Gate 7 component harness.
///
/// This proof cannot authorize delivery. It deliberately excludes process
/// binding, pane mode, action safety, and durable composer holds. The daemon's
/// normal gate remains the only authority for a real write.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerRepresentationProof {
    ExactStaged,
    WriteSafeClean,
    WriteSafeGhost,
    HiddenOrAmbiguous,
}

/// Classifies the visible composer through production representation parsers.
///
/// Callers must not use this as a write-readiness decision. It exists only so
/// the opt-in live harness can measure the same exact staged-row and
/// clean-or-ghost screen representations the daemon consumes.
#[doc(hidden)]
pub fn prove_composer_representation(
    manifest: &Manifest,
    screen: &str,
    expected_staged: Option<&str>,
) -> ComposerRepresentationProof {
    if let Some(expected) = expected_staged {
        if exact_staging_proof(
            manifest,
            screen,
            StagingTarget::ExactRow(expected),
            expected,
        )
        .is_some()
        {
            return ComposerRepresentationProof::ExactStaged;
        }
    } else {
        if clean_composer_proof(manifest, screen) {
            return ComposerRepresentationProof::WriteSafeClean;
        }
        let plain = strip_csi(screen);
        let winner = fusion::screen_winner_esc(manifest, &plain, Some(screen));
        if winner.is_some_and(|rule| {
            rule.state == AgentState::Idle
                && rule.composer_semantic == Some(ComposerSemantic::GhostSuggestion)
        }) {
            return ComposerRepresentationProof::WriteSafeGhost;
        }
    }
    ComposerRepresentationProof::HiddenOrAmbiguous
}

/// Extract the active single-line notification composer for a local diff.
///
/// The prompt must satisfy both the manifest extraction pattern and an
/// IdleWithInput rule. The declared trailer must follow the extracted rows
/// exactly. These two checks keep transcript prompts and unrelated pane text
/// out of the result.
pub(crate) fn exact_composer_content_from_joined_capture(
    manifest: &Manifest,
    screen: &str,
) -> ComposerContentProof {
    exact_composer_content_for_state(manifest, screen, AgentState::IdleWithInput, None)
}

/// Extract exact occupied input or a visibly empty clean composer.
///
/// Projection and recovery need both outcomes. Ghost suggestions and
/// ambiguous input remain unprovable, and collapsed paste chips remain hidden.
pub(crate) fn composer_content_for_projection_from_joined_capture(
    manifest: &Manifest,
    screen: &str,
) -> ComposerContentProof {
    match exact_composer_content_from_joined_capture(manifest, screen) {
        ComposerContentProof::Unprovable => exact_composer_content_for_state(
            manifest,
            screen,
            AgentState::Idle,
            Some(ComposerSemantic::Clean),
        ),
        proof => proof,
    }
}

pub(crate) fn exact_composer_content_for_state(
    manifest: &Manifest,
    screen: &str,
    state: AgentState,
    required_semantic: Option<ComposerSemantic>,
) -> ComposerContentProof {
    if collapsed_chip_row(manifest, screen).is_some() {
        return ComposerContentProof::Hidden;
    }
    let (Some(prompt), Some(continuation)) = (
        manifest.composer_prompt.as_ref(),
        manifest.composer_continuation.as_ref(),
    ) else {
        return ComposerContentProof::Unsupported;
    };
    let composer_rules: Vec<_> = manifest
        .rules
        .iter()
        .filter(|rule| {
            rule.state == state
                && required_semantic.is_none_or(|semantic| rule.composer_semantic == Some(semantic))
        })
        .collect();
    if composer_rules.is_empty() {
        return ComposerContentProof::Unprovable;
    }

    let rows = joined_composer_rows(screen);
    let structural_unstyled = manifest.injection.unstyled_composer_proof
        == Some(UnstyledComposerProof::StructuralTrailer)
        && !screen.contains('\u{1b}');
    let start = rows.len().saturating_sub(VERIFY_REGION);
    let window = &rows[start..];
    let trailers: Vec<usize> = (0..window.len())
        .filter(|at| trailer_follows(manifest, &window[*at..]))
        .collect();
    let [trailer_at] = trailers.as_slice() else {
        return ComposerContentProof::Unprovable;
    };

    let prompts: Vec<(usize, &str)> = window[..*trailer_at]
        .iter()
        .enumerate()
        .filter_map(|(at, (raw, plain))| {
            let content = captured_content(prompt, plain)?;
            (composer_rules
                .iter()
                .any(|rule| rule.matches_row(plain, raw))
                || (state == AgentState::IdleWithInput && structural_unstyled && raw == plain))
                .then_some((at, content))
        })
        .filter(|(prompt_at, _)| {
            window[prompt_at + 1..*trailer_at].iter().all(|(_, plain)| {
                captured_continuation_content(manifest, continuation, plain).is_some()
            })
        })
        .collect();
    let [(prompt_at, first)] = prompts.as_slice() else {
        return ComposerContentProof::Unprovable;
    };

    let mut content = vec![(*first).to_string()];
    for (_, plain) in &window[prompt_at + 1..*trailer_at] {
        if captured_content(prompt, plain).is_some() {
            return ComposerContentProof::Unprovable;
        }
        let Some(line) = captured_continuation_content(manifest, continuation, plain) else {
            return ComposerContentProof::Unprovable;
        };
        content.push(line.to_string());
    }
    // tmux 3.6a retains right-padding cells in a joined capture. Normalize
    // them only after a manifest rule has classified one prompt row as clean.
    // Occupied composer rows keep every byte for exact ownership checks.
    if state == AgentState::Idle
        && required_semantic == Some(ComposerSemantic::Clean)
        && content.len() == 1
        && content[0].bytes().all(|byte| byte == b' ')
    {
        content[0].clear();
    }
    ComposerContentProof::Visible(content.join("\n"))
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn captured_content<'a>(
    pattern: &cyclops_manifest::Regex,
    row: &'a str,
) -> Option<&'a str> {
    let captures = pattern.captures(row)?;
    let whole = captures.get(0)?;
    if whole.start() != 0 || whole.end() != row.len() {
        return None;
    }
    captures.name("content").map(|content| content.as_str())
}

/// Extract one continuation row without allowing a legacy seed to redefine
/// the exact payload comparison.
///
/// The old shipped AGY pattern captured its renderer's two-cell continuation
/// gutter as `content`. In the measured AGY 1.1.23 doorbell, a non-space
/// payload byte immediately follows that gutter. Strip the gutter only for
/// the complete, exact pre-change shipped manifest source and only when a
/// third leading ASCII space is absent; a third space could be deliberate
/// input and must remain a mismatch. An operator-customized manifest with the
/// same regex does not match that source fingerprint and therefore fails
/// closed. New manifests express the gutter in their regex, so they never
/// enter this compatibility path.
pub(crate) const LEGACY_AGY_PRE_GUTTER_MANIFEST_SHA256: [u8; 32] = [
    0x9c, 0xfc, 0x99, 0xfd, 0x61, 0xc8, 0x36, 0xa6, 0x54, 0xce, 0x15, 0x24, 0x2c, 0xca, 0xa3, 0x7c,
    0xaf, 0x53, 0xaa, 0xbc, 0xee, 0x5e, 0xec, 0x1d, 0x02, 0xab, 0xee, 0x5b, 0xe2, 0x28, 0x94, 0x98,
];

pub(crate) fn captured_continuation_content<'a>(
    manifest: &Manifest,
    pattern: &cyclops_manifest::Regex,
    row: &'a str,
) -> Option<&'a str> {
    let content = captured_content(pattern, row)?;
    let legacy_agy_pattern = manifest.agent.id == "agy"
        && manifest.source_digest() == LEGACY_AGY_PRE_GUTTER_MANIFEST_SHA256
        && pattern.as_str() == "^(?P<content>.*)$";
    if legacy_agy_pattern && content.starts_with("  ") && content.as_bytes().get(2) != Some(&b' ') {
        return Some(&content[2..]);
    }
    Some(content)
}

/// Does this pattern match the ENTIRE row, rather than some run inside it?
///
/// Terminality again, in a second place: a chip pattern that matches a
/// substring proves a chip appeared somewhere on the row, not that the row
/// IS the chip, and a row carrying payload either side of it would pass.
/// Anchors in manifest data cannot be relied on for that, so the span is
/// checked here where no vendor can forget it.
pub(crate) fn whole_row(re: &cyclops_manifest::Regex, row: &str) -> bool {
    re.find(row)
        .is_some_and(|m| m.start() == 0 && m.end() == row.len())
}

/// The recognized chip row when the collapsed representation matches.
///
/// Equality against this row is equality of the screen representation only.
/// The payload behind a chip is not on screen, so it cannot prove exact
/// notification ownership and cannot authorize Enter.
pub(crate) fn collapsed_chip_row(manifest: &Manifest, screen: &str) -> Option<String> {
    if manifest.composer_chips.is_empty()
        || manifest.composer_chips.len() != manifest.composer_chips_esc.len()
    {
        return None;
    }
    // No separate "is this an escaped capture" guard: the escaped half of
    // a measured chip contains escape bytes, so a plain capture fails it
    // on its own. Adding a guard on top would only stop a vendor whose
    // chip genuinely renders unstyled from ever declaring one.
    let rows = composer_rows(screen);
    let start = rows.len().saturating_sub(VERIFY_REGION);
    let window = &rows[start..];
    for rule in manifest
        .rules
        .iter()
        .filter(|r| r.state == AgentState::IdleWithInput)
    {
        let cyclops_manifest::Region::BottomNonEmptyLines(n) = rule.region else {
            continue;
        };
        // The rule's own region bounds where its chip may appear.
        let from = window.len().saturating_sub(n);
        // Exactly one chip row, for the same reason the sentinel needs
        // exactly one. A styled copy of the chip sitting in the
        // transcript above the live composer is the shape that produces
        // two, and which one transport owns is then a guess.
        let hits: Vec<usize> = window
            .iter()
            .enumerate()
            .skip(from)
            .filter(|(_, (raw, plain))| {
                let chip = manifest
                    .composer_chips
                    .iter()
                    .zip(manifest.composer_chips_esc.iter())
                    .any(|(p, e)| whole_row(p, plain.trim()) && whole_row(e, raw));
                // The manifest decides whether this row is its composer,
                // with its own clause semantics. Reimplementing that as
                // "plain matched OR escaped matched" would let either half
                // carry a rule that was written to need both.
                chip && rule.matches_row(plain, raw)
            })
            .map(|(i, _)| i)
            .collect();
        let [at] = hits[..] else {
            continue;
        };
        // A collapsed payload proves no more than a visible one does. The
        // chip says the composer holds a paste; only the vendor's own
        // chrome following it says the composer holds nothing ELSE, and a
        // line typed under the chip is exactly what that catches.
        if !trailer_follows(manifest, &window[at + 1..]) {
            continue;
        }
        return Some(window[at].1.trim().to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// ACK registry (used by the matcher in ack.rs)
// ---------------------------------------------------------------------------
