//! DPKV (Dopravní podnik Karlovy Vary): polohy městských autobusů z dopravního portálu https://dopravniportal.dpkv.cz/dic/
//! (vrstva „Kde je můj autobus?“). Bez přihlášení.
//!
//! - polohy: `POST /dic/FeatureService/GetFeatures` s vrstvou "RT spoje point" a výřezem v S-JTSK (EPSG:5514);
//!   prvek má id "<linka>_<číslo vozu>", souřadnice x / y v S-JTSK a HTML značky (šipka `rotate(<azimut>deg)` jen
//!   u jedoucího vozu),
//! - podrobnosti: `GET /dic/TabularData/RTConn` (HTML tabulka): řádek má title "<vůz>/<id prvku>", bezbariérovost,
//!   cílovou zastávku, zpoždění ("2 min.", "*" = neznámé) a poslední obslouženou zastávku.
//!
//! Souřadnice se převádějí z S-JTSK (Křovákovo zobrazení, Besselův elipsoid) na WGS84 Helmertovou transformací
//! (přesnost kolem 1 m, na mapu stačí).

use crate::merge::Vehicle;
use serde_json::Value;
use std::collections::HashMap;
use std::f64::consts::PI;

pub const AGENCY: &str = "Dopravní podnik Karlovy Vary, a.s.";

pub struct Snapshot {
    pub vehicles: Vec<Vehicle>,
    /// /api/raw/dpkv: rozebraná vozidla (souřadnice už ve WGS84)
    pub raw: Vec<u8>,
}

/// EPSG:5514 (x, y záporné, v metrech) → (zeměpisná šířka, délka) ve WGS84.
pub fn jtsk_to_wgs84(x5514: f64, y5514: f64) -> (f64, f64) {
    // S-JTSK: X = jižní směr (kladné ~1e6), Y = západní směr (kladné ~6e5); EPSG:5514 má obě osy záporné a prohozené
    let (xj, yj) = (-y5514, -x5514);

    // Křovákovo zobrazení zpět na Besselův elipsoid
    let e = 0.081696831215303_f64;
    let n = 0.97992470462083_f64;
    let konst_u_ro = 12310230.12797036_f64;
    let (sin_uq, cos_uq) = (0.863499969506341_f64, 0.504348889819882_f64);
    let (sin_vq, cos_vq) = (0.420215144586493_f64, 0.907424504992097_f64);
    let alfa = 1.000597498371542_f64;
    let k = 1.003419163966575_f64;

    let ro = (xj * xj + yj * yj).sqrt();
    let epsilon = 2.0 * (yj / (ro + xj)).atan();
    let d = epsilon / n;
    let s = 2.0 * ((1.0 / n * (konst_u_ro / ro).ln()).exp()).atan() - PI / 2.0;
    let (sin_s, cos_s) = s.sin_cos();
    let sin_u = sin_uq * sin_s - cos_uq * cos_s * d.cos();
    let cos_u = (1.0 - sin_u * sin_u).sqrt();
    let sin_dv = d.sin() * cos_s / cos_u;
    let cos_dv = (1.0 - sin_dv * sin_dv).sqrt();
    let sin_v = sin_vq * cos_dv - cos_vq * sin_dv;
    let cos_v = cos_vq * cos_dv + sin_vq * sin_dv;
    let l_jtsk = 2.0 * (sin_v / (1.0 + cos_v)).atan() / alfa;
    let t = (2.0 / alfa * ((1.0 + sin_u) / cos_u / k).ln()).exp();
    let mut pom = (t - 1.0) / (t + 1.0);
    for _ in 0..50 {
        let sin_b = pom;
        let p = t * (e * ((1.0 + e * sin_b) / (1.0 - e * sin_b)).ln()).exp();
        pom = (p - 1.0) / (p + 1.0);
        if (pom - sin_b).abs() < 1e-15 {
            break;
        }
    }
    let b_jtsk = (pom / (1.0 - pom * pom).sqrt()).atan();

    // Bessel (výška 245 m, průměr ČR) → pravoúhlé souřadnice
    let a_b = 6377397.15508_f64;
    let f_b = 1.0 / 299.152812853_f64;
    let e2_b = 1.0 - (1.0 - f_b) * (1.0 - f_b);
    let h = 245.0;
    let rn = a_b / (1.0 - e2_b * b_jtsk.sin().powi(2)).sqrt();
    let x = (rn + h) * b_jtsk.cos() * l_jtsk.cos();
    let y = (rn + h) * b_jtsk.cos() * l_jtsk.sin();
    let z = ((1.0 - e2_b) * rn + h) * b_jtsk.sin();

    // Helmertova transformace S-JTSK → WGS84
    let (dx, dy, dz) = (570.69_f64, 85.69_f64, 462.84_f64);
    let sec = PI / 180.0 / 3600.0;
    let (wx, wy, wz) = (-4.99821 * sec, -1.58676 * sec, -5.2611 * sec);
    let m = 3.543e-6_f64;
    let xn = dx + (1.0 + m) * (x + wz * y - wy * z);
    let yn = dy + (1.0 + m) * (-wz * x + y + wx * z);
    let zn = dz + (1.0 + m) * (wy * x - wx * y + z);

    // pravoúhlé → zeměpisné na WGS84
    let a = 6378137.0_f64;
    let f_1 = 298.257223563_f64;
    let a_b2 = f_1 / (f_1 - 1.0);
    let p = (xn * xn + yn * yn).sqrt();
    let e2 = 1.0 - (1.0 - 1.0 / f_1).powi(2);
    let theta = (zn * a_b2 / p).atan();
    let (st, ct) = theta.sin_cos();
    let tt = (zn + e2 * a_b2 * a * st.powi(3)) / (p - e2 * a * ct.powi(3));
    let lat = tt.atan();
    let lon = 2.0 * (yn / (p + xn)).atan();
    (lat.to_degrees(), lon.to_degrees())
}

