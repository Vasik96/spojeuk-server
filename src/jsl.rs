//! Úzká čtečka JavaScriptových literálů pro TrafficState.js z DPMD (jen std).
//!
//! Soubor není JSON: klíče bez uvozovek, řetězce v zpětných apostrofech, HTML se skládá z proměnných
//! stránky (`` `…`+vehicleImageUrl+`834.jpg` ``). Nic se nevyhodnocuje. Čte se jen to, co je potřeba:
//! `jmeno = hodnota;`, hodnota = objekt / pole / řetězec / číslo / true / false / null / identifikátor,
//! spojované přes `+`. Neznámý identifikátor zůstane v textu jako `${jmeno}`.
//! Cokoli jiného je chyba (změnil-li se tvar souboru, má se to ukázat, ne se hádat).

#[derive(Debug, Clone, PartialEq)]
pub enum Js {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Js>),
    Obj(Vec<(String, Js)>),
}

impl Js {
    pub fn get(&self, k: &str) -> Option<&Js> {
        match self {
            Js::Obj(m) => m.iter().find(|(kk, _)| kk == k).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Js]> {
        match self {
            Js::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// Skalár jako text (null, pole a objekt = prázdný řetězec).
    pub fn text(&self) -> String {
        match self {
            Js::Str(s) => s.clone(),
            Js::Num(n) => num_text(*n),
            Js::Bool(b) => b.to_string(),
            _ => String::new(),
        }
    }

    pub fn truthy(&self) -> bool {
        match self {
            Js::Null => false,
            Js::Bool(b) => *b,
            Js::Num(n) => *n != 0.0 && !n.is_nan(),
            Js::Str(s) => !s.is_empty() && s != "0",
            _ => true,
        }
    }
}

fn num_text(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        (n as i64).to_string()
    } else {
        n.to_string()
    }
}

// ---------------------------------------------------------------- parser

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

const MAX_DEPTH: usize = 32;

impl<'a> P<'a> {
    fn err<T>(&self, msg: &str) -> Result<T, String> {
        Err(format!("JS: {} (pozice {})", msg, self.i))
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) -> Result<(), String> {
        loop {
            while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            let rest = &self.b[self.i..];
            if rest.starts_with(b"/*") {
                match rest[2..].windows(2).position(|w| w == b"*/") {
                    Some(p) => self.i += 2 + p + 2,
                    None => return self.err("neuzavřený komentář"),
                }
            } else if rest.starts_with(b"//") {
                while self.i < self.b.len() && self.b[self.i] != b'\n' {
                    self.i += 1;
                }
            } else {
                return Ok(());
            }
        }
    }

    fn ident(&mut self) -> String {
        let st = self.i;
        while self.i < self.b.len() && (self.b[self.i].is_ascii_alphanumeric() || matches!(self.b[self.i], b'_' | b'$' | b'.')) {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.b[st..self.i]).into_owned()
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            self.err(&format!("očekáváno '{}'", c as char))
        }
    }

    fn hex(&mut self, n: usize) -> Result<u32, String> {
        let end = self.i + n;
        let s = self.b.get(self.i..end).and_then(|s| std::str::from_utf8(s).ok());
        match s.and_then(|s| u32::from_str_radix(s, 16).ok()) {
            Some(v) => {
                self.i = end;
                Ok(v)
            }
            None => self.err("chybná \\u / \\x sekvence"),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let q = self.b[self.i];
        self.i += 1;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => return self.err("neuzavřený řetězec"),
            };
            self.i += 1;
            if c == q {
                break;
            }
            if c != b'\\' {
                buf.push(c);
                continue;
            }
            let e = match self.peek() {
                Some(e) => e,
                None => return self.err("neuzavřený řetězec"),
            };
            self.i += 1;
            match e {
                b'n' => buf.push(b'\n'),
                b't' => buf.push(b'\t'),
                b'r' => buf.push(b'\r'),
                b'b' => buf.push(8),
                b'f' => buf.push(12),
                b'0' => buf.push(0),
                b'\n' => {}
                b'x' => {
                    let v = self.hex(2)?;
                    push_char(&mut buf, v);
                }
                b'u' => {
                    let mut v = self.hex(4)?;
                    if (0xD800..0xDC00).contains(&v) && self.b[self.i..].starts_with(b"\\u") {
                        let save = self.i;
                        self.i += 2;
                        match self.hex(4) {
                            Ok(lo) if (0xDC00..0xE000).contains(&lo) => v = 0x10000 + ((v - 0xD800) << 10) + (lo - 0xDC00),
                            _ => self.i = save,
                        }
                    }
                    push_char(&mut buf, v);
                }
                other => buf.push(other),
            }
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn number(&mut self) -> Result<f64, String> {
        let st = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let digits = |p: &mut P| {
            while p.i < p.b.len() && p.b[p.i].is_ascii_digit() {
                p.i += 1;
            }
        };
        digits(self);
        if self.peek() == Some(b'.') {
            self.i += 1;
            digits(self);
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            digits(self);
        }
        match std::str::from_utf8(&self.b[st..self.i]).ok().and_then(|s| s.parse::<f64>().ok()) {
            Some(n) if n.is_finite() => Ok(n),
            _ => {
                self.i = st;
                self.err("chybné číslo")
            }
        }
    }

    fn primary(&mut self, depth: usize) -> Result<Js, String> {
        if depth > MAX_DEPTH {
            return self.err("příliš hluboké vnoření");
        }
        self.skip_ws()?;
        match self.peek() {
            None => self.err("neočekávaný konec"),
            Some(b'{') => {
                self.i += 1;
                let mut m: Vec<(String, Js)> = Vec::new();
                loop {
                    self.skip_ws()?;
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                        break;
                    }
                    let key = match self.peek() {
                        Some(b'"') | Some(b'\'') | Some(b'`') => self.string()?,
                        _ => {
                            let k = self.ident();
                            if k.is_empty() {
                                return self.err("očekáván klíč");
                            }
                            k
                        }
                    };
                    self.skip_ws()?;
                    self.expect(b':')?;
                    let v = self.value(depth + 1)?;
                    m.push((key, v));
                    self.skip_ws()?;
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {}
                        _ => return self.err("očekáváno ',' nebo '}'"),
                    }
                }
                Ok(Js::Obj(m))
            }
            Some(b'[') => {
                self.i += 1;
                let mut a: Vec<Js> = Vec::new();
                loop {
                    self.skip_ws()?;
                    if self.peek() == Some(b']') {
                        self.i += 1;
                        break;
                    }
                    a.push(self.value(depth + 1)?);
                    self.skip_ws()?;
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {}
                        _ => return self.err("očekáváno ',' nebo ']'"),
                    }
                }
                Ok(Js::Arr(a))
            }
            Some(b'"') | Some(b'\'') | Some(b'`') => Ok(Js::Str(self.string()?)),
            Some(c) if c.is_ascii_digit() || c == b'-' || c == b'.' => Ok(Js::Num(self.number()?)),
            Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let name = self.ident();
                Ok(match name.as_str() {
                    "true" => Js::Bool(true),
                    "false" => Js::Bool(false),
                    "null" | "undefined" => Js::Null,
                    _ => Js::Str(format!("${{{}}}", name)),
                })
            }
            Some(_) => self.err("neočekávaný znak"),
        }
    }

    /// primary ('+' primary)*
    fn value(&mut self, depth: usize) -> Result<Js, String> {
        let mut v = self.primary(depth)?;
        loop {
            self.skip_ws()?;
            if self.peek() != Some(b'+') {
                return Ok(v);
            }
            self.i += 1;
            let r = self.primary(depth)?;
            v = match (&v, &r) {
                (Js::Num(a), Js::Num(b)) => Js::Num(a + b),
                _ => Js::Str(v.text() + &r.text()),
            };
        }
    }
}

