//! IDOL (Integrovaná doprava Libereckého kraje): polohy autobusů a vlaků z veřejné mapy https://mpvnet.cz/idol/map.
//! Kvůli Česká Lípě a spojům mezi Libereckým a Ústeckým krajem (Šluknovsko, Varnsdorf).
//!
//! Mapa se ptá `POST /idol/map/mapData` s výřezem ({w, s, e, n, zoom, showStops, mapFilterId}); bez přihlášení,
//! stačí Referer / Origin mapy. Výřez je celá ČR (IDOL vrací všechny své vozy, i spoje do Prahy), neomezuje se.
//!
//! Záznam vozidla (`transmitters[]`):
//! - `t`: "H" autobus, "T" vlak; `x` / `y` = zeměpisná šířka / délka; `cr` = dopravce,
//! - `dm` = odchylka v minutách, `ds` = text odchylky ("$z…" zpoždění, "$p…" náskok),
//! - `dt` + `tm` = čas polohy (pražský místní), `ico` = "a_<azimut>.png" nebo "a_stoji.png",
//! - `cn` autobus: "linkaCIS|linka|spoj|posl. zastávka|…|příští zastávka|…|…|výchozí|cílová|…|evidenční číslo|…",
//! - `cn` vlak: "Os 6327|L6|číslo vlaku|posl. stanice|…|||||cílová|…|", `n` = "Os 6327".
//!
//! Autobusy mají linku a číslo spoje podle CIS (appka podle nich najde spoj v jízdním řádu), vozy se identifikují
//! evidenčním číslem. Vlaky se se SŽ (a PID) sloučí podle čísla vlaku, IDOL tak doplní vlaky mimo výřez SŽ.

use crate::jsl;
use crate::merge::{parse_time_ms, Vehicle};
use serde_json::Value;

pub struct Snapshot {
    pub vehicles: Vec<Vehicle>,
    /// /api/raw/idol: odpověď mapy tak, jak přišla (jen vozidla a čas, bez nastavení mapy)
    pub raw: Vec<u8>,
}

/// Starší poloha se nebere (vlaky hlásí polohu ve stanici, mezi stanicemi bývá i desítky minut stará).
const MAX_AGE_S: i64 = 600;

fn heading_of(ico: &str) -> Option<f64> {
    let deg = ico.strip_prefix("a_")?.strip_suffix(".png")?;
    deg.parse::<f64>().ok().filter(|h| h.is_finite() && (0.0..=360.0).contains(h))
}

/// Tramvaj: IDOL rozlišuje jen vlak / silniční vozidlo. Tramvaje v Libereckém kraji jezdí jen u DPMLJ (Dopravní podnik
/// měst Liberce a Jablonce n. N.) a mají linky pod 12 (2, 3, 5 v Liberci, 11 do Jablonce); autobusy DPMLJ mají vyšší
/// čísla. Odhad podle dopravce a linky, ověřit na /api/raw/idol přes den.
fn is_tram(carrier: &str, line: &str) -> bool {
    let c = carrier.to_lowercase();
    let dpmlj = c.contains("liberce a jablonce") || c.contains("dpmlj");
    dpmlj && line.parse::<u32>().map_or(false, |n| n < 12)
}

/// Městská doprava (MHD / MAD): podle názvu dopravce, jinak regionální.
fn is_city(carrier: &str) -> bool {
    let c = carrier.to_uppercase();
    c.contains("MHD") || c.contains("MAD") || c.contains("DOPRAVNÍ PODNIK")
}

fn vehicle_of(o: &Value, now_ms: i64) -> Option<Vehicle> {
    let s = |k: &str| o.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let lat = o.get("x")?.as_f64()?;
    let lng = o.get("y")?.as_f64()?;
    if !(48.0..=52.0).contains(&lat) || !(12.0..=19.0).contains(&lng) {
        return None;
    }
    let fix = jsl::cz_datetime_to_iso(&format!("{} {}", s("dt"), s("tm"))).and_then(|iso| parse_time_ms(&iso));
    let age_sec = fix.map_or(0, |t| ((now_ms - t) / 1000).max(0));
    if age_sec > MAX_AGE_S {
        return None;
    }
    let dm = o.get("dm").and_then(|v| v.as_i64()).unwrap_or(0);
    let delay = if s("ds").starts_with("$p") { -dm.abs() } else { dm };
    let carrier = s("cr");
    let cn = s("cn");
    let f: Vec<&str> = cn.split('|').map(|x| x.trim()).collect();
    let at = |i: usize| f.get(i).copied().unwrap_or("").to_string();
    let heading = heading_of(&s("ico"));

    match s("t").as_str() {
        "T" => {
            let number = at(2).trim_start_matches('0').to_string();
            let name = s("n");
            if number.is_empty() && name.is_empty() {
                return None;
            }
            Some(Vehicle {
                source: "idol".into(),
                segment: "regional".into(),
                lat,
                lng,
                line: if name.is_empty() { at(1) } else { name },
                delay,
                age_sec,
                train: true,
                cis: number,
                agency: carrier,
                dest: at(9),
                heading,
                mode: "train".into(),
                ..Default::default()
            })
        }
        "H" => {
            let line = at(1);
            let plate = at(11);
            if line.is_empty() {
                return None;
            }
            let cis_line = at(0);
            let tram = is_tram(&carrier, &line);
            let city = tram || is_city(&carrier);
            Some(Vehicle {
                source: "idol".into(),
                segment: if city { "city" } else { "regional" }.into(),
                lat,
                lng,
                line,
                delay,
                age_sec,
                cis: if !cis_line.is_empty() && cis_line.bytes().all(|b| b.is_ascii_digit()) { cis_line } else { String::new() },
                trip: at(2),
                agency: carrier,
                dest: at(9),
                heading,
                city,
                mode: if tram { "tram" } else { "bus" }.into(),
                // identita vozu: evidenční číslo (uid "l<číslo>", viz merge::uid_of)
                vkey: plate,
                ..Default::default()
            })
        }
        _ => None,
    }
}

