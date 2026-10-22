//! Sonos casting manager: topology, group selection, queue loading and
//! transport/volume control for the selected group.
//!
//! All shared state sits behind a `std::sync::Mutex` that is never held across
//! an `.await`; network calls work on values cloned out of it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::error::SoneError;
use crate::sonos::discovery::{discover, fetch_topology};
use crate::sonos::services::{self, PositionInfo, TransportState};
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
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// `sonos-state` is re-sent at least this often so the frontend re-anchors
/// its position interpolation.
const STATE_REANCHOR: Duration = Duration::from_secs(5);
const TOPOLOGY_REFRESH: Duration = Duration::from_secs(30);
const TOPOLOGY_RETRY_EMPTY: Duration = Duration::from_secs(60);
/// Sonos caps `RequestedCount` at 100.
const QUEUE_PAGE: u32 = 100;
const RECONNECTING_AFTER_FAILURES: u32 = 3;
const LOST_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Reachability {
    Ok,
    Reconnecting,
    Lost,
}

/// Turns a stream of poll successes/failures into a [`Reachability`]:
/// 3 consecutive failures = Reconnecting, 30 s since the first failure = Lost,
/// any success = Ok.
#[derive(Default)]
pub struct ReachTracker {
    failures: u32,
    first_failure: Option<Instant>,
}

