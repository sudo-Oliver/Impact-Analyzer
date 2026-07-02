# eia — Enterprise Impact Analyzer

A fast, pipeline-safe CLI for analyzing Terraform and OpenTofu JSON plans *before* you apply them. Written in Rust.

```
eia check plan.json                        # Validate for safety issues and drift
eia blast plan.json module.vpc.aws_vpc.main  # Who breaks if this changes?
eia view  plan.json                        # Interactive TUI browser
eia graph plan.json --view                 # Render dependency graph as SVG
```

---

## The problem this solves

You run `tofu plan`, it looks fine, you apply — and something breaks two layers down. A VPC update
takes out the subnets, which breaks the RDS cluster, which kills the app servers. By the time the
error surfaces, the damage is done.

`eia` answers the questions you should ask *before* you hit apply:

- Is this plan safe? Does anything look like an accidental destroy-all?
- Has infrastructure drifted from what the plan expects?
- If I change *this* resource, what else breaks?
- What does the full dependency graph look like?

It reads the JSON plan output (`tofu show -json`) and gives you answers in milliseconds — no cloud
API calls, no provider plugins, no internet access required.

---

## Quick start

```bash
# 1. Install
brew tap sudo-Oliver/tap
brew install eia
brew install graphviz        # only needed for: eia graph --view

# 2. Generate a plan
tofu plan -out=plan.binary
tofu show -json plan.binary > plan.json

# 3. Check it
eia check plan.json
```

If the plan is clean, exit code is `0`. If issues are found, exit code is `2` and details are
printed to stderr. A technical error (bad file, wrong format) exits with `1`.

Update to the latest version at any time:

```bash
brew upgrade eia
```

---

## Commands

### `eia check`

Validates the plan and reports drift. Designed for CI/CD pipelines.

```bash
# From a file
eia check plan.json

# From stdin (pipe from tofu directly)
tofu show -json .terraform/plans/latest.json | eia check -

# Machine-readable JSON for CI scripts and PR bots
eia check plan.json --format json | jq '.issues[].severity'
```

Exit codes: `0` clean · `1` technical error · `2` validation issues found

**What it checks:**
- `errored: true` in the plan (the provider signals a failed plan)
- Empty resource addresses (malformed plan output)
- Destroy-all pattern (every resource scheduled for deletion — almost always unintentional)
- Duplicate addresses in `resource_changes`
- Dependency cycles in the resource graph
- Drift between `prior_state`, `planned_values`, and `resource_changes`

```
$ eia check plan.json
⚠ All 47 resources are scheduled for deletion — verify this is intentional
  ~ aws_instance.app_server     (modified outside Terraform)
  - aws_security_group.bastion  (removed outside Terraform)

$ echo $?
2
```

**Version intelligence:** `eia check` automatically detects the `tofu` or `terraform` binary on
your PATH in a background thread (200 ms hard timeout). If your binary or plan format is newer than
the tested ceiling, you get a warning — never a silent parse failure.

### `eia blast`

Computes the blast radius of a single resource: every resource that transitively depends on it.

```bash
eia blast plan.json module.network.aws_vpc.main
```

```
Blast radius of module.network.aws_vpc.main (8 resource(s) affected):

  aws_db_subnet_group.primary
  aws_elasticache_subnet_group.cache
  aws_instance.app[0]
  aws_instance.app[1]
  aws_lb.public
  aws_lb_target_group.api
  aws_security_group.app
  aws_subnet.private[0]
```

Use `--format json` to feed results into PR comment bots or approval gates.

### `eia view`

An interactive TUI that lets you scroll through every resource change and see its blast radius live.

```
┌─ eia  Enterprise Impact Analyzer ──────────────────────────────────────────┐
│  47 resource(s)  ·  12 change(s)  ·  2 drift(s)                           │
├─ Resources ─────────────────┬─ Details ──────────────────────────────────── ┤
│  + aws_vpc.main             │ Address   module.network.aws_vpc.main         │
│▶ ~ aws_subnet.private[0]    │ Type      aws_subnet                          │
│  ~ aws_subnet.private[1]    │ Provider  registry.terraform.io/hashicorp/aws │
│  - aws_security_group.old   │ Action    update                              │
│  + aws_instance.app[0]      │                                               │
│  + aws_instance.app[1]      │ Blast radius  (3 affected)                    │
│  ~ aws_lb.public            │   ↳ aws_instance.app[0]                       │
│  ...                        │   ↳ aws_instance.app[1]                       │
│                             │   ↳ aws_lb.public                             │
├─────────────────────────────┴────────────────────────────────────────────── ┤
│  ↑ ↓  or  j k   Navigate    q / Esc   Quit                                 │
└─────────────────────────────────────────────────────────────────────────────┘
```

Color coding: `+` green (create) · `~` yellow (update) · `-` red (delete) · `±` magenta (replace) · cyan (read) · gray (no-op)

The blast radius is recalculated live on every keypress using an iterative DFS over the dependency
graph — no stale cache, no pre-computation overhead.

### `eia graph`

Exports the transitively-reduced dependency graph as Graphviz DOT.

```bash
eia graph plan.json                    # stdout
eia graph plan.json --out deps.dot     # write to file
eia graph plan.json --view             # render SVG and open (requires Graphviz)
eia graph plan.json --no-reduce        # skip transitive reduction
```

Transitive reduction removes redundant edges (if A→B→C and A→C, the direct A→C edge is removed)
so the graph stays readable even for large plans.

---

## Architectural scope and best practices

### What eia can and cannot see

`eia` operates on the JSON plan file, which is the output of `tofu show -json`. The plan records
every resource dependency that OpenTofu or Terraform resolved statically at plan time: both explicit
`depends_on` entries and implicit references such as `subnet_ids = module.vpc.aws_subnet.public[*].id`.

