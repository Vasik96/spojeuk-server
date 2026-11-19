//! Sloučení zdrojů poloh. Port z webu: lib/fusion.mjs, lib/hub.mjs, normalize() z lib/feed.mjs
//! a parseDpmul() z lib/sources.mjs. Výstup /api/vehicles má stejný tvar jako /api/vehicles v Node serveru.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

const SAME_SPOT_M: f64 = 150.0;
const M_PER_DEG_LAT: f64 = 110540.0;
const M_PER_DEG_LNG: f64 = 111320.0;
const DPMUL_AGENCY: &str = "Dopravní podnik města Ústí nad Labem";

// ---------------------------------------------------------------- model

#[derive(Clone, Default, Serialize, Debug)]
pub struct Vehicle {
    pub source: String,
    pub segment: String,
    pub id: i64,
    pub lat: f64,
    pub lng: f64,
    pub line: String,
    pub delay: i64,
    #[serde(rename = "ageSec")]
    pub age_sec: i64,
    pub state: i64,
    pub train: bool,
    pub cis: String,
    pub trip: String,
    pub agency: String,
    pub dest: String,
    pub low: Option<bool>,
    pub ac: Option<bool>,
    pub heading: Option<f64>,
    pub city: bool,
    /// DPMÚL: číslo aktuální a konečné stanice (pomáhá appce přiřadit vůz ke spoji)
    pub station: i64,
    #[serde(rename = "finalStation")]
    pub final_station: i64,
    pub sources: Vec<String>,
    pub ids: Vec<String>,
    pub uid: String,
    /// Druh dopravy pro appku (barva na mapě): bus | tram | metro | trolleybus | train | ferry | funicular.
    /// PID ho zná z GTFS route_type, ostatní zdroje dostanou bus / train v [fuse].
    pub mode: String,
    /// PID: klíč vozidla "<route_type>:<číslo vozu>" / "train:<číslo vlaku>" / "trip:<gtfs spoj>" (nejde do výstupu)
    #[serde(skip)]
    pub vkey: String,
}

pub struct SourceInput {
    pub id: &'static str,
    pub name: &'static str,
    pub segments: &'static [&'static str],
    pub health: &'static str, // live | stale | down
    pub stale_vehicle_sec: i64,
    pub vehicles: Vec<Vehicle>,
    pub held: usize,
    pub failures: u64,
    pub last_error: String,
}

pub fn health_of(last_ok: f64, now: f64, max_age_s: f64) -> &'static str {
    if last_ok <= 0.0 {
        "down"
    } else if now - last_ok <= max_age_s {
        "live"
    } else {
        "stale"
    }
}

/// Obdélník (lat_min, lng_min, lat_max, lng_max) pro omezení zdroje; None = bez omezení.
pub type Bbox = [f64; 4];

/// "49.15,12.8,51.1,15.4" → Bbox; nesmyslná hodnota = None.
pub fn parse_bbox(s: &str) -> Option<Bbox> {
    let p: Vec<f64> = s.split(',').filter_map(|x| x.trim().parse::<f64>().ok()).collect();
    if p.len() == 4 && s.split(',').count() == 4 && p[0] < p[2] && p[1] < p[3] {
        Some([p[0], p[1], p[2], p[3]])
    } else {
        None
    }
}

pub fn in_bbox(b: Option<&Bbox>, lat: f64, lng: f64) -> bool {
    b.map_or(true, |b| lat >= b[0] && lng >= b[1] && lat <= b[2] && lng <= b[3])
}

pub fn is_mhd(vid: i64) -> bool {
    (300000..=309999).contains(&vid)
}

// ---------------------------------------------------------------- JS-like pomocníci

pub fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f.fract() == 0.0 && f.abs() < 1e15 { (f as i64).to_string() } else { f.to_string() }
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        _ => String::new(),
    }
}

/// `String(v ?? '')`
pub fn str_or_empty(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(x) => js_string(x),
    }
}

/// `Number(v)` (NaN když nejde převést)
pub fn js_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => {
            if *b { 1.0 } else { 0.0 }
        }
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() { 0.0 } else { t.parse::<f64>().unwrap_or(f64::NAN) }
        }
        _ => f64::NAN,
    }
}

/// `Number(v) || 0`
pub fn num_or0(v: Option<&Value>) -> f64 {
    let n = js_number(v);
    if n.is_nan() { 0.0 } else { n }
}

pub fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map_or(false, |f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `parseInt(String(v), 10) || 0`
pub fn js_parse_int(v: Option<&Value>) -> i64 {
    let s = match v {
        None | Some(Value::Null) => return 0,
        Some(x) => js_string(x),
    };
    let t = s.trim_start();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: i64 = digits.parse().unwrap_or(0);
    if neg { -n } else { n }
}

fn opt_bool(v: Option<&Value>) -> Option<bool> {
    match v {
        None | Some(Value::Null) => None,
        other => Some(js_truthy(other)),
    }
}

/// feed.mjs clean(): null / 'null' / prázdné → None
fn clean(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(x) => {
            let s = js_string(x);
            if s == "null" || s.trim().is_empty() { None } else { Some(s.trim().to_string()) }
        }
    }
}

