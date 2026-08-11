use std::collections::HashSet;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use gloop_core::{
    ContextSpec, Edge, EdgeCondition, EdgeKind, Graph, GraphError, GraphMetadata, GraphPolicies,
    GraphSpec, IssueSeverity, LoopCondition, Node, NodeKind, NodeStatus, OutputSpec, PromptSpec,
    RetryPolicy, RunBudgets, WorkspaceSpec,
    graph::MAX_GRAPH_SOURCE_BYTES,
    graph::{GRAPH_API_VERSION, GRAPH_KIND},
};
use indexmap::IndexMap;
use schemars::schema_for;
use serde_json::Value;

fn root_examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .unwrap()
        .join("examples")
}

fn validate_has_error(graph: &Graph, expected_code: &str) {
    let issues = graph.validate();
    let issue_codes: HashSet<&str> = issues
        .iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .map(|issue| issue.code.as_str())
        .collect();

    assert!(
        issue_codes.contains(expected_code),
        "expected error code {expected_code} in {issue_codes:?}"
    );
}

fn collect_error_codes(graph: &Graph) -> HashSet<String> {
    graph
        .validate()
        .into_iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .map(|issue| issue.code)
        .collect()
}

fn test_graph(goal: impl Into<String>, nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
    Graph {
        api_version: GRAPH_API_VERSION.to_owned(),
        kind: GRAPH_KIND.to_owned(),
        metadata: GraphMetadata {
            name: "test".to_owned(),
            version: "1.0.0".to_owned(),
            description: None,
            labels: IndexMap::new(),
        },
        spec: GraphSpec {
            goal: goal.into(),
            policies: GraphPolicies::default(),
            budgets: RunBudgets::default(),
            nodes,
            edges,
        },
    }
}

#[test]
fn examples_parse_validate_and_compile() {
    for entry in std::fs::read_dir(root_examples_dir()).unwrap() {
        let path = entry.unwrap().path();
        if !path
            .extension()
            .is_some_and(|ext| ext == "yaml" || ext == "yml")
        {
            continue;
        }
        let graph = Graph::from_path(&path)
            .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
        let errors: Vec<_> = graph
            .validate()
            .into_iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .collect();
        assert!(
            errors.is_empty(),
            "validation errors in {}: {errors:?}",
            path.display()
        );
        graph
            .compile()
            .unwrap_or_else(|error| panic!("failed to compile {}: {error}", path.display()));
    }
}

#[test]
fn validates_unknown_nodes_and_edges() {
    let graph = test_graph(
        "goal",
        vec![Node::agent("a", "first")],
        vec![
            Edge {
                from: "a".to_owned(),
                to: "missing".to_owned(),
                kind: EdgeKind::Data,
                when: None,
            },
            Edge {
                from: "missing".to_owned(),
                to: "a".to_owned(),
                kind: EdgeKind::Control,
                when: None,
            },
        ],
    );
    validate_has_error(&graph, "unknown_edge_source");
    validate_has_error(&graph, "unknown_edge_destination");
}

#[test]
fn detects_duplicate_node_ids_and_duplicate_edges() {
    let duplicate_id_graph = test_graph(
        "goal",
        vec![Node::agent("a", "first"), Node::agent("a", "second")],
        vec![],
    );
    validate_has_error(&duplicate_id_graph, "duplicate_node_id");

    let duplicate_edges_graph = test_graph(
        "goal",
        vec![
            Node::agent("a", "first"),
            Node::agent("b", "second"),
            Node::agent("c", "third"),
        ],
        vec![
            Edge::data("a", "b"),
            Edge::data("a", "b"),
            Edge {
                from: "b".to_owned(),
                to: "c".to_owned(),
                kind: EdgeKind::Failure,
                when: None,
            },
        ],
    );
    let warnings = duplicate_edges_graph
        .validate()
        .into_iter()
        .filter(|issue| issue.severity == IssueSeverity::Warning)
        .map(|issue| issue.code)
        .collect::<Vec<_>>();
    assert!(warnings.iter().any(|code| code == "duplicate_edge"));
    duplicate_edges_graph
        .compile()
        .expect("duplicate edges are allowed");
}

#[test]
fn rejects_unknown_node_fields_in_yaml() {
    let yaml = r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: sample
spec:
  goal: test
  nodes:
  - id: agent
    kind: agent
    prompt: test
    securty: true
  - id: second
    kind: command
    argv: ["true"]
    workspce: current
    budget: 10
"#;
    let error = Graph::from_yaml_str(yaml).unwrap_err();
    let message = format!("{error}");
    assert!(
        message.contains("unknown field `securty`")
            || message.contains("unknown field `workspce`")
            || message.contains("unknown field `budget`")
    );
}

#[test]
fn stable_fan_in_and_fan_out_ordering() {
    let graph = Graph::new(
        "ordering",
        "check stable fan-out/fan-in ordering",
        vec![
            Node::agent("a", "a"),
            Node::agent("b", "b"),
            Node::agent("c", "c"),
        ],
    );
    let mut graph = graph;
    graph.spec.edges = vec![
        Edge::data("a", "b"),
        Edge::data("a", "c"),
        Edge::data("b", "c"),
    ];
    let compiled = graph.compile().expect("graph compiles");
    assert_eq!(compiled.order, vec!["a", "b", "c"]);

    let outgoing_from_a: Vec<_> = compiled
        .outgoing_edges("a")
        .map(|edge| edge.to.clone())
        .collect();
    assert_eq!(outgoing_from_a, vec!["b", "c"]);

    let incoming_to_c: Vec<_> = compiled
        .incoming_edges("c")
        .map(|edge| edge.from.clone())
        .collect();
    assert_eq!(incoming_to_c, vec!["a", "b"]);
}

