//! Interactive and template-driven graph builders for gloop.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use dialoguer::{Confirm, Input, MultiSelect, Select, theme::ColorfulTheme};
use gloop_core::graph::{
    MAX_CONTEXT_BYTES, MAX_DURATION_SECONDS, MAX_FAN_OUT, MAX_LOOP_ITERATIONS, MAX_OUTPUT_BYTES,
    MAX_PARALLELISM, MAX_RETRY_ATTEMPTS,
};
use gloop_core::{
    ContextSpec, Edge, EdgeCondition, EdgeKind, FailurePolicy, Graph, IssueSeverity, LoopCondition,
    Node, NodeKind, NodeStatus, OutputFormat, OutputSpec, PromptSpec, RetryPolicy, RunBudgets,
    WorkspaceSpec,
};
use serde_json::Value;

use crate::templates::{validate_init_template_name, DEFAULT_TEMPLATE_GOAL};

const INTERACTIVE_NESTING_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy)]
pub enum GraphTemplate {
    Direct,
    PlanImplementVerify,
    ParallelResearchReduce,
    ReviewFixLoop,
}

pub fn request_graph(
    name: impl Into<String>,
    goal: impl Into<String>,
    request: impl Into<String>,
) -> Graph {
    template_graph(
        name,
        goal,
        GraphTemplate::Direct,
        Some(request.into()),
        None,
        None,
    )
}

pub fn template_graph(
    name: impl Into<String>,
    goal: impl Into<String>,
    template: GraphTemplate,
    request: Option<String>,
    provider_profiles: Option<Vec<String>>,
    loop_cap: Option<u32>,
) -> Graph {
    let name = name.into();
    let goal = goal.into();
    let profiles = provider_profiles.unwrap_or_default();

    match template {
        GraphTemplate::Direct => direct_template(name, goal, request, &profiles),
        GraphTemplate::PlanImplementVerify => {
            plan_implement_verify_template(name, goal, request, &profiles)
        }
        GraphTemplate::ParallelResearchReduce => {
            parallel_research_reduce_template(name, goal, request, &profiles)
        }
        GraphTemplate::ReviewFixLoop => {
            review_fix_loop_template(name, goal, request, &profiles, loop_cap)
        }
    }
}

fn direct_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
) -> Graph {
    let request = request.unwrap_or_else(|| "complete the requested task".to_owned());
    Graph::new(
        name,
        goal,
        vec![agent_node(
            "request",
            &format!("Act as an assistant and complete this request:\n{request}"),
            profiles.first().cloned(),
            1,
        )],
    )
}

fn plan_implement_verify_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
) -> Graph {
    let request = request.unwrap_or_else(|| "the requested work".to_owned());
    let mut graph = Graph::new(
        name,
        goal,
        vec![
            agent_node(
                "plan",
                &format!(
                    "Plan role: planner. Write a concrete implementation plan for:\n{request}"
                ),
                profiles.first().cloned(),
                1,
            ),
            agent_node(
                "implement",
                &format!("Plan role: implementer. Execute the plan for:\n{request}"),
                profiles.get(1).cloned(),
                1,
            ),
            verify_node(
                "verify",
                vec!["git".into(), "diff".into(), "--check".into()],
            ),
        ],
    );
    graph.spec.edges.push(Edge::data("plan", "implement"));
    graph.spec.edges.push(Edge::data("implement", "verify"));
    graph
}

fn parallel_research_reduce_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
) -> Graph {
    let request = request.unwrap_or_else(|| "the requested problem".to_owned());
    let mut graph = Graph::new(
        name,
        goal,
        vec![
            agent_node(
                "research_one",
                &format!("Research lane 1 for:\n{request}"),
                profiles.first().cloned(),
                2,
            ),
            agent_node(
                "research_two",
                &format!("Research lane 2 for:\n{request}"),
                profiles.get(1).cloned(),
                2,
            ),
            agent_node(
                "research_three",
                &format!("Research lane 3 for:\n{request}"),
                profiles.get(2).cloned(),
                2,
            ),
            reduce_node(
                "synthesize",
                "Combine findings from all lanes into one reconciled recommendation.",
                profiles.get(3).cloned(),
            ),
        ],
    );
    graph.spec.edges = vec![
        Edge::data("research_one", "synthesize"),
        Edge::data("research_two", "synthesize"),
        Edge::data("research_three", "synthesize"),
    ];
    graph.metadata.description = Some("parallel research and synthesis".to_owned());
    graph
}

fn review_fix_loop_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
    loop_cap: Option<u32>,
) -> Graph {
    let request = request.unwrap_or_else(|| "review the artifact".to_owned());
    let max_iterations = loop_cap.unwrap_or(4);
    let review_profile = profiles.first().cloned();
    let fix_profile = profiles.get(1).cloned();

    let nested_nodes = vec![
        agent_node(
            "review",
            &format!(
                "Reviewer role: inspect output for defects and risks, then request follow-up fixes for:\n{request}"
            ),
            review_profile,
            1,
        ),
        agent_node(
            "fix",
            &format!("Implement requested fixes for:\n{request}"),
            fix_profile,
            1,
        ),
        verify_node("judge", vec!["git".into(), "diff".into(), "--check".into()]),
    ];
    let mut nested_graph = Graph::new(
        "review-fix-iteration",
        "single review-fix iteration",
        nested_nodes,
    );
    nested_graph.spec.edges = vec![Edge::data("review", "fix"), Edge::data("fix", "judge")];
    nested_graph.metadata.description = Some("bounded review/fix iteration".to_owned());

    let loop_node = Node {
        id: "review_fix_loop".to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: gloop_core::RetryPolicy::default(),
        timeout_seconds: None,
        workspace: gloop_core::WorkspaceSpec::default(),
        context: gloop_core::ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Loop {
            graph: Box::new(nested_graph),
            until: LoopCondition {
                node: "judge".to_owned(),
                status: gloop_core::NodeStatus::Succeeded,
                output_contains: None,
                json_pointer: None,
                equals: None,
            },
            max_iterations,
            stagnation_after: 2,
        },
    };

    let publish = command_node(
        "publish",
        vec!["printf".into(), "ready for release\n".into()],
    );
    let mut graph = Graph::new(name, goal, vec![loop_node, publish]);
    graph.spec.edges.push(Edge {
        from: "review_fix_loop".to_owned(),
        to: "publish".to_owned(),
        kind: EdgeKind::Control,
        when: None,
    });
    graph
}

#[cfg(test)]
pub fn graph_from_yaml_bytes(contents: impl AsRef<str>) -> Result<Graph> {
    Graph::from_yaml_str(contents.as_ref())
        .map_err(|error| anyhow!("failed to parse graph YAML: {error}"))
}

pub fn interactive_graph() -> Result<Graph> {
    interactive_graph_with_seed(None, None)
}

pub fn interactive_graph_with_seed(name: Option<&str>, goal: Option<&str>) -> Result<Graph> {
    let theme = ColorfulTheme::default();
    interactive_graph_inner(&theme, 0, name, goal)
}

pub fn interactive_template_init() -> Result<Graph> {
    let theme = ColorfulTheme::default();
    let template_name = prompt_template_name(&theme, None)?;
    let description = prompt_optional_description(&theme)?;
    let base = prompt_template_base(&theme)?;
    let mut graph = match base {
        TemplateBase::Builtin(template) => {
            let knobs = prompt_template_knobs(&theme, template)?;
            template_graph(
                &template_name,
                DEFAULT_TEMPLATE_GOAL,
                template,
                knobs.request,
                Some(knobs.provider_profiles),
                knobs.loop_cap,
            )
        }
        TemplateBase::Custom => interactive_graph_with_seed(Some(&template_name), Some(DEFAULT_TEMPLATE_GOAL))?,
    };
    graph.metadata.name = template_name;
    if let Some(description) = description {
        graph.metadata.description = Some(description);
    }
    Ok(graph)
}