fn js_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// Malá písmena bez diakritiky ("Ústí" → "usti").
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' | 'ą' => 'a',
            'č' | 'ć' | 'ç' => 'c',
            'ď' | 'đ' => 'd',
            'é' | 'è' | 'ê' | 'ë' | 'ě' | 'ē' | 'ę' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ľ' | 'ĺ' | 'ł' => 'l',
            'ň' | 'ń' | 'ñ' => 'n',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ő' => 'o',
            'ř' | 'ŕ' => 'r',
            'š' | 'ś' => 's',
            'ť' => 't',
            'ú' | 'ù' | 'û' | 'ü' | 'ů' | 'ű' => 'u',
            'ý' | 'ÿ' => 'y',
            'ž' | 'ź' | 'ż' => 'z',
            o => o,
        })
        .collect()
}

/// Linka pro porovnávání: "070" == "70".
pub fn norm_line(s: &str) -> String {
    let t = s.trim();
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        let z = t.trim_start_matches('0');
        if z.is_empty() { "0".to_string() } else { z.to_string() }
    } else {
        fold(t)
    }
}

pub fn distance_m(la1: f64, lo1: f64, la2: f64, lo2: f64) -> f64 {
    ((lo2 - lo1) * M_PER_DEG_LNG * (la1 * std::f64::consts::PI / 180.0).cos()).hypot((la2 - la1) * M_PER_DEG_LAT)
}

fn bearing(la1: f64, lo1: f64, la2: f64, lo2: f64) -> f64 {
    let (f1, f2, dl) = (la1.to_radians(), la2.to_radians(), (lo2 - lo1).to_radians());
    let y = dl.sin() * f2.cos();
    let x = f1.cos() * f2.sin() - f1.sin() * f2.cos() * dl.cos();
    (y.atan2(x).to_degrees() + 360.0) % 360.0
}

// ---------------------------------------------------------------- čas (Europe/Prague bez knihoven)

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Posun Europe/Prague od UTC v sekundách (letní čas: poslední neděle března až října, 01:00 UTC).
fn prague_offset(utc_s: i64) -> i64 {
    let (y, _, _) = civil_from_days(utc_s.div_euclid(86400));
    let last_sun = |m: i64| -> i64 {
        let d31 = days_from_civil(y, m, 31);
        let wd = (d31 + 4).rem_euclid(7); // 0 = neděle
        days_from_civil(y, m, 31 - wd)
    };
    let start = last_sun(3) * 86400 + 3600;
    let end = last_sun(10) * 86400 + 3600;
    if utc_s >= start && utc_s < end { 7200 } else { 3600 }
}

/// Místní pražský čas zapsaný jako "UTC" (ms) → skutečné UTC ms.
fn prague_local_to_utc_ms(naive_ms: i64) -> i64 {
    let secs = naive_ms.div_euclid(1000);
    let t = secs - prague_offset(secs);
    let t = secs - prague_offset(t);
    t * 1000 + naive_ms.rem_euclid(1000)
}

/// "2026-11-10T22:15:03[.123][Z|±HH:MM|±HHMM|±HH]" → ms od epochy. Bez zóny = pražský místní čas.
pub fn parse_time_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || !s.is_ascii() {
        return None;
    }
    let num = |from: usize, len: usize| -> Option<i64> {
        let mut n = 0i64;
        for &c in &b[from..from + len] {
            if !c.is_ascii_digit() {
                return None;
            }
            n = n * 10 + (c - b'0') as i64;
        }
        Some(n)
    };
    if b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, sec) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let mut i = 19;
    let mut frac_ms = 0i64;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let (mut digits, mut ms) = (0, 0i64);
        let st = i;
        while i < b.len() && b[i].is_ascii_digit() {
            if digits < 3 {
                ms = ms * 10 + (b[i] - b'0') as i64;
                digits += 1;
            }
            i += 1;
        }
        if i == st {
            return None;
        }
        while digits < 3 {
            ms *= 10;
            digits += 1;
        }
        frac_ms = ms;
    }
    let naive = (days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec) * 1000 + frac_ms;
    let rest = &s[i..];
    if rest.is_empty() {
        return Some(prague_local_to_utc_ms(naive));
    }
    if rest == "Z" || rest == "z" {
        return Some(naive);
    }
    let sign = match rest.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let z: String = rest[1..].chars().filter(|c| *c != ':').collect();
    if !z.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (zh, zm) = match z.len() {
        2 => (z.parse::<i64>().ok()?, 0),
        4 => (z[..2].parse::<i64>().ok()?, z[2..].parse::<i64>().ok()?),
        _ => return None,
    };
    Some(naive - sign * (zh * 3600 + zm * 60) * 1000)
}

// ---------------------------------------------------------------- DÚK

/// Stáří GPS polohy v okamžiku pozorování (z LastActivityDT), 0 když chybí.
pub fn gps_age_sec(e: &Value, now_s: f64) -> i64 {
    match e.get("LastActivityDT").and_then(|v| parse_time_ms(&js_string(v))) {
        Some(ms) => (((now_s * 1000.0) as i64 - ms) / 1000).max(0),
        None => 0,
    }
}

