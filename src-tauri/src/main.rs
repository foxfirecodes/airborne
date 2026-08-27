#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use pr_watcher::{
    alerts::NativeAlertSink,
    commands::{self, AppState},
    store::Store,
    tray,
};
use std::sync::Arc;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WindowEvent,
};
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(move |app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db = data_dir.join("pr-watcher.sqlite3");
            let state = AppState::new(
                Arc::new(Store::open(db.to_str().ok_or("invalid database path")?)?),
                Arc::new(NativeAlertSink {
                    app: app.handle().clone(),
                }),
            );
            let poller = state.poller.clone();
            app.manage(state);
            let open =
                MenuItem::with_id(app, "open-dashboard", "Open dashboard", true, None::<&str>)?;
            let unread =
                MenuItem::with_id(app, "unread-alerts", "Unread alerts", true, None::<&str>)?;
            let watches = MenuItem::with_id(
                app,
                "watched-prs",
                "Watched pull requests",
                true,
                None::<&str>,
            )?;
            let refresh = MenuItem::with_id(app, "refresh-now", "Refresh now", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&unread, &watches, &refresh, &open, &quit])?;
            let unread_item = unread.clone();
            let watches_item = watches.clone();
            TrayIconBuilder::with_id("pr-watcher-tray")
                .menu(&menu)
                .icon(tray::normal_icon())
                .tooltip("PR Watcher")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open-dashboard" | "unread-alerts" | "watched-prs" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "refresh-now" => {
                        let poller = app.state::<AppState>().poller.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = poller.refresh().await;
                        });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            tauri::async_runtime::spawn(async move { poller.run_forever().await });
            let blink_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut state = tray::TrayState::default();
                loop {
                    let unread = blink_app
                        .state::<AppState>()
                        .store
                        .unread_count()
                        .unwrap_or(0);
                    let watches = blink_app
                        .state::<AppState>()
                        .store
                        .watches()
                        .map(|items| items.len())
                        .unwrap_or(0);
                    let _ = unread_item.set_text(format!("Unread alerts ({unread})"));
                    let _ = watches_item.set_text(format!("Watched pull requests ({watches})"));
                    state.set_unread(unread);
                    if let Some(icon) = blink_app.tray_by_id("pr-watcher-tray") {
                        let image = if state.tick() {
                            tray::attention_icon()
                        } else {
                            tray::normal_icon()
                        };
                        let _ = icon.set_icon(Some(image));
                        let _ = icon.set_tooltip(Some(if unread == 0 {
                            "PR Watcher".into()
                        } else {
                            format!("PR Watcher — {unread} unread alerts")
                        }));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_dashboard,
            commands::add_watch,
            commands::create_rule,
            commands::update_rule,
            commands::delete_rule,
            commands::refresh_now,
            commands::mark_alert_read,
            commands::mark_all_alerts_read,
            commands::get_settings,
            commands::save_settings,
            commands::open_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running PR Watcher");
}