impl ReachTracker {
    pub fn record(&mut self, ok: bool, now: Instant) -> Reachability {
        if ok {
            self.failures = 0;
            self.first_failure = None;
            return Reachability::Ok;
        }
        self.failures += 1;
        let first = *self.first_failure.get_or_insert(now);
        if now.saturating_duration_since(first) >= LOST_AFTER {
            Reachability::Lost
        } else if self.failures >= RECONNECTING_AFTER_FAILURES {
            Reachability::Reconnecting
        } else {
            Reachability::Ok
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MemberVolume {
    pub member_id: String,
    pub volume: u8,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SonosState {
    pub coordinator_id: String,
    pub transport_state: TransportState,
    /// 0-based queue index; `None` when not playing from the queue (TV input).
    pub track_index: Option<usize>,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    pub group_volume: u8,
    pub muted: bool,
    pub member_volumes: Vec<MemberVolume>,
    pub repeat: u8,
    pub current_item: Option<QueueItem>,
    pub reachability: Reachability,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SonosQueue {
    pub coordinator_id: String,
    pub update_id: u32,
    pub items: Vec<QueueItem>,
    /// A load's back-fill (or appends queued behind it) is still running:
    /// `items` is partial and will grow.
    pub backfilling: bool,
}

/// The queue index and current item of a [`PositionInfo`]. `from_queue` is
/// whether the transport plays from the speaker's queue (see
/// [`services::is_queue_uri`]): other sources (radio, line-in, Spotify
/// Connect, AirPlay, TV) report `Track` 1 too, which is not a queue position.
/// Their item is never treated as TIDAL (it isn't in the queue SONE mirrors).
pub fn current_from_position(
    info: &PositionInfo,
    from_queue: bool,
    ip: &str,
) -> (Option<usize>, Option<QueueItem>) {
    let kind = classify_uri(&info.track_uri);
    if kind == QueueItemKind::Tv {
        let tv = QueueItem {
            kind: QueueItemKind::Tv,
            tidal_track_id: None,
            title: "TV".into(),
            artist: String::new(),
            album_art_url: None,
            duration_secs: None,
        };
        return (None, Some(tv));
    }
    let parsed = if info.track_uri.is_empty() {
        None
    } else {
        parse_didl_items(&info.track_meta, ip).into_iter().next()
    };
    if from_queue {
        // 1-based on Sonos (0 = none).
        let index = info.track_number.checked_sub(1).map(|i| i as usize);
        return (index, parsed);
    }
    if info.track_uri.is_empty() {
        return (None, None);
    }
    let item = match parsed {
        Some(item) => QueueItem {
            kind: QueueItemKind::Other,
            tidal_track_id: None,
            ..item
        },
        None => QueueItem {
            kind: QueueItemKind::Other,
            tidal_track_id: None,
            title: String::new(),
            artist: String::new(),
            album_art_url: None,
            duration_secs: info.duration_secs,
        },
    };
    (None, Some(item))
}

/// The group stopped by itself mid-queue (not at the end, not by SONE, not a
/// non-queue source such as the TV input or radio, before or after):
/// playback was interrupted, e.g. a track became unavailable. `prev` is the
/// last state that wasn't Transitioning.
pub fn detect_unexpected_stop(
    prev: &SonosState,
    next: &SonosState,
    queue_len: usize,
    user_stopped: bool,
) -> bool {
    if user_stopped
        || prev.transport_state != TransportState::Playing
        || next.transport_state != TransportState::Stopped
    {
        return false;
    }
    prev.track_index.is_some() && matches!(next.track_index, Some(i) if i + 1 < queue_len)
}

/// A `Playing` observation may clear `user_stopped` only if SONE did not mark
/// a user stop since the observation's transport read began (otherwise the
/// observation is stale).
pub fn should_clear_user_stopped(epoch_at_read: u64, current_epoch: u64) -> bool {
    epoch_at_read == current_epoch
}

/// Whether `next` should be emitted: first state, any change besides the
/// position, or the re-anchor interval elapsed.
pub fn state_needs_emit(
    last: Option<&SonosState>,
    next: &SonosState,
    since_last_emit: Duration,
) -> bool {
    let Some(last) = last else { return true };
    let mut a = last.clone();
    a.position_secs = 0.0;
    let mut b = next.clone();
    b.position_secs = 0.0;
    a != b || since_last_emit >= STATE_REANCHOR
}

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

/// How many of `wanted` more tracks fit in a queue of `queue_len`.
pub fn append_room(queue_len: usize, wanted: usize) -> usize {
    wanted.min(MAX_QUEUE_TRACKS.saturating_sub(queue_len))
}

/// Takes the next batch of the pending tail for a queue of `queue_len`
/// tracks: in order, at most [`ADD_BATCH`], and only what fits under
/// [`MAX_QUEUE_TRACKS`]. Once the queue is full the rest is dropped. Returns
/// the batch and how many ids were dropped.
pub fn next_tail_batch(tail: &mut VecDeque<u64>, queue_len: usize) -> (Vec<u64>, usize) {
    let take = append_room(queue_len, tail.len().min(ADD_BATCH));
    if take == 0 {
        let dropped = tail.len();
        tail.clear();
        return (Vec::new(), dropped);
    }
    (tail.drain(..take).collect(), 0)
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
    /// Consecutive refreshes that found nothing while groups were known.
    empty_refreshes: u32,
    selected: Option<String>,
    manual_ip: Option<String>,
    last_coordinator: Option<String>,
    tidal_sn: Option<u32>,
}

/// The running back-fill and the appends queued behind it.
#[derive(Default)]
struct Backfill {
    /// Generation of the running back-fill; `None` when none runs. Only the
    /// one matching `fill_generation` counts.
    running: Option<u64>,
    /// Ids `queue_append` received while the back-fill ran; it adds them, in
    /// order, after its own batches.
    tail: VecDeque<u64>,
}

struct Inner {
    soap: SoapClient,
    state: Mutex<State>,
    /// Bumped by every new load/selection (and queue wipes); a running
    /// back-fill whose generation no longer matches stops.
    fill_generation: AtomicU64,
    /// Leaf lock (nothing else is taken while it is held). `fill_generation`
    /// is only bumped under it, so a pending tail never outlives its load.
    backfill: Mutex<Backfill>,
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
    /// Set by SONE's own pause / queue_clear / load_queue; cleared by the
    /// poller on the next observed Playing. Suppresses the unexpected-stop error.
    user_stopped: AtomicBool,
    /// Incremented on every `user_stopped` set (see `should_clear_user_stopped`).
    stop_epoch: AtomicU64,
    /// Identifies the current poller; changed (under the `poller` lock) by
    /// every `select`. A poller whose token no longer matches exits.
    poller_token: AtomicU64,
    /// Set by `start`; the poller emits through it.
    app: Mutex<Option<AppHandle>>,
    /// The running poller for the selected group (at most one).
    poller: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
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
                backfill: Mutex::new(Backfill::default()),
                queue_lock: Arc::new(tokio::sync::Mutex::new(())),
                own_update_id: Mutex::new(None),
                position: Mutex::new(None),
                user_stopped: AtomicBool::new(false),
                stop_epoch: AtomicU64::new(0),
                poller_token: AtomicU64::new(0),
                app: Mutex::new(None),
                poller: Mutex::new(None),
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

    /// Written by the poller.
    pub fn record_position(&self, position: Option<f32>) {
        *lock(&self.inner.position) = position;
    }

    pub fn tidal_sn(&self) -> Option<u32> {
        lock(&self.inner.state).tidal_sn
    }

    fn mark_user_stopped(&self) {
        self.inner.stop_epoch.fetch_add(1, Ordering::SeqCst);
        self.inner.user_stopped.store(true, Ordering::SeqCst);
    }

    /// New generation: cancels a running back-fill's remaining batches and
    /// discards the appends queued behind it.
    fn bump_generation(&self) -> u64 {
        let mut b = lock(&self.inner.backfill);
        b.tail.clear();
        self.inner.fill_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Whether the current generation's back-fill (or its pending tail) is
    /// still adding tracks.
    pub fn backfilling(&self) -> bool {
        let b = lock(&self.inner.backfill);
        b.running == Some(self.inner.fill_generation.load(Ordering::SeqCst))
    }

    /// Queues `ids` behind the running back-fill of the current generation;
    /// false (nothing queued) when none runs.
    fn push_tail_if_backfilling(&self, ids: &[u64]) -> bool {
        let mut b = lock(&self.inner.backfill);
        if b.running != Some(self.inner.fill_generation.load(Ordering::SeqCst)) {
            return false;
        }
        b.tail.extend(ids.iter().copied());
        true
    }

    /// The back-fill of `generation` is over (no-op if a newer one took over).
    fn finish_backfill(&self, generation: u64) {
        let mut b = lock(&self.inner.backfill);
        if b.running == Some(generation) {
            b.running = None;
            b.tail.clear();
        }
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

    /// Reloads the topology. Seeds, in order: the manual IP plus the IPs of
    /// currently known members (cheap, immune to a lossy SSDP round); SSDP
    /// discovery only if that fails. A non-empty topology is only replaced by
    /// an empty one after two consecutive empty refreshes.
    pub async fn refresh_topology(&self) -> Result<Vec<SonosGroup>, SoneError> {
        let (manual, known) = {
            let st = lock(&self.inner.state);
            let mut known: Vec<String> = st.manual_ip.iter().cloned().collect();
            for m in st.groups.iter().flat_map(|g| g.members.iter()) {
                if !known.contains(&m.ip) {
                    known.push(m.ip.clone());
                }
            }
            (st.manual_ip.clone(), known)
        };
        let mut result = Err(SoneError::Sonos("no Sonos speakers to query".into()));
        if !known.is_empty() {
            result = fetch_topology(&self.inner.soap, &known).await;
        }
        if result.is_err() {
            let seeds = discover(DISCOVERY_TIMEOUT).await;
            if !seeds.is_empty() {
                result = fetch_topology(&self.inner.soap, &seeds).await;
            } else if manual.is_none() {
                // Nothing answered: an empty, completed scan.
                result = Ok(Vec::new());
            }
        }
        let mut st = lock(&self.inner.state);
        st.scanned = true;
        let groups = result?;
        if groups.is_empty() && !st.groups.is_empty() {
            st.empty_refreshes += 1;
            if st.empty_refreshes < 2 {
                return Ok(st.groups.clone());
            }
        }
        st.empty_refreshes = 0;
        st.groups = groups.clone();
        Ok(groups)
    }

    /// Discovers speakers in the background: emits `sonos-topology` after each
    /// refresh, every 30 s (every 60 s while no group was found). Also hands
    /// the app handle to the poller.
    pub fn start(&self, app: AppHandle) {
        *lock(&self.inner.app) = Some(app.clone());
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                if let Err(e) = this.refresh_topology().await {
                    log::debug!("[sonos] topology refresh failed: {e}");
                }
                let groups = lock(&this.inner.state).groups.clone();
                let _ = app.emit("sonos-topology", &groups);
                tokio::time::sleep(if groups.is_empty() {
                    TOPOLOGY_RETRY_EMPTY
                } else {
                    TOPOLOGY_REFRESH
                })
                .await;
            }
        });
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
        // One critical section under the poller lock: concurrent selects
        // serialize, so the surviving poller always matches `selected`.
        let mut poller = lock(&self.inner.poller);
        {
            let mut st = lock(&self.inner.state);
            match &coordinator_id {
                Some(id) => st.last_coordinator = Some(id.clone()),
                None if forget_last => st.last_coordinator = None,
                None => {}
            }
            st.selected = coordinator_id.clone();
        }
        // New token first: the old poller (abort is asynchronous) sees it no
        // longer owns the selection and exits before touching anything.
        let token = self.inner.poller_token.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(old) = poller.take() {
            old.abort();
        }
        self.bump_generation();
        self.set_own_update_id(None);
        *lock(&self.inner.position) = None;

        let (Some(id), Some(app)) = (coordinator_id, lock(&self.inner.app).clone()) else {
            return;
        };
        let this = self.clone();
        let handle = tauri::async_runtime::spawn(async move {
            this.poll_loop(app, id, token).await;
        });
        if let Some(stale) = poller.replace(handle) {
            stale.abort();
        }
    }

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
    ///
    /// Returns whether a back-fill continues in the background.
    pub async fn load_queue(
        &self,
        track_ids: &[u64],
        start_index: usize,
        position_secs: f64,
        play: bool,
    ) -> Result<bool, SoneError> {
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
        let needs_fill = !plan.append.is_empty() || !plan.prepend.is_empty();
        if needs_fill {
            // Before the first queue write: every queue the poller reads from
            // now on reports `backfilling`, and appends queue up behind it.
            lock(&self.inner.backfill).running = Some(generation);
        }
        let soap = &self.inner.soap;
        self.mark_user_stopped();

        if let Err(e) = services::remove_all(soap, &ip).await {
            self.finish_backfill(generation);
            return Err(e);
        }
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
            self.finish_backfill(generation);
            log::warn!(
                "[sonos] load_queue failed after clearing the queue; queue left empty/partial: {e}"
            );
            return Err(e);
        }

        if needs_fill {
            let this = self.clone();
            // The owned guard moves into the task: no gap in which another
            // queue op could run before the prepend phase starts.
            tauri::async_runtime::spawn(async move {
                this.run_backfill(guard, ip, sn, generation, plan).await;
                this.finish_backfill(generation);
            });
        }
        Ok(needs_fill)
    }

    /// The queue's current length if the back-fill of `generation` may add
    /// its next batch (see [`fill_should_continue`]); `None` if it must stop.
    async fn fill_check(&self, ip: &str, generation: u64) -> Option<usize> {
        if generation != self.inner.fill_generation.load(Ordering::SeqCst) {
            return None;
        }
        match services::browse(&self.inner.soap, ip, "Q:0", 0, 1).await {
            Ok(b) => fill_should_continue(
                self.own_update_id(),
                b.update_id,
                generation,
                self.inner.fill_generation.load(Ordering::SeqCst),
            )
            .then_some(b.total_matches as usize),
            Err(e) => {
                log::warn!("[sonos] back-fill stopped, cannot read queue: {e}");
                None
            }
        }
    }

    /// Back-fill order: earlier tracks first (inserted before the playing
    /// start track), then the tail, then whatever `queue_append` queued up
    /// meanwhile. `prepend_guard` (the queue lock handed over by
    /// `load_queue`) is held for the whole prepend phase, so foreground index
    /// ops never see a half-inserted prepend and a generation bump can only
    /// cancel the remaining append batches. The append phases take the lock
    /// per batch. The caller marks the back-fill finished afterwards.
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
            if self.fill_check(&ip, generation).await.is_none() {
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
            if self.fill_check(&ip, generation).await.is_none() {
                return;
            }
            if let Err(e) = self.add_tracks(&ip, sn, chunk, 0).await {
                log::warn!("[sonos] back-fill append failed: {e}");
                return;
            }
        }

        // The pending tail. Whether it is empty is decided under the
        // backfill lock, together with clearing `running`, so an append
        // either lands in the tail before that or goes the direct way after.
        loop {
            let _guard = self.inner.queue_lock.lock().await;
            let Some(queue_len) = self.fill_check(&ip, generation).await else {
                return;
            };
            let batch = {
                let mut b = lock(&self.inner.backfill);
                if b.running != Some(generation) {
                    return;
                }
                let (batch, dropped) = next_tail_batch(&mut b.tail, queue_len);
                if dropped > 0 {
                    log::info!(
                        "[sonos] queue full ({MAX_QUEUE_TRACKS}): dropped {dropped} appended tracks"
                    );
                }
                if batch.is_empty() {
                    b.running = None;
                    return;
                }
                batch
            };
            if let Err(e) = self.add_tracks(&ip, sn, &batch, 0).await {
                log::warn!("[sonos] back-fill tail append failed: {e}");
                return;
            }
        }
    }

    /// Appends `ids` at the end of the queue. While the current load's
    /// back-fill runs they queue up behind it (it adds them in order once
    /// its own tracks are in); otherwise they go in right away. Whatever
    /// exceeds the queue cap is dropped.
    pub async fn queue_append(&self, ids: &[u64]) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        if ids.is_empty() {
            return Ok(());
        }
        let sn = self.ensure_sn(&ip).await?;
        // Checked before the lock too: the prepend phase holds it for long.
        if self.push_tail_if_backfilling(ids) {
            return Ok(());
        }
        let _guard = self.inner.queue_lock.lock().await;
        // A load may have started while we waited.
        if self.push_tail_if_backfilling(ids) {
            return Ok(());
        }
        let queue_len = services::browse(&self.inner.soap, &ip, "Q:0", 0, 1)
            .await?
            .total_matches as usize;
        let fit = append_room(queue_len, ids.len());
        if fit < ids.len() {
            log::info!(
                "[sonos] queue full ({MAX_QUEUE_TRACKS}): dropped {} appended tracks",
                ids.len() - fit
            );
        }
        self.add_tracks(&ip, sn, &ids[..fit], 0).await
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
        self.mark_user_stopped();
        services::remove_all(&self.inner.soap, &ip).await?;
        self.set_own_update_id(None);
        Ok(())
    }

    // ---- poller -----------------------------------------------------------

    /// (coordinator ip, visible members as (id, ip)) of a group.
    fn poll_target(&self, coordinator_id: &str) -> Option<(String, Vec<(String, String)>)> {
        let st = lock(&self.inner.state);
        let g = st
            .groups
            .iter()
            .find(|g| g.coordinator_id == coordinator_id)?;
        let ip = g.members.first()?.ip.clone();
        Some((
            ip,
            g.members
                .iter()
                .map(|m| (m.id.clone(), m.ip.clone()))
                .collect(),
        ))
    }

    /// One poll of the coordinator. Transport state and position are the
    /// liveness probe (their failure is an error); the rest falls back to the
    /// previous values. `media` caches (track URI, plays from the queue):
    /// `GetMediaInfo` is only asked again when the track URI changes.
    async fn read_state(
        &self,
        coordinator_id: &str,
        ip: &str,
        members: &[(String, String)],
        prev: Option<&SonosState>,
        media: &mut Option<(String, bool)>,
    ) -> Result<SonosState, SoneError> {
        let soap = &self.inner.soap;
        let transport_state = services::transport_state(soap, ip).await?;
        let info = services::position_info(soap, ip).await?;
        let from_queue = match media.as_ref() {
            Some((uri, from_queue)) if *uri == info.track_uri => *from_queue,
            _ => match services::media_info(soap, ip).await {
                Ok(current_uri) => {
                    let from_queue = services::is_queue_uri(&current_uri);
                    *media = Some((info.track_uri.clone(), from_queue));
                    from_queue
                }
                Err(e) => {
                    // Not cached, so the next poll asks again; assume the queue
                    // meanwhile (the behaviour before GetMediaInfo was used).
                    log::debug!("[sonos] media info failed: {e}");
                    true
                }
            },
        };
        let (vol, mute, mode) = tokio::join!(
            services::get_group_volume(soap, ip),
            services::get_group_mute(soap, ip),
            services::get_play_mode(soap, ip),
        );
        let mut member_volumes = Vec::with_capacity(members.len());
        for (id, mip) in members {
            let volume = match services::get_volume(soap, mip).await {
                Ok(v) => Some(v),
                Err(_) => prev
                    .and_then(|p| p.member_volumes.iter().find(|m| &m.member_id == id))
                    .map(|m| m.volume),
            };
            if let Some(volume) = volume {
                member_volumes.push(MemberVolume {
                    member_id: id.clone(),
                    volume,
                });
            }
        }

        let is_tv = classify_uri(&info.track_uri) == QueueItemKind::Tv;
        let (track_index, current_item) = current_from_position(&info, from_queue, ip);
        Ok(SonosState {
            coordinator_id: coordinator_id.to_string(),
            transport_state,
            track_index,
            position_secs: if is_tv { 0.0 } else { info.position_secs },
            duration_secs: if is_tv { None } else { info.duration_secs },
            group_volume: vol.ok().or(prev.map(|p| p.group_volume)).unwrap_or(0),
            muted: mute.ok().or(prev.map(|p| p.muted)).unwrap_or(false),
            member_volumes,
            repeat: mode
                .ok()
                .map(|m| services::play_mode_to_repeat(&m))
                .or(prev.map(|p| p.repeat))
                .unwrap_or(0),
            current_item,
            reachability: Reachability::Ok,
        })
    }

    /// Browses the whole queue in pages of [`QUEUE_PAGE`].
    async fn read_queue(&self, ip: &str) -> Result<(u32, Vec<QueueItem>), SoneError> {
        let mut items = Vec::new();
        let mut start = 0u32;
        loop {
            let page = services::browse(&self.inner.soap, ip, "Q:0", start, QUEUE_PAGE).await?;
            items.extend(parse_didl_items(&page.didl, ip));
            start += page.number_returned;
            if page.number_returned == 0 || start >= page.total_matches {
                return Ok((page.update_id, items));
            }
        }
    }

    fn owns(&self, token: u64) -> bool {
        self.inner.poller_token.load(Ordering::SeqCst) == token
    }

    /// Polls the selected group's coordinator once a second until aborted or
    /// superseded. Aborting only takes effect at an await point, so every
    /// side effect re-checks `owns(token)` after the awaits before it.
    async fn poll_loop(self, app: AppHandle, coordinator_id: String, token: u64) {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut tracker = ReachTracker::default();
        let mut prev: Option<SonosState> = None;
        let mut last_emit = Instant::now();
        // Last state that wasn't Transitioning: an unexpected stop is judged
        // against it, so Playing → Transitioning → Stopped still counts.
        let mut settled: Option<SonosState> = None;
        let mut media: Option<(String, bool)> = None;
        let mut queue_update_id: Option<u32> = None;
        let mut queue_len = 0usize;
        // Last emitted queue, re-sent when only `backfilling` changes.
        let mut last_queue: Option<SonosQueue> = None;
        loop {
            interval.tick().await;
            let now = Instant::now();
            let epoch_at_read = self.inner.stop_epoch.load(Ordering::SeqCst);
            // A group that left the topology counts as a failed poll, so
            // reachability can still reach Lost.
            let read = match self.poll_target(&coordinator_id) {
                None => Err(SoneError::Sonos("group not in topology".into())),
                Some((ip, members)) => self
                    .read_state(&coordinator_id, &ip, &members, prev.as_ref(), &mut media)
                    .await
                    .map(|s| (s, ip)),
            };
            if !self.owns(token) {
                return;
            }
            let (mut state, ip) = match read {
                Ok(r) => r,
                Err(e) => {
                    log::debug!("[sonos] poll failed: {e}");
                    let reach = tracker.record(false, now);
                    // Surface a reachability change even without fresh data.
                    if let Some(p) = prev.as_mut() {
                        if p.reachability != reach {
                            p.reachability = reach;
                            let _ = app.emit("sonos-state", &*p);
                            last_emit = now;
                        }
                    }
                    continue;
                }
            };
            state.reachability = tracker.record(true, now);
            self.record_position(Some(state.position_secs as f32));

            // Queue change detection.
            let probe = services::browse(&self.inner.soap, &ip, "Q:0", 0, 1).await;
            if !self.owns(token) {
                return;
            }
            match probe {
                Ok(b) if queue_update_id != Some(b.update_id) => {
                    // A change that is neither our last emitted view nor our own
                    // write is a foreign edit: cancel any running back-fill. If
                    // the queue lock is held, an own mutation is in flight, so
                    // skip. The first observation after selecting has no
                    // baseline to compare against.
                    if !self.owns(token) {
                        return;
                    }
                    if queue_update_id.is_some() {
                        if let Ok(_free) = self.inner.queue_lock.try_lock() {
                            if self.own_update_id() != Some(b.update_id) {
                                self.bump_generation();
                            }
                        }
                    }
                    let queue = self.read_queue(&ip).await;
                    if !self.owns(token) {
                        return;
                    }
                    match queue {
                        Ok((update_id, items)) => {
                            queue_len = items.len();
                            queue_update_id = Some(update_id);
                            let q = SonosQueue {
                                coordinator_id: coordinator_id.clone(),
                                update_id,
                                items,
                                backfilling: self.backfilling(),
                            };
                            let _ = app.emit("sonos-queue", &q);
                            last_queue = Some(q);
                        }
                        Err(e) => log::debug!("[sonos] queue read failed: {e}"),
                    }
                }
                Ok(_) => {
                    // Same queue, but the back-fill may have just finished
                    // (or started) without a write of its own.
                    if let Some(q) = last_queue.as_mut() {
                        let backfilling = self.backfilling();
                        if q.backfilling != backfilling {
                            q.backfilling = backfilling;
                            let _ = app.emit("sonos-queue", &*q);
                        }
                    }
                }
                Err(e) => log::debug!("[sonos] queue probe failed: {e}"),
            }

            if !self.owns(token) {
                return;
            }
            if state.transport_state == TransportState::Playing {
                if should_clear_user_stopped(
                    epoch_at_read,
                    self.inner.stop_epoch.load(Ordering::SeqCst),
                ) {
                    self.inner.user_stopped.store(false, Ordering::SeqCst);
                }
            } else if let Some(p) = &settled {
                let user_stopped = self.inner.user_stopped.load(Ordering::SeqCst);
                if detect_unexpected_stop(p, &state, queue_len, user_stopped) {
                    let _ = app.emit(
                        "sonos-playback-error",
                        serde_json::json!({ "kind": "unavailable" }),
                    );
                }
            }

            if state_needs_emit(prev.as_ref(), &state, now.duration_since(last_emit)) {
                let _ = app.emit("sonos-state", &state);
                last_emit = now;
            }
            if state.transport_state != TransportState::Transitioning {
                settled = Some(state.clone());
            }
            prev = Some(state);
        }
    }

    // ---- transport ----------------------------------------------------------

    pub async fn play(&self) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        services::play(&self.inner.soap, &ip).await
    }