/// Surový záznam Portabo → vozidlo, nebo None když je nepoužitelný. `age_sec` = celkové stáří polohy.
pub fn duk_vehicle(e: &Value, age_sec: i64) -> Option<Vehicle> {
    let lat = js_number(e.get("Latitude"));
    let lng = js_number(e.get("Longitude"));
    let state = match e.get("State") {
        None | Some(Value::Null) => 255.0,
        s => js_number(s),
    };
    if state == 255.0 || lat.is_nan() || lng.is_nan() || lat == 0.0 || lng == 0.0 {
        return None;
    }
    let id = num_or0(e.get("ID")) as i64;
    let train = (20000..=29999).contains(&id);
    let route_id = clean(e.get("RouteID")).unwrap_or_default();
    let mut line = clean(e.get("qride_linename")).or_else(|| clean(e.get("LineID"))).unwrap_or_else(|| "?".into());
    if line == "200" {
        line = "X".into();
    }
    let agency = clean(e.get("agency")).or_else(|| clean(e.get("agency_name"))).unwrap_or_default();
    let dest = clean(e.get("headsign"))
        .or_else(|| clean(e.get("destination")))
        .or_else(|| clean(e.get("last_stop")))
        .unwrap_or_default();
    let city = is_mhd(id);
    let fa = fold(&agency);
    let segment = if train {
        "regional"
    } else if city || fa.contains("dopravni podnik mesta usti") || fa.contains("dpmul") {
        "city"
    } else {
        "regional"
    };
    Some(Vehicle {
        source: "duk".into(),
        segment: segment.into(),
        id,
        lat,
        lng,
        line,
        delay: num_or0(e.get("Delay")).round() as i64,
        age_sec,
        state: state as i64,
        train,
        cis: if train { route_id.clone() } else { format!("{:0>6}", js_parse_int(e.get("CISLineID")).to_string()) },
        trip: if train { String::new() } else { route_id },
        agency,
        dest,
        low: opt_bool(e.get("HasLowfloor")),
        ac: opt_bool(e.get("isAirConditioned")),
        heading: None,
        city,
        ..Default::default()
    })
}

// ---------------------------------------------------------------- DPMÚL

pub fn parse_delay_minutes(v: Option<&Value>) -> i64 {
    let s = match v {
        None | Some(Value::Null) => String::new(),
        Some(x) => js_string(x),
    };
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let p: Vec<&str> = body.split(':').collect();
    let all_digits = |x: &str| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit());
    if p.len() == 3 && (1..=2).contains(&p[0].len()) && p[1].len() == 2 && p[2].len() == 2 && p.iter().all(|x| all_digits(x)) {
        let total = p[0].parse::<i64>().unwrap_or(0) * 3600 + p[1].parse::<i64>().unwrap_or(0) * 60 + p[2].parse::<i64>().unwrap_or(0);
        return js_round((if neg { -total } else { total }) as f64 / 60.0);
    }
    // parseFloat: nejdelší číselný prefix
    let t = s.replace(',', ".");
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    match t[..i].parse::<f64>() {
        Ok(n) if i > ds && n.is_finite() => js_round(n),
        _ => 0,
    }
}

/// Odpověď DPMÚL (pole objektů) → vozidla. `stations` = naučené polohy stanic (stationNumber → (lat, lng)).
pub fn parse_dpmul(arr: &[Value], now_ms: i64, stations: &mut HashMap<i64, (f64, f64)>) -> Vec<Vehicle> {
    let mut out = Vec::new();
    for o in arr {
        let (lat_v, lng_v) = (o.get("latitude"), o.get("longitude"));
        if matches!(lat_v, None | Some(Value::Null)) || matches!(lng_v, None | Some(Value::Null)) {
            continue;
        }
        let (lat, lng) = (js_number(lat_v), js_number(lng_v));
        if !(lat >= 48.0 && lat <= 52.0 && lng >= 12.0 && lng <= 19.0) {
            continue;
        }
        let mut observed = if js_truthy(o.get("lastActivity")) { o.get("lastActivity").and_then(|v| parse_time_ms(&js_string(v))) } else { None };
        if observed.is_none() {
            observed = o.get("gpsTimestamp").and_then(|v| parse_time_ms(&js_string(v)));
        }
        if let Some(ob) = observed {
            if now_ms - ob > 10 * 60_000 {
                continue;
            }
        }
        let station = num_or0(o.get("stationNumber")) as i64;
        let at_stop = js_truthy(o.get("arrivedAt")) || js_truthy(o.get("stoppedAt"));
        let mut heading = None;
        if at_stop {
            if station != 0 {
                stations.insert(station, (lat, lng));
            }
        } else if let Some(&(slat, slng)) = stations.get(&station) {
            if station != 0 && distance_m(lat, lng, slat, slng) > 30.0 {
                if js_truthy(o.get("inTransitTo")) || js_truthy(o.get("incomingAt")) {
                    heading = Some(bearing(lat, lng, slat, slng));
                } else if js_truthy(o.get("leftAt")) {
                    heading = Some(bearing(slat, slng, lat, lng));
                }
            }
        }
        let mut line = match o.get("lineName") {
            None | Some(Value::Null) => "?".to_string(),
            Some(x) => js_string(x).trim().to_string(),
        };
        if line.is_empty() {
            line = "?".into();
        }
        if line == "200" {
            line = "X".into();
        }
        out.push(Vehicle {
            source: "dpmul".into(),
            segment: "city".into(),
            id: num_or0(o.get("vehicleNumber")) as i64,
            lat,
            lng,
            line,
            delay: parse_delay_minutes(o.get("delay")),
            age_sec: observed.map_or(0, |ob| ((now_ms - ob) / 1000).max(0)),
            state: 0,
            train: false,
            agency: DPMUL_AGENCY.into(),
            low: opt_bool(o.get("isWheelchairAccessible")),
            ac: opt_bool(o.get("isAirConditioned")),
            heading,
            city: true,
            station,
            final_station: num_or0(o.get("finalStationNumber")) as i64,
            ..Default::default()
        });
    }
    out
}

