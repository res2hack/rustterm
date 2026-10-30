// =========================================================
// RustTerm - Frontend Controller
// Built for high stability, multi-auth, xterm.js integration,
// and complete host/key export & import.
// =========================================================

import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import '@xterm/xterm/css/xterm.css';
import { DEFAULT_HOSTS } from './default_hosts.js';

// Check if running inside Tauri
const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
let tauriCore = null;

if (isTauri) {
  try {
    tauriCore = window.__TAURI__.core;
  } catch (e) {
    console.log('Tauri core API not available on window, falling back to direct invoke');
  }
}

async function invokeTauri(cmd, args = {}) {
  if (isTauri && window.__TAURI__ && window.__TAURI__.core) {
    try {
      return await window.__TAURI__.core.invoke(cmd, args);
    } catch (err) {
      console.warn(`Tauri invoke '${cmd}' failed:`, err);
      throw err;
    }
  }
  // Fallback for browser preview / mock mode
  console.log(`[Mock Tauri Call] ${cmd}`, args);
  return mockTauriHandler(cmd, args);
}

// =========================================================
// State
// =========================================================
let hosts = [];
let activeTag = 'all';
let searchQuery = '';
let activeSessionId = 'home';
const sessions = new Map(); // id -> { id, host, terminal, fitAddon, element, status, keepaliveTimer, heartbeatPulse }
let currentFontSize = 14;

// Vault View States & Multi-Select
let vaultViewMode = localStorage.getItem('rustterm_vault_view_mode') || 'card';
let vaultSearchQuery = '';
let vaultActiveTag = 'all';
let sidebarCollapsed = localStorage.getItem('rustterm_sidebar_collapsed') === 'true';
const selectedHostIds = new Set(); // Multi-selection for bulk actions

// Per-Vault Command History System State
let isHistorySidebarOpen = true; // Always default open for active SSH sessions
let cmdHistoryFilter = 'all'; // 'all' | 'favorites'
let cmdHistorySearchQuery = '';

// Helper: Host Session History (for Connect Resume Session)
function getHostSessionHistory(hostId) {
  try {
    const raw = localStorage.getItem('rustterm_session_history');
    if (!raw) return null;
    const map = JSON.parse(raw);
    return map[hostId] || null;
  } catch (e) {
    return null;
  }
}

function recordHostSession(hostId, sessionName) {
  try {
    const raw = localStorage.getItem('rustterm_session_history');
    const map = raw ? JSON.parse(raw) : {};
    map[hostId] = {
      hasOldSession: true,
      sessionName: sessionName || 'rustterm',
      lastConnected: Date.now()
    };
    localStorage.setItem('rustterm_session_history', JSON.stringify(map));
  } catch (e) { }
}

function hasOldSession(host) {
  // 1. Check if an active session tab is currently running in RustTerm
  const isOpen = Array.from(sessions.values()).some(s => s.host.id === host.id);
  if (isOpen) return true;

  // 2. Check if persistent session is enabled AND was connected before
  if (host.persistent_session !== false) {
    const hist = getHostSessionHistory(host.id);
    if (hist && hist.hasOldSession) {
      return true;
    }
  }

  return false;
}

// Vault Hosts (Imported from Termius & backup credentials)
// DEFAULT_HOSTS is imported from ./default_hosts.js

// =========================================================
// Helper to check favorite status consistently
function isFavoriteHost(h) {
  return Boolean(h && (h.is_favorite === true || h.is_favorite === 'true' || h.is_favorite === 1));
}

// =========================================================
// Initialization
// =========================================================
document.addEventListener('DOMContentLoaded', async () => {
  loadStoredHosts();
  updateHostCount();
  if (sidebarCollapsed) {
    const app = document.getElementById('app');
    if (app) app.classList.add('sidebar-collapsed');
  }
  setupEventListeners();
  renderTagFilters();
  renderHostList();
  renderVaultTagFilters();
  renderVaultStats();
  renderVaultHosts();
  updateAuthModeUI('password');
  setVaultViewMode(vaultViewMode);
});

function loadStoredHosts() {
  try {
    const raw = localStorage.getItem('rustterm_hosts');
    if (raw) {
      hosts = JSON.parse(raw);
      // Auto-migrate if stored hosts are only the old 2 demo hosts or empty
      const isOnlyDemo = hosts.length <= 2 && hosts.every(h => h.id && h.id.startsWith('host-demo-'));
      if (isOnlyDemo || hosts.length === 0) {
        hosts = [...DEFAULT_HOSTS];
        saveHostsToStorage();
      } else {
        // Ensure favorite boolean is configured; if none is favorite, set the first host as favorite
        const hasAnyFav = hosts.some(isFavoriteHost);
        if (!hasAnyFav && hosts.length > 0) {
          hosts[0].is_favorite = true;
          localStorage.setItem('rustterm_hosts', JSON.stringify(hosts));
        }
      }
    } else {
      hosts = [...DEFAULT_HOSTS];
      saveHostsToStorage();
    }
  } catch (e) {
    hosts = [...DEFAULT_HOSTS];
  }
  updateHostCount();
}

function saveHostsToStorage() {
  localStorage.setItem('rustterm_hosts', JSON.stringify(hosts));
  updateHostCount();
  renderVaultStats();
  if (isTauri) {
    invokeTauri('save_hosts', { hosts }).catch(() => { });
  }
}

function updateHostCount() {
  const el = document.getElementById('host-count');
  const favCount = hosts.filter(isFavoriteHost).length;
  if (el) el.textContent = favCount;
}

// =========================================================
// Event Listeners
// =========================================================
function setupEventListeners() {
  // Home / Vault Tab
  const homeTab = document.getElementById('tab-home');
  if (homeTab) {
    homeTab.addEventListener('click', () => switchToHomeTab());
  }

  // Sidebar Collapse / Expand Controls
  const btnToggleSidebar = document.getElementById('btn-toggle-sidebar');
  if (btnToggleSidebar) {
    btnToggleSidebar.addEventListener('click', () => toggleSidebar());
  }
  const btnExpandSidebar = document.getElementById('btn-expand-sidebar');
  if (btnExpandSidebar) {
    btnExpandSidebar.addEventListener('click', () => toggleSidebar(false));
  }
  // Keyboard Shortcut: Ctrl+B / Cmd+B for Sidebar Toggle
  window.addEventListener('keydown', (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'b') {
      e.preventDefault();
      toggleSidebar();
    }
  });

  // Add Host Modal
  const btnAddHost = document.getElementById('btn-add-host');
  if (btnAddHost) {
    btnAddHost.addEventListener('click', () => openHostModal());
  }
  const btnVaultNewHost = document.getElementById('btn-vault-new-host');
  if (btnVaultNewHost) {
    btnVaultNewHost.addEventListener('click', () => openHostModal());
  }
  const btnVaultBackup = document.getElementById('btn-vault-backup');
  if (btnVaultBackup) {
    btnVaultBackup.addEventListener('click', () => openExportModal());
  }
  const btnVaultQuickConnect = document.getElementById('btn-vault-quick-connect');
  if (btnVaultQuickConnect) {
    btnVaultQuickConnect.addEventListener('click', () => openQuickConnectModal());
  }

  // Vault Batch Selection Actions
  const btnBatchFav = document.getElementById('btn-batch-favorite');
  if (btnBatchFav) btnBatchFav.addEventListener('click', () => handleBatchFavorite(true));
  const btnBatchUnfav = document.getElementById('btn-batch-unfavorite');
  if (btnBatchUnfav) btnBatchUnfav.addEventListener('click', () => handleBatchFavorite(false));
  const btnBatchDel = document.getElementById('btn-batch-delete');
  if (btnBatchDel) btnBatchDel.addEventListener('click', () => handleBatchDelete());
  const btnBatchClear = document.getElementById('btn-batch-clear');
  if (btnBatchClear) btnBatchClear.addEventListener('click', () => handleBatchClear());

  // View Switcher (Card vs Table)
  const btnViewCard = document.getElementById('btn-view-card');
  if (btnViewCard) {
    btnViewCard.addEventListener('click', () => setVaultViewMode('card'));
  }
  const btnViewTable = document.getElementById('btn-view-table');
  if (btnViewTable) {
    btnViewTable.addEventListener('click', () => setVaultViewMode('table'));
  }

  // Vault Search
  const vaultSearchInput = document.getElementById('vault-search-input');
  if (vaultSearchInput) {
    vaultSearchInput.addEventListener('input', (e) => {
      vaultSearchQuery = e.target.value.toLowerCase().trim();
      renderVaultHosts();
    });
  }

  // Global / shortcut for vault search
  window.addEventListener('keydown', (e) => {
    if (e.key === '/' && activeSessionId === 'home' && document.activeElement !== vaultSearchInput) {
      e.preventDefault();
      if (vaultSearchInput) vaultSearchInput.focus();
    }
  });

  document.getElementById('btn-close-host-modal').addEventListener('click', closeHostModal);
  document.getElementById('btn-cancel-host').addEventListener('click', closeHostModal);
  document.getElementById('host-form').addEventListener('submit', handleSaveHost);

  // Search & Filter (Sidebar)
  document.getElementById('host-search').addEventListener('input', (e) => {
    searchQuery = e.target.value.toLowerCase().trim();
    renderHostList();
  });

  // Auth Mode Radios
  document.querySelectorAll('input[name="auth_mode"]').forEach(radio => {
    radio.addEventListener('change', (e) => {
      updateAuthModeUI(e.target.value);
    });
  });

  // Toggle Password Visibility
  document.getElementById('btn-toggle-pwd').addEventListener('click', () => {
    const input = document.getElementById('form-password');
    input.type = input.type === 'password' ? 'text' : 'password';
  });

  // Key File Browser
  const filePicker = document.getElementById('file-key-picker');
  document.getElementById('btn-browse-key').addEventListener('click', () => filePicker.click());
  filePicker.addEventListener('change', (e) => {
    const file = e.target.files[0];
    if (file) {
      const reader = new FileReader();
      reader.onload = (event) => {
        document.getElementById('form-private-key').value = event.target.result;
        showToast('Private key imported from file', 'success');
      };
      reader.readAsText(file);
    }
  });

  // Quick Connect Modal Handlers
  const btnCloseQuick = document.getElementById('btn-close-quick-modal');
  if (btnCloseQuick) btnCloseQuick.addEventListener('click', closeQuickConnectModal);
  const btnCancelQuick = document.getElementById('btn-cancel-quick');
  if (btnCancelQuick) btnCancelQuick.addEventListener('click', closeQuickConnectModal);

  const quickForm = document.getElementById('quick-connect-form');
  if (quickForm) quickForm.addEventListener('submit', handleQuickConnectSubmit);

  const quickTargetInput = document.getElementById('quick-ssh-input');
  if (quickTargetInput) {
    quickTargetInput.addEventListener('input', handleQuickTargetInput);
  }

  const btnToggleQuickPwd = document.getElementById('btn-toggle-quick-pwd');
  if (btnToggleQuickPwd) {
    btnToggleQuickPwd.addEventListener('click', () => {
      const pwd = document.getElementById('quick-password');
      if (pwd) pwd.type = pwd.type === 'password' ? 'text' : 'password';
    });
  }

  const modalQuick = document.getElementById('modal-quick-connect');
  if (modalQuick) {
    modalQuick.addEventListener('click', (e) => {
      if (e.target === modalQuick) closeQuickConnectModal();
    });
  }

  // Backup / Export / Import Modal
  document.getElementById('btn-export-import').addEventListener('click', openExportModal);
  document.getElementById('btn-close-export-modal').addEventListener('click', closeExportModal);

  // Export Tabs
  document.querySelectorAll('.export-tab').forEach(tab => {
    tab.addEventListener('click', (e) => {
      document.querySelectorAll('.export-tab').forEach(t => t.classList.remove('active'));
      document.querySelectorAll('.tab-pane').forEach(p => p.classList.remove('active'));
      e.target.classList.add('active');
      const targetPane = document.getElementById(e.target.dataset.tab);
      if (targetPane) targetPane.classList.add('active');
    });
  });

  document.getElementById('btn-generate-export').addEventListener('click', generateExportJSON);
  document.getElementById('btn-copy-export').addEventListener('click', copyExportJSON);
  document.getElementById('btn-download-export').addEventListener('click', downloadExportJSON);
  document.getElementById('btn-open-export-folder').addEventListener('click', () => {
    if (lastSavedBackupPath) {
      openExplorerToFolder(lastSavedBackupPath);
    }
  });


  // Import
  const importFileInput = document.getElementById('import-file-input');
  document.getElementById('btn-select-import-file').addEventListener('click', () => importFileInput.click());
  importFileInput.addEventListener('change', handleImportFileSelect);
  document.getElementById('btn-execute-import').addEventListener('click', handleExecuteImport);

  // Settings Modal
  document.getElementById('btn-open-settings').addEventListener('click', () => {
    document.getElementById('modal-settings').classList.remove('hidden');
  });
  document.getElementById('btn-close-settings-modal').addEventListener('click', () => {
    document.getElementById('modal-settings').classList.add('hidden');
  });
  document.getElementById('btn-save-settings').addEventListener('click', () => {
    document.getElementById('modal-settings').classList.add('hidden');
    showToast('Preferences saved', 'success');
  });

  // Terminal Toolbar Actions
  document.getElementById('tb-btn-reconnect').addEventListener('click', () => {
    if (activeSessionId) reconnectSession(activeSessionId);
  });
  document.getElementById('tb-btn-clear').addEventListener('click', () => {
    const s = sessions.get(activeSessionId);
    if (s && s.terminal) s.terminal.clear();
  });
  document.getElementById('tb-btn-disconnect').addEventListener('click', () => {
    if (activeSessionId) closeSessionTab(activeSessionId);
  });
  // Terminal Toolbar SFTP Button
  document.getElementById('tb-btn-sftp').addEventListener('click', () => {
    if (activeSessionId) {
      const s = sessions.get(activeSessionId);
      if (s && s.host) openSftpModal(s.host);
    }
  });

  // SFTP Modal Controls
  document.getElementById('btn-close-sftp-modal').addEventListener('click', closeSftpModal);
  document.getElementById('btn-close-sftp-bottom').addEventListener('click', closeSftpModal);
  document.getElementById('sftp-btn-up').addEventListener('click', () => {
    if (currentSftpParentPath) sftpLoadDirectory(currentSftpParentPath);
  });
  document.getElementById('sftp-btn-refresh').addEventListener('click', () => {
    sftpLoadDirectory(currentSftpPath);
  });
  document.getElementById('sftp-btn-go').addEventListener('click', () => {
    const p = document.getElementById('sftp-path-input').value.trim();
    if (p) sftpLoadDirectory(p);
  });
  document.getElementById('sftp-path-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      const p = e.target.value.trim();
      if (p) sftpLoadDirectory(p);
    }
  });
  document.getElementById('sftp-btn-new-folder').addEventListener('click', sftpCreateFolder);
  const uploader = document.getElementById('sftp-file-uploader');
  document.getElementById('sftp-btn-upload').addEventListener('click', () => uploader.click());
  uploader.addEventListener('change', handleSftpUploadSelect);
  document.getElementById('sftp-btn-open-explorer').addEventListener('click', () => {
    if (defaultLocalDownloadDir) openExplorerToFolder(defaultLocalDownloadDir);
  });
  document.getElementById('sftp-btn-reveal-file').addEventListener('click', () => {
    if (lastDownloadedLocalPath) openExplorerToFolder(lastDownloadedLocalPath);
  });

  // Command History Sidebar Controls
  const btnToggleHist = document.getElementById('tb-btn-toggle-history');
  if (btnToggleHist) btnToggleHist.addEventListener('click', () => toggleCommandHistorySidebar());

  const btnCloseHist = document.getElementById('btn-close-cmd-history');
  if (btnCloseHist) btnCloseHist.addEventListener('click', () => toggleCommandHistorySidebar(false));

  const btnClearHist = document.getElementById('btn-clear-vault-history');
  if (btnClearHist) btnClearHist.addEventListener('click', handleClearVaultHistoryClick);

  const searchHistInput = document.getElementById('cmd-hist-search');
  if (searchHistInput) {
    searchHistInput.addEventListener('input', (e) => {
      cmdHistorySearchQuery = e.target.value.trim().toLowerCase();
      const clearBtn = document.getElementById('cmd-hist-search-clear');
      if (clearBtn) clearBtn.classList.toggle('hidden', !cmdHistorySearchQuery);
      renderCommandHistorySidebar();
    });
  }

  const searchHistClear = document.getElementById('cmd-hist-search-clear');
  if (searchHistClear) {
    searchHistClear.addEventListener('click', () => {
      if (searchHistInput) {
        searchHistInput.value = '';
        cmdHistorySearchQuery = '';
        searchHistClear.classList.add('hidden');
        renderCommandHistorySidebar();
        searchHistInput.focus();
      }
    });
  }

  const tabHistAll = document.getElementById('cmd-tab-all');
  const tabHistFavs = document.getElementById('cmd-tab-favs');
  if (tabHistAll) {
    tabHistAll.addEventListener('click', () => {
      cmdHistoryFilter = 'all';
      tabHistAll.classList.add('active');
      if (tabHistFavs) tabHistFavs.classList.remove('active');
      renderCommandHistorySidebar();
    });
  }
  if (tabHistFavs) {
    tabHistFavs.addEventListener('click', () => {
      cmdHistoryFilter = 'favorites';
      tabHistFavs.classList.add('active');
      if (tabHistAll) tabHistAll.classList.remove('active');
      renderCommandHistorySidebar();
    });
  }

  // Global Responsive Resize Handlers (Eliminates having to fullscreen/unfullscreen to fix layout)
  window.addEventListener('resize', () => {
    fitActiveTerminalSession();
  });

  // Attach ResizeObserver to terminal container for seamless auto-fitting without fullscreen toggling
  const termContainer = document.getElementById('terminal-container');
  if (termContainer && window.ResizeObserver) {
    const termRO = new ResizeObserver(() => {
      requestAnimationFrame(() => fitActiveTerminalSession());
    });
    termRO.observe(termContainer);
  }

  // Disable default browser context menu globally, but open SSH terminal context menu when right-clicking on terminal
  window.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    const termPane = e.target.closest('.terminal-pane') || e.target.closest('#terminal-container');
    if (termPane && activeSessionId && activeSessionId !== 'home') {
      openTerminalContextMenu(e, activeSessionId);
    }
  }, true);

  // Action submenu & context menu outside-click, escape, and scroll dismissal
  window.addEventListener('click', (e) => {
    if (!e.target.closest('#active-action-menu') &&
      !e.target.closest('#active-terminal-context-menu') &&
      !e.target.closest('.btn-host-actions') &&
      !e.target.closest('.btn-vcard-menu') &&
      !e.target.closest('.btn-open-actions') &&
      !e.target.closest('.btn-open-table-actions')) {
      closeAllActionMenus();
    }
  });
  window.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeAllActionMenus();
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'h') {
      if (activeSessionId && activeSessionId !== 'home') {
        e.preventDefault();
        toggleCommandHistorySidebar();
      }
    }
  });
  window.addEventListener('resize', closeAllActionMenus);
  window.addEventListener('scroll', closeAllActionMenus, true);
}