#[test]
fn validates_conditional_edges() {
    let missing_condition = test_graph(
        "goal",
        vec![Node::agent("a", "first"), Node::agent("b", "second")],
        vec![Edge {
            from: "a".to_owned(),
            to: "b".to_owned(),
            kind: EdgeKind::Conditional,
            when: None,
        }],
    );
    validate_has_error(&missing_condition, "missing_edge_condition");

    let incomplete_condition = test_graph(
        "goal",
        vec![Node::agent("a", "first"), Node::agent("b", "second")],
        vec![Edge {
            from: "a".to_owned(),
            to: "b".to_owned(),
            kind: EdgeKind::Conditional,
            when: Some(EdgeCondition {
                status: None,
                output_contains: None,
                json_pointer: Some("/ok".to_owned()),
                equals: None,
            }),
        }],
    );
    validate_has_error(&incomplete_condition, "incomplete_edge_json_condition");
}

#[test]
fn nested_loop_must_be_bounded_and_target_real_node() {
    let nested = Graph::new("nested", "inner", vec![Node::agent("inner", "noop")]);
    let unbounded_loop = test_graph(
        "outer",
        vec![Node {
            id: "loop".to_owned(),
            label: None,
            requires: vec![],
            resources: vec![],
            retry: RetryPolicy::default(),
            timeout_seconds: None,
            workspace: WorkspaceSpec::Current,
            context: ContextSpec::default(),
            continue_on_failure: false,
            kind: NodeKind::Loop {
                graph: Box::new(nested.clone()),
                until: LoopCondition {
                    node: "inner".to_owned(),
                    status: NodeStatus::Succeeded,
                    output_contains: None,
                    json_pointer: None,
                    equals: None,
                },
                max_iterations: 0,
                stagnation_after: 1,
            },
        }],
        vec![],
    );
    validate_has_error(&unbounded_loop, "unbounded_loop");

    let unknown_nested_condition_node = test_graph(
        "outer",
        vec![Node {
            id: "loop".to_owned(),
            label: None,
            requires: vec![],
            resources: vec![],
            retry: RetryPolicy::default(),
            timeout_seconds: None,
            workspace: WorkspaceSpec::Current,
            context: ContextSpec::default(),
            continue_on_failure: false,
            kind: NodeKind::Loop {
                graph: Box::new(nested),
                until: LoopCondition {
                    node: "missing".to_owned(),
                    status: NodeStatus::Succeeded,
                    output_contains: None,
                    json_pointer: Some("/ok".to_owned()),
                    equals: Some(Value::Bool(true)),
                },
                max_iterations: 3,
                stagnation_after: 1,
            },
        }],
        vec![],
    );
    validate_has_error(
        &unknown_nested_condition_node,
        "unknown_loop_condition_node",
    );
}

#[test]
fn workspace_inheritance_is_validated() {
    let self_inherit = Graph::new(
        "self-workspace",
        "goal",
        vec![{
            let mut node = Node::agent("self", "run");
            node.workspace = WorkspaceSpec::Inherit {
                node: "self".to_owned(),
            };
            node
        }],
    );
    validate_has_error(&self_inherit, "self_workspace_inheritance");

    let missing_source = Graph::new(
        "missing-workspace-source",
        "goal",
        vec![{
            let mut node = Node::agent("child", "run");
            node.workspace = WorkspaceSpec::Inherit {
                node: "missing".to_owned(),
            };
            node
        }],
    );
    validate_has_error(&missing_source, "unknown_workspace_source");

    let invalid_inheritance = test_graph(
        "invalid-workspace-inheritance",
        vec![Node::agent("source", "run source"), {
            let mut node = Node::agent("consumer", "run");
            node.workspace = WorkspaceSpec::Inherit {
                node: "source".to_owned(),
            };
            node
        }],
        vec![],
    );
    let invalid_inheritance_codes = collect_error_codes(&invalid_inheritance);
    assert!(
        invalid_inheritance_codes.contains("unordered_workspace_inheritance"),
        "expected unordered_workspace_inheritance in {invalid_inheritance_codes:?}"
    );

    let valid_inheritance = test_graph(
        "valid-workspace-inheritance",
        vec![Node::agent("source", "run source"), {
            let mut node = Node::agent("consumer", "run");
            node.workspace = WorkspaceSpec::Inherit {
                node: "source".to_owned(),
            };
            node
        }],
        vec![Edge {
            from: "source".to_owned(),
            to: "consumer".to_owned(),
            kind: EdgeKind::Data,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Succeeded),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        }],
    );
    assert!(
        collect_error_codes(&valid_inheritance).is_empty(),
        "valid inheritance edge should not emit validation errors"
    );

    let invalid_inheritance_condition = test_graph(
        "invalid-workspace-inheritance-condition",
        vec![Node::agent("source", "run source"), {
            let mut node = Node::agent("consumer", "run");
            node.workspace = WorkspaceSpec::Inherit {
                node: "source".to_owned(),
            };
            node
        }],
        vec![Edge {
            from: "source".to_owned(),
            to: "consumer".to_owned(),
            kind: EdgeKind::Data,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Failed),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        }],
    );
    validate_has_error(
        &invalid_inheritance_condition,
        "workspace_inheritance_requires_success_edge",
    );
}

#[test]
fn validates_blank_worktree_base_rejected() {
    let mut node = Node::agent("agent", "run");
    node.workspace = WorkspaceSpec::Worktree {
        base: Some("   ".to_owned()),
        auto_commit: false,
    };
    let graph = test_graph("blank-worktree-base", vec![node], vec![]);
    validate_has_error(&graph, "empty_worktree_base");
}

#[test]
fn validates_agent_worktree_requires_fan_out_one() {
    let mut node = Node::agent("agent", "run");
    node.workspace = WorkspaceSpec::Worktree {
        base: Some("/tmp".to_owned()),
        auto_commit: false,
    };
    if let NodeKind::Agent { fan_out, .. } = &mut node.kind {
        *fan_out = 2;
    }
    let graph = test_graph("worktree-fanout", vec![node], vec![]);
    validate_has_error(&graph, "agent_worktree_requires_fan_out_one");
}

