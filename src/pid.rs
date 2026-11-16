//! PID (Pražská integrovaná doprava): polohy z Golemio, `GET /v2/vehiclepositions` (GeoJSON, klíč v `X-Access-Token`).
//! Popis zdroje a zvláštnosti (PID-Q1 … Q10): dossier pid.md z projektu obehy.
//!
//! Golemio u `includeNotTracking=true` vrací jeden Feature na každý spoj oběhu vozidla (předchozí, běžící,
//! příští), všechny na jedné poloze vozidla. Z nich se tu vybírá jeden záznam na vozidlo (PID-Q6):
//!   - běžící spoj (`on_track` / `at_stop`, před `off_track`),
//!   - jinak nejbližší příští spoj jako `state = 2` (nejede spoj, stejně jako stavy 2/3 u DÚK),
//!   - jinak nic (po posledním spoji, zrušený spoj).
//!
//! Vlaky (`route_type` 2) nemají číslo vozu (PID-Q2); klíčem je číslo vlaku z `cis.trip_number`, a protože
//! `Vehicle.cis` u vlaků nese číslo vlaku stejně jako u SŽ a DÚK, spojí je `merge::fuse` do jednoho vlaku.
//! Vlak se bere jen za jízdy: bez čísla vozu nelze poznat, kde stojí ten, co teprve pojede.

use crate::merge::{js_number, js_parse_int, js_string, js_truthy, parse_time_ms, Vehicle};
use serde_json::Value;
use std::collections::HashMap;

/// Starší poloha se zahodí (stejně jako u ostatních zdrojů).
const MAX_AGE_S: i64 = 600;

fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut c = v;
    for k in path {
        c = c.get(*k)?;
    }
    Some(c)
}

/// Text bez okrajových mezer; null, "null" a "none" (cis.line_id u vlaků) = prázdný.
fn text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(x) => {
            let s = js_string(x);
            let t = s.trim();
            if t.eq_ignore_ascii_case("null") || t.eq_ignore_ascii_case("none") {
                String::new()
            } else {
                t.to_string()
            }
        }
    }
}

fn flag(v: Option<&Value>) -> Option<bool> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        other => Some(js_truthy(other)),
    }
}

struct Meta<'a> {
    rt: i64,
    train: bool,
    fleet: &'a str,
    train_no: &'a str,
    vkey: &'a str,
    idle: bool,
}

