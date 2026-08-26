//! Data-safety tests for the shipped manifests, locked against real
//! captures from the M1 soak, the codex ghost probe (2026-08-02), and the
//! live Claude Code 2.1.221 rig (2026-08-06). These encode the M1 gate
//! fixes; loosening them reopens measured injection hazards.

use std::path::Path;

use cyclops_manifest::{load_dir, AckEvidence, LifecycleCertainty, LifecycleRole, Manifest};
use cyclops_proto::{AgentState, ComposerSemantic};

fn shipped() -> std::collections::HashMap<String, Manifest> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
    load_dir(&dir).unwrap()
}

#[test]
fn shipped_composer_semantics_match_measured_rules_only() {
    use ComposerSemantic::{Ambiguous, Clean, GhostSuggestion, HumanInput};

    let all = shipped();
    let expected = [
        ("claude", "composer_empty", Clean),
        ("claude", "composer_has_staged_input", HumanInput),
        ("claude", "composer_styled_input", HumanInput),
        ("claude", "composer_unstyled_input", HumanInput),
        ("codex", "composer_typed_input", HumanInput),
        ("codex", "composer_ghost_suggestion", GhostSuggestion),
        ("codex", "composer_empty_or_ghost", Ambiguous),
        ("cursor", "composer_typed_input", HumanInput),
        ("cursor", "composer_ghost_or_empty", Ambiguous),
        ("cursor", "composer_plain_fallback", Ambiguous),
        ("agy", "composer_empty", Clean),
        ("agy", "composer_has_input", HumanInput),
    ];

    for &(manifest_id, rule_id, semantic) in &expected {
        let rule = all[manifest_id]
            .rules
            .iter()
            .find(|rule| rule.id == rule_id)
            .unwrap_or_else(|| panic!("missing {manifest_id}/{rule_id}"));
        assert_eq!(
            rule.composer_semantic,
            Some(semantic),
            "{manifest_id}/{rule_id} has the wrong composer meaning"
        );
    }

    let annotated = all
        .values()
        .flat_map(|manifest| manifest.rules.iter())
        .filter(|rule| rule.composer_semantic.is_some())
        .count();
    assert_eq!(
        annotated,
        expected.len(),
        "a shipped rule gained an unreviewed composer meaning"
    );
    assert_eq!(
        all["claude"]
            .rules
            .iter()
            .find(|rule| rule.id == "composer_ghost_suggestion")
            .unwrap()
            .composer_semantic,
        None,
        "Claude's ghost shape is not yet backed by a live Claude capture"
    );
}

fn version_between<'a>(capture: &'a str, start: &str, end: &str) -> &'a str {
    let value = capture
        .split_once(start)
        .unwrap_or_else(|| panic!("fixture lacks version marker {start:?}"))
        .1;
    value
        .split_once(end)
        .unwrap_or_else(|| panic!("fixture version lacks terminator {end:?}"))
        .0
}

/// `version_tested` names the capture that anchors each shipped ruleset.
/// Newer partial captures do not silently strengthen that whole-manifest
/// claim. Promoting it requires a new authoritative fixture and this table
/// changing together.
#[test]
fn version_tested_matches_each_authoritative_fixture() {
    let all = shipped();
    let cases = [
        (
            "claude",
            version_between(
                include_str!("fixtures/claude_idle_2_1_221.txt"),
                "Claude Code v",
                " ",
            ),
        ),
        (
            "codex",
            version_between(
                include_str!("fixtures/codex_working_composer_plain.txt"),
                "OpenAI Codex (v",
                ")",
            ),
        ),
        (
            "agy",
            version_between(
                include_str!("fixtures/agy_working_composer_plain.txt"),
                "Antigravity CLI ",
                "\n",
            ),
        ),
        (
            "cursor",
            version_between(
                include_str!("fixtures/cursor_working_composer_plain.txt"),
                "\n  v",
                "\n",
            ),
        ),
    ];

    for (id, fixture_version) in cases {
        assert_eq!(
            all[id].agent.version_tested, fixture_version,
            "{id}: version_tested diverged from its authoritative fixture"
        );
    }
}

/// Cursor's checked-in evidence contains terminal captures, not paired
/// start and end hook payloads. Prose stating that both events carried a
/// generation id is not enough to select the exact lifecycle.
#[test]
fn cursor_turn_correlation_waits_for_paired_hook_payload_fixtures() {
    let all = shipped();
    let cursor = &all["cursor"];
    assert!(
        cursor.hooks.turn_key_fields.is_empty(),
        "Cursor needs paired current start and end payload fixtures first"
    );

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut cursor_fixtures = 0;
    for entry in std::fs::read_dir(dir).expect("fixture directory") {
        let path = entry.expect("fixture entry").path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("cursor_") {
            continue;
        }
        cursor_fixtures += 1;
        let bytes = std::fs::read(&path).expect("Cursor fixture");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("generation_id") && !text.contains("hook_event_name"),
            "{name} became hook evidence; review turn_key_fields instead of keeping this limitation"
        );
    }
    assert!(cursor_fixtures > 0, "Cursor fixture inventory is empty");
}

/// Claude's current hook payload can identify the submitted prompt but cannot
/// match UserPromptSubmit to Stop. Start and dispatch are one event-local
/// candidate; visual evidence owns acceptance and completion.
#[test]
fn claude_ships_an_unkeyed_dispatch_start_with_a_visual_end() {
    let all = shipped();
    let claude = &all["claude"];
    assert_eq!(
        claude.hooks.lifecycle_event("UserPromptSubmit"),
        Some((LifecycleRole::Start, LifecycleCertainty::Candidate))
    );
    assert_eq!(claude.hooks.ack.as_deref(), Some("UserPromptSubmit"));
    assert_eq!(claude.hooks.ack_evidence, AckEvidence::Dispatch);
    assert_eq!(claude.hooks.ack_payload_field.as_deref(), Some("prompt"));
    assert!(claude.hooks.turn_key_fields.is_empty());
    assert!(claude.hooks.turn_end.is_none());
    assert!(claude.hooks.turn_end_confirmed.is_empty());
    assert_eq!(claude.hooks.turn_end_settle_ms, 0);
    assert!(!claude.hooks.has_lifecycle_role(LifecycleRole::End));
    assert_eq!(claude.hooks.lifecycle_event("Stop"), None);
}