function updateAuthModeUI(mode) {
  const pwdGroup = document.getElementById('group-password');
  const keyGroup = document.getElementById('group-private-key');

  if (mode === 'password') {
    pwdGroup.classList.remove('hidden');
    keyGroup.classList.add('hidden');
  } else if (mode === 'private_key') {
    pwdGroup.classList.add('hidden');
    keyGroup.classList.remove('hidden');
  } else if (mode === 'both') {
    // User requested feature: BOTH Password AND Key!
    pwdGroup.classList.remove('hidden');
    keyGroup.classList.remove('hidden');
  } else if (mode === 'interactive') {
    pwdGroup.classList.add('hidden');
    keyGroup.classList.add('hidden');
  }
}

// =========================================================
// Host Management
// =========================================================
function renderTagFilters() {
  const container = document.getElementById('tag-filters');
  if (!container) return; // Tag filter bar removed from sidebar to maximize room
  const allTags = new Set();
  hosts.forEach(h => {
    if (Array.isArray(h.tags)) h.tags.forEach(t => allTags.add(t));
  });

  container.innerHTML = `<button class="tag-pill ${activeTag === 'all' ? 'active' : ''}" data-tag="all">All</button>`;
  allTags.forEach(tag => {
    const btn = document.createElement('button');
    btn.className = `tag-pill ${activeTag === tag ? 'active' : ''}`;
    btn.dataset.tag = tag;
    btn.textContent = tag;
    btn.addEventListener('click', () => {
      activeTag = tag;
      renderTagFilters();
      renderHostList();
    });
    container.appendChild(btn);
  });

  const allBtn = container.querySelector('[data-tag="all"]');
  if (allBtn) {
    allBtn.addEventListener('click', () => {
      activeTag = 'all';
      renderTagFilters();
      renderHostList();
    });
  }
}

function renderHostList() {
  updateHostCount();
  const container = document.getElementById('host-list');
  if (!container) return;
  container.innerHTML = '';

  const totalFavorites = hosts.filter(isFavoriteHost).length;
  if (totalFavorites === 0) {
    container.innerHTML = `
      <div class="sidebar-empty-favorites">
        <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="#f59e0b" stroke-width="1.8">
          <polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"></polygon>
        </svg>
        <p>No Favorites Yet</p>
        <span>Click the ⭐ star on any Vault host to pin it to this sidebar.</span>
      </div>
    `;
    return;
  }

  const filtered = hosts.filter(h => {
    // Only display hosts that are marked as favorite
    if (!isFavoriteHost(h)) return false;

    const matchesSearch = !searchQuery ||
      (h.label && h.label.toLowerCase().includes(searchQuery)) ||
      (h.hostname && h.hostname.toLowerCase().includes(searchQuery)) ||
      (h.username && h.username.toLowerCase().includes(searchQuery)) ||
      (Array.isArray(h.tags) && h.tags.some(t => t.toLowerCase().includes(searchQuery)));
    return matchesSearch;
  });

  if (filtered.length === 0) {
    container.innerHTML = `<div class="empty-hint" style="color: var(--text-dim); text-align: center; padding: 24px 0; font-size: 0.82rem;">No favorites match your search</div>`;
    return;
  }

  filtered.forEach(host => {
    const card = document.createElement('div');
    const hostSessions = Array.from(sessions.values()).filter(s => s.host.id === host.id);
    const isConnected = hostSessions.some(s => s.status === 'connected');
    const isConnecting = !isConnected && hostSessions.some(s => s.status === 'connecting' || s.status === 'reconnecting');
    const isFailed = !isConnected && !isConnecting && hostSessions.some(s => s.status === 'failed' || s.status === 'disconnected');

    let statusClass = '';
    if (isConnected) statusClass = 'connected';
    else if (isConnecting) statusClass = 'connecting';
    else if (isFailed) statusClass = 'failed';

    card.className = `host-card ${statusClass ? `active-session ${statusClass}` : ''}`;

    card.innerHTML = `
      <div class="host-card-top">
        <div class="host-label-wrap">
          <span class="host-status-indicator ${statusClass}"></span>
          <span class="host-title">${escapeHtml(host.label)}</span>
        </div>
        <div class="host-card-actions">
          <button class="btn-fav-star is-fav btn-toggle-fav" title="Remove from Favorites (Unpin from Sidebar)">
            <svg width="13" height="13" viewBox="0 0 24 24" fill="#f59e0b" stroke="#f59e0b" stroke-width="2"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"></polygon></svg>
          </button>
          <button class="icon-subbtn btn-host-actions" title="Actions Menu (New Session, Resume, SFTP, Edit, Delete)">
            <svg width="15" height="15" viewBox="0 0 24 24" fill="currentColor"><circle cx="12" cy="12" r="2"></circle><circle cx="12" cy="5" r="2"></circle><circle cx="12" cy="19" r="2"></circle></svg>
          </button>
        </div>
      </div>
      <div class="host-card-meta">${escapeHtml(host.username)}@${escapeHtml(host.hostname)}:${host.port}</div>
    `;

    // Click on card -> Connect SSH (default resume or new)
    card.addEventListener('click', (e) => {
      if (e.target.closest('.icon-subbtn') || e.target.closest('.btn-fav-star')) return;
      launchSSHSession(host);
    });

    // Unpin favorite button
    card.querySelector('.btn-toggle-fav').addEventListener('click', (e) => {
      e.stopPropagation();
      toggleFavorite(host.id);
    });

    // Action submenu button
    card.querySelector('.btn-host-actions').addEventListener('click', (e) => {
      e.stopPropagation();
      openActionMenu(e, host, e.currentTarget);
    });

    container.appendChild(card);
  });
}

function openHostModal(host = null) {
  const modal = document.getElementById('modal-host');
  const title = document.getElementById('modal-host-title');
  const form = document.getElementById('host-form');
  form.reset();

  if (host) {
    title.textContent = 'Edit SSH Host';
    document.getElementById('form-host-id').value = host.id;
    document.getElementById('form-label').value = host.label || '';
    document.getElementById('form-group').value = host.group || '';
    document.getElementById('form-hostname').value = host.hostname || '';
    document.getElementById('form-port').value = host.port || 22;
    document.getElementById('form-username').value = host.username || '';

    const radio = document.querySelector(`input[name="auth_mode"][value="${host.auth_mode || 'password'}"]`);
    if (radio) radio.checked = true;
    updateAuthModeUI(host.auth_mode || 'password');

    document.getElementById('form-password').value = host.password || '';
    document.getElementById('form-private-key').value = host.private_key || '';
    document.getElementById('form-passphrase').value = host.passphrase || '';
    document.getElementById('form-keepalive').value = host.keepalive_interval || 30;
    document.getElementById('form-keepalive-max').value = host.keepalive_max || 3;
    document.getElementById('form-auto-reconnect').checked = host.auto_reconnect !== false;
    document.getElementById('form-persistent-session').checked = host.persistent_session !== false;
    document.getElementById('form-session-name').value = host.session_name || 'rustterm';
    document.getElementById('form-tags').value = (host.tags || []).join(', ');
    const favCb = document.getElementById('form-favorite');
    if (favCb) favCb.checked = host.is_favorite === true;
  } else {
    title.textContent = 'Configure SSH Host';
    document.getElementById('form-host-id').value = '';
    document.getElementById('form-port').value = 22;
    document.getElementById('form-keepalive').value = 30;
    document.getElementById('form-keepalive-max').value = 3;
    document.getElementById('form-auto-reconnect').checked = true;
    document.getElementById('form-persistent-session').checked = true;
    document.getElementById('form-session-name').value = 'rustterm';
    document.querySelector('input[name="auth_mode"][value="password"]').checked = true;
    updateAuthModeUI('password');
    const favCb = document.getElementById('form-favorite');
    if (favCb) favCb.checked = false;
  }

  modal.classList.remove('hidden');
}

function closeHostModal() {
  document.getElementById('modal-host').classList.add('hidden');
}

function handleSaveHost(e) {
  e.preventDefault();
  const id = document.getElementById('form-host-id').value || `host-${Date.now()}`;
  const label = document.getElementById('form-label').value.trim();
  const group = document.getElementById('form-group').value.trim();
  const hostname = document.getElementById('form-hostname').value.trim();
  const port = parseInt(document.getElementById('form-port').value, 10) || 22;
  const username = document.getElementById('form-username').value.trim();
  const authMode = document.querySelector('input[name="auth_mode"]:checked').value;
  const password = document.getElementById('form-password').value;
  const privateKey = document.getElementById('form-private-key').value.trim();
  const passphrase = document.getElementById('form-passphrase').value;
  const keepaliveInterval = parseInt(document.getElementById('form-keepalive').value, 10) || 30;
  const keepaliveMax = parseInt(document.getElementById('form-keepalive-max').value, 10) || 3;
  const autoReconnect = document.getElementById('form-auto-reconnect').checked;
  const persistentSession = document.getElementById('form-persistent-session').checked;
  const sessionName = document.getElementById('form-session-name').value.trim() || 'rustterm';
  const tags = document.getElementById('form-tags').value
    .split(',')
    .map(t => t.trim())
    .filter(Boolean);
  const isFavorite = document.getElementById('form-favorite') ? document.getElementById('form-favorite').checked : false;

  const newHost = {
    id,
    label,
    group,
    hostname,
    port,
    username,
    auth_mode: authMode,
    password,
    private_key: privateKey,
    passphrase,
    keepalive_interval: keepaliveInterval,
    keepalive_max: keepaliveMax,
    auto_reconnect: autoReconnect,
    persistent_session: persistentSession,
    session_name: sessionName,
    tags,
    is_favorite: isFavorite
  };

  const existingIdx = hosts.findIndex(h => h.id === id);
  if (existingIdx >= 0) {
    hosts[existingIdx] = newHost;
    showToast(`Updated '${label}'`, 'success');
  } else {
    hosts.push(newHost);
    showToast(`Created host '${label}'`, 'success');
  }

  saveHostsToStorage();
  renderTagFilters();
  renderHostList();
  renderVaultTagFilters();
  renderVaultStats();
  renderVaultHosts();
  closeHostModal();
}

function deleteHost(id) {
  const host = hosts.find(h => h.id === id);
  if (!host) return;
  if (confirm(`Are you sure you want to delete '${host.label}'?`)) {
    hosts = hosts.filter(h => h.id !== id);
    selectedHostIds.delete(id);
    saveHostsToStorage();
    renderTagFilters();
    renderHostList();
    renderVaultTagFilters();
    renderVaultStats();
    renderVaultHosts();
    updateBatchBar();
    showToast(`Deleted '${host.label}'`, 'info');
  }
}

// =========================================================
// Sidebar Toggle (Full Screen Mode)
// =========================================================
function toggleSidebar(forceState = null) {
  const app = document.getElementById('app');
  if (forceState !== null) {
    sidebarCollapsed = forceState;
  } else {
    sidebarCollapsed = !sidebarCollapsed;
  }
  if (sidebarCollapsed) {
    app.classList.add('sidebar-collapsed');
  } else {
    app.classList.remove('sidebar-collapsed');
  }
  localStorage.setItem('rustterm_sidebar_collapsed', sidebarCollapsed ? 'true' : 'false');
  if (activeSessionId && activeSessionId !== 'home') {
    const s = sessions.get(activeSessionId);
    if (s && s.fitAddon) setTimeout(() => s.fitAddon.fit(), 280);
  }
}

