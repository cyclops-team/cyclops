//! Gate 7 Stage-and-Clear Evidence Soak Component.
//!
//! Narrow Scope: Validates Doorbell Format 3 staging representation detection
//! and composer-clear representation against dynamically installed AI agent CLIs.
//! This harness is strictly one component of Gate 7 evidence, not the complete
//! Gate 7 certification or frozen campaign closure.
//!
//! Operational Invariants:
//! 1. Privacy: Reports and harness output contain no raw prompt, token, or
//!    captured text. Vendor state is confined to the ephemeral scratch home.
//! 2. Opt-in: Live execution is strictly gated behind #[ignore] and
//!    `CYCLOPS_LIVE_VENDOR=1` plus a caller-supplied 40-character frozen SHA.
//! 3. Zero Mutation: Ordinary tests validate schemas and guards without
//!    invoking vendor binaries or modifying committed evidence.
//! 4. F24 Isolation: Ephemeral scratch directories rooted via `cyclops_proto::scratch::scratch_dir`.
//! 5. Production Parser: Staging and clear representations are classified by
//!    the daemon's exact screen parser. This component never authorizes a
//!    delivery and does not replace process binding or the full injection gate.
//! 6. Fail-Closed: Immediate defect abortion with non-zero test exit on failure.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use cyclops_manifest::{load_dir, Manifest};
use cyclops_proto::scratch::{scratch_dir, scratch_root};
use cyclops_proto::{render_doorbell_v3, NotificationAttemptId};
use cyclops_testrig::TmuxServer;
use cyclopsd::{prove_composer_representation, ComposerRepresentationProof};
use serde::{Deserialize, Serialize};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
const STATE_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Schema Definitions (validation/raw/soak/stage_and_clear_soak_component_report.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CampaignVerdict {
    Passed,
    Failed,
    Limitation,
    AdminDecision,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClosedReasonCode {
    AllTrialsVerified,
    ClearKeysUnsupportedManualOnly,
    VerificationMismatch,
    ClearFailed,
    InitialIdleTimeout,
    VendorLaunchFailed,
    StageCommandFailed,
    ClearCommandFailed,
    HarnessAborted,
    VendorUnavailable,
    OptInRequired,
    BinaryIdentityMismatch,
    RequiredVendorLimitation,
    AdminDecisionRequired,
    OutOfScopeIntegrationSuite,
    OutOfScopeOfflineFixture,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstallationStatus {
    InstalledLive,
    UnavailableOfflineGate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitCommitMetadata {
    pub sha: String,
    pub tree_clean: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostPlatformMetadata {
    pub os: String,
    pub arch: String,
    pub platform_tuple: String,
    pub kernel_release: String,
    pub tmux_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CampaignMetadata {
    pub campaign_name: String,
    pub opt_in_flag: String,
    pub metric_scope: String,
    pub total_trials: usize,
    pub total_duration_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredVersionInfo {
    pub semver_parsed: String,
    pub presence: InstallationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ProofCounts {
    pub exact_row_proof: usize,
    pub sentinel_proof: usize,
    pub collapsed_chip: usize,
    pub structural_trailer: usize,
    pub safe_clear_representations: usize,
    pub failures: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VendorSummary {
    pub vendor_id: String,
    pub installation_status: InstallationStatus,
    pub discovered_version: DiscoveredVersionInfo,
    pub version_claim_manifest: String,
    pub total_trials: usize,
    pub widths_tested: Vec<u16>,
    pub proof_counts: ProofCounts,
    pub verdict: CampaignVerdict,
    pub reason_code: ClosedReasonCode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuralInputMetrics {
    pub char_length: usize,
    pub byte_length: usize,
    pub line_count: usize,
    pub is_multiline: bool,
    pub has_unicode: bool,
    pub has_code_fence: bool,
    pub contains_blank_lines: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LatencyMetricsMs {
    pub staging_detection_ms: f64,
    pub clear_teardown_ms: f64,
    pub total_wall_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationRecord {
    pub trial_id: String,
    pub vendor_id: String,
    pub timestamp: String,
    pub git_commit_sha: String,
    pub platform_tuple: String,
    pub discovered_version: String,
    pub terminal_dimensions: (u16, u16),
    pub structural_input_metrics: StructuralInputMetrics,
    pub proof_classification: ComposerRepresentationProof,
    pub clear_representation_verified: bool,
    pub latency_metrics_ms: LatencyMetricsMs,
    pub verdict: CampaignVerdict,
    pub reason_code: ClosedReasonCode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Gate7CellStatus {
    pub cell_id: String,
    pub description: String,
    pub coverage_tier: String,
    pub verdict: Option<CampaignVerdict>,
    pub reason_code: ClosedReasonCode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StageAndClearComponentReport {
    pub schema_version: String,
    pub report_id: String,
    pub generated_at: String,
    pub component_scope: String,
    pub git_commit: GitCommitMetadata,
    pub host_platform: HostPlatformMetadata,
    pub campaign_metadata: CampaignMetadata,
    pub component_verdict: CampaignVerdict,
    pub component_reason_code: ClosedReasonCode,
    pub gate7_closed_cell_inventory: Vec<Gate7CellStatus>,
    pub vendor_summaries: Vec<VendorSummary>,
    pub observations: Vec<ObservationRecord>,
}

struct FailureArtifactGuard {
    path: PathBuf,
    report: Option<StageAndClearComponentReport>,
}

impl FailureArtifactGuard {
    fn new(path: PathBuf, report: StageAndClearComponentReport) -> Self {
        Self {
            path,
            report: Some(report),
        }
    }

    fn disarm(&mut self) {
        self.report = None;
    }
}

impl Drop for FailureArtifactGuard {
    fn drop(&mut self) {
        let Some(report) = self.report.take() else {
            return;
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

// ---------------------------------------------------------------------------
// RAII Temp Directory Root (F24 Scoped)
// ---------------------------------------------------------------------------

pub struct Gate7ScratchHome {
    path: PathBuf,
}

impl Gate7ScratchHome {
    pub fn new() -> Result<Self, std::io::Error> {
        let path = scratch_dir(&format!("cyc-gate7-soak-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Gate7ScratchHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn isolated_vendor_environment(home: &Path) -> Vec<(String, String)> {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    [
        ("PATH", PathBuf::from(path)),
        ("HOME", home.to_path_buf()),
        ("CYCLOPS_HOME", home.join(".cyclops")),
        ("CODEX_HOME", home.join(".codex")),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_STATE_HOME", home.join(".local/state")),
        ("XDG_CACHE_HOME", home.join(".cache")),
        ("XDG_DATA_HOME", home.join(".local/share")),
        ("XDG_RUNTIME_DIR", home.join(".runtime")),
        ("TMPDIR", home.join("tmp")),
        ("TERM", PathBuf::from("xterm-256color")),
        ("LANG", PathBuf::from("en_US.UTF-8")),
        ("LC_ALL", PathBuf::from("en_US.UTF-8")),
        ("SHELL", PathBuf::from("/bin/sh")),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string_lossy().into_owned()))
    .collect()
}

fn prepare_isolated_vendor_home(home: &Path) -> Result<(), std::io::Error> {
    for path in [
        home.to_path_buf(),
        home.join(".cyclops"),
        home.join(".codex"),
        home.join(".config"),
        home.join(".local/state"),
        home.join(".cache"),
        home.join(".local/share"),
        home.join(".runtime"),
        home.join("tmp"),
    ] {
        std::fs::create_dir_all(path)?;
    }
    Ok(())
}

fn apply_isolated_vendor_environment(command: &mut Command, home: &Path) {
    command.env_clear();
    for (key, value) in isolated_vendor_environment(home) {
        command.env(key, value);
    }
}

fn isolated_vendor_launch_command(home: &Path, launch_command: &str) -> String {
    let environment = isolated_vendor_environment(home)
        .into_iter()
        .map(|(key, value)| format!("{key}={}", sh_quote(&value)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "exec /usr/bin/env -i {environment} /bin/sh -c {}",
        sh_quote(launch_command)
    )
}

// ---------------------------------------------------------------------------
// Validation & Verification Machinery
// ---------------------------------------------------------------------------

/// Computes current UTC timestamp in ISO 8601 format.
pub fn utc_now_iso8601() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs();
    let secs = total_secs % 60;
    let total_mins = total_secs / 60;
    let mins = total_mins % 60;
    let total_hours = total_mins / 60;
    let hours = total_hours % 24;
    let mut days = (total_hours / 24) as i64;

    let mut year = 1970;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let days_in_year = if leap { 366 } else { 365 };
        if days >= days_in_year {
            days -= days_in_year;
            year += 1;
        } else {
            break;
        }
    }

    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for &d in &month_days {
        if days >= d {
            days -= d;
            month += 1;
        } else {
            break;
        }
    }
    let day = days + 1;

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:{secs:02}Z")
}

/// Validates that a supplied string is an exact 40-character lowercase hex SHA.
pub fn validate_sha40(sha: &str) -> Result<(), String> {
    if sha.len() != 40 {
        return Err(format!(
            "SHA must be exactly 40 characters, found length {}",
            sha.len()
        ));
    }
    if !sha
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err("SHA must consist strictly of lowercase hexadecimal characters".to_string());
    }
    Ok(())
}

/// Verifies that the current Git repository matches the caller-supplied SHA
/// and has no uncommitted changes.
pub fn verify_checkout_integrity(expected_sha: &str) -> Result<GitCommitMetadata, String> {
    validate_sha40(expected_sha)?;

    let head_out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("failed to run git rev-parse HEAD: {e}"))?;

    if !head_out.status.success() {
        return Err("git rev-parse HEAD exited with failure status".to_string());
    }

    let actual_sha = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
    if actual_sha != expected_sha {
        return Err(format!(
            "checkout SHA mismatch: expected {expected_sha}, actual HEAD is {actual_sha}"
        ));
    }

    let status_out = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("failed to run git status --porcelain: {e}"))?;

    let status_str = String::from_utf8_lossy(&status_out.stdout);
    let tree_clean = status_str.trim().is_empty();
    if !tree_clean {
        return Err(
            "repository working tree is dirty; campaign requires a clean checkout".to_string(),
        );
    }

    Ok(GitCommitMetadata {
        sha: actual_sha,
        tree_clean,
    })
}

/// Pure verifier for the exact build identity against a frozen commit.
pub fn verify_build_id_pure(build_id: &str, expected_sha: &str) -> Result<(), String> {
    validate_sha40(expected_sha)?;
    if build_id.is_empty() || build_id == "unknown" {
        return Err("build identity is empty or unknown".to_string());
    }
    if build_id.contains(".dirty") {
        return Err("build identity indicates dirty sources".to_string());
    }
    if build_id != expected_sha {
        return Err(format!(
            "build identity {build_id} does not exactly match frozen SHA {expected_sha}"
        ));
    }
    Ok(())
}

/// Verifies that the compiled library/test build ref matches the caller SHA.
pub fn verify_build_ref_integrity(expected_sha: &str) -> Result<GitCommitMetadata, String> {
    let meta = verify_checkout_integrity(expected_sha)?;
    verify_build_id_pure(env!("CYCLOPS_BUILD_ID"), expected_sha)?;
    Ok(meta)
}

/// Probes dynamic installed CLI versions on the host.
pub fn discover_installed_vendor(
    vendor_id: &str,
    manifest: Option<&Manifest>,
    isolated_home: &Path,
) -> (InstallationStatus, DiscoveredVersionInfo) {
    let cmd = match vendor_id {
        "codex" => "codex",
        "claude" => "claude",
        "agy" => "agy",
        "cursor" => manifest
            .and_then(|m| m.agent.launch.as_deref())
            .unwrap_or("cursor-agent"),
        other => other,
    };

    let mut version_command = Command::new(cmd);
    version_command.arg("--version");
    apply_isolated_vendor_environment(&mut version_command, isolated_home);
    let ver_out = version_command.output();
    match ver_out {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let semver = raw
                .split_whitespace()
                .find(|w| w.chars().any(|c| c.is_ascii_digit()))
                .unwrap_or("unknown")
                .trim_matches(|c: char| !c.is_ascii_digit() && c != '.')
                .to_string();
            (
                InstallationStatus::InstalledLive,
                DiscoveredVersionInfo {
                    semver_parsed: semver,
                    presence: InstallationStatus::InstalledLive,
                },
            )
        }
        _ => (
            InstallationStatus::UnavailableOfflineGate,
            DiscoveredVersionInfo {
                semver_parsed: "none".to_string(),
                presence: InstallationStatus::UnavailableOfflineGate,
            },
        ),
    }
}

/// Discovers host platform tuple and tmux version.
pub fn discover_host_platform() -> HostPlatformMetadata {
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();
    let platform_tuple = format!("{os}-{arch}");

    let uname_out = Command::new("uname").arg("-r").output();
    let kernel_release = match uname_out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    };

    let tmux_out = Command::new("tmux").arg("-V").output();
    let tmux_version = match tmux_out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    };

    HostPlatformMetadata {
        os,
        arch,
        platform_tuple,
        kernel_release,
        tmux_version,
    }
}

/// Generates content-free structural metrics for an input string.
pub fn structural_metrics_for(input: &str) -> StructuralInputMetrics {
    let char_length = input.chars().count();
    let byte_length = input.len();
    let line_count = input.lines().count().max(1);
    let is_multiline = line_count > 1;
    let has_unicode = !input.is_ascii();
    let has_code_fence = input.contains("```");
    let contains_blank_lines = input.lines().any(|l| l.trim().is_empty());

    StructuralInputMetrics {
        char_length,
        byte_length,
        line_count,
        is_multiline,
        has_unicode,
        has_code_fence,
        contains_blank_lines,
    }
}

#[allow(clippy::too_many_arguments)]
fn trial_observation(
    vendor_id: &str,
    trial_index: usize,
    width: u16,
    commit_sha: &str,
    platform_tuple: &str,
    discovered_version: &str,
    structural_input_metrics: StructuralInputMetrics,
    proof_classification: ComposerRepresentationProof,
    clear_representation_verified: bool,
    staging_detection_ms: f64,
    clear_teardown_ms: f64,
    total_wall_ms: f64,
    verdict: CampaignVerdict,
    reason_code: ClosedReasonCode,
) -> ObservationRecord {
    ObservationRecord {
        trial_id: format!("obs-{vendor_id}-{trial_index}"),
        vendor_id: vendor_id.to_string(),
        timestamp: utc_now_iso8601(),
        git_commit_sha: commit_sha.to_string(),
        platform_tuple: platform_tuple.to_string(),
        discovered_version: discovered_version.to_string(),
        terminal_dimensions: (width, 24),
        structural_input_metrics,
        proof_classification,
        clear_representation_verified,
        latency_metrics_ms: LatencyMetricsMs {
            staging_detection_ms,
            clear_teardown_ms,
            total_wall_ms,
        },
        verdict,
        reason_code,
    }
}

fn validate_report_consistency(report: &StageAndClearComponentReport) -> Result<(), String> {
    let reason_matches_verdict = match report.component_verdict {
        CampaignVerdict::Passed => {
            report.component_reason_code == ClosedReasonCode::AllTrialsVerified
        }
        CampaignVerdict::Failed => !matches!(
            report.component_reason_code,
            ClosedReasonCode::AllTrialsVerified
                | ClosedReasonCode::RequiredVendorLimitation
                | ClosedReasonCode::AdminDecisionRequired
        ),
        CampaignVerdict::Limitation => {
            report.component_reason_code == ClosedReasonCode::RequiredVendorLimitation
        }
        CampaignVerdict::AdminDecision => {
            report.component_reason_code == ClosedReasonCode::AdminDecisionRequired
        }
    };
    if !reason_matches_verdict {
        return Err("component reason code contradicts component verdict".to_string());
    }

    if report.campaign_metadata.total_trials != report.observations.len() {
        return Err("campaign total_trials does not equal observation count".to_string());
    }

    for summary in &report.vendor_summaries {
        let observations: Vec<_> = report
            .observations
            .iter()
            .filter(|observation| observation.vendor_id == summary.vendor_id)
            .collect();
        if summary.total_trials != observations.len() {
            return Err(format!(
                "vendor {} total_trials does not equal observation count",
                summary.vendor_id
            ));
        }

        let mut observed_widths: Vec<u16> = observations
            .iter()
            .map(|observation| observation.terminal_dimensions.0)
            .collect();
        observed_widths.sort_unstable();
        observed_widths.dedup();
        if summary.widths_tested != observed_widths {
            return Err(format!(
                "vendor {} widths_tested does not equal attempted widths",
                summary.vendor_id
            ));
        }

        if summary.verdict == CampaignVerdict::Failed
            && !observations
                .iter()
                .any(|observation| observation.verdict == CampaignVerdict::Failed)
        {
            return Err(format!(
                "vendor {} failed without a failed observation",
                summary.vendor_id
            ));
        }
    }

    Ok(())
}

/// Pure opt-in value checker.
pub fn check_opt_in_value(val: Option<&str>) -> Result<(), ClosedReasonCode> {
    if val == Some("1") {
        Ok(())
    } else {
        Err(ClosedReasonCode::OptInRequired)
    }
}

/// Recursively verifies that a serde_json::Value contains none of the prohibited sentinels.
pub fn assert_no_sentinels(val: &serde_json::Value, sentinels: &[&str]) {
    match val {
        serde_json::Value::String(s) => {
            for &sentinel in sentinels {
                assert!(
                    !s.contains(sentinel),
                    "privacy leak violation: serialized JSON string contained prohibited pattern"
                );
            }
        }
        serde_json::Value::Array(arr) => {
            for elem in arr {
                assert_no_sentinels(elem, sentinels);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                for &sentinel in sentinels {
                    assert!(
                        !k.contains(sentinel),
                        "privacy leak violation: serialized JSON key contained prohibited pattern"
                    );
                }
                assert_no_sentinels(v, sentinels);
            }
        }
        _ => {}
    }
}

/// Builds the single cell this component is capable of measuring.
///
/// The full Gate 7 inventory belongs to the frozen-candidate campaign. Keeping
/// unrelated cells out of this artifact prevents an unexecuted requirement
/// from looking certified merely because it appeared in a component report.
pub fn build_gate7_cell_inventory(stage_cell_verdict: CampaignVerdict) -> Vec<Gate7CellStatus> {
    let stage_reason = match stage_cell_verdict {
        CampaignVerdict::Passed => ClosedReasonCode::AllTrialsVerified,
        CampaignVerdict::Failed => ClosedReasonCode::VerificationMismatch,
        CampaignVerdict::Limitation => ClosedReasonCode::RequiredVendorLimitation,
        CampaignVerdict::AdminDecision => ClosedReasonCode::AdminDecisionRequired,
    };

    vec![Gate7CellStatus {
        cell_id: "g7_stage_and_clear_representation".to_string(),
        description: "Live Doorbell Format 3 staging and composer-clear screen representation"
            .to_string(),
        coverage_tier: "STAGE_CLEAR_COMPONENT".to_string(),
        verdict: Some(stage_cell_verdict),
        reason_code: stage_reason,
    }]
}

/// Evaluates overall component verdict given individual vendor summaries.
pub fn compute_component_verdict(summaries: &[VendorSummary]) -> CampaignVerdict {
    if summaries.is_empty() {
        return CampaignVerdict::Limitation;
    }
    if summaries
        .iter()
        .any(|s| s.verdict == CampaignVerdict::Failed)
    {
        return CampaignVerdict::Failed;
    }

    // All required live vendors (including cursor) must be InstalledLive, total_trials > 0, and Passed
    let required_vendors = ["codex", "claude", "agy", "cursor"];
    let mut all_required_passed = true;
    for &req in &required_vendors {
        let found = summaries.iter().find(|s| s.vendor_id == req);
        match found {
            Some(s)
                if s.installation_status == InstallationStatus::InstalledLive
                    && s.total_trials > 0
                    && s.verdict == CampaignVerdict::Passed => {}
            _ => {
                all_required_passed = false;
                break;
            }
        }
    }

    if all_required_passed {
        CampaignVerdict::Passed
    } else {
        CampaignVerdict::Limitation
    }
}

/// Pure campaign controller driving fail-fast trial sequencing across vendors.
pub fn run_campaign_controller<F>(
    vendors: &[&str],
    mut run_vendor_trials: F,
) -> (CampaignVerdict, Vec<VendorSummary>, Vec<ObservationRecord>)
where
    F: FnMut(&str) -> (VendorSummary, Vec<ObservationRecord>, bool),
{
    let mut vendor_summaries = Vec::new();
    let mut observations = Vec::new();
    let mut fatal_error = false;

    for &v in vendors {
        let (summary, obs, failed) = run_vendor_trials(v);
        vendor_summaries.push(summary);
        observations.extend(obs);
        if failed {
            fatal_error = true;
            break; // Fail-fast: Stop running further vendors immediately
        }
    }

    let verdict = if fatal_error {
        CampaignVerdict::Failed
    } else {
        compute_component_verdict(&vendor_summaries)
    };

    (verdict, vendor_summaries, observations)
}

/// Bounded condition poll matching the testrig measured interval pattern.
pub fn wait_condition<F>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if predicate() {
            return true;
        }
        std::thread::sleep(SAMPLE_INTERVAL);
    }
    predicate()
}

fn representation_is_write_safe(proof: ComposerRepresentationProof) -> bool {
    matches!(
        proof,
        ComposerRepresentationProof::WriteSafeClean | ComposerRepresentationProof::WriteSafeGhost
    )
}

fn has_measured_clear_action(manifest: &Manifest) -> bool {
    !manifest.injection.clear_keys.is_empty()
}

// ---------------------------------------------------------------------------
// Unit Tests for Harness Safety, Schema & Guards (Run in Ordinary Test Suite)
// ---------------------------------------------------------------------------

#[test]
fn test_sha40_validation_rules() {
    assert!(validate_sha40("5ee0f17e00000000000000000000000000000000").is_ok());
    assert!(validate_sha40("5ee0f17").is_err()); // short
    assert!(validate_sha40("5EE0F17E00000000000000000000000000000000").is_err()); // uppercase
    assert!(validate_sha40("5ee0f17e0000000000000000000000000000000g").is_err());
    // non-hex
}

#[test]
fn test_pure_build_identity_verifier_requires_exact_clean_sha() {
    let sha = "5ee0f17e00000000000000000000000000000000";
    assert!(verify_build_id_pure(sha, sha).is_ok());
    assert!(verify_build_id_pure("5ee0f17", sha).is_err());
    assert!(verify_build_id_pure("5ee0f17e00000000000000000000000000000000.dirty", sha).is_err());
    assert!(verify_build_id_pure("unknown", sha).is_err());
    assert!(verify_build_id_pure("", sha).is_err());
    assert!(verify_build_id_pure("deadbeefe00000000000000000000000000000000", sha).is_err());
}

#[test]
fn test_content_free_structural_metrics() {
    let metrics = structural_metrics_for("hello world\n```rust\nfn main() {}\n```\n");
    assert_eq!(metrics.line_count, 4);
    assert!(metrics.is_multiline);
    assert!(metrics.has_code_fence);
    assert!(!metrics.has_unicode);
}

#[test]
fn test_cursor_classification_pure_unit() {
    let info = DiscoveredVersionInfo {
        semver_parsed: "none".to_string(),
        presence: InstallationStatus::UnavailableOfflineGate,
    };
    let summary = VendorSummary {
        vendor_id: "cursor".to_string(),
        installation_status: InstallationStatus::UnavailableOfflineGate,
        discovered_version: info,
        version_claim_manifest: "2026.07.23-e383d2b".to_string(),
        total_trials: 0,
        widths_tested: vec![],
        proof_counts: ProofCounts::default(),
        verdict: CampaignVerdict::Limitation,
        reason_code: ClosedReasonCode::VendorUnavailable,
    };
    assert_eq!(summary.verdict, CampaignVerdict::Limitation);
    assert_eq!(summary.reason_code, ClosedReasonCode::VendorUnavailable);
}

#[test]
fn test_schema_serialization_round_trip() {
    let report = StageAndClearComponentReport {
        schema_version: "1.0.0".to_string(),
        report_id: "lve-20260827T203000Z-5ee0f17e00".to_string(),
        generated_at: "2026-08-27T20:30:00Z".to_string(),
        component_scope: "Gate 7 Stage-and-Clear Soak Component".to_string(),
        git_commit: GitCommitMetadata {
            sha: "5ee0f17e00000000000000000000000000000000".to_string(),
            tree_clean: true,
        },
        host_platform: discover_host_platform(),
        campaign_metadata: CampaignMetadata {
            campaign_name: "Gate 7 Stage-and-Clear Soak".to_string(),
            opt_in_flag: "CYCLOPS_LIVE_VENDOR=1".to_string(),
            metric_scope: "staging and composer-clear representation".to_string(),
            total_trials: 0,
            total_duration_ms: 0.0,
        },
        component_verdict: CampaignVerdict::Limitation,
        component_reason_code: ClosedReasonCode::RequiredVendorLimitation,
        gate7_closed_cell_inventory: build_gate7_cell_inventory(CampaignVerdict::Limitation),
        vendor_summaries: vec![],
        observations: vec![],
    };

    let json_str = serde_json::to_string_pretty(&report).expect("serialize");
    let deserialized: StageAndClearComponentReport =
        serde_json::from_str(&json_str).expect("deserialize");
    assert_eq!(deserialized.schema_version, "1.0.0");
    assert_eq!(deserialized.git_commit.sha, report.git_commit.sha);
    assert_eq!(deserialized.component_verdict, CampaignVerdict::Limitation);
    assert_eq!(deserialized.gate7_closed_cell_inventory.len(), 1);
}

#[test]
fn test_check_opt_in_value_variants() {
    assert_eq!(check_opt_in_value(Some("1")), Ok(()));
    assert_eq!(
        check_opt_in_value(Some("0")),
        Err(ClosedReasonCode::OptInRequired)
    );
    assert_eq!(
        check_opt_in_value(Some("true")),
        Err(ClosedReasonCode::OptInRequired)
    );
    assert_eq!(
        check_opt_in_value(Some("")),
        Err(ClosedReasonCode::OptInRequired)
    );
    assert_eq!(
        check_opt_in_value(None),
        Err(ClosedReasonCode::OptInRequired)
    );
}

#[test]
fn test_missing_vendor_prevents_passed_verdict() {
    let summaries = vec![
        VendorSummary {
            vendor_id: "codex".to_string(),
            installation_status: InstallationStatus::InstalledLive,
            discovered_version: DiscoveredVersionInfo {
                semver_parsed: "0.150.1".to_string(),
                presence: InstallationStatus::InstalledLive,
            },
            version_claim_manifest: "0.149.1".to_string(),
            total_trials: 2,
            widths_tested: vec![60, 100],
            proof_counts: ProofCounts {
                exact_row_proof: 2,
                safe_clear_representations: 2,
                ..Default::default()
            },
            verdict: CampaignVerdict::Passed,
            reason_code: ClosedReasonCode::AllTrialsVerified,
        },
        VendorSummary {
            vendor_id: "claude".to_string(),
            installation_status: InstallationStatus::UnavailableOfflineGate,
            discovered_version: DiscoveredVersionInfo {
                semver_parsed: "none".to_string(),
                presence: InstallationStatus::UnavailableOfflineGate,
            },
            version_claim_manifest: "2.1.221".to_string(),
            total_trials: 0,
            widths_tested: vec![],
            proof_counts: ProofCounts::default(),
            verdict: CampaignVerdict::Limitation,
            reason_code: ClosedReasonCode::VendorUnavailable,
        },
    ];

    let verdict = compute_component_verdict(&summaries);
    assert_ne!(
        verdict,
        CampaignVerdict::Passed,
        "missing required vendor must prevent Passed component verdict"
    );
    assert_eq!(verdict, CampaignVerdict::Limitation);
}

#[test]
fn test_missing_cursor_yields_limitation_when_others_pass() {
    let summaries = vec![
        VendorSummary {
            vendor_id: "codex".to_string(),
            installation_status: InstallationStatus::InstalledLive,
            discovered_version: DiscoveredVersionInfo {
                semver_parsed: "0.150.1".to_string(),
                presence: InstallationStatus::InstalledLive,
            },
            version_claim_manifest: "0.149.1".to_string(),
            total_trials: 2,
            widths_tested: vec![60, 100],
            proof_counts: ProofCounts {
                exact_row_proof: 2,
                safe_clear_representations: 2,
                ..Default::default()
            },
            verdict: CampaignVerdict::Passed,
            reason_code: ClosedReasonCode::AllTrialsVerified,
        },
        VendorSummary {
            vendor_id: "claude".to_string(),
            installation_status: InstallationStatus::InstalledLive,
            discovered_version: DiscoveredVersionInfo {
                semver_parsed: "2.1.248".to_string(),
                presence: InstallationStatus::InstalledLive,
            },
            version_claim_manifest: "2.1.221".to_string(),
            total_trials: 2,
            widths_tested: vec![60, 100],
            proof_counts: ProofCounts {
                exact_row_proof: 2,
                safe_clear_representations: 2,
                ..Default::default()
            },
            verdict: CampaignVerdict::Passed,
            reason_code: ClosedReasonCode::AllTrialsVerified,
        },
        VendorSummary {
            vendor_id: "agy".to_string(),
            installation_status: InstallationStatus::InstalledLive,
            discovered_version: DiscoveredVersionInfo {
                semver_parsed: "1.1.22".to_string(),
                presence: InstallationStatus::InstalledLive,
            },
            version_claim_manifest: "1.1.18".to_string(),
            total_trials: 2,
            widths_tested: vec![60, 100],
            proof_counts: ProofCounts {
                exact_row_proof: 2,
                safe_clear_representations: 2,
                ..Default::default()
            },
            verdict: CampaignVerdict::Passed,
            reason_code: ClosedReasonCode::AllTrialsVerified,
        },
        VendorSummary {
            vendor_id: "cursor".to_string(),
            installation_status: InstallationStatus::UnavailableOfflineGate,
            discovered_version: DiscoveredVersionInfo {
                semver_parsed: "none".to_string(),
                presence: InstallationStatus::UnavailableOfflineGate,
            },
            version_claim_manifest: "2026.07.23-e383d2b".to_string(),
            total_trials: 0,
            widths_tested: vec![],
            proof_counts: ProofCounts::default(),
            verdict: CampaignVerdict::Limitation,
            reason_code: ClosedReasonCode::VendorUnavailable,
        },
    ];

    let verdict = compute_component_verdict(&summaries);
    assert_eq!(
        verdict,
        CampaignVerdict::Limitation,
        "Codex, Claude, AGY passed with Cursor unavailable/limitation must yield Limitation"
    );
}

#[test]
fn test_controller_fail_fast_stops_subsequent_vendors() {
    let vendors = vec!["vendor_a", "vendor_b", "vendor_c"];
    let mut executed_vendors = Vec::new();

    let (verdict, summaries, _) = run_campaign_controller(&vendors, |v| {
        executed_vendors.push(v.to_string());
        if v == "vendor_a" {
            (
                VendorSummary {
                    vendor_id: v.to_string(),
                    installation_status: InstallationStatus::InstalledLive,
                    discovered_version: DiscoveredVersionInfo {
                        semver_parsed: "1.0.0".to_string(),
                        presence: InstallationStatus::InstalledLive,
                    },
                    version_claim_manifest: "1.0.0".to_string(),
                    total_trials: 1,
                    widths_tested: vec![60],
                    proof_counts: ProofCounts {
                        failures: 1,
                        ..Default::default()
                    },
                    verdict: CampaignVerdict::Failed,
                    reason_code: ClosedReasonCode::VerificationMismatch,
                },
                vec![],
                true, // failed = true
            )
        } else {
            (
                VendorSummary {
                    vendor_id: v.to_string(),
                    installation_status: InstallationStatus::InstalledLive,
                    discovered_version: DiscoveredVersionInfo {
                        semver_parsed: "1.0.0".to_string(),
                        presence: InstallationStatus::InstalledLive,
                    },
                    version_claim_manifest: "1.0.0".to_string(),
                    total_trials: 1,
                    widths_tested: vec![60],
                    proof_counts: ProofCounts::default(),
                    verdict: CampaignVerdict::Passed,
                    reason_code: ClosedReasonCode::AllTrialsVerified,
                },
                vec![],
                false,
            )
        }
    });

    assert_eq!(verdict, CampaignVerdict::Failed);
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        executed_vendors,
        vec!["vendor_a".to_string()],
        "controller must stop immediately on first vendor failure"
    );
}

#[test]
fn test_cell_inventory_reflects_actual_campaign_verdict() {
    let inventory_passed = build_gate7_cell_inventory(CampaignVerdict::Passed);
    assert_eq!(inventory_passed[0].verdict, Some(CampaignVerdict::Passed));
    assert_eq!(
        inventory_passed[0].reason_code,
        ClosedReasonCode::AllTrialsVerified
    );

    let inventory_failed = build_gate7_cell_inventory(CampaignVerdict::Failed);
    assert_eq!(inventory_failed[0].verdict, Some(CampaignVerdict::Failed));
    assert_eq!(
        inventory_failed[0].reason_code,
        ClosedReasonCode::VerificationMismatch
    );

    let inventory_limited = build_gate7_cell_inventory(CampaignVerdict::Limitation);
    assert_eq!(
        inventory_limited[0].reason_code,
        ClosedReasonCode::RequiredVendorLimitation,
        "a partially executed limited campaign must not claim zero trials"
    );
}

#[test]
fn test_report_consistency_requires_one_observation_per_attempted_trial() {
    let sha = "5ee0f17e00000000000000000000000000000000";
    let observation = trial_observation(
        "codex",
        1,
        60,
        sha,
        "darwin-arm64",
        "0.150.1",
        structural_metrics_for("cyclops inbox claim m-att_test"),
        ComposerRepresentationProof::HiddenOrAmbiguous,
        false,
        5_000.0,
        0.0,
        5_000.0,
        CampaignVerdict::Failed,
        ClosedReasonCode::InitialIdleTimeout,
    );
    let summary = VendorSummary {
        vendor_id: "codex".to_string(),
        installation_status: InstallationStatus::InstalledLive,
        discovered_version: DiscoveredVersionInfo {
            semver_parsed: "0.150.1".to_string(),
            presence: InstallationStatus::InstalledLive,
        },
        version_claim_manifest: "0.149.1".to_string(),
        total_trials: 1,
        widths_tested: vec![60],
        proof_counts: ProofCounts {
            failures: 1,
            ..Default::default()
        },
        verdict: CampaignVerdict::Failed,
        reason_code: ClosedReasonCode::InitialIdleTimeout,
    };
    let mut report = StageAndClearComponentReport {
        schema_version: "1.0.0".to_string(),
        report_id: "lve-test".to_string(),
        generated_at: "2026-08-27T20:30:00Z".to_string(),
        component_scope: "Gate 7 Stage-and-Clear Soak Component".to_string(),
        git_commit: GitCommitMetadata {
            sha: sha.to_string(),
            tree_clean: true,
        },
        host_platform: discover_host_platform(),
        campaign_metadata: CampaignMetadata {
            campaign_name: "Gate 7 Stage-and-Clear Soak".to_string(),
            opt_in_flag: "CYCLOPS_LIVE_VENDOR=1".to_string(),
            metric_scope: "staging and composer-clear representation".to_string(),
            total_trials: 1,
            total_duration_ms: 5_000.0,
        },
        component_verdict: CampaignVerdict::Failed,
        component_reason_code: ClosedReasonCode::InitialIdleTimeout,
        gate7_closed_cell_inventory: build_gate7_cell_inventory(CampaignVerdict::Failed),
        vendor_summaries: vec![summary],
        observations: vec![observation],
    };

    assert!(validate_report_consistency(&report).is_ok());

    report.campaign_metadata.total_trials = 0;
    assert!(validate_report_consistency(&report).is_err());
    report.campaign_metadata.total_trials = 1;

    report.vendor_summaries[0].widths_tested = vec![60, 100];
    assert!(validate_report_consistency(&report).is_err());
    report.vendor_summaries[0].widths_tested = vec![60];

    report.observations[0].verdict = CampaignVerdict::Passed;
    assert!(validate_report_consistency(&report).is_err());
}

#[test]
fn test_f24_scratch_directory_root() {
    let scratch = Gate7ScratchHome::new().expect("create scratch");
    assert!(scratch.path().exists());
    let expected_root = scratch_root();
    assert!(
        scratch.path().starts_with(&expected_root),
        "scratch directory must start with F24 scratch root"
    );
}

#[test]
fn test_failure_guard_writes_a_closed_failed_artifact_on_unwind_path() {
    let scratch = Gate7ScratchHome::new().expect("create scratch");
    let path = scratch.path().join("failed-component.json");
    {
        let _guard = FailureArtifactGuard::new(
            path.clone(),
            StageAndClearComponentReport {
                schema_version: "1.0.0".to_string(),
                report_id: "lve-abort-test".to_string(),
                generated_at: "2026-08-27T20:30:00Z".to_string(),
                component_scope: "Gate 7 Stage-and-Clear Representation Component".to_string(),
                git_commit: GitCommitMetadata {
                    sha: "5ee0f17e00000000000000000000000000000000".to_string(),
                    tree_clean: true,
                },
                host_platform: discover_host_platform(),
                campaign_metadata: CampaignMetadata {
                    campaign_name: "Gate 7 Stage-and-Clear Representation Component".to_string(),
                    opt_in_flag: "CYCLOPS_LIVE_VENDOR=1".to_string(),
                    metric_scope: "staging and composer-clear representation".to_string(),
                    total_trials: 0,
                    total_duration_ms: 0.0,
                },
                component_verdict: CampaignVerdict::Failed,
                component_reason_code: ClosedReasonCode::HarnessAborted,
                gate7_closed_cell_inventory: build_gate7_cell_inventory(CampaignVerdict::Failed),
                vendor_summaries: vec![],
                observations: vec![],
            },
        );
    }

    let value: StageAndClearComponentReport =
        serde_json::from_slice(&std::fs::read(&path).expect("failure artifact must be written"))
            .expect("failure artifact schema");
    assert_eq!(value.component_verdict, CampaignVerdict::Failed);
    assert_eq!(
        value.component_reason_code,
        ClosedReasonCode::HarnessAborted
    );
}

#[test]
fn test_vendor_state_environment_is_fully_scratch_rooted() {
    let scratch = Gate7ScratchHome::new().expect("create scratch");
    let home = scratch.path().join("vendor-home");
    prepare_isolated_vendor_home(&home).expect("prepare vendor home");
    let environment = isolated_vendor_environment(&home);

    for required in [
        "HOME",
        "CYCLOPS_HOME",
        "CODEX_HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "TMPDIR",
    ] {
        let value = environment
            .iter()
            .find_map(|(key, value)| (key == required).then_some(value))
            .unwrap_or_else(|| panic!("missing isolated environment key {required}"));
        assert!(
            Path::new(value).starts_with(&home),
            "{required} escaped the isolated vendor home"
        );
    }

    let command = isolated_vendor_launch_command(&home, "codex");
    assert!(command.starts_with("exec /usr/bin/env -i "));
    assert!(command.contains("CODEX_HOME="));
    assert!(command.contains("XDG_DATA_HOME="));
    assert!(command.contains("XDG_RUNTIME_DIR="));
}

#[test]
fn test_adversarial_serialized_report_leak_check() {
    let sentinels = [
        "SECRET_USER_PROMPT_KEYWORD_XYZ",
        "BEARER_TOKEN_999988887777",
        "/private/var/folders/secret/path",
        "raw_unfiltered_command_output",
    ];

    let metrics =
        structural_metrics_for("SECRET_USER_PROMPT_KEYWORD_XYZ with BEARER_TOKEN_999988887777");
    let obs = ObservationRecord {
        trial_id: "obs-001".to_string(),
        vendor_id: "codex".to_string(),
        timestamp: "2026-08-27T20:30:00Z".to_string(),
        git_commit_sha: "5ee0f17e00000000000000000000000000000000".to_string(),
        platform_tuple: "darwin-arm64".to_string(),
        discovered_version: "0.150.1".to_string(),
        terminal_dimensions: (60, 24),
        structural_input_metrics: metrics,
        proof_classification: ComposerRepresentationProof::ExactStaged,
        clear_representation_verified: true,
        latency_metrics_ms: LatencyMetricsMs {
            staging_detection_ms: 12.0,
            clear_teardown_ms: 8.0,
            total_wall_ms: 20.0,
        },
        verdict: CampaignVerdict::Passed,
        reason_code: ClosedReasonCode::AllTrialsVerified,
    };

    let report = StageAndClearComponentReport {
        schema_version: "1.0.0".to_string(),
        report_id: "lve-test".to_string(),
        generated_at: "2026-08-27T20:30:00Z".to_string(),
        component_scope: "Gate 7 Stage-and-Clear Soak Component".to_string(),
        git_commit: GitCommitMetadata {
            sha: "5ee0f17e00000000000000000000000000000000".to_string(),
            tree_clean: true,
        },
        host_platform: discover_host_platform(),
        campaign_metadata: CampaignMetadata {
            campaign_name: "Gate 7 Stage-and-Clear Soak".to_string(),
            opt_in_flag: "CYCLOPS_LIVE_VENDOR=1".to_string(),
            metric_scope: "staging and composer-clear representation".to_string(),
            total_trials: 1,
            total_duration_ms: 20.0,
        },
        component_verdict: CampaignVerdict::Passed,
        component_reason_code: ClosedReasonCode::AllTrialsVerified,
        gate7_closed_cell_inventory: build_gate7_cell_inventory(CampaignVerdict::Passed),
        vendor_summaries: vec![],
        observations: vec![obs],
    };

    let val = serde_json::to_value(&report).expect("convert to value");
    assert_no_sentinels(&val, &sentinels);
}

#[test]
fn test_delivery_prove_composer_representation_regression() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
    let manifests = load_dir(&manifest_dir).expect("load shipped manifests");
    let claude = manifests.get("claude").expect("claude manifest");

    let attempt_id = NotificationAttemptId::generate();
    let format3_doorbell = render_doorbell_v3(attempt_id);

    // 1. Exact Format 3 Staged -> ExactStaged
    let staged_capture = format!(
        "❯\u{a0}{format3_doorbell}\n\
         ────────────────────────────────────────────────────────────────\n\
           Haiku 4.5 · low · ~ · Ctx: 76% · 200K window · 47K used\n\
           ⏵⏵ bypass permissions on (shift+tab to cycle)"
    );
    assert_eq!(
        prove_composer_representation(claude, &staged_capture, Some(&format3_doorbell)),
        ComposerRepresentationProof::ExactStaged
    );

    // 2. Different human text -> HiddenOrAmbiguous
    let different_human_capture = "❯\u{a0}some different human prompt\n\
         ────────────────────────────────────────────────────────────────\n\
           Haiku 4.5 · low · ~ · Ctx: 76% · 200K window · 47K used\n\
           ⏵⏵ bypass permissions on (shift+tab to cycle)";
    assert_eq!(
        prove_composer_representation(claude, different_human_capture, Some(&format3_doorbell)),
        ComposerRepresentationProof::HiddenOrAmbiguous
    );

    // 3. True empty composer -> Clean
    let clean_empty_capture = "❯\u{a0}\n\
         ────────────────────────────────────────────────────────────────\n\
           Haiku 4.5 · low · ~ · Ctx: 76% · 200K window · 47K used\n\
           ⏵⏵ bypass permissions on (shift+tab to cycle)";
    assert_eq!(
        prove_composer_representation(claude, clean_empty_capture, None),
        ComposerRepresentationProof::WriteSafeClean
    );

    // 4. Tabs and non-breaking spaces are composer bytes, not terminal padding.
    // They must remain occupied even when a clean rule also matches the row.
    let whitespace_tab_capture = "❯\u{a0}\t\n\
         ────────────────────────────────────────────────────────────────\n\
           Haiku 4.5 · low · ~ · Ctx: 76% · 200K window · 47K used\n\
           ⏵⏵ bypass permissions on (shift+tab to cycle)";
    assert_eq!(
        prove_composer_representation(claude, whitespace_tab_capture, None),
        ComposerRepresentationProof::HiddenOrAmbiguous
    );

    let whitespace_nbsp_capture = "❯\u{a0}\u{a0}\n\
         ────────────────────────────────────────────────────────────────\n\
           Haiku 4.5 · low · ~ · Ctx: 76% · 200K window · 47K used\n\
           ⏵⏵ bypass permissions on (shift+tab to cycle)";
    assert_eq!(
        prove_composer_representation(claude, whitespace_nbsp_capture, None),
        ComposerRepresentationProof::HiddenOrAmbiguous
    );

    let codex = manifests.get("codex").expect("codex manifest");
    let codex_ghost =
        include_str!("../../cyclops-manifest/tests/fixtures/codex_ghost_composer_esc.txt");
    assert_eq!(
        prove_composer_representation(codex, codex_ghost, None),
        ComposerRepresentationProof::WriteSafeGhost,
        "the component may classify the shipped ghost representation but cannot authorize a write"
    );
}

#[test]
fn test_missing_clear_action_is_a_vendor_neutral_limitation() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
    let manifests = load_dir(&manifest_dir).expect("load shipped manifests");
    assert!(has_measured_clear_action(
        manifests.get("codex").expect("codex manifest")
    ));
    assert!(has_measured_clear_action(
        manifests.get("claude").expect("claude manifest")
    ));
    assert!(!has_measured_clear_action(
        manifests.get("agy").expect("agy manifest")
    ));
    assert!(!has_measured_clear_action(
        manifests.get("cursor").expect("cursor manifest")
    ));
}

// ---------------------------------------------------------------------------
// Opt-In Live Vendor Soak Campaign (Ignored by Default in Ordinary Test Suite)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "live vendor harness requires CYCLOPS_LIVE_VENDOR=1, CYCLOPS_FROZEN_SHA=<40-char-sha>, and installed binaries"]
fn live_vendor_evidence_campaign_opt_in() {
    // 1. Strict opt-in runtime refusal (panics if not set)
    check_opt_in_value(std::env::var("CYCLOPS_LIVE_VENDOR").as_deref().ok())
        .expect("CYCLOPS_LIVE_VENDOR=1 must be set to run live campaign");

    // 2. Validate caller-supplied 40-character frozen SHA and build ref
    let expected_sha = std::env::var("CYCLOPS_FROZEN_SHA")
        .expect("CYCLOPS_FROZEN_SHA must be provided as a 40-character hex SHA");
    let commit_meta = verify_build_ref_integrity(&expected_sha)
        .expect("build ref and checkout integrity verification failed");
    let host_meta = discover_host_platform();
    let report_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../validation/raw/soak/stage_and_clear_soak_component_report.json");
    let mut failure_artifact = FailureArtifactGuard::new(
        report_path.clone(),
        StageAndClearComponentReport {
            schema_version: "1.0.0".to_string(),
            report_id: format!(
                "lve-stage-clear-abort-{}-{}",
                utc_now_iso8601(),
                &commit_meta.sha[..10]
            ),
            generated_at: utc_now_iso8601(),
            component_scope: "Gate 7 Stage-and-Clear Representation Component".to_string(),
            git_commit: commit_meta.clone(),
            host_platform: host_meta.clone(),
            campaign_metadata: CampaignMetadata {
                campaign_name: "Gate 7 Stage-and-Clear Representation Component".to_string(),
                opt_in_flag: "CYCLOPS_LIVE_VENDOR=1".to_string(),
                metric_scope: "Doorbell Format 3 staging and composer-clear representation"
                    .to_string(),
                total_trials: 0,
                total_duration_ms: 0.0,
            },
            component_verdict: CampaignVerdict::Failed,
            component_reason_code: ClosedReasonCode::HarnessAborted,
            gate7_closed_cell_inventory: build_gate7_cell_inventory(CampaignVerdict::Failed),
            vendor_summaries: vec![],
            observations: vec![],
        },
    );

    // 3. Prepare isolated RAII 0700 scratch directory conforming to F24
    let scratch = Gate7ScratchHome::new().expect("create scratch directory");
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
    let manifests = load_dir(&manifest_dir).expect("load shipped manifests");
    let server = TmuxServer::new("gate7-stage-clear");

    let vendors = vec!["codex", "claude", "agy", "cursor"];
    let widths = [60u16, 100u16];
    let start_instant = Instant::now();

    let (component_verdict, vendor_summaries, observations) =
        run_campaign_controller(&vendors, |v| {
            let manifest = manifests.get(v);
            let discovery_home = scratch.path().join("discovery").join(v);
            prepare_isolated_vendor_home(&discovery_home)
                .expect("create isolated vendor discovery home");
            let (status, info) = discover_installed_vendor(v, manifest, &discovery_home);
            if status == InstallationStatus::UnavailableOfflineGate {
                return (
                    VendorSummary {
                        vendor_id: v.to_string(),
                        installation_status: status,
                        discovered_version: info,
                        version_claim_manifest: manifest
                            .map(|m| m.agent.version_tested.clone())
                            .unwrap_or_default(),
                        total_trials: 0,
                        widths_tested: vec![],
                        proof_counts: ProofCounts::default(),
                        verdict: CampaignVerdict::Limitation,
                        reason_code: ClosedReasonCode::VendorUnavailable,
                    },
                    vec![],
                    false,
                );
            }

            let m = manifest.expect("manifest must exist for discovered vendor");
            let clear_action_available = has_measured_clear_action(m);

            // A vendor without a measured clear action cannot participate in
            // this component. Do not guess a key or spam the live composer.
            if !clear_action_available {
                return (
                    VendorSummary {
                        vendor_id: v.to_string(),
                        installation_status: status,
                        discovered_version: info,
                        version_claim_manifest: m.agent.version_tested.clone(),
                        total_trials: 0,
                        widths_tested: vec![],
                        proof_counts: ProofCounts::default(),
                        verdict: CampaignVerdict::Limitation,
                        reason_code: ClosedReasonCode::ClearKeysUnsupportedManualOnly,
                    },
                    vec![],
                    false,
                );
            }

            let mut counts = ProofCounts::default();
            let mut trial_obs = Vec::new();
            let mut vendor_failed = false;
            let mut failure_reason = None;
            let mut trial_index = 0;
            let mut attempted_widths = Vec::new();
            let platform_tuple = discover_host_platform().platform_tuple;

            for &width in &widths {
                trial_index += 1;
                attempted_widths.push(width);
                let session_name = format!("g7-{v}-{width}-{}", trial_index);
                let launch_cmd = m.agent.launch.as_deref().unwrap_or(v);
                let trial_home = scratch.path().join("trials").join(&session_name);
                prepare_isolated_vendor_home(&trial_home)
                    .expect("create isolated vendor state directory");
                let trial_home_str = trial_home.to_string_lossy();
                let isolated_launch = isolated_vendor_launch_command(&trial_home, launch_cmd);

                let t_trial_start = Instant::now();
                let attempt_id = NotificationAttemptId::generate();
                let doorbell_cmd = render_doorbell_v3(attempt_id);
                let metrics = structural_metrics_for(&doorbell_cmd);

                // Spawn the vendor with all ordinary home and XDG state rooted
                // in the exact F24 scratch tree. An installed binary that
                // cannot reach a clean composer under this isolation fails the
                // campaign instead of reading or writing the operator's home.
                let launch = server.run(&[
                    "new-session",
                    "-d",
                    "-s",
                    &session_name,
                    "-x",
                    &width.to_string(),
                    "-y",
                    "24",
                    "-c",
                    &trial_home_str,
                    &isolated_launch,
                ]);

                if !launch.status.success() {
                    counts.failures += 1;
                    vendor_failed = true;
                    failure_reason = Some(ClosedReasonCode::VendorLaunchFailed);
                    let elapsed = t_trial_start.elapsed().as_secs_f64() * 1000.0;
                    trial_obs.push(trial_observation(
                        v,
                        trial_index,
                        width,
                        &commit_meta.sha,
                        &platform_tuple,
                        &info.semver_parsed,
                        metrics,
                        ComposerRepresentationProof::HiddenOrAmbiguous,
                        false,
                        elapsed,
                        0.0,
                        elapsed,
                        CampaignVerdict::Failed,
                        ClosedReasonCode::VendorLaunchFailed,
                    ));
                    break;
                }

                let target_pane = format!("{session_name}:0.0");

                // 1. Bounded condition wait for a write-safe composer representation.
                let initial_clean = wait_condition(STATE_SETTLE_TIMEOUT, || {
                    let esc = server.run(&["capture-pane", "-e", "-p", "-J", "-t", &target_pane]);
                    let raw_esc = String::from_utf8_lossy(&esc.stdout);
                    representation_is_write_safe(prove_composer_representation(m, &raw_esc, None))
                });

                if !initial_clean {
                    counts.failures += 1;
                    vendor_failed = true;
                    failure_reason = Some(ClosedReasonCode::InitialIdleTimeout);
                    let elapsed = t_trial_start.elapsed().as_secs_f64() * 1000.0;
                    trial_obs.push(trial_observation(
                        v,
                        trial_index,
                        width,
                        &commit_meta.sha,
                        &platform_tuple,
                        &info.semver_parsed,
                        metrics,
                        ComposerRepresentationProof::HiddenOrAmbiguous,
                        false,
                        elapsed,
                        0.0,
                        elapsed,
                        CampaignVerdict::Failed,
                        ClosedReasonCode::InitialIdleTimeout,
                    ));
                    server.run(&["kill-session", "-t", &session_name]);
                    break;
                }

                // 2. Stage format 3 compact doorbell
                let stage = server.run(&["send-keys", "-t", &target_pane, "-l", &doorbell_cmd]);
                if !stage.status.success() {
                    counts.failures += 1;
                    vendor_failed = true;
                    failure_reason = Some(ClosedReasonCode::StageCommandFailed);
                    let elapsed = t_trial_start.elapsed().as_secs_f64() * 1000.0;
                    trial_obs.push(trial_observation(
                        v,
                        trial_index,
                        width,
                        &commit_meta.sha,
                        &platform_tuple,
                        &info.semver_parsed,
                        metrics,
                        ComposerRepresentationProof::HiddenOrAmbiguous,
                        false,
                        elapsed,
                        0.0,
                        elapsed,
                        CampaignVerdict::Failed,
                        ClosedReasonCode::StageCommandFailed,
                    ));
                    server.run(&["kill-session", "-t", &session_name]);
                    break;
                }

                // 3. Bounded condition wait for exact staging proof
                let staged_verified = wait_condition(STATE_SETTLE_TIMEOUT, || {
                    let esc = server.run(&["capture-pane", "-e", "-p", "-J", "-t", &target_pane]);
                    let raw_esc = String::from_utf8_lossy(&esc.stdout);
                    prove_composer_representation(m, &raw_esc, Some(&doorbell_cmd))
                        == ComposerRepresentationProof::ExactStaged
                });

                let t_stage = t_trial_start.elapsed().as_secs_f64() * 1000.0;

                if !staged_verified {
                    counts.failures += 1;
                    vendor_failed = true;
                    failure_reason = Some(ClosedReasonCode::VerificationMismatch);
                    let elapsed = t_trial_start.elapsed().as_secs_f64() * 1000.0;
                    trial_obs.push(trial_observation(
                        v,
                        trial_index,
                        width,
                        &commit_meta.sha,
                        &platform_tuple,
                        &info.semver_parsed,
                        metrics,
                        ComposerRepresentationProof::HiddenOrAmbiguous,
                        false,
                        t_stage,
                        0.0,
                        elapsed,
                        CampaignVerdict::Failed,
                        ClosedReasonCode::VerificationMismatch,
                    ));
                    server.run(&["kill-session", "-t", &session_name]);
                    break;
                }
                counts.exact_row_proof += 1;

                // 4. Clear execution with bounded wait
                let t_clear_start = Instant::now();
                let mut clear_command_ok = true;
                for k in &m.injection.clear_keys {
                    if !server
                        .run(&["send-keys", "-t", &target_pane, k])
                        .status
                        .success()
                    {
                        clear_command_ok = false;
                        break;
                    }
                }
                if !clear_command_ok {
                    counts.failures += 1;
                    vendor_failed = true;
                    failure_reason = Some(ClosedReasonCode::ClearCommandFailed);
                    let elapsed = t_trial_start.elapsed().as_secs_f64() * 1000.0;
                    trial_obs.push(trial_observation(
                        v,
                        trial_index,
                        width,
                        &commit_meta.sha,
                        &platform_tuple,
                        &info.semver_parsed,
                        metrics,
                        ComposerRepresentationProof::HiddenOrAmbiguous,
                        false,
                        t_stage,
                        t_clear_start.elapsed().as_secs_f64() * 1000.0,
                        elapsed,
                        CampaignVerdict::Failed,
                        ClosedReasonCode::ClearCommandFailed,
                    ));
                    server.run(&["kill-session", "-t", &session_name]);
                    break;
                }

                let clear_representation = wait_condition(STATE_SETTLE_TIMEOUT, || {
                    let esc = server.run(&["capture-pane", "-e", "-p", "-J", "-t", &target_pane]);
                    let raw_esc = String::from_utf8_lossy(&esc.stdout);
                    representation_is_write_safe(prove_composer_representation(m, &raw_esc, None))
                });

                let t_clear = t_clear_start.elapsed().as_secs_f64() * 1000.0;

                if !clear_representation {
                    counts.failures += 1;
                    vendor_failed = true;
                    failure_reason = Some(ClosedReasonCode::ClearFailed);
                    let elapsed = t_trial_start.elapsed().as_secs_f64() * 1000.0;
                    trial_obs.push(trial_observation(
                        v,
                        trial_index,
                        width,
                        &commit_meta.sha,
                        &platform_tuple,
                        &info.semver_parsed,
                        metrics,
                        ComposerRepresentationProof::HiddenOrAmbiguous,
                        false,
                        t_stage,
                        t_clear,
                        elapsed,
                        CampaignVerdict::Failed,
                        ClosedReasonCode::ClearFailed,
                    ));
                    server.run(&["kill-session", "-t", &session_name]);
                    break;
                }
                counts.safe_clear_representations += 1;

                // Clean session teardown
                server.run(&["kill-session", "-t", &session_name]);

                let total_trial_time = t_trial_start.elapsed().as_secs_f64() * 1000.0;
                trial_obs.push(trial_observation(
                    v,
                    trial_index,
                    width,
                    &commit_meta.sha,
                    &platform_tuple,
                    &info.semver_parsed,
                    metrics,
                    ComposerRepresentationProof::ExactStaged,
                    true,
                    t_stage,
                    t_clear,
                    total_trial_time,
                    CampaignVerdict::Passed,
                    ClosedReasonCode::AllTrialsVerified,
                ));
            }

            let summary_verdict = if vendor_failed {
                CampaignVerdict::Failed
            } else {
                CampaignVerdict::Passed
            };
            let summary_reason = if vendor_failed {
                failure_reason.expect("a failed vendor records its exact reason")
            } else {
                ClosedReasonCode::AllTrialsVerified
            };

            (
                VendorSummary {
                    vendor_id: v.to_string(),
                    installation_status: status,
                    discovered_version: info,
                    version_claim_manifest: m.agent.version_tested.clone(),
                    total_trials: trial_obs.len(),
                    widths_tested: attempted_widths,
                    proof_counts: counts,
                    verdict: summary_verdict,
                    reason_code: summary_reason,
                },
                trial_obs,
                vendor_failed,
            )
        });

    drop(scratch);

    let total_duration_ms = start_instant.elapsed().as_secs_f64() * 1000.0;
    let component_reason_code = match component_verdict {
        CampaignVerdict::Passed => ClosedReasonCode::AllTrialsVerified,
        CampaignVerdict::Failed => vendor_summaries
            .iter()
            .find(|summary| summary.verdict == CampaignVerdict::Failed)
            .map(|summary| summary.reason_code)
            .unwrap_or(ClosedReasonCode::HarnessAborted),
        CampaignVerdict::Limitation => ClosedReasonCode::RequiredVendorLimitation,
        CampaignVerdict::AdminDecision => ClosedReasonCode::AdminDecisionRequired,
    };

    let report = StageAndClearComponentReport {
        schema_version: "1.0.0".to_string(),
        report_id: format!(
            "lve-stage-clear-{}-{}",
            utc_now_iso8601(),
            &commit_meta.sha[..10]
        ),
        generated_at: utc_now_iso8601(),
        component_scope: "Gate 7 Stage-and-Clear Representation Component (no process binding or delivery authorization)".to_string(),
        git_commit: commit_meta,
        host_platform: host_meta,
        campaign_metadata: CampaignMetadata {
            campaign_name: "Gate 7 Stage-and-Clear Representation Component".to_string(),
            opt_in_flag: "CYCLOPS_LIVE_VENDOR=1".to_string(),
            metric_scope: "Doorbell Format 3 staging and composer-clear screen representation"
                .to_string(),
            total_trials: observations.len(),
            total_duration_ms,
        },
        component_verdict,
        component_reason_code,
        gate7_closed_cell_inventory: build_gate7_cell_inventory(component_verdict),
        vendor_summaries,
        observations,
    };

    validate_report_consistency(&report).expect("campaign report must be internally consistent");

    if let Some(parent) = report_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json_str = serde_json::to_string_pretty(&report).expect("serialize report");
    std::fs::write(&report_path, json_str).expect("write stage and clear component report");
    failure_artifact.disarm();

    assert_ne!(
        component_verdict,
        CampaignVerdict::Failed,
        "live vendor component found a defect; the failure artifact was written"
    );
}