#[test]
fn composer_actions_ship_only_for_measured_vendors() {
    let all = shipped();
    for id in ["claude", "codex"] {
        let manifest = &all[id];
        assert_eq!(manifest.injection.clear_keys, ["C-c"], "{id}");
        assert!(manifest.composer_prompt.is_some(), "{id}");
        assert!(manifest.composer_continuation.is_some(), "{id}");
    }
    for id in ["agy", "cursor"] {
        let manifest = &all[id];
        assert!(manifest.injection.clear_keys.is_empty(), "{id}");
        assert!(manifest.composer_prompt.is_none(), "{id}");
        assert!(manifest.composer_continuation.is_none(), "{id}");
    }
    for manifest in all.values() {
        assert!(
            manifest
                .injection
                .clear_keys
                .iter()
                .all(|key| !matches!(key.as_str(), "C-e" | "C-u")),
            "{} ships a partial-line clear sequence",
            manifest.agent.id
        );
    }
}

/// M1 review, HIGH: with the sparkle title at 1000 and staged input at 950,
/// the fused state read idle while a human draft sat in the composer, and
/// the gate would paste over it and auto-submit. The staged-input rule must
/// outrank the idle title.
#[test]
fn claude_staged_input_outranks_idle_sparkle() {
    let all = shipped();
    let claude = &all["claude"];
    // Idle-shaped screen except for a human draft on the composer line.
    let screen = "\u{273b} Crunched for 1s\n\
        ────────────────────────────────────────\n\
        ❯ draft text\n\
        ────────────────────────────────────────\n\
        \u{20}\u{20}Haiku 4.5 · /tmp/proj · Ctx: 86%\n\
        \u{20}\u{20}⏵⏵ bypass permissions on (shift+tab to cycle)";
    // The escaped view of the same screen, carrying the measured composer
    // signature (F55): 'ESC[39m' + glyph + NBSP, draft text unstyled.
    let esc = screen.replace("❯ draft text", "\u{1b}[39m❯\u{a0}draft text");
    let r = claude
        .evaluate_esc("\u{2733} Done", screen, Some(&esc))
        .unwrap();
    assert_eq!(r.id, "composer_has_staged_input");
    assert_eq!(r.state, AgentState::IdleWithInput);

    // The same screen with an empty composer still reads idle by title.
    let idle_screen = screen.replace("❯ draft text", "❯ ");
    let idle_esc = esc.replace("\u{1b}[39m❯\u{a0}draft text", "\u{1b}[39m❯\u{a0}");
    let r = claude
        .evaluate_esc("\u{2733} Done", &idle_screen, Some(&idle_esc))
        .unwrap();
    assert_eq!(r.state, AgentState::Idle);
}

/// F55 (ghost probe, Claude Code 2.1.222, 2026-08-13): the plain capture
/// cannot separate a human draft from ghost/suggestion text or from the
/// submitted-prompt echo in scrollback — all three render '❯ <text>'. The
/// escaped capture can: real input follows the composer's NBSP unstyled,
/// a suggestion arrives styled, and the echo repaints the glyph with its
/// own colors and a plain space. Locked against the probe's real captures
/// where they exist; the ghost line itself never rendered during the probe
/// and is constructed from the convention codex and cursor both measured
/// (ESC[2m after the glyph), so the discriminator has a test even before
/// a live Claude ghost is captured.
#[test]
fn claude_ghost_echo_and_chip_read_off_the_escaped_capture() {
    let all = shipped();
    let claude = &all["claude"];

    // Typed text (real capture): idle_with_input, never inject over it.
    let typed_plain = include_str!("fixtures/claude_typed_composer_plain.txt");
    let typed_esc = include_str!("fixtures/claude_typed_composer_esc.txt");
    let r = claude
        .evaluate_esc("proj", typed_plain, Some(typed_esc))
        .unwrap();
    assert_eq!(r.id, "composer_has_staged_input");
    assert_eq!(r.state, AgentState::IdleWithInput);

    // The collapsed paste chip (real capture) renders unstyled after the
    // NBSP, exactly like typed text: still staged input.
    let chip_plain = include_str!("fixtures/claude_pasted_chip_plain.txt");
    let chip_esc = include_str!("fixtures/claude_pasted_chip_esc.txt");
    let r = claude
        .evaluate_esc("proj", chip_plain, Some(chip_esc))
        .unwrap();
    assert_eq!(r.id, "composer_has_staged_input");
    assert_eq!(r.state, AgentState::IdleWithInput);

    // A submitted-prompt echo sitting inside the bottom window (its '❯ '
    // uses a plain space and its own colors, F55) with an empty composer
    // below it: the old plain-only rule held delivery on this, and it must
    // read idle now.
    let echo_plain = "❯ Reply with exactly the word OK and nothing else.\n\
        ────────────────────────────────────────\n\
        ❯ \n\
        ────────────────────────────────────────\n\
        \u{20}\u{20}Opus 5 · ~/proj · 1000K window\n\
        \u{20}\u{20}⏸ manual mode on";
    let echo_esc = echo_plain.replace(
        "❯ Reply with exactly the word OK and nothing else.",
        "\u{1b}[38;5;239m\u{1b}[48;5;237m❯ \u{1b}[38;5;231mReply with exactly the word OK and nothing else.\u{1b}[39m",
    );
    let echo_esc = echo_esc.replace("❯ \n", "\u{1b}[39m❯\u{a0}\n");
    let r = claude
        .evaluate_esc("proj", echo_plain, Some(&echo_esc))
        .unwrap();
    assert_eq!(r.state, AgentState::Idle);

    // Ghost text: styled right after the NBSP. Idle, and named as the
    // ghost so `--source detection` says what was actually on screen.
    let ghost_plain = echo_plain.replace("❯ \n", "❯ Try \"fix the tests\"\n");
    let ghost_esc = echo_esc.replace(
        "\u{1b}[39m❯\u{a0}\n",
        "\u{1b}[39m❯\u{a0}\u{1b}[2mTry \"fix the tests\"\u{1b}[0m\n",
    );
    let r = claude
        .evaluate_esc("proj", &ghost_plain, Some(&ghost_esc))
        .unwrap();
    assert_eq!(r.id, "composer_ghost_suggestion");
    assert_eq!(r.state, AgentState::Idle);

    // Without an escaped capture the staged rule fails closed and the
    // title tier answers (a real Claude pane always has one). This is the
    // residual gap's direction: a missing -e capture plus an idle title
    // reads idle, the same trade codex and cursor accepted.
    let r = claude.evaluate("\u{2733} Done", typed_plain).unwrap();
    assert_eq!(r.id, "title_idle_sparkle");
    assert_eq!(r.state, AgentState::Idle);
}

