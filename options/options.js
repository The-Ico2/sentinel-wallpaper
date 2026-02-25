function escapeHtml(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#039;');
}

function parseSentinelData() {
  try {
    const params = new URLSearchParams(window.location.search || '');
    const raw = params.get('sentinelData');
    if (!raw) return {};

    try {
      return JSON.parse(raw);
    } catch (_) {
      return JSON.parse(decodeURIComponent(raw));
    }
  } catch (_) {
    return {};
  }
}

const sentinelData = parseSentinelData();
const wallpaperData = sentinelData.wallpaper || {};
const wallpapers = Array.isArray(wallpaperData.assets) ? wallpaperData.assets : [];
let monitors = Array.isArray(wallpaperData.monitors) ? wallpaperData.monitors : [];
const profiles = Array.isArray(wallpaperData.profiles) ? wallpaperData.profiles : [];
const addonId = sentinelData.addonId || 'sentinel.addon.wallpaper';
const hostedBySentinel = sentinelData.hosted === true;
const CARD_COLUMNS_KEY = 'wallpaper:cardColumns';

function hasAnyHostBridge() {
  return !!(
    (window.ipc && typeof window.ipc.postMessage === 'function')
    || (window.chrome && window.chrome.webview && typeof window.chrome.webview.postMessage === 'function')
    || (window.parent && window.parent !== window)
  );
}

function getInitialCardColumns() {
  const saved = localStorage.getItem(CARD_COLUMNS_KEY);
  return saved === '5' ? 5 : 3;
}

let cardColumns = getInitialCardColumns();
let saveStatus = '';

const assignmentState = {
  selectedMonitorIds: new Set(),
  assignments: normalizeAssignments(wallpaperData.assignments || {})
};

function normalizeAssignments(rawAssignments) {
  const normalized = { ...(rawAssignments || {}) };
  const ordered = getPhysicalMonitors().slice();

  ordered.forEach((monitor, idx) => {
    const key0 = String(idx);
    const key1 = String(idx + 1);
    if (rawAssignments && rawAssignments[key0] && !normalized[monitor.id]) {
      normalized[monitor.id] = rawAssignments[key0];
    }
    if (rawAssignments && rawAssignments[key1] && !normalized[monitor.id]) {
      normalized[monitor.id] = rawAssignments[key1];
    }
  });

  return normalized;
}

function toPhysicalMonitor(monitor) {
  const scale = Number(monitor && monitor.scale) > 0 ? Number(monitor.scale) : 1;
  return {
    ...monitor,
    scale,
    pxX: Math.round(monitor.x || 0),
    pxY: Math.round(monitor.y || 0),
    pxWidth: Math.max(1, Math.round(monitor.width || 0)),
    pxHeight: Math.max(1, Math.round(monitor.height || 0))
  };
}

function getPhysicalMonitors() {
  const base = monitors.map(toPhysicalMonitor);
  if (base.length <= 1) {
    return base.map(m => ({ ...m, layoutX: m.pxX }));
  }

  const minHeight = Math.max(1, Math.min(...base.map(m => m.pxHeight || 1)));
  const rowTolerance = Math.max(80, Math.floor(minHeight / 4));

  const byRow = [...base].sort((a, b) => b.pxY - a.pxY);
  const rows = [];

  byRow.forEach(monitor => {
    const row = rows.find(r => Math.abs(monitor.pxY - r.anchorY) <= rowTolerance);
    if (row) {
      row.monitors.push(monitor);
    } else {
      rows.push({ anchorY: monitor.pxY, monitors: [monitor] });
    }
  });

  rows.sort((a, b) => b.anchorY - a.anchorY);

  const ordered = [];
  rows.forEach(row => {
    row.monitors.sort((a, b) => a.pxX - b.pxX);
    ordered.push(...row.monitors);
  });

  return ordered.map(m => ({ ...m, layoutX: m.pxX }));
}

function getSelectedWallpaperId() {
  return localStorage.getItem('wallpaper:selected') || wallpaperData.wallpaper_id || (wallpapers[0] && wallpapers[0].id) || null;
}

function findWallpaper(id) {
  return wallpapers.find(w => w.id === id) || null;
}