    pub async fn pause(&self) -> Result<(), SoneError> {
        let (_, ip) = self.selected_coordinator()?;
        self.mark_user_stopped();
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

    fn st(transport: TransportState, idx: Option<usize>) -> SonosState {
        SonosState {
            coordinator_id: "RINCON_A".into(),
            transport_state: transport,
            track_index: idx,
            position_secs: 10.0,
            duration_secs: Some(200.0),
            group_volume: 30,
            muted: false,
            member_volumes: vec![MemberVolume {
                member_id: "RINCON_A".into(),
                volume: 30,
            }],
            repeat: 0,
            current_item: None,
            reachability: Reachability::Ok,
        }
    }

    #[test]
    fn reach_tracker_transitions() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut r = ReachTracker::default();
        assert_eq!(r.record(false, t0), Reachability::Ok);
        assert_eq!(r.record(false, t0 + s(1)), Reachability::Ok);
        assert_eq!(r.record(false, t0 + s(2)), Reachability::Reconnecting);
        assert_eq!(r.record(false, t0 + s(30)), Reachability::Lost);
        assert_eq!(r.record(true, t0 + s(31)), Reachability::Ok);
        // A success resets the failure window.
        assert_eq!(r.record(false, t0 + s(100)), Reachability::Ok);
    }