/// M1 soak, SAFETY: Claude 2.1.220's folder-trust dialog contains 'Enter to
/// confirm', so startup_modal (auto_dismiss, decline Escape) matched it, and
/// Escape EXITS the CLI. The dedicated trust_dialog rule must win and must
/// never auto-dismiss: trust is a human decision.
#[test]
fn claude_trust_dialog_never_auto_dismissed() {
    let all = shipped();
    let claude = &all["claude"];
    let dialog = include_str!("fixtures/claude_trust_dialog.txt");

    let r = claude.evaluate("plain title", dialog).unwrap();
    assert_eq!(r.id, "trust_dialog");
    assert_eq!(r.state, AgentState::BlockedModal);
    assert!(!r.auto_dismiss, "trust dialog must park, not auto-dismiss");
    assert!(r.decline_keys.is_empty(), "no keys: Escape exits the CLI");

    // The shadow hazard is real: startup_modal alone matches this capture.
    // Only the priority ordering keeps it away from the decline path.
    let startup = claude
        .rules
        .iter()
        .find(|r| r.id == "startup_modal")
        .expect("startup_modal rule");
    let lines: Vec<&str> = dialog.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(startup.matches(dialog, &lines));
    let trust = claude
        .rules
        .iter()
        .find(|r| r.id == "trust_dialog")
        .expect("trust_dialog rule");
    assert!(trust.priority > startup.priority);
}

/// M1 review, HIGH: codex ghost suggestions and typed text are identical in
/// the plain capture, so idle_with_input was unreachable. MEASURED (ghost
/// probe, codex-cli 0.146.0): SGR discriminates: ghost text is dim
/// (ESC[2m), typed text is bare. Locked against the probe's real captures.
#[test]
fn codex_ghost_vs_typed_probed_fixtures() {
    let all = shipped();
    let codex = &all["codex"];
    let ghost_plain = include_str!("fixtures/codex_ghost_composer_plain.txt");
    let ghost_esc = include_str!("fixtures/codex_ghost_composer_esc.txt");
    let typed_plain = include_str!("fixtures/codex_typed_composer_plain.txt");
    let typed_esc = include_str!("fixtures/codex_typed_composer_esc.txt");

    // Pristine composer, ghost suggestion rendered dim: idle, safe.
    let r = codex
        .evaluate_esc("proj", ghost_plain, Some(ghost_esc))
        .unwrap();
    assert_eq!(r.id, "composer_ghost_suggestion");
    assert_eq!(r.state, AgentState::Idle);

    // Typed literal text: idle_with_input, never inject over it.
    let r = codex
        .evaluate_esc("proj", typed_plain, Some(typed_esc))
        .unwrap();
    assert_eq!(r.id, "composer_typed_input");
    assert_eq!(r.state, AgentState::IdleWithInput);

    // Without an escaped capture the esc rules fail closed and the plain
    // fallback still calls typed text idle. This documents the residual
    // gap until the daemon supplies capture-pane -e captures.
    let r = codex.evaluate("proj", typed_plain).unwrap();
    assert_eq!(r.id, "composer_empty_or_ghost");
    assert_eq!(r.state, AgentState::Idle);
}

/// Codex 0.149.1 adds a truecolor SGR between the bold composer style and
/// the prompt glyph. The occupied composer must still outrank the plain
/// idle fallback or a staged Cyclops doorbell cannot be verified.
#[test]
fn codex_0_149_1_colored_prompt_is_staged_input() {
    let all = shipped();
    let codex = &all["codex"];
    let escaped = include_str!("fixtures/codex_staged_0_149_1_esc.txt");
    let plain = cyclops_manifest::strip_csi(escaped);

    let rule = codex.evaluate_esc("proj", &plain, Some(escaped)).unwrap();
    assert_eq!(rule.id, "composer_typed_input");
    assert_eq!(rule.state, AgentState::IdleWithInput);

    let doorbell =
        "cyclops inbox claim m-4c0cdcbf9cb04cf983ef2c6aa206eac9 #c:xvXB2rLoTC2SpbRj5fnDFA";
    let human = escaped.replace(doorbell, "review my local changes");
    let human_plain = cyclops_manifest::strip_csi(&human);
    let rule = codex
        .evaluate_esc("proj", &human_plain, Some(&human))
        .unwrap();
    assert_eq!(rule.id, "composer_typed_input");
    assert_eq!(rule.state, AgentState::IdleWithInput);

    let ghost = escaped.replace(doorbell, "\u{1b}[2mSummarize recent commits\u{1b}[0m");
    let ghost_plain = cyclops_manifest::strip_csi(&ghost);
    let rule = codex
        .evaluate_esc("proj", &ghost_plain, Some(&ghost))
        .unwrap();
    assert_eq!(rule.id, "composer_ghost_suggestion");
    assert_eq!(rule.state, AgentState::Idle);
}

/// MEASURED 2026-08-17 (codex-cli 0.147.0, live pane at 120x40): a paste long
/// enough to collapse renders as a COLORED chip, so the first significant
/// character after the glyph is an SGR introducer rather than a byte. The
/// typed rule required a bare byte there, so a staged chip read idle — which
/// let the gate paste a second message onto the first, and left delivery
/// unable to verify its own staging (the field failure: a long message stayed
/// in codex's composer, unsubmitted, behind "outcome unknown").
///
/// The fixtures are a real capture of that exact state, so a future rule
/// edit is checked against what codex draws, not against a hand-written
/// approximation of it.
#[test]
fn codex_collapsed_paste_chip_is_staged_input() {
    let all = shipped();
    let codex = &all["codex"];
    let chip_plain = include_str!("fixtures/codex_pasted_chip_plain.txt");
    let chip_esc = include_str!("fixtures/codex_pasted_chip_esc.txt");

    let r = codex
        .evaluate_esc("proj", chip_plain, Some(chip_esc))
        .unwrap();
    assert_eq!(r.id, "composer_typed_input");
    assert_eq!(r.state, AgentState::IdleWithInput);

    // The transcript renders a past turn with a bold-dim glyph, while the
    // composer's is bold only. Exact composer matching prevents transcript
    // residue from satisfying active-composer verification.
    let transcript = "\u{1b}[1;2m›  \u{1b}[0m[cyclops m-diag01] FROM: tester  SUBJECT: s\n\
        \u{1b}[1m›\u{1b}[0m \u{1b}[2mSummarize recent commits\u{1b}[0m\n\
        \u{1b}[38;2;246;226;183mgpt-5.6-sol high\u{1b}[0m · ~/proj";
    let r = codex
        .evaluate_esc(
            "proj",
            &cyclops_manifest::strip_csi(transcript),
            Some(transcript),
        )
        .unwrap();
    assert_eq!(
        r.state,
        AgentState::Idle,
        "a submitted turn in the transcript is not a staged composer"
    );
}