// =========================================================
// Vault Manager: View Mode & Stats
// =========================================================
function setVaultViewMode(mode) {
  vaultViewMode = mode;
  localStorage.setItem('rustterm_vault_view_mode', mode);

  const btnCard = document.getElementById('btn-view-card');
  const btnTable = document.getElementById('btn-view-table');
  if (btnCard && btnTable) {
    if (mode === 'table') {
      btnCard.classList.remove('active');
      btnTable.classList.add('active');
    } else {
      btnCard.classList.add('active');
      btnTable.classList.remove('active');
    }
  }

  renderVaultHosts();
}

function renderVaultStats() {
  const totalEl = document.getElementById('vstat-total');
  const activeEl = document.getElementById('vstat-active');
  const resumeEl = document.getElementById('vstat-persistent');
  const badgeEl = document.getElementById('tab-home-badge');

  const total = hosts.length;
  const activeCount = sessions.size;
  const persistentCount = hosts.filter(h => hasOldSession(h)).length;

  if (totalEl) totalEl.textContent = total;
  if (activeEl) activeEl.textContent = activeCount;
  if (resumeEl) resumeEl.textContent = persistentCount;
  if (badgeEl) badgeEl.textContent = total;
}

function renderVaultTagFilters() {
  const container = document.getElementById('vault-tag-filters');
  if (!container) return;

  const allTags = new Set();
  hosts.forEach(h => {
    if (Array.isArray(h.tags)) h.tags.forEach(t => allTags.add(t));
  });

  container.innerHTML = `<button class="tag-pill ${vaultActiveTag === 'all' ? 'active' : ''}" data-tag="all">All (${hosts.length})</button>`;

  allTags.forEach(tag => {
    const count = hosts.filter(h => Array.isArray(h.tags) && h.tags.includes(tag)).length;
    const btn = document.createElement('button');
    btn.className = `tag-pill ${vaultActiveTag === tag ? 'active' : ''}`;
    btn.dataset.tag = tag;
    btn.textContent = `${tag} (${count})`;
    btn.addEventListener('click', () => {
      vaultActiveTag = tag;
      renderVaultTagFilters();
      renderVaultHosts();
    });
    container.appendChild(btn);
  });

  const allBtn = container.querySelector('[data-tag="all"]');
  if (allBtn) {
    allBtn.addEventListener('click', () => {
      vaultActiveTag = 'all';
      renderVaultTagFilters();
      renderVaultHosts();
    });
  }
}

function toggleFavorite(hostId) {
  const host = hosts.find(h => h.id === hostId);
  if (!host) return;
  host.is_favorite = !isFavoriteHost(host);
  saveHostsToStorage();
  updateHostCount();
  renderHostList();
  renderVaultHosts();
  updateBatchBar();
  showToast(host.is_favorite ? `⭐ '${host.label}' added to Favorites (Pinned to Sidebar)` : `'${host.label}' removed from Favorites`, 'info');
}

function updateBatchBar() {
  const bar = document.getElementById('vault-batch-bar');
  const countPill = document.getElementById('batch-selected-count');
  if (!bar || !countPill) return;

  // Clean up any stale selected host IDs
  for (const id of selectedHostIds) {
    if (!hosts.some(h => h.id === id)) {
      selectedHostIds.delete(id);
    }
  }

  const count = selectedHostIds.size;
  countPill.textContent = count;
  if (count > 0) {
    bar.classList.remove('hidden');
  } else {
    bar.classList.add('hidden');
  }

  // Update table master checkbox if present
  const thSelectAll = document.getElementById('th-select-all');
  if (thSelectAll) {
    const visible = getFilteredVaultHosts();
    if (visible.length > 0 && visible.every(h => selectedHostIds.has(h.id))) {
      thSelectAll.checked = true;
      thSelectAll.indeterminate = false;
    } else if (visible.some(h => selectedHostIds.has(h.id))) {
      thSelectAll.checked = false;
      thSelectAll.indeterminate = true;
    } else {
      thSelectAll.checked = false;
      thSelectAll.indeterminate = false;
    }
  }
}

function getFilteredVaultHosts() {
  const q = vaultSearchQuery.toLowerCase().trim();
  return hosts.filter(h => {
    const matchesTag = vaultActiveTag === 'all' || (Array.isArray(h.tags) && h.tags.includes(vaultActiveTag));
    const matchesSearch = !q ||
      (h.label && h.label.toLowerCase().includes(q)) ||
      (h.hostname && h.hostname.toLowerCase().includes(q)) ||
      (h.username && h.username.toLowerCase().includes(q)) ||
      (h.group && h.group.toLowerCase().includes(q)) ||
      (Array.isArray(h.tags) && h.tags.some(t => t.toLowerCase().includes(q)));
    return matchesTag && matchesSearch;
  });
}

function handleBatchFavorite(setFav = true) {
  if (selectedHostIds.size === 0) return;
  let count = 0;
  hosts.forEach(h => {
    if (selectedHostIds.has(h.id)) {
      h.is_favorite = setFav;
      count++;
    }
  });
  saveHostsToStorage();
  renderHostList();
  renderVaultHosts();
  updateBatchBar();
  showToast(setFav ? `⭐ ${count} host(s) added to Favorites` : `${count} host(s) removed from Favorites`, 'success');
}

function handleBatchDelete() {
  const count = selectedHostIds.size;
  if (count === 0) return;
  if (confirm(`Are you sure you want to delete ${count} selected host(s)? This action cannot be undone.`)) {
    hosts = hosts.filter(h => !selectedHostIds.has(h.id));
    selectedHostIds.clear();
    saveHostsToStorage();
    renderTagFilters();
    renderHostList();
    renderVaultTagFilters();
    renderVaultStats();
    renderVaultHosts();
    updateBatchBar();
    showToast(`${count} host(s) deleted successfully`, 'info');
  }
}

function handleBatchClear() {
  selectedHostIds.clear();
  renderVaultHosts();
  updateBatchBar();
}

function renderVaultHosts() {
  const container = document.getElementById('vault-hosts-container');
  if (!container) return;

  const filtered = getFilteredVaultHosts();

  if (filtered.length === 0) {
    container.innerHTML = `
      <div style="text-align: center; padding: 60px 20px; color: var(--text-dim);">
        <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" style="margin-bottom: 12px; opacity: 0.5;">
          <circle cx="11" cy="11" r="8"></circle><line x1="21" y1="21" x2="16.65" y2="16.65"></line>
        </svg>
        <h3 style="font-size: 1.1rem; color: var(--text-muted); margin-bottom: 6px;">No Matching Vault Hosts Found</h3>
        <p style="font-size: 0.84rem;">Try adjusting your search query or tag filters.</p>
      </div>
    `;
    updateBatchBar();
    return;
  }

  if (vaultViewMode === 'table') {
    renderVaultTableView(container, filtered);
  } else {
    renderVaultCardView(container, filtered);
  }
  updateBatchBar();
}

// =========================================================
// Unified Action Submenu Controller
// =========================================================
let activeActionMenuElement = null;

function closeAllActionMenus() {
  if (activeActionMenuElement) {
    activeActionMenuElement.remove();
    activeActionMenuElement = null;
  }
  const termMenu = document.getElementById('active-terminal-context-menu');
  if (termMenu) {
    termMenu.remove();
  }
}

function openActionMenu(e, host, triggerBtn) {
  e.stopPropagation();
  closeAllActionMenus();

  const isConnected = Array.from(sessions.values()).some(s => s.host.id === host.id);
  const oldSessionAvailable = hasOldSession(host);
  const isFav = isFavoriteHost(host);

  const rect = triggerBtn.getBoundingClientRect();

  const menu = document.createElement('div');
  menu.className = 'action-submenu-dropdown';
  menu.id = 'active-action-menu';

  menu.innerHTML = `
    <div class="menu-item-group-title">${escapeHtml(host.label)}</div>
    <div class="menu-item menu-item-connect-new" data-action="connect-new">
      <div class="menu-item-icon">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#10b981" stroke-width="2.5"><polygon points="5 3 19 12 5 21 5 3"></polygon></svg>
      </div>
      <div class="menu-text-wrap">
        <span class="menu-item-text">Connect New Session</span>
        <span class="menu-item-hint">Open a new independent terminal session</span>
      </div>
    </div>
    <div class="menu-item menu-item-connect-resume ${oldSessionAvailable ? '' : 'disabled'}" data-action="connect-resume" title="${oldSessionAvailable ? 'Reconnect to existing server session (tmux/screen)' : 'No existing server session available to resume'}">
      <div class="menu-item-icon">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="${oldSessionAvailable ? '#06b6d4' : '#64748b'}" stroke-width="2.5"><polyline points="23 4 23 10 17 10"></polyline><polyline points="1 20 1 14 7 14"></polyline><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"></path></svg>
      </div>
      <div class="menu-text-wrap">
        <span class="menu-item-text">Connect Resume Session</span>
        <span class="menu-item-hint">${oldSessionAvailable ? 'Resume server session (tmux/screen)' : '(No previous session)'}</span>
      </div>
    </div>
    <div class="menu-divider"></div>
    <div class="menu-item" data-action="sftp">
      <div class="menu-item-icon">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#38bdf8" stroke-width="2"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path><polyline points="7 10 12 15 17 10"></polyline><line x1="12" y1="15" x2="12" y2="3"></line></svg>
      </div>
      <div class="menu-text-wrap">
        <span class="menu-item-text">Download Files (SFTP)</span>
        <span class="menu-item-hint">Browse & download files from server</span>
      </div>
    </div>
    <div class="menu-item" data-action="favorite">
      <div class="menu-item-icon">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="${isFav ? '#f59e0b' : 'none'}" stroke="${isFav ? '#f59e0b' : 'currentColor'}" stroke-width="2"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"></polygon></svg>
      </div>
      <div class="menu-text-wrap">
        <span class="menu-item-text">${isFav ? 'Remove from Favorites' : 'Add to Favorites'}</span>
        <span class="menu-item-hint">${isFav ? 'Unpin host from sidebar' : 'Pin host to sidebar'}</span>
      </div>
    </div>
    <div class="menu-item" data-action="edit">
      <div class="menu-item-icon">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 20h9"></path><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z"></path></svg>
      </div>
      <div class="menu-text-wrap">
        <span class="menu-item-text">Edit Configuration</span>
        <span class="menu-item-hint">Modify credentials & connection</span>
      </div>
    </div>
    <div class="menu-divider"></div>
    <div class="menu-item danger" data-action="delete">
      <div class="menu-item-icon">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#f43f5e" stroke-width="2"><polyline points="3 6 5 6 21 6"></polyline><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path></svg>
      </div>
      <div class="menu-text-wrap">
        <span class="menu-item-text">Delete Host</span>
        <span class="menu-item-hint">Delete host from system</span>
      </div>
    </div>
  `;

  document.body.appendChild(menu);
  activeActionMenuElement = menu;

  const menuWidth = 260;
  let top = rect.bottom + 6;
  let left = rect.right - menuWidth;

  if (left < 10) left = 10;
  if (left + menuWidth > window.innerWidth - 10) {
    left = window.innerWidth - menuWidth - 10;
  }

  const menuHeight = 310;
  if (top + menuHeight > window.innerHeight - 10) {
    top = rect.top - menuHeight - 6;
    if (top < 10) top = 10;
  }

  menu.style.position = 'fixed';
  menu.style.top = `${top}px`;
  menu.style.left = `${left}px`;
  menu.style.zIndex = '10000';

  menu.addEventListener('click', (ev) => {
    ev.stopPropagation();
    const item = ev.target.closest('.menu-item');
    if (!item) return;
    if (item.classList.contains('disabled')) return;

    const action = item.dataset.action;
    closeAllActionMenus();

    if (action === 'connect-new') {
      launchSSHSession(host, { mode: 'new' });
    } else if (action === 'connect-resume') {
      launchSSHSession(host, { mode: 'resume' });
    } else if (action === 'sftp') {
      openSftpModal(host);
    } else if (action === 'favorite') {
      toggleFavorite(host.id);
    } else if (action === 'edit') {
      openHostModal(host);
    } else if (action === 'delete') {
      deleteHost(host.id);
    }
  });
}

