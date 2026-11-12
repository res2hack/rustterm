# RustTerm

RustTerm is a desktop SSH and SFTP client built with **Tauri 2.0 (Rust)** and **xterm.js**.

## Features

- **SSH Terminal**: Multi-tab SSH terminal with font zoom, keepalive support, and ANSI color emulation.
- **Copy & Paste**:
  - `Ctrl+Shift+C` to copy selected text.
  - `Ctrl+V` to paste with automatic input sanitization (strips bracketed paste escape sequences, invisible Unicode characters, and prompt symbols).
  - Right-click context menu (Copy, Paste, Select All, Clear).
- **Session Auto-Resume**: Optional persistent remote shell sessions using `tmux` or `screen`.
- **Integrated Dual-Pane SFTP**:
  - Side-by-side **Local Computer** and **Target Server** panes with multi-select, rename, recursive delete, and overwrite/skip prompts.
  - Drag and drop files or folders from Windows Explorer or directly between Local and Target panes.
  - **Edit with Auto-Upload**: Open remote files in your default local editor and automatically re-upload changes on save.
  - **Sudo Mode**: Toggle `sudo` elevation in the SFTP header to browse, transfer, and edit root-owned files (supports `NOPASSWD` and passworded `sudo`).
  - **Fast Stream & Resume**: Download large folders using concurrent SFTP workers or `tar` streaming over SSH with exclude filters (e.g., `node_modules`, `.next`, `.git`), live transfer speed/progress, cancel support, and 1-click **Resume** on interrupted downloads.
- **Host Vault**: Manage server credentials locally with support for passwords, private keys (PEM/OpenSSH), and passphrases.
- **Command History**: Per-host command history sidebar with search and favorites.
- **Import & Export**: Backup and restore hosts locally using JSON format.

## Getting Started

### Prerequisites

- [Node.js](https://nodejs.org/) (v18+)
- [Rust](https://www.rust-lang.org/) (v1.75+)
- C compiler for `libssh2` (MinGW-w64 on Windows, or MSVC)

### Development

```bash
# Install dependencies
npm install

# Run frontend in browser preview
npm run dev

# Run desktop application
npm run tauri dev
```

### Build Executable

```bash
# Build frontend
npm run build

# Build release binary
cd src-tauri
cargo build --release
```

The compiled binary will be located in `src-tauri/target/release/tauri-app.exe`.

## License

This project is licensed under the [MIT License](LICENSE).
