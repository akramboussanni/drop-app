#![deny(unused_must_use)]
#![feature(fn_traits)]
#![feature(duration_constructors)]
#![feature(duration_millis_float)]
#![feature(iterator_try_collect)]
#![feature(nonpoison_mutex)]
#![feature(sync_nonpoison)]
#![deny(clippy::all)]

use std::{
    collections::HashMap, env, fs::File, io::Write, panic::PanicHookInfo, path::Path, str::FromStr,
    sync::nonpoison::Mutex, time::SystemTime,
};

use ::client::{app_status::AppStatus, autostart::sync_autostart_on_startup, user::User};
use ::download_manager::DownloadManagerWrapper;
use ::games::{library::Game, scan::scan_install_dirs};
use ::process::ProcessManagerWrapper;
use ::remote::{
    auth::{self, HandshakeRequestBody, HandshakeResponse, generate_authorization_header},
    cache::clear_cached_object,
    error::RemoteAccessError,
    fetch_object::fetch_object_wrapper,
    offline,
    server_proto::{handle_server_proto_offline_wrapper, handle_server_proto_wrapper},
    utils::DROP_CLIENT_ASYNC,
};
use database::{
    DB, GameDownloadStatus, borrow_db_checked, borrow_db_mut_checked, db::DATA_ROOT_DIR,
    interface::DatabaseImpls,
};
use log::{LevelFilter, debug, info, warn};
use log4rs::{
    Config,
    append::{console::ConsoleAppender, file::FileAppender},
    config::{Appender, Root},
    encode::pattern::PatternEncoder,
};
use serde::Serialize;
use tauri::{
    AppHandle, Manager, RunEvent, WindowEvent,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_dialog::DialogExt;
use url::Url;
use utils::app_emit;

use crate::client::cleanup_and_exit;

mod client;
mod collections;
mod download_manager;
mod downloads;
mod games;
mod process;
mod remote;
mod settings;

use client::*;
use collections::*;
use download_manager::*;
use downloads::*;
use games::*;
use process::*;
use remote::*;
use settings::*;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    status: AppStatus,
    user: Option<User>,
    games: HashMap<String, Game>,
}

async fn setup(handle: AppHandle) -> AppState {
    let logfile = FileAppender::builder()
        .encoder(Box::new(PatternEncoder::new(
            "{d} | {l} | {f}:{L} - {m}{n}",
        )))
        .append(false)
        .build(DATA_ROOT_DIR.join("./drop.log"))
        .expect("Failed to setup logfile");

    let console = ConsoleAppender::builder()
        .encoder(Box::new(PatternEncoder::new(
            "{d} | {l} | {f}:{L} - {m}{n}",
        )))
        .build();

    let log_level = env::var("RUST_LOG").unwrap_or(String::from("Info"));

    let config = Config::builder()
        .appenders(vec![
            Appender::builder().build("logfile", Box::new(logfile)),
            Appender::builder().build("console", Box::new(console)),
        ])
        .build(
            Root::builder()
                .appenders(vec!["logfile", "console"])
                .build(LevelFilter::from_str(&log_level).expect("Invalid log level")),
        )
        .expect("Failed to build config");

    log4rs::init_config(config).expect("Failed to initialise log4rs");

    let games = HashMap::new();

    ProcessManagerWrapper::init(handle.clone());
    DownloadManagerWrapper::init(handle.clone());

    debug!("checking if database is set up");
    let is_set_up = DB.database_is_set_up();

    scan_install_dirs();

    if !is_set_up {
        return AppState {
            status: AppStatus::NotConfigured,
            user: None,
            games,
        };
    }

    debug!("database is set up");

    // TODO: Account for possible failure
    let (app_status, user) = auth::setup().await;

    let db_handle = borrow_db_checked();
    let mut missing_games = Vec::new();
    let statuses = db_handle.applications.game_statuses.clone();
    drop(db_handle);

    for (game_id, status) in statuses {
        match status {
            GameDownloadStatus::Remote {} => {}
            GameDownloadStatus::PartiallyInstalled { .. } => {}
            GameDownloadStatus::SetupRequired {
                version_name: _,
                install_dir,
            } => {
                let install_dir_path = Path::new(&install_dir);
                if !install_dir_path.exists() {
                    missing_games.push(game_id);
                }
            }
            GameDownloadStatus::Installed {
                version_name: _,
                install_dir,
            } => {
                let install_dir_path = Path::new(&install_dir);
                if !install_dir_path.exists() {
                    missing_games.push(game_id);
                }
            }
        }
    }

    info!("detected games missing: {missing_games:?}");

    let mut db_handle = borrow_db_mut_checked();
    for game_id in missing_games {
        db_handle
            .applications
            .game_statuses
            .entry(game_id)
            .and_modify(|v| *v = GameDownloadStatus::Remote {});
    }

    drop(db_handle);

    debug!("finished setup!");

    // Sync autostart state
    if let Err(e) = sync_autostart_on_startup(&handle) {
        warn!("failed to sync autostart state: {e}");
    }

    AppState {
        status: app_status,
        user,
        games,
    }
}

