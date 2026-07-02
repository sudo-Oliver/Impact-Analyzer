mod graph;
mod parser;
mod ui;
mod version;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use graph::ImpactGraph;
use parser::{
    detect_drift, parse_plan_file, parse_plan_reader, validate_plan, DriftKind, ValidationSeverity,
};

// ── CLI definition ────────────────────────────────────────────────────────────

/// Enterprise Impact Analyzer for Terraform / OpenTofu plans.
///
/// Reads JSON plan output from `tofu show -json` or `terraform show -json`
/// and provides drift detection, safety validation, blast-radius analysis,
/// and dependency graph export.
#[derive(Parser)]
#[command(name = "eia", version, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a plan for safety issues and drift.
    ///
    /// Designed for CI/CD pipelines.
    /// Exit 0 = no issues, 1 = technical error, 2 = validation issues found.
    ///
    /// SCOPE: This tool analyzes the explicit and implicit dependencies that
    /// OpenTofu / Terraform records in the JSON plan. Dependencies expressed as
    /// hardcoded strings (e.g. a subnet ID passed as a literal instead of a
    /// resource reference) are invisible to the provider and therefore absent
    /// from the plan — this tool cannot detect them either.
    ///
    /// For complete coverage, combine eia with a static source linter:
    ///   tflint   https://github.com/terraform-linters/tflint
    ///   checkov  https://github.com/bridgecrewio/checkov
    ///
    /// Recommended pipeline order:
    ///   1. tflint / checkov   (catches hardcoded IDs and code-style issues)
    ///   2. tofu plan -out=plan.binary && tofu show -json plan.binary > plan.json
    ///   3. eia check plan.json  (catches logical risks and blast radius)
    Check {
        /// Path to the JSON plan file produced by `tofu show -json`. Use '-' for stdin.
        plan: String,
        /// Output format.
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },

    /// Show every resource impacted if a specific resource changes.
    ///
    /// Traverses the dependency graph backwards from the given address and
    /// returns every resource that transitively depends on it.
    ///
    /// SCOPE: Only dependencies visible in the JSON plan are considered.
    /// Hardcoded resource references (string literals instead of Terraform
    /// references) do not appear in depends_on and are therefore not tracked.
    /// Use tflint or checkov to catch those at the source level.
    Blast {
        /// Path to the JSON plan file. Use '-' for stdin.
        plan: String,
        /// Full resource address (e.g. `module.vpc.aws_subnet.public[0]`).
        address: String,
        /// Output format.
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },

    /// Browse a plan interactively in the terminal.
    ///
    /// 3-panel layout: resource list on the left (color-coded by action),
    /// live-calculated blast radius on the right. Navigate with ↑/↓ or j/k,
    /// quit with q or Esc.
    View {
        /// Path to the JSON plan file. Use '-' for stdin.
        plan: String,
    },

    /// Generate a transitively-reduced dependency graph in Graphviz DOT format.
    Graph {
        /// Path to the JSON plan file. Use '-' for stdin.
        plan: String,
        /// Write DOT output to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Render to SVG and open in the system viewer (requires Graphviz `dot`).
        #[arg(long)]
        view: bool,
        /// Skip transitive reduction (faster, but graph may contain redundant edges).
        #[arg(long)]
        no_reduce: bool,
    },
}

#[derive(Clone, ValueEnum)]
enum Format {
    Text,
    Json,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(e) => {
            let is_tty = io::stderr().is_terminal();
            let (red, reset) = if is_tty { ("\x1b[31m", "\x1b[0m") } else { ("", "") };
            eprintln!("{}error:{} {}", red, reset, e);
            for cause in e.chain().skip(1) {
                eprintln!("       {}", cause);
            }
            1
        }
    };
    process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Check { plan, format } => cmd_check(&plan, format),
        Command::Blast { plan, address, format } => cmd_blast(&plan, &address, format),
        Command::View  { plan } => cmd_view(&plan),
        Command::Graph { plan, out, view, no_reduce } => cmd_graph(&plan, out, view, no_reduce),
    }
}

