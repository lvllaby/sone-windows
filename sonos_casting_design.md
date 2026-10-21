# Sonos casting and remote control — design

Date: 2026-10-21
Status: approved design, pending spec review

## 1. Intent

SONE should control and play on Sonos speakers the way the Spotify desktop app did via Spotify Connect: pick a speaker/group, and from then on SONE's normal player controls (play/pause, skip, seek, volume, queue) drive that Sonos group, which streams TIDAL itself through the TIDAL service linked in the Sonos app.

The user's setup: a solo Sonos Beam and a group of two Sonos One SL, TIDAL already linked in the Sonos system.

### Agreed requirements

| # | Requirement |
|---|---|
| R1 | A Sonos button appears in the player bar only when Sonos speakers are discovered on the LAN. |
| R2 | Clicking it opens a picker listing "This computer" and every existing Sonos group. Selecting a group makes it the playback target. |
| R3 | **Cast + remote:** while a Sonos group is the target, everything played in SONE plays on that group, and all SONE transport/queue/volume controls act on it. |
| R4 | Grouping is **not** managed by SONE (pick among existing groups only). The picker shows group volume and, for multi-speaker groups, per-speaker volume. |
| R5 | **Handoff both ways:** switching target mid-playback carries the queue, current track and position over (local → Sonos, Sonos → local, Sonos → Sonos). |
| R6 | The Sonos queue is the source of truth while casting: the speakers keep playing the whole queue after SONE closes or the PC sleeps. On next launch SONE reattaches to the remembered group and mirrors its state. |

### Assumptions

- PC and speakers are on the same LAN; multicast is not blocked by the router.
- All new code is cross-platform (no `cfg` blocks needed); the Linux build must keep compiling. Windows is the platform verified manually.
- Out of scope for v1: regrouping speakers, Sonos alarms/sleep timers, other Sonos music services, SONE streaming its own audio to Sonos, UPnP event subscriptions (GENA), scrobbling of Sonos playback by SONE.

### Approach

Native Rust Sonos client (SSDP + SOAP over the local UPnP API) in the backend, plus a "playback target" seam in the frontend's `usePlaybackActions`. Rejected: moving the queue into the backend (large refactor of working code) and a Node/Python sidecar (packaging burden on an already-intricate installer). The `sonor` crate is not used (largely unmaintained); SoCo (Python) is the reference for SOAP actions and TIDAL URI formats.

## 2. Probe (first task, throwaway)

Casting depends on Sonos-specific TIDAL URIs whose exact form has drifted across firmware, roughly:

- Resource URI: `x-sonos-http:track%2f<trackId>.flac?sid=174&flags=<flags>&sn=<sn>`
- DIDL-Lite metadata with `<desc>SA_RINCON44551_X_#Svc44551-0-Token</desc>` (44551 = 174 × 256 + 7)

`sn` is the account serial number of the TIDAL link inside the Sonos system.

The probe, run against the user's Beam, must:

1. Discover `sn` by browsing items TIDAL already put in the Sonos system (`FV:2` favourites, `R:0` recents, `Q:0` queue) and parsing their URIs.
2. Play one TIDAL track on the Beam from a URI and DIDL we built ourselves, via `AddURIToQueue` + queue playback.
3. Capture sanitized fixture XML (`GetZoneGroupState`, a `Browse Q:0` result, a SOAP fault) for unit tests.

**Success:** one track plays from a built URI and `sn` is recovered programmatically. **If it fails:** stop and re-scope to remote-control-only (mirroring and controlling what is already playing) before any UI is built. Probe code is not kept as-is.

## 3. Backend: `src-tauri/src/sonos/`

