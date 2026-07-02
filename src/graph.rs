use std::collections::{HashMap, HashSet};
use std::fmt;

use petgraph::algo::{has_path_connecting, toposort};
use petgraph::graph::{DiGraph, EdgeIndex, NodeIndex};
use petgraph::Direction;

use crate::parser::{Action, Plan, PlannedModule};

// ── Node types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Resource,
    Module,
    Output,
}

/// Stored directly in the DiGraph — kept intentionally small.
/// All rich metadata lives in `ImpactGraph::metadata`.
#[derive(Debug, Clone)]
pub struct NodeData {
    pub address: String,
    pub kind: NodeKind,
}

// ── Edge types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeKind {
    /// Declared explicitly via `depends_on` in the configuration.
    Explicit,
    /// Inferred from module membership (resource belongs to a child module).
    Implicit,
}

// ── External metadata ─────────────────────────────────────────────────────────

/// Full resource metadata stored outside the graph to keep the DiGraph lean.
/// Indexed by `NodeIndex` — access via `ImpactGraph::meta`.
#[derive(Debug, Clone, Default)]
pub struct NodeMeta {
    pub resource_type: Option<String>,
    pub actions: Vec<Action>,
    pub provider: Option<String>,
    pub module_address: Option<String>,
}

// ── Cycle error ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct CycleError {
    pub node_address: String,
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "dependency cycle detected involving {:?} — Terraform plans must be acyclic",
            self.node_address
        )
    }
}

impl std::error::Error for CycleError {}

// ── ImpactGraph ───────────────────────────────────────────────────────────────

/// Directed dependency graph built from a parsed plan.
///
/// **Edge convention**: A → B means "A depends on B".
/// - Topological sort gives dependencies before dependents (apply order).
/// - Blast radius of B = follow incoming edges recursively from B.
pub struct ImpactGraph {
    graph: DiGraph<NodeData, EdgeKind>,
    address_to_index: HashMap<String, NodeIndex>,
    // NodeIndex is a u32 offset. Vec<NodeMeta> maps that offset directly to memory:
    // meta[idx.index()] is a single pointer-offset read with no hashing and no Option tag.
    // Invariant: metadata.len() == graph.node_count() at all times (maintained by
    // get_or_insert_node). remove_node() is never called (static analysis graph).
    metadata: Vec<NodeMeta>,
}