/// MEASURED 2026-08-08 (codex-cli 0.147.0, tmux 120x40), SAFETY: Codex keeps
/// the ghost composer line on screen for the WHOLE turn. A live capture taken
/// while "• Working (0s • esc to interrupt)" is rendering still carries
/// "› Run /review on my current changes" two lines below it, so the composer
/// rules match continuously while the agent generates.
///
/// This is the ordering the cursor manifest was deliberately written against
/// (see cursor_working_outranks_the_live_composer_placeholder, which names
/// this exact defect and declines to copy it). Codex shipped it anyway:
/// composer_empty_or_ghost sat at 1000 and screen_working at 800, and
/// evaluation takes the first match after sorting by descending priority, so
/// screen_working was never reached and every Codex turn read idle.
///
/// The consequence is not cosmetic. idle is in [injection].safe_states, so a
/// pane mid-generation looked safe to paste into.
#[test]
fn codex_working_outranks_the_live_ghost_composer() {
    let all = shipped();
    let codex = &all["codex"];
    let plain = include_str!("fixtures/codex_working_composer_plain.txt");
    let esc = include_str!("fixtures/codex_working_composer_esc.txt");

    // The title tier is useless for Codex (static project directory), so the
    // capture is the only sensor that can answer this.
    let r = codex.evaluate_esc("cxwork", plain, Some(esc)).unwrap();
    assert_eq!(r.id, "screen_working");
    assert_eq!(r.state, AgentState::Working);

    // The shadow is real: the idle composer rule matches this same mid-turn
    // capture. Only the ordering keeps the pane from reading idle.
    let ghost = codex
        .rules
        .iter()
        .find(|r| r.id == "composer_empty_or_ghost")
        .expect("composer_empty_or_ghost rule");
    assert!(ghost.matches_esc(plain, &non_empty(plain), Some(&non_empty(esc))));
    let working = codex
        .rules
        .iter()
        .find(|r| r.id == "screen_working")
        .expect("screen_working rule");
    assert!(
        working.priority > ghost.priority,
        "screen_working ({}) must outrank composer_empty_or_ghost ({})",
        working.priority,
        ghost.priority
    );

    // Codex prints "esc to interrupt" lowercase. The rule carried the
    // capitalized form only, so that clause never fired on a real pane.
    assert!(plain.contains("esc to interrupt"));

    // A plain capture with no escaped companion must reach the same verdict:
    // the working indicator is literal text, not an SGR discrimination.
    let r = codex.evaluate("cxwork", plain).unwrap();
    assert_eq!(r.id, "screen_working");
}

#[test]
fn codex_title_spinner_survives_queued_input_below_screen_spinner() {
    let all = shipped();
    let codex = &all["codex"];
    let idle_plain = include_str!("fixtures/codex_ghost_composer_plain.txt");
    let idle_esc = include_str!("fixtures/codex_ghost_composer_esc.txt");
    for frame in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
        let title = format!("{frame} project");
        let rule = codex
            .evaluate_esc(&title, idle_plain, Some(idle_esc))
            .unwrap();
        assert_eq!(rule.id, "title_working_spinner");
        assert_eq!(rule.state, AgentState::Working);
    }

    let idle = codex
        .evaluate_esc("project", idle_plain, Some(idle_esc))
        .unwrap();
    assert_eq!(idle.state, AgentState::Idle);
    assert_ne!(idle.id, "title_working_spinner");

    for title in ["project ⠋ active", "⠋project", "⠋ "] {
        let rule = codex
            .evaluate_esc(title, idle_plain, Some(idle_esc))
            .unwrap();
        assert_ne!(rule.id, "title_working_spinner");
    }

    let queued = [
        "» [tmux-bridge from:peer] queued message",
        "  queued line 2",
        "  queued line 3",
        "  queued line 4",
        "  queued line 5",
        "  queued line 6",
        "  queued line 7",
        "  tab to queue message",
    ]
    .join("\n");
    let plain = format!("• Working (51s • esc to interrupt)\n{queued}");

    let spinner_from_bottom = plain
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .position(|line| line.contains("esc to interrupt"));
    assert_eq!(spinner_from_bottom, Some(8));

    assert!(codex.evaluate("project", &plain).is_none());
    let rule = codex.evaluate("⠴ project", &plain).unwrap();
    assert_eq!(rule.id, "title_working_spinner");
    assert_eq!(rule.state, AgentState::Working);
}

/// MEASURED 2026-08-08 (agy 1.1.11, tmux 120x40), SAFETY: the same defect
/// codex carried, plus a worse one. agy clears its composer the moment a turn
/// starts, so the bare '>' matches composer_empty for the whole turn while
/// screen_working sat below it at 800.
///
/// The second defect is that none of the rule's old clauses matched a running
/// turn at all. The only mid-turn paint is a braille spinner plus
/// "Generating...". '▸ Thought for' is a POST-turn summary that stays on
/// screen, so simply raising the priority without fixing the vocabulary would
/// have wedged every finished pane as working.
///
/// The fixture is a live capture with the operator's account address, shell
/// prompt, and two absolute home paths replaced by neutral stand-ins. No line
/// any rule keys on was touched: the spinner, the composer, and the step
/// markers are byte-for-byte as captured.
#[test]
fn agy_working_outranks_the_cleared_composer() {
    let all = shipped();
    let agy = &all["agy"];
    let plain = include_str!("fixtures/agy_working_composer_plain.txt");

    // agy publishes the hostname as its title, so the capture is the only
    // sensor with anything to say.
    let r = agy.evaluate("mac", plain).unwrap();
    assert_eq!(r.id, "screen_working");
    assert_eq!(r.state, AgentState::Working);

    // The shadow is real: the composer is empty mid-turn, so the idle rule
    // matches this same capture.
    let empty = agy
        .rules
        .iter()
        .find(|r| r.id == "composer_empty")
        .expect("composer_empty rule");
    assert!(empty.matches(plain, &non_empty(plain)));
    let working = agy
        .rules
        .iter()
        .find(|r| r.id == "screen_working")
        .expect("screen_working rule");
    assert!(
        working.priority > empty.priority,
        "screen_working ({}) must outrank composer_empty ({})",
        working.priority,
        empty.priority
    );

    // The vocabulary the rule now keys on, and the one it must not. This
    // fixture is a live tool turn that ALSO carries '▸ Thought for' from the
    // preceding turn, which is the whole reason that string cannot be a
    // working signal: it survives the turn that produced it.
    assert!(plain.contains("Generating..."));
    assert!(plain.contains("▸ Thought for"));

    // A settled pane still showing that summary must read idle, not working.
    let settled = "▸ Thought for 1s, 352 tokens\n\
                   ────────────\n\
                   >\n\
                   ────────────\n";
    assert!(!working.matches(settled, &non_empty(settled)));
    let r = agy.evaluate("mac", settled).unwrap();
    assert_eq!(r.id, "composer_empty");
    assert_eq!(r.state, AgentState::Idle);
}