    #[test]
    fn unexpected_stop_detected_mid_queue() {
        let prev = st(TransportState::Playing, Some(2));
        let next = st(TransportState::Stopped, Some(2));
        assert!(detect_unexpected_stop(&prev, &next, 10, false));
    }

    #[test]
    fn stop_at_queue_end_is_not_unexpected() {
        let prev = st(TransportState::Playing, Some(9));
        let next = st(TransportState::Stopped, Some(9));
        assert!(!detect_unexpected_stop(&prev, &next, 10, false));
        let prev = st(TransportState::Playing, Some(2));
        let next = st(TransportState::Stopped, Some(2));
        assert!(!detect_unexpected_stop(&prev, &next, 10, true));
    }

    #[test]
    fn stop_from_non_playing_or_tv_is_not_unexpected() {
        let prev = st(TransportState::Paused, Some(2));
        let next = st(TransportState::Stopped, Some(2));
        assert!(!detect_unexpected_stop(&prev, &next, 10, false));
        let prev = st(TransportState::Playing, None);
        let next = st(TransportState::Stopped, None);
        assert!(!detect_unexpected_stop(&prev, &next, 10, false));
    }

    #[test]
    fn stop_after_switching_from_a_non_queue_source_is_not_unexpected() {
        let prev = st(TransportState::Playing, None);
        let next = st(TransportState::Stopped, Some(2));
        assert!(!detect_unexpected_stop(&prev, &next, 10, false));
    }

