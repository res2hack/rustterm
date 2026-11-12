use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File as LocalFile};
use std::io::{BufWriter as StdBufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use ssh2::{Session, Sftp};
use tauri::{AppHandle, Emitter};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::ssh::{
    establish_authenticated_russh_session, establish_authenticated_session, HostConfig,
    RusshClientHandler,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteFileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SftpListResult {
    pub current_path: String,
    pub parent_path: Option<String>,
    pub entries: Vec<RemoteFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_file: bool,
    pub size: u64,
    pub modified: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LocalListResult {
    pub current_path: String,
    pub parent_path: Option<String>,
    pub entries: Vec<LocalFileEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LocalQuickPaths {
    pub drives: Vec<String>,
    pub home: String,
    pub desktop: String,
    pub downloads: String,
    pub documents: String,
    pub transfers: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DownloadResult {
    pub local_path: String,
    pub bytes_transferred: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DownloadDirResult {
    pub local_dir: String,
    pub total_files: u32,
    pub total_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UploadResult {
    pub remote_path: String,
    pub bytes_transferred: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchTransferResult {
    pub total_files: u32,
    pub total_bytes: u64,
    pub skipped_items: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictItem {
    pub name: String,
    pub source_path: String,
    pub dest_path: String,
    pub source_is_dir: bool,
    pub dest_is_dir: bool,
    pub existing_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferTargetItem {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchInfo {
    pub watch_id: String,
    pub sftp_session_id: String,
    pub remote_path: String,
    pub file_name: String,
    pub local_temp_path: String,
    pub upload_count: u32,
    pub last_upload_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SftpTransferProgressEvent {
    pub sftp_session_id: String,
    pub direction: String,
    pub current_item: String,
    pub total_files: u32,
    pub total_bytes: u64,
    pub speed_bps: u64,
    pub elapsed_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SftpAutoUploadEvent {
    pub sftp_session_id: String,
    pub watch_id: String,
    pub remote_path: String,
    pub file_name: String,
    pub bytes_transferred: u64,
    pub upload_count: u32,
    pub status: String,
    pub error: Option<String>,
}

pub struct ActiveFileWatch {
    pub info: WatchInfo,
    pub active: Arc<AtomicBool>,
}

#[derive(Clone)]
pub enum ActiveSftpBackend {
    Russh {
        sftp: Arc<russh_sftp::client::SftpSession>,
        session: Arc<russh::client::Handle<RusshClientHandler>>,
    },
    Ssh2 {
        sess: Arc<Mutex<Session>>,
    },
}

pub struct ActiveSftpSession {
    pub backend: ActiveSftpBackend,
    pub host: HostConfig,
    pub current_dir: String,
}

#[derive(Default)]
pub struct SftpManager {
    sessions: Arc<Mutex<HashMap<String, ActiveSftpSession>>>,
    watches: Arc<Mutex<HashMap<String, ActiveFileWatch>>>,
    cancel_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

pub async fn establish_sftp_russh(
    host: &HostConfig,
) -> Result<(russh_sftp::client::SftpSession, russh::client::Handle<RusshClientHandler>), String> {
    let session = establish_authenticated_russh_session(host).await?;
    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| format!("Failed to open session channel for SFTP: {}", e))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("Failed to request SFTP subsystem: {}", e))?;
    let sftp = russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| format!("Failed to initialize SFTP session: {}", e))?;
    sftp.set_timeout(60);
    Ok((sftp, session))
}

impl SftpManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            watches: Arc::new(Mutex::new(HashMap::new())),
            cancel_flags: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn reset_cancel_flag(&self, sftp_session_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        let mut lock = self.cancel_flags.lock();
        if let Some(old) = lock.insert(sftp_session_id.to_string(), flag.clone()) {
            old.store(true, Ordering::SeqCst);
        }
        flag
    }

    pub fn cancel_transfer(&self, sftp_session_id: &str) -> bool {
        let lock = self.cancel_flags.lock();
        if let Some(flag) = lock.get(sftp_session_id) {
            flag.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub async fn connect(&self, sftp_session_id: String, host: HostConfig) -> Result<String, String> {
        // Modern async pure-Rust SFTP engine (russh-sftp)
        // Supports Ed25519, OpenSSH private keys, password, RSA, interactive
        match establish_sftp_russh(&host).await {
            Ok((sftp, session_handle)) => {
                let initial_dir = match sftp.canonicalize(".").await {
                    Ok(p) => {
                        let clean = p.replace('\\', "/");
                        if clean.is_empty() { "/".to_string() } else { clean }
                    }
                    Err(_) => match sftp.canonicalize("/").await {
                        Ok(p) => p.replace('\\', "/"),
                        Err(_) => "/".to_string(),
                    },
                };

                let mut lock = self.sessions.lock();
                lock.insert(
                    sftp_session_id.clone(),
                    ActiveSftpSession {
                        backend: ActiveSftpBackend::Russh {
                            sftp: Arc::new(sftp),
                            session: Arc::new(session_handle),
                        },
                        host,
                        current_dir: initial_dir.clone(),
                    },
                );

                println!(
                    "[RustTerm SFTP] [russh engine] Connected session {}. Initial directory: {}",
                    sftp_session_id, initial_dir
                );
                Ok(initial_dir)
            }
            Err(e) => {
                println!("[RustTerm SFTP] russh engine returned: {}, checking fallback...", e);
                if host.auth_mode == "private_key" {
                    return Err(e);
                }
                println!("[RustTerm SFTP] Falling back to ssh2 for host: {}", host.hostname);
                self.connect_ssh2(sftp_session_id, host)
            }
        }
    }

    fn connect_ssh2(&self, sftp_session_id: String, host: HostConfig) -> Result<String, String> {
        let sess = establish_authenticated_session(&host)?;
        sess.set_blocking(true);

        let initial_dir = {
            let sftp = sess
                .sftp()
                .map_err(|e| format!("Failed to initialize SFTP subsystem: {}", e))?;
            match sftp.realpath(Path::new(".")) {
                Ok(pb) => pb.to_string_lossy().replace('\\', "/"),
                Err(_) => "/".to_string(),
            }
        };

        let mut lock = self.sessions.lock();
        lock.insert(
            sftp_session_id.clone(),
            ActiveSftpSession {
                backend: ActiveSftpBackend::Ssh2 {
                    sess: Arc::new(Mutex::new(sess)),
                },
                host,
                current_dir: initial_dir.clone(),
            },
        );

        println!(
            "[RustTerm SFTP] [ssh2 engine] Connected session {}. Initial directory: {}",
            sftp_session_id, initial_dir
        );
        Ok(initial_dir)
    }

    pub async fn disconnect(&self, sftp_session_id: &str) -> Result<(), String> {
        // Cancel any active transfer for this session
        {
            let mut c_lock = self.cancel_flags.lock();
            if let Some(flag) = c_lock.remove(sftp_session_id) {
                flag.store(true, Ordering::SeqCst);
            }
        }

        // Stop all active file-edit watchers belonging to this session
        {
            let mut w_lock = self.watches.lock();
            let keys_to_remove: Vec<String> = w_lock
                .iter()
                .filter(|(_, w)| w.info.sftp_session_id == sftp_session_id)
                .map(|(k, _)| k.clone())
                .collect();
            for k in keys_to_remove {
                if let Some(w) = w_lock.remove(&k) {
                    w.active.store(false, Ordering::SeqCst);
                }
            }
        }

        // Remove temp edit folder for this session
        let edit_dir = get_session_edit_temp_dir(sftp_session_id);
        if edit_dir.exists() {
            let _ = tokio::fs::remove_dir_all(&edit_dir).await;
        }

        let session = {
            let mut lock = self.sessions.lock();
            lock.remove(sftp_session_id)
        };

        if let Some(session) = session {
            match session.backend {
                ActiveSftpBackend::Russh { sftp, session } => {
                    let _ = sftp.close().await;
                    let _ = session.disconnect(russh::Disconnect::ByApplication, "", "").await;
                    println!("[RustTerm SFTP] Disconnected russh session {}", sftp_session_id);
                }
                ActiveSftpBackend::Ssh2 { sess } => {
                    let s = sess.lock();
                    let _ = s.disconnect(None, "SFTP closed", None);
                    println!("[RustTerm SFTP] Disconnected ssh2 session {}", sftp_session_id);
                }
            }
        }
        Ok(())
    }

    pub async fn list_dir(&self, sftp_session_id: &str, target_path: Option<String>) -> Result<SftpListResult, String> {
        let (backend, current_dir) = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found. Please connect first.".to_string())?;
            (session.backend.clone(), session.current_dir.clone())
        };

        let remote_path = match target_path {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => current_dir,
        };

        let (real_path, mut entries) = match backend {
            ActiveSftpBackend::Russh { ref sftp, .. } => {
                let canonical = match sftp.canonicalize(&remote_path).await {
                    Ok(p) => p.replace('\\', "/"),
                    Err(_) => remote_path.clone(),
                };

                let read_dir = sftp
                    .read_dir(&canonical)
                    .await
                    .map_err(|e| format!("Failed to read directory '{}': {}", canonical, e))?;

                let mut items = Vec::new();
                for entry in read_dir {
                    let name = entry.file_name();
                    if name.is_empty() || name == "." || name == ".." {
                        continue;
                    }

                    let full_item_path = if canonical == "/" {
                        format!("/{}", name)
                    } else if canonical.ends_with('/') {
                        format!("{}{}", canonical, name)
                    } else {
                        format!("{}/{}", canonical, name)
                    };

                    let ft = entry.file_type();
                    let meta = entry.metadata();

                    items.push(RemoteFileEntry {
                        name,
                        path: full_item_path,
                        is_dir: ft.is_dir(),
                        is_file: ft.is_file(),
                        is_symlink: ft.is_symlink(),
                        size: meta.size.unwrap_or(0),
                        modified: meta.mtime.unwrap_or(0) as u64,
                    });
                }
                (canonical, items)
            }
            ActiveSftpBackend::Ssh2 { ref sess } => {
                let sess_guard = sess.lock();
                let sftp = sess_guard
                    .sftp()
                    .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;

                let real_path = match sftp.realpath(Path::new(&remote_path)) {
                    Ok(pb) => pb.to_string_lossy().replace('\\', "/"),
                    Err(_) => remote_path.clone(),
                };

                let entries_raw = sftp
                    .readdir(Path::new(&real_path))
                    .map_err(|e| format!("Failed to read directory '{}': {}", real_path, e))?;

                let mut items = Vec::new();
                for (pb, stat) in entries_raw {
                    let name = pb
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() || name == "." || name == ".." {
                        continue;
                    }

                    let full_item_path = if real_path == "/" {
                        format!("/{}", name)
                    } else if real_path.ends_with('/') {
                        format!("{}{}", real_path, name)
                    } else {
                        format!("{}/{}", real_path, name)
                    };

                    let ft = stat.file_type();
                    items.push(RemoteFileEntry {
                        name,
                        path: full_item_path,
                        is_dir: ft.is_dir(),
                        is_file: ft.is_file(),
                        is_symlink: ft.is_symlink(),
                        size: stat.size.unwrap_or(0),
                        modified: stat.mtime.unwrap_or(0),
                    });
                }
                (real_path, items)
            }
        };

        // Directories first, then alphabetical
        entries.sort_by(|a, b| {
            if a.is_dir != b.is_dir {
                b.is_dir.cmp(&a.is_dir)
            } else {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            }
        });

        // Compute parent path
        let parent_path = if real_path == "/" || real_path.is_empty() {
            None
        } else {
            let p = Path::new(&real_path);
            p.parent().map(|par| {
                let s = par.to_string_lossy().replace('\\', "/");
                if s.is_empty() {
                    "/".to_string()
                } else {
                    s
                }
            })
        };

        {
            let mut lock = self.sessions.lock();
            if let Some(session) = lock.get_mut(sftp_session_id) {
                session.current_dir = real_path.clone();
            }
        }

        Ok(SftpListResult {
            current_path: real_path,
            parent_path,
            entries,
        })
    }

    pub async fn download_file(
        &self,
        sftp_session_id: &str,
        remote_path: &str,
        local_path: Option<String>,
    ) -> Result<DownloadResult, String> {
        let (backend, hostname) = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            (session.backend.clone(), session.host.hostname.clone())
        };

        let filename = Path::new(remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("downloaded_file");

        let target_local_path: PathBuf = match local_path {
            Some(ref p) if !p.trim().is_empty() => {
                let pb = PathBuf::from(p);
                if pb.is_dir() {
                    pb.join(filename)
                } else {
                    pb
                }
            }
            _ => {
                let default_dir = get_default_local_transfers_dir(&hostname);
                default_dir.join(filename)
            }
        };

        if let Some(parent) = target_local_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create local directory '{}': {}", parent.display(), e))?;
        }

        let total_bytes: u64 = match backend {
            ActiveSftpBackend::Russh { ref sftp, .. } => {
                let mut remote_file = sftp
                    .open(remote_path)
                    .await
                    .map_err(|e| format!("Failed to open remote file '{}': {}", remote_path, e))?;

                let mut local_file = tokio::fs::File::create(&target_local_path)
                    .await
                    .map_err(|e| format!("Failed to create local file '{}': {}", target_local_path.display(), e))?;

                tokio::io::copy(&mut remote_file, &mut local_file)
                    .await
                    .map_err(|e| format!("Error transferring remote file: {}", e))?
            }
            ActiveSftpBackend::Ssh2 { ref sess } => {
                let sess_guard = sess.lock();
                let sftp = sess_guard
                    .sftp()
                    .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;

                let mut remote_file = sftp
                    .open(Path::new(remote_path))
                    .map_err(|e| format!("Failed to open remote file '{}': {}", remote_path, e))?;

                let mut local_file = LocalFile::create(&target_local_path)
                    .map_err(|e| format!("Failed to create local file '{}': {}", target_local_path.display(), e))?;

                let mut buffer = [0u8; 64 * 1024];
                let mut bytes = 0u64;
                loop {
                    let n = remote_file
                        .read(&mut buffer)
                        .map_err(|e| format!("Error reading remote file: {}", e))?;
                    if n == 0 {
                        break;
                    }
                    local_file
                        .write_all(&buffer[..n])
                        .map_err(|e| format!("Error writing local file: {}", e))?;
                    bytes += n as u64;
                }
                bytes
            }
        };

        println!(
            "[RustTerm SFTP] Downloaded {} bytes: {} -> {}",
            total_bytes,
            remote_path,
            target_local_path.display()
        );

        Ok(DownloadResult {
            local_path: target_local_path.to_string_lossy().to_string(),
            bytes_transferred: total_bytes,
        })
    }

    pub async fn download_dir(
        &self,
        sftp_session_id: &str,
        remote_dir: &str,
        local_dir: Option<String>,
    ) -> Result<DownloadDirResult, String> {
        let (backend, hostname) = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            (session.backend.clone(), session.host.hostname.clone())
        };

        let folder_name = Path::new(remote_dir)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("folder");

        let target_local_dir: PathBuf = match local_dir {
            Some(ref p) if !p.trim().is_empty() => PathBuf::from(p).join(folder_name),
            _ => {
                let default_dir = get_default_local_transfers_dir(&hostname);
                default_dir.join(folder_name)
            }
        };

        let mut total_files = 0u32;
        let mut total_bytes = 0u64;

        match backend {
            ActiveSftpBackend::Russh { ref sftp, .. } => {
                Self::recursive_download_russh(
                    sftp,
                    remote_dir,
                    &target_local_dir,
                    &mut total_files,
                    &mut total_bytes,
                )
                .await?;
            }
            ActiveSftpBackend::Ssh2 { ref sess } => {
                let sess_guard = sess.lock();
                let sftp = sess_guard
                    .sftp()
                    .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;

                Self::recursive_download_ssh2(
                    &sftp,
                    remote_dir,
                    &target_local_dir,
                    &mut total_files,
                    &mut total_bytes,
                )?;
            }
        }

        println!(
            "[RustTerm SFTP] Downloaded directory: {} files, {} bytes to {}",
            total_files,
            total_bytes,
            target_local_dir.display()
        );

        Ok(DownloadDirResult {
            local_dir: target_local_dir.to_string_lossy().to_string(),
            total_files,
            total_bytes,
        })
    }

    fn recursive_download_russh<'a>(
        sftp: &'a russh_sftp::client::SftpSession,
        remote_dir: &'a str,
        local_dir: &'a Path,
        total_files: &'a mut u32,
        total_bytes: &'a mut u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            tokio::fs::create_dir_all(local_dir)
                .await
                .map_err(|e| format!("Failed to create local folder '{}': {}", local_dir.display(), e))?;

            let read_dir = sftp
                .read_dir(remote_dir)
                .await
                .map_err(|e| format!("Failed to read remote directory '{}': {}", remote_dir, e))?;

            for entry in read_dir {
                let name = entry.file_name();
                if name.is_empty() || name == "." || name == ".." {
                    continue;
                }

                let remote_item_path = if remote_dir.ends_with('/') {
                    format!("{}{}", remote_dir, name)
                } else {
                    format!("{}/{}", remote_dir, name)
                };
                let local_item_path = local_dir.join(&name);

                let ft = entry.file_type();
                if ft.is_dir() {
                    Self::recursive_download_russh(
                        sftp,
                        &remote_item_path,
                        &local_item_path,
                        total_files,
                        total_bytes,
                    )
                    .await?;
                } else if ft.is_file() {
                    if let Ok(mut rfile) = sftp.open(&remote_item_path).await {
                        if let Ok(mut lfile) = tokio::fs::File::create(&local_item_path).await {
                            if let Ok(n) = tokio::io::copy(&mut rfile, &mut lfile).await {
                                *total_bytes += n;
                                *total_files += 1;
                            }
                        }
                    }
                }
            }

            Ok(())
        })
    }

    fn recursive_download_ssh2(
        sftp: &Sftp,
        remote_dir: &str,
        local_dir: &Path,
        total_files: &mut u32,
        total_bytes: &mut u64,
    ) -> Result<(), String> {
        fs::create_dir_all(local_dir)
            .map_err(|e| format!("Failed to create local folder '{}': {}", local_dir.display(), e))?;

        let entries = sftp
            .readdir(Path::new(remote_dir))
            .map_err(|e| format!("Failed to read remote directory '{}': {}", remote_dir, e))?;

        let mut buffer = [0u8; 64 * 1024];

        for (pb, stat) in entries {
            let name = pb
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }

            let remote_item_path = if remote_dir.ends_with('/') {
                format!("{}{}", remote_dir, name)
            } else {
                format!("{}/{}", remote_dir, name)
            };
            let local_item_path = local_dir.join(&name);

            let ft = stat.file_type();
            if ft.is_dir() {
                Self::recursive_download_ssh2(
                    sftp,
                    &remote_item_path,
                    &local_item_path,
                    total_files,
                    total_bytes,
                )?;
            } else if ft.is_file() {
                if let Ok(mut rfile) = sftp.open(Path::new(&remote_item_path)) {
                    if let Ok(mut lfile) = LocalFile::create(&local_item_path) {
                        loop {
                            match rfile.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(n) => {
                                    if lfile.write_all(&buffer[..n]).is_ok() {
                                        *total_bytes += n as u64;
                                    } else {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        *total_files += 1;
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn upload_file(
        &self,
        sftp_session_id: &str,
        local_path: &str,
        remote_dir: Option<String>,
    ) -> Result<UploadResult, String> {
        let (backend, current_dir) = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            (session.backend.clone(), session.current_dir.clone())
        };

        let local_pb = Path::new(local_path);
        if !local_pb.exists() {
            return Err(format!("Local file '{}' does not exist", local_path));
        }

        let filename = local_pb
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("uploaded_file");

        let dest_dir = remote_dir.unwrap_or(current_dir);
        let remote_file_path = if dest_dir.ends_with('/') {
            format!("{}{}", dest_dir, filename)
        } else {
            format!("{}/{}", dest_dir, filename)
        };

        let total_bytes: u64 = match backend {
            ActiveSftpBackend::Russh { ref sftp, .. } => {
                let mut local_file = tokio::fs::File::open(local_pb)
                    .await
                    .map_err(|e| format!("Failed to open local file: {}", e))?;

                let mut remote_file = sftp
                    .create(&remote_file_path)
                    .await
                    .map_err(|e| format!("Failed to create remote file '{}': {}", remote_file_path, e))?;

                tokio::io::copy(&mut local_file, &mut remote_file)
                    .await
                    .map_err(|e| format!("Error uploading file: {}", e))?
            }
            ActiveSftpBackend::Ssh2 { ref sess } => {
                let sess_guard = sess.lock();
                let sftp = sess_guard
                    .sftp()
                    .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;

                let mut local_file = LocalFile::open(local_pb)
                    .map_err(|e| format!("Failed to open local file: {}", e))?;

                let mut remote_file = sftp
                    .create(Path::new(&remote_file_path))
                    .map_err(|e| format!("Failed to create remote file '{}': {}", remote_file_path, e))?;

                let mut buffer = [0u8; 64 * 1024];
                let mut bytes = 0u64;
                loop {
                    let n = local_file
                        .read(&mut buffer)
                        .map_err(|e| format!("Error reading local file: {}", e))?;
                    if n == 0 {
                        break;
                    }
                    remote_file
                        .write_all(&buffer[..n])
                        .map_err(|e| format!("Error writing remote file: {}", e))?;
                    bytes += n as u64;
                }
                bytes
            }
        };

        println!(
            "[RustTerm SFTP] Uploaded {} bytes to {}",
            total_bytes, remote_file_path
        );

        Ok(UploadResult {
            remote_path: remote_file_path,
            bytes_transferred: total_bytes,
        })
    }

    pub async fn create_dir(
        &self,
        sftp_session_id: &str,
        parent_dir: &str,
        dir_name: &str,
    ) -> Result<(), String> {
        let backend = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            session.backend.clone()
        };

        let full_dir_path = if parent_dir.ends_with('/') {
            format!("{}{}", parent_dir, dir_name)
        } else {
            format!("{}/{}", parent_dir, dir_name)
        };

        match backend {
            ActiveSftpBackend::Russh { ref sftp, .. } => {
                sftp.create_dir(&full_dir_path)
                    .await
                    .map_err(|e| format!("Failed to create remote directory '{}': {}", full_dir_path, e))?;
            }
            ActiveSftpBackend::Ssh2 { ref sess } => {
                let sess_guard = sess.lock();
                let sftp = sess_guard
                    .sftp()
                    .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;
                sftp.mkdir(Path::new(&full_dir_path), 0o755)
                    .map_err(|e| format!("Failed to create remote directory '{}': {}", full_dir_path, e))?;
            }
        }

        Ok(())
    }

    pub async fn delete_item(
        &self,
        sftp_session_id: &str,
        remote_path: &str,
        is_dir: bool,
    ) -> Result<(), String> {
        let backend = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            session.backend.clone()
        };

        delete_remote_entry(&backend, remote_path, is_dir).await
    }

    pub async fn delete_items(
        &self,
        sftp_session_id: &str,
        items: Vec<TransferTargetItem>,
    ) -> Result<u32, String> {
        let backend = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            session.backend.clone()
        };

        let mut deleted = 0u32;
        for item in items {
            delete_remote_entry(&backend, &item.path, item.is_dir).await?;
            deleted += 1;
        }
        Ok(deleted)
    }

    pub async fn rename_item(
        &self,
        sftp_session_id: &str,
        old_path: &str,
        new_name: &str,
    ) -> Result<String, String> {
        let clean_name = new_name.trim();
        if clean_name.is_empty() || clean_name.contains('/') || clean_name.contains('\\') {
            return Err("Invalid new name".to_string());
        }

        let backend = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            session.backend.clone()
        };

        let parent = Path::new(old_path)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| "/".to_string());
        let new_path = join_remote_path(&parent, clean_name);

        match backend {
            ActiveSftpBackend::Russh { ref sftp, .. } => {
                sftp.rename(old_path, &new_path)
                    .await
                    .map_err(|e| format!("Failed to rename '{}': {}", old_path, e))?;
            }
            ActiveSftpBackend::Ssh2 { ref sess } => {
                let sess_guard = sess.lock();
                let sftp = sess_guard
                    .sftp()
                    .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;
                sftp.rename(Path::new(old_path), Path::new(&new_path), None)
                    .map_err(|e| format!("Failed to rename '{}': {}", old_path, e))?;
            }
        }

        Ok(new_path)
    }

    pub async fn check_remote_conflicts(
        &self,
        sftp_session_id: &str,
        local_paths: Vec<String>,
        remote_dir: String,
    ) -> Result<Vec<ConflictItem>, String> {
        let backend = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            session.backend.clone()
        };

        let mut conflicts = Vec::new();
        for lp in local_paths {
            let p = Path::new(&lp);
            let name = match p.file_name().and_then(|n| n.to_str()) {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => continue,
            };
            let source_is_dir = p.is_dir();
            let dest_path = join_remote_path(&remote_dir, &name);

            let existing = match backend {
                ActiveSftpBackend::Russh { ref sftp, .. } => {
                    match sftp.symlink_metadata(&dest_path).await {
                        Ok(attr) => Some((attr.is_dir(), attr.size.unwrap_or(0))),
                        Err(_) => None,
                    }
                }
                ActiveSftpBackend::Ssh2 { ref sess } => {
                    let sess_guard = sess.lock();
                    let sftp = sess_guard
                        .sftp()
                        .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;
                    match sftp.lstat(Path::new(&dest_path)) {
                        Ok(stat) => Some((stat.file_type().is_dir(), stat.size.unwrap_or(0))),
                        Err(_) => None,
                    }
                }
            };

            if let Some((dest_is_dir, existing_size)) = existing {
                conflicts.push(ConflictItem {
                    name,
                    source_path: lp,
                    dest_path,
                    source_is_dir,
                    dest_is_dir,
                    existing_size,
                });
            }
        }

        Ok(conflicts)
    }

    pub async fn upload_paths(
        &self,
        app: &AppHandle,
        sftp_session_id: &str,
        local_paths: Vec<String>,
        remote_dir: String,
        overwrite: bool,
    ) -> Result<BatchTransferResult, String> {
        let backend = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            session.backend.clone()
        };

        let mut total_files = 0u32;
        let mut total_bytes = 0u64;
        let mut skipped_items = 0u32;

        match backend {
            ActiveSftpBackend::Russh { ref sftp, .. } => {
                for lp in local_paths {
                    let local_pb = PathBuf::from(&lp);
                    if !local_pb.exists() {
                        return Err(format!("Local path '{}' does not exist", lp));
                    }
                    let name = local_pb
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| format!("Invalid path '{}'", lp))?;
                    let remote_target = join_remote_path(&remote_dir, name);

                    if local_pb.is_dir() {
                        if !overwrite && sftp.symlink_metadata(&remote_target).await.is_ok() {
                            skipped_items += 1;
                            continue;
                        }
                        recursive_upload_russh(
                            app,
                            sftp_session_id,
                            sftp,
                            &local_pb,
                            &remote_target,
                            overwrite,
                            &mut total_files,
                            &mut total_bytes,
                            &mut skipped_items,
                        )
                        .await?;
                    } else {
                        if !overwrite && sftp.symlink_metadata(&remote_target).await.is_ok() {
                            skipped_items += 1;
                            continue;
                        }
                        let n = upload_single_file_russh(sftp, &local_pb, &remote_target).await?;
                        total_files += 1;
                        total_bytes += n;
                        emit_transfer_progress(app, sftp_session_id, "upload", name, total_files, total_bytes);
                    }
                }
            }
            ActiveSftpBackend::Ssh2 { ref sess } => {
                let sess_guard = sess.lock();
                let sftp = sess_guard
                    .sftp()
                    .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;

                for lp in local_paths {
                    let local_pb = PathBuf::from(&lp);
                    if !local_pb.exists() {
                        return Err(format!("Local path '{}' does not exist", lp));
                    }
                    let name = local_pb
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| format!("Invalid path '{}'", lp))?;
                    let remote_target = join_remote_path(&remote_dir, name);

                    if local_pb.is_dir() {
                        if !overwrite && sftp.lstat(Path::new(&remote_target)).is_ok() {
                            skipped_items += 1;
                            continue;
                        }
                        recursive_upload_ssh2(
                            app,
                            sftp_session_id,
                            &sftp,
                            &local_pb,
                            &remote_target,
                            overwrite,
                            &mut total_files,
                            &mut total_bytes,
                            &mut skipped_items,
                        )?;
                    } else {
                        if !overwrite && sftp.lstat(Path::new(&remote_target)).is_ok() {
                            skipped_items += 1;
                            continue;
                        }
                        let n = upload_single_file_ssh2(&sftp, &local_pb, &remote_target)?;
                        total_files += 1;
                        total_bytes += n;
                        emit_transfer_progress(app, sftp_session_id, "upload", name, total_files, total_bytes);
                    }
                }
            }
        }

        Ok(BatchTransferResult {
            total_files,
            total_bytes,
            skipped_items,
        })
    }

    pub async fn download_paths(
        &self,
        app: &AppHandle,
        sftp_session_id: &str,
        items: Vec<TransferTargetItem>,
        local_dir: String,
        overwrite: bool,
        excludes: Vec<String>,
    ) -> Result<BatchTransferResult, String> {
        let norm_excludes = normalize_exclude_patterns(&excludes);
        let backend = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            session.backend.clone()
        };

        let cancel_flag = self.reset_cancel_flag(sftp_session_id);
        let started_at = Instant::now();

        let local_base = PathBuf::from(&local_dir);
        tokio::fs::create_dir_all(&local_base)
            .await
            .map_err(|e| format!("Failed to prepare local folder '{}': {}", local_dir, e))?;

        match backend {
            ActiveSftpBackend::Russh { ref sftp, .. } => {
                let mut queue: Vec<(String, PathBuf, u64)> = Vec::new();
                let mut skipped_items = 0u32;

                for item in items {
                    if cancel_flag.load(Ordering::SeqCst) {
                        return Err("Transfer cancelled by user".to_string());
                    }
                    if matches_sftp_exclude(&item.name, &item.name, &item.name, &norm_excludes) {
                        skipped_items += 1;
                        continue;
                    }
                    let target_local = local_base.join(&item.name);
                    if item.is_dir {
                        collect_remote_downloads_russh(
                            sftp,
                            &item.path,
                            &target_local,
                            &item.name,
                            "",
                            &norm_excludes,
                            overwrite,
                            &cancel_flag,
                            &mut queue,
                            &mut skipped_items,
                        )
                        .await?;
                    } else {
                        if !overwrite {
                            if let Ok(m) = tokio::fs::metadata(&target_local).await {
                                if m.is_file() && m.len() == item.size {
                                    skipped_items += 1;
                                    continue;
                                }
                            }
                        }
                        queue.push((item.path, target_local, item.size));
                    }
                }

                let total_files = Arc::new(AtomicU32::new(0));
                let total_bytes = Arc::new(AtomicU64::new(0));
                let last_emit_ms = Arc::new(AtomicU64::new(0));
                let semaphore = Arc::new(Semaphore::new(6));
                let mut join_set = JoinSet::new();

                for (remote_path, local_path, _size) in queue {
                    if cancel_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let permit = semaphore
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| "Transfer aborted".to_string())?;
                    let sftp_c = sftp.clone();
                    let app_c = app.clone();
                    let sid_c = sftp_session_id.to_string();
                    let cancel_c = cancel_flag.clone();
                    let files_c = total_files.clone();
                    let bytes_c = total_bytes.clone();
                    let last_emit_c = last_emit_ms.clone();

                    join_set.spawn(async move {
                        let _permit = permit;
                        if cancel_c.load(Ordering::SeqCst) {
                            return Err("Transfer cancelled by user".to_string());
                        }
                        let file_name = Path::new(&remote_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("file")
                            .to_string();

                        download_single_file_russh_cancellable(
                            &sftp_c,
                            &remote_path,
                            &local_path,
                            &cancel_c,
                            |delta| {
                                let cur_bytes = bytes_c.fetch_add(delta, Ordering::Relaxed) + delta;
                                emit_throttled_progress(
                                    &app_c,
                                    &sid_c,
                                    "download",
                                    &file_name,
                                    files_c.load(Ordering::Relaxed),
                                    cur_bytes,
                                    started_at,
                                    &last_emit_c,
                                );
                            },
                        )
                        .await?;

                        let cur_files = files_c.fetch_add(1, Ordering::Relaxed) + 1;
                        let cur_bytes = bytes_c.load(Ordering::Relaxed);
                        emit_throttled_progress(
                            &app_c,
                            &sid_c,
                            "download",
                            &file_name,
                            cur_files,
                            cur_bytes,
                            started_at,
                            &last_emit_c,
                        );
                        Ok::<(), String>(())
                    });
                }

                let mut first_err: Option<String> = None;
                while let Some(res) = join_set.join_next().await {
                    match res {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                            cancel_flag.store(true, Ordering::SeqCst);
                        }
                        Err(e) => {
                            if first_err.is_none() {
                                first_err = Some(format!("Worker task error: {}", e));
                            }
                            cancel_flag.store(true, Ordering::SeqCst);
                        }
                    }
                }

                if let Some(err) = first_err {
                    return Err(err);
                }
                if cancel_flag.load(Ordering::SeqCst) {
                    return Err("Transfer cancelled by user".to_string());
                }

                Ok(BatchTransferResult {
                    total_files: total_files.load(Ordering::Relaxed),
                    total_bytes: total_bytes.load(Ordering::Relaxed),
                    skipped_items,
                })
            }
            ActiveSftpBackend::Ssh2 { ref sess } => {
                let mut total_files = 0u32;
                let mut total_bytes = 0u64;
                let mut skipped_items = 0u32;

                let sess_guard = sess.lock();
                let sftp = sess_guard
                    .sftp()
                    .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;

                for item in items {
                    if cancel_flag.load(Ordering::SeqCst) {
                        return Err("Transfer cancelled by user".to_string());
                    }
                    if matches_sftp_exclude(&item.name, &item.name, &item.name, &norm_excludes) {
                        skipped_items += 1;
                        continue;
                    }
                    let target_local = local_base.join(&item.name);
                    if item.is_dir {
                        Self::recursive_download_ssh2_opts(
                            app,
                            sftp_session_id,
                            &sftp,
                            &item.path,
                            &target_local,
                            &item.name,
                            "",
                            &norm_excludes,
                            overwrite,
                            &cancel_flag,
                            started_at,
                            &mut total_files,
                            &mut total_bytes,
                            &mut skipped_items,
                        )?;
                    } else {
                        if !overwrite {
                            if let Ok(m) = fs::metadata(&target_local) {
                                if m.is_file() && m.len() == item.size {
                                    skipped_items += 1;
                                    continue;
                                }
                            }
                        }
                        let n = download_single_file_ssh2(&sftp, &item.path, &target_local)?;
                        total_files += 1;
                        total_bytes += n;
                        emit_transfer_progress_timed(app, sftp_session_id, "download", &item.name, total_files, total_bytes, started_at);
                    }
                }

                Ok(BatchTransferResult {
                    total_files,
                    total_bytes,
                    skipped_items,
                })
            }
        }
    }

    pub async fn stream_download(
        &self,
        app: &AppHandle,
        sftp_session_id: &str,
        items: Vec<TransferTargetItem>,
        local_dir: String,
        overwrite: bool,
        excludes: Vec<String>,
    ) -> Result<BatchTransferResult, String> {
        if items.is_empty() {
            return Ok(BatchTransferResult {
                total_files: 0,
                total_bytes: 0,
                skipped_items: 0,
            });
        }

        let backend = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            session.backend.clone()
        };

        let cancel_flag = self.reset_cancel_flag(sftp_session_id);
        let started_at = Instant::now();

        let local_base = PathBuf::from(&local_dir);
        tokio::fs::create_dir_all(&local_base)
            .await
            .map_err(|e| format!("Failed to prepare local folder '{}': {}", local_dir, e))?;

        // Group items by remote parent directory for `tar -cf - -C <parent> <names...>`
        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for item in &items {
            let clean_path = item.path.trim_end_matches('/');
            let (parent, name) = match clean_path.rfind('/') {
                Some(0) => ("/".to_string(), clean_path[1..].to_string()),
                Some(idx) => (clean_path[..idx].to_string(), clean_path[idx + 1..].to_string()),
                None => (".".to_string(), clean_path.to_string()),
            };
            if !name.is_empty() {
                grouped.entry(parent).or_default().push(name);
            }
        }

        let mut total_files = 0u32;
        let mut total_bytes = 0u64;
        let mut skipped_items = 0u32;

        for (parent_dir, names) in grouped {
            if cancel_flag.load(Ordering::SeqCst) {
                return Err("Transfer cancelled by user".to_string());
            }

            let exclude_flags = build_tar_exclude_args(&excludes, &names);
            let quoted_names: Vec<String> = names.iter().map(|n| posix_shell_quote(n)).collect();
            let cmd = if exclude_flags.is_empty() {
                format!(
                    "LC_ALL=C tar -cf - -C {} {}",
                    posix_shell_quote(&parent_dir),
                    quoted_names.join(" ")
                )
            } else {
                format!(
                    "LC_ALL=C tar {} -cf - -C {} {}",
                    exclude_flags.join(" "),
                    posix_shell_quote(&parent_dir),
                    quoted_names.join(" ")
                )
            };

            let stats = match backend {
                ActiveSftpBackend::Russh { ref session, .. } => {
                    let mut channel = session
                        .channel_open_session()
                        .await
                        .map_err(|e| format!("Failed to open SSH stream channel: {}", e))?;

                    channel
                        .exec(true, cmd.as_str())
                        .await
                        .map_err(|e| format!("Failed to execute tar stream on remote host: {}", e))?;

                    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
                    let app_c = app.clone();
                    let sid_c = sftp_session_id.to_string();
                    let local_base_c = local_base.clone();

                    let extractor_handle = tokio::task::spawn_blocking(move || {
                        let mut extractor = TarStreamExtractor::new(
                            local_base_c,
                            overwrite,
                            app_c,
                            sid_c,
                            started_at,
                        );
                        while let Some(chunk) = rx.blocking_recv() {
                            extractor.feed(&chunk)?;
                        }
                        extractor.finish()
                    });

                    let mut stderr_buf = String::new();
                    let mut exit_code: Option<u32> = None;

                    loop {
                        if cancel_flag.load(Ordering::SeqCst) {
                            let _ = channel.close().await;
                            drop(tx);
                            let _ = extractor_handle.await;
                            return Err("Transfer cancelled by user".to_string());
                        }

                        match channel.wait().await {
                            Some(russh::ChannelMsg::Data { data }) => {
                                if tx.send(data.to_vec()).await.is_err() {
                                    break;
                                }
                            }
                            Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                                if stderr_buf.len() < 4096 {
                                    stderr_buf.push_str(&String::from_utf8_lossy(&data));
                                }
                            }
                            Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                                exit_code = Some(exit_status);
                            }
                            Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => {
                                break;
                            }
                            _ => {}
                        }
                    }

                    drop(tx);
                    let extract_res = extractor_handle
                        .await
                        .map_err(|e| format!("Extractor thread error: {}", e))??;

                    if let Some(code) = exit_code {
                        if code != 0 && extract_res.total_files == 0 && extract_res.skipped_items == 0 {
                            let detail = stderr_buf.trim();
                            return Err(if detail.is_empty() {
                                format!("Remote tar stream exited with code {}. Use regular SFTP download if shell/tar is unavailable.", code)
                            } else {
                                format!("Remote tar stream error (code {}): {}", code, detail)
                            });
                        }
                    }
                    extract_res
                }
                ActiveSftpBackend::Ssh2 { ref sess } => {
                    let sess_c = sess.clone();
                    let app_c = app.clone();
                    let sid_c = sftp_session_id.to_string();
                    let local_base_c = local_base.clone();
                    let cancel_c = cancel_flag.clone();
                    let cmd_c = cmd.clone();

                    tokio::task::spawn_blocking(move || -> Result<TarExtractStats, String> {
                        let sess_guard = sess_c.lock();
                        let mut channel = sess_guard
                            .channel_session()
                            .map_err(|e| format!("Failed to open SSH exec channel: {}", e))?;
                        channel
                            .exec(&cmd_c)
                            .map_err(|e| format!("Failed to execute tar stream: {}", e))?;

                        let mut extractor = TarStreamExtractor::new(
                            local_base_c,
                            overwrite,
                            app_c,
                            sid_c,
                            started_at,
                        );
                        let mut buf = vec![0u8; 128 * 1024];
                        loop {
                            if cancel_c.load(Ordering::SeqCst) {
                                let _ = channel.close();
                                return Err("Transfer cancelled by user".to_string());
                            }
                            match channel.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => extractor.feed(&buf[..n])?,
                                Err(e) => return Err(format!("Error reading tar stream: {}", e)),
                            }
                        }

                        let mut stderr_str = String::new();
                        let _ = channel.stderr().read_to_string(&mut stderr_str);
                        let _ = channel.wait_close();
                        let code = channel.exit_status().unwrap_or(0);
                        let stats = extractor.finish()?;
                        if code != 0 && stats.total_files == 0 && stats.skipped_items == 0 {
                            let detail = stderr_str.trim();
                            return Err(if detail.is_empty() {
                                format!("Remote tar stream failed with exit code {}", code)
                            } else {
                                format!("Remote tar stream error: {}", detail)
                            });
                        }
                        Ok(stats)
                    })
                    .await
                    .map_err(|e| format!("Stream task error: {}", e))??
                }
            };

            total_files += stats.total_files;
            total_bytes += stats.total_bytes;
            skipped_items += stats.skipped_items;
        }

        Ok(BatchTransferResult {
            total_files,
            total_bytes,
            skipped_items,
        })
    }

    fn recursive_download_ssh2_opts(
        app: &AppHandle,
        sftp_session_id: &str,
        sftp: &Sftp,
        remote_dir: &str,
        local_dir: &Path,
        rel_with_root: &str,
        rel_inner: &str,
        excludes: &[String],
        overwrite: bool,
        cancel_flag: &AtomicBool,
        started_at: Instant,
        total_files: &mut u32,
        total_bytes: &mut u64,
        skipped_items: &mut u32,
    ) -> Result<(), String> {
        if cancel_flag.load(Ordering::SeqCst) {
            return Err("Transfer cancelled by user".to_string());
        }

        fs::create_dir_all(local_dir)
            .map_err(|e| format!("Failed to create local folder '{}': {}", local_dir.display(), e))?;

        let entries = sftp
            .readdir(Path::new(remote_dir))
            .map_err(|e| format!("Failed to read remote directory '{}': {}", remote_dir, e))?;

        for (pb, stat) in entries {
            if cancel_flag.load(Ordering::SeqCst) {
                return Err("Transfer cancelled by user".to_string());
            }
            let name = pb
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }

            let child_rel_root = format!("{}/{}", rel_with_root, name);
            let child_rel_inner = if rel_inner.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", rel_inner, name)
            };
            if matches_sftp_exclude(&child_rel_root, &child_rel_inner, &name, excludes) {
                *skipped_items += 1;
                continue;
            }

            let remote_item_path = join_remote_path(remote_dir, &name);
            let local_item_path = local_dir.join(&name);

            let ft = stat.file_type();
            if ft.is_dir() {
                Self::recursive_download_ssh2_opts(
                    app,
                    sftp_session_id,
                    sftp,
                    &remote_item_path,
                    &local_item_path,
                    &child_rel_root,
                    &child_rel_inner,
                    excludes,
                    overwrite,
                    cancel_flag,
                    started_at,
                    total_files,
                    total_bytes,
                    skipped_items,
                )?;
            } else if ft.is_file() {
                if !overwrite {
                    if let Ok(m) = fs::metadata(&local_item_path) {
                        if m.is_file() && m.len() == stat.size.unwrap_or(u64::MAX) {
                            *skipped_items += 1;
                            continue;
                        }
                    }
                }
                if let Ok(n) = download_single_file_ssh2(sftp, &remote_item_path, &local_item_path) {
                    *total_bytes += n;
                    *total_files += 1;
                    emit_transfer_progress_timed(app, sftp_session_id, "download", &name, *total_files, *total_bytes, started_at);
                }
            }
        }

        Ok(())
    }

    pub async fn edit_remote_file(
        &self,
        app: AppHandle,
        sftp_session_id: String,
        remote_path: String,
    ) -> Result<WatchInfo, String> {
        // Return existing watch if this remote file is already open for editing
        {
            let lock = self.watches.lock();
            if let Some(existing) = lock.values().find(|w| {
                w.info.sftp_session_id == sftp_session_id && w.info.remote_path == remote_path
            }) {
                let info = existing.info.clone();
                let temp_path = PathBuf::from(&info.local_temp_path);
                drop(lock);
                open_with_default_editor(&app, &temp_path)?;
                return Ok(info);
            }
        }

        let backend = {
            let lock = self.sessions.lock();
            let session = lock
                .get(&sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            session.backend.clone()
        };

        let file_name = Path::new(&remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("remote_file")
            .to_string();

        let watch_id = uuid::Uuid::new_v4().to_string();
        let temp_dir = get_session_edit_temp_dir(&sftp_session_id).join(&watch_id[..8]);
        tokio::fs::create_dir_all(&temp_dir)
            .await
            .map_err(|e| format!("Failed to create temporary edit directory: {}", e))?;

        let local_temp_path = temp_dir.join(&file_name);

        // Download current remote file contents into local temp path
        match backend {
            ActiveSftpBackend::Russh { ref sftp, .. } => {
                download_single_file_russh(sftp, &remote_path, &local_temp_path).await?;
            }
            ActiveSftpBackend::Ssh2 { ref sess } => {
                let sess_guard = sess.lock();
                let sftp = sess_guard
                    .sftp()
                    .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;
                download_single_file_ssh2(&sftp, &remote_path, &local_temp_path)?;
            }
        }

        let initial_bytes = tokio::fs::read(&local_temp_path)
            .await
            .map_err(|e| format!("Failed to read temporary edited file: {}", e))?;
        let initial_meta = tokio::fs::metadata(&local_temp_path).await.ok();
        let initial_mtime = initial_meta.as_ref().and_then(|m| m.modified().ok());
        let initial_len = initial_meta.as_ref().map(|m| m.len()).unwrap_or(initial_bytes.len() as u64);

        let info = WatchInfo {
            watch_id: watch_id.clone(),
            sftp_session_id: sftp_session_id.clone(),
            remote_path: remote_path.clone(),
            file_name: file_name.clone(),
            local_temp_path: local_temp_path.to_string_lossy().to_string(),
            upload_count: 0,
            last_upload_at: 0,
        };

        let active_flag = Arc::new(AtomicBool::new(true));

        {
            let mut lock = self.watches.lock();
            lock.insert(
                watch_id.clone(),
                ActiveFileWatch {
                    info: info.clone(),
                    active: active_flag.clone(),
                },
            );
        }

        // Open with user's OS default editor
        open_with_default_editor(&app, &local_temp_path)?;

        // Spawn background file watcher to auto-upload on save
        let watches_arc = self.watches.clone();
        let sessions_arc = self.sessions.clone();
        let watch_id_task = watch_id.clone();

        tokio::spawn(async move {
            let mut last_synced_bytes = initial_bytes;
            let mut last_mtime = initial_mtime;
            let mut last_len = initial_len;

            loop {
                tokio::time::sleep(Duration::from_millis(450)).await;

                if !active_flag.load(Ordering::SeqCst) {
                    break;
                }
                if !sessions_arc.lock().contains_key(&sftp_session_id) {
                    break;
                }

                let meta = match tokio::fs::metadata(&local_temp_path).await {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                let cur_mtime = meta.modified().ok();
                let cur_len = meta.len();

                if cur_mtime != last_mtime || cur_len != last_len {
                    // Brief debounce so atomic saves by editors finish flushing to disk
                    tokio::time::sleep(Duration::from_millis(250)).await;

                    let new_bytes = match tokio::fs::read(&local_temp_path).await {
                        Ok(b) => b,
                        Err(_) => continue,
                    };

                    if let Ok(m2) = tokio::fs::metadata(&local_temp_path).await {
                        last_mtime = m2.modified().ok();
                        last_len = m2.len();
                    } else {
                        last_mtime = cur_mtime;
                        last_len = cur_len;
                    }

                    if new_bytes == last_synced_bytes {
                        continue;
                    }

                    match upload_bytes_to_backend(&backend, &remote_path, &new_bytes).await {
                        Ok(bytes_transferred) => {
                            last_synced_bytes = new_bytes;
                            let now_ts = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);

                            let mut new_count = 1u32;
                            {
                                let mut w_lock = watches_arc.lock();
                                if let Some(w) = w_lock.get_mut(&watch_id_task) {
                                    w.info.upload_count += 1;
                                    w.info.last_upload_at = now_ts;
                                    new_count = w.info.upload_count;
                                }
                            }

                            println!(
                                "[RustTerm SFTP] Auto-uploaded {} ({} bytes) -> {}",
                                file_name, bytes_transferred, remote_path
                            );

                            let _ = app.emit(
                                "sftp-auto-upload",
                                SftpAutoUploadEvent {
                                    sftp_session_id: sftp_session_id.clone(),
                                    watch_id: watch_id_task.clone(),
                                    remote_path: remote_path.clone(),
                                    file_name: file_name.clone(),
                                    bytes_transferred,
                                    upload_count: new_count,
                                    status: "success".to_string(),
                                    error: None,
                                },
                            );
                        }
                        Err(err) => {
                            let _ = app.emit(
                                "sftp-auto-upload",
                                SftpAutoUploadEvent {
                                    sftp_session_id: sftp_session_id.clone(),
                                    watch_id: watch_id_task.clone(),
                                    remote_path: remote_path.clone(),
                                    file_name: file_name.clone(),
                                    bytes_transferred: 0,
                                    upload_count: 0,
                                    status: "error".to_string(),
                                    error: Some(err),
                                },
                            );
                        }
                    }
                }
            }
        });

        Ok(info)
    }

    pub fn stop_watch(&self, watch_id: &str) -> Result<(), String> {
        let removed = {
            let mut lock = self.watches.lock();
            lock.remove(watch_id)
        };
        if let Some(w) = removed {
            w.active.store(false, Ordering::SeqCst);
            let p = PathBuf::from(&w.info.local_temp_path);
            if let Some(parent) = p.parent() {
                let _ = fs::remove_dir_all(parent);
            }
        }
        Ok(())
    }

    pub fn reopen_watch(&self, app: &AppHandle, watch_id: &str) -> Result<(), String> {
        let temp_path = {
            let lock = self.watches.lock();
            let w = lock
                .get(watch_id)
                .ok_or_else(|| "File watch not found".to_string())?;
            PathBuf::from(&w.info.local_temp_path)
        };
        open_with_default_editor(app, &temp_path)
    }

    pub fn list_watches(&self, sftp_session_id: &str) -> Vec<WatchInfo> {
        let lock = self.watches.lock();
        lock.values()
            .filter(|w| w.info.sftp_session_id == sftp_session_id)
            .map(|w| w.info.clone())
            .collect()
    }

    pub async fn upload_bytes(
        &self,
        sftp_session_id: &str,
        file_name: &str,
        remote_dir: &str,
        bytes: Vec<u8>,
    ) -> Result<UploadResult, String> {
        let backend = {
            let lock = self.sessions.lock();
            let session = lock
                .get(sftp_session_id)
                .ok_or_else(|| "SFTP session not found".to_string())?;
            session.backend.clone()
        };
        let remote_path = join_remote_path(remote_dir, file_name);
        let written = upload_bytes_to_backend(&backend, &remote_path, &bytes).await?;
        Ok(UploadResult {
            remote_path,
            bytes_transferred: written,
        })
    }
}

fn join_remote_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{}", name)
    } else if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
}

