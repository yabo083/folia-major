// src-tauri/src/lib.rs
// Folia Tauri 入口：模块声明 + run() 装配。
// 所有桌面能力按领域拆在独立模块（settings/window/cache/network/kugou/netease/
// handoff/stage/obs/remote），此处只做注册（managed states + invoke_handler）。

mod cache;
mod discord;
mod handoff;
mod kugou;
mod netease;
mod network;
mod obs;
mod remote;
mod settings;
mod stage;
mod thumbar;
mod updater;
mod video_export;
mod voice;
mod window;

const DESKTOP_PWA_CACHE_RESET_SCRIPT: &str = r#"
void (async () => {
  if (!('serviceWorker' in navigator) || !('caches' in window)) return;
  const registrations = await navigator.serviceWorker.getRegistrations();
  const cacheNames = await caches.keys();
  if (registrations.length === 0 && cacheNames.length === 0) return;
  await Promise.all(registrations.map((registration) => registration.unregister()));
  await Promise.all(cacheNames.map((name) => caches.delete(name)));
  location.reload();
})().catch((error) => console.error('[Folia] failed to clear stale desktop PWA cache', error));
"#;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri::Manager;

    let app = tauri::Builder::default()
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Finished
                && payload.url().host_str() == Some("tauri.localhost")
            {
                let _ = webview.eval(DESKTOP_PWA_CACHE_RESET_SCRIPT);
            }
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            window::focus_main_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .map_err(|error| format!("resolve app data directory: {error}"))?;

            let settings = settings::SettingsStore::open(&app_data_dir)?;
            app.manage(settings);

            let kugou = kugou::KugouApiState::open(&app_data_dir);
            app.manage(kugou);

            let netease = netease::NeteaseApiState::new();
            let netease_server = netease::NeteaseApiServer::start(app.handle(), netease.clone())?;
            app.manage(netease);
            app.manage(netease_server);

            app.manage(network::NetworkState::new());
            app.manage(window::MainWindowState::new());
            app.manage(handoff::WindowPlaybackHandoffStore::new());
            app.manage(remote::RemoteControlState::new());
            app.manage(thumbar::ThumbarState::new());
            app.manage(discord::DiscordPresenceController::new(app.handle()));
            app.manage(voice::VoiceInputPauseMonitor::new(app.handle()));

            let stage_state = stage::StageState::new(app.handle(), app_data_dir.clone());
            app.manage(stage_state);
            let obs_state = obs::ObsBrowserSourceState::new(app.handle());
            app.manage(obs_state);

            // M9 视频导出：一次性 write token 存储 + 主窗口 prepare/restore 快照。
            app.manage(video_export::VideoExportTokenStore::new());
            app.manage(video_export::VideoExportWindowState::new());

            // M10 自动更新：状态机（端点/公钥构建期注入，缺失即 fail closed）。
            app.manage(updater::UpdaterState::new(app.handle().clone()));

            window::setup(app);
            thumbar::setup(app.handle());

            // M10 启动更新检查（延迟 4.5s，镜像 electron scheduleStartupUpdateCheck）。
            updater::schedule_startup_check(app.handle());

            if let Some(stage) = app.try_state::<stage::StageState>() {
                let _ = stage.sync_and_serve(app.handle());
            }
            if let Some(obs) = app.try_state::<obs::ObsBrowserSourceState>() {
                let _ = obs.sync_and_serve(app.handle());
            }

            // M8 启动即同步语音输入暂停监控（同 Electron createWindow 后的 syncState）。
            voice::sync_state(app.handle());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // settings
            settings::get_settings,
            settings::save_settings,
            settings::set_app_locale,
            settings::get_cache_directory,
            settings::choose_cache_directory,
            settings::reset_cache_directory,
            // window
            window::window_focus_main,
            window::window_minimize,
            window::window_toggle_maximize,
            window::window_toggle_fullscreen,
            window::window_close,
            window::window_is_maximized,
            window::window_get_transparent_mode,
            window::window_set_transparent_mode,
            window::window_playback_handoff_consume,
            window::window_playback_handoff_submit,
            window::window_set_native_theme,
            window::window_get_click_through,
            window::window_set_click_through,
            window::window_set_click_through_unlock_hover,
            window::window_get_always_on_top,
            window::window_set_always_on_top,
            // obs
            obs::obs_browser_source_get_status,
            obs::obs_browser_source_set_enabled,
            obs::obs_browser_source_regenerate_token,
            obs::obs_browser_source_publish_config,
            obs::obs_browser_source_publish_clock,
            obs::obs_browser_source_publish_audio,
            // stage
            stage::stage_get_status,
            stage::stage_set_enabled,
            stage::stage_regenerate_token,
            stage::stage_clear_state,
            stage::stage_complete_external_play,
            stage::stage_publish_player_snapshot,
            stage::stage_complete_player_control,
            stage::stage_complete_player_queue,
            // remote
            remote::remote_control_open,
            remote::remote_control_toggle,
            remote::remote_control_close,
            remote::remote_control_get_always_on_top,
            remote::remote_control_set_always_on_top,
            remote::remote_control_publish_snapshot,
            remote::remote_control_get_snapshot,
            remote::remote_control_send_command,
            remote::playback_sync_bridge_get_status,
            // network
            network::lyric_proxy_fetch,
            network::open_external_url,
            network::generate_theme,
            // cache
            cache::get_audio_cache,
            cache::has_audio_cache,
            cache::save_audio_cache,
            cache::get_audio_cache_usage,
            cache::get_audio_cache_stats,
            cache::clear_audio_cache,
            cache::get_cover_cache,
            cache::save_cover_cache,
            cache::remove_cover_cache,
            cache::get_cover_cache_usage,
            cache::clear_cover_cache,
            // kugou
            kugou::kugou_api_request,
            kugou::kugou_api_status,
            // netease
            netease::get_netease_port,
            netease::get_netease_api_status,
            // M8: thumbar / Discord / voice input pause
            thumbar::thumbar_update_buttons,
            discord::discord_presence_get_status,
            discord::discord_presence_publish_snapshot,
            voice::voice_input_pause_get_status,
            // M9: video export (save dialog + sentinel source + window prepare/restore + raw write)
            video_export::video_export_choose_path,
            video_export::video_export_get_main_window_source,
            video_export::video_export_prepare_window,
            video_export::video_export_restore_window,
            video_export::video_export_write_file,
            // M10: auto updater (tauri-plugin-updater; contract mirrors electron main.cjs)
            updater::get_update_status,
            updater::updates_check,
            updater::updates_mark_seen,
            updater::updates_open_release_page,
            updater::updates_download,
            updater::updates_quit_and_install,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // M8 退出清理：停止语音轮询、关闭 Discord IPC 连接（best-effort）。
    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            voice::stop(app_handle);
            discord::destroy(app_handle);
        }
    });
}
