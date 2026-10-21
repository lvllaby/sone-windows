use tauri::State;

use crate::sonos::manager::SonosSnapshot;
use crate::sonos::tidal::QueueItem;
use crate::sonos::topology::SonosGroup;
use crate::AppState;
use crate::SoneError;

/// Persists the manager's TIDAL `sn` if it differs from the saved one.
fn persist_sn(state: &AppState) -> Result<(), SoneError> {
    let Some(sn) = state.sonos.tidal_sn() else {
        return Ok(());
    };
    let mut settings = state.load_settings().unwrap_or_default();
    if settings.sonos_tidal_sn != Some(sn) {
        settings.sonos_tidal_sn = Some(sn);
        state.save_settings(&settings)?;
    }
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_get_snapshot(state: State<'_, AppState>) -> Result<SonosSnapshot, SoneError> {
    Ok(state.sonos.snapshot().await)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_rescan(state: State<'_, AppState>) -> Result<Vec<SonosGroup>, SoneError> {
    log::debug!("[sonos_rescan]");
    state.sonos.refresh_topology().await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_set_manual_ip(
    state: State<'_, AppState>,
    ip: Option<String>,
) -> Result<Vec<SonosGroup>, SoneError> {
    log::debug!("[sonos_set_manual_ip]: ip={:?}", ip);
    let ip = state.sonos.set_manual_ip(ip)?;
    let mut settings = state.load_settings().unwrap_or_default();
    settings.sonos_manual_ip = ip;
    state.save_settings(&settings)?;
    state.sonos.refresh_topology().await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_select_group(
    state: State<'_, AppState>,
    coordinator_id: Option<String>,
    forget_last: bool,
) -> Result<(), SoneError> {
    log::debug!(
        "[sonos_select_group]: coordinator_id={:?}, forget_last={}",
        coordinator_id,
        forget_last
    );
    state.sonos.select(coordinator_id.clone(), forget_last);
    // Some(id): remember it. None + forget: clear it. None + keep: untouched.
    if coordinator_id.is_some() || forget_last {
        let mut settings = state.load_settings().unwrap_or_default();
        settings.sonos_last_coordinator = coordinator_id;
        state.save_settings(&settings)?;
    }
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_group_now_playing(
    state: State<'_, AppState>,
    coordinator_id: String,
) -> Result<Option<QueueItem>, SoneError> {
    state.sonos.group_now_playing(&coordinator_id).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_load_queue(
    state: State<'_, AppState>,
    track_ids: Vec<u64>,
    start_index: usize,
    position_secs: f64,
    play: bool,
) -> Result<(), SoneError> {
    log::debug!(
        "[sonos_load_queue]: tracks={}, start_index={}, position_secs={:.1}, play={}",
        track_ids.len(),
        start_index,
        position_secs,
        play
    );
    let result = state
        .sonos
        .load_queue(&track_ids, start_index, position_secs, play)
        .await;
    // The sn may have been discovered even if a later step failed.
    if let Err(e) = persist_sn(&state) {
        log::warn!("[sonos_load_queue] failed to persist sn: {e}");
    }
    result
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_play(state: State<'_, AppState>) -> Result<(), SoneError> {
    state.sonos.play().await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_pause(state: State<'_, AppState>) -> Result<(), SoneError> {
    state.sonos.pause().await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_next(state: State<'_, AppState>) -> Result<(), SoneError> {
    state.sonos.next().await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_previous(state: State<'_, AppState>) -> Result<(), SoneError> {
    state.sonos.previous().await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_seek(state: State<'_, AppState>, position_secs: f64) -> Result<(), SoneError> {
    state.sonos.seek(position_secs).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_play_index(state: State<'_, AppState>, index: usize) -> Result<(), SoneError> {
    state.sonos.play_index(index).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_queue_insert(
    state: State<'_, AppState>,
    track_ids: Vec<u64>,
    at: usize,
) -> Result<(), SoneError> {
    let result = state.sonos.queue_insert(&track_ids, at).await;
    if let Err(e) = persist_sn(&state) {
        log::warn!("[sonos_queue_insert] failed to persist sn: {e}");
    }
    result
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_queue_remove(state: State<'_, AppState>, index: usize) -> Result<(), SoneError> {
    state.sonos.queue_remove(index).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_queue_replace_after(
    state: State<'_, AppState>,
    index: usize,
    track_ids: Vec<u64>,
) -> Result<(), SoneError> {
    let result = state.sonos.queue_replace_after(index, &track_ids).await;
    if let Err(e) = persist_sn(&state) {
        log::warn!("[sonos_queue_replace_after] failed to persist sn: {e}");
    }
    result
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_queue_clear(state: State<'_, AppState>) -> Result<(), SoneError> {
    state.sonos.queue_clear().await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_set_group_volume(
    state: State<'_, AppState>,
    volume: u8,
) -> Result<(), SoneError> {
    state.sonos.set_group_volume(volume).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_set_member_volume(
    state: State<'_, AppState>,
    member_id: String,
    volume: u8,
) -> Result<(), SoneError> {
    state.sonos.set_member_volume(&member_id, volume).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_set_mute(state: State<'_, AppState>, muted: bool) -> Result<(), SoneError> {
    state.sonos.set_mute(muted).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sonos_set_play_mode(state: State<'_, AppState>, repeat: u8) -> Result<(), SoneError> {
    state.sonos.set_play_mode(repeat).await
}