function getGithubUsername(url) {
  if (!url) return null;
  const match = String(url).match(/github\.com\/([^/?#]+)/i);
  return match && match[1] ? match[1] : null;
}

function getAuthorAvatar(wallpaper) {
  if (wallpaper.author_avatar_url) return wallpaper.author_avatar_url;
  const username = getGithubUsername(wallpaper.author_url);
  if (!username) return '';
  return `https://github.com/${username}.png?size=64`;
}

function getWallpaperDescription(wallpaper) {
  return wallpaper.short_description || wallpaper.description || 'No description available.';
}

function resolveMonitorWallpaperId(monitorId) {
  const ordered = getPhysicalMonitors();
  const ordinal = ordered.findIndex(m => m.id === monitorId);
  return assignmentState.assignments[monitorId]
    || (ordinal >= 0 ? assignmentState.assignments[String(ordinal)] : null)
    || (ordinal >= 0 ? assignmentState.assignments[String(ordinal + 1)] : null)
    || assignmentState.assignments['*']
    || wallpaperData.wallpaper_id
    || null;
}

function ensureSelectedMonitor() {
  if (assignmentState.selectedMonitorIds.size > 0) return;
  const primary = monitors.find(m => m.primary);
  const selected = (primary && primary.id) || (monitors[0] && monitors[0].id) || null;
  if (selected) assignmentState.selectedMonitorIds = new Set([selected]);
}

function assignSelectedWallpaperToCurrentMonitor() {
  const selectedWallpaperId = getSelectedWallpaperId();
  if (!selectedWallpaperId) return;
  ensureSelectedMonitor();
  assignmentState.selectedMonitorIds.forEach(monitorId => {
    assignmentState.assignments[monitorId] = selectedWallpaperId;
  });
}

function resolveMonitorIndexesForIds(monitorIds) {
  const ordered = getPhysicalMonitors();
  const indexes = [];
  (monitorIds || []).forEach(id => {
    const idx = ordered.findIndex(m => m.id === id);
    if (idx >= 0) indexes.push(String(idx));
  });
  return Array.from(new Set(indexes));
}

function postAssignmentUpdate(wallpaperId, monitorIds) {
  const normalizedMonitorIds = Array.from(new Set((monitorIds || []).filter(Boolean)));
  const monitorIndexes = resolveMonitorIndexesForIds(normalizedMonitorIds);
  const payload = {
    type: 'wallpaper_apply_assignment',
    addonId,
    wallpaperId,
    monitorIds: normalizedMonitorIds,
    monitorIndexes
  };

  try {
    localStorage.setItem('sentinel:lastWallpaperAssignment', JSON.stringify(payload));
  } catch (_) {}

  let sent = false;

  // Primary: __sentinelIPC bridge injected by Sentinel init script.
  // Same-origin custom protocol lets iframe directly call parent's bridge.
  if (!sent && typeof window.__sentinelIPC === 'function') {
    try {
      sent = !!window.__sentinelIPC(payload);
    } catch (err) {
      console.warn('[Sentinel][Wallpaper] __sentinelIPC failed', err);
    }
  }

  // Fallback: same-origin direct call to parent's bridge
  if (!sent && window.parent && window.parent !== window) {
    try {
      if (typeof window.parent.__sentinelBridgePost === 'function') {
        var msg = JSON.stringify(payload);
        sent = !!window.parent.__sentinelBridgePost(msg);
      }
    } catch (err) {
      console.warn('[Sentinel][Wallpaper] parent.__sentinelBridgePost failed', err);
    }
  }

  // Fallback: same-origin direct call to parent's ipc
  if (!sent && window.parent && window.parent !== window) {
    try {
      if (window.parent.ipc && typeof window.parent.ipc.postMessage === 'function') {
        window.parent.ipc.postMessage(JSON.stringify(payload));
        sent = true;
      }
    } catch (err) {
      console.warn('[Sentinel][Wallpaper] parent.ipc.postMessage failed', err);
    }
  }

  // Fallback: direct window.ipc.postMessage (only works from top-level frame)
  if (!sent) {
    try {
      if (window.ipc && typeof window.ipc.postMessage === 'function') {
        window.ipc.postMessage(JSON.stringify(payload));
        sent = true;
      }
    } catch (err) {
      console.warn('[Sentinel][Wallpaper] window.ipc.postMessage failed', err);
    }
  }

  const bridgeAvailable = hasAnyHostBridge();
  if (sent) {
    saveStatus = 'Sent to Sentinel host';
  } else if (hostedBySentinel || bridgeAvailable) {
    saveStatus = 'Failed to send to Sentinel host (queued locally)';
  } else {
    saveStatus = 'Preview only (assignment queued locally)';
  }
  return sent;
}

function replayQueuedAssignmentOnLoad() {
  try {
    const raw = localStorage.getItem('sentinel:lastWallpaperAssignment');
    if (!raw) return;
    const payload = JSON.parse(raw);
    if (!payload || payload.type !== 'wallpaper_apply_assignment') return;

    let sent = false;
    if (typeof window.__sentinelIPC === 'function') {
      try { sent = !!window.__sentinelIPC(payload); } catch (_) {}
    }
    if (!sent && window.parent && window.parent !== window) {
      try {
        if (typeof window.parent.__sentinelBridgePost === 'function') {
          sent = !!window.parent.__sentinelBridgePost(JSON.stringify(payload));
        }
      } catch (_) {}
    }
    if (!sent && window.parent && window.parent !== window) {
      try {
        if (window.parent.ipc && typeof window.parent.ipc.postMessage === 'function') {
          window.parent.ipc.postMessage(JSON.stringify(payload));
          sent = true;
        }
      } catch (_) {}
    }
    if (!sent) {
      try {
        if (window.ipc && typeof window.ipc.postMessage === 'function') {
          window.ipc.postMessage(JSON.stringify(payload));
          sent = true;
        }
      } catch (_) {}
    }

    if (sent) {
      saveStatus = 'Sent queued assignment to Sentinel host';
      localStorage.removeItem('sentinel:lastWallpaperAssignment');
    }
  } catch (_) {}
}

/* ── Generic IPC sender ── */
function sendIPC(payload) {
  const msg = typeof payload === 'string' ? payload : JSON.stringify(payload);
  try {
    if (typeof window.__sentinelIPC === 'function') {
      if (window.__sentinelIPC(payload)) return true;
    }
  } catch (_) {}
  try {
    if (window.parent && typeof window.parent.__sentinelBridgePost === 'function') {
      if (window.parent.__sentinelBridgePost(msg)) return true;
    }
  } catch (_) {}
  try {
    if (window.parent && window.parent.ipc && typeof window.parent.ipc.postMessage === 'function') {
      window.parent.ipc.postMessage(msg); return true;
    }
  } catch (_) {}
  try {
    if (window.ipc && typeof window.ipc.postMessage === 'function') {
      window.ipc.postMessage(msg); return true;
    }
  } catch (_) {}
  return false;
}

function postConfigUpdate(path, value) {
  return sendIPC({
    type: 'config_update',
    addonId: addonId,
    path: path,
    value: value
  });
}

function postWallpaperPropertyUpdate(property, value) {
  ensureSelectedMonitor();
  const monitorIndexes = resolveMonitorIndexesForIds(Array.from(assignmentState.selectedMonitorIds));
  return sendIPC({
    type: 'wallpaper_update_property',
    addonId: addonId,
    monitorIndexes: monitorIndexes,
    property: property,
    value: value
  });
}

function postClearCache() {
  return sendIPC({
    type: 'clear_cache',
    addonId: addonId
  });
}

function getProfileForMonitorIdx(monitorIdx) {
  return profiles.find(p => p.monitor_index === String(monitorIdx)) || null;
}

function getSelectedMonitorProfile() {
  if (assignmentState.selectedMonitorIds.size === 0) return null;
  const ordered = getPhysicalMonitors();
  const firstId = Array.from(assignmentState.selectedMonitorIds)[0];
  const idx = ordered.findIndex(m => m.id === firstId);
  if (idx < 0) return null;
  return getProfileForMonitorIdx(idx);
}

function emptyState(root, title, description) {
  root.innerHTML = `
    <div class="panel">
      <h3 style="margin-top:0">${escapeHtml(title)}</h3>
      <div class="small">${escapeHtml(description)}</div>
    </div>
  `;
}

function initCustomSelects(container) {
  (container || document).querySelectorAll('.custom-select').forEach(sel => {
    const trigger = sel.querySelector('.select-trigger');
    const dropdown = sel.querySelector('.select-dropdown');
    if (!trigger || !dropdown) return;

    trigger.addEventListener('click', (e) => {
      e.stopPropagation();
      // Close all other dropdowns
      document.querySelectorAll('.custom-select.open').forEach(s => {
        if (s !== sel) s.classList.remove('open');
      });
      sel.classList.toggle('open');
    });

    dropdown.querySelectorAll('.select-option').forEach(opt => {
      opt.addEventListener('click', (e) => {
        e.stopPropagation();
        const value = opt.dataset.value;
        const label = opt.textContent.trim();
        const valueEl = sel.querySelector('.select-value');
        if (valueEl) valueEl.textContent = label;
        sel.dataset.value = value;
        dropdown.querySelectorAll('.select-option').forEach(o => o.classList.remove('selected'));
        opt.classList.add('selected');
        sel.classList.remove('open');
      });
    });
  });

  // Close dropdowns on outside click
  document.addEventListener('click', () => {
    document.querySelectorAll('.custom-select.open').forEach(s => s.classList.remove('open'));
  });
}

function renderMonitorLayout(root) {
  const canvas = document.getElementById('monitor-layout-preview');
  if (!canvas) return;
  const viewport = canvas.parentElement;
  const physicalMonitors = getPhysicalMonitors();
  if (!physicalMonitors.length) {
    canvas.innerHTML = '<div class="small" style="padding:20px;text-align:center;">No monitor topology data available.</div>';
    return;
  }

  // Snap adjacent monitor edges — use generous tolerance to handle DPI scaling gaps
  const sorted = [...physicalMonitors].sort((a, b) => a.layoutX - b.layoutX);
  const snapped = sorted.map(m => ({ ...m }));
  for (let i = 1; i < snapped.length; i++) {
    const prev = snapped[i - 1];
    const prevEnd = prev.layoutX + prev.pxWidth;
    const gap = Math.abs(snapped[i].layoutX - prevEnd);
    // Snap if gap (or overlap) is within 2% of the monitor width
    const tolerance = Math.max(60, Math.floor(prev.pxWidth * 0.02));
    if (gap <= tolerance) {
      snapped[i].layoutX = prevEnd;
    }
  }

  const minX = Math.min(...snapped.map(m => m.layoutX));
  const minY = Math.min(...snapped.map(m => m.pxY));
  const maxX = Math.max(...snapped.map(m => m.layoutX + m.pxWidth));
  const maxY = Math.max(...snapped.map(m => m.pxY + m.pxHeight));

  const totalW = Math.max(1, maxX - minX);
  const totalH = Math.max(1, maxY - minY);
  const viewportW = Math.max(320, (viewport ? viewport.clientWidth || viewport.offsetWidth : 0) || 920);
  const maxH = Math.max(200, Math.min(400, Math.floor(window.innerHeight * 0.35)));
  const scale = Math.min((viewportW - 48) / totalW, maxH / totalH);
  const usedW = Math.ceil(totalW * scale);
  const usedH = Math.ceil(totalH * scale);

  canvas.style.width = `${usedW}px`;
  canvas.style.height = `${usedH}px`;
  canvas.innerHTML = '';

  // Build ordinal index map
  const ordered = getPhysicalMonitors();
  const ordinalMap = new Map();
  ordered.forEach((m, idx) => ordinalMap.set(m.id, idx));

  snapped.forEach(m => {
    const left = (m.layoutX - minX) * scale;
    const top = (m.pxY - minY) * scale;
    const width = Math.max(60, m.pxWidth * scale);
    const height = Math.max(40, m.pxHeight * scale);
    const selected = assignmentState.selectedMonitorIds.has(m.id);
    const ordinal = ordinalMap.has(m.id) ? ordinalMap.get(m.id) : '?';

    const wallpaperId = resolveMonitorWallpaperId(m.id);
    const wallpaper = findWallpaper(wallpaperId);

    const el = document.createElement('div');
    el.className = 'monitor-rect' + (selected ? ' selected' : '');
    el.style.cssText = `left:${left}px;top:${top}px;width:${width}px;height:${height}px;`;

    const bgStyle = (wallpaper && wallpaper.preview_url)
      ? `background-image:linear-gradient(rgba(0,0,0,0.35),rgba(0,0,0,0.35)),url('${wallpaper.preview_url}');background-size:cover;background-position:center;`
      : '';

    el.innerHTML = `
      <div class="monitor-wallpaper-preview" style="${bgStyle}"></div>
      <div class="monitor-overlay">
        <span class="monitor-id">${escapeHtml(String(ordinal))}</span>
        <span class="monitor-res-label">${escapeHtml(m.pxWidth + ' \u00d7 ' + m.pxHeight)}</span>
        ${m.primary ? '<span class="badge-primary">Primary</span>' : ''}
      </div>
      <div class="monitor-name-tag">${escapeHtml((wallpaper && wallpaper.name) || wallpaperId || 'None')}</div>
    `;

    el.onclick = (event) => {
      if (event.ctrlKey || event.metaKey) {
        if (assignmentState.selectedMonitorIds.has(m.id)) {
          assignmentState.selectedMonitorIds.delete(m.id);
        } else {
          assignmentState.selectedMonitorIds.add(m.id);
        }
        if (assignmentState.selectedMonitorIds.size === 0) {
          assignmentState.selectedMonitorIds.add(m.id);
        }
      } else {
        assignmentState.selectedMonitorIds = new Set([m.id]);
      }
      loadLibrary();
    };

    canvas.appendChild(el);
  });
}

function loadLibrary() {
  const root = document.getElementById('library-root');
  if (!root) return;

  if (!wallpapers.length) {
    emptyState(root, 'No wallpapers found', 'No real wallpaper assets were provided by Sentinel for this addon.');
    return;
  }

  ensureSelectedMonitor();

  const selectedCount = assignmentState.selectedMonitorIds.size;
  const profile = getSelectedMonitorProfile();
  const modeVal = escapeHtml((profile && profile.mode) || wallpaperData.mode || 'fill');
  const zVal = escapeHtml((profile && profile.z_index) || wallpaperData.z_index || 'desktop');
  const isEnabled = profile ? profile.enabled : (wallpaperData.enabled !== false);
  const enabledChecked = isEnabled ? 'checked' : '';
  const chevronSvg = '<svg class="select-chevron" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"/></svg>';

  const modeOptions = ['fill','fit','stretch','center','span'];
  const zOptions = ['desktop','normal','topmost'];

  root.innerHTML = `
    <div class="monitor-controls-row">
      <span class="controls-label">${selectedCount} Monitor${selectedCount !== 1 ? 's' : ''} Selected</span>
      <div class="control-group">
        <label class="toggle-switch">
          <input type="checkbox" id="lib-enabled-toggle" ${enabledChecked}>
          <span class="toggle-track"><span class="toggle-thumb"></span></span>
          <span class="toggle-label">Enabled</span>
        </label>
      </div>
      <div class="control-group">
        <div class="custom-select" data-value="${modeVal}" data-field="mode">
          <button type="button" class="select-trigger">
            <span class="select-label">Mode</span>
            <span class="select-value">${modeVal}</span>
            ${chevronSvg}
          </button>
          <div class="select-dropdown">
            ${modeOptions.map(v => `<div class="select-option${v === modeVal ? ' selected' : ''}" data-value="${v}">${escapeHtml(v.charAt(0).toUpperCase() + v.slice(1))}</div>`).join('')}
          </div>
        </div>
      </div>
      <div class="control-group">
        <div class="custom-select" data-value="${zVal}" data-field="z_index">
          <button type="button" class="select-trigger">
            <span class="select-label">Z-Index</span>
            <span class="select-value">${zVal}</span>
            ${chevronSvg}
          </button>
          <div class="select-dropdown">
            ${zOptions.map(v => `<div class="select-option${v === zVal ? ' selected' : ''}" data-value="${v}">${escapeHtml(v.charAt(0).toUpperCase() + v.slice(1))}</div>`).join('')}
          </div>
        </div>
      </div>
      ${saveStatus ? `<span class="small">${escapeHtml(saveStatus)}</span>` : ''}
    </div>

    <div class="monitor-section">
      <div class="section-header">
        <h3>Monitor Layout Preview</h3>
        <span class="section-hint"><kbd>Ctrl+Click</kbd> to multi-select</span>
      </div>
      <div class="monitor-layout-viewport">
        <div class="monitor-layout-canvas" id="monitor-layout-preview"></div>
      </div>
    </div>

    <div class="wallpaper-section">
      <div class="section-header">
        <h3>Wallpapers</h3>
        <div class="column-toggle" role="group" aria-label="Card column layout">
          <button type="button" class="col-btn${cardColumns === 3 ? ' active' : ''}" data-cols="3">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="3" y="3" width="5" height="18"/><rect x="10" y="3" width="5" height="18"/><rect x="17" y="3" width="5" height="18"/></svg>
            3
          </button>
          <button type="button" class="col-btn${cardColumns === 5 ? ' active' : ''}" data-cols="5">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="1" y="3" width="3.4" height="18"/><rect x="5.8" y="3" width="3.4" height="18"/><rect x="10.6" y="3" width="3.4" height="18"/><rect x="15.4" y="3" width="3.4" height="18"/><rect x="20.2" y="3" width="3.4" height="18"/></svg>
            5
          </button>
        </div>
      </div>
      <div class="wallpaper-grid cols-${cardColumns}" id="wallpaper-grid"></div>
    </div>
  `;

  const grid = document.getElementById('wallpaper-grid');
  const selectedWallpaperId = getSelectedWallpaperId();

  wallpapers.forEach(w => {
    const selected = w.id === selectedWallpaperId;
    const authorName = w.author_name || 'Unknown';
    const authorUrl = w.author_url || '';
    const authorAvatar = getAuthorAvatar(w);
    const lastUpdated = w.last_updated || 'unknown';
    const description = getWallpaperDescription(w);

    const card = document.createElement('div');
    card.className = 'wallpaper-card' + (selected ? ' selected' : '');

    const thumbnailHtml = w.preview_url
      ? `<img src="${escapeHtml(w.preview_url)}" alt="${escapeHtml(w.name || w.id)}" loading="lazy" />`
      : `<div class="thumbnail-placeholder"><svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="3" y="3" width="18" height="18" rx="2" ry="2"/><circle cx="8.5" cy="8.5" r="1.5"/><polyline points="21 15 16 10 5 21"/></svg></div>`;

    const avatarHtml = authorAvatar
      ? `<div class="github-profile-picture" style="background-image:url('${escapeHtml(authorAvatar)}');background-size:cover;background-position:center;"></div>`
      : `<div class="github-profile-picture"></div>`;

    const authorHtml = authorUrl
      ? `<a class="author-link" href="${escapeHtml(authorUrl)}" target="_blank" rel="noreferrer" onclick="event.stopPropagation()">${avatarHtml}<span class="github-username">${escapeHtml(authorName)}</span></a>`
      : `<span class="author-link">${avatarHtml}<span class="github-username">${escapeHtml(authorName)}</span></span>`;

    card.innerHTML = `
      <div class="card-thumbnail">${thumbnailHtml}</div>
      <div class="card-body">
        <div class="card-title">${escapeHtml(w.name || w.id)}</div>
        <div class="tags">${(w.tags || []).map(t => `<span class="pill">${escapeHtml(t)}</span>`).join('')}</div>
        <p class="card-description">${escapeHtml(description)}</p>
        <div class="card-meta">
          ${authorHtml}
          <time class="last-update">${escapeHtml(lastUpdated)}</time>
        </div>
      </div>
    `;

    card.onclick = () => {
      localStorage.setItem('wallpaper:selected', w.id);
      assignSelectedWallpaperToCurrentMonitor();
      postAssignmentUpdate(w.id, Array.from(assignmentState.selectedMonitorIds));
      loadLibrary();
    };
    grid.appendChild(card);
  });

  document.querySelectorAll('.col-btn').forEach(btn => {
    btn.addEventListener('click', () => {
      const next = Number(btn.dataset.cols) === 5 ? 5 : 3;
      if (next === cardColumns) return;
      cardColumns = next;
      localStorage.setItem(CARD_COLUMNS_KEY, String(cardColumns));
      loadLibrary();
    });
  });

  // Wire up Enable toggle
  const enableToggle = document.getElementById('lib-enabled-toggle');
  if (enableToggle) {
    enableToggle.addEventListener('change', () => {
      postWallpaperPropertyUpdate('enabled', enableToggle.checked);
    });
  }

  // Wire up Mode/Z-Index custom selects for wallpaper property updates
  root.querySelectorAll('.monitor-controls-row .custom-select').forEach(sel => {
    const field = sel.dataset.field;
    if (!field) return;
    sel.querySelectorAll('.select-option').forEach(opt => {
      opt.addEventListener('click', () => {
        postWallpaperPropertyUpdate(field, opt.dataset.value);
      });
    });
  });

  initCustomSelects(root);
  renderMonitorLayout(root);
}

function postEditableSave(wallpaperId, editableKey, value, manifestPath) {
  return sendIPC({
    type: 'wallpaper_save_editable',
    addonId: addonId,
    wallpaperId: wallpaperId,
    key: editableKey,
    value: value,
    manifestPath: manifestPath
  });
}

function postCapturePreview(wallpaperId, manifestPath) {
  return sendIPC({
    type: 'wallpaper_capture_preview',
    addonId: addonId,
    wallpaperId: wallpaperId,
    manifestPath: manifestPath
  });
}

function pushCssVarToIframe(iframe, varName, value) {
  if (!iframe) return;
  var directOk = false;
  try {
    var doc = iframe.contentDocument;
    if (doc && doc.documentElement) {
      doc.documentElement.style.setProperty(varName, value);
      directOk = true;
    }
  } catch (_) {}
  // Always try postMessage as well — handles cross-origin and ensures
  // the wallpaper's own listener can react to the change.
  if (!directOk) {
    try {
      if (iframe.contentWindow) {
        iframe.contentWindow.postMessage({
          type: '__sentinel_set_css_var',
          variable: varName,
          value: value
        }, '*');
      }
    } catch (_) {}
  }
}

function buildEditableControl(key, entry, item, iframe) {
  const selector = entry.selector || 'text';
  const variable = entry.variable || '';
  const value = entry.value;
  const label = key.replace(/([A-Z])/g, ' $1').replace(/[-_]/g, ' ').replace(/^\w/, c => c.toUpperCase());

  const row = document.createElement('div');
  row.className = 'property-row';

  const labelEl = document.createElement('label');
  labelEl.className = 'property-label';
  labelEl.textContent = label;
  row.appendChild(labelEl);

  function onValueChange(newValue) {
    entry.value = newValue;
    if (variable) pushCssVarToIframe(iframe, variable, typeof newValue === 'number' ? String(newValue) : newValue);
    postEditableSave(item.id, key, newValue, item.manifest_path);
    // Schedule preview capture after a short debounce
    clearTimeout(onValueChange._captureTimer);
    onValueChange._captureTimer = setTimeout(() => {
      postCapturePreview(item.id, item.manifest_path);
    }, 2000);
  }

  if (selector === 'color-picker') {
    const wrap = document.createElement('div');
    wrap.className = 'color-input-wrap';
    // Strip alpha channel for color input (only supports 6-char hex)
    const hexVal = typeof value === 'string' ? value.substring(0, 7) : '#000000';
    const colorInput = document.createElement('input');
    colorInput.type = 'color';
    colorInput.className = 'color-input';
    colorInput.value = hexVal;
    const hexSpan = document.createElement('span');
    hexSpan.className = 'color-hex';
    hexSpan.textContent = value || hexVal;
    wrap.appendChild(colorInput);
    wrap.appendChild(hexSpan);
    row.appendChild(wrap);

    colorInput.addEventListener('input', () => {
      // Preserve alpha if original had one
      const origAlpha = typeof value === 'string' && value.length === 9 ? value.substring(7) : '';
      const newVal = colorInput.value + origAlpha;
      hexSpan.textContent = newVal;
      onValueChange(newVal);
    });
  } else if (selector === 'slider') {
    const wrap = document.createElement('div');
    wrap.className = 'slider-control';
    const rangeInput = document.createElement('input');
    rangeInput.type = 'range';
    rangeInput.className = 'range-slider';
    rangeInput.min = entry.min != null ? String(entry.min) : '0';
    rangeInput.max = entry.max != null ? String(entry.max) : '100';
    rangeInput.step = entry.step != null ? String(entry.step) : '1';
    rangeInput.value = value != null ? String(value) : '0';
    const valSpan = document.createElement('span');
    valSpan.className = 'slider-value';
    valSpan.textContent = String(value != null ? value : 0);
    wrap.appendChild(rangeInput);
    wrap.appendChild(valSpan);
    row.appendChild(wrap);

    rangeInput.addEventListener('input', () => {
      const numVal = Number(rangeInput.value);
      valSpan.textContent = String(numVal);
      onValueChange(numVal);
    });
  } else if (selector === 'font-picker') {
    const fonts = ['Arial', 'Helvetica', 'Verdana', 'Georgia', 'Times New Roman',
      'Courier New', 'Trebuchet MS', 'Segoe UI', 'Inter', 'Roboto',
      'Poppins', 'JetBrains Mono', 'Cascadia Code', 'Fira Code', 'system-ui'];
    const sel = document.createElement('div');
    sel.className = 'custom-select editor-select';
    sel.dataset.value = value || 'Arial';
    const chevronSvg = '<svg class="select-chevron" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"/></svg>';
    sel.innerHTML = `
      <button class="select-trigger" type="button">
        <span class="select-value">${escapeHtml(value || 'Arial')}</span>
        ${chevronSvg}
      </button>
      <div class="select-dropdown">
        ${fonts.map(f => `<div class="select-option${f === value ? ' selected' : ''}" data-value="${escapeHtml(f)}" style="font-family:'${escapeHtml(f)}',sans-serif">${escapeHtml(f)}</div>`).join('')}
      </div>
    `;
    row.appendChild(sel);

    // Wire select after appending
    setTimeout(() => {
      sel.querySelectorAll('.select-option').forEach(opt => {
        opt.addEventListener('click', () => {
          onValueChange(opt.dataset.value);
        });
      });
    }, 0);
  } else {
    // Default text input
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'css-var-value';
    input.value = value != null ? String(value) : '';
    input.spellcheck = false;
    row.appendChild(input);

    let debounceTimer = null;
    input.addEventListener('input', () => {
      clearTimeout(debounceTimer);
      debounceTimer = setTimeout(() => {
        onValueChange(input.value);
      }, 300);
    });
  }

  return row;
}

function buildEditableGroup(groupKey, groupData, item, iframe) {
  const svgChevron = '<svg class="collapse-chevron" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"/></svg>';
  const groupName = groupData.name || groupKey.replace(/[-_]/g, ' ').replace(/^\w/, c => c.toUpperCase());
  const groupDesc = groupData.description || '';

  const group = document.createElement('div');
  group.className = 'property-group';

  const title = document.createElement('div');
  title.className = 'property-group-title';
  title.innerHTML = `
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3h7a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-7m0-18H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h7m0-18v18"/></svg>
    ${escapeHtml(groupName)}
    ${svgChevron}
  `;
  if (groupDesc) {
    title.title = groupDesc;
  }
  title.addEventListener('click', () => group.classList.toggle('collapsed'));
  group.appendChild(title);

  const body = document.createElement('div');
  body.className = 'property-group-body';

  Object.keys(groupData).forEach(subKey => {
    if (subKey === 'name' || subKey === 'description') return;
    const subEntry = groupData[subKey];
    if (subEntry && typeof subEntry === 'object' && subEntry.selector) {
      body.appendChild(buildEditableControl(subKey, subEntry, item, iframe));
    }
  });

  group.appendChild(body);
  return group;
}

function loadEditor() {
  const root = document.getElementById('editor-root');
  if (!root) return;

  if (!wallpapers.length) {
    emptyState(root, 'No wallpapers available', 'Editor needs real wallpaper assets from Sentinel payload.');
    return;
  }

  const selectedId = getSelectedWallpaperId();
  const item = findWallpaper(selectedId) || wallpapers[0];
  if (!item) { emptyState(root, 'No wallpaper selected', 'Select a wallpaper in the Library tab first.'); return; }

  const htmlUrl = item.html_url || '';
  const editable = (item.editable && typeof item.editable === 'object' && !Array.isArray(item.editable)) ? item.editable : {};
  const hasEditable = Object.keys(editable).length > 0;

  root.innerHTML = `
    <div class="editor-layout">
      <div class="editor-preview-pane">
        <div class="section-header">
          <h3>Live Preview</h3>
          <span class="section-hint">${escapeHtml(item.name || item.id)}</span>
        </div>
        <div class="editor-preview-canvas" id="editor-preview-canvas">
          ${htmlUrl
            ? `<iframe id="editor-wallpaper-frame" src="${escapeHtml(htmlUrl)}" title="${escapeHtml(item.name || item.id)}"></iframe>`
            : item.preview_url
              ? `<img src="${escapeHtml(item.preview_url)}" alt="${escapeHtml(item.name || item.id)}" />`
              : `<div class="preview-placeholder">
                  <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round">
                    <path d="M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/>
                    <polyline points="13 2 13 9 20 9"/>
                  </svg>
                  <span>No wallpaper HTML found</span>
                </div>`
          }
        </div>
      </div>

      <div class="editor-properties" id="editor-properties">
        <div class="section-header">
          <h3>Properties</h3>
          <span class="section-hint">Changes save automatically</span>
        </div>
      </div>
    </div>
  `;

  const iframe = document.getElementById('editor-wallpaper-frame');
  const propsContainer = document.getElementById('editor-properties');

  if (!hasEditable) {
    const emptyMsg = document.createElement('div');
    emptyMsg.className = 'property-group';
    emptyMsg.innerHTML = '<div style="padding:16px;text-align:center;color:var(--text-tertiary);font-size:13px;">This wallpaper has no editable properties.</div>';
    propsContainer.appendChild(emptyMsg);
  } else {
    // Separate top-level controls from groups
    const topLevelEntries = [];
    const groups = [];

    Object.keys(editable).forEach(key => {
      const entry = editable[key];
      if (!entry || typeof entry !== 'object') return;
      if (entry.selector) {
        // Top-level editable control
        topLevelEntries.push({ key, entry });
      } else if (entry.name || Object.values(entry).some(v => v && typeof v === 'object' && v.selector)) {
        // Group
        groups.push({ key, data: entry });
      }
    });

    // Build top-level controls in a "General" group
    if (topLevelEntries.length > 0) {
      const svgChevron = '<svg class="collapse-chevron" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"/></svg>';
      const generalGroup = document.createElement('div');
      generalGroup.className = 'property-group';
      const title = document.createElement('div');
      title.className = 'property-group-title';
      title.innerHTML = `
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="13.5" cy="6.5" r="2.5"/><path d="M17.08 6.5a6 6 0 0 1-10.92 4.5"/><circle cx="6" cy="17.5" r="2.5"/><path d="M8.5 17.5a6 6 0 0 1 10.13-4.42"/><circle cx="19.5" cy="17.5" r="2.5"/><path d="M14.42 17.28A6 6 0 0 1 3 13.5"/></svg>
        General
        ${svgChevron}
      `;
      title.addEventListener('click', () => generalGroup.classList.toggle('collapsed'));
      generalGroup.appendChild(title);

      const body = document.createElement('div');
      body.className = 'property-group-body';
      topLevelEntries.forEach(({ key, entry }) => {
        body.appendChild(buildEditableControl(key, entry, item, iframe));
      });
      generalGroup.appendChild(body);
      propsContainer.appendChild(generalGroup);
    }

    // Build named groups
    groups.forEach(({ key, data }) => {
      propsContainer.appendChild(buildEditableGroup(key, data, item, iframe));
    });
  }

  // Apply current editable CSS variables to the iframe once loaded
  if (iframe && htmlUrl) {
    iframe.addEventListener('load', () => {
      applyAllEditableVars(editable, iframe);
    });
  }

  initCustomSelects(root);
}

function applyAllEditableVars(editable, iframe) {
  if (!editable || !iframe) return;
  Object.keys(editable).forEach(key => {
    const entry = editable[key];
    if (!entry || typeof entry !== 'object') return;
    if (entry.variable && entry.value != null) {
      pushCssVarToIframe(iframe, entry.variable, typeof entry.value === 'number' ? String(entry.value) : entry.value);
    } else {
      // Might be a group
      Object.keys(entry).forEach(subKey => {
        const sub = entry[subKey];
        if (sub && typeof sub === 'object' && sub.variable && sub.value != null) {
          pushCssVarToIframe(iframe, sub.variable, typeof sub.value === 'number' ? String(sub.value) : sub.value);
        }
      });
    }
  });
}

function loadDiscover() {
  const root = document.getElementById('discover-root');
  if (!root) return;

  if (!wallpapers.length) {
    emptyState(root, 'No discoverable wallpapers', 'Sentinel returned no wallpaper metadata for discovery.');
    return;
  }

  const filterChips = ['All', 'Popular', 'Recent', 'Animated', 'Static', 'HDR'];
  let activeFilter = 'All';

  const downloadSvg = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>';

  const gradients = [
    'linear-gradient(135deg, #667eea 0%, #764ba2 100%)',
    'linear-gradient(135deg, #f093fb 0%, #f5576c 100%)',
    'linear-gradient(135deg, #4facfe 0%, #00f2fe 100%)',
    'linear-gradient(135deg, #43e97b 0%, #38f9d7 100%)',
    'linear-gradient(135deg, #fa709a 0%, #fee140 100%)',
    'linear-gradient(135deg, #a18cd1 0%, #fbc2eb 100%)',
    'linear-gradient(135deg, #0c3483 0%, #a2b6df 100%)',
    'linear-gradient(135deg, #ffecd2 0%, #fcb69f 100%)',
    'linear-gradient(135deg, #30cfd0 0%, #330867 100%)'
  ];

  function buildDiscoverCard(w, index) {
    const authorName = w.author_name || 'Unknown';
    const gradient = gradients[index % gradients.length];
    const previewBg = w.preview_url
      ? `background-image: url('${escapeHtml(w.preview_url)}'); background-size: cover; background-position: center;`
      : `background: ${gradient};`;

    return `
      <div class="discover-card">
        <div class="discover-thumb">
          <div class="discover-thumb-bg" style="${previewBg}"></div>
          <div class="discover-card-overlay">
            <span class="discover-downloads">${downloadSvg} ${Math.floor(Math.random() * 4000 + 500)}</span>
          </div>
        </div>
        <div class="discover-info">
          <span class="discover-name">${escapeHtml(w.name || w.id)}</span>
          <span class="discover-author">by ${escapeHtml(authorName)}</span>
        </div>
      </div>
    `;
  }

  function render(query) {
    const q = (query || '').trim().toLowerCase();
    const filtered = wallpapers.filter(w => {
      if (q) {
        const inName = (w.name || '').toLowerCase().includes(q);
        const inId = (w.id || '').toLowerCase().includes(q);
        const inTags = (w.tags || []).some(t => String(t).toLowerCase().includes(q));
        if (!inName && !inId && !inTags) return false;
      }
      if (activeFilter !== 'All') {
        const tags = (w.tags || []).map(t => String(t).toLowerCase());
        return tags.includes(activeFilter.toLowerCase());
      }
      return true;
    });

    // Split into "Featured" (first half) and "Trending" (second half)
    const mid = Math.max(3, Math.ceil(filtered.length / 2));
    const featured = filtered.slice(0, mid);
    const trending = filtered.slice(mid);

    let html = `
      <div class="discover-header">
        <div class="search-bar">
          <svg class="search-icon" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="11" cy="11" r="8"/>
            <line x1="21" y1="21" x2="16.65" y2="16.65"/>
          </svg>
          <input type="text" class="search-input" id="discover-search" placeholder="Search wallpapers, themes, effects..." value="${escapeHtml(q)}">
        </div>
        <div class="filter-chips">
          ${filterChips.map(chip => `<button class="filter-chip${chip === activeFilter ? ' active' : ''}" data-filter="${escapeHtml(chip)}">${escapeHtml(chip)}</button>`).join('')}
        </div>
      </div>

      <div class="discover-section">
        <div class="section-header">
          <h3>Featured</h3>
          <button class="text-btn">View all</button>
        </div>
        <div class="discover-grid">
          ${featured.map((w, i) => buildDiscoverCard(w, i)).join('')}
        </div>
      </div>
    `;

    if (trending.length) {
      html += `
        <div class="discover-section">
          <div class="section-header">
            <h3>Trending This Week</h3>
            <button class="text-btn">View all</button>
          </div>
          <div class="discover-grid">
            ${trending.map((w, i) => buildDiscoverCard(w, mid + i)).join('')}
          </div>
        </div>
      `;
    }

    root.innerHTML = html;

    // Wire up search
    const searchInput = document.getElementById('discover-search');
    if (searchInput) {
      searchInput.addEventListener('input', e => render(e.target.value));
      // Restore focus + cursor position
      searchInput.focus();
      searchInput.setSelectionRange(searchInput.value.length, searchInput.value.length);
    }

    // Wire up filter chips
    root.querySelectorAll('.filter-chip').forEach(chip => {
      chip.addEventListener('click', () => {
        activeFilter = chip.dataset.filter || 'All';
        render(searchInput ? searchInput.value : '');
      });
    });
  }

  render('');
}

function loadSettings() {
  const root = document.getElementById('settings-root');
  if (!root) return;

  // ── helpers ──
  const d = wallpaperData;
  const chevronSvg = '<svg class="select-chevron" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"/></svg>';

  function tog(value) { return value ? 'checked' : ''; }
  function sel(current, value) { return current === value ? ' selected' : ''; }

  function buildToggle(name, desc, checked, configPath) {
    return `<div class="setting-item" data-config-path="${escapeHtml(configPath)}" data-control="toggle">
      <div class="setting-info"><span class="setting-name">${escapeHtml(name)}</span><span class="setting-description">${escapeHtml(desc)}</span></div>
      <label class="toggle-switch"><input type="checkbox" ${checked}><span class="toggle-track"><span class="toggle-thumb"></span></span></label>
    </div>`;
  }

  function buildSelect(name, desc, currentValue, options, configPath) {
    const cv = escapeHtml(String(currentValue ?? options[0]?.[0] ?? ''));
    const currentLabel = escapeHtml(String((options.find(o => String(o[0]) === String(currentValue)) || options[0] || ['',''])[1]));
    return `<div class="setting-item" data-config-path="${escapeHtml(configPath)}" data-control="select">
      <div class="setting-info"><span class="setting-name">${escapeHtml(name)}</span><span class="setting-description">${escapeHtml(desc)}</span></div>
      <div class="custom-select settings-select" data-value="${cv}">
        <button class="select-trigger" type="button"><span class="select-value">${currentLabel}</span>${chevronSvg}</button>
        <div class="select-dropdown">
          ${options.map(([v, label]) => `<div class="select-option${sel(String(currentValue), String(v))}" data-value="${escapeHtml(String(v))}">${escapeHtml(label)}</div>`).join('')}
        </div>
      </div>
    </div>`;
  }

  function buildNumber(name, desc, value, unit, configPath) {
    const v = value ?? 0;
    return `<div class="setting-item" data-config-path="${escapeHtml(configPath)}" data-control="number">
      <div class="setting-info"><span class="setting-name">${escapeHtml(name)}</span><span class="setting-description">${escapeHtml(desc)}</span></div>
      <div class="number-input-wrap"><input type="number" class="number-input" value="${escapeHtml(String(v))}" min="0" step="any"><span class="number-unit">${escapeHtml(unit)}</span></div>
    </div>`;
  }

  // ── read values ──
  const logLevel    = d.log_level || 'warn';
  const tickSleep   = d.tick_sleep_ms ?? 8;
  const watcherOn   = d.watcher_enabled ?? true;
  const watcherMs   = d.watcher_interval_ms ?? 600;
  const reapplyPause= d.reapply_on_pause_change ?? true;
  const updateCheck = d.update_check ?? true;
  const debugMode   = d.debug ?? false;

  const pauseFocus  = d.pause_focus || 'off';
  const pauseMax    = d.pause_maximized || 'off';
  const pauseFS     = d.pause_fullscreen || 'off';
  const pauseBat    = d.pause_battery || 'all-monitors';
  const pauseChkMs  = d.pause_check_interval_ms ?? 100;

  const sendMove    = d.interactions_send_move ?? true;
  const sendClick   = d.interactions_send_click ?? true;
  const pollMs      = d.interactions_poll_interval_ms ?? 8;
  const moveThresh  = d.interactions_move_threshold_px ?? 0.5;

  const audioOn     = d.audio_enabled ?? true;
  const audioSample = d.audio_sample_interval_ms ?? 100;
  const audioRefresh= d.audio_endpoint_refresh_ms ?? 1200;
  const audioRetry  = d.audio_retry_interval_ms ?? 2000;
  const audioThresh = d.audio_change_threshold ?? 0.015;
  const audioQuant  = d.audio_quantize_decimals ?? 2;

  const logPause    = d.log_pause_state_changes ?? true;
  const logWatcher  = d.log_watcher_reloads ?? true;

  const cacheBytes  = d.cache_size_bytes ?? 0;
  const cacheMB     = (cacheBytes / (1024 * 1024)).toFixed(1);
  const cachePercent= Math.min(100, Math.round(cacheBytes / (1024 * 1024 * 1024) * 100));
  const addonRoot   = d.addon_root_path || '';
  const addonVersion= d.addon_version || '1.0.0';
  const backendVersion = d.backend_version || '0.0.0';

  const pauseOpts = [['off','Off'],['current-monitor','Current Monitor'],['all-monitors','All Monitors']];

  root.innerHTML = `
    <!-- ═══ General ═══ -->
    <div class="settings-group">
      <div class="settings-group-header">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>
        <h3>General</h3>
      </div>
      <div class="settings-list">
        ${buildToggle('File Watcher', 'Watch wallpaper files for changes and auto-reload', tog(watcherOn), 'settings.performance.watcher.enabled')}
        ${buildNumber('Watcher Interval', 'How often the file watcher checks for changes', watcherMs, 'ms', 'settings.performance.watcher.interval_ms')}
        ${buildToggle('Reapply on Pause Change', 'Re-apply wallpaper when pause state changes', tog(reapplyPause), 'settings.runtime.reapply_on_pause_change')}
        ${buildToggle('Check for Updates', 'Automatically check for addon updates on launch', tog(updateCheck), 'settings.development.update_check')}
      </div>
    </div>

    <!-- ═══ Pausing ═══ -->
    <div class="settings-group">
      <div class="settings-group-header">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="6" y="4" width="4" height="16"/><rect x="14" y="4" width="4" height="16"/></svg>
        <h3>Pausing</h3>
      </div>
      <div class="settings-list">
        ${buildSelect('Pause on Focus Loss', 'Pause wallpapers when the desktop loses focus', pauseFocus, pauseOpts, 'settings.performance.pausing.focus')}
        ${buildSelect('Pause on Maximized', 'Pause wallpapers when a window is maximized', pauseMax, pauseOpts, 'settings.performance.pausing.maximized')}
        ${buildSelect('Pause on Fullscreen', 'Pause wallpapers when a fullscreen application is detected', pauseFS, pauseOpts, 'settings.performance.pausing.fullscreen')}
        ${buildSelect('Pause on Battery', 'Pause wallpapers when running on battery power', pauseBat, pauseOpts, 'settings.performance.pausing.battery')}
        ${buildNumber('Pause Check Interval', 'How often the pause state is evaluated', pauseChkMs, 'ms', 'settings.performance.pausing.check_interval_ms')}
      </div>
    </div>

    <!-- ═══ Performance ═══ -->
    <div class="settings-group">
      <div class="settings-group-header">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/></svg>
        <h3>Performance</h3>
      </div>
      <div class="settings-list">
        ${buildSelect('Tick Sleep', 'Milliseconds between runtime tick cycles', tickSleep, [['4','4 ms'],['8','8 ms'],['16','16 ms'],['32','32 ms'],['50','50 ms'],['100','100 ms']], 'settings.runtime.tick_sleep_ms')}
        ${buildToggle('Send Mouse Move', 'Forward mouse move events to wallpaper scripts', tog(sendMove), 'settings.performance.interactions.send_move')}
        ${buildToggle('Send Mouse Click', 'Forward mouse click events to wallpaper scripts', tog(sendClick), 'settings.performance.interactions.send_click')}
        ${buildNumber('Interaction Poll Interval', 'How often mouse position is sampled for wallpaper input', pollMs, 'ms', 'settings.performance.interactions.poll_interval_ms')}
        ${buildNumber('Move Threshold', 'Minimum pixel distance before a mouse move event is sent', moveThresh, 'px', 'settings.performance.interactions.move_threshold_px')}
      </div>
    </div>

    <!-- ═══ Audio ═══ -->
    <div class="settings-group">
      <div class="settings-group-header">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"/><path d="M19.07 4.93a10 10 0 0 1 0 14.14"/><path d="M15.54 8.46a5 5 0 0 1 0 7.07"/></svg>
        <h3>Audio</h3>
      </div>
      <div class="settings-list">
        ${buildToggle('Audio Visualizer', 'Enable audio data capture for visualizer wallpapers', tog(audioOn), 'settings.performance.audio.enabled')}
        ${buildNumber('Sample Interval', 'How often audio levels are sampled', audioSample, 'ms', 'settings.performance.audio.sample_interval_ms')}
        ${buildNumber('Endpoint Refresh', 'How often the audio endpoint device is refreshed', audioRefresh, 'ms', 'settings.performance.audio.endpoint_refresh_ms')}
        ${buildNumber('Retry Interval', 'Delay before retrying after an audio capture error', audioRetry, 'ms', 'settings.performance.audio.retry_interval_ms')}
        ${buildNumber('Change Threshold', 'Minimum level change to trigger an update to wallpapers', audioThresh, '', 'settings.performance.audio.change_threshold')}
        ${buildNumber('Quantize Decimals', 'Decimal places for audio level values sent to wallpapers', audioQuant, '', 'settings.performance.audio.quantize_decimals')}
      </div>
    </div>

    <!-- ═══ Diagnostics ═══ -->
    <div class="settings-group">
      <div class="settings-group-header">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/><line x1="16" y1="13" x2="8" y2="13"/><line x1="16" y1="17" x2="8" y2="17"/><polyline points="10 9 9 9 8 9"/></svg>
        <h3>Diagnostics</h3>
      </div>
      <div class="settings-list">
        ${buildSelect('Log Level', 'Controls verbosity of wallpaper runtime logging', logLevel, [['error','Error'],['warn','Warn'],['info','Info'],['debug','Debug'],['trace','Trace']], 'settings.development.log_level')}
        ${buildToggle('Debug Mode', 'Enable extra debug information and developer tools', tog(debugMode), 'settings.development.debug')}
        ${buildToggle('Log Pause State Changes', 'Write a log entry whenever wallpaper pause state changes', tog(logPause), 'settings.diagnostics.log_pause_state_changes')}
        ${buildToggle('Log Watcher Reloads', 'Write a log entry when the file watcher triggers a reload', tog(logWatcher), 'settings.diagnostics.log_watcher_reloads')}
      </div>
    </div>

    <!-- ═══ Storage ═══ -->
    <div class="settings-group">
      <div class="settings-group-header">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/><polyline points="3.27 6.96 12 12.01 20.73 6.96"/><line x1="12" y1="22.08" x2="12" y2="12"/></svg>
        <h3>Storage</h3>
      </div>
      <div class="settings-list">
        <div class="setting-item">
          <div class="setting-info"><span class="setting-name">Cache Size</span><span class="setting-description">Wallpaper thumbnails and preview cache</span></div>
          <div class="storage-indicator"><span class="storage-value">${cacheMB} MB</span><div class="storage-bar"><div class="storage-bar-fill" style="width: ${cachePercent}%;"></div></div></div>
        </div>
        <div class="setting-item">
          <div class="setting-info"><span class="setting-name">Addon Location</span><span class="setting-description">${escapeHtml(addonRoot) || 'Unknown'}</span></div>
        </div>
        <div class="setting-item">
          <div class="setting-info"><span class="setting-name">Clear Cache</span><span class="setting-description">Remove all cached thumbnails and preview data</span></div>
          <button class="btn btn-danger btn-sm" type="button" id="clear-cache-btn">Clear</button>
        </div>
      </div>
    </div>

    <!-- ═══ About ═══ -->
    <div class="settings-group">
      <div class="settings-group-header">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>
        <h3>About</h3>
      </div>
      <div class="settings-list">
        <div class="setting-item">
          <div class="setting-info"><span class="setting-name">Wallpaper Addon</span><span class="setting-description">Sentinel Wallpaper Engine</span></div>
          <span class="version-badge">v${escapeHtml(addonVersion)}</span>
        </div>
        <div class="setting-item">
          <div class="setting-info"><span class="setting-name">Sentinel Core</span><span class="setting-description">Backend runtime version</span></div>
          <span class="version-badge">v${escapeHtml(backendVersion)}</span>
        </div>
        <div class="setting-item">
          <div class="setting-info"><span class="setting-name">Addon ID</span><span class="setting-description">${escapeHtml(addonId)}</span></div>
        </div>
      </div>
    </div>
  `;

  // Wire up custom select dropdowns
  initCustomSelects(root);

  // ── Wire up settings save to config ──
  let saveDebounce = null;
  function scheduleSave(configPath, value) {
    clearTimeout(saveDebounce);
    saveDebounce = setTimeout(() => {
      postConfigUpdate(configPath, value);
    }, 300);
  }

  // Toggle switches
  root.querySelectorAll('.setting-item[data-control="toggle"]').forEach(item => {
    const configPath = item.dataset.configPath;
    if (!configPath) return;
    const checkbox = item.querySelector('input[type="checkbox"]');
    if (!checkbox) return;
    checkbox.addEventListener('change', () => {
      postConfigUpdate(configPath, checkbox.checked);
    });
  });

  // Custom selects
  root.querySelectorAll('.setting-item[data-control="select"]').forEach(item => {
    const configPath = item.dataset.configPath;
    if (!configPath) return;
    item.querySelectorAll('.select-option').forEach(opt => {
      opt.addEventListener('click', () => {
        let val = opt.dataset.value;
        // Convert numeric strings for tick_sleep_ms
        if (configPath.endsWith('_ms') || configPath.endsWith('tick_sleep_ms')) {
          val = Number(val);
        }
        postConfigUpdate(configPath, val);
      });
    });
  });

  // Number inputs
  root.querySelectorAll('.setting-item[data-control="number"]').forEach(item => {
    const configPath = item.dataset.configPath;
    if (!configPath) return;
    const input = item.querySelector('input[type="number"]');
    if (!input) return;
    input.addEventListener('change', () => {
      const val = parseFloat(input.value);
      if (!isNaN(val)) scheduleSave(configPath, val);
    });
  });

  // Clear cache button
  const clearBtn = document.getElementById('clear-cache-btn');
  if (clearBtn) {
    clearBtn.addEventListener('click', () => {
      clearBtn.textContent = 'Clearing...';
      clearBtn.disabled = true;
      postClearCache();
      setTimeout(() => {
        clearBtn.textContent = 'Cleared!';
        const indicator = root.querySelector('.storage-value');
        if (indicator) indicator.textContent = '0.0 MB';
        const bar = root.querySelector('.storage-bar-fill');
        if (bar) bar.style.width = '0%';
        setTimeout(() => {
          clearBtn.textContent = 'Clear';
          clearBtn.disabled = false;
        }, 2000);
      }, 500);
    });
  }
}

/* ── Live monitor update listener ── */
window.addEventListener('message', (event) => {
  if (!event.data) return;
  let data = event.data;
  if (typeof data === 'string') {
    try { data = JSON.parse(data); } catch (_) { return; }
  }
  if (data && data.type === '__sentinel_monitors' && Array.isArray(data.monitors)) {
    monitors.length = 0;
    monitors.push(...data.monitors);
    if (document.getElementById('library-root')) {
      renderMonitorLayout(document.getElementById('library-root'));
    }
  }
});

window.addEventListener('DOMContentLoaded', () => {
  replayQueuedAssignmentOnLoad();
  loadLibrary();
  loadEditor();
  loadDiscover();
  loadSettings();

  let resizeRaf = null;
  window.addEventListener('resize', () => {
    if (resizeRaf !== null) return;
    resizeRaf = window.requestAnimationFrame(() => {
      resizeRaf = null;
      if (document.getElementById('library-root')) {
        loadLibrary();
      }
    });
  });
});
