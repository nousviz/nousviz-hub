use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};

const KEYRING_SERVICE: &str = "com.nousviz.hub";
const TOKEN_LOCALSTORAGE_KEY: &str = "nousviz_auth_token";

#[derive(Serialize, Deserialize, Clone, Default)]
struct Instance {
    id: String,
    name: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    // Only used when the OS keychain is unavailable (e.g. Linux without a
    // secret service). Plaintext in instances.json — the UI flags it.
    #[serde(skip_serializing_if = "Option::is_none")]
    token_fallback: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Config {
    instances: Vec<Instance>,
}

struct AppState {
    config: Mutex<Config>,
    config_path: PathBuf,
}

#[derive(Serialize)]
struct InstanceView {
    id: String,
    name: String,
    url: String,
    email: Option<String>,
    has_token: bool,
    token_in_fallback: bool,
}

fn view(inst: &Instance) -> InstanceView {
    let in_keyring = keyring::Entry::new(KEYRING_SERVICE, &inst.id)
        .and_then(|e| e.get_password())
        .is_ok();
    InstanceView {
        id: inst.id.clone(),
        name: inst.name.clone(),
        url: inst.url.clone(),
        email: inst.email.clone(),
        has_token: in_keyring || inst.token_fallback.is_some(),
        token_in_fallback: !in_keyring && inst.token_fallback.is_some(),
    }
}

fn load_config(path: &PathBuf) -> Config {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(state: &AppState) -> Result<(), String> {
    let config = state.config.lock().unwrap();
    let json = serde_json::to_string_pretty(&*config).map_err(|e| e.to_string())?;
    if let Some(dir) = state.config_path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fs::write(&state.config_path, json).map_err(|e| e.to_string())
}

fn normalize_url(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/').to_string();
    let parsed = tauri::Url::parse(&url).map_err(|_| format!("Not a valid URL: {url}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("URL must start with http:// or https://".into());
    }
    Ok(url)
}

fn store_token(inst: &mut Instance, token: &str) {
    match keyring::Entry::new(KEYRING_SERVICE, &inst.id).and_then(|e| e.set_password(token)) {
        Ok(()) => inst.token_fallback = None,
        Err(_) => inst.token_fallback = Some(token.to_string()),
    }
}

fn get_token(inst: &Instance) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, &inst.id)
        .and_then(|e| e.get_password())
        .ok()
        .or_else(|| inst.token_fallback.clone())
}

fn delete_token(inst: &mut Instance) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &inst.id) {
        let _ = entry.delete_credential();
    }
    inst.token_fallback = None;
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_instances(state: State<'_, AppState>) -> Vec<InstanceView> {
    state.config.lock().unwrap().instances.iter().map(view).collect()
}

#[tauri::command]
fn add_instance(state: State<'_, AppState>, name: String, url: String) -> Result<InstanceView, String> {
    let url = normalize_url(&url)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Name is required".into());
    }
    let inst = Instance {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        url,
        email: None,
        token_fallback: None,
    };
    let result = view(&inst);
    state.config.lock().unwrap().instances.push(inst);
    save_config(&state)?;
    Ok(result)
}

#[tauri::command]
fn remove_instance(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    {
        let mut config = state.config.lock().unwrap();
        if let Some(inst) = config.instances.iter_mut().find(|i| i.id == id) {
            delete_token(inst);
        }
        config.instances.retain(|i| i.id != id);
    }
    if let Some(window) = app.get_webview_window(&format!("instance-{id}")) {
        let _ = window.close();
    }
    save_config(&state)
}

#[tauri::command]
async fn login_instance(
    state: State<'_, AppState>,
    id: String,
    email: String,
    password: String,
) -> Result<InstanceView, String> {
    let url = {
        let config = state.config.lock().unwrap();
        config
            .instances
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.url.clone())
            .ok_or("Instance not found")?
    };

    let res = http_client()?
        .post(format!("{url}/api/auth/login"))
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .map_err(|e| format!("Could not reach {url}: {e}"))?;

    let status = res.status();
    let body: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let detail = body["detail"].as_str().unwrap_or("Login failed");
        return Err(detail.to_string());
    }
    let token = body["token"]
        .as_str()
        .ok_or("Server response had no token")?
        .to_string();

    let result = {
        let mut config = state.config.lock().unwrap();
        let inst = config
            .instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or("Instance not found")?;
        inst.email = Some(email);
        store_token(inst, &token);
        view(inst)
    };
    save_config(&state)?;
    Ok(result)
}

#[tauri::command]
fn logout_instance(state: State<'_, AppState>, id: String) -> Result<InstanceView, String> {
    let result = {
        let mut config = state.config.lock().unwrap();
        let inst = config
            .instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or("Instance not found")?;
        delete_token(inst);
        view(inst)
    };
    save_config(&state)?;
    Ok(result)
}

#[tauri::command]
async fn check_health(url: String) -> Result<serde_json::Value, String> {
    let url = normalize_url(&url)?;
    let res = http_client()?
        .get(format!("{url}/api/health"))
        .send()
        .await
        .map_err(|e| format!("Unreachable: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status().as_u16()));
    }
    res.json().await.map_err(|e| e.to_string())
}

#[tauri::command]
fn open_instance(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    let (name, url, token) = {
        let config = state.config.lock().unwrap();
        let inst = config
            .instances
            .iter()
            .find(|i| i.id == id)
            .ok_or("Instance not found")?;
        (inst.name.clone(), inst.url.clone(), get_token(inst))
    };

    let label = format!("instance-{id}");
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.set_focus();
        return Ok(());
    }

    let external = tauri::Url::parse(&url).map_err(|e| e.to_string())?;
    let mut builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(external))
        .title(format!("{name} — NousViz"))
        .inner_size(1360.0, 860.0);

    // The NousViz web app reads its session from localStorage and sends it
    // as X-Session-Token; seeding it here lands the user already signed in.
    if let Some(token) = token {
        let script = format!(
            "(function() {{ try {{ localStorage.setItem({key}, {token}); }} catch (e) {{}} }})();",
            key = serde_json::to_string(TOKEN_LOCALSTORAGE_KEY).unwrap(),
            token = serde_json::to_string(&token).unwrap(),
        );
        builder = builder.initialization_script(&script);
    }

    builder.build().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let config_path = app
                .path()
                .app_data_dir()
                .expect("no app data dir")
                .join("instances.json");
            app.manage(AppState {
                config: Mutex::new(load_config(&config_path)),
                config_path,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_instances,
            add_instance,
            remove_instance,
            login_instance,
            logout_instance,
            check_health,
            open_instance,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