pub fn custom_panic_handler(e: &PanicHookInfo) -> Option<()> {
    let crash_file = DATA_ROOT_DIR.join(format!(
        "crash-{}.log",
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs()
    ));
    let mut file = File::create_new(crash_file).ok()?;
    file.write_all(format!("Drop crashed with the following panic:\n{e}").as_bytes())
        .ok()?;
    drop(file);

    Some(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    std::panic::set_hook(Box::new(|e| {
        let _ = custom_panic_handler(e);
        println!("{e}");
    }));

    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_dialog::init());

    #[cfg(desktop)]
    #[allow(unused_variables)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|_app, argv, _cwd| {
            // when defining deep link schemes at runtime, you must also check `argv` here
        }));
    }

    let app = builder
        .plugin(tauri_plugin_deep_link::init())
        .invoke_handler(tauri::generate_handler![
            // Core utils
            fetch_state,
            quit,
            fetch_system_data,
            // User utils
            update_settings,
            fetch_settings,
            // Auth
            auth_initiate,
            auth_initiate_code,
            retry_connect,
            manual_recieve_handshake,
            sign_out,
            // Remote
            use_remote,
            gen_drop_url,
            fetch_drop_object,
            // Library
            fetch_library,
            fetch_game,
            add_download_dir,
            delete_download_dir,
            fetch_download_dir_stats,
            fetch_game_status,
            fetch_game_version_options,
            update_game_configuration,
            // Collections
            fetch_collections,
            fetch_collection,
            create_collection,
            add_game_to_collection,
            delete_collection,
            delete_game_in_collection,
            // Downloads
            download_game,
            resume_download,
            move_download_in_queue,
            pause_downloads,
            resume_downloads,
            cancel_game,
            uninstall_game,
            // Processes
            launch_game,
            kill_game,
            toggle_autostart,
            get_autostart_enabled,
            open_process_logs
        ])
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimize"]),
        ))
        .setup(|app| {
            let handle = app.handle().clone();

            tauri::async_runtime::block_on(async move {
                let state = setup(handle).await;
                info!("initialized drop client");
                app.manage(Mutex::new(state));

                {
                    use tauri_plugin_deep_link::DeepLinkExt;
                    let _ = app.deep_link().register_all();
                    debug!("registered all pre-defined deep links");
                }

                let handle = app.handle().clone();

                let _main_window = tauri::WebviewWindowBuilder::new(
                    &handle,
                    "main", // BTW this is not the name of the window, just the label. Keep this 'main', there are permissions & configs that depend on it
                    tauri::WebviewUrl::App("main".into()),
                )
                .title("Drop Desktop App")
                .min_inner_size(1000.0, 500.0)
                .inner_size(1536.0, 864.0)
                .decorations(false)
                .shadow(false)
                .data_directory(DATA_ROOT_DIR.join(".webview"))
                .build()
                .expect("Failed to build main window");

                app.deep_link().on_open_url(move |event| {
                    debug!("handling drop:// url");
                    let binding = event.urls();
                    let url = match binding.first() {
                        Some(url) => url,
                        None => {
                            warn!("No value recieved from deep link. Is this a drop server?");
                            return;
                        }
                    };
                    if let Some("handshake") = url.host_str() {
                        tauri::async_runtime::spawn(recieve_handshake(
                            handle.clone(),
                            url.path().to_string(),
                        ));
                    }
                });
                let open_menu_item = MenuItem::with_id(app, "open", "Open", true, None::<&str>)
                    .expect("Failed to generate open menu item");

                let sep = PredefinedMenuItem::separator(app)
                    .expect("Failed to generate menu separator item");

                let quit_menu_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
                    .expect("Failed to generate quit menu item");

                let menu = Menu::with_items(
                    app,
                    &[
                        &open_menu_item,
                        &sep,
                        /*
                        &MenuItem::with_id(app, "show_library", "Library", true, None::<&str>)?,
                        &MenuItem::with_id(app, "show_settings", "Settings", true, None::<&str>)?,
                        &PredefinedMenuItem::separator(app)?,
                         */
                        &quit_menu_item,
                    ],
                )
                .expect("Failed to generate menu");

                run_on_tray(|| {
                    TrayIconBuilder::new()
                        .icon(
                            app.default_window_icon()
                                .expect("Failed to get default window icon")
                                .clone(),
                        )
                        .menu(&menu)
                        .on_menu_event(|app, event| match event.id.as_ref() {
                            "open" => {
                                app.webview_windows()
                                    .get("main")
                                    .expect("Failed to get webview")
                                    .show()
                                    .expect("Failed to show window");
                            }
                            "quit" => {
                                cleanup_and_exit(app);
                            }

                            _ => {
                                warn!("menu event not handled: {:?}", event.id);
                            }
                        })
                        .build(app)
                        .expect("error while setting up tray menu");
                });

                {
                    let mut db_handle = borrow_db_mut_checked();
                    if let Some(original) = db_handle.prev_database.take() {
                        let canonicalised = match original.canonicalize() {
                            Ok(o) => o,
                            Err(_) => original,
                        };
                        warn!(
                            "Database corrupted. Original file at {}",
                            canonicalised.display()
                        );
                        app.dialog()
                            .message(format!(
                                "Database corrupted. A copy has been saved at: {}",
                                canonicalised.display()
                            ))
                            .title("Database corrupted")
                            .show(|_| {});
                    }
                }
            });

            Ok(())
        })
        .register_asynchronous_uri_scheme_protocol("object", move |_ctx, request, responder| {
            tauri::async_runtime::spawn(async move {
                fetch_object_wrapper(request, responder).await;
            });
        })
        .register_asynchronous_uri_scheme_protocol("server", |ctx, request, responder| {
            tauri::async_runtime::block_on(async move {
                let state = ctx
                    .app_handle()
                    .state::<tauri::State<'_, Mutex<AppState>>>();

                offline!(
                    state,
                    handle_server_proto_wrapper,
                    handle_server_proto_offline_wrapper,
                    request,
                    responder
                )
                .await;
            });
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                run_on_tray(|| {
                    window.hide().expect("Failed to close window in tray");
                    api.prevent_close();
                });
            }
        })
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    app.run(|_app_handle, event| {
        if let RunEvent::ExitRequested { code, api, .. } = event {
            run_on_tray(|| {
                if code.is_none() {
                    api.prevent_exit();
                }
            });
        }
    });
}

