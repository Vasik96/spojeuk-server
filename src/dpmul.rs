//! DPMÚL (Dopravní podnik města Ústí nad Labem): převod surové odpovědi dopravce na vozidla.
//!
//! Surový tvar (pole objektů, buď přímo, nebo v objektu pod libovolným klíčem, typicky "vehicles"):
//! {"vehicleNumber": 666, "delay": "00:00:00" | null, "lineName": "72", "routeNumber": 2035, "transportType": "O",
//!  "longitude": 14.02, "latitude": 50.65, "stationNumber": 884, "finalStationNumber": 368, "azimuth": 0,
//!  "gpsTimestamp": "2026-11-23T12:22:10" (bez pásma = pražský čas), "lastActivity": "...+01:00",
//!  "isWheelchairAccessible": true, "isAirConditioned": true, ...}
//!
//! Druh vozidla určuje `transportType`: "O" = trolejbus, "T" = autobus. Neznámá hodnota druh nenastaví
//! (zůstane prázdný a [merge::fuse] z něj udělá autobus), aby se nový typ omylem neobarvil jako trolejbus.
//!
//! Hotový formát `{"vehicles": [...]}` (tvar merge::Vehicle) z externího adaptéru se dál bere beze změny, viz [raw_list].

use crate::merge::{parse_time_ms, Vehicle};
use serde_json::Value;

pub const AGENCY: &str = "Dopravní podnik města Ústí nad Labem a.s.";

/// Polohy starší než tohle se nepoužijí (vůz, který přestal hlásit).
const MAX_AGE_S: i64 = 600;

/// `transportType` → druh dopravy pro appku ("trolleybus" | "bus"), neznámé = "".
pub fn mode_of(transport_type: &str) -> &'static str {
    match transport_type.trim().to_ascii_uppercase().as_str() {
        "O" => "trolleybus",
        "T" => "bus",
        _ => "",
    }
}

/// Je to surová odpověď dopravce? Vrátí pole vozidel, když je v odpovědi pole objektů s `vehicleNumber`
/// (nebo `transportType`) – ať je odpověď přímo pole, nebo objekt s polem pod jakýmkoli klíčem.
/// Odpověď ve tvaru merge::Vehicle (`lat`/`lng`) ani prázdné pole za surové nepovažuje.
pub fn raw_list(root: &Value) -> Option<&Vec<Value>> {
    let is_raw = |a: &Vec<Value>| {
        a.first().and_then(|o| o.as_object()).map_or(false, |o| o.contains_key("vehicleNumber") || o.contains_key("transportType"))
    };
    match root {
        Value::Array(a) if is_raw(a) => Some(a),
        Value::Object(m) => m.values().find_map(|v| match v {
            Value::Array(a) if is_raw(a) => Some(a),
            _ => None,
        }),
        _ => None,
    }
}

/// .NET TimeSpan ("00:02:30", "-00:01:00", "1.00:05:00") → minuty (zaokrouhleno, záporné = před časem); null / nesmysl = 0.
fn delay_minutes(v: Option<&Value>) -> i64 {
    let Some(s) = v.and_then(|v| v.as_str()) else { return 0 };
    let t = s.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    let mut it = t.split(':');
    let (Some(h), Some(m)) = (it.next(), it.next()) else { return 0 };
    let sec = it.next().unwrap_or("0");
    let (days, hours) = match h.split_once('.') {
        Some((d, h)) => (d.parse::<i64>().ok(), h.parse::<i64>().ok()),
        None => (Some(0), h.parse::<i64>().ok()),
    };
    let (Some(days), Some(hours), Ok(mins)) = (days, hours, m.parse::<i64>()) else { return 0 };
    let secs = sec.split('.').next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let total = days * 86400 + hours * 3600 + mins * 60 + secs;
    let min = (total + 30) / 60;
    if neg { -min } else { min }
}

