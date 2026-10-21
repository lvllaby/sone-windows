//! Typed wrappers over the Sonos UPnP actions SONE uses (AVTransport,
//! RenderingControl, GroupRenderingControl, ContentDirectory, topology).
//! Pure parse/mapping helpers are unit-tested; the async calls are thin.

use std::collections::HashMap;

use serde::Serialize;

use crate::error::SoneError;
use crate::sonos::soap::{format_hms, parse_hms, Service, SoapClient};

#[allow(dead_code)] // used by the Task 6 poller
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TransportState {
    Playing,
    Paused,
    Stopped,
    Transitioning,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PositionInfo {
    /// 1-based queue position; 0 = none.
    pub track_number: u32,
    pub position_secs: f64,
    /// `None` when the source has no duration (e.g. TV input, radio).
    pub duration_secs: Option<f64>,
    pub track_uri: String,
    /// Raw DIDL of the current track. Empty when the speaker reports
    /// `NOT_IMPLEMENTED` (TV input), so callers can treat empty as "no metadata".
    pub track_meta: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BrowseResult {
    pub didl: String,
    pub number_returned: u32,
    pub total_matches: u32,
    pub update_id: u32,
}

/// Frontend repeat value (0 off, 1 all, 2 one) to Sonos `PlayMode`. Shuffle is
/// not managed here; unknown values fall back to `NORMAL`.
pub fn repeat_to_play_mode(repeat: u8) -> &'static str {
    match repeat {
        1 => "REPEAT_ALL",
        2 => "REPEAT_ONE",
        _ => "NORMAL",
    }
}

/// Sonos `PlayMode` to repeat value; shuffle variants map to their repeat part.
/// Unknown modes map to 0.
pub fn play_mode_to_repeat(mode: &str) -> u8 {
    match mode {
        "REPEAT_ALL" | "SHUFFLE" => 1,
        "REPEAT_ONE" | "SHUFFLE_REPEAT_ONE" => 2,
        _ => 0,
    }
}

#[allow(dead_code)] // used by the Task 6 poller
/// Unknown or missing states map to `Stopped`.
pub fn parse_transport_state(map: &HashMap<String, String>) -> TransportState {
    match map.get("CurrentTransportState").map(String::as_str) {
        Some("PLAYING") => TransportState::Playing,
        Some("PAUSED_PLAYBACK") => TransportState::Paused,
        Some("TRANSITIONING") => TransportState::Transitioning,
        _ => TransportState::Stopped,
    }
}

fn get_u32(map: &HashMap<String, String>, key: &str) -> u32 {
    map.get(key)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

pub fn parse_position_info(map: &HashMap<String, String>) -> PositionInfo {
    let text = |k: &str| map.get(k).cloned().unwrap_or_default();
    let meta = text("TrackMetaData");
    PositionInfo {
        track_number: get_u32(map, "Track"),
        position_secs: parse_hms(&text("RelTime")).unwrap_or(0.0),
        duration_secs: parse_hms(&text("TrackDuration")),
        track_uri: text("TrackURI"),
        track_meta: if meta == "NOT_IMPLEMENTED" {
            String::new()
        } else {
            meta
        },
    }
}

pub fn parse_browse(map: &HashMap<String, String>) -> BrowseResult {
    BrowseResult {
        didl: map.get("Result").cloned().unwrap_or_default(),
        number_returned: get_u32(map, "NumberReturned"),
        total_matches: get_u32(map, "TotalMatches"),
        update_id: get_u32(map, "UpdateID"),
    }
}

fn clamp_volume(v: u8) -> u8 {
    v.min(100)
}

fn require<'a>(
    map: &'a HashMap<String, String>,
    action: &str,
    key: &str,
) -> Result<&'a String, SoneError> {
    map.get(key)
        .ok_or_else(|| SoneError::Sonos(format!("{action}: missing {key} in response")))
}

fn require_u32(map: &HashMap<String, String>, action: &str, key: &str) -> Result<u32, SoneError> {
    require(map, action, key)?
        .trim()
        .parse()
        .map_err(|_| SoneError::Sonos(format!("{action}: invalid {key} in response")))
}