#[test]
fn validates_workspace_inherit_edge_status_must_be_succeeded() {
    let no_condition_inheritance = test_graph(
        "workspace-inheritance-no-condition",
        vec![Node::agent("source", "run source"), {
            let mut node = Node::agent("consumer", "run");
            node.workspace = WorkspaceSpec::Inherit {
                node: "source".to_owned(),
            };
            node
        }],
        vec![Edge {
            from: "source".to_owned(),
            to: "consumer".to_owned(),
            kind: EdgeKind::Data,
            when: None,
        }],
    );
    assert!(
        collect_error_codes(&no_condition_inheritance).is_empty(),
        "inheritance without explicit when should default to succeeded"
    );

    let failure_inheritance = test_graph(
        "workspace-inheritance-failure-edge",
        vec![Node::agent("source", "run source"), {
            let mut node = Node::agent("consumer", "run");
            node.workspace = WorkspaceSpec::Inherit {
                node: "source".to_owned(),
            };
            node
        }],
        vec![Edge {
            from: "source".to_owned(),
            to: "consumer".to_owned(),
            kind: EdgeKind::Failure,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Failed),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        }],
    );
    validate_has_error(
        &failure_inheritance,
        "workspace_inheritance_requires_success_edge",
    );
}

#[test]
fn validates_edge_conditions_are_terminal_and_failure_specific() {
    let failure_with_success_condition = test_graph(
        "failure edge condition",
        vec![
            Node::agent("source", "run source"),
            Node::agent("consumer", "run"),
        ],
        vec![Edge {
            from: "source".to_owned(),
            to: "consumer".to_owned(),
            kind: EdgeKind::Failure,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Succeeded),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        }],
    );
    validate_has_error(
        &failure_with_success_condition,
        "failure_edge_must_match_failed_status",
    );

    let nonterminal_condition = test_graph(
        "nonterminal edge condition",
        vec![
            Node::agent("source", "run source"),
            Node::agent("consumer", "run"),
        ],
        vec![Edge {
            from: "source".to_owned(),
            to: "consumer".to_owned(),
            kind: EdgeKind::Data,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Running),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        }],
    );
    validate_has_error(&nonterminal_condition, "invalid_edge_condition_status");
}

#[test]
#[allow(clippy::too_many_lines)]
fn validate_numeric_limits_with_boundaries() {
    let mut wall_time_ok = test_graph("max-wall-time", vec![Node::agent("a", "prompt")], vec![]);
    wall_time_ok.spec.budgets.wall_time_seconds = Some(31_536_000);
    assert!(
        collect_error_codes(&wall_time_ok).is_empty(),
        "wall time at boundary should be allowed"
    );

    let mut wall_time_exceeded =
        test_graph("max-wall-time", vec![Node::agent("a", "prompt")], vec![]);
    wall_time_exceeded.spec.budgets.wall_time_seconds = Some(31_536_001);
    assert!(
        collect_error_codes(&wall_time_exceeded).contains("wall_time_exceeds_limit"),
        "wall time just above max should be rejected"
    );

    let mut timeout_node = Node::agent("a", "prompt");
    timeout_node.timeout_seconds = Some(31_536_000);
    let timeout_node_ok = test_graph("max-timeout", vec![timeout_node], vec![]);
    assert!(
        collect_error_codes(&timeout_node_ok).is_empty(),
        "timeout at boundary should be allowed"
    );

    let mut timeout_node_exceeded = Node::agent("a", "prompt");
    timeout_node_exceeded.timeout_seconds = Some(31_536_001);
    let timeout_exceeded_graph = test_graph("max-timeout", vec![timeout_node_exceeded], vec![]);
    assert!(
        collect_error_codes(&timeout_exceeded_graph).contains("node_timeout_exceeds_limit"),
        "timeout just above max should be rejected"
    );

    let mut backoff_node = Node::agent("a", "prompt");
    backoff_node.retry.backoff_seconds = 31_536_000;
    let backoff_ok = test_graph("max-backoff", vec![backoff_node], vec![]);
    assert!(
        collect_error_codes(&backoff_ok).is_empty(),
        "retry backoff at boundary should be allowed"
    );

    let mut backoff_node_exceeded = Node::agent("a", "prompt");
    backoff_node_exceeded.retry.backoff_seconds = 31_536_001;
    let backoff_exceeded = test_graph("max-backoff", vec![backoff_node_exceeded], vec![]);
    assert!(
        collect_error_codes(&backoff_exceeded).contains("retry_backoff_exceeds_limit"),
        "retry backoff just above max should be rejected"
    );

    let mut fanout_ok = Node::agent("a", "prompt");
    if let NodeKind::Agent { fan_out, .. } = &mut fanout_ok.kind {
        *fan_out = 256;
    }
    let fanout_graph_ok = test_graph("max-fan-out", vec![fanout_ok], vec![]);
    assert!(
        collect_error_codes(&fanout_graph_ok).is_empty(),
        "fan_out at boundary should be allowed"
    );

    let mut fanout_exceeded = Node::agent("a", "prompt");
    if let NodeKind::Agent { fan_out, .. } = &mut fanout_exceeded.kind {
        *fan_out = 257;
    }
    let fanout_graph_exceeded = test_graph("max-fan-out", vec![fanout_exceeded], vec![]);
    assert!(
        collect_error_codes(&fanout_graph_exceeded).contains("excessive_fan_out"),
        "fan_out above max should be rejected"
    );

    let mut parallel_ok = Graph::new(
        "parallel",
        "max parallel boundary",
        vec![Node::agent("a", "start"), Node::agent("b", "finish")],
    );
    parallel_ok.spec.edges = vec![Edge::data("a", "b")];
    parallel_ok.spec.policies.max_parallel = 256;
    assert!(
        collect_error_codes(&parallel_ok).is_empty(),
        "max_parallel at boundary should be allowed",
    );

    let mut parallel_too_high = parallel_ok;
    parallel_too_high.spec.policies.max_parallel = 257;
    assert!(
        collect_error_codes(&parallel_too_high).contains("excessive_parallelism"),
        "max_parallel above max should be rejected",
    );

    let mut retry_ok = Node::agent("retry", "prompt");
    retry_ok.retry.max_attempts = 16;
    let retry_ok_graph = test_graph("retry", vec![retry_ok], vec![]);
    assert!(
        collect_error_codes(&retry_ok_graph).is_empty(),
        "max_attempts at boundary should be allowed",
    );

    let mut retry_too_high = Node::agent("retry", "prompt");
    retry_too_high.retry.max_attempts = 17;
    let retry_too_high_graph = test_graph("retry", vec![retry_too_high], vec![]);
    assert!(
        collect_error_codes(&retry_too_high_graph).contains("excessive_retry_attempts"),
        "max_attempts above max should be rejected",
    );

    let nested = Graph::new("nested-loop", "inner", vec![Node::agent("inner", "noop")]);
    let loop_ok = test_graph(
        "outer",
        vec![Node {
            id: "loop".to_owned(),
            label: None,
            requires: vec![],
            resources: vec![],
            retry: RetryPolicy::default(),
            timeout_seconds: None,
            workspace: WorkspaceSpec::Current,
            context: ContextSpec::default(),
            continue_on_failure: false,
            kind: NodeKind::Loop {
                graph: Box::new(nested.clone()),
                until: LoopCondition {
                    node: "inner".to_owned(),
                    status: NodeStatus::Succeeded,
                    output_contains: None,
                    json_pointer: None,
                    equals: None,
                },
                max_iterations: 1024,
                stagnation_after: 1,
            },
        }],
        vec![],
    );
    assert!(
        collect_error_codes(&loop_ok).is_empty(),
        "max_iterations at boundary should be allowed",
    );

    let mut loop_too_high = loop_ok;
    if let NodeKind::Loop { max_iterations, .. } = &mut loop_too_high.spec.nodes[0].kind {
        *max_iterations = 1025;
    }
    assert!(
        collect_error_codes(&loop_too_high).contains("excessive_loop_iterations"),
        "max_iterations above max should be rejected",
    );

    let mut loop_workload_too_high = loop_too_high;
    let mut loop_nested_nodes: Vec<_> = Vec::new();
    for i in 0..4000 {
        loop_nested_nodes.push(Node::agent(format!("inner_{i}"), "prompt"));
    }
    loop_workload_too_high.spec.policies.max_parallel = 256;
    if let NodeKind::Loop {
        max_iterations,
        graph: nested,
        until,
        ..
    } = &mut loop_workload_too_high.spec.nodes[0].kind
    {
        *max_iterations = 1024;
        **nested = Graph::new("nested", "loop-work", loop_nested_nodes);
        until.node = "inner_0".to_owned();
    }
    assert!(
        collect_error_codes(&loop_workload_too_high).contains("excessive_loop_workload")
            || collect_error_codes(&loop_workload_too_high).contains("parallel_workload_overflow"),
        "loop workload at boundary should be limited",
    );
}