fn push_char(buf: &mut Vec<u8>, cp: u32) {
    let c = char::from_u32(cp).unwrap_or('\u{fffd}');
    let mut tmp = [0u8; 4];
    buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
}

/// `jmeno = hodnota;` za sebou (volitelně s `var`/`let`/`const`), komentáře se přeskočí.
pub fn parse_statements(src: &str) -> Result<Vec<(String, Js)>, String> {
    let mut p = P { b: src.as_bytes(), i: 0 };
    let mut out = Vec::new();
    loop {
        p.skip_ws()?;
        if p.i >= p.b.len() {
            return Ok(out);
        }
        let mut name = p.ident();
        if matches!(name.as_str(), "var" | "let" | "const") {
            p.skip_ws()?;
            name = p.ident();
        }
        if name.is_empty() {
            return p.err("očekáván název proměnné");
        }
        p.skip_ws()?;
        p.expect(b'=')?;
        let v = p.value(0)?;
        p.skip_ws()?;
        if p.peek() == Some(b';') {
            p.i += 1;
        }
        out.push((name, v));
    }
}

// ---------------------------------------------------------------- popup (HTML) a hodnoty v něm

pub fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
}

fn split_lines(body: &str) -> Vec<&str> {
    // <br>, <br/> i <br />
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let lower = rest.as_bytes();
        let mut found = None;
        let mut k = 0;
        while k + 3 < lower.len() {
            if lower[k] == b'<' && (lower[k + 1] | 0x20) == b'b' && (lower[k + 2] | 0x20) == b'r' {
                if let Some(e) = rest[k..].find('>') {
                    found = Some((k, k + e + 1));
                    break;
                }
            }
            k += 1;
        }
        match found {
            Some((s, e)) => {
                out.push(&rest[..s]);
                rest = &rest[e..];
            }
            None => {
                out.push(rest);
                return out;
            }
        }
    }
}

