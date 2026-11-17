use anyhow::{bail, Context, Result};
use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, Request, Response, StatusCode, Uri},
    response::{IntoResponse, Json},
    routing::any,
    Router,
};
use clap::{Parser, Subcommand};
use hyper::upgrade;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::Mutex,
};

#[derive(Parser)]
#[command(
    name = "argus-center",
    version,
    about = "Argus reverse relay center (Rust)"
)]
struct Cli {
    #[arg(long, default_value = "./state", global = true)]
    state: PathBuf,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Init {
        #[arg(long)]
        public_url: String,
        #[arg(long, default_value = "127.0.0.1:18999")]
        listen: String,
        #[arg(long, default_value_t = 18998)]
        chisel_port: u16,
        #[arg(long, default_value = "chisel")]
        chisel: String,
    },
    Serve,
    PairCreate {
        #[arg(long)]
        name: String,
        #[arg(long)]
        service: String,
    },
    ServiceAdd {
        #[arg(long)]
        pair: String,
        #[arg(long)]
        service: String,
    },
    PairInvite {
        #[arg(long)]
        name: String,
    },
    JoinApprove {
        #[arg(long)]
        pair: String,
        #[arg(long)]
        proof: String,
    },
    JoinList {
        #[arg(long)]
        pair: Option<String>,
    },
    WatchdogSet {
        #[arg(long)]
        pair: String,
        #[arg(long)]
        service: String,
        #[arg(long)]
        script: PathBuf,
        #[arg(long, default_value_t = 300)]
        cooldown_secs: u64,
    },
    InstallService,
    Status,
}
#[derive(Clone, Serialize, Deserialize)]
struct Service {
    name: String,
    local_port: u16,
    relay_port: u16,
    // Only application-authenticated HTTP/WS allowed in initial exposed MVP.
    protocol: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Watchdog {
    service: String,
    script: String,
    sha256: String,
    cooldown_secs: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Pair {
    name: String,
    username: String,
    password: String,
    revision: u64,
    services: BTreeMap<String, Service>,
    #[serde(default)]
    watchdogs: Vec<Watchdog>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Invitation {
    hash: String,
    pair: String,
    expires: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct JoinRequest {
    proof: String,
    pair: String,
    expires: u64,
    approved: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Config {
    public_url: String,
    listen: String,
    chisel_port: u16,
    chisel_path: String,
    next_port: u16,
    pairs: BTreeMap<String, Pair>,
    invitations: Vec<Invitation>,
    #[serde(default)]
    join_requests: Vec<JoinRequest>,
}
#[derive(Deserialize)]
struct JoinBody {
    proof: String,
}
#[derive(Deserialize)]
struct JoinClaim {
    nonce: String,
}
#[derive(Serialize)]
struct Enrollment {
    center: String,
    pair: String,
    username: String,
    password: String,
    fingerprint: String,
    revision: u64,
    services: Vec<Service>,
    interval_secs: u64,
    watchdogs: Vec<Watchdog>,
}
#[derive(Clone)]
struct App {
    dir: PathBuf,
    http: Client<HttpConnector, Body>,
    write_guard: Arc<Mutex<()>>,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn random_secret() -> String {
    let mut b = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}
fn hash(v: &str) -> String {
    hex::encode(Sha256::digest(v.as_bytes()))
}
fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}
fn read_config(dir: &Path) -> Result<Config> {
    Ok(serde_json::from_slice(
        &fs::read(config_path(dir)).context("center uninitialized")?,
    )?)
}
fn persist(dir: &Path, cfg: &Config) -> Result<()> {
    fs::create_dir_all(dir)?;
    // Atomic on Unix. Windows rename-overwrite needs removing old first.
    let file = config_path(dir);
    let tmp = dir.join("config.json.new");
    fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    if file.exists() {
        fs::remove_file(&file)?;
    }
    fs::rename(tmp, file)?;
    write_authfile(dir, cfg)
}
fn write_authfile(dir: &Path, cfg: &Config) -> Result<()> {
    let mut users: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pair in cfg.pairs.values() {
        let rules: Vec<String> = pair
            .services
            .values()
            .map(|s| format!(r"^R:127\.0\.0\.1:{}$", s.relay_port))
            .collect();
        users.insert(format!("{}:{}", pair.username, pair.password), rules);
    }
    let out = dir.join("chisel-users.json");
    let tmp = dir.join("chisel-users.json.new");
    fs::write(&tmp, serde_json::to_vec_pretty(&users)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    if out.exists() {
        fs::remove_file(&out)?;
    }
    fs::rename(tmp, out)?;
    Ok(())
}
fn parse_service(spec: &str, relay_port: u16) -> Result<Service> {
    let bits: Vec<&str> = spec.split(':').collect();
    if bits.len() != 2 {
        bail!("service must be NAME:LOCAL_PORT");
    }
    let name = bits[0];
    if !safe_name(name) {
        bail!("service name accepts only a-z, 0-9 and '-'");
    }
    let port: u16 = bits[1].parse()?;
    if port == 0 {
        bail!("service local port cannot be zero");
    }
    Ok(Service {
        name: name.into(),
        local_port: port,
        relay_port,
        protocol: "http-ws-app-auth".into(),
    })
}
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}
fn proof_ok(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn join_approve(dir: &Path, pair: String, proof: String) -> Result<()> {
    if !safe_name(&pair) || !proof_ok(&proof) {
        bail!("invalid join approval");
    }
    let mut cfg = read_config(dir)?;
    let req = cfg
        .join_requests
        .iter_mut()
        .find(|x| x.pair == pair && x.proof == proof && x.expires > now())
        .context("no matching active join request")?;
    if req.approved {
        bail!("join request already approved");
    }
    req.approved = true;
    persist(dir, &cfg)?;
    println!("JOIN_APPROVED={pair} PROOF={proof}");
    Ok(())
}
fn join_list(dir: &Path, pair: Option<String>) -> Result<()> {
    let cfg = read_config(dir)?;
    for req in cfg
        .join_requests
        .iter()
        .filter(|x| x.expires > now() && pair.as_deref().is_none_or(|p| p == x.pair))
    {
        println!(
            "JOIN_REQUEST={} PROOF={} APPROVED={} EXPIRES_IN={}",
            req.pair,
            req.proof,
            req.approved,
            req.expires.saturating_sub(now())
        );
    }
    Ok(())
}
fn invite(dir: &Path, cfg: &mut Config, pair: &str) -> Result<()> {
    if !cfg.pairs.contains_key(pair) {
        bail!("unknown pair");
    }
    let token = random_secret();
    cfg.invitations
        .retain(|x| x.expires > now() && x.pair != pair);
    cfg.invitations.push(Invitation {
        hash: hash(&token),
        pair: pair.into(),
        expires: now() + 900,
    });
    persist(dir, cfg)?;
    println!("PAIR={pair}\nVALID_FOR=900 seconds\nENROLL_TOKEN={token}");
    println!(
        "CLIENT_COMMAND=argus-client enroll --center {} --token <ENROLL_TOKEN> --run",
        cfg.public_url
    );
    Ok(())
}
fn init(
    dir: &Path,
    public_url: String,
    listen: String,
    chisel_port: u16,
    chisel: String,
) -> Result<()> {
    if config_path(dir).exists() {
        bail!("center already initialized at {}", dir.display());
    }
    if !public_url.starts_with("https://")
        && !public_url.starts_with("http://127.0.0.1")
        && !public_url.starts_with("http://localhost")
    {
        bail!("public center URL must be HTTPS (loopback HTTP is allowed for tests)");
    }
    fs::create_dir_all(dir)?;
    let key = dir.join("chisel.key");
    let status = std::process::Command::new(&chisel)
        .arg("server")
        .arg("--keygen")
        .arg(&key)
        .status()
        .context("chisel keygen (install chisel first)")?;
    if !status.success() {
        bail!("chisel keygen failed");
    }
    let cfg = Config {
        public_url: public_url.trim_end_matches('/').into(),
        listen,
        chisel_port,
        chisel_path: chisel,
        next_port: 31000,
        pairs: BTreeMap::new(),
        invitations: Vec::new(),
        join_requests: Vec::new(),
    };
    persist(dir, &cfg)?;
    println!("CENTER_INITIALIZED={}", dir.display());
    Ok(())
}
fn pair_create(dir: &Path, name: String, spec: String) -> Result<()> {
    if !safe_name(&name) {
        bail!("invalid pair name");
    }
    let mut cfg = read_config(dir)?;
    if cfg.pairs.contains_key(&name) {
        bail!("pair already exists");
    }
    let service = parse_service(&spec, cfg.next_port)?;
    cfg.next_port = cfg
        .next_port
        .checked_add(1)
        .context("port range exhausted")?;
    let mut services = BTreeMap::new();
    services.insert(service.name.clone(), service);
    cfg.pairs.insert(
        name.clone(),
        Pair {
            name: name.clone(),
            username: format!("p-{name}"),
            password: random_secret(),
            revision: 1,
            services,
            watchdogs: Vec::new(),
        },
    );
    invite(dir, &mut cfg, &name)?;
    println!("PAIR_CREATED={name}");
    Ok(())
}
fn service_add(dir: &Path, pair: String, spec: String) -> Result<()> {
    let mut cfg = read_config(dir)?;
    let service = parse_service(&spec, cfg.next_port)?;
    let item = cfg.pairs.get_mut(&pair).context("unknown pair")?;
    if item.services.contains_key(&service.name) {
        bail!("service already exists");
    }
    cfg.next_port = cfg
        .next_port
        .checked_add(1)
        .context("port range exhausted")?;
    item.services.insert(service.name.clone(), service.clone());
    item.revision += 1;
    let revision = item.revision;
    persist(dir, &cfg)?;
    println!(
        "SERVICE_ADDED=/s/{pair}/{} (revision={})",
        service.name, revision
    );
    Ok(())
}
async fn handle(State(app): State<App>, mut req: Request<Body>) -> Response<Body> {
    let path = req.uri().path().to_string();
    if path == "/healthz" {
        return Json(serde_json::json!({"status":"ok","service":"argus-center"})).into_response();
    }
    if let Some(pairname) = path.strip_prefix("/api/join/request/") {
        if req.method() != axum::http::Method::POST {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        if !safe_name(pairname) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let Ok(bytes) = to_bytes(req.into_body(), 1024).await else {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        };
        let Ok(body) = serde_json::from_slice::<JoinBody>(&bytes) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if !proof_ok(&body.proof) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let _guard = app.write_guard.lock().await;
        let Ok(mut cfg) = read_config(&app.dir) else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        if !cfg.pairs.contains_key(pairname) {
            return StatusCode::NOT_FOUND.into_response();
        }
        cfg.join_requests.retain(|x| x.expires > now());
        if cfg.join_requests.iter().any(|x| x.proof == body.proof) {
            return StatusCode::CONFLICT.into_response();
        }
        if cfg
            .join_requests
            .iter()
            .filter(|x| x.pair == pairname)
            .count()
            >= 8
        {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        cfg.join_requests.push(JoinRequest {
            proof: body.proof.clone(),
            pair: pairname.into(),
            expires: now() + 900,
            approved: false,
        });
        if persist(&app.dir, &cfg).is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        return Json(serde_json::json!({"status":"pending","proof":body.proof,"expires_in":900}))
            .into_response();
    }
    if let Some(pairname) = path.strip_prefix("/api/join/claim/") {
        if req.method() != axum::http::Method::POST {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        if !safe_name(pairname) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let Ok(bytes) = to_bytes(req.into_body(), 1024).await else {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        };
        let Ok(body) = serde_json::from_slice::<JoinClaim>(&bytes) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if !proof_ok(&body.nonce) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let _guard = app.write_guard.lock().await;
        let Ok(mut cfg) = read_config(&app.dir) else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let target = hash(&body.nonce);
        let Some(pos) = cfg
            .join_requests
            .iter()
            .position(|x| x.pair == pairname && x.proof == target && x.expires > now())
        else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        if !cfg.join_requests[pos].approved {
            return (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({"status":"pending"})),
            )
                .into_response();
        }
        let Some(pair) = cfg.pairs.get(pairname).cloned() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let Ok(fingerprint) = fs::read_to_string(app.dir.join("fingerprint.txt")) else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        if fingerprint.trim().is_empty() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        cfg.join_requests.remove(pos);
        if persist(&app.dir, &cfg).is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        return Json(Enrollment {
            center: cfg.public_url,
            pair: pair.name,
            username: pair.username,
            password: pair.password,
            fingerprint: fingerprint.trim().into(),
            revision: pair.revision,
            services: pair.services.into_values().collect(),
            interval_secs: 30,
            watchdogs: pair.watchdogs,
        })
        .into_response();
    }
    if let Some(token) = path.strip_prefix("/api/enroll/") {
        if req.method() != axum::http::Method::POST {
            return (StatusCode::METHOD_NOT_ALLOWED, "POST required").into_response();
        }
        // Atomic invitation consumption per process.
        let _guard = app.write_guard.lock().await;
        let Ok(mut cfg) = read_config(&app.dir) else {
            return (StatusCode::SERVICE_UNAVAILABLE, "state").into_response();
        };
        let target = hash(token);
        let Some(pos) = cfg
            .invitations
            .iter()
            .position(|x| x.hash == target && x.expires > now())
        else {
            return (StatusCode::UNAUTHORIZED, "expired or consumed invitation").into_response();
        };
        let invitation = cfg.invitations.remove(pos);
        let Some(pair) = cfg.pairs.get(&invitation.pair).cloned() else {
            return (StatusCode::BAD_REQUEST, "pair missing").into_response();
        };
        let Ok(fingerprint) = fs::read_to_string(app.dir.join("fingerprint.txt")) else {
            return (StatusCode::SERVICE_UNAVAILABLE, "transport not ready").into_response();
        };
        if fingerprint.trim().is_empty() {
            return (StatusCode::SERVICE_UNAVAILABLE, "fingerprint missing").into_response();
        }
        if persist(&app.dir, &cfg).is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, "persist failure").into_response();
        }
        return Json(Enrollment {
            center: cfg.public_url,
            pair: pair.name,
            username: pair.username,
            password: pair.password,
            fingerprint: fingerprint.trim().into(),
            revision: pair.revision,
            services: pair.services.into_values().collect(),
            interval_secs: 30,
            watchdogs: pair.watchdogs,
        })
        .into_response();
    }
    if let Some(pairname) = path.strip_prefix("/api/sync/") {
        let Ok(cfg) = read_config(&app.dir) else {
            return (StatusCode::INTERNAL_SERVER_ERROR, "state").into_response();
        };
        let Some(pair) = cfg.pairs.get(pairname) else {
            return (StatusCode::NOT_FOUND, "pair").into_response();
        };
        let authorized = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|x| x.to_str().ok())
            .map(|x| x == format!("Bearer {}", pair.password))
            .unwrap_or(false);
        if !authorized {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        };
        return Json(Enrollment {
            center: cfg.public_url,
            pair: pair.name.clone(),
            username: pair.username.clone(),
            password: pair.password.clone(),
            fingerprint: fs::read_to_string(app.dir.join("fingerprint.txt"))
                .unwrap_or_default()
                .trim()
                .into(),
            revision: pair.revision,
            services: pair.services.values().cloned().collect(),
            interval_secs: 30,
            watchdogs: pair.watchdogs.clone(),
        })
        .into_response();
    }
    let Ok(cfg) = read_config(&app.dir) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "state").into_response();
    };
    let (target_port, new_path) = if path.starts_with("/_wire/") {
        let Some(pairname) = path.split('/').nth(2) else {
            return (StatusCode::NOT_FOUND, "unknown").into_response();
        };
        if !cfg.pairs.contains_key(pairname) {
            return (StatusCode::NOT_FOUND, "unknown pair").into_response();
        }
        (cfg.chisel_port, path.clone())
    } else if let Some(rest) = path.strip_prefix("/s/") {
        let mut parts = rest.splitn(3, '/');
        let Some(pairname) = parts.next() else {
            return (StatusCode::NOT_FOUND, "pair").into_response();
        };
        let Some(svcname) = parts.next() else {
            return (StatusCode::NOT_FOUND, "service").into_response();
        };
        let Some(service) = cfg
            .pairs
            .get(pairname)
            .and_then(|p| p.services.get(svcname))
        else {
            return (StatusCode::NOT_FOUND, "unknown service").into_response();
        };
        let suffix = parts.next().unwrap_or("");
        (service.relay_port, format!("/{suffix}"))
    } else {
        return (StatusCode::NOT_FOUND, "unknown route").into_response();
    };
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let uri: Uri = match format!("http://127.0.0.1:{target_port}{new_path}{query}").parse() {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid URI").into_response(),
    };
    *req.uri_mut() = uri;
    req.headers_mut().insert(
        header::HOST,
        format!("127.0.0.1:{target_port}").parse().unwrap(),
    );
    // Both Chisel and remote Orca may require HTTP Upgrade (WebSocket). Proxy
    // the two upgraded TCP streams byte-for-byte, not decoded WebSocket frames.
    let wants_upgrade = req.headers().get(header::UPGRADE).is_some();
    let downstream = if wants_upgrade {
        Some(upgrade::on(&mut req))
    } else {
        None
    };
    match app.http.request(req).await {
        Ok(mut upstream) => {
            if upstream.status() == StatusCode::SWITCHING_PROTOCOLS {
                if let Some(downstream) = downstream {
                    let upstream_upgrade = upgrade::on(&mut upstream);
                    tokio::spawn(async move {
                        if let (Ok(a), Ok(b)) = tokio::join!(downstream, upstream_upgrade) {
                            let (mut a, mut b) = (TokioIo::new(a), TokioIo::new(b));
                            let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                        }
                    });
                }
            }
            upstream.map(Body::new)
        }
        Err(_) => (StatusCode::BAD_GATEWAY, "reverse listener offline").into_response(),
    }
}
fn install_service(dir: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let state = fs::canonicalize(dir).context("center state directory missing")?;
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var_os("HOME").context("HOME missing")?;
        let unit_dir = PathBuf::from(home).join(".config/systemd/user");
        fs::create_dir_all(&unit_dir)?;
        let unit = unit_dir.join("argus-center.service");
        let text=format!("[Unit]\nDescription=Argus Relay Center\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart={} --state {} serve\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",exe.display(),state.display());
        fs::write(&unit, text)?;
        let status = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()?;
        if !status.success() {
            bail!("systemd reload failed");
        }
        let status = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", "argus-center.service"])
            .status()?;
        if !status.success() {
            bail!("systemd enable failed");
        }
        println!("CENTER_AUTOSTART_INSTALLED={}", unit.display());
    }
    #[cfg(windows)]
    {
        let launch = format!(
            "\\\"{}\\\" --state \\\"{}\\\" serve",
            exe.display(),
            state.display()
        );
        let status = std::process::Command::new("schtasks")
            .args([
                "/Create",
                "/F",
                "/SC",
                "ONLOGON",
                "/TN",
                "Argus-Center",
                "/TR",
                &launch,
            ])
            .status()?;
        if !status.success() {
            bail!("Windows scheduled task registration failed");
        }
        println!("CENTER_AUTOSTART_INSTALLED=Argus-Center (on login)");
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    bail!("service installer not implemented on this OS");
    Ok(())
}
async fn serve(dir: PathBuf) -> Result<()> {
    let cfg = read_config(&dir)?;
    let key = dir.join("chisel.key");
    let users = dir.join("chisel-users.json");
    let chisel_path = cfg.chisel_path.clone();
    let port = cfg.chisel_port;
    let key2 = key.clone();
    let users2 = users.clone();
    let dir2 = dir.clone();
    tokio::spawn(async move {
        loop {
            let mut c = Command::new(&chisel_path);
            c.arg("server")
                .arg("--host")
                .arg("127.0.0.1")
                .arg("--port")
                .arg(port.to_string())
                .arg("--reverse")
                .arg("--keyfile")
                .arg(&key2)
                .arg("--authfile")
                .arg(&users2)
                .stderr(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            match c.spawn() {
                Ok(mut child) => {
                    if let Some(stderr) = child.stderr.take() {
                        let dir = dir2.clone();
                        tokio::spawn(async move {
                            let mut lines = BufReader::new(stderr).lines();
                            while let Ok(Some(line)) = lines.next_line().await {
                                if let Some(f) = line.split("Fingerprint ").nth(1) {
                                    let candidate =
                                        f.trim().split_whitespace().next().unwrap_or("");
                                    if !candidate.is_empty() {
                                        let _ = fs::write(dir.join("fingerprint.txt"), candidate);
                                    }
                                }
                                if line.contains("error") {
                                    eprintln!("chisel: {line}");
                                }
                            }
                        });
                    }
                    let _ = child.wait().await;
                    eprintln!("Chisel exited. Restarting...");
                }
                Err(e) => eprintln!("Unable to spawn Chisel: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
    let health_dir = dir.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(config) = read_config(&health_dir) {
                let mut snapshot = BTreeMap::<String, bool>::new();
                for (pair, p) in &config.pairs {
                    for (service, s) in &p.services {
                        let address = format!("127.0.0.1:{}", s.relay_port);
                        let online = tokio::time::timeout(
                            Duration::from_millis(700),
                            tokio::net::TcpStream::connect(address),
                        )
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false);
                        snapshot.insert(format!("{pair}/{service}"), online);
                    }
                }
                if let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) {
                    let _ = fs::write(health_dir.join("health.json"), bytes);
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    let app = App {
        dir,
        http: Client::builder(TokioExecutor::new()).build_http(),
        write_guard: Arc::new(Mutex::new(())),
    };
    let router = Router::new().fallback(any(handle)).with_state(app);
    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    println!("ARGUS_CENTER_READY=http://{}", cfg.listen);
    axum::serve(listener, router).await?;
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Action::Init {
            public_url,
            listen,
            chisel_port,
            chisel,
        } => init(&cli.state, public_url, listen, chisel_port, chisel),
        Action::Serve => serve(cli.state).await,
        Action::PairCreate { name, service } => pair_create(&cli.state, name, service),
        Action::ServiceAdd { pair, service } => service_add(&cli.state, pair, service),
        Action::PairInvite { name } => {
            let mut cfg = read_config(&cli.state)?;
            invite(&cli.state, &mut cfg, &name)
        }
        Action::JoinApprove { pair, proof } => join_approve(&cli.state, pair, proof),
        Action::JoinList { pair } => join_list(&cli.state, pair),
        Action::WatchdogSet {
            pair,
            service,
            script,
            cooldown_secs,
        } => {
            if !safe_name(&pair) || !safe_name(&service) {
                bail!("invalid pair/service");
            }
            let mut cfg = read_config(&cli.state)?;
            let item = cfg.pairs.get_mut(&pair).context("unknown pair")?;
            if !item.services.contains_key(&service) {
                bail!("unknown service");
            }
            let content = fs::read_to_string(&script)?;
            if content.len() > 16384 || content.trim().is_empty() {
                bail!("script must be 1..16384 bytes");
            }
            let spec = Watchdog {
                service: service.clone(),
                sha256: hash(&content),
                script: content,
                cooldown_secs: cooldown_secs.max(60),
            };
            item.watchdogs.retain(|w| w.service != service);
            item.watchdogs.push(spec);
            item.revision += 1;
            persist(&cli.state, &cfg)?;
            println!("WATCHDOG_CONFIGURED={pair}/{service}");
            Ok(())
        }
        Action::InstallService => install_service(&cli.state),
        Action::Status => {
            let cfg = read_config(&cli.state)?;
            println!(
                "CENTER={} BIND={} REVERSE_INGRESS={}",
                cfg.public_url, cfg.listen, cfg.chisel_port
            );
            for p in cfg.pairs.values() {
                for s in p.services.values() {
                    println!(
                        "PAIR={} SERVICE={} REVISION={} LOCAL={} RELAY={} URL={}/s/{}/{}",
                        p.name,
                        s.name,
                        p.revision,
                        s.local_port,
                        s.relay_port,
                        cfg.public_url,
                        p.name,
                        s.name
                    );
                }
            }
            if let Ok(health) = fs::read_to_string(cli.state.join("health.json")) {
                println!("REVERSE_LISTENER_HEALTH={}", health.replace("\n", ""));
            }
            Ok(())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_parser_rejects_invalid_names() {
        assert!(parse_service("abc:6768", 31000).is_ok());
        assert!(parse_service("../oops:2", 31000).is_err());
        assert!(parse_service("ssh:0", 31000).is_err());
    }
    #[test]
    fn join_proof_rejects_malformed() {
        assert!(proof_ok(&"a".repeat(64)));
        assert!(!proof_ok(&"a".repeat(63)));
        assert!(!proof_ok(&"A".repeat(64)));
        assert!(!proof_ok(&"g".repeat(64)));
    }
    #[test]
    fn token_hash_is_not_token() {
        let token = random_secret();
        assert_ne!(token, hash(&token));
        assert_eq!(hash(&token), hash(&token));
    }
}