#[test]
fn validates_nested_graph_nesting_depth_limit() {
    let mut nested = Graph::new(
        "nested-tail",
        "depth-boundary",
        vec![Node::agent("leaf", "prompt")],
    );
    for level in (0..33).rev() {
        let mut wrapper = Node::agent(format!("depth_{level}"), "prompt");
        wrapper.kind = NodeKind::Subgraph {
            graph: Box::new(nested),
        };
        nested = Graph::new(format!("nested-{level}"), "nested", vec![wrapper]);
    }
    let errors = collect_error_codes(&nested);
    assert!(
        errors.contains("excessive_nesting_depth"),
        "nested depth above max should be rejected"
    );
}

#[test]
fn validates_nested_graph_node_count_limit() {
    let mut nested_nodes = Vec::new();
    for idx in 0..10_001 {
        nested_nodes.push(Node::agent(format!("n_{idx}"), "prompt"));
    }
    let nested = Graph::new("nested-count", "count-boundary", nested_nodes);
    let mut outer = Graph::new(
        "outer",
        "nesting count",
        vec![Node::agent("outer", "prompt")],
    );
    outer.spec.nodes[0].kind = NodeKind::Subgraph {
        graph: Box::new(nested),
    };
    let errors = collect_error_codes(&outer);
    assert!(
        errors.contains("excessive_nested_node_count")
            || errors.contains("nested_node_count_overflow"),
        "too many nested nodes should be rejected"
    );

    let mut boundary_nested_nodes = Vec::new();
    for idx in 0..9_999 {
        boundary_nested_nodes.push(Node::agent(format!("n_{idx}"), "prompt"));
    }
    let boundary_nested = Graph::new("nested-count", "count-boundary", boundary_nested_nodes);
    let mut boundary_outer = Graph::new(
        "outer",
        "nesting count",
        vec![Node::agent("outer", "prompt")],
    );
    boundary_outer.spec.nodes[0].kind = NodeKind::Subgraph {
        graph: Box::new(boundary_nested),
    };
    assert!(
        !collect_error_codes(&boundary_outer).contains("excessive_nested_node_count")
            && !collect_error_codes(&boundary_outer).contains("nested_node_count_overflow"),
        "total nested node count with root at boundary should be allowed",
    );
    let mut boundary_too_high_nested_nodes = Vec::new();
    for idx in 0..10_000 {
        boundary_too_high_nested_nodes.push(Node::agent(format!("n_{idx}"), "prompt"));
    }
    let boundary_too_high_nested = Graph::new(
        "nested-count",
        "count-boundary",
        boundary_too_high_nested_nodes,
    );
    let mut boundary_too_high_outer = Graph::new(
        "outer",
        "nesting count",
        vec![Node::agent("outer", "prompt")],
    );
    boundary_too_high_outer.spec.nodes[0].kind = NodeKind::Subgraph {
        graph: Box::new(boundary_too_high_nested),
    };
    assert!(
        collect_error_codes(&boundary_too_high_outer).contains("excessive_nested_node_count")
            || collect_error_codes(&boundary_too_high_outer).contains("nested_node_count_overflow"),
        "total nested node count above limit should be rejected",
    );
}

