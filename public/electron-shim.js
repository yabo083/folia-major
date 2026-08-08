// electron-shim.js
// Tauri 二开的兼容层：把 Electron preload 的 window.electron API 映射到 Tauri 命令。
// 仅当运行在 Tauri WebView 内时生效；普通浏览器环境保持 window.electron 未定义（前端有守卫）。
// 命令尚未实现时返回安全默认值，保证应用可启动，模块逐个实现后自然替换。

(function () {
  if (!window.__TAURI__ || !window.__TAURI__.core) {
    return;
  }
  var invoke = window.__TAURI__.core.invoke;
  var listen = window.__TAURI__.event.listen;

  var NOW = function () { return Date.now(); };

  // 安全默认值表：命令未实现或抛错时兜底，保持应用可启动
  var FALLBACKS = {
    getSettings: function () { return Promise.resolve({}); },
    saveSettings: function () { return Promise.resolve(true); },
    setAppLocale: function () { return Promise.resolve(true); },
    getCacheDirectory: function () { return Promise.resolve({ path: '', isDefault: true }); },
    chooseCacheDirectory: function () { return Promise.resolve(null); },
    resetCacheDirectory: function () { return Promise.resolve(true); },
    getUpdateStatus: function () {
      return Promise.resolve({
        status: 'idle', currentVersion: '0.0.0', availableVersion: null,
        updateUrl: '', error: null, lastCheckedAt: null, downloadProgress: null,
        supported: false, updateCheckSupported: false, updateCheckSupportReason: 'unsupported',
        platform: 'win32', updateCheckEnabled: true, autoUpdateEnabled: false,
        lastSeenVersion: null, updateSeen: false,
      });
    },
    checkForUpdates: function () { return Promise.resolve(this.getUpdateStatus()); },
    markUpdateSeen: function () { return Promise.resolve(this.getUpdateStatus()); },
    openUpdateReleasePage: function () { return Promise.resolve(true); },
    openExternalUrl: function () { return Promise.resolve(true); },
    downloadUpdate: function () { return Promise.resolve(this.getUpdateStatus()); },
    quitAndInstallUpdate: function () { return Promise.resolve(true); },
    getAudioCache: function () { return Promise.resolve(null); },
    hasAudioCache: function () { return Promise.resolve(false); },
    saveAudioCache: function () { return Promise.resolve(true); },
    getAudioCacheUsage: function () { return Promise.resolve(0); },
    getAudioCacheStats: function () { return Promise.resolve(null); },
    clearAudioCache: function () { return Promise.resolve(true); },
    getCoverCache: function () { return Promise.resolve(null); },
    saveCoverCache: function () { return Promise.resolve(true); },
    removeCoverCache: function () { return Promise.resolve(true); },
    getCoverCacheUsage: function () { return Promise.resolve(0); },
    clearCoverCache: function () { return Promise.resolve(true); },
    generateTheme: function () { return Promise.reject(new Error('generate_theme not implemented')); },
    fetchLyricProxy: function () { return Promise.reject(new Error('lyric_proxy_fetch not implemented')); },
    getNeteasePort: function () { return Promise.resolve(30000); },
    getNeteaseApiStatus: function () {
      return Promise.resolve({ status: 'error', port: null, error: 'Netease API not implemented yet', updatedAt: NOW() });
    },
    getKugouApiStatus: function () { return Promise.resolve({ available: false, error: null }); },
    kugouRequest: function () { return Promise.reject(new Error('kugou_api_request not implemented')); },
    minimizeWindow: function () { return Promise.resolve(true); },
    toggleMaximizeWindow: function () { return Promise.resolve(true); },
    toggleFullscreenWindow: function () { return Promise.resolve(true); },
    closeWindow: function () { return Promise.resolve(true); },
    focusMainWindow: function () { return Promise.resolve(true); },
    isWindowMaximized: function () { return Promise.resolve(false); },
    getWindowTransparentMode: function () { return Promise.resolve(false); },
    setWindowTransparentMode: function () { return Promise.resolve(true); },
    consumeWindowPlaybackHandoff: function () { return Promise.resolve(null); },
    submitWindowPlaybackHandoff: function () { return Promise.resolve(true); },
    setNativeTheme: function () { return Promise.resolve(true); },
    getMainWindowClickThroughEnabled: function () { return Promise.resolve(false); },
    setMainWindowClickThroughEnabled: function () { return Promise.resolve(true); },
    setMainWindowClickThroughUnlockHover: function () { return Promise.resolve(true); },
    getMainWindowAlwaysOnTop: function () { return Promise.resolve(false); },
    setMainWindowAlwaysOnTop: function () { return Promise.resolve(true); },
    getObsBrowserSourceStatus: function () {
      return Promise.resolve({ enabled: false, port: 32108, token: null, url: null, clientCount: 0 });
    },
    setObsBrowserSourceEnabled: function () { return Promise.resolve(true); },
    regenerateObsBrowserSourceToken: function () { return Promise.resolve(null); },
    publishObsBrowserSourceConfig: function () { return Promise.resolve(true); },
    publishObsBrowserSourceClock: function () { return Promise.resolve(true); },
    publishObsBrowserSourceAudio: function () { return Promise.resolve(true); },
    getDiscordPresenceStatus: function () {
      return Promise.resolve({
        enabled: false, configured: false, connected: false, error: null,
        applicationId: null, updatedAt: NOW(),
      });
    },
    publishDiscordPresenceSnapshot: function () { return Promise.resolve(true); },
    getPlaybackSyncBridgeStatus: function () {
      return Promise.resolve({ remoteControlOpen: false, discordPresenceEnabled: false });
    },
    getVoiceInputPauseStatus: function () {
      return Promise.resolve({ active: false, enabled: false, supported: false });
    },
    updateTaskbarControls: function () { return Promise.resolve(true); },
    openRemoteControl: function () { return Promise.resolve(true); },
    toggleRemoteControl: function () { return Promise.resolve(true); },
    closeRemoteControl: function () { return Promise.resolve(true); },
    getRemoteControlAlwaysOnTop: function () { return Promise.resolve(true); },
    setRemoteControlAlwaysOnTop: function () { return Promise.resolve(true); },
    publishRemoteControlSnapshot: function () { return Promise.resolve(true); },
    getRemoteControlSnapshot: function () { return Promise.resolve(null); },
    sendRemoteControlCommand: function () { return Promise.resolve(true); },
    chooseVideoExportPath: function () { return Promise.resolve({ canceled: true, filePath: null }); },
    getMainWindowCaptureSource: function () { return Promise.resolve(null); },
    prepareVideoExportWindow: function () { return Promise.resolve(false); },
    restoreVideoExportWindow: function () { return Promise.resolve(false); },
    writeVideoExportFile: function () { return Promise.reject(new Error('video_export_write_file not implemented')); },
    getStageStatus: function () {
      return Promise.resolve({ enabled: false, port: 32107, token: null, url: null, sessionActive: false });
    },
    setStageEnabled: function () { return Promise.resolve(true); },
    regenerateStageToken: function () { return Promise.resolve(null); },
    clearStageState: function () { return Promise.resolve(true); },
    completeStageExternalPlayRequest: function () { return Promise.resolve(true); },
    publishStagePlayerSnapshot: function () { return Promise.resolve(true); },
    completeStagePlayerControlRequest: function () { return Promise.resolve(true); },
    completeStagePlayerQueueRequest: function () { return Promise.resolve(true); },
    debugGetRenderedFonts: function () { return Promise.reject(new Error('debug_get_rendered_fonts not implemented')); },
  };

  // 统一调用：有实现调 Tauri 命令，失败或无实现走兜底
  function call(name, payload, fallbackKey) {
    try {
      return Promise.resolve(invoke(name, payload || {}))
        .catch(function () {
          var fb = FALLBACKS[fallbackKey];
          return fb ? fb.call(FALLBACKS) : undefined;
        });
    } catch (e) {
      var fb2 = FALLBACKS[fallbackKey];
      return fb2 ? fb2.call(FALLBACKS) : Promise.resolve(undefined);
    }
  }

  function on(channel, callback) {
    try {
      var p = listen(channel, function (event) { callback(event.payload); });
      return function () { p.then(function (unlisten) { unlisten(); }); };
    } catch (e) {
      return function () { };
    }
  }

  // ---- M9: 视频导出（getDisplayMedia 适配 + 原始 IPC 写文件）----
  // WebView2 无法提供 Electron desktopCapturer 的 source id，改为：
  // 1. chooseVideoExportPath 在用户手势内**同步**调用 getDisplayMedia
  //    （displaySurface:'window'），但**先等**显示流成功解析，再打开 Rust 保存对话框，
  //    避免窗口选择器与保存对话框重叠；选择器拒绝/取消则不开保存框并如实传播原错误。
  // 2. getMainWindowCaptureSource 返回哨兵 source id（Rust 仅当主窗口存在时返回）。
  // 3. 只拦截"desktop + 哨兵 id"的遗留 getUserMedia 调用并返回当前尝试的流，
  //    其余 getUserMedia 原样放行（绝不改动普通采集）。
  // 4. 写文件走原始 IPC：ArrayBuffer 作为请求体 + token 头，不经 JSON 序列化。
  // 注意：getDisplayMedia 需要瞬时用户激活；远程控制事件触发导出时主窗口无手势，
  // 采集会如实失败（错误信息由渲染层展示），绝不伪造捕获。
  //
  // 每次导出尝试使用独立 session（generation 隔离）：
  //   - 新尝试开始即取消/停掉上一尝试的已解析流；
  //   - 过期尝试的 getDisplayMedia 晚归会立即停掉自己的轨道，且绝不覆盖当前尝试；
  //   - restore/取消只影响当前未消费的尝试；已消费的流归渲染层 stop。
  var VIDEO_EXPORT_SENTINEL_SOURCE_ID = '__folia_tauri_getdisplaymedia_sentinel__';
  var VIDEO_EXPORT_DEFAULT_FRAME_RATE = 60;

  var videoExportAttemptSeq = 0;
  var videoExportSession = null; // 当前尝试会话 { seq, stream, cancelled, consumed }
  var videoExportTokens = Object.create(null);
  var nativeGetUserMedia = null;

  // 停掉尝试持有的未消费流（消费后流归渲染层，此处不再触碰）。
  function stopVideoExportSessionStream(session) {
    if (session && session.stream && !session.consumed) {
      session.stream.getTracks().forEach(function (track) { track.stop(); });
      session.stream = null;
    }
  }

  // 取消指定尝试（仅影响该尝试的未消费流）；若它就是当前尝试则清空当前会话。
  function cancelVideoExportSession(session) {
    if (!session) { return; }
    session.cancelled = true;
    stopVideoExportSessionStream(session);
    if (videoExportSession === session) {
      videoExportSession = null;
    }
  }

  // 取消当前尝试（restore/保存对话框取消路径使用）。
  function cancelVideoExportAttempt() {
    cancelVideoExportSession(videoExportSession);
  }

  // 默认文件名形如 `<title>-<width>x<height>.mp4`：从中解析导出尺寸作为理想值。
  function parseVideoExportIdealSize(defaultName) {
    var match = /-(\d{3,4})x(\d{3,4})\.(?:mp4|webm)$/.exec(String(defaultName || ''));
    if (match) {
      var width = parseInt(match[1], 10);
      var height = parseInt(match[2], 10);
      if (width >= 320 && height >= 320) {
        return { width: Math.min(width, 3840), height: Math.min(height, 3840) };
      }
    }
    return { width: 1920, height: 1080 };
  }

  // 同步开始一次捕获尝试（保持瞬时用户激活）：
  // 取消上一尝试并停其已解析流，然后同步调用 getDisplayMedia。
  // 返回 { session, streamPromise }：streamPromise 只在当前尝试仍有效且未取消时
  // 解析为流；过期待 resolve 立即停掉自身轨道（reject 原样传播）。
  function beginVideoExportAttempt(defaultName) {
    var previous = videoExportSession;
    if (previous) {
      previous.cancelled = true;
      stopVideoExportSessionStream(previous);
    }
    var session = {
      seq: ++videoExportAttemptSeq,
      stream: null,
      cancelled: false,
      consumed: false,
    };
    videoExportSession = session;

    var streamPromise;
    if (!navigator || !navigator.mediaDevices || typeof navigator.mediaDevices.getDisplayMedia !== 'function') {
      streamPromise = Promise.reject(new Error('Display capture (getDisplayMedia) is not available in this webview.'));
    } else {
      var ideal = parseVideoExportIdealSize(defaultName);
      try {
        streamPromise = Promise.resolve(navigator.mediaDevices.getDisplayMedia({
          video: {
            displaySurface: 'window',
            width: { ideal: ideal.width },
            height: { ideal: ideal.height },
            frameRate: { ideal: VIDEO_EXPORT_DEFAULT_FRAME_RATE },
          },
          audio: false,
        }));
      } catch (e) {
        streamPromise = Promise.reject(e);
      }
    }

    // 守卫链：晚归的过期/已取消尝试立即停掉自己的轨道，绝不覆盖当前尝试。
    var guarded = streamPromise.then(function (stream) {
      if (videoExportSession !== session) {
        if (stream) {
          stream.getTracks().forEach(function (track) { track.stop(); });
        }
        return null;
      }
      if (session.cancelled) {
        if (stream) {
          stream.getTracks().forEach(function (track) { track.stop(); });
        }
        return null;
      }
      session.stream = stream;
      return stream;
    });
    guarded.catch(function () { /* 防未处理拒绝；真实错误由调用方 await 传播 */ });
    return { session: session, streamPromise: guarded };
  }

  // 仅当遗留约束是"desktop + 哨兵 id"时才视为视频导出的捕获请求。
  function isVideoExportDesktopRequest(constraints) {
    if (!constraints || typeof constraints !== 'object') { return false; }
    var video = constraints.video;
    if (!video || typeof video !== 'object') { return false; }
    var mandatory = video.mandatory;
    if (!mandatory || typeof mandatory !== 'object') { return false; }
    return mandatory.chromeMediaSource === 'desktop'
      && mandatory.chromeMediaSourceId === VIDEO_EXPORT_SENTINEL_SOURCE_ID;
  }

  // 从遗留 mandatory 约束提取合理理想值，best-effort 应用到已解析的显示轨道。
  function tryApplyVideoExportTrackConstraints(stream, constraints) {
    var mandatory = constraints && constraints.video && constraints.video.mandatory;
    if (!mandatory) { return; }
    var width = mandatory.maxWidth || mandatory.minWidth;
    var height = mandatory.maxHeight || mandatory.minHeight;
    var frameRate = mandatory.maxFrameRate;
    var ideal = {};
    if (typeof width === 'number' && width > 0) { ideal.width = { ideal: Math.round(width) }; }
    if (typeof height === 'number' && height > 0) { ideal.height = { ideal: Math.round(height) }; }
    if (typeof frameRate === 'number' && frameRate > 0) { ideal.frameRate = { ideal: frameRate }; }
    if (Object.keys(ideal).length === 0) { return; }
    stream.getVideoTracks().forEach(function (track) {
      try { track.applyConstraints(ideal).catch(function () {}); } catch (e) { /* best effort */ }
    });
  }

  function videoExportGetUserMedia(constraints) {
    if (!isVideoExportDesktopRequest(constraints)) {
      return nativeGetUserMedia.apply(navigator.mediaDevices, arguments);
    }
    var session = videoExportSession;
    if (!session) {
      return Promise.reject(new Error('Video export display capture was never started.'));
    }
    if (session.cancelled || !session.stream) {
      return Promise.reject(new Error('Video export display capture failed or was cancelled.'));
    }
    tryApplyVideoExportTrackConstraints(session.stream, constraints);
    session.consumed = true; // 流移交给渲染层，此后由渲染层 stop
    return Promise.resolve(session.stream);
  }

  if (navigator && navigator.mediaDevices && typeof navigator.mediaDevices.getUserMedia === 'function') {
    nativeGetUserMedia = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
    try {
      Object.defineProperty(navigator.mediaDevices, 'getUserMedia', {
        configurable: true,
        writable: true,
        value: videoExportGetUserMedia,
      });
    } catch (e) {
      navigator.mediaDevices.getUserMedia = videoExportGetUserMedia;
    }
  }

  // M9 严格调用：真实 invoke，拒绝不吞错（渲染层 try/catch 负责），不做假成功兜底。
  function videoExportInvoke(name, payload) {
    try {
      return Promise.resolve(invoke(name, payload || {}));
    } catch (e) {
      return Promise.reject(e);
    }
  }

  // 严格调用（远程控制域）：真实 invoke，拒绝不吞错。
  // 远程控制命令已经有真实实现，绝不兜底成假成功（否则渲染层永远不知道命令失败）。
  function strictInvoke(name, payload) {
    return videoExportInvoke(name, payload);
  }

  window.electron = {
    getSettings: function () { return call('get_settings', {}, 'getSettings'); },
    saveSettings: function (key, value) { return call('save_settings', { key: key, value: value }, 'saveSettings'); },
    setAppLocale: function (localeKey) { return call('set_app_locale', { locale: localeKey }, 'setAppLocale'); },
    getCacheDirectory: function () { return call('get_cache_directory', {}, 'getCacheDirectory'); },
    chooseCacheDirectory: function () { return call('choose_cache_directory', {}, 'chooseCacheDirectory'); },
    resetCacheDirectory: function () { return call('reset_cache_directory', {}, 'resetCacheDirectory'); },
    getUpdateStatus: function () { return call('get_update_status', {}, 'getUpdateStatus'); },
    checkForUpdates: function () { return call('updates_check', {}, 'checkForUpdates'); },
    markUpdateSeen: function (version) { return call('updates_mark_seen', { version: version }, 'markUpdateSeen'); },
    openUpdateReleasePage: function (version) { return call('updates_open_release_page', { version: version }, 'openUpdateReleasePage'); },
    openExternalUrl: function (url) { return call('open_external_url', { url: url }, 'openExternalUrl'); },
    downloadUpdate: function () { return call('updates_download', {}, 'downloadUpdate'); },
    quitAndInstallUpdate: function () { return call('updates_quit_and_install', {}, 'quitAndInstallUpdate'); },
    onUpdateStatusChanged: function (cb) { return on('update-status-changed', cb); },
    getAudioCache: function (cacheKey) { return call('get_audio_cache', { cacheKey: cacheKey }, 'getAudioCache'); },
    hasAudioCache: function (cacheKey) { return call('has_audio_cache', { cacheKey: cacheKey }, 'hasAudioCache'); },
    saveAudioCache: function (cacheKey, data, mimeType) { return call('save_audio_cache', { cacheKey: cacheKey, data: data, mimeType: mimeType }, 'saveAudioCache'); },
    getAudioCacheUsage: function () { return call('get_audio_cache_usage', {}, 'getAudioCacheUsage'); },
    getAudioCacheStats: function () { return call('get_audio_cache_stats', {}, 'getAudioCacheStats'); },
    clearAudioCache: function () { return call('clear_audio_cache', {}, 'clearAudioCache'); },
    getCoverCache: function (cacheKey) { return call('get_cover_cache', { cacheKey: cacheKey }, 'getCoverCache'); },
    saveCoverCache: function (cacheKey, data, mimeType) { return call('save_cover_cache', { cacheKey: cacheKey, data: data, mimeType: mimeType }, 'saveCoverCache'); },
    removeCoverCache: function (cacheKey) { return call('remove_cover_cache', { cacheKey: cacheKey }, 'removeCoverCache'); },
    getCoverCacheUsage: function () { return call('get_cover_cache_usage', {}, 'getCoverCacheUsage'); },
    clearCoverCache: function () { return call('clear_cover_cache', {}, 'clearCoverCache'); },
    generateTheme: function (lyricsText, options) { return call('generate_theme', { lyricsText: lyricsText, options: options }, 'generateTheme'); },
    fetchLyricProxy: function (url, init) { return call('lyric_proxy_fetch', { url: url, init: init }, 'fetchLyricProxy'); },
    getNeteasePort: function () { return call('get_netease_port', {}, 'getNeteasePort'); },
    getNeteaseApiStatus: function () { return call('get_netease_api_status', {}, 'getNeteaseApiStatus'); },
    onNeteaseApiStatusChanged: function (cb) { return on('netease-api-status-changed', cb); },
    getKugouApiStatus: function () { return call('kugou_api_status', {}, 'getKugouApiStatus'); },
    kugouRequest: function (operation, params) { return call('kugou_api_request', { operation: operation, params: params }, 'kugouRequest'); },
    minimizeWindow: function () { return call('window_minimize', {}, 'minimizeWindow'); },
    toggleMaximizeWindow: function () { return call('window_toggle_maximize', {}, 'toggleMaximizeWindow'); },
    toggleFullscreenWindow: function () { return call('window_toggle_fullscreen', {}, 'toggleFullscreenWindow'); },
    closeWindow: function () { return call('window_close', {}, 'closeWindow'); },
    focusMainWindow: function () { return call('window_focus_main', {}, 'focusMainWindow'); },
    isWindowMaximized: function () { return call('window_is_maximized', {}, 'isWindowMaximized'); },
    getWindowTransparentMode: function () { return call('window_get_transparent_mode', {}, 'getWindowTransparentMode'); },
    setWindowTransparentMode: function (enabled, handoff) { return call('window_set_transparent_mode', { enabled: enabled, handoff: handoff }, 'setWindowTransparentMode'); },
    consumeWindowPlaybackHandoff: function () { return call('window_playback_handoff_consume', {}, 'consumeWindowPlaybackHandoff'); },
    submitWindowPlaybackHandoff: function (requestId, handoff) { return call('window_playback_handoff_submit', { requestId: requestId, handoff: handoff }, 'submitWindowPlaybackHandoff'); },
    onWindowPlaybackHandoffRequested: function (cb) { return on('window-playback-handoff-requested', cb); },
    setNativeTheme: function (themeSource) { return call('window_set_native_theme', { themeSource: themeSource }, 'setNativeTheme'); },
    getMainWindowClickThroughEnabled: function () { return call('window_get_click_through', {}, 'getMainWindowClickThroughEnabled'); },
    setMainWindowClickThroughEnabled: function (enabled) { return call('window_set_click_through', { enabled: enabled }, 'setMainWindowClickThroughEnabled'); },
    setMainWindowClickThroughUnlockHover: function (active) { return call('window_set_click_through_unlock_hover', { active: active }, 'setMainWindowClickThroughUnlockHover'); },
    getMainWindowAlwaysOnTop: function () { return call('window_get_always_on_top', {}, 'getMainWindowAlwaysOnTop'); },
    setMainWindowAlwaysOnTop: function (enabled) { return call('window_set_always_on_top', { enabled: enabled }, 'setMainWindowAlwaysOnTop'); },
    onMainWindowClickThroughChanged: function (cb) { return on('main-window-click-through-changed', cb); },
    getObsBrowserSourceStatus: function () { return call('obs_browser_source_get_status', {}, 'getObsBrowserSourceStatus'); },
    setObsBrowserSourceEnabled: function (enabled) { return call('obs_browser_source_set_enabled', { enabled: enabled }, 'setObsBrowserSourceEnabled'); },
    regenerateObsBrowserSourceToken: function () { return call('obs_browser_source_regenerate_token', {}, 'regenerateObsBrowserSourceToken'); },
    publishObsBrowserSourceConfig: function (config) { return call('obs_browser_source_publish_config', { config: config }, 'publishObsBrowserSourceConfig'); },
    publishObsBrowserSourceClock: function (clock) { return call('obs_browser_source_publish_clock', { clock: clock }, 'publishObsBrowserSourceClock'); },
    publishObsBrowserSourceAudio: function (audio) { return call('obs_browser_source_publish_audio', { audio: audio }, 'publishObsBrowserSourceAudio'); },
    getDiscordPresenceStatus: function () { return call('discord_presence_get_status', {}, 'getDiscordPresenceStatus'); },
    publishDiscordPresenceSnapshot: function (snapshot) { return call('discord_presence_publish_snapshot', { snapshot: snapshot }, 'publishDiscordPresenceSnapshot'); },
    getPlaybackSyncBridgeStatus: function () { return call('playback_sync_bridge_get_status', {}, 'getPlaybackSyncBridgeStatus'); },
    getVoiceInputPauseStatus: function () { return call('voice_input_pause_get_status', {}, 'getVoiceInputPauseStatus'); },
    onVoiceInputStateChanged: function (cb) { return on('voice-input-state-changed', cb); },
    onPlaybackSyncBridgeStatusChanged: function (cb) { return on('playback-sync-bridge-status-changed', cb); },
    onDiscordPresenceStatusChanged: function (cb) { return on('discord-presence-status-changed', cb); },
    onObsBrowserSourceStatusChanged: function (cb) { return on('obs-browser-source-status-changed', cb); },
    updateTaskbarControls: function (state) { return call('thumbar_update_buttons', { state: state }, 'updateTaskbarControls'); },
    onTaskbarControl: function (cb) { return on('thumbar-action', cb); },
    openRemoteControl: function () { return strictInvoke('remote_control_open', {}); },
    toggleRemoteControl: function () { return strictInvoke('remote_control_toggle', {}); },
    closeRemoteControl: function () { return strictInvoke('remote_control_close', {}); },
    getRemoteControlAlwaysOnTop: function () { return strictInvoke('remote_control_get_always_on_top', {}); },
    setRemoteControlAlwaysOnTop: function (alwaysOnTop) { return strictInvoke('remote_control_set_always_on_top', { alwaysOnTop: alwaysOnTop }); },
    publishRemoteControlSnapshot: function (snapshot) { return strictInvoke('remote_control_publish_snapshot', { snapshot: snapshot }); },
    getRemoteControlSnapshot: function () { return strictInvoke('remote_control_get_snapshot', {}); },
    sendRemoteControlCommand: function (command) { return strictInvoke('remote_control_send_command', { command: command }); },
    onRemoteControlCommand: function (cb) { return on('remote-control-command', cb); },
    onRemoteControlSnapshot: function (cb) { return on('remote-control-snapshot', cb); },
    chooseVideoExportPath: function (defaultName, extension, displayName) {
      // 同步启动 getDisplayMedia（利用用户手势），但先等显示流解析成功，
      // 再打开保存对话框：选择器与保存框不重叠；选择器拒绝/取消不开保存框并如实报错。
      var attempt = beginVideoExportAttempt(defaultName);
      return attempt.streamPromise.then(function (stream) {
        if (!stream) {
          throw new Error('Video export display capture failed or was cancelled.');
        }
        return videoExportInvoke('video_export_choose_path', { defaultName: defaultName, extension: extension, displayName: displayName });
      }).then(function (result) {
        result = result || {};
        if (result.canceled || !result.filePath) {
          videoExportTokens = Object.create(null); // 取消：清空本会话 token
          cancelVideoExportAttempt(); // 停掉未消费流
          return { canceled: true, filePath: null };
        }
        videoExportTokens = Object.create(null); // 新会话：token 按 filePath 键控
        if (result.writeToken) {
          videoExportTokens[result.filePath] = result.writeToken;
        }
        return { canceled: false, filePath: result.filePath };
      }, function (error) {
        // 只取消本尝试，绝不影响可能已开始的新尝试。
        cancelVideoExportSession(attempt.session);
        throw error;
      });
    },
    getMainWindowCaptureSource: function () {
      // 真实命令结果逐字返回（null/哨兵），拒绝原样传播，绝不伪造成功。
      return videoExportInvoke('video_export_get_main_window_source', {});
    },
    prepareVideoExportWindow: function (size) {
      return videoExportInvoke('video_export_prepare_window', { size: size })
        .then(function (result) { return result === true; });
    },
    restoreVideoExportWindow: function () {
      // 渲染层以 void 调用（fire-and-forget 清理），不接受拒绝；如实返回。
      cancelVideoExportAttempt(); // 仅影响当前未消费尝试；已消费的流由渲染层 stop
      return videoExportInvoke('video_export_restore_window', {}).then(
        function (result) { return result === true; },
        function () { return false; }
      );
    },
    writeVideoExportFile: function (filePath, data) {
      // 原始 IPC：data（ArrayBuffer/Uint8Array）作为请求体，token 放在 ASCII 请求头。
      var token = videoExportTokens[filePath];
      if (!token) {
        return Promise.reject(new Error('No active video export write token for the selected path.'));
      }
      return Promise.resolve().then(function () {
        return invoke('video_export_write_file', data, {
          headers: { 'X-Folia-Video-Export-Token': token }
        });
      }).then(function (result) {
        delete videoExportTokens[filePath];
        return result === true || result === null || result === undefined ? true : Boolean(result);
      });
    },
    getStageStatus: function () { return call('stage_get_status', {}, 'getStageStatus'); },
    setStageEnabled: function (enabled) { return call('stage_set_enabled', { enabled: enabled }, 'setStageEnabled'); },
    regenerateStageToken: function () { return call('stage_regenerate_token', {}, 'regenerateStageToken'); },
    clearStageState: function () { return call('stage_clear_state', {}, 'clearStageState'); },
    completeStageExternalPlayRequest: function (result) { return call('stage_complete_external_play', { result: result }, 'completeStageExternalPlayRequest'); },
    publishStagePlayerSnapshot: function (snapshot, options) { return call('stage_publish_player_snapshot', { snapshot: snapshot, options: options }, 'publishStagePlayerSnapshot'); },
    completeStagePlayerControlRequest: function (result) { return call('stage_complete_player_control', { result: result }, 'completeStagePlayerControlRequest'); },
    completeStagePlayerQueueRequest: function (result) { return call('stage_complete_player_queue', { result: result }, 'completeStagePlayerQueueRequest'); },
    onStageSessionUpdated: function (cb) { return on('stage-session-updated', cb); },
    onStageSessionCleared: function (cb) { return on('stage-session-cleared', cb); },
    onStageExternalPlayRequest: function (cb) { return on('stage-external-play-request', cb); },
    onStagePlayerControlRequest: function (cb) { return on('stage-player-control-request', cb); },
    onStagePlayerQueueRequest: function (cb) { return on('stage-player-queue-request', cb); },
    debugGetRenderedFonts: function (selector) { return call('debug_get_rendered_fonts', { selector: selector }, 'debugGetRenderedFonts'); },
  };

  // ---- M5: WebView2 CORS 中继 ----
  // 页面源为 tauri://localhost，跨源 fetch 会被 CORS 拦截。对白名单内的目标 host，
  // 把 fetch 改写为走 lyric_proxy_fetch 命令（Rust 侧无 CORS）；白名单外原样放行。
  // 仅白名单中继，不做黑名单绕过；网易云 localhost 等请求走原生 fetch（自带 CORS 头）。
  var LYRIC_PROXY_RELAY_HOSTS = function (hostname) {
    hostname = String(hostname).toLowerCase();
    return hostname === 'qq.com'
      || hostname.endsWith('.qq.com')
      || hostname === 'y.gtimg.cn'
      || hostname === 'kugou.com'
      || hostname.endsWith('.kugou.com')
      || hostname === 'kgimg.com'
      || hostname.endsWith('.kgimg.com')
      || hostname === 'amll-ttml-db.stevexmh.net';
  };

  function collectHeaders(headers, target) {
    if (!headers) return target;
    if (typeof headers.forEach === 'function') {
      headers.forEach(function (value, key) { target[key] = value; });
    } else if (typeof headers === 'object') {
      for (var key in headers) {
        if (Object.prototype.hasOwnProperty.call(headers, key) && typeof headers[key] === 'string') {
          target[key] = headers[key];
        }
      }
    }
    return target;
  }

  // 把 fetch init（含 Request 对象上的方法/头/体）归一为 lyric_proxy_fetch 的 init 形状。
  function buildLyricProxyInit(input, init) {
    var out = {};
    var method = init && typeof init.method === 'string' ? init.method : null;
    var headers = init && init.headers ? init.headers : null;
    var body = init && typeof init.body === 'string' ? init.body : null;
    if (input && typeof input === 'object' && typeof input.url === 'string') {
      if (!method && typeof input.method === 'string') method = input.method;
      if (!headers && input.headers) headers = input.headers;
      if (!body && typeof input.body === 'string') body = input.body;
    }
    if (method) out.method = method;
    if (body) out.body = body;
    var mergedHeaders = {};
    collectHeaders(headers, mergedHeaders);
    var headerKeys = Object.keys(mergedHeaders);
    if (headerKeys.length) out.headers = mergedHeaders;
    return out;
  }

  // 把命令结果包装成 fetch 兼容的 Response：ok/status/statusText/headers/json()/text()/arrayBuffer()/blob()。
  // 二进制保真：bodyData（base64）优先用于 arrayBuffer()/blob()；bodyText 用于 text()/json()。
  function decodeBase64Bytes(base64) {
    var binary = atob(base64);
    var length = binary.length;
    var bytes = new Uint8Array(length);
    for (var i = 0; i < length; i++) {
      bytes[i] = binary.charCodeAt(i);
    }
    return bytes;
  }

  function toLyricProxyResponse(result) {
    var headerObj = (result && typeof result.headers === 'object' && result.headers) || {};
    var headers = new Headers();
    Object.keys(headerObj).forEach(function (key) {
      try { headers.set(key, String(headerObj[key])); } catch (e) { /* 忽略非法头名 */ }
    });
    var status = Number(result && result.status) || 0;
    var statusText = result && typeof result.statusText === 'string' ? result.statusText : '';
    var bodyText = result && typeof result.bodyText === 'string' ? result.bodyText : '';
    var bodyBytes = typeof result.bodyData === 'string' && result.bodyData
      ? decodeBase64Bytes(result.bodyData)
      : new TextEncoder().encode(bodyText);
    var bodyBuffer = bodyBytes.buffer.slice(bodyBytes.byteOffset, bodyBytes.byteOffset + bodyBytes.byteLength);
    return {
      ok: status >= 200 && status < 300,
      status: status,
      statusText: statusText,
      headers: headers,
      redirected: false,
      type: 'basic',
      url: '',
      clone: function () { return toLyricProxyResponse(result); },
      text: function () { return Promise.resolve(bodyText); },
      json: function () {
        return Promise.resolve().then(function () { return JSON.parse(bodyText); });
      },
      arrayBuffer: function () { return Promise.resolve(bodyBuffer); },
      blob: function () { return Promise.resolve(new Blob([bodyBuffer])); },
    };
  }

  var nativeFetch = window.fetch;
  window.fetch = function (input, init) {
    var urlStr = typeof input === 'string' ? input : (input && typeof input === 'object' && typeof input.url === 'string' ? input.url : null);
    if (urlStr) {
      try {
        var parsed = new URL(urlStr, window.location.href);
        if ((parsed.protocol === 'http:' || parsed.protocol === 'https:')
          && LYRIC_PROXY_RELAY_HOSTS(parsed.hostname)) {
          return window.electron.fetchLyricProxy(urlStr, buildLyricProxyInit(input, init))
            .then(toLyricProxyResponse);
        }
      } catch (e) {
        // URL 解析失败按原生 fetch 处理
      }
    }
    return nativeFetch.apply(this, arguments);
  };
})();
