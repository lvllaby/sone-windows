//! Minimal UPnP SOAP client for Sonos speakers (LAN only, port 1400).

use std::collections::HashMap;
use std::time::Duration;

use quick_xml::escape::escape;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::SoneError;

pub const SONOS_PORT: u16 = 1400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    AVTransport,
    RenderingControl,
    GroupRenderingControl,
    ZoneGroupTopology,
    ContentDirectory,
}

impl Service {
    pub fn control_path(&self) -> &'static str {
        match self {
            Service::AVTransport => "/MediaRenderer/AVTransport/Control",
            Service::RenderingControl => "/MediaRenderer/RenderingControl/Control",
            Service::GroupRenderingControl => "/MediaRenderer/GroupRenderingControl/Control",
            Service::ZoneGroupTopology => "/ZoneGroupTopology/Control",
            Service::ContentDirectory => "/MediaServer/ContentDirectory/Control",
        }
    }

    pub fn urn(&self) -> &'static str {
        match self {
            Service::AVTransport => "urn:schemas-upnp-org:service:AVTransport:1",
            Service::RenderingControl => "urn:schemas-upnp-org:service:RenderingControl:1",
            Service::GroupRenderingControl => {
                "urn:schemas-upnp-org:service:GroupRenderingControl:1"
            }
            Service::ZoneGroupTopology => "urn:schemas-upnp-org:service:ZoneGroupTopology:1",
            Service::ContentDirectory => "urn:schemas-upnp-org:service:ContentDirectory:1",
        }
    }
}

/// Build a SOAP request envelope; argument values are XML-escaped.
pub fn build_envelope(service: Service, action: &str, args: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, value) in args {
        body.push_str(&format!("<{name}>{}</{name}>", escape(value)));
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{urn}">{body}</u:{action}></s:Body></s:Envelope>"#,
        urn = service.urn()
    )
}

