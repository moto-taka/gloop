//! Local browser editor for graph files and project templates.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use gloop_core::{Graph, IssueSeverity, ValidationIssue};
use gloop_provider::CatalogModel;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::{
    atomic_write::{
        write_text_atomic_if_unchanged_sync, write_text_atomic_sync, write_text_no_replace_sync,
    },
    templates,
};

const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const PROBE_BYTES: usize = 8 * 1024;
const MAX_IDLE_PRECONNECT_READERS: usize = 32;
const MAX_UNCLASSIFIED_HEADER_READERS: usize = MAX_IDLE_PRECONNECT_READERS;
const RESERVED_AUTHENTICATED_SLOTS: usize = 4;
const MAX_PENDING_REQUEST_READERS: usize =
    MAX_IDLE_PRECONNECT_READERS + RESERVED_AUTHENTICATED_SLOTS;
const REQUEST_QUEUE_WAIT: Duration = Duration::from_millis(10);
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(30);
const CLOSE_IDLE_GRACE: Duration = Duration::from_millis(250);
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_CAPACITY_REJECTION_BODY: &[u8] = br#"{"error":"idle connection capacity exceeded"}"#;

#[derive(Debug, Clone, Copy)]
struct ServerTuning {
    read_timeout: Duration,
    request_deadline: Duration,
    max_pending_readers: usize,
}

