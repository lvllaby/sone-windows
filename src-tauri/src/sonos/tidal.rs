//! TIDAL-on-Sonos: building the URIs/DIDL Sonos accepts for TIDAL tracks and
//! parsing queue items back into a frontend-friendly shape.

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::Serialize;

use crate::sonos::soap::{parse_hms, SONOS_PORT};

/// TIDAL's service id on Sonos (probed, firmware 94.1).
pub const TIDAL_SID: u32 = 174;
pub const TIDAL_TRACK_FLAGS: u32 = 24616;
pub const TIDAL_TRACK_EXT: &str = "flac";
pub const TIDAL_ITEM_ID_PREFIX: &str = "10032020";
pub const TIDAL_DESC_TOKEN: &str = "SA_RINCON44551_X_#Svc44551-0-Token";

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum QueueItemKind {
    Tidal,
    Tv,
    Other,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub kind: QueueItemKind,
    pub tidal_track_id: Option<u64>,
    pub title: String,
    pub artist: String,
    pub album_art_url: Option<String>,
    pub duration_secs: Option<f64>,
}

/// `x-sonos-http:track%2f<id>.flac?sid=174&flags=24616&sn=<sn>` (note the
/// lowercase `%2f`, which is what the firmware accepts).
pub fn build_track_uri(track_id: u64, sn: u32) -> String {
    format!(
        "x-sonos-http:track%2f{track_id}.{TIDAL_TRACK_EXT}?sid={TIDAL_SID}&flags={TIDAL_TRACK_FLAGS}&sn={sn}"
    )
}

/// Minimal DIDL-Lite for a TIDAL track. The firmware ignores client-supplied
/// metadata and fetches it from the service, so the title is left empty.
/// Raw XML: the SOAP layer escapes it when used as an argument.
pub fn build_track_didl(track_id: u64) -> String {
    format!(
        concat!(
            r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
            r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" "#,
            r#"xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/" "#,
            r#"xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/">"#,
            r#"<item id="{prefix}track%2f{id}" parentID="-1" restricted="true">"#,
            r#"<dc:title></dc:title>"#,
            r#"<upnp:class>object.item.audioItem.musicTrack</upnp:class>"#,
            r#"<desc id="cdudn" nameSpace="urn:schemas-rinconnetworks-com:metadata-1-0/">{token}</desc>"#,
            r#"</item></DIDL-Lite>"#
        ),
        prefix = TIDAL_ITEM_ID_PREFIX,
        id = track_id,
        token = TIDAL_DESC_TOKEN,
    )
}