fn emit_transfer_progress(
    app: &AppHandle,
    sftp_session_id: &str,
    direction: &str,
    current_item: &str,
    total_files: u32,
    total_bytes: u64,
) {
    let _ = app.emit(
        "sftp-transfer-progress",
        SftpTransferProgressEvent {
            sftp_session_id: sftp_session_id.to_string(),
            direction: direction.to_string(),
            current_item: current_item.to_string(),
            total_files,
            total_bytes,
            speed_bps: 0,
            elapsed_secs: 0,
        },
    );
}

fn emit_transfer_progress_timed(
    app: &AppHandle,
    sftp_session_id: &str,
    direction: &str,
    current_item: &str,
    total_files: u32,
    total_bytes: u64,
    started_at: Instant,
) {
    let elapsed_ms = started_at.elapsed().as_millis().max(1) as u64;
    let speed_bps = total_bytes.saturating_mul(1000) / elapsed_ms;
    let _ = app.emit(
        "sftp-transfer-progress",
        SftpTransferProgressEvent {
            sftp_session_id: sftp_session_id.to_string(),
            direction: direction.to_string(),
            current_item: current_item.to_string(),
            total_files,
            total_bytes,
            speed_bps,
            elapsed_secs: elapsed_ms / 1000,
        },
    );
}