// ── `eia check` ───────────────────────────────────────────────────────────────

fn cmd_check(plan_path: &str, format: Format) -> Result<i32> {
    // Kick off binary detection before parsing — the 200 ms timeout runs in
    // parallel while we load and parse the plan, so overhead is usually zero.
    let binary_handle = version::spawn_binary_detect();

    let plan = load_plan(plan_path)?;

    // 1. Format-version minor-ahead check (synchronous — plan is in hand).
    if let Some(version::VersionWarning::FormatMinorAhead { found }) =
        version::check_format_version(&plan.format_version)
    {
        eprintln!(
            "warning: plan format_version {} is newer than tested {} — \
             unknown fields will be ignored; consider updating eia",
            found,
            version::MAX_KNOWN_FORMAT,
        );
    }

    // 2. Binary version check (join the background thread — near-instant by now).
    if let Ok(Some(ref info)) = binary_handle.join() {
        if let Some(version::VersionWarning::BinaryAhead { ref name, ref found, ref tested_max }) =
            version::check_binary_version(info)
        {
            eprintln!(
                "warning: {} {} is newer than tested {}.x — \
                 plan output may contain fields eia does not yet handle",
                name, found, tested_max,
            );
        }
    }

    let issues = validate_plan(&plan);
    let drift = detect_drift(&plan);
    let graph = ImpactGraph::build(&plan);
    let cycle = graph.topological_order().err();

    let has_issues = !issues.is_empty() || cycle.is_some();
    let drift_non_trivial: Vec<_> = drift
        .iter()
        .filter(|d| d.drift_kind != DriftKind::Unchanged)
        .collect();

    match format {
        Format::Json => {
            let issues_json: Vec<_> = issues
                .iter()
                .map(|i| {
                    serde_json::json!({
                        "severity": match i.severity {
                            ValidationSeverity::Error   => "error",
                            ValidationSeverity::Warning => "warning",
                        },
                        "message": i.message,
                        "address": i.address,
                    })
                })
                .collect();

            let drift_json: Vec<_> = drift_non_trivial
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "address": d.address,
                        "kind": match d.drift_kind {
                            DriftKind::Added     => "added",
                            DriftKind::Removed   => "removed",
                            DriftKind::Modified  => "modified",
                            DriftKind::Unchanged => "unchanged",
                        },
                    })
                })
                .collect();

            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "valid":  !has_issues,
                    "cycle":  cycle.as_ref().map(|e| e.node_address.clone()),
                    "issues": issues_json,
                    "drift":  drift_json,
                }))?
            );
        }

        Format::Text => {
            let color = colors();

            if let Some(ref e) = cycle {
                eprintln!(
                    "{}{} Cycle detected:{} {}",
                    color.red, color.bold, color.reset, e.node_address
                );
            }

            for issue in &issues {
                let (sigil, col) = match issue.severity {
                    ValidationSeverity::Error   => ("✗", color.red),
                    ValidationSeverity::Warning => ("⚠", color.yellow),
                };
                let addr = issue
                    .address
                    .as_deref()
                    .map(|a| format!(" ({})", a))
                    .unwrap_or_default();
                eprintln!("{}{} {}{}{}", col, sigil, issue.message, addr, color.reset);
            }

            if !drift_non_trivial.is_empty() {
                println!("\nDrift detected ({} resource(s)):", drift_non_trivial.len());
                for d in &drift_non_trivial {
                    let (sigil, col) = match d.drift_kind {
                        DriftKind::Added    => ("+", color.green),
                        DriftKind::Removed  => ("-", color.red),
                        DriftKind::Modified => ("~", color.yellow),
                        DriftKind::Unchanged => (" ", color.reset),
                    };
                    println!("  {}{} {}{}", col, sigil, d.address, color.reset);
                }
            }

            if !has_issues && drift_non_trivial.is_empty() {
                println!("{}✓ No issues found. Plan is safe to apply.{}", color.green, color.reset);
            }
        }
    }

    Ok(if has_issues { 2 } else { 0 })
}

