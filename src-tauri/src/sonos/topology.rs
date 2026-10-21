//! Sonos zone group topology: parses `GetZoneGroupState` into groups of rooms.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::Serialize;

use crate::error::SoneError;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SonosMember {
    pub id: String,
    pub room_name: String,
    pub ip: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SonosGroup {
    pub id: String,
    pub coordinator_id: String,
    pub name: String,
    /// Visible members only, coordinator first.
    pub members: Vec<SonosMember>,
    pub reachable: bool,
    /// UUIDs of every member including hidden ones (stereo-pair secondaries,
    /// subs, surrounds), so a remembered coordinator that was re-paired into
    /// a hidden role can still be located.
    #[serde(skip)]
    pub(crate) all_member_ids: Vec<String>,
}

/// Group being built while walking the XML.
struct PendingGroup {
    id: String,
    coordinator_id: String,
    members: Vec<SonosMember>,
    all_member_ids: Vec<String>,
}

fn attrs(e: &BytesStart) -> Result<Vec<(String, String)>, SoneError> {
    let mut out = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|e| SoneError::Parse(format!("zone topology attribute: {e}")))?;
        let key = String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned();
        let value = a
            .unescape_value()
            .map_err(|e| SoneError::Parse(format!("zone topology value: {e}")))?
            .into_owned();
        out.push((key, value));
    }
    Ok(out)
}

fn attr<'a>(list: &'a [(String, String)], key: &str) -> Option<&'a str> {
    list.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// `http://192.0.2.34:1400/xml/device_description.xml` -> `192.0.2.34`.
fn ip_from_location(location: &str) -> Option<String> {
    let rest = location.split("://").nth(1)?;
    let host = rest.split('/').next()?;
    let ip = host.split(':').next()?;
    (!ip.is_empty()).then(|| ip.to_string())
}

fn finish(p: PendingGroup) -> Option<SonosGroup> {
    let mut members = p.members;
    // Coordinator first; the rest keep their document order.
    if let Some(pos) = members.iter().position(|m| m.id == p.coordinator_id) {
        let coord = members.remove(pos);
        members.insert(0, coord);
    }
    if members.is_empty() {
        return None;
    }
    let name = members
        .iter()
        .map(|m| m.room_name.as_str())
        .collect::<Vec<_>>()
        .join(" + ");
    Some(SonosGroup {
        id: p.id,
        coordinator_id: p.coordinator_id,
        name,
        members,
        reachable: true,
        all_member_ids: p.all_member_ids,
    })
}

