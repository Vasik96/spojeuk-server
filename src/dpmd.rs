//! DPMD (Dopravní podnik města Děčína): polohy městských autobusů z mapy https://doprava.dpmdas.cz/.
//! Zdroj: https://tabla.dpmdas.cz/data/TrafficState.js (JavaScript, ne JSON; viz jsl.rs), bez přihlášení.
//! Popis zdroje: https://github.com/Sap1k/obehy/blob/main/docs/sources/dpmd.md
//!
//! - jen vozidla ve službě (parkovaná mají `off`, poslední polohu i týdny starou) a s linkou,
//! - číslo řidiče se nikam nedostane (ani do /api/raw/dpmd),
//! - linka CIS = "515" + veřejná linka (201 → 515201), spoj = `Spoj` z popupu.

use crate::jsl::{self, Js};
use crate::merge::{parse_time_ms, Vehicle};

pub const AGENCY: &str = "Dopravní podnik města Děčína, a.s.";

pub struct Snapshot {
    pub vehicles: Vec<Vehicle>,
    /// /api/raw/dpmd: {"TimeStamp": ..., "buspositions": [vozidla ve službě bez řidiče, tras a popupů]}
    pub raw: Vec<u8>,
    /// TimeStamp ze souboru (kdy ho server vygeneroval), jako UTC ms
    pub stamp_ms: Option<i64>,
    pub in_service: usize,
}

fn header_timestamp(src: &str) -> Option<&str> {
    let k = "\"TimeStamp\":\"";
    let p = src.find(k)?;
    if p > 512 {
        return None;
    }
    let rest = &src[p + k.len()..];
    Some(&rest[..rest.find('"')?])
}

fn is_parked(o: &Js) -> bool {
    o.get("off").map_or(false, |v| v.truthy())
}

fn top(o: &Js, k: &str) -> String {
    o.get(k).map(|v| v.text().trim().to_string()).unwrap_or_default()
}

fn leading_int(s: &str) -> i64 {
    let d: String = s.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    d.parse().unwrap_or(0)
}

fn vehicle_of(o: &Js, now_ms: i64, stamp_ms: Option<i64>) -> Option<Vehicle> {
    let body = o.get("body").map(|b| b.text()).unwrap_or_default();
    let field = |l: &str| jsl::body_field(&body, l);

    let mut line = top(o, "line");
    if line.is_empty() {
        line = field("Linka").unwrap_or_default();
    }
    if line.is_empty() {
        return None;
    }
    let (lat, lng) = jsl::parse_coords(&top(o, "coords"))?;
    if !(48.0..=52.0).contains(&lat) || !(12.0..=19.0).contains(&lng) {
        return None;
    }
    let mut id = leading_int(&top(o, "vehicle"));
    if id == 0 {
        id = leading_int(&field("Autobus").unwrap_or_default());
    }
    let fix = field("Čas")
        .and_then(|s| jsl::cz_datetime_to_iso(&s))
        .and_then(|s| parse_time_ms(&s))
        .or(stamp_ms);
    let age_sec = fix.map_or(0, |t| ((now_ms - t) / 1000).max(0));
    if age_sec > 600 {
        return None;
    }
    // směr z `arrow` se mění i ve stoje, pod pár km/h je to šum
    let speed = field("Rychlost").and_then(|s| jsl::first_number(&s));
    let heading = if speed.map_or(false, |s| s >= 3.0) { top(o, "arrow").parse::<f64>().ok().filter(|h| h.is_finite()) } else { None };
    let cis = if line.bytes().all(|b| b.is_ascii_digit()) { format!("515{}", line) } else { String::new() };
    Some(Vehicle {
        source: "dpmd".into(),
        segment: "city".into(),
        id,
        lat,
        lng,
        line,
        delay: field("Zpoždění").map_or(0, |s| jsl::delay_minutes(&s)),
        age_sec,
        state: 0,
        train: false,
        cis,
        trip: field("Spoj").unwrap_or_default(),
        agency: AGENCY.into(),
        dest: field("Konečná").unwrap_or_default(),
        low: None,
        ac: None,
        heading,
        city: true,
        ..Default::default()
    })
}

