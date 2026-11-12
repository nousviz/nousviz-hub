// Local NousViz instance: Hermes-style setup wizard + lifecycle management.
//
// The Hub downloads everything it needs into its own app-data dir (uv →
// Python 3.12, Node LTS, portable Postgres via postgresql_embedded, the
// latest nousviz-app release), runs core's own scripts/setup.sh against the
// embedded database, builds the SPA once, then runs the API/worker/scheduler
// as child processes and serves the built frontend + /api proxy from a tiny
// built-in HTTP server. The result registers as a normal instance row.
//
// macOS + Linux only: NousViz core's setup.sh refuses native Windows.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

const NOUSVIZ_REPO: &str = "nousviz/nousviz-app";
const NODE_DIST_BASE: &str = "https://nodejs.org/dist/latest-v20.x";
const USER_AGENT: &str = "nousviz-hub";

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct LocalConfig {
    version: Option<String>,
    pg_port: u16,
    api_port: u16,
    web_port: u16,
    pg_password: String,
    installed: bool,
}

#[derive(Default)]
struct Procs {
    api: Option<Child>,
    worker: Option<Child>,
    scheduler: Option<Child>,
    web: Option<Arc<tiny_http::Server>>,
    pg: Option<postgresql_embedded::blocking::PostgreSQL>,
}

pub struct LocalState {
    base: PathBuf,
    config: Mutex<LocalConfig>,
    procs: Mutex<Procs>,
    setup_running: Mutex<bool>,
}

#[derive(Serialize)]
pub struct LocalStatus {
    supported: bool,
    installed: bool,
    running: bool,
    setup_running: bool,
    version: Option<String>,
    url: Option<String>,
}

// ── State init ────────────────────────────────────────────────────────

pub fn init(app: &tauri::App) {
    let base = app
        .path()
        .app_data_dir()
        .expect("no app data dir")
        .join("local");
    let config = load_local_config(&base);
    let installed = config.installed;
    app.manage(LocalState {
        base,
        config: Mutex::new(config),
        procs: Mutex::new(Procs::default()),
        setup_running: Mutex::new(false),
    });

    // Auto-start the local stack on launch, like a Hermes gateway.
    if installed && supported() {
        let handle = app.handle().clone();
        std::thread::spawn(move || {
            if let Err(e) = start_stack(&handle) {
                log_line(&handle, &format!("Local instance auto-start failed: {e}"));
            }
        });
    }

    // Debug/test hook: NOUSVIZ_HUB_AUTOSETUP=1 runs the setup wizard on
    // launch without UI interaction (used for headless end-to-end testing).
    if supported() && std::env::var("NOUSVIZ_HUB_AUTOSETUP").is_ok() {
        let handle = app.handle().clone();
        let state = app.state::<LocalState>();
        *state.setup_running.lock().unwrap() = true;
        std::thread::spawn(move || run_setup(handle));
    }
}

pub fn shutdown(app: &AppHandle) {
    if let Some(state) = app.try_state::<LocalState>() {
        stop_stack(&state);
    }
}

fn supported() -> bool {
    cfg!(unix)
}

fn config_path(base: &Path) -> PathBuf {
    base.join("local.json")
}

fn load_local_config(base: &Path) -> LocalConfig {
    fs::read_to_string(config_path(base))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_local_config(state: &LocalState) -> Result<(), String> {
    let config = state.config.lock().unwrap();
    fs::create_dir_all(&state.base).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&*config).map_err(|e| e.to_string())?;
    fs::write(config_path(&state.base), json).map_err(|e| e.to_string())
}

fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| e.to_string())
}

// ── Event helpers ─────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
struct StepEvent<'a> {
    step: &'a str,
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

fn step(app: &AppHandle, step: &str, status: &str, detail: Option<String>) {
    eprintln!("[local-setup] step {step}: {status} {}", detail.as_deref().unwrap_or(""));
    let _ = app.emit("local-setup-progress", StepEvent { step, status, detail });
}