fn run_on_tray<T: FnOnce()>(f: T) {
    if match std::env::var("NO_TRAY_ICON") {
        Ok(s) => s.to_lowercase() != "true",
        Err(_) => true,
    } {
        (f)();
    }
}

// TODO: Refactor
pub async fn recieve_handshake(app: AppHandle, path: String) {
    // Tell the app we're processing
    app_emit!(&app, "auth/processing", ());

    let handshake_result = recieve_handshake_logic(&app, path).await;
    if let Err(e) = handshake_result {
        warn!("error with authentication: {e}");
        app_emit!(&app, "auth/failed", e.to_string());
        return;
    }

    let app_state = app.state::<Mutex<AppState>>();

    let (app_status, user) = auth::setup().await;

    let mut state_lock = app_state.lock();

    state_lock.status = app_status;
    state_lock.user = user;

    let _ = clear_cached_object("collections");
    let _ = clear_cached_object("library");

    drop(state_lock);

    app_emit!(&app, "auth/finished", ());
}

// TODO: Refactor
async fn recieve_handshake_logic(app: &AppHandle, path: String) -> Result<(), RemoteAccessError> {
    let path_chunks: Vec<&str> = path.split('/').collect();
    if path_chunks.len() != 3 {
        app_emit!(app, "auth/failed", ());
        return Err(RemoteAccessError::HandshakeFailed(
            "failed to parse token".to_string(),
        ));
    }

    let base_url = {
        let handle = borrow_db_checked();
        Url::parse(handle.base_url.as_str())?
    };

    let client_id = path_chunks
        .get(1)
        .expect("Failed to get client id from path chunks");
    let token = path_chunks
        .get(2)
        .expect("Failed to get token from path chunks");
    let body = HandshakeRequestBody::new((client_id).to_string(), (token).to_string());

    let endpoint = base_url.join("/api/v1/client/auth/handshake")?;
    let client = DROP_CLIENT_ASYNC.clone();
    let response = client.post(endpoint).json(&body).send().await?;
    debug!("handshake responsded with {}", response.status().as_u16());
    if !response.status().is_success() {
        return Err(RemoteAccessError::InvalidResponse(response.json().await?));
    }
    let response_struct: HandshakeResponse = response.json().await?;

    {
        let mut handle = borrow_db_mut_checked();
        handle.auth = Some(response_struct.into());
    }

    let web_token = {
        let header = generate_authorization_header();
        let token = client
            .post(base_url.join("/api/v1/client/user/webtoken")?)
            .header("Authorization", header)
            .send()
            .await?;

        token.text().await?
    };
    let mut handle = borrow_db_mut_checked();
    handle.auth.as_mut().unwrap().web_token = Some(web_token);

    Ok(())
}
