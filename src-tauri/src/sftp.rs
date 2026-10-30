use std::collections::HashMap;
use std::fs::{self, File as LocalFile};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use ssh2::{Session, Sftp};

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

#[derive(Clone)]
pub enum ActiveSftpBackend {
    Russh {
        sftp: Arc<russh_sftp::client::SftpSession>,
        _session: Arc<russh::client::Handle<RusshClientHandler>>,
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
    Ok((sftp, session))
}

impl SftpManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
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
                            _session: Arc::new(session_handle),
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
        let session = {
            let mut lock = self.sessions.lock();
            lock.remove(sftp_session_id)
        };

        if let Some(session) = session {
            match session.backend {
                ActiveSftpBackend::Russh { sftp, _session } => {
                    let _ = sftp.close().await;
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

        match backend {
            ActiveSftpBackend::Russh { ref sftp, .. } => {
                if is_dir {
                    sftp.remove_dir(remote_path)
                        .await
                        .map_err(|e| format!("Failed to remove directory '{}': {}", remote_path, e))?;
                } else {
                    sftp.remove_file(remote_path)
                        .await
                        .map_err(|e| format!("Failed to delete file '{}': {}", remote_path, e))?;
                }
            }
            ActiveSftpBackend::Ssh2 { ref sess } => {
                let sess_guard = sess.lock();
                let sftp = sess_guard
                    .sftp()
                    .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;
                let p = Path::new(remote_path);
                if is_dir {
                    sftp.rmdir(p)
                        .map_err(|e| format!("Failed to remove directory '{}': {}", remote_path, e))?;
                } else {
                    sftp.unlink(p)
                        .map_err(|e| format!("Failed to delete file '{}': {}", remote_path, e))?;
                }
            }
        }

        Ok(())
    }
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
