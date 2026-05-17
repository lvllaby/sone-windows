#[cfg(target_os = "linux")]
use ksni::menu::{MenuItem, StandardItem};
#[cfg(target_os = "linux")]
use ksni::TrayMethods;
use tauri::{Emitter, Manager};

/// Wrapper around platform-specific tray handles for tooltip updates.
/// Stored in Tauri managed state via `app.manage()`.
#[cfg(target_os = "linux")]
pub struct TrayHandle(ksni::Handle<SoneTray>);

#[cfg(target_os = "windows")]
pub struct TrayHandle(tauri::tray::TrayIcon);

impl TrayHandle {
    #[cfg(target_os = "linux")]
    pub async fn update_tooltip(&self, text: String) {
        self.0
            .update(move |tray| {
                tray.tooltip = text;
            })
            .await;
    }

    #[cfg(target_os = "windows")]
    pub async fn update_tooltip(&self, text: String) {
        let _ = self.0.set_tooltip(Some(text));
    }
}

#[cfg(target_os = "linux")]
struct SoneTray {
    app_handle: tauri::AppHandle,
    tooltip: String,
    icon: ksni::Icon,
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for SoneTray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoneTray")
            .field("tooltip", &self.tooltip)
            .finish()
    }
}

/// Convert RGBA8 pixel data to ARGB32 in network byte order (big-endian),
/// as required by the StatusNotifierItem D-Bus protocol.
#[cfg(target_os = "linux")]
fn rgba_to_argb(rgba: &[u8]) -> Vec<u8> {
    let mut argb = Vec::with_capacity(rgba.len());
    for pixel in rgba.chunks_exact(4) {
        argb.push(pixel[3]); // A
        argb.push(pixel[0]); // R
        argb.push(pixel[1]); // G
        argb.push(pixel[2]); // B
    }
    argb
}