/// M1 soak, BINDING: native Claude installs report pane_current_command as
/// the version string ("2.1.220"), so process_names never binds. The
/// manifest must carry the argv fallback data the daemon binds with.
#[test]
fn claude_carries_argv_basenames_fallback() {
    let all = shipped();
    let claude = &all["claude"];
    assert_eq!(claude.agent.process_names, vec!["claude"]);
    assert_eq!(claude.agent.argv_basenames, vec!["claude"]);
}

/// Every non-empty line of a capture, the shape `matches` wants for showing
/// that a rule would have fired on its own.
fn non_empty(capture: &str) -> Vec<&str> {
    capture.lines().filter(|l| !l.trim().is_empty()).collect()
}

/// MEASURED 2026-08-06 (Claude Code 2.1.221, unfocused tmux pane): "esc to
/// interrupt" never appeared on the daemon's grid across a 17s streaming
/// turn (0.7s samples plus full mid-turn captures), and the empty composer
/// stays visible below the stream, so mid-turn the screen tier reads
/// composer_empty. Only the title spinner says working. The second
/// assertion pins that screen-tier gap ON PURPOSE: a future real mid-turn
/// screen rule must flip it consciously.
#[test]
fn claude_midturn_streaming_2_1_221() {
    let all = shipped();
    let claude = &all["claude"];
    let grid = include_str!("fixtures/claude_midturn_streaming_2_1_221.txt");

    // The spinner title decides working over the idle-shaped screen.
    let r = claude
        .evaluate("\u{2802} Reply with exactly OK", grid)
        .unwrap();
    assert_eq!(r.id, "title_working_spinner");
    assert_eq!(r.state, AgentState::Working);

    // Pre-OSC hostname title: no title rule fires and the screen tier
    // calls the same mid-turn grid idle. Known gap, deliberately pinned.
    let r = claude.evaluate("mac", grid).unwrap();
    assert_eq!(r.id, "composer_empty");
    assert_eq!(r.state, AgentState::Idle);
}

/// MEASURED 2026-08-06 (Claude Code 2.1.221): idle after launch. The title
/// carries the ✳ sparkle; the screen alone reads idle by the empty
/// composer, whose glyph is followed by U+00A0, not a plain space.
#[test]
fn claude_idle_2_1_221() {
    let all = shipped();
    let claude = &all["claude"];
    let grid = include_str!("fixtures/claude_idle_2_1_221.txt");

    let r = claude.evaluate("\u{2733} Claude Code", grid).unwrap();
    assert_eq!(r.id, "title_idle_sparkle");
    assert_eq!(r.state, AgentState::Idle);

    // Screen tier alone: the empty composer is the idle signal.
    let r = claude.evaluate("mac", grid).unwrap();
    assert_eq!(r.id, "composer_empty");
    assert_eq!(r.state, AgentState::Idle);
}

/// MEASURED 2026-08-06 (Claude Code 2.1.221): the trust dialog's wording
/// still carries 'safety check' and 'trust this folder', and BOTH lower
/// modal rules shadow this capture: startup_modal on 'Enter to confirm',
/// permission_prompt on the '❯ 1. Yes, ...' option line. Only the 1300
/// priority keeps the human-only park in charge; Escape exits the CLI.
#[test]
fn claude_trust_dialog_2_1_221() {
    let all = shipped();
    let claude = &all["claude"];
    let dialog = include_str!("fixtures/claude_trust_dialog_2_1_221.txt");

    let r = claude.evaluate("mac", dialog).unwrap();
    assert_eq!(r.id, "trust_dialog");
    assert_eq!(r.state, AgentState::BlockedModal);
    assert!(!r.auto_dismiss, "trust dialog must park, not auto-dismiss");
    assert!(r.decline_keys.is_empty(), "no keys: Escape exits the CLI");

    let trust = claude
        .rules
        .iter()
        .find(|r| r.id == "trust_dialog")
        .expect("trust_dialog rule");
    let lines = non_empty(dialog);
    for shadow_id in ["startup_modal", "permission_prompt"] {
        let shadow = claude
            .rules
            .iter()
            .find(|r| r.id == shadow_id)
            .expect(shadow_id);
        assert!(
            shadow.matches(dialog, &lines),
            "{shadow_id} lost its shadow"
        );
        assert!(trust.priority > shadow.priority, "{shadow_id} must lose");
    }
}

/// Title table measured on 2.1.221: the ✳ prefix reads idle whatever the
/// summary text after it, every braille frame reads working, and the
/// pre-OSC hostname seed matches no title rule (the screen tier decides).
#[test]
fn claude_title_table_2_1_221() {
    let all = shipped();
    let claude = &all["claude"];

    // Empty screen isolates the title tier.
    let r = claude
        .evaluate("\u{2733} Reply with exactly OK", "")
        .unwrap();
    assert_eq!(r.id, "title_idle_sparkle");
    assert_eq!(r.state, AgentState::Idle);

    for title in [
        "\u{2802} Reply with exactly OK",
        "\u{2810} Reply with exactly OK",
        "\u{2802} Claude Code",
    ] {
        let r = claude.evaluate(title, "").unwrap();
        assert_eq!(r.id, "title_working_spinner", "{title}");
        assert_eq!(r.state, AgentState::Working, "{title}");
    }

    assert!(
        claude.evaluate("mac", "").is_none(),
        "the hostname seed must match no title rule"
    );
}