/// Value of query parameter `key` in `uri`.
fn query_param<'a>(uri: &'a str, key: &str) -> Option<&'a str> {
    let query = uri.split_once('?')?.1;
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// Track id from a TIDAL URI; `None` unless it carries `sid=174`.
pub fn tidal_track_id_from_uri(uri: &str) -> Option<u64> {
    if query_param(uri, "sid")?.parse::<u32>().ok()? != TIDAL_SID {
        return None;
    }
    let rest = uri.strip_prefix("x-sonos-http:track")?;
    let rest = rest
        .strip_prefix("%2f")
        .or_else(|| rest.strip_prefix("%2F"))?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

pub fn classify_uri(uri: &str) -> QueueItemKind {
    if uri.starts_with("x-sonos-htastream:") {
        QueueItemKind::Tv
    } else if tidal_track_id_from_uri(uri).is_some() {
        QueueItemKind::Tidal
    } else {
        QueueItemKind::Other
    }
}

#[derive(Clone, Copy)]
enum Field {
    Res,
    Title,
    Creator,
    Art,
}

/// Raw fields of one `<item>` while walking the DIDL.
#[derive(Default)]
struct PendingItem {
    uri: String,
    title: String,
    artist: String,
    art: String,
    duration: Option<f64>,
}

impl PendingItem {
    fn finish(self, speaker_ip: &str) -> QueueItem {
        let kind = classify_uri(&self.uri);
        let title = if kind == QueueItemKind::Tv {
            "TV".to_string()
        } else {
            self.title
        };
        let album_art_url = if self.art.is_empty() {
            None
        } else if self.art.starts_with("http://") || self.art.starts_with("https://") {
            Some(self.art)
        } else if self.art.starts_with('/') {
            Some(format!("http://{speaker_ip}:{SONOS_PORT}{}", self.art))
        } else {
            Some(format!("http://{speaker_ip}:{SONOS_PORT}/{}", self.art))
        };
        QueueItem {
            kind,
            tidal_track_id: tidal_track_id_from_uri(&self.uri),
            title,
            artist: self.artist,
            album_art_url,
            duration_secs: self.duration,
        }
    }
}

/// Parses a DIDL-Lite document (already unescaped from the SOAP envelope)
/// into queue items. Malformed XML yields whatever parsed before the error.
pub fn parse_didl_items(didl: &str, speaker_ip: &str) -> Vec<QueueItem> {
    let mut reader = Reader::from_str(didl);
    reader.config_mut().trim_text(true);
    let mut items = Vec::new();
    let mut pending: Option<PendingItem> = None;
    let mut field: Option<Field> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"item" => pending = Some(PendingItem::default()),
                b"res" => {
                    field = Some(Field::Res);
                    if let Some(item) = pending.as_mut() {
                        item.duration = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.local_name().as_ref() == b"duration")
                            .and_then(|a| a.unescape_value().ok())
                            .and_then(|v| parse_hms(&v));
                    }
                }
                b"title" => field = Some(Field::Title),
                b"creator" => field = Some(Field::Creator),
                b"albumArtURI" => field = Some(Field::Art),
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if let (Some(item), Some(f)) = (pending.as_mut(), field) {
                    let Ok(text) = t.unescape() else { continue };
                    match f {
                        Field::Res => item.uri.push_str(&text),
                        Field::Title => item.title.push_str(&text),
                        Field::Creator => item.artist.push_str(&text),
                        Field::Art => item.art.push_str(&text),
                    }
                }
            }
            Ok(Event::End(e)) => {
                field = None;
                if e.local_name().as_ref() == b"item" {
                    if let Some(item) = pending.take() {
                        items.push(item.finish(speaker_ip));
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    items
}

/// Text of every `<res>` element in a DIDL-Lite document (XML-unescaped once),
/// in document order. Used to read the raw URIs `QueueItem` doesn't carry.
pub fn didl_res_uris(didl: &str) -> Vec<String> {
    let mut reader = Reader::from_str(didl);
    reader.config_mut().trim_text(true);
    let mut uris = Vec::new();
    let mut current: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"res" => {
                current = Some(String::new());
            }
            Ok(Event::Text(t)) => {
                if let (Some(uri), Ok(text)) = (current.as_mut(), t.unescape()) {
                    uri.push_str(&text);
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"res" => {
                if let Some(uri) = current.take().filter(|u| !u.is_empty()) {
                    uris.push(uri);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    uris
}

/// Account serial (`sn=`) of the first TIDAL URI in `uris`.
pub fn find_tidal_sn<'a>(uris: impl IntoIterator<Item = &'a str>) -> Option<u32> {
    uris.into_iter()
        .filter(|u| tidal_track_id_from_uri(u).is_some())
        .find_map(|u| query_param(u, "sn")?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonos::soap::parse_response;

    fn fixture_items() -> Vec<QueueItem> {
        let body = include_str!("fixtures/browse_queue.xml");
        let didl = parse_response(body, "Browse").unwrap()["Result"].clone();
        parse_didl_items(&didl, "192.0.2.34")
    }

    #[test]
    fn track_uri_round_trips() {
        let u = build_track_uri(123456789, 3);
        assert_eq!(tidal_track_id_from_uri(&u), Some(123456789));
        assert!(u.contains("sid=174") && u.contains("sn=3"));
        assert_eq!(
            u,
            "x-sonos-http:track%2f123456789.flac?sid=174&flags=24616&sn=3"
        );
    }

    #[test]
    fn non_tidal_uri_has_no_track_id() {
        assert_eq!(
            tidal_track_id_from_uri("x-sonos-http:track%2f5.mp3?sid=9&sn=1"),
            None
        );
    }

    #[test]
    fn classify_tv_input() {
        assert_eq!(
            classify_uri("x-sonos-htastream:RINCON_00000000000101400:spdif"),
            QueueItemKind::Tv
        );
        assert_eq!(
            classify_uri("x-sonos-http:track%2f1.flac?sid=174&sn=1"),
            QueueItemKind::Tidal
        );
        assert_eq!(classify_uri("x-rincon-mp3radio:foo"), QueueItemKind::Other);
    }

    #[test]
    fn parse_queue_fixture() {
        let items = fixture_items();
        assert_eq!(items.len(), 4);
        let first = &items[0];
        assert_eq!(first.kind, QueueItemKind::Tidal);
        assert_eq!(first.tidal_track_id, Some(57770228));
        assert_eq!(first.title, "Affinity.exe");
        assert_eq!(first.artist, "Haken");
        assert_eq!(first.duration_secs, Some(84.0));
        let art = first.album_art_url.as_deref().unwrap();
        assert!(art.starts_with("http://192.0.2.34:1400/getaa?s=1&u="));
        assert!(!art.contains("&amp;"));
    }

    #[test]
    fn parse_queue_item_without_metadata() {
        let items = fixture_items();
        let item = &items[3];
        assert_eq!(item.kind, QueueItemKind::Tidal);
        assert_eq!(item.tidal_track_id, Some(240071017));
        assert_eq!(item.title, "");
        assert_eq!(item.artist, "");
        assert_eq!(item.duration_secs, None);
        assert!(item.album_art_url.is_some());
    }

    #[test]
    fn absolute_art_url_and_tv_title() {
        let didl = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="Q:0/1"><res duration="NOT_IMPLEMENTED">x-sonos-htastream:RINCON_1:spdif</res><upnp:albumArtURI>https://example.com/a.jpg</upnp:albumArtURI></item></DIDL-Lite>"#;
        let items = parse_didl_items(didl, "192.0.2.34");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, QueueItemKind::Tv);
        assert_eq!(items[0].title, "TV");
        assert_eq!(items[0].duration_secs, None);
        assert_eq!(
            items[0].album_art_url.as_deref(),
            Some("https://example.com/a.jpg")
        );
    }

    #[test]
    fn res_uris_from_queue_fixture() {
        let body = include_str!("fixtures/browse_queue.xml");
        let didl = parse_response(body, "Browse").unwrap()["Result"].clone();
        let uris = didl_res_uris(&didl);
        assert_eq!(uris.len(), 4);
        assert!(uris.iter().all(|u| u.starts_with("x-sonos-http:track%2f")));
        assert_eq!(find_tidal_sn(uris.iter().map(String::as_str)), Some(3));
    }

    #[test]
    fn find_sn_from_mixed_uris() {
        assert_eq!(
            find_tidal_sn([
                "x-rincon-mp3radio:foo",
                "x-sonos-http:track%2f1.flac?sid=174&flags=8232&sn=7"
            ]),
            Some(7)
        );
        assert_eq!(find_tidal_sn(["x-rincon-mp3radio:foo"]), None);
    }

    #[test]
    fn didl_is_well_formed() {
        let didl = build_track_didl(1);
        let mut reader = Reader::from_str(&didl);
        loop {
            if reader.read_event().expect("well-formed XML") == Event::Eof {
                break;
            }
        }
        assert!(didl.contains(TIDAL_DESC_TOKEN));
        assert!(didl.contains("10032020track%2f1"));
    }
}