fn log_line(app: &AppHandle, line: &str) {
    eprintln!("[local-setup] {line}");
    let _ = app.emit("local-setup-log", line.to_string());
}

// ── Subprocess + download helpers ─────────────────────────────────────

/// Run a command, streaming stdout+stderr lines to the wizard log.
fn run_logged(app: &AppHandle, cmd: &mut Command, what: &str) -> Result<(), String> {
    log_line(app, &format!("$ {what}"));
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{what}: failed to start: {e}"))?;

    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let h = app.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                log_line(&h, &line);
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        let h = app.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                log_line(&h, &line);
            }
        }));
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    for r in readers {
        let _ = r.join();
    }
    if !status.success() {
        return Err(format!("{what} exited with {status}"));
    }
    Ok(())
}

fn blocking_client(timeout: Option<std::time::Duration>) -> Result<reqwest::blocking::Client, String> {
    let mut b = reqwest::blocking::Client::builder().user_agent(USER_AGENT);
    b = match timeout {
        Some(t) => b.timeout(t),
        None => b.timeout(std::time::Duration::from_secs(3600)),
    };
    b.build().map_err(|e| e.to_string())
}

fn download(app: &AppHandle, url: &str, dest: &Path) -> Result<(), String> {
    log_line(app, &format!("Downloading {url}"));
    let res = blocking_client(None)?
        .get(url)
        .send()
        .map_err(|e| format!("download {url}: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("download {url}: HTTP {}", res.status()));
    }
    let bytes = res.bytes().map_err(|e| e.to_string())?;
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fs::write(dest, &bytes).map_err(|e| e.to_string())?;
    log_line(app, &format!("Saved {} ({} bytes)", dest.display(), bytes.len()));
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("shasum failed".into());
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
        .ok_or_else(|| "shasum produced no output".into())
}

fn extract_tgz(app: &AppHandle, archive: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    run_logged(
        app,
        Command::new("tar")
            .arg("xzf")
            .arg(archive)
            .arg("--strip-components=1")
            .arg("-C")
            .arg(dest),
        &format!("tar xzf {}", archive.file_name().unwrap_or_default().to_string_lossy()),
    )
}

// ── Paths ─────────────────────────────────────────────────────────────

struct Dirs {
    tools_uv: PathBuf,
    uv_bin: PathBuf,
    python_root: PathBuf,
    pybin: PathBuf,
    node: PathBuf,
    pg_install: PathBuf,
    pg_data: PathBuf,
    pg_passfile: PathBuf,
    app: PathBuf,
    logs: PathBuf,
    tmp: PathBuf,
}

fn dirs(base: &Path) -> Dirs {
    Dirs {
        tools_uv: base.join("tools/uv"),
        uv_bin: base.join("tools/uv/uv"),
        python_root: base.join("tools/python"),
        pybin: base.join("tools/pybin"),
        node: base.join("tools/node"),
        pg_install: base.join("pg"),
        pg_data: base.join("pgdata"),
        pg_passfile: base.join(".pgpass"),
        app: base.join("app"),
        logs: base.join("logs"),
        tmp: base.join("tmp"),
    }
}

/// PATH for child processes: our toolchain first, then the system basics.
fn tool_path(d: &Dirs) -> String {
    format!(
        "{}:{}:{}:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        d.pybin.display(),
        d.node.join("bin").display(),
        d.pg_install.join("bin").display(),
    )
}

// ── Setup steps ───────────────────────────────────────────────────────

fn step_uv(app: &AppHandle, d: &Dirs) -> Result<(), String> {
    if d.uv_bin.exists() {
        log_line(app, "uv already installed — skipping");
        return Ok(());
    }
    fs::create_dir_all(&d.tools_uv).map_err(|e| e.to_string())?;
    run_logged(
        app,
        Command::new("bash")
            .args(["-c", "curl -LsSf https://astral.sh/uv/install.sh | sh"])
            .env("UV_INSTALL_DIR", &d.tools_uv)
            .env("UV_NO_MODIFY_PATH", "1"),
        "install uv",
    )?;
    if !d.uv_bin.exists() {
        return Err("uv installer finished but uv binary not found".into());
    }
    Ok(())
}

fn step_python(app: &AppHandle, d: &Dirs) -> Result<(), String> {
    let python3 = d.pybin.join("python3");
    if python3.exists() {
        log_line(app, "Python 3.12 already installed — skipping");
        return Ok(());
    }
    run_logged(
        app,
        Command::new(&d.uv_bin)
            .args(["python", "install", "3.12"])
            .env("UV_PYTHON_INSTALL_DIR", &d.python_root)
            .env("UV_MANAGED_PYTHON", "1"),
        "uv python install 3.12",
    )?;
    // UV_MANAGED_PYTHON=1: without it, `uv python find` can return some
    // unrelated interpreter (e.g. an activated venv in $HOME).
    let out = Command::new(&d.uv_bin)
        .args(["python", "find", "3.12"])
        .env("UV_PYTHON_INSTALL_DIR", &d.python_root)
        .env("UV_MANAGED_PYTHON", "1")
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("uv python find 3.12 failed".into());
    }
    let interpreter = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if interpreter.is_empty() {
        return Err("uv python find returned no interpreter".into());
    }
    log_line(app, &format!("Python interpreter: {interpreter}"));
    fs::create_dir_all(&d.pybin).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        for name in ["python3", "python"] {
            let link = d.pybin.join(name);
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(&interpreter, &link).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn node_platform() -> Result<(String, String), String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => return Err(format!("unsupported OS for local install: {other}")),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => return Err(format!("unsupported arch: {other}")),
    };
    Ok((os.to_string(), arch.to_string()))
}