/// MEASURED 2026-08-05, BINDING: the Cursor entry points are symlinks to a
/// bash wrapper whose last line is `exec -a "$0" "$NODE_BIN" index.js`, so
/// the surviving process is the bundled node and tmux reports
/// pane_current_command = "node". Binding on that name would claim every
/// node process on the machine, so this manifest binds on argv alone —
/// which the same exec makes reliable, since it sets argv[0] to the invoked
/// path. "node" must never appear in process_names.
#[test]
fn cursor_binds_on_argv_and_never_claims_node() {
    let all = shipped();
    let cursor = &all["cursor"];
    assert_eq!(
        cursor.agent.argv_basenames,
        vec!["agent", "cursor-agent"],
        "both installed entry-point names must bind"
    );
    assert!(
        !cursor.agent.process_names.iter().any(|n| n == "node"),
        "binding on the bundled node runtime would claim every node process"
    );
}

/// MEASURED 2026-08-05, SAFETY: Cursor keeps a dim placeholder in the
/// composer for the WHOLE turn ("Add a follow-up" while it generates), so
/// the composer rules match continuously while the agent is working. Codex
/// orders its composer rules ABOVE screen_working; copying that order here
/// would make a Cursor pane read idle for the entire turn and let the gate
/// paste into a live generation. Working must outrank the composer.
#[test]
fn cursor_working_outranks_the_live_composer_placeholder() {
    let all = shipped();
    let cursor = &all["cursor"];
    let plain = include_str!("fixtures/cursor_working_composer_plain.txt");
    let esc = include_str!("fixtures/cursor_working_composer_esc.txt");

    let r = cursor
        .evaluate_esc("Cursor Agent", plain, Some(esc))
        .unwrap();
    assert_eq!(r.id, "screen_working");
    assert_eq!(r.state, AgentState::Working);

    // The shadow is real: the idle composer rule matches this same capture,
    // because the placeholder never goes away. Only the ordering keeps the
    // pane from reading idle mid-turn.
    let ghost = cursor
        .rules
        .iter()
        .find(|r| r.id == "composer_ghost_or_empty")
        .expect("composer_ghost_or_empty rule");
    assert!(ghost.matches_esc(plain, &non_empty(plain), Some(&non_empty(esc))));
    let working = cursor
        .rules
        .iter()
        .find(|r| r.id == "screen_working")
        .expect("screen_working rule");
    assert!(working.priority > ghost.priority);
}

/// MEASURED 2026-08-05, SAFETY: answering Cursor's workspace-trust dialog
/// does NOT clear it from the screen. "Do you trust the contents of this
/// directory?" is still on the pane long after the agent is idle and taking
/// prompts, so a rule matching the question alone reports blocked_modal
/// forever and parks every delivery to a healthy pane. The '▶' selection
/// marker is present only while the dialog owns the keyboard.
#[test]
fn cursor_trust_dialog_ignores_one_already_answered() {
    let all = shipped();
    let cursor = &all["cursor"];
    let live = include_str!("fixtures/cursor_trust_dialog.txt");
    // Captured after pressing [a]: the agent is idle and usable, and the
    // dialog's text is still on screen above the composer.
    let answered = include_str!("fixtures/cursor_ghost_composer_plain.txt");

    let r = cursor.evaluate("Cursor Agent", live).unwrap();
    assert_eq!(r.id, "trust_dialog");
    assert_eq!(r.state, AgentState::BlockedModal);
    assert!(
        !r.auto_dismiss,
        "declining quits the CLI; trust is a human decision"
    );

    // The answered capture still carries the question, so the second clause
    // is what does the work here, not the region size.
    assert!(
        answered.contains("Do you trust the contents of this directory"),
        "fixture no longer exercises the stale-dialog hazard"
    );
    let r = cursor.evaluate("Cursor Agent", answered).unwrap();
    assert_eq!(
        r.state,
        AgentState::Idle,
        "an answered dialog is not a modal"
    );
}

/// MEASURED 2026-08-05: Cursor renders the approval prompt's selection
/// marker with the SAME '→' glyph the composer uses, so on a plain capture
/// the composer rules also match an approval screen. The approval rule
/// outranking them is what keeps a pane waiting on a human from reading as
/// idle_with_input and taking a delivery.
#[test]
fn cursor_approval_outranks_the_shared_arrow_glyph() {
    let all = shipped();
    let cursor = &all["cursor"];
    let prompt = include_str!("fixtures/cursor_approval_prompt.txt");

    let r = cursor.evaluate("Cursor Agent", prompt).unwrap();
    assert_eq!(r.id, "approval_prompt");
    assert_eq!(r.state, AgentState::BlockedPermission);
    assert!(!r.auto_dismiss, "an approval belongs to the human");

    let fallback = cursor
        .rules
        .iter()
        .find(|r| r.id == "composer_plain_fallback")
        .expect("composer_plain_fallback rule");
    assert!(fallback.matches(prompt, &non_empty(prompt)));
    let approval = cursor
        .rules
        .iter()
        .find(|r| r.id == "approval_prompt")
        .expect("approval_prompt rule");
    assert!(approval.priority > fallback.priority);
}

/// MEASURED 2026-08-05: an empty Cursor composer renders a placeholder whose
/// dim (ESC[2m) starts AT the glyph — the opposite arrangement from Codex,
/// where the glyph is bold and only the ghost text after it is dim. The
/// placeholder text itself changes between the first turn and later ones
/// ("Plan, search, build anything" then "Add a follow-up"), so only the dim
/// glyph is safe to match.
#[test]
fn cursor_ghost_vs_typed_probed_fixtures() {
    let all = shipped();
    let cursor = &all["cursor"];
    let ghost_plain = include_str!("fixtures/cursor_ghost_composer_plain.txt");
    let ghost_esc = include_str!("fixtures/cursor_ghost_composer_esc.txt");
    let typed_plain = include_str!("fixtures/cursor_typed_composer_plain.txt");
    let typed_esc = include_str!("fixtures/cursor_typed_composer_esc.txt");

    let r = cursor
        .evaluate_esc("Cursor Agent", ghost_plain, Some(ghost_esc))
        .unwrap();
    assert_eq!(r.id, "composer_ghost_or_empty");
    assert_eq!(r.state, AgentState::Idle);

    let r = cursor
        .evaluate_esc("Cursor Agent", typed_plain, Some(typed_esc))
        .unwrap();
    assert_eq!(r.id, "composer_typed_input");
    assert_eq!(r.state, AgentState::IdleWithInput);

    // Without an escaped capture the esc rules fail closed and the plain
    // fallback calls typed text idle. Same residual gap as codex, recorded
    // here so a future change to the daemon's capture path is noticed.
    let r = cursor.evaluate("Cursor Agent", typed_plain).unwrap();
    assert_eq!(r.id, "composer_plain_fallback");
    assert_eq!(r.state, AgentState::Idle);
}