pub(crate) fn restore_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();

        // Wayland GTK CSD workaround: after hide()+show(), GTK client-side
        // decoration hit-test regions go stale — buttons render but ignore
        // pointer events.  Toggling decorations forces GTK to recalculate.
        //
        // Only relevant when native chrome is active (the escape-hatch path).
        // The custom React titlebar isn't subject to this hit-test staleness,
        // so we skip the flicker entirely in the default case.
        //
        // Per tauri-apps/tauri#11856 the bug reproduces on KDE Wayland too,
        // so no desktop-specific skip.
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            let state = app.state::<crate::AppState>();
            let wants = state
                .decorations
                .load(std::sync::atomic::Ordering::Relaxed);
            if wants {
                let _ = window.set_decorations(false);
                let _ = window.set_decorations(true);
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl ksni::Tray for SoneTray {
    fn id(&self) -> String {
        "sone".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }

    fn title(&self) -> String {
        self.tooltip.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.tooltip.clone(),
            description: String::new(),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        restore_window(&self.app_handle);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Show".into(),
                activate: Box::new(|this: &mut Self| {
                    restore_window(&this.app_handle);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Play / Pause".into(),
                activate: Box::new(|this: &mut Self| {
                    this.app_handle.emit("tray:toggle-play", ()).ok();
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Next Track".into(),
                activate: Box::new(|this: &mut Self| {
                    this.app_handle.emit("tray:next-track", ()).ok();
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Previous Track".into(),
                activate: Box::new(|this: &mut Self| {
                    this.app_handle.emit("tray:prev-track", ()).ok();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|this: &mut Self| {
                    this.app_handle.exit(0);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Spawn the ksni tray on the tokio runtime. Non-blocking — registers the
/// tray handle in Tauri state once the D-Bus connection is established.
/// If it fails, logs a warning and disables minimize-to-tray.
#[cfg(target_os = "linux")]
pub fn setup(app: &tauri::App) {
    let icon_bytes = include_bytes!("../icons/icon.png");
    let icon = match image::load_from_memory(icon_bytes) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            ksni::Icon {
                width: w as i32,
                height: h as i32,
                data: rgba_to_argb(rgba.as_raw()),
            }
        }
        Err(e) => {
            log::warn!("Failed to decode tray icon: {e}");
            return;
        }
    };

    let tray = SoneTray {
        app_handle: app.handle().clone(),
        tooltip: "Sone".into(),
        icon,
    };

    let app_handle = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        // In Flatpak/Snap sandbox, ksni can't own a well-known D-Bus name
        // (would need --own-name with a dynamic PID-based name).
        // disable_dbus_name makes it register via the unique connection name instead.
        let is_sandboxed =
            std::env::var("FLATPAK_ID").is_ok() || std::env::var("SNAP").is_ok();
        match tray.disable_dbus_name(is_sandboxed).spawn().await {
            Ok(handle) => {
                app_handle.manage(TrayHandle(handle));
                log::info!("ksni tray icon registered");
            }
            Err(e) => {
                log::warn!("Failed to create ksni tray: {e}");
                let state = app_handle.state::<crate::AppState>();
                state
                    .minimize_to_tray
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
}

#[cfg(target_os = "windows")]
pub fn setup(app: &tauri::App) {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
    use tauri::tray::{TrayIconBuilder, TrayIconEvent, MouseButton, MouseButtonState};

    let app_handle = app.handle().clone();

    // 1. Create Menu Items
    let show_i = match MenuItemBuilder::with_id("show", "Show").build(app) {
        Ok(item) => item,
        Err(e) => {
            log::error!("Failed to build tray show item: {e}");
            return;
        }
    };
    let play_i = match MenuItemBuilder::with_id("play", "Play / Pause").build(app) {
        Ok(item) => item,
        Err(e) => {
            log::error!("Failed to build tray play item: {e}");
            return;
        }
    };
    let next_i = match MenuItemBuilder::with_id("next", "Next Track").build(app) {
        Ok(item) => item,
        Err(e) => {
            log::error!("Failed to build tray next item: {e}");
            return;
        }
    };
    let prev_i = match MenuItemBuilder::with_id("prev", "Previous Track").build(app) {
        Ok(item) => item,
        Err(e) => {
            log::error!("Failed to build tray prev item: {e}");
            return;
        }
    };
    let quit_i = match MenuItemBuilder::with_id("quit", "Quit").build(app) {
        Ok(item) => item,
        Err(e) => {
            log::error!("Failed to build tray quit item: {e}");
            return;
        }
    };

    let sep1 = match PredefinedMenuItem::separator(app) {
        Ok(sep) => sep,
        Err(e) => {
            log::error!("Failed to build tray separator: {e}");
            return;
        }
    };
    let sep2 = match PredefinedMenuItem::separator(app) {
        Ok(sep) => sep,
        Err(e) => {
            log::error!("Failed to build tray separator: {e}");
            return;
        }
    };

    // 2. Build Context Menu
    let menu = match MenuBuilder::new(app)
        .item(&show_i)
        .item(&sep1)
        .item(&play_i)
        .item(&next_i)
        .item(&prev_i)
        .item(&sep2)
        .item(&quit_i)
        .build()
    {
        Ok(m) => m,
        Err(e) => {
            log::error!("Failed to build tray menu: {e}");
            return;
        }
    };

    // 3. Load System Tray Icon
    let icon_bytes = include_bytes!("../icons/icon.png");
    let icon = match tauri::image::Image::from_bytes(icon_bytes) {
        Ok(img) => img,
        Err(e) => {
            log::error!("Failed to decode tray icon: {e}");
            return;
        }
    };

    // 4. Build and configure the System Tray
    let tray_builder = TrayIconBuilder::new()
        .icon(icon)
        .tooltip("Sone")
        .menu(&menu)
        .show_menu_on_left_click(false) // So we can restore the window on left-click, and show menu on right-click
        .on_menu_event(move |app_handle, event| {
            match event.id().0.as_str() {
                "show" => {
                    restore_window(app_handle);
                }
                "play" => {
                    let _ = app_handle.emit("tray:toggle-play", ());
                }
                "next" => {
                    let _ = app_handle.emit("tray:next-track", ());
                }
                "prev" => {
                    let _ = app_handle.emit("tray:prev-track", ());
                }
                "quit" => {
                    app_handle.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                restore_window(tray.app_handle());
            }
        });

    match tray_builder.build(app) {
        Ok(tray_icon) => {
            app_handle.manage(TrayHandle(tray_icon));
            log::info!("Windows native tray icon registered");
        }
        Err(e) => {
            log::warn!("Failed to create Windows native tray: {e}");
            let state = app_handle.state::<crate::AppState>();
            state
                .minimize_to_tray
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