    #[test]
    fn tail_batches_drain_in_order_up_to_16() {
        let mut tail: VecDeque<u64> = (1..=40).collect();
        let (b, dropped) = next_tail_batch(&mut tail, 100);
        assert_eq!(b, (1..=16).collect::<Vec<_>>());
        assert_eq!(dropped, 0);
        let (b, _) = next_tail_batch(&mut tail, 116);
        assert_eq!(b, (17..=32).collect::<Vec<_>>());
        let (b, _) = next_tail_batch(&mut tail, 132);
        assert_eq!(b, (33..=40).collect::<Vec<_>>());
        let (b, dropped) = next_tail_batch(&mut tail, 140);
        assert!(b.is_empty());
        assert_eq!(dropped, 0);
    }

    #[test]
    fn tail_batches_respect_the_1000_cap() {
        let mut tail: VecDeque<u64> = (1..=20).collect();
        // Room for 5: they go in, the rest is dropped on the next call.
        let (b, dropped) = next_tail_batch(&mut tail, 995);
        assert_eq!(b, vec![1, 2, 3, 4, 5]);
        assert_eq!(dropped, 0);
        let (b, dropped) = next_tail_batch(&mut tail, 1000);
        assert!(b.is_empty());
        assert_eq!(dropped, 15);
        assert!(tail.is_empty());
        // Over-full (foreign edits) behaves like full.
        let mut tail: VecDeque<u64> = VecDeque::from(vec![7]);
        assert_eq!(next_tail_batch(&mut tail, 1200), (Vec::new(), 1));
    }