function renderVaultCardView(container, hostList) {
  const grid = document.createElement('div');
  grid.className = 'vault-cards-grid';

  hostList.forEach(host => {
    const isConnected = Array.from(sessions.values()).some(s => s.host.id === host.id);
    const oldSessionAvailable = hasOldSession(host);
    const isSelected = selectedHostIds.has(host.id);

    let authLabel = 'Password';
    if (host.auth_mode === 'private_key') authLabel = 'Key';
    if (host.auth_mode === 'both') authLabel = 'Pass + Key';
    if (host.auth_mode === 'interactive') authLabel = 'Interactive';

    let statusDotClass = isConnected ? 'active' : (oldSessionAvailable ? 'resumable' : 'idle');
    let statusText = '';
    let statusBadgeClass = '';

    if (isConnected) {
      statusText = `🟢 Active Session Running (Connected Live)`;
      statusBadgeClass = 'active';
    } else if (oldSessionAvailable) {
      const hist = getHostSessionHistory(host.id);
      const sname = (hist && hist.sessionName) || host.session_name || 'rustterm';
      statusText = `🔵 Session Available on Server (tmux/screen: ${escapeHtml(sname)})`;
      statusBadgeClass = 'resumable';
    } else {
      statusText = `⚪ No Previous Session (Ready to Connect)`;
      statusBadgeClass = 'idle';
    }

    const card = document.createElement('div');
    card.className = `vault-card ${isConnected ? 'has-active' : ''} ${isSelected ? 'card-selected' : ''}`;
    card.dataset.hostId = host.id;
    card.innerHTML = `
      <div class="vcard-header">
        <div class="vcard-title-block">
          <div class="vcard-cb-wrap">
            <input type="checkbox" class="vault-cb vcard-cb" data-host-id="${host.id}" ${isSelected ? 'checked' : ''} title="Select this host for batch actions" />
          </div>
          <div class="vcard-avatar ${isConnected ? 'online' : ''}">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <rect x="2" y="2" width="20" height="8" rx="2" ry="2"></rect>
              <rect x="2" y="14" width="20" height="8" rx="2" ry="2"></rect>
              <line x1="6" y1="6" x2="6.01" y2="6"></line>
              <line x1="6" y1="18" x2="6.01" y2="18"></line>
            </svg>
            <span class="vcard-status-dot ${statusDotClass}"></span>
          </div>
          <div>
            <div class="vcard-title">${escapeHtml(host.label)}</div>
            <div class="vcard-group">${escapeHtml(host.group || 'Default Vault')}</div>
          </div>
        </div>

        <div class="vcard-top-actions">
          <button class="btn-fav-star ${isFavoriteHost(host) ? 'is-fav' : ''} btn-card-fav" title="${isFavoriteHost(host) ? 'Remove from Favorites (Unpin from Sidebar)' : 'Add to Favorites (Pin to Sidebar)'}">
            <svg width="15" height="15" viewBox="0 0 24 24" fill="${isFavoriteHost(host) ? '#f59e0b' : 'none'}" stroke="${isFavoriteHost(host) ? '#f59e0b' : 'currentColor'}" stroke-width="2"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"></polygon></svg>
          </button>
          <button class="icon-subbtn btn-vcard-menu" title="Actions Menu (New Session, Resume, SFTP, Edit, Delete)">
            <svg width="15" height="15" viewBox="0 0 24 24" fill="currentColor"><circle cx="12" cy="12" r="2"></circle><circle cx="12" cy="5" r="2"></circle><circle cx="12" cy="19" r="2"></circle></svg>
          </button>
        </div>
      </div>

      <div class="vcard-details">
        <div class="vcard-endpoint">
          <span class="vcard-user">${escapeHtml(host.username)}</span><span class="vcard-at">@</span><span class="vcard-host">${escapeHtml(host.hostname)}</span><span class="vcard-colon">:</span><span class="vcard-port">${host.port}</span>
        </div>
        <div class="vcard-badges">
          <span class="auth-badge">${authLabel}</span>
          <span class="tag-badge" style="border-color: rgba(16, 185, 129, 0.3); color: #34d399;">Keepalive: ${host.keepalive_interval || 30}s</span>
          ${host.persistent_session !== false ? `<span class="tag-badge" style="border-color: rgba(6, 182, 212, 0.4); color: #38bdf8;">Auto-Resume</span>` : ''}
          ${(host.tags || []).map(t => `<span class="tag-badge">${escapeHtml(t)}</span>`).join('')}
        </div>
      </div>

      <div class="vcard-session-info ${statusBadgeClass}">
        <span>${statusText}</span>
      </div>

      <div class="vcard-actions-row">
        <button class="btn btn-connect-primary btn-quick-launch" title="Connect SSH to ${escapeHtml(host.label)}">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polygon points="5 3 19 12 5 21 5 3"></polygon></svg>
          <span>Connect</span>
        </button>
        <button class="btn btn-card-actions-dropdown btn-open-actions" title="Actions Menu (New Session, Resume, SFTP, Edit, Delete)">
          <span>Actions</span>
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="6 9 12 15 18 9"></polyline></svg>
        </button>
      </div>
    `;

    // Checkbox toggle
    const cb = card.querySelector('.vcard-cb');
    cb.addEventListener('change', (e) => {
      e.stopPropagation();
      if (e.target.checked) {
        selectedHostIds.add(host.id);
        card.classList.add('card-selected');
      } else {
        selectedHostIds.delete(host.id);
        card.classList.remove('card-selected');
      }
      updateBatchBar();
    });

    // Favorite toggle button
    card.querySelector('.btn-card-fav').addEventListener('click', (e) => {
      e.stopPropagation();
      toggleFavorite(host.id);
    });

    // Connect Primary (Auto Resume / New) button
    card.querySelector('.btn-quick-launch').addEventListener('click', (e) => {
      e.stopPropagation();
      launchSSHSession(host);
    });

    // Submenu actions trigger
    const menuTrigger = (e) => {
      e.stopPropagation();
      openActionMenu(e, host, e.currentTarget);
    };
    card.querySelector('.btn-vcard-menu').addEventListener('click', menuTrigger);
    card.querySelector('.btn-open-actions').addEventListener('click', menuTrigger);

    grid.appendChild(card);
  });

  container.innerHTML = '';
  container.appendChild(grid);
}

function renderVaultTableView(container, hostList) {
  const wrapper = document.createElement('div');
  wrapper.className = 'vault-table-wrapper';

  let rowsHtml = '';
  hostList.forEach(host => {
    const isConnected = Array.from(sessions.values()).some(s => s.host.id === host.id);
    const oldSessionAvailable = hasOldSession(host);
    const isSelected = selectedHostIds.has(host.id);

    let authLabel = 'Password';
    if (host.auth_mode === 'private_key') authLabel = 'Key';
    if (host.auth_mode === 'both') authLabel = 'Pass + Key';
    if (host.auth_mode === 'interactive') authLabel = 'Interactive';

    let dotColor = isConnected ? '#10b981' : (oldSessionAvailable ? '#06b6d4' : '#64748b');
    let sessionStatusStr = isConnected
      ? '<span style="color: #34d399; font-weight: 600;">● Active (Live)</span>'
      : (oldSessionAvailable ? `<span style="color: #38bdf8; font-weight: 500;">● Resume Ready</span>` : '<span style="color: var(--text-dim);">○ None</span>');

    rowsHtml += `
      <tr class="${isConnected ? 'has-active' : ''} ${isSelected ? 'row-selected' : ''}" data-host-id="${host.id}">
        <td style="text-align: center; width: 34px;">
          <input type="checkbox" class="vault-cb row-cb" data-host-id="${host.id}" ${isSelected ? 'checked' : ''} title="Select this host" />
        </td>
        <td style="text-align: center; width: 32px;">
          <button class="btn-fav-star ${isFavoriteHost(host) ? 'is-fav' : ''} btn-row-fav" title="${isFavoriteHost(host) ? 'Remove from Favorites (Unpin from Sidebar)' : 'Add to Favorites (Pin to Sidebar)'}">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="${isFavoriteHost(host) ? '#f59e0b' : 'none'}" stroke="${isFavoriteHost(host) ? '#f59e0b' : 'currentColor'}" stroke-width="2"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"></polygon></svg>
          </button>
        </td>
        <td style="width: 24px;"><span style="display:inline-block; width:8px; height:8px; border-radius:50%; background-color:${dotColor}; box-shadow: 0 0 6px ${dotColor};"></span></td>
        <td>
          <div class="t-host-title">${escapeHtml(host.label)}</div>
          <div class="t-host-group">${escapeHtml(host.group || 'Default')}</div>
        </td>
        <td>
          <span class="t-endpoint">${escapeHtml(host.hostname)}:${host.port}</span>
        </td>
        <td>
          <span style="font-family: var(--font-mono); color: #f59e0b; font-weight:600;">${escapeHtml(host.username)}</span>
          <span class="auth-badge" style="margin-left: 6px;">${authLabel}</span>
        </td>
        <td>
          <div style="display:flex; flex-wrap:wrap; gap:4px;">
            ${(host.tags || []).map(t => `<span class="tag-badge">${escapeHtml(t)}</span>`).join('')}
          </div>
        </td>
        <td>
          <span style="font-family: var(--font-mono); font-size: 0.78rem; color: #34d399;">${host.keepalive_interval || 30}s</span>
        </td>
        <td>
          <div style="font-family: var(--font-mono); font-size: 0.75rem;">${sessionStatusStr}</div>
        </td>
        <td style="text-align: right; width: 110px;">
          <div class="t-actions-wrap">
            <button class="btn btn-table-actions btn-open-table-actions" title="Actions Menu (New Session, Resume, SFTP, Edit, Delete)">
              <span>Actions</span>
              <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="6 9 12 15 18 9"></polyline></svg>
            </button>
          </div>
        </td>
      </tr>
    `;
  });

  wrapper.innerHTML = `
    <table class="vault-table">
      <thead>
        <tr>
          <th style="width: 34px; text-align: center;">
            <input type="checkbox" id="th-select-all" class="vault-cb" title="Select all hosts" />
          </th>
          <th style="width: 32px; text-align: center;" title="Favorite Status">⭐</th>
          <th style="width: 24px;"></th>
          <th>Host Name / Label</th>
          <th>Address & Port</th>
          <th>User & Auth</th>
          <th>Tags</th>
          <th>Keepalive</th>
          <th>Session Status</th>
          <th style="text-align: right; width: 110px;">Actions</th>
        </tr>
      </thead>
      <tbody>
        ${rowsHtml}
      </tbody>
    </table>
  `;

  // Attach master select-all in header
  const thSelectAll = wrapper.querySelector('#th-select-all');
  if (thSelectAll) {
    thSelectAll.addEventListener('change', (e) => {
      const isChecked = e.target.checked;
      hostList.forEach(h => {
        if (isChecked) selectedHostIds.add(h.id);
        else selectedHostIds.delete(h.id);
      });
      renderVaultHosts();
    });
  }

  // Attach event listeners for table buttons & checkboxes
  wrapper.querySelectorAll('tbody tr').forEach(row => {
    const hostId = row.dataset.hostId;
    const host = hostList.find(h => h.id === hostId);
    if (!host) return;

    // Row checkbox
    const rowCb = row.querySelector('.row-cb');
    if (rowCb) {
      rowCb.addEventListener('change', (e) => {
        if (e.target.checked) {
          selectedHostIds.add(host.id);
          row.classList.add('row-selected');
        } else {
          selectedHostIds.delete(host.id);
          row.classList.remove('row-selected');
        }
        updateBatchBar();
      });
    }

    // Row favorite star
    const favBtn = row.querySelector('.btn-row-fav');
    if (favBtn) {
      favBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        toggleFavorite(host.id);
      });
    }

    // Actions submenu button
    const actionsBtn = row.querySelector('.btn-open-table-actions');
    if (actionsBtn) {
      actionsBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        openActionMenu(e, host, e.currentTarget);
      });
    }

    // Row click launches default connect (unless clicking checkbox, star, or actions)
    row.addEventListener('click', (e) => {
      if (e.target.closest('.vault-cb') || e.target.closest('.btn-row-fav') || e.target.closest('.btn-open-table-actions')) return;
      launchSSHSession(host);
    });
  });

  container.innerHTML = '';
  container.appendChild(wrapper);
}

// =========================================================
// Terminal & SSH Session Engine
// =========================================================

function fitActiveTerminalSession() {
  if (!activeSessionId || activeSessionId === 'home') return;
  const s = sessions.get(activeSessionId);
  if (!s || !s.fitAddon || !s.terminal) return;

  try {
    s.fitAddon.fit();
    const cols = s.terminal.cols;
    const rows = s.terminal.rows;
    if (cols > 0 && rows > 0) {
      invokeTauri('resize_ssh_terminal', {
        sessionId: s.id,
        cols,
        rows
      }).catch(() => { });
    }
  } catch (e) {
    console.warn('fitActiveTerminalSession error:', e);
  }
}

async function reconnectSession(sessionId) {
  const session = sessions.get(sessionId);
  if (!session) return;
  session.status = 'reconnecting';
  updateSessionToolbar(session);
  session.terminal.writeln('\r\n\x1b[1;33m[RECONNECT]\x1b[0m Reconnecting to SSH server...');
  try {
    fitActiveTerminalSession();
    await invokeTauri('connect_ssh', {
      sessionId: session.id,
      hostConfig: session.host,
      cols: session.terminal.cols || 120,
      rows: session.terminal.rows || 36
    });
    session.status = 'connected';
    updateSessionToolbar(session);
    session.terminal.writeln('\x1b[1;32m[RESTORED]\x1b[0m SSH Session reconnected successfully.\r\n');
    showToast('Session reconnected', 'success');
    fitActiveTerminalSession();
  } catch (e) {
    session.status = 'failed';
    updateSessionToolbar(session);
    session.terminal.writeln(`\x1b[1;31m[FAILED]\x1b[0m Reconnect failed: ${e.message || e}\r\n`);
    showToast(`Reconnect failed: ${e.message || e}`, 'error');
  }
}

async function launchSSHSession(host, options = {}) {
  const mode = options.mode || 'new';

  if (mode === 'resume') {
    // 1. If an active session tab is already running in RustTerm, switch to it!
    const existing = Array.from(sessions.values()).find(s => s.host.id === host.id);
    if (existing) {
      switchSessionTab(existing.id);
      showToast(`Switched to active session for '${host.label}'`, 'success');
      return;
    }
  }

  // Determine session configuration
  let hostForConnection = { ...host };
  if (mode === 'new' && host.persistent_session !== false) {
    // Generate fresh session name so it starts an independent session
    const suffix = Math.random().toString(36).substring(2, 6);
    hostForConnection.session_name = `${host.session_name || 'rustterm'}_${suffix}`;
  } else {
    hostForConnection.session_name = host.session_name || 'rustterm';
  }

  const sessionId = `session-${Date.now()}`;
  const vaultView = document.getElementById('vault-view');
  if (vaultView) vaultView.classList.add('hidden');
  document.getElementById('session-toolbar').classList.remove('hidden');

  // Always ensure command history sidebar is open on new connection
  isHistorySidebarOpen = true;
  localStorage.setItem('rustterm_history_sidebar_open', 'true');

  // Record host session history so "Resume" becomes available for this host
  recordHostSession(host.id, hostForConnection.session_name);

  // Deactivate existing panes so new pane mounts cleanly
  document.querySelectorAll('.terminal-pane').forEach(p => p.classList.remove('active'));

  // Create Terminal DOM Pane and activate it immediately so it acquires DOM layout
  const pane = document.createElement('div');
  pane.className = 'terminal-pane active';
  pane.id = `pane-${sessionId}`;
  document.getElementById('terminal-container').appendChild(pane);

  // Apply history sidebar layout before initial fit
  applyHistorySidebarVisibility();

  // Initialize xterm.js instance
  const term = new Terminal({
    cursorBlink: true,
    fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
    fontSize: currentFontSize,
    lineHeight: 1.25,
    ignoreBracketedPasteMode: true,
    rightClickSelectsWord: false,
    theme: {
      background: '#060910',
      foreground: '#f1f5f9',
      cursor: '#06b6d4',
      cursorAccent: '#060910',
      selectionBackground: 'rgba(6, 182, 212, 0.35)',
      black: '#0f172a',
      red: '#ef4444',
      green: '#10b981',
      yellow: '#f59e0b',
      blue: '#3b82f6',
      magenta: '#8b5cf6',
      cyan: '#06b6d4',
      white: '#e2e8f0',
      brightBlack: '#475569',
      brightRed: '#f87171',
      brightGreen: '#34d399',
      brightYellow: '#fbbf24',
      brightBlue: '#60a5fa',
      brightMagenta: '#a78bfa',
      brightCyan: '#22d3ee',
      brightWhite: '#ffffff'
    }
  });

  const fitAddon = new FitAddon();
  term.loadAddon(fitAddon);
  term.loadAddon(new WebLinksAddon());

  term.open(pane);

  // Keyboard shortcut handlers (Ctrl+Shift+C: Copy, Ctrl+Shift+V: Paste)
  term.attachCustomKeyEventHandler((event) => {
    const isCtrlOrMeta = event.ctrlKey || event.metaKey;

    // Ctrl+Shift+C: Copy selected terminal text
    if (isCtrlOrMeta && event.shiftKey && (event.key === 'C' || event.key === 'c')) {
      if (event.type === 'keydown') {
        const sessionObj = sessions.get(sessionId);
        copyTerminalSelection(sessionObj);
      }
      return false; // Prevent xterm and browser default
    }

    // Ctrl+Shift+V: Paste cleaned clipboard text
    if (isCtrlOrMeta && event.shiftKey && (event.key === 'V' || event.key === 'v')) {
      if (event.type === 'keydown') {
        handleTerminalPasteAction(sessionId);
      }
      return false; // Prevent xterm and browser default
    }

    return true;
  });

  // Intercept native DOM paste event on the terminal pane to clean input before sending
  pane.addEventListener('paste', (e) => {
    e.preventDefault();
    e.stopPropagation();
    const clipData = (e.clipboardData || window.clipboardData)?.getData('text/plain') || '';
    if (clipData) {
      pasteTextToTerminal(sessionId, clipData);
    }
  }, true);

  // Instantly measure viewport to prevent half-screen 30-row glitches
  fitAddon.fit();
  let initialCols = term.cols || 120;
  let initialRows = term.rows || 36;
  const termContEl = document.getElementById('terminal-container');
  if (termContEl && (initialCols <= 80 || initialRows <= 24)) {
    const w = termContEl.clientWidth - 28;
    const h = termContEl.clientHeight - 20;
    if (w > 100 && h > 100) {
      const approxCols = Math.floor(w / 9.2);
      const approxRows = Math.floor(h / 17);
      if (approxCols > initialCols) initialCols = approxCols;
      if (approxRows > initialRows) initialRows = approxRows;
    }
  }

  // Print connection banner
  const modeBadge = mode === 'resume' ? 'RESUME SESSION' : 'NEW SESSION';
  term.writeln('\x1b[1;36m┌──────────────────────────────────────────────────────────────┐\x1b[0m');
  term.writeln(`\x1b[1;36m│\x1b[0m  \x1b[1;32mRustTerm SSH Client\x1b[0m - Connecting to \x1b[1;33m${host.username}@${host.hostname}:${host.port}\x1b[0m  \x1b[1;36m│\x1b[0m`);
  term.writeln(`\x1b[1;36m│\x1b[0m  Mode: \x1b[1;35m${modeBadge}\x1b[0m  |  Auto-Resume: \x1b[1;36m${hostForConnection.session_name}\x1b[0m  \x1b[1;36m│\x1b[0m`);
  term.writeln(`\x1b[1;36m│\x1b[0m  Keepalive: \x1b[1;32m${host.keepalive_interval || 30}s\x1b[0m (Termius Engine)  \x1b[1;36m│\x1b[0m`);
  term.writeln('\x1b[1;36m└──────────────────────────────────────────────────────────────┘\x1b[0m\r\n');

  // Register session object
  const session = {
    id: sessionId,
    host: hostForConnection,
    terminal: term,
    fitAddon,
    element: pane,
    status: 'connecting',
    keepaliveIntervalSec: host.keepalive_interval || 30,
    keepaliveTimer: null,
    inputBuffer: ''
  };
  sessions.set(sessionId, session);

  // Create and add Session Tab
  createSessionTab(session);
  switchSessionTab(sessionId);

  // Hook terminal input -> send to backend & record command history
  term.onData(data => {
    invokeTauri('send_ssh_data', { sessionId, data });
    handleSessionTerminalInput(sessionId, data);
  });

  // Handle PTY window resize
  term.onResize(({ cols, rows }) => {
    invokeTauri('resize_ssh_terminal', { sessionId, cols, rows });
  });

  // Listen for real-time SSH output from Rust backend
  if (isTauri && window.__TAURI__ && window.__TAURI__.event) {
    try {
      const unlisten = await window.__TAURI__.event.listen(`ssh-output-${sessionId}`, (event) => {
        term.write(event.payload);
      });
      session.unlisten = unlisten;
    } catch (e) {
      console.warn('Could not register event listener:', e);
    }
  }

  // Call Rust backend to connect SSH with explicit viewport cols and rows
  try {
    const result = await invokeTauri('connect_ssh', {
      sessionId,
      hostConfig: hostForConnection,
      cols: initialCols,
      rows: initialRows
    });

    session.status = 'connected';
    updateSessionToolbar(session);
    startKeepaliveHeartbeat(session);
    showToast(`Connected to ${host.label} (${modeBadge})`, 'success');

    // Immediate sync: ensure remote PTY matches exact dimensions without needing window resize
    requestAnimationFrame(() => {
      fitActiveTerminalSession();
    });
  } catch (err) {
    session.status = 'failed';
    updateSessionToolbar(session);
    term.writeln(`\r\n\x1b[1;31m[ERROR]\x1b[0m Connection failed: ${err.message || err}\r\n`);
    showToast(`Failed to connect to ${host.label}`, 'error');
  }

  renderHostList();
  renderVaultStats();
  renderVaultHosts();
}

