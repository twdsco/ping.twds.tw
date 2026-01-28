use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU16, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, mpsc, RwLock};
use tower_http::services::ServeDir;
use trippy_core::{Builder, State as TracerState, Tracer};

static TRACE_ID: AtomicU16 = AtomicU16::new(0);
static SEQ_OFFSET: AtomicU16 = AtomicU16::new(0);

fn next_tracer_ids() -> (u16, u16) {
    let id = TRACE_ID.fetch_add(1, Ordering::Relaxed);
    let seq = SEQ_OFFSET.fetch_add(256, Ordering::Relaxed); // spread sequences apart
    (id, seq)
}

// ============ Config ============

#[derive(Debug, Deserialize, Clone)]
struct Config {
    custom_duration_secs: u64,
    sources: Vec<Source>,
    destinations: Vec<Destination>,
    #[serde(default = "default_max_custom_per_user")]
    max_custom_per_user: usize,
    #[serde(default = "default_max_custom_global")]
    max_custom_global: usize,
}

fn default_max_custom_per_user() -> usize { 10 }
fn default_max_custom_global() -> usize { 75 }

#[derive(Debug, Deserialize, Clone)]
struct Source {
    name: String,
    ip: IpAddr,
}

#[derive(Debug, Deserialize, Clone)]
struct Destination {
    host: String,
}

// ============ Messages ============

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg {
    Config { sources: Vec<String> },
    SharedUpdate { data: HashMap<String, Vec<MtrResult>> },
    CustomUpdate { dest: String, remaining_secs: u64, results: Vec<MtrResult> },
    CustomComplete { dest: String, results: Vec<MtrResult> },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    AddDestination { dest: String },
}

#[derive(Serialize, Clone, Default)]
struct MtrResult {
    source: String,
    avg_ms: Option<f64>,
    hops: Vec<HopInfo>,
}

#[derive(Serialize, Clone)]
struct HopInfo {
    ttl: u8,
    addr: String,
    ptr: String,
    avg_ms: f64,
    best_ms: Option<f64>,
    worst_ms: Option<f64>,
    loss_pct: f64,
    sent: usize,
    recv: usize,
}

// ============ App State ============

struct AppState {
    config: Config,
    shared_results: Arc<RwLock<HashMap<String, Vec<MtrResult>>>>,
    broadcast_tx: broadcast::Sender<ServerMsg>,
    global_custom_count: Arc<AtomicUsize>,
}

// ============ MTR Functions ============

fn get_interface_for_ip(ip: &IpAddr) -> Option<String> {
    use std::process::Command;
    let output = Command::new("ip")
        .args(["-o", "addr", "show"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains(&ip.to_string()) {
            return line.split_whitespace().nth(1).map(|s| s.to_string());
        }
    }
    None
}

fn resolve_ptr(addr: &IpAddr) -> String {
    dns_lookup::lookup_addr(addr).unwrap_or_default()
}

fn extract_results(state: &TracerState, source_name: &str, target: IpAddr) -> MtrResult {
    let hops: Vec<HopInfo> = state
        .hops()
        .iter()
        .filter(|h| h.total_recv() > 0)
        .map(|h| {
            let addr = h.addrs().next().copied();
            HopInfo {
                ttl: h.ttl(),
                addr: addr.map(|a| a.to_string()).unwrap_or_default(),
                ptr: addr.map(|a| resolve_ptr(&a)).unwrap_or_default(),
                avg_ms: h.avg_ms(),
                best_ms: h.best_ms(),
                worst_ms: h.worst_ms(),
                loss_pct: h.loss_pct(),
                sent: h.total_sent(),
                recv: h.total_recv(),
            }
        })
        .collect();

    // Only show avg_ms if the final destination was reached
    let avg_ms = hops.last().and_then(|h| {
        if h.addr == target.to_string() {
            Some(h.avg_ms)
        } else {
            None
        }
    });

    MtrResult {
        source: source_name.to_string(),
        avg_ms,
        hops,
    }
}

async fn run_mtr(
    source: Source,
    dest: IpAddr,
    dest_str: String,
    results: Arc<RwLock<HashMap<String, Vec<MtrResult>>>>,
) {
    let interface = get_interface_for_ip(&source.ip);
    let (trace_id, initial_seq) = next_tracer_ids();
    let mut builder = Builder::new(dest)
        .source_addr(Some(source.ip))
        .trace_identifier(trace_id)
        .initial_sequence(initial_seq)
        .max_samples(10);
    
    if let Some(ref iface) = interface {
        builder = builder.interface(Some(iface.clone()));
    }

    let tracer = match builder.build()
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to create tracer for {} -> {}: {}", source.name, dest_str, e);
            return;
        }
    };

    let (tracer, _handle) = match tracer.spawn() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to spawn tracer: {}", e);
            return;
        }
    };

    let source_name = source.name.clone();
    let dest_key = dest_str.clone();

    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let state = tracer.snapshot();
        let result = extract_results(&state, &source_name, dest);
        
        let mut map = results.write().await;
        let entry = map.entry(dest_key.clone()).or_insert_with(Vec::new);
        if let Some(existing) = entry.iter_mut().find(|r| r.source == source_name) {
            *existing = result;
        } else {
            entry.push(result);
        }
    }
}

