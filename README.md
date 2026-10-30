# RustTerm

RustTerm is a desktop SSH and SFTP client built with **Tauri 2.0 (Rust)** and **xterm.js**.

## Features

- **SSH Terminal**: Multi-tab SSH terminal with font zoom, keepalive support, and ANSI color emulation.
- **Copy & Paste**:
  - `Ctrl+Shift+C` to copy selected text.
  - `Ctrl+V` to paste with automatic input sanitization (strips bracketed paste escape sequences, invisible Unicode characters, and prompt symbols).
  - Right-click context menu (Copy, Paste, Select All, Clear).
- **Session Auto-Resume**: Optional persistent remote shell sessions using `tmux` or `screen`.
- **Integrated SFTP**: File manager to browse, upload, download individual files, or transfer entire folders.
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
