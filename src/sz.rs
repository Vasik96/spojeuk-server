//! Správa železnic: HTTPS GET bez hlavičky User-Agent (SŽ banuje IP, které na ně pouští služby;
//! běžné HTTP knihovny User-Agent přidávají vždy, proto minimální vlastní klient nad native-tls),
//! převod S-JTSK → WGS84 a parsování vrstvy vlaků. Port ze lib/sources.mjs.

use crate::merge::{js_number, js_parse_int, num_or0, str_or_empty, Vehicle};
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};

const MAX_BODY: usize = 32 << 20;

// ---------------------------------------------------------------- HTTP klient bez User-Agent

enum Mode {
    Len(usize),
    Chunked,
    ToEof,
    NoBody,
}

fn find(buf: &[u8], pat: &[u8]) -> Option<usize> {
    buf.windows(pat.len()).position(|w| w == pat)
}

fn decode_chunked(mut d: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let le = find(d, b"\r\n")?;
        let line = std::str::from_utf8(&d[..le]).ok()?;
        let size = usize::from_str_radix(line.split(';').next()?.trim(), 16).ok()?;
        d = &d[le + 2..];
        if size == 0 {
            return Some(out);
        }
        if d.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&d[..size]);
        d = &d[size + 2..];
    }
}

fn parse_head(head: &[u8]) -> Result<(u16, Mode), String> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "špatná odpověď serveru".to_string())?;
    let (mut chunked, mut len) = (false, None);
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            } else if k == "content-length" {
                len = v.parse::<usize>().ok();
            }
        }
    }
    let mode = if status == 204 || status == 304 || (100..200).contains(&status) {
        Mode::NoBody
    } else if chunked {
        Mode::Chunked
    } else if let Some(n) = len {
        Mode::Len(n)
    } else {
        Mode::ToEof
    };
    Ok((status, mode))
}

/// HTTPS GET (HTTP/1.1, Connection: close) bez User-Agent a bez Accept-Encoding. Vrací (status, tělo).
pub fn http_get(url: &str, timeout: Duration) -> Result<(u16, Vec<u8>), String> {
    let rest = url.strip_prefix("https://").ok_or_else(|| "podporováno jen https".to_string())?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|e| e.to_string())?),
        None => (host_port, 443),
    };
    let deadline = Instant::now() + timeout;
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS: {e}"))?
        .next()
        .ok_or_else(|| "DNS: žádná adresa".to_string())?;
    let tcp = TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("connect: {e}"))?;
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();
    let connector = native_tls::TlsConnector::new().map_err(|e| format!("TLS: {e}"))?;
    let mut tls = connector.connect(host, tcp).map_err(|e| format!("TLS: {e}"))?;

    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n\r\n");
    tls.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
    tls.flush().ok();

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16384];
    let mut head: Option<(u16, usize, Mode)> = None; // status, začátek těla, režim
    let mut eof = false;
    loop {
        if head.is_none() {
            if let Some(p) = find(&buf, b"\r\n\r\n") {
                let (status, mode) = parse_head(&buf[..p])?;
                head = Some((status, p + 4, mode));
            }
        }
        if let Some((status, start, mode)) = &head {
            let body = &buf[*start..];
            match mode {
                Mode::NoBody => return Ok((*status, Vec::new())),
                Mode::Len(n) if body.len() >= *n => return Ok((*status, body[..*n].to_vec())),
                Mode::Chunked if body.ends_with(b"0\r\n\r\n") => {
                    return decode_chunked(body).map(|b| (*status, b)).ok_or_else(|| "špatné chunked kódování".to_string());
                }
                _ => {}
            }
            if eof {
                return match mode {
                    Mode::ToEof => Ok((*status, body.to_vec())),
                    Mode::Chunked => decode_chunked(body).map(|b| (*status, b)).ok_or_else(|| "neúplná odpověď".to_string()),
                    _ => Err("neúplná odpověď".to_string()),
                };
            }
        } else if eof {
            return Err("spojení uzavřeno bez odpovědi".to_string());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("timeout".to_string());
        }
        tls.get_ref().set_read_timeout(Some(left)).ok();
        match tls.read(&mut chunk) {
            Ok(0) => eof = true,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_BODY {
                    return Err("odpověď je příliš velká".to_string());
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err("timeout".to_string())
            }
            // server zavřel TLS bez close_notify: bereme jako konec dat
            Err(_) => eof = true,
        }
    }
}

// ---------------------------------------------------------------- S-JTSK → WGS84