fn emit_throttled_progress(
    app: &AppHandle,
    sftp_session_id: &str,
    direction: &str,
    current_item: &str,
    total_files: u32,
    total_bytes: u64,
    started_at: Instant,
    last_emit_ms: &AtomicU64,
) {
    let now_ms = started_at.elapsed().as_millis() as u64;
    let prev = last_emit_ms.load(Ordering::Relaxed);
    if now_ms.saturating_sub(prev) >= 250 {
        if last_emit_ms
            .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            emit_transfer_progress_timed(
                app,
                sftp_session_id,
                direction,
                current_item,
                total_files,
                total_bytes,
                started_at,
            );
        }
    }
}

fn posix_shell_quote(input: &str) -> String {
    let escaped = input.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

fn normalize_exclude_patterns(excludes: &[String]) -> Vec<String> {
    let mut patterns: Vec<String> = Vec::new();
    for raw in excludes {
        let sanitized: String = raw.chars().filter(|c| !c.is_control()).collect();
        let mut pat = sanitized.trim().replace('\\', "/");
        while pat.starts_with("./") {
            pat = pat[2..].to_string();
        }
        let pat = pat.trim_matches('/').to_string();
        if !pat.is_empty() && !patterns.contains(&pat) {
            patterns.push(pat);
        }
    }
    patterns
}

fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(si) = star_idx {
            pi = si + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn matches_sftp_exclude(rel_with_root: &str, rel_inner: &str, name: &str, excludes: &[String]) -> bool {
    if excludes.is_empty() {
        return false;
    }
    for pat in excludes {
        if pat.contains('/') {
            if (!rel_inner.is_empty() && simple_glob_match(pat, rel_inner))
                || simple_glob_match(pat, rel_with_root)
            {
                return true;
            }
        } else if simple_glob_match(pat, name) {
            return true;
        }
    }
    false
}

fn build_tar_exclude_args(excludes: &[String], root_names: &[String]) -> Vec<String> {
    let mut patterns = normalize_exclude_patterns(excludes);
    let snapshot = patterns.clone();

    for pat in &snapshot {
        // If pattern has '/' (e.g. "public/cache"), also match relative to streamed root folder name
        if pat.contains('/') {
            for rn in root_names {
                let prefix = format!("{}/", rn);
                if !pat.starts_with(&prefix) {
                    let rooted = format!("{}{}", prefix, pat);
                    if !patterns.contains(&rooted) {
                        patterns.push(rooted);
                    }
                }
            }
        }
    }

    patterns
        .into_iter()
        .map(|p| format!("--exclude={}", posix_shell_quote(&p)))
        .collect()
}

fn collect_remote_downloads_russh<'a>(
    sftp: &'a russh_sftp::client::SftpSession,
    remote_dir: &'a str,
    local_dir: &'a Path,
    rel_with_root: &'a str,
    rel_inner: &'a str,
    excludes: &'a [String],
    overwrite: bool,
    cancel_flag: &'a AtomicBool,
    queue: &'a mut Vec<(String, PathBuf, u64)>,
    skipped_items: &'a mut u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        if cancel_flag.load(Ordering::SeqCst) {
            return Err("Transfer cancelled by user".to_string());
        }

        tokio::fs::create_dir_all(local_dir)
            .await
            .map_err(|e| format!("Failed to create local folder '{}': {}", local_dir.display(), e))?;

        let read_dir = sftp
            .read_dir(remote_dir)
            .await
            .map_err(|e| format!("Failed to read remote directory '{}': {}", remote_dir, e))?;

        for entry in read_dir {
            if cancel_flag.load(Ordering::SeqCst) {
                return Err("Transfer cancelled by user".to_string());
            }
            let name = entry.file_name();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }

            let child_rel_root = format!("{}/{}", rel_with_root, name);
            let child_rel_inner = if rel_inner.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", rel_inner, name)
            };
            if matches_sftp_exclude(&child_rel_root, &child_rel_inner, &name, excludes) {
                *skipped_items += 1;
                continue;
            }

            let remote_item_path = join_remote_path(remote_dir, &name);
            let local_item_path = local_dir.join(&name);

            let ft = entry.file_type();
            if ft.is_dir() {
                collect_remote_downloads_russh(
                    sftp,
                    &remote_item_path,
                    &local_item_path,
                    &child_rel_root,
                    &child_rel_inner,
                    excludes,
                    overwrite,
                    cancel_flag,
                    queue,
                    skipped_items,
                )
                .await?;
            } else if ft.is_file() {
                let remote_size = entry.metadata().size.unwrap_or(u64::MAX);
                if !overwrite {
                    if let Ok(m) = tokio::fs::metadata(&local_item_path).await {
                        if m.is_file() && m.len() == remote_size {
                            *skipped_items += 1;
                            continue;
                        }
                    }
                }
                queue.push((remote_item_path, local_item_path, remote_size));
            }
        }

        Ok(())
    })
}