/// Every shipped trailer pattern must match the chrome captured from a real
/// session, and must NOT match ordinary payload text. A trailer regex that
/// matches payload would let content after the sentinel pass as chrome,
/// which is the one direction that is unsafe.
#[test]
fn shipped_composer_trailers_match_captured_chrome_only() {
    let cases: &[(&str, &[&str], &[&str])] = &[
        (
            "claude",
            &[
                "────────────────────────",
                "  Opus 5 · xhigh · ~/projects/clops · Ctx: 97% · 5h: 93% · 1000K window · 28K used",
                "  Fable 5 · xhigh · ~/projects/agentic_dev/clops · Ctx: 58% · 5h: 94% · 7d: 75% · 1000K window · 423K used",
                "  Opus 5 (1M context) · xhigh · ~/projects/agentic_dev/cy…",
                "  Opus 5 (1M context) · xhigh · ~/projects/agentic_dev/cyclops-worktrees/mess…",
                "  Opus 5 (1M context) · xhigh · ~/projects/agentic_dev/cyclops-worktrees/messaging-integration · …",
                "  Opus 5 (1M context) · xhigh · ~/projects/agentic_dev/cyclops-worktrees/messaging-integration · Ctx: 95% · 5h: 93% · 7d: …",
                "  paste again to expand",
                "  ⏸ manual mode on · ← for agents",
            ],
            &["review the auth change", "[cyclops:end m-1]", "· a line with a dot ·"],
        ),
        (
            "codex",
            &[
                "  gpt-5.6-sol xhigh · ~ · Full Access · Context 87% left · weekly 97% left",
                "  gpt-5.6-sol high · /tmp/x · 258K window · 2.87M used",
            ],
            &["please review this", "[cyclops:end m-1]", "a · b · c"],
        ),
        (
            "agy",
            &[
                "────────────────────────",
                "Gemini 3.7 Flash · High · ~ · Full · Ctx: 79% · 97% 5h, 83% wk · (195K / 1048K)",
            ],
            &["run the tests", "[cyclops:end m-1]", "Gemini said something"],
        ),
    ];
    for (id, chrome, payload) in cases {
        let m = &shipped()[*id];
        assert!(
            !m.composer_trailers.is_empty(),
            "{id}: no trailer patterns shipped"
        );
        for line in *chrome {
            assert!(
                m.composer_trailers.iter().any(|r| r.is_match(line)),
                "{id}: captured chrome unmatched: {line:?}"
            );
        }
        for line in *payload {
            assert!(
                !m.composer_trailers.iter().any(|r| r.is_match(line)),
                "{id}: payload text matched a trailer pattern: {line:?}"
            );
        }
    }
}

/// Issue 16, the destructive direction of F55: on Claude Code 2.1.232 a
/// typed slash command renders STYLED
/// ('ESC[39m' + glyph + NBSP + 'ESC[38;5;153m/model'), which the
/// staged-input clause cannot see, and a ghost rule that claimed all
/// styling read it as idle. The gate then pasted into the human's draft
/// and submitted it. Styling that is not provably the dim ghost must hold.
#[test]
fn claude_typed_slash_command_is_never_idle() {
    let claude = &shipped()["claude"];
    let plain = "transcript\n────────\n❯ /model\n────────\n  Opus 5 · 1000K window";
    let esc = "transcript\n────────\n\u{1b}[39m❯\u{a0}\u{1b}[38;5;153m/model\u{1b}[39m\n────────\n  Opus 5 · 1000K window";
    let r = claude.evaluate_esc("proj", plain, Some(esc)).unwrap();
    assert_eq!(
        r.state,
        AgentState::IdleWithInput,
        "a typed slash command must hold, not invite a paste (rule {})",
        r.id
    );
    assert_eq!(r.id, "composer_styled_input");

    // The dim ghost convention still reads idle: F55's benefit is intact.
    let ghost_esc = "transcript\n────────\n\u{1b}[39m❯\u{a0}\u{1b}[2mTry \"fix the tests\"\u{1b}[0m\n────────\n  Opus 5 · 1000K window";
    let ghost_plain =
        "transcript\n────────\n❯ Try \"fix the tests\"\n────────\n  Opus 5 · 1000K window";
    let r = claude
        .evaluate_esc("proj", ghost_plain, Some(ghost_esc))
        .unwrap();
    assert_eq!(r.id, "composer_ghost_suggestion");
    assert_eq!(r.state, AgentState::Idle);

    // Unstyled typed text is unchanged: the staged clause still wins.
    let typed_esc =
        "transcript\n────────\n\u{1b}[39m❯\u{a0}fix the parser\n────────\n  Opus 5 · 1000K window";
    let typed_plain = "transcript\n────────\n❯ fix the parser\n────────\n  Opus 5 · 1000K window";
    let r = claude
        .evaluate_esc("proj", typed_plain, Some(typed_esc))
        .unwrap();
    assert_eq!(r.id, "composer_has_staged_input");
    assert_eq!(r.state, AgentState::IdleWithInput);
}

/// A trailer pattern that also matches ordinary prose would let payload
/// text after the sentinel pass as chrome, and a truncated paste would then
/// be submitted. These are adversarial lines derived from each shipped
/// pattern: text a person could plausibly write, shaped as closely as prose
/// gets to a status row.
#[test]
fn shipped_trailers_reject_adversarial_payload_text() {
    let adversarial = [
        // Derived from the "N K window" patterns.
        "please use a 128K window",
        "we should bump it to a 200K window and retest",
        "context · budget · 128K window",
        // Derived from the codex "Context N% left" pattern.
        "Context 50% left is not enough for this refactor",
        "note · warning · Context 12% left",
        // Derived from the agy status row.
        "Gemini said · the answer · Ctx: 50%",
        "Gemini 3.7 · Ctx: 90%",
        "Opus 5 said · xhigh · this is prose…",
        // Derived from the hint rows.
        "paste again to expand the section",
        "? for shortcuts, otherwise read the guide",
        "⏸ pause the run before merging",
        // Box-rule lookalikes that are not a full rule row.
        "──── section heading ────",
        "see ─ the dash above",
    ];
    for id in ["claude", "codex", "agy"] {
        let m = &shipped()[id];
        for line in adversarial {
            assert!(
                !m.composer_trailers.iter().any(|r| r.is_match(line.trim())),
                "{id}: payload text would be treated as chrome: {line:?}"
            );
        }
    }
}

