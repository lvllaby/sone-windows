use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, PlatformConfig, MediaPlayback};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, Window};

pub struct WindowsMediaHandle {
    controls: Arc<Mutex<Option<MediaControls>>>,
}

impl WindowsMediaHandle {
    pub fn new(app_handle: AppHandle) -> Self {
        let controls = Arc::new(Mutex::new(None));
        let controls_clone = controls.clone();
        
        // We need to wait for the main window to be created to get HWND
        tauri::async_runtime::spawn(async move {
            // Wait for main window
            let mut window = None;
            for _ in 0..50 { // wait up to 5 seconds
                if let Some(w) = app_handle.get_webview_window("main") {
                    window = Some(w);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            if let Some(w) = window {
                let hwnd = w.hwnd().ok().map(|h| h.0 as *mut std::ffi::c_void);
                let config = PlatformConfig {
                    dbus_name: "sone.player",
                    display_name: "Sone",
                    hwnd,
                };

                match MediaControls::new(config) {
                    Ok(mut m) => {
                        let app = app_handle.clone();
                        m.attach(move |event| match event {
                            MediaControlEvent::Play => { app.emit("tray:toggle-play", ()).ok(); },
                            MediaControlEvent::Pause => { app.emit("tray:toggle-play", ()).ok(); },
                            MediaControlEvent::TogglePlayPause => { app.emit("tray:toggle-play", ()).ok(); },
                            MediaControlEvent::Next => { app.emit("tray:next-track", ()).ok(); },
                            MediaControlEvent::Previous => { app.emit("tray:prev-track", ()).ok(); },
                            MediaControlEvent::Stop => { app.emit("mpris:stop", ()).ok(); },
                            _ => {}
                        }).ok();
                        *controls_clone.lock().unwrap() = Some(m);
                        log::info!("SMTC initialized successfully");
                    }
                    Err(e) => {
                        log::error!("Failed to initialize SMTC: {e}");
                    }
                }
            } else {
                log::error!("Could not find main window for SMTC initialization");
            }
        });

        Self { controls }
    }

    pub fn update_metadata(
        &self,
        title: &str,
        artist: &str,
        album: &str,
        art_url: &str,
        duration_secs: f64,
    ) {
        if let Some(controls) = self.controls.lock().unwrap().as_mut() {
            let mut metadata = MediaMetadata {
                title: Some(title),
                album: Some(album),
                artist: Some(artist),
                duration: Some(std::time::Duration::from_secs_f64(duration_secs)),
                ..Default::default()
            };
            if !art_url.is_empty() {
                metadata.cover_url = Some(art_url);
            }
            controls.set_metadata(metadata).ok();
        }
    }

    pub fn set_playback_status(&self, is_playing: bool) {
        if let Some(controls) = self.controls.lock().unwrap().as_mut() {
            controls.set_playback(if is_playing {
                MediaPlayback::Playing { progress: None }
            } else {
                MediaPlayback::Paused { progress: None }
            }).ok();
        }
    }

    pub fn stop(&self) {
        if let Some(controls) = self.controls.lock().unwrap().as_mut() {
            controls.set_playback(MediaPlayback::Stopped).ok();
        }
    }
}
