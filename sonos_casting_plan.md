# Sonos Casting and Remote Control Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let SONE cast TIDAL playback to existing Sonos groups and remote-control them (transport, queue, group/per-speaker volume) from its normal player UI, with handoff both ways and reattach on restart.

**Architecture:** A cross-platform Rust module `src-tauri/src/sonos/` speaks the Sonos local UPnP API: SSDP discovery, SOAP calls and a 1 s poller that emits `sonos-*` events. In the frontend, a `playbackTargetAtom` seam in `usePlaybackActions` routes actions to `sonosActions.ts` while casting. `useSonosSync` mirrors the Sonos queue, which is the source of truth, into the existing playback atoms, so the existing UI renders it unchanged.

**Tech Stack:** Rust (Tauri 2, tokio, reqwest 0.11, new deps `quick-xml` and `if-addrs`), React 19, Jotai, Tailwind 4, lucide-react.

**Spec:** `sonos_casting_design.md` (repo root). Read it alongside this plan.

**Deviations from the spec (decided while planning):**
- No `notify_track_started_remote` command. `AppInitializer` already pushes SMTC and Discord metadata from `currentTrackAtom`/`isPlayingAtom` subscriptions, so mirroring the atoms updates SMTC and Discord for free. Not calling `notify_track_started` while casting is what skips scrobbling.
- No `sonos_queue_move`. Drag-reorder in the queue drawer uses `sonos_queue_replace_after(currentIndex, newOrder)`.
- Sonos atoms live in a new `src/atoms/sonos.ts`; only `playbackTargetAtom` goes in `atoms/playback.ts`.
- `sonos_select_group` takes `forget_last: bool`, so the unreachable fallback (keeps the remembered group) and the user choosing This computer (forgets it) can be told apart.

## Global Constraints