async fn av(
    soap: &SoapClient,
    ip: &str,
    action: &str,
    args: &[(&str, &str)],
) -> Result<HashMap<String, String>, SoneError> {
    let mut all = vec![("InstanceID", "0")];
    all.extend_from_slice(args);
    soap.call(ip, Service::AVTransport, action, &all).await
}

pub async fn play(soap: &SoapClient, ip: &str) -> Result<(), SoneError> {
    av(soap, ip, "Play", &[("Speed", "1")]).await.map(drop)
}

pub async fn pause(soap: &SoapClient, ip: &str) -> Result<(), SoneError> {
    av(soap, ip, "Pause", &[]).await.map(drop)
}

pub async fn next(soap: &SoapClient, ip: &str) -> Result<(), SoneError> {
    av(soap, ip, "Next", &[]).await.map(drop)
}

pub async fn previous(soap: &SoapClient, ip: &str) -> Result<(), SoneError> {
    av(soap, ip, "Previous", &[]).await.map(drop)
}

pub async fn seek_time(soap: &SoapClient, ip: &str, secs: f64) -> Result<(), SoneError> {
    let target = format_hms(secs);
    av(
        soap,
        ip,
        "Seek",
        &[("Unit", "REL_TIME"), ("Target", &target)],
    )
    .await
    .map(drop)
}

pub async fn seek_track(soap: &SoapClient, ip: &str, number_1based: u32) -> Result<(), SoneError> {
    let target = number_1based.to_string();
    av(
        soap,
        ip,
        "Seek",
        &[("Unit", "TRACK_NR"), ("Target", &target)],
    )
    .await
    .map(drop)
}

/// Point the coordinator's transport at its own queue.
pub async fn set_queue_as_source(
    soap: &SoapClient,
    ip: &str,
    coordinator_id: &str,
) -> Result<(), SoneError> {
    let uri = format!("x-rincon-queue:{coordinator_id}#0");
    av(
        soap,
        ip,
        "SetAVTransportURI",
        &[("CurrentURI", &uri), ("CurrentURIMetaData", "")],
    )
    .await
    .map(drop)
}

