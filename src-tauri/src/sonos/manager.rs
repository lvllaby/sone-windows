//! Sonos casting manager: topology, group selection, queue loading and
//! transport/volume control for the selected group.
//!
//! All shared state sits behind a `std::sync::Mutex` that is never held across
//! an `.await`; network calls work on values cloned out of it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::error::SoneError;
use crate::sonos::discovery::{discover, fetch_topology};
use crate::sonos::services;
use crate::sonos::soap::SoapClient;
use crate::sonos::tidal::{
    build_track_didl, build_track_uri, classify_uri, didl_res_uris, find_tidal_sn,
    parse_didl_items, QueueItem, QueueItemKind,
};
use crate::sonos::topology::{find_group_for_member, SonosGroup};
use crate::Settings;

/// Message of the `SoneError::Sonos` returned when no TIDAL account serial
/// (`sn`) can be found on the speaker.
pub const SN_UNKNOWN: &str = "TIDAL_SN_UNKNOWN";

/// Sonos refuses queues beyond this many tracks.
const MAX_QUEUE_TRACKS: usize = 1000;
/// `AddMultipleURIsToQueue` accepts at most 16 URIs per call.
const ADD_BATCH: usize = 16;
/// Containers searched (in order) for a TIDAL URI carrying the account `sn`.
const SN_SOURCES: [&str; 3] = ["Q:0", "FV:2", "R:0/0"];
const SN_BROWSE_COUNT: u32 = 100;
const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(2500);

pub struct LoadPlan {
    /// Tracks before the start track; inserted at position 1 by the back-fill.
    pub prepend: Vec<u64>,
    /// First batch (starting at the start track), loaded before playback.
    pub initial: Vec<u64>,
    /// Index of the start track within `initial`.
    pub initial_start: usize,
    /// Tracks after `initial`; appended by the back-fill.
    pub append: Vec<u64>,
}

/// Splits a queue so playback can start after one `AddMultipleURIsToQueue`
/// call: the start track plus 15 followers go first, the remainder is filled
/// in afterwards. The total is capped at [`MAX_QUEUE_TRACKS`]; the cap trims
/// `append` first and, only when the start track is very late, the oldest
/// `prepend` entries.
pub fn plan_load(track_ids: &[u64], start_index: usize) -> LoadPlan {
    if track_ids.is_empty() {
        return LoadPlan {
            prepend: Vec::new(),
            initial: Vec::new(),
            initial_start: 0,
            append: Vec::new(),
        };
    }
    let start = start_index.min(track_ids.len() - 1);
    let initial_end = (start + ADD_BATCH).min(track_ids.len());
    let initial = track_ids[start..initial_end].to_vec();
    let mut prepend = track_ids[..start].to_vec();
    let mut append = track_ids[initial_end..].to_vec();

    let room = MAX_QUEUE_TRACKS.saturating_sub(initial.len());
    if prepend.len() > room {
        prepend.drain(..prepend.len() - room);
    }
    append.truncate(room - prepend.len());
    LoadPlan {
        prepend,
        initial,
        initial_start: 0,
        append,
    }
}

/// Whether the background back-fill may add its next batch: it was not
/// superseded by a newer load/selection (`generation`), and nobody but us has
/// touched the queue since our last write (`observed == own`).
pub fn fill_should_continue(
    own_update_id: Option<u32>,
    observed_update_id: u32,
    generation: u64,
    current_generation: u64,
) -> bool {
    if generation != current_generation {
        return false;
    }
    match own_update_id {
        None => true,
        Some(own) => own == observed_update_id,
    }
}