fn step_node(app: &AppHandle, d: &Dirs) -> Result<(), String> {
    if d.node.join("bin/node").exists() {
        log_line(app, "Node.js already installed — skipping");
        return Ok(());
    }
    let (os, arch) = node_platform()?;
    let sums = blocking_client(Some(std::time::Duration::from_secs(30)))?
        .get(format!("{NODE_DIST_BASE}/SHASUMS256.txt"))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(|e| format!("fetch node checksums: {e}"))?;
    let suffix = format!("-{os}-{arch}.tar.gz");
    let (want_sha, filename) = sums
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        })
        .find(|(_, f)| f.ends_with(&suffix))
        .ok_or(format!("no Node build for {os}-{arch} in SHASUMS256.txt"))?;

    let archive = d.tmp.join(&filename);
    download(app, &format!("{NODE_DIST_BASE}/{filename}"), &archive)?;
    let got = sha256_file(&archive)?;
    if got != want_sha {
        return Err(format!("Node checksum mismatch: expected {want_sha}, got {got}"));
    }
    log_line(app, "Node checksum verified");
    extract_tgz(app, &archive, &d.node)?;
    let _ = fs::remove_file(&archive);
    Ok(())
}

/// True when a postmaster started from our data dir is still alive — e.g.
/// after a hard kill of the Hub, where the daemonized server survives.
fn pg_orphan_alive(data_dir: &Path) -> bool {
    let Ok(pidfile) = fs::read_to_string(data_dir.join("postmaster.pid")) else {
        return false;
    };
    let Some(pid) = pidfile.lines().next().and_then(|l| l.trim().parse::<i32>().ok()) else {
        return false;
    };
    #[cfg(unix)]
    unsafe {
        return libc_kill_check(pid);
    }
    #[cfg(not(unix))]
    false
}

#[cfg(unix)]
unsafe fn libc_kill_check(pid: i32) -> bool {
    // kill(pid, 0) checks existence without sending a signal.
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, 0) == 0 }
}