// ── `eia blast` ───────────────────────────────────────────────────────────────

fn cmd_blast(plan_path: &str, address: &str, format: Format) -> Result<i32> {
    let plan = load_plan(plan_path)?;
    let graph = ImpactGraph::build(&plan);
    let mut affected = graph.blast_radius(address);
    affected.sort();

    match format {
        Format::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "source":         address,
                    "affected_count": affected.len(),
                    "affected":       affected,
                }))?
            );
        }

        Format::Text => {
            let color = colors();
            if affected.is_empty() {
                println!("No other resources depend on {}.", address);
            } else {
                println!(
                    "Blast radius of {}{}{} ({}{}{} resource(s) affected):\n",
                    color.bold,
                    address,
                    color.reset,
                    color.yellow,
                    affected.len(),
                    color.reset,
                );
                for addr in &affected {
                    println!("  {}", addr);
                }
            }
        }
    }

    Ok(0)
}

// ── `eia view` ────────────────────────────────────────────────────────────────

fn cmd_view(plan_path: &str) -> Result<i32> {
    let plan = load_plan(plan_path)?;
    ui::run_tui(plan)?;
    Ok(0)
}

// ── `eia graph` ───────────────────────────────────────────────────────────────

fn cmd_graph(
    plan_path: &str,
    out: Option<PathBuf>,
    view: bool,
    no_reduce: bool,
) -> Result<i32> {
    let plan = load_plan(plan_path)?;
    let mut graph = ImpactGraph::build(&plan);

    if !no_reduce {
        graph.transitive_reduce();
    }

    let dot = graph.to_dot();

    if view {
        open_with_graphviz(&dot)?;
        return Ok(0);
    }

    match out {
        Some(ref path) => {
            std::fs::write(path, &dot)
                .with_context(|| format!("failed to write DOT output to {:?}", path))?;
            eprintln!("DOT written to {}", path.display());
        }
        None => print!("{}", dot),
    }

    Ok(0)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Load a plan from a file path or from stdin if path is "-".
fn load_plan(path: &str) -> Result<parser::Plan> {
    if path == "-" {
        parse_plan_reader(io::stdin().lock()).context("could not read plan from stdin")
    } else {
        parse_plan_file(path)
    }
}

/// Render DOT content to SVG via `dot` and open with the system viewer.
fn open_with_graphviz(dot: &str) -> Result<()> {
    use std::process::{Command, Stdio};

    let svg_path = std::env::temp_dir().join(format!(
        "eia_graph_{}.svg",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    ));

    // Pipe DOT into `dot -Tsvg -o <file>`.
    let mut child = Command::new("dot")
        .args(["-Tsvg", "-o", svg_path.to_str().unwrap_or("eia_graph.svg")])
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to spawn `dot` — is Graphviz installed? (brew install graphviz)")?;

    child
        .stdin
        .take()
        .context("could not open dot stdin")?
        .write_all(dot.as_bytes())
        .context("failed to write DOT to graphviz")?;

    child.wait().context("dot process failed")?;

    open_path(&svg_path);
    eprintln!("Graph written to {}", svg_path.display());
    Ok(())
}

/// Open a file with the system default application.
fn open_path(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = process::Command::new("cmd")
        .args(["/c", "start", "", path.to_str().unwrap_or("")])
        .spawn();
}

// ── Color support ─────────────────────────────────────────────────────────────

struct Colors {
    red:    &'static str,
    yellow: &'static str,
    green:  &'static str,
    bold:   &'static str,
    reset:  &'static str,
}

/// Returns ANSI color codes when stdout is a real terminal; empty strings otherwise.
/// This prevents color garbage in CI logs, Jenkins, and non-interactive pipelines.
fn colors() -> Colors {
    if io::stdout().is_terminal() {
        Colors {
            red:    "\x1b[31m",
            yellow: "\x1b[33m",
            green:  "\x1b[32m",
            bold:   "\x1b[1m",
            reset:  "\x1b[0m",
        }
    } else {
        Colors { red: "", yellow: "", green: "", bold: "", reset: "" }
    }
}