/// On Claude Code 2.1.236 an active turn keeps the idle sparkle title and
/// the composer prompt, so
/// title and screen agreed on idle and the gate could admit a write into
/// a pane mid-generation.
///
/// The fixture is minimized from a live escaped capture of a running tool
/// task. Transcript prose above the evidence window is neutralized while
/// row count, styling, and the active status and composer rows are retained.
/// Its plain sibling is derived here so both forms describe one moment.
#[test]
fn claude_active_status_row_reads_working_not_idle() {
    let claude = &shipped()["claude"];
    let esc = include_str!("fixtures/claude_working_2_1_236_esc.txt");
    let plain = strip_sgr(esc);
    let r = claude
        .evaluate_esc("\u{2733} Cooking", &plain, Some(esc))
        .unwrap();
    assert_eq!(
        r.state,
        AgentState::Working,
        "an active turn must not read idle (rule {})",
        r.id
    );
    assert_eq!(r.id, "composer_working_spinner_status");

    // A completed step keeps its words on screen forever, so the
    // discriminator has to be the styling rather than the verb: uniform
    // gray, no running timer, and the pane is free again.
    let done_esc = "\u{1b}[38;5;246m⏺ Cooked for 28m 7s\u{1b}[39m\n\u{1b}[38;5;244m────────\n\u{1b}[39m❯\u{a0}\n\u{1b}[39m  \u{1b}[38;5;174mOpus 5\u{1b}[38;5;246m · 1000K window";
    let r = claude
        .evaluate_esc("\u{2733} Done", &strip_sgr(done_esc), Some(done_esc))
        .unwrap();
    assert_ne!(
        r.state,
        AgentState::Working,
        "a completed step must not pin the pane as working forever"
    );
}

/// Claude Code 2.1.243 emits a plain activity row when NO_COLOR is set.
/// The cycling glyph and ellipsis identify the live row. Completed transcript
/// rows use a past-tense result followed by "for" and must not hold the pane.
#[test]
fn claude_no_color_active_status_reads_working() {
    let claude = &shipped()["claude"];
    let grid = include_str!("fixtures/claude_working_no_color_2_1_243.txt");
    assert!(!grid.contains('\u{1b}'));

    let active = claude
        .evaluate_esc("\u{2733} Hook self-test", grid, Some(grid))
        .unwrap();
    assert_eq!(active.id, "composer_working_spinner_status_no_color");
    assert_eq!(active.state, AgentState::Working);
    assert!(
        !claude
            .rules
            .iter()
            .find(|rule| rule.id == active.id)
            .expect("matched rule is compiled")
            .lifecycle_evidence
    );

    let update = grid.replace(
        "  ⎿  Tip: Use /permissions to pre-approve and pre-deny bash, edit, and MCP tools",
        "  ✔ Update installed · Restart to update",
    );
    let update_reading = claude
        .evaluate_esc("\u{2733} Hook self-test", &update, Some(&update))
        .unwrap();
    assert_eq!(
        update_reading.id,
        "composer_working_spinner_status_no_color"
    );
    assert_eq!(update_reading.state, AgentState::Working);

    let mode = "  ⏵⏵ bypass permissions on (shift+tab to cycle)";
    let status_only = grid.replace(&format!("\n{mode}"), "");
    let hint_only = status_only.replace(
        "  Haiku 4.5 · low · Ctx: 76% · 200K window · 47K used",
        "  Haiku 4.5 · low · Ctx: 76% · 200K window · 47K used\n  paste again to expand",
    );
    let hint_and_mode = grid.replace(
        mode,
        "  paste again to expand\n  ⏵⏵ bypass permissions on (shift+tab to cycle)",
    );
    for variant in [status_only, hint_only, grid.to_string(), hint_and_mode] {
        let reading = claude
            .evaluate_esc("\u{2733} Hook self-test", &variant, Some(&variant))
            .unwrap();
        assert_eq!(reading.id, "composer_working_spinner_status_no_color");
        assert_eq!(reading.state, AgentState::Working);
    }

    for completed in ["\u{273b} Brewed for 12s", "\u{2733} Cooked for 2s"] {
        let done = grid.replace("\u{2733} Flambéing… (1s · ↓ 90 tokens)", completed);
        let reading = claude
            .evaluate_esc("\u{2733} Hook self-test", &done, Some(&done))
            .unwrap();
        assert_ne!(
            reading.state,
            AgentState::Working,
            "a completed transcript row must not hold the pane: {completed:?}"
        );
    }

    let staged = include_str!("fixtures/claude_staged_no_color_2_1_243.txt")
        .replace("  Reply not needed.", "  ✳ Flambéing… (1s)");
    let reading = claude
        .evaluate_esc("\u{2733} Hook self-test", &staged, Some(&staged))
        .unwrap();
    assert_eq!(
        reading.state,
        AgentState::IdleWithInput,
        "spinner-shaped payload text must not visually accept a blocked prompt"
    );
    assert_eq!(reading.id, "composer_unstyled_input");

    let long_body = (0..12)
        .map(|index| format!("  draft line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let long_staged = include_str!("fixtures/claude_staged_no_color_2_1_243.txt")
        .replace("  Reply not needed.", &long_body);
    let long_reading = claude
        .evaluate_esc("\u{2733} Hook self-test", &long_staged, Some(&long_staged))
        .unwrap();
    assert_eq!(long_reading.state, AgentState::IdleWithInput);
    assert_eq!(long_reading.id, "composer_unstyled_input");

    let idle_with_echo = concat!(
        "✳ Flambéing… (1s)\n",
        "❯ previous submitted prompt\n",
        "  completed answer\n",
        "────────────────────────────────────────────────────────────────\n",
        "❯\u{a0}\n",
        "────────────────────────────────────────────────────────────────\n",
        "  Haiku 4.5 · low · Ctx: 76% · 200K window · 47K used\n",
        "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
    );
    let idle = claude
        .evaluate_esc(
            "\u{2733} Hook self-test",
            idle_with_echo,
            Some(idle_with_echo),
        )
        .unwrap();
    assert_eq!(
        idle.state,
        AgentState::Idle,
        "a transcript echo must not look like composer input (rule {})",
        idle.id
    );
    assert_eq!(idle.id, "title_idle_sparkle");
}

/// De-escape a capture the way the daemon does before matching plain
/// patterns, so a fixture can be its own plain sibling.
fn strip_sgr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('\u{1b}') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match rest.find('m') {
            Some(end) => rest = &rest[end + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}
