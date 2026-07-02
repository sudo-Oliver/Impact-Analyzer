//! Version-intelligence layer.
//!
//! Two independent checks run when `eia check` starts:
//!
//! 1. **Format-version check** (synchronous): compares `plan.format_version` against
//!    the highest minor we have tested. Same major but higher minor → warning.
//!    Major mismatch is already rejected by the parser itself.
//!
//! 2. **Binary auto-detection** (background thread, 200 ms hard timeout): runs
//!    `tofu --version` or `terraform --version`, parses the version with the
//!    `semver` crate, and warns if the binary is newer than the tested maximum.
//!    If neither binary is on PATH, or the timeout fires, the check is silently
//!    skipped — it never blocks the main flow.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use semver::Version;

// ── Known-good version ceilings ───────────────────────────────────────────────

/// Highest plan format_version minor this build was tested against ("1.X").
/// Major-version validation lives in the parser; we only track minor here.
pub const MAX_KNOWN_FORMAT: &str = "1.2";

/// Highest OpenTofu binary minor tested (inclusive).
const MAX_TOFU: (u64, u64) = (1, 9);

/// Highest Terraform binary minor tested (inclusive).
const MAX_TERRAFORM: (u64, u64) = (1, 10);

const DETECT_TIMEOUT: Duration = Duration::from_millis(200);

// ── Public types ──────────────────────────────────────────────────────────────

pub struct BinaryInfo {
    pub name:    String,
    pub version: String,
}

pub enum VersionWarning {
    /// plan format_version minor exceeds the tested ceiling — continue, but warn.
    FormatMinorAhead { found: String },
    /// Detected binary minor exceeds the tested ceiling — continue, but warn.
    BinaryAhead { name: String, found: String, tested_max: String },
}

// ── Format version check (synchronous) ───────────────────────────────────────

/// Check the plan's `format_version` against [`MAX_KNOWN_FORMAT`].
///
/// Returns `None` (silent) when within the known-good range.
/// Returns `Some(FormatMinorAhead)` when the minor is strictly ahead.
/// Major mismatch is already handled by the parser, so no need to repeat it.
pub fn check_format_version(format_version: &str) -> Option<VersionWarning> {
    let known_minor = MAX_KNOWN_FORMAT
        .split('.')
        .nth(1)?
        .parse::<u64>()
        .ok()?;
    let found_minor = format_version
        .split('.')
        .nth(1)?
        .parse::<u64>()
        .ok()?;

    if found_minor > known_minor {
        Some(VersionWarning::FormatMinorAhead { found: format_version.to_owned() })
    } else {
        None
    }
}

// ── Binary auto-detection (background) ───────────────────────────────────────

/// Spawn a background thread that probes `tofu` and `terraform` in parallel.
///
/// The thread has a hard [`DETECT_TIMEOUT`] baked in. Joining the handle after
/// plan parsing is done is essentially free — the timeout has already elapsed.
pub fn spawn_binary_detect() -> thread::JoinHandle<Option<BinaryInfo>> {
    thread::spawn(|| {
        let (tx, rx) = mpsc::channel::<Option<BinaryInfo>>();

        for name in ["tofu", "terraform"] {
            let tx  = tx.clone();
            let name = name.to_owned();
            thread::spawn(move || {
                let info = run_version_cmd(&name)
                    .map(|version| BinaryInfo { name, version });
                let _ = tx.send(info);
            });
        }

        // Take the first reply that arrives within the deadline.
        rx.recv_timeout(DETECT_TIMEOUT).ok().flatten()
    })
}

/// Check a detected binary against the tested-maximum for its tool family.
///
/// Uses `semver::Version` for robust parsing (handles pre-release tags like
/// "1.8.0-rc.1" without panicking). Returns `None` when within the known range.
pub fn check_binary_version(info: &BinaryInfo) -> Option<VersionWarning> {
    let is_tofu = info.name.contains("tofu");
    let (max_maj, max_min) = if is_tofu { MAX_TOFU } else { MAX_TERRAFORM };
    let tested_max = format!("{}.{}", max_maj, max_min);

    let found = Version::parse(&info.version).ok()?;

    if found.major == max_maj && found.minor > max_min {
        Some(VersionWarning::BinaryAhead {
            name:        info.name.clone(),
            found:       info.version.clone(),
            tested_max,
        })
    } else {
        None
    }
}

