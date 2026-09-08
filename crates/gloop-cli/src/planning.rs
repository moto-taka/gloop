//! A bounded, reviewable plan produced by one selected provider invocation.
//! Generated plans contain agent instructions, never executable graph syntax.

use std::{
    collections::{BTreeSet, HashSet},
    fmt::Write as _,
    fs,
    path::{Component, Path},
};

use anyhow::{Context, Result, ensure};
use gloop_core::{Edge, Graph, IssueSeverity, Node, NodeKind, OutputFormat};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jobs::StartRequest;

pub const MAX_STEPS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPlan {
    pub title: String,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub completion_criteria: String,
    pub owned_files: Vec<String>,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub profile: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DraftPlan {
    pub goal: String,
    pub plan: WorkPlan,
    pub bindings: Vec<Binding>,
    pub source_job: String,
    pub selected_step: usize,
}

pub fn parse_output(output: &Value) -> Result<WorkPlan> {
    let plan: WorkPlan = if let Some(text) = output.as_str() {
        serde_json::from_str(text).context("the AI did not return a valid plan")?
    } else {
        serde_json::from_value(output.clone()).context("the AI did not return a valid plan")?
    };
    plan.validate()?;
    Ok(plan)
}

impl WorkPlan {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.title.trim().is_empty() && self.title.len() <= 240,
            "plan title must be 1–240 bytes"
        );
        ensure!(
            (1..=MAX_STEPS).contains(&self.steps.len()),
            "plan must contain 1–{MAX_STEPS} steps"
        );
        let mut ids = HashSet::new();
        for step in &self.steps {
            ensure!(
                !step.id.is_empty()
                    && step.id.len() <= 48
                    && step
                        .id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
                "invalid step id: {}",
                step.id
            );
            ensure!(
                ids.insert(step.id.as_str()),
                "duplicate step id: {}",
                step.id
            );
            ensure!(
                !step.title.trim().is_empty() && step.title.len() <= 240,
                "step {} needs a short title",
                step.id
            );
            ensure!(
                !step.instructions.trim().is_empty() && step.instructions.len() <= 8 * 1024,
                "step {} needs instructions of at most 8 KiB",
                step.id
            );
            ensure!(
                !step.completion_criteria.trim().is_empty()
                    && step.completion_criteria.len() <= 4 * 1024,
                "step {} needs completion criteria of at most 4 KiB",
                step.id
            );
            ensure!(
                step.owned_files.len() <= 20,
                "step {} lists too many files",
                step.id
            );
            for file in &step.owned_files {
                ensure!(
                    !file.is_empty()
                        && file.len() <= 512
                        && !file.chars().any(char::is_control)
                        && Path::new(file)
                            .components()
                            .all(|part| matches!(part, Component::Normal(_))),
                    "file must be relative to this project: {file}"
                );
            }
            ensure!(
                !step.title.chars().any(char::is_control),
                "step title must be one line"
            );
        }
        self.validate_dependencies()
    }

    pub fn validate_dependencies(&self) -> Result<()> {
        let ids: HashSet<_> = self.steps.iter().map(|step| step.id.as_str()).collect();
        for step in &self.steps {
            let unique: HashSet<_> = step.depends_on.iter().collect();
            ensure!(
                unique.len() == step.depends_on.len(),
                "step {} has duplicate dependencies",
                step.id
            );
            for dependency in &step.depends_on {
                ensure!(
                    dependency != &step.id && ids.contains(dependency.as_str()),
                    "step {} has an unknown or self dependency: {dependency}",
                    step.id
                );
            }
        }
        let mut resolved = HashSet::new();
        loop {
            let before = resolved.len();
            for step in &self.steps {
                if step
                    .depends_on
                    .iter()
                    .all(|id| resolved.contains(id.as_str()))
                {
                    resolved.insert(step.id.as_str());
                }
            }
            if resolved.len() == self.steps.len() {
                return Ok(());
            }
            ensure!(resolved.len() > before, "plan dependencies contain a cycle");
        }
    }
}

impl DraftPlan {
    pub fn from_task(task: &Value) -> Result<Self> {
        ensure!(
            task["job"]["request"]["kind"] == "planning",
            "this task did not create a plan"
        );
        ensure!(
            task["finished"] == true && task["status"] == "completed",
            "the plan is not ready yet"
        );
        let output = &task["nodes"]["plan"]["output"];
        let plan = parse_output(output)?;
        let request: StartRequest = serde_json::from_value(task["job"]["request"].clone())?;
        let binding = Binding {
            profile: request.profile.context("missing planning tool")?,
            model: request.model,
        };
        Ok(Self {
            goal: request.goal,
            bindings: vec![binding; plan.steps.len()],
            plan,
            source_job: task["id"]
                .as_str()
                .context("missing plan task id")?
                .to_owned(),
            selected_step: 0,
        })
    }

