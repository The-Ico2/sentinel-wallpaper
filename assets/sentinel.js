/**
 * Sentinel SDK v2.0
 * ------------------
 * Drop this file into any Sentinel wallpaper / widget HTML project.
 * It bridges the WebView2 native message channel and provides a
 * subscription-based API so you only receive the data you care about.
 *
 * Architecture:
 *   Sentinel Backend  ──(IPC)──▶  Wallpaper Addon (Rust)  ──(WebView2 postMessage)──▶  This SDK
 *
 * The wallpaper addon pushes these message types:
 *   native_registry  – full sysdata + appdata snapshot (periodic, ~100ms)
 *   native_pause     – wallpaper paused/resumed
 *   native_css_vars  – live CSS variable updates from manifest editable changes
 *
 * Registry format (v2):
 *   sysdata: {
 *     displays: [{ id, category, subtype, metadata: { primary, x, y, width, height, ... } }],
 *     cpu: { ... }, gpu: { ... }, ram: { ... }, storage: { ... },
 *     network: { ... }, audio: { ... }, time: { ... }, keyboard: { ... },
 *     mouse: { ... }, power: { ... }, bluetooth: { ... }, wifi: { ... },
 *     system: { ... }, processes: { ... }, idle: { ... }
 *   }
 *   appdata: {
 *     "MONITOR_ID": {
 *       windows: [{ focused, app_name, app_icon, exe_path, window_title,
 *                   pid, window_state, size: { width, height },
 *                   position: { x, y } }]
 *     }
 *   }
 *
 * Usage:
 *   <script src="sentinel.js"></script>
 *   <script>
 *     // Subscribe to sysdata categories (only called when data changes)
 *     Sentinel.subscribe('cpu', data => { ... });
 *     Sentinel.subscribe('gpu', data => { ... });
 *     Sentinel.subscribe('ram', data => { ... });
 *     Sentinel.subscribe('displays', displays => { ... });
 *
 *     // Subscribe to media session data (extracted from audio)
 *     Sentinel.subscribe('media', session => {
 *       console.log(session.title, session.artist, session.playing);
 *     });
 *
 *     // Subscribe to per-monitor appdata
 *     Sentinel.subscribe('appdata', allMonitors => { ... });
 *     Sentinel.subscribe('appdata:MONITOR_0', monitorData => { ... });
 *
 *     // Lifecycle / notification events
 *     Sentinel.on('pause', e => { ... });
 *     Sentinel.on('resume', e => { ... });
 *
 *     // Read current cached state at any time
 *     const cpu = Sentinel.get('cpu');
 *     const allSys = Sentinel.sysdata;
 *     const allApp = Sentinel.appdata;
 *
 *     // Media session helpers
 *     const media = Sentinel.media;                   // current media session
 *     const media2 = Sentinel.get('media');            // same thing via get()
 *
 *     // Display helpers
 *     const monitors = Sentinel.displays;              // flat metadata array
 *     const primary  = Sentinel.getDisplay('0');        // single display metadata
 *     const windows  = Sentinel.getWindows('MONITOR_0'); // windows on a monitor
 *   </script>
 */
