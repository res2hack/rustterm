# RustTerm — High-Stability SSH Client

**RustTerm** is a high-performance, modern SSH client built with **Rust**, **Tauri 2.0**, and **xterm.js**. Engineered specifically for mission-critical server management, RustTerm delivers rock-solid connection stability that prevents premature timeouts and drops, full multi-authentication support (combined Password + Private Key), remote persistent session recovery, integrated SFTP transfers, and open local-first credential management without vendor cloud lock-in.

---

## ⚡ Key Highlights & Features

- **🛡️ Ultra-Stable Connection (Anti-Disconnect)**:
  - Built on native **Rust + `libssh2`** (the same high-performance SSH engine powering enterprise clients).
  - Configurable **Heartbeat Keepalive loop** (default 30s, adjustable from 5s to 300s) with live heartbeat pulse indicator.
  - Keeps idle sessions alive through aggressive NAT routers, firewalls, and cloud security groups.
  - Intelligent **Auto-Reconnect** handles temporary network packet drops seamlessly.

- **⚡ Persistent Session Auto-Resume (Uninterrupted Tasks)**:
  - If your network disconnects during long-running commands (e.g., `npm run dev`, compilation, `docker build`, `top`, `vim`, or file downloads), your server processes **will not terminate** (immune to `SIGHUP`).
  - Powered by an automatic remote `tmux` / `screen` bootstrap architecture.
  - Upon reconnection (automatic or manual), RustTerm re-attaches to the exact same terminal screen so you can resume immediately.

- **📜 Per-Vault Command History Sidebar**:
  - Right-hand collapsible sidebar displaying commands executed in the session.
  - **Vault Isolation**: History is strictly partitioned per host; switching tabs changes history to match the active host.
  - **Safe Command Insertion**: Clicking a command inserts it directly into the terminal without auto-executing, allowing you to review or modify before pressing <kbd>Enter</kbd>.
  - **Favorites & Search**: Star (⭐) frequent commands, filter by favorites, search query filter, and instant copy-to-clipboard button.
  - **Collapsible**: Smooth width toggle via the header button, the toolbar `History` button, or <kbd>Ctrl</kbd>+<kbd>H</kbd>.

- **📁 Integrated SFTP File & Folder Manager**:
  - Integrated high-speed SFTP subsystem directly accessible from the session toolbar or vault card.
  - **Single File Download**: Download remote files to your local machine with 1 click.
  - **Recursive Directory Transfer**: Download entire nested directory trees with live file count and transferred byte metrics.
  - **Upload Files**: Transfer local files directly to any remote directory.
  - **Create & Delete Items**: Create remote directories or delete remote files/folders.
  - **1-Click Open in File Explorer**: Open the local transfer destination folder (`Downloads/RustTerm_Transfers`) instantly in Windows Explorer.

- **🔑 Comprehensive Multi-Auth Engine**:
  - **Password Authentication**: Standard secure password authentication.
  - **Private Key Authentication**: Supports OpenSSH, PEM, Ed25519, RSA, ECDSA formats with optional passphrase decryption.
  - **Combined Password + Private Key**: Full support for fortified servers and bastion hosts requiring both a public key and a password simultaneously.
  - **Keyboard Interactive**: Dynamic challenge-response prompt support.

- **📦 100% Open Backup, Export & Import**:
  - Export your entire host vault along with private keys in JSON format.
  - Copy backup JSON directly to the clipboard or save to a file.
  - Restore hosts and keys instantly on any machine with zero cloud dependency.

- **🗂️ Fullscreen Vault & Batch Multi-Select**:
  - Dual viewing modes: **Card View** (responsive visual cards) and **Table View** (multi-column spreadsheet view).
  - **Connect New Session** (starts an independent new session) vs. **Connect Resume Session** (re-attaches to existing persistent server sessions).
  - **Bulk Actions**: Select multiple hosts simultaneously to delete in bulk or toggle favorites with the floating action bar.

- **🪟 Pure Native Desktop Subsystem**:
  - Compiled with `IMAGE_SUBSYSTEM_WINDOWS_GUI` (`subsystem = 2`).
  - Zero command prompt or console window popups when launching or running the client.

- **🖥️ Termius Cyber Dark Aesthetic**:
  - Multi-tab multitasking terminal support.
  - Full VT100/ANSI/Unicode/24-bit TrueColor emulation via **xterm.js**.
  - Font zoom controls (`- 14px +`), buffer clearing, real-time connection status pills, and quick connect modal.

---

## 🛠️ Project Structure

```
termius/
├── src/                          # Modern Frontend UI (HTML5, Vanilla CSS, ES Modules)
│   ├── index.html                # Application layout, modals, vault view, history sidebar
│   ├── styles.css                # Cyber Dark design system, glassmorphism, responsive grid
│   └── main.js                   # Terminal manager, xterm.js controller, IPC layer
├── src-tauri/                    # Rust Backend & Tauri 2.0 Core
│   ├── Cargo.toml                # Dependencies: ssh2, tokio, parking_lot, dirs, serde
│   ├── tauri.conf.json           # Tauri desktop app configuration
│   └── src/
│       ├── lib.rs                # IPC Command Handlers between WebView and Rust backend
│       ├── main.rs               # Native Windows desktop application entry point
│       ├── ssh.rs                # Native SSH2 engine, keepalive loop & persistent shell
│       └── sftp.rs               # SFTP subsystem: transfers, recursive directory download
├── package.json                  # Node.js dependencies & scripts
└── vite.config.js                # Vite development server and bundling configuration
```

---

## 🚀 Getting Started & Building

### Prerequisites

Ensure the following tools are installed on your workstation:
1. **Node.js** (v18.0 or later) & `npm`
2. **Rust** (v1.75 or later) via `rustup`
3. **C Compiler** for `libssh2`:
   - Windows: MinGW-w64 (GCC) or Microsoft C++ Build Tools (MSVC)
   - Linux: `gcc`, `pkg-config`, `libssl-dev`
   - macOS: Xcode Command Line Tools

---

### 1. Development Mode (Browser Preview)

To run the frontend preview in the browser:

```bash
npm install
npm run dev
```

Open your browser at **http://localhost:1420/**. (Mock handlers will simulate SSH/SFTP calls in standard browser preview).

---

### 2. Desktop Development Mode (Tauri Native App)

To run the live desktop application with the native Rust backend connected:

```bash
npm run tauri dev
```

*Note for Windows MinGW users: Ensure your MinGW `bin` directory and Cargo `bin` directory are included in your `PATH` before running.*

---

### 3. Building Production Binary (`.exe`)

To compile the optimized, standalone desktop executable:

```bash
npm run build
cd src-tauri
cargo build --release
```

The resulting binary will be generated under `src-tauri/target/release/`.

---

## 🔒 Security & Privacy

- **Local Storage Only**: All credentials, private keys, host addresses, and command history remain on your local device.
- **Zero Telemetry**: RustTerm does not track usage, collect telemetry, or transmit data to external servers.
- **Direct P2P SSH**: SSH and SFTP connections are initiated directly from your workstation to your remote servers.

---

## 📄 License

This project is open-source and available under the [MIT License](LICENSE).