/// Hodnota z řádku `Label: hodnota` v popupu (řádky jsou odděleny `<br>`); prázdná hodnota = None.
pub fn body_field(body: &str, label: &str) -> Option<String> {
    for seg in split_lines(body) {
        let t = strip_tags(seg);
        let t = t.trim();
        if let Some(rest) = t.strip_prefix(label) {
            if let Some(v) = rest.trim_start().strip_prefix(':') {
                let v = v.trim();
                return if v.is_empty() { None } else { Some(v.to_string()) };
            }
        }
    }
    None
}

/// Popup bez řádků začínajících daným popiskem (kvůli odstranění čísla řidiče z výstupu).
pub fn drop_body_field(body: &str, label: &str) -> String {
    split_lines(body)
        .into_iter()
        .filter(|seg| !strip_tags(seg).trim().starts_with(label))
        .collect::<Vec<_>>()
        .join("<br>")
}

/// `50.78115082N, 14.22463036E` → (lat, lng). Písmeno rozhoduje (N/S = šířka, E/W = délka, S/W záporné);
/// bez písmen se bere pořadí šířka, délka.
pub fn parse_coords(s: &str) -> Option<(f64, f64)> {
    let parts: Vec<&str> = s.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    if parts.len() != 2 {
        return None;
    }
    let mut vals: [Option<f64>; 2] = [None, None]; // lat, lng
    let mut plain: Vec<f64> = Vec::new();
    for p in parts {
        let last = p.chars().last()?;
        let (num_s, kind) = match last.to_ascii_uppercase() {
            'N' | 'S' | 'E' | 'W' => (&p[..p.len() - last.len_utf8()], Some(last.to_ascii_uppercase())),
            _ => (p, None),
        };
        let n: f64 = num_s.trim().parse().ok()?;
        match kind {
            Some('N') => vals[0] = Some(n),
            Some('S') => vals[0] = Some(-n),
            Some('E') => vals[1] = Some(n),
            Some('W') => vals[1] = Some(-n),
            _ => plain.push(n),
        }
    }
    if plain.len() == 2 {
        return Some((plain[0], plain[1]));
    }
    match (vals[0], vals[1]) {
        (Some(a), Some(b)) if plain.is_empty() => Some((a, b)),
        _ => None,
    }
}