async fn download_single_file_russh_cancellable<F>(
    sftp: &russh_sftp::client::SftpSession,
    remote_path: &str,
    local_path: &Path,
    cancel_flag: &AtomicBool,
    mut on_bytes: F,
) -> Result<u64, String>
where
    F: FnMut(u64),
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

    if let Some(parent) = local_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create local directory '{}': {}", parent.display(), e))?;
    }

    let mut remote_file = sftp
        .open(remote_path)
        .await
        .map_err(|e| format!("Failed to open remote file '{}': {}", remote_path, e))?;
    let local_file = tokio::fs::File::create(local_path)
        .await
        .map_err(|e| format!("Failed to create local file '{}': {}", local_path.display(), e))?;
    let mut writer = BufWriter::with_capacity(256 * 1024, local_file);

    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;

    loop {
        if cancel_flag.load(Ordering::SeqCst) {
            drop(writer);
            let _ = tokio::fs::remove_file(local_path).await;
            return Err("Transfer cancelled by user".to_string());
        }
        let n = remote_file
            .read(&mut buf)
            .await
            .map_err(|e| format!("Error reading remote file '{}': {}", remote_path, e))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .await
            .map_err(|e| format!("Error writing local file '{}': {}", local_path.display(), e))?;
        let delta = n as u64;
        total += delta;
        on_bytes(delta);
    }

    writer
        .flush()
        .await
        .map_err(|e| format!("Error flushing local file '{}': {}", local_path.display(), e))?;
    Ok(total)
}