// ---------------------------------------------------------------- slučování (fusion.mjs)

/// Základní číslo ústeckého vozu bez předpony 30 a nul (300451 → 451, 301212 → 1212, 451 → 451).
pub fn base_vehicle_id(id: i64) -> i64 {
    if id <= 0 {
        return 0;
    }
    let s = id.to_string();
    if s.len() >= 4 && s.starts_with("30") {
        let rest = s[2..].trim_start_matches('0');
        if let Ok(base) = rest.parse::<i64>() {
            if !rest.is_empty() && base > 0 {
                return base;
            }
        }
    }
    id
}

pub fn same_vehicle_id(a: i64, b: i64) -> bool {
    a > 0 && b > 0 && (a == b || base_vehicle_id(a) == base_vehicle_id(b))
}

fn train_no(v: &Vehicle) -> String {
    if v.train { v.cis.trim().trim_start_matches('0').to_string() } else { String::new() }
}

fn same_vehicle(a: &Vehicle, b: &Vehicle) -> bool {
    // Děčínské vozy (DPMD) mají vlastní číslování, které se s ústeckým překrývá (834 je v obou městech)
    // a žádný jiný zdroj je nezná: nikdy se neslučují s ničím jiným.
    let own_city = |v: &Vehicle| matches!(v.source.as_str(), "dpmd" | "dpkv" | "pmdp");
    if own_city(a) || own_city(b) {
        return false;
    }
    // IDOL (Liberecký kraj): autobusy mají vlastní identitu (evidenční číslo) a v Libereckém kraji jiný zdroj nejezdí,
    // neslučují se; vlaky IDOL se párují s ostatními podle čísla vlaku níž.
    if (a.source == "idol" && !a.train) || (b.source == "idol" && !b.train) {
        return false;
    }
    // PID číslo vozu je z jiné číselné řady než DÚK (a mezi druhy dopravy se opakuje): u autobusů se s jiným zdrojem
    // páruje podle linky a polohy. Vlaky se párují číslem vlaku jako vždy (PID, SŽ i DÚK ho mají v `cis`).
    let pid_pair = a.source == "pid" || b.source == "pid";
    if a.train || b.train {
        let (na, nb) = (train_no(a), train_no(b));
        if !na.is_empty() && na == nb {
            return true;
        }
        if !na.is_empty() && !nb.is_empty() {
            return false; // jiné číslo vlaku = jiný vlak
        }
    } else if a.id != 0 && b.id != 0 && !pid_pair {
        return same_vehicle_id(a.id, b.id);
    }
    if norm_line(&a.line) != norm_line(&b.line) {
        return false;
    }
    let age_diff = (a.age_sec - b.age_sec).abs().min(120) as f64;
    distance_m(a.lat, a.lng, b.lat, b.lng) <= SAME_SPOT_M + age_diff * 15.0 // autobus ujede až ~15 m/s
}

macro_rules! pick_str {
    ($g:expr, $wi:expr, $f:ident) => {{
        let w = &$g[$wi].$f;
        if !w.is_empty() {
            w.clone()
        } else {
            $g.iter().enumerate().filter(|(i, _)| *i != $wi).map(|(_, o)| &o.$f).find(|s| !s.is_empty()).cloned().unwrap_or_default()
        }
    }};
}
macro_rules! pick_opt {
    ($g:expr, $wi:expr, $f:ident) => {{
        if $g[$wi].$f.is_some() {
            $g[$wi].$f
        } else {
            $g.iter().enumerate().filter(|(i, _)| *i != $wi).find_map(|(_, o)| o.$f)
        }
    }};
}

/// Vítěz skupiny: zdroj s nejčerstvější polohou (nejnižší `age_sec`). Žádná pevná priorita: u každého vozu
/// rozhoduje, který zdroj ho právě hlásí nejnověji. Při přesné shodě stáří vyhrává první ve skupině
/// (skupina je řazená podle id zdroje, takže výsledek je deterministický a nepřeskakuje mezi voláními).
fn resolve(group: Vec<Vehicle>) -> Vehicle {
    let mut wi = 0usize;
    for bi in 1..group.len() {
        if group[bi].age_sec < group[wi].age_sec {
            wi = bi;
        }
    }
    let cis = pick_str!(group, wi, cis);
    let trip = pick_str!(group, wi, trip);
    let dest = pick_str!(group, wi, dest);
    let low = pick_opt!(group, wi, low);
    let ac = pick_opt!(group, wi, ac);
    let heading = pick_opt!(group, wi, heading);
    let station = if group[wi].station != 0 { group[wi].station } else { group.iter().map(|v| v.station).find(|s| *s != 0).unwrap_or(0) };
    let final_station = if group[wi].final_station != 0 {
        group[wi].final_station
    } else {
        group.iter().map(|v| v.final_station).find(|s| *s != 0).unwrap_or(0)
    };
    let train = group.iter().any(|v| v.train);
    let mode = if train { "train".to_string() } else { pick_str!(group, wi, mode) };
    let sources: Vec<String> = group.iter().map(|v| v.source.clone()).collect();
    let ids: Vec<String> = group.iter().filter(|v| v.id != 0).map(|v| format!("{}:{}", v.source, v.id)).collect();
    let mut w = group[wi].clone();
    w.train = train;
    w.mode = mode;
    w.cis = cis;
    w.trip = trip;
    w.dest = dest;
    w.low = low;
    w.ac = ac;
    w.heading = heading;
    w.station = station;
    w.final_station = final_station;
    w.sources = sources;
    w.ids = ids;
    // PID autobus sloučený s DÚK: uid dává ten druhý zdroj, aby se nezměnilo podle toho, kdo zrovna vyhrál
    if !train && group.iter().any(|v| v.source == "pid") {
        if let Some(o) = group.iter().find(|v| v.source != "pid") {
            w.uid = uid_of(o);
        }
    }
    w
}