/// Enqueue `(uri, didl)` pairs (1..=16 per call) via `AddMultipleURIsToQueue`.
/// `at_1based` 0 appends. Returns `NewUpdateID`.
pub async fn add_uris(
    soap: &SoapClient,
    ip: &str,
    items: &[(String, String)],
    at_1based: u32,
) -> Result<u32, SoneError> {
    let uris = items
        .iter()
        .map(|i| i.0.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let metas = items
        .iter()
        .map(|i| i.1.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let count = items.len().to_string();
    let at = at_1based.to_string();
    let r = av(
        soap,
        ip,
        "AddMultipleURIsToQueue",
        &[
            ("UpdateID", "0"),
            ("NumberOfURIs", &count),
            ("EnqueuedURIs", &uris),
            ("EnqueuedURIsMetaData", &metas),
            ("ContainerURI", ""),
            ("ContainerMetaData", ""),
            ("DesiredFirstTrackNumberEnqueued", &at),
            ("EnqueueAsNext", "0"),
        ],
    )
    .await?;
    require_u32(&r, "AddMultipleURIsToQueue", "NewUpdateID")
}

pub async fn remove_all(soap: &SoapClient, ip: &str) -> Result<(), SoneError> {
    av(soap, ip, "RemoveAllTracksFromQueue", &[])
        .await
        .map(drop)
}

/// Remove `count` tracks from 1-based `start`. `update_id` is the queue's
/// current update id. Returns `NewUpdateID`.
pub async fn remove_range(
    soap: &SoapClient,
    ip: &str,
    start_1based: u32,
    count: u32,
    update_id: u32,
) -> Result<u32, SoneError> {
    let (u, s, c) = (
        update_id.to_string(),
        start_1based.to_string(),
        count.to_string(),
    );
    let r = av(
        soap,
        ip,
        "RemoveTrackRangeFromQueue",
        &[
            ("UpdateID", &u),
            ("StartingIndex", &s),
            ("NumberOfTracks", &c),
        ],
    )
    .await?;
    require_u32(&r, "RemoveTrackRangeFromQueue", "NewUpdateID")
}

pub async fn set_play_mode(soap: &SoapClient, ip: &str, mode: &str) -> Result<(), SoneError> {
    av(soap, ip, "SetPlayMode", &[("NewPlayMode", mode)])
        .await
        .map(drop)
}

#[allow(dead_code)] // used by the Task 6 poller
pub async fn transport_state(soap: &SoapClient, ip: &str) -> Result<TransportState, SoneError> {
    Ok(parse_transport_state(
        &av(soap, ip, "GetTransportInfo", &[]).await?,
    ))
}

pub async fn position_info(soap: &SoapClient, ip: &str) -> Result<PositionInfo, SoneError> {
    Ok(parse_position_info(
        &av(soap, ip, "GetPositionInfo", &[]).await?,
    ))
}

pub async fn get_play_mode(soap: &SoapClient, ip: &str) -> Result<String, SoneError> {
    let r = av(soap, ip, "GetTransportSettings", &[]).await?;
    Ok(require(&r, "GetTransportSettings", "PlayMode")?.clone())
}

pub async fn browse(
    soap: &SoapClient,
    ip: &str,
    object_id: &str,
    start: u32,
    count: u32,
) -> Result<BrowseResult, SoneError> {
    let (s, c) = (start.to_string(), count.to_string());
    let r = soap
        .call(
            ip,
            Service::ContentDirectory,
            "Browse",
            &[
                ("ObjectID", object_id),
                ("BrowseFlag", "BrowseDirectChildren"),
                ("Filter", "*"),
                ("StartingIndex", &s),
                ("RequestedCount", &c),
                ("SortCriteria", ""),
            ],
        )
        .await?;
    Ok(parse_browse(&r))
}

#[allow(dead_code)] // used by the Task 6 poller
/// Group volume (0-100); call on the group coordinator.
pub async fn get_group_volume(soap: &SoapClient, ip: &str) -> Result<u8, SoneError> {
    let r = soap
        .call(
            ip,
            Service::GroupRenderingControl,
            "GetGroupVolume",
            &[("InstanceID", "0")],
        )
        .await?;
    Ok(require_u32(&r, "GetGroupVolume", "CurrentVolume")?.min(100) as u8)
}

pub async fn set_group_volume(soap: &SoapClient, ip: &str, vol: u8) -> Result<(), SoneError> {
    let v = clamp_volume(vol).to_string();
    soap.call(
        ip,
        Service::GroupRenderingControl,
        "SetGroupVolume",
        &[("InstanceID", "0"), ("DesiredVolume", &v)],
    )
    .await
    .map(drop)
}

#[allow(dead_code)] // used by the Task 6 poller
pub async fn get_group_mute(soap: &SoapClient, ip: &str) -> Result<bool, SoneError> {
    let r = soap
        .call(
            ip,
            Service::GroupRenderingControl,
            "GetGroupMute",
            &[("InstanceID", "0")],
        )
        .await?;
    Ok(require(&r, "GetGroupMute", "CurrentMute")?.trim() == "1")
}

pub async fn set_group_mute(soap: &SoapClient, ip: &str, mute: bool) -> Result<(), SoneError> {
    soap.call(
        ip,
        Service::GroupRenderingControl,
        "SetGroupMute",
        &[
            ("InstanceID", "0"),
            ("DesiredMute", if mute { "1" } else { "0" }),
        ],
    )
    .await
    .map(drop)
}

#[allow(dead_code)] // used by the Task 6 poller
/// Per-speaker volume (0-100).
pub async fn get_volume(soap: &SoapClient, ip: &str) -> Result<u8, SoneError> {
    let r = soap
        .call(
            ip,
            Service::RenderingControl,
            "GetVolume",
            &[("InstanceID", "0"), ("Channel", "Master")],
        )
        .await?;
    Ok(require_u32(&r, "GetVolume", "CurrentVolume")?.min(100) as u8)
}

pub async fn set_volume(soap: &SoapClient, ip: &str, vol: u8) -> Result<(), SoneError> {
    let v = clamp_volume(vol).to_string();
    soap.call(
        ip,
        Service::RenderingControl,
        "SetVolume",
        &[
            ("InstanceID", "0"),
            ("Channel", "Master"),
            ("DesiredVolume", &v),
        ],
    )
    .await
    .map(drop)
}

#[allow(dead_code)] // used by the Task 6 poller
/// Raw `ZoneGroupState` XML string (parsed by `topology`).
pub async fn zone_group_state(soap: &SoapClient, ip: &str) -> Result<String, SoneError> {
    let r = soap
        .call(ip, Service::ZoneGroupTopology, "GetZoneGroupState", &[])
        .await?;
    Ok(require(&r, "GetZoneGroupState", "ZoneGroupState")?.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonos::soap::parse_response;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn position_info_parses_tidal_and_tv() {
        let m = parse_response(
            include_str!("fixtures/position_info_tidal.xml"),
            "GetPositionInfo",
        )
        .unwrap();
        let p = parse_position_info(&m);
        assert_eq!(p.track_number, 5);
        assert_eq!(p.position_secs, 0.0);
        assert_eq!(p.duration_secs, Some(940.0));
        assert_eq!(
            p.track_uri,
            "x-sonos-http:track%2f57770232.flac?sid=174&flags=24616&sn=3"
        );
        assert!(p.track_meta.contains("DIDL-Lite"));

        let m = parse_response(
            include_str!("fixtures/position_info_tv.xml"),
            "GetPositionInfo",
        )
        .unwrap();
        let p = parse_position_info(&m);
        assert_eq!(p.track_number, 1);
        assert_eq!(p.duration_secs, None);
        assert_eq!(p.position_secs, 0.0);
        assert_eq!(p.track_meta, "");
        assert!(p.track_uri.starts_with("x-sonos-htastream:"));
    }

    #[test]
    fn transport_state_maps_paused_playback() {
        let st = |s: &str| parse_transport_state(&map(&[("CurrentTransportState", s)]));
        assert_eq!(st("PLAYING"), TransportState::Playing);
        assert_eq!(st("PAUSED_PLAYBACK"), TransportState::Paused);
        assert_eq!(st("STOPPED"), TransportState::Stopped);
        assert_eq!(st("TRANSITIONING"), TransportState::Transitioning);
        assert_eq!(st("WHATEVER"), TransportState::Stopped);
        assert_eq!(
            parse_transport_state(&HashMap::new()),
            TransportState::Stopped
        );
    }

    #[test]
    fn play_mode_round_trip() {
        for r in 0..=2u8 {
            assert_eq!(play_mode_to_repeat(repeat_to_play_mode(r)), r);
        }
        assert_eq!(repeat_to_play_mode(9), "NORMAL");
        assert_eq!(play_mode_to_repeat("SHUFFLE_REPEAT_ONE"), 2);
        assert_eq!(play_mode_to_repeat("SHUFFLE"), 1);
        assert_eq!(play_mode_to_repeat("SHUFFLE_NOREPEAT"), 0);
        assert_eq!(play_mode_to_repeat("garbage"), 0);
    }

    #[test]
    fn browse_reads_update_id() {
        let m = parse_response(include_str!("fixtures/browse_queue.xml"), "Browse").unwrap();
        let b = parse_browse(&m);
        assert_eq!(b.number_returned, 4);
        assert_eq!(b.total_matches, 10);
        assert_eq!(b.update_id, 47);
        assert!(b.didl.starts_with("<DIDL-Lite"));
    }

    #[test]
    fn clamp_volume_caps_at_100() {
        assert_eq!(clamp_volume(250), 100);
        assert_eq!(clamp_volume(42), 42);
    }
}