function startKeepaliveHeartbeat(session) {
  if (session.keepaliveTimer) clearInterval(session.keepaliveTimer);

  session.keepaliveTimer = setInterval(async () => {
    if (session.status !== 'connected') return;

    // Trigger visual heartbeat pulse on active tab
    if (activeSessionId === session.id) {
      const heart = document.querySelector('.heart-icon');
      if (heart) {
        heart.classList.add('pulse');
        setTimeout(() => heart.classList.remove('pulse'), 800);
      }
    }

    // Send keepalive packet to backend
    try {
      await invokeTauri('send_ssh_keepalive', { sessionId: session.id });
    } catch (e) {
      console.warn('Keepalive ping missed:', e);
      if (session.host.auto_reconnect) {
        handleAutoReconnect(session);
      }
    }
  }, session.keepaliveIntervalSec * 1000);
}

function handleAutoReconnect(session) {
  session.status = 'reconnecting';
  updateSessionToolbar(session);
  session.terminal.writeln('\r\n\x1b[1;33m[STABILITY MONITOR]\x1b[0m Connection dropped. Auto-reconnecting in 3s (Keepalive recovery)...');

  setTimeout(async () => {
    try {
      fitActiveTerminalSession();
      await invokeTauri('connect_ssh', {
        sessionId: session.id,
        hostConfig: session.host,
        cols: session.terminal.cols || 120,
        rows: session.terminal.rows || 36
      });
      session.status = 'connected';
      updateSessionToolbar(session);
      session.terminal.writeln('\x1b[1;32m[RESTORED]\x1b[0m SSH Session successfully restored.\r\n');
      showToast('Connection restored', 'success');
      fitActiveTerminalSession();
    } catch (e) {
      session.status = 'failed';
      updateSessionToolbar(session);
      session.terminal.writeln(`\x1b[1;31m[FAILED]\x1b[0m Reconnect attempt failed: ${e.message || e}\r\n`);
    }
  }, 3000);
}

function updateSessionTabStatus(session) {
  if (!session) return;
  const tab = document.getElementById(`tab-${session.id}`);
  if (!tab) return;
  const dot = tab.querySelector('.tab-dot');
  const status = session.status || 'connecting';

  tab.classList.remove('status-connecting', 'status-connected', 'status-disconnected', 'status-failed', 'status-reconnecting');
  tab.classList.add(`status-${status}`);

  if (dot) {
    dot.className = `tab-dot ${status}`;
  }
}

function createSessionTab(session) {
  const container = document.getElementById('tabs-container');
  const tab = document.createElement('div');
  const status = session.status || 'connecting';
  tab.className = `session-tab status-${status}`;
  tab.id = `tab-${session.id}`;
  tab.innerHTML = `
    <span class="tab-dot ${status}"></span>
    <span class="tab-label">${escapeHtml(session.host.label)}</span>
    <span class="tab-close-btn" title="Close Tab">&times;</span>
  `;

  tab.addEventListener('click', (e) => {
    if (e.target.classList.contains('tab-close-btn')) {
      e.stopPropagation();
      closeSessionTab(session.id);
      return;
    }
    switchSessionTab(session.id);
  });

  container.appendChild(tab);
}

function switchSessionTab(sessionId) {
  if (sessionId === 'home') {
    switchToHomeTab();
    return;
  }

  activeSessionId = sessionId;
  const session = sessions.get(sessionId);
  if (!session) return;

  // Deactivate home tab and all tabs
  document.querySelectorAll('.session-tab').forEach(t => t.classList.remove('active'));
  const activeTab = document.getElementById(`tab-${sessionId}`);
  if (activeTab) activeTab.classList.add('active');

  // Hide vault view, show toolbar
  const vaultView = document.getElementById('vault-view');
  if (vaultView) vaultView.classList.add('hidden');
  document.getElementById('session-toolbar').classList.remove('hidden');

  // Update panes active state
  document.querySelectorAll('.terminal-pane').forEach(p => p.classList.remove('active'));
  if (session.element) session.element.classList.add('active');

  // Synchronize command history sidebar for this session's vault host
  applyHistorySidebarVisibility();
  renderCommandHistorySidebar();

  // Immediate responsive fit and focus
  requestAnimationFrame(() => {
    fitActiveTerminalSession();
    if (session.terminal) session.terminal.focus();
  });

  updateSessionToolbar(session);
}

function switchToHomeTab() {
  activeSessionId = 'home';
  document.querySelectorAll('.session-tab').forEach(t => t.classList.remove('active'));
  const homeTab = document.getElementById('tab-home');
  if (homeTab) homeTab.classList.add('active');

  // Show vault view, hide toolbar and panes
  const vaultView = document.getElementById('vault-view');
  if (vaultView) vaultView.classList.remove('hidden');
  document.getElementById('session-toolbar').classList.add('hidden');

  // Hide history sidebar when on home vault view
  const histSidebar = document.getElementById('cmd-history-sidebar');
  if (histSidebar) histSidebar.classList.add('hidden');
  const termContainer = document.getElementById('terminal-container');
  if (termContainer) termContainer.classList.remove('has-history-sidebar');

  document.querySelectorAll('.terminal-pane').forEach(p => p.classList.remove('active'));

  renderVaultStats();
  renderVaultHosts();
}

function closeSessionTab(sessionId) {
  const session = sessions.get(sessionId);
  if (!session) return;

  if (session.keepaliveTimer) clearInterval(session.keepaliveTimer);
  if (session.unlisten) session.unlisten();
  invokeTauri('disconnect_ssh', { sessionId });

  if (session.terminal) session.terminal.dispose();
  if (session.element) session.element.remove();

  const tab = document.getElementById(`tab-${sessionId}`);
  if (tab) tab.remove();

  sessions.delete(sessionId);

  // Switch to another open tab or to Vault home tab!
  const remaining = Array.from(sessions.keys());
  if (remaining.length > 0) {
    switchSessionTab(remaining[remaining.length - 1]);
  } else {
    switchToHomeTab();
  }

  renderHostList();
  renderVaultStats();
  renderVaultHosts();
}

function updateSessionToolbar(session) {
  const dot = document.getElementById('tb-status-dot');
  const text = document.getElementById('tb-status-text');
  const pill = document.getElementById('tb-status-pill');
  const hostLabel = document.getElementById('tb-host-label');
  const keepaliveLabel = document.getElementById('tb-heartbeat-label');
  const persistentLabel = document.getElementById('tb-persistent-label');
  const persistentEl = document.getElementById('tb-persistent');

  const status = session.status || 'connecting';
  if (dot) dot.className = `status-dot ${status}`;
  if (text) text.textContent = status.toUpperCase();
  if (pill) {
    pill.classList.remove('connected', 'connecting', 'reconnecting', 'failed', 'disconnected');
    pill.classList.add(status);
  }
  if (hostLabel) hostLabel.textContent = `${session.host.username}@${session.host.hostname}:${session.host.port}`;
  if (keepaliveLabel) keepaliveLabel.textContent = `Keepalive: ${session.keepaliveIntervalSec}s`;

  if (persistentLabel && persistentEl) {
    if (session.host.persistent_session !== false) {
      persistentLabel.textContent = `Auto-Resume: ON (${session.host.session_name || 'rustterm'})`;
      persistentEl.classList.remove('hidden');
    } else {
      persistentEl.classList.add('hidden');
    }
  }

  updateSessionTabStatus(session);
}


function adjustFontSize(delta) {
  currentFontSize = Math.max(10, Math.min(26, currentFontSize + delta));
  document.getElementById('tb-font-size-label').textContent = `${currentFontSize}px`;
  sessions.forEach(s => {
    if (s.terminal) {
      s.terminal.options.fontSize = currentFontSize;
    }
  });
  requestAnimationFrame(() => {
    fitActiveTerminalSession();
  });
}

// =========================================================
// Quick Connect Modal & Instant Execution
// =========================================================
function openQuickConnectModal() {
  const modal = document.getElementById('modal-quick-connect');
  if (!modal) return;
  modal.classList.remove('hidden');
  const input = document.getElementById('quick-ssh-input');
  if (input) {
    input.focus();
    input.select();
  }
}

function closeQuickConnectModal() {
  const modal = document.getElementById('modal-quick-connect');
  if (modal) modal.classList.add('hidden');
}

function handleQuickTargetInput(e) {
  let val = e.target.value.trim();
  // Strip optional 'ssh ' prefix if user pasted 'ssh user@host'
  if (val.toLowerCase().startsWith('ssh ')) {
    val = val.substring(4).trim();
  }

  let username = 'root';
  let hostname = '';
  let port = 22;

  if (val.includes('@')) {
    const parts = val.split('@');
    username = parts[0] || 'root';
    val = parts[1] || '';
  }

  if (val.includes(':')) {
    const parts = val.split(':');
    hostname = parts[0];
    port = parseInt(parts[1], 10) || 22;
  } else {
    hostname = val;
  }

  const uInput = document.getElementById('quick-username');
  const hInput = document.getElementById('quick-hostname');
  const pInput = document.getElementById('quick-port');

  if (uInput && username) uInput.value = username;
  if (hInput && hostname) hInput.value = hostname;
  if (pInput && port) pInput.value = port;
}

function handleQuickConnectSubmit(e) {
  e.preventDefault();
  let hostname = document.getElementById('quick-hostname').value.trim();
  let port = parseInt(document.getElementById('quick-port').value, 10) || 22;
  let username = document.getElementById('quick-username').value.trim();
  const rawTarget = document.getElementById('quick-ssh-input').value.trim();

  // Fallback parse if user pressed Enter directly on the target input
  if ((!hostname || !username) && rawTarget) {
    let clean = rawTarget.toLowerCase().startsWith('ssh ') ? rawTarget.substring(4).trim() : rawTarget;
    if (clean.includes('@')) {
      const p = clean.split('@');
      username = p[0] || username || 'root';
      clean = p[1] || '';
    }
    if (clean.includes(':')) {
      const p = clean.split(':');
      hostname = p[0] || hostname;
      port = parseInt(p[1], 10) || port || 22;
    } else if (clean) {
      hostname = clean;
    }
  }

  if (!hostname) {
    showToast('Please enter an SSH target host or IP address!', 'error');
    return;
  }
  if (!username) username = 'root';

  const password = document.getElementById('quick-password').value;
  const privateKey = document.getElementById('quick-private-key').value.trim();
  const saveVault = document.getElementById('quick-save-vault').checked;

  let authMode = 'password';
  if (password && privateKey) {
    authMode = 'both';
  } else if (privateKey) {
    authMode = 'private_key';
  }

  const quickHost = {
    id: `host-quick-${Date.now()}`,
    label: `${username}@${hostname}`,
    group: 'Quick Connect',
    hostname,
    port,
    username,
    auth_mode: authMode,
    password,
    private_key: privateKey,
    passphrase: '',
    keepalive_interval: 30,
    keepalive_max: 3,
    auto_reconnect: true,
    persistent_session: true,
    session_name: 'rustterm',
    tags: ['QuickConnect'],
    is_favorite: false
  };

  if (saveVault) {
    hosts.push(quickHost);
    saveHostsToStorage();
    renderTagFilters();
    renderVaultTagFilters();
    renderVaultStats();
    renderVaultHosts();
    showToast(`Saved to Vault: '${quickHost.label}'`, 'success');
  }

  closeQuickConnectModal();
  showToast(`⚡ Connecting to ${username}@${hostname}:${port}...`, 'info');
  launchSSHSession(quickHost, { mode: 'new' });
}