/// Text mezi `<tag ...>` a `</tag>` bez vnořených značek, s rozbalenými základními HTML entitami.
fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// HTML entity: pojmenované základní a číselné (&#237; = í, &#xED; = í), portál jimi kóduje diakritiku.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p..];
        let end = after.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| {
            let name = &after[1..e];
            let c = match name {
                "nbsp" => Some(' '),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                _ if name.starts_with("#x") || name.starts_with("#X") => u32::from_str_radix(&name[2..], 16).ok().and_then(char::from_u32),
                _ if name.starts_with('#') => name[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => { out.push(c); rest = &after[e + 1..]; }
            None => { out.push('&'); rest = &after[1..]; }
        }
    }
    out.push_str(rest);
    out
}

fn attr<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let k = format!("{name}=\"");
    let p = s.find(&k)? + k.len();
    Some(&s[p..p + s[p..].find('"')?])
}

/// Obsahy buněk `<td>` řádku tabulky.
fn cells(row: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = row;
    while let Some(p) = rest.find("<td") {
        let after = &rest[p..];
        let Some(open_end) = after.find('>') else { break };
        let Some(close) = after.find("</td>") else { break };
        if close < open_end {
            break;
        }
        out.push(&after[..close]);
        rest = &after[close + 5..];
    }
    out
}

struct RowInfo {
    dest: String,
    delay: i64,
    low: bool,
}

/// Tabulka RTConn → id prvku ("<linka>_<vůz>") → cíl, zpoždění, bezbariérovost.
fn parse_table(html: &str) -> HashMap<String, RowInfo> {
    let mut map = HashMap::new();
    for row in html.split("<tr").skip(1) {
        let Some(title) = attr(row, "title") else { continue };
        let Some((_, fid)) = title.split_once('/') else { continue };
        let c = cells(row);
        if c.len() < 5 {
            continue;
        }
        // ikona bezbariérovosti je v řádku vždy, u nebezbariérového vozu skrytá (display:none)
        let low = c[1].contains("IsBarrierFree") && !c[1].contains("display:none");
        let dest = strip_tags(c[2]);
        let delay_txt = strip_tags(c[3]);
        let delay = delay_txt.trim_end_matches('.').trim_end_matches("min").trim().parse::<i64>().unwrap_or(0);
        map.insert(fid.trim().to_string(), RowInfo { dest, delay, low });
    }
    map
}

fn heading_of(html: &str) -> Option<f64> {
    let p = html.find("rotate(")? + 7;
    let rest = &html[p..];
    rest[..rest.find("deg")?].trim().parse::<f64>().ok().filter(|h| h.is_finite())
}

