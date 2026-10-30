// Default Vault Hosts template for RustTerm
// Add your own servers or import from Termius / WinSCP JSON backup

export const DEFAULT_HOSTS = [
  {
    "id": "host-demo-server-1",
    "label": "Demo Ubuntu Server",
    "group": "Examples",
    "hostname": "192.168.1.100",
    "port": 22,
    "username": "ubuntu",
    "auth_mode": "password",
    "password": "",
    "private_key": "",
    "passphrase": "",
    "keepalive_interval": 30,
    "keepalive_max": 3,
    "auto_reconnect": true,
    "persistent_session": true,
    "session_name": "rustterm",
    "tags": [
      "Example",
      "Staging"
    ],
    "is_favorite": true
  }
];