    #[test]
    fn append_room_caps_at_1000() {
        assert_eq!(append_room(0, 30), 30);
        assert_eq!(append_room(990, 30), 10);
        assert_eq!(append_room(1000, 30), 0);
        assert_eq!(append_room(1500, 30), 0);
    }

    fn tidal_position() -> PositionInfo {
        let m = crate::sonos::soap::parse_response(
            include_str!("fixtures/position_info_tidal.xml"),
            "GetPositionInfo",
        )
        .unwrap();
        services::parse_position_info(&m)
    }

    #[test]
    fn queue_source_maps_track_number_to_index() {
        let (index, item) = current_from_position(&tidal_position(), true, "192.0.2.34");
        assert_eq!(index, Some(4));
        let item = item.unwrap();
        assert_eq!(item.kind, QueueItemKind::Tidal);
        assert_eq!(item.tidal_track_id, Some(57770232));
    }

    #[test]
    fn non_queue_source_has_no_index_and_is_not_tidal() {
        let mut info = tidal_position();
        info.track_number = 1;
        let (index, item) = current_from_position(&info, false, "192.0.2.34");
        assert_eq!(index, None);
        let item = item.unwrap();
        assert_eq!(item.kind, QueueItemKind::Other);
        assert_eq!(item.tidal_track_id, None);

        // A stream without metadata still reports something to show.
        info.track_uri = "x-sonosapi-stream:s24896?sid=254".into();
        info.track_meta = String::new();
        let (index, item) = current_from_position(&info, false, "192.0.2.34");
        assert_eq!(index, None);
        assert_eq!(item.unwrap().kind, QueueItemKind::Other);
    }

