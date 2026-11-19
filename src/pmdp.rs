//! PMDP (Plzeňské městské dopravní podniky): tramvaje, trolejbusy a autobusy MHD Plzeň z mapy
//! https://jizdnirady.pmdp.cz/provoz. Bez přihlášení.
//!
//! Mapa dostává stav přes klasický ASP.NET SignalR (hub `provozHub`): klient se zaregistruje metodou
//! `registrace(null)` (null = všechny linky) a server posílá `updateTraffic` se všemi vozy zhruba po 2–8 s.
//! Server se připojuje transportem longPolling (obyčejné HTTP dotazy, viz main.rs pmdp_session), tady je jen rozbor.
//!
//! Vůz: VehicleNumber, Line {Name, Number (linka CIS, např. 445001), TractionType (1 tramvaj, 2 trolejbus,
//! 3 autobus), IsBarrierFree}, DelayMin, NextStopName, DestinationName, Latitude, Longitude, Azimuth,
//! Timestamp (ISO s časovou zónou), IsAirConditioned, ConnectionId (spoj; jde do `trip`).
//!
//! Trasa spoje: `POST /provoz/trasaspoje` {SegmentId: {ConnectionId}, IncludeDetailedPath: true} s cookie relace
//! a tokenem X-API-Request-Verification-Token ze stránky /provoz (viz main.rs pmdp_trip). PMDP data nejsou
//! v celostátním jízdním řádu pod stejnými čísly, takže appka i web berou zastávky a tvar trasy odtud.

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
        let conn = o.get("ConnectionId").and_then(|v| v.as_i64()).map(|c| c.to_string()).unwrap_or_default();
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
            trip: conn,
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

/// "2000-01-01T22:15:00+01:00" → sekundy od půlnoci (datum je u PMDP vždy fiktivní, platí jen čas).
fn sec_of_day(t: &str) -> Option<i64> {
    let hms = t.split('T').nth(1)?;
    let mut it = hms.split(|c| c == ':' || c == '+' || c == '-' || c == '.' || c == 'Z');
    let h: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let s: i64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    Some(h * 3600 + m * 60 + s)
}

/// Odpověď trasaspoje → stejný tvar jako /api/vehicle-trip webu a appky:
/// {line, head, agency, isTrain, mode, stops: [[název, lat, lng, příjezd, odjezd]], route: [[lat, lng]]}.
/// Časy v sekundách od půlnoci; spoj přes půlnoc pokračuje za 86400.
pub fn parse_path(v: &Value) -> Result<Value, String> {
    let line = v.get("Line").ok_or("chybí Line")?;
    let name = line.get("Name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let traction = line.get("TractionType").and_then(|x| x.as_i64()).unwrap_or(3);
    let list = v.get("Stops").and_then(|x| x.as_array()).ok_or("chybí Stops")?;
    let mut stops = Vec::with_capacity(list.len());
    let mut prev = -1i64;
    let mut shift = 0i64;
    let mut fix = |t: Option<i64>, prev: &mut i64| -> Option<i64> {
        let mut t = t? + shift;
        if *prev >= 0 && t + 12 * 3600 < *prev {
            shift += 86400;
            t += 86400;
        }
        *prev = t;
        Some(t)
    };
    for st in list {
        let pl = st.get("Placement");
        let lat = pl.and_then(|p| p.get("Latitude")).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let lng = pl.and_then(|p| p.get("Longitude")).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let t = |k: &str| st.get(k).and_then(|x| x.as_str()).and_then(sec_of_day);
        let (a, d) = (t("ArrivalTime"), t("DepartureTime"));
        let arr = fix(a.or(d), &mut prev).unwrap_or(-1);
        let dep = if d.is_some() && a.is_some() { fix(d, &mut prev).unwrap_or(arr) } else { arr };
        let nm = st.get("Name").and_then(|x| x.as_str()).unwrap_or("").trim();
        stops.push(serde_json::json!([nm, lat, lng, arr, dep]));
    }
    let route: Vec<Value> = v
        .get("DetailedPath")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| Some(serde_json::json!([p.get("Latitude")?.as_f64()?, p.get("Longitude")?.as_f64()?])))
                .collect()
        })
        .unwrap_or_default();
    let head = list.last().and_then(|s| s.get("Name")).and_then(|x| x.as_str()).unwrap_or("").trim();
    Ok(serde_json::json!({
        "line": name,
        "head": head,
        "agency": AGENCY,
        "isTrain": false,
        "mode": mode_of(traction),
        "stops": stops,
        "route": if route.len() >= 2 { Value::Array(route) } else { Value::Null },
    }))
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
        assert_eq!(t.trip, "-685625");
    }

    #[test]
    fn path_times_and_shape() {
        let v: Value = serde_json::from_str(r#"{"Line":{"Name":"1","Number":445001,"TractionType":1},
          "Stops":[
            {"Placement":{"Latitude":49.72,"Longitude":13.39},"Name":"Slovany","ArrivalTime":null,"DepartureTime":"2000-01-01T23:58:00+01:00"},
            {"Placement":{"Latitude":49.73,"Longitude":13.38},"Name":"Náměstí Republiky","ArrivalTime":"2000-01-01T23:59:00+01:00","DepartureTime":"2000-01-01T23:59:30+01:00"},
            {"Placement":{"Latitude":49.78,"Longitude":13.36},"Name":"Bolevec","ArrivalTime":"2000-01-01T00:10:00+01:00","DepartureTime":null}],
          "DetailedPath":[{"Latitude":49.72,"Longitude":13.39},{"Latitude":49.75,"Longitude":13.37},{"Latitude":49.78,"Longitude":13.36}]}"#).unwrap();
        let r = parse_path(&v).unwrap();
        assert_eq!(r["mode"], "tram");
        assert_eq!(r["head"], "Bolevec");
        assert_eq!(r["stops"][0], serde_json::json!(["Slovany", 49.72, 13.39, 86280, 86280]));
        assert_eq!(r["stops"][1][3], 86340);
        assert_eq!(r["stops"][1][4], 86370);
        assert_eq!(r["stops"][2][3], 86400 + 600, "přes půlnoc");
        assert_eq!(r["route"].as_array().unwrap().len(), 3);
    }
}