**The limit:** If a resource reference is expressed as a hardcoded string literal instead of a
Terraform resource reference, the provider never records it as a dependency. It does not appear in
`depends_on`. `eia` cannot see it.

Example of the problem:

```hcl
# This dependency IS visible — tofu knows aws_subnet.public must exist first
resource "aws_db_subnet_group" "main" {
  subnet_ids = [aws_subnet.public.id]
}

# This dependency is NOT visible — the subnet ID is a hardcoded string
resource "aws_db_subnet_group" "main" {
  subnet_ids = ["subnet-0abc1234"]
}
```

This is a "garbage in, garbage out" constraint of the plan format, not a limitation of `eia`
specifically. No plan-analysis tool can recover information that the provider did not emit.

### Recommended CI/CD pipeline

Run three layers of analysis in sequence. Each catches a different class of problem:

```
Step 1: Static source analysis
  tflint   — catches hardcoded IDs, deprecated syntax, provider-specific anti-patterns
             https://github.com/terraform-linters/tflint

  checkov  — security and compliance policy checks on the source code
             https://github.com/bridgecrewio/checkov

Step 2: Plan generation
  tofu plan -out=plan.binary
  tofu show -json plan.binary > plan.json

Step 3: Plan analysis
  eia check plan.json
```

In a CI script:

```bash
# Layer 1 — source linting (catches hardcoded references before plan is generated)
tflint --recursive
checkov -d . --quiet

# Layer 2 — plan generation
tofu plan -out=plan.binary
tofu show -json plan.binary > plan.json

# Layer 3 — plan analysis (catches logical risks and blast radius)
eia check plan.json
```

The layers are complementary, not redundant. `tflint` and `checkov` work on source code and catch
issues before any cloud API is called. `eia` works on the resolved plan and can answer questions
about runtime dependency cascades that no static linter can see.

---

## Installation

### Homebrew

```bash
brew tap sudo-Oliver/tap
brew install eia
brew upgrade eia          # update
brew uninstall eia        # remove
```

### Build from source

Requires Rust 1.74+.

```bash
git clone https://github.com/sudo-Oliver/Impact-Analyzer
cd Impact-Analyzer
cargo build --release
# Binary is at ./target/release/eia
```

The `eia graph --view` flag additionally requires
[Graphviz](https://graphviz.org) (`brew install graphviz`).

---

## Architecture

```
src/
├── parser.rs   Streaming JSON deserialization + semantic validation
├── graph.rs    DAG construction, topological sort, blast radius, transitive reduction
├── version.rs  Background binary detection and format-version checks
├── main.rs     CLI (clap) with subcommands, exit codes, TTY-aware color
└── ui.rs       Interactive TUI (ratatui) with live blast-radius rendering
```

### Parser layer (`parser.rs`)

Reads plan output using `serde_json::Deserializer::from_reader` with a `BufReader`, so files of
any size stream through without loading into RAM. Covers the full OpenTofu and Terraform JSON plan
spec including:

- `resource_drift` (OpenTofu extension)
- `Action::Forget` (OpenTofu: removes from state without destroying infrastructure)
- `importing` blocks (import-generated config)
- Terraform-only fields (`applyable`, `complete`, `proposed_unknown`)
- `errored: bool` with `#[serde(default)]` — absent in clean plans, must not cause a parse failure

`de.end()` is called after deserialization to catch truncated files. It tolerates trailing
whitespace and newlines (common from CI log redirects) but rejects genuine trailing content.

### Graph layer (`graph.rs`)

Builds a `petgraph::DiGraph` from `depends_on` arrays and module containment. Edge convention:
A→B means "A depends on B." `depends_on` entries in the plan are always absolute addresses —
the graph layer never prepends module prefixes.

**Metadata storage:** `Vec<NodeMeta>` indexed by `NodeIndex::index()` — direct pointer-offset
access, no hashing.

**Blast radius:** Iterative DFS over `Direction::Incoming` edges.

**Transitive reduction:** Collects redundant edges as `(NodeIndex, NodeIndex)` pairs and uses
`find_edge` at removal time, avoiding the `EdgeIndex` invalidation problem from petgraph's
swap-remove.

### Version intelligence (`version.rs`)

Spawned at the start of `eia check` in a background thread with a 200 ms hard timeout. Probes
`tofu --version` and `terraform --version` in parallel via sub-threads, takes the first reply, and
uses the `semver` crate to compare against tested-maximum version ceilings. By the time plan
parsing is done, the timeout has almost always already elapsed — zero measurable overhead.

### Interface layer (`main.rs`, `ui.rs`)

The CLI uses `clap` derive macros. Errors are formatted as two lines:

```
error: could not open plan file 'missing.json'
       No such file or directory (os error 2)
```

Color output is gated on `io::stdout().is_terminal()` — no ANSI codes in CI pipelines.

The TUI renders every frame from scratch (immediate mode). The blast radius is calculated inside
the render closure using O(V+E) iterative DFS — imperceptibly fast even for plans with hundreds
of resources.

---

## Compatibility

| Tool      | Versions                              |
|-----------|---------------------------------------|
| OpenTofu  | 1.x (all plan format versions 1.x)    |
| Terraform | 0.12+ (plan format 1.x)               |

The parser uses `#[serde(default)]` throughout and never sets `deny_unknown_fields`, so new fields
added by future OpenTofu or Terraform releases are silently ignored rather than causing parse
failures.

---

## Development

```bash
cargo test          # 39 unit tests across parser, graph, and version layers
cargo run -- check tests/fixtures/plan.json
cargo run -- view  tests/fixtures/plan.json
```