fn pg_settings(state: &LocalState, d: &Dirs) -> postgresql_embedded::Settings {
    let cfg = state.config.lock().unwrap();
    let mut s = postgresql_embedded::Settings::default();
    s.version = postgresql_embedded::V16.clone();
    s.installation_dir = d.pg_install.clone();
    s.data_dir = d.pg_data.clone();
    s.password_file = d.pg_passfile.clone();
    s.host = "127.0.0.1".to_string();
    s.port = cfg.pg_port;
    s.password = cfg.pg_password.clone();
    s.temporary = false;
    s.timeout = Some(std::time::Duration::from_secs(300));
    s
}

fn step_postgres(app: &AppHandle, state: &LocalState, d: &Dirs) -> Result<(), String> {
    {
        let procs = state.procs.lock().unwrap();
        if let Some(pg) = &procs.pg {
            if pg.status() == postgresql_embedded::Status::Started {
                log_line(app, "Postgres already running — skipping");
                return Ok(());
            }
        }
    }
    log_line(app, "Setting up embedded PostgreSQL 16 (downloads on first run)...");
    let mut pg = postgresql_embedded::blocking::PostgreSQL::new(pg_settings(state, d));
    if pg_orphan_alive(&d.pg_data) {
        log_line(app, "Embedded Postgres already running from a previous session — reusing it");
    } else {
        pg.setup().map_err(|e| format!("postgres setup: {e}"))?;
        pg.start().map_err(|e| format!("postgres start: {e}"))?;
    }
    log_line(
        app,
        &format!("PostgreSQL running on 127.0.0.1:{}", pg.settings().port),
    );

    // Create a superuser role named after the OS user so core setup.sh's
    // macOS `psql -U $USER` superuser path works (with PGPASSWORD set).
    let os_user = std::env::var("USER").map_err(|_| "no $USER in environment")?;
    let (port, password) = {
        let cfg = state.config.lock().unwrap();
        (cfg.pg_port, cfg.pg_password.clone())
    };
    let psql = pg.settings().binary_dir().join("psql");
    let sql = format!(
        "DO $$ BEGIN \
           IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{os_user}') THEN \
             CREATE ROLE \"{os_user}\" LOGIN SUPERUSER PASSWORD '{password}'; \
           ELSE \
             ALTER ROLE \"{os_user}\" WITH LOGIN SUPERUSER PASSWORD '{password}'; \
           END IF; \
         END $$;"
    );
    run_logged(
        app,
        Command::new(&psql)
            .args(["-h", "127.0.0.1", "-p", &port.to_string(), "-U", "postgres", "-d", "postgres", "-v", "ON_ERROR_STOP=1", "-c", &sql])
            .env("PGPASSWORD", &password),
        "create OS-user superuser role",
    )?;

    state.procs.lock().unwrap().pg = Some(pg);
    Ok(())
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

fn step_release(app: &AppHandle, state: &LocalState, d: &Dirs) -> Result<String, String> {
    let rel: GhRelease = blocking_client(Some(std::time::Duration::from_secs(30)))?
        .get(format!("https://api.github.com/repos/{NOUSVIZ_REPO}/releases/latest"))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("fetch latest release: {e}"))?;
    let version = rel.tag_name.trim_start_matches('v').to_string();
    log_line(app, &format!("Latest NousViz release: {}", rel.tag_name));

    let installed_version = fs::read_to_string(d.app.join("VERSION"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if installed_version == version {
        log_line(app, "NousViz source already present — skipping download");
        return Ok(version);
    }

    let tarball = rel
        .assets
        .iter()
        .find(|a| a.name.ends_with(".tar.gz"))
        .ok_or("release has no .tar.gz asset")?;
    let sha_asset = rel.assets.iter().find(|a| a.name.ends_with(".sha256"));

    let archive = d.tmp.join(&tarball.name);
    download(app, &tarball.browser_download_url, &archive)?;
    if let Some(sha_asset) = sha_asset {
        let sha_path = d.tmp.join(&sha_asset.name);
        download(app, &sha_asset.browser_download_url, &sha_path)?;
        let want = fs::read_to_string(&sha_path)
            .map_err(|e| e.to_string())?
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        let got = sha256_file(&archive)?;
        if want.is_empty() || got != want {
            return Err(format!("release checksum mismatch: expected {want}, got {got}"));
        }
        log_line(app, "Release checksum verified");
    }
    extract_tgz(app, &archive, &d.app)?;
    let _ = fs::remove_file(&archive);

    state.config.lock().unwrap().version = Some(version.clone());
    save_local_config(state)?;
    Ok(version)
}

fn step_env_file(app: &AppHandle, state: &LocalState, d: &Dirs) -> Result<(), String> {
    let env_path = d.app.join(".env");
    if env_path.exists() {
        log_line(app, ".env already exists — keeping it");
        return Ok(());
    }
    let (pg_port, api_port, web_port) = {
        let cfg = state.config.lock().unwrap();
        (cfg.pg_port, cfg.api_port, cfg.web_port)
    };
    let example = fs::read_to_string(d.app.join(".env.example"))
        .map_err(|e| format!("read .env.example: {e}"))?;
    let mut lines: Vec<String> = Vec::new();
    for line in example.lines() {
        let replaced = if line.starts_with("POSTGRES_HOST=") {
            "POSTGRES_HOST=127.0.0.1".to_string()
        } else if line.starts_with("POSTGRES_PORT=") {
            format!("POSTGRES_PORT={pg_port}")
        } else if line.starts_with("API_PORT=") {
            format!("API_PORT={api_port}")
        } else if line.starts_with("WEB_PORT=") {
            format!("WEB_PORT={web_port}")
        } else if line.starts_with("CORS_ORIGINS=") {
            format!("CORS_ORIGINS=http://127.0.0.1:{web_port},http://localhost:{web_port}")
        } else {
            line.to_string()
        };
        lines.push(replaced);
    }
    fs::write(&env_path, lines.join("\n") + "\n").map_err(|e| e.to_string())?;
    log_line(app, &format!(".env written (pg:{pg_port} api:{api_port} web:{web_port})"));
    Ok(())
}

fn step_core_setup(app: &AppHandle, state: &LocalState, d: &Dirs) -> Result<(), String> {
    let password = state.config.lock().unwrap().pg_password.clone();
    run_logged(
        app,
        Command::new("bash")
            .arg("scripts/setup.sh")
            .current_dir(&d.app)
            .env("PATH", tool_path(d))
            .env("PGPASSWORD", &password),
        "bash scripts/setup.sh",
    )
}

fn step_web_build(app: &AppHandle, d: &Dirs) -> Result<(), String> {
    let dist_index = d.app.join("apps/web/dist/index.html");
    if dist_index.exists() {
        log_line(app, "Frontend already built — skipping");
        return Ok(());
    }
    run_logged(
        app,
        Command::new(d.node.join("bin/npm"))
            .args(["run", "build"])
            .current_dir(d.app.join("apps/web"))
            .env("PATH", tool_path(d)),
        "npm run build (apps/web)",
    )
}

// ── Built-in web server: static dist + /api proxy ─────────────────────

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn proxy_request(
    client: &reqwest::blocking::Client,
    api_port: u16,
    mut req: tiny_http::Request,
) {
    let url = format!("http://127.0.0.1:{}{}", api_port, req.url());
    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);

    let mut body = Vec::new();
    let _ = req.as_reader().read_to_end(&mut body);

    let mut builder = client.request(method, &url);
    for h in req.headers() {
        let name = h.field.as_str().as_str();
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        builder = builder.header(name, h.value.as_str());
    }
    let result = builder.body(body).send();

    match result {
        Ok(res) => {
            let status = res.status().as_u16();
            let mut headers = Vec::new();
            for (name, value) in res.headers() {
                let n = name.as_str();
                if n.eq_ignore_ascii_case("transfer-encoding")
                    || n.eq_ignore_ascii_case("connection")
                    || n.eq_ignore_ascii_case("content-length")
                {
                    continue;
                }
                if let Ok(h) = tiny_http::Header::from_bytes(n.as_bytes(), value.as_bytes()) {
                    headers.push(h);
                }
            }
            let bytes = res.bytes().unwrap_or_default();
            let mut response = tiny_http::Response::from_data(bytes.to_vec())
                .with_status_code(tiny_http::StatusCode(status));
            for h in headers {
                response.add_header(h);
            }
            let _ = req.respond(response);
        }
        Err(e) => {
            let _ = req.respond(
                tiny_http::Response::from_string(format!("Upstream error: {e}"))
                    .with_status_code(tiny_http::StatusCode(502)),
            );
        }
    }
}

fn serve_static(dist: &Path, req: tiny_http::Request) {
    let raw_path = req.url().split('?').next().unwrap_or("/").to_string();
    let rel = raw_path.trim_start_matches('/');
    // Reject traversal; serve index.html for "/" and unknown extension-less
    // paths (SPA client-side routing).
    let mut target = if rel.is_empty() || rel.contains("..") {
        dist.join("index.html")
    } else {
        dist.join(rel)
    };
    if !target.is_file() {
        target = dist.join("index.html");
    }
    match fs::read(&target) {
        Ok(data) => {
            let mut response = tiny_http::Response::from_data(data);
            if let Ok(h) =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type(&target).as_bytes())
            {
                response.add_header(h);
            }
            let _ = req.respond(response);
        }
        Err(_) => {
            let _ = req.respond(
                tiny_http::Response::from_string("Not found")
                    .with_status_code(tiny_http::StatusCode(404)),
            );
        }
    }
}