/// Pražské vozy PID (tramvaj, metro, přívoz, trolejbus, pražský autobus) žádný jiný zdroj nezná: neseskupují se.
fn standalone(v: &Vehicle) -> bool {
    v.source == "pid" && !v.train && v.segment == "city"
}

pub fn fuse(inputs: &[SourceInput]) -> Vec<Vehicle> {
    // pořadí zdrojů slouží jen k deterministickému řazení skupin a ids (ne k preferenci zdroje)
    let mut ordered: Vec<&SourceInput> = inputs.iter().collect();
    ordered.sort_by_key(|i| i.id);
    let mut out = Vec::new();
    for segment in ["city", "regional"] {
        let mut groups: Vec<Vec<Vehicle>> = Vec::new();
        for input in &ordered {
            for v in &input.vehicles {
                if v.segment != segment || standalone(v) {
                    continue;
                }
                let mut target: Option<usize> = None;
                let mut best = f64::INFINITY;
                for (gi, g) in groups.iter().enumerate() {
                    if g.iter().any(|x| x.source == v.source) || !same_vehicle(&g[0], v) {
                        continue;
                    }
                    let d = distance_m(g[0].lat, g[0].lng, v.lat, v.lng);
                    if d < best {
                        best = d;
                        target = Some(gi);
                    }
                }
                match target {
                    Some(gi) => groups[gi].push(v.clone()),
                    None => groups.push(vec![v.clone()]),
                }
            }
        }
        for input in &ordered {
            if segment != "city" {
                break;
            }
            for v in input.vehicles.iter().filter(|v| standalone(v)) {
                let mut v = v.clone();
                v.sources = vec![v.source.clone()];
                v.ids = if v.id != 0 { vec![format!("{}:{}", v.source, v.id)] } else { vec![] };
                out.push(v);
            }
        }
        for g in groups {
            if g.len() > 1 {
                out.push(resolve(g));
                continue;
            }
            let mut v = g.into_iter().next().unwrap();
            // Vůz, který zná jen jeden zdroj, se zahodí, když je zastaralý a jiný živý zdroj ten segment pro stejný druh
            // vozidel (vlak / autobus) právě čerstvě pokrývá a vůz nevidí. DPMD (vlastní číslování, nikdo jiný tam nejezdí)
            // se nezahazuje nikdy.
            let ghost = v.source != "dpmd"
                && v.source != "dpkv"
                && v.source != "pmdp"
                && !(v.source == "idol" && !v.train)
                && match ordered.iter().find(|i| i.id == v.source) {
                    Some(own) => {
                        v.age_sec > own.stale_vehicle_sec
                            && ordered.iter().any(|o| {
                                o.id != own.id
                                    && o.health == "live"
                                    && o.segments.contains(&segment)
                                    && o.vehicles.iter().any(|x| x.segment == segment && x.train == v.train)
                            })
                    }
                    None => false,
                };
            if !ghost {
                v.sources = vec![v.source.clone()];
                v.ids = if v.id != 0 { vec![format!("{}:{}", v.source, v.id)] } else { vec![] };
                out.push(v);
            }
        }
    }
    for v in &mut out {
        if v.train {
            v.mode = "train".into();
        } else if v.mode.is_empty() {
            v.mode = "bus".into();
        }
    }
    out
}

/// Stabilní klíč vozu napříč zdroji (ID se mezi DPMÚL a DÚK liší předponou 30).
pub fn uid_of(v: &Vehicle) -> String {
    if v.source == "dpmd" && v.id != 0 {
        return format!("d{}", v.id); // vlastní číselná řada, nesmí splynout s "v834" z Ústí
    }
    if v.source == "dpkv" && v.id != 0 {
        return format!("k{}", v.id); // Karlovy Vary: vlastní číselná řada
    }
    if v.source == "pmdp" && v.id != 0 {
        return format!("z{}", v.id); // Plzeň: vlastní číselná řada
    }
    if v.train && !v.cis.is_empty() {
        return format!("t{}", v.cis.trim().trim_start_matches('0'));
    }
    if v.source == "idol" && !v.vkey.is_empty() {
        return format!("l{}", v.vkey); // IDOL: evidenční číslo vozu, vlastní řada
    }
    if v.source == "pid" && !v.vkey.is_empty() {
        return format!("g{}", v.vkey); // PID: číslo vozu platí jen v rámci druhu dopravy, nesmí splynout s "v451" z Ústí
    }
    if v.id != 0 {
        return format!("v{}", base_vehicle_id(v.id));
    }
    format!("p{:.4},{:.4}", v.lat, v.lng)
}