impl Default for ServerTuning {
    fn default() -> Self {
        Self {
            read_timeout: SOCKET_READ_TIMEOUT,
            request_deadline: REQUEST_READ_DEADLINE,
            max_pending_readers: MAX_PENDING_REQUEST_READERS,
        }
    }
}

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
        create_only: bool,
    },
    ProjectTemplate {
        repo: PathBuf,
        force: bool,
        saved_name: Option<String>,
        expected_sha256: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ProfileOption {
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub runtime_default: bool,
    pub default_model: Option<String>,
    pub models: Vec<CatalogModel>,
    pub discovery: String,
    pub discovery_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GuiResult {
    pub graph: Graph,
    pub written: Option<PathBuf>,
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
    runtime_default: bool,
    default_model: Option<String>,
    models: Vec<CatalogModel>,
    discovery: String,
    discovery_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SaveRequest {
    graph: Value,
}

#[derive(Debug, Clone)]
struct RequestTarget {
    method: String,
    path: String,
    body: Vec<u8>,
    token: Option<String>,
    origin: Option<String>,
}

#[derive(Debug)]
struct RequestHeaders {
    method: String,
    path: String,
    content_length: usize,
    token: Option<String>,
    origin: Option<String>,
}

enum IncomingRequest {
    Ready(RequestTarget),
    NeedsBody(RequestHeaders),
    Rejected { status: u16, body: Vec<u8> },
}

struct DeferredConnection {
    accept_order: u64,
    accepted_at: Instant,
    stream: TcpStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionProbe {
    Idle,
    Request { headers_complete: bool },
}

struct PendingRequest {
    accept_order: u64,
    stream: TcpStream,
    incoming: IncomingRequest,
}

pub fn launch(
    graph: Graph,
    profiles: &[ProfileOption],
    target: GuiTarget,
    language: Language,
) -> Result<GuiResult> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("bind local GUI server")?;
    let address = listener.local_addr().context("read local GUI address")?;
    let token = Ulid::new().to_string().to_lowercase();
    let url = format!("http://127.0.0.1:{}/#{token}", address.port());
    open_browser(&url)?;

    serve(
        &listener,
        &token,
        graph,
        profiles,
        target,
        language,
        ServerTuning::default(),
    )
}

fn reject_unclassified_idle(mut stream: TcpStream) {
    let _ = stream.set_nonblocking(true);
    let header = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        IDLE_CAPACITY_REJECTION_BODY.len()
    );
    let mut payload = header.into_bytes();
    payload.extend_from_slice(IDLE_CAPACITY_REJECTION_BODY);
    let _ = stream.write(&payload);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn register_accept_order(
    accept_order: &Arc<AtomicU64>,
    unresolved_orders: &Arc<std::sync::Mutex<BTreeSet<u64>>>,
) -> u64 {
    let order = accept_order.fetch_add(1, Ordering::Relaxed) + 1;
    unresolved_orders
        .lock()
        .expect("local GUI unresolved order tracking")
        .insert(order);
    order
}

fn release_accept_order(order: u64, unresolved_orders: &Arc<std::sync::Mutex<BTreeSet<u64>>>) {
    unresolved_orders
        .lock()
        .expect("local GUI unresolved order tracking")
        .remove(&order);
}

fn probe_connection(stream: &TcpStream) -> Result<ConnectionProbe> {
    stream
        .set_nonblocking(true)
        .context("configure local GUI connection probe")?;
    let mut probe = [0_u8; PROBE_BYTES];
    let result = match stream.peek(&mut probe) {
        Ok(0) => Ok(ConnectionProbe::Idle),
        Ok(read) => Ok(ConnectionProbe::Request {
            headers_complete: probe[..read].windows(4).any(|window| window == b"\r\n\r\n"),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(ConnectionProbe::Idle),
        Err(source) => Err(source).context("peek local GUI connection"),
    };
    stream
        .set_nonblocking(false)
        .context("restore local GUI connection blocking mode")?;
    result
}

fn admit_deferred_idle(
    deferred_streams: &mut Vec<DeferredConnection>,
    connection: DeferredConnection,
    unresolved_orders: &Arc<std::sync::Mutex<BTreeSet<u64>>>,
) -> Result<Option<(DeferredConnection, ConnectionProbe)>> {
    if deferred_streams.len() >= MAX_IDLE_PRECONNECT_READERS {
        let evicted = deferred_streams.remove(0);
        let probe = match probe_connection(&evicted.stream) {
            Ok(probe) => probe,
            Err(error) => {
                release_accept_order(evicted.accept_order, unresolved_orders);
                reject_unclassified_idle(evicted.stream);
                return Err(error);
            }
        };
        if matches!(probe, ConnectionProbe::Request { .. }) {
            deferred_streams.push(connection);
            return Ok(Some((evicted, probe)));
        }
        release_accept_order(evicted.accept_order, unresolved_orders);
        reject_unclassified_idle(evicted.stream);
    }
    deferred_streams.push(connection);
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn spawn_request_reader(
    stream: TcpStream,
    accept_order: u64,
    headers_complete: bool,
    request_sender: &mpsc::Sender<PendingRequest>,
    active_readers: &Arc<AtomicUsize>,
    unclassified_readers: &Arc<AtomicUsize>,
    token: &Arc<str>,
    origin: &Arc<str>,
    tuning: ServerTuning,
) {
    if !headers_complete {
        unclassified_readers.fetch_add(1, Ordering::Relaxed);
    }
    active_readers.fetch_add(1, Ordering::Relaxed);
    let sender = request_sender.clone();
    let reader_count = Arc::clone(active_readers);
    let unclassified_count = Arc::clone(unclassified_readers);
    let token = Arc::clone(token);
    let origin = Arc::clone(origin);
    thread::spawn(move || {
        let mut stream = stream;
        let incoming = match stream
            .set_read_timeout(Some(tuning.read_timeout))
            .context("configure local GUI request stream")
            .and_then(|()| read_headers(&mut stream, tuning))
        {
            Ok(headers) => build_incoming_request(headers, &token, &origin),
            Err(error) => IncomingRequest::Rejected {
                status: 400,
                body: serde_json::to_vec(&json!({
                    "error": format!("invalid request: {error}"),
                }))
                .unwrap_or_else(|_| br#"{"error":"invalid request"}"#.to_vec()),
            },
        };
        if !headers_complete {
            unclassified_count.fetch_sub(1, Ordering::Relaxed);
        }
        if sender
            .send(PendingRequest {
                accept_order,
                stream,
                incoming,
            })
            .is_err()
        {
            // Keep `accept_order` in `unresolved_orders` so Close cannot bypass a
            // completed reader handoff while the server thread is still alive.
        }
        reader_count.fetch_sub(1, Ordering::Relaxed);
    });
}

#[allow(clippy::too_many_arguments)]
fn start_unclassified_reader(
    stream: TcpStream,
    accept_order: u64,
    headers_complete: bool,
    request_sender: &mpsc::Sender<PendingRequest>,
    active_readers: &Arc<AtomicUsize>,
    unclassified_readers: &Arc<AtomicUsize>,
    unresolved_orders: &Arc<std::sync::Mutex<BTreeSet<u64>>>,
    token: &Arc<str>,
    origin: &Arc<str>,
    tuning: ServerTuning,
) {
    if active_readers.load(Ordering::Relaxed) >= tuning.max_pending_readers {
        release_accept_order(accept_order, unresolved_orders);
        reject_unclassified_idle(stream);
        return;
    }
    spawn_request_reader(
        stream,
        accept_order,
        headers_complete,
        request_sender,
        active_readers,
        unclassified_readers,
        token,
        origin,
        tuning,
    );
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn receive_pending(
    listener: &TcpListener,
    request_sender: &mpsc::Sender<PendingRequest>,
    request_receiver: &mpsc::Receiver<PendingRequest>,
    active_readers: &Arc<AtomicUsize>,
    unclassified_readers: &Arc<AtomicUsize>,
    unresolved_orders: &Arc<std::sync::Mutex<BTreeSet<u64>>>,
    accept_order: &Arc<AtomicU64>,
    deferred_streams: &mut Vec<DeferredConnection>,
    auth: &(Arc<str>, Arc<str>),
    tuning: ServerTuning,
) -> Result<PendingRequest> {
    if let Ok(request) = request_receiver.try_recv() {
        return Ok(request);
    }
    let (token, origin) = auth;
    loop {
        let mut promoted = 0;
        while promoted < deferred_streams.len()
            && active_readers.load(Ordering::Relaxed) < tuning.max_pending_readers
        {
            let connection = deferred_streams.remove(promoted);
            let stream = connection.stream;
            let probe = probe_connection(&stream)?;
            if let ConnectionProbe::Request { headers_complete } = probe {
                if headers_complete
                    || unclassified_readers.load(Ordering::Relaxed)
                        < MAX_UNCLASSIFIED_HEADER_READERS
                {
                    start_unclassified_reader(
                        stream,
                        connection.accept_order,
                        headers_complete,
                        request_sender,
                        active_readers,
                        unclassified_readers,
                        unresolved_orders,
                        token,
                        origin,
                        tuning,
                    );
                } else {
                    deferred_streams.insert(
                        promoted,
                        DeferredConnection {
                            accept_order: connection.accept_order,
                            accepted_at: connection.accepted_at,
                            stream,
                        },
                    );
                    promoted += 1;
                }
            } else {
                deferred_streams.insert(
                    promoted,
                    DeferredConnection {
                        accept_order: connection.accept_order,
                        accepted_at: connection.accepted_at,
                        stream,
                    },
                );
                promoted += 1;
            }
        }
        loop {
            if active_readers.load(Ordering::Relaxed) >= tuning.max_pending_readers
                || unclassified_readers.load(Ordering::Relaxed) >= MAX_UNCLASSIFIED_HEADER_READERS
            {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let accept_order = register_accept_order(accept_order, unresolved_orders);
                    let probe = probe_connection(&stream)?;
                    if let ConnectionProbe::Request { headers_complete } = probe {
                        start_unclassified_reader(
                            stream,
                            accept_order,
                            headers_complete,
                            request_sender,
                            active_readers,
                            unclassified_readers,
                            unresolved_orders,
                            token,
                            origin,
                            tuning,
                        );
                    } else if let Some((
                        connection,
                        ConnectionProbe::Request { headers_complete },
                    )) = admit_deferred_idle(
                        deferred_streams,
                        DeferredConnection {
                            accept_order,
                            accepted_at: Instant::now(),
                            stream,
                        },
                        unresolved_orders,
                    )? {
                        start_unclassified_reader(
                            connection.stream,
                            connection.accept_order,
                            headers_complete,
                            request_sender,
                            active_readers,
                            unclassified_readers,
                            unresolved_orders,
                            token,
                            origin,
                            tuning,
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error).context("accept local GUI connection"),
            }
        }
        match request_receiver.recv_timeout(REQUEST_QUEUE_WAIT) {
            Ok(request) => return Ok(request),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("local GUI request reader stopped"));
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn serve(
    listener: &TcpListener,
    token: &str,
    graph: Graph,
    profiles: &[ProfileOption],
    mut target: GuiTarget,
    language: Language,
    tuning: ServerTuning,
) -> Result<GuiResult> {
    let address = listener.local_addr().context("read local GUI address")?;
    let origin = format!("http://{address}");
    let mut current_graph = graph;
    let mut written = None;
    listener
        .set_nonblocking(true)
        .context("configure local GUI listener")?;
    let (request_sender, request_receiver) = mpsc::channel::<PendingRequest>();
    let active_readers = Arc::new(AtomicUsize::new(0));
    let unclassified_readers = Arc::new(AtomicUsize::new(0));
    let unresolved_orders = Arc::new(std::sync::Mutex::new(BTreeSet::<u64>::new()));
    let accept_order = Arc::new(AtomicU64::new(0));
    let token = Arc::<str>::from(token);
    let origin = Arc::<str>::from(origin.as_str());
    let auth = (Arc::clone(&token), Arc::clone(&origin));
    let mut pending: BTreeMap<u64, PendingRequest> = BTreeMap::new();
    let mut in_flight_saves: BTreeSet<u64> = BTreeSet::new();
    let mut deferred_streams: Vec<DeferredConnection> = Vec::new();
    let mut closing = false;

    while !closing || !pending.is_empty() || !in_flight_saves.is_empty() {
        while let Ok(pending_request) = request_receiver.try_recv() {
            insert_pending_request(&mut pending, &unresolved_orders, pending_request);
        }

        if let Some(close_order) = pending
            .iter()
            .find_map(|(&order, request)| is_close_request(&request.incoming).then_some(order))
        {
            discard_idle_deferred_before_close(
                close_order,
                &mut deferred_streams,
                &unresolved_orders,
            )?;
        }

        if let Some(order) = next_dispatchable(&pending, &in_flight_saves, &unresolved_orders) {
            let entry = pending
                .remove(&order)
                .expect("dispatchable request must exist");
            let is_save = is_save_request(&entry.incoming);
            if is_save {
                in_flight_saves.insert(order);
            }
            let mut stream = entry.stream;
            let request = match entry.incoming {
                IncomingRequest::Ready(request) => request,
                IncomingRequest::Rejected { status, body } => {
                    // Discard unread request bytes at the kernel boundary without
                    // consuming them in application code. This keeps an
                    // unauthorized body out of the process while allowing the
                    // response to close cleanly instead of causing a TCP reset.
                    let _ = stream.shutdown(Shutdown::Read);
                    let _ = write_response(&mut stream, status, "application/json", &body);
                    if is_save {
                        in_flight_saves.remove(&order);
                    }
                    continue;
                }
                IncomingRequest::NeedsBody(headers) => {
                    let body = match read_body(&mut stream, &headers, tuning) {
                        Ok(body) => body,
                        Err(error) => {
                            let payload = serde_json::to_vec(&json!({
                                "error": format!("invalid request: {error}"),
                            }))
                            .unwrap_or_else(|_| br#"{"error":"invalid request"}"#.to_vec());
                            let _ = write_response(&mut stream, 408, "application/json", &payload);
                            if is_save {
                                in_flight_saves.remove(&order);
                            }
                            continue;
                        }
                    };
                    RequestTarget {
                        method: headers.method,
                        path: headers.path,
                        body,
                        token: headers.token,
                        origin: headers.origin,
                    }
                }
            };

            let is_root = request.method == "GET" && route_path(&request.path) == "/";
            if !is_root && !authorized(&request, token.as_ref(), origin.as_ref()) {
                let _ = write_response(
                    &mut stream,
                    401,
                    "application/json",
                    br#"{"error":"unauthorized"}"#,
                );
                if is_save {
                    in_flight_saves.remove(&order);
                }
                continue;
            }

            match (request.method.as_str(), route_path(&request.path)) {
                ("GET", "/") => {
                    let html = gui_html();
                    let _ = write_response(
                        &mut stream,
                        200,
                        "text/html; charset=utf-8",
                        html.as_bytes(),
                    );
                }
                ("GET", "/api/state") => {
                    if let Ok(payload) = serde_json::to_vec(&build_state(
                        &current_graph,
                        profiles,
                        language,
                        &target,
                    )?) {
                        let _ = write_response(&mut stream, 200, "application/json", &payload);
                    }
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
                        }))
                        .unwrap_or_default();
                        let _ = write_response(&mut stream, 200, "application/json", &payload);
                    }
                    Err(error) => {
                        let payload = serde_json::to_vec(&json!({
                            "success": false,
                            "error": error.to_string(),
                        }))
                        .unwrap_or_default();
                        let _ = write_response(&mut stream, 422, "application/json", &payload);
                    }
                },
                ("POST", "/api/close") => {
                    let _ = write_response(
                        &mut stream,
                        200,
                        "application/json",
                        br#"{"success":true}"#,
                    );
                    closing = true;
                }
                _ => {
                    let _ = write_response(
                        &mut stream,
                        404,
                        "application/json",
                        br#"{"error":"not found"}"#,
                    );
                }
            }

            if is_save {
                in_flight_saves.remove(&order);
            }

            if closing {
                break;
            }
            continue;
        }

        if closing {
            break;
        }

        let pending_request = receive_pending(
            listener,
            &request_sender,
            &request_receiver,
            &active_readers,
            &unclassified_readers,
            &unresolved_orders,
            &accept_order,
            &mut deferred_streams,
            &auth,
            tuning,
        )?;
        insert_pending_request(&mut pending, &unresolved_orders, pending_request);
    }

    Ok(GuiResult {
        graph: current_graph,
        written,
    })
}

fn is_save_request(incoming: &IncomingRequest) -> bool {
    match incoming {
        IncomingRequest::Ready(request) => {
            request.method == "POST" && route_path(&request.path) == "/api/save"
        }
        IncomingRequest::NeedsBody(headers) => {
            headers.method == "POST" && route_path(&headers.path) == "/api/save"
        }
        IncomingRequest::Rejected { .. } => false,
    }
}

fn is_close_request(incoming: &IncomingRequest) -> bool {
    matches!(
        incoming,
        IncomingRequest::Ready(request)
            if request.method == "POST" && route_path(&request.path) == "/api/close"
    )
}

fn can_dispatch_close(
    order: u64,
    pending: &BTreeMap<u64, PendingRequest>,
    in_flight_saves: &BTreeSet<u64>,
    unresolved_orders: &BTreeSet<u64>,
) -> bool {
    for (&other_order, other) in pending {
        if other_order >= order {
            break;
        }
        if is_save_request(&other.incoming) {
            return false;
        }
    }
    if in_flight_saves
        .iter()
        .any(|other_order| *other_order < order)
    {
        return false;
    }
    !unresolved_orders
        .iter()
        .any(|other_order| *other_order < order)
}

fn can_dispatch(
    order: u64,
    pending: &BTreeMap<u64, PendingRequest>,
    in_flight_saves: &BTreeSet<u64>,
    unresolved_orders: &BTreeSet<u64>,
) -> bool {
    let Some(entry) = pending.get(&order) else {
        return false;
    };
    if is_close_request(&entry.incoming) {
        return can_dispatch_close(order, pending, in_flight_saves, unresolved_orders);
    }
    true
}

fn discard_idle_deferred_before_close(
    close_order: u64,
    deferred_streams: &mut Vec<DeferredConnection>,
    unresolved_orders: &Arc<std::sync::Mutex<BTreeSet<u64>>>,
) -> Result<()> {
    let mut retained = Vec::with_capacity(deferred_streams.len());
    for connection in deferred_streams.drain(..) {
        if connection.accept_order >= close_order {
            retained.push(connection);
            continue;
        }
        match probe_connection(&connection.stream)? {
            ConnectionProbe::Idle if connection.accepted_at.elapsed() >= CLOSE_IDLE_GRACE => {
                release_accept_order(connection.accept_order, unresolved_orders);
                reject_unclassified_idle(connection.stream);
            }
            ConnectionProbe::Idle => {
                retained.push(connection);
            }
            ConnectionProbe::Request { .. } => retained.push(connection),
        }
    }
    *deferred_streams = retained;
    Ok(())
}

fn insert_pending_request(
    pending: &mut BTreeMap<u64, PendingRequest>,
    unresolved_orders: &Arc<std::sync::Mutex<BTreeSet<u64>>>,
    request: PendingRequest,
) {
    let order = request.accept_order;
    pending.insert(order, request);
    unresolved_orders
        .lock()
        .expect("local GUI unresolved order tracking")
        .remove(&order);
}

fn next_dispatchable(
    pending: &BTreeMap<u64, PendingRequest>,
    in_flight_saves: &BTreeSet<u64>,
    unresolved_orders: &Arc<std::sync::Mutex<BTreeSet<u64>>>,
) -> Option<u64> {
    let unresolved = unresolved_orders
        .lock()
        .expect("local GUI unresolved order tracking");
    pending
        .keys()
        .copied()
        .find(|order| can_dispatch(*order, pending, in_flight_saves, &unresolved))
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
        if node.profile().is_some() {
            continue;
        }
        if let Some(model) = node.model().filter(|model| !model.trim().is_empty()) {
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
                runtime_default: profile.runtime_default,
                default_model: profile.default_model.clone(),
                models: profile.models.clone(),
                discovery: profile.discovery.clone(),
                discovery_error: profile.discovery_error.clone(),
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
            create_only,
        } => {
            if let Some(expected) = expected_sha256 {
                let actual = file_sha256(path)?;
                if actual != *expected {
                    return Err(anyhow!(
                        "graph changed on disk while the editor was open; reload before saving"
                    ));
                }
            } else if *create_only && std::fs::symlink_metadata(&*path).is_ok() {
                return Err(anyhow!(
                    "a graph was created at {} while the editor was open; reload before saving",
                    path.display()
                ));
            }
            path.clone()
        }
        GuiTarget::ProjectTemplate {
            repo,
            force,
            saved_name,
            expected_sha256,
        } => {
            templates::validate_init_template_name(&graph.metadata.name)
                .map_err(|error| anyhow!(error))?;
            templates::ensure_managed_directory(
                repo,
                std::path::Path::new(templates::TEMPLATES_DIR),
            )
            .context("unsafe project template directory")?;
            if let Some(saved) = saved_name.as_deref()
                && saved != graph.metadata.name
            {
                return Err(anyhow!(
                    "template name cannot change after the first save; close and reopen the editor"
                ));
            }
            let path = templates::template_path(repo, &graph.metadata.name);
            if let Some(expected) = expected_sha256.as_deref() {
                write_text_atomic_if_unchanged_sync(&path, expected, &yaml)
                    .context("write graph template")?;
            } else if *force {
                write_text_atomic_sync(&path, &yaml).context("write graph template")?;
            } else {
                write_text_no_replace_sync(&path, &yaml).context("write graph template")?;
            }
            *saved_name = Some(graph.metadata.name.clone());
            *expected_sha256 = Some(file_sha256(&path)?);
            return Ok((graph, path));
        }
    };
    if let GuiTarget::GraphFile {
        expected_sha256,
        create_only,
        ..
    } = target
    {
        match (expected_sha256.as_deref(), *create_only) {
            (Some(expected), _) => write_text_atomic_if_unchanged_sync(&path, expected, &yaml)
                .context("write graph file")?,
            (None, true) => {
                write_text_no_replace_sync(&path, &yaml).context("create graph file")?;
            }
            (None, false) => write_text_atomic_sync(&path, &yaml).context("write graph file")?,
        }
    } else {
        write_text_atomic_sync(&path, &yaml).context("write graph file")?;
    }
    if let GuiTarget::GraphFile {
        expected_sha256, ..
    } = target
    {
        *expected_sha256 = Some(file_sha256(&path)?);
    }
    if let GuiTarget::GraphFile { create_only, .. } = target {
        *create_only = false;
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

fn build_incoming_request(headers: RequestHeaders, token: &str, origin: &str) -> IncomingRequest {
    let route = route_path(&headers.path);
    let is_root = headers.method == "GET" && route == "/";
    let authorized = is_root || authorized_headers(&headers, token, origin);
    let allows_body = headers.method == "POST" && route == "/api/save";
    if headers.content_length > MAX_REQUEST_BYTES {
        return IncomingRequest::Rejected {
            status: 400,
            body: br#"{"error":"request body is too large"}"#.to_vec(),
        };
    }
    if !allows_body && headers.content_length > 0 {
        return IncomingRequest::Rejected {
            status: 400,
            body: br#"{"error":"request body is not allowed"}"#.to_vec(),
        };
    }
    if !authorized {
        return IncomingRequest::Rejected {
            status: 401,
            body: br#"{"error":"unauthorized"}"#.to_vec(),
        };
    }
    if headers.content_length == 0 {
        return IncomingRequest::Ready(RequestTarget {
            method: headers.method,
            path: headers.path,
            body: Vec::new(),
            token: headers.token,
            origin: headers.origin,
        });
    }
    IncomingRequest::NeedsBody(headers)
}

fn authorized_headers(headers: &RequestHeaders, token: &str, origin: &str) -> bool {
    headers.token.as_deref() == Some(token)
        && headers
            .origin
            .as_deref()
            .is_none_or(|candidate| candidate == origin)
}

fn read_headers(stream: &mut TcpStream, tuning: ServerTuning) -> Result<RequestHeaders> {
    let deadline = Instant::now() + tuning.read_timeout;
    let mut buffer = Vec::with_capacity(4096);
    loop {
        if Instant::now() >= deadline {
            return Err(anyhow!("local GUI request timed out"));
        }
        let mut byte = [0u8; 1];
        let read = match stream.read(&mut byte) {
            Ok(0) => {
                return Err(anyhow!("local GUI connection closed before request"));
            }
            Ok(read) => read,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(source) => return Err(source).context("read local GUI request"),
        };
        debug_assert_eq!(read, 1);
        buffer.push(byte[0]);
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(anyhow!("local GUI request headers are too large"));
        }
        if buffer.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header = std::str::from_utf8(&buffer).context("GUI request headers")?;
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
    let mut content_length_headers = 0;
    let mut transfer_encoding = false;
    let mut token = None;
    let mut origin = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length_headers += 1;
            content_length = value
                .parse::<usize>()
                .context("invalid GUI content length")?;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            transfer_encoding = true;
        } else if name.eq_ignore_ascii_case("x-gloop-token") {
            token = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("origin") {
            origin = Some(value.to_owned());
        }
    }
    if transfer_encoding || content_length_headers > 1 {
        return Err(anyhow!("unsupported GUI request framing"));
    }
    Ok(RequestHeaders {
        method,
        path,
        content_length,
        token,
        origin,
    })
}

fn read_body(
    stream: &mut TcpStream,
    headers: &RequestHeaders,
    tuning: ServerTuning,
) -> Result<Vec<u8>> {
    stream
        .set_read_timeout(Some(tuning.request_deadline))
        .context("configure local GUI body read timeout")?;
    let deadline = Instant::now() + tuning.request_deadline;
    let mut body = Vec::with_capacity(headers.content_length);
    while body.len() < headers.content_length {
        if Instant::now() >= deadline {
            return Err(anyhow!("local GUI request body timed out"));
        }
        let mut chunk = vec![0_u8; (headers.content_length - body.len()).min(8192)];
        let read = match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(anyhow!("local GUI connection closed before body"));
            }
            Ok(read) => read,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(source) => return Err(source).context("read local GUI body"),
        };
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(headers.content_length);
    Ok(body)
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    stream
        .set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT))
        .context("configure local GUI response timeout")?;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        408 => "Request Timeout",
        422 => "Unprocessable Entity",
        503 => "Service Unavailable",
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpStream},
        path::PathBuf,
        time::Duration,
    };
    use tempfile::tempdir;

    fn save_body() -> Vec<u8> {
        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        serde_json::to_vec(&json!({"graph": graph})).expect("save body")
    }

    fn send_request(address: SocketAddr, request: &str) -> std::io::Result<String> {
        let mut stream = TcpStream::connect(address)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(request.as_bytes())?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response)
    }

    fn spawn_server(
        listener: TcpListener,
        token: String,
        graph: Graph,
        target: GuiTarget,
        tuning: ServerTuning,
    ) -> thread::JoinHandle<Result<GuiResult>> {
        thread::spawn(move || serve(&listener, &token, graph, &[], target, Language::Ja, tuning))
    }

    fn dummy_stream() -> TcpStream {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        TcpStream::connect(address).expect("connect")
    }

    fn close_request(order: u64) -> PendingRequest {
        PendingRequest {
            accept_order: order,
            stream: dummy_stream(),
            incoming: IncomingRequest::Ready(RequestTarget {
                method: "POST".to_owned(),
                path: "/api/close".to_owned(),
                body: Vec::new(),
                token: Some("test-token".to_owned()),
                origin: Some("http://127.0.0.1".to_owned()),
            }),
        }
    }

    fn save_needs_body_request(order: u64) -> PendingRequest {
        PendingRequest {
            accept_order: order,
            stream: dummy_stream(),
            incoming: IncomingRequest::NeedsBody(RequestHeaders {
                method: "POST".to_owned(),
                path: "/api/save".to_owned(),
                content_length: 1,
                token: Some("test-token".to_owned()),
                origin: Some("http://127.0.0.1".to_owned()),
            }),
        }
    }

    #[test]
    fn completed_lower_save_blocks_higher_close_until_admitted() {
        let unresolved_orders = Arc::new(std::sync::Mutex::new(BTreeSet::from([1_u64])));
        let mut pending = BTreeMap::from([(2_u64, close_request(2))]);
        let in_flight_saves = BTreeSet::new();

        let unresolved = unresolved_orders
            .lock()
            .expect("local GUI unresolved order tracking");
        assert!(
            !can_dispatch_close(2, &pending, &in_flight_saves, &unresolved),
            "Close must wait while a lower save is still unresolved"
        );
        drop(unresolved);

        insert_pending_request(&mut pending, &unresolved_orders, save_needs_body_request(1));

        let unresolved = unresolved_orders
            .lock()
            .expect("local GUI unresolved order tracking");
        assert!(
            unresolved.is_empty(),
            "admitting into pending must resolve the lower order"
        );
        assert!(
            !can_dispatch_close(2, &pending, &in_flight_saves, &unresolved),
            "Close must still wait for the lower pending save"
        );
    }

    #[test]
    fn admit_deferred_idle_evicts_oldest_and_retains_newest() {
        let unresolved_orders = Arc::new(std::sync::Mutex::new(BTreeSet::<u64>::new()));
        let accept_order = Arc::new(AtomicU64::new(0));
        let mut deferred = Vec::new();
        let mut clients = Vec::new();
        let mut servers = Vec::new();
        for _ in 0..=MAX_IDLE_PRECONNECT_READERS {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
            let address = listener.local_addr().expect("address");
            let client = TcpStream::connect(address).expect("connect");
            let (server, _) = listener.accept().expect("accept");
            clients.push(client);
            servers.push(server);
        }

        for server in servers.drain(..MAX_IDLE_PRECONNECT_READERS) {
            let order = register_accept_order(&accept_order, &unresolved_orders);
            admit_deferred_idle(
                &mut deferred,
                DeferredConnection {
                    accept_order: order,
                    accepted_at: Instant::now(),
                    stream: server,
                },
                &unresolved_orders,
            )
            .expect("admit idle connection");
        }
        assert_eq!(deferred.len(), MAX_IDLE_PRECONNECT_READERS);

        let newest = servers.pop().expect("newest server");
        let order = register_accept_order(&accept_order, &unresolved_orders);
        admit_deferred_idle(
            &mut deferred,
            DeferredConnection {
                accept_order: order,
                accepted_at: Instant::now(),
                stream: newest,
            },
            &unresolved_orders,
        )
        .expect("admit idle connection");
        assert_eq!(deferred.len(), MAX_IDLE_PRECONNECT_READERS);

        let mut evicted = clients.remove(0);
        evicted
            .set_read_timeout(Some(Duration::from_millis(250)))
            .expect("read timeout");
        let mut buffer = [0_u8; 256];
        let read = evicted.read(&mut buffer).expect("read evicted response");
        let response = std::str::from_utf8(&buffer[..read]).expect("utf8");
        assert!(
            response.starts_with("HTTP/1.1 503 Service Unavailable"),
            "evicted response: {response}"
        );
        assert!(response.contains("idle connection capacity exceeded"));

        let mut retained = clients.pop().expect("retained client");
        retained
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("read timeout");
        retained.set_nonblocking(true).expect("nonblocking");
        match retained.read(&mut buffer) {
            Ok(0) => panic!("newest idle connection closed before sending a request"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Ok(read) => panic!("unexpected data on retained idle connection: {read} bytes"),
            Err(error) => panic!("unexpected retained read error: {error}"),
        }
    }

    #[test]
    fn deferred_idle_storage_stays_capped_under_saturation() {
        let unresolved_orders = Arc::new(std::sync::Mutex::new(BTreeSet::<u64>::new()));
        let accept_order = Arc::new(AtomicU64::new(0));
        let mut deferred = Vec::new();
        let mut clients = Vec::new();
        for _ in 0..(MAX_IDLE_PRECONNECT_READERS + 8) {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
            let address = listener.local_addr().expect("address");
            let client = TcpStream::connect(address).expect("connect");
            let (server, _) = listener.accept().expect("accept");
            let order = register_accept_order(&accept_order, &unresolved_orders);
            admit_deferred_idle(
                &mut deferred,
                DeferredConnection {
                    accept_order: order,
                    accepted_at: Instant::now(),
                    stream: server,
                },
                &unresolved_orders,
            )
            .expect("admit idle connection");
            clients.push(client);
            assert!(
                deferred.len() <= MAX_IDLE_PRECONNECT_READERS,
                "deferred idle storage grew past cap: {}",
                deferred.len()
            );
        }
        assert_eq!(deferred.len(), MAX_IDLE_PRECONNECT_READERS);
    }

    #[test]
    fn saturated_idle_holding_area_retains_new_connection_for_later_request() {
        let repo = tempdir().expect("temp repo");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let token = "test-token".to_owned();
        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        let target = GuiTarget::GraphFile {
            path: repo.path().join("graph.yaml"),
            expected_sha256: None,
            create_only: true,
        };
        let server = spawn_server(
            listener,
            token.clone(),
            graph,
            target,
            ServerTuning {
                read_timeout: Duration::from_millis(250),
                request_deadline: Duration::from_secs(2),
                max_pending_readers: MAX_PENDING_REQUEST_READERS,
            },
        );
        let mut idle_clients = (0..MAX_IDLE_PRECONNECT_READERS)
            .map(|_| TcpStream::connect(address).expect("idle connection"))
            .collect::<Vec<_>>();
        let mut newest_idle = TcpStream::connect(address).expect("newest idle connection");
        newest_idle
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("read timeout");
        newest_idle.set_nonblocking(true).expect("nonblocking");
        let mut probe = [0_u8; 1];
        match newest_idle.read(&mut probe) {
            Ok(0) => panic!("newest idle connection closed before sending a request"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Ok(read) => panic!("unexpected data on newest idle connection: {read} bytes"),
            Err(error) => panic!("unexpected newest idle read error: {error}"),
        }
        newest_idle
            .set_nonblocking(false)
            .expect("restore blocking");

        let mut evicted = idle_clients.remove(0);
        evicted
            .set_read_timeout(Some(Duration::from_millis(250)))
            .expect("read timeout");
        let mut evicted_buffer = [0_u8; 256];
        let read = evicted.read(&mut evicted_buffer).expect("read evicted");
        let evicted_response = std::str::from_utf8(&evicted_buffer[..read]).expect("utf8");
        assert!(
            evicted_response.starts_with("HTTP/1.1 503 Service Unavailable"),
            "evicted response: {evicted_response}"
        );

        let origin = format!("http://{address}");
        let state_request = format!(
            "GET /api/state HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nConnection: close\r\n\r\n"
        );
        newest_idle
            .write_all(state_request.as_bytes())
            .expect("state request");
        let mut state_response = String::new();
        newest_idle
            .read_to_string(&mut state_response)
            .expect("state response");
        let close_request = format!(
            "POST /api/close HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = send_request(address, &close_request);
        let result = server.join().expect("server thread").expect("serve GUI");
        drop(idle_clients);

        assert!(
            state_response.starts_with("HTTP/1.1 200 OK"),
            "state response: {state_response}"
        );
        assert!(result.written.is_none());
    }

    #[test]
    fn idle_preconnect_connections_do_not_block_authenticated_requests() {
        let repo = tempdir().expect("temp repo");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let token = "test-token".to_owned();
        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        let target = GuiTarget::GraphFile {
            path: repo.path().join("graph.yaml"),
            expected_sha256: None,
            create_only: true,
        };
        let server = spawn_server(
            listener,
            token.clone(),
            graph,
            target,
            ServerTuning {
                read_timeout: Duration::from_millis(250),
                request_deadline: Duration::from_secs(2),
                max_pending_readers: MAX_PENDING_REQUEST_READERS,
            },
        );
        let idle_connections = (0..MAX_PENDING_REQUEST_READERS)
            .map(|_| TcpStream::connect(address).expect("idle connection"))
            .collect::<Vec<_>>();
        let started = Instant::now();
        let origin = format!("http://{address}");
        let state_request = format!(
            "GET /api/state HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nConnection: close\r\n\r\n"
        );
        let state_response = send_request(address, &state_request).expect("state response");
        let close_request = format!(
            "POST /api/close HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let close_response = send_request(address, &close_request).expect("close response");
        let result = server.join().expect("server thread").expect("serve GUI");
        drop(idle_connections);

        assert!(
            state_response.starts_with("HTTP/1.1 200 OK"),
            "state response: {state_response}"
        );
        assert!(
            close_response.starts_with("HTTP/1.1 200 OK"),
            "close response: {close_response}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(750),
            "authenticated request took {:?}",
            started.elapsed()
        );
        assert!(result.written.is_none());
    }

    #[test]
    fn unauthorized_save_declared_body_is_never_read() {
        let repo = tempdir().expect("temp repo");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let token = "test-token".to_owned();
        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        let target = GuiTarget::GraphFile {
            path: repo.path().join("graph.yaml"),
            expected_sha256: None,
            create_only: true,
        };
        let server = spawn_server(
            listener,
            token.clone(),
            graph,
            target,
            ServerTuning::default(),
        );
        let mut stream = TcpStream::connect(address).expect("connect");
        stream
            .write_all(
                format!(
                    "POST /api/save HTTP/1.1\r\nHost: {address}\r\nX-Gloop-Token: wrong\r\nContent-Length: 4194304\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("write headers");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        drop(stream);
        let close_request = format!(
            "POST /api/close HTTP/1.1\r\nHost: {address}\r\nX-Gloop-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = send_request(address, &close_request);
        let result = server.join().expect("join").expect("serve");
        assert!(response.contains("401"));
        assert!(result.written.is_none());
    }

    #[test]
    fn unauthorized_piggybacked_body_is_rejected_without_reading_remainder() {
        let repo = tempdir().expect("temp repo");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let token = "test-token".to_owned();
        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        let target = GuiTarget::GraphFile {
            path: repo.path().join("graph.yaml"),
            expected_sha256: None,
            create_only: true,
        };
        let server = spawn_server(
            listener,
            token.clone(),
            graph,
            target,
            ServerTuning::default(),
        );
        let started = Instant::now();
        let mut stream = TcpStream::connect(address).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let declared = 4 * 1024 * 1024;
        let piggyback = b"{\"graph\":";
        stream
            .write_all(
                format!(
                    "POST /api/save HTTP/1.1\r\nHost: {address}\r\nX-Gloop-Token: wrong\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("write headers");
        stream.write_all(piggyback).expect("write piggyback");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        drop(stream);
        let close_request = format!(
            "POST /api/close HTTP/1.1\r\nHost: {address}\r\nX-Gloop-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = send_request(address, &close_request);
        let result = server.join().expect("join").expect("serve");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "unauthorized piggybacked body took {:?}",
            started.elapsed()
        );
        assert!(response.contains("401"));
        assert!(result.written.is_none());
    }

    #[test]
    fn deferred_no_byte_save_blocks_later_close_until_identified() {
        let repo = tempdir().expect("temp repo");
        let path = repo.path().join("graph.yaml");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let token = "test-token".to_owned();
        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        let target = GuiTarget::GraphFile {
            path: path.clone(),
            expected_sha256: None,
            create_only: true,
        };
        let server = spawn_server(
            listener,
            token.clone(),
            graph,
            target,
            ServerTuning::default(),
        );
        let origin = format!("http://{address}");
        let body = save_body();
        let mut save_stream = TcpStream::connect(address).expect("save connect");
        std::thread::sleep(Duration::from_millis(100));
        let mut close_stream = TcpStream::connect(address).expect("close connect");
        close_stream
            .write_all(
                format!(
                    "POST /api/close HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("close headers");
        close_stream
            .set_nonblocking(true)
            .expect("close nonblocking");
        let mut close_probe = [0_u8; 1];
        match close_stream.read(&mut close_probe) {
            Ok(0) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Ok(read) => panic!("close completed early with {read} bytes"),
            Err(error) => panic!("unexpected close read error: {error}"),
        }
        close_stream
            .set_nonblocking(false)
            .expect("restore close blocking");
        save_stream
            .write_all(
                format!(
                    "POST /api/save HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("save headers");
        save_stream.write_all(&body).expect("save body");
        let mut save_response = String::new();
        save_stream
            .read_to_string(&mut save_response)
            .expect("save response");
        close_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("restore close read timeout");
        let mut close_response = String::new();
        close_stream
            .read_to_string(&mut close_response)
            .expect("close response");
        let result = server.join().expect("join").expect("serve");
        assert!(
            save_response.starts_with("HTTP/1.1 200 OK"),
            "save response: {save_response}"
        );
        assert!(
            close_response.starts_with("HTTP/1.1 200 OK"),
            "close response: {close_response}"
        );
        assert_eq!(result.written.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn partial_header_flood_does_not_block_authenticated_requests() {
        let repo = tempdir().expect("temp repo");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let token = "test-token".to_owned();
        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        let target = GuiTarget::GraphFile {
            path: repo.path().join("graph.yaml"),
            expected_sha256: None,
            create_only: true,
        };
        let server = spawn_server(
            listener,
            token.clone(),
            graph,
            target,
            ServerTuning {
                read_timeout: Duration::from_millis(250),
                request_deadline: Duration::from_secs(2),
                max_pending_readers: MAX_PENDING_REQUEST_READERS,
            },
        );
        let flood_count = MAX_UNCLASSIFIED_HEADER_READERS + 32;
        let mut flood_streams = Vec::new();
        for _ in 0..flood_count {
            let mut stream = TcpStream::connect(address).expect("flood connect");
            stream.write_all(b"P").expect("partial header byte");
            flood_streams.push(stream);
        }
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        let origin = format!("http://{address}");
        let state_request = format!(
            "GET /api/state HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nConnection: close\r\n\r\n"
        );
        let state_response = send_request(address, &state_request).expect("state response");
        let close_request = format!(
            "POST /api/close HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = send_request(address, &close_request);
        let result = server.join().expect("server thread").expect("serve GUI");
        drop(flood_streams);
        assert!(
            state_response.starts_with("HTTP/1.1 200 OK"),
            "state response: {state_response}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "authenticated request took {:?}",
            started.elapsed()
        );
        assert!(result.written.is_none());
    }

    #[test]
    fn close_waits_for_unresolved_save_before_dispatching() {
        let repo = tempdir().expect("temp repo");
        let path = repo.path().join("graph.yaml");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let token = "test-token".to_owned();
        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        let target = GuiTarget::GraphFile {
            path: path.clone(),
            expected_sha256: None,
            create_only: true,
        };
        let server = spawn_server(
            listener,
            token.clone(),
            graph,
            target,
            ServerTuning::default(),
        );
        let origin = format!("http://{address}");
        let body = save_body();
        let mut save_stream = TcpStream::connect(address).expect("save connect");
        save_stream
            .write_all(
                format!(
                    "POST /api/save HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: {}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("partial save headers");
        std::thread::sleep(Duration::from_millis(100));
        let mut close_stream = TcpStream::connect(address).expect("close connect");
        close_stream
            .write_all(
                format!(
                    "POST /api/close HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("close headers");
        close_stream
            .set_nonblocking(true)
            .expect("close nonblocking");
        let mut close_probe = [0_u8; 1];
        match close_stream.read(&mut close_probe) {
            Ok(0) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Ok(read) => panic!("close completed early with {read} bytes"),
            Err(error) => panic!("unexpected close read error: {error}"),
        }
        close_stream
            .set_nonblocking(false)
            .expect("restore close blocking");
        save_stream
            .write_all(b"\r\n\r\n")
            .expect("complete save headers");
        save_stream.write_all(&body).expect("save body");
        let mut save_response = String::new();
        save_stream
            .read_to_string(&mut save_response)
            .expect("save response");
        close_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("restore close read timeout");
        let mut close_response = String::new();
        close_stream
            .read_to_string(&mut close_response)
            .expect("close response");
        let result = server.join().expect("join").expect("serve");
        assert!(
            save_response.starts_with("HTTP/1.1 200 OK"),
            "save response: {save_response}"
        );
        assert!(
            close_response.starts_with("HTTP/1.1 200 OK"),
            "close response: {close_response}"
        );
        assert_eq!(result.written.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn save_header_precedes_later_close_when_save_body_is_delayed() {
        let repo = tempdir().expect("temp repo");
        let path = repo.path().join("graph.yaml");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let token = "test-token".to_owned();
        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        let target = GuiTarget::GraphFile {
            path: path.clone(),
            expected_sha256: None,
            create_only: true,
        };
        let server = spawn_server(
            listener,
            token.clone(),
            graph,
            target,
            ServerTuning::default(),
        );
        let origin = format!("http://{address}");
        let body = save_body();
        let mut save_stream = TcpStream::connect(address).expect("save connect");
        save_stream
            .write_all(
                format!(
                    "POST /api/save HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("save headers");
        let mut close_stream = TcpStream::connect(address).expect("close connect");
        close_stream
            .write_all(
                format!(
                    "POST /api/close HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("close headers");
        save_stream.write_all(&body).expect("save body");
        let mut save_response = String::new();
        save_stream
            .read_to_string(&mut save_response)
            .expect("save response");
        let mut close_response = String::new();
        close_stream
            .read_to_string(&mut close_response)
            .expect("close response");
        let result = server.join().expect("join").expect("serve");
        assert!(
            save_response.starts_with("HTTP/1.1 200 OK"),
            "save response: {save_response}"
        );
        assert!(
            close_response.starts_with("HTTP/1.1 200 OK"),
            "close response: {close_response}"
        );
        assert_eq!(result.written.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn successful_save_survives_broken_reply() {
        let repo = tempdir().expect("temp repo");
        let path = repo.path().join("graph.yaml");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let token = "test-token".to_owned();
        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        let target = GuiTarget::GraphFile {
            path: path.clone(),
            expected_sha256: None,
            create_only: true,
        };
        let server = spawn_server(
            listener,
            token.clone(),
            graph,
            target,
            ServerTuning::default(),
        );
        let origin = format!("http://{address}");
        let body = save_body();
        let mut save_stream = TcpStream::connect(address).expect("save connect");
        save_stream
            .write_all(
                format!(
                    "POST /api/save HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("save headers");
        save_stream.write_all(&body).expect("save body");
        drop(save_stream);
        let state_request = format!(
            "GET /api/state HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nConnection: close\r\n\r\n"
        );
        let state_response = send_request(address, &state_request).expect("state");
        let close_request = format!(
            "POST /api/close HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nX-Gloop-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = send_request(address, &close_request);
        let result = server.join().expect("join").expect("serve");
        assert!(state_response.contains("\"graph\""));
        assert_eq!(result.written.as_deref(), Some(path.as_path()));
        assert!(path.is_file());
    }

    #[test]
    fn build_state_models_include_profile_less_node_models() {
        let mut graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        if let gloop_core::NodeKind::Agent { profile, model, .. } = &mut graph.spec.nodes[0].kind {
            *profile = None;
            *model = Some("legacy-model".to_owned());
        } else {
            panic!("expected agent node");
        }

        let state = build_state(
            &graph,
            &[],
            Language::En,
            &GuiTarget::GraphFile {
                path: PathBuf::from("graph.yaml"),
                expected_sha256: None,
                create_only: true,
            },
        )
        .expect("state");

        assert!(state.models.contains(&"legacy-model".to_owned()));
    }

    #[test]
    fn build_state_models_exclude_profile_defaults() {
        use gloop_provider::CatalogModel;

        let graph =
            Graph::from_yaml_str(include_str!("../../../examples/direct.yaml")).expect("graph");
        let profiles = vec![
            ProfileOption {
                name: "openai".to_owned(),
                kind: "openai".to_owned(),
                enabled: true,
                runtime_default: false,
                default_model: Some("gpt-5".to_owned()),
                models: vec![CatalogModel::uniform("gpt-5")],
                discovery: "unsupported".to_owned(),
                discovery_error: None,
            },
            ProfileOption {
                name: "anthropic".to_owned(),
                kind: "anthropic".to_owned(),
                enabled: true,
                runtime_default: false,
                default_model: Some("claude-opus-4".to_owned()),
                models: vec![CatalogModel::uniform("claude-opus-4")],
                discovery: "unsupported".to_owned(),
                discovery_error: None,
            },
        ];
        let state = build_state(
            &graph,
            &profiles,
            Language::En,
            &GuiTarget::GraphFile {
                path: PathBuf::from("graph.yaml"),
                expected_sha256: None,
                create_only: true,
            },
        )
        .expect("state");
        assert!(!state.models.contains(&"gpt-5".to_owned()));
        assert!(!state.models.contains(&"claude-opus-4".to_owned()));
    }

    #[test]
    fn first_builtin_gui_save_is_create_only() {
        let repo = tempdir().expect("temp repo");
        let path = repo.path().join(".gloop/graphs/direct.yaml");
        let mut target = GuiTarget::GraphFile {
            path: path.clone(),
            expected_sha256: None,
            create_only: true,
        };

        let (_, written) = save_graph(&save_body(), &mut target).expect("create graph");

        assert_eq!(written, path);
        assert!(path.is_file());
        assert!(matches!(
            target,
            GuiTarget::GraphFile {
                expected_sha256: Some(_),
                create_only: false,
                ..
            }
        ));
    }

    #[test]
    fn first_builtin_gui_save_does_not_replace_a_racing_file() {
        let repo = tempdir().expect("temp repo");
        let path = repo.path().join(".gloop/graphs/direct.yaml");
        std::fs::create_dir_all(path.parent().expect("graph parent")).expect("parent");
        std::fs::write(&path, "created elsewhere").expect("racing graph");
        let mut target = GuiTarget::GraphFile {
            path,
            expected_sha256: None,
            create_only: true,
        };

        let error = save_graph(&save_body(), &mut target).expect_err("must refuse overwrite");

        assert!(error.to_string().contains("created at"));
    }
}