/// Feature → vozidlo, nebo None (bez polohy / času nebo příliš stará). Geografický filtr žádný.
fn vehicle_of(f: &Value, props: &Value, now_ms: i64, m: &Meta) -> Option<Vehicle> {
    let c = at(f, &["geometry", "coordinates"])?.as_array()?;
    let (lng, lat) = (js_number(c.first()), js_number(c.get(1)));
    if lat.is_nan() || lng.is_nan() || lat == 0.0 || lng == 0.0 {
        return None;
    }
    // bez času určení nelze poznat stáří, takže se takový záznam nebere
    let fix = at(props, &["last_position", "origin_timestamp"]).and_then(|v| parse_time_ms(&js_string(v)))?;
    let age_sec = ((now_ms - fix) / 1000).max(0);
    if age_sec > MAX_AGE_S {
        return None;
    }

    let mut line = text(at(props, &["trip", "gtfs", "route_short_name"]));
    if line.is_empty() {
        line = text(at(props, &["trip", "cis", "line_short_name"]));
    }
    if line.is_empty() {
        line = "?".into();
    }
    // regionální autobus = má linku v CIS (PID-Q3: městské linky mají cis.line_id null)
    let cis_line = text(at(props, &["trip", "cis", "line_id"]));
    let cis_line_num = !cis_line.is_empty() && cis_line.bytes().all(|b| b.is_ascii_digit());
    let (segment, city) = if m.train {
        ("regional", false)
    } else if matches!(m.rt, 0 | 1 | 4 | 11) || !cis_line_num {
        ("city", true) // tramvaj, metro, přívoz, trolejbus, pražský autobus
    } else {
        ("regional", false)
    };
    let id = if m.train { js_parse_int(Some(&Value::String(m.train_no.to_string()))) } else { js_parse_int(Some(&Value::String(m.fleet.to_string()))) };
    let delay_s = js_number(at(props, &["last_position", "delay", "actual"]));
    let delay = if m.idle || delay_s.is_nan() { 0 } else { (delay_s / 60.0).round() as i64 };
    let heading = {
        let h = js_number(at(props, &["last_position", "bearing"]));
        if h.is_finite() && !matches!(at(props, &["last_position", "bearing"]), Some(Value::Null) | None) { Some(h) } else { None }
    };
    let agency = {
        let real = text(at(props, &["trip", "agency_name", "real"]));
        if real.is_empty() { text(at(props, &["trip", "agency_name", "scheduled"])) } else { real }
    };
    Some(Vehicle {
        source: "pid".into(),
        segment: segment.into(),
        id,
        lat,
        lng,
        line,
        delay,
        age_sec,
        state: if m.idle { 2 } else { 0 },
        train: m.train,
        cis: if m.train {
            m.train_no.trim_start_matches('0').to_string()
        } else if cis_line_num {
            format!("{:0>6}", cis_line)
        } else {
            String::new()
        },
        trip: if m.train { String::new() } else { text(at(props, &["trip", "cis", "trip_number"])) },
        agency,
        dest: text(at(props, &["trip", "gtfs", "trip_headsign"])),
        low: flag(at(props, &["trip", "wheelchair_accessible"])),
        ac: flag(at(props, &["trip", "air_conditioned"])),
        heading,
        city,
        vkey: m.vkey.to_string(),
        ..Default::default()
    })
}

#[derive(Default)]
struct Group {
    /// (pořadí: 0 = on_track / at_stop, 1 = off_track; začátek spoje v ms, vozidlo)
    running: Option<(u8, i64, Vehicle)>,
    /// (začátek spoje v ms, vozidlo): nejbližší příští spoj
    upcoming: Option<(i64, Vehicle)>,
}

