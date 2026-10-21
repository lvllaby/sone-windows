//! SSDP discovery of Sonos speakers and topology fetch.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::Instant;

use super::soap::{Service, SoapClient};
use super::topology::{parse_zone_group_state, SonosGroup};
use crate::error::SoneError;

const SSDP_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const ZONEPLAYER_ST: &str = "urn:schemas-upnp-org:device:ZonePlayer:1";

const SEARCH: &str = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n";

/// Interfaces worth probing: drops loopback, link-local and unspecified.
pub fn candidate_interfaces(addrs: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    addrs
        .iter()
        .copied()
        .filter(|a| !a.is_loopback() && !a.is_link_local() && !a.is_unspecified())
        .collect()
}

/// Return the speaker IP from an SSDP response if it is a ZonePlayer.
/// Header names are case-insensitive.
pub fn parse_ssdp_location(resp: &str) -> Option<String> {
    let mut location = None;
    let mut is_zoneplayer = false;
    for line in resp.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "location" => location = Some(value.to_string()),
            "st" => is_zoneplayer = value.eq_ignore_ascii_case(ZONEPLAYER_ST),
            _ => {}
        }
    }
    if !is_zoneplayer {
        return None;
    }
    let rest = location?.split("://").nth(1)?.to_string();
    let ip = rest.split('/').next()?.split(':').next()?.to_string();
    ip.parse::<Ipv4Addr>().ok().map(|_| ip)
}

/// Multicast an M-SEARCH on every candidate interface and collect unique
/// speaker IPs until `timeout` elapses.
pub async fn discover(timeout: Duration) -> Vec<String> {
    let addrs: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .collect();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut tasks = Vec::new();
    for iface in candidate_interfaces(&addrs) {
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            let Ok(sock) = UdpSocket::bind((iface, 0)).await else {
                return;
            };
            let dest = SocketAddrV4::new(SSDP_ADDR, SSDP_PORT);
            // Twice: UDP is lossy.
            for _ in 0..2 {
                let _ = sock.send_to(SEARCH.as_bytes(), dest).await;
            }
            let deadline = Instant::now() + timeout;
            let mut buf = [0u8; 2048];
            while let Ok(Ok((n, _))) =
                tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await
            {
                if let Some(ip) = parse_ssdp_location(&String::from_utf8_lossy(&buf[..n])) {
                    let _ = tx.send(ip);
                }
            }
        }));
    }
    drop(tx);

    // The channel closes once every interface task has finished.
    let mut found: Vec<String> = Vec::new();
    while let Some(ip) = rx.recv().await {
        if !found.contains(&ip) {
            found.push(ip);
        }
    }
    found
}

/// Ask the seed speakers in order for the zone topology; first success wins.
pub async fn fetch_topology(
    soap: &SoapClient,
    seed_ips: &[String],
) -> Result<Vec<SonosGroup>, SoneError> {
    let mut last_err = SoneError::Sonos("no Sonos speakers to query".into());
    for ip in seed_ips {
        let result = async {
            let map = soap
                .call(ip, Service::ZoneGroupTopology, "GetZoneGroupState", &[])
                .await?;
            let xml = map.get("ZoneGroupState").ok_or_else(|| {
                SoneError::Sonos("GetZoneGroupState: missing ZoneGroupState".into())
            })?;
            parse_zone_group_state(xml)
        }
        .await;
        match result {
            Ok(groups) => return Ok(groups),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn ssdp_parses_zoneplayer_location() {
        let resp = "HTTP/1.1 200 OK\r\nlocation: http://192.0.2.10:1400/xml/device_description.xml\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n";
        assert_eq!(parse_ssdp_location(resp), Some("192.0.2.10".to_string()));
        let resp = "HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\nLOCATION: http://192.0.2.11:1400/x.xml\r\n";
        assert_eq!(parse_ssdp_location(resp), Some("192.0.2.11".to_string()));
    }

    #[test]
    fn ssdp_ignores_non_zoneplayer_responses() {
        let resp = "HTTP/1.1 200 OK\r\nlocation: http://192.0.2.10:1400/xml/device_description.xml\r\nST: upnp:rootdevice\r\n";
        assert_eq!(parse_ssdp_location(resp), None);
    }

    #[test]
    fn interfaces_skip_loopback_and_link_local() {
        let addrs = [
            Ipv4Addr::new(127, 0, 0, 1),
            Ipv4Addr::new(169, 254, 3, 4),
            Ipv4Addr::new(192, 168, 1, 5),
        ];
        assert_eq!(
            candidate_interfaces(&addrs),
            vec![Ipv4Addr::new(192, 168, 1, 5)]
        );
    }
}