// =========================================================
// Streaming TAR Extractor (Zero Server Disk, Pipelined IO)
// Supports USTAR, GNU LongName ('L'), PAX ('x'), and Base-256 (>8 GB)
// =========================================================

#[derive(Debug, Clone)]
struct TarExtractStats {
    total_files: u32,
    total_bytes: u64,
    skipped_items: u32,
}

enum TarMetaKind {
    GnuLongName,
    PaxPerFile,
    Skip,
}

enum TarReadState {
    Header,
    MetaData {
        kind: TarMetaKind,
        remaining: u64,
        padding: u64,
        buf: Vec<u8>,
    },
    FileData {
        writer: Option<StdBufWriter<LocalFile>>,
        remaining: u64,
        padding: u64,
        skipped: bool,
    },
    SkippingData {
        remaining: u64,
        padding: u64,
    },
    SkippingPadding {
        remaining: u64,
    },
    Done,
}

struct TarStreamExtractor {
    dest_root: PathBuf,
    overwrite: bool,
    app: AppHandle,
    sftp_session_id: String,
    started_at: Instant,
    last_emit: Instant,
    header_buf: Vec<u8>,
    zero_blocks: u8,
    state: TarReadState,
    pending_gnu_name: Option<String>,
    pending_pax_path: Option<String>,
    pending_pax_size: Option<u64>,
    current_name: String,
    total_files: u32,
    total_bytes: u64,
    skipped_items: u32,
}

impl TarStreamExtractor {
    fn new(
        dest_root: PathBuf,
        overwrite: bool,
        app: AppHandle,
        sftp_session_id: String,
        started_at: Instant,
    ) -> Self {
        Self {
            dest_root,
            overwrite,
            app,
            sftp_session_id,
            started_at,
            last_emit: Instant::now(),
            header_buf: Vec::with_capacity(512),
            zero_blocks: 0,
            state: TarReadState::Header,
            pending_gnu_name: None,
            pending_pax_path: None,
            pending_pax_size: None,
            current_name: String::new(),
            total_files: 0,
            total_bytes: 0,
            skipped_items: 0,
        }
    }