/// Odpověď Golemio (FeatureCollection) → jedno vozidlo na oběh (viz výše).
pub fn parse(data: &Value, now_ms: i64) -> Vec<Vehicle> {
    let Some(features) = data.get("features").and_then(|v| v.as_array()) else { return Vec::new() };
    let mut order: Vec<Group> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    for f in features {
        let Some(props) = f.get("properties") else { continue };
        let state = text(at(props, &["last_position", "state_position"]));
        if state == "canceled" || js_truthy(at(props, &["last_position", "is_canceled"])) {
            continue; // zrušený spoj poloh nemá (PID-Q4); rušení spojů zde nedělá nic
        }
        let running = matches!(state.as_str(), "on_track" | "at_stop" | "off_track")
            || (state.is_empty() && js_truthy(at(props, &["last_position", "tracking"])));
        let upcoming = state.starts_with("before_track");
        if !running && !upcoming {
            continue; // after_track: spoj skončil
        }

        let rt = {
            let n = js_number(at(props, &["trip", "gtfs", "route_type"]));
            if n.is_nan() { -1 } else { n as i64 }
        };
        let train = rt == 2;
        let fleet = text(at(props, &["trip", "vehicle_registration_number"]));
        let train_no = text(at(props, &["trip", "cis", "trip_number"]));
        let gtfs_trip = text(at(props, &["trip", "gtfs", "trip_id"]));
        // klíč vozidla (PID-Q1, Q2): číslo vozu je jen v rámci druhu dopravy, vlaky a metro ho nemají
        let vkey = if train && !train_no.is_empty() {
            format!("train:{}", train_no.trim_start_matches('0'))
        } else if !train && !fleet.is_empty() {
            format!("{rt}:{fleet}")
        } else if !gtfs_trip.is_empty() {
            format!("trip:{gtfs_trip}")
        } else {
            continue;
        };
        // příští spoj dává smysl jen u vozidla s číslem vozu (jinak by šlo o jiný vlak / jízdu metra)
        let groupable = !train && !fleet.is_empty();
        if upcoming && !groupable {
            continue;
        }
        let start = at(props, &["trip", "start_timestamp"]).and_then(|v| parse_time_ms(&js_string(v))).unwrap_or(i64::MAX);
        let rank: u8 = if state == "off_track" { 1 } else { 0 };

        let gi = *index.entry(vkey.clone()).or_insert_with(|| {
            order.push(Group::default());
            order.len() - 1
        });
        let g = &mut order[gi];
        let meta = Meta { rt, train, fleet: &fleet, train_no: &train_no, vkey: &vkey, idle: upcoming };
        if running {
            // na trase před off_track, při shodě později začatý spoj
            let better = match &g.running {
                None => true,
                Some((r, s, _)) => rank < *r || (rank == *r && start > *s),
            };
            if better {
                if let Some(v) = vehicle_of(f, props, now_ms, &meta) {
                    g.running = Some((rank, start, v));
                }
            }
        } else {
            let better = g.upcoming.as_ref().map_or(true, |(s, _)| start < *s);
            if better {
                if let Some(v) = vehicle_of(f, props, now_ms, &meta) {
                    g.upcoming = Some((start, v));
                }
            }
        }
    }

    order
        .into_iter()
        .filter_map(|g| g.running.map(|(_, _, v)| v).or_else(|| g.upcoming.map(|(_, v)| v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-11-16T12:00:30+01:00";

    fn feat(state: &str, rt: i64, fleet: Option<&str>, trip: &str, extra_trip: &str, start: &str, lon: f64, lat: f64) -> String {
        let fleet = fleet.map_or("null".to_string(), |f| format!("\"{f}\""));
        format!(
            r#"{{"type":"Feature","geometry":{{"type":"Point","coordinates":[{lon},{lat}]}},"properties":{{
              "last_position":{{"origin_timestamp":"2026-11-16T11:00:25.000Z","bearing":90,"delay":{{"actual":125}},"state_position":"{state}","tracking":{tr},"is_canceled":false}},
              "trip":{{"gtfs":{{"trip_id":"{trip}","route_type":{rt},"route_short_name":"9","trip_headsign":"Sídliště Řepy"}},
                       "vehicle_registration_number":{fleet},"start_timestamp":"{start}",
                       "agency_name":{{"real":"DPP"}},"wheelchair_accessible":true,"air_conditioned":false{extra_trip}}}}}}}"#,
            tr = matches!(state, "on_track" | "at_stop" | "off_track"),
        )
    }

    fn collection(features: &[String]) -> Value {
        serde_json::from_str(&format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))).unwrap()
    }

    fn now_ms() -> i64 {
        parse_time_ms(NOW).unwrap()
    }

    #[test]
    fn one_entry_per_vehicle_running_wins() {
        let d = collection(&[
            feat("after_track", 0, Some("9001"), "t0", "", "2026-11-16T10:00:00Z", 14.4, 50.08),
            feat("on_track", 0, Some("9001"), "t1", "", "2026-11-16T10:55:00Z", 14.4, 50.08),
            feat("before_track", 0, Some("9001"), "t2", "", "2026-11-16T11:30:00Z", 14.4, 50.08),
        ]);
        let v = parse(&d, now_ms());
        assert_eq!(v.len(), 1);
        let v = &v[0];
        assert_eq!((v.source.as_str(), v.segment.as_str(), v.id, v.line.as_str(), v.state, v.delay, v.age_sec), ("pid", "city", 9001, "9", 0, 2, 5));
        assert_eq!((v.dest.as_str(), v.agency.as_str(), v.low, v.ac, v.heading, v.city), ("Sídliště Řepy", "DPP", Some(true), Some(false), Some(90.0), true));
        assert_eq!(v.vkey, "0:9001");
    }

    #[test]
    fn upcoming_is_idle_and_nearest_start_wins() {
        let d = collection(&[
            feat("before_track", 0, Some("9002"), "late", "", "2026-11-16T11:40:00Z", 14.4, 50.08),
            feat("before_track_delayed", 0, Some("9002"), "soon", "", "2026-11-16T11:10:00Z", 14.4, 50.08),
            feat("after_track", 0, Some("9003"), "x", "", "2026-11-16T10:00:00Z", 14.4, 50.08),
        ]);
        let v = parse(&d, now_ms());
        assert_eq!(v.len(), 1, "after_track samotné se nebere");
        assert_eq!((v[0].id, v[0].state, v[0].delay), (9002, 2, 0));
    }

    #[test]
    fn cancelled_and_bad_positions_are_dropped() {
        let mut c = feat("canceled", 3, None, "c", "", "2026-11-16T11:00:00Z", 14.4, 50.08);
        c = c.replace("\"is_canceled\":false", "\"is_canceled\":true");
        let d = collection(&[
            c,
            feat("on_track", 0, Some("1"), "a", "", "2026-11-16T10:55:00Z", 0.0, 0.0),
        ]);
        assert!(parse(&d, now_ms()).is_empty());
        // stará poloha
        assert!(parse(&collection(&[feat("on_track", 0, Some("3"), "d", "", "2026-11-16T10:55:00Z", 14.4, 50.08)]), now_ms() + 700_000).is_empty());
    }

    #[test]
    fn same_fleet_number_in_two_modes_stays_two_vehicles() {
        let d = collection(&[
            feat("on_track", 0, Some("7000"), "tram", "", "2026-11-16T10:55:00Z", 14.4, 50.08),
            feat("on_track", 3, Some("7000"), "bus", "", "2026-11-16T10:55:00Z", 14.5, 50.09),
        ]);
        let v = parse(&d, now_ms());
        assert_eq!(v.len(), 2);
        assert_ne!(v[0].vkey, v[1].vkey);
    }

    #[test]
    fn regional_bus_has_cis_and_trip() {
        let d = collection(&[feat(
            "on_track", 3, Some("5555"), "b", r#","cis":{"line_id":"100388","trip_number":42}"#, "2026-11-16T10:55:00Z", 14.4, 50.0,
        )]);
        let v = &parse(&d, now_ms())[0];
        assert_eq!((v.segment.as_str(), v.city, v.cis.as_str(), v.trip.as_str(), v.train), ("regional", false, "100388", "42", false));
    }

    #[test]
    fn train_keyed_by_number_and_only_when_running() {
        let cis = r#","cis":{"line_id":"none","trip_number":"09715"}"#;
        let d = collection(&[
            feat("on_track", 2, None, "z1", cis, "2026-11-16T10:55:00Z", 14.3, 50.1),
            feat("before_track", 2, None, "z2", r#","cis":{"line_id":"none","trip_number":"9999"}"#, "2026-11-16T11:30:00Z", 14.3, 50.1),
        ]);
        let v = parse(&d, now_ms());
        assert_eq!(v.len(), 1);
        let t = &v[0];
        assert!(t.train && t.segment == "regional" && !t.city);
        assert_eq!((t.cis.as_str(), t.id, t.vkey.as_str(), t.trip.as_str()), ("9715", 9715, "train:9715", ""));
    }

    #[test]
    fn metro_without_fleet_is_keyed_by_trip() {
        let d = collection(&[feat("on_track", 1, None, "metro-1", "", "2026-11-16T10:55:00Z", 14.4, 50.08)]);
        let v = &parse(&d, now_ms())[0];
        assert_eq!((v.vkey.as_str(), v.id, v.segment.as_str()), ("trip:metro-1", 0, "city"));
    }
}