// =========================================================
// Export / Import (Including Private Keys!)
// =========================================================
let lastSavedBackupPath = '';

function openExportModal() {
  generateExportJSON();
  const card = document.getElementById('export-result-card');
  if (card) card.classList.add('hidden');
  document.getElementById('modal-export').classList.remove('hidden');
}

function closeExportModal() {
  document.getElementById('modal-export').classList.add('hidden');
}

function generateExportJSON() {
  const includeKeys = document.getElementById('chk-include-keys').checked;
  const exportPayload = {
    app: 'RustTerm',
    version: '1.0.0',
    exported_at: new Date().toISOString(),
    security_notice: includeKeys
      ? 'CONTAINS SENSITIVE PRIVATE KEYS & PASSWORDS. STORE SECURELY.'
      : 'Keys and passwords stripped.',
    hosts: hosts.map(h => {
      const clone = { ...h };
      if (!includeKeys) {
        delete clone.password;
        delete clone.private_key;
        delete clone.passphrase;
      }
      return clone;
    })
  };

  const jsonStr = JSON.stringify(exportPayload, null, 2);
  document.getElementById('export-json-area').value = jsonStr;
}

function copyExportJSON() {
  const area = document.getElementById('export-json-area');
  navigator.clipboard.writeText(area.value);
  showToast('Export JSON copied to clipboard!', 'success');
}

async function downloadExportJSON() {
  const jsonStr = document.getElementById('export-json-area').value;
  if (!jsonStr) return;

  try {
    const savedPath = await invokeTauri('save_backup_file', { jsonData: jsonStr });
    lastSavedBackupPath = savedPath;

    const card = document.getElementById('export-result-card');
    const pathVal = document.getElementById('export-result-path');
    if (pathVal) pathVal.textContent = savedPath;
    if (card) card.classList.remove('hidden');

    showToast('Backup successfully saved!', 'success');
  } catch (err) {
    console.warn('Native backup save error, falling back to browser download:', err);
    const blob = new Blob([jsonStr], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `rustterm_hosts_backup_${new Date().toISOString().slice(0, 10)}.json`;
    a.click();
    URL.revokeObjectURL(url);
    showToast('Backup file downloaded via browser', 'success');
  }
}


function handleImportFileSelect(e) {
  const file = e.target.files[0];
  if (file) {
    const reader = new FileReader();
    reader.onload = (event) => {
      document.getElementById('import-json-area').value = event.target.result;
    };
    reader.readAsText(file);
  }
}

function parseCsvHosts(csvText) {
  const rows = [];
  let row = [];
  let field = '';
  let inQuotes = false;
  for (let i = 0; i < csvText.length; i++) {
    const c = csvText[i];
    if (c === '"') {
      if (inQuotes && csvText[i + 1] === '"') {
        field += '"';
        i++;
      } else {
        inQuotes = !inQuotes;
      }
    } else if (c === ',' && !inQuotes) {
      row.push(field);
      field = '';
    } else if ((c === '\r' || c === '\n') && !inQuotes) {
      if (c === '\r' && csvText[i + 1] === '\n') i++;
      row.push(field);
      rows.push(row);
      row = [];
      field = '';
    } else {
      field += c;
    }
  }
  if (field || row.length) {
    row.push(field);
    rows.push(row);
  }
  if (rows.length <= 1) return [];

  const headers = rows[0].map(h => (h || '').trim());
  const idxGroups = headers.findIndex(h => /groups/i.test(h));
  const idxLabel = headers.findIndex(h => /label/i.test(h));
  const idxTags = headers.findIndex(h => /tags/i.test(h));
  const idxHost = headers.findIndex(h => /hostname|host/i.test(h));
  const idxPort = headers.findIndex(h => /port/i.test(h));
  const idxUser = headers.findIndex(h => /username|user/i.test(h));
  const idxPass = headers.findIndex(h => /password|pass/i.test(h));
  const idxKey = headers.findIndex(h => /key/i.test(h));

  const list = [];
  for (let i = 1; i < rows.length; i++) {
    const r = rows[i];
    if (!r || r.length < 2) continue;
    const hostname = idxHost >= 0 ? (r[idxHost] || '').trim() : '';
    const username = idxUser >= 0 ? (r[idxUser] || '').trim() : '';
    if (!hostname || !username) continue;

    const label = idxLabel >= 0 ? (r[idxLabel] || '').trim() : `${username}@${hostname}`;
    const group = idxGroups >= 0 ? (r[idxGroups] || '').trim() : '';
    const port = idxPort >= 0 ? parseInt(r[idxPort], 10) || 22 : 22;
    const password = idxPass >= 0 ? (r[idxPass] || '').trim() : '';
    const keyRef = idxKey >= 0 ? (r[idxKey] || '').trim() : '';
    const rawTags = idxTags >= 0 ? (r[idxTags] || '').split(/[;,]/).map(t => t.trim()).filter(Boolean) : [];

    let auth_mode = 'password';
    if (keyRef && password) auth_mode = 'both';
    else if (keyRef) auth_mode = 'private_key';

    list.push({
      label,
      group,
      hostname,
      port,
      username,
      auth_mode,
      password,
      private_key: '',
      passphrase: '',
      tags: rawTags,
      is_favorite: false
    });
  }
  return list;
}

function handleExecuteImport() {
  const text = document.getElementById('import-json-area').value.trim();
  if (!text) {
    showToast('Please provide JSON backup or CSV content', 'error');
    return;
  }

  try {
    let importedList = [];
    const trimmed = text.trim();

    if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
      const parsed = JSON.parse(trimmed);
      if (Array.isArray(parsed)) {
        importedList = parsed;
      } else if (parsed && Array.isArray(parsed.hosts)) {
        importedList = parsed.hosts;
      } else {
        throw new Error("Invalid structure. Expected an array of hosts or an object with 'hosts' array.");
      }
    } else {
      importedList = parseCsvHosts(trimmed);
      if (importedList.length === 0) {
        throw new Error("Invalid file content. Expected JSON backup or Termius CSV format.");
      }
    }

    let count = 0;
    importedList.forEach(item => {
      if (item.hostname && item.username) {
        const id = item.id || `host-${Date.now()}-${Math.random().toString(36).substr(2, 5)}`;
        const existingIdx = hosts.findIndex(h => h.id === id || (h.hostname === item.hostname && h.username === item.username && h.port === item.port));

        const hostData = {
          id,
          label: item.label || `${item.username}@${item.hostname}`,
          group: item.group || '',
          hostname: item.hostname,
          port: item.port || 22,
          username: item.username,
          auth_mode: item.auth_mode || (item.private_key ? 'private_key' : 'password'),
          password: item.password || '',
          private_key: item.private_key || '',
          passphrase: item.passphrase || '',
          keepalive_interval: item.keepalive_interval || 30,
          keepalive_max: item.keepalive_max || 3,
          auto_reconnect: item.auto_reconnect !== false,
          persistent_session: item.persistent_session !== false,
          session_name: item.session_name || 'rustterm',
          tags: Array.isArray(item.tags) ? item.tags : [],
          is_favorite: item.is_favorite === true
        };

        if (existingIdx >= 0) {
          hosts[existingIdx] = hostData;
        } else {
          hosts.push(hostData);
        }
        count++;
      }
    });

    saveHostsToStorage();
    renderTagFilters();
    renderHostList();
    renderVaultTagFilters();
    renderVaultStats();
    renderVaultHosts();
    closeExportModal();
    showToast(`Successfully imported ${count} hosts (including keys)!`, 'success');
  } catch (err) {
    showToast(`Import error: ${err.message}`, 'error');
  }
}

// =========================================================
// SFTP File & Folder Manager Controller
// =========================================================
let currentSftpSessionId = null;
let currentSftpHost = null;
let currentSftpPath = '/';
let currentSftpParentPath = null;
let defaultLocalDownloadDir = '';
let lastDownloadedLocalPath = '';

async function openSftpModal(host) {
  currentSftpHost = host;
  currentSftpSessionId = `sftp-${host.id}-${Date.now()}`;

  const modal = document.getElementById('modal-sftp');
  document.getElementById('sftp-modal-host-label').textContent = `${host.username}@${host.hostname}:${host.port}`;
  const badge = document.getElementById('sftp-status-badge');
  badge.className = 'status-badge-small online';
  badge.textContent = 'Connecting...';
  document.getElementById('sftp-transfer-alert').classList.add('hidden');
  document.getElementById('sftp-btn-reveal-file').classList.add('hidden');

  // Load default transfers directory
  try {
    defaultLocalDownloadDir = await invokeTauri('sftp_get_default_transfers_dir', { hostLabel: host.label || host.hostname });
    document.getElementById('sftp-local-dest-display').textContent = defaultLocalDownloadDir;
  } catch (e) {
    console.warn('Could not get default transfer dir:', e);
  }

  modal.classList.remove('hidden');

  // Connect SFTP
  try {
    const initialDir = await invokeTauri('sftp_connect', {
      sftpSessionId: currentSftpSessionId,
      hostConfig: host
    });
    badge.textContent = 'Connected';
    currentSftpPath = initialDir || '/';
    await sftpLoadDirectory(currentSftpPath);
  } catch (err) {
    badge.className = 'status-badge-small';
    badge.style.background = 'rgba(239, 68, 68, 0.2)';
    badge.style.color = '#f87171';
    badge.textContent = 'Failed';
    const tbody = document.getElementById('sftp-file-tbody');
    tbody.innerHTML = `<tr><td colspan="4" style="text-align: center; padding: 24px; color: #f87171;">Connection failed: ${escapeHtml(err.message || err)}</td></tr>`;
    showToast(`SFTP Connection error: ${err.message || err}`, 'error');
  }
}

async function closeSftpModal() {
  document.getElementById('modal-sftp').classList.add('hidden');
  if (currentSftpSessionId) {
    invokeTauri('sftp_disconnect', { sftpSessionId: currentSftpSessionId }).catch(() => { });
    currentSftpSessionId = null;
  }
}

async function sftpLoadDirectory(targetPath = null) {
  if (!currentSftpSessionId) return;
  const tbody = document.getElementById('sftp-file-tbody');
  const emptyHint = document.getElementById('sftp-empty-hint');
  emptyHint.classList.add('hidden');
  tbody.innerHTML = `<tr><td colspan="4" style="text-align: center; padding: 24px; color: var(--text-dim);"><div class="transfer-spinner" style="margin: 0 auto 8px auto;"></div>Loading remote files...</td></tr>`;

  try {
    const res = await invokeTauri('sftp_list_dir', {
      sftpSessionId: currentSftpSessionId,
      targetPath: targetPath || currentSftpPath
    });

    currentSftpPath = res.current_path;
    currentSftpParentPath = res.parent_path;
    document.getElementById('sftp-path-input').value = currentSftpPath;

    renderSftpEntries(res.entries);
  } catch (err) {
    tbody.innerHTML = `<tr><td colspan="4" style="text-align: center; padding: 24px; color: #f87171;">Failed to read directory: ${escapeHtml(err.message || err)}</td></tr>`;
    showToast(`SFTP listing failed: ${err.message || err}`, 'error');
  }
}

function renderSftpEntries(entries) {
  const tbody = document.getElementById('sftp-file-tbody');
  const emptyHint = document.getElementById('sftp-empty-hint');
  const statsEl = document.getElementById('sftp-stats-text');
  tbody.innerHTML = '';

  if (!entries || entries.length === 0) {
    emptyHint.classList.remove('hidden');
    statsEl.textContent = '0 items';
    return;
  }

  emptyHint.classList.add('hidden');
  const dirCount = entries.filter(e => e.is_dir).length;
  const fileCount = entries.filter(e => !e.is_dir).length;
  statsEl.textContent = `${entries.length} items (${dirCount} folders, ${fileCount} files)`;

  entries.forEach(entry => {
    const tr = document.createElement('tr');
    tr.className = entry.is_dir ? 'clickable' : '';

    let icon = '📄';
    if (entry.is_dir) {
      icon = '📁';
    } else if (entry.name.endsWith('.zip') || entry.name.endsWith('.tar') || entry.name.endsWith('.gz')) {
      icon = '📦';
    } else if (entry.name.endsWith('.js') || entry.name.endsWith('.rs') || entry.name.endsWith('.py') || entry.name.endsWith('.json') || entry.name.endsWith('.sh')) {
      icon = '📝';
    } else if (entry.name.endsWith('.png') || entry.name.endsWith('.jpg') || entry.name.endsWith('.svg')) {
      icon = '🖼️';
    }

    const sizeFormatted = entry.is_dir ? '—' : formatFileSize(entry.size);
    const dateFormatted = entry.modified ? formatDate(entry.modified) : '—';

    tr.innerHTML = `
      <td>
        <div class="file-name-cell">
          <span class="file-icon">${icon}</span>
          <span class="file-name-text">${escapeHtml(entry.name)}</span>
        </div>
      </td>
      <td class="col-size">${sizeFormatted}</td>
      <td class="col-modified">${dateFormatted}</td>
      <td class="col-actions">
        <div class="sftp-row-actions">
          ${entry.is_dir
        ? `<button class="btn-table-action download-btn btn-dl-dir" title="Download entire folder and contents from server">
                <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path><polyline points="7 10 12 15 17 10"></polyline><line x1="12" y1="15" x2="12" y2="3"></line></svg>
                <span>Download Folder</span>
               </button>`
        : `<button class="btn-table-action download-btn btn-dl-file" title="Download file from server">
                <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path><polyline points="7 10 12 15 17 10"></polyline><line x1="12" y1="15" x2="12" y2="3"></line></svg>
                <span>Download File</span>
               </button>`
      }
          <button class="btn-table-action delete-btn btn-del-item" title="Delete from server">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="3 6 5 6 21 6"></polyline><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path></svg>
          </button>
        </div>
      </td>
    `;

    // Folder navigation on row click (if not clicking action button)
    if (entry.is_dir) {
      tr.addEventListener('click', (e) => {
        if (e.target.closest('.btn-table-action')) return;
        sftpLoadDirectory(entry.path);
      });

      const dlDirBtn = tr.querySelector('.btn-dl-dir');
      if (dlDirBtn) {
        dlDirBtn.addEventListener('click', (e) => {
          e.stopPropagation();
          sftpDownloadFolder(entry.path, entry.name);
        });
      }
    } else {
      const dlFileBtn = tr.querySelector('.btn-dl-file');
      if (dlFileBtn) {
        dlFileBtn.addEventListener('click', (e) => {
          e.stopPropagation();
          sftpDownloadFile(entry.path, entry.name);
        });
      }
    }

    const delBtn = tr.querySelector('.btn-del-item');
    if (delBtn) {
      delBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        sftpDeleteItem(entry.path, entry.is_dir, entry.name);
      });
    }

    tbody.appendChild(tr);
  });
}