    fn feed(&mut self, mut chunk: &[u8]) -> Result<(), String> {
        while !chunk.is_empty() {
            match &mut self.state {
                TarReadState::Done => break,
                TarReadState::Header => {
                    let need = 512 - self.header_buf.len();
                    let take = chunk.len().min(need);
                    self.header_buf.extend_from_slice(&chunk[..take]);
                    chunk = &chunk[take..];

                    if self.header_buf.len() == 512 {
                        let hdr: [u8; 512] = self.header_buf[..512]
                            .try_into()
                            .map_err(|_| "Invalid tar header size".to_string())?;
                        self.header_buf.clear();
                        self.process_header(&hdr)?;
                    }
                }
                TarReadState::MetaData {
                    kind,
                    remaining,
                    padding,
                    buf,
                } => {
                    let take = (chunk.len() as u64).min(*remaining) as usize;
                    if buf.len() < 1024 * 1024 {
                        buf.extend_from_slice(&chunk[..take]);
                    }
                    *remaining -= take as u64;
                    chunk = &chunk[take..];

                    if *remaining == 0 {
                        let pad = *padding;
                        let data = std::mem::take(buf);
                        match kind {
                            TarMetaKind::GnuLongName => {
                                let s = trim_tar_nul_str(&data);
                                if !s.is_empty() {
                                    self.pending_gnu_name = Some(s);
                                }
                            }
                            TarMetaKind::PaxPerFile => {
                                let (p, sz) = parse_pax_records(&data);
                                if let Some(path) = p {
                                    self.pending_pax_path = Some(path);
                                }
                                if let Some(size) = sz {
                                    self.pending_pax_size = Some(size);
                                }
                            }
                            TarMetaKind::Skip => {}
                        }
                        self.state = if pad > 0 {
                            TarReadState::SkippingPadding { remaining: pad }
                        } else {
                            TarReadState::Header
                        };
                    }
                }
                TarReadState::FileData {
                    writer,
                    remaining,
                    padding,
                    skipped,
                } => {
                    let take = (chunk.len() as u64).min(*remaining) as usize;
                    if let Some(w) = writer.as_mut() {
                        w.write_all(&chunk[..take])
                            .map_err(|e| format!("Failed writing extracted file '{}': {}", self.current_name, e))?;
                    }
                    *remaining -= take as u64;
                    chunk = &chunk[take..];

                    if !*skipped {
                        self.total_bytes += take as u64;
                        if self.last_emit.elapsed() >= Duration::from_millis(250) {
                            self.last_emit = Instant::now();
                            emit_transfer_progress_timed(
                                &self.app,
                                &self.sftp_session_id,
                                "stream",
                                &self.current_name,
                                self.total_files,
                                self.total_bytes,
                                self.started_at,
                            );
                        }
                    }

                    if *remaining == 0 {
                        if let Some(mut w) = writer.take() {
                            w.flush()
                                .map_err(|e| format!("Failed flushing '{}': {}", self.current_name, e))?;
                            self.total_files += 1;
                        } else if *skipped {
                            self.skipped_items += 1;
                        }
                        let pad = *padding;
                        self.state = if pad > 0 {
                            TarReadState::SkippingPadding { remaining: pad }
                        } else {
                            TarReadState::Header
                        };
                    }
                }
                TarReadState::SkippingData { remaining, padding } => {
                    let take = (chunk.len() as u64).min(*remaining) as usize;
                    *remaining -= take as u64;
                    chunk = &chunk[take..];
                    if *remaining == 0 {
                        let pad = *padding;
                        self.state = if pad > 0 {
                            TarReadState::SkippingPadding { remaining: pad }
                        } else {
                            TarReadState::Header
                        };
                    }
                }
                TarReadState::SkippingPadding { remaining } => {
                    let take = (chunk.len() as u64).min(*remaining) as usize;
                    *remaining -= take as u64;
                    chunk = &chunk[take..];
                    if *remaining == 0 {
                        self.state = TarReadState::Header;
                    }
                }
            }
        }
        Ok(())
    }

    fn process_header(&mut self, hdr: &[u8; 512]) -> Result<(), String> {
        if hdr.iter().all(|&b| b == 0) {
            self.zero_blocks += 1;
            if self.zero_blocks >= 2 {
                self.state = TarReadState::Done;
            }
            return Ok(());
        }
        self.zero_blocks = 0;

        // Verify tar header checksum
        let expected_sum = parse_tar_octal(&hdr[148..156])
            .ok_or_else(|| "Corrupted tar stream header (invalid checksum field)".to_string())?;
        let mut actual_sum: u64 = 0;
        for (i, &b) in hdr.iter().enumerate() {
            actual_sum += if (148..156).contains(&i) { b' ' as u64 } else { b as u64 };
        }
        if actual_sum != expected_sum {
            return Err("Invalid tar stream checksum (remote command output may not be a valid tar archive)".to_string());
        }

        let typeflag = hdr[156];
        let hdr_size = parse_tar_size(&hdr[124..136])
            .ok_or_else(|| "Invalid tar entry size".to_string())?;

        // Handle metadata blocks first (do not clear pending attributes for these!)
        match typeflag {
            b'L' => {
                let pad = (512 - (hdr_size % 512)) % 512;
                self.state = if hdr_size == 0 {
                    TarReadState::Header
                } else {
                    TarReadState::MetaData {
                        kind: TarMetaKind::GnuLongName,
                        remaining: hdr_size,
                        padding: pad,
                        buf: Vec::with_capacity(hdr_size.min(4096) as usize),
                    }
                };
                return Ok(());
            }
            b'x' | b'X' => {
                let pad = (512 - (hdr_size % 512)) % 512;
                self.state = if hdr_size == 0 {
                    TarReadState::Header
                } else {
                    TarReadState::MetaData {
                        kind: TarMetaKind::PaxPerFile,
                        remaining: hdr_size,
                        padding: pad,
                        buf: Vec::with_capacity(hdr_size.min(4096) as usize),
                    }
                };
                return Ok(());
            }
            b'K' | b'g' => {
                let pad = (512 - (hdr_size % 512)) % 512;
                self.state = if hdr_size > 0 {
                    TarReadState::MetaData {
                        kind: TarMetaKind::Skip,
                        remaining: hdr_size,
                        padding: pad,
                        buf: Vec::new(),
                    }
                } else {
                    TarReadState::Header
                };
                return Ok(());
            }
            _ => {}
        }

        let raw_path = if let Some(pax_p) = self.pending_pax_path.take() {
            self.pending_gnu_name.take();
            pax_p
        } else if let Some(gnu_p) = self.pending_gnu_name.take() {
            gnu_p
        } else {
            let name = trim_tar_nul_str(&hdr[0..100]);
            let magic = &hdr[257..262];
            if magic == b"ustar" {
                let prefix = trim_tar_nul_str(&hdr[345..500]);
                if prefix.is_empty() {
                    name
                } else if name.is_empty() {
                    prefix
                } else {
                    format!("{}/{}", prefix, name)
                }
            } else {
                name
            }
        };

        let entry_size = self.pending_pax_size.take().unwrap_or(hdr_size);
        let padding = (512 - (entry_size % 512)) % 512;

        let rel_path = match sanitize_tar_rel_path(&raw_path) {
            Some(p) => p,
            None => {
                self.state = if entry_size > 0 {
                    TarReadState::SkippingData {
                        remaining: entry_size,
                        padding,
                    }
                } else if padding > 0 {
                    TarReadState::SkippingPadding { remaining: padding }
                } else {
                    TarReadState::Header
                };
                return Ok(());
            }
        };

        let target_path = self.dest_root.join(&rel_path);
        self.current_name = rel_path.to_string_lossy().to_string();

        match typeflag {
            b'5' => {
                fs::create_dir_all(&target_path).map_err(|e| {
                    format!("Failed to create extracted folder '{}': {}", target_path.display(), e)
                })?;
                self.state = if entry_size > 0 {
                    TarReadState::SkippingData {
                        remaining: entry_size,
                        padding,
                    }
                } else {
                    TarReadState::Header
                };
            }
            b'0' | 0 => {
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        format!("Failed to create parent folder '{}': {}", parent.display(), e)
                    })?;
                }

                let mut skip_file = false;
                if !self.overwrite {
                    if let Ok(m) = fs::metadata(&target_path) {
                        if m.is_file() && m.len() == entry_size {
                            skip_file = true;
                        }
                    }
                }

                if entry_size == 0 {
                    if skip_file {
                        self.skipped_items += 1;
                    } else {
                        LocalFile::create(&target_path).map_err(|e| {
                            format!("Failed to create extracted file '{}': {}", target_path.display(), e)
                        })?;
                        self.total_files += 1;
                    }
                    self.state = TarReadState::Header;
                } else {
                    let writer = if skip_file {
                        None
                    } else {
                        let f = LocalFile::create(&target_path).map_err(|e| {
                            format!("Failed to create extracted file '{}': {}", target_path.display(), e)
                        })?;
                        Some(StdBufWriter::with_capacity(256 * 1024, f))
                    };
                    self.state = TarReadState::FileData {
                        writer,
                        remaining: entry_size,
                        padding,
                        skipped: skip_file,
                    };
                }
            }
            _ => {
                self.state = if entry_size > 0 {
                    TarReadState::SkippingData {
                        remaining: entry_size,
                        padding,
                    }
                } else if padding > 0 {
                    TarReadState::SkippingPadding { remaining: padding }
                } else {
                    TarReadState::Header
                };
            }
        }

        Ok(())
    }

    fn finish(self) -> Result<TarExtractStats, String> {
        match self.state {
            TarReadState::FileData {
                writer,
                remaining,
                ..
            } if remaining > 0 => {
                if let Some(mut w) = writer {
                    let _ = w.flush();
                }
                return Err(format!(
                    "Stream interrupted mid-file '{}' ({} bytes remaining). You can resume remaining files via Parallel SFTP Download + Skip Existing.",
                    self.current_name, remaining
                ));
            }
            TarReadState::Header if !self.header_buf.is_empty() => {
                return Err("TAR stream ended unexpectedly with incomplete header. You can resume remaining files via Parallel SFTP Download + Skip Existing.".to_string());
            }
            TarReadState::FileData {
                writer: Some(mut w), ..
            } => {
                let _ = w.flush();
            }
            _ => {}
        }

        emit_transfer_progress_timed(
            &self.app,
            &self.sftp_session_id,
            "stream",
            &self.current_name,
            self.total_files,
            self.total_bytes,
            self.started_at,
        );
        Ok(TarExtractStats {
            total_files: self.total_files,
            total_bytes: self.total_bytes,
            skipped_items: self.skipped_items,
        })
    }
}

fn trim_tar_nul_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim_end_matches('\n').to_string()
}

fn parse_tar_octal(bytes: &[u8]) -> Option<u64> {
    let s = String::from_utf8_lossy(bytes);
    let trimmed = s.trim_matches(|c: char| c == '\0' || c == ' ');
    if trimmed.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(trimmed, 8).ok()
}

fn parse_tar_size(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return Some(0);
    }
    // GNU/POSIX Base-256 encoding when MSB is set (files > 8 GiB)
    if bytes[0] & 0x80 != 0 {
        let mut val: u64 = (bytes[0] & 0x7f) as u64;
        for &b in &bytes[1..] {
            val = (val << 8) | (b as u64);
        }
        return Some(val);
    }
    parse_tar_octal(bytes)
}

fn parse_pax_records(data: &[u8]) -> (Option<String>, Option<u64>) {
    let mut path = None;
    let mut size = None;
    let mut pos = 0usize;

    while pos < data.len() {
        let rest = &data[pos..];
        let space_idx = match rest.iter().position(|&b| b == b' ') {
            Some(i) => i,
            None => break,
        };
        let len_str = String::from_utf8_lossy(&rest[..space_idx]);
        let rec_len: usize = match len_str.trim().parse() {
            Ok(l) if l > space_idx + 1 && pos + l <= data.len() => l,
            _ => break,
        };
        let record = &data[pos + space_idx + 1..pos + rec_len];
        let record_trimmed = record.strip_suffix(b"\n").unwrap_or(record);
        if let Some(eq_idx) = record_trimmed.iter().position(|&b| b == b'=') {
            let key = &record_trimmed[..eq_idx];
            let val = String::from_utf8_lossy(&record_trimmed[eq_idx + 1..]).to_string();
            if key == b"path" {
                path = Some(val);
            } else if key == b"size" {
                if let Ok(sz) = val.parse::<u64>() {
                    size = Some(sz);
                }
            }
        }
        pos += rec_len;
    }

    (path, size)
}

