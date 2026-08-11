use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de::Error as DeError};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const GRAPH_API_VERSION: &str = "gloop.dev/v1alpha1";
pub const GRAPH_KIND: &str = "Graph";
pub const MAX_DURATION_SECONDS: u64 = 31_536_000;
pub const MAX_FAN_OUT: usize = 256;
pub const MAX_PARALLELISM: usize = 256;
const MAX_PARALLEL_WORKLOAD: usize = 1_048_576;
pub const MAX_RETRY_ATTEMPTS: u32 = 16;
pub const MAX_LOOP_ITERATIONS: u32 = 1_024;
const MAX_LOOP_WORKLOAD: usize = 1_000_000;
const MAX_GRAPH_NESTING_DEPTH: usize = 32;
const MAX_NESTED_NODE_COUNT: usize = 10_000;
const MAX_NODE_ID_BYTES: usize = 256;
const MAX_EDGE_ID_BYTES: usize = 256;
const MAX_RESOURCE_ID_BYTES: usize = 256;
const MAX_PROFILE_REFERENCE_BYTES: usize = 64;
const MAX_MODEL_ID_BYTES: usize = 512;
pub const MAX_GRAPH_SOURCE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CONTEXT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

struct ValidateLoopParams<'a> {
    node_path: &'a str,
    until: &'a LoopCondition,
    max_iterations: u32,
    stagnation_after: u32,
    node_retry_attempts: u32,
    issues: &'a mut Vec<ValidationIssue>,
    nested_node_budget: &'a mut usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: GraphMetadata,
    pub spec: GraphSpec,
}

impl Graph {
    pub fn new(name: impl Into<String>, goal: impl Into<String>, nodes: Vec<Node>) -> Self {
        Self {
            api_version: GRAPH_API_VERSION.to_owned(),
            kind: GRAPH_KIND.to_owned(),
            metadata: GraphMetadata {
                name: name.into(),
                version: "1.0.0".to_owned(),
                description: None,
                labels: IndexMap::new(),
            },
            spec: GraphSpec {
                goal: goal.into(),
                policies: GraphPolicies::default(),
                budgets: RunBudgets::default(),
                nodes,
                edges: Vec::new(),
            },
        }
    }

    pub fn from_yaml_str(source: &str) -> Result<Self, GraphError> {
        if source.len() > MAX_GRAPH_SOURCE_BYTES {
            return Err(GraphError::SourceTooLarge {
                source_bytes: source.len(),
                max_bytes: MAX_GRAPH_SOURCE_BYTES,
            });
        }
        serde_yaml_ng::from_str(source).map_err(GraphError::Yaml)
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, GraphError> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| GraphError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        let mut source = String::new();
        let mut bounded = io::BufReader::new(file).take((MAX_GRAPH_SOURCE_BYTES + 1) as u64);
        bounded
            .read_to_string(&mut source)
            .map_err(|source| GraphError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        Self::from_yaml_str(&source)
    }

    pub fn to_yaml(&self) -> Result<String, GraphError> {
        serde_yaml_ng::to_string(self).map_err(GraphError::Yaml)
    }

    pub fn hash(&self) -> Result<String, GraphError> {
        let canonical = serde_json::to_vec(self).map_err(GraphError::Json)?;
        Ok(hex::encode(Sha256::digest(canonical)))
    }

    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        let mut nested_node_budget = MAX_NESTED_NODE_COUNT;
        self.validate_at("$", &mut issues, &mut nested_node_budget, true);
        issues
    }

    pub fn compile(&self) -> Result<CompiledGraph, GraphError> {
        let issues = self.validate();
        if issues
            .iter()
            .any(|issue| issue.severity == IssueSeverity::Error)
        {
            return Err(GraphError::Validation(issues));
        }

        let mut index = IndexMap::new();
        for (position, node) in self.spec.nodes.iter().enumerate() {
            index.insert(node.id.clone(), position);
        }

        let mut incoming: IndexMap<String, Vec<usize>> = self
            .spec
            .nodes
            .iter()
            .map(|node| (node.id.clone(), Vec::new()))
            .collect();
        let mut outgoing = incoming.clone();
        for (edge_index, edge) in self.spec.edges.iter().enumerate() {
            incoming
                .get_mut(&edge.to)
                .expect("validated destination")
                .push(edge_index);
            outgoing
                .get_mut(&edge.from)
                .expect("validated source")
                .push(edge_index);
        }

        let order = stable_topological_order(&self.spec.nodes, &self.spec.edges)
            .expect("validated graph is acyclic");

        Ok(CompiledGraph {
            graph: self.clone(),
            order,
            index,
            incoming,
            outgoing,
        })
    }

    fn validate_at(
        &self,
        path: &str,
        issues: &mut Vec<ValidationIssue>,
        nested_node_budget: &mut usize,
        track_nested_nodes: bool,
    ) {
        self.validate_envelope_and_policies(path, issues);
        if track_nested_nodes {
            if self.spec.nodes.len() > *nested_node_budget {
                issues.push(ValidationIssue::error(
                    "excessive_nested_node_count",
                    format!("{path}.spec.nodes"),
                    format!("nested graph node count must be at most {MAX_NESTED_NODE_COUNT}"),
                ));
                *nested_node_budget = 0;
            } else {
                *nested_node_budget -= self.spec.nodes.len();
            }
            if !Graph::validate_nested_graph_structure(path, self, 0, nested_node_budget, issues) {
                return;
            }
        }
        match Graph::estimate_graph_work(self) {
            Some(total_work) if total_work > MAX_LOOP_WORKLOAD => {
                issues.push(ValidationIssue::error(
                    "excessive_graph_workload",
                    format!("{path}.spec.nodes"),
                    format!("estimated graph workload must be at most {MAX_LOOP_WORKLOAD}"),
                ));
            }
            None => {
                issues.push(ValidationIssue::error(
                    "graph_workload_overflow",
                    format!("{path}.spec.nodes"),
                    "graph workload arithmetic overflow",
                ));
            }
            Some(_) => {}
        }
        let ids = self.validate_nodes(path, issues, nested_node_budget);
        self.validate_edges(path, &ids, issues);
        self.validate_cycle(path, &ids, issues);
        self.validate_resources(path, issues);
    }

