#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use minter_node_control::{Controller, Settings, Status};
use std::sync::Arc;
use tauri::{
    Emitter, Manager, State,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_dialog::DialogExt;

type Backend<'a> = State<'a, Arc<Controller>>;

#[tauri::command]
fn load_settings(backend: Backend<'_>) -> Result<Settings, String> {
    backend.settings().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn save_settings(backend: Backend<'_>, settings: Settings) -> Result<(), String> {
    backend.save(settings).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn start_node(backend: Backend<'_>) -> Result<(), String> {
    backend.start().await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn stop_node(backend: Backend<'_>, control_id: Option<String>) -> Result<(), String> {
    backend
        .stop_target(control_id.as_deref())
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn node_status(backend: Backend<'_>) -> Result<Status, String> {
    backend.status().await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn node_logs(backend: Backend<'_>) -> Result<String, String> {
    backend.logs().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn choose_path(app: tauri::AppHandle, directory: bool) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let picker = app.dialog().file();
        let picked = if directory {
            picker.blocking_pick_folder()
        } else {
            picker.blocking_pick_file()
        };
        picked.map(|path| path.to_string())
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn exit_app(
    app: tauri::AppHandle,
    backend: Backend<'_>,
    control_id: Option<String>,
    acknowledged_error: Option<bool>,
) -> Result<(), String> {
    if let Some(id) = control_id {
        backend
            .stop_target(Some(&id))
            .await
            .map_err(|e| format!("{e:#}"))?;
    } else if let Err(error) = backend.stop_owned().await {
        // The UI can acknowledge a displayed terminal error and exit after the
        // child has been joined. An unconfirmed/live shutdown remains an error.
        if !acknowledged_error.unwrap_or(false)
            || backend
                .status()
                .await
                .map_err(|e| format!("{e:#}"))?
                .managed
        {
            return Err(format!("{error:#}"));
        }
    }
    app.exit(0);
    Ok(())
}

fn show(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show(app)))
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let home = std::env::var_os("MINTER_DESKTOP_HOME").map(std::path::PathBuf::from);
            let config_dir = home.clone().unwrap_or(app.path().app_config_dir()?);
            let data_dir = home
                .unwrap_or(app.path().app_local_data_dir()?)
                .join("node");
            let config = config_dir.join("settings.json");
            app.manage(Arc::new(Controller::new(config, data_dir)?));
            let open = MenuItem::with_id(app, "open", "Открыть Minter Node", true, None::<&str>)?;
            let quit =
                MenuItem::with_id(app, "quit", "Остановить ноду и выйти", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;
            TrayIconBuilder::new()
                .icon(
                    app.default_window_icon()
                        .ok_or("Отсутствует значок приложения")?
                        .clone(),
                )
                .tooltip("Minter Node")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show(app),
                    "quit" => {
                        let backend = app.state::<Arc<Controller>>().inner().clone();
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let result = async {
                                let status = backend.status().await?;
                                anyhow::ensure!(
                                    status.phase != "error",
                                    "{}",
                                    status.error.as_deref().unwrap_or("Ошибка ноды")
                                );
                                if let Some(id) = status.control_id.filter(|_| status.controllable)
                                {
                                    backend.stop_target(Some(&id)).await?;
                                } else if status.managed {
                                    backend.stop_owned().await?;
                                }
                                Ok::<_, anyhow::Error>(())
                            }
                            .await;
                            if let Err(error) = result {
                                let message = format!("Остановка ноды: {error:#}");
                                let _ = app.emit("node-control-error", message);
                                show(&app);
                                return;
                            }
                            app.exit(0);
                        });
                    }
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            load_settings,
            save_settings,
            start_node,
            stop_node,
            node_status,
            node_logs,
            choose_path,
            exit_app
        ])
        .run(tauri::generate_context!())
        .expect("Не удалось запустить Minter Node");
}