fn sanitize_tar_rel_path(raw: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in raw.split(&['/', '\\'][..]) {
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        #[cfg(target_os = "windows")]
        {
            if part.ends_with(':') {
                continue;
            }
            let safe_part: String = part
                .chars()
                .map(|c| match c {
                    '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
                    _ => c,
                })
                .collect();
            out.push(safe_part);
        }
        #[cfg(not(target_os = "windows"))]
        {
            out.push(part);
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

async fn delete_remote_entry(
    backend: &ActiveSftpBackend,
    remote_path: &str,
    is_dir: bool,
) -> Result<(), String> {
    match backend {
        ActiveSftpBackend::Russh { sftp, .. } => {
            if is_dir {
                recursive_delete_russh(sftp, remote_path).await
            } else {
                sftp.remove_file(remote_path)
                    .await
                    .map_err(|e| format!("Failed to delete file '{}': {}", remote_path, e))
            }
        }
        ActiveSftpBackend::Ssh2 { sess } => {
            let sess_guard = sess.lock();
            let sftp = sess_guard
                .sftp()
                .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;
            if is_dir {
                recursive_delete_ssh2(&sftp, remote_path)
            } else {
                sftp.unlink(Path::new(remote_path))
                    .map_err(|e| format!("Failed to delete file '{}': {}", remote_path, e))
            }
        }
    }
}

fn recursive_delete_russh<'a>(
    sftp: &'a russh_sftp::client::SftpSession,
    remote_dir: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        let read_dir = sftp
            .read_dir(remote_dir)
            .await
            .map_err(|e| format!("Failed to read directory '{}' for deletion: {}", remote_dir, e))?;

        for entry in read_dir {
            let name = entry.file_name();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            let child_path = join_remote_path(remote_dir, &name);
            let ft = entry.file_type();
            if ft.is_dir() && !ft.is_symlink() {
                recursive_delete_russh(sftp, &child_path).await?;
            } else {
                sftp.remove_file(&child_path)
                    .await
                    .map_err(|e| format!("Failed to delete '{}': {}", child_path, e))?;
            }
        }

        sftp.remove_dir(remote_dir)
            .await
            .map_err(|e| format!("Failed to remove directory '{}': {}", remote_dir, e))
    })
}

fn recursive_delete_ssh2(sftp: &Sftp, remote_dir: &str) -> Result<(), String> {
    let entries = sftp
        .readdir(Path::new(remote_dir))
        .map_err(|e| format!("Failed to read directory '{}' for deletion: {}", remote_dir, e))?;

    for (pb, stat) in entries {
        let name = pb
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let child_path = join_remote_path(remote_dir, &name);
        let ft = stat.file_type();
        if ft.is_dir() && !ft.is_symlink() {
            recursive_delete_ssh2(sftp, &child_path)?;
        } else {
            sftp.unlink(Path::new(&child_path))
                .map_err(|e| format!("Failed to delete '{}': {}", child_path, e))?;
        }
    }

    sftp.rmdir(Path::new(remote_dir))
        .map_err(|e| format!("Failed to remove directory '{}': {}", remote_dir, e))
}

async fn ensure_remote_dir_russh(
    sftp: &russh_sftp::client::SftpSession,
    remote_dir: &str,
) -> Result<(), String> {
    match sftp.metadata(remote_dir).await {
        Ok(attr) => {
            if attr.is_dir() {
                Ok(())
            } else {
                Err(format!("Remote path '{}' exists and is not a directory", remote_dir))
            }
        }
        Err(_) => sftp
            .create_dir(remote_dir)
            .await
            .map_err(|e| format!("Failed to create remote directory '{}': {}", remote_dir, e)),
    }
}

fn ensure_remote_dir_ssh2(sftp: &Sftp, remote_dir: &str) -> Result<(), String> {
    let p = Path::new(remote_dir);
    match sftp.stat(p) {
        Ok(stat) => {
            if stat.file_type().is_dir() {
                Ok(())
            } else {
                Err(format!("Remote path '{}' exists and is not a directory", remote_dir))
            }
        }
        Err(_) => sftp
            .mkdir(p, 0o755)
            .map_err(|e| format!("Failed to create remote directory '{}': {}", remote_dir, e)),
    }
}

async fn upload_single_file_russh(
    sftp: &russh_sftp::client::SftpSession,
    local_path: &Path,
    remote_path: &str,
) -> Result<u64, String> {
    use tokio::io::{AsyncWriteExt, BufReader};

    let local_file = tokio::fs::File::open(local_path)
        .await
        .map_err(|e| format!("Failed to open local file '{}': {}", local_path.display(), e))?;
    let mut reader = BufReader::with_capacity(256 * 1024, local_file);
    let mut remote_file = sftp
        .create(remote_path)
        .await
        .map_err(|e| format!("Failed to create remote file '{}': {}", remote_path, e))?;
    let bytes = tokio::io::copy_buf(&mut reader, &mut remote_file)
        .await
        .map_err(|e| format!("Error uploading '{}': {}", remote_path, e))?;
    let _ = remote_file.shutdown().await;
    Ok(bytes)
}

fn upload_single_file_ssh2(
    sftp: &Sftp,
    local_path: &Path,
    remote_path: &str,
) -> Result<u64, String> {
    let mut local_file = LocalFile::open(local_path)
        .map_err(|e| format!("Failed to open local file '{}': {}", local_path.display(), e))?;
    let mut remote_file = sftp
        .create(Path::new(remote_path))
        .map_err(|e| format!("Failed to create remote file '{}': {}", remote_path, e))?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut bytes = 0u64;
    loop {
        let n = local_file
            .read(&mut buffer)
            .map_err(|e| format!("Error reading local file: {}", e))?;
        if n == 0 {
            break;
        }
        remote_file
            .write_all(&buffer[..n])
            .map_err(|e| format!("Error writing remote file: {}", e))?;
        bytes += n as u64;
    }
    Ok(bytes)
}

async fn download_single_file_russh(
    sftp: &russh_sftp::client::SftpSession,
    remote_path: &str,
    local_path: &Path,
) -> Result<u64, String> {
    use tokio::io::{AsyncWriteExt, BufReader, BufWriter};

    if let Some(parent) = local_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create local directory '{}': {}", parent.display(), e))?;
    }
    let remote_file = sftp
        .open(remote_path)
        .await
        .map_err(|e| format!("Failed to open remote file '{}': {}", remote_path, e))?;
    let mut reader = BufReader::with_capacity(256 * 1024, remote_file);
    let local_file = tokio::fs::File::create(local_path)
        .await
        .map_err(|e| format!("Failed to create local file '{}': {}", local_path.display(), e))?;
    let mut writer = BufWriter::with_capacity(256 * 1024, local_file);
    let n = tokio::io::copy_buf(&mut reader, &mut writer)
        .await
        .map_err(|e| format!("Error transferring remote file '{}': {}", remote_path, e))?;
    let _ = writer.flush().await;
    Ok(n)
}

fn download_single_file_ssh2(
    sftp: &Sftp,
    remote_path: &str,
    local_path: &Path,
) -> Result<u64, String> {
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create local directory '{}': {}", parent.display(), e))?;
    }
    let mut remote_file = sftp
        .open(Path::new(remote_path))
        .map_err(|e| format!("Failed to open remote file '{}': {}", remote_path, e))?;
    let mut local_file = StdBufWriter::with_capacity(256 * 1024, LocalFile::create(local_path)
        .map_err(|e| format!("Failed to create local file '{}': {}", local_path.display(), e))?);
    let mut buffer = vec![0u8; 256 * 1024];
    let mut bytes = 0u64;
    loop {
        let n = remote_file
            .read(&mut buffer)
            .map_err(|e| format!("Error reading remote file: {}", e))?;
        if n == 0 {
            break;
        }
        local_file
            .write_all(&buffer[..n])
            .map_err(|e| format!("Error writing local file: {}", e))?;
        bytes += n as u64;
    }
    Ok(bytes)
}

async fn upload_bytes_to_backend(
    backend: &ActiveSftpBackend,
    remote_path: &str,
    bytes: &[u8],
) -> Result<u64, String> {
    match backend {
        ActiveSftpBackend::Russh { sftp, .. } => {
            use tokio::io::AsyncWriteExt;
            let mut remote_file = sftp
                .create(remote_path)
                .await
                .map_err(|e| format!("Failed to open remote file '{}' for save: {}", remote_path, e))?;
            remote_file
                .write_all(bytes)
                .await
                .map_err(|e| format!("Failed to write remote file '{}': {}", remote_path, e))?;
            remote_file
                .shutdown()
                .await
                .map_err(|e| format!("Failed to finalize remote file '{}': {}", remote_path, e))?;
            Ok(bytes.len() as u64)
        }
        ActiveSftpBackend::Ssh2 { sess } => {
            let sess_guard = sess.lock();
            let sftp = sess_guard
                .sftp()
                .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;
            let mut remote_file = sftp
                .create(Path::new(remote_path))
                .map_err(|e| format!("Failed to open remote file '{}' for save: {}", remote_path, e))?;
            remote_file
                .write_all(bytes)
                .map_err(|e| format!("Failed to write remote file '{}': {}", remote_path, e))?;
            Ok(bytes.len() as u64)
        }
    }
}

fn recursive_upload_russh<'a>(
    app: &'a AppHandle,
    sftp_session_id: &'a str,
    sftp: &'a russh_sftp::client::SftpSession,
    local_dir: &'a Path,
    remote_dir: &'a str,
    overwrite: bool,
    total_files: &'a mut u32,
    total_bytes: &'a mut u64,
    skipped_items: &'a mut u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        ensure_remote_dir_russh(sftp, remote_dir).await?;

        let mut entries = tokio::fs::read_dir(local_dir)
            .await
            .map_err(|e| format!("Failed to read local directory '{}': {}", local_dir.display(), e))?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| format!("Failed to iterate local directory: {}", e))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.is_empty() {
                continue;
            }
            let local_item_path = entry.path();
            let remote_item_path = join_remote_path(remote_dir, &name);

            let ft = entry
                .file_type()
                .await
                .map_err(|e| format!("Failed to inspect '{}': {}", local_item_path.display(), e))?;

            if ft.is_dir() {
                recursive_upload_russh(
                    app,
                    sftp_session_id,
                    sftp,
                    &local_item_path,
                    &remote_item_path,
                    overwrite,
                    total_files,
                    total_bytes,
                    skipped_items,
                )
                .await?;
            } else if ft.is_file() {
                if !overwrite && sftp.symlink_metadata(&remote_item_path).await.is_ok() {
                    *skipped_items += 1;
                    continue;
                }
                let n = upload_single_file_russh(sftp, &local_item_path, &remote_item_path).await?;
                *total_files += 1;
                *total_bytes += n;
                emit_transfer_progress(app, sftp_session_id, "upload", &name, *total_files, *total_bytes);
            }
        }

        Ok(())
    })
}

fn recursive_upload_ssh2(
    app: &AppHandle,
    sftp_session_id: &str,
    sftp: &Sftp,
    local_dir: &Path,
    remote_dir: &str,
    overwrite: bool,
    total_files: &mut u32,
    total_bytes: &mut u64,
    skipped_items: &mut u32,
) -> Result<(), String> {
    ensure_remote_dir_ssh2(sftp, remote_dir)?;

    let entries = fs::read_dir(local_dir)
        .map_err(|e| format!("Failed to read local directory '{}': {}", local_dir.display(), e))?;

    for entry_res in entries {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty() {
            continue;
        }
        let local_item_path = entry.path();
        let remote_item_path = join_remote_path(remote_dir, &name);
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if ft.is_dir() {
            recursive_upload_ssh2(
                app,
                sftp_session_id,
                sftp,
                &local_item_path,
                &remote_item_path,
                overwrite,
                total_files,
                total_bytes,
                skipped_items,
            )?;
        } else if ft.is_file() {
            if !overwrite && sftp.lstat(Path::new(&remote_item_path)).is_ok() {
                *skipped_items += 1;
                continue;
            }
            let n = upload_single_file_ssh2(sftp, &local_item_path, &remote_item_path)?;
            *total_files += 1;
            *total_bytes += n;
            emit_transfer_progress(app, sftp_session_id, "upload", &name, *total_files, *total_bytes);
        }
    }

    Ok(())
}

fn get_session_edit_temp_dir(sftp_session_id: &str) -> PathBuf {
    let safe_id: String = sftp_session_id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    std::env::temp_dir().join("RustTerm_Edit").join(safe_id)
}