/// Parse the `ZoneGroupState` value (with or without a `<ZoneGroups>`
/// wrapper) into groups sorted by name.
pub fn parse_zone_group_state(xml: &str) -> Result<Vec<SonosGroup>, SoneError> {
    let mut reader = Reader::from_str(xml);
    let mut groups = Vec::new();
    let mut current: Option<PendingGroup> = None;

    loop {
        let ev = reader
            .read_event()
            .map_err(|e| SoneError::Parse(format!("invalid zone topology XML: {e}")))?;
        match ev {
            Event::Start(e) | Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                match name.as_str() {
                    "ZoneGroup" => {
                        let a = attrs(&e)?;
                        current = Some(PendingGroup {
                            id: attr(&a, "ID").unwrap_or_default().to_string(),
                            coordinator_id: attr(&a, "Coordinator").unwrap_or_default().to_string(),
                            members: Vec::new(),
                            all_member_ids: Vec::new(),
                        });
                    }
                    "ZoneGroupMember" | "Satellite" => {
                        if let Some(g) = current.as_mut() {
                            let a = attrs(&e)?;
                            let id = attr(&a, "UUID").unwrap_or_default().to_string();
                            g.all_member_ids.push(id.clone());
                            // Satellites (Sub/surrounds) never count as rooms.
                            let hidden = name == "Satellite" || attr(&a, "Invisible") == Some("1");
                            if !hidden {
                                if let Some(ip) = attr(&a, "Location").and_then(ip_from_location) {
                                    g.members.push(SonosMember {
                                        id,
                                        room_name: attr(&a, "ZoneName")
                                            .unwrap_or_default()
                                            .to_string(),
                                        ip,
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                if e.local_name().as_ref() == b"ZoneGroup" {
                    if let Some(g) = current.take().and_then(finish) {
                        groups.push(g);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    groups.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(groups)
}

/// Find the group containing `member_id`, including hidden members (a
/// remembered coordinator may have become a hidden pair secondary).
pub fn find_group_for_member<'a>(
    groups: &'a [SonosGroup],
    member_id: &str,
) -> Option<&'a SonosGroup> {
    groups.iter().find(|g| {
        g.members.iter().any(|m| m.id == member_id)
            || g.all_member_ids.iter().any(|id| id == member_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonos::soap::parse_response;

    fn fixture_groups() -> Vec<SonosGroup> {
        let map = parse_response(
            include_str!("fixtures/zone_group_state.xml"),
            "GetZoneGroupState",
        )
        .unwrap();
        parse_zone_group_state(&map["ZoneGroupState"]).unwrap()
    }

    const TWO_ROOMS: &str = r#"<ZoneGroupState><ZoneGroups><ZoneGroup Coordinator="RINCON_C" ID="RINCON_C:1"><ZoneGroupMember UUID="RINCON_O" Location="http://192.0.2.2:1400/xml/device_description.xml" ZoneName="Other Room"/><ZoneGroupMember UUID="RINCON_C" Location="http://192.0.2.1:1400/xml/device_description.xml" ZoneName="Coordinator Room"><Satellite UUID="RINCON_S" Location="http://192.0.2.3:1400/xml/device_description.xml" ZoneName="Coordinator Room" Invisible="1"/></ZoneGroupMember></ZoneGroup></ZoneGroups></ZoneGroupState>"#;

    #[test]
    fn topology_parses_groups_and_names() {
        let groups = fixture_groups();
        assert_eq!(groups.len(), 2);
        // Sorted by name.
        assert_eq!(groups[0].name, "Kök & Bad");
        assert_eq!(groups[1].name, "Sonos Music");
        assert_eq!(groups[0].coordinator_id, "RINCON_00000000000301400");
        assert_eq!(groups[0].members.len(), 1);
        assert_eq!(groups[0].members[0].ip, "192.0.2.214");
        assert_eq!(groups[1].coordinator_id, "RINCON_00000000000101400");
        assert_eq!(groups[1].members[0].ip, "192.0.2.34");
        assert!(groups.iter().all(|g| g.reachable));
    }

    #[test]
    fn topology_joins_visible_rooms_coordinator_first() {
        let groups = parse_zone_group_state(TWO_ROOMS).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Coordinator Room + Other Room");
        assert_eq!(groups[0].members[0].id, "RINCON_C");
        assert_eq!(groups[0].members[1].id, "RINCON_O");
    }

    #[test]
    fn topology_skips_invisible_members() {
        let groups = fixture_groups();
        let pair = &groups[1];
        assert_eq!(pair.members.len(), 1);
        assert!(pair
            .members
            .iter()
            .all(|m| m.id != "RINCON_00000000000201400"));
        assert_eq!(pair.members[0].id, "RINCON_00000000000101400");
    }

    #[test]
    fn topology_skips_satellites() {
        let groups = parse_zone_group_state(TWO_ROOMS).unwrap();
        assert!(groups[0].members.iter().all(|m| m.id != "RINCON_S"));
        assert_eq!(groups[0].members.len(), 2);
    }

    #[test]
    fn topology_unescapes_room_names() {
        let groups = fixture_groups();
        assert!(groups
            .iter()
            .flat_map(|g| &g.members)
            .any(|m| m.room_name == "Kök & Bad"));
    }

    #[test]
    fn find_group_for_member_finds_regrouped_coordinator() {
        let groups = fixture_groups();
        // Hidden pair secondary still resolves to its group.
        let g = find_group_for_member(&groups, "RINCON_00000000000201400").unwrap();
        assert_eq!(g.name, "Sonos Music");
        let g = find_group_for_member(&groups, "RINCON_00000000000301400").unwrap();
        assert_eq!(g.name, "Kök & Bad");
        assert!(find_group_for_member(&groups, "RINCON_UNKNOWN").is_none());
        // Satellites count too.
        let two = parse_zone_group_state(TWO_ROOMS).unwrap();
        assert!(find_group_for_member(&two, "RINCON_S").is_some());
    }
}