    fn validate_envelope_and_policies(&self, path: &str, issues: &mut Vec<ValidationIssue>) {
        if self.api_version != GRAPH_API_VERSION {
            issues.push(ValidationIssue::error(
                "unsupported_api_version",
                format!("{path}.apiVersion"),
                format!(
                    "expected {GRAPH_API_VERSION:?}, found {:?}",
                    self.api_version
                ),
            ));
        }
        if self.kind != GRAPH_KIND {
            issues.push(ValidationIssue::error(
                "invalid_kind",
                format!("{path}.kind"),
                format!("expected {GRAPH_KIND:?}, found {:?}", self.kind),
            ));
        }
        if !is_valid_identifier(&self.metadata.name) {
            issues.push(ValidationIssue::error(
                "invalid_graph_name",
                format!("{path}.metadata.name"),
                "use lowercase letters, digits, '-' or '_', starting with a letter",
            ));
        }
        if self.metadata.name.len() > MAX_NODE_ID_BYTES {
            issues.push(ValidationIssue::error(
                "excessive_graph_name_length",
                format!("{path}.metadata.name"),
                format!("name length must be at most {MAX_NODE_ID_BYTES}"),
            ));
        }
        if self.spec.goal.trim().is_empty() {
            issues.push(ValidationIssue::error(
                "empty_goal",
                format!("{path}.spec.goal"),
                "goal must not be empty",
            ));
        }
        if self.spec.nodes.is_empty() {
            issues.push(ValidationIssue::error(
                "empty_graph",
                format!("{path}.spec.nodes"),
                "graph must contain at least one node",
            ));
        }
        if self.spec.policies.max_parallel == 0 {
            issues.push(ValidationIssue::error(
                "invalid_parallelism",
                format!("{path}.spec.policies.max_parallel"),
                "max_parallel must be at least 1",
            ));
        }
        if self.spec.policies.max_parallel > MAX_PARALLELISM {
            issues.push(ValidationIssue::error(
                "excessive_parallelism",
                format!("{path}.spec.policies.max_parallel"),
                format!("max_parallel must be at most {MAX_PARALLELISM}"),
            ));
        }
        if self
            .spec
            .budgets
            .wall_time_seconds
            .is_some_and(|wall_time_seconds| wall_time_seconds > MAX_DURATION_SECONDS)
        {
            issues.push(ValidationIssue::error(
                "wall_time_exceeds_limit",
                format!("{path}.spec.budgets.wall_time_seconds"),
                format!("wall_time_seconds must be between 0 and {MAX_DURATION_SECONDS} inclusive"),
            ));
        }
    }