    #[test]
    fn tv_input_stays_tv() {
        let m = crate::sonos::soap::parse_response(
            include_str!("fixtures/position_info_tv.xml"),
            "GetPositionInfo",
        )
        .unwrap();
        let info = services::parse_position_info(&m);
        for from_queue in [false, true] {
            let (index, item) = current_from_position(&info, from_queue, "192.0.2.214");
            assert_eq!(index, None);
            let item = item.unwrap();
            assert_eq!(item.kind, QueueItemKind::Tv);
            assert_eq!(item.title, "TV");
        }
    }

    #[test]
    fn stale_playing_does_not_clear_user_stopped() {
        assert!(should_clear_user_stopped(4, 4));
        assert!(!should_clear_user_stopped(4, 5));
    }

    #[test]
    fn state_emission_cadence() {
        let a = st(TransportState::Playing, Some(1));
        let mut moved = a.clone();
        moved.position_secs = 55.0;
        let short = Duration::from_secs(1);
        assert!(state_needs_emit(None, &a, short));
        // Only the position moved: wait for the 5 s re-anchor.
        assert!(!state_needs_emit(Some(&a), &moved, short));
        assert!(state_needs_emit(Some(&a), &moved, Duration::from_secs(5)));
        let mut louder = a.clone();
        louder.group_volume = 31;
        assert!(state_needs_emit(Some(&a), &louder, short));
        let mut lost = a.clone();
        lost.reachability = Reachability::Lost;
        assert!(state_needs_emit(Some(&a), &lost, short));
    }
}