pub fn open_with_default_editor(app: &AppHandle, path: &Path) -> Result<(), String> {
    let path_str = path.to_string_lossy().to_string();
    match app.opener().open_path(&path_str, None::<&str>) {
        Ok(()) => Ok(()),
        Err(err) => {
            #[cfg(target_os = "windows")]
            {
                std::process::Command::new("notepad.exe")
                    .arg(&path_str)
                    .spawn()
                    .map_err(|e| format!("Failed to open file '{}' ({}; fallback failed: {})", path_str, err, e))?;
                Ok(())
            }
            #[cfg(not(target_os = "windows"))]
            {
                Err(format!("Failed to open file '{}': {}", path_str, err))
            }
        }
    }
}

// =========================================================
// Local Filesystem Commands (Left Pane of Dual-Pane SFTP)
// =========================================================

pub fn local_get_quick_paths(host_label: &str) -> LocalQuickPaths {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let desktop = dirs::desktop_dir().unwrap_or_else(|| home.join("Desktop"));
    let downloads = dirs::download_dir().unwrap_or_else(|| home.join("Downloads"));
    let documents = dirs::document_dir().unwrap_or_else(|| home.join("Documents"));
    let transfers = get_default_local_transfers_dir(host_label);

    let mut drives = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            if Path::new(&root).exists() {
                drives.push(root);
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        drives.push("/".to_string());
    }

    LocalQuickPaths {
        drives,
        home: home.to_string_lossy().to_string(),
        desktop: desktop.to_string_lossy().to_string(),
        downloads: downloads.to_string_lossy().to_string(),
        documents: documents.to_string_lossy().to_string(),
        transfers: transfers.to_string_lossy().to_string(),
    }
}

fn normalize_local_input_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    }
    #[cfg(target_os = "windows")]
    {
        if trimmed.len() == 2 && trimmed.as_bytes()[1] == b':' {
            return PathBuf::from(format!("{}\\", trimmed));
        }
    }
    PathBuf::from(trimmed)
}

pub fn local_list_dir(target_path: &str) -> Result<LocalListResult, String> {
    let dir_pb = normalize_local_input_path(target_path);
    if !dir_pb.exists() {
        return Err(format!("Local path '{}' does not exist", dir_pb.display()));
    }
    if !dir_pb.is_dir() {
        return Err(format!("Local path '{}' is not a directory", dir_pb.display()));
    }

    let read_dir = fs::read_dir(&dir_pb)
        .map_err(|e| format!("Failed to read local directory '{}': {}", dir_pb.display(), e))?;

    let mut entries = Vec::new();
    for entry_res in read_dir {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty() {
            continue;
        }
        let path_pb = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = meta.is_dir();
        let is_file = meta.is_file();
        let size = if is_dir { 0 } else { meta.len() };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        entries.push(LocalFileEntry {
            name,
            path: path_pb.to_string_lossy().to_string(),
            is_dir,
            is_file,
            size,
            modified,
        });
    }

    entries.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            b.is_dir.cmp(&a.is_dir)
        } else {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        }
    });

    let current_path = dir_pb.to_string_lossy().to_string();
    let parent_path = dir_pb.parent().map(|p| p.to_string_lossy().to_string()).filter(|p| !p.is_empty() && p != &current_path);

    Ok(LocalListResult {
        current_path,
        parent_path,
        entries,
    })
}

pub fn local_create_dir(parent_dir: &str, dir_name: &str) -> Result<(), String> {
    let clean = dir_name.trim();
    if clean.is_empty() {
        return Err("Folder name cannot be empty".to_string());
    }
    let full = Path::new(parent_dir).join(clean);
    fs::create_dir_all(&full)
        .map_err(|e| format!("Failed to create local folder '{}': {}", full.display(), e))
}

pub fn local_delete_items(paths: Vec<String>) -> Result<u32, String> {
    let mut deleted = 0u32;
    for p_str in paths {
        let p = Path::new(&p_str);
        if !p.exists() {
            continue;
        }
        if p.is_dir() {
            fs::remove_dir_all(p)
                .map_err(|e| format!("Failed to delete local folder '{}': {}", p.display(), e))?;
        } else {
            fs::remove_file(p)
                .map_err(|e| format!("Failed to delete local file '{}': {}", p.display(), e))?;
        }
        deleted += 1;
    }
    Ok(deleted)
}

pub fn local_rename_item(old_path: &str, new_name: &str) -> Result<String, String> {
    let clean = new_name.trim();
    if clean.is_empty() || clean.contains('/') || clean.contains('\\') {
        return Err("Invalid new name".to_string());
    }
    let old_pb = Path::new(old_path);
    let parent = old_pb
        .parent()
        .ok_or_else(|| "Cannot determine parent directory".to_string())?;
    let new_pb = parent.join(clean);
    fs::rename(old_pb, &new_pb)
        .map_err(|e| format!("Failed to rename '{}': {}", old_path, e))?;
    Ok(new_pb.to_string_lossy().to_string())
}

pub fn local_check_conflicts(
    items: Vec<TransferTargetItem>,
    local_dir: &str,
) -> Result<Vec<ConflictItem>, String> {
    let dest_root = Path::new(local_dir);
    let mut conflicts = Vec::new();

    for item in items {
        let candidate = dest_root.join(&item.name);
        if candidate.exists() {
            let meta = fs::metadata(&candidate).ok();
            let dest_is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(candidate.is_dir());
            let existing_size = if dest_is_dir {
                0
            } else {
                meta.as_ref().map(|m| m.len()).unwrap_or(0)
            };
            conflicts.push(ConflictItem {
                name: item.name,
                source_path: item.path,
                dest_path: candidate.to_string_lossy().to_string(),
                source_is_dir: item.is_dir,
                dest_is_dir,
                existing_size,
            });
        }
    }

    Ok(conflicts)
}

pub fn local_copy_paths(
    source_paths: Vec<String>,
    dest_dir: &str,
    overwrite: bool,
) -> Result<BatchTransferResult, String> {
    let dest_root = Path::new(dest_dir);
    fs::create_dir_all(dest_root)
        .map_err(|e| format!("Failed to prepare local destination '{}': {}", dest_dir, e))?;

    let mut total_files = 0u32;
    let mut total_bytes = 0u64;
    let mut skipped_items = 0u32;

    for src_str in source_paths {
        let src_pb = Path::new(&src_str);
        if !src_pb.exists() {
            continue;
        }
        let name = match src_pb.file_name().and_then(|n| n.to_str()) {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let target_pb = dest_root.join(name);

        // Avoid copying onto itself
        if let (Ok(c1), Ok(c2)) = (fs::canonicalize(src_pb), fs::canonicalize(&target_pb)) {
            if c1 == c2 {
                skipped_items += 1;
                continue;
            }
        }

        if src_pb.is_dir() {
            if !overwrite && target_pb.exists() {
                skipped_items += 1;
                continue;
            }
            copy_local_dir_recursive(src_pb, &target_pb, overwrite, &mut total_files, &mut total_bytes, &mut skipped_items)?;
        } else {
            if !overwrite && target_pb.exists() {
                skipped_items += 1;
                continue;
            }
            let copied = fs::copy(src_pb, &target_pb)
                .map_err(|e| format!("Failed to copy '{}': {}", src_pb.display(), e))?;
            total_files += 1;
            total_bytes += copied;
        }
    }

    Ok(BatchTransferResult {
        total_files,
        total_bytes,
        skipped_items,
    })
}

fn copy_local_dir_recursive(
    src_dir: &Path,
    dest_dir: &Path,
    overwrite: bool,
    total_files: &mut u32,
    total_bytes: &mut u64,
    skipped_items: &mut u32,
) -> Result<(), String> {
    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed to create folder '{}': {}", dest_dir.display(), e))?;

    let entries = fs::read_dir(src_dir)
        .map_err(|e| format!("Failed to read folder '{}': {}", src_dir.display(), e))?;

    for entry_res in entries {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        let src_item = entry.path();
        let dest_item = dest_dir.join(entry.file_name());
        let ft = match entry.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };

        if ft.is_dir() {
            copy_local_dir_recursive(&src_item, &dest_item, overwrite, total_files, total_bytes, skipped_items)?;
        } else if ft.is_file() {
            if !overwrite && dest_item.exists() {
                *skipped_items += 1;
                continue;
            }
            let n = fs::copy(&src_item, &dest_item)
                .map_err(|e| format!("Failed to copy '{}': {}", src_item.display(), e))?;
            *total_files += 1;
            *total_bytes += n;
        }
    }

    Ok(())
}

pub fn get_default_local_transfers_dir(host_label: &str) -> PathBuf {
    let mut base = dirs::download_dir().unwrap_or_else(|| {
        let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        p.push("Downloads");
        p
    });

    base.push("RustTerm_Transfers");
    let safe_host: String = host_label
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    base.push(safe_host);
    let _ = fs::create_dir_all(&base);
    base
}

pub fn open_path_in_explorer(target_path: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let p = Path::new(target_path);
        if p.is_file() {
            std::process::Command::new("explorer")
                .arg(format!("/select,{}", p.display()))
                .spawn()
                .map_err(|e| format!("Failed to launch Windows Explorer: {}", e))?;
        } else {
            std::process::Command::new("explorer")
                .arg(target_path)
                .spawn()
                .map_err(|e| format!("Failed to launch Windows Explorer: {}", e))?;
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let p = Path::new(target_path);
        let folder = if p.is_file() { p.parent().unwrap_or(p) } else { p };
        std::process::Command::new("xdg-open")
            .arg(folder)
            .spawn()
            .map_err(|e| format!("Failed to open file manager: {}", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_join_remote_path() {
        assert_eq!(join_remote_path("/", "etc"), "/etc");
        assert_eq!(join_remote_path("/home/ubuntu", "app.js"), "/home/ubuntu/app.js");
        assert_eq!(join_remote_path("/home/ubuntu/", "app.js"), "/home/ubuntu/app.js");
    }

    #[test]
    fn test_local_fs_operations_and_conflicts() {
        let base = std::env::temp_dir().join(format!("rustterm_sftp_test_{}", uuid::Uuid::new_v4()));
        let src_dir = base.join("src");
        let dest_dir = base.join("dest");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();

        let src_file = src_dir.join("hello.txt");
        let sub_dir = src_dir.join("nested");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(&src_file, b"new content").unwrap();
        fs::write(sub_dir.join("inner.txt"), b"inside").unwrap();

        // Pre-populate dest with conflicting hello.txt
        let dest_file = dest_dir.join("hello.txt");
        fs::write(&dest_file, b"old").unwrap();

        let listed = local_list_dir(&src_dir.to_string_lossy()).unwrap();
        assert_eq!(listed.entries.len(), 2);
        assert!(listed.entries[0].is_dir);
        assert_eq!(listed.entries[0].name, "nested");

        let conflicts = local_check_conflicts(
            vec![
                TransferTargetItem {
                    path: src_file.to_string_lossy().to_string(),
                    name: "hello.txt".to_string(),
                    is_dir: false,
                    size: 11,
                },
                TransferTargetItem {
                    path: sub_dir.to_string_lossy().to_string(),
                    name: "nested".to_string(),
                    is_dir: true,
                    size: 0,
                },
            ],
            &dest_dir.to_string_lossy(),
        )
        .unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].name, "hello.txt");
        assert_eq!(conflicts[0].existing_size, 3);

        // Copy with overwrite = false should skip hello.txt and copy nested/inner.txt
        let res_skip = local_copy_paths(
            vec![
                src_file.to_string_lossy().to_string(),
                sub_dir.to_string_lossy().to_string(),
            ],
            &dest_dir.to_string_lossy(),
            false,
        )
        .unwrap();
        assert_eq!(res_skip.skipped_items, 1);
        assert_eq!(res_skip.total_files, 1);
        assert_eq!(fs::read(&dest_file).unwrap(), b"old");
        assert_eq!(fs::read(dest_dir.join("nested").join("inner.txt")).unwrap(), b"inside");

        // Copy with overwrite = true should overwrite hello.txt
        let res_overwrite = local_copy_paths(
            vec![src_file.to_string_lossy().to_string()],
            &dest_dir.to_string_lossy(),
            true,
        )
        .unwrap();
        assert_eq!(res_overwrite.skipped_items, 0);
        assert_eq!(res_overwrite.total_files, 1);
        assert_eq!(fs::read(&dest_file).unwrap(), b"new content");

        // Rename & delete recursive
        let renamed = local_rename_item(&dest_file.to_string_lossy(), "hello2.txt").unwrap();
        assert!(Path::new(&renamed).exists());
        let deleted_count = local_delete_items(vec![base.to_string_lossy().to_string()]).unwrap();
        assert_eq!(deleted_count, 1);
        assert!(!base.exists());
    }
}