async function sftpDownloadFile(remotePath, fileName) {
  if (!currentSftpSessionId) return;
  const alertEl = document.getElementById('sftp-transfer-alert');
  const alertText = document.getElementById('sftp-transfer-text');
  const spinner = document.getElementById('sftp-transfer-spinner');
  const revealBtn = document.getElementById('sftp-btn-reveal-file');

  alertEl.classList.remove('hidden');
  spinner.classList.remove('hidden');
  revealBtn.classList.add('hidden');
  alertText.textContent = `Downloading '${fileName}' from server...`;

  try {
    const res = await invokeTauri('sftp_download_file', {
      sftpSessionId: currentSftpSessionId,
      remotePath: remotePath,
      localPath: null
    });

    lastDownloadedLocalPath = res.local_path;
    spinner.classList.add('hidden');
    alertText.textContent = `✔ Downloaded '${fileName}' (${formatFileSize(res.bytes_transferred)}) to ${res.local_path}`;
    revealBtn.classList.remove('hidden');
    showToast(`Downloaded '${fileName}'`, 'success');
  } catch (err) {
    spinner.classList.add('hidden');
    alertText.textContent = `❌ Download failed: ${err.message || err}`;
    showToast(`Download failed: ${err.message || err}`, 'error');
  }
}

async function sftpDownloadFolder(remoteDir, folderName) {
  if (!currentSftpSessionId) return;
  const alertEl = document.getElementById('sftp-transfer-alert');
  const alertText = document.getElementById('sftp-transfer-text');
  const spinner = document.getElementById('sftp-transfer-spinner');
  const revealBtn = document.getElementById('sftp-btn-reveal-file');

  alertEl.classList.remove('hidden');
  spinner.classList.remove('hidden');
  revealBtn.classList.add('hidden');
  alertText.textContent = `Downloading folder '${folderName}' and all contents from server...`;

  try {
    const res = await invokeTauri('sftp_download_dir', {
      sftpSessionId: currentSftpSessionId,
      remoteDir: remoteDir,
      localDir: null
    });

    lastDownloadedLocalPath = res.local_dir;
    spinner.classList.add('hidden');
    alertText.textContent = `✔ Downloaded folder '${folderName}' (${res.total_files} files, ${formatFileSize(res.total_bytes)}) to ${res.local_dir}`;
    revealBtn.classList.remove('hidden');
    showToast(`Folder '${folderName}' downloaded successfully!`, 'success');
  } catch (err) {
    spinner.classList.add('hidden');
    alertText.textContent = `❌ Folder download failed: ${err.message || err}`;
    showToast(`Folder download failed: ${err.message || err}`, 'error');
  }
}

async function handleSftpUploadSelect(e) {
  const file = e.target.files[0];
  if (!file || !currentSftpSessionId) return;

  const alertEl = document.getElementById('sftp-transfer-alert');
  const alertText = document.getElementById('sftp-transfer-text');
  const spinner = document.getElementById('sftp-transfer-spinner');
  alertEl.classList.remove('hidden');
  spinner.classList.remove('hidden');
  alertText.textContent = `Uploading '${file.name}' to server...`;

  try {
    const localFilePath = file.path || file.name;
    const res = await invokeTauri('sftp_upload_file', {
      sftpSessionId: currentSftpSessionId,
      localPath: localFilePath,
      remoteDir: currentSftpPath
    });

    spinner.classList.add('hidden');
    alertText.textContent = `✔ Uploaded '${file.name}' (${formatFileSize(res.bytes_transferred)}) to server`;
    showToast(`Uploaded '${file.name}'`, 'success');
    await sftpLoadDirectory(currentSftpPath);
  } catch (err) {
    spinner.classList.add('hidden');
    alertText.textContent = `❌ Upload failed: ${err.message || err}`;
    showToast(`Upload failed: ${err.message || err}`, 'error');
  }
  e.target.value = '';
}

async function sftpCreateFolder() {
  if (!currentSftpSessionId) return;
  const folderName = prompt('Enter name for new remote directory:');
  if (!folderName || !folderName.trim()) return;

  try {
    await invokeTauri('sftp_create_dir', {
      sftpSessionId: currentSftpSessionId,
      parentDir: currentSftpPath,
      dirName: folderName.trim()
    });
    showToast(`Created folder '${folderName.trim()}'`, 'success');
    await sftpLoadDirectory(currentSftpPath);
  } catch (err) {
    showToast(`Failed to create folder: ${err.message || err}`, 'error');
  }
}

async function sftpDeleteItem(itemPath, isDir, name) {
  if (!currentSftpSessionId) return;
  const itemType = isDir ? 'directory' : 'file';
  if (!confirm(`Are you sure you want to delete remote ${itemType} '${name}'?`)) return;

  try {
    await invokeTauri('sftp_delete_item', {
      sftpSessionId: currentSftpSessionId,
      remotePath: itemPath,
      isDir: isDir
    });
    showToast(`Deleted '${name}'`, 'info');
    await sftpLoadDirectory(currentSftpPath);
  } catch (err) {
    showToast(`Delete failed: ${err.message || err}`, 'error');
  }
}

async function openExplorerToFolder(path) {
  try {
    await invokeTauri('sftp_open_in_explorer', { folderPath: path });
  } catch (e) {
    console.warn('Could not launch explorer:', e);
  }
}

function formatFileSize(bytes) {
  if (!bytes || bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + ' ' + sizes[i];
}

function formatDate(epochSec) {
  if (!epochSec) return '—';
  const d = new Date(epochSec * 1000);
  return d.toLocaleDateString(undefined, {
    month: 'short',
    day: 'numeric',
    year: 'numeric',
    hour: '2-digit',
    minute: '2-digit'
  });
}

// =========================================================
// Mock Handler (For Browser Dev & Fallback)
// =========================================================
function mockTauriHandler(cmd, args) {
  if (cmd === 'connect_ssh') {
    const s = sessions.get(args.sessionId);
    if (s && s.terminal) {
      setTimeout(() => {
        s.terminal.writeln('\x1b[32m✔ Authenticated using ' + (args.hostConfig.auth_mode || 'password').toUpperCase() + '\x1b[0m');
        s.terminal.writeln('\x1b[36m✔ Keepalive loop started (Interval: ' + (args.hostConfig.keepalive_interval || 30) + 's)\x1b[0m');
        if (args.hostConfig.persistent_session !== false) {
          s.terminal.writeln('\x1b[35m⚡ Persistent Session active (' + (args.hostConfig.session_name || 'rustterm') + ')\x1b[0m');
        }
        s.terminal.writeln('\r\nWelcome to Ubuntu 24.04 LTS (GNU/Linux 6.8.0 x86_64)');
        s.terminal.writeln(' * Documentation:  https://help.ubuntu.com');
        s.terminal.writeln(' * Management:     https://landscape.canonical.com\r\n');
        s.terminal.write(`${args.hostConfig.username}@${args.hostConfig.hostname}:~$ `);
      }, 400);
    }
    return { success: true };
  }
  if (cmd === 'send_ssh_data') {
    const s = sessions.get(args.sessionId);
    if (s && s.terminal) {
      if (args.data === '\r') {
        s.terminal.write('\r\n' + `${s.host.username}@${s.host.hostname}:~$ `);
      } else if (args.data === '\u007F') {
        s.terminal.write('\b \b');
      } else {
        s.terminal.write(args.data);
      }
    }
    return { success: true };
  }
  if (cmd === 'send_ssh_keepalive') {
    return { success: true };
  }
  if (cmd === 'sftp_get_default_transfers_dir') {
    return '~/Downloads/RustTerm_Transfers/' + (args.hostLabel || 'server');
  }
  if (cmd === 'sftp_connect') {
    return '/home/' + (args.hostConfig.username || 'ubuntu');
  }
  if (cmd === 'sftp_list_dir') {
    const p = args.targetPath || '/home/ubuntu';
    return {
      current_path: p,
      parent_path: p === '/' ? null : '/',
      entries: [
        { name: 'projects', path: p + '/projects', is_dir: true, is_file: false, is_symlink: false, size: 4096, modified: Math.floor(Date.now() / 1000) - 3600 },
        { name: 'logs', path: p + '/logs', is_dir: true, is_file: false, is_symlink: false, size: 4096, modified: Math.floor(Date.now() / 1000) - 7200 },
        { name: 'app_backup.tar.gz', path: p + '/app_backup.tar.gz', is_dir: false, is_file: true, is_symlink: false, size: 14285714, modified: Math.floor(Date.now() / 1000) - 86400 },
        { name: 'server_config.yaml', path: p + '/server_config.yaml', is_dir: false, is_file: true, is_symlink: false, size: 2450, modified: Math.floor(Date.now() / 1000) - 1200 },
        { name: 'deploy.sh', path: p + '/deploy.sh', is_dir: false, is_file: true, is_symlink: false, size: 8520, modified: Math.floor(Date.now() / 1000) - 500 }
      ]
    };
  }
  if (cmd === 'sftp_download_file') {
    return {
      local_path: '~/Downloads/RustTerm_Transfers/' + args.remotePath.split('/').pop(),
      bytes_transferred: 14285714
    };
  }
  if (cmd === 'sftp_download_dir') {
    return {
      local_dir: '~/Downloads/RustTerm_Transfers/' + args.remoteDir.split('/').pop(),
      total_files: 38,
      total_bytes: 42500000
    };
  }
  if (cmd === 'sftp_upload_file') {
    return {
      remote_path: args.remoteDir + '/uploaded_file',
      bytes_transferred: 51200
    };
  }
  if (cmd === 'save_backup_file') {
    return '~/Downloads/RustTerm_Backups/rustterm_backup_' + new Date().toISOString().slice(0, 10) + '.json';
  }
  if (cmd === 'sftp_create_dir' || cmd === 'sftp_delete_item' || cmd === 'sftp_open_in_explorer') {
    return true;
  }
  return { ok: true };
}



// =========================================================
// UI Helpers
// =========================================================
function showToast(message, type = 'info') {
  const container = document.getElementById('toast-container');
  const toast = document.createElement('div');
  toast.className = `toast ${type}`;
  toast.textContent = message;
  container.appendChild(toast);
  setTimeout(() => toast.remove(), 3200);
}

function escapeHtml(str) {
  if (!str) return '';
  return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

// =========================================================
// Per-Vault Command History System
// =========================================================
function getVaultHistoryKey(host) {
  if (!host) return 'unknown_vault';
  if (host.id) return `vault_${host.id}`;
  return `vault_${host.username || 'root'}@${host.hostname || 'localhost'}:${host.port || 22}`;
}

function loadVaultCommandHistory(vaultKey) {
  try {
    const all = JSON.parse(localStorage.getItem('rustterm_vault_cmd_histories') || '{}');
    return Array.isArray(all[vaultKey]) ? all[vaultKey] : [];
  } catch (e) {
    return [];
  }
}

function saveVaultCommandHistory(vaultKey, items) {
  try {
    const all = JSON.parse(localStorage.getItem('rustterm_vault_cmd_histories') || '{}');
    all[vaultKey] = items;
    localStorage.setItem('rustterm_vault_cmd_histories', JSON.stringify(all));
  } catch (e) {
    console.error('Failed to save vault command history:', e);
  }
}

function addCommandToHistory(vaultKey, rawCmd) {
  if (!rawCmd) return;
  const command = rawCmd.trim();
  if (!command || command.length < 1) return;

  let items = loadVaultCommandHistory(vaultKey);

  // Check if exact command already exists
  const existingIdx = items.findIndex(it => it.command === command);
  let isFav = false;
  if (existingIdx !== -1) {
    isFav = items[existingIdx].isFavorite || false;
    items.splice(existingIdx, 1);
  }

  // Prepend to top
  items.unshift({
    id: `cmd_${Date.now()}_${Math.random().toString(36).substring(2, 7)}`,
    command,
    timestamp: Date.now(),
    isFavorite: isFav
  });

  // Keep up to 200 commands per vault
  if (items.length > 200) {
    items = items.slice(0, 200);
  }

  saveVaultCommandHistory(vaultKey, items);

  // If this vault is currently active in view, re-render sidebar
  if (activeSessionId && activeSessionId !== 'home') {
    const activeSession = sessions.get(activeSessionId);
    if (activeSession && getVaultHistoryKey(activeSession.host) === vaultKey) {
      renderCommandHistorySidebar();
    }
  }
}

function handleSessionTerminalInput(sessionId, data) {
  const session = sessions.get(sessionId);
  if (!session) return;
  if (session.inputBuffer === undefined) session.inputBuffer = '';

  const vaultKey = getVaultHistoryKey(session.host);

  // Check for Enter key (\r or \n)
  if (data.includes('\r') || data.includes('\n')) {
    const parts = data.split(/[\r\n]+/);
    const firstPart = parts[0];
    const fullCmd = (session.inputBuffer + firstPart).trim();
    if (fullCmd) {
      addCommandToHistory(vaultKey, fullCmd);
    }
    session.inputBuffer = parts.length > 1 ? parts[parts.length - 1] : '';
    return;
  }

  // Backspace (\x7f or \x08)
  if (data === '\x7f' || data === '\x08') {
    if (session.inputBuffer.length > 0) {
      session.inputBuffer = session.inputBuffer.slice(0, -1);
    }
    return;
  }

  // Ctrl+C (\x03) or Ctrl+U (\x15)
  if (data === '\x03' || data === '\x15') {
    session.inputBuffer = '';
    return;
  }

  // Ctrl+W (\x17)
  if (data === '\x17') {
    session.inputBuffer = session.inputBuffer.replace(/\s*\S+$/, '');
    return;
  }

  // Ignore terminal escape sequences (arrows, home, end, etc.)
  if (data.startsWith('\x1b')) {
    return;
  }

  // Regular input or pasted text
  session.inputBuffer += data;
}

function applyHistorySidebarVisibility() {
  const sidebar = document.getElementById('cmd-history-sidebar');
  const toggleBtn = document.getElementById('tb-btn-toggle-history');

  if (!sidebar) return;

  const shouldShow = isHistorySidebarOpen && activeSessionId && activeSessionId !== 'home';

  if (shouldShow) {
    sidebar.classList.remove('hidden');
    if (toggleBtn) toggleBtn.classList.add('active');
  } else {
    sidebar.classList.add('hidden');
    if (toggleBtn) toggleBtn.classList.remove('active');
  }

  // Refit terminal after layout width change
  requestAnimationFrame(() => {
    fitActiveTerminalSession();
  });
}

function toggleCommandHistorySidebar(forceState) {
  if (typeof forceState === 'boolean') {
    isHistorySidebarOpen = forceState;
  } else {
    isHistorySidebarOpen = !isHistorySidebarOpen;
  }

  localStorage.setItem('rustterm_history_sidebar_open', isHistorySidebarOpen);
  applyHistorySidebarVisibility();
  if (isHistorySidebarOpen) {
    renderCommandHistorySidebar();
  }
}

function renderCommandHistorySidebar() {
  const sidebar = document.getElementById('cmd-history-sidebar');
  if (!sidebar || sidebar.classList.contains('hidden')) return;

  if (!activeSessionId || activeSessionId === 'home') return;
  const session = sessions.get(activeSessionId);
  if (!session || !session.host) return;

  const vaultKey = getVaultHistoryKey(session.host);
  const badge = document.getElementById('cmd-hist-vault-badge');
  if (badge) {
    badge.textContent = session.host.label || `${session.host.username}@${session.host.hostname}`;
    badge.title = `History for vault host: ${session.host.label} (${session.host.hostname})`;
  }

  const items = loadVaultCommandHistory(vaultKey);

  // Update count badges
  const totalCount = items.length;
  const favCount = items.filter(it => it.isFavorite).length;

  const countAllEl = document.getElementById('cmd-count-all');
  if (countAllEl) countAllEl.textContent = totalCount;
  const countFavsEl = document.getElementById('cmd-count-favs');
  if (countFavsEl) countFavsEl.textContent = favCount;

  // Filter items
  let filtered = items;
  if (cmdHistoryFilter === 'favorites') {
    filtered = filtered.filter(it => it.isFavorite);
  }
  if (cmdHistorySearchQuery) {
    filtered = filtered.filter(it => it.command.toLowerCase().includes(cmdHistorySearchQuery));
  }

  const listContainer = document.getElementById('cmd-history-list');
  if (!listContainer) return;

  if (filtered.length === 0) {
    if (cmdHistorySearchQuery) {
      listContainer.innerHTML = `
        <div class="cmd-empty">
          <div class="cmd-empty-icon">🔍</div>
          <span class="cmd-empty-title">No Matching Commands</span>
          <span class="cmd-empty-desc">No commands match "${escapeHtml(cmdHistorySearchQuery)}".</span>
        </div>`;
    } else if (cmdHistoryFilter === 'favorites') {
      listContainer.innerHTML = `
        <div class="cmd-empty">
          <div class="cmd-empty-icon">⭐</div>
          <span class="cmd-empty-title">No Starred Commands</span>
          <span class="cmd-empty-desc">Click the star icon on any command to pin it to favorites for this host.</span>
        </div>`;
    } else {
      listContainer.innerHTML = `
        <div class="cmd-empty">
          <div class="cmd-empty-icon">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="4 17 10 11 4 5"></polyline><line x1="12" y1="19" x2="20" y2="19"></line></svg>
          </div>
          <span class="cmd-empty-title">No History Yet</span>
          <span class="cmd-empty-desc">Commands you execute in this SSH session will appear here automatically.</span>
        </div>`;
    }
    return;
  }

  listContainer.innerHTML = filtered.map(item => {
    const starIcon = item.isFavorite ? '⭐' : '☆';
    const favClass = item.isFavorite ? 'is-fav' : '';
    const favTitle = item.isFavorite ? 'Remove from favorites' : 'Add to favorites';
    const timeStr = formatRelativeTime(item.timestamp);

    return `
      <div class="cmd-item" data-cmd-id="${item.id}" title="Click to insert command into terminal (without running)">
        <div class="cmd-item-header">
          <span class="cmd-time">${timeStr}</span>
          <div class="cmd-item-actions">
            <button class="cmd-action-btn btn-cmd-fav ${favClass}" data-cmd-id="${item.id}" title="${favTitle}">
              ${starIcon}
            </button>
            <button class="cmd-action-btn btn-cmd-copy" data-cmd-id="${item.id}" title="Copy command to clipboard">
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path></svg>
            </button>
            <button class="cmd-action-btn btn-cmd-del" data-cmd-id="${item.id}" title="Delete command">
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>
            </button>
          </div>
        </div>
        <div class="cmd-text">${escapeHtml(item.command)}</div>
      </div>
    `;
  }).join('');

  // Attach event handlers
  listContainer.querySelectorAll('.cmd-item').forEach(itemEl => {
    const cmdId = itemEl.dataset.cmdId;
    const item = items.find(it => it.id === cmdId);
    if (!item) return;

    // Click whole item: insert into terminal (NO auto-enter)
    itemEl.addEventListener('click', (e) => {
      if (e.target.closest('.cmd-action-btn')) return;
      pasteCommandToTerminal(activeSessionId, item.command);
    });

    // Star / Favorite button
    const favBtn = itemEl.querySelector('.btn-cmd-fav');
    if (favBtn) {
      favBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        toggleCommandFavorite(vaultKey, cmdId);
      });
    }

    // Copy button
    const copyBtn = itemEl.querySelector('.btn-cmd-copy');
    if (copyBtn) {
      copyBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        copyCommandText(item.command);
      });
    }

    // Delete button
    const delBtn = itemEl.querySelector('.btn-cmd-del');
    if (delBtn) {
      delBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        deleteCommandFromHistory(vaultKey, cmdId);
      });
    }
  });
}

