use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use ssh2::Session;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub group: Option<String>,
    pub hostname: String,
    pub port: u16,
    pub username: String,
    pub auth_mode: String, // "password", "private_key", "both", "interactive"
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub private_key: Option<String>,
    #[serde(default)]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub keepalive_interval: Option<u32>,
    #[serde(default)]
    pub keepalive_max: Option<u32>,
    #[serde(default)]
    pub auto_reconnect: Option<bool>,
    #[serde(default)]
    pub persistent_session: Option<bool>,
    #[serde(default)]
    pub session_name: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportPayload {
    pub app: String,
    pub version: String,
    pub exported_at: String,
    pub security_notice: String,
    pub hosts: Vec<HostConfig>,
}

pub enum SshCommand {
    Data(Vec<u8>),
    Resize(u32, u32),
    Keepalive,
    Disconnect,
}

pub struct ActiveSession {
    pub tx: UnboundedSender<SshCommand>,
    pub is_alive: Arc<AtomicBool>,
}

pub struct RusshClientHandler;

impl russh::client::Handler for RusshClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub async fn establish_authenticated_russh_session(
    host: &HostConfig,
) -> Result<russh::client::Handle<RusshClientHandler>, String> {
    println!("[RustTerm SSH] [russh engine] Connecting to {}:{}", host.hostname, host.port);

    let config = Arc::new(russh::client::Config {
        inactivity_timeout: Some(Duration::from_secs(host.keepalive_interval.unwrap_or(30) as u64 * 3)),
        keepalive_interval: Some(Duration::from_secs(host.keepalive_interval.unwrap_or(30) as u64)),
        ..Default::default()
    });

    let connect_fut = russh::client::connect(config, (host.hostname.as_str(), host.port), RusshClientHandler);
    let mut session = tokio::time::timeout(Duration::from_secs(12), connect_fut)
        .await
        .map_err(|_| format!("Connection timeout to {}:{}", host.hostname, host.port))?
        .map_err(|e| format!("Failed to connect to {}:{}: {}", host.hostname, host.port, e))?;

    let mut authenticated = false;

    match host.auth_mode.as_str() {
        "password" => {
            let pwd = host.password.as_deref().unwrap_or("");
            let res = session
                .authenticate_password(&host.username, pwd)
                .await
                .map_err(|e| format!("Password auth error: {}", e))?;
            authenticated = res.success();
        }
        "private_key" => {
            let key_str = host.private_key.as_deref().unwrap_or("");
            if key_str.trim().is_empty() {
                return Err("Private key content is empty".to_string());
            }
            let pass = host.passphrase.as_deref().filter(|s| !s.is_empty());
            let secret_key = russh::keys::decode_secret_key(key_str, pass)
                .map_err(|e| format!("Failed to parse private key: {}", e))?;
            let key_with_alg = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(secret_key), None);
            let res = session
                .authenticate_publickey(&host.username, key_with_alg)
                .await
                .map_err(|e| format!("Public key authentication error: {}", e))?;
            authenticated = res.success();
        }
        "both" => {
            let key_str = host.private_key.as_deref().unwrap_or("");
            let pass = host.passphrase.as_deref().filter(|s| !s.is_empty());
            if !key_str.trim().is_empty() {
                if let Ok(secret_key) = russh::keys::decode_secret_key(key_str, pass) {
                    let key_with_alg = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(secret_key), None);
                    if let Ok(res) = session.authenticate_publickey(&host.username, key_with_alg).await {
                        authenticated = res.success();
                    }
                }
            }
            if !authenticated {
                let pwd = host.password.as_deref().unwrap_or("");
                if !pwd.is_empty() {
                    let res = session
                        .authenticate_password(&host.username, pwd)
                        .await
                        .map_err(|e| format!("Password auth error: {}", e))?;
                    authenticated = res.success();
                }
            }
        }
        "interactive" => {
            let pwd = host.password.as_deref().unwrap_or("");
            let res = session
                .authenticate_password(&host.username, pwd)
                .await
                .map_err(|e| format!("Interactive auth error: {}", e))?;
            authenticated = res.success();
        }
        _ => {
            return Err(format!("Unknown auth mode: {}", host.auth_mode));
        }
    }

    if !authenticated {
        return Err(format!(
            "Authentication failed for user '{}' on {}:{}. Please check credentials or private key.",
            host.username, host.hostname, host.port
        ));
    }

    println!("[RustTerm SSH] [russh engine] Authenticated user '{}' on {}:{}", host.username, host.hostname, host.port);
    Ok(session)
}


