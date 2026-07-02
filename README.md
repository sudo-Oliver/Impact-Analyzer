# eia — Enterprise Impact Analyzer

A fast, pipeline-safe CLI for analyzing Terraform and OpenTofu JSON plans before you apply them. Written in Rust.

```
eia check plan.json          # Validate for safety issues and drift
eia blast plan.json aws_vpc.main   # Who breaks if this changes?
eia view  plan.json          # Interactive TUI browser
eia graph plan.json --view   # Render dependency graph as SVG
```

---

## Why this exists

A `terraform apply` on a large plan can silently cascade. One deleted security group can tear down a database cluster. By the time the error surfaces, the damage is done.

`eia` reads the JSON plan output (`tofu show -json` / `terraform show -json`) and answers the questions that matter *before* you apply:

- Is this plan safe? Does anything look like a destroy-all?
- Has infrastructure drifted from what the plan expects?
- If I change *this* resource, what else breaks?
- What does the full dependency graph look like?

---

## Installation

```bash
git clone https://github.com/your-username/impact-analyzer
cd impact-analyzer
cargo build --release
# Binary is at ./target/release/eia
```

Requires Rust 1.74+ and, for `--view`, [Graphviz](https://graphviz.org) (`brew install graphviz`).

---

## Commands

### `eia check`

Validates the plan and reports drift. Designed for CI/CD pipelines.

```bash
tofu show -json .terraform/plans/latest.json | eia check -
```

Exit codes:
- `0` — no issues
- `1` — technical error (I/O, parse failure)
- `2` — validation issues found

**What it checks:**
- `errored: true` in the plan (Terraform/OpenTofu signals a failed plan)
- Empty resource addresses (malformed plan output)
- Destroy-all pattern (every resource is being deleted — almost always a mistake)
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

```bash
# Machine-readable output for CI scripts
eia check plan.json --format json | jq '.issues[].severity'
```

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

Use `--format json` to integrate into PR comment bots or approval gates.

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

The blast radius is calculated **live on every keypress** using an iterative DFS over the dependency graph. No stale cache, no pre-computation overhead.

### `eia graph`

Exports the transitively-reduced dependency graph as Graphviz DOT.

```bash
eia graph plan.json                    # stdout
eia graph plan.json --out deps.dot     # write to file
eia graph plan.json --view             # render SVG and open
eia graph plan.json --no-reduce        # skip transitive reduction
```

Transitive reduction removes redundant edges (if A→B→C and A→C, the direct A→C edge is removed) so the graph stays readable even for large plans.

---

## Architecture

```
src/
├── parser.rs   Streaming JSON deserialization + semantic validation
├── graph.rs    DAG construction, topological sort, blast radius, transitive reduction
├── main.rs     CLI (clap) with subcommands, exit codes, TTY-aware color
└── ui.rs       Interactive TUI (ratatui) with live blast-radius rendering
```

### Parser layer (`parser.rs`)

Reads plan output using `serde_json::Deserializer::from_reader` with a `BufReader`, so files of any size stream through without loading into RAM. Covers the full OpenTofu and Terraform JSON plan spec including:

- `resource_drift` (OpenTofu extension)
- `Action::Forget` (OpenTofu: removes from state without destroying infrastructure)
- `importing` blocks (import-generated config)
- Terraform-only fields (`applyable`, `complete`, `proposed_unknown`)
- `errored: bool` with `#[serde(default)]` — absent in clean plans, must not cause a parse failure

`de.end()` is called after deserialization to catch truncated files. It tolerates trailing whitespace and newlines (common from CI log redirects) but rejects genuine trailing content.

### Graph layer (`graph.rs`)

Builds a `petgraph::DiGraph` from `depends_on` arrays and module containment. Edge convention: A→B means "A depends on B."

**Metadata storage:** `Vec<NodeMeta>` indexed by `NodeIndex::index()` — direct pointer-offset access, no hashing. The invariant `metadata.len() == graph.node_count()` is maintained by `get_or_insert_node`, which pushes a `NodeMeta::default()` entry every time a new node is added. `remove_node` is never called (static analysis graph), so indices never shift.

**Topological sort:** `petgraph::algo::toposort` returns nodes in dependents-first order (destroy order). `.rev()` converts to apply order.

**Blast radius:** Iterative DFS over `Direction::Incoming` edges — finds all nodes that transitively depend on the target.

**Transitive reduction:** Collects redundant edges as `(NodeIndex, NodeIndex)` pairs rather than `EdgeIndex`, because petgraph's `remove_edge` uses swap-remove which invalidates existing `EdgeIndex` values. `find_edge` is used to locate each edge at removal time.

### Interface layer (`main.rs`, `ui.rs`)

The CLI uses `clap` derive macros. Errors are formatted as two lines:

```
error: could not open plan file 'missing.json'
       No such file or directory (os error 2)
```

Color output is gated on `io::stdout().is_terminal()` — no ANSI codes in CI pipelines.

The TUI renders every frame from scratch (immediate mode). The blast radius is calculated inside the render closure itself. With Rust's O(V+E) iterative DFS this is imperceptibly fast even for plans with hundreds of resources.

---

## Compatibility

| Tool | Versions |
|---|---|
| OpenTofu | 1.x (all plan format versions 1.x) |
| Terraform | 0.12+ (plan format 1.x, state format 0.x and 1.x) |

The parser uses `#[serde(default)]` throughout and never sets `deny_unknown_fields`, so new fields added by future OpenTofu or Terraform releases are silently ignored rather than causing parse failures.

---

## Development

```bash
cargo test          # 25 unit tests across parser and graph layers
cargo run -- check tests/fixtures/plan.json
cargo run -- view  tests/fixtures/plan.json
```