fn start_web_server(d: &Dirs, web_port: u16, api_port: u16) -> Result<Arc<tiny_http::Server>, String> {
    let dist = d.app.join("apps/web/dist");
    let server = tiny_http::Server::http(("127.0.0.1", web_port))
        .map_err(|e| format!("web server on 127.0.0.1:{web_port}: {e}"))?;
    let server = Arc::new(server);
    let s = server.clone();
    std::thread::spawn(move || {
        // Plugin syncs through /api can run for minutes — no client timeout.
        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(3600))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        for req in s.incoming_requests() {
            let path = req.url().split('?').next().unwrap_or("/").to_string();
            let is_api = path.starts_with("/api/")
                || path == "/api"
                || path.starts_with("/uploads/")
                || path == "/openapi.json"
                || path == "/openapi.yaml";
            let dist = dist.clone();
            let client = client.clone();
            std::thread::spawn(move || {
                if is_api {
                    proxy_request(&client, api_port, req);
                } else {
                    serve_static(&dist, req);
                }
            });
        }
    });
    Ok(server)
}

// ── Stack lifecycle ───────────────────────────────────────────────────

fn spawn_logged(d: &Dirs, log_name: &str, mut cmd: Command) -> Result<Child, String> {
    fs::create_dir_all(&d.logs).map_err(|e| e.to_string())?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(d.logs.join(log_name))
        .map_err(|e| e.to_string())?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .map_err(|e| format!("spawn {log_name}: {e}"))
}