    fn validate_nodes(
        &self,
        path: &str,
        issues: &mut Vec<ValidationIssue>,
        nested_node_budget: &mut usize,
    ) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        for (node_index, node) in self.spec.nodes.iter().enumerate() {
            let node_path = format!("{path}.spec.nodes[{node_index}]");
            if !is_valid_identifier(&node.id) {
                issues.push(ValidationIssue::error(
                    "invalid_node_id",
                    format!("{node_path}.id"),
                    "use lowercase letters, digits, '-' or '_', starting with a letter",
                ));
            }
            if node.id.len() > MAX_NODE_ID_BYTES {
                issues.push(ValidationIssue::error(
                    "excessive_node_id_length",
                    format!("{node_path}.id"),
                    format!("node id must be at most {MAX_NODE_ID_BYTES} bytes"),
                ));
            }
            if !ids.insert(node.id.clone()) {
                issues.push(ValidationIssue::error(
                    "duplicate_node_id",
                    format!("{node_path}.id"),
                    format!("node {:?} is declared more than once", node.id),
                ));
            }
            Graph::validate_node(
                node_path.as_str(),
                node,
                self.spec.policies.max_parallel,
                issues,
                nested_node_budget,
            );
            self.validate_workspace(node_path.as_str(), node, issues);
        }
        ids
    }

    #[allow(clippy::too_many_lines)]
    fn validate_node(
        node_path: &str,
        node: &Node,
        max_parallel: usize,
        issues: &mut Vec<ValidationIssue>,
        nested_node_budget: &mut usize,
    ) {
        if node.context.max_bytes > MAX_CONTEXT_BYTES {
            issues.push(ValidationIssue::error(
                "context_bytes_exceeds_limit",
                format!("{node_path}.context.max_bytes"),
                format!("context.max_bytes must be between 0 and {MAX_CONTEXT_BYTES} inclusive"),
            ));
        }
        if node.retry.max_attempts == 0 {
            issues.push(ValidationIssue::error(
                "invalid_retry_limit",
                format!("{node_path}.retry.max_attempts"),
                "max_attempts must be at least 1",
            ));
        }
        if node.retry.max_attempts > MAX_RETRY_ATTEMPTS {
            issues.push(ValidationIssue::error(
                "excessive_retry_attempts",
                format!("{node_path}.retry.max_attempts"),
                format!("max_attempts must be at most {MAX_RETRY_ATTEMPTS}"),
            ));
        }
        if node.retry.backoff_seconds > MAX_DURATION_SECONDS {
            issues.push(ValidationIssue::error(
                "retry_backoff_exceeds_limit",
                format!("{node_path}.retry.backoff_seconds"),
                format!(
                    "retry.backoff_seconds must be between 0 and {MAX_DURATION_SECONDS} inclusive"
                ),
            ));
        }
        for (index, profile) in node.retry.rebind_profiles.iter().enumerate() {
            if profile.trim().is_empty() {
                issues.push(ValidationIssue::error(
                    "empty_retry_rebind_profile",
                    format!("{node_path}.retry.rebind_profiles[{index}]"),
                    "retry rebind profile names must not be empty",
                ));
            }
            if profile.len() > MAX_PROFILE_REFERENCE_BYTES {
                issues.push(ValidationIssue::error(
                    "retry_rebind_profile_too_long",
                    format!("{node_path}.retry.rebind_profiles[{index}]"),
                    format!(
                        "retry rebind profile names must not exceed {MAX_PROFILE_REFERENCE_BYTES} bytes"
                    ),
                ));
            }
        }
        let available_rebind_attempts = node.retry.max_attempts.saturating_sub(1) as usize;
        if node.retry.rebind_profiles.len() > available_rebind_attempts {
            issues.push(ValidationIssue::error(
                "excessive_retry_rebind_profiles",
                format!("{node_path}.retry.rebind_profiles"),
                "retry rebind profiles must not outnumber attempts after the first",
            ));
        }
        if !node.retry.rebind_profiles.is_empty()
            && !matches!(
                &node.kind,
                NodeKind::Agent { .. } | NodeKind::Reduce { .. } | NodeKind::Synthesize { .. }
            )
        {
            issues.push(ValidationIssue::error(
                "unsupported_retry_rebind_profiles",
                format!("{node_path}.retry.rebind_profiles"),
                "retry rebind profiles are only supported by agent, reduce, and synthesize nodes",
            ));
        }
        if node
            .timeout_seconds
            .is_some_and(|timeout_seconds| timeout_seconds > MAX_DURATION_SECONDS)
        {
            issues.push(ValidationIssue::error(
                "node_timeout_exceeds_limit",
                format!("{node_path}.timeout_seconds"),
                format!("timeout_seconds must be between 0 and {MAX_DURATION_SECONDS} inclusive"),
            ));
        }
        if node.fan_out() == 0 {
            issues.push(ValidationIssue::error(
                "invalid_fan_out",
                format!("{node_path}.fan_out"),
                "fan_out must be at least 1",
            ));
        }
        if node.fan_out() > MAX_FAN_OUT {
            issues.push(ValidationIssue::error(
                "excessive_fan_out",
                format!("{node_path}.fan_out"),
                format!("fan_out must be at most {MAX_FAN_OUT}"),
            ));
        }
        if max_parallel > 0 {
            let max_parallel_work = max_parallel.checked_mul(node.fan_out());
            if max_parallel_work.is_none() {
                issues.push(ValidationIssue::error(
                    "parallel_workload_overflow",
                    format!("{node_path}.fan_out"),
                    "fan_out * max_parallel overflows",
                ));
            } else if let Some(max_parallel_work) = max_parallel_work
                && max_parallel_work > MAX_PARALLEL_WORKLOAD
            {
                issues.push(ValidationIssue::error(
                    "excessive_parallel_work",
                    format!("{node_path}.fan_out"),
                    format!("fan_out * max_parallel must be at most {MAX_PARALLEL_WORKLOAD}",),
                ));
            }
        }
        match &node.kind {
            NodeKind::Agent { output, .. }
            | NodeKind::Reduce { output, .. }
            | NodeKind::Synthesize { output, .. }
            | NodeKind::Command { output, .. }
            | NodeKind::Verify { output, .. } => {
                if output.max_bytes > MAX_OUTPUT_BYTES {
                    issues.push(ValidationIssue::error(
                        "output_bytes_exceeds_limit",
                        format!("{node_path}.output.max_bytes"),
                        format!(
                            "output.max_bytes must be between 0 and {MAX_OUTPUT_BYTES} inclusive"
                        ),
                    ));
                }
            }
            NodeKind::Gate { .. } | NodeKind::Loop { .. } | NodeKind::Subgraph { .. } => {}
        }
        match &node.kind {
            NodeKind::Agent {
                prompt,
                profile,
                model,
                ..
            }
            | NodeKind::Reduce {
                prompt,
                profile,
                model,
                ..
            }
            | NodeKind::Synthesize {
                prompt,
                profile,
                model,
                ..
            } => {
                if matches!(node.workspace, WorkspaceSpec::Worktree { .. }) && node.fan_out() != 1 {
                    issues.push(ValidationIssue::error(
                        "agent_worktree_requires_fan_out_one",
                        format!("{node_path}.fan_out"),
                        "agent workspace worktree must have fan_out = 1",
                    ));
                }
                if prompt.is_empty() {
                    issues.push(ValidationIssue::error(
                        "empty_prompt",
                        format!("{node_path}.prompt"),
                        "agent prompt must not be empty",
                    ));
                }
                if model.as_ref().is_some_and(|value| value.trim().is_empty()) {
                    issues.push(ValidationIssue::error(
                        "empty_model",
                        format!("{node_path}.model"),
                        "model must not be empty",
                    ));
                }
                if model
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_MODEL_ID_BYTES)
                {
                    issues.push(ValidationIssue::error(
                        "model_id_too_long",
                        format!("{node_path}.model"),
                        format!("model must not exceed {MAX_MODEL_ID_BYTES} bytes"),
                    ));
                }
                if profile
                    .as_ref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    issues.push(ValidationIssue::error(
                        "empty_profile",
                        format!("{node_path}.profile"),
                        "profile must not be empty",
                    ));
                }
                if profile
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_PROFILE_REFERENCE_BYTES)
                {
                    issues.push(ValidationIssue::error(
                        "profile_reference_too_long",
                        format!("{node_path}.profile"),
                        format!("profile must not exceed {MAX_PROFILE_REFERENCE_BYTES} bytes"),
                    ));
                }
            }
            NodeKind::Command { argv, .. } | NodeKind::Verify { argv, .. } => {
                Graph::validate_command_argv(node_path, argv, issues);
            }
            NodeKind::Gate { message, .. } => {
                if message.trim().is_empty() {
                    issues.push(ValidationIssue::error(
                        "empty_gate_message",
                        format!("{node_path}.message"),
                        "gate message must not be empty",
                    ));
                }
            }
            NodeKind::Loop {
                graph,
                until,
                max_iterations,
                stagnation_after,
                ..
            } => {
                Graph::validate_loop(
                    graph,
                    ValidateLoopParams {
                        node_path,
                        until,
                        max_iterations: *max_iterations,
                        stagnation_after: *stagnation_after,
                        node_retry_attempts: node.retry.max_attempts,
                        issues,
                        nested_node_budget,
                    },
                );
            }
            NodeKind::Subgraph { graph } => {
                graph.validate_at(
                    &format!("{node_path}.graph"),
                    issues,
                    nested_node_budget,
                    false,
                );
            }
        }
    }

    fn validate_loop(graph: &Graph, params: ValidateLoopParams<'_>) {
        let ValidateLoopParams {
            node_path,
            until,
            max_iterations,
            stagnation_after,
            node_retry_attempts,
            issues,
            nested_node_budget,
        } = params;
        if max_iterations == 0 {
            issues.push(ValidationIssue::error(
                "unbounded_loop",
                format!("{node_path}.max_iterations"),
                "a loop must have max_iterations >= 1",
            ));
        }
        if max_iterations > MAX_LOOP_ITERATIONS {
            issues.push(ValidationIssue::error(
                "excessive_loop_iterations",
                format!("{node_path}.max_iterations"),
                format!("max_iterations must be at most {MAX_LOOP_ITERATIONS}"),
            ));
        }
        if stagnation_after == 0 {
            issues.push(ValidationIssue::error(
                "invalid_stagnation_limit",
                format!("{node_path}.stagnation_after"),
                "stagnation_after must be at least 1",
            ));
        }
        match Graph::estimate_loop_workload(graph, max_iterations as usize, node_retry_attempts) {
            Some(loop_work) if loop_work > MAX_LOOP_WORKLOAD => {
                issues.push(ValidationIssue::error(
                    "excessive_loop_workload",
                    format!("{node_path}.max_iterations"),
                    format!("estimated loop workload must be at most {MAX_LOOP_WORKLOAD}"),
                ));
            }
            None => {
                issues.push(ValidationIssue::error(
                    "loop_workload_overflow",
                    format!("{node_path}.max_iterations"),
                    "loop workload arithmetic overflow",
                ));
            }
            Some(_) => {}
        }
        graph.validate_at(
            &format!("{node_path}.graph"),
            issues,
            nested_node_budget,
            false,
        );
        if !graph
            .spec
            .nodes
            .iter()
            .any(|nested| nested.id == until.node)
        {
            issues.push(ValidationIssue::error(
                "unknown_loop_condition_node",
                format!("{node_path}.until.node"),
                format!("nested node {:?} does not exist", until.node),
            ));
        }
        if !matches!(until.status, NodeStatus::Succeeded | NodeStatus::Skipped) {
            issues.push(ValidationIssue::error(
                "invalid_loop_condition_status",
                format!("{node_path}.until.status"),
                "loop completion status must be succeeded or skipped; nested failures propagate",
            ));
        }
        if until.json_pointer.is_some() != until.equals.is_some() {
            issues.push(ValidationIssue::error(
                "incomplete_loop_json_condition",
                format!("{node_path}.until"),
                "json_pointer and equals must be specified together",
            ));
        }
    }

    fn validate_workspace(&self, node_path: &str, node: &Node, issues: &mut Vec<ValidationIssue>) {
        match &node.workspace {
            WorkspaceSpec::Inherit { node: source } => {
                if source == &node.id {
                    issues.push(ValidationIssue::error(
                        "self_workspace_inheritance",
                        format!("{node_path}.workspace.node"),
                        "a node cannot inherit its own workspace",
                    ));
                }
                if !self
                    .spec
                    .nodes
                    .iter()
                    .any(|candidate| &candidate.id == source)
                {
                    issues.push(ValidationIssue::error(
                        "unknown_workspace_source",
                        format!("{node_path}.workspace.node"),
                        format!("node {source:?} does not exist"),
                    ));
                    return;
                }

                let inheritance_edges: Vec<&Edge> = self
                    .spec
                    .edges
                    .iter()
                    .filter(|edge| edge.from == *source && edge.to == node.id)
                    .collect();

                if inheritance_edges.is_empty() {
                    issues.push(ValidationIssue::error(
                        "unordered_workspace_inheritance",
                        format!("{node_path}.workspace.node"),
                        format!("node {source:?} must have a direct edge to {:?}", node.id),
                    ));
                    return;
                }

                if inheritance_edges
                    .iter()
                    .any(|edge| edge.kind == EdgeKind::Failure)
                {
                    issues.push(ValidationIssue::error(
                        "workspace_inheritance_requires_success_edge",
                        format!("{node_path}.workspace.node"),
                        format!(
                            "inherited workspace cannot use failure edges from {:?} to {:?}",
                            source, node.id
                        ),
                    ));
                }
                for edge in &inheritance_edges {
                    if let Some(condition) = edge.when.as_ref()
                        && condition.status != Some(NodeStatus::Succeeded)
                    {
                        issues.push(ValidationIssue::error(
                            "workspace_inheritance_requires_success_edge",
                            format!("{node_path}.workspace.node"),
                            format!(
                                "inherited workspace requires edge status to be succeeded from {:?} to {:?}",
                                source, node.id
                            ),
                        ));
                    }
                }
            }
            WorkspaceSpec::Worktree { base, .. } => {
                if base.as_ref().is_some_and(|value| value.trim().is_empty()) {
                    issues.push(ValidationIssue::error(
                        "empty_worktree_base",
                        format!("{node_path}.workspace.base"),
                        "worktree base must not be empty",
                    ));
                }
            }
            _ => {}
        }
    }

    fn estimate_loop_workload(
        graph: &Graph,
        max_iterations: usize,
        node_retry_attempts: u32,
    ) -> Option<usize> {
        let estimated_iteration_work = Graph::estimate_graph_work(graph)?;
        let iteration_work = max_iterations.checked_mul(estimated_iteration_work)?;
        iteration_work.checked_mul(node_retry_attempts as usize)
    }

    fn estimate_graph_work(graph: &Graph) -> Option<usize> {
        let mut total = 0usize;
        for node in &graph.spec.nodes {
            total = total.checked_add(Graph::estimate_node_work(node)?)?;
        }
        Some(total)
    }

    fn estimate_node_work(node: &Node) -> Option<usize> {
        match &node.kind {
            NodeKind::Loop {
                graph,
                max_iterations,
                ..
            } => Graph::estimate_loop_workload(
                graph,
                *max_iterations as usize,
                node.retry.max_attempts,
            ),
            NodeKind::Subgraph { graph } => {
                Graph::estimate_graph_work(graph)?.checked_mul(node.retry.max_attempts as usize)
            }
            _ => node.fan_out().checked_mul(node.retry.max_attempts as usize),
        }
    }

    fn validate_nested_graph_structure(
        path: &str,
        graph: &Graph,
        initial_depth: usize,
        nested_node_budget: &mut usize,
        issues: &mut Vec<ValidationIssue>,
    ) -> bool {
        let mut stack = Vec::new();
        for (node_index, node) in graph.spec.nodes.iter().enumerate() {
            if let NodeKind::Subgraph { graph } | NodeKind::Loop { graph, .. } = &node.kind {
                stack.push((
                    format!("{path}.spec.nodes[{node_index}].graph"),
                    graph,
                    initial_depth + 1,
                ));
            }
        }
        let mut counted_nodes = 0usize;
        while let Some((nested_path, nested_graph, depth)) = stack.pop() {
            if depth > MAX_GRAPH_NESTING_DEPTH {
                issues.push(ValidationIssue::error(
                    "excessive_nesting_depth",
                    nested_path,
                    format!("graph nesting depth must be at most {MAX_GRAPH_NESTING_DEPTH}"),
                ));
                return false;
            }
            if let Some(total) = counted_nodes.checked_add(nested_graph.spec.nodes.len()) {
                counted_nodes = total;
            } else {
                issues.push(ValidationIssue::error(
                    "nested_node_count_overflow",
                    nested_path,
                    "nested node count overflows",
                ));
                return false;
            }
            if counted_nodes > *nested_node_budget {
                issues.push(ValidationIssue::error(
                    "excessive_nested_node_count",
                    nested_path,
                    format!("nested graph node count must be at most {MAX_NESTED_NODE_COUNT}"),
                ));
                return false;
            }
            for (node_index, node) in nested_graph.spec.nodes.iter().enumerate() {
                match &node.kind {
                    NodeKind::Subgraph { graph } | NodeKind::Loop { graph, .. } => {
                        stack.push((
                            format!("{nested_path}.spec.nodes[{node_index}].graph"),
                            graph,
                            depth + 1,
                        ));
                    }
                    NodeKind::Agent { .. }
                    | NodeKind::Command { .. }
                    | NodeKind::Reduce { .. }
                    | NodeKind::Verify { .. }
                    | NodeKind::Synthesize { .. }
                    | NodeKind::Gate { .. } => {}
                }
            }
        }
        if let Some(remaining) = nested_node_budget.checked_sub(counted_nodes) {
            *nested_node_budget = remaining;
        } else {
            issues.push(ValidationIssue::error(
                "nested_node_count_overflow",
                path.to_owned(),
                "nested node budget underflows",
            ));
            return false;
        }
        true
    }

    fn validate_edges(
        &self,
        path: &str,
        ids: &BTreeSet<String>,
        issues: &mut Vec<ValidationIssue>,
    ) {
        for (edge_index, edge) in self.spec.edges.iter().enumerate() {
            let edge_path = format!("{path}.spec.edges[{edge_index}]");
            if edge.from.len() > MAX_EDGE_ID_BYTES {
                issues.push(ValidationIssue::error(
                    "excessive_edge_endpoint_length",
                    format!("{edge_path}.from"),
                    format!("edge endpoint must be at most {MAX_EDGE_ID_BYTES} bytes"),
                ));
            }
            if edge.to.len() > MAX_EDGE_ID_BYTES {
                issues.push(ValidationIssue::error(
                    "excessive_edge_endpoint_length",
                    format!("{edge_path}.to"),
                    format!("edge endpoint must be at most {MAX_EDGE_ID_BYTES} bytes"),
                ));
            }
            if !ids.contains(&edge.from) {
                issues.push(ValidationIssue::error(
                    "unknown_edge_source",
                    format!("{edge_path}.from"),
                    format!("node {:?} does not exist", edge.from),
                ));
            }
            if !ids.contains(&edge.to) {
                issues.push(ValidationIssue::error(
                    "unknown_edge_destination",
                    format!("{edge_path}.to"),
                    format!("node {:?} does not exist", edge.to),
                ));
            }
            if edge.from == edge.to {
                issues.push(ValidationIssue::error(
                    "self_edge",
                    edge_path.clone(),
                    "outer graphs are DAGs; use a bounded loop node instead",
                ));
            }
            if edge.kind == EdgeKind::Conditional && edge.when.is_none() {
                issues.push(ValidationIssue::error(
                    "missing_edge_condition",
                    format!("{edge_path}.when"),
                    "conditional edges require a when condition",
                ));
            }
            if let Some(condition) = &edge.when
                && condition.json_pointer.is_some() != condition.equals.is_some()
            {
                issues.push(ValidationIssue::error(
                    "incomplete_edge_json_condition",
                    format!("{edge_path}.when"),
                    "json_pointer and equals must be specified together",
                ));
            }
            if let Some(condition) = &edge.when
                && !condition.status.is_none_or(NodeStatus::is_terminal)
            {
                issues.push(ValidationIssue::error(
                    "invalid_edge_condition_status",
                    format!("{edge_path}.when.status"),
                    "edge condition status must be a terminal state",
                ));
            }
            if edge.kind == EdgeKind::Failure
                && edge
                    .when
                    .as_ref()
                    .is_some_and(|condition| condition.status == Some(NodeStatus::Succeeded))
            {
                issues.push(ValidationIssue::error(
                    "failure_edge_must_match_failed_status",
                    format!("{edge_path}.when.status"),
                    "failure edges must not use succeeded status; omit status or use failed",
                ));
            }
        }

        let mut seen_edges = BTreeSet::new();
        for (edge_index, edge) in self.spec.edges.iter().enumerate() {
            let key = (edge.from.as_str(), edge.to.as_str(), edge.kind);
            if !seen_edges.insert(key) {
                issues.push(ValidationIssue::warning(
                    "duplicate_edge",
                    format!("{path}.spec.edges[{edge_index}]"),
                    format!(
                        "duplicate {:?} edge from {:?} to {:?}",
                        edge.kind, edge.from, edge.to
                    ),
                ));
            }
        }
    }

    fn validate_cycle(
        &self,
        path: &str,
        ids: &BTreeSet<String>,
        issues: &mut Vec<ValidationIssue>,
    ) {
        if ids.len() == self.spec.nodes.len()
            && self.spec.edges.iter().all(|edge| {
                ids.contains(&edge.from) && ids.contains(&edge.to) && edge.from != edge.to
            })
            && stable_topological_order(&self.spec.nodes, &self.spec.edges).is_none()
        {
            issues.push(ValidationIssue::error(
                "cycle_detected",
                format!("{path}.spec.edges"),
                "outer graphs must be acyclic; express cycles as bounded loop nodes",
            ));
        }
    }

    fn validate_resources(&self, path: &str, issues: &mut Vec<ValidationIssue>) {
        let mut resource_writers: HashMap<&str, Vec<&str>> = HashMap::new();
        for (node_index, node) in self.spec.nodes.iter().enumerate() {
            let node_path = format!("{path}.spec.nodes[{node_index}]");
            for resource in &node.resources {
                if resource.len() > MAX_RESOURCE_ID_BYTES {
                    issues.push(ValidationIssue::error(
                        "excessive_resource_id_length",
                        format!("{node_path}.resources"),
                        format!("resource id must be at most {MAX_RESOURCE_ID_BYTES} bytes"),
                    ));
                }
                resource_writers
                    .entry(resource.as_str())
                    .or_default()
                    .push(node.id.as_str());
            }
        }
        for (resource, writers) in resource_writers {
            if writers.len() > 1 {
                issues.push(ValidationIssue::warning(
                    "shared_resource",
                    format!("{path}.spec.nodes"),
                    format!(
                        "resource {resource:?} is claimed by {}; runtime will serialize them",
                        writers.join(", ")
                    ),
                ));
            }
        }
    }

    fn validate_command_argv(node_path: &str, argv: &[String], issues: &mut Vec<ValidationIssue>) {
        if argv.is_empty() || argv[0].trim().is_empty() {
            issues.push(ValidationIssue::error(
                "empty_command",
                format!("{node_path}.argv"),
                "argv must contain an executable",
            ));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GraphMetadata {
    pub name: String,
    #[serde(default = "default_graph_version")]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub labels: IndexMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GraphSpec {
    pub goal: String,
    #[serde(default)]
    pub policies: GraphPolicies,
    #[serde(default)]
    pub budgets: RunBudgets,
    pub nodes: Vec<Node>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<Edge>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GraphPolicies {
    pub max_parallel: usize,
    pub failure: FailurePolicy,
}

impl Default for GraphPolicies {
    fn default() -> Self {
        Self {
            max_parallel: 4,
            failure: FailurePolicy::FailFast,
        }
    }
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    #[default]
    FailFast,
    Continue,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RunBudgets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_time_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_calls: Option<u32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct Node {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub retry: RetryPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub workspace: WorkspaceSpec,
    #[serde(default)]
    pub context: ContextSpec,
    #[serde(default)]
    pub continue_on_failure: bool,
    #[serde(flatten)]
    pub kind: NodeKind,
}

const NODE_COMMON_FIELDS: [&str; 10] = [
    "id",
    "label",
    "requires",
    "resources",
    "retry",
    "timeout_seconds",
    "workspace",
    "context",
    "continue_on_failure",
    "kind",
];

const NODE_KIND_AGENT_FIELDS: [&str; 5] = ["prompt", "profile", "model", "fan_out", "output"];
const NODE_KIND_COMMAND_FIELDS: [&str; 3] = ["argv", "env", "output"];
const NODE_KIND_REDUCE_FIELDS: [&str; 4] = ["prompt", "profile", "model", "output"];
const NODE_KIND_VERIFY_FIELDS: [&str; 3] = ["argv", "env", "output"];
const NODE_KIND_SYNTHESIZE_FIELDS: [&str; 4] = ["prompt", "profile", "model", "output"];
const NODE_KIND_GATE_FIELDS: [&str; 2] = ["message", "default"];
const NODE_KIND_LOOP_FIELDS: [&str; 4] = ["graph", "until", "max_iterations", "stagnation_after"];
const NODE_KIND_SUBGRAPH_FIELDS: [&str; 1] = ["graph"];

#[derive(Deserialize)]
struct NodeCommon {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    resources: Vec<String>,
    #[serde(default)]
    retry: RetryPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    workspace: WorkspaceSpec,
    #[serde(default)]
    context: ContextSpec,
    #[serde(default)]
    continue_on_failure: bool,
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Value::Object(map) = Value::deserialize(deserializer)? else {
            return Err(DeError::invalid_type(
                serde::de::Unexpected::Other("non-object"),
                &"a map",
            ));
        };

        let mut kind_name = None;
        let mut kind_map: serde_json::Map<String, Value> = serde_json::Map::new();

        let Some(kind_value) = map.get("kind") else {
            return Err(DeError::missing_field("kind"));
        };
        match kind_value {
            Value::String(value) => {
                kind_name = Some(value.as_str());
            }
            Value::Object(object) if object.len() == 1 => {
                if let Some((candidate_kind, candidate_fields)) = object.iter().next() {
                    let candidate_kind = candidate_kind.as_str();
                    kind_name = Some(candidate_kind);
                    kind_map = candidate_fields
                        .as_object()
                        .map(|fields| {
                            fields
                                .iter()
                                .map(|(field, value)| (field.to_owned(), value.to_owned()))
                                .collect()
                        })
                        .unwrap_or_default();
                }
            }
            _ => {
                return Err(DeError::custom("`kind` field must be a string"));
            }
        }
        let kind_name = kind_name.unwrap_or_default();
        let kind_fields: &'static [&'static str] = match kind_name {
            "agent" => &NODE_KIND_AGENT_FIELDS,
            "command" => &NODE_KIND_COMMAND_FIELDS,
            "reduce" => &NODE_KIND_REDUCE_FIELDS,
            "verify" => &NODE_KIND_VERIFY_FIELDS,
            "synthesize" => &NODE_KIND_SYNTHESIZE_FIELDS,
            "gate" => &NODE_KIND_GATE_FIELDS,
            "loop" => &NODE_KIND_LOOP_FIELDS,
            "subgraph" => &NODE_KIND_SUBGRAPH_FIELDS,
            _ => &[],
        };

        let known_fields: std::collections::HashSet<&str> = NODE_COMMON_FIELDS
            .iter()
            .copied()
            .chain(kind_fields.iter().copied())
            .collect();

        let unknown_field = map
            .keys()
            .find(|field| !known_fields.contains(field.as_str()));

        if let Some(unknown_field) = unknown_field {
            return Err(DeError::custom(format!("unknown field `{unknown_field}`",)));
        }

        if kind_map.is_empty() {
            kind_map = map
                .iter()
                .filter(|(field, _)| !NODE_COMMON_FIELDS.contains(&field.as_str()))
                .map(|(field, value)| (field.to_owned(), value.to_owned()))
                .collect();
        }
        kind_map.insert("kind".to_owned(), Value::String(kind_name.to_owned()));
        let kind =
            serde_json::from_value(Value::Object(kind_map)).map_err(serde::de::Error::custom)?;

        let common_map: serde_json::Map<String, Value> = map
            .iter()
            .filter(|(field, _)| NODE_COMMON_FIELDS.contains(&field.as_str()))
            .map(|(field, value)| (field.to_owned(), value.to_owned()))
            .collect();
        let common: NodeCommon =
            serde_json::from_value(Value::Object(common_map)).map_err(serde::de::Error::custom)?;

        Ok(Self {
            id: common.id,
            label: common.label,
            requires: common.requires,
            resources: common.resources,
            retry: common.retry,
            timeout_seconds: common.timeout_seconds,
            workspace: common.workspace,
            context: common.context,
            continue_on_failure: common.continue_on_failure,
            kind,
        })
    }
}

impl Node {
    pub fn agent(id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: None,
            requires: Vec::new(),
            resources: Vec::new(),
            retry: RetryPolicy::default(),
            timeout_seconds: None,
            workspace: WorkspaceSpec::default(),
            context: ContextSpec::default(),
            continue_on_failure: false,
            kind: NodeKind::Agent {
                prompt: PromptSpec::Inline(prompt.into()),
                profile: None,
                model: None,
                fan_out: 1,
                output: OutputSpec::default(),
            },
        }
    }

    pub fn command(id: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            id: id.into(),
            label: None,
            requires: Vec::new(),
            resources: Vec::new(),
            retry: RetryPolicy::default(),
            timeout_seconds: None,
            workspace: WorkspaceSpec::default(),
            context: ContextSpec::default(),
            continue_on_failure: false,
            kind: NodeKind::Command {
                argv,
                env: IndexMap::new(),
                output: OutputSpec::default(),
            },
        }
    }

    pub fn profile(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::Agent { profile, .. }
            | NodeKind::Reduce { profile, .. }
            | NodeKind::Synthesize { profile, .. } => profile.as_deref(),
            _ => None,
        }
    }

    pub fn model(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::Agent { model, .. }
            | NodeKind::Reduce { model, .. }
            | NodeKind::Synthesize { model, .. } => model.as_deref(),
            _ => None,
        }
    }

    pub fn fan_out(&self) -> usize {
        match &self.kind {
            NodeKind::Agent { fan_out, .. } => *fan_out,
            _ => 1,
        }
    }

    pub fn output(&self) -> Option<&OutputSpec> {
        match &self.kind {
            NodeKind::Agent { output, .. }
            | NodeKind::Reduce { output, .. }
            | NodeKind::Synthesize { output, .. }
            | NodeKind::Command { output, .. }
            | NodeKind::Verify { output, .. } => Some(output),
            NodeKind::Gate { .. } | NodeKind::Loop { .. } | NodeKind::Subgraph { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeKind {
    Agent {
        prompt: PromptSpec,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default = "default_fan_out")]
        fan_out: usize,
        #[serde(default)]
        output: OutputSpec,
    },
    Command {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
        env: IndexMap<String, String>,
        #[serde(default)]
        output: OutputSpec,
    },
    Reduce {
        prompt: PromptSpec,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default)]
        output: OutputSpec,
    },
    Verify {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
        env: IndexMap<String, String>,
        #[serde(default)]
        output: OutputSpec,
    },
    Synthesize {
        prompt: PromptSpec,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default)]
        output: OutputSpec,
    },
    Gate {
        message: String,
        #[serde(default)]
        default: GateDefault,
    },
    Loop {
        graph: Box<Graph>,
        until: LoopCondition,
        max_iterations: u32,
        #[serde(default = "default_stagnation_after")]
        stagnation_after: u32,
    },
    Subgraph {
        graph: Box<Graph>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum PromptSpec {
    Inline(String),
    Package {
        file: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
        variables: IndexMap<String, String>,
    },
}

impl PromptSpec {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Inline(value) => value.trim().is_empty(),
            Self::Package { file, .. } => file.as_os_str().is_empty(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_seconds: u64,
    pub rebind_profiles: Vec<String>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff_seconds: 0,
            rebind_profiles: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceSpec {
    #[default]
    Current,
    Readonly,
    Worktree {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        #[serde(default)]
        auto_commit: bool,
    },
    Inherit {
        node: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ContextSpec {
    pub include_dependencies: bool,
    pub files: Vec<PathBuf>,
    pub max_bytes: usize,
}

impl Default for ContextSpec {
    fn default() -> Self {
        Self {
            include_dependencies: true,
            files: Vec::new(),
            max_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct OutputSpec {
    pub format: OutputFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_schema: Option<Value>,
    pub max_bytes: usize,
}

impl Default for OutputSpec {
    fn default() -> Self {
        Self {
            format: OutputFormat::Text,
            schema: None,
            inline_schema: None,
            max_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GateDefault {
    Approve,
    #[default]
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LoopCondition {
    pub node: String,
    #[serde(default = "default_success_status")]
    pub status: NodeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_pointer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub kind: EdgeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<EdgeCondition>,
}

impl Edge {
    pub fn data(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            kind: EdgeKind::Data,
            when: None,
        }
    }
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    #[default]
    Data,
    Control,
    Resource,
    Conditional,
    Failure,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EdgeCondition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<NodeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_pointer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    #[default]
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Blocked,
    Cancelled,
}

impl NodeStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Skipped | Self::Blocked | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone)]
pub struct CompiledGraph {
    pub graph: Graph,
    pub order: Vec<String>,
    pub index: IndexMap<String, usize>,
    pub incoming: IndexMap<String, Vec<usize>>,
    pub outgoing: IndexMap<String, Vec<usize>>,
}

impl CompiledGraph {
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.index
            .get(id)
            .and_then(|index| self.graph.spec.nodes.get(*index))
    }

    pub fn incoming_edges(&self, id: &str) -> impl Iterator<Item = &Edge> {
        self.incoming
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|index| self.graph.spec.edges.get(*index))
    }

    pub fn outgoing_edges(&self, id: &str) -> impl Iterator<Item = &Edge> {
        self.outgoing
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|index| self.graph.spec.edges.get(*index))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ValidationIssue {
    pub severity: IssueSeverity,
    pub code: String,
    pub path: String,
    pub message: String,
}

impl ValidationIssue {
    fn error(code: impl Into<String>, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: IssueSeverity::Error,
            code: code.into(),
            path: path.into(),
            message: message.into(),
        }
    }

    fn warning(
        code: impl Into<String>,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: IssueSeverity::Warning,
            code: code.into(),
            path: path.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Error,
    Warning,
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("failed to read graph {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid graph YAML: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error("graph source too large: {source_bytes} bytes, limit is {max_bytes}")]
    SourceTooLarge {
        source_bytes: usize,
        max_bytes: usize,
    },
    #[error("failed to serialize graph: {0}")]
    Json(#[from] serde_json::Error),
    #[error("graph validation failed")]
    Validation(Vec<ValidationIssue>),
}

fn stable_topological_order(nodes: &[Node], edges: &[Edge]) -> Option<Vec<String>> {
    let mut indegree: IndexMap<&str, usize> = nodes
        .iter()
        .map(|node| (node.id.as_str(), 0_usize))
        .collect();
    let mut outgoing: IndexMap<&str, Vec<&str>> = nodes
        .iter()
        .map(|node| (node.id.as_str(), Vec::new()))
        .collect();

    for edge in edges {
        *indegree.get_mut(edge.to.as_str())? += 1;
        outgoing.get_mut(edge.from.as_str())?.push(edge.to.as_str());
    }

    let mut ready: VecDeque<&str> = indegree
        .iter()
        .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
        .collect();
    let mut order = Vec::with_capacity(nodes.len());

    while let Some(id) = ready.pop_front() {
        order.push(id.to_owned());
        for destination in &outgoing[id] {
            let degree = indegree
                .get_mut(destination)
                .expect("destination was validated");
            *degree -= 1;
            if *degree == 0 {
                ready.push_back(destination);
            }
        }
    }

    (order.len() == nodes.len()).then_some(order)
}

fn is_valid_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'
                || character == '_'
        })
}

fn default_graph_version() -> String {
    "1.0.0".to_owned()
}

const fn default_fan_out() -> usize {
    1
}

const fn default_stagnation_after() -> u32 {
    2
}

const fn default_success_status() -> NodeStatus {
    NodeStatus::Succeeded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_with_edges(edges: Vec<Edge>) -> Graph {
        let mut graph = Graph::new(
            "sample",
            "test graph",
            vec![
                Node::command("first", vec!["true".to_owned()]),
                Node::command("second", vec!["true".to_owned()]),
            ],
        );
        graph.spec.edges = edges;
        graph
    }

    #[test]
    fn compiles_in_stable_order() {
        let graph = graph_with_edges(vec![Edge::data("first", "second")]);
        let compiled = graph.compile().expect("graph compiles");
        assert_eq!(compiled.order, ["first", "second"]);
    }

    #[test]
    fn rejects_cycles() {
        let graph = graph_with_edges(vec![
            Edge::data("first", "second"),
            Edge::data("second", "first"),
        ]);
        let issues = graph.validate();
        assert!(issues.iter().any(|issue| issue.code == "cycle_detected"));
    }

    #[test]
    fn round_trips_yaml() {
        let graph = graph_with_edges(vec![Edge::data("first", "second")]);
        let yaml = graph.to_yaml().expect("serialize graph");
        let parsed = Graph::from_yaml_str(&yaml).expect("parse graph");
        assert_eq!(parsed, graph);
    }

    fn agent_with_model(id: impl Into<String>, model: Option<String>) -> Node {
        let mut node = Node::agent(id, "prompt");
        if let NodeKind::Agent {
            model: node_model, ..
        } = &mut node.kind
        {
            *node_model = model;
        }
        node
    }

    #[test]
    fn rejects_empty_model() {
        for value in [String::new(), "   ".to_owned()] {
            let graph = Graph::new(
                "sample",
                "test graph",
                vec![agent_with_model("agent", Some(value))],
            );
            let issues = graph.validate();
            assert!(issues.iter().any(|issue| issue.code == "empty_model"));
        }
    }

    #[test]
    fn rejects_empty_profile() {
        let mut node = Node::agent("agent", "prompt");
        if let NodeKind::Agent { profile, .. } = &mut node.kind {
            *profile = Some("   ".to_owned());
        }
        let issues = Graph::new("sample", "test graph", vec![node]).validate();
        assert!(issues.iter().any(|issue| issue.code == "empty_profile"));
    }

    #[test]
    fn rejects_oversized_profile_model_and_rebind_references() {
        let mut node = agent_with_model(
            "agent",
            Some("m".repeat(MAX_MODEL_ID_BYTES.saturating_add(1))),
        );
        if let NodeKind::Agent { profile, .. } = &mut node.kind {
            *profile = Some("p".repeat(MAX_PROFILE_REFERENCE_BYTES.saturating_add(1)));
        }
        node.retry.max_attempts = 2;
        node.retry.rebind_profiles = vec!["r".repeat(MAX_PROFILE_REFERENCE_BYTES + 1)];

        let issues = Graph::new("sample", "test graph", vec![node]).validate();
        for code in [
            "model_id_too_long",
            "profile_reference_too_long",
            "retry_rebind_profile_too_long",
        ] {
            assert!(issues.iter().any(|issue| issue.code == code), "{code}");
        }
    }

    #[test]
    fn graph_schema_includes_node_model() {
        let schema = schemars::schema_for!(Graph);
        let schema_str = serde_json::to_string(&schema).expect("serialize schema");
        assert!(schema_str.contains(r#""model""#));
    }

    #[test]
    fn round_trips_yaml_with_model() {
        let graph = Graph::new(
            "sample",
            "test graph",
            vec![agent_with_model("agent", Some("test-model".into()))],
        );
        let yaml = graph.to_yaml().expect("serialize graph");
        let parsed = Graph::from_yaml_str(&yaml).expect("parse graph");
        assert_eq!(parsed, graph);
    }

    #[test]
    fn rejects_unknown_node_fields() {
        let yaml = concat!(
            "apiVersion: gloop.dev/v1alpha1\n",
            "kind: Graph\n",
            "metadata:\n",
            "  name: sample\n",
            "spec:\n",
            "  goal: test\n",
            "  nodes:\n",
            "  - id: agent\n",
            "    kind: agent\n",
            "    prompt: test\n",
            "    securty: true\n",
            "  - id: second\n",
            "    kind: command\n",
            "    argv: [\"true\"]\n",
            "    workspce: current\n",
            "    budget: 10\n",
        );
        let error = Graph::from_yaml_str(yaml).unwrap_err();
        let text = format!("{error}");
        assert!(
            text.contains("unknown field `securty`")
                || text.contains("unknown field `workspce`")
                || text.contains("unknown field `budget`")
        );
    }

    #[test]
    fn node_deserialize_allows_embedded_kind_object() {
        let yaml = concat!(
            "apiVersion: gloop.dev/v1alpha1\n",
            "kind: Graph\n",
            "metadata:\n",
            "  name: legacy-kind\n",
            "spec:\n",
            "  goal: test\n",
            "  nodes:\n",
            "  - id: agent\n",
            "    kind:\n",
            "      agent:\n",
            "        prompt: legacy format\n",
        );
        let graph = Graph::from_yaml_str(yaml).expect("parse graph");
        assert_eq!(graph.spec.nodes.len(), 1);
        assert!(matches!(graph.spec.nodes[0].kind, NodeKind::Agent { .. }));
    }

    #[test]
    fn rejects_invalid_kind_shape() {
        let yaml = concat!(
            "apiVersion: gloop.dev/v1alpha1\n",
            "kind: Graph\n",
            "metadata:\n",
            "  name: bad-kind\n",
            "spec:\n",
            "  goal: test\n",
            "  nodes:\n",
            "  - id: agent\n",
            "    kind:\n",
            "      - invalid\n",
        );
        let error = Graph::from_yaml_str(yaml).unwrap_err();
        assert!(
            format!("{error}").contains("`kind` field must be a string")
                || format!("{error}").contains("invalid type")
        );
    }

    #[test]
    fn rejects_nonterminal_edge_condition_status() {
        let graph = graph_with_edges(vec![Edge {
            from: "first".to_owned(),
            to: "second".to_owned(),
            kind: EdgeKind::Conditional,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Pending),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        }]);
        let issues = graph.validate();
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "invalid_edge_condition_status")
        );
    }

    #[test]
    fn rejects_failure_edge_with_success_status_condition() {
        let graph = graph_with_edges(vec![Edge {
            from: "first".to_owned(),
            to: "second".to_owned(),
            kind: EdgeKind::Failure,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Succeeded),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        }]);
        let issues = graph.validate();
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "failure_edge_must_match_failed_status")
        );
    }

    #[test]
    fn rejects_huge_node_and_edge_ids() {
        let node_id = "a".repeat(MAX_NODE_ID_BYTES + 1);
        let graph = Graph::new(
            "sample",
            "test graph",
            vec![
                Node::command(node_id.clone(), vec!["true".to_owned()]),
                Node::command("second".to_owned(), vec!["true".to_owned()]),
            ],
        );
        let graph = {
            let mut graph = graph;
            graph.spec.edges = vec![Edge {
                from: node_id,
                to: "second".to_owned(),
                kind: EdgeKind::Data,
                when: None,
            }];
            graph
        };
        let issues = graph.validate();
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "excessive_node_id_length")
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "excessive_edge_endpoint_length")
        );
    }

    #[test]
    fn rejects_nonterminal_loop_condition_status() {
        let inner = Graph::new("inner", "inner", vec![Node::agent("inner", "prompt")]);
        let mut node = Node::agent("loop", "prompt");
        node.kind = NodeKind::Loop {
            graph: Box::new(inner),
            until: LoopCondition {
                node: "inner".to_owned(),
                status: NodeStatus::Pending,
                output_contains: None,
                json_pointer: None,
                equals: None,
            },
            max_iterations: 2,
            stagnation_after: 1,
        };
        let graph = Graph::new("sample", "test graph", vec![node]);
        let issues = graph.validate();
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "invalid_loop_condition_status")
        );
    }

    #[test]
    fn rejects_huge_resource_ids() {
        let mut node = Node::command("first", vec!["true".to_owned()]);
        node.resources
            .push("resource-".repeat(MAX_RESOURCE_ID_BYTES + 1));
        let graph = Graph::new("sample", "test graph", vec![node]);
        let issues = graph.validate();
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "excessive_resource_id_length")
        );
    }
}