async fn run_custom_mtr(
    sources: Vec<Source>,
    dest: String,
    duration_secs: u64,
    tx: mpsc::Sender<ServerMsg>,
) {
    let dest_ip: IpAddr = match dest.parse().or_else(|_| {
        use std::net::ToSocketAddrs;
        format!("{}:0", dest)
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .map(|s| s.ip())
            .ok_or(())
    }) {
        Ok(ip) => ip,
        Err(_) => {
            let _ = tx.send(ServerMsg::CustomComplete {
                dest,
                results: vec![],
            }).await;
            return;
        }
    };

    let mut tracers: Vec<(String, Tracer)> = vec![];
    
    for source in &sources {
        let interface = get_interface_for_ip(&source.ip);
        let (trace_id, initial_seq) = next_tracer_ids();
        let mut builder = Builder::new(dest_ip)
            .source_addr(Some(source.ip))
            .trace_identifier(trace_id)
            .initial_sequence(initial_seq)
            .max_samples(10);
        
        if let Some(ref iface) = interface {
            builder = builder.interface(Some(iface.clone()));
        }

        let tracer = match builder.build()
        {
            Ok(t) => t,
            Err(_) => continue,
        };

        match tracer.spawn() {
            Ok((t, _)) => tracers.push((source.name.clone(), t)),
            Err(_) => continue,
        }
    }

    let start = Instant::now();

    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let elapsed = start.elapsed().as_secs();
        
        let results: Vec<MtrResult> = tracers
            .iter()
            .map(|(name, tracer)| extract_results(&tracer.snapshot(), name, dest_ip))
            .collect();

        if elapsed >= duration_secs {
            let _ = tx.send(ServerMsg::CustomComplete {
                dest: dest.clone(),
                results,
            }).await;
            break;
        }

        let remaining = duration_secs - elapsed;
        let _ = tx.send(ServerMsg::CustomUpdate {
            dest: dest.clone(),
            remaining_secs: remaining,
            results,
        }).await;
    }
}

// ============ WebSocket Handler ============

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();
    let mut broadcast_rx = state.broadcast_tx.subscribe();
    let (custom_tx, mut custom_rx) = mpsc::channel::<ServerMsg>(100);

    // Send config
    let config_msg = ServerMsg::Config {
        sources: state.config.sources.iter().map(|s| s.name.clone()).collect(),
    };
    let _ = sender.send(Message::Text(serde_json::to_string(&config_msg).unwrap().into())).await;

    // Send current shared results
    let data = state.shared_results.read().await.clone();
    let msg = ServerMsg::SharedUpdate { data };
    let _ = sender.send(Message::Text(serde_json::to_string(&msg).unwrap().into())).await;

    let sources = state.config.sources.clone();
    let duration = state.config.custom_duration_secs;
    let max_per_user = state.config.max_custom_per_user;
    let max_global = state.config.max_custom_global;
    let global_count = state.global_custom_count.clone();
    let mut active_custom: usize = 0;

    loop {
        tokio::select! {
            Some(msg) = receiver.next() => {
                if let Ok(Message::Text(text)) = msg {
                    if let Ok(ClientMsg::AddDestination { dest }) = serde_json::from_str(&text) {
                        if active_custom >= max_per_user {
                            continue;
                        }
                        if global_count.load(Ordering::Relaxed) >= max_global {
                            continue;
                        }
                        active_custom += 1;
                        global_count.fetch_add(1, Ordering::Relaxed);
                        let tx = custom_tx.clone();
                        let srcs = sources.clone();
                        let gc = global_count.clone();
                        tokio::spawn(async move {
                            run_custom_mtr(srcs, dest, duration, tx).await;
                            gc.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                }
            }
            Ok(msg) = broadcast_rx.recv() => {
                let _ = sender.send(Message::Text(serde_json::to_string(&msg).unwrap().into())).await;
            }
            Some(msg) = custom_rx.recv() => {
                if matches!(msg, ServerMsg::CustomComplete { .. }) {
                    active_custom = active_custom.saturating_sub(1);
                }
                let _ = sender.send(Message::Text(serde_json::to_string(&msg).unwrap().into())).await;
            }
            else => break,
        }
    }
}

// ============ Main ============

#[tokio::main]
async fn main() {
    let config_str = std::fs::read_to_string("config.toml").expect("Failed to read config.toml");
    let config: Config = toml::from_str(&config_str).expect("Failed to parse config.toml");

    println!("Loaded config:");
    println!("  Sources: {:?}", config.sources.iter().map(|s| (&s.name, &s.ip)).collect::<Vec<_>>());
    println!("  Destinations: {:?}", config.destinations.iter().map(|d| &d.host).collect::<Vec<_>>());
    println!("  Custom duration: {}s", config.custom_duration_secs);

    let (broadcast_tx, _) = broadcast::channel::<ServerMsg>(100);
    let shared_results = Arc::new(RwLock::new(HashMap::new()));

    // Start shared MTR tasks
    for dest in &config.destinations {
        let dest_ip: IpAddr = match dest.host.parse().or_else(|_| {
            use std::net::ToSocketAddrs;
            format!("{}:0", dest.host)
                .to_socket_addrs()
                .ok()
                .and_then(|mut addrs| addrs.next())
                .map(|s| s.ip())
                .ok_or(())
        }) {
            Ok(ip) => ip,
            Err(_) => {
                eprintln!("Failed to resolve destination: {}", dest.host);
                continue;
            }
        };
        for source in &config.sources {
            let source = source.clone();
            let dest_str = dest.host.clone();
            let results = shared_results.clone();
            tokio::spawn(run_mtr(source, dest_ip, dest_str, results));
        }
    }

    // Broadcast shared results once per second
    let broadcast_results = shared_results.clone();
    let broadcast_tx_clone = broadcast_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let data = broadcast_results.read().await.clone();
            let _ = broadcast_tx_clone.send(ServerMsg::SharedUpdate { data });
        }
    });

    let state = Arc::new(AppState {
        config,
        shared_results,
        broadcast_tx,
        global_custom_count: Arc::new(AtomicUsize::new(0)),
    });

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .nest_service("/", ServeDir::new("static"))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 10000));
    println!("Server running on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