fn raw_entry(o: &Js, out: &mut String) {
    let m = match o {
        Js::Obj(m) => m,
        _ => return,
    };
    out.push('{');
    let mut first = true;
    for (k, v) in m {
        if matches!(k.as_str(), "driver" | "shapes" | "header" | "hint") {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        jsl::json_str(k, out);
        out.push(':');
        match (k.as_str(), v) {
            ("body", Js::Str(b)) => jsl::json_str(&jsl::drop_body_field(b, "Řidič"), out),
            _ => jsl::to_json(v, out),
        }
    }
    out.push('}');
}

/// Tělo TrafficState.js → vozidla ve službě. Chyba = soubor nejde přečíst (useknutý, změněný tvar).
pub fn parse(src: &str, now_ms: i64) -> Result<Snapshot, String> {
    let stamp = header_timestamp(src);
    let stamp_ms = stamp.and_then(parse_time_ms);
    let stmts = jsl::parse_statements(src)?;
    let list = stmts
        .iter()
        .find(|(k, _)| k == "buspositions")
        .and_then(|(_, v)| v.as_arr())
        .ok_or_else(|| "chybí buspositions".to_string())?;

    let mut vehicles = Vec::new();
    let mut raw = String::from("{\"TimeStamp\":");
    match stamp {
        Some(s) => jsl::json_str(s, &mut raw),
        None => raw.push_str("null"),
    }
    raw.push_str(",\"buspositions\":[");
    let mut in_service = 0usize;
    for o in list {
        if is_parked(o) {
            continue;
        }
        if in_service > 0 {
            raw.push(',');
        }
        in_service += 1;
        raw_entry(o, &mut raw);
        if let Some(v) = vehicle_of(o, now_ms, stamp_ms) {
            vehicles.push(v);
        }
    }
    raw.push_str("]}");
    Ok(Snapshot { vehicles, raw: raw.into_bytes(), stamp_ms, in_service })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"/* {"TimeStamp":"2026-10-10T12:24:37.371"} */ busstops=[{stopid1:'1'}]; buspositions=[{id:'a',line:'201',vehicle:'834',driver:'1310',coords:'50.78115082N, 14.22463036E',arrow:67,shapes:`<gpx/>`,header:`h`,hint:`x`,body:`<br>Čas: 10.10.2026 12:24:38<br>Řidič: 1310<br>Autobus: 834<br>Linka: 201<br>Spoj: 231<br>Konečná: Nemocnice<br>Rychlost: 5 km/h<br>Zpoždění: 2 min.`},{id:'b',line:'',vehicle:'115',off:1,coords:'50.7N, 14.2E',body:`<br>Čas: 01.10.2026 10:00:00`},{id:'c',line:'204',vehicle:'700',coords:'50.7N, 14.2E',arrow:10,body:`<br>Čas: 10.10.2026 12:24:30<br>Rychlost: 0 km/h<br>Linka: 204<br>Spoj: 230`},{id:'d',line:'209',vehicle:'701',coords:'0N, 0E',body:``}]; datahash='x';"#;

    #[test]
    fn maps_vehicles() {
        let now = parse_time_ms("2026-10-10T12:24:48").unwrap(); // místní čas
        let s = parse(SAMPLE, now).unwrap();
        assert_eq!(s.in_service, 3);
        assert_eq!(s.vehicles.len(), 2); // parkovaný pryč, nulová poloha pryč
        let v = &s.vehicles[0];
        assert_eq!((v.source.as_str(), v.id, v.line.as_str(), v.cis.as_str(), v.trip.as_str(), v.dest.as_str()), ("dpmd", 834, "201", "515201", "231", "Nemocnice"));
        assert_eq!((v.delay, v.age_sec, v.heading), (2, 10, Some(67.0)));
        assert!((v.lat - 50.78115082).abs() < 1e-9 && (v.lng - 14.22463036).abs() < 1e-9);
        let w = &s.vehicles[1];
        assert_eq!((w.id, w.delay, w.heading, w.age_sec, w.dest.as_str()), (700, 0, None, 18, ""));
        assert_eq!(s.stamp_ms, parse_time_ms("2026-10-10T12:24:37.371"));
    }

    #[test]
    fn raw_has_no_driver() {
        let now = parse_time_ms("2026-10-10T12:24:48").unwrap();
        let s = parse(SAMPLE, now).unwrap();
        let raw = String::from_utf8(s.raw).unwrap();
        assert!(raw.starts_with("{\"TimeStamp\":\"2026-10-10T12:24:37.371\",\"buspositions\":[{\"id\":\"a\""));
        assert!(!raw.contains("1310") && !raw.contains("driver") && !raw.contains("gpx") && !raw.contains("Řidič"));
        assert!(raw.contains("Spoj: 231"));
    }

    #[test]
    fn stale_fix_dropped_and_broken_input() {
        let late = parse_time_ms("2026-10-10T13:00:00").unwrap();
        assert_eq!(parse(SAMPLE, late).unwrap().vehicles.len(), 0);
        assert!(parse("busstops=[{a:1", 0).is_err());
        assert!(parse("datahash='x';", 0).is_err());
    }
}
