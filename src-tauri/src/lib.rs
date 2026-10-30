mod ssh;
mod sftp;

use std::sync::Arc;
use ssh::{HostConfig, SshManager};
use sftp::{
    DownloadDirResult, DownloadResult, SftpListResult, SftpManager, UploadResult,
};

use tauri::{AppHandle, Manager, State};

pub struct AppState {
    pub ssh: Arc<SshManager>,
    pub sftp: Arc<SftpManager>,
}

// =========================================================
// Terminal & SSH Shell Commands
// =========================================================

#[tauri::command]
async fn connect_ssh(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    host_config: HostConfig,
    cols: Option<u32>,
    rows: Option<u32>,
) -> Result<(), String> {
    state.ssh.connect(app, session_id, host_config, cols, rows).await
}

#[tauri::command]
fn send_ssh_data(
    state: State<'_, AppState>,
    session_id: String,
    data: String,
) -> Result<(), String> {
    state.ssh.send_data(&session_id, &data)
}

#[tauri::command]
fn send_ssh_keepalive(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<u32, String> {
    state.ssh.send_keepalive(&session_id)
}

#[tauri::command]
fn resize_ssh_terminal(
    state: State<'_, AppState>,
    session_id: String,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    state.ssh.resize(&session_id, cols, rows)
}

#[tauri::command]
fn disconnect_ssh(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    state.ssh.disconnect(&session_id)
}

#[tauri::command]
fn list_saved_hosts() -> Result<Vec<HostConfig>, String> {
    Ok(ssh::load_hosts())
}

#[tauri::command]
fn save_hosts(hosts: Vec<HostConfig>) -> Result<(), String> {
    ssh::save_hosts_to_disk(&hosts)
}

#[tauri::command]
fn export_hosts(include_keys: bool) -> Result<String, String> {
    ssh::export_hosts_json(include_keys)
}

#[tauri::command]
fn import_hosts(json_data: String) -> Result<usize, String> {
    ssh::import_hosts_json(&json_data)
}

#[tauri::command]
fn save_backup_file(json_data: String) -> Result<String, String> {
    let mut dir = dirs::download_dir().unwrap_or_else(|| {
        let mut p = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        p.push("Downloads");
        p
    });
    dir.push("RustTerm_Backups");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create backup directory '{}': {}", dir.display(), e))?;

    let filename = format!("rustterm_backup_{}.json", chrono::Local::now().format("%Y%m%d_%H%M%S"));
    let full_path = dir.join(&filename);

    std::fs::write(&full_path, &json_data)
        .map_err(|e| format!("Failed to write backup file '{}': {}", full_path.display(), e))?;

    println!("[RustTerm] Saved backup file to: {}", full_path.display());
    Ok(full_path.to_string_lossy().to_string())
}


// =========================================================
// SFTP File & Folder Manager Commands
// =========================================================

#[tauri::command]
async fn sftp_connect(
    state: State<'_, AppState>,
    sftp_session_id: String,
    host_config: HostConfig,
) -> Result<String, String> {
    state.sftp.connect(sftp_session_id, host_config).await
}

#[tauri::command]
async fn sftp_disconnect(
    state: State<'_, AppState>,
    sftp_session_id: String,
) -> Result<(), String> {
    state.sftp.disconnect(&sftp_session_id).await
}

#[tauri::command]
async fn sftp_list_dir(
    state: State<'_, AppState>,
    sftp_session_id: String,
    target_path: Option<String>,
) -> Result<SftpListResult, String> {
    state.sftp.list_dir(&sftp_session_id, target_path).await
}

#[tauri::command]
async fn sftp_download_file(
    state: State<'_, AppState>,
    sftp_session_id: String,
    remote_path: String,
    local_path: Option<String>,
) -> Result<DownloadResult, String> {
    state.sftp.download_file(&sftp_session_id, &remote_path, local_path).await
}

#[tauri::command]
async fn sftp_download_dir(
    state: State<'_, AppState>,
    sftp_session_id: String,
    remote_dir: String,
    local_dir: Option<String>,
) -> Result<DownloadDirResult, String> {
    state.sftp.download_dir(&sftp_session_id, &remote_dir, local_dir).await
}

#[tauri::command]
async fn sftp_upload_file(
    state: State<'_, AppState>,
    sftp_session_id: String,
    local_path: String,
    remote_dir: Option<String>,
) -> Result<UploadResult, String> {
    state.sftp.upload_file(&sftp_session_id, &local_path, remote_dir).await
}

#[tauri::command]
async fn sftp_create_dir(
    state: State<'_, AppState>,
    sftp_session_id: String,
    parent_dir: String,
    dir_name: String,
) -> Result<(), String> {
    state.sftp.create_dir(&sftp_session_id, &parent_dir, &dir_name).await
}

#[tauri::command]
async fn sftp_delete_item(
    state: State<'_, AppState>,
    sftp_session_id: String,
    remote_path: String,
    is_dir: bool,
) -> Result<(), String> {
    state.sftp.delete_item(&sftp_session_id, &remote_path, is_dir).await
}

#[tauri::command]
fn sftp_get_default_transfers_dir(host_label: String) -> Result<String, String> {
    let p = sftp::get_default_local_transfers_dir(&host_label);
    Ok(p.to_string_lossy().to_string())
}

#[tauri::command]
fn sftp_open_in_explorer(folder_path: String) -> Result<(), String> {
    sftp::open_path_in_explorer(&folder_path)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app_state = AppState {
        ssh: Arc::new(SshManager::new()),
        sftp: Arc::new(SftpManager::new()),
    };

    let res = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            connect_ssh,
            send_ssh_data,
            send_ssh_keepalive,
            resize_ssh_terminal,
            disconnect_ssh,
            list_saved_hosts,
            save_hosts,
            export_hosts,
            import_hosts,
            save_backup_file,
            sftp_connect,
            sftp_disconnect,
            sftp_list_dir,
            sftp_download_file,
            sftp_download_dir,
            sftp_upload_file,
            sftp_create_dir,
            sftp_delete_item,
            sftp_get_default_transfers_dir,
            sftp_open_in_explorer
        ])
        .run(tauri::generate_context!());

    if let Err(e) = res {
        let _ = std::fs::write("d:\\Project\\lab\\termius\\rustterm_run_err.log", format!("Tauri run error: {}\n", e));
    }
}