// =========================================================
// Terminal Clipboard Sanitizer & Clipboard Actions
// =========================================================

function cleanTerminalPasteText(text) {
  if (!text || typeof text !== 'string') return '';

  let cleaned = text;

  // 1. Remove bracketed paste escape sequences
  cleaned = cleaned.replace(/\x1b\[200~([\s\S]*?)\x1b\[201~/g, '$1');
  cleaned = cleaned.replace(/^\x1b\[200~/, '').replace(/\x1b\[201~$/, '');
  cleaned = cleaned.replace(/^(\^\[\[|\[)?(200|00)~/, '');
  cleaned = cleaned.replace(/(\^\[\[|\[)?(201|01)~$/, '');

  // 2. Remove UTF-8 Byte Order Mark (BOM) & zero-width / invisible Unicode characters
  cleaned = cleaned.replace(/[\uFEFF\u200B\u200C\u200D\u2060\u180E\u200E\u200F\u202A-\u202E\u2066-\u2069\u00AD]/g, '');

  // 3. Normalize non-breaking spaces to standard space
  cleaned = cleaned.replace(/[\u00A0\u202F\u3000]/g, ' ');

  // 4. Normalize smart quotes and unicode dashes that break shell commands
  cleaned = cleaned.replace(/[\u2018\u2019\u201A\u201B]/g, "'");
  cleaned = cleaned.replace(/[\u201C\u201D\u201E\u201F]/g, '"');
  cleaned = cleaned.replace(/[\u2014\u2015]/g, '--');
  cleaned = cleaned.replace(/[\u2012\u2013]/g, '-');

  // 5. Strip shell prompt prefixes copied from markdown / chat ($ , # , > , ❯ , ➜ , % , PS ...>)
  const lines = cleaned.split(/\r\n|\r|\n/);
  const cleanedLines = lines.map(line => {
    let l = line.replace(/^\s*PS\s+[A-Za-z0-9_:\\\/\.\-\(\) ]*>\s+/, '');
    l = l.replace(/^\s*(\$|#|❯|➜|%|>)\s+/, '');
    return l;
  });
  cleaned = cleanedLines.join('\r');

  // 6. Strip ASCII control characters except \t and \r
  cleaned = cleaned.replace(/[\x00-\x08\x0B\x0C\x0E-\x1F\x7F]/g, '');

  return cleaned;
}

function pasteTextToTerminal(sessionId, rawText, notify = false) {
  if (!sessionId || sessionId === 'home') return;
  const session = sessions.get(sessionId);
  if (!session) {
    showToast('No active terminal session', 'warning');
    return;
  }

  const cleaned = cleanTerminalPasteText(rawText);
  if (!cleaned) return;

  // Send cleaned text directly to remote SSH shell
  invokeTauri('send_ssh_data', { sessionId, data: cleaned });

  // Update session input buffer and history tracker
  handleSessionTerminalInput(sessionId, cleaned);

  if (session.terminal) {
    session.terminal.focus();
  }

  if (notify) {
    showToast('Pasted and cleaned text into terminal', 'info');
  }
}

async function handleTerminalPasteAction(sessionId) {
  if (!sessionId || sessionId === 'home') return;
  try {
    if (navigator.clipboard && navigator.clipboard.readText) {
      const text = await navigator.clipboard.readText();
      if (text) {
        pasteTextToTerminal(sessionId, text, true);
        return;
      }
    }
    showToast('Clipboard is empty or inaccessible', 'warning');
  } catch (err) {
    try {
      const ta = document.createElement('textarea');
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.focus();
      document.execCommand('paste');
      const val = ta.value;
      document.body.removeChild(ta);
      if (val) {
        pasteTextToTerminal(sessionId, val, true);
        return;
      }
    } catch (_) { }
    showToast('Unable to read clipboard: ' + (err.message || err), 'error');
  }
}

function pasteCommandToTerminal(sessionId, cmdText) {
  if (!sessionId || sessionId === 'home') return;
  const session = sessions.get(sessionId);
  if (!session) {
    showToast('No active terminal session', 'warning');
    return;
  }

  const cleaned = cleanTerminalPasteText(cmdText);
  if (!cleaned) return;

  // Send typed command to remote SSH shell without \r or \n
  invokeTauri('send_ssh_data', { sessionId, data: cleaned });

  // Update session input buffer
  session.inputBuffer = (session.inputBuffer || '') + cleaned;

  // Focus terminal so the cursor is immediately at the end of the command
  if (session.terminal) {
    session.terminal.focus();
  }
  showToast('Command inserted into terminal (press Enter to run)', 'info');
}

function copyToClipboard(text, successMsg = 'Copied to clipboard') {
  if (!text) return;
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text).then(() => {
      showToast(successMsg, 'success');
    }).catch(() => {
      fallbackCopy(text, successMsg);
    });
  } else {
    fallbackCopy(text, successMsg);
  }
}

function fallbackCopy(text, successMsg = 'Copied to clipboard') {
  try {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    document.execCommand('copy');
    document.body.removeChild(ta);
    showToast(successMsg, 'success');
  } catch (e) {
    showToast('Could not copy to clipboard', 'error');
  }
}

function copyCommandText(cmdText) {
  copyToClipboard(cmdText, 'Command copied to clipboard');
}

function copyTerminalSelection(session) {
  if (!session || !session.terminal) return;
  const selectedText = session.terminal.getSelection();
  if (selectedText) {
    copyToClipboard(selectedText, 'Selected text copied to clipboard');
  } else {
    showToast('No text selected in terminal', 'info');
  }
}

function openTerminalContextMenu(e, sessionId) {
  closeAllActionMenus();
  if (!sessionId || sessionId === 'home') return;
  const session = sessions.get(sessionId);
  if (!session || !session.terminal) return;

  const hasSelection = session.terminal.hasSelection();

  const menu = document.createElement('div');
  menu.className = 'action-submenu-dropdown';
  menu.id = 'active-terminal-context-menu';

  menu.innerHTML = `
    <div class="menu-item-group-title">SSH Terminal</div>
    <div class="menu-item ${hasSelection ? '' : 'disabled'}" data-action="copy" title="${hasSelection ? 'Copy selected text (Ctrl+Shift+C)' : 'No text selected'}">
      <div class="menu-item-icon">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="${hasSelection ? '#38bdf8' : '#64748b'}" stroke-width="2">
          <rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect>
          <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path>
        </svg>
      </div>
      <div class="menu-text-wrap">
        <span class="menu-item-text">Copy</span>
        <span class="menu-item-hint">Ctrl+Shift+C</span>
      </div>
    </div>
    <div class="menu-item" data-action="paste" title="Paste & clean command into terminal (Ctrl+V)">
      <div class="menu-item-icon">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#10b981" stroke-width="2">
          <path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"></path>
          <rect x="8" y="2" width="8" height="4" rx="1" ry="1"></rect>
        </svg>
      </div>
      <div class="menu-text-wrap">
        <span class="menu-item-text">Paste</span>
        <span class="menu-item-hint">Ctrl+V / Cleaned</span>
      </div>
    </div>
    <div class="menu-divider"></div>
    <div class="menu-item" data-action="select-all">
      <div class="menu-item-icon">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#a78bfa" stroke-width="2">
          <polyline points="9 11 12 14 22 4"></polyline>
          <path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"></path>
        </svg>
      </div>
      <div class="menu-text-wrap">
        <span class="menu-item-text">Select All</span>
        <span class="menu-item-hint">Select buffer text</span>
      </div>
    </div>
    <div class="menu-item" data-action="clear">
      <div class="menu-item-icon">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#f59e0b" stroke-width="2">
          <polyline points="3 6 5 6 21 6"></polyline>
          <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path>
        </svg>
      </div>
      <div class="menu-text-wrap">
        <span class="menu-item-text">Clear Terminal</span>
        <span class="menu-item-hint">Clear screen buffer</span>
      </div>
    </div>
  `;

  document.body.appendChild(menu);
  activeActionMenuElement = menu;

  const menuWidth = 220;
  const menuHeight = 170;

  let left = e.clientX;
  let top = e.clientY;

  if (left + menuWidth > window.innerWidth - 10) {
    left = window.innerWidth - menuWidth - 10;
  }
  if (top + menuHeight > window.innerHeight - 10) {
    top = e.clientY - menuHeight;
    if (top < 10) top = 10;
  }

  menu.style.position = 'fixed';
  menu.style.top = `${top}px`;
  menu.style.left = `${left}px`;
  menu.style.zIndex = '10000';

  menu.addEventListener('click', (ev) => {
    ev.stopPropagation();
    const item = ev.target.closest('.menu-item');
    if (!item) return;
    if (item.classList.contains('disabled')) return;

    const action = item.dataset.action;
    closeAllActionMenus();

    if (action === 'copy') {
      copyTerminalSelection(session);
      if (session.terminal) session.terminal.focus();
    } else if (action === 'paste') {
      handleTerminalPasteAction(sessionId);
    } else if (action === 'select-all') {
      if (session.terminal) {
        session.terminal.selectAll();
        session.terminal.focus();
        showToast('All buffer text selected', 'info');
      }
    } else if (action === 'clear') {
      if (session.terminal) {
        session.terminal.clear();
        session.terminal.focus();
        showToast('Terminal buffer cleared', 'info');
      }
    }
  });
}

function toggleCommandFavorite(vaultKey, cmdId) {
  const items = loadVaultCommandHistory(vaultKey);
  const it = items.find(c => c.id === cmdId);
  if (it) {
    it.isFavorite = !it.isFavorite;
    saveVaultCommandHistory(vaultKey, items);
    renderCommandHistorySidebar();
    showToast(it.isFavorite ? 'Command added to favorites' : 'Command removed from favorites', 'info');
  }
}

function deleteCommandFromHistory(vaultKey, cmdId) {
  let items = loadVaultCommandHistory(vaultKey);
  items = items.filter(c => c.id !== cmdId);
  saveVaultCommandHistory(vaultKey, items);
  renderCommandHistorySidebar();
  showToast('Command deleted from history', 'info');
}

function handleClearVaultHistoryClick() {
  if (!activeSessionId || activeSessionId === 'home') return;
  const session = sessions.get(activeSessionId);
  if (!session || !session.host) return;

  const vaultKey = getVaultHistoryKey(session.host);
  const label = session.host.label || session.host.hostname;

  if (confirm(`Clear all command history for "${label}"?`)) {
    saveVaultCommandHistory(vaultKey, []);
    renderCommandHistorySidebar();
    showToast(`Command history cleared for ${label}`, 'info');
  }
}

function formatRelativeTime(timestamp) {
  if (!timestamp) return '';
  const diffSec = Math.floor((Date.now() - timestamp) / 1000);
  if (diffSec < 15) return 'Just now';
  if (diffSec < 60) return `${diffSec}s ago`;
  const diffMin = Math.floor(diffSec / 60);
  if (diffMin < 60) return `${diffMin}m ago`;
  const diffHour = Math.floor(diffMin / 60);
  if (diffHour < 24) return `${diffHour}h ago`;
  const diffDay = Math.floor(diffHour / 24);
  if (diffDay < 7) return `${diffDay}d ago`;
  return new Date(timestamp).toLocaleDateString();
}