;(function (root) {
  'use strict';

  /* ═══════════════════════════════════════════════
   *  Internal state
   * ═══════════════════════════════════════════════ */
  const _subscribers = {};      // category → [callbacks]
  const _eventListeners = {};   // event name → [callbacks]
  const _prevHash = {};         // category → hash for change detection
  let _sysdata = null;
  let _appdata = null;
  let _paused = false;
  let _monitorBounds = null;
  const _lastDemandSig = { value: '' };

  const SYS_SECTION_KEYS = {
    cpu: 'cpu', gpu: 'gpu', ram: 'ram', storage: 'storage',
    network: 'network', audio: 'audio', time: 'time',
    keyboard: 'keyboard', mouse: 'mouse', power: 'power',
    bluetooth: 'bluetooth', wifi: 'wifi', system: 'system',
    processes: 'processes', idle: 'idle', displays: 'displays'
  };

  function computeDemandSections() {
    var set = {};
    for (var key in _subscribers) {
      if (!_subscribers.hasOwnProperty(key)) continue;
      var arr = _subscribers[key];
      if (!arr || arr.length === 0) continue;

      if (key === 'sysdata') {
        for (var section in SYS_SECTION_KEYS) {
          if (SYS_SECTION_KEYS.hasOwnProperty(section)) set[SYS_SECTION_KEYS[section]] = true;
        }
        continue;
      }

      if (key === 'appdata' || key.indexOf('appdata:') === 0) {
        set.appdata = true;
        continue;
      }

      // 'media' is a virtual key — it needs the 'audio' section
      if (key === 'media') {
        set.audio = true;
        continue;
      }

      if (SYS_SECTION_KEYS[key]) {
        set[SYS_SECTION_KEYS[key]] = true;
      }
    }

    return Object.keys(set).sort();
  }

  function publishDemandSections() {
    if (!(root.chrome && root.chrome.webview && typeof root.chrome.webview.postMessage === 'function')) return;

    var sections = computeDemandSections();
    var sig = sections.join('|');
    if (_lastDemandSig.value === sig) return;
    _lastDemandSig.value = sig;

    try {
      root.chrome.webview.postMessage({
        type: 'sentinel_demands',
        sections: sections
      });
    } catch (_) {}
  }

  /* Fast, cheap string hash for change detection (djb2) */
  function djb2(str) {
    let h = 5381;
    for (let i = 0, len = str.length; i < len; i++) {
      h = ((h << 5) + h + str.charCodeAt(i)) | 0;
    }
    return h;
  }

  /* ═══════════════════════════════════════════════
   *  Core message handler
   * ═══════════════════════════════════════════════ */
  function handleMessage(d) {
    if (!d || typeof d !== 'object') return;

    switch (d.type) {

      /* ─── Registry snapshot (sysdata + appdata) ─── */
      case 'native_registry':
        if (d.sysdata) {
          _sysdata = d.sysdata;
          dispatchSysdata(d.sysdata);
        }
        if (d.appdata) {
          _appdata = d.appdata;
          dispatchAppdata(d.appdata);
        }
        emit('registry', { sysdata: _sysdata, appdata: _appdata });
        break;

      /* ─── Pause / Resume ─── */
      case 'native_pause':
        _paused = !!d.paused;
        emit(_paused ? 'pause' : 'resume', { paused: _paused });
        emit('pausechange', { paused: _paused });
        break;

      /* ─── Live CSS variable updates from manifest editable changes ─── */
      case 'native_css_vars':
        if (d.vars && typeof d.vars === 'object') {
          var el = document.documentElement;
          for (var k in d.vars) {
            if (d.vars.hasOwnProperty(k)) {
              el.style.setProperty(k, d.vars[k]);
            }
          }
          emit('cssvarchange', d.vars);
        }
        break;

      /* ─── Per-monitor bounds (for local cursor mapping) ─── */
      case 'native_monitor_bounds':
        _monitorBounds = {
          left: Number(d.left) || 0,
          top: Number(d.top) || 0,
          width: Number(d.width) || 1,
          height: Number(d.height) || 1,
        };
        emit('monitorbounds', _monitorBounds);
        break;
    }
  }

  /* ─── Dispatch per-category sysdata subscriptions ─── */
  function dispatchSysdata(sys) {
    // Dynamically iterate all keys present in the sysdata object
    // so that newly-added categories are picked up automatically.
    for (var key in sys) {
      if (!sys.hasOwnProperty(key)) continue;
      if (!(key in _subscribers) || _subscribers[key].length === 0) continue;

      var val = sys[key];
      if (val === undefined || val === null) continue;

      // Change detection: only fire callback when data actually changed
      var json = JSON.stringify(val);
      var hash = djb2(json);
      if (_prevHash[key] === hash) continue;
      _prevHash[key] = hash;

      var cbs = _subscribers[key];
      for (var i = 0; i < cbs.length; i++) {
        try { cbs[i](val); } catch (e) { console.error('[Sentinel] subscriber error (' + key + '):', e); }
      }
    }

    // Fire 'media' virtual subscribers with the media_session from audio
    if (sys.audio && sys.audio.media_session) {
      var mediaVal = sys.audio.media_session;
      var mediaKey = 'media';
      if (mediaKey in _subscribers && _subscribers[mediaKey].length > 0) {
        var mJson = JSON.stringify(mediaVal);
        var mHash = djb2(mJson);
        if (_prevHash[mediaKey] !== mHash) {
          _prevHash[mediaKey] = mHash;
          var mCbs = _subscribers[mediaKey];
          for (var mi = 0; mi < mCbs.length; mi++) {
            try { mCbs[mi](mediaVal); } catch (e) { console.error('[Sentinel] subscriber error (media):', e); }
          }
        }
      }
    }

    // Also fire the wildcard 'sysdata' subscription with the full object
    dispatch('sysdata', sys);
  }

  /* ─── Dispatch per-monitor appdata subscriptions ─── */
  function dispatchAppdata(app) {
    // Fire the full 'appdata' subscription
    dispatch('appdata', app);

    // Fire per-monitor subscriptions: subscribe('appdata:MONITOR_0', cb)
    if (app && typeof app === 'object') {
      for (var monitorId in app) {
        if (!app.hasOwnProperty(monitorId)) continue;
        var subKey = 'appdata:' + monitorId;
        if (!(subKey in _subscribers) || _subscribers[subKey].length === 0) continue;

        var monitorData = app[monitorId];
        var json = JSON.stringify(monitorData);
        var hash = djb2(json);
        if (_prevHash[subKey] === hash) continue;
        _prevHash[subKey] = hash;

        var cbs = _subscribers[subKey];
        for (var i = 0; i < cbs.length; i++) {
          try { cbs[i](monitorData); } catch (e) { console.error('[Sentinel] subscriber error (' + subKey + '):', e); }
        }
      }
    }
  }

  /* ─── Generic dispatch (for appdata wildcard, sysdata wildcard) ─── */
  function dispatch(name, data) {
    var cbs = _subscribers[name];
    if (!cbs || cbs.length === 0) return;
    var json = JSON.stringify(data);
    var hash = djb2(json);
    if (_prevHash[name] === hash) return;
    _prevHash[name] = hash;
    for (var i = 0; i < cbs.length; i++) {
      try { cbs[i](data); } catch (e) { console.error('[Sentinel] subscriber error (' + name + '):', e); }
    }
  }

  /* ─── Event emitter ─── */
  function emit(name, data) {
    var cbs = _eventListeners[name];
    if (!cbs || cbs.length === 0) return;
    for (var i = 0; i < cbs.length; i++) {
      try { cbs[i](data); } catch (e) { console.error('[Sentinel] event error (' + name + '):', e); }
    }
  }

  /* ═══════════════════════════════════════════════
   *  Public API
   * ═══════════════════════════════════════════════ */
  var Sentinel = {

    /** Current SDK version */
    version: '2.0.0',

    /** Whether the wallpaper is currently paused */
    get paused() { return _paused; },

    /** Latest full sysdata snapshot (or null) */
    get sysdata() { return _sysdata; },

    /** Latest full appdata snapshot (or null) */
    get appdata() { return _appdata; },

    /**
     * Latest media session data.
     * Prefers the dedicated sysdata.media entry; falls back to audio.media_session.
     * Contains: playing, title, artist, album, timeline, playback_status, etc.
     * Returns null if no media data is available.
     * @returns {object|null}
     */
    get media() {
      if (!_sysdata) return null;
      if (_sysdata.media) return _sysdata.media;
      if (_sysdata.audio && _sysdata.audio.media_session) return _sysdata.audio.media_session;
      return null;
    },

    /**
     * Monitor bounds for this WebView instance.
     * { left, top, width, height } in virtual-screen pixels, or null.
     * @returns {object|null}
     */
    get monitorBounds() { return _monitorBounds; },

    /**
     * Flat array of display metadata objects (unwrapped from registry entries).
     * Each element is the raw metadata: { id, primary, x, y, width, height, scale, ... }
     * Returns empty array if no display data is available.
     * @returns {Array<object>}
     */
    get displays() {
      if (!_sysdata || !Array.isArray(_sysdata.displays)) return [];
      return _sysdata.displays.map(function (entry) {
        return entry.metadata || entry;
      });
    },

    /**
     * Array of monitor IDs present in appdata.
     * @returns {Array<string>}
     */
    get monitors() {
      if (!_appdata || typeof _appdata !== 'object') return [];
      return Object.keys(_appdata);
    },

    /**
     * Get a specific sysdata category's current cached data.
     * @param {string} category - e.g. 'cpu', 'gpu', 'ram', 'storage', 'network',
     *   'audio', 'media', 'time', 'keyboard', 'mouse', 'power', 'bluetooth',
     *   'wifi', 'system', 'processes', 'idle', 'displays'
     * @returns {object|null}
     */
    get(category) {
      if (category === 'media') return this.media;
      return _sysdata ? (_sysdata[category] || null) : null;
    },

    /**
     * Get a single display's metadata by its ID.
     * @param {string} id - Display ID (e.g. '0', '1')
     * @returns {object|null} The display metadata, or null if not found
     */
    getDisplay(id) {
      if (!_sysdata || !Array.isArray(_sysdata.displays)) return null;
      for (var i = 0; i < _sysdata.displays.length; i++) {
        var entry = _sysdata.displays[i];
        var entryId = entry.id || (entry.metadata && entry.metadata.id);
        if (entryId === id || entryId === String(id)) {
          return entry.metadata || entry;
        }
      }
      return null;
    },

    /**
     * Get the window list for a specific monitor.
     * @param {string} monitorId - Monitor ID as used in appdata keys
     * @returns {Array<object>} Array of window objects, or empty array
     */
    getWindows(monitorId) {
      if (!_appdata || typeof _appdata !== 'object') return [];
      var monitor = _appdata[monitorId];
      if (!monitor || !Array.isArray(monitor.windows)) return [];
      return monitor.windows;
    },

    /**
     * Get the focused window on a specific monitor (or any monitor if omitted).
     * @param {string} [monitorId] - Optional monitor ID to restrict search
     * @returns {object|null} The focused window object, or null
     */
    getFocusedWindow(monitorId) {
      if (!_appdata || typeof _appdata !== 'object') return null;
      var monitors = monitorId ? [monitorId] : Object.keys(_appdata);
      for (var m = 0; m < monitors.length; m++) {
        var mon = _appdata[monitors[m]];
        if (!mon || !Array.isArray(mon.windows)) continue;
        for (var w = 0; w < mon.windows.length; w++) {
          if (mon.windows[w].focused) return mon.windows[w];
        }
      }
      return null;
    },

    /**
     * Subscribe to a data category. The callback fires only when the data
     * for that category has changed since the last update.
     *
     * Categories:
     *   sysdata keys: cpu, gpu, ram, storage, network, audio, media, time,
     *                 keyboard, mouse, power, bluetooth, wifi, system,
     *                 processes, idle, displays
     *   Wildcards:    sysdata (full object), appdata (full object)
     *   Per-monitor:  appdata:MONITOR_ID (e.g. 'appdata:MONITOR_0')
     *
     * @param {string}   category - Data category name
     * @param {function} callback - function(data)
     * @returns {function} Unsubscribe function
     */
    subscribe(category, callback) {
      if (typeof callback !== 'function') {
        throw new Error('[Sentinel] subscribe callback must be a function');
      }
      if (!_subscribers[category]) _subscribers[category] = [];
      _subscribers[category].push(callback);
      publishDemandSections();

      // Return unsubscribe function
      return function unsubscribe() {
        var arr = _subscribers[category];
        if (!arr) return;
        var idx = arr.indexOf(callback);
        if (idx !== -1) arr.splice(idx, 1);
        publishDemandSections();
      };
    },

    /**
    * Listen for lifecycle / notification events.
     *
     * Events:
     *   pause       – { paused: true }
     *   resume      – { paused: false }
     *   pausechange – { paused: bool }
     *   registry    – { sysdata, appdata }  (raw, every update)
     *   cssvarchange – { varName: value, ... }
     *
     * @param {string}   event    - Event name
     * @param {function} callback - function(data)
     * @returns {function} Unsubscribe function
     */
    on(event, callback) {
      if (typeof callback !== 'function') {
        throw new Error('[Sentinel] on() callback must be a function');
      }
      if (!_eventListeners[event]) _eventListeners[event] = [];
      _eventListeners[event].push(callback);

      return function off() {
        var arr = _eventListeners[event];
        if (!arr) return;
        var idx = arr.indexOf(callback);
        if (idx !== -1) arr.splice(idx, 1);
      };
    },

    /**
     * Remove a specific event listener.
     * @param {string}   event
     * @param {function} callback
     */
    off(event, callback) {
      var arr = _eventListeners[event];
      if (!arr) return;
      var idx = arr.indexOf(callback);
      if (idx !== -1) arr.splice(idx, 1);
    },

    /**
     * Remove all subscribers and event listeners.
     */
    clear() {
      for (var k in _subscribers) delete _subscribers[k];
      for (var k in _eventListeners) delete _eventListeners[k];
      for (var k in _prevHash) delete _prevHash[k];
      publishDemandSections();
    },

    /* ─── Utility helpers ─── */

    /**
     * Format bytes to human-readable string.
     * @param {number} bytes
     * @param {number} [decimals=2]
     * @returns {string}
     */
    formatBytes(bytes, decimals) {
      if (bytes == null || isNaN(bytes) || bytes === 0) return '0 B';
      var d = decimals != null ? decimals : 2;
      var units = ['B', 'KB', 'MB', 'GB', 'TB'];
      var i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
      return (bytes / Math.pow(1024, i)).toFixed(d) + ' ' + units[i];
    },

    /**
     * Format a percentage value.
     * @param {number} value
     * @param {number} [decimals=1]
     * @returns {string}
     */
    formatPercent(value, decimals) {
      if (value == null || isNaN(value)) return '—';
      return value.toFixed(decimals != null ? decimals : 1) + '%';
    },

    /**
     * Format bytes/sec to human-readable speed.
     * @param {number} bps
     * @returns {string}
     */
    formatSpeed(bps) {
      if (bps == null || isNaN(bps)) return '—';
      if (bps < 1024) return bps.toFixed(0) + ' B/s';
      if (bps < 1048576) return (bps / 1024).toFixed(1) + ' KB/s';
      return (bps / 1048576).toFixed(2) + ' MB/s';
    },

    /**
     * Format temperature object { average_c, ... } to string.
     * @param {object} temp
     * @returns {string}
     */
    formatTemp(temp) {
      if (temp == null || typeof temp !== 'object') return '—';
      var avg = temp.average_c;
      if (avg == null || avg === 0) return '—';
      return avg.toFixed(1) + '°C';
    }
  };

  /* ═══════════════════════════════════════════════
   *  Wire up WebView2 message channel
   * ═══════════════════════════════════════════════ */
  if (root.chrome && root.chrome.webview && root.chrome.webview.addEventListener) {
    root.chrome.webview.addEventListener('message', function (e) {
      handleMessage(e.data);
    });
  }

  // Also support manual dispatch for testing / other environments
  Sentinel._handleMessage = handleMessage;

  /* ═══════════════════════════════════════════════
   *  Export
   * ═══════════════════════════════════════════════ */
  root.Sentinel = Sentinel;

})(typeof globalThis !== 'undefined' ? globalThis : typeof window !== 'undefined' ? window : this);