/// GetFeatures (JSON) + RTConn (HTML) → vozidla. Chyba = jiný tvar odpovědi map.
pub fn parse(features: &str, table: &str) -> Result<Snapshot, String> {
    let v: Value = serde_json::from_str(features).map_err(|e| format!("JSON: {e}"))?;
    let list = v
        .get("layers")
        .and_then(|l| l.as_array())
        .and_then(|ls| ls.iter().find(|l| l.get("id").and_then(|i| i.as_str()) == Some("RT spoje point")))
        .and_then(|l| l.get("featureList"))
        .and_then(|f| f.as_array())
        .ok_or_else(|| "chybí vrstva RT spoje point".to_string())?;
    let rows = parse_table(table);
    let mut vehicles = Vec::new();
    for f in list {
        let Some(fid) = f.get("id").and_then(|i| i.as_str()) else { continue };
        let Some((line, vno)) = fid.split_once('_') else { continue };
        let (Some(x), Some(y)) = (f.get("x").and_then(|v| v.as_f64()), f.get("y").and_then(|v| v.as_f64())) else { continue };
        let (lat, lng) = jtsk_to_wgs84(x, y);
        if !(48.0..=52.0).contains(&lat) || !(12.0..=19.0).contains(&lng) || line.is_empty() {
            continue;
        }
        let html = f.get("html").and_then(|h| h.as_str()).unwrap_or("");
        let info = rows.get(fid);
        vehicles.push(Vehicle {
            source: "dpkv".into(),
            segment: "city".into(),
            id: vno.parse().unwrap_or(0),
            lat,
            lng,
            line: line.to_string(),
            delay: info.map_or(0, |r| r.delay),
            age_sec: 0, // mapa ukazuje aktuální polohy, čas polohy neposílá
            agency: AGENCY.into(),
            dest: info.map(|r| r.dest.clone()).unwrap_or_default(),
            low: info.map(|r| r.low),
            heading: heading_of(html),
            city: true,
            mode: "bus".into(),
            ..Default::default()
        });
    }
    let raw = serde_json::to_vec(&serde_json::json!({ "vehicles": vehicles })).unwrap_or_default();
    Ok(Snapshot { vehicles, raw })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jtsk_matches_pyproj() {
        // referenční hodnoty: pyproj EPSG:5514 → EPSG:4326
        for (x, y, lat, lon) in [
            (-850139.0, -1010973.0, 50.230955, 12.869596),
            (-743000.0, -1043000.0, 50.087347, 14.418594),
            (-470000.0, -1100000.0, 49.850014, 18.289075),
        ] {
            let (la, lo) = jtsk_to_wgs84(x, y);
            assert!((la - lat).abs() < 2e-5 && (lo - lon).abs() < 3e-5, "{x},{y}: {la},{lo} vs {lat},{lon}");
        }
    }

    #[test]
    fn entities() {
        assert_eq!(decode_entities("Doub&#237; &amp; Star&#xE1; Role &x"), "Doubí & Stará Role &x");
    }

    #[test]
    fn features_and_table() {
        let features = r#"{"layers":[{"id":"RT spoje point","featureList":[
          {"id":"2_145","x":-849984,"y":-1010986,"html":"<div class='rtconn-holder'><div class='rtconn-arrow' style='transform:rotate(308deg);'></div><div class='rtconn-text'>2</div></div>"},
          {"id":"6_149","x":-850139,"y":-1010973,"html":"<div class='rtconn-holder'><div class='rtconn-text'>6</div></div>"}]}]}"#;
        let table = r#"<table><tr><th>Linka</th></tr>
          <tr><td title="145/2_145"><div>2</div></td>
              <td><div style="background-image:url(/x/rtConnTabularDataIcon-IsBarrierFree.png);width:22px"></div></td>
              <td style="width:200px;">Divadelní náměstí</td><td class="ta-right">2 min.</td><td style="width:200px;">Jízdárenská</td></tr>
          <tr><td title="149/6_149"><div>6</div></td>
              <td><div style="background-image:url(/x/rtConnTabularDataIcon-IsBarrierFree.png);display:none"></div></td>
              <td style="width:200px;">Doubí</td><td class="ta-right">*</td><td style="width:200px;">Tržnice</td></tr></table>"#;
        let s = parse(features, table).unwrap();
        assert_eq!(s.vehicles.len(), 2);
        let a = &s.vehicles[0];
        assert_eq!((a.id, a.line.as_str(), a.dest.as_str(), a.delay, a.low, a.heading), (145, "2", "Divadelní náměstí", 2, Some(true), Some(308.0)));
        let b = &s.vehicles[1];
        assert_eq!((b.id, b.dest.as_str(), b.delay, b.low, b.heading), (149, "Doubí", 0, Some(false), None));
        assert!((b.lat - 50.2310).abs() < 1e-3 && (b.lng - 12.8696).abs() < 1e-3);
    }
}