/// Jedno vozidlo = jeden záznam na uid; ponechá se čerstvější poloha, při shodě první.
fn dedupe_by_uid(vs: Vec<Vehicle>) -> Vec<Vehicle> {
    let mut best: HashMap<String, usize> = HashMap::new();
    for (i, v) in vs.iter().enumerate() {
        let replace = match best.get(&v.uid) {
            Some(&j) => v.age_sec < vs[j].age_sec,
            None => true,
        };
        if replace {
            best.insert(v.uid.clone(), i);
        }
    }
    vs.into_iter().enumerate().filter(|(i, v)| best.get(&v.uid) == Some(i)).map(|(_, v)| v).collect()
}

// ---------------------------------------------------------------- výstup /api/vehicles

#[derive(Serialize)]
struct SourceInfo<'a> {
    id: &'a str,
    name: &'a str,
    health: &'a str,
    vehicles: usize,
    failures: u64,
    #[serde(rename = "lastError")]
    last_error: &'a str,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    time: i64,
    vehicles: Vec<Vehicle>,
    held: usize,
    outage: bool,
    #[serde(rename = "upstreamAgeSec")]
    upstream_age_sec: Option<i64>,
    #[serde(rename = "pollSec")]
    poll_sec: u32,
    source: String,
    sources: Vec<SourceInfo<'a>>,
}

/// Sloučený výstup (stejný tvar jako Hub.snapshot() v Node serveru).
pub fn snapshot(inputs: &[SourceInput], now_s: f64, duk_outage: bool, duk_last_ok: f64) -> Vec<u8> {
    let mut vehicles = fuse(inputs);
    for v in vehicles.iter_mut() {
        if v.uid.is_empty() {
            v.uid = uid_of(v);
        }
    }
    let vehicles = dedupe_by_uid(vehicles);
    let live: Vec<&SourceInput> = inputs.iter().filter(|r| r.health == "live" && !r.vehicles.is_empty()).collect();
    let any_ok = inputs.iter().any(|r| r.health != "down");
    let snap = Snapshot {
        time: (now_s * 1000.0) as i64,
        vehicles,
        held: inputs.iter().map(|r| r.held).sum(),
        outage: duk_outage || !any_ok,
        upstream_age_sec: if duk_last_ok > 0.0 { Some((now_s - duk_last_ok).floor() as i64) } else { None },
        poll_sec: 10,
        source: if live.is_empty() { "žádný".to_string() } else { live.iter().map(|r| r.name).collect::<Vec<_>>().join(", ") },
        sources: inputs
            .iter()
            .map(|r| SourceInfo {
                id: r.id,
                name: r.name,
                health: r.health,
                vehicles: r.vehicles.len(),
                failures: r.failures,
                last_error: &r.last_error,
            })
            .collect(),
    };
    serde_json::to_vec(&snap).unwrap_or_else(|_| b"{\"vehicles\":[]}".to_vec())
}