impl ImpactGraph {
    fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            address_to_index: HashMap::new(),
            metadata: Vec::new(),
        }
    }

    // ── Graph construction ────────────────────────────────────────────────────

    /// Build the impact graph from a parsed plan.
    ///
    /// Three-phase construction:
    /// 1. Add all nodes (resources, modules, outputs) from `planned_values`.
    /// 2. Add dependency edges from `depends_on` fields.
    /// 3. Annotate nodes with their change actions from `resource_changes`.
    pub fn build(plan: &Plan) -> Self {
        let mut ig = Self::new();

        if let Some(pv) = &plan.planned_values {
            ig.add_nodes_recursive(&pv.root_module);
            for name in pv.outputs.keys() {
                ig.get_or_insert_node(&format!("output.{}", name), NodeKind::Output);
            }
        }

        if let Some(pv) = &plan.planned_values {
            ig.add_edges_recursive(&pv.root_module);
        }

        for rc in &plan.resource_changes {
            if let Some(&idx) = ig.address_to_index.get(&rc.address) {
                ig.metadata[idx.index()].actions = rc.change.actions.clone();
            }
        }

        ig
    }

    fn get_or_insert_node(&mut self, address: &str, kind: NodeKind) -> NodeIndex {
        if let Some(&idx) = self.address_to_index.get(address) {
            return idx;
        }
        let idx = self.graph.add_node(NodeData {
            address: address.to_owned(),
            kind,
        });
        // Push a default so metadata[idx.index()] is always a valid slot.
        // add_node() assigns sequential indices, so push keeps Vec in lock-step.
        // Resource nodes overwrite this default in add_nodes_recursive.
        self.metadata.push(NodeMeta::default());
        self.address_to_index.insert(address.to_owned(), idx);
        idx
    }

    fn add_nodes_recursive(&mut self, module: &PlannedModule) {
        if let Some(addr) = &module.address {
            self.get_or_insert_node(addr, NodeKind::Module);
        }
        for res in &module.resources {
            let idx = self.get_or_insert_node(&res.address, NodeKind::Resource);
            self.metadata[idx.index()] = NodeMeta {
                resource_type: res.resource_type.clone(),
                actions: vec![],
                provider: res.provider_name.clone(),
                module_address: res.module_address.clone(),
            };
        }
        for child in &module.child_modules {
            self.add_nodes_recursive(child);
        }
    }

    fn add_edges_recursive(&mut self, module: &PlannedModule) {
        for res in &module.resources {
            let from_idx = match self.address_to_index.get(&res.address) {
                Some(&idx) => idx,
                None => continue,
            };
            for dep in &res.depends_on {
                // depends_on entries in the plan JSON are already absolute addresses.
                // Never prepend the module prefix — doing so creates double-path nodes
                // like "module.vpc.module.vpc.aws_subnet.pub" that never match anything.
                let to_idx = self.get_or_insert_node(dep, NodeKind::Resource);
                if !self.graph.contains_edge(from_idx, to_idx) {
                    self.graph.add_edge(from_idx, to_idx, EdgeKind::Explicit);
                }
            }
        }

        for child in &module.child_modules {
            self.add_edges_recursive(child);
        }
    }

    // ── Query API ─────────────────────────────────────────────────────────────

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    pub fn index_of(&self, address: &str) -> Option<NodeIndex> {
        self.address_to_index.get(address).copied()
    }

    /// Direct O(1) pointer-offset access — never returns None for valid NodeIndex.
    pub fn meta(&self, idx: NodeIndex) -> &NodeMeta {
        &self.metadata[idx.index()]
    }

    pub fn node_data(&self, idx: NodeIndex) -> Option<&NodeData> {
        self.graph.node_weight(idx)
    }

    // ── DAG validation ────────────────────────────────────────────────────────

    /// Returns `true` if the graph contains no dependency cycles.
    /// Terraform plans are required to be acyclic; this is a fast pre-check.
    /// Time complexity: O(V + E).
    pub fn is_dag(&self) -> bool {
        toposort(&self.graph, None).is_ok()
    }

    /// Returns resource addresses in topological order — dependencies before
    /// the resources that need them (correct `tofu apply` order).
    ///
    /// petgraph's `toposort` returns dependents-first (destroy order); we reverse
    /// so that the result matches Terraform's apply sequence: base infrastructure
    /// first, derived resources last.
    ///
    /// Returns `Err(CycleError)` if the graph contains a cycle.
    pub fn topological_order(&self) -> Result<Vec<String>, CycleError> {
        toposort(&self.graph, None)
            .map(|indices| {
                indices
                    .into_iter()
                    .rev() // reverse: petgraph gives destroy order, we want apply order
                    .filter_map(|idx| self.graph.node_weight(idx))
                    .map(|n| n.address.clone())
                    .collect()
            })
            .map_err(|cycle| {
                let addr = self
                    .graph
                    .node_weight(cycle.node_id())
                    .map(|n| n.address.clone())
                    .unwrap_or_default();
                CycleError { node_address: addr }
            })
    }

    // ── Blast radius ──────────────────────────────────────────────────────────

    /// Returns all resource addresses that would be impacted if `address` changes.
    ///
    /// Implements forward-reachability on the reversed graph: since A → B means
    /// "A depends on B", blast-radius of B = all A that can reach B, i.e. all
    /// nodes reachable from B when edges are followed backwards (via incoming).
    ///
    /// Uses iterative DFS — no recursion, no stack overflow risk.
    pub fn blast_radius(&self, address: &str) -> Vec<String> {
        let start = match self.address_to_index.get(address) {
            Some(&idx) => idx,
            None => return vec![],
        };

        let mut visited: HashSet<NodeIndex> = HashSet::new();
        let mut stack = vec![start];
        visited.insert(start);

        while let Some(current) = stack.pop() {
            // Incoming edges: nodes that depend on `current`
            for dependent in self.graph.neighbors_directed(current, Direction::Incoming) {
                if visited.insert(dependent) {
                    stack.push(dependent);
                }
            }
        }

        visited
            .into_iter()
            .filter(|&idx| idx != start)
            .filter_map(|idx| self.graph.node_weight(idx))
            .map(|n| n.address.clone())
            .collect()
    }

    // ── Transitive reduction ──────────────────────────────────────────────────

    /// Remove redundant edges: if A → B exists AND another path A → … → B
    /// exists, the direct edge A → B adds no information and is removed.
    ///
    /// Produces a cleaner visualization without changing reachability.
    /// **Only call on a DAG** — behavior on cyclic graphs is undefined.
    /// Time complexity: O(E · (V + E)).
    pub fn transitive_reduce(&mut self) {
        // Collect (source, target) of redundant edges.
        // We store endpoints rather than EdgeIndex because petgraph swap-removes
        // edges, which would invalidate indices after the first removal.
        let redundant: Vec<(NodeIndex, NodeIndex)> = self
            .graph
            .edge_indices()
            .collect::<Vec<EdgeIndex>>()
            .into_iter()
            .filter_map(|ei| {
                let (u, v) = self.graph.edge_endpoints(ei)?;
                if self.has_alternative_path(u, v) {
                    Some((u, v))
                } else {
                    None
                }
            })
            .collect();

        for (u, v) in redundant {
            if let Some(ei) = self.graph.find_edge(u, v) {
                self.graph.remove_edge(ei);
            }
        }
    }

    /// Returns `true` if `to` is reachable from `from` via a path of length ≥ 2
    /// (i.e., at least one intermediate node exists). Used for transitive reduction.
    fn has_alternative_path(&self, from: NodeIndex, to: NodeIndex) -> bool {
        for intermediate in self.graph.neighbors(from) {
            if intermediate == to {
                continue; // skip the direct edge
            }
            if has_path_connecting(&self.graph, intermediate, to, None) {
                return true;
            }
        }
        false
    }

    // ── DOT export ────────────────────────────────────────────────────────────

    /// Export the graph in Graphviz DOT format.
    /// Node labels show the resource address and pending change action.
    /// Explicit dependency edges are solid; implicit (module) edges are dashed.
    pub fn to_dot(&self) -> String {
        use std::fmt::Write as FmtWrite;
        let mut out = String::from("digraph impact {\n    rankdir=BT;\n    node [shape=box];\n");

        for idx in self.graph.node_indices() {
            let data = &self.graph[idx];
            let actions = self.metadata[idx.index()]
                .actions
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join("+");

            let (label, color) = if actions.is_empty() {
                (data.address.clone(), "black")
            } else {
                let color = match actions.as_str() {
                    "create" => "green4",
                    "delete" => "red3",
                    "update" => "goldenrod3",
                    "delete+create" | "create+delete" => "orangered",
                    _ => "black",
                };
                (format!("{} [{}]", data.address, actions), color)
            };

            let _ = writeln!(
                out,
                "    n{} [label=\"{}\" color=\"{}\"];",
                idx.index(),
                label.replace('"', "'"),
                color
            );
        }

        for ei in self.graph.edge_indices() {
            let (u, v) = self.graph.edge_endpoints(ei).unwrap();
            let style = match &self.graph[ei] {
                EdgeKind::Explicit => "solid",
                EdgeKind::Implicit => "dashed",
            };
            let _ = writeln!(
                out,
                "    n{} -> n{} [style=\"{}\"];",
                u.index(),
                v.index(),
                style
            );
        }

        out.push('}');
        out
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_plan_reader;
    use std::io::Cursor;

    fn parse(json: &str) -> Plan {
        parse_plan_reader(Cursor::new(json.as_bytes())).unwrap()
    }

    /// Minimal plan JSON with a flat list of resources and their deps.
    fn plan_json(resources: &[(&str, &str, &[&str])]) -> String {
        // resources: (address, type, depends_on[])
        let res_json: Vec<String> = resources
            .iter()
            .map(|(addr, rtype, deps)| {
                let deps_json = deps
                    .iter()
                    .map(|d| format!("\"{}\"", d))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    r#"{{"address":"{addr}","type":"{rtype}","depends_on":[{deps_json}]}}"#
                )
            })
            .collect();

        let changes_json: Vec<String> = resources
            .iter()
            .map(|(addr, _, _)| {
                format!(r#"{{"address":"{addr}","change":{{"actions":["create"]}}}}"#)
            })
            .collect();

        format!(
            r#"{{"format_version":"1.0","planned_values":{{"root_module":{{"resources":[{}]}}}},"resource_changes":[{}]}}"#,
            res_json.join(","),
            changes_json.join(",")
        )
    }

    #[test]
    fn build_nodes_and_edges() {
        let json = plan_json(&[
            ("aws_vpc.main", "aws_vpc", &[]),
            ("aws_subnet.pub", "aws_subnet", &["aws_vpc.main"]),
        ]);
        let plan = parse(&json);
        let g = ImpactGraph::build(&plan);

        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn topological_order_respects_dependencies() {
        let json = plan_json(&[
            ("aws_vpc.main", "aws_vpc", &[]),
            ("aws_subnet.pub", "aws_subnet", &["aws_vpc.main"]),
            ("aws_instance.web", "aws_instance", &["aws_subnet.pub"]),
        ]);
        let plan = parse(&json);
        let g = ImpactGraph::build(&plan);

        let order = g.topological_order().unwrap();
        let pos = |addr: &str| order.iter().position(|a| a == addr).unwrap();

        // vpc must come before subnet, subnet before instance
        assert!(pos("aws_vpc.main") < pos("aws_subnet.pub"));
        assert!(pos("aws_subnet.pub") < pos("aws_instance.web"));
    }

    #[test]
    fn blast_radius_finds_all_dependents() {
        // vpc ← subnet ← instance
        //              ← lb
        let json = plan_json(&[
            ("aws_vpc.main", "aws_vpc", &[]),
            ("aws_subnet.pub", "aws_subnet", &["aws_vpc.main"]),
            ("aws_instance.web", "aws_instance", &["aws_subnet.pub"]),
            ("aws_lb.front", "aws_lb", &["aws_subnet.pub"]),
        ]);
        let plan = parse(&json);
        let g = ImpactGraph::build(&plan);

        let mut radius = g.blast_radius("aws_vpc.main");
        radius.sort();

        assert!(radius.contains(&"aws_subnet.pub".to_string()));
        assert!(radius.contains(&"aws_instance.web".to_string()));
        assert!(radius.contains(&"aws_lb.front".to_string()));
        assert!(!radius.contains(&"aws_vpc.main".to_string()));
    }

    #[test]
    fn blast_radius_of_leaf_is_empty() {
        let json = plan_json(&[
            ("aws_vpc.main", "aws_vpc", &[]),
            ("aws_subnet.pub", "aws_subnet", &["aws_vpc.main"]),
        ]);
        let plan = parse(&json);
        let g = ImpactGraph::build(&plan);

        // aws_subnet.pub has no dependents
        let radius = g.blast_radius("aws_subnet.pub");
        assert!(radius.is_empty());
    }

    #[test]
    fn is_dag_true_for_acyclic_graph() {
        let json = plan_json(&[
            ("aws_vpc.main", "aws_vpc", &[]),
            ("aws_subnet.pub", "aws_subnet", &["aws_vpc.main"]),
        ]);
        let g = ImpactGraph::build(&parse(&json));
        assert!(g.is_dag());
    }

    #[test]
    fn transitive_reduce_removes_redundant_edge() {
        // A → B → C and A → C (the second A→C is redundant)
        let json = plan_json(&[
            ("c", "null_resource", &[]),
            ("b", "null_resource", &["c"]),
            ("a", "null_resource", &["b", "c"]), // a→c is redundant via a→b→c
        ]);
        let plan = parse(&json);
        let mut g = ImpactGraph::build(&plan);

        let edges_before = g.edge_count();
        g.transitive_reduce();
        let edges_after = g.edge_count();

        assert!(edges_after < edges_before, "redundant edge should have been removed");
        assert_eq!(edges_after, 2); // only a→b and b→c remain
    }

    #[test]
    fn to_dot_produces_valid_output() {
        let json = plan_json(&[
            ("aws_vpc.main", "aws_vpc", &[]),
            ("aws_subnet.pub", "aws_subnet", &["aws_vpc.main"]),
        ]);
        let g = ImpactGraph::build(&parse(&json));
        let dot = g.to_dot();

        assert!(dot.starts_with("digraph impact {"));
        assert!(dot.contains("aws_vpc.main"));
        assert!(dot.contains("aws_subnet.pub"));
        assert!(dot.ends_with('}'));
    }

    #[test]
    fn action_annotation_from_resource_changes() {
        let json = plan_json(&[("aws_vpc.main", "aws_vpc", &[])]);
        let plan = parse(&json);
        let g = ImpactGraph::build(&plan);

        let idx = g.index_of("aws_vpc.main").unwrap();
        assert_eq!(g.meta(idx).actions, [Action::Create]);
    }

    #[test]
    fn unknown_address_blast_radius_returns_empty() {
        let json = plan_json(&[("aws_vpc.main", "aws_vpc", &[])]);
        let g = ImpactGraph::build(&parse(&json));
        assert!(g.blast_radius("does_not_exist").is_empty());
    }
}