- All new Rust code is cross-platform. No `cfg(target_os)` blocks are needed, and the Linux build must keep compiling. `npm run check` (eslint, prettier, clippy `-D warnings`, cargo fmt, knip) must pass at the end of every task that touches its scope.
- LAN HTTP uses its own `reqwest::Client` with **no proxy** (`.no_proxy()`) and a **3 s** timeout. It never uses `build_http_client`.
- Polling cadence: state **~1 s**, topology **30 s**, rediscovery **60 s** while nothing has been found.
- `AddMultipleURIsToQueue` in batches of **16**. Sonos queue cap **1000** items. History sent ≤ **50**. Fast start: current track + the next **15**.
- Reachability: **3** consecutive failed polls → Reconnecting; **30 s** unreachable → fall back to This computer **without starting local playback**, keeping `sonos_last_coordinator`.
- Volume requests are throttled to one per **100 ms**, with the final value always sent.
- Sonos `SHUFFLE*` play modes are never set by SONE. While casting, scrobbling is driven only by `useSonosSync` (Task 8) from mirrored state, never by `playTrack` (spec §4, superseded note: SONE scrobbles casts).
- UI copy, verbatim: tooltip `Sonos speakers`; picker header `Play on`; row `This computer`; `Playing on <group>`; `Reconnecting to <group>…`; `Couldn't reach <group>`; `Track unavailable on Sonos — skipping`; `Play any TIDAL track once from the Sonos app, then retry`; `Applies to This computer only`; Settings `N speakers found` / `None found`, button `Rescan`; non-TIDAL TV item title `TV`.
- Group display name: the coordinator's room name, or member room names joined with ` + ` (coordinator first) when a group has more than one visible member.
- Every new Tauri command is registered in `generate_handler!` in `src-tauri/src/lib.rs`. Commands use `#[tauri::command(rename_all = "camelCase")]`, and payload structs use `#[serde(rename_all = "camelCase")]`.
- Commits go on `feature/sonos-casting`, and every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Run git and cargo through `rtk` (user's CLAUDE.md).

## Review Focus

1. **Non-Sonos SSDP responders.** Routers and TVs often answer every M-SEARCH. They must be ignored rather than listed or queried. Test: `ssdp_ignores_non_zoneplayer_responses` (Task 2).
2. **Names and titles with XML-special or non-ASCII characters** (`Kök & Bad`, `Kids' Room`, a track titled `<3`) must round-trip intact, and SOAP args must be escaped. Tests: `envelope_escapes_args` (Task 1), `topology_unescapes_room_names` (Task 2).
3. **Stereo pairs, bonded Sub or surrounds** (members with `Invisible="1"`, satellites) must not show up as separate speakers or volume sliders. Test: `topology_skips_invisible_members` (Task 2).
4. **Odd time strings** (`NOT_IMPLEMENTED`, `""`, `0:00:00` for radio/TV, `10:02:03` past 10 h) parse to `None` or the right seconds and never panic. Test: `parse_hms_edge_cases` (Task 1).
5. **Another controller replacing the queue while SONE is still back-filling it.** SONE must stop filling rather than splice tracks into someone else's queue. Test: `fill_stops_on_foreign_update` (Task 5).

---

### Task 0: Probe (throwaway, gate for everything else)

Needs the user at the speakers. The orchestrator runs this task in the main session, not in a subagent.

**Files:**
- Create (never committed): `src-tauri/examples/sonos_probe.rs`
- Create (committed): `src-tauri/src/sonos/fixtures/zone_group_state.xml`, `browse_queue.xml`, `position_info_tv.xml` (if the Beam's TV input can be captured), `soap_fault.xml`
- Modify: `sonos_casting_design.md` (append a "Probe findings" section)

**Interfaces:**
- Produces: the constants Task 3 hard-codes. `TIDAL_SID` (expected 174), `TIDAL_TRACK_FLAGS`, `TIDAL_TRACK_EXT` (`flac` or `mp4`), `TIDAL_ITEM_ID_PREFIX`, `TIDAL_DESC_TOKEN` (expected `SA_RINCON44551_X_#Svc44551-0-Token`), the observed `upnp:class`, and the user's `sn`. Also the fixtures Tasks 1–3 parse.

- [ ] **Step 1:** Ask the user to play any TIDAL track on the Beam from the Sonos app and to add one TIDAL track to Sonos Favourites. Wait for confirmation.
- [ ] **Step 2:** Write `sonos_probe.rs` as a standalone tokio binary (reqwest + std UDP only, no crate imports) with these subcommands:
  - `discover`: SSDP M-SEARCH (Task 2 message), print each `LOCATION`.
  - `topology <ip>`: print raw `GetZoneGroupState` response.
  - `browse <ip> <Q:0|FV:2|R:0>`: print raw `Browse` `Result`.
  - `position <ip>`: print raw `GetPositionInfo`.
  - `play <ip> <rincon> <uri> <didl-file>`: `RemoveAllTracksFromQueue`, `AddURIToQueue`, `SetAVTransportURI` to `x-rincon-queue:<rincon>#0`, `Seek TRACK_NR 1`, `Play`.
- [ ] **Step 3:** Run `rtk cargo run --example sonos_probe -- discover` from `src-tauri`. Expected: at least one `http://<ip>:1400/xml/device_description.xml` per speaker.
- [ ] **Step 4:** Run `topology`, then `browse` for `Q:0`, `FV:2` and `R:0`. From the TIDAL items, record `sn`, flags, extension, item-id prefix, `desc` token and `upnp:class`.
- [ ] **Step 5:** Build the URI and DIDL for a **different** TIDAL track id taken from SONE, then run `play` against the Beam. **Success:** the user hears that track. If no flag/format combination plays after trying SoCo's known TIDAL variants, **stop and report to the user**. The fallback is the remote-control-only re-scope (spec §2).
- [ ] **Step 6:** Save the sanitized fixtures:
  - IPs → `192.0.2.x`.
  - RINCON ids → `RINCON_00000000000101400` style.
  - Household id → `Sonos_HHID`.
  - `sn` → `3`.
  - Hand-edit `zone_group_state.xml` to add one member with `Invisible="1"` and one room named `Kök &amp; Bad`.
- [ ] **Step 7:** Append "Probe findings" (the constants, with the real `sn` replaced by "user's sn discovered at runtime") to `sonos_casting_design.md`. Delete `sonos_probe.rs`, then commit:

```bash
rtk git add src-tauri/src/sonos/fixtures sonos_casting_design.md
rtk git commit -m "docs(sonos): record probe findings and test fixtures"
```

---

### Task 1: SOAP foundation and error variant

**Files:**
- Create: `src-tauri/src/sonos/mod.rs`, `src-tauri/src/sonos/soap.rs`
- Modify: `src-tauri/Cargo.toml` (add `quick-xml = "0.36"`, `if-addrs = "0.13"`), `src-tauri/src/error.rs` (add variant), `src-tauri/src/lib.rs` (`mod sonos;`)

**Interfaces:**
- Produces:
  - `SoneError::Sonos(String)` with `#[error("Sonos error: {0}")]`.
  - `pub const SONOS_PORT: u16 = 1400;`
  - `pub enum Service { AVTransport, RenderingControl, GroupRenderingControl, ZoneGroupTopology, ContentDirectory }` with `fn control_path(&self) -> &'static str` and `fn urn(&self) -> &'static str`.
  - `pub fn build_envelope(service: Service, action: &str, args: &[(&str, &str)]) -> String`
  - `pub fn parse_response(body: &str, action: &str) -> Result<HashMap<String, String>, SoneError>` (text children of `<u:{action}Response>`, XML-unescaped)
  - `pub fn parse_fault(body: &str) -> Option<u32>`
  - `pub fn parse_hms(s: &str) -> Option<f64>`
  - `pub fn format_hms(secs: f64) -> String` (`H:MM:SS`)
  - `pub struct SoapClient`, with `SoapClient::new() -> Self` and `async fn call(&self, ip: &str, service: Service, action: &str, args: &[(&str, &str)]) -> Result<HashMap<String, String>, SoneError>`

- [ ] **Step 1: Write failing tests** in `soap.rs` `#[cfg(test)] mod tests`:

```rust
#[test] fn envelope_escapes_args() {
    let e = build_envelope(Service::AVTransport, "AddURIToQueue", &[("EnqueuedURIMetaData", "<DIDL-Lite>Kök & <3</DIDL-Lite>")]);
    assert!(e.contains("&lt;DIDL-Lite&gt;Kök &amp; &lt;3&lt;/DIDL-Lite&gt;"));
    assert!(e.contains(r#"<u:AddURIToQueue xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">"#));
}
#[test] fn parse_response_unescapes_values() { /* body with <CurrentURI>x-sonos-http:track%2f1.flac?sid=174&amp;sn=3</CurrentURI> → map["CurrentURI"] ends "&sn=3" */ }
#[test] fn parse_fault_reads_upnp_error_code() { assert_eq!(parse_fault(include_str!("fixtures/soap_fault.xml")), Some(/* code from fixture */)); }
#[test] fn parse_hms_edge_cases() {
    assert_eq!(parse_hms("0:03:25"), Some(205.0));
    assert_eq!(parse_hms("10:02:03"), Some(36123.0));
    assert_eq!(parse_hms("NOT_IMPLEMENTED"), None);
    assert_eq!(parse_hms(""), None);
    assert_eq!(parse_hms("0:00:00"), Some(0.0));
    assert_eq!(format_hms(3725.4), "1:02:05");
}
```

- [ ] **Step 2:** Run `rtk cargo test sonos::soap` in `src-tauri`. Expected: compile errors or FAIL (not implemented).
- [ ] **Step 3: Implement** the interfaces above.
  - Control paths: `/MediaRenderer/AVTransport/Control`, `/MediaRenderer/RenderingControl/Control`, `/MediaRenderer/GroupRenderingControl/Control`, `/ZoneGroupTopology/Control`, `/MediaServer/ContentDirectory/Control`. URNs are `urn:schemas-upnp-org:service:<Name>:1`.
  - `call` POSTs to `http://{ip}:1400{path}` with `Content-Type: text/xml; charset="utf-8"` and `SOAPACTION: "{urn}#{action}"`.
  - A 500 with a fault becomes `SoneError::Sonos(format!("{action} failed (UPnP {code})"))`. Transport errors go through the existing `From<reqwest::Error>` (`Network`).
  - Use `quick_xml::escape::escape` and `quick_xml::Reader`.
- [ ] **Step 4:** Run `rtk cargo test sonos::soap`. Expected: 4 passed.
- [ ] **Step 5:** Run `rtk cargo clippy -- -D warnings` in `src-tauri`. Expected: no warnings. Dead-code warnings for not-yet-used items are fine to silence with `#[allow(dead_code)]` on `mod sonos` until Task 5 wires it up; remove that attribute in Task 5.
- [ ] **Step 6: Commit:** `feat(sonos): SOAP client and Sonos error variant`

---

### Task 2: Discovery and topology

**Files:**
- Create: `src-tauri/src/sonos/discovery.rs`, `src-tauri/src/sonos/topology.rs`

**Interfaces:**
- Consumes: `SoapClient`, `Service::ZoneGroupTopology` (Task 1).
- Produces (`topology.rs`, all `#[derive(Debug, Clone, Serialize, PartialEq)] #[serde(rename_all = "camelCase")]`):
  - `pub struct SonosMember { pub id: String, pub room_name: String, pub ip: String }`
  - `pub struct SonosGroup { pub id: String, pub coordinator_id: String, pub name: String, pub members: Vec<SonosMember>, pub reachable: bool }`, where `members` lists visible members only, coordinator first.
  - `pub fn parse_zone_group_state(xml: &str) -> Result<Vec<SonosGroup>, SoneError>` (accepts the whole SOAP response value, with or without a `<ZoneGroups>` wrapper; groups sorted by `name`)
  - `pub fn find_group_for_member<'a>(groups: &'a [SonosGroup], member_id: &str) -> Option<&'a SonosGroup>`
- Produces (`discovery.rs`):
  - `pub fn candidate_interfaces(addrs: &[Ipv4Addr]) -> Vec<Ipv4Addr>`
  - `pub fn parse_ssdp_location(resp: &str) -> Option<String>` (returns the IP)
  - `pub async fn discover(timeout: Duration) -> Vec<String>` (unique IPs)
  - `pub async fn fetch_topology(soap: &SoapClient, seed_ips: &[String]) -> Result<Vec<SonosGroup>, SoneError>` (tries seeds in order, first success wins)

- [ ] **Step 1: Write failing tests:**

```rust
// topology.rs
#[test] fn topology_parses_groups_and_names() { /* fixture: groups.len() == 2; Beam group name == coordinator room; One SL group name == "A + B" with coordinator first; member ips from Location */ }
#[test] fn topology_skips_invisible_members() { /* the Invisible="1" member is absent from members */ }
#[test] fn topology_unescapes_room_names() { /* some member.room_name == "Kök & Bad" */ }
#[test] fn find_group_for_member_finds_regrouped_coordinator() { /* id of a non-coordinator member → Some(its group) ; unknown id → None */ }
// discovery.rs
#[test] fn ssdp_parses_zoneplayer_location() { /* "HTTP/1.1 200 OK\r\nlocation: http://192.0.2.10:1400/xml/device_description.xml\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n" → Some("192.0.2.10") (header names case-insensitive) */ }
#[test] fn ssdp_ignores_non_zoneplayer_responses() { /* same with "ST: upnp:rootdevice" → None */ }
#[test] fn interfaces_skip_loopback_and_link_local() { /* [127.0.0.1, 169.254.3.4, 192.168.1.5] → [192.168.1.5] */ }
```

- [ ] **Step 2:** Run `rtk cargo test sonos::`. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Discovery binds one tokio `UdpSocket` per candidate interface (`if_addrs::get_if_addrs()`) on `iface:0` and sends to `239.255.255.250:1900`:
    `M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: "ssdp:discover"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n`
  - Send it twice, then collect responses until `timeout`.
  - Member IP comes from the `Location` attribute.
- [ ] **Step 4:** Run `rtk cargo test sonos::`. Expected: all pass.
- [ ] **Step 5: Commit:** `feat(sonos): SSDP discovery and zone topology parsing`

---

### Task 3: TIDAL URIs and queue items

**Files:**
- Create: `src-tauri/src/sonos/tidal.rs`

**Interfaces:**
- Consumes: the Task 0 constants, which are hard-coded here as `pub const`.
- Produces:
  - `#[derive(Debug, Clone, Copy, Serialize, PartialEq)] #[serde(rename_all = "lowercase")] pub enum QueueItemKind { Tidal, Tv, Other }`
  - `#[derive(Debug, Clone, Serialize, PartialEq)] #[serde(rename_all = "camelCase")] pub struct QueueItem { pub kind: QueueItemKind, pub tidal_track_id: Option<u64>, pub title: String, pub artist: String, pub album_art_url: Option<String>, pub duration_secs: Option<f64> }`
  - `pub fn build_track_uri(track_id: u64, sn: u32) -> String`
  - `pub fn build_track_didl(track_id: u64) -> String` (minimal DIDL-Lite item: id `{TIDAL_ITEM_ID_PREFIX}track%2f{id}`, class and desc from the probe; empty title is fine because Sonos fetches metadata from the service)
  - `pub fn tidal_track_id_from_uri(uri: &str) -> Option<u64>` (requires `sid=174`)
  - `pub fn classify_uri(uri: &str) -> QueueItemKind` (`x-sonos-htastream:` → `Tv`)
  - `pub fn parse_didl_items(didl: &str, speaker_ip: &str) -> Vec<QueueItem>` (reads `res`, `dc:title`, `dc:creator`, `upnp:albumArtURI`; a relative art URI `/getaa?…` becomes `http://{ip}:1400/getaa?…`; `res@duration` goes through `parse_hms`; a `Tv` item gets title `TV`)
  - `pub fn find_tidal_sn<'a>(uris: impl IntoIterator<Item = &'a str>) -> Option<u32>`

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn track_uri_round_trips() { let u = build_track_uri(123456789, 3); assert_eq!(tidal_track_id_from_uri(&u), Some(123456789)); assert!(u.contains("sid=174") && u.contains("sn=3")); }
#[test] fn non_tidal_uri_has_no_track_id() { assert_eq!(tidal_track_id_from_uri("x-sonos-http:track%2f5.mp3?sid=9&sn=1"), None); }
#[test] fn classify_tv_input() { assert_eq!(classify_uri("x-sonos-htastream:RINCON_00000000000101400:spdif"), QueueItemKind::Tv); }
#[test] fn parse_queue_fixture() { /* browse_queue.xml → first item kind Tidal with Some(id); art url absolute http://192.0.2.x:1400/getaa…; titles unescaped */ }
#[test] fn find_sn_from_mixed_uris() { assert_eq!(find_tidal_sn(["x-rincon-mp3radio:foo", "x-sonos-http:track%2f1.flac?sid=174&flags=8232&sn=7"]), Some(7)); assert_eq!(find_tidal_sn(["x-rincon-mp3radio:foo"]), None); }
#[test] fn didl_is_well_formed() { /* quick_xml reader over build_track_didl(1) reaches Eof without error and contains TIDAL_DESC_TOKEN */ }
```

- [ ] **Step 2:** Run `rtk cargo test sonos::tidal`. Expected: FAIL.
- [ ] **Step 3:** Implement the interfaces above.
- [ ] **Step 4:** Run `rtk cargo test sonos::tidal`. Expected: 6 passed.
- [ ] **Step 5: Commit:** `feat(sonos): TIDAL-on-Sonos URI building and queue item parsing`

---

### Task 4: Typed service calls

**Files:**
- Create: `src-tauri/src/sonos/services.rs`

**Interfaces:**
- Consumes: `SoapClient`, `Service`, `parse_hms`, `format_hms` (Task 1); `parse_didl_items`, `QueueItem` (Task 3).
- Produces:
  - `#[derive(Debug, Clone, Copy, Serialize, PartialEq)] #[serde(rename_all = "lowercase")] pub enum TransportState { Playing, Paused, Stopped, Transitioning }` (`PAUSED_PLAYBACK` maps to `Paused`)
  - `pub struct PositionInfo { pub track_number: u32 /* 1-based, 0 = none */, pub position_secs: f64, pub duration_secs: Option<f64>, pub track_uri: String, pub track_meta: String }`
  - `pub struct BrowseResult { pub didl: String, pub number_returned: u32, pub total_matches: u32, pub update_id: u32 }`
  - `pub fn repeat_to_play_mode(repeat: u8) -> &'static str` (0 `NORMAL`, 1 `REPEAT_ALL`, 2 `REPEAT_ONE`)
  - `pub fn play_mode_to_repeat(mode: &str) -> u8` (shuffle variants map to their repeat part: `SHUFFLE` → 1, `SHUFFLE_REPEAT_ONE` → 2, `SHUFFLE_NOREPEAT` → 0)
  - `pub fn parse_transport_state(map: &HashMap<String, String>) -> TransportState`
  - `pub fn parse_position_info(map: &HashMap<String, String>) -> PositionInfo`
  - `pub fn parse_browse(map: &HashMap<String, String>) -> BrowseResult`
  - Free `async fn`s taking `(soap: &SoapClient, ip: &str, …)`:
    - `play`, `pause`, `next`, `previous`
    - `seek_time(secs: f64)`, `seek_track(number_1based: u32)`
    - `set_queue_as_source(coordinator_id: &str)`
    - `add_uris(items: &[(String, String)], at_1based: u32 /* 0 = end */) -> Result<u32 /* NewUpdateID */, _>`
    - `remove_all`, `remove_range(start_1based: u32, count: u32, update_id: u32) -> Result<u32, _>`
    - `set_play_mode(mode: &str)`
    - `transport_state`, `position_info`, `get_play_mode() -> Result<String, _>`
    - `browse(object_id: &str, start: u32, count: u32) -> Result<BrowseResult, _>`
    - `get_group_volume`/`set_group_volume(u8)`, `get_group_mute`/`set_group_mute(bool)`, `get_volume`/`set_volume(u8)` (per member IP)
    - `zone_group_state() -> Result<String, _>`

- [ ] **Step 1: Write failing tests:** `position_info_parses_tidal_and_tv` (fixtures from Task 0; for TV `duration_secs == None`), `transport_state_maps_paused_playback`, `play_mode_round_trip` (`0..=2` through both fns; `"SHUFFLE_REPEAT_ONE"` → 2), `browse_reads_update_id`.
- [ ] **Step 2:** Run `rtk cargo test sonos::services`. Expected: FAIL.
- [ ] **Step 3: Implement.** All AVTransport/RenderingControl calls pass `InstanceID=0`. Specific args:
  - `Play` takes `Speed=1`. Volume calls use `Channel=Master`.
  - `Seek` uses `Unit=REL_TIME` + `Target=format_hms(secs)`, or `Unit=TRACK_NR` + `Target=n`.
  - `SetAVTransportURI`: `CurrentURI=x-rincon-queue:{coordinator_id}#0`, `CurrentURIMetaData=""`.
  - `AddMultipleURIsToQueue`: `UpdateID=0`, `NumberOfURIs`, `EnqueuedURIs` and `EnqueuedURIsMetaData` (both space-joined, as SoCo does), `ContainerURI=""`, `ContainerMetaData=""`, `DesiredFirstTrackNumberEnqueued=at`, `EnqueueAsNext=0`.
  - `RemoveTrackRangeFromQueue`: `UpdateID`, `StartingIndex`, `NumberOfTracks`.
  - `Browse`: `BrowseFlag=BrowseDirectChildren`, `Filter=*`, `SortCriteria=""`.
- [ ] **Step 4:** Run `rtk cargo test sonos::services`. Expected: pass.
- [ ] **Step 5: Commit:** `feat(sonos): typed AVTransport, rendering and content directory calls`

---

### Task 5: SonosManager, commands, settings

**Files:**
- Create: `src-tauri/src/sonos/manager.rs`, `src-tauri/src/commands/sonos.rs`
- Modify:
  - `src-tauri/src/commands/mod.rs` (`pub mod sonos;`)
  - `src-tauri/src/lib.rs`: add `Settings` fields `#[serde(default)] sonos_last_coordinator: Option<String>`, `sonos_tidal_sn: Option<u32>` and `sonos_manual_ip: Option<String>`; add `AppState.sonos: sonos::SonosManager`; register the commands; remove the Task 1 `allow(dead_code)`.
  - `src-tauri/src/commands/playback.rs`: `get_playback_position` returns `state.sonos.casting_position()` when it is `Some`.

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces (`manager.rs`):
  - `pub struct LoadPlan { pub prepend: Vec<u64>, pub initial: Vec<u64>, pub initial_start: usize, pub append: Vec<u64> }`
  - `pub fn plan_load(track_ids: &[u64], start_index: usize) -> LoadPlan`: `initial = ids[start .. start+16]`, `append` = the rest after that, `prepend = ids[..start]`, and the total is truncated to 1000 by trimming `append`.
  - `pub fn fill_should_continue(own_update_id: Option<u32>, observed_update_id: u32, generation: u64, current_generation: u64) -> bool`: false if the generations differ; true if `own_update_id` is `None`; otherwise true only when observed equals own.
  - `#[derive(Serialize)] #[serde(rename_all = "camelCase")] pub struct SonosSnapshot { pub groups: Vec<SonosGroup>, pub selected: Option<String>, pub attachable: Option<String> /* coordinator of the group containing sonos_last_coordinator */, pub sn_known: bool, pub manual_ip: Option<String> }`
  - `pub const SN_UNKNOWN: &str = "TIDAL_SN_UNKNOWN";` (the message of the `SoneError::Sonos` returned by `load_queue` when no `sn` can be found)
  - `impl SonosManager`:
    - `new(settings: &Settings) -> Self`
    - `casting_position(&self) -> Option<f32>` (sync, a `std::sync::Mutex` read)
    - `async fn snapshot`, `refresh_topology`, `select`
    - `async fn load_queue(track_ids, start_index, position_secs, play)`: `plan_load` → remove_all → add `initial` → set_queue_as_source → seek_track(initial_start+1) → seek_time → set_play_mode → play if `play`; then spawn the back-fill (append `append` in batches of 16, then insert `prepend` at position 1), checking `fill_should_continue` before each batch.
    - `transport` helpers, `queue_insert`, `queue_remove`, `queue_replace_after`, `queue_clear`, volume helpers.
    - `sn` discovery (browse `FV:2`, `R:0`, `Q:0` → `find_tidal_sn`, persisted) runs lazily inside `load_queue` when `sn` is unknown.
  - Any new load or `select` bumps the fill generation. Each own queue mutation records the returned `NewUpdateID` as `own_update_id`.
- Produces (commands, all `Result<_, SoneError>`):
  - `sonos_get_snapshot() -> SonosSnapshot`, `sonos_rescan() -> Vec<SonosGroup>`, `sonos_set_manual_ip(ip: Option<String>) -> Vec<SonosGroup>`
  - `sonos_select_group(coordinator_id: Option<String>, forget_last: bool)`, `sonos_group_now_playing(coordinator_id: String) -> Option<QueueItem>`
  - `sonos_load_queue(track_ids: Vec<u64>, start_index: usize, position_secs: f64, play: bool)`
  - `sonos_play`, `sonos_pause`, `sonos_next`, `sonos_previous`, `sonos_seek(position_secs: f64)`, `sonos_play_index(index: usize /* 0-based */)`
  - `sonos_queue_insert(track_ids: Vec<u64>, at: usize /* 0-based */)`, `sonos_queue_remove(index: usize)`, `sonos_queue_replace_after(index: usize, track_ids: Vec<u64>)`, `sonos_queue_clear`
  - `sonos_set_group_volume(volume: u8)`, `sonos_set_member_volume(member_id: String, volume: u8)`, `sonos_set_mute(muted: bool)`, `sonos_set_play_mode(repeat: u8)`

  All transport, queue and volume commands act on the selected coordinator, and return `SoneError::Sonos("No Sonos group selected")` when there is none.

- [ ] **Step 1: Write failing tests** in `manager.rs`:

```rust
#[test] fn plan_load_fast_start() { let ids: Vec<u64> = (1..=40).collect(); let p = plan_load(&ids, 10);
    assert_eq!(p.prepend, (1..=10).collect::<Vec<_>>()); assert_eq!(p.initial, (11..=26).collect::<Vec<_>>());
    assert_eq!(p.initial_start, 0); assert_eq!(p.append, (27..=40).collect::<Vec<_>>()); }
#[test] fn plan_load_caps_at_1000() { let ids: Vec<u64> = (1..=1500).collect(); let p = plan_load(&ids, 0);
    assert_eq!(p.prepend.len() + p.initial.len() + p.append.len(), 1000); }
#[test] fn plan_load_short_queue() { let p = plan_load(&[7], 0); assert_eq!(p.initial, vec![7]); assert!(p.append.is_empty() && p.prepend.is_empty()); }
#[test] fn fill_stops_on_foreign_update() {
    assert!(fill_should_continue(Some(12), 12, 3, 3));
    assert!(!fill_should_continue(Some(12), 13, 3, 3));   // someone else edited the queue
    assert!(!fill_should_continue(Some(12), 12, 3, 4));   // superseded by a newer load
    assert!(fill_should_continue(None, 99, 3, 3));
}
```

- [ ] **Step 2:** Run `rtk cargo test sonos::manager`. Expected: FAIL.
- [ ] **Step 3:** Implement `manager.rs`, `commands/sonos.rs`, the Settings fields, the `AppState` field and the command registration.
- [ ] **Step 4:** Run `rtk cargo test sonos::`. Expected: all pass. Then `npm run lint:rust`, expected: clean.
- [ ] **Step 5: Smoke test** with the user. Run `npm run tauri dev`, then in devtools:
  - `await window.__TAURI_INTERNALS__.invoke("sonos_rescan")` lists both groups.
  - `invoke("sonos_select_group", {coordinatorId: "<beam>", forgetLast: false})`, then `invoke("sonos_load_queue", {trackIds: [<a few ids>], startIndex: 0, positionSecs: 30, play: true})` plays from 0:30 on the Beam.
  - `invoke("sonos_set_group_volume", {volume: 20})` changes the volume.
- [ ] **Step 6: Commit:** `feat(sonos): manager, Tauri commands and persisted Sonos settings`

---

### Task 6: Poller, events, startup discovery

**Files:**
- Modify: `src-tauri/src/sonos/manager.rs`, `src-tauri/src/lib.rs` (spawn `state.sonos.start(app_handle)` in `.setup`, next to the audio-device pre-warm)

**Interfaces:**
- Consumes: Task 5.
- Produces:
  - `#[derive(Debug, Clone, Copy, Serialize, PartialEq)] #[serde(rename_all = "lowercase")] pub enum Reachability { Ok, Reconnecting, Lost }`
  - `pub struct ReachTracker`, with `fn record(&mut self, ok: bool, now: Instant) -> Reachability`
  - `#[derive(Serialize, Clone, PartialEq)] #[serde(rename_all = "camelCase")] pub struct SonosState { pub coordinator_id: String, pub transport_state: TransportState, pub track_index: Option<usize> /* 0-based */, pub position_secs: f64, pub duration_secs: Option<f64>, pub group_volume: u8, pub muted: bool, pub member_volumes: Vec<MemberVolume>, pub repeat: u8, pub current_item: Option<QueueItem>, pub reachability: Reachability }`
  - `pub struct MemberVolume { pub member_id: String, pub volume: u8 }`
  - `#[derive(Serialize, Clone)] #[serde(rename_all = "camelCase")] pub struct SonosQueue { pub coordinator_id: String, pub update_id: u32, pub items: Vec<QueueItem> }`
  - `pub fn detect_unexpected_stop(prev: &SonosState, next: &SonosState, queue_len: usize, user_stopped: bool) -> bool`
  - Events:
    - `sonos-topology` (`Vec<SonosGroup>`)
    - `sonos-state` (`SonosState`): emitted when any field other than `position_secs` changes, and otherwise at least every 5 s so the frontend re-anchors.
    - `sonos-queue` (`SonosQueue`)
    - `sonos-playback-error` (`{ "kind": "unavailable" }`)
  - `SonosManager::start(&self, app: AppHandle)`: discovery (seeded with `sonos_manual_ip`) → emit topology; topology loop every 30 s; rediscovery every 60 s while there are no groups.
  - The poller is started and stopped by `select`. Each tick it reads transport state, position, group volume/mute, member volumes, play mode and `browse("Q:0", 0, 1).update_id`. When the update id changes it re-browses `Q:0` in pages of 100 and emits `sonos-queue`. An unexpected update id also bumps the fill generation.

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn reach_tracker_transitions() { let t0 = Instant::now(); let mut r = ReachTracker::default();
    assert_eq!(r.record(false, t0), Reachability::Ok); assert_eq!(r.record(false, t0 + 1s), Reachability::Ok);
    assert_eq!(r.record(false, t0 + 2s), Reachability::Reconnecting); assert_eq!(r.record(false, t0 + 30s), Reachability::Lost);
    assert_eq!(r.record(true, t0 + 31s), Reachability::Ok); }
#[test] fn unexpected_stop_detected_mid_queue() { /* prev Playing idx 2, next Stopped idx 2, queue_len 10, user_stopped false → true */ }
#[test] fn stop_at_queue_end_is_not_unexpected() { /* idx 9 of 10 → false; user_stopped true → false */ }
```

- [ ] **Step 2:** Run `rtk cargo test sonos::manager`. Expected: FAIL.
- [ ] **Step 3:** Implement. `user_stopped` is set by SONE's own `pause`, `queue_clear` and `load_queue`, and cleared on the next Playing state.
- [ ] **Step 4:** Run `rtk cargo test sonos::`, then `npm run lint:rust`. Expected: pass and clean.
- [ ] **Step 5: Smoke test.** With the dev app running, `listen("sonos-state", console.log)` in devtools logs about once a second after `sonos_select_group`. Skipping a track in the Sonos app logs a new `trackIndex` within about 2 s.
- [ ] **Step 6: Commit:** `feat(sonos): state poller, reachability tracking and startup discovery`

---

### Task 7: Frontend API, atoms, queue helpers, mirroring

**Files:**
- Create: `src/api/sonos.ts`, `src/atoms/sonos.ts`, `src/lib/sonosQueue.ts`, `src/hooks/useSonosSync.ts`
- Modify: `src/atoms/playback.ts` (add `playbackTargetAtom`), `src/components/AppInitializer.tsx` (call `useSonosSync()`)

**Interfaces:**
- Consumes: the Task 5–6 commands and events.
- Produces:
  - `api/sonos.ts`: TS types mirroring the Rust structs (`SonosGroup`, `SonosMember`, `QueueItem`, `SonosState`, `SonosQueue`, `SonosSnapshot`, `Reachability`), one typed wrapper per command (`sonosLoadQueue(trackIds, startIndex, positionSecs, play)`, …), and `export const SONOS_SN_UNKNOWN = "TIDAL_SN_UNKNOWN"`.
  - `atoms/playback.ts`: `export type PlaybackTarget = { type: "local" } | { type: "sonos"; coordinatorId: string; name: string }` and `playbackTargetAtom = atom<PlaybackTarget>({ type: "local" })`.
  - `atoms/sonos.ts`:
    - `sonosGroupsAtom: SonosGroup[]`, `sonosStateAtom: SonosState | null`, `sonosQueueAtom: SonosQueue | null`
    - `sonosGroupVolumeAtom: number` (0–1)
    - `sonosHandoffAtom: { to: string } | null` (`to` is a coordinator id or `"local"`)
    - `sonosPendingPlayNextAtom: number`
  - `lib/sonosQueue.ts` (pure):
    - `flattenForSonos(history: Track[], current: Track, manual: Track[], queue: Track[]): { trackIds: number[]; startIndex: number; overflow: Track[] }` (history trimmed to its last 50; total capped at 1000; `overflow` is the upcoming tail that didn't fit)
    - `splitMirror(tracks: Track[], index: number): { history: Track[]; current: Track | null; queue: Track[] }`
    - `queueIndexToSonos(currentIndex: number, queueIndex: number): number` (= `currentIndex + 1 + queueIndex`)
    - `playNextInsertIndex(currentIndex: number, pending: number): number`
    - `placeholderTrack(item: QueueItem, index: number): Track` (`id: -(index + 1)`, `album.cover = item.albumArtUrl`, which `getTidalImageUrl` already passes through)
  - `useSonosSync()`:
    - Listens to `sonos-topology` → `sonosGroupsAtom`.
    - While the target is `sonos`, `sonos-queue` resolves items to `Track[]` (TIDAL ids via `getTrack`, max 6 concurrent, a module-level `Map` cache, misses and failures → `placeholderTrack`). Then `splitMirror` → `historyAtom`/`currentTrackAtom`/`queueAtom`, and `manualQueueAtom = []`.
    - `sonos-state` → `sonosStateAtom`, `isPlayingAtom = transportState === "playing"`, `sonosGroupVolumeAtom = groupVolume / 100`, re-slice on `trackIndex` change, and `repeatAtom` when `repeat` differs from the last value sent.
    - Exposes nothing.

- [ ] **Step 1:** Write `sonosQueue.ts` and a scratch self-check `src/lib/sonosQueue.check.ts`, which is **not committed** because the repo has no JS runner. It runs under `npx tsx` and asserts:
  - `flattenForSonos` with 60 history items keeps the last 50 and `startIndex === 50`.
  - Manual items come before queue items.
  - No history, a current track and 1,200 upcoming items → `trackIds.length === 1000` and `overflow.length === 201`.
  - `splitMirror(t, 0).history` is empty, and `splitMirror(t, t.length - 1).queue` is empty.
  - `placeholderTrack({kind: "tv", …}, 0).title === "TV"` and its id is negative.
- [ ] **Step 2:** Run `npx tsx src/lib/sonosQueue.check.ts`. Expected: exits 0 with no assertion errors. Then delete the file.
- [ ] **Step 3:** Implement `api/sonos.ts`, the atoms and `useSonosSync`, and mount it in `AppInitializer`.
- [ ] **Step 4:** Run `npm run build && npm run lint && npm run knip`. Expected: clean. Knip may flag exports that are only used from Task 8 on; mark them used in Task 8 rather than deleting them.
- [ ] **Step 5: Smoke test.** In `useSonosSync`, add a dev-only hook, `if (import.meta.env.DEV) window.__sonosDebugSetTarget = (coordinatorId, name) => store.set(playbackTargetAtom, { type: "sonos", coordinatorId, name })`, which Task 10 removes. In the dev app, run `invoke("sonos_select_group", {coordinatorId: "<beam>", forgetLast: false})`, then `__sonosDebugSetTarget("<beam>", "Beam")`. Expected: the queue drawer and player bar show the Beam's queue and current track, and skipping in the Sonos app updates them within about 2 s.
- [ ] **Step 6: Commit:** `feat(sonos): frontend API, atoms and Sonos queue mirroring`

---

### Task 8: Sonos actions and guards in `usePlaybackActions`

**Files:**
- Create: `src/hooks/sonosActions.ts`
- Modify: `src/hooks/usePlaybackActions.ts`, `src/hooks/useSonosSync.ts` (autoplay top-up, overflow top-up, unavailable-skip handling)

**Interfaces:**
- Consumes: Task 7.
- Produces: `createSonosActions(store: Store, showToast: ShowToast)`, built once with `useMemo` in `usePlaybackActions`. It returns:
  - `loadFromAtoms(play: boolean, positionSecs?: number): Promise<void>` (`flattenForSonos` → `sonosLoadQueue`; stashes `overflow` in a module-level ref for top-up)
  - `pause()`, `resume()`, `next()`, `previous()`, `seek(secs)`, `playIndex(queueIndex: number)`
  - `insertNext(tracks: Track[])`, `insertAfterPending(tracks: Track[])`, `appendEnd(tracks: Track[])`, `removeAt(queueIndex: number)`, `replaceUpcoming(tracks: Track[])`
  - `setVolume(level: number)` (throttled to 100 ms, trailing call), `setMuted(muted: boolean)`
  - Every method no-ops while `sonosHandoffAtom` is non-null. Command failures toast `Couldn't reach <group>`.

Guards in `usePlaybackActions`, all checking `store.get(playbackTargetAtom).type === "sonos"` and leaving the local code untouched:

| Action | While casting |
|---|---|
| `playTrack` | Run the existing history push and set `currentTrackAtom`, then `loadFromAtoms(true)` instead of `play_tidal_track`. Do not call `notify_track_started`. |
| `pauseTrack` / `resumeTrack` / `seekTo` / `setVolume` | `pause` / `resume` / `seek` / `setVolume` |
| `playNext` | `next()` |
| `playPrevious` | The same >3 s restart rule as local: `seek(0)` past 3 s, else `previous()` |
| `playFromQueue(index)` | `playIndex(index)` |
| `addToQueue` | `insertAfterPending` |
| `playNextInQueue` | `insertNext` |
| `appendToQueue` | `appendEnd` |
| `removeFromQueue` | `removeAt` |
| `setQueueTracks({ reorder: true })` | Existing atom update, then `replaceUpcoming(newQueue)` |
| `toggleShuffle` | Existing atom update, then `replaceUpcoming(store.get(queueAtom))` |
| `clearQueue` | Existing atom update, then `replaceUpcoming([])` |

`setQueueTracks` (non-reorder), `setShuffledQueue`, `playFromSource` and `playAllFromSource` need no guard: they shape atoms and then call `playTrack`. In casting mode, `insertNext` and `insertAfterPending` also update `queueAtom` optimistically and maintain `sonosPendingPlayNextAtom`. The pending count resets when a `sonos-queue` arrives whose `updateId` SONE didn't cause.

`useSonosSync` additions:
- **Top-up:** when `trackIndex >= items.length - 2`, if there's stashed overflow, `appendEnd(next ≤ 200)`; else if `autoplayAtom` is set, append recommendations using the existing autoplay fetch from the local path.
- **Unavailable track:** `sonos-playback-error` → `next()` + toast `Track unavailable on Sonos — skipping`, counted against `MAX_CONSECUTIVE_PLAY_FAILS` through `consecutiveFailCountAtom`. Export that constant from `usePlaybackActions.ts` (or wherever it is defined) to share it.
- **Non-TIDAL content:** when the current item's kind isn't `tidal`, `insertNext`, `insertAfterPending`, `replaceUpcoming` and autoplay are skipped.
- **Scrobbling casts:**
  - When the pair (Sonos `trackIndex`, current TIDAL track id) changes, so that a repeat of the same track at a new index counts as a new play, call `notify_track_started` with the same payload shape `playTrack` builds locally, with `chosenByUser: false`. Placeholder (non-TIDAL) tracks are never sent.
  - On `transportState` `playing` → `paused`, call `notify_track_paused`; on `paused` → `playing`, call `notify_track_resumed`. `sonosActions.seek` calls `notify_track_seeked`.
  - Nothing is sent while `sonosHandoffAtom` is set, so the handoff itself doesn't count as a play.

- [ ] **Step 1:** Implement `sonosActions.ts` and the guards.
- [ ] **Step 2:** Run `npm run build && npm run lint && npm run knip`. Expected: clean.
- [ ] **Step 3: Manual check on the Beam** (target set as in Task 7). Each must reach the speaker:
  - Play an album from its page.
  - Pause/resume, next, previous (both sides of 3 s) and seek.
  - "Play next" and "Add to queue" order, plus remove and drag-reorder in the queue drawer.
  - Shuffle on/off and repeat 0/1/2 (the Sonos app shows the mode).
  - Media keys and tray.
  - A cast track played past the scrobble threshold appears once on Last.fm, and the next cast track shows as "now playing".
- [ ] **Step 4: Commit:** `feat(sonos): route playback actions to Sonos while casting`

---

### Task 9: Handoff, reconnect, fallback

**Files:**
- Create: `src/hooks/useSonosHandoff.ts`
- Modify: `src/atoms/playback.ts` (`pendingResumePositionAtom = atom<number | null>(null)`), `src/hooks/usePlaybackActions.ts` (the `resumeTrack` "finished" branch seeks to `pendingResumePositionAtom` after starting, then clears it), `src/components/AppInitializer.tsx` (mount the attach effect)

**Interfaces:**
- Consumes: Tasks 7–8; `usePlaybackActions().playTrack/seekTo/pauseTrack`; `getInterpolatedPosition`.
- Produces: `useSonosHandoff(): { switchTarget(to: "local" | string): Promise<void>; handoffInProgress: boolean }` and `useSonosAutoAttach()` (mounted once in `AppInitializer`).
  - **local → G:** follow spec §6 steps 1–5. Set `sonosHandoffAtom`, snapshot, `pauseTrack()`, `sonosSelectGroup(G, false)`, set the target, `loadFromAtoms(wasPlaying, position)`, then `stop_track`.
    - On error, restore the local target, call `sonosSelectGroup(null, false)` and resume local if it was playing.
    - Toast `Play any TIDAL track once from the Sonos app, then retry` when the error message equals `SONOS_SN_UNKNOWN`; otherwise `Couldn't reach <group>`.
    - Nothing loaded locally → select + set target only (mirror).
  - **G → local:** `notify_track_stopped`, `sonosPause()`, `sonosSelectGroup(null, true)`, set the target to local, keep the mirrored atoms. The fallback below also calls `notify_track_stopped`.
    - If G was playing: `playTrack(current, { skipHistoryPush: true })` then `seekTo(pos)`.
    - Otherwise set `pendingResumePositionAtom = pos` and leave the track paused.
  - **G → H:** pause G, then the local → H path without local play/pause.
  - **Fallback:** a `sonos-state` with `reachability === "lost"` → `sonosSelectGroup(null, false)`, set the target to local with `isPlayingAtom = false` and **no** local play, toast `Couldn't reach <group>`.
  - **`useSonosAutoAttach`:** on startup and on each `sonos-topology`, if `snapshot.attachable` is set, the target is local, `isPlayingAtom` is false and no attach has been attempted this session → `sonosSelectGroup(attachable, false)`, set the target, clear `manualQueueAtom`/`originalQueueAtom`, `shuffleAtom = false`. No transport command.

- [ ] **Step 1:** Implement `useSonosHandoff.ts`, `useSonosAutoAttach` and the `pendingResumePositionAtom` handling.
- [ ] **Step 2:** Run `npm run build && npm run lint && npm run knip`. Expected: clean.
- [ ] **Step 3: Manual check.** Add the dev-only hook `if (import.meta.env.DEV) window.__sonosDebugSwitch = switchTarget` (Task 10 removes it) and drive it from devtools:
  - Local mid-track → Beam keeps position within about 1 s; Beam → local resumes at position.
  - Beam → One SL group hands off.
  - Close SONE: music continues. Relaunch: attaches without a transport change.
  - Unplug the Beam: about 30 s later the target is This computer and there's no local audio.
- [ ] **Step 4: Commit:** `feat(sonos): handoff between targets, auto-attach and fallback`

---

### Task 10: UI (button, picker, indicator, settings)

**Files:**
- Create: `src/components/SonosButton.tsx`, `src/components/SonosPicker.tsx`, `src/components/PlayingOnIndicator.tsx`
- Modify:
  - `src/components/PlayerBar.tsx` (button between Lyrics and `VolumeSlider` in `DrawerButtons`; indicator under the transport controls)
  - `src/components/VolumeSlider.tsx` (casting: value from `sonosGroupVolumeAtom`, change → `setVolume`, mute → `setMuted`; `volumeAtom` untouched)
  - `src/components/QualityBadge.tsx` (casting → label `Sonos`, no detail)
  - `src/hooks/useMiniplayerEmitter.ts` + `src/components/MiniPlayer.tsx` (payload field `playingOn: string | null`; render the indicator text)
  - `src/components/MaximizedPlayer.tsx` (indicator)
  - `src/components/SettingsModal.tsx` (a "Sonos" section before Utilities: status `N speakers found`/`None found`, `Rescan` → `sonosRescan`, manual IP input → `sonosSetManualIp` on blur/Enter; plus the note `Applies to This computer only` under the exclusive mode, bit-perfect and output-device controls while casting)

**Interfaces:**
- Consumes: `useSonosHandoff().switchTarget`, the Task 7 atoms, `sonosGroupNowPlaying`, `sonosSetMemberVolume`.
- Produces: `SonosButton` renders `null` when `sonosGroupsAtom` is empty, uses lucide `Speaker` size 16, carries `title="Sonos speakers"`, and has the `text-th-accent` class while casting (the same pattern as `DrawerButtons`).
  - **`SonosPicker`** (via `MenuPortal`, anchored above the button):
    - A `Play on` header, a `This computer` row (lucide `Monitor`) and one row per group with a check on the active row.
    - A now-playing subtitle `<room> · <artist> – <title>`, fetched once on open.
    - Greyed rows for groups with `reachable === false`.
    - A spinner on the row whose id matches `sonosHandoffAtom.to`, with the other rows disabled.
    - When a Sonos group is active: a group volume slider, plus per-member sliders only when `members.length > 1`. Member sliders are throttled at 100 ms.
    - Escape and outside click close it.
  - **`PlayingOnIndicator`:** renders only while casting, as `Playing on <group>` (or `Reconnecting to <group>…` when `reachability === "reconnecting"`). Clicking it opens the picker.

- [ ] **Step 1:** Implement the components and modifications. Remove the `__sonosDebugSetTarget` and `__sonosDebugSwitch` dev hooks (Tasks 7 and 9).
- [ ] **Step 2:** Run `npm run build && npm run lint && npm run format && npm run knip`. Expected: clean. `rtk grep "__sonosDebug" src` should find nothing.
- [ ] **Step 3: Manual check:**
  - The button is hidden with the speakers' network cable out or Wi-Fi off on the PC, and appears within 60 s of them coming back.
  - The picker lists both groups and This computer; switching works from the picker.
  - Group and per-speaker sliders work on the One SL group; the Beam shows no per-speaker sliders.
  - The miniplayer and maximized player show `Playing on Beam`. The Settings Sonos section shows the count, Rescan works, and a manual IP finds the speakers.
- [ ] **Step 4: Commit:** `feat(sonos): Sonos button, target picker, playing-on indicator and settings`

---

### Task 11: Final verification

**Files:**
- Modify: `sonos_casting_design.md` only if behaviour deliberately differs (record it).

- [ ] **Step 1:** Run `npm run check`. Expected: eslint, prettier, clippy, fmt and knip all clean.
- [ ] **Step 2:** Run `rtk cargo test` in `src-tauri`. Expected: every `sonos::` test passes (about 25).
- [ ] **Step 3:** With the user, walk the spec §8 manual checklist items 1–11 on the real speakers and record pass/fail for each in the session. Any fail goes back to its owning task.
- [ ] **Step 4:** Confirm `src-tauri/examples/sonos_probe.rs` and `src/lib/sonosQueue.check.ts` don't exist and aren't tracked (`rtk git status`).
- [ ] **Step 5: Commit** any fixes: `fix(sonos): address verification findings`