/// S-JTSK / Krovak East North (EPSG:5514) → (lat, lng) WGS84, nebo None.
pub fn sjtsk_to_wgs84(east: f64, north: f64) -> Option<(f64, f64)> {
    use std::f64::consts::PI;
    let a = 6377397.155_f64;
    let f: f64 = 1.0 / 299.1528128;
    let e2 = 2.0 * f - f * f;
    let e = e2.sqrt();
    let phi_c = 49.5_f64.to_radians();
    let lambda_0 = (24.0 + 50.0 / 60.0_f64).to_radians();
    let alpha_c = (30.0 + 17.0 / 60.0 + 17.3031 / 3600.0_f64).to_radians();
    let phi_p = 78.5_f64.to_radians();
    let k_p: f64 = 0.9999;
    let a_ = (a * (1.0 - e2).sqrt()) / (1.0 - e2 * phi_c.sin().powi(2));
    let b = (1.0 + (e2 * phi_c.cos().powi(4)) / (1.0 - e2)).sqrt();
    let gamma_0 = (phi_c.sin() / b).asin();
    let t0 = ((PI / 4.0 + gamma_0 / 2.0).tan() * ((1.0 + e * phi_c.sin()) / (1.0 - e * phi_c.sin())).powf((e * b) / 2.0))
        / (PI / 4.0 + phi_c / 2.0).tan().powf(b);
    let n = phi_p.sin();
    let r0 = (k_p * a_) / phi_p.tan();
    if east.is_nan() || north.is_nan() || (east == 0.0 && north == 0.0) {
        return None;
    }
    let (xp, yp) = (-north, -east);
    let r = xp.hypot(yp);
    if r == 0.0 {
        return None;
    }
    let d = yp.atan2(xp) / n;
    let s = 2.0 * (((r0 / r).powf(1.0 / n) * (PI / 4.0 + phi_p / 2.0).tan()).atan() - PI / 4.0);
    let u = (alpha_c.cos() * s.sin() - alpha_c.sin() * s.cos() * d.cos()).asin();
    let v = ((s.cos() * d.sin()) / u.cos()).asin();
    let mut phi = u;
    for _ in 0..8 {
        phi = 2.0
            * ((t0.powf(-1.0 / b) * (u / 2.0 + PI / 4.0).tan().powf(1.0 / b) * ((1.0 + e * phi.sin()) / (1.0 - e * phi.sin())).powf(e / 2.0)).atan()
                - PI / 4.0);
    }
    let lam = lambda_0 - v / b;
    // Helmert (position vector), EPSG:15992 S-JTSK → WGS84
    let nn = a / (1.0 - e2 * phi.sin().powi(2)).sqrt();
    let x = nn * phi.cos() * lam.cos();
    let y = nn * phi.cos() * lam.sin();
    let z = nn * (1.0 - e2) * phi.sin();
    let sec = PI / 648000.0;
    let (rx, ry, rz, ds) = (4.998 * sec, 1.587 * sec, 5.261 * sec, 1.0 + 3.56e-6);
    let x2 = 570.8 + ds * (x - rz * y + ry * z);
    let y2 = 85.7 + ds * (rz * x + y - rx * z);
    let z2 = 462.8 + ds * (-ry * x + rx * y + z);
    let (a2, e2b) = (6378137.0_f64, 0.00669437999014_f64);
    let p = x2.hypot(y2);
    let mut la = z2.atan2(p * (1.0 - e2b));
    for _ in 0..6 {
        let n2 = a2 / (1.0 - e2b * la.sin().powi(2)).sqrt();
        la = (z2 + e2b * n2 * la.sin()).atan2(p);
    }
    let lat = la.to_degrees();
    let lng = y2.atan2(x2).to_degrees();
    if (47.0..=52.0).contains(&lat) && (11.0..=20.0).contains(&lng) {
        Some((lat, lng))
    } else {
        None // mimo ČR = chybná data
    }
}

/// Odpověď SŽ ({"result":[...]}) → všechny vlaky.
pub fn parse_sz(data: &Value) -> Vec<Vehicle> {
    let mut out = Vec::new();
    let Some(arr) = data.get("result").and_then(|r| r.as_array()) else { return out };
    for (i, f) in arr.iter().enumerate() {
        let Some(p) = f.get("properties") else { continue };
        if p.get("type").and_then(|t| t.as_str()) != Some("V") {
            continue; // jen vlaky
        }
        let Some(c) = f.get("geometry").and_then(|g| g.get("coordinates")).and_then(|c| c.as_array()) else { continue };
        if c.len() < 2 {
            continue;
        }
        let Some((lat, lng)) = sjtsk_to_wgs84(js_number(c.first()), js_number(c.get(1))) else { continue };
        let cat = str_or_empty(p.get("tt")).trim().to_string();
        let num = str_or_empty(p.get("tn")).trim().to_string();
        let parsed_id = js_parse_int(Some(&Value::String(num.clone())));
        out.push(Vehicle {
            source: "train".into(),
            segment: "regional".into(),
            train: true,
            id: if parsed_id != 0 { parsed_id } else { i as i64 + 1 },
            lat,
            lng,
            line: if cat.is_empty() { num.clone() } else { format!("{cat} {num}") },
            delay: num_or0(p.get("de")).trunc() as i64,
            age_sec: 0,
            state: 0,
            cis: num,
            trip: str_or_empty(p.get("id")),
            agency: str_or_empty(p.get("d")),
            dest: str_or_empty(p.get("ln")).trim().to_string(),
            low: None,
            ac: None,
            heading: None,
            city: false,
            ..Default::default()
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sjtsk_praha() {
        let (lat, lng) = sjtsk_to_wgs84(-743000.0, -1043000.0).unwrap();
        assert!((lat - 50.08).abs() < 0.1 && (lng - 14.42).abs() < 0.1, "{lat} {lng}");
        assert!(sjtsk_to_wgs84(0.0, 0.0).is_none());
        assert!(sjtsk_to_wgs84(f64::NAN, 1.0).is_none());
    }

    #[test]
    fn parse_trains() {
        let out = parse_sz(&json!({"result": [
            {"properties": {"type": "V", "tt": "Os", "tn": "9780", "ln": "Louny", "de": 3, "d": "ČD", "id": "x1"}, "geometry": {"coordinates": [-743000, -1043000]}},
            {"properties": {"type": "S", "tn": "1"}, "geometry": {"coordinates": [-743000, -1043000]}},
            {"properties": {"type": "V", "tn": "2"}, "geometry": {"coordinates": [0, 0]}},
        ]}));
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].line.as_str(), out[0].cis.as_str(), out[0].delay, out[0].id), ("Os 9780", "9780", 3, 9780));
    }

    #[test]
    fn chunked() {
        assert_eq!(decode_chunked(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(), b"Wikipedia");
        assert!(decode_chunked(b"4\r\nWi").is_none());
    }
}
