//! Local task workspace on the same authenticated loopback server as the editor.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use gloop_core::Graph;
use gloop_provider::{ProfileKind, ProviderRegistry};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{commands, gui, i18n::Language, jobs, templates};

pub async fn launch(repo: PathBuf, trusted: bool, language: Language, no_open: bool) -> Result<()> {
    let repo = std::fs::canonicalize(repo).context("open project directory")?;
    let graph = Graph::new("workspace", "", Vec::new());
    tokio::task::spawn_blocking(move || {
        gui::launch_with_browser(
            graph,
            &[],
            gui::GuiTarget::Workspace {
                repo,
                trust_project_profiles: trusted,
            },
            language,
            !no_open,
        )
    })
    .await??;
    Ok(())
}

#[derive(Debug)]
pub struct Workspace {
    repo: PathBuf,
    trusted: bool,
    language: Language,
    handle: tokio::runtime::Handle,
    setup: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Submit {
    request: jobs::StartRequest,
    id: Option<String>,
}

impl Workspace {
    pub fn new(repo: PathBuf, trusted: bool, language: Language) -> Result<Self> {
        Ok(Self {
            repo,
            trusted,
            language,
            handle: tokio::runtime::Handle::try_current()?,
            setup: None,
        })
    }

    pub fn dispatch(
        &mut self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> (u16, &'static str, Vec<u8>) {
        if method == "GET" && path == "/" {
            return (
                200,
                "text/html; charset=utf-8",
                include_bytes!("workspace.html").to_vec(),
            );
        }
        let handle = self.handle.clone();
        match handle.block_on(self.api(method, path, body)) {
            Ok(value) => (
                200,
                "application/json",
                serde_json::to_vec(&value).expect("JSON value serializes"),
            ),
            Err(error) => (
                422,
                "application/json",
                serde_json::to_vec(&json!({"error": format!("{error:#}")}))
                    .expect("JSON error serializes"),
            ),
        }
    }

    async fn api(&mut self, method: &str, path: &str, body: &[u8]) -> Result<Value> {
        match (method, path) {
            ("GET", "/api/workspace") => Ok(
                json!({"repo": self.repo, "language": self.language.as_str(), "trusted_project_profiles": self.trusted}),
            ),
            ("GET", "/api/providers" | "/api/providers/refresh") => {
                if self.setup.is_none() || path.ends_with("/refresh") {
                    self.setup = Some(provider_setup(&self.repo, self.trusted).await?);
                }
                Ok(self.setup.clone().expect("setup loaded"))
            }
            ("GET", "/api/workflows") => {
                let paths = templates::list_graph_files(&self.repo).map_err(anyhow::Error::msg)?;
                let mut graphs = Vec::new();
                for path in paths.into_iter().take(100) {
                    match Graph::from_path(&path) {
                        Ok(graph) => graphs.push(json!({"path": path.strip_prefix(&self.repo).unwrap_or(&path), "graph": graph, "issues": graph.validate()})),
                        Err(error) => graphs.push(json!({"path": path.strip_prefix(&self.repo).unwrap_or(&path), "error": error.to_string()})),
                    }
                }
                Ok(json!({"workflows": graphs}))
            }
            ("GET", "/api/tasks") => jobs::list(&self.repo),
            ("POST", "/api/tasks") => {
                let submit: Submit =
                    serde_json::from_slice(body).context("invalid task request")?;
                Ok(browser_task(
                    jobs::start(&self.repo, submit.request, self.trusted, submit.id).await?,
                ))
            }
            ("GET", path) if path.starts_with("/api/tasks/") => Ok(browser_task(
                jobs::detail(&self.repo, path.trim_start_matches("/api/tasks/")).await?,
            )),
            ("POST", path) if path.starts_with("/api/tasks/") && path.ends_with("/stop") => {
                let id = path
                    .trim_start_matches("/api/tasks/")
                    .trim_end_matches("/stop");
                Ok(browser_task(jobs::cancel(&self.repo, id).await?))
            }
            _ => bail!("unknown workspace action"),
        }
    }
}

pub async fn provider_setup(repo: &std::path::Path, trusted: bool) -> Result<Value> {
    let choices = commands::build_profile_choices(repo, trusted)?;
    let store = jobs::profiles(repo, trusted)?;
    let registry = ProviderRegistry::new(store.clone());
    let mut pending = tokio::task::JoinSet::new();
    for choice in &choices {
        let registry = registry.clone();
        let name = choice.name.clone();
        pending.spawn(async move {
            let result = registry.probe(&name, CancellationToken::new()).await;
            (name, result)
        });
    }
    let options = commands::resolve_profile_options(repo, trusted, &choices).await?;
    let mut probes = std::collections::HashMap::new();
    while let Some(result) = pending.join_next().await {
        let (name, probe) = result?;
        probes.insert(name, probe);
    }
    let mut profiles = Vec::new();
    for option in options {
        let probe = probes
            .remove(&option.name)
            .context("missing AI tool check")?;
        let (available, reason) = match probe {
            Ok(result) => (
                result.available,
                result.failure.map(|failure| format!("{failure:?}")),
            ),
            Err(error) => (false, Some(error.to_string())),
        };
        let (executable, kind) = match store.get(&option.name).map(|profile| &profile.kind) {
            Some(ProfileKind::Command(command)) => (command.argv.first().cloned(), "cli"),
            _ => (None, "api"),
        };
        profiles.push(json!({
            "name": option.name, "enabled": option.enabled, "available": available,
            "reason": reason, "executable": executable, "kind": kind,
            "default_model": option.default_model, "models": option.models,
            "discovery": option.discovery, "discovery_error": option.discovery_error,
        }));
    }
    Ok(json!({"profiles": profiles}))
}

fn browser_task(mut task: Value) -> Value {
    let mut remaining = 256 * 1024;
    if let Some(nodes) = task["nodes"].as_object_mut() {
        for node in nodes.values_mut() {
            if let Some(output) = node.get("output").filter(|output| !output.is_null()) {
                let text = output
                    .as_str()
                    .map_or_else(|| output.to_string(), str::to_owned);
                let mut end = text.len().min(64 * 1024).min(remaining);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                remaining -= end;
                if end < text.len() {
                    node["output"] = json!(&text[..end]);
                    node["output_truncated"] = json!(true);
                }
            }
        }
    }
    if let Some(request) = task["job"]["request"].as_object_mut() {
        request.remove("graph");
    }
    task
}