/// Frontend indices are 0-based, Sonos queue positions are 1-based. This is
/// the single place the conversion happens.
pub fn queue_position(index: usize) -> u32 {
    u32::try_from(index)
        .unwrap_or(u32::MAX - 1)
        .saturating_add(1)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SonosSnapshot {
    pub groups: Vec<SonosGroup>,
    pub selected: Option<String>,
    /// Coordinator of the group containing the remembered `sonos_last_coordinator`.
    pub attachable: Option<String>,
    pub sn_known: bool,
    pub manual_ip: Option<String>,
}

#[derive(Default)]
struct State {
    groups: Vec<SonosGroup>,
    scanned: bool,
    selected: Option<String>,
    manual_ip: Option<String>,
    last_coordinator: Option<String>,
    tidal_sn: Option<u32>,
}

struct Inner {
    soap: SoapClient,
    state: Mutex<State>,
    /// Bumped by every new load/selection (and queue wipes); a running
    /// back-fill whose generation no longer matches stops.
    fill_generation: AtomicU64,
    /// Serializes queue mutations. Foreground ops (load, insert, remove,
    /// replace, clear, play_index) hold it for their whole duration; the
    /// back-fill holds it for its entire prepend phase and per batch while
    /// appending. Never held while a std `Mutex` guard is alive.
    queue_lock: Arc<tokio::sync::Mutex<()>>,
    /// `NewUpdateID` of our own latest queue mutation.
    own_update_id: Mutex<Option<u32>>,
    /// Latest playback position seen on the selected group (written by the
    /// poller); `None` until one is recorded.
    position: Mutex<Option<f32>>,
}

/// Cheaply cloneable handle (the back-fill task holds a clone).
#[derive(Clone)]
pub struct SonosManager {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn no_group_selected() -> SoneError {
    SoneError::Sonos("No Sonos group selected".into())
}

fn unknown_group() -> SoneError {
    SoneError::Sonos("Unknown Sonos group".into())
}

fn track_items(ids: &[u64], sn: u32) -> Vec<(String, String)> {
    ids.iter()
        .map(|&id| (build_track_uri(id, sn), build_track_didl(id)))
        .collect()
}

fn normalize_ip(ip: Option<String>) -> Result<Option<String>, SoneError> {
    match ip.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => s
            .parse::<std::net::Ipv4Addr>()
            .map(|a| Some(a.to_string()))
            .map_err(|_| SoneError::Sonos(format!("Invalid IP address: {s}"))),
    }
}

impl SonosManager {
    pub fn new(settings: &Settings) -> Self {
        let state = State {
            manual_ip: settings.sonos_manual_ip.clone(),
            last_coordinator: settings.sonos_last_coordinator.clone(),
            tidal_sn: settings.sonos_tidal_sn,
            ..State::default()
        };
        Self {
            inner: Arc::new(Inner {
                soap: SoapClient::new(),
                state: Mutex::new(state),
                fill_generation: AtomicU64::new(0),
                queue_lock: Arc::new(tokio::sync::Mutex::new(())),
                own_update_id: Mutex::new(None),
                position: Mutex::new(None),
            }),
        }
    }

    // ---- state accessors ------------------------------------------------

    /// Position of the casting group: `Some` only while a group is selected
    /// and the poller has recorded one.
    pub fn casting_position(&self) -> Option<f32> {
        lock(&self.inner.state).selected.as_ref()?;
        *lock(&self.inner.position)
    }

    /// Written by the poller (Task 6).
    #[allow(dead_code)]
    pub fn record_position(&self, position: Option<f32>) {
        *lock(&self.inner.position) = position;
    }

    pub fn tidal_sn(&self) -> Option<u32> {
        lock(&self.inner.state).tidal_sn
    }

    pub fn manual_ip(&self) -> Option<String> {
        lock(&self.inner.state).manual_ip.clone()
    }

    fn bump_generation(&self) -> u64 {
        self.inner.fill_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn set_own_update_id(&self, id: Option<u32>) {
        *lock(&self.inner.own_update_id) = id;
    }

    fn own_update_id(&self) -> Option<u32> {
        *lock(&self.inner.own_update_id)
    }

    // ---- topology and selection ------------------------------------------

    pub async fn snapshot(&self) -> SonosSnapshot {
        let needs_scan = !lock(&self.inner.state).scanned;
        if needs_scan {
            if let Err(e) = self.refresh_topology().await {
                log::debug!("[sonos] initial scan failed: {e}");
            }
        }
        let st = lock(&self.inner.state);
        let attachable = st
            .last_coordinator
            .as_deref()
            .and_then(|id| find_group_for_member(&st.groups, id))
            .map(|g| g.coordinator_id.clone());
        SonosSnapshot {
            groups: st.groups.clone(),
            selected: st.selected.clone(),
            attachable,
            sn_known: st.tidal_sn.is_some(),
            manual_ip: st.manual_ip.clone(),
        }
    }

    /// Re-discovers speakers (plus the manual IP) and reloads the topology.
    pub async fn refresh_topology(&self) -> Result<Vec<SonosGroup>, SoneError> {
        let manual = lock(&self.inner.state).manual_ip.clone();
        let mut seeds: Vec<String> = manual.into_iter().collect();
        for ip in discover(DISCOVERY_TIMEOUT).await {
            if !seeds.contains(&ip) {
                seeds.push(ip);
            }
        }
        if seeds.is_empty() {
            let mut st = lock(&self.inner.state);
            st.groups.clear();
            st.scanned = true;
            return Ok(Vec::new());
        }
        let result = fetch_topology(&self.inner.soap, &seeds).await;
        let mut st = lock(&self.inner.state);
        st.scanned = true;
        let groups = result?;
        st.groups = groups.clone();
        Ok(groups)
    }

    /// Validates and stores the manual speaker IP (persisting is the
    /// caller's job). Returns the normalized value.
    pub fn set_manual_ip(&self, ip: Option<String>) -> Result<Option<String>, SoneError> {
        let ip = normalize_ip(ip)?;
        lock(&self.inner.state).manual_ip = ip.clone();
        Ok(ip)
    }

    /// Selects the casting group (or none). `forget_last` only matters for
    /// `None`: it also clears the remembered coordinator.
    pub fn select(&self, coordinator_id: Option<String>, forget_last: bool) {
        {
            let mut st = lock(&self.inner.state);
            match &coordinator_id {
                Some(id) => st.last_coordinator = Some(id.clone()),
                None if forget_last => st.last_coordinator = None,
                None => {}
            }
            st.selected = coordinator_id;
        }
        self.bump_generation();
        self.set_own_update_id(None);
        *lock(&self.inner.position) = None;
        self.on_selection_changed();
    }

    /// Seam for Task 6: start/stop the poller for the new selection.
    fn on_selection_changed(&self) {}

    /// (coordinator id, coordinator ip) of the selected group.
    fn selected_coordinator(&self) -> Result<(String, String), SoneError> {
        let st = lock(&self.inner.state);
        let id = st.selected.clone().ok_or_else(no_group_selected)?;
        Self::coordinator_ip(&st, &id).map(|ip| (id, ip))
    }

    fn coordinator_ip(st: &State, coordinator_id: &str) -> Result<String, SoneError> {
        st.groups
            .iter()
            .find(|g| g.coordinator_id == coordinator_id)
            .and_then(|g| g.members.first())
            .map(|m| m.ip.clone())
            .ok_or_else(unknown_group)
    }

    // ---- now playing ------------------------------------------------------

    /// What a group is playing right now (used by the picker); `None` when idle.
    pub async fn group_now_playing(
        &self,
        coordinator_id: &str,
    ) -> Result<Option<QueueItem>, SoneError> {
        let ip = {
            let st = lock(&self.inner.state);
            Self::coordinator_ip(&st, coordinator_id)?
        };
        let info = services::position_info(&self.inner.soap, &ip).await?;
        if info.track_uri.is_empty() {
            return Ok(None);
        }
        if classify_uri(&info.track_uri) == QueueItemKind::Tv {
            return Ok(Some(QueueItem {
                kind: QueueItemKind::Tv,
                tidal_track_id: None,
                title: "TV".into(),
                artist: String::new(),
                album_art_url: None,
                duration_secs: None,
            }));
        }
        Ok(parse_didl_items(&info.track_meta, &ip).into_iter().next())
    }

    // ---- sn discovery -----------------------------------------------------

    async fn ensure_sn(&self, ip: &str) -> Result<u32, SoneError> {
        if let Some(sn) = self.tidal_sn() {
            return Ok(sn);
        }
        for object in SN_SOURCES {
            let browsed =
                match services::browse(&self.inner.soap, ip, object, 0, SN_BROWSE_COUNT).await {
                    Ok(b) => b,
                    Err(e) => {
                        log::debug!("[sonos] sn probe {object} failed: {e}");
                        continue;
                    }
                };
            let uris = didl_res_uris(&browsed.didl);
            if let Some(sn) = find_tidal_sn(uris.iter().map(String::as_str)) {
                lock(&self.inner.state).tidal_sn = Some(sn);
                return Ok(sn);
            }
        }
        Err(SoneError::Sonos(SN_UNKNOWN.into()))
    }

    // ---- queue ------------------------------------------------------------

    /// Adds `ids` in batches of at most 16, starting at `at_1based` (0 =
    /// append) and advancing the position per batch. Records each batch's
    /// `NewUpdateID` as ours.
    async fn add_tracks(
        &self,
        ip: &str,
        sn: u32,
        ids: &[u64],
        at_1based: u32,
    ) -> Result<(), SoneError> {
        let mut at = at_1based;
        for chunk in ids.chunks(ADD_BATCH) {
            let id = services::add_uris(&self.inner.soap, ip, &track_items(chunk, sn), at).await?;
            self.set_own_update_id(Some(id));
            if at != 0 {
                at += chunk.len() as u32;
            }
        }
        Ok(())
    }

    /// Replaces the coordinator's queue with `track_ids`, starts at
    /// `start_index` / `position_secs`, and back-fills the rest in the
    /// background (earlier tracks first, then the tail).
    ///
    /// If a step fails after `remove_all`, the speaker's queue is left
    /// empty or partial (no rollback; the failure is logged).
    ///
    /// The queue lock is held from `remove_all` through `play` and then handed
    /// to the back-fill task, so nothing can run between the foreground part
    /// and the start of the prepend phase.
    pub async fn load_queue(
        &self,
        track_ids: &[u64],
        start_index: usize,
        position_secs: f64,
        play: bool,
    ) -> Result<(), SoneError> {
        let (coordinator_id, ip) = self.selected_coordinator()?;
        if track_ids.is_empty() {
            return Err(SoneError::Sonos("No tracks to load".into()));
        }
        // Must happen before the queue is wiped: the sn is read from it.
        let sn = self.ensure_sn(&ip).await?;
        // Bump first so a running back-fill gives up the lock quickly.
        let generation = self.bump_generation();
        let guard = Arc::clone(&self.inner.queue_lock).lock_owned().await;
        let plan = plan_load(track_ids, start_index);
        let soap = &self.inner.soap;

        services::remove_all(soap, &ip).await?;
        // From here on a failure leaves the speaker's queue empty/partial.
        let foreground = async {
            self.set_own_update_id(None);
            self.add_tracks(&ip, sn, &plan.initial, 0).await?;
            services::set_queue_as_source(soap, &ip, &coordinator_id).await?;
            services::seek_track(soap, &ip, queue_position(plan.initial_start)).await?;
            if position_secs > 0.0 {
                services::seek_time(soap, &ip, position_secs).await?;
            }
            // SONE never uses Sonos shuffle: keep the repeat part, drop the rest.
            let repeat = match services::get_play_mode(soap, &ip).await {
                Ok(m) => services::play_mode_to_repeat(&m),
                Err(e) => {
                    log::debug!("[sonos] get_play_mode failed, assuming NORMAL: {e}");
                    0
                }
            };
            services::set_play_mode(soap, &ip, services::repeat_to_play_mode(repeat)).await?;
            if play {
                services::play(soap, &ip).await?;
            }
            Ok::<(), SoneError>(())
        };
        if let Err(e) = foreground.await {
            log::warn!(
                "[sonos] load_queue failed after clearing the queue; queue left empty/partial: {e}"
            );
            return Err(e);
        }

        if !plan.append.is_empty() || !plan.prepend.is_empty() {
            let this = self.clone();
            // The owned guard moves into the task: no gap in which another
            // queue op could run before the prepend phase starts.
            tauri::async_runtime::spawn(async move {
                this.run_backfill(guard, ip, sn, generation, plan).await;
            });
        }
        Ok(())
    }

    async fn fill_ok(&self, ip: &str, generation: u64) -> bool {
        if generation != self.inner.fill_generation.load(Ordering::SeqCst) {
            return false;
        }
        match services::browse(&self.inner.soap, ip, "Q:0", 0, 1).await {
            Ok(b) => fill_should_continue(
                self.own_update_id(),
                b.update_id,
                generation,
                self.inner.fill_generation.load(Ordering::SeqCst),
            ),
            Err(e) => {
                log::warn!("[sonos] back-fill stopped, cannot read queue: {e}");
                false
            }
        }
    }

    /// Back-fill order: earlier tracks first (inserted before the playing
    /// start track), then the tail. `prepend_guard` (the queue lock handed
    /// over by `load_queue`) is held for the whole prepend phase, so
    /// foreground index ops never see a half-inserted prepend and a
    /// generation bump can only cancel the remaining append batches. The
    /// append phase takes the lock per batch.
    async fn run_backfill(
        &self,
        prepend_guard: tokio::sync::OwnedMutexGuard<()>,
        ip: String,
        sn: u32,
        generation: u64,
        plan: LoadPlan,
    ) {
        let mut at = 1u32;
        for chunk in plan.prepend.chunks(ADD_BATCH) {
            if !self.fill_ok(&ip, generation).await {
                return;
            }
            if let Err(e) = self.add_tracks(&ip, sn, chunk, at).await {
                log::warn!("[sonos] back-fill prepend failed: {e}");
                return;
            }
            at += chunk.len() as u32;
        }
        drop(prepend_guard);

        for chunk in plan.append.chunks(ADD_BATCH) {
            let _guard = self.inner.queue_lock.lock().await;
            if !self.fill_ok(&ip, generation).await {
                return;
            }
            if let Err(e) = self.add_tracks(&ip, sn, chunk, 0).await {
                log::warn!("[sonos] back-fill append failed: {e}");
                return;
            }
        }
    }

    /// Inserts `ids` before 0-based queue index `at`.
    pub async fn queue_insert(&self, ids: &[u64], at: usize) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        let sn = self.ensure_sn(&ip).await?;
        let _guard = self.inner.queue_lock.lock().await;
        self.add_tracks(&ip, sn, ids, queue_position(at)).await
    }

    /// Removes the track at 0-based queue index `index`.
    pub async fn queue_remove(&self, index: usize) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        let _guard = self.inner.queue_lock.lock().await;
        let update_id = services::browse(&self.inner.soap, &ip, "Q:0", 0, 1)
            .await?
            .update_id;
        let new_id =
            services::remove_range(&self.inner.soap, &ip, queue_position(index), 1, update_id)
                .await?;
        self.set_own_update_id(Some(new_id));
        Ok(())
    }

    /// Drops everything after 0-based `index` and appends `ids`.
    pub async fn queue_replace_after(&self, index: usize, ids: &[u64]) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        let sn = self.ensure_sn(&ip).await?;
        // Take the lock first: a running prepend phase holds it and finishes
        // (so indices are valid). Only then bump the generation, which cancels
        // just the remaining append batches that would re-add the old tail.
        let _guard = self.inner.queue_lock.lock().await;
        self.bump_generation();
        let browsed = services::browse(&self.inner.soap, &ip, "Q:0", 0, 1).await?;
        let keep = index.saturating_add(1);
        let total = browsed.total_matches as usize;
        if total > keep {
            let new_id = services::remove_range(
                &self.inner.soap,
                &ip,
                queue_position(keep),
                (total - keep) as u32,
                browsed.update_id,
            )
            .await?;
            self.set_own_update_id(Some(new_id));
        }
        let room = MAX_QUEUE_TRACKS.saturating_sub(keep.min(total));
        self.add_tracks(&ip, sn, &ids[..ids.len().min(room)], 0)
            .await
    }

    pub async fn queue_clear(&self) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        // Lock first so a running prepend completes; the bump then cancels
        // only the remaining append batches.
        let _guard = self.inner.queue_lock.lock().await;
        self.bump_generation();
        services::remove_all(&self.inner.soap, &ip).await?;
        self.set_own_update_id(None);
        Ok(())
    }

    // ---- transport ----------------------------------------------------------

    pub async fn play(&self) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        services::play(&self.inner.soap, &ip).await
    }

    pub async fn pause(&self) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        services::pause(&self.inner.soap, &ip).await
    }

    pub async fn next(&self) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        services::next(&self.inner.soap, &ip).await
    }

    pub async fn previous(&self) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        services::previous(&self.inner.soap, &ip).await
    }

    pub async fn seek(&self, position_secs: f64) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        services::seek_time(&self.inner.soap, &ip, position_secs).await
    }

    /// Jumps to 0-based queue index `index` and plays.
    pub async fn play_index(&self, index: usize) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        let _guard = self.inner.queue_lock.lock().await;
        services::seek_track(&self.inner.soap, &ip, queue_position(index)).await?;
        services::play(&self.inner.soap, &ip).await
    }

    // ---- volume / mode --------------------------------------------------------

    pub async fn set_group_volume(&self, volume: u8) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        services::set_group_volume(&self.inner.soap, &ip, volume).await
    }

    pub async fn set_member_volume(&self, member_id: &str, volume: u8) -> Result<(), SoneError> {
        let ip = {
            let st = lock(&self.inner.state);
            let id = st.selected.clone().ok_or_else(no_group_selected)?;
            st.groups
                .iter()
                .find(|g| g.coordinator_id == id)
                .ok_or_else(unknown_group)?
                .members
                .iter()
                .find(|m| m.id == member_id)
                .map(|m| m.ip.clone())
                .ok_or_else(|| SoneError::Sonos("Unknown Sonos group member".into()))?
        };
        services::set_volume(&self.inner.soap, &ip, volume).await
    }

    pub async fn set_mute(&self, muted: bool) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        services::set_group_mute(&self.inner.soap, &ip, muted).await
    }

    pub async fn set_play_mode(&self, repeat: u8) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        services::set_play_mode(&self.inner.soap, &ip, services::repeat_to_play_mode(repeat)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_load_fast_start() {
        let ids: Vec<u64> = (1..=40).collect();
        let p = plan_load(&ids, 10);
        assert_eq!(p.prepend, (1..=10).collect::<Vec<_>>());
        assert_eq!(p.initial, (11..=26).collect::<Vec<_>>());
        assert_eq!(p.initial_start, 0);
        assert_eq!(p.append, (27..=40).collect::<Vec<_>>());
    }
    #[test]
    fn plan_load_caps_at_1000() {
        let ids: Vec<u64> = (1..=1500).collect();
        let p = plan_load(&ids, 0);
        assert_eq!(p.prepend.len() + p.initial.len() + p.append.len(), 1000);
    }
    #[test]
    fn plan_load_caps_when_start_is_late() {
        let ids: Vec<u64> = (1..=1500).collect();
        let p = plan_load(&ids, 1200);
        assert_eq!(p.initial[0], 1201);
        assert_eq!(p.prepend.len() + p.initial.len() + p.append.len(), 1000);
    }
    #[test]
    fn plan_load_short_queue() {
        let p = plan_load(&[7], 0);
        assert_eq!(p.initial, vec![7]);
        assert!(p.append.is_empty() && p.prepend.is_empty());
    }
    #[test]
    fn plan_load_start_out_of_range_clamps() {
        let p = plan_load(&[1, 2, 3], 9);
        assert_eq!(p.initial, vec![3]);
        assert_eq!(p.prepend, vec![1, 2]);
    }
    #[test]
    fn plan_load_empty() {
        let p = plan_load(&[], 0);
        assert!(p.initial.is_empty() && p.prepend.is_empty() && p.append.is_empty());
    }
    #[test]
    fn fill_stops_on_foreign_update() {
        assert!(fill_should_continue(Some(12), 12, 3, 3));
        assert!(!fill_should_continue(Some(12), 13, 3, 3));
        assert!(!fill_should_continue(Some(12), 12, 3, 4));
        assert!(fill_should_continue(None, 99, 3, 3));
    }
    #[test]
    fn frontend_index_is_zero_based_sonos_is_one_based() {
        assert_eq!(queue_position(0), 1);
        assert_eq!(queue_position(41), 42);
    }
}