#[test]
fn validates_loop_subgraph_workload_overflow() {
    let mut nested_nodes = Vec::new();
    for idx in 0..4000 {
        nested_nodes.push(Node::agent(format!("inner_{idx}"), "prompt"));
    }
    let nested_graph = Graph::new("nested", "nested", nested_nodes);
    let mut outer = Graph::new(
        "outer",
        "loop subgraph workload",
        vec![Node::agent("loop", "prompt")],
    );
    let until_node = "inner_0".to_owned();
    outer.spec.nodes[0].kind = NodeKind::Loop {
        graph: Box::new(nested_graph),
        until: LoopCondition {
            node: until_node,
            status: NodeStatus::Succeeded,
            output_contains: None,
            json_pointer: None,
            equals: None,
        },
        max_iterations: 1024,
        stagnation_after: 1,
    };
    outer.spec.nodes[0].retry.max_attempts = 1;

    assert!(
        collect_error_codes(&outer).contains("excessive_loop_workload"),
        "loop workload with nested subgraph should be bounded",
    );
}

#[test]
fn rejects_unreachable_or_failure_status_as_a_loop_completion_condition() {
    for status in [
        NodeStatus::Pending,
        NodeStatus::Ready,
        NodeStatus::Running,
        NodeStatus::Failed,
        NodeStatus::Blocked,
        NodeStatus::Cancelled,
    ] {
        let inner = Graph::new("inner", "inner", vec![Node::agent("condition", "prompt")]);
        let mut loop_node = Node::agent("loop", "prompt");
        loop_node.kind = NodeKind::Loop {
            graph: Box::new(inner),
            until: LoopCondition {
                node: "condition".to_owned(),
                status,
                output_contains: None,
                json_pointer: None,
                equals: None,
            },
            max_iterations: 2,
            stagnation_after: 1,
        };
        let graph = test_graph("invalid-loop-status", vec![loop_node], vec![]);

        assert!(
            collect_error_codes(&graph).contains("invalid_loop_condition_status"),
            "loop completion status {status:?} should be rejected",
        );
    }
}

#[test]
fn rejects_unusable_retry_rebind_profiles() {
    let mut blank = Node::agent("blank", "prompt");
    blank.retry.max_attempts = 2;
    blank.retry.rebind_profiles = vec!["  ".to_owned()];
    let blank_graph = test_graph("blank-rebind", vec![blank], vec![]);
    assert!(collect_error_codes(&blank_graph).contains("empty_retry_rebind_profile"));

    let mut excessive = Node::agent("excessive", "prompt");
    excessive.retry.max_attempts = 2;
    excessive.retry.rebind_profiles = vec!["one".to_owned(), "two".to_owned()];
    let excessive_graph = test_graph("excessive-rebind", vec![excessive], vec![]);
    assert!(collect_error_codes(&excessive_graph).contains("excessive_retry_rebind_profiles"));

    let mut command = Node::command("command", vec!["true".to_owned()]);
    command.retry.max_attempts = 2;
    command.retry.rebind_profiles = vec!["unused".to_owned()];
    let command_graph = test_graph("unsupported-rebind", vec![command], vec![]);
    assert!(collect_error_codes(&command_graph).contains("unsupported_retry_rebind_profiles"));
}

#[test]
fn validates_nested_loop_retry_workload_overflow() {
    let leaf = Graph::new("leaf", "leaf", vec![Node::agent("leaf", "prompt")]);
    let mut inner_loop_node = Node::agent("inner", "prompt");
    inner_loop_node.kind = NodeKind::Loop {
        graph: Box::new(leaf),
        until: LoopCondition {
            node: "leaf".to_owned(),
            status: NodeStatus::Succeeded,
            output_contains: None,
            json_pointer: None,
            equals: None,
        },
        max_iterations: 1024,
        stagnation_after: 1,
    };
    inner_loop_node.retry.max_attempts = 16;
    let inner = Graph::new("inner-body", "inner body", vec![inner_loop_node]);

    let mut outer_loop = Node::agent("outer", "prompt");
    outer_loop.kind = NodeKind::Loop {
        graph: Box::new(inner),
        until: LoopCondition {
            node: "inner".to_owned(),
            status: NodeStatus::Succeeded,
            output_contains: None,
            json_pointer: None,
            equals: None,
        },
        max_iterations: 1024,
        stagnation_after: 1,
    };
    outer_loop.retry.max_attempts = 16;

    let outer = test_graph("outer-workload", vec![outer_loop], vec![]);

    assert!(
        collect_error_codes(&outer).contains("excessive_loop_workload"),
        "nested loop workload with retries should be bounded",
    );
}