#[derive(Debug, Clone, Copy)]
enum TemplateBase {
    Builtin(GraphTemplate),
    Custom,
}

fn prompt_template_name(theme: &ColorfulTheme, default: Option<&str>) -> Result<String> {
    loop {
        let mut input = Input::with_theme(theme)
            .with_prompt("Template name (kebab-case, max 64 characters)");
        if let Some(default) = default {
            input = input.default(default.to_owned());
        }
        let candidate: String = input.interact_text()?;
        if let Err(error) = validate_init_template_name(&candidate) {
            eprintln!("{error}");
            continue;
        }
        return Ok(candidate);
    }
}

fn prompt_optional_description(theme: &ColorfulTheme) -> Result<Option<String>> {
    let value: String = Input::with_theme(theme)
        .with_prompt("Optional template description")
        .allow_empty(true)
        .interact_text()?;
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.trim().to_owned()))
    }
}

fn prompt_template_base(theme: &ColorfulTheme) -> Result<TemplateBase> {
    let labels = [
        "direct",
        "plan-implement-verify",
        "parallel-research-reduce",
        "review-fix-loop",
        "custom",
    ];
    let selected = Select::with_theme(theme)
        .with_prompt("Base template")
        .items(labels)
        .default(0)
        .interact()?;

    match selected {
        0 => Ok(TemplateBase::Builtin(GraphTemplate::Direct)),
        1 => Ok(TemplateBase::Builtin(GraphTemplate::PlanImplementVerify)),
        2 => Ok(TemplateBase::Builtin(GraphTemplate::ParallelResearchReduce)),
        3 => Ok(TemplateBase::Builtin(GraphTemplate::ReviewFixLoop)),
        4 => Ok(TemplateBase::Custom),
        _ => unreachable!("invalid base template selection"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TemplateKnobs {
    request: Option<String>,
    provider_profiles: Vec<String>,
    loop_cap: Option<u32>,
}

fn prompt_template_knobs(theme: &ColorfulTheme, template: GraphTemplate) -> Result<TemplateKnobs> {
    let request = prompt_optional_text(theme, "Optional request text")?;
    let provider_profiles = prompt_csv(theme, "Optional provider profiles (comma-separated)")?;
    let loop_cap = if matches!(template, GraphTemplate::ReviewFixLoop) {
        prompt_optional_number(
            theme,
            "Optional loop iteration cap",
            1u32,
            MAX_LOOP_ITERATIONS,
        )?
    } else {
        None
    };

    Ok(TemplateKnobs {
        request,
        provider_profiles,
        loop_cap,
    })
}

fn prompt_optional_text(theme: &ColorfulTheme, prompt: &str) -> Result<Option<String>> {
    let value: String = Input::with_theme(theme)
        .with_prompt(prompt)
        .allow_empty(true)
        .interact_text()?;
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn interactive_graph_inner(
    theme: &ColorfulTheme,
    depth: usize,
    name_seed: Option<&str>,
    goal_seed: Option<&str>,
) -> Result<Graph> {
    let name = prompt_identifier(theme, "Graph name", name_seed, &[])?;
    let goal = prompt_nonempty_text(theme, "Graph goal", goal_seed)?;
    let settings = prompt_graph_settings(theme)?;

    let mut graph = Graph::new(name, goal, Vec::new());
    apply_graph_settings(&mut graph, settings);

    loop {
        let actions = wizard_actions(depth);
        let labels: Vec<&str> = actions.iter().map(|(label, _)| *label).collect();
        let selected_action = Select::with_theme(theme)
            .with_prompt("Choose an action")
            .items(&labels)
            .default(actions.len() - 1)
            .interact()?;
        let action = actions[selected_action].1;

        if matches!(action, WizardAction::Finish) {
            if graph.spec.nodes.is_empty() {
                eprintln!("A graph must contain at least one node.");
                continue;
            }
            break;
        }

        let existing_ids: Vec<&str> = graph
            .spec
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect();
        let id = prompt_identifier(theme, "Node ID", None, &existing_ids)?;
        let mut node = build_node_for_action(theme, action, &id, depth)?;

        let common_settings = prompt_common_node_settings(theme, &node, &graph.spec.nodes)?;
        apply_common_node_settings(&mut node, common_settings)?;
        if let Some(node_output) = node_output_mut(&mut node) {
            let output = prompt_output_spec(theme)?;
            *node_output = output;
        }

        let inherit_source = match &node.workspace {
            WorkspaceSpec::Inherit { node } => Some(node.as_str()),
            _ => None,
        };
        let dependency_ids: Vec<&str> = graph
            .spec
            .nodes
            .iter()
            .map(|candidate| candidate.id.as_str())
            .filter(|candidate| Some(*candidate) != inherit_source)
            .collect();
        let selected = if dependency_ids.is_empty() {
            Vec::new()
        } else {
            MultiSelect::with_theme(theme)
                .with_prompt("Pick dependency edges")
                .items(&dependency_ids)
                .interact()?
        };
        let dependency_drafts = if selected.is_empty() {
            Vec::new()
        } else {
            select_dependency_drafts(theme, &dependency_ids, &selected, &id)?
        };

        let mut draft = graph.clone();
        draft.spec.edges.extend(build_dependency_edges(
            &dependency_ids,
            &id,
            &selected,
            &dependency_drafts,
        )?);
        ensure_workspace_inheritance_edge(&node, &mut draft.spec.edges)?;
        draft.spec.nodes.push(node);
        validate_graph_errors(&draft)?;
        graph = draft;
        eprintln!("Added node {id}.");
    }

    validate_graph_errors(&graph)?;
    Ok(graph)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardAction {
    Agent,
    Command,
    Verify,
    Gate,
    Reduce,
    Synthesize,
    Loop,
    Subgraph,
    Finish,
}

fn wizard_actions(depth: usize) -> Vec<(&'static str, WizardAction)> {
    let mut actions = vec![
        ("Add agent node", WizardAction::Agent),
        ("Add command node", WizardAction::Command),
        ("Add verify node", WizardAction::Verify),
        ("Add gate node", WizardAction::Gate),
        ("Add reduce node", WizardAction::Reduce),
        ("Add synthesize node", WizardAction::Synthesize),
    ];
    if depth < INTERACTIVE_NESTING_LIMIT {
        actions.push(("Add bounded loop node", WizardAction::Loop));
        actions.push(("Add subgraph node", WizardAction::Subgraph));
    }
    actions.push(("Finish", WizardAction::Finish));
    actions
}

fn build_node_for_action(
    theme: &ColorfulTheme,
    action: WizardAction,
    id: &str,
    depth: usize,
) -> Result<Node> {
    match action {
        WizardAction::Agent => build_agent_node(theme, id),
        WizardAction::Command => build_command_node(theme, id),
        WizardAction::Verify => build_verify_node(theme, id),
        WizardAction::Gate => build_gate_node(theme, id),
        WizardAction::Reduce => build_reduce_node(theme, id),
        WizardAction::Synthesize => build_synthesize_node(theme, id),
        WizardAction::Loop => build_loop_node(theme, id, depth),
        WizardAction::Subgraph => build_subgraph_node(theme, id, depth),
        WizardAction::Finish => Err(anyhow!("finish is not a node action")),
    }
}

fn prompt_identifier(
    theme: &ColorfulTheme,
    prompt: &str,
    default: Option<&str>,
    existing: &[&str],
) -> Result<String> {
    loop {
        let mut input = Input::with_theme(theme).with_prompt(format!(
            "{prompt} (lowercase, digits, -/_; must start with a letter)"
        ));
        if let Some(default) = default {
            input = input.default(default.to_owned());
        }
        let candidate: String = input.interact_text()?;
        if !is_valid_identifier(&candidate) {
            eprintln!(
                "Invalid identifier. Use lowercase letters, digits, '-' or '_' and start with a letter."
            );
            continue;
        }
        if existing.contains(&candidate.as_str()) {
            eprintln!("That identifier is already in use.");
            continue;
        }
        return Ok(candidate);
    }
}

fn prompt_nonempty_text(
    theme: &ColorfulTheme,
    prompt: &str,
    default: Option<&str>,
) -> Result<String> {
    loop {
        let mut input = Input::with_theme(theme).with_prompt(prompt);
        if let Some(default) = default {
            input = input.default(default.to_owned());
        }
        let value: String = input.interact_text()?;
        if value.trim().is_empty() {
            eprintln!("{prompt} cannot be empty.");
            continue;
        }
        return Ok(value);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphSettings {
    max_parallel: usize,
    failure: FailurePolicy,
    budgets: RunBudgets,
}

fn prompt_graph_settings(theme: &ColorfulTheme) -> Result<GraphSettings> {
    let max_parallel =
        prompt_bounded_number(theme, "Maximum parallel nodes", 4usize, 1, MAX_PARALLELISM)?;
    let failure = match Select::with_theme(theme)
        .with_prompt("Failure policy")
        .items(["fail_fast", "continue"])
        .default(0)
        .interact()?
    {
        0 => FailurePolicy::FailFast,
        1 => FailurePolicy::Continue,
        _ => unreachable!("invalid failure policy selection"),
    };
    let wall_time_seconds = prompt_optional_number(
        theme,
        "Optional wall-time budget in seconds",
        0u64,
        MAX_DURATION_SECONDS,
    )?;
    let model_calls = prompt_optional_number(theme, "Optional model-call budget", 0u32, u32::MAX)?;

    Ok(GraphSettings {
        max_parallel,
        failure,
        budgets: RunBudgets {
            wall_time_seconds,
            model_calls,
        },
    })
}

fn apply_graph_settings(graph: &mut Graph, settings: GraphSettings) {
    graph.spec.policies.max_parallel = settings.max_parallel;
    graph.spec.policies.failure = settings.failure;
    graph.spec.budgets = settings.budgets;
}

fn prompt_bounded_number<T>(
    theme: &ColorfulTheme,
    prompt: &str,
    default: T,
    minimum: T,
    maximum: T,
) -> Result<T>
where
    T: Copy + PartialOrd + std::fmt::Display + std::str::FromStr,
    T::Err: std::fmt::Display,
{
    loop {
        let value: T = Input::with_theme(theme)
            .with_prompt(prompt)
            .default(default)
            .interact_text()?;
        if (minimum..=maximum).contains(&value) {
            return Ok(value);
        }
        eprintln!("Value must be between {minimum} and {maximum}.");
    }
}

fn prompt_optional_number<T>(
    theme: &ColorfulTheme,
    prompt: &str,
    minimum: T,
    maximum: T,
) -> Result<Option<T>>
where
    T: Copy + PartialOrd + std::fmt::Display + std::str::FromStr,
    T::Err: std::fmt::Display,
{
    loop {
        let value: String = Input::with_theme(theme)
            .with_prompt(format!("{prompt} (blank for none)"))
            .allow_empty(true)
            .interact_text()?;
        if value.trim().is_empty() {
            return Ok(None);
        }
        match value.trim().parse::<T>() {
            Ok(value) if (minimum..=maximum).contains(&value) => return Ok(Some(value)),
            Ok(_) => eprintln!("Value must be between {minimum} and {maximum}."),
            Err(error) => eprintln!("Invalid number: {error}"),
        }
    }
}

fn validate_graph_errors(graph: &Graph) -> Result<()> {
    for issue in graph.validate() {
        if issue.severity == IssueSeverity::Error {
            return Err(anyhow!(
                "graph validation failed [{}] at {}: {}",
                issue.code,
                issue.path,
                issue.message
            ));
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct DependencyDraft {
    kind: EdgeKind,
    when: Option<EdgeCondition>,
}

fn select_dependency_drafts(
    theme: &ColorfulTheme,
    dependency_ids: &[&str],
    selected_indices: &[usize],
    to_node: &str,
) -> Result<Vec<DependencyDraft>> {
    let kind_options = ["data", "control", "resource", "failure", "conditional"];
    let mut results = Vec::with_capacity(selected_indices.len());

    for &index in selected_indices {
        let dependency_id = dependency_ids
            .get(index)
            .ok_or_else(|| anyhow!("invalid dependency selection"))?;
        let kind = match Select::with_theme(theme)
            .with_prompt(format!(
                "Dependency edge kind from '{dependency_id}' to '{to_node}'"
            ))
            .items(kind_options)
            .default(0)
            .interact()?
        {
            0 => EdgeKind::Data,
            1 => EdgeKind::Control,
            2 => EdgeKind::Resource,
            3 => EdgeKind::Failure,
            4 => EdgeKind::Conditional,
            _ => unreachable!("invalid edge kind selection"),
        };

        let when = if matches!(kind, EdgeKind::Conditional) {
            Some(select_conditional_when(theme, dependency_id, to_node)?)
        } else {
            None
        };

        results.push(DependencyDraft { kind, when });
    }

    Ok(results)
}

fn select_terminal_status(theme: &ColorfulTheme, from: &str, to_node: &str) -> Result<NodeStatus> {
    let options = ["succeeded", "failed", "skipped", "blocked", "cancelled"];
    let status = Select::with_theme(theme)
        .with_prompt(format!(
            "Terminal status condition for '{from}' -> '{to_node}'"
        ))
        .items(options)
        .default(0)
        .interact()?;

    Ok(match status {
        0 => NodeStatus::Succeeded,
        1 => NodeStatus::Failed,
        2 => NodeStatus::Skipped,
        3 => NodeStatus::Blocked,
        4 => NodeStatus::Cancelled,
        _ => unreachable!("invalid status selection"),
    })
}

fn select_loop_terminal_status(
    theme: &ColorfulTheme,
    from: &str,
    loop_node: &str,
) -> Result<NodeStatus> {
    let options = ["succeeded", "skipped"];
    let status = Select::with_theme(theme)
        .with_prompt(format!(
            "Successful terminal status for '{from}' -> loop '{loop_node}'"
        ))
        .items(options)
        .default(0)
        .interact()?;

    Ok(match status {
        0 => NodeStatus::Succeeded,
        1 => NodeStatus::Skipped,
        _ => unreachable!("invalid loop terminal status selection"),
    })
}

fn select_conditional_when(
    theme: &ColorfulTheme,
    dependency_id: &str,
    to_node: &str,
) -> Result<EdgeCondition> {
    let status = select_terminal_status(theme, dependency_id, to_node)?;
    let (output_contains, json_pointer, equals) = prompt_condition_filters(theme)?;

    Ok(EdgeCondition {
        status: Some(status),
        output_contains,
        json_pointer,
        equals,
    })
}

fn prompt_condition_filters(
    theme: &ColorfulTheme,
) -> Result<(Option<String>, Option<String>, Option<Value>)> {
    let output_contains = if Confirm::with_theme(theme)
        .with_prompt("Add output_contains condition?")
        .default(false)
        .interact()?
    {
        Some(prompt_nonempty_text(theme, "Enter output_contains", None)?)
    } else {
        None
    };

    let json_pointer = if Confirm::with_theme(theme)
        .with_prompt("Add json_pointer + equals condition?")
        .default(false)
        .interact()?
    {
        let pointer = loop {
            let pointer: String = Input::with_theme(theme)
                .with_prompt("JSON Pointer (blank selects the document root)")
                .allow_empty(true)
                .interact_text()?;
            if is_valid_json_pointer(&pointer) {
                break pointer;
            }
            eprintln!(
                "Invalid RFC 6901 JSON Pointer. Use '' or a '/'-prefixed pointer with ~0/~1 escapes."
            );
        };
        let equals = loop {
            let literal: String = Input::with_theme(theme)
                .with_prompt("Expected JSON literal (for strings include quotes)")
                .allow_empty(true)
                .interact_text()?;
            match parse_json_literal(&literal) {
                Ok(value) => break value,
                Err(error) => eprintln!("Invalid JSON literal: {error}"),
            }
        };
        Some((pointer, equals))
    } else {
        None
    };

    Ok((
        output_contains,
        json_pointer.as_ref().map(|value| value.0.clone()),
        json_pointer.as_ref().map(|value| value.1.clone()),
    ))
}

fn parse_json_literal(input: &str) -> Result<Value> {
    serde_json::from_str(input).context("expected one JSON value")
}

fn is_valid_json_pointer(pointer: &str) -> bool {
    if pointer.is_empty() {
        return true;
    }
    if !pointer.starts_with('/') {
        return false;
    }

    let bytes = pointer.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            let Some(escaped) = bytes.get(index + 1) else {
                return false;
            };
            if !matches!(escaped, b'0' | b'1') {
                return false;
            }
            index += 1;
        }
        index += 1;
    }
    true
}

fn build_dependency_edges(
    dependency_ids: &[&str],
    node_id: &str,
    selected_indices: &[usize],
    drafts: &[DependencyDraft],
) -> Result<Vec<Edge>> {
    if selected_indices.len() != drafts.len() {
        return Err(anyhow!("dependency draft mismatch"));
    }

    let mut edges = Vec::with_capacity(selected_indices.len());
    for (&index, draft) in std::iter::zip(selected_indices, drafts.iter()) {
        let from = dependency_ids
            .get(index)
            .ok_or_else(|| anyhow!("invalid dependency index"))?;

        if let EdgeKind::Conditional = draft.kind {
            let condition = draft
                .when
                .as_ref()
                .ok_or_else(|| anyhow!("conditional edges require a condition"))?;

            if !condition.status.is_some_and(NodeStatus::is_terminal) {
                return Err(anyhow!("conditional edges require a terminal status"));
            }

            if condition.equals.is_some() != condition.json_pointer.is_some() {
                return Err(anyhow!(
                    "json_pointer and equals must be specified together"
                ));
            }
            if let Some(pointer) = condition.json_pointer.as_deref()
                && !is_valid_json_pointer(pointer)
            {
                return Err(anyhow!("invalid json_pointer format"));
            }
        } else if draft.when.is_some() {
            return Err(anyhow!("condition can only be used with conditional edges"));
        }

        edges.push(Edge {
            from: (*from).to_owned(),
            to: node_id.to_owned(),
            kind: draft.kind,
            when: draft.when.clone(),
        });
    }

    Ok(edges)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommonNodeSettings {
    resources: Vec<String>,
    retry: RetryPolicy,
    timeout_seconds: Option<u64>,
    workspace: WorkspaceSpec,
    context: ContextSpec,
}

fn prompt_common_node_settings(
    theme: &ColorfulTheme,
    node: &Node,
    prior_nodes: &[Node],
) -> Result<CommonNodeSettings> {
    let resources = prompt_csv(theme, "Resources (comma-separated)")?;
    let max_attempts =
        prompt_bounded_number(theme, "Maximum retry attempts", 1u32, 1, MAX_RETRY_ATTEMPTS)?;
    let backoff_seconds = prompt_bounded_number(
        theme,
        "Retry backoff in seconds",
        0u64,
        0,
        MAX_DURATION_SECONDS,
    )?;
    let rebind_profiles = if node_supports_profiles(node) && max_attempts > 1 {
        let max_rebind_profiles = usize::try_from(max_attempts - 1)
            .context("retry attempt count does not fit this platform")?;
        loop {
            let profiles = prompt_csv(
                theme,
                "Retry rebind profiles in attempt order (comma-separated)",
            )?;
            if profiles.len() <= max_rebind_profiles {
                break profiles;
            }
            eprintln!(
                "At most {} rebind profile(s) can be used with {max_attempts} attempts.",
                max_attempts - 1
            );
        }
    } else {
        Vec::new()
    };
    let timeout_seconds = prompt_optional_number(
        theme,
        "Optional node timeout in seconds",
        0u64,
        MAX_DURATION_SECONDS,
    )?;
    let workspace = prompt_workspace(theme, node, prior_nodes)?;
    let include_dependencies = Confirm::with_theme(theme)
        .with_prompt("Include dependency outputs in context?")
        .default(true)
        .interact()?;
    let files = prompt_csv(theme, "Additional context files (comma-separated)")?
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let max_bytes = prompt_bounded_number(
        theme,
        "Maximum context bytes",
        256 * 1024,
        0,
        MAX_CONTEXT_BYTES,
    )?;

    Ok(CommonNodeSettings {
        resources,
        retry: RetryPolicy {
            max_attempts,
            backoff_seconds,
            rebind_profiles,
        },
        timeout_seconds,
        workspace,
        context: ContextSpec {
            include_dependencies,
            files,
            max_bytes,
        },
    })
}

fn prompt_csv(theme: &ColorfulTheme, prompt: &str) -> Result<Vec<String>> {
    let value: String = Input::with_theme(theme)
        .with_prompt(prompt)
        .allow_empty(true)
        .interact_text()?;
    Ok(parse_csv(&value))
}

fn parse_csv(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    for item in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let item = item.to_owned();
        if !values.contains(&item) {
            values.push(item);
        }
    }
    values
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceChoice {
    Current,
    Worktree,
    Inherit,
}

fn prompt_workspace(
    theme: &ColorfulTheme,
    node: &Node,
    prior_nodes: &[Node],
) -> Result<WorkspaceSpec> {
    let mut choices = vec![("current", WorkspaceChoice::Current)];
    if node.fan_out() == 1 {
        choices.push(("worktree", WorkspaceChoice::Worktree));
    } else {
        eprintln!("Worktree is unavailable for this agent because fan_out must be 1.");
    }
    if !prior_nodes.is_empty() {
        choices.push(("inherit a prior node's workspace", WorkspaceChoice::Inherit));
    }
    let labels: Vec<&str> = choices.iter().map(|(label, _)| *label).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Workspace mode")
        .items(&labels)
        .default(0)
        .interact()?;

    match choices[selected].1 {
        WorkspaceChoice::Current => Ok(WorkspaceSpec::Current),
        WorkspaceChoice::Worktree => {
            let base: String = Input::with_theme(theme)
                .with_prompt("Optional worktree base revision (blank uses captured run base)")
                .allow_empty(true)
                .interact_text()?;
            let auto_commit = Confirm::with_theme(theme)
                .with_prompt("Auto-commit successful changes in the retained worktree?")
                .default(false)
                .interact()?;
            Ok(WorkspaceSpec::Worktree {
                base: (!base.trim().is_empty()).then(|| base.trim().to_owned()),
                auto_commit,
            })
        }
        WorkspaceChoice::Inherit => {
            let ids: Vec<&str> = prior_nodes.iter().map(|prior| prior.id.as_str()).collect();
            let selected = Select::with_theme(theme)
                .with_prompt("Prior node whose workspace should be inherited")
                .items(&ids)
                .default(ids.len() - 1)
                .interact()?;
            eprintln!(
                "A direct success/default data edge from '{}' will be added automatically.",
                ids[selected]
            );
            Ok(WorkspaceSpec::Inherit {
                node: ids[selected].to_owned(),
            })
        }
    }
}

fn node_supports_profiles(node: &Node) -> bool {
    matches!(
        &node.kind,
        NodeKind::Agent { .. } | NodeKind::Reduce { .. } | NodeKind::Synthesize { .. }
    )
}

fn apply_common_node_settings(node: &mut Node, settings: CommonNodeSettings) -> Result<()> {
    if matches!(settings.workspace, WorkspaceSpec::Readonly) {
        return Err(anyhow!(
            "readonly workspaces are not offered because gloop cannot enforce them"
        ));
    }
    if matches!(settings.workspace, WorkspaceSpec::Worktree { .. }) && node.fan_out() != 1 {
        return Err(anyhow!("worktree agent nodes require fan_out = 1"));
    }

    node.resources = settings.resources;
    node.retry = settings.retry;
    node.timeout_seconds = settings.timeout_seconds;
    node.workspace = settings.workspace;
    node.context = settings.context;
    Ok(())
}

fn ensure_workspace_inheritance_edge(node: &Node, edges: &mut Vec<Edge>) -> Result<()> {
    let WorkspaceSpec::Inherit { node: source } = &node.workspace else {
        return Ok(());
    };

    let existing: Vec<&Edge> = edges
        .iter()
        .filter(|edge| edge.from == *source && edge.to == node.id)
        .collect();
    if existing.is_empty() {
        edges.push(Edge::data(source, &node.id));
        return Ok(());
    }
    if existing.iter().any(|edge| edge.kind == EdgeKind::Failure) {
        return Err(anyhow!(
            "workspace inheritance cannot use a failure edge from '{source}'"
        ));
    }
    if existing.iter().any(|edge| {
        edge.when
            .as_ref()
            .is_some_and(|when| when.status != Some(NodeStatus::Succeeded))
    }) {
        return Err(anyhow!(
            "workspace inheritance requires a default or succeeded edge from '{source}'"
        ));
    }
    Ok(())
}

fn prompt_output_spec(theme: &ColorfulTheme) -> Result<OutputSpec> {
    let format = match Select::with_theme(theme)
        .with_prompt("Output format")
        .items(["text", "json"])
        .default(0)
        .interact()?
    {
        0 => OutputFormat::Text,
        1 => OutputFormat::Json,
        _ => unreachable!("invalid output format selection"),
    };
    let max_bytes = prompt_bounded_number(
        theme,
        "Maximum output bytes",
        1024 * 1024,
        0,
        MAX_OUTPUT_BYTES,
    )?;
    Ok(OutputSpec {
        format,
        schema: None,
        inline_schema: None,
        max_bytes,
    })
}

fn node_output_mut(node: &mut Node) -> Option<&mut OutputSpec> {
    match &mut node.kind {
        NodeKind::Agent { output, .. }
        | NodeKind::Command { output, .. }
        | NodeKind::Reduce { output, .. }
        | NodeKind::Verify { output, .. }
        | NodeKind::Synthesize { output, .. } => Some(output),
        NodeKind::Gate { .. } | NodeKind::Loop { .. } | NodeKind::Subgraph { .. } => None,
    }
}

fn prompt_profile_model(
    theme: &ColorfulTheme,
    id: &str,
    kind: &str,
) -> Result<(Option<String>, Option<String>)> {
    let profile: String = Input::with_theme(theme)
        .with_prompt(format!("Optional {kind} profile id for '{id}'"))
        .allow_empty(true)
        .interact_text()?;
    let profile = if profile.trim().is_empty() {
        None
    } else {
        Some(profile.trim().to_owned())
    };

    let model: String = Input::with_theme(theme)
        .with_prompt(format!("Optional {kind} model id for '{id}'"))
        .allow_empty(true)
        .interact_text()?;
    let model = if model.trim().is_empty() {
        None
    } else {
        Some(model.trim().to_owned())
    };

    Ok((profile, model))
}

fn build_agent_node(theme: &ColorfulTheme, id: &str) -> Result<Node> {
    let prompt = prompt_nonempty_text(theme, &format!("Prompt for agent node '{id}'"), None)?;
    let fan_out = prompt_bounded_number(theme, "Fan out", 1usize, 1, MAX_FAN_OUT)?;
    let (profile, model) = prompt_profile_model(theme, id, "agent")?;

    let mut node = agent_node(id, &prompt, profile, fan_out);
    if let NodeKind::Agent {
        model: node_model, ..
    } = &mut node.kind
    {
        *node_model = model;
    }
    Ok(node)
}

fn build_reduce_node(theme: &ColorfulTheme, id: &str) -> Result<Node> {
    let prompt = prompt_nonempty_text(theme, &format!("Prompt for reduce node '{id}'"), None)?;
    let (profile, model) = prompt_profile_model(theme, id, "reduce")?;
    let mut node = reduce_node(id, &prompt, profile);
    if let NodeKind::Reduce {
        model: node_model, ..
    } = &mut node.kind
    {
        *node_model = model;
    }
    Ok(node)
}

fn build_synthesize_node(theme: &ColorfulTheme, id: &str) -> Result<Node> {
    let prompt = prompt_nonempty_text(theme, &format!("Prompt for synthesize node '{id}'"), None)?;
    let (profile, model) = prompt_profile_model(theme, id, "synthesize")?;
    let mut node = synthesize_node(id, &prompt, profile);
    if let NodeKind::Synthesize {
        model: node_model, ..
    } = &mut node.kind
    {
        *node_model = model;
    }
    Ok(node)
}

fn build_command_node(theme: &ColorfulTheme, id: &str) -> Result<Node> {
    Ok(command_node(id, prompt_command_argv(theme, id)?))
}

fn build_verify_node(theme: &ColorfulTheme, id: &str) -> Result<Node> {
    Ok(verify_node(id, prompt_command_argv(theme, id)?))
}

fn prompt_command_argv(theme: &ColorfulTheme, id: &str) -> Result<Vec<String>> {
    let executable = prompt_nonempty_text(theme, &format!("Executable for '{id}'"), None)?;

    loop {
        let args_text: String = Input::with_theme(theme)
            .with_prompt("Arguments (quoted tokens supported, e.g. \"a b\" '--flag')")
            .allow_empty(true)
            .interact_text()?;
        let mut argv = vec![executable.trim().to_owned()];
        if args_text.trim().is_empty() {
            return Ok(argv);
        }
        match parse_argv(&args_text) {
            Ok(arguments) => {
                argv.extend(arguments);
                return Ok(argv);
            }
            Err(error) => eprintln!("Invalid arguments: {error}. Please try again."),
        }
    }
}

fn build_gate_node(theme: &ColorfulTheme, id: &str) -> Result<Node> {
    let message = prompt_nonempty_text(theme, &format!("Approval prompt for '{id}'"), None)?;
    Ok(gate_node(id, &message))
}

fn build_loop_node(theme: &ColorfulTheme, id: &str, depth: usize) -> Result<Node> {
    let nested_name = format!("{id}-body");
    let nested_goal = format!("Bounded iteration body for {id}");
    let nested = interactive_graph_inner(theme, depth + 1, Some(&nested_name), Some(&nested_goal))?;
    let nested_ids: Vec<&str> = nested
        .spec
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Nested node used as the loop terminal condition")
        .items(&nested_ids)
        .default(nested_ids.len() - 1)
        .interact()?;
    let condition_node = nested_ids[selected].to_owned();
    let status = select_loop_terminal_status(theme, &condition_node, id)?;
    let (output_contains, json_pointer, equals) = prompt_condition_filters(theme)?;
    let max_iterations = prompt_bounded_number(
        theme,
        "Maximum loop iterations",
        4u32,
        1,
        MAX_LOOP_ITERATIONS,
    )?;
    let stagnation_after = prompt_bounded_number(
        theme,
        "Stop after this many stagnant iterations",
        2u32.min(max_iterations),
        1,
        max_iterations,
    )?;

    Ok(loop_node(
        id,
        nested,
        LoopCondition {
            node: condition_node,
            status,
            output_contains,
            json_pointer,
            equals,
        },
        max_iterations,
        stagnation_after,
    ))
}

fn build_subgraph_node(theme: &ColorfulTheme, id: &str, depth: usize) -> Result<Node> {
    let nested_name = format!("{id}-graph");
    let nested_goal = format!("Nested workflow for {id}");
    let nested = interactive_graph_inner(theme, depth + 1, Some(&nested_name), Some(&nested_goal))?;
    Ok(subgraph_node(id, nested))
}

fn loop_node(
    id: &str,
    graph: Graph,
    until: LoopCondition,
    max_iterations: u32,
    stagnation_after: u32,
) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: RetryPolicy::default(),
        timeout_seconds: None,
        workspace: WorkspaceSpec::Current,
        context: ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Loop {
            graph: Box::new(graph),
            until,
            max_iterations,
            stagnation_after,
        },
    }
}

fn subgraph_node(id: &str, graph: Graph) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: RetryPolicy::default(),
        timeout_seconds: None,
        workspace: WorkspaceSpec::Current,
        context: ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Subgraph {
            graph: Box::new(graph),
        },
    }
}

fn parse_argv(input: &str) -> Result<Vec<String>> {
    let mut args = Vec::new();
    let mut token = String::new();
    let mut token_started = false;
    let mut state = QuoteState::Out;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match state {
            QuoteState::Out => match ch {
                '\'' => {
                    state = QuoteState::Single;
                    token_started = true;
                }
                '"' => {
                    state = QuoteState::Double;
                    token_started = true;
                }
                '\\' => {
                    let escaped = chars
                        .next()
                        .ok_or_else(|| anyhow!("unterminated escape sequence"))?;
                    token.push(escaped);
                    token_started = true;
                }
                c if c.is_whitespace() => {
                    if token_started {
                        args.push(std::mem::take(&mut token));
                        token_started = false;
                    }
                }
                c => {
                    token.push(c);
                    token_started = true;
                }
            },
            QuoteState::Single => match ch {
                '\'' => state = QuoteState::Out,
                c => token.push(c),
            },
            QuoteState::Double => match ch {
                '"' => state = QuoteState::Out,
                '\\' => {
                    let next = chars
                        .next()
                        .ok_or_else(|| anyhow!("unterminated escape sequence"))?;
                    match next {
                        'n' => token.push('\n'),
                        't' => token.push('\t'),
                        '"' => token.push('"'),
                        '\\' => token.push('\\'),
                        _ => token.push(next),
                    }
                    token_started = true;
                }
                c => token.push(c),
            },
        }
    }

    if !matches!(state, QuoteState::Out) {
        return Err(anyhow!("unterminated quote in command input"));
    }
    if token_started {
        args.push(token);
    }
    if args.is_empty() {
        return Err(anyhow!("command cannot be empty"));
    }

    Ok(args)
}

enum QuoteState {
    Out,
    Single,
    Double,
}

fn agent_node(id: &str, prompt: &str, profile: Option<String>, fan_out: usize) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: gloop_core::RetryPolicy::default(),
        timeout_seconds: None,
        workspace: gloop_core::WorkspaceSpec::default(),
        context: gloop_core::ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Agent {
            prompt: PromptSpec::Inline(prompt.to_owned()),
            profile,
            model: None,
            fan_out,
            output: OutputSpec::default(),
        },
    }
}

fn command_node(id: &str, argv: Vec<String>) -> Node {
    Node::command(id, argv)
}

fn verify_node(id: &str, argv: Vec<String>) -> Node {
    let mut base = command_node(id, argv);
    let (argv, env) = match &mut base.kind {
        NodeKind::Command { argv, env, .. } => (std::mem::take(argv), std::mem::take(env)),
        _ => unreachable!("command_node must return a command node"),
    };

    base.kind = NodeKind::Verify {
        argv,
        env,
        output: OutputSpec::default(),
    };
    base
}

fn reduce_node(id: &str, prompt: &str, profile: Option<String>) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: gloop_core::RetryPolicy::default(),
        timeout_seconds: None,
        workspace: gloop_core::WorkspaceSpec::default(),
        context: gloop_core::ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Reduce {
            prompt: PromptSpec::Inline(prompt.to_owned()),
            profile,
            model: None,
            output: OutputSpec::default(),
        },
    }
}

