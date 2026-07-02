use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;

// ── Leaf enums ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    NoOp,
    Create,
    Read,
    Update,
    Delete,
    // Removes the resource from state without destroying the real infrastructure object.
    Forget,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::NoOp => f.write_str("no-op"),
            Action::Create => f.write_str("create"),
            Action::Read => f.write_str("read"),
            Action::Update => f.write_str("update"),
            Action::Delete => f.write_str("delete"),
            Action::Forget => f.write_str("forget"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceMode {
    Managed,
    Data,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ResourceIndex {
    Number(i64),
    String(String),
}

// ── Change structs ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ImportInfo {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChangeDetails {
    pub actions: Vec<Action>,
    #[serde(default)]
    pub before: Option<serde_json::Value>,
    #[serde(default)]
    pub after: Option<serde_json::Value>,
    #[serde(default)]
    pub after_unknown: Option<serde_json::Value>,
    #[serde(default)]
    pub before_sensitive: Option<serde_json::Value>,
    #[serde(default)]
    pub after_sensitive: Option<serde_json::Value>,
    // Paths into the object that caused the replace action, e.g. [["triggers"]].
    #[serde(default)]
    pub replace_paths: Option<serde_json::Value>,
    // Present when this change is an import operation.
    #[serde(default)]
    pub importing: Option<ImportInfo>,
    #[serde(default)]
    pub generated_config: Option<String>,
}

impl ChangeDetails {
    pub fn is_noop(&self) -> bool {
        self.actions == [Action::NoOp]
    }

    pub fn is_replace(&self) -> bool {
        self.actions.contains(&Action::Create) && self.actions.contains(&Action::Delete)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputChangeDetails {
    pub actions: Vec<Action>,
    #[serde(default)]
    pub before: Option<serde_json::Value>,
    #[serde(default)]
    pub after: Option<serde_json::Value>,
    #[serde(default)]
    pub after_unknown: Option<serde_json::Value>,
    #[serde(default)]
    pub before_sensitive: Option<serde_json::Value>,
    #[serde(default)]
    pub after_sensitive: Option<serde_json::Value>,
    #[serde(default)]
    pub importing: Option<ImportInfo>,
    #[serde(default)]
    pub generated_config: Option<String>,
}

// ── Resource structs ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ResourceChange {
    pub address: String,
    #[serde(default)]
    pub previous_address: Option<String>,
    #[serde(default)]
    pub module_address: Option<String>,
    #[serde(default)]
    pub mode: Option<ResourceMode>,
    #[serde(rename = "type", default)]
    pub resource_type: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub index: Option<ResourceIndex>,
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub deposed: Option<String>,
    #[serde(default)]
    pub action_reason: Option<String>,
    pub change: ChangeDetails,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputChange {
    pub change: OutputChangeDetails,
    #[serde(default)]
    pub sensitive: bool,
}

// ── PlannedValues tree ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct PlannedResource {
    pub address: String,
    #[serde(default)]
    pub module_address: Option<String>,
    #[serde(default)]
    pub mode: Option<ResourceMode>,
    #[serde(rename = "type", default)]
    pub resource_type: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub index: Option<ResourceIndex>,
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub schema_version: Option<u64>,
    #[serde(default)]
    pub values: Option<serde_json::Value>,
    #[serde(default)]
    pub sensitive_values: Option<serde_json::Value>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub tainted: Option<bool>,
    #[serde(default)]
    pub deposed_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlannedModule {
    // Root module has no address field in the JSON.
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub resources: Vec<PlannedResource>,
    #[serde(default)]
    pub child_modules: Vec<PlannedModule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlannedOutput {
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    #[serde(rename = "type", default)]
    pub value_type: Option<serde_json::Value>,
    #[serde(default)]
    pub deprecated: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlannedValues {
    pub root_module: PlannedModule,
    #[serde(default)]
    pub outputs: HashMap<String, PlannedOutput>,
}

// ── State tree ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct StateResource {
    pub address: String,
    #[serde(default)]
    pub module_address: Option<String>,
    #[serde(default)]
    pub mode: Option<ResourceMode>,
    #[serde(rename = "type", default)]
    pub resource_type: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub index: Option<ResourceIndex>,
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub schema_version: Option<u64>,
    #[serde(default)]
    pub values: Option<serde_json::Value>,
    #[serde(default)]
    pub sensitive_values: Option<serde_json::Value>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub tainted: Option<bool>,
    #[serde(default)]
    pub deposed_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StateModule {
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub resources: Vec<StateResource>,
    #[serde(default)]
    pub child_modules: Vec<StateModule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StateValues {
    pub root_module: StateModule,
    #[serde(default)]
    pub outputs: HashMap<String, PlannedOutput>,
}

#[derive(Debug, Deserialize)]
pub struct State {
    // Optional: Terraform's state spec does not guarantee this field; OpenTofu
    // always writes it. Validation is skipped when absent.
    #[serde(default)]
    pub format_version: Option<String>,
    #[serde(default)]
    pub terraform_version: Option<String>,
    #[serde(default)]
    pub values: Option<StateValues>,
    #[serde(default)]
    pub checks: Vec<serde_json::Value>,
}

pub type PriorState = State;

// ── Top-level Plan ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct PlanVariable {
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    #[serde(default)]
    pub deprecated: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Plan {
    pub format_version: String,
    #[serde(default)]
    pub terraform_version: Option<String>,
    #[serde(default)]
    pub variables: HashMap<String, PlanVariable>,
    #[serde(default)]
    pub resource_changes: Vec<ResourceChange>,
    // Resources changed outside Terraform (infrastructure drift detected by provider refresh).
    #[serde(default)]
    pub resource_drift: Vec<ResourceChange>,
    #[serde(default)]
    pub output_changes: HashMap<String, OutputChange>,
    #[serde(default)]
    pub planned_values: Option<PlannedValues>,
    #[serde(default)]
    pub prior_state: Option<PriorState>,
    #[serde(default)]
    pub configuration: Option<serde_json::Value>,
    #[serde(default)]
    pub relevant_attributes: Vec<serde_json::Value>,
    #[serde(default)]
    pub checks: Vec<serde_json::Value>,
    #[serde(default)]
    pub timestamp: Option<String>,
    // Only written to JSON when true; default=false covers clean plans.
    #[serde(default)]
    pub errored: bool,
    // Terraform-specific: whether automation should attempt to apply this plan.
    // Absent in OpenTofu plans (defaults to None).
    #[serde(default)]
    pub applyable: Option<bool>,
    // Terraform-specific: whether apply is expected to fully converge the state.
    #[serde(default)]
    pub complete: Option<bool>,
    // Terraform-specific: planned values with unknown attributes marked true.
    #[serde(default)]
    pub proposed_unknown: Option<serde_json::Value>,
}

// ── Version validation ────────────────────────────────────────────────────────

fn parse_major_version(version: &str) -> anyhow::Result<u64> {
    let major_str = version
        .split('.')
        .next()
        .ok_or_else(|| anyhow!("malformed version string {:?}: missing '.' separator", version))?;
    major_str
        .parse::<u64>()
        .with_context(|| format!("malformed major version in {:?}", version))
}

fn validate_plan_format_version(version: &str) -> anyhow::Result<()> {
    let major = parse_major_version(version)?;
    if major != 1 {
        bail!(
            "unsupported plan format_version {:?}: expected major version 1, got {}",
            version,
            major
        );
    }
    Ok(())
}

fn validate_state_format_version(version: &str) -> anyhow::Result<()> {
    let major = parse_major_version(version)?;
    // Real OpenTofu emits "0.2"; the spec documentation shows "1.0". Accept both.
    if major != 0 && major != 1 {
        bail!(
            "unsupported state format_version {:?}: expected major version 0 or 1, got {}",
            version,
            major
        );
    }
    Ok(())
}

// ── Parse functions (streaming) ───────────────────────────────────────────────

/// Parse a plan from any reader. Uses streaming deserialization — the full
/// document is never loaded into a single allocation.
pub fn parse_plan_reader<R: Read>(reader: R) -> anyhow::Result<Plan> {
    let mut de = serde_json::Deserializer::from_reader(reader);
    let plan = Plan::deserialize(&mut de).context("failed to deserialize plan JSON")?;
    // end() skips trailing whitespace (including CI-appended newlines) then
    // errors on any remaining non-whitespace content, catching truncated files.
    de.end()
        .context("unexpected trailing content in plan JSON — file may be corrupt or truncated")?;
    validate_plan_format_version(&plan.format_version)?;
    Ok(plan)
}

pub fn parse_plan_file<P: AsRef<Path>>(path: P) -> anyhow::Result<Plan> {
    let file = File::open(path.as_ref())
        .with_context(|| format!("failed to open plan file {:?}", path.as_ref()))?;
    parse_plan_reader(BufReader::new(file))
}

pub fn parse_state_reader<R: Read>(reader: R) -> anyhow::Result<State> {
    let mut de = serde_json::Deserializer::from_reader(reader);
    let state = State::deserialize(&mut de).context("failed to deserialize state JSON")?;
    de.end()
        .context("unexpected trailing content in state JSON — file may be corrupt or truncated")?;
    // Terraform state spec does not require format_version; validate only when present.
    if let Some(version) = &state.format_version {
        validate_state_format_version(version)?;
    }
    Ok(state)
}

pub fn parse_state_file<P: AsRef<Path>>(path: P) -> anyhow::Result<State> {
    let file = File::open(path.as_ref())
        .with_context(|| format!("failed to open state file {:?}", path.as_ref()))?;
    parse_state_reader(BufReader::new(file))
}

// ── Module traversal ──────────────────────────────────────────────────────────

/// Collect all planned resources from every nesting level using iterative DFS.
/// Returns borrowed references — no `serde_json::Value` fields are cloned.
pub fn collect_planned_resources(module: &PlannedModule) -> Vec<&PlannedResource> {
    let mut resources = Vec::new();
    let mut stack = vec![module];
    while let Some(current) = stack.pop() {
        resources.extend(current.resources.iter());
        stack.extend(current.child_modules.iter());
    }
    resources
}

/// Collect all state resources from every nesting level using iterative DFS.
pub fn collect_state_resources(module: &StateModule) -> Vec<&StateResource> {
    let mut resources = Vec::new();
    let mut stack = vec![module];
    while let Some(current) = stack.pop() {
        resources.extend(current.resources.iter());
        stack.extend(current.child_modules.iter());
    }
    resources
}

/// Resolve a module address to a stable string ID for use as a graph node key.
/// Root module has no address in the JSON; falls back to "root".
pub fn module_id(module: &PlannedModule) -> &str {
    module.address.as_deref().unwrap_or("root")
}

// ── Drift detection ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftKind {
    Added,
    Removed,
    Modified,
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct DriftEntry {
    pub address: String,
    pub drift_kind: DriftKind,
    pub prior_values: Option<serde_json::Value>,
    pub planned_values: Option<serde_json::Value>,
}

/// Compare `prior_state` against `planned_values` to classify each resource.
/// `resource_changes` is authoritative for "Modified" — the provider may detect
/// drift that a naive value comparison would miss.
pub fn detect_drift(plan: &Plan) -> Vec<DriftEntry> {
    let prior_resources = plan
        .prior_state
        .as_ref()
        .and_then(|s| s.values.as_ref())
        .map(|v| collect_state_resources(&v.root_module))
        .unwrap_or_default();

    let prior_map: HashMap<&str, &StateResource> = prior_resources
        .iter()
        .map(|r| (r.address.as_str(), *r))
        .collect();

    let planned_resources = plan
        .planned_values
        .as_ref()
        .map(|v| collect_planned_resources(&v.root_module))
        .unwrap_or_default();

    let planned_map: HashMap<&str, &PlannedResource> = planned_resources
        .iter()
        .map(|r| (r.address.as_str(), *r))
        .collect();

    let changed_addresses: HashSet<&str> = plan
        .resource_changes
        .iter()
        .filter(|rc| {
            rc.change
                .actions
                .iter()
                .any(|a| matches!(a, Action::Update | Action::Delete | Action::Create))
        })
        .map(|rc| rc.address.as_str())
        .collect();

    let all_addresses: HashSet<&str> = prior_map
        .keys()
        .copied()
        .chain(planned_map.keys().copied())
        .collect();

    all_addresses
        .into_iter()
        .map(|address| {
            let in_prior = prior_map.get(address);
            let in_planned = planned_map.get(address);
            let drift_kind = match (in_prior, in_planned) {
                (None, Some(_)) => DriftKind::Added,
                (Some(_), None) => DriftKind::Removed,
                (Some(_), Some(_)) => {
                    if changed_addresses.contains(address) {
                        DriftKind::Modified
                    } else {
                        DriftKind::Unchanged
                    }
                }
                (None, None) => unreachable!("address originated from union of both maps"),
            };
            DriftEntry {
                address: address.to_owned(),
                drift_kind,
                prior_values: in_prior.and_then(|r| r.values.clone()),
                planned_values: in_planned.and_then(|r| r.values.clone()),
            }
        })
        .collect()
}

// ── Structural validation ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub severity: ValidationSeverity,
    pub message: String,
    pub address: Option<String>,
}

/// Run all structural checks against a parsed plan.
/// All checks are independent — the full list is always returned.
pub fn validate_plan(plan: &Plan) -> Vec<ValidationIssue> {
    let mut issues: Vec<ValidationIssue> = Vec::new();

    if plan.errored {
        issues.push(ValidationIssue {
            severity: ValidationSeverity::Error,
            message: "plan is marked as errored; results may be incomplete".to_string(),
            address: None,
        });
    }

    for rc in &plan.resource_changes {
        if rc.address.is_empty() {
            issues.push(ValidationIssue {
                severity: ValidationSeverity::Error,
                message: "resource_change has an empty address field".to_string(),
                address: Some(rc.address.clone()),
            });
        }
    }

    let total = plan.resource_changes.len();
    if total > 0 {
        let destroy_count = plan
            .resource_changes
            .iter()
            .filter(|rc| rc.change.actions == [Action::Delete])
            .count();
        if destroy_count == total {
            issues.push(ValidationIssue {
                severity: ValidationSeverity::Warning,
                message: format!(
                    "all {} resource(s) will be destroyed — verify this is intentional",
                    total
                ),
                address: None,
            });
        }
    }

    if plan.resource_changes.is_empty()
        && plan.output_changes.is_empty()
        && plan.resource_drift.is_empty()
    {
        issues.push(ValidationIssue {
            severity: ValidationSeverity::Warning,
            message: "plan contains no resource or output changes".to_string(),
            address: None,
        });
    }

    let mut seen: HashSet<&str> = HashSet::new();
    for rc in &plan.resource_changes {
        if !seen.insert(rc.address.as_str()) {
            issues.push(ValidationIssue {
                severity: ValidationSeverity::Error,
                message: format!(
                    "duplicate resource address {:?} in resource_changes",
                    rc.address
                ),
                address: Some(rc.address.clone()),
            });
        }
    }

    issues
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn minimal_plan(extra: &str) -> String {
        format!(r#"{{"format_version":"1.2","resource_changes":[]{}}}"#, extra)
    }

    #[test]
    fn parse_minimal_plan() {
        let json = minimal_plan("");
        let plan = parse_plan_reader(Cursor::new(json.as_bytes())).unwrap();
        assert_eq!(plan.format_version, "1.2");
        assert!(plan.resource_changes.is_empty());
        assert!(!plan.errored);
    }

    #[test]
    fn rejects_unsupported_plan_version() {
        let json = r#"{"format_version":"2.0","resource_changes":[]}"#;
        let err = parse_plan_reader(Cursor::new(json.as_bytes())).unwrap_err();
        assert!(err.to_string().contains("unsupported plan format_version"));
    }

    #[test]
    fn tolerates_trailing_newline() {
        let json = format!("{}\n", minimal_plan(""));
        parse_plan_reader(Cursor::new(json.as_bytes())).unwrap();
    }

    #[test]
    fn errored_defaults_to_false_when_absent() {
        let json = minimal_plan("");
        let plan = parse_plan_reader(Cursor::new(json.as_bytes())).unwrap();
        assert!(!plan.errored);
    }

    #[test]
    fn validate_empty_plan_warns() {
        let json = minimal_plan("");
        let plan = parse_plan_reader(Cursor::new(json.as_bytes())).unwrap();
        let issues = validate_plan(&plan);
        assert!(issues
            .iter()
            .any(|i| i.severity == ValidationSeverity::Warning
                && i.message.contains("no resource or output changes")));
    }

    #[test]
    fn validate_destroy_all_warns() {
        let json = r#"{
            "format_version": "1.2",
            "resource_changes": [{
                "address": "aws_instance.web",
                "change": { "actions": ["delete"] }
            }]
        }"#;
        let plan = parse_plan_reader(Cursor::new(json.as_bytes())).unwrap();
        let issues = validate_plan(&plan);
        assert!(issues
            .iter()
            .any(|i| i.severity == ValidationSeverity::Warning
                && i.message.contains("will be destroyed")));
    }

    #[test]
    fn action_is_noop_and_is_replace() {
        let noop = ChangeDetails {
            actions: vec![Action::NoOp],
            before: None,
            after: None,
            after_unknown: None,
            before_sensitive: None,
            after_sensitive: None,
            replace_paths: None,
            importing: None,
            generated_config: None,
        };
        assert!(noop.is_noop());
        assert!(!noop.is_replace());

        let replace = ChangeDetails {
            actions: vec![Action::Delete, Action::Create],
            ..noop.clone()
        };
        assert!(replace.is_replace());
        assert!(!replace.is_noop());
    }

    #[test]
    fn action_forget_deserializes() {
        let json = r#"{
            "format_version": "1.2",
            "resource_changes": [{
                "address": "aws_instance.old",
                "change": { "actions": ["forget"] }
            }]
        }"#;
        let plan = parse_plan_reader(Cursor::new(json.as_bytes())).unwrap();
        assert_eq!(plan.resource_changes[0].change.actions, [Action::Forget]);
    }

    #[test]
    fn importing_field_deserializes() {
        let json = r#"{
            "format_version": "1.2",
            "resource_changes": [{
                "address": "aws_instance.imported",
                "change": {
                    "actions": ["create"],
                    "importing": { "id": "i-abc123" }
                }
            }]
        }"#;
        let plan = parse_plan_reader(Cursor::new(json.as_bytes())).unwrap();
        let import = plan.resource_changes[0].change.importing.as_ref().unwrap();
        assert_eq!(import.id, "i-abc123");
    }

    #[test]
    fn state_accepts_version_zero() {
        let json = r#"{"format_version":"0.2"}"#;
        parse_state_reader(Cursor::new(json.as_bytes())).unwrap();
    }

    #[test]
    fn state_accepts_version_one() {
        let json = r#"{"format_version":"1.0"}"#;
        parse_state_reader(Cursor::new(json.as_bytes())).unwrap();
    }

    #[test]
    fn terraform_state_without_format_version() {
        // Terraform's state spec does not include format_version in all cases.
        let json = r#"{"terraform_version":"1.5.0","values":{"root_module":{}}}"#;
        let state = parse_state_reader(Cursor::new(json.as_bytes())).unwrap();
        assert!(state.format_version.is_none());
        assert_eq!(state.terraform_version.as_deref(), Some("1.5.0"));
    }

    #[test]
    fn terraform_plan_applyable_and_complete() {
        let json = r#"{
            "format_version": "1.0",
            "applyable": true,
            "complete": false,
            "resource_changes": []
        }"#;
        let plan = parse_plan_reader(Cursor::new(json.as_bytes())).unwrap();
        assert_eq!(plan.applyable, Some(true));
        assert_eq!(plan.complete, Some(false));
    }

    #[test]
    fn opentofu_plan_applyable_absent_defaults_to_none() {
        let json = minimal_plan("");
        let plan = parse_plan_reader(Cursor::new(json.as_bytes())).unwrap();
        assert!(plan.applyable.is_none());
        assert!(plan.complete.is_none());
    }

    #[test]
    fn module_id_returns_root_for_root_module() {
        let module = PlannedModule {
            address: None,
            resources: vec![],
            child_modules: vec![],
        };
        assert_eq!(module_id(&module), "root");
    }

    #[test]
    fn parse_state_rejects_wrong_version() {
        let json = r#"{"format_version":"2.0"}"#;
        let err = parse_state_reader(Cursor::new(json.as_bytes())).unwrap_err();
        assert!(err.to_string().contains("unsupported state format_version"));
    }
}
