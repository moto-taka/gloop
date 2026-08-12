//! Local browser editor for graph files and project templates.

use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    process::Stdio,
};

use anyhow::{Context, Result, anyhow};
use gloop_core::{Graph, IssueSeverity, ValidationIssue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::{
    atomic_write::{write_text_atomic_if_unchanged_sync, write_text_atomic_sync},
    templates,
};

const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Language {
    En,
    Ja,
}

impl Language {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ja => "ja",
        }
    }
}

#[derive(Debug, Clone)]
pub enum GuiTarget {
    GraphFile {
        path: PathBuf,
        expected_sha256: Option<String>,
    },
    ProjectTemplate {
        repo: PathBuf,
        force: bool,
        saved_name: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ProfileOption {
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub default_model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GuiResult {
    pub graph: Graph,
    pub written: PathBuf,
}

#[derive(Debug, Serialize)]
struct GuiState {
    graph: Value,
    profiles: Vec<ProfileOptionPayload>,
    models: Vec<String>,
    language: &'static str,
    target: &'static str,
}

#[derive(Debug, Serialize)]
struct ProfileOptionPayload {
    name: String,
    kind: String,
    enabled: bool,
    default_model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SaveRequest {
    graph: Value,
}

#[derive(Debug, Deserialize)]
struct RequestTarget {
    method: String,
    path: String,
    body: Vec<u8>,
    token: Option<String>,
    origin: Option<String>,
}

pub fn launch(
    graph: Graph,
    profiles: &[ProfileOption],
    mut target: GuiTarget,
    language: Language,
) -> Result<GuiResult> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("bind local GUI server")?;
    let address = listener.local_addr().context("read local GUI address")?;
    let token = Ulid::new().to_string().to_lowercase();
    let url = format!("http://127.0.0.1:{}/#{token}", address.port());
    open_browser(&url)?;

    let mut current_graph = graph;
    let mut written = None;
    loop {
        let (mut stream, _) = listener.accept().context("accept local GUI connection")?;
        let request = read_request(&mut stream)?;
        let is_root = request.method == "GET" && route_path(&request.path) == "/";
        if !is_root && !authorized(&request, &token, &format!("http://{address}")) {
            write_response(
                &mut stream,
                401,
                "application/json",
                br#"{"error":"unauthorized"}"#,
            )?;
            continue;
        }

        match (request.method.as_str(), route_path(&request.path)) {
            ("GET", "/") => {
                let html = gui_html();
                write_response(
                    &mut stream,
                    200,
                    "text/html; charset=utf-8",
                    html.as_bytes(),
                )?;
            }
            ("GET", "/api/state") => {
                let payload = serde_json::to_vec(&build_state(
                    &current_graph,
                    profiles,
                    language,
                    &target,
                )?)?;
                write_response(&mut stream, 200, "application/json", &payload)?;
            }
            ("POST", "/api/save") => match save_graph(&request.body, &mut target) {
                Ok((graph, path)) => {
                    current_graph = graph;
                    written = Some(path.clone());
                    let payload = serde_json::to_vec(&json!({
                        "success": true,
                        "written": path,
                        "message": match language {
                            Language::En => "Saved. Keep editing or close the editor.",
                            Language::Ja => "保存しました。続けて編集するか、エディタを閉じてください。",
                        },
                    }))?;
                    write_response(&mut stream, 200, "application/json", &payload)?;
                }
                Err(error) => {
                    let payload = serde_json::to_vec(&json!({
                        "success": false,
                        "error": error.to_string(),
                    }))?;
                    write_response(&mut stream, 422, "application/json", &payload)?;
                }
            },
            ("POST", "/api/close") => {
                write_response(&mut stream, 200, "application/json", br#"{"success":true}"#)?;
                break;
            }
            _ => write_response(
                &mut stream,
                404,
                "application/json",
                br#"{"error":"not found"}"#,
            )?,
        }
    }

    let written = written.ok_or_else(|| anyhow!("GUI closed before saving"))?;
    Ok(GuiResult {
        graph: current_graph,
        written,
    })
}

fn build_state(
    graph: &Graph,
    profiles: &[ProfileOption],
    language: Language,
    target: &GuiTarget,
) -> Result<GuiState> {
    let graph_value = serde_json::to_value(graph).context("serialize graph for GUI")?;
    let mut models = BTreeSet::new();
    for node in &graph.spec.nodes {
        if let Some(model) = node.model().filter(|model| !model.trim().is_empty()) {
            models.insert(model.to_owned());
        }
    }
    for profile in profiles {
        if let Some(model) = profile
            .default_model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
        {
            models.insert(model.to_owned());
        }
    }
    Ok(GuiState {
        graph: graph_value,
        profiles: profiles
            .iter()
            .map(|profile| ProfileOptionPayload {
                name: profile.name.clone(),
                kind: profile.kind.clone(),
                enabled: profile.enabled,
                default_model: profile.default_model.clone(),
            })
            .collect(),
        models: models.into_iter().collect(),
        language: language.as_str(),
        target: match target {
            GuiTarget::GraphFile { .. } => "graph",
            GuiTarget::ProjectTemplate { .. } => "template",
        },
    })
}

fn save_graph(body: &[u8], target: &mut GuiTarget) -> Result<(Graph, PathBuf)> {
    let request: SaveRequest = serde_json::from_slice(body).context("parse GUI save request")?;
    let graph: Graph = serde_json::from_value(request.graph).context("parse graph from GUI")?;
    let issues = graph.validate();
    let errors = issues
        .iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .collect::<Vec<&ValidationIssue>>();
    if !errors.is_empty() {
        return Err(anyhow!(
            "graph validation failed: {}",
            errors
                .iter()
                .map(|issue| format!("[{}] {}", issue.code, issue.message))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let yaml = graph.to_yaml().context("serialize graph YAML")?;
    let path = match target {
        GuiTarget::GraphFile {
            path,
            expected_sha256,
        } => {
            if let Some(expected) = expected_sha256 {
                let actual = file_sha256(path)?;
                if actual != *expected {
                    return Err(anyhow!(
                        "graph changed on disk while the editor was open; reload before saving"
                    ));
                }
            }
            path.clone()
        }
        GuiTarget::ProjectTemplate {
            repo,
            force,
            saved_name,
        } => {
            templates::validate_init_template_name(&graph.metadata.name)
                .map_err(|error| anyhow!(error))?;
            let path = templates::template_path(repo, &graph.metadata.name);
            let same_saved_name = saved_name
                .as_deref()
                .is_none_or(|saved| saved == graph.metadata.name);
            if *force && same_saved_name {
                write_text_atomic_sync(&path, &yaml).context("write graph template")?;
            } else {
                crate::atomic_write::write_text_no_replace_sync(&path, &yaml)
                    .context("write graph template")?;
            }
            *saved_name = Some(graph.metadata.name.clone());
            return Ok((graph, path));
        }
    };
    if let GuiTarget::GraphFile {
        expected_sha256: Some(expected),
        ..
    } = target
    {
        write_text_atomic_if_unchanged_sync(&path, expected, &yaml)
            .context("write graph file")?;
    } else {
        write_text_atomic_sync(&path, &yaml).context("write graph file")?;
    }
    if let GuiTarget::GraphFile {
        expected_sha256, ..
    } = target
    {
        *expected_sha256 = Some(file_sha256(&path)?);
    }
    Ok((graph, path))
}

fn authorized(request: &RequestTarget, token: &str, origin: &str) -> bool {
    request.token.as_deref() == Some(token)
        && request
            .origin
            .as_deref()
            .is_none_or(|candidate| candidate == origin)
}

fn route_path(path: &str) -> &str {
    path.split_once('?').map_or(path, |(route, _)| route)
}

fn read_request(stream: &mut TcpStream) -> Result<RequestTarget> {
    let mut buffer = Vec::with_capacity(8192);
    let header_end = loop {
        let mut chunk = [0u8; 8192];
        let read = stream.read(&mut chunk).context("read local GUI request")?;
        if read == 0 {
            return Err(anyhow!("local GUI connection closed before request"));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_REQUEST_BYTES {
            return Err(anyhow!("local GUI request is too large"));
        }
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(anyhow!("local GUI request headers are too large"));
        }
    };
    let header = std::str::from_utf8(&buffer[..header_end]).context("GUI request headers")?;
    let mut lines = header.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing GUI request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow!("missing GUI method"))?
        .to_owned();
    let path = parts
        .next()
        .ok_or_else(|| anyhow!("missing GUI path"))?
        .to_owned();
    let mut content_length = 0;
    let mut token = None;
    let mut origin = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .parse::<usize>()
                .context("invalid GUI content length")?;
        } else if name.eq_ignore_ascii_case("x-gloop-token") {
            token = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("origin") {
            origin = Some(value.to_owned());
        }
    }
    if content_length > MAX_REQUEST_BYTES || header_end + content_length > MAX_REQUEST_BYTES {
        return Err(anyhow!("local GUI request body is too large"));
    }
    while buffer.len() < header_end + content_length {
        let remaining = header_end + content_length - buffer.len();
        let mut chunk = vec![0u8; remaining.min(8192)];
        let read = stream.read(&mut chunk).context("read local GUI body")?;
        if read == 0 {
            return Err(anyhow!("local GUI connection closed before body"));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Ok(RequestTarget {
        method,
        path,
        body: buffer[header_end..header_end + content_length].to_vec(),
        token,
        origin,
    })
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush().context("flush local GUI response")?;
    Ok(())
}

fn open_browser(url: &str) -> Result<()> {
    let mut command = if let Ok(browser) = std::env::var("BROWSER") {
        Command::new(browser)
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    } else {
        Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("open GUI in the system browser")?;
    Ok(())
}

pub fn file_sha256(path: &PathBuf) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path).context("inspect graph before GUI save")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(anyhow!("graph save target is not a regular file"));
    }
    let bytes = std::fs::read(path).context("read graph before GUI save")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn gui_html() -> String {
    include_str!("gui.html").to_owned()
}