#[test]
fn validates_top_level_graph_workload_overflow() {
    let mut top_nodes = Vec::new();
    for idx in 0..250 {
        let mut node = Node::agent(format!("n_{idx}"), "prompt");
        if let NodeKind::Agent { fan_out, .. } = &mut node.kind {
            *fan_out = 256;
        }
        node.retry.max_attempts = 16;
        top_nodes.push(node);
    }
    let top = test_graph("top-level-workload", top_nodes, vec![]);

    assert!(
        collect_error_codes(&top).contains("excessive_graph_workload")
            || collect_error_codes(&top).contains("graph_workload_overflow"),
        "top-level graph workload should be bounded",
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn validate_memory_limits_with_boundaries() {
    let mut context_ok = Node::agent("a", "prompt");
    context_ok.context.max_bytes = 64 * 1024 * 1024;
    let context_ok_graph = test_graph("max-context-bytes", vec![context_ok], vec![]);
    assert!(
        collect_error_codes(&context_ok_graph).is_empty(),
        "context max_bytes at boundary should be allowed"
    );

    let mut context_exceeded = Node::agent("a", "prompt");
    context_exceeded.context.max_bytes = 64 * 1024 * 1024 + 1;
    let context_exceeded_graph = test_graph("max-context-bytes", vec![context_exceeded], vec![]);
    assert!(
        collect_error_codes(&context_exceeded_graph).contains("context_bytes_exceeds_limit"),
        "context max_bytes above max should be rejected"
    );

    let mut agent_with_large_output = Node::agent("agent", "prompt");
    if let NodeKind::Agent { output, .. } = &mut agent_with_large_output.kind {
        output.max_bytes = 64 * 1024 * 1024 + 1;
    }
    let agent_graph = test_graph(
        "max-output-bytes-agent",
        vec![agent_with_large_output],
        vec![],
    );
    assert!(
        collect_error_codes(&agent_graph).contains("output_bytes_exceeds_limit"),
        "agent output max_bytes above max should be rejected"
    );

    let mut agent_with_boundary_output = Node::agent("agent", "prompt");
    if let NodeKind::Agent { output, .. } = &mut agent_with_boundary_output.kind {
        output.max_bytes = 64 * 1024 * 1024;
    }
    let agent_graph = test_graph(
        "max-output-bytes-agent",
        vec![agent_with_boundary_output],
        vec![],
    );
    assert!(
        !collect_error_codes(&agent_graph).contains("output_bytes_exceeds_limit"),
        "agent output max_bytes at boundary should be allowed"
    );

    let mut reduce = Node::agent("reduce", "prompt");
    reduce.kind = NodeKind::Reduce {
        prompt: PromptSpec::Inline("reduce".into()),
        profile: None,
        model: None,
        output: OutputSpec::default(),
    };
    if let NodeKind::Reduce { output, .. } = &mut reduce.kind {
        output.max_bytes = 64 * 1024 * 1024 + 1;
    }
    let reduce_graph = test_graph("max-output-bytes-reduce", vec![reduce], vec![]);
    assert!(
        collect_error_codes(&reduce_graph).contains("output_bytes_exceeds_limit"),
        "reduce output max_bytes above max should be rejected"
    );
    let mut reduce = Node::agent("reduce", "prompt");
    reduce.kind = NodeKind::Reduce {
        prompt: PromptSpec::Inline("reduce".into()),
        profile: None,
        model: None,
        output: OutputSpec::default(),
    };
    if let NodeKind::Reduce { output, .. } = &mut reduce.kind {
        output.max_bytes = 64 * 1024 * 1024;
    }
    let reduce_graph = test_graph("max-output-bytes-reduce", vec![reduce], vec![]);
    assert!(
        !collect_error_codes(&reduce_graph).contains("output_bytes_exceeds_limit"),
        "reduce output max_bytes at boundary should be allowed"
    );

    let mut synth = Node::agent("synth", "prompt");
    synth.kind = NodeKind::Synthesize {
        prompt: PromptSpec::Inline("synth".into()),
        profile: None,
        model: None,
        output: OutputSpec::default(),
    };
    if let NodeKind::Synthesize { output, .. } = &mut synth.kind {
        output.max_bytes = 64 * 1024 * 1024 + 1;
    }
    let synth_graph = test_graph("max-output-bytes-synth", vec![synth], vec![]);
    assert!(
        collect_error_codes(&synth_graph).contains("output_bytes_exceeds_limit"),
        "synthesize output max_bytes above max should be rejected"
    );
    let mut synth = Node::agent("synth", "prompt");
    synth.kind = NodeKind::Synthesize {
        prompt: PromptSpec::Inline("synth".into()),
        profile: None,
        model: None,
        output: OutputSpec::default(),
    };
    if let NodeKind::Synthesize { output, .. } = &mut synth.kind {
        output.max_bytes = 64 * 1024 * 1024;
    }
    let synth_graph = test_graph("max-output-bytes-synth", vec![synth], vec![]);
    assert!(
        !collect_error_codes(&synth_graph).contains("output_bytes_exceeds_limit"),
        "synthesize output max_bytes at boundary should be allowed"
    );

    let mut command = Node::agent("command", "prompt");
    command.kind = NodeKind::Command {
        argv: vec!["echo".to_owned(), "ok".to_owned()],
        env: IndexMap::new(),
        output: OutputSpec::default(),
    };
    if let NodeKind::Command { output, .. } = &mut command.kind {
        output.max_bytes = 64 * 1024 * 1024 + 1;
    }
    let command_graph = test_graph("max-output-bytes-command", vec![command], vec![]);
    assert!(
        collect_error_codes(&command_graph).contains("output_bytes_exceeds_limit"),
        "command output max_bytes above max should be rejected"
    );
    let mut command = Node::agent("command", "prompt");
    command.kind = NodeKind::Command {
        argv: vec!["echo".to_owned(), "ok".to_owned()],
        env: IndexMap::new(),
        output: OutputSpec::default(),
    };
    if let NodeKind::Command { output, .. } = &mut command.kind {
        output.max_bytes = 64 * 1024 * 1024;
    }
    let command_graph = test_graph("max-output-bytes-command", vec![command], vec![]);
    assert!(
        !collect_error_codes(&command_graph).contains("output_bytes_exceeds_limit"),
        "command output max_bytes at boundary should be allowed"
    );

    let mut verify = Node::agent("verify", "prompt");
    verify.kind = NodeKind::Verify {
        argv: vec!["echo".to_owned(), "ok".to_owned()],
        env: IndexMap::new(),
        output: OutputSpec::default(),
    };
    if let NodeKind::Verify { output, .. } = &mut verify.kind {
        output.max_bytes = 64 * 1024 * 1024 + 1;
    }
    let verify_graph = test_graph("max-output-bytes-verify", vec![verify], vec![]);
    assert!(
        collect_error_codes(&verify_graph).contains("output_bytes_exceeds_limit"),
        "verify output max_bytes above max should be rejected"
    );
    let mut verify = Node::agent("verify", "prompt");
    verify.kind = NodeKind::Verify {
        argv: vec!["echo".to_owned(), "ok".to_owned()],
        env: IndexMap::new(),
        output: OutputSpec::default(),
    };
    if let NodeKind::Verify { output, .. } = &mut verify.kind {
        output.max_bytes = 64 * 1024 * 1024;
    }
    let verify_graph = test_graph("max-output-bytes-verify", vec![verify], vec![]);
    assert!(
        !collect_error_codes(&verify_graph).contains("output_bytes_exceeds_limit"),
        "verify output max_bytes at boundary should be allowed"
    );
}

#[test]
fn render_outputs_smoke() {
    let graph = Graph::new(
        "render",
        "render graph artifacts",
        vec![Node::agent("a", "start"), Node::agent("b", "finish")],
    );
    let mut graph = graph;
    graph.spec.edges = vec![Edge::data("a", "b")];
    let compiled = graph.compile().expect("graph compiles");

    let mermaid = compiled.render_mermaid();
    let dot = compiled.render_dot();
    let explanation = compiled.explain();

    assert!(mermaid.contains("flowchart TD"));
    assert!(mermaid.contains("n_a"));
    assert!(dot.contains("digraph gloop"));
    assert!(dot.contains("\"a\""));
    assert!(explanation.contains("Graph: render"));
}

#[test]
fn render_dot_escapes_identifiers_and_labels() {
    let compiled = test_graph(
        "escape",
        vec![Node {
            id: "a".to_owned(),
            label: Some("raw \"label\"".to_owned()),
            requires: vec![],
            resources: vec![],
            retry: RetryPolicy::default(),
            timeout_seconds: None,
            workspace: WorkspaceSpec::Current,
            context: ContextSpec::default(),
            continue_on_failure: false,
            kind: NodeKind::Agent {
                prompt: PromptSpec::Inline("prompt".into()),
                profile: None,
                model: None,
                fan_out: 1,
                output: OutputSpec::default(),
            },
        }],
        vec![],
    )
    .compile()
    .expect("graph compiles");
    let mut compiled = compiled;
    compiled.graph.spec.nodes[0].id = "node\"weird\\name\nline".to_owned();
    compiled.graph.spec.nodes[0].label = Some("line one\nline two\\path\"end".to_owned());

    let dot = compiled.render_dot();
    assert!(dot.contains("node\\\"weird\\\\name\\nline"));
    assert!(dot.contains("line one\\nline two\\\\path\\\"end"));
}

#[test]
fn render_mermaid_escapes_label_syntax_and_html() {
    let mut node = Node::agent("safe", "prompt");
    node.label = Some("raw \"label\"]\nclick n_safe callback <script>&\\`".to_owned());
    let compiled = Graph::new("mermaid-escape", "escape", vec![node])
        .compile()
        .expect("graph compiles");

    let mermaid = compiled.render_mermaid();
    assert!(!mermaid.contains("\nclick"));
    assert!(!mermaid.contains("<script>"));
    assert!(mermaid.contains("&quot;"));
    assert!(mermaid.contains("&lt;script&gt;"));
    assert!(mermaid.contains("&amp;"));
    assert!(mermaid.contains("&#92;"));
    assert!(mermaid.contains("&#96;"));
}

#[test]
fn generates_graph_json_schema() {
    let schema = schema_for!(Graph);
    let rendered = serde_json::to_string_pretty(&schema).expect("serialize schema");
    assert!(!rendered.is_empty());
    assert!(rendered.contains('"'));
    let value: Value = serde_json::from_str(&rendered).expect("schema json");
    assert!(value.get("title").is_some());
}

#[test]
fn parser_rejects_unknown_top_level_and_nested_fields() {
    let top_level_unknown = r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
unknown_scope: forbidden
metadata:
  name: unknown-top-level
  version: "1.0.0"
spec:
  goal: test
  nodes: []
"#;
    assert!(matches!(
        Graph::from_yaml_str(top_level_unknown),
        Err(GraphError::Yaml(_))
    ));

    let spec_unknown = r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: unknown-spec
  version: "1.0.0"
spec:
  goal: test
  unicorns: true
  nodes: []
"#;
    assert!(matches!(
        Graph::from_yaml_str(spec_unknown),
        Err(GraphError::Yaml(_))
    ));

    let node_kind_unknown = r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: unknown-node-kind
  version: "1.0.0"
spec:
  goal: test
  nodes:
    - id: a
      requires: []
      resources: []
      retry:
        max_attempts: 1
        backoff_seconds: 0
        rebind_profiles: []
      workspace:
        mode: current
      context:
        include_dependencies: true
        files: []
        max_bytes: 262144
      continue_on_failure: false
      kind: agent
      prompt: test
      fantasy_mode: true
"#;
    assert!(matches!(
        Graph::from_yaml_str(node_kind_unknown),
        Err(GraphError::Yaml(_))
    ));
}

#[test]
fn parser_rejects_duplicate_mapping_keys_explicitly() {
    let duplicate_mapping_key = r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: duplicate-key
  version: "1.0.0"
  name: duplicate-name
spec:
  goal: test
  nodes: []
"#;
    let parsed = Graph::from_yaml_str(duplicate_mapping_key);
    assert!(
        parsed.is_err(),
        "GAP: duplicate YAML mapping keys currently accepted; parser currently does not fail closed on `name` duplication"
    );
    if let Ok(graph) = parsed {
        assert_eq!(
            graph.metadata.name, "duplicate-name",
            "GAP: duplicate keys were last-write-wins (`name` unexpectedly kept `duplicate-name`)"
        );
    }
}

#[test]
fn parser_rejects_multi_document_yaml_input() {
    let multi_document = r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: one
  version: "1.0.0"
spec:
  goal: first
  nodes: []
---
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: two
  version: "1.0.0"
spec:
  goal: second
  nodes: []
"#;
    assert!(matches!(
        Graph::from_yaml_str(multi_document),
        Err(GraphError::Yaml(_))
    ));
}

#[test]
fn parser_handles_alias_heavy_input_with_bounded_behavior() {
    let alias_chain = r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: alias-heavy
  version: "1.0.0"
spec:
  goal: alias
  nodes:
    - id: a
      requires: []
      resources: []
      retry: &retry
        max_attempts: 1
        backoff_seconds: 0
        rebind_profiles: []
      workspace:
        mode: current
      context:
        include_dependencies: true
        files: []
        max_bytes: 262144
      continue_on_failure: false
      kind: agent
      prompt: &prompt shared
    - id: b
      requires: [a]
      resources: []
      retry: *retry
      workspace:
        mode: current
      context:
        include_dependencies: true
        files: []
        max_bytes: 262144
      continue_on_failure: false
      kind: agent
      prompt: *prompt
    - id: c
      requires: [b]
      resources: []
      retry: *retry
      workspace:
        mode: current
      context:
        include_dependencies: true
        files: []
        max_bytes: 262144
      continue_on_failure: false
      kind: agent
      prompt: *prompt
    - id: d
      requires: [c]
      resources: []
      retry: *retry
      workspace:
        mode: current
      context:
        include_dependencies: true
        files: []
        max_bytes: 262144
      continue_on_failure: false
      kind: agent
      prompt: *prompt
    - id: e
      requires: [d]
      resources: []
      retry: *retry
      workspace:
        mode: current
      context:
        include_dependencies: true
        files: []
        max_bytes: 262144
      continue_on_failure: false
      kind: agent
      prompt: *prompt
  edges: []
"#;
    let graph = Graph::from_yaml_str(alias_chain)
        .unwrap_or_else(|error| panic!("alias-heavy input rejected unexpectedly: {error}"));
    let errors = graph.validate();
    assert!(
        !errors
            .iter()
            .any(|issue| issue.severity == IssueSeverity::Error),
        "alias-heavy YAML should parse/validate: {errors:?}"
    );
}

#[test]
fn parser_rejects_graph_source_above_max_size() {
    let base = r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: oversized
  version: "1.0.0"
spec:
  goal: oversized graph
  nodes:
    - id: a
      requires: []
      resources: []
      retry:
        max_attempts: 1
        backoff_seconds: 0
        rebind_profiles: []
      workspace:
        mode: current
      context:
        include_dependencies: true
        files: []
        max_bytes: 262144
      continue_on_failure: false
      kind: agent
      prompt: ok
  edges: []
"#;
    let mut oversized = base.to_string();
    if oversized.len() < MAX_GRAPH_SOURCE_BYTES + 1 {
        oversized.push('\n');
        oversized.push_str(&" ".repeat(MAX_GRAPH_SOURCE_BYTES + 1 - oversized.len()));
    }

    let result = Graph::from_yaml_str(&oversized);
    assert!(
        matches!(result, Err(GraphError::SourceTooLarge { .. })),
        "oversized inline YAML should be rejected"
    );
    let errors = collect_error_codes(&test_graph(
        "max-source-size",
        vec![Node::agent("a", "ok")],
        vec![],
    ));
    assert!(!errors.contains("excessive_graph_workload"));

    let path = std::env::temp_dir().join("gloop-graph-over-size.yaml");
    std::fs::write(&path, &oversized).expect("write oversized graph source");
    let from_path_result = Graph::from_path(&path);
    let _ = std::fs::remove_file(&path);
    assert!(
        matches!(from_path_result, Err(GraphError::SourceTooLarge { .. })),
        "oversized graph source files should be rejected"
    );
}

#[test]
fn from_path_rejects_sparse_oversized_source() {
    let path = std::env::temp_dir().join("gloop-sparse-oversized.yaml");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .expect("create sparse test file");
    let base_yaml = r#"apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: sparse-oversized
  version: "1.0.0"
spec:
  goal: sparse oversized
  nodes:
    - id: a
      requires: []
      resources: []
      retry:
        max_attempts: 1
        backoff_seconds: 0
        rebind_profiles: []
      workspace:
        mode: current
      context:
        include_dependencies: true
        files: []
        max_bytes: 262144
      continue_on_failure: false
      kind: agent
      prompt: ok
  edges: []
"#;
    file.write_all(base_yaml.as_bytes())
        .expect("write base yaml");
    file.seek(SeekFrom::Start(MAX_GRAPH_SOURCE_BYTES as u64))
        .expect("seek sparse source size");
    file.write_all(b" ")
        .expect("write sparse source terminal byte");
    file.sync_all().expect("flush sparse source");
    drop(file);

    let result = Graph::from_path(&path);
    let _ = std::fs::remove_file(&path);
    assert!(
        matches!(result, Err(GraphError::SourceTooLarge { .. })),
        "sparse oversized graph source should be rejected by from_path"
    );
}

#[test]
fn parser_allows_graph_source_at_max_size() {
    let base = r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: bounded
  version: "1.0.0"
spec:
  goal: bounded graph
  nodes:
    - id: a
      requires: []
      resources: []
      retry:
        max_attempts: 1
        backoff_seconds: 0
        rebind_profiles: []
      workspace:
        mode: current
      context:
        include_dependencies: true
        files: []
        max_bytes: 262144
      continue_on_failure: false
      kind: agent
      prompt: ok
  edges: []
"#;
    let mut bounded = base.to_string();
    if bounded.len() < MAX_GRAPH_SOURCE_BYTES {
        bounded.push('\n');
        bounded.push_str(&" ".repeat(MAX_GRAPH_SOURCE_BYTES - bounded.len()));
    }

    let graph = Graph::from_yaml_str(&bounded).expect("bounded graph source should parse");
    assert!(
        graph
            .validate()
            .into_iter()
            .all(|issue| issue.severity != IssueSeverity::Error),
        "bounded graph source should be valid"
    );
}