#[derive(Default)]
pub struct SshManager {
    sessions: Arc<Mutex<HashMap<String, ActiveSession>>>,
}

struct InteractiveAuth {
    password: String,
}

impl ssh2::KeyboardInteractivePrompt for InteractiveAuth {
    fn prompt<'a>(
        &mut self,
        _username: &str,
        _instructions: &str,
        prompts: &[ssh2::Prompt<'a>],
    ) -> Vec<String> {
        prompts
            .iter()
            .map(|p| {
                if p.text.to_lowercase().contains("password") {
                    self.password.clone()
                } else {
                    String::new()
                }
            })
            .collect()
    }
}

pub fn auth_with_key(
    sess: &Session,
    username: &str,
    key_content: &str,
    passphrase: Option<&str>,
) -> Result<(), String> {
    let mut temp_key_path = std::env::temp_dir();
    temp_key_path.push(format!("rustterm_key_{}.pem", uuid::Uuid::new_v4()));

    std::fs::write(&temp_key_path, key_content)
        .map_err(|e| format!("Failed to prepare private key for authentication: {}", e))?;

    let res = sess
        .userauth_pubkey_file(username, None, &temp_key_path, passphrase)
        .map_err(|e| format!("Public key authentication failed: {}", e));

    let _ = std::fs::remove_file(&temp_key_path);
    res
}

pub fn establish_authenticated_session(host: &HostConfig) -> Result<Session, String> {
    println!("[RustTerm SSH] Connecting to {}:{}", host.hostname, host.port);

    let addr_str = format!("{}:{}", host.hostname, host.port);
    let socket_addrs: Vec<SocketAddr> = addr_str
        .to_socket_addrs()
        .map_err(|e| format!("Failed to resolve hostname {}: {}", host.hostname, e))?
        .collect();

    if socket_addrs.is_empty() {
        return Err(format!("Could not resolve host: {}", host.hostname));
    }

    let timeout = Duration::from_secs(10);
    let stream = TcpStream::connect_timeout(&socket_addrs[0], timeout)
        .map_err(|e| format!("TCP connection timeout to {}: {}", addr_str, e))?;

    stream.set_nodelay(true).ok();

    let mut sess = Session::new()
        .map_err(|e| format!("Failed to initialize SSH2 session: {}", e))?;

    sess.set_tcp_stream(stream);
    sess.set_blocking(true);
    sess.handshake()
        .map_err(|e| format!("SSH Handshake failed: {}", e))?;

    let interval_sec = host.keepalive_interval.unwrap_or(30);
    sess.set_keepalive(true, interval_sec);

    let username = &host.username;
    let authenticated: bool;

    match host.auth_mode.as_str() {
        "password" => {
            let pwd = host.password.as_deref().unwrap_or("");
            sess.userauth_password(username, pwd)
                .map_err(|e| format!("Password authentication failed for {}: {}", username, e))?;
            authenticated = sess.authenticated();
        }
        "private_key" => {
            let key_content = host.private_key.as_deref().unwrap_or("");
            if key_content.is_empty() {
                return Err("Private key content is empty".to_string());
            }
            let pass = host.passphrase.as_deref().filter(|s| !s.is_empty());
            auth_with_key(&sess, username, key_content, pass)?;
            authenticated = sess.authenticated();
        }
        "both" => {
            let key_content = host.private_key.as_deref().unwrap_or("");
            let pass = host.passphrase.as_deref().filter(|s| !s.is_empty());

            if !key_content.is_empty() {
                let _ = auth_with_key(&sess, username, key_content, pass);
            }

            if !sess.authenticated() {
                let pwd = host.password.as_deref().unwrap_or("");
                if !pwd.is_empty() {
                    sess.userauth_password(username, pwd)
                        .map_err(|e| format!("Secondary password auth failed: {}", e))?;
                }
            }
            authenticated = sess.authenticated();
        }
        "interactive" => {
            let pwd = host.password.as_deref().unwrap_or("").to_string();
            let mut handler = InteractiveAuth { password: pwd };
            sess.userauth_keyboard_interactive(username, &mut handler)
                .map_err(|e| format!("Keyboard interactive auth failed: {}", e))?;
            authenticated = sess.authenticated();
        }
        _ => {
            return Err(format!("Unknown authentication mode: {}", host.auth_mode));
        }
    }

    if !authenticated {
        return Err("Authentication unsuccessful. Please check credentials.".to_string());
    }

    println!("[RustTerm SSH] Authenticated user '{}' on {}:{}", username, host.hostname, host.port);
    Ok(sess)
}