/// `10.10.2026 12:24:38` → `2026-10-10T12:24:38` (místní čas, bez zóny).
pub fn cz_datetime_to_iso(s: &str) -> Option<String> {
    let (d, t) = s.trim().split_once(' ')?;
    let dp: Vec<&str> = d.trim().split('.').collect();
    let tp: Vec<&str> = t.trim().split(':').collect();
    if dp.len() != 3 || tp.len() != 3 {
        return None;
    }
    let n = |x: &str| x.trim().parse::<u32>().ok();
    let (dd, mm, yy) = (n(dp[0])?, n(dp[1])?, n(dp[2])?);
    let (h, mi, se) = (n(tp[0])?, n(tp[1])?, n(tp[2])?);
    if !(1..=31).contains(&dd) || !(1..=12).contains(&mm) || yy < 2000 || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", yy, mm, dd, h, mi, se))
}

/// První číslo v textu (`5 km/h` → 5, `-1,5` → -1.5).
pub fn first_number(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && !b[i].is_ascii_digit() {
        i += 1;
    }
    if i >= b.len() {
        return None;
    }
    let st = if i > 0 && b[i - 1] == b'-' { i - 1 } else { i };
    let mut j = i;
    while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.' || b[j] == b',') {
        j += 1;
    }
    s[st..j].replace(',', ".").trim_end_matches('.').parse().ok()
}

/// `2 min.` → 2, `1 h 5 min.` → 65, `1:05` → 65 (zpoždění v minutách).
pub fn delay_minutes(s: &str) -> i64 {
    let t = s.trim();
    if let Some((h, m)) = t.split_once(':') {
        if let (Ok(h), Ok(m)) = (h.trim().parse::<i64>(), m.trim().chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<i64>()) {
            return h * 60 + m;
        }
    }
    let b = t.as_bytes();
    let (mut total, mut i) = (0i64, 0usize);
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let st = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let n: i64 = t[st..i].parse().unwrap_or(0);
            let unit = t[i..].trim_start().chars().next().map(|c| c.to_ascii_lowercase());
            total += if unit == Some('h') { n * 60 } else { n };
        } else {
            i += 1;
        }
    }
    total
}

// ---------------------------------------------------------------- výstup JSON