// ── Internals ─────────────────────────────────────────────────────────────────

fn run_version_cmd(binary: &str) -> Option<String> {
    use std::process::{Command, Stdio};
    Command::new(binary)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| extract_semver(&s))
}

/// Extract the first "vMAJOR.MINOR.PATCH" token from version command output.
///
/// OpenTofu:  "OpenTofu v1.8.2\n..."
/// Terraform: "Terraform v1.10.0\n..."
fn extract_semver(output: &str) -> Option<String> {
    output
        .lines()
        .next()?
        .split_whitespace()
        .find(|tok| tok.starts_with('v') && tok[1..].contains('.'))
        .map(|tok| tok.trim_start_matches('v').to_owned())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Format version ────────────────────────────────────────────────────────

    #[test]
    fn format_within_known_is_silent() {
        assert!(check_format_version("1.0").is_none());
        assert!(check_format_version("1.1").is_none());
        assert!(check_format_version("1.2").is_none()); // exact ceiling — still ok
    }

    #[test]
    fn format_minor_ahead_warns() {
        let w = check_format_version("1.9");
        assert!(matches!(w, Some(VersionWarning::FormatMinorAhead { .. })));
    }

    #[test]
    fn format_different_major_is_ignored_here() {
        // Major mismatch is the parser's job. This function only looks at minor.
        // "2.0" has minor 0 which is not > 2 (MAX_KNOWN_FORMAT minor), so no warn.
        // (The parser already rejected it before we get here.)
        assert!(check_format_version("2.0").is_none());
    }

    // ── Binary version ────────────────────────────────────────────────────────

    #[test]
    fn tofu_within_known_is_silent() {
        let info = BinaryInfo { name: "tofu".into(), version: "1.8.0".into() };
        assert!(check_binary_version(&info).is_none());

        let info = BinaryInfo { name: "tofu".into(), version: "1.9.3".into() };
        assert!(check_binary_version(&info).is_none()); // exactly at ceiling
    }

    #[test]
    fn tofu_ahead_warns() {
        let info = BinaryInfo { name: "tofu".into(), version: "1.99.0".into() };
        assert!(matches!(
            check_binary_version(&info),
            Some(VersionWarning::BinaryAhead { .. })
        ));
    }

    #[test]
    fn terraform_within_known_is_silent() {
        let info = BinaryInfo { name: "terraform".into(), version: "1.9.0".into() };
        assert!(check_binary_version(&info).is_none());

        let info = BinaryInfo { name: "terraform".into(), version: "1.10.5".into() };
        assert!(check_binary_version(&info).is_none()); // exactly at ceiling
    }

    #[test]
    fn terraform_ahead_warns() {
        let info = BinaryInfo { name: "terraform".into(), version: "1.11.0".into() };
        assert!(matches!(
            check_binary_version(&info),
            Some(VersionWarning::BinaryAhead { .. })
        ));
    }

    #[test]
    fn pre_release_version_is_parsed_by_semver() {
        // semver crate handles "1.8.0-rc.1" correctly — no panic.
        let info = BinaryInfo { name: "tofu".into(), version: "1.8.0-rc.1".into() };
        // 1.8.0-rc.1 is within 1.9 ceiling → no warning
        assert!(check_binary_version(&info).is_none());
    }

    // ── Version string extraction ─────────────────────────────────────────────

    #[test]
    fn extracts_tofu_version() {
        let out = "OpenTofu v1.8.2\non linux/amd64\n";
        assert_eq!(extract_semver(out), Some("1.8.2".into()));
    }

    #[test]
    fn extracts_terraform_version() {
        let out = "Terraform v1.10.0\non darwin/arm64\n";
        assert_eq!(extract_semver(out), Some("1.10.0".into()));
    }

    #[test]
    fn extract_returns_none_for_garbage() {
        assert_eq!(extract_semver("command not found"), None);
        assert_eq!(extract_semver(""), None);
    }
}