/// Parse the text children of `<u:{action}Response>` into a map. Values are
/// XML-unescaped exactly once (Sonos nests escaped DIDL / ZoneGroupState).
pub fn parse_response(body: &str, action: &str) -> Result<HashMap<String, String>, SoneError> {
    let wanted = format!("{action}Response");
    let mut reader = Reader::from_str(body);
    let mut map = HashMap::new();
    let mut in_response = false;
    let mut current: Option<String> = None;
    let mut found = false;

    loop {
        let ev = reader
            .read_event()
            .map_err(|e| SoneError::Sonos(format!("{action}: invalid response XML: {e}")))?;
        match ev {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if !in_response {
                    if name == wanted {
                        in_response = true;
                        found = true;
                    }
                } else if current.is_none() {
                    map.insert(name.clone(), String::new());
                    current = Some(name);
                }
            }
            Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if !in_response {
                    if name == wanted {
                        found = true;
                        break;
                    }
                } else if current.is_none() {
                    map.insert(name, String::new());
                }
            }
            Event::Text(t) => {
                if let Some(name) = &current {
                    let text = t
                        .unescape()
                        .map_err(|e| SoneError::Sonos(format!("{action}: bad text: {e}")))?;
                    map.entry(name.clone()).or_default().push_str(&text);
                }
            }
            Event::CData(c) => {
                if let Some(name) = &current {
                    map.entry(name.clone())
                        .or_default()
                        .push_str(&String::from_utf8_lossy(&c));
                }
            }
            Event::End(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if current.as_deref() == Some(name.as_str()) {
                    current = None;
                } else if in_response && name == wanted {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if found {
        Ok(map)
    } else {
        Err(SoneError::Sonos(format!(
            "{action}: response element not found"
        )))
    }
}

/// Extract the UPnP `<errorCode>` from a SOAP fault body.
pub fn parse_fault(body: &str) -> Option<u32> {
    let mut reader = Reader::from_str(body);
    let mut in_code = false;
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => in_code = e.local_name().as_ref() == b"errorCode",
            Event::Text(t) if in_code => return t.unescape().ok()?.trim().parse().ok(),
            Event::End(_) => in_code = false,
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// Parse `H:MM:SS` (optionally with fractional seconds) into seconds.
pub fn parse_hms(s: &str) -> Option<f64> {
    let mut parts = s.trim().split(':');
    let h: u64 = parts.next()?.parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !sec.is_finite() || sec < 0.0 {
        return None;
    }
    Some((h * 3600 + m * 60) as f64 + sec)
}

/// Format seconds as `H:MM:SS` (fractions truncated).
pub fn format_hms(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    format!(
        "{}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

/// SOAP client for Sonos. Bypasses the app proxy: this is LAN traffic.
pub struct SoapClient {
    http: reqwest::Client,
}

impl SoapClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("failed to build Sonos HTTP client");
        Self { http }
    }

    pub async fn call(
        &self,
        ip: &str,
        service: Service,
        action: &str,
        args: &[(&str, &str)],
    ) -> Result<HashMap<String, String>, SoneError> {
        let url = format!("http://{ip}:{SONOS_PORT}{}", service.control_path());
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", r#"text/xml; charset="utf-8""#)
            .header("SOAPACTION", format!("\"{}#{action}\"", service.urn()))
            .body(build_envelope(service, action, args))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if status.is_success() {
            parse_response(&text, action)
        } else if let Some(code) = parse_fault(&text) {
            Err(SoneError::Sonos(format!("{action} failed (UPnP {code})")))
        } else {
            Err(SoneError::Sonos(format!("{action} failed (HTTP {status})")))
        }
    }
}

impl Default for SoapClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_escapes_args() {
        let e = build_envelope(
            Service::AVTransport,
            "AddURIToQueue",
            &[("EnqueuedURIMetaData", "<DIDL-Lite>Kök & <3</DIDL-Lite>")],
        );
        assert!(e.contains("&lt;DIDL-Lite&gt;Kök &amp; &lt;3&lt;/DIDL-Lite&gt;"));
        assert!(
            e.contains(r#"<u:AddURIToQueue xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">"#)
        );
    }

    #[test]
    fn parse_response_unescapes_values() {
        let body = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetPositionInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><CurrentURI>x-sonos-http:track%2f1.flac?sid=174&amp;sn=3</CurrentURI><TrackMetaData>&lt;DIDL-Lite&gt;a &amp;amp; b&lt;/DIDL-Lite&gt;</TrackMetaData><Empty></Empty><Empty2/></u:GetPositionInfoResponse></s:Body></s:Envelope>"#;
        let map = parse_response(body, "GetPositionInfo").unwrap();
        assert_eq!(
            map["CurrentURI"],
            "x-sonos-http:track%2f1.flac?sid=174&sn=3"
        );
        assert_eq!(map["TrackMetaData"], "<DIDL-Lite>a &amp; b</DIDL-Lite>");
        assert_eq!(map["Empty"], "");
        assert_eq!(map["Empty2"], "");
    }

    #[test]
    fn parse_response_accepts_self_closing_response() {
        let body = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:PlayResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"/></s:Body></s:Envelope>"#;
        assert!(parse_response(body, "Play").unwrap().is_empty());
    }

    #[test]
    fn parse_response_missing_action_is_error() {
        assert!(parse_response("<a/>", "Play").is_err());
    }

    #[test]
    fn parse_fault_reads_upnp_error_code() {
        assert_eq!(
            parse_fault(include_str!("fixtures/soap_fault.xml")),
            Some(710)
        );
        assert_eq!(parse_fault("<a/>"), None);
    }

    #[test]
    fn parse_hms_edge_cases() {
        assert_eq!(parse_hms("0:03:25"), Some(205.0));
        assert_eq!(parse_hms("10:02:03"), Some(36123.0));
        assert_eq!(parse_hms("NOT_IMPLEMENTED"), None);
        assert_eq!(parse_hms(""), None);
        assert_eq!(parse_hms("0:00:00"), Some(0.0));
        assert_eq!(format_hms(3725.4), "1:02:05");
    }
}