fn synthesize_node(id: &str, prompt: &str, profile: Option<String>) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: gloop_core::RetryPolicy::default(),
        timeout_seconds: None,
        workspace: gloop_core::WorkspaceSpec::default(),
        context: gloop_core::ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Synthesize {
            prompt: PromptSpec::Inline(prompt.to_owned()),
            profile,
            model: None,
            output: OutputSpec::default(),
        },
    }
}

fn gate_node(id: &str, message: &str) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: gloop_core::RetryPolicy::default(),
        timeout_seconds: None,
        workspace: gloop_core::WorkspaceSpec::default(),
        context: gloop_core::ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Gate {
            message: message.to_owned(),
            default: gloop_core::GateDefault::Reject,
        },
    }
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
        && !value.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{
        CommonNodeSettings, DependencyDraft, Edge, EdgeCondition, EdgeKind, Graph, GraphSettings,
        GraphTemplate, NodeKind, NodeStatus, WizardAction, apply_common_node_settings,
        apply_graph_settings, build_dependency_edges, ensure_workspace_inheritance_edge,
        graph_from_yaml_bytes, is_valid_json_pointer, loop_node, node_output_mut, parse_csv,
        parse_json_literal, subgraph_node, template_graph, wizard_actions,
    };
    use super::{parse_argv, synthesize_node};
    use gloop_core::{
        ContextSpec, FailurePolicy, IssueSeverity, LoopCondition, OutputFormat, RetryPolicy,
        RunBudgets, WorkspaceSpec,
    };
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn parse_argv_with_quotes() {
        let parsed = parse_argv(r#"printf "hello world" 'a b' test"#).expect("parse");
        assert_eq!(
            parsed,
            vec![
                "printf".to_owned(),
                "hello world".to_owned(),
                "a b".to_owned(),
                "test".to_owned(),
            ]
        );
    }

    #[test]
    fn parse_argv_with_escaped_space() {
        let parsed = parse_argv("printf a\\ b").expect("parse");
        assert_eq!(parsed, vec!["printf".to_owned(), "a b".to_owned()]);
    }

    #[test]
    fn parse_argv_with_empty_quoted_argument() {
        let parsed = parse_argv(r#"printf "" "a b""#).expect("parse");
        assert_eq!(
            parsed,
            vec!["printf".to_owned(), String::new(), "a b".to_owned()]
        );
    }

    fn assert_no_profiles(node: &NodeKind) {
        match node {
            NodeKind::Agent { profile, .. } | NodeKind::Reduce { profile, .. } => {
                assert!(profile.is_none());
            }
            NodeKind::Loop { graph, .. } => {
                for nested_node in &graph.spec.nodes {
                    assert_no_profiles(&nested_node.kind);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn template_no_profiles_without_input() {
        let templates = [
            GraphTemplate::Direct,
            GraphTemplate::PlanImplementVerify,
            GraphTemplate::ParallelResearchReduce,
            GraphTemplate::ReviewFixLoop,
        ];

        for template in templates {
            let graph = template_graph("t", "g", template, Some("request".to_owned()), None, None);
            for node in &graph.spec.nodes {
                assert_no_profiles(&node.kind);
            }
        }
    }

    #[test]
    fn templates_roundtrip_yaml() {
        let examples = [
            "direct.yaml",
            "plan-implement-verify.yaml",
            "parallel-research-reduce.yaml",
            "bounded-loop.yaml",
            "command-only.yaml",
        ];
        let examples_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples");
        for file in examples {
            let content = fs::read_to_string(examples_root.join(file)).expect("example exists");
            let graph = graph_from_yaml_bytes(content).expect("example parses");
            assert!(graph.validate().is_empty());
            let yaml = graph.to_yaml().expect("serialize");
            let reparsed = graph_from_yaml_bytes(yaml).expect("roundtrip parses");
            assert_eq!(graph, reparsed);
        }
    }

    #[test]
    fn dependency_edges_have_declared_kinds() {
        let dependencies = ["research_one", "review", "verify"];
        let selected = [0usize, 2];
        let drafts = vec![
            DependencyDraft {
                kind: EdgeKind::Control,
                when: None,
            },
            DependencyDraft {
                kind: EdgeKind::Failure,
                when: None,
            },
        ];
        let edges = build_dependency_edges(&dependencies, "synthesize", &selected, &drafts)
            .expect("edges should be built");

        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].from, "research_one");
        assert_eq!(edges[0].to, "synthesize");
        assert_eq!(edges[0].kind, EdgeKind::Control);
        assert_eq!(edges[1].from, "verify");
        assert_eq!(edges[1].to, "synthesize");
        assert_eq!(edges[1].kind, EdgeKind::Failure);
        assert!(edges[0].when.is_none());
    }

    #[test]
    fn dependency_edges_reject_wrong_shape() {
        let dependencies = ["a", "b"];
        let drafts = vec![DependencyDraft {
            kind: EdgeKind::Data,
            when: None,
        }];
        assert!(build_dependency_edges(&dependencies, "x", &[5], &drafts).is_err());

        let selected = [0usize, 1usize];
        assert!(build_dependency_edges(&dependencies, "x", &selected, &drafts).is_err());
    }

    #[test]
    fn dependency_edges_reject_conditional_without_when() {
        let dependencies = ["a"];
        let drafts = vec![DependencyDraft {
            kind: EdgeKind::Conditional,
            when: None,
        }];
        assert!(build_dependency_edges(&dependencies, "x", &[0], &drafts).is_err());
    }

    #[test]
    fn dependency_edges_allow_conditional_with_terminal_status() {
        let dependencies = ["a"];
        let drafts = vec![DependencyDraft {
            kind: EdgeKind::Conditional,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Succeeded),
                output_contains: Some("ok".to_owned()),
                json_pointer: Some("/result".to_owned()),
                equals: Some(serde_json::Value::String("true".to_owned())),
            }),
        }];
        let edges = build_dependency_edges(&dependencies, "x", &[0], &drafts)
            .expect("conditional edge should be built");

        assert_eq!(edges.len(), 1);
        let when = edges[0].when.as_ref().expect("when condition exists");
        assert_eq!(when.status, Some(NodeStatus::Succeeded));
    }

    #[test]
    fn graph_validates_conditional_edge_with_terminal_status() {
        let mut nodes = vec![
            super::agent_node("a", "prompt", None, 1),
            super::command_node("b", vec!["echo".into(), "done".into()]),
        ];
        nodes.push(super::agent_node("c", "sink", None, 1));
        let mut graph = Graph::new("g", "validate", nodes);
        graph.spec.edges.push(Edge {
            from: "a".to_owned(),
            to: "c".to_owned(),
            kind: EdgeKind::Conditional,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Succeeded),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        });
        let issues = graph.validate();
        assert!(
            issues
                .iter()
                .all(|issue| issue.severity != gloop_core::IssueSeverity::Error)
        );
    }

    #[test]
    fn graph_rejects_conditional_edge_with_non_terminal_status_helper() {
        let dependencies = ["a"];
        let drafts = vec![DependencyDraft {
            kind: EdgeKind::Conditional,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Running),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        }];
        assert!(build_dependency_edges(&dependencies, "x", &[0], &drafts).is_err());
    }

    #[test]
    fn json_pointer_validator_follows_rfc_6901_string_form() {
        assert!(is_valid_json_pointer(""));
        assert!(is_valid_json_pointer("/"));
        assert!(is_valid_json_pointer("/result"));
        assert!(is_valid_json_pointer("/items/0/name"));
        assert!(is_valid_json_pointer("/a//b"));
        assert!(is_valid_json_pointer("/a~1b/~0key/日本語 space"));
        assert!(!is_valid_json_pointer("result"));
        assert!(!is_valid_json_pointer("#/result"));
        assert!(!is_valid_json_pointer("/bad~"));
        assert!(!is_valid_json_pointer("/bad~2escape"));

        let invalid = build_dependency_edges(
            &["source"],
            "sink",
            &[0],
            &[DependencyDraft {
                kind: EdgeKind::Conditional,
                when: Some(EdgeCondition {
                    status: Some(NodeStatus::Succeeded),
                    output_contains: None,
                    json_pointer: Some("/bad~2escape".to_owned()),
                    equals: Some(json!(true)),
                }),
            }],
        );
        assert!(invalid.is_err());
    }

    #[test]
    fn json_condition_values_keep_their_types() {
        assert_eq!(parse_json_literal("true").expect("boolean"), json!(true));
        assert_eq!(parse_json_literal("42").expect("number"), json!(42));
        assert_eq!(parse_json_literal("null").expect("null"), json!(null));
        assert_eq!(
            parse_json_literal(r#"{"route":"review"}"#).expect("object"),
            json!({"route": "review"})
        );
        assert_eq!(
            parse_json_literal(r#""review""#).expect("string"),
            json!("review")
        );
        assert!(parse_json_literal("plain text is not JSON").is_err());

        let dependencies = ["judge"];
        let edges = build_dependency_edges(
            &dependencies,
            "next",
            &[0],
            &[DependencyDraft {
                kind: EdgeKind::Conditional,
                when: Some(EdgeCondition {
                    status: Some(NodeStatus::Succeeded),
                    output_contains: None,
                    json_pointer: Some("/done".to_owned()),
                    equals: Some(json!(true)),
                }),
            }],
        )
        .expect("typed condition");
        assert_eq!(
            edges[0].when.as_ref().and_then(|when| when.equals.clone()),
            Some(json!(true))
        );

        let mut graph = Graph::new(
            "typed-condition",
            "route using typed JSON",
            vec![
                super::command_node("judge", vec!["true".to_owned()]),
                super::command_node("next", vec!["true".to_owned()]),
            ],
        );
        graph.spec.edges = edges;
        assert_no_validation_errors(&graph);
        let reparsed = Graph::from_yaml_str(&graph.to_yaml().expect("serialize"))
            .expect("typed condition roundtrip");
        assert_eq!(
            reparsed.spec.edges[0]
                .when
                .as_ref()
                .and_then(|when| when.equals.clone()),
            Some(json!(true))
        );
    }

    #[test]
    fn graph_and_common_node_settings_roundtrip_strict_schema() {
        let mut node = super::agent_node("author", "write", Some("primary".to_owned()), 1);
        apply_common_node_settings(
            &mut node,
            CommonNodeSettings {
                resources: vec!["repository".to_owned(), "network".to_owned()],
                retry: RetryPolicy {
                    max_attempts: 3,
                    backoff_seconds: 2,
                    rebind_profiles: vec!["backup-a".to_owned(), "backup-b".to_owned()],
                },
                timeout_seconds: Some(90),
                workspace: WorkspaceSpec::Current,
                context: ContextSpec {
                    include_dependencies: false,
                    files: vec![PathBuf::from("README.md"), PathBuf::from("docs/SCHEMA.md")],
                    max_bytes: 32_768,
                },
            },
        )
        .expect("apply common settings");
        let output = node_output_mut(&mut node).expect("agent has output");
        output.format = OutputFormat::Json;
        output.max_bytes = 65_536;

        let mut graph = Graph::new("authored", "exercise settings", vec![node]);
        apply_graph_settings(
            &mut graph,
            GraphSettings {
                max_parallel: 7,
                failure: FailurePolicy::Continue,
                budgets: RunBudgets {
                    wall_time_seconds: Some(600),
                    model_calls: Some(20),
                },
            },
        );
        assert_no_validation_errors(&graph);

        let reparsed = Graph::from_yaml_str(&graph.to_yaml().expect("serialize"))
            .expect("strict-schema roundtrip");
        assert_eq!(reparsed, graph);
        assert_eq!(reparsed.spec.policies.max_parallel, 7);
        assert_eq!(reparsed.spec.policies.failure, FailurePolicy::Continue);
        assert_eq!(reparsed.spec.budgets.wall_time_seconds, Some(600));
        assert_eq!(reparsed.spec.budgets.model_calls, Some(20));
        let authored = &reparsed.spec.nodes[0];
        assert_eq!(authored.resources, ["repository", "network"]);
        assert_eq!(authored.retry.max_attempts, 3);
        assert_eq!(authored.retry.backoff_seconds, 2);
        assert_eq!(authored.retry.rebind_profiles, ["backup-a", "backup-b"]);
        assert_eq!(authored.timeout_seconds, Some(90));
        assert!(!authored.context.include_dependencies);
        assert_eq!(authored.context.files.len(), 2);
        assert_eq!(authored.context.max_bytes, 32_768);
        assert_eq!(
            authored.output().expect("output").format,
            OutputFormat::Json
        );
        assert_eq!(authored.output().expect("output").max_bytes, 65_536);
    }

    #[test]
    fn worktree_to_inherit_adds_direct_success_edge() {
        let mut owner = super::agent_node("owner", "edit", None, 1);
        owner.workspace = WorkspaceSpec::Worktree {
            base: Some("main".to_owned()),
            auto_commit: true,
        };
        let mut followup = super::command_node("followup", vec!["true".to_owned()]);
        followup.workspace = WorkspaceSpec::Inherit {
            node: "owner".to_owned(),
        };
        let mut edges = Vec::new();
        ensure_workspace_inheritance_edge(&followup, &mut edges).expect("inherit edge");

        assert_eq!(edges, vec![Edge::data("owner", "followup")]);
        let mut graph = Graph::new(
            "workspace-chain",
            "reuse retained workspace",
            vec![owner, followup],
        );
        graph.spec.edges = edges;
        assert_no_validation_errors(&graph);
        assert_eq!(
            Graph::from_yaml_str(&graph.to_yaml().expect("serialize")).expect("roundtrip"),
            graph
        );
    }

    #[test]
    fn worktree_rejects_agent_fan_out_above_one() {
        let mut node = super::agent_node("parallel", "edit", None, 2);
        let error = apply_common_node_settings(
            &mut node,
            CommonNodeSettings {
                resources: Vec::new(),
                retry: RetryPolicy::default(),
                timeout_seconds: None,
                workspace: WorkspaceSpec::Worktree {
                    base: None,
                    auto_commit: false,
                },
                context: ContextSpec::default(),
            },
        )
        .expect_err("worktree fan-out must be rejected");
        assert!(error.to_string().contains("fan_out = 1"));
    }

    #[test]
    fn nested_loop_and_subgraph_roundtrip_and_validate() {
        let loop_body = Graph::new(
            "loop-body",
            "produce completion JSON",
            vec![super::command_node(
                "judge",
                vec!["printf".to_owned(), r#"{"done":true}"#.to_owned()],
            )],
        );
        let loop_node = loop_node(
            "iterate",
            loop_body,
            LoopCondition {
                node: "judge".to_owned(),
                status: NodeStatus::Succeeded,
                output_contains: None,
                json_pointer: Some("/done".to_owned()),
                equals: Some(json!(true)),
            },
            5,
            2,
        );
        let nested = Graph::new(
            "nested-work",
            "run nested work",
            vec![super::command_node("work", vec!["true".to_owned()])],
        );
        let subgraph = subgraph_node("nested", nested);
        let graph = Graph::new("composed", "exercise nesting", vec![loop_node, subgraph]);
        assert_no_validation_errors(&graph);

        let yaml = graph.to_yaml().expect("serialize nested graph");
        let reparsed = Graph::from_yaml_str(&yaml).expect("parse nested graph");
        assert_eq!(reparsed, graph);
        assert_no_validation_errors(&reparsed);
    }

    #[test]
    fn nesting_actions_stop_at_the_interactive_ux_limit() {
        let at_limit = wizard_actions(super::INTERACTIVE_NESTING_LIMIT);
        assert!(
            !at_limit.iter().any(|(_, action)| {
                matches!(action, WizardAction::Loop | WizardAction::Subgraph)
            })
        );
        let below_limit = wizard_actions(super::INTERACTIVE_NESTING_LIMIT - 1);
        assert!(
            below_limit
                .iter()
                .any(|(_, action)| *action == WizardAction::Loop)
        );
        assert!(
            below_limit
                .iter()
                .any(|(_, action)| *action == WizardAction::Subgraph)
        );
    }

    #[test]
    fn csv_inputs_are_trimmed_and_deduplicated() {
        assert_eq!(
            parse_csv("repo, network,repo,, docs "),
            ["repo", "network", "docs"]
        );
    }

    #[test]
    fn build_synthesize_node_kind_and_yaml_roundtrip() {
        let mut node = synthesize_node("synth", "combine findings", Some("provider-x".to_owned()));
        if let NodeKind::Synthesize { model, .. } = &mut node.kind {
            *model = Some("model-y".to_owned());
        } else {
            panic!("expected synthesize node");
        }

        let graph = Graph::new("synth-graph", "synthesize in tests", vec![node.clone()]);
        let yaml = graph.to_yaml().expect("serialize graph");

        assert!(yaml.contains("kind: synthesize"));
        assert!(yaml.contains("prompt:"));

        let reparsed_graph = graph_from_yaml_bytes(yaml).expect("serialize+roundtrip parse");
        match &reparsed_graph.spec.nodes[0].kind {
            NodeKind::Synthesize {
                prompt,
                profile,
                model,
                ..
            } => {
                assert_eq!(prompt, &node_prompt());
                assert_eq!(profile.as_deref(), Some("provider-x"));
                assert_eq!(model.as_deref(), Some("model-y"));
            }
            _ => panic!("expected synthesize node"),
        }

        let reparsed = graph_from_yaml_bytes(
            r#"apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: synth-graph
  version: "1.0.0"
spec:
  goal: synthesize in tests
  nodes:
    - id: synth
      kind: synthesize
      prompt: |-
        combine findings
  edges: []"#,
        )
        .expect("parse inline synthesize yaml");
        match &reparsed.spec.nodes[0].kind {
            NodeKind::Synthesize {
                prompt,
                profile,
                model,
                ..
            } => {
                assert_eq!(
                    prompt,
                    &gloop_core::PromptSpec::Inline("combine findings".to_owned())
                );
                assert_eq!(profile, &None);
                assert_eq!(model, &None);
            }
            _ => panic!("expected parsed synthesize node"),
        }
    }

    fn node_prompt() -> gloop_core::PromptSpec {
        gloop_core::PromptSpec::Inline("combine findings".to_owned())
    }

    fn assert_no_validation_errors(graph: &Graph) {
        let errors: Vec<_> = graph
            .validate()
            .into_iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .collect();
        assert!(errors.is_empty(), "validation errors: {errors:#?}");
    }
}