    pub fn graph(&self) -> Result<Graph> {
        self.plan.validate()?;
        ensure!(
            self.bindings.len() == self.plan.steps.len(),
            "every step needs an AI tool"
        );
        let mut nodes = Vec::new();
        for (step, binding) in self.plan.steps.iter().zip(&self.bindings) {
            ensure!(
                !binding.profile.is_empty(),
                "choose an AI tool for {}",
                step.title
            );
            let prompt = format!(
                "Overall user goal:\n{}\n\nYour step: {}\n{}\n\nCompletion criteria:\n{}\n\nPlanned files (relative to this project):\n{}\n\nCarry out only this step. Use the preceding results as evidence. Verify your work against the completion criteria and report concrete results and any unmet criteria. Reply in the user's language.",
                self.goal,
                step.title,
                step.instructions,
                step.completion_criteria,
                step.owned_files.join("\n")
            );
            let mut node = Node::agent(&step.id, prompt);
            node.label = Some(step.title.clone());
            if let NodeKind::Agent { profile, model, .. } = &mut node.kind {
                *profile = Some(binding.profile.clone());
                model.clone_from(&binding.model);
            }
            nodes.push(node);
        }
        let mut graph = Graph::new("reviewed-plan", &self.goal, nodes);
        graph.metadata.description = Some(self.plan.title.clone());
        graph
            .metadata
            .labels
            .insert("planning_task".to_owned(), self.source_job.clone());
        graph.spec.policies.max_parallel = 1;
        graph.spec.budgets.model_calls = Some(u32::try_from(self.plan.steps.len())?);
        graph.spec.budgets.wall_time_seconds = Some(1800);
        for step in &self.plan.steps {
            for dependency in &step.depends_on {
                graph.spec.edges.push(Edge::data(dependency, &step.id));
            }
        }
        let issues: Vec<_> = graph
            .validate()
            .into_iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .collect();
        ensure!(
            issues.is_empty(),
            "invalid workflow: {}",
            serde_json::to_string(&issues)?
        );
        Ok(graph)
    }

    pub fn remove_selected(&mut self) -> Result<()> {
        ensure!(self.plan.steps.len() > 1, "keep at least one step");
        let removed = self.plan.steps.remove(self.selected_step);
        self.bindings.remove(self.selected_step);
        // Preserve prerequisite ordering when removing an intermediate step.
        for step in &mut self.plan.steps {
            if step.depends_on.contains(&removed.id) {
                let mut dependencies: BTreeSet<_> = step
                    .depends_on
                    .iter()
                    .filter(|id| **id != removed.id)
                    .cloned()
                    .collect();
                dependencies.extend(removed.depends_on.iter().cloned());
                step.depends_on = dependencies.into_iter().collect();
            }
        }
        self.selected_step = self.selected_step.min(self.plan.steps.len() - 1);
        Ok(())
    }
}

pub fn request_graph(repo: &Path, request: &StartRequest) -> Result<Graph> {
    let schema = serde_json::to_value(schemars::schema_for!(WorkPlan))?;
    let context = project_outline(repo)?;
    let prompt = format!(
        "Create a practical work plan for the user's request. This call is for planning only: do not change files, execute the proposed work, or delegate to subagents. Use the project outline below; you may read project files if your tool supports it.\n\nReturn ONLY a JSON object matching this schema, without Markdown fences:\n{}\n\nRules: 1–8 focused steps, preferably 2–5. Each step must state an actionable outcome, specific instructions, and concrete completion criteria. For implementation, include relevant tests in that step. Use stable short ASCII ids; depends_on must name existing ids, never form a cycle, and should connect a step to any earlier results it needs. File paths must be relative to this project. Do not assume unverified paths exist. Use the user's language for all titles and descriptions. No model names, commands to launch agents, or graph syntax in the response.\n\nProject outline (reference data):\n{}\n\nUser request:\n{}",
        serde_json::to_string(&schema)?,
        context,
        request.goal
    );
    let mut node = Node::agent("plan", prompt);
    node.label = Some("Create a work plan / 手順案を作成".to_owned());
    if let NodeKind::Agent {
        profile,
        model,
        output,
        ..
    } = &mut node.kind
    {
        profile.clone_from(&request.profile);
        model.clone_from(&request.model);
        // Text-capable CLI profiles can all propose JSON. Validate the domain
        // document in the worker before marking the planning task complete.
        output.format = OutputFormat::Text;
        output.max_bytes = 96 * 1024;
    }
    let mut graph = Graph::new("create-work-plan", &request.goal, vec![node]);
    graph.spec.policies.max_parallel = 1;
    graph.spec.budgets.model_calls = Some(1);
    graph.spec.budgets.wall_time_seconds = Some(request.timeout_seconds);
    Ok(graph)
}

