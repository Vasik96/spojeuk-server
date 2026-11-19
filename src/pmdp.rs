//! PMDP (Plzeňské městské dopravní podniky): tramvaje, trolejbusy a autobusy MHD Plzeň z mapy
//! https://jizdnirady.pmdp.cz/provoz. Bez přihlášení.
//!
//! Mapa dostává stav přes klasický ASP.NET SignalR (hub `provozHub`): klient se zaregistruje metodou
//! `registrace(null)` (null = všechny linky) a server posílá `updateTraffic` se všemi vozy zhruba po 2–8 s.
//! Server se připojuje transportem longPolling (obyčejné HTTP dotazy, viz main.rs pmdp_session), tady je jen rozbor.
//!
//! Vůz: VehicleNumber, Line {Name, Number (linka CIS, např. 445001), TractionType (1 tramvaj, 2 trolejbus,
//! 3 autobus), IsBarrierFree}, DelayMin, NextStopName, DestinationName, Latitude, Longitude, Azimuth,
//! Timestamp (ISO s časovou zónou), IsAirConditioned.

use crate::merge::{parse_time_ms, Vehicle};
use serde_json::Value;

pub const AGENCY: &str = "Plzeňské městské dopravní podniky, a.s.";

/// Druh dopravy podle trakce linky (Line.TractionType).
fn mode_of(traction: i64) -> &'static str {
    match traction {
        1 => "tram",
        2 => "trolleybus",
        _ => "bus",
    }
}

/// Polohy starší než tohle se nepoužijí (vůz, který přestal hlásit).
const MAX_AGE_S: i64 = 600;

/// Argument `updateTraffic` (pole vozů) → vozidla.
pub fn parse_traffic(list: &[Value], now_ms: i64) -> Vec<Vehicle> {
    let mut out = Vec::with_capacity(list.len());
    for o in list {
        let s = |k: &str| o.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let line_obj = o.get("Line");
        let line = line_obj.and_then(|l| l.get("Name")).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let (Some(lat), Some(lng)) = (o.get("Latitude").and_then(|v| v.as_f64()), o.get("Longitude").and_then(|v| v.as_f64())) else { continue };
        if line.is_empty() || !(48.0..=52.0).contains(&lat) || !(12.0..=19.0).contains(&lng) {
            continue;
        }
        let fix = parse_time_ms(&s("Timestamp"));
        let age_sec = fix.map_or(0, |t| ((now_ms - t) / 1000).max(0));
        if age_sec > MAX_AGE_S {
            continue;
        }
        let traction = line_obj.and_then(|l| l.get("TractionType")).and_then(|v| v.as_i64()).unwrap_or(3);
        let cis = line_obj.and_then(|l| l.get("Number")).and_then(|v| v.as_i64()).filter(|n| *n > 0).map(|n| n.to_string()).unwrap_or_default();
        let id = s("VehicleNumber").parse::<i64>().unwrap_or(0);
        out.push(Vehicle {
            source: "pmdp".into(),
            segment: "city".into(),
            id,
            lat,
            lng,
            line,
            delay: o.get("DelayMin").and_then(|v| v.as_i64()).unwrap_or(0),
            age_sec,
            cis,
            agency: AGENCY.into(),
            dest: s("DestinationName"),
            low: line_obj.and_then(|l| l.get("IsBarrierFree")).and_then(|v| v.as_bool()),
            ac: o.get("IsAirConditioned").and_then(|v| v.as_bool()),
            heading: o.get("Azimuth").and_then(|v| v.as_f64()).filter(|h| h.is_finite()),
            city: true,
            mode: mode_of(traction).into(),
            ..Default::default()
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traction_and_fields() {
        let list: Vec<Value> = serde_json::from_str(r#"[
          {"VehicleNumber":"1381","Line":{"Name":"1","Number":445001,"TractionType":1,"IsBarrierFree":true},"ConnectionId":-685625,
           "DelayMin":1,"NextStopName":"Lékařská fakulta","DestinationName":"Bolevec","Latitude":49.76096,"Longitude":13.36998,
           "Azimuth":48,"Timestamp":"2026-11-19T22:31:36.4853742+01:00","IsAirConditioned":true},
          {"VehicleNumber":"561","Line":{"Name":"16","Number":445016,"TractionType":2,"IsBarrierFree":false},
           "DelayMin":0,"DestinationName":"Bory","Latitude":49.73,"Longitude":13.38,"Azimuth":0,"Timestamp":"2026-11-19T22:31:30+01:00"},
          {"VehicleNumber":"201","Line":{"Name":"30","Number":445030,"TractionType":3},"Latitude":49.7,"Longitude":13.4,"Timestamp":"2026-11-19T21:00:00+01:00"}
        ]"#).unwrap();
        let now = parse_time_ms("2026-11-19T22:31:40+01:00").unwrap();
        let vs = parse_traffic(&list, now);
        assert_eq!(vs.len(), 2, "stará poloha (21:00) se nebere");
        let t = &vs[0];
        assert_eq!((t.id, t.line.as_str(), t.cis.as_str(), t.mode.as_str(), t.dest.as_str()), (1381, "1", "445001", "tram", "Bolevec"));
        assert_eq!((t.delay, t.low, t.ac, t.heading, t.age_sec), (1, Some(true), Some(true), Some(48.0), 3));
        assert_eq!(vs[1].mode, "trolleybus");
    }
}
