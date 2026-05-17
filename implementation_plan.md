# Sone Windows Migration: Native Audio Engine Architecture

This document details the architectural changes required to port the Sone audio player to Windows using GStreamer (`wasapisink`) and `souvlaki` (for SMTC), ensuring a single-codebase, update-proof design using Rust conditional compilation.

## User Review Required

> [!WARNING]
> Toggling `exclusive` mode on Windows WASAPI mid-playback requires dropping and re-acquiring the device handle. This means we must briefly pause/null the pipeline, set the property, and resume. This will cause a very brief audio gap (similar to TIDAL's official desktop app when toggling exclusive mode). Please review the `toggle_exclusive_mode` flow below to confirm this is acceptable.

## Open Questions

1. **Volume Control in Bit-Perfect:** In bit-perfect mode, do you want to bypass the GStreamer `volume` element entirely to ensure 100% untouched bitstream, or just `audioconvert` and `audioresample`? (Bypassing volume usually implies application volume control is disabled, relying on external DAC hardware volume).
2. **HWND retrieval:** `souvlaki` on Windows requires a window handle (`HWND`). Is it acceptable for the media controller initialization to depend on the main Tauri `Window` object?

## Proposed Changes

### 1. Dynamic Audio Engine (GStreamer via Tauri Commands)

#### Conditional Setup & Pipeline Architecture
On Linux, you currently use a custom ALSA writer thread. On Windows, we will use GStreamer's native `wasapisink` to achieve both Shared and Exclusive playback. We achieve this via conditional `#[cfg(target_os = "windows")]` blocks when building the pipeline.

```rust
// audio/pipeline.rs
#[cfg(target_os = "windows")]
pub fn create_sink() -> gst::Element {
    let sink = gst::ElementFactory::make("wasapisink")
        .name("audio_sink")
        .build()
        .expect("Failed to create wasapisink");
    
    // By default, obey the user's config
    sink.set_property("exclusive", false);
    sink.set_property("low-latency", true);
    sink
}

#[cfg(target_os = "linux")]
pub fn create_sink() -> gst::Element {
    // Your existing ALSA logic or alsasink
    gst::ElementFactory::make("alsasink")
        .name("audio_sink")
        .build()
        .expect("Failed to create alsasink")
}
```

#### Toggling Exclusive Mode Mid-Playback
To change the `exclusive` property dynamically from a React frontend toggle, we expose a Tauri command. The pipeline must be taken to `READY` or `NULL` state to release the device, property updated, and then brought back to `PLAYING`.

```rust
// commands.rs
#[tauri::command]
pub async fn toggle_exclusive_mode(
    state: tauri::State<'_, AudioState>, 
    exclusive: bool
) -> Result<(), String> {
    let pipeline = state.pipeline.lock().await;
    
    // 1. Get current position to seamlessly resume
    let mut position = gst::ClockTime::ZERO;
    if let Some(pos) = pipeline.query_position::<gst::ClockTime>() {
        position = pos;
    }

    // 2. Safely pause and drop the audio device handle
    pipeline.set_state(gst::State::Ready).map_err(|e| e.to_string())?;

    // 3. Update the sink property
    #[cfg(target_os = "windows")]
    if let Some(sink) = pipeline.by_name("audio_sink") {
        sink.set_property("exclusive", exclusive);
    }

    // 4. Resume playback and seek to the exact previous position
    pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;
    
    if position > gst::ClockTime::ZERO {
        pipeline.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            position,
        ).map_err(|e| e.to_string())?;
    }

    Ok(())
}
```

### 2. Bit-Perfect Topology

A bit-perfect pipeline implies the decoded PCM data is sent to the DAC *exactly* as it was decoded, without any sample rate conversion or bit-depth padding.

**Pipeline Structure:**
We will use a dynamic `decodebin` pad-added connection. Based on the UI state (stored in a global `Config` or `Arc<AtomicBool>`), we conditionally link through resamplers or bypass them.

```rust
// audio/bitperfect.rs
fn on_pad_added(
    decodebin: &gst::Element, 
    pad: &gst::Pad, 
    pipeline: &gst::Pipeline, 
    is_bit_perfect: bool
) {
    let sink = pipeline.by_name("audio_sink").unwrap();
    let sink_pad = sink.static_pad("sink").unwrap();

    if is_bit_perfect {
        // Bit-Perfect: Decodebin -> [Volume (optional)] -> WasapiSink
        // The DAC must natively support the exact sample rate and format!
        
        #[cfg(target_os = "windows")]
        sink.set_property("exclusive", true); // Bit-perfect on Windows requires Exclusive mode
        
        pad.link(&sink_pad).expect("Failed to link bit-perfect pipeline");
        log::info!("Pipeline linked in Bit-Perfect Mode");
    } else {
        // Shared/Resampled Mode: Decodebin -> AudioConvert -> AudioResample -> WasapiSink
        let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
        let resample = gst::ElementFactory::make("audioresample").build().unwrap();
        
        pipeline.add_many([&conv, &resample]).unwrap();
        conv.sync_state_with_parent().unwrap();
        resample.sync_state_with_parent().unwrap();

        gst::Element::link_many([&conv, &resample, &sink]).unwrap();
        
        let conv_sink = conv.static_pad("sink").unwrap();
        pad.link(&conv_sink).expect("Failed to link resampled pipeline");
        log::info!("Pipeline linked in Shared/Resampled Mode");
    }
}
```

### 3. Media Controls (Replacing MPRIS with SMTC)

We will use the `souvlaki` crate, which provides a unified trait abstraction over Windows SMTC, Linux MPRIS, and macOS Now Playing.

**Implementation Structure:**
```rust
// media_controls.rs
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, PlatformConfig};
use std::sync::mpsc;
use tauri::Window;

pub struct SoneMediaController {
    controls: MediaControls,
}

impl SoneMediaController {
    pub fn new(window: &Window) -> Result<Self, Box<dyn std::error::Error>> {
        #[cfg(target_os = "windows")]
        let hwnd = Some(window.hwnd().unwrap().0 as *mut std::ffi::c_void);
        #[cfg(not(target_os = "windows"))]
        let hwnd = None;

        let config = PlatformConfig {
            dbus_name: "sone.player",      // For Linux MPRIS
            display_name: "Sone",          // For Windows SMTC
            hwnd,
        };

        let mut controls = MediaControls::new(config)?;

        // Attach event listener (Play/Pause/Next/Prev from hardware keys)
        let (tx, rx) = mpsc::channel();
        controls.attach(move |event: MediaControlEvent| {
            tx.send(event).unwrap();
        })?;

        // In a real implementation, you'd spawn a thread reading `rx` 
        // and emitting Tauri events to the frontend or updating GStreamer directly.
        
        Ok(Self { controls })
    }

    pub fn update_metadata(&mut self, title: &str, artist: &str, album: &str) {
        self.controls
            .set_metadata(MediaMetadata {
                title: Some(title),
                artist: Some(artist),
                album: Some(album),
                ..Default::default()
            })
            .ok();
    }
}
```

### 4. Deep Linking & Build Setup

**Deep Linking (`tidal://`) via Registry:**
Tauri's `tauri-plugin-deep-link` correctly handles Windows Registry keys, provided you update `tauri.conf.json`:
```json
"plugins": {
  "deep-link": {
    "desktop": {
      "schemes": ["tidal"]
    }
  }
}
```
*Note: Make sure your `Cargo.toml` specifies `tauri-plugin-deep-link` and it is initialized in `main.rs`.*

**MSVC GStreamer Setup:**
To ensure `rustc` finds GStreamer, instruct users to set `GSTREAMER_1_0_ROOT_MSVC_X86_64` (the default environment variable set by the GStreamer Windows installer). The `gstreamer-rs` build scripts automatically detect this.

## Verification Plan

1. **Architecture Code Compile:** Verify the provided snippets compile cleanly on Windows targets by running `cargo check --target x86_64-pc-windows-msvc`.
2. **Exclusive Gap Test:** Confirm with the user that the small gap during pipeline pause/resume for the exclusive toggle is acceptable.
3. **Souvlaki Initialization:** Verify that `souvlaki` can retrieve the HWND successfully from the Tauri window.