/// Odpověď mapData → vozidla. Chyba = jiný tvar odpovědi (chybí `transmitters`).
pub fn parse(body: &str, now_ms: i64) -> Result<Snapshot, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("JSON: {e}"))?;
    let list = v.get("transmitters").and_then(|t| t.as_array()).ok_or_else(|| "chybí transmitters".to_string())?;
    let vehicles = list.iter().filter_map(|o| vehicle_of(o, now_ms)).collect();
    let raw = serde_json::json!({ "transmitters": list }).to_string().into_bytes();
    Ok(Snapshot { vehicles, raw })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_for(dt: &str, tm: &str, plus_s: i64) -> i64 {
        parse_time_ms(&jsl::cz_datetime_to_iso(&format!("{dt} {tm}")).unwrap()).unwrap() + plus_s * 1000
    }

    #[test]
    fn bus_and_train() {
        let body = r#"{"transmitters":[
          {"t":"H","c":0,"dm":2,"ds":"$z2~min.~05~s","cr":"BusLine MAD Česká Lípa","x":50.70249,"y":14.53849,"dt":"19.11.2026","tm":"22:06:16","ico":"a_270.png",
           "cn":"505202|202|72|Česká Lípa,Šluknovská|-5|Česká Lípa,K Rozvodně|0|1327|Česká Lípa,Hlavní nádraží|Česká Lípa,Sídliště Lada|1|5L58208|0|0","cat":2},
          {"t":"T","c":681,"dm":4,"ds":"$z4~min.~00~s","n":"Os 6329","cr":"České dráhy","x":50.890934,"y":15.139472,"dt":"19.11.2026","tm":"22:06:00","ico":"a_stoji.png",
           "cn":"Os 6329|L6|6329|Raspenava|240|||||Liberec|1|","cat":1},
          {"t":"H","dm":0,"ds":"$p-1~min.","cr":"ČSAD Liberec","x":50.6,"y":14.5,"dt":"19.11.2026","tm":"21:00:00","ico":"a_0.png","cn":"500260|260|57|A|0|B|0|0|C|D|1|6L1|0"}
        ]}"#;
        let now = now_for("19.11.2026", "22:06:20", 0);
        let s = parse(body, now).unwrap();
        assert_eq!(s.vehicles.len(), 2, "stará poloha (21:00) se nebere");
        let b = &s.vehicles[0];
        assert_eq!((b.line.as_str(), b.cis.as_str(), b.trip.as_str(), b.dest.as_str()), ("202", "505202", "72", "Česká Lípa,Sídliště Lada"));
        assert_eq!((b.vkey.as_str(), b.delay, b.heading, b.city, b.age_sec), ("5L58208", 2, Some(270.0), true, 4));
        let t = &s.vehicles[1];
        assert!(t.train);
        assert_eq!((t.line.as_str(), t.cis.as_str(), t.dest.as_str(), t.heading), ("Os 6329", "6329", "Liberec", None));
    }

    #[test]
    fn liberec_tram_by_carrier_and_line() {
        assert!(is_tram("Dopravní podnik měst Liberce a Jablonce nad Nisou, a.s.", "3"));
        assert!(is_tram("DPMLJ", "11"));
        assert!(!is_tram("Dopravní podnik měst Liberce a Jablonce nad Nisou, a.s.", "24"));
        assert!(!is_tram("BusLine MAD Česká Lípa", "3"));
    }

    #[test]
    fn early_is_negative() {
        let body = r#"{"transmitters":[{"t":"H","dm":1,"ds":"$p-1~min.","cr":"X","x":50.6,"y":14.5,"dt":"19.11.2026","tm":"22:00:00","ico":"a_0.png","cn":"500260|260|57|A|0|B|0|0|C|D|1|6L1|0"}]}"#;
        let s = parse(body, now_for("19.11.2026", "22:00:30", 0)).unwrap();
        assert_eq!(s.vehicles[0].delay, -1);
    }
}