impl SshManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn connect(
        &self,
        app: AppHandle,
        session_id: String,
        host: HostConfig,
        cols: Option<u32>,
        rows: Option<u32>,
    ) -> Result<(), String> {
        // Modern SSH Engine (russh): native pure-Rust support for Ed25519, OpenSSH format, RSA, ECDSA
        match self.connect_russh(app.clone(), session_id.clone(), host.clone(), cols, rows).await {
            Ok(()) => Ok(()),
            Err(e) => {
                println!("[RustTerm SSH] russh engine returned: {}, checking fallback...", e);
                // If it's private key auth and failed in russh, returning the descriptive error is best
                if host.auth_mode == "private_key" {
                    return Err(e);
                }
                // For password or other auth modes, attempt fallback to ssh2
                println!("[RustTerm SSH] Falling back to ssh2 for host: {}", host.hostname);
                self.connect_ssh2(app, session_id, host, cols, rows)
            }
        }
    }

    async fn connect_russh(
        &self,
        app: AppHandle,
        session_id: String,
        host: HostConfig,
        cols: Option<u32>,
        rows: Option<u32>,
    ) -> Result<(), String> {
        let session = establish_authenticated_russh_session(&host).await?;

        let initial_cols = cols.unwrap_or(120);
        let initial_rows = rows.unwrap_or(36);

        let mut channel = session
            .channel_open_session()
            .await
            .map_err(|e| format!("Failed to open session channel: {}", e))?;

        channel
            .request_pty(false, "xterm-256color", initial_cols, initial_rows, 0, 0, &[])
            .await
            .map_err(|e| format!("Failed to request PTY: {}", e))?;

        if host.persistent_session.unwrap_or(false) {
            let sname = host
                .session_name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("rustterm");
            let sname_clean: String = sname
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            let sname_safe = if sname_clean.is_empty() { "rustterm" } else { &sname_clean };

            let bootstrap = format!(
                "if command -v tmux >/dev/null 2>&1; then tmux set-option -g status off 2>/dev/null; tmux set-option -t \"{0}\" status off 2>/dev/null; tmux new-session -A -s \"{0}\" \\; set-option -g status off \\; set-option -t \"{0}\" status off; elif command -v screen >/dev/null 2>&1; then screen -xRR \"{0}\"; else exec ${{SHELL:-/bin/sh}} -l; fi",
                sname_safe
            );
            println!("[RustTerm SSH] Bootstrapping persistent shell session: {}", sname_safe);
            channel
                .exec(true, bootstrap.as_str())
                .await
                .map_err(|e| format!("Failed to spawn persistent session: {}", e))?;
        } else {
            channel
                .request_shell(true)
                .await
                .map_err(|e| format!("Failed to spawn shell: {}", e))?;
        }

        let (tx, mut rx) = unbounded_channel::<SshCommand>();
        let is_alive = Arc::new(AtomicBool::new(true));

        let worker_alive = Arc::clone(&is_alive);
        let worker_sess_id = session_id.clone();
        let app_handle = app.clone();

        tokio::spawn(async move {
            println!("[RustTerm SSH] russh event loop started for: {}", worker_sess_id);

            loop {
                tokio::select! {
                    cmd = rx.recv() => {
                        match cmd {
                            Some(SshCommand::Data(bytes)) => {
                                if let Err(e) = channel.data(&bytes[..]).await {
                                    eprintln!("[RustTerm SSH] russh channel write error: {}", e);
                                    break;
                                }
                            }
                            Some(SshCommand::Resize(cols, rows)) => {
                                let _ = channel.window_change(cols, rows, 0, 0).await;
                            }
                            Some(SshCommand::Keepalive) => {
                                // russh handles keepalives automatically via config
                            }
                            Some(SshCommand::Disconnect) | None => {
                                let _ = channel.eof().await;
                                break;
                            }
                        }
                    }
                    msg = channel.wait() => {
                        match msg {
                            Some(russh::ChannelMsg::Data { data }) => {
                                let text = String::from_utf8_lossy(&data).to_string();
                                let _ = app_handle.emit(&format!("ssh-output-{}", worker_sess_id), text);
                            }
                            Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                                let text = String::from_utf8_lossy(&data).to_string();
                                let _ = app_handle.emit(&format!("ssh-output-{}", worker_sess_id), text);
                            }
                            Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => {
                                let _ = app_handle.emit(
                                    &format!("ssh-output-{}", worker_sess_id),
                                    "\r\n[Connection closed by remote host]\r\n",
                                );
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }

            worker_alive.store(false, Ordering::Relaxed);
            let _ = channel.eof().await;
            let _ = session.disconnect(russh::Disconnect::ByApplication, "", "").await;
            println!("[RustTerm SSH] russh event loop finished for: {}", worker_sess_id);
        });

        let mut lock = self.sessions.lock();
        lock.insert(session_id, ActiveSession { tx, is_alive });
        Ok(())
    }

    fn connect_ssh2(
        &self,
        app: AppHandle,
        session_id: String,
        host: HostConfig,
        cols: Option<u32>,
        rows: Option<u32>,
    ) -> Result<(), String> {
        let sess = establish_authenticated_session(&host)?;

        let initial_cols = cols.unwrap_or(120);
        let initial_rows = rows.unwrap_or(36);

        let mut channel = sess
            .channel_session()
            .map_err(|e| format!("Failed to open SSH session channel: {}", e))?;

        channel
            .request_pty("xterm-256color", None, Some((initial_cols, initial_rows, 0, 0)))
            .map_err(|e| format!("Failed to request PTY: {}", e))?;

        if host.persistent_session.unwrap_or(false) {
            let sname = host
                .session_name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("rustterm");
            let sname_clean: String = sname
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            let sname_safe = if sname_clean.is_empty() { "rustterm" } else { &sname_clean };

            let bootstrap = format!(
                "if command -v tmux >/dev/null 2>&1; then tmux set-option -g status off 2>/dev/null; tmux set-option -t \"{0}\" status off 2>/dev/null; tmux new-session -A -s \"{0}\" \\; set-option -g status off \\; set-option -t \"{0}\" status off; elif command -v screen >/dev/null 2>&1; then screen -xRR \"{0}\"; else exec ${{SHELL:-/bin/sh}} -l; fi",
                sname_safe
            );
            println!("[RustTerm SSH] Bootstrapping persistent shell session (status bar off): {}", sname_safe);
            channel
                .exec(&bootstrap)
                .map_err(|e| format!("Failed to spawn persistent session: {}", e))?;
        } else {
            channel
                .shell()
                .map_err(|e| format!("Failed to spawn shell: {}", e))?;
        }

        sess.set_blocking(false);

        let (tx, mut rx) = unbounded_channel::<SshCommand>();
        let is_alive = Arc::new(AtomicBool::new(true));

        let worker_alive = Arc::clone(&is_alive);
        let worker_sess_id = session_id.clone();
        let app_handle = app.clone();

        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            println!("[RustTerm SSH] [ssh2] Worker loop active for session: {}", worker_sess_id);

            while worker_alive.load(Ordering::Relaxed) {
                let mut did_work = false;

                while let Ok(cmd) = rx.try_recv() {
                    did_work = true;
                    match cmd {
                        SshCommand::Data(bytes) => {
                            let mut offset = 0;
                            while offset < bytes.len() {
                                match channel.write(&bytes[offset..]) {
                                    Ok(n) => {
                                        offset += n;
                                    }
                                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                        std::thread::sleep(Duration::from_millis(1));
                                    }
                                    Err(e) => {
                                        eprintln!("[RustTerm SSH] Write error: {}", e);
                                        break;
                                    }
                                }
                            }
                            let _ = channel.flush();
                        }
                        SshCommand::Resize(cols, rows) => {
                            let _ = channel.request_pty_size(cols, rows, None, None);
                        }
                        SshCommand::Keepalive => {
                            let _ = sess.keepalive_send();
                        }
                        SshCommand::Disconnect => {
                            worker_alive.store(false, Ordering::Relaxed);
                            break;
                        }
                    }
                }

                if !worker_alive.load(Ordering::Relaxed) {
                    break;
                }

                match channel.read(&mut buf) {
                    Ok(0) => {
                        println!("[RustTerm SSH] Remote host closed session: {}", worker_sess_id);
                        let _ = app_handle.emit(&format!("ssh-output-{}", worker_sess_id), "\r\n[Connection closed by remote host]\r\n");
                        worker_alive.store(false, Ordering::Relaxed);
                        break;
                    }
                    Ok(n) => {
                        did_work = true;
                        let text = String::from_utf8_lossy(&buf[..n]).to_string();
                        let _ = app_handle.emit(&format!("ssh-output-{}", worker_sess_id), text);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        eprintln!("[RustTerm SSH] Read error: {}", e);
                        let _ = app_handle.emit(&format!("ssh-output-{}", worker_sess_id), format!("\r\n[Session error: {}]\r\n", e));
                        worker_alive.store(false, Ordering::Relaxed);
                        break;
                    }
                }

                if !did_work {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }

            let _ = channel.send_eof();
            let _ = channel.close();
            let _ = sess.disconnect(None, "Session closed", None);
            println!("[RustTerm SSH] [ssh2] Clean session exit: {}", worker_sess_id);
        });

        let mut lock = self.sessions.lock();
        lock.insert(
            session_id,
            ActiveSession {
                tx,
                is_alive,
            },
        );

        Ok(())
    }

    pub fn send_data(&self, session_id: &str, data: &str) -> Result<(), String> {
        let lock = self.sessions.lock();
        if let Some(session) = lock.get(session_id) {
            session
                .tx
                .send(SshCommand::Data(data.as_bytes().to_vec()))
                .map_err(|e| format!("Failed to queue SSH data: {}", e))?;
            Ok(())
        } else {
            Err("Session not found".to_string())
        }
    }

    pub fn send_keepalive(&self, session_id: &str) -> Result<u32, String> {
        let lock = self.sessions.lock();
        if let Some(session) = lock.get(session_id) {
            session
                .tx
                .send(SshCommand::Keepalive)
                .map_err(|e| format!("Failed to queue keepalive: {}", e))?;
            Ok(30)
        } else {
            Err("Session not found".to_string())
        }
    }

    pub fn resize(&self, session_id: &str, cols: u32, rows: u32) -> Result<(), String> {
        let lock = self.sessions.lock();
        if let Some(session) = lock.get(session_id) {
            session
                .tx
                .send(SshCommand::Resize(cols, rows))
                .map_err(|e| format!("Failed to queue resize: {}", e))?;
            Ok(())
        } else {
            Err("Session not found".to_string())
        }
    }

    pub fn disconnect(&self, session_id: &str) -> Result<(), String> {
        let mut lock = self.sessions.lock();
        if let Some(session) = lock.remove(session_id) {
            session.is_alive.store(false, Ordering::Relaxed);
            let _ = session.tx.send(SshCommand::Disconnect);
        }
        Ok(())
    }
}