// ---------------------------------------------------------------- testy (porty z test/sources.test.mjs)

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn veh(source: &str, id: i64, lat: f64, line: &str, age: i64) -> Vehicle {
        Vehicle { source: source.into(), segment: "city".into(), id, lat, lng: 14.04, line: line.into(), age_sec: age, ..Default::default() }
    }
    fn inp(id: &'static str, segs: &'static [&'static str], health: &'static str, stale: i64, vehicles: Vec<Vehicle>) -> SourceInput {
        SourceInput { id, name: id, segments: segs, health, stale_vehicle_sec: stale, vehicles, held: 0, failures: 0, last_error: String::new() }
    }

    #[test]
    fn id_prefix() {
        assert_eq!(base_vehicle_id(300451), 451);
        assert_eq!(base_vehicle_id(301212), 1212);
        assert_eq!(base_vehicle_id(451), 451);
        assert!(same_vehicle_id(300451, 451));
        assert!(!same_vehicle_id(300451, 452));
    }

    #[test]
    fn fuse_same_bus_freshest_wins_and_trip_filled() {
        let dpmul = inp("dpmul", &["city"], "live", 90, vec![veh("dpmul", 451, 50.6601, "81", 2)]);
        let mut d = veh("duk", 300451, 50.66, "81", 20);
        d.cis = "000081".into();
        d.trip = "2359".into();
        d.dest = "Církvice".into();
        let duk = inp("duk", &["city", "regional"], "live", 40, vec![d]);
        let out = fuse(&[duk, dpmul]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "dpmul");
        assert_eq!(out[0].cis, "000081");
        assert_eq!(out[0].dest, "Církvice");
        assert_eq!(out[0].ids, vec!["dpmul:451", "duk:300451"]);
        assert_eq!(out[0].mode, "bus", "zdroje bez druhu dopravy = autobus");
    }

    #[test]
    fn fuse_ghost_removed_when_other_source_live() {
        let dpmul = inp("dpmul", &["city"], "live", 90, vec![veh("dpmul", 500, 50.6, "81", 1)]);
        let duk = inp("duk", &["city", "regional"], "live", 40, vec![veh("duk", 300777, 50.65, "81", 200)]);
        assert_eq!(fuse(&[dpmul, duk]).len(), 1);
    }

    #[test]
    fn uid_same_for_both_sources() {
        assert_eq!(uid_of(&veh("dpmul", 451, 50.0, "1", 0)), uid_of(&veh("duk", 300451, 50.0, "1", 0)));
    }

    #[test]
    fn dpmd_never_merges_with_usti_vehicle_of_same_number() {
        let dpmul = inp("dpmul", &["city"], "live", 90, vec![veh("dpmul", 834, 50.6601, "81", 2)]);
        // stejné číslo vozu i linka a poloha, přesto jiné město
        let dpmd = inp("dpmd", &[], "live", 90, vec![veh("dpmd", 834, 50.6601, "81", 2)]);
        let out = fuse(&[dpmul, dpmd]);
        assert_eq!(out.len(), 2);
        assert_ne!(uid_of(&veh("dpmd", 834, 50.0, "1", 0)), uid_of(&veh("dpmul", 834, 50.0, "1", 0)));
    }

    #[test]
    fn dpmd_does_not_cover_segments_or_get_ghosted() {
        // starý děčínský vůz se nezahodí jen proto, že Ústí je živé; a DPMD nezamaskuje zastaralé DÚK vozy
        let dpmul = inp("dpmul", &["city"], "live", 90, vec![veh("dpmul", 500, 50.6, "81", 1)]);
        let dpmd = inp("dpmd", &[], "live", 90, vec![veh("dpmd", 800, 50.78, "201", 300)]);
        assert_eq!(fuse(&[dpmul, dpmd]).len(), 2);
        let dpmd = inp("dpmd", &[], "live", 90, vec![]);
        let duk = inp("duk", &["city", "regional"], "live", 40, vec![veh("duk", 300777, 50.65, "81", 200)]);
        assert_eq!(fuse(&[dpmd, duk]).len(), 1);
    }

    fn train(source: &str, no: &str, lat: f64, age: i64, line: &str) -> Vehicle {
        Vehicle { source: source.into(), segment: "regional".into(), train: true, cis: no.into(), id: no.parse().unwrap_or(0), lat, lng: 14.04, line: line.into(), age_sec: age, ..Default::default() }
    }

    #[test]
    fn pid_train_merges_with_sz_by_train_number() {
        let sz = inp("train", &["regional"], "live", 40, vec![train("train", "9715", 50.1, 4, "Os 9715"), train("train", "9999", 50.3, 4, "Os 9999")]);
        let mut p = train("pid", "9715", 50.1001, 12, "S9");
        p.dest = "Praha".into();
        p.delay = 3;
        let pid = inp("pid", &[], "live", 90, vec![p]);
        let out = fuse(&[sz, pid]);
        assert_eq!(out.len(), 2, "vlak 9715 z obou zdrojů je jeden, 9999 jen v SŽ");
        let m = out.iter().find(|v| v.cis == "9715").unwrap();
        assert_eq!(m.sources, vec!["pid", "train"]);
        assert_eq!(m.mode, "train");
        assert_eq!(m.source, "train", "SŽ je čerstvější");
        assert_eq!(m.dest, "Praha", "chybějící údaj se doplní z PID");
        assert_eq!(uid_of(m), "t9715");
    }

    #[test]
    fn pid_train_with_other_number_is_not_merged() {
        let sz = inp("train", &["regional"], "live", 40, vec![train("train", "9715", 50.1, 4, "Os 9715")]);
        let pid = inp("pid", &[], "live", 90, vec![train("pid", "9716", 50.1, 4, "Os 9716")]);
        assert_eq!(fuse(&[sz, pid]).len(), 2);
    }

    #[test]
    fn pid_regional_bus_merges_with_duk_by_line_and_place_and_keeps_uid() {
        let mut d = veh("duk", 24567, 50.0, "388", 8);
        d.segment = "regional".into();
        let duk = inp("duk", &["city", "regional"], "live", 40, vec![d]);
        let mut p = veh("pid", 7694, 50.0003, "388", 1);
        p.segment = "regional".into();
        p.vkey = "3:7694".into();
        p.dest = "Mělník".into();
        let pid = inp("pid", &[], "live", 90, vec![p]);
        let out = fuse(&[duk, pid]);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].source.as_str(), out[0].dest.as_str()), ("pid", "Mělník"));
        assert_eq!(out[0].uid, uid_of(&veh("duk", 24567, 50.0, "388", 0)), "uid podle DÚK, i když vyhrál PID");
        assert_eq!(out[0].sources, vec!["duk", "pid"]);
    }

    #[test]
    fn pid_city_vehicles_never_merge_and_uid_has_own_namespace() {
        // stejné číslo vozu a linka i místo jako ústecký vůz: PID pražský vůz je samostatný
        let dpmul = inp("dpmul", &["city"], "live", 90, vec![veh("dpmul", 451, 50.0, "9", 2)]);
        let mut p = veh("pid", 451, 50.0, "9", 2);
        p.vkey = "0:451".into();
        let pid = inp("pid", &[], "live", 90, vec![p.clone()]);
        let mut out = fuse(&[dpmul, pid]);
        assert_eq!(out.len(), 2);
        let mut q = p;
        q.vkey = "3:451".into();
        assert_ne!(uid_of(&q), uid_of(&out.remove(0)));
        assert_eq!(uid_of(&q), "g3:451");
    }

    #[test]
    fn pid_city_vehicle_survives_stale_while_duk_is_live() {
        let duk = inp("duk", &["city", "regional"], "live", 40, vec![]);
        let mut p = veh("pid", 1, 50.08, "22", 300);
        p.vkey = "0:1".into();
        let pid = inp("pid", &[], "live", 90, vec![p]);
        assert_eq!(fuse(&[duk, pid]).len(), 1);
    }

    #[test]
    fn freshest_source_wins_per_vehicle_dynamically() {
        // DÚK čerstvější než DPMÚL: vyhraje DÚK
        let dpmul = inp("dpmul", &["city"], "live", 90, vec![veh("dpmul", 451, 50.66, "81", 30)]);
        let duk = inp("duk", &["city", "regional"], "live", 40, vec![veh("duk", 300451, 50.66, "81", 3)]);
        let out = fuse(&[dpmul, duk]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "duk");
        // a obráceně, bez ohledu na pořadí zdrojů na vstupu
        let dpmul = inp("dpmul", &["city"], "live", 90, vec![veh("dpmul", 451, 50.66, "81", 1)]);
        let duk = inp("duk", &["city", "regional"], "live", 40, vec![veh("duk", 300451, 50.66, "81", 9)]);
        let out = fuse(&[duk, dpmul]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "dpmul");
    }

    #[test]
    fn fuse_equal_age_is_deterministic() {
        let a = inp("dpmul", &["city"], "live", 90, vec![veh("dpmul", 451, 50.66, "81", 5)]);
        let b = inp("duk", &["city", "regional"], "live", 40, vec![veh("duk", 300451, 50.66, "81", 5)]);
        assert_eq!(fuse(&[a, b])[0].source, "dpmul");
        let a = inp("dpmul", &["city"], "live", 90, vec![veh("dpmul", 451, 50.66, "81", 5)]);
        let b = inp("duk", &["city", "regional"], "live", 40, vec![veh("duk", 300451, 50.66, "81", 5)]);
        assert_eq!(fuse(&[b, a])[0].source, "dpmul");
    }

    #[test]
    fn stale_lone_vehicle_of_any_source_dropped_when_other_source_live() {
        // zastaralý osamělý vůz DPMÚL se zahodí, když DÚK živě pokrývá město (dřív to platilo jen opačně)
        let dpmul = inp("dpmul", &["city"], "live", 90, vec![veh("dpmul", 500, 50.6, "81", 200)]);
        let duk = inp("duk", &["city", "regional"], "live", 40, vec![veh("duk", 300777, 50.65, "82", 2)]);
        let out = fuse(&[dpmul, duk]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "duk");
    }

    #[test]
    fn stale_bus_kept_when_other_live_source_has_only_trains() {
        let mut b = veh("duk", 24567, 50.0, "388", 200);
        b.segment = "regional".into();
        let duk = inp("duk", &["city", "regional"], "live", 40, vec![b]);
        let sz = inp("train", &["regional"], "live", 40, vec![train("train", "9715", 50.1, 4, "Os 9715")]);
        assert_eq!(fuse(&[duk, sz]).len(), 2);
    }

    #[test]
    fn delay_and_time() {
        assert_eq!(parse_delay_minutes(Some(&json!("00:05:00"))), 5);
        assert_eq!(parse_delay_minutes(Some(&json!("-00:01:30"))), -1);
        assert_eq!(parse_delay_minutes(Some(&json!("3,4"))), 3);
        assert_eq!(parse_delay_minutes(None), 0);
        assert_eq!(parse_time_ms("2026-07-01T12:00:00"), parse_time_ms("2026-07-01T10:00:00Z"));
        assert_eq!(parse_time_ms("2026-11-10T22:00:00"), parse_time_ms("2026-11-10T21:00:00Z"));
        assert_eq!(parse_time_ms("2026-11-10T21:59:50+01:00"), parse_time_ms("2026-11-10T20:59:50Z"));
    }

    #[test]
    fn dpmul_parsing() {
        let now = parse_time_ms("2026-11-10T21:00:00Z").unwrap();
        let body = vec![
            json!({"vehicleNumber": 451, "lineName": "81", "delay": "00:05:00", "latitude": 50.66, "longitude": 14.04, "lastActivity": "2026-11-10T21:59:50+01:00", "isWheelchairAccessible": true, "isAirConditioned": false}),
            json!({"vehicleNumber": 2, "lineName": "200", "latitude": 50.67, "longitude": 14.05, "gpsTimestamp": "2026-11-10T22:00:00"}),
            json!({"vehicleNumber": 3, "lineName": "1", "latitude": null, "longitude": null}),
            json!({"vehicleNumber": 4, "lineName": "2", "latitude": 10, "longitude": 10}),
        ];
        let v = parse_dpmul(&body, now, &mut HashMap::new());
        assert_eq!(v.iter().map(|x| x.id).collect::<Vec<_>>(), vec![451, 2]);
        assert_eq!(v[0].delay, 5);
        assert_eq!(v[0].age_sec, 10);
        assert_eq!(v[0].low, Some(true));
        assert_eq!(v[0].ac, Some(false));
        assert_eq!(v[1].line, "X");
    }

    #[test]
    fn duk_normalize() {
        let e = json!({"ID": 300451, "Latitude": 50.66, "Longitude": 14.04, "State": 0, "Delay": 2, "LineID": "80", "CISLineID": 80, "RouteID": "2359"});
        let v = duk_vehicle(&e, 3).unwrap();
        assert_eq!((v.cis.as_str(), v.trip.as_str(), v.city, v.train, v.segment.as_str()), ("000080", "2359", true, false, "city"));
        let t = duk_vehicle(&json!({"ID": 21234, "Latitude": 50.6, "Longitude": 14.0, "State": 0, "RouteID": " 1193 "}), 0).unwrap();
        assert!(t.train && t.cis == "1193" && t.trip.is_empty() && t.segment == "regional");
        assert!(duk_vehicle(&json!({"ID": 1, "Latitude": 50.6, "Longitude": 14.0, "State": 255}), 0).is_none());
    }
}