| File | Responsibility |
|---|---|
| `discovery.rs` | SSDP `M-SEARCH` for `urn:schemas-upnp-org:device:ZonePlayer:1` to 239.255.255.250:1900, sent from **every IPv4 interface** (Hyper-V/VPN adapters otherwise capture the multicast). First responder → `ZoneGroupTopology.GetZoneGroupState` → full household. Also accepts a manually entered speaker IP as a seed. |
| `soap.rs` | Generic `call(host, service, action, args) -> Result<xml>`: envelope, `SOAPAction` header, UPnP fault code → `SoneError::Sonos`. Uses a dedicated `reqwest` client with a short timeout and **no proxy** (LAN traffic must bypass the user's proxy setting). |
| `services.rs` | Typed wrappers. **AVTransport:** `Play`, `Pause`, `Next`, `Previous`, `Seek` (`REL_TIME`, `TRACK_NR`), `SetAVTransportURI` (`x-rincon-queue:<RINCON>#0`), `AddURIToQueue`, `AddMultipleURIsToQueue` (batches of 16), `RemoveTrackFromQueue`, `ReorderTracksInQueue`, `RemoveAllTracksFromQueue`, `SetPlayMode`, `GetTransportInfo`, `GetPositionInfo`, `GetMediaInfo`. **GroupRenderingControl:** `Get/SetGroupVolume`, `Get/SetGroupMute`, `SnapshotGroupVolume`. **RenderingControl:** per-member `Get/SetVolume`. **ContentDirectory:** `Browse` (`Q:0`, `FV:2`, `R:0`). |
| `tidal.rs` | TIDAL track id → Sonos URI + DIDL-Lite; Sonos queue item → `QueueItem` (TIDAL track id if the URI is a TIDAL one, else title/artist/art from DIDL). `sn` discovery. Exact formats fixed by the probe. |
| `manager.rs` | `SonosManager` (field on `AppState`): topology snapshot, selected group (keyed by **coordinator RINCON id**, since group ids change on regroup), last polled state, poller task handle, handoff lock. |

XML parsing uses `quick-xml`; interface enumeration uses `if-addrs`. Both are new dependencies.

### Commands (`src-tauri/src/commands/sonos.rs`, registered in `generate_handler!`)

- Discovery/selection: `sonos_list_groups`, `sonos_rescan`, `sonos_set_manual_ip(Option<String>)`, `sonos_select_group(Option<coordinator_id>)` (`None` = This computer), `sonos_group_now_playing(coordinator_id)` (one-shot, for picker subtitles).
- Transport: `sonos_load_queue(track_ids, start_index, position_secs, play)`, `sonos_play`, `sonos_pause`, `sonos_next`, `sonos_previous`, `sonos_seek(position_secs)`, `sonos_play_index(index)`.
- Queue: `sonos_queue_insert(track_ids, at)`, `sonos_queue_remove(index)`, `sonos_queue_move(from, to)`, `sonos_queue_replace_after(index, track_ids)` (shuffle toggle), `sonos_queue_clear`.
- Volume/mode: `sonos_set_group_volume(0–100)`, `sonos_set_member_volume(member_id, 0–100)`, `sonos_set_mute(bool)`, `sonos_set_play_mode(repeat: 0|1|2)`.

Existing `get_playback_position` returns the last polled Sonos position while a group is selected, so `src/lib/playbackPosition.ts` works unchanged.

### Polling and events

- While a group is selected: a tokio task polls the coordinator every ~1 s (`GetTransportInfo`, `GetPositionInfo`, group volume/mute, member volumes) and emits **`sonos-state`** when anything changes: `{ coordinatorId, transportState, trackIndex, positionSecs, durationSecs, groupVolume, muted, memberVolumes, playMode, currentItem }`.
- When the queue `UpdateID` changes, it re-browses `Q:0` and emits **`sonos-queue`**: `{ updateId, items: QueueItem[] }`. This also catches edits made from the Sonos app.
- Topology refresh every 30 s → **`sonos-topology`** (`Group[]` with coordinator, members, room names, reachability). With no speakers found, discovery repeats every 60 s.

### Persistence

`Settings` gains `sonos_last_coordinator: Option<String>`, `sonos_tidal_sn: Option<u32>` and `sonos_manual_ip: Option<String>`. They're encrypted along with the rest of `settings.json`.

## 4. Frontend: playback-target layer

### The seam

- `atoms/playback.ts`: `playbackTargetAtom: { type: "local" } | { type: "sonos"; coordinatorId: string; name: string }`, plus `sonosGroupsAtom`, `sonosStateAtom`, `sonosGroupVolumeAtom` and `sonosHandoffAtom` (in-progress flag and target).
- Each public action in `usePlaybackActions` (`playTrack`, `pauseTrack`, `resumeTrack`, `playNext`, `playPrevious`, `seekTo`, `setVolume`, `setQueueTracks`, `appendToQueue`, `addToQueue`, `playNextInQueue`, `removeFromQueue`, `playFromQueue`, `toggleShuffle`, `setShuffledQueue`, `clearQueue`, `playFromSource`, `playAllFromSource`) gets a guard at the top. When the target is Sonos, it delegates to `src/hooks/sonosActions.ts` and leaves the local code paths unchanged.
- Components, shortcuts, miniplayer, tray and SMTC/`mpris:*` handlers already go through `usePlaybackActions`, so they need no changes.

### Sonos queue as source of truth

- **Flattening (on load):** `[...history (last ≤50), current, ...manualQueue, ...queue]`, starting at index `history.length`. Capped at 1,000 items. When more remain, SONE appends further tracks as the Sonos nears the end of the loaded portion, but only while SONE is running.
- **Mirroring:** `useSonosSync` (mounted in `AppInitializer`) maps `sonos-queue` and `sonos-state` onto the existing atoms. Items before the index go to `historyAtom`, the item at the index to `currentTrackAtom`, items after it to `queueAtom`, and the transport state to `isPlayingAtom`. TIDAL ids resolve to `Track` objects through the existing API cache, with batch fetches for misses. Non-TIDAL items become placeholder tracks built from the DIDL title, artist and art.
- **Play next:** `manualQueueAtom` stays empty while casting. "Play next" inserts at `index + 1 + pendingPlayNextCount`. That counter is local, decrements as the index advances, and resets on any queue change SONE didn't make.
- Pure helpers (flatten, mirror split, index math) live in `src/lib/sonosQueue.ts`.

### Modes

- **Repeat** maps to the Sonos play modes `NORMAL`, `REPEAT_ALL` and `REPEAT_ONE`.
- **Shuffle** keeps SONE's own algorithm. Toggling it calls `sonos_queue_replace_after(currentIndex, newOrder)`. The Sonos `SHUFFLE*` modes are never used, because they break the index-to-track mapping.
- **Autoplay:** when SONE is running, autoplay is enabled and the Sonos is within 2 items of the end, SONE appends recommendations. When SONE is not running, the Sonos stops at the end of the queue.

### Volume, position, OS integration

- `VolumeSlider` shows `sonosGroupVolumeAtom` (0–100 mapped to 0–1) while casting. The stored local `volumeAtom` is never overwritten. Mute uses `sonos_set_mute`.
- **Position:** the backend serves the polled value through `get_playback_position`, as described in section 3.
- **OS integration:** on track change, SONE calls a new command, `notify_track_started_remote`. It updates SMTC and Discord metadata and status but **skips scrobbling**, because Sonos can scrobble to Last.fm itself and doing both would double-count. The GStreamer `track-finished` listener never fires while casting.

## 5. UI

- **`SonosButton`** (in `PlayerBar.tsx`, between the Lyrics button and `VolumeSlider`):
  - Renders only when `sonosGroupsAtom` is non-empty.
  - Uses the lucide `Speaker` icon with the tooltip "Sonos speakers". It's faint by default and accent-coloured while casting. No Sonos logo (trademark).
- **`SonosPicker.tsx`** is a popover via `MenuPortal`, anchored above the button.
  - **Rows:** a "Play on" header, a "This computer" row with a monitor icon, and one row per group. A group is labelled with the coordinator's room name, or "Room + Room" for multi-speaker groups.
  - **Row state:** the active row has a check. Each group row has an optional now-playing subtitle, fetched once when the picker opens. Unreachable groups are greyed.
  - **Switching:** clicking a row starts the handoff, with a spinner on that row and the other rows disabled.
  - **Volume:** under the list, when a Sonos group is active, there's a group volume slider. Groups with more than one member also get a per-speaker slider for each member.
  - **Closing:** Escape or an outside click closes the picker.
- **"Playing on …" indicator:** a slim accent line under the transport controls in `PlayerBar` reads "Playing on <group>" (or "Reconnecting to <group>…"). Clicking it opens the picker. The same label appears in the miniplayer (via `useMiniplayerEmitter`) and the maximized player.
- **While casting:**
  - `QualityBadge` shows "Sonos" instead of the local stream info.
  - Exclusive mode, bit-perfect and output-device settings stay visible with the note "Applies to This computer only".
- **Settings, new "Sonos" row:**
  - Discovery status: "N speakers found" or "None found".
  - A **Rescan** button.
  - A **manual speaker IP** field.

## 6. Handoff and reconnect

### Local → group G

1. Snapshot the current track, interpolated position, `isPlaying`, history, manual queue, queue and repeat mode.
2. Pause local playback (`pause_track`). On any failure below, resume local playback and show a toast.
3. Make sure `sn` is known; if it isn't, abort (see section 7).
4. On G, load for a fast start:
   - `RemoveAllTracksFromQueue`, then add the current track plus the next ~15, then `SetAVTransportURI` to the queue.
   - `Seek TRACK_NR`, `Seek REL_TIME` to the position, `SetPlayMode`, and `Play` if local was playing. Target: under 2 s to sound.
   - In the background, append the rest of the upcoming queue, then insert history in front of the current track. The poller absorbs the index shift.
5. Set `playbackTargetAtom`, persist G's coordinator id, stop the local pipeline (`stop_track`) and start the poller.

Two special cases:
- If nothing is loaded locally, SONE just selects G and mirrors it, without touching G's queue.
- If G is already playing something else, its queue is replaced **without asking**, as Spotify Connect does.

### Group G → This computer

1. Use the mirrored queue plus the current index and position.
2. `Pause` G. Its queue is left intact.
3. Set the target to local, rebuild `historyAtom`, `currentTrackAtom` and `queueAtom` from the mirror, and play the current track locally at that position. If G was paused, load the track paused at that position instead.
4. Stop the poller and clear `sonos_last_coordinator`.

### Group G → group H

The same as G → local followed by local → H, without playing locally in between.

### Reconnect on startup

- Discovery starts at launch. If `sonos_last_coordinator` is found, SONE attaches in **mirror mode**. It issues no transport commands and skips restoring the persisted local queue.
- If speakers appear late, SONE still attaches, unless local playback has started in the meantime.
- If the remembered coordinator now belongs to a larger group (it was regrouped), SONE attaches to that group.
- If it isn't found, SONE stays on This computer.

### Non-TIDAL content on the Sonos

- This covers radio, other apps and the Beam's HDMI/TV input (`x-sonos-htastream`). The item is shown from its DIDL metadata, or as "TV" for the TV input.
- Transport and volume keep working. Actions that need TIDAL ids are disabled until a TIDAL queue is loaded: shuffle rewrite, "play next" ordering and autoplay.
- If another controller replaces the queue mid-cast, SONE follows it.

## 7. Error handling

| Situation | Behaviour |
|---|---|
| No speakers discovered | Button hidden. Settings shows "None found", with Rescan and a manual-IP fallback, which covers SSDP replies being dropped on a "Public" network profile. |
| `sn` unknown | The picker says: "Play any TIDAL track once from the Sonos app, then retry". The handoff aborts and local playback resumes. Mirroring and remote control still work. |
| Track unavailable on Sonos | An unexpected `STOPPED` mid-queue triggers `Next` and the toast "Track unavailable on Sonos — skipping". The existing `MAX_CONSECUTIVE_PLAY_FAILS` cap applies. |
| A command fails or times out | Toast: "Couldn't reach <group>". The optimistic UI is corrected on the next poll. |
| 3 consecutive failed polls (~3 s) | The group is marked unreachable, and the indicator shows "Reconnecting to <group>…". |
| Unreachable for 30 s | The target falls back to This computer **without starting local playback**. `sonos_last_coordinator` is kept, so the next launch reattaches. |
| Volume slider drag | Throttled to one request per 100 ms, with the final value sent on release. |
| Concurrent actions | One handoff at a time (the manager lock). Transport actions during a handoff are ignored. |

## 8. Testing

These are the repo's first Rust unit tests (`cargo test`, no new dependencies), using sanitized fixtures captured by the probe:
- Topology XML parsing into groups, coordinators, members and room names.
- TIDAL URI and DIDL building, and the queue URI → track id round trip, including non-TIDAL items.
- SOAP fault parsing and the UPnP error mapping.

No JS test runner is added. `src/lib/sonosQueue.ts` stays pure, so one can be added later.

Manual checklist on the user's speakers:
1. Both groups appear in the picker; the button is hidden when the speakers are unplugged or unreachable.
2. Cast an album mid-track to the Beam, and the position is preserved within about 1 s.
3. Play/pause, next, previous and seek work from the player bar, media keys, tray and miniplayer.
4. Group volume and per-speaker volume work on the One SL group.
5. Queue edits and track changes made in the Sonos app appear in SONE within about 2 s.
6. Close SONE and playback continues. Relaunch and SONE reattaches in mirror mode without interrupting playback.
7. Switching to This computer resumes locally at the right position; Beam → One SL group hands off correctly.
8. The Beam's TV input shows "TV", and volume still works.
9. Unplugging the speaker leads to "Reconnecting" and then the fallback, with no local audio starting.
10. Shuffle and repeat toggles while casting behave as described in section 4.
11. `npm run check` passes, including clippy, so the Linux build still compiles.

## 9. Order of work

1. Probe (section 2), the gate for everything else.
2. Backend `sonos` module with unit tests, plus commands and polling.
3. Frontend atoms, `useSonosSync` and the mirroring.
4. `sonosActions` and the guards in `usePlaybackActions`.
5. UI: button, picker, indicator and the Settings row.
6. Handoff and reconnect.
7. Error-handling polish and the manual checklist.