/// Pole surových vozidel → vozidla (zdroj, segment a city doplní server v pollerovi).
pub fn parse_raw(list: &[Value], now_ms: i64) -> Vec<Vehicle> {
    let mut out = Vec::with_capacity(list.len());
    for o in list {
        let s = |k: &str| o.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let int = |k: &str| {
            o.get(k).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))).unwrap_or(0)
        };
        let (Some(lat), Some(lng)) = (o.get("latitude").and_then(|v| v.as_f64()), o.get("longitude").and_then(|v| v.as_f64())) else { continue };
        let mut line = s("lineName");
        if line.is_empty() || lat == 0.0 || lng == 0.0 {
            continue;
        }
        // stejné pravidlo jako u DÚK (merge::duk_vehicle): linka 200 je interní označení, ve výstupu je "X"
        if line == "200" {
            line = "X".into();
        }
        let fix = parse_time_ms(&s("gpsTimestamp")).or_else(|| parse_time_ms(&s("lastActivity")));
        let age_sec = fix.map_or(0, |t| ((now_ms - t) / 1000).max(0));
        if age_sec > MAX_AGE_S {
            continue;
        }
        out.push(Vehicle {
            source: "dpmul".into(),
            segment: "city".into(),
            id: int("vehicleNumber"),
            lat,
            lng,
            line,
            delay: delay_minutes(o.get("delay")),
            age_sec,
            agency: AGENCY.into(),
            low: o.get("isWheelchairAccessible").and_then(|v| v.as_bool()),
            ac: o.get("isAirConditioned").and_then(|v| v.as_bool()),
            // azimut 0 hlásí i jedoucí vozy (neznámý směr), skutečný směr bývá s desetinami
            heading: o.get("azimuth").and_then(|v| v.as_f64()).filter(|h| h.is_finite() && *h != 0.0),
            city: true,
            station: int("stationNumber"),
            final_station: int("finalStationNumber"),
            mode: mode_of(&s("transportType")).into(),
            ..Default::default()
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"vehicles":[
      {"vehicleNumber":666,"delay":"00:00:00","lineName":"72","routeNumber":2035,"transportType":"O","longitude":14.025486,"latitude":50.657767,
       "stationNumber":884,"stationPost":1,"stationPlatform":"1","finalStationNumber":368,"azimuth":0,"gpsTimestamp":"2026-11-23T12:22:10",
       "lastActivity":"2026-11-23T12:22:10.8680835+01:00","isWheelchairAccessible":true,"isAirConditioned":true},
      {"vehicleNumber":669,"delay":null,"lineName":"80","routeNumber":5081,"transportType":"O","longitude":14.034585,"latitude":50.661402,
       "stationNumber":974,"finalStationNumber":260,"azimuth":170.8,"gpsTimestamp":"2026-11-23T12:22:06",
       "lastActivity":"2026-11-23T12:22:06.5342824+01:00","isWheelchairAccessible":true,"isAirConditioned":false},
      {"vehicleNumber":801,"delay":"-00:02:00","lineName":"9","transportType":"T","longitude":14.0,"latitude":50.66,
       "gpsTimestamp":"2026-11-23T12:22:00","lastActivity":"2026-11-23T12:22:00+01:00"},
      {"vehicleNumber":802,"delay":"00:04:40","lineName":"9","transportType":"X","longitude":14.0,"latitude":50.66,
       "gpsTimestamp":"2026-11-23T12:22:00"}
    ]}"#;

    // 2026-11-23T12:22:30 pražského času (CET, UTC+1) = 11:22:30 UTC
    fn now_ms() -> i64 {
        parse_time_ms("2026-11-23T12:22:30+01:00").unwrap()
    }

    #[test]
    fn o_is_trolleybus_t_is_bus() {
        assert_eq!(mode_of("O"), "trolleybus");
        assert_eq!(mode_of("T"), "bus");
        assert_eq!(mode_of(" o "), "trolleybus");
        assert_eq!(mode_of("t"), "bus");
        assert_eq!(mode_of("X"), "");
        assert_eq!(mode_of(""), "");
    }

    #[test]
    fn parses_raw_vehicles() {
        let root: Value = serde_json::from_str(SAMPLE).unwrap();
        let vs = parse_raw(raw_list(&root).expect("surový formát"), now_ms());
        assert_eq!(vs.len(), 4);
        let a = &vs[0];
        assert_eq!((a.id, a.line.as_str(), a.mode.as_str(), a.delay, a.age_sec), (666, "72", "trolleybus", 0, 20));
        assert_eq!((a.station, a.final_station, a.heading, a.low, a.ac), (884, 368, None, Some(true), Some(true)));
        let b = &vs[1];
        assert_eq!((b.id, b.mode.as_str(), b.heading, b.delay, b.ac), (669, "trolleybus", Some(170.8), 0, Some(false)));
        assert_eq!((vs[2].mode.as_str(), vs[2].delay), ("bus", -2));
        assert_eq!((vs[3].mode.as_str(), vs[3].delay), ("", 5), "neznámý typ nesmí být trolejbus; 4:40 = 5 min");
    }

    #[test]
    fn line_200_is_x() {
        let root: Value = serde_json::from_str(
            r#"[{"vehicleNumber":5,"lineName":"200","transportType":"T","latitude":50.6,"longitude":14.0,"gpsTimestamp":"2026-11-23T12:22:20"},
                {"vehicleNumber":6,"lineName":"20","transportType":"T","latitude":50.6,"longitude":14.0,"gpsTimestamp":"2026-11-23T12:22:20"}]"#,
        )
        .unwrap();
        let vs = parse_raw(raw_list(&root).unwrap(), now_ms());
        assert_eq!((vs[0].line.as_str(), vs[1].line.as_str()), ("X", "20"));
    }

    #[test]
    fn stale_or_positionless_vehicles_are_dropped() {
        let root: Value = serde_json::from_str(
            r#"[{"vehicleNumber":1,"lineName":"72","transportType":"O","latitude":50.6,"longitude":14.0,"gpsTimestamp":"2026-11-23T12:00:00"},
                {"vehicleNumber":2,"lineName":"72","transportType":"O","latitude":0,"longitude":0,"gpsTimestamp":"2026-11-23T12:22:00"},
                {"vehicleNumber":3,"lineName":"","transportType":"O","latitude":50.6,"longitude":14.0,"gpsTimestamp":"2026-11-23T12:22:00"}]"#,
        )
        .unwrap();
        assert!(parse_raw(raw_list(&root).unwrap(), now_ms()).is_empty());
    }

    #[test]
    fn detects_raw_vs_adapter_format() {
        let adapter: Value = serde_json::from_str(r#"{"vehicles":[{"id":451,"lat":50.66,"lng":14.0,"line":"81","mode":"trolleybus"}]}"#).unwrap();
        assert!(raw_list(&adapter).is_none());
        let empty: Value = serde_json::from_str(r#"{"vehicles":[]}"#).unwrap();
        assert!(raw_list(&empty).is_none());
        let bare: Value = serde_json::from_str(r#"[{"vehicleNumber":1}]"#).unwrap();
        assert!(raw_list(&bare).is_some());
        let other_key: Value = serde_json::from_str(r#"{"x":1,"data":[{"transportType":"T"}]}"#).unwrap();
        assert!(raw_list(&other_key).is_some());
    }

    #[test]
    fn delay_formats() {
        let d = |s: &str| delay_minutes(Some(&Value::String(s.into())));
        assert_eq!((d("00:00:00"), d("00:03:00"), d("-00:01:29"), d("1.00:05:00"), d("00:02:30.1234"), d("nesmysl")), (0, 3, -1, 1445, 3, 0));
        assert_eq!(delay_minutes(None), 0);
        assert_eq!(delay_minutes(Some(&Value::Null)), 0);
    }
}