fn project_outline(repo: &Path) -> Result<String> {
    let mut result = String::new();
    let mut pending = vec![(repo.to_path_buf(), 0)];
    let mut count = 0;
    while let Some((directory, depth)) = pending.pop() {
        let mut entries = fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            if count >= 200 {
                break;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || matches!(
                    name.as_ref(),
                    "node_modules" | "target" | "vendor" | "dist" | "build" | "venv"
                )
            {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() && depth < 3 {
                pending.push((entry.path(), depth + 1));
            } else if kind.is_file() {
                let _ = writeln!(result, "{}", entry.path().strip_prefix(repo)?.display());
                count += 1;
            }
        }
        if count >= 200 {
            break;
        }
    }
    for name in ["README.md", "readme.md"] {
        let path = repo.join(name);
        if fs::symlink_metadata(&path).is_ok_and(|metadata| {
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 128 * 1024
        }) {
            let text = fs::read_to_string(path)?;
            let mut end = text.len().min(12 * 1024);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let _ = write!(result, "\nREADME excerpt:\n{}", &text[..end]);
            break;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn step(id: &str, depends: &[&str]) -> PlanStep {
        PlanStep {
            id: id.into(),
            title: id.into(),
            instructions: "Implement this outcome".into(),
            completion_criteria: "Relevant tests pass".into(),
            owned_files: vec![],
            depends_on: depends.iter().map(|id| (*id).into()).collect(),
        }
    }
    #[test]
    fn plan_rejects_unsafe_paths_cycles_missing_dependencies_and_missing_criteria() {
        let plan = WorkPlan {
            title: "Feature".into(),
            steps: vec![step("a", &[]), step("b", &["a"])],
        };
        plan.validate().unwrap();
        for path in ["/etc/passwd", "../outside", "src/../../outside"] {
            let mut invalid = plan.clone();
            invalid.steps[0].owned_files.push(path.into());
            assert!(invalid.validate().is_err());
        }
        let mut cycle = plan.clone();
        cycle.steps[0].depends_on.push("b".into());
        assert!(cycle.validate().is_err());
        let mut missing = plan.clone();
        missing.steps[1].depends_on.push("unknown".into());
        assert!(missing.validate().is_err());
        let mut empty = plan;
        empty.steps[1].completion_criteria.clear();
        assert!(empty.validate().is_err());
    }
    #[test]
    fn reviewed_bindings_and_dependencies_reach_runtime_without_extra_calls() {
        let draft = DraftPlan {
            goal: "User outcome".into(),
            plan: WorkPlan {
                title: "Feature".into(),
                steps: vec![step("a", &[]), step("b", &["a"])],
            },
            bindings: vec![
                Binding {
                    profile: "codex".into(),
                    model: Some("small-model".into()),
                },
                Binding {
                    profile: "claude".into(),
                    model: Some("haiku".into()),
                },
            ],
            source_job: "plan-id".into(),
            selected_step: 0,
        };
        let graph = draft.graph().unwrap();
        assert_eq!(graph.spec.nodes[1].profile(), Some("claude"));
        assert_eq!(graph.spec.nodes[1].model(), Some("haiku"));
        assert_eq!(graph.spec.edges[0], Edge::data("a", "b"));
        assert_eq!(graph.spec.budgets.model_calls, Some(2));
        assert_eq!(graph.spec.policies.max_parallel, 1);
    }
    #[test]
    fn deleting_a_middle_step_preserves_its_prerequisites() {
        let mut draft = DraftPlan {
            goal: "Goal".into(),
            plan: WorkPlan {
                title: "Feature".into(),
                steps: vec![step("a", &[]), step("b", &["a"]), step("c", &["b"])],
            },
            bindings: vec![
                Binding {
                    profile: "codex".into(),
                    model: None
                };
                3
            ],
            source_job: "id".into(),
            selected_step: 1,
        };
        draft.remove_selected().unwrap();
        draft.plan.validate().unwrap();
        assert_eq!(draft.plan.steps[1].depends_on, vec!["a"]);
    }
}