// =========================================================
// Storage & Export / Import Functions
// =========================================================
fn get_storage_path() -> PathBuf {
    let mut dir = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("rustterm");
    std::fs::create_dir_all(&dir).ok();
    dir.push("hosts.json");
    dir
}

pub fn load_hosts() -> Vec<HostConfig> {
    let path = get_storage_path();
    if let Ok(data) = std::fs::read_to_string(&path) {
        if let Ok(hosts) = serde_json::from_str::<Vec<HostConfig>>(&data) {
            return hosts;
        }
    }
    Vec::new()
}

pub fn save_hosts_to_disk(hosts: &[HostConfig]) -> Result<(), String> {
    let path = get_storage_path();
    let json_data = serde_json::to_string_pretty(hosts)
        .map_err(|e| format!("JSON serialization error: {}", e))?;
    std::fs::write(&path, json_data)
        .map_err(|e| format!("Failed to write hosts to {}: {}", path.display(), e))?;
    Ok(())
}

pub fn export_hosts_json(include_keys: bool) -> Result<String, String> {
    let hosts = load_hosts();
    let sanitized_hosts: Vec<HostConfig> = hosts
        .into_iter()
        .map(|mut h| {
            if !include_keys {
                h.password = None;
                h.private_key = None;
                h.passphrase = None;
            }
            h
        })
        .collect();

    let payload = ExportPayload {
        app: "RustTerm".to_string(),
        version: "1.0.0".to_string(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        security_notice: if include_keys {
            "CONTAINS RAW PRIVATE KEYS & PASSWORDS. STORE SECURELY.".to_string()
        } else {
            "Credentials and private keys have been stripped.".to_string()
        },
        hosts: sanitized_hosts,
    };

    serde_json::to_string_pretty(&payload)
        .map_err(|e| format!("Failed to export JSON: {}", e))
}

pub fn import_hosts_json(json_str: &str) -> Result<usize, String> {
    let mut imported_hosts: Vec<HostConfig> = if let Ok(payload) = serde_json::from_str::<ExportPayload>(json_str) {
        payload.hosts
    } else if let Ok(list) = serde_json::from_str::<Vec<HostConfig>>(json_str) {
        list
    } else {
        return Err("Invalid JSON structure for host import".to_string());
    };

    let mut current_hosts = load_hosts();
    let mut count = 0;

    for host in imported_hosts.drain(..) {
        if let Some(pos) = current_hosts.iter().position(|h| h.id == host.id) {
            current_hosts[pos] = host;
        } else {
            current_hosts.push(host);
        }
        count += 1;
    }

    save_hosts_to_disk(&current_hosts)?;
    Ok(count)
}