fn stack_is_running(state: &LocalState) -> bool {
    let mut procs = state.procs.lock().unwrap();
    match procs.api.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(None)),
        None => false,
    }
}

pub fn start_stack(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<LocalState>();
    if !state.config.lock().unwrap().installed {
        return Err("Local NousViz is not installed yet".into());
    }
    if stack_is_running(&state) {
        return Ok(());
    }
    let d = dirs(&state.base);
    let (pg_port, api_port, web_port) = {
        let cfg = state.config.lock().unwrap();
        (cfg.pg_port, cfg.api_port, cfg.web_port)
    };
    let _ = pg_port;

    // 1. Postgres (postgresql_embedded no-ops if the data dir is initialized).
    {
        let needs_pg = {
            let procs = state.procs.lock().unwrap();
            !matches!(
                procs.pg.as_ref().map(|p| p.status()),
                Some(postgresql_embedded::Status::Started)
            )
        };
        if needs_pg {
            let mut pg = postgresql_embedded::blocking::PostgreSQL::new(pg_settings(&state, &d));
            if pg_orphan_alive(&d.pg_data) {
                // A previous hard-killed Hub left the server running; adopt it.
                // stop() runs pg_ctl against the data dir, so shutdown still works.
                log_line(app, "Embedded Postgres already running from a previous session — reusing it");
            } else {
                pg.setup().map_err(|e| format!("postgres setup: {e}"))?;
                pg.start().map_err(|e| format!("postgres start: {e}"))?;
            }
            state.procs.lock().unwrap().pg = Some(pg);
        }
    }

    // 2. API + workers.
    let venv_python = d.app.join(".venv/bin/python3");
    let api = spawn_logged(&d, "api.log", {
        let mut c = Command::new(&venv_python);
        c.args([
            "-m",
            "uvicorn",
            "apps.api.src.main:app",
            "--host",
            "127.0.0.1",
            "--port",
            &api_port.to_string(),
        ])
        .current_dir(&d.app);
        c
    })?;
    let worker = spawn_logged(&d, "jobs-worker.log", {
        let mut c = Command::new(&venv_python);
        c.arg("apps/worker/src/run_jobs.py").current_dir(&d.app);
        c
    })?;
    let scheduler = spawn_logged(&d, "scheduler.log", {
        let mut c = Command::new(&venv_python);
        c.arg("apps/worker/src/run_scheduler.py").current_dir(&d.app);
        c
    })?;

    // 3. Frontend + proxy.
    let web = start_web_server(&d, web_port, api_port)?;

    {
        let mut procs = state.procs.lock().unwrap();
        procs.api = Some(api);
        procs.worker = Some(worker);
        procs.scheduler = Some(scheduler);
        procs.web = Some(web);
    }

    // 4. Wait until healthy.
    let client = blocking_client(Some(std::time::Duration::from_secs(5)))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if std::time::Instant::now() > deadline {
            return Err("API did not become healthy within 90s — check logs in app data dir".into());
        }
        if let Ok(res) = client
            .get(format!("http://127.0.0.1:{api_port}/api/health"))
            .send()
        {
            if res.status().is_success() {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
    Ok(())
}

pub fn stop_stack(state: &LocalState) {
    let mut procs = state.procs.lock().unwrap();
    for child in [
        procs.api.take(),
        procs.worker.take(),
        procs.scheduler.take(),
    ]
    .into_iter()
    .flatten()
    {
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
    }
    if let Some(web) = procs.web.take() {
        web.unblock();
    }
    if let Some(pg) = procs.pg.take() {
        let _ = pg.stop();
    }
}

// ── Instance registration ─────────────────────────────────────────────

fn register_instance(app: &AppHandle, web_port: u16) -> Result<(), String> {
    let state = app.state::<crate::AppState>();
    let url = format!("http://127.0.0.1:{web_port}");
    {
        let mut config = state.config.lock().unwrap();
        if config.instances.iter().any(|i| i.url == url) {
            return Ok(());
        }
        config.instances.push(crate::Instance {
            id: uuid::Uuid::new_v4().to_string(),
            name: "This computer".to_string(),
            url,
            email: None,
            token_fallback: None,
        });
    }
    crate::save_config(&state)
}

// ── The full setup chain ──────────────────────────────────────────────

fn run_setup(app: AppHandle) {
    let result = (|| -> Result<(), String> {
        let state = app.state::<LocalState>();
        let d = dirs(&state.base);
        fs::create_dir_all(&d.tmp).map_err(|e| e.to_string())?;

        // Allocate ports + password once; keep them forever after.
        {
            let mut cfg = state.config.lock().unwrap();
            if cfg.pg_port == 0 {
                cfg.pg_port = free_port()?;
            }
            if cfg.api_port == 0 {
                cfg.api_port = free_port()?;
            }
            if cfg.web_port == 0 {
                cfg.web_port = free_port()?;
            }
            if cfg.pg_password.is_empty() {
                cfg.pg_password = uuid::Uuid::new_v4().simple().to_string();
            }
        }
        save_local_config(&state)?;

        step(&app, "uv", "running", None);
        step_uv(&app, &d).map_err(|e| fail(&app, "uv", e))?;
        step(&app, "uv", "ok", None);

        step(&app, "python", "running", None);
        step_python(&app, &d).map_err(|e| fail(&app, "python", e))?;
        step(&app, "python", "ok", None);

        step(&app, "node", "running", None);
        step_node(&app, &d).map_err(|e| fail(&app, "node", e))?;
        step(&app, "node", "ok", None);

        step(&app, "postgres", "running", None);
        step_postgres(&app, &state, &d).map_err(|e| fail(&app, "postgres", e))?;
        step(&app, "postgres", "ok", None);

        step(&app, "release", "running", None);
        let version = step_release(&app, &state, &d).map_err(|e| fail(&app, "release", e))?;
        step(&app, "release", "ok", Some(format!("v{version}")));

        step(&app, "configure", "running", None);
        step_env_file(&app, &state, &d).map_err(|e| fail(&app, "configure", e))?;
        step_core_setup(&app, &state, &d).map_err(|e| fail(&app, "configure", e))?;
        step(&app, "configure", "ok", None);

        step(&app, "build", "running", None);
        step_web_build(&app, &d).map_err(|e| fail(&app, "build", e))?;
        step(&app, "build", "ok", None);

        {
            let mut cfg = state.config.lock().unwrap();
            cfg.installed = true;
        }
        save_local_config(&state)?;

        step(&app, "start", "running", None);
        start_stack(&app).map_err(|e| fail(&app, "start", e))?;
        let web_port = state.config.lock().unwrap().web_port;
        register_instance(&app, web_port).map_err(|e| fail(&app, "start", e))?;
        step(&app, "start", "ok", None);

        let _ = app.emit("local-setup-done", true);
        Ok(())
    })();

    if let Err(e) = result {
        log_line(&app, &format!("Setup failed: {e}"));
        let _ = app.emit("local-setup-done", false);
    }

    let state = app.state::<LocalState>();
    *state.setup_running.lock().unwrap() = false;
}

fn fail(app: &AppHandle, step_id: &str, err: String) -> String {
    step(app, step_id, "error", Some(err.clone()));
    err
}

// ── Commands ──────────────────────────────────────────────────────────

#[tauri::command]
pub fn local_status(state: tauri::State<'_, LocalState>) -> LocalStatus {
    let cfg = state.config.lock().unwrap().clone();
    LocalStatus {
        supported: supported(),
        installed: cfg.installed,
        running: stack_is_running(&state),
        setup_running: *state.setup_running.lock().unwrap(),
        version: cfg.version.clone(),
        url: if cfg.installed {
            Some(format!("http://127.0.0.1:{}", cfg.web_port))
        } else {
            None
        },
    }
}

#[tauri::command]
pub fn local_setup(app: AppHandle, state: tauri::State<'_, LocalState>) -> Result<(), String> {
    if !supported() {
        return Err("Local install is only available on macOS and Linux (NousViz core does not support native Windows yet)".into());
    }
    {
        let mut running = state.setup_running.lock().unwrap();
        if *running {
            return Err("Setup is already running".into());
        }
        *running = true;
    }
    std::thread::spawn(move || run_setup(app));
    Ok(())
}

#[tauri::command]
pub fn local_start(app: AppHandle) -> Result<(), String> {
    start_stack(&app)
}

#[tauri::command]
pub fn local_stop(state: tauri::State<'_, LocalState>) -> Result<(), String> {
    stop_stack(&state);
    Ok(())
}