pub fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn to_json(v: &Js, out: &mut String) {
    match v {
        Js::Null => out.push_str("null"),
        Js::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Js::Num(n) if n.is_finite() => out.push_str(&num_text(*n)),
        Js::Num(_) => out.push_str("null"),
        Js::Str(s) => json_str(s, out),
        Js::Arr(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                to_json(x, out);
            }
            out.push(']');
        }
        Js::Obj(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_str(k, out);
                out.push(':');
                to_json(x, out);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"/* {"TimeStamp":"2026-10-10T12:24:37.371"} */ busstops=[{stopid1:'0000000002',coords:`50.7N, 14.2E`,}]; buspositions=[{id:'02f766f8',line:'201',vehicle:'834',driver:'1310',coords:'50.78115082N, 14.22463036E',arrow:67,fuel:'C',final1:{name:'Nemocnice',coords:'50.7N, 14.2E'},final2:null,shapes:`<gpx><name>AUT_00002</name></gpx>`,header:`<b>201</b>`,body:`<img src="`+vehicleImageUrl + `834.jpg"><br>Čas: 10.10.2026 12:24:38<br>Řidič: 1310<br>Autobus: 834<br>Typ vozu: MAN<br>SPZ: 9U51792<br>Linka: 201<br>Spoj: 231<br>Turnus: 6119<br>Konečná: Nemocnice<br>Rychlost: 5 km/h<br>Zpoždění: 2 min.<br>Dopravce: DPMD, a.s.<br>Jízdní řád<br>12:07 - Hl.nádr.<br>&darr;&nbsp;&nbsp;&darr;<br>12:30 - Nemocnice`},{id:'x',line:'',vehicle:'115',off:1,coords:'50.7N, 14.2E',body:`<br>Čas: 01.10.2026 10:00:00`}]; legends=[{c:'yellow'}]; datahash='c4bb';"#;

    #[test]
    fn parses_statements() {
        let st = parse_statements(SAMPLE).unwrap();
        let names: Vec<&str> = st.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["busstops", "buspositions", "legends", "datahash"]);
        let bp = st[1].1.as_arr().unwrap();
        assert_eq!(bp.len(), 2);
        assert_eq!(bp[0].get("line").unwrap().text(), "201");
        assert_eq!(bp[0].get("arrow").unwrap().text(), "67");
        assert_eq!(bp[0].get("final2"), Some(&Js::Null));
        let body = bp[0].get("body").unwrap().text();
        assert!(body.starts_with("<img src=\"${vehicleImageUrl}834.jpg\">"));
        assert!(bp[1].get("off").unwrap().truthy());
        assert_eq!(st[3].1.text(), "c4bb");
    }

    #[test]
    fn body_fields() {
        let st = parse_statements(SAMPLE).unwrap();
        let body = st[1].1.as_arr().unwrap()[0].get("body").unwrap().text();
        assert_eq!(body_field(&body, "Linka").as_deref(), Some("201"));
        assert_eq!(body_field(&body, "Spoj").as_deref(), Some("231"));
        assert_eq!(body_field(&body, "Konečná").as_deref(), Some("Nemocnice"));
        assert_eq!(body_field(&body, "Zpoždění").as_deref(), Some("2 min."));
        assert_eq!(body_field(&body, "Typ vozu").as_deref(), Some("MAN"));
        assert_eq!(body_field(&body, "Neexistuje"), None);
        let no_driver = drop_body_field(&body, "Řidič");
        assert!(!no_driver.contains("1310") && no_driver.contains("Spoj: 231"));
    }

    #[test]
    fn values() {
        assert_eq!(parse_coords("50.78115082N, 14.22463036E"), Some((50.78115082, 14.22463036)));
        assert_eq!(parse_coords("14.2E, 50.7N"), Some((50.7, 14.2)));
        assert_eq!(parse_coords("50.7S, 14.2W"), Some((-50.7, -14.2)));
        assert_eq!(parse_coords("50.7, 14.2"), Some((50.7, 14.2)));
        assert_eq!(parse_coords("blbost"), None);
        assert_eq!(cz_datetime_to_iso("10.10.2026 12:24:38").as_deref(), Some("2026-10-10T12:24:38"));
        assert_eq!(cz_datetime_to_iso("1.2.2026 1:02:03").as_deref(), Some("2026-02-01T01:02:03"));
        assert_eq!(cz_datetime_to_iso("32.1.2026 1:02:03"), None);
        assert_eq!(first_number("5 km/h"), Some(5.0));
        assert_eq!(first_number("12,5 km/h"), Some(12.5));
        assert_eq!(first_number("km/h"), None);
        assert_eq!(delay_minutes("2 min."), 2);
        assert_eq!(delay_minutes("1 h 5 min."), 65);
        assert_eq!(delay_minutes("1:05"), 65);
        assert_eq!(delay_minutes(""), 0);
    }

    #[test]
    fn errors_and_escapes() {
        assert!(parse_statements("a=[{b:1").is_err());
        assert!(parse_statements("a=`neuzavřený").is_err());
        assert!(parse_statements("a=@;").is_err());
        let st = parse_statements(r#"a='x\'yá\n'+1;b=-1.5e1;c=[1,2,];"#).unwrap();
        assert_eq!(st[0].1, Js::Str("x'yá\n1".into()));
        assert_eq!(st[1].1, Js::Num(-15.0));
        assert_eq!(st[2].1.as_arr().unwrap().len(), 2);
        let deep = format!("a={}{};", "[".repeat(100), "]".repeat(100));
        assert!(parse_statements(&deep).is_err());
    }

    #[test]
    fn json_out() {
        let st = parse_statements(r#"a={k:'q"\n',n:3,f:1.5,z:null,l:[true]};"#).unwrap();
        let mut s = String::new();
        to_json(&st[0].1, &mut s);
        assert_eq!(s, r#"{"k":"q\"\n","n":3,"f":1.5,"z":null,"l":[true]}"#);
    }
}
