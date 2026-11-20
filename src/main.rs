//! spoje-feed: drží poslední známé polohy vozidel z Portabo GetTraffic (DÚK) a dalších zdrojů.
//!
//! DÚK (Portabo): každých POLL_S sekund stáhne feed, zapamatuje si každé použitelné vozidlo (podle ID)
//! a když vozidlo z feedu zmizí (nebo přijde se State 255 / nulovou polohou), dál ho vrací
//! s poslední známou polohou, maximálně HOLD_*_S sekund. Výstup je {"VehicleList": [...]} + "_ageSec".
//!
//! DPMÚL (MHD Ústí nad Labem): volitelný externí konektor. Zapíná se proměnnou DPMUL_URL (bez ní se kanál
//! nedotazuje); ta ukazuje na samostatný adaptér, který vrací už hotový formát {"vehicles": [...]} (tvar
//! merge::Vehicle). Volitelný DPMUL_API_KEY se posílá jako `Authorization: Bearer`. Vozidla jdou do /api/vehicles;
//! volitelné pole "traffic" (původní odpověď) se beze změny vrací na /dpmul/traffic a /api/raw/dpmul.
//! Vypnutý nebo neodpovídající konektor = oba vrací [].
//!
//! SŽ (Správa železnic): vlaky z mapy SŽ (bez User-Agent, min. 10 s mezi dotazy, 403 = dlouhá pauza).
//!
//! DPMD (Dopravní podnik města Děčína): městské autobusy z mapy doprava.dpmdas.cz (TrafficState.js, JavaScript
//! místo JSONu, čte ho jsl.rs; gzip se rozbaluje v inflate.rs). DÚK Děčín nezná, tak se bere přímo od dopravce.
//!
//! IDOL (Liberecký kraj): autobusy a vlaky z veřejné mapy mpvnet.cz/idol/map (kvůli Česká Lípě a spojům
//! mezi Libereckým a Ústeckým krajem), viz idol.rs. Výřez se neomezuje (celá ČR), vlaky se sloučí se SŽ.
//!
//! DPKV (Karlovy Vary): městské autobusy z dopravního portálu dopravniportal.dpkv.cz (souřadnice S-JTSK se převádějí
//! na WGS84), viz dpkv.rs.
//!
//! PMDP (Plzeň): tramvaje, trolejbusy a autobusy z mapy jizdnirady.pmdp.cz/provoz přes SignalR (longPolling),
//! viz pmdp.rs a pmdp_session.
//!
//! PID (Golemio): všechny druhy dopravy PID včetně vlaků v oblasti PID. Zapíná se klíčem GOLEMIO_API_KEY (bez něj se
//! kanál nedotazuje). Vlaky PID se sloučí s vlaky SŽ (a DÚK) podle čísla vlaku, viz pid.rs a merge.rs.
//!
//! Endpointy:
//!   GET /duk/traffic, /dpmul/traffic, /health          původní (zůstávají kvůli Android appce a webu)
//!   GET /api/                                          přehled API v2
//!   GET /api/vehicles                                  všechna vozidla sloučená ze zdrojů (nejčerstvější poloha)
//!   GET /api/raw/duk | /api/raw/dpmul | /api/raw/sz | /api/raw/dpmd | /api/raw/idol | /api/raw/dpkv | /api/raw/pmdp   jednotlivé zdroje
//!   GET /api/raw/pid                                   PID: už rozebraná vozidla (Golemio je velké, surové se nedrží)
//!   GET /api/health                                    totéž co /health
//!
//! Rate limiting: kyblík žetonů na IP klienta (viz ratelimit.rs). Výchozí RATE_PER_MIN=30 a RATE_BURST=10:
//! appka s dotazem každých 5–10 s se k limitu nedostane a rezerva pokryje i dotazy mimo rozvrh
//! (start, návrat z pozadí, appka bez zdroje dat si řekne hned). /health se neomezuje. Nad limit = 429 + Retry-After.
//! Bez async runtime: pár vláken, blokující I/O.

mod dpkv;
mod dpmd;
mod pmdp;
mod idol;
mod inflate;
mod jsl;
mod merge;
mod pid;
mod ratelimit;
mod sz;

use std::{
    collections::HashMap,
    env, fs,
    io::{Cursor, Read},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    net::IpAddr,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use ratelimit::{client_ip, ip_key, RateLimiter, Verdict};
use merge::{Bbox, SourceInput, Vehicle};
use tiny_http::{Header, Method, Response, Server, StatusCode};

macro_rules! info { ($($a:tt)*) => { eprintln!("INFO {}", format!($($a)*)) } }
macro_rules! warn { ($($a:tt)*) => { eprintln!("WARN {}", format!($($a)*)) } }

const UA: &str = "spoje-feed/0.1";
const HOLD_IDLE_S: f64 = 30.0; // stavy 2/3 (nejede spoj) jen krátce
const HTTP_WORKERS: usize = 4;

// ---------------------------------------------------------------- konfigurace

struct Config {
    upstream: String,
    poll_s: f64,
    hold_mhd_s: f64,
    hold_other_s: f64,
    bind: String,
    port: u16,
    state_file: PathBuf,
    dpmul_enabled: bool,
    dpmul_url: String,
    /// tajemství: nikdy se nelogguje ani nevypisuje
    dpmul_api_key: String,
    dpmul_poll_s: f64,
    dpmul_timeout_s: f64,
    dpmul_hold_s: f64,
    sz_enabled: bool,
    sz_url: String,
    sz_poll_s: f64,
    sz_hold_s: f64,
    /// obdélník (lat_min,lng_min,lat_max,lng_max) pro vlaky SŽ; None = všechny
    sz_box: Option<Bbox>,
    dpmd_enabled: bool,
    dpmd_url: String,
    dpmd_poll_s: f64,
    dpmd_hold_s: f64,
    idol_enabled: bool,
    idol_url: String,
    idol_poll_s: f64,
    idol_hold_s: f64,
    dpkv_enabled: bool,
    dpkv_base: String,
    dpkv_poll_s: f64,
    dpkv_hold_s: f64,
    pmdp_enabled: bool,
    pmdp_base: String,
    pmdp_hold_s: f64,
    pid_enabled: bool,
    pid_url: String,
    /// tajemství: nikdy se nelogguje ani nevypisuje
    pid_key: String,
    pid_poll_s: f64,
    pid_hold_s: f64,
    /// obdélník pro PID; None = celá PID
    pid_box: Option<Bbox>,
    /// 0 = bez omezení
    rate_per_min: f64,
    rate_burst: f64,
    /// kolik proxy stojí před serverem (X-Forwarded-For); výchozí 1 při bind na loopback (typicky Caddy/nginx)
    trust_proxy: usize,
    /// IP bez omezení (čárkou oddělené), např. vlastní webový server
    rate_exempt: Vec<IpAddr>,
}

fn env_str(k: &str, d: &str) -> String {
    env::var(k).unwrap_or_else(|_| d.to_string())
}
fn env_f(k: &str, d: f64) -> f64 {
    env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d)
}

/// Obdélník z prostředí "lat_min,lng_min,lat_max,lng_max"; nenastaveno = výchozí, "off" / "0" / prázdné = bez omezení.
fn env_box(k: &str, d: Bbox) -> Option<Bbox> {
    let Ok(v) = env::var(k) else { return Some(d) };
    let t = v.trim().to_ascii_lowercase();
    if matches!(t.as_str(), "" | "off" | "none" | "0") {
        return None;
    }
    merge::parse_bbox(&t).or_else(|| {
        warn!("{} '{}' nelze přečíst (lat_min,lng_min,lat_max,lng_max), platí výchozí", k, v);
        Some(d)
    })
}

fn is_loopback_bind(b: &str) -> bool {
    b == "localhost" || b.parse::<IpAddr>().map_or(false, |ip| ip.is_loopback())
}

impl Config {
    fn from_env() -> Self {
        let bind = env_str("BIND", "127.0.0.1");
        Config {
            upstream: env_str("UPSTREAM_URL", "https://tabule.portabo.cz/api/v1-tabule/cis/GetTraffic/0"),
            poll_s: env_f("POLL_S", 10.0),
            hold_mhd_s: env_f("HOLD_MHD_S", 300.0),
            hold_other_s: env_f("HOLD_OTHER_S", 180.0),
            bind: bind.clone(),
            port: env_f("PORT", 8787.0) as u16,
            state_file: PathBuf::from(env_str("STATE_FILE", "/var/lib/spoje-feed/state.json")),
            // externí konektor (např. privátní adaptér na http://127.0.0.1:8788/vehicles); prázdné = vypnuto
            dpmul_url: env_str("DPMUL_URL", "").trim().to_string(),
            dpmul_enabled: !env_str("DPMUL_URL", "").trim().is_empty(),
            dpmul_api_key: env_str("DPMUL_API_KEY", "").trim().to_string(),
            dpmul_poll_s: env_f("DPMUL_POLL_S", 5.0),
            dpmul_timeout_s: env_f("DPMUL_TIMEOUT_S", 30.0),
            dpmul_hold_s: env_f("DPMUL_HOLD_S", 120.0),
            sz_enabled: env_str("SZ", "1") != "0",
            sz_url: env_str(
                "SZ_URL",
                "https://mapy.spravazeleznic.cz/serverside/request2.php?module=Layers%5COsVlaky&action=load",
            ),
            // SŽ banuje IP, které na ně pouští služby: nikdy rychleji než jednou za 10 s
            sz_poll_s: env_f("SZ_POLL_S", 10.5).max(10.0),
            sz_hold_s: env_f("SZ_HOLD_S", 120.0),
            // Ústecký kraj, Praha a Středočeský kraj až na jih (Příbramsko, Benešovsko)
            sz_box: env_box("SZ_BOX", [49.15, 12.8, 51.1, 15.4]),
            dpmd_enabled: env_str("DPMD", "1") != "0",
            dpmd_url: env_str("DPMD_URL", "https://tabla.dpmdas.cz/data/TrafficState.js"),
            // soubor se u DPMD generuje po 5 s; 10 s stačí (po gzipu ~35 KB na dotaz)
            dpmd_poll_s: env_f("DPMD_POLL_S", 10.0).max(5.0),
            dpmd_hold_s: env_f("DPMD_HOLD_S", 120.0),
            idol_enabled: env_str("IDOL", "1") != "0",
            idol_url: env_str("IDOL_URL", "https://mpvnet.cz/idol/map/mapData"),
            // mapa IDOL se sama obnovuje po 10 s
            idol_poll_s: env_f("IDOL_POLL_S", 10.0).max(5.0),
            idol_hold_s: env_f("IDOL_HOLD_S", 120.0),
            dpkv_enabled: env_str("DPKV", "1") != "0",
            dpkv_base: env_str("DPKV_BASE", "https://dopravniportal.dpkv.cz/dic"),
            dpkv_poll_s: env_f("DPKV_POLL_S", 10.0).max(5.0),
            dpkv_hold_s: env_f("DPKV_HOLD_S", 120.0),
            pmdp_enabled: env_str("PMDP", "1") != "0",
            pmdp_base: env_str("PMDP_BASE", "https://jizdnirady.pmdp.cz"),
            pmdp_hold_s: env_f("PMDP_HOLD_S", 120.0),
            pid_key: env_str("GOLEMIO_API_KEY", "").trim().to_string(),
            pid_enabled: env_str("PID", "1") != "0" && !env_str("GOLEMIO_API_KEY", "").trim().is_empty(),
            pid_url: env_str("PID_URL", "https://api.golemio.cz/v2/vehiclepositions?limit=10000&includeNotTracking=true"),
            // limit Golemio je 20 dotazů za 8 s na klíč, odpověď se cachuje 5 s
            pid_poll_s: env_f("PID_POLL_S", 10.0).max(5.0),
            pid_hold_s: env_f("PID_HOLD_S", 120.0),
            // permisivně: Praha a okolí, na sever až za Ústí
            pid_box: env_box("PID_BOX", [49.2, 13.0, 51.0, 15.5]),
            rate_per_min: env_f("RATE_PER_MIN", 30.0).max(0.0),
            rate_burst: env_f("RATE_BURST", 10.0).max(1.0),
            trust_proxy: env_f("TRUST_PROXY", if is_loopback_bind(&bind) { 1.0 } else { 0.0 }).max(0.0) as usize,
            rate_exempt: env_str("RATE_EXEMPT", "")
                .split(',')
                .filter_map(|s| s.trim().parse::<IpAddr>().ok())
                .collect(),
        }
    }

    fn ttl(&self, vid: i64, state: i64) -> f64 {
        let t = if is_mhd(vid) { self.hold_mhd_s } else { self.hold_other_s };
        if state == 0 || state == 1 { t } else { t.min(HOLD_IDLE_S) }
    }
}

// ---------------------------------------------------------------- sdílený stav

#[derive(Serialize, Deserialize, Clone)]
struct Item {
    e: Value,
    ts: f64,
    state: i64,
    /// stáří GPS polohy v okamžiku pozorování (z LastActivityDT); jen pro /api/vehicles
    #[serde(default)]
    gps: i64,
}

struct Duk {
    store: HashMap<i64, Item>,
    payload: Arc<[u8]>,
    last_ok: f64,
    failures: u64,
    last_error: String,
    vehicles: usize,
    held: usize,
    held_mhd: usize,
}

struct Dpmul {
    /// pole "traffic" z konektoru beze změny (pro /dpmul/traffic)
    payload: Arc<[u8]>,
    list: Vec<Vehicle>,
    list_ts: f64,
    last_ok: f64,
    failures: u64,
    last_error: String,
    vehicles: usize,
    last_fetch_ms: u64,
}

struct Sz {
    payload: Arc<[u8]>, // surová odpověď SŽ
    list: Vec<Vehicle>,
    list_ts: f64,
    last_ok: f64,
    failures: u64,
    last_error: String,
    vehicles: usize,
    blocked_until: f64,
    blocked_count: u32,
    last_request: f64,
}

struct Idol {
    payload: Arc<[u8]>, // /api/raw/idol (JSON)
    list: Vec<Vehicle>,
    list_ts: f64,
    last_ok: f64,
    failures: u64,
    last_error: String,
    vehicles: usize,
    last_fetch_ms: u64,
}

struct Dpmd {
    payload: Arc<[u8]>, // /api/raw/dpmd (JSON)
    list: Vec<Vehicle>,
    list_ts: f64,
    last_ok: f64,
    failures: u64,
    last_error: String,
    vehicles: usize, // ve službě
    last_fetch_ms: u64,
}

struct Pid {
    list: Vec<Vehicle>,
    list_ts: f64,
    last_ok: f64,
    failures: u64,
    last_error: String,
    vehicles: usize,
    features: usize,
    blocked_until: f64,
    last_fetch_ms: u64,
}

struct Stop {
    m: Mutex<bool>,
    cv: Condvar,
}

impl Stop {
    fn set(&self) {
        *lock(&self.m) = true;
        self.cv.notify_all();
    }
    fn is_set(&self) -> bool {
        *lock(&self.m)
    }
    /// Počká max `d`, vrátí true pokud byl mezitím nastaven stop.
    fn wait(&self, d: Duration) -> bool {
        let g = lock(&self.m);
        let (g, _) = self.cv.wait_timeout_while(g, d, |s| !*s).unwrap_or_else(|e| e.into_inner());
        *g
    }
}

struct Shared {
    cfg: &'static Config,
    duk: Mutex<Duk>,
    dpmul: Mutex<Dpmul>,
    sz: Mutex<Sz>,
    dpmd: Mutex<Dpmd>,
    idol: Mutex<Idol>,
    dpkv: Mutex<Idol>, // stejný stav jako IDOL (poslední seznam, raw JSON, chyby)
    pmdp: Mutex<Idol>,
    pmdp_api: Mutex<PmdpApi>,
    pid: Mutex<Pid>,
    vcache: Mutex<(f64, Arc<[u8]>)>, // sestavený /api/vehicles a kdy
    limiter: Option<RateLimiter>,
    stop: Stop,
    start: Instant,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn is_mhd(vid: i64) -> bool {
    (300000..=309999).contains(&vid)
}

// ---------------------------------------------------------------- DÚK

fn num_i(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => {
            let s = s.trim();
            s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().map(|f| f as i64))
        }
        _ => None,
    }
}

fn num_f(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// (id, state) pokud je záznam použitelný.
fn usable(e: &Value) -> Option<(i64, i64)> {
    let vid = num_i(e.get("ID")?)?;
    let state = match e.get("State") {
        None => 255,
        Some(v) => num_i(v)?,
    };
    let lat = num_f(e.get("Latitude")?)?;
    let lon = num_f(e.get("Longitude")?)?;
    if vid == 0 || state == 255 || lat == 0.0 || lon == 0.0 {
        return None;
    }
    Some((vid, state))
}

impl Duk {
    /// Přepočítá výstup a vyhodí prošlé záznamy.
    fn rebuild(&mut self, now: f64, cfg: &Config) {
        let mut out: Vec<Value> = Vec::with_capacity(self.store.len());
        let (mut held, mut held_mhd) = (0usize, 0usize);
        self.store.retain(|&vid, it| {
            let age = now - it.ts;
            if age > cfg.ttl(vid, it.state) {
                return false;
            }
            if age > cfg.poll_s * 1.5 {
                held += 1;
                if is_mhd(vid) {
                    held_mhd += 1;
                }
            }
            let mut e = it.e.clone();
            if let Value::Object(m) = &mut e {
                m.insert("_ageSec".into(), json!(age as i64));
            }
            out.push(e);
            true
        });
        let n = out.len();
        let body = json!({"VehicleList": out, "_server": {"time": now as i64, "held": held}});
        self.payload = Arc::from(serde_json::to_vec(&body).unwrap_or_else(|_| b"{\"VehicleList\":[]}".to_vec()));
        if (held, held_mhd) != (self.held, self.held_mhd) {
            info!("vozidel={}, drženo={} (z toho MHD {})", n, held, held_mhd);
        }
        self.vehicles = n;
        self.held = held;
        self.held_mhd = held_mhd;
    }
}

fn make_agent(timeout: Duration, redirects: u32) -> ureq::Agent {
    let tls = native_tls::TlsConnector::new().expect("TLS init");
    ureq::AgentBuilder::new()
        .tls_connector(Arc::new(tls))
        .timeout(timeout)
        .redirects(redirects)
        .build()
}

fn read_body(resp: ureq::Response) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    resp.into_reader().take(64 << 20).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

fn fetch_duk(agent: &ureq::Agent, cfg: &Config) -> Result<Value, String> {
    let resp = agent
        .get(&cfg.upstream)
        .set("User-Agent", UA)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| e.to_string())?;
    let body = read_body(resp)?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

fn save(sh: &Shared) {
    let json = {
        let g = lock(&sh.duk);
        serde_json::to_vec(&g.store)
    };
    let res = (|| -> Result<(), String> {
        let json = json.map_err(|e| e.to_string())?;
        let f = &sh.cfg.state_file;
        if let Some(dir) = f.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = f.with_extension("tmp");
        fs::write(&tmp, json).map_err(|e| e.to_string())?;
        fs::rename(&tmp, f).map_err(|e| e.to_string())
    })();
    if let Err(e) = res {
        warn!("uložení stavu selhalo: {}", e);
    }
}

fn load(sh: &Shared) {
    let raw = match fs::read(&sh.cfg.state_file) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => return warn!("stav nelze načíst: {}", e),
    };
    match serde_json::from_slice::<HashMap<i64, Item>>(&raw) {
        Ok(map) => {
            let mut g = lock(&sh.duk);
            g.store.extend(map);
            g.rebuild(now(), sh.cfg);
            info!("načteno {} vozidel ze stavu", g.store.len());
        }
        Err(e) => warn!("stav nelze načíst: {}", e),
    }
}

fn poller(sh: Arc<Shared>) {
    let cfg = sh.cfg;
    let agent = make_agent(Duration::from_secs(10), 5);
    let mut last_save = 0.0;
    while !sh.stop.is_set() {
        let t0 = now();
        match fetch_duk(&agent, cfg) {
            Ok(data) => {
                let t = now();
                let mut g = lock(&sh.duk);
                if let Some(list) = data.get("VehicleList").and_then(|v| v.as_array()) {
                    for e in list {
                        if let Some((vid, state)) = usable(e) {
                            g.store.insert(vid, Item { e: e.clone(), ts: t, state, gps: merge::gps_age_sec(e, t) });
                        }
                    }
                }
                g.rebuild(t, cfg);
                g.last_ok = t;
                g.failures = 0;
                g.last_error.clear();
            }
            Err(e) => {
                let mut g = lock(&sh.duk);
                g.failures += 1;
                g.last_error = e.clone();
                let n = g.failures;
                if n == 1 || n == 6 || n % 60 == 0 {
                    warn!("upstream selhal ({}x): {}", n, e);
                }
                g.rebuild(now(), cfg);
            }
        }
        if now() - last_save > 30.0 {
            save(&sh);
            last_save = now();
        }
        if sh.stop.wait(Duration::from_secs_f64((cfg.poll_s - (now() - t0)).max(0.0))) {
            break;
        }
    }
}

// ---------------------------------------------------------------- DPMÚL

fn dpmul_fetch(agent: &ureq::Agent, cfg: &Config) -> Result<Vec<u8>, String> {
    // první pokus může narazit na spojení, které server mezitím zavřel; pak se zkusí nové
    for attempt in 1..=2 {
        let t = Instant::now();
        let mut req = agent.get(&cfg.dpmul_url).set("User-Agent", UA).set("Accept", "application/json");
        if !cfg.dpmul_api_key.is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", cfg.dpmul_api_key));
        }
        match req.call() {
            Ok(resp) => {
                if resp.status() != 200 {
                    return Err(format!("HTTP {}", resp.status()));
                }
                return read_body(resp);
            }
            Err(ureq::Error::Status(c, _)) => return Err(format!("HTTP {}", c)),
            Err(e) => {
                if attempt == 2 || t.elapsed() > Duration::from_secs(2) {
                    return Err(e.to_string());
                }
            }
        }
    }
    Err("nedosažitelné".into())
}

/// Odpověď konektoru: {"vehicles": [...], "traffic": [...]?} – vozidla ve tvaru merge::Vehicle,
/// "traffic" (volitelné) je původní odpověď pro /dpmul/traffic a předává se bajt po bajtu.
#[derive(Deserialize)]
struct VehicleFeed {
    vehicles: Vec<Vehicle>,
    #[serde(default)]
    traffic: Option<Box<serde_json::value::RawValue>>,
}

fn dpmul_poller(sh: Arc<Shared>) {
    let cfg = sh.cfg;
    // Agent drží pool spojení (keep-alive), DNS a TLS se neplatí při každém dotazu
    let agent = make_agent(Duration::from_secs_f64(cfg.dpmul_timeout_s), 0);
    while !sh.stop.is_set() {
        let t0 = now();
        let res = dpmul_fetch(&agent, cfg)
            .and_then(|body| serde_json::from_slice::<VehicleFeed>(&body).map_err(|e| e.to_string()));
        match res {
            Ok(mut feed) => {
                let t = now();
                // zdroj a segment určuje tento server, ne konektor (na nich stojí slučování a uid)
                let list: Vec<Vehicle> = std::mem::take(&mut feed.vehicles)
                    .into_iter()
                    .filter(|v| v.lat.is_finite() && v.lng.is_finite() && v.lat != 0.0 && v.lng != 0.0)
                    .map(|v| Vehicle { source: "dpmul".into(), segment: "city".into(), city: true, age_sec: v.age_sec.max(0), ..v })
                    .collect();
                let traffic: Arc<[u8]> = match &feed.traffic {
                    Some(r) if r.get().trim_start().starts_with('[') => Arc::from(r.get().as_bytes()),
                    _ => Arc::from(&b"[]"[..]),
                };
                let mut g = lock(&sh.dpmul);
                g.payload = traffic;
                g.vehicles = list.len();
                g.list = list;
                g.list_ts = t;
                g.last_ok = t;
                g.failures = 0;
                g.last_error.clear();
                g.last_fetch_ms = ((t - t0) * 1000.0) as u64;
            }
            Err(e) => {
                let mut g = lock(&sh.dpmul);
                g.failures += 1;
                g.last_error = e.clone();
                let n = g.failures;
                if n == 1 || n == 6 || n % 60 == 0 {
                    warn!("DPMÚL konektor selhal ({}x): {}", n, e);
                }
            }
        }
        if sh.stop.wait(Duration::from_secs_f64((cfg.dpmul_poll_s - (now() - t0)).max(0.0))) {
            break;
        }
    }
}

/// Poslední "traffic" z konektoru DPMÚL (/dpmul/traffic, /api/raw/dpmul) a jeho stáří; vypnuto, bez dat nebo starší než DPMUL_HOLD_S = [].
fn dpmul_body(sh: &Shared) -> (Arc<[u8]>, Option<f64>) {
    let (last, body) = {
        let g = lock(&sh.dpmul);
        (g.last_ok, g.payload.clone())
    };
    if !sh.cfg.dpmul_enabled || last == 0.0 {
        return (Arc::from(&b"[]"[..]), None);
    }
    let age = now() - last;
    (if age <= sh.cfg.dpmul_hold_s { body } else { Arc::from(&b"[]"[..]) }, Some(age))
}

// ---------------------------------------------------------------- Správa železnic

fn sz_fail(g: &mut Sz, msg: String) {
    g.failures += 1;
    let n = g.failures;
    if n == 1 || n == 6 || n % 60 == 0 {
        warn!("SŽ selhala ({}x): {}", n, msg);
    }
    g.last_error = msg;
}

fn sz_poll_once(sh: &Shared) {
    let cfg = sh.cfg;
    let t = now();
    {
        let mut g = lock(&sh.sz);
        if t < g.blocked_until || t - g.last_request < 10.0 {
            return;
        }
        g.last_request = t;
    }
    let res = sz::http_get(&cfg.sz_url, Duration::from_secs(10));
    let mut g = lock(&sh.sz);
    match res {
        Ok((403, _)) => {
            // 10 min, pak 20, 40, max. 60 min
            let pause = (600.0 * 2f64.powi(g.blocked_count.min(3) as i32)).min(3600.0);
            g.blocked_count += 1;
            g.blocked_until = t + pause;
            sz_fail(&mut g, format!("SŽ vrátila 403, pauza {} s", pause as i64));
        }
        Ok((st, _)) if !(200..300).contains(&st) => sz_fail(&mut g, format!("HTTP {st}")),
        Ok((_, body)) => match serde_json::from_slice::<Value>(&body) {
            Ok(v) => {
                let list = sz::parse_sz(&v, cfg.sz_box.as_ref());
                g.vehicles = list.len();
                g.list = list;
                g.payload = Arc::from(body);
                g.list_ts = t;
                g.last_ok = t;
                g.blocked_count = 0;
                g.failures = 0;
                g.last_error.clear();
            }
            Err(e) => sz_fail(&mut g, e.to_string()),
        },
        Err(e) => sz_fail(&mut g, e),
    }
}

fn sz_poller(sh: Arc<Shared>) {
    while !sh.stop.is_set() {
        sz_poll_once(&sh);
        if sh.stop.wait(Duration::from_secs_f64(sh.cfg.sz_poll_s)) {
            break;
        }
    }
}

/// Surová odpověď SŽ a její stáří; starší než SZ_HOLD_S (nebo vypnutá / ještě žádná) = prázdný výsledek.
fn sz_body(sh: &Shared) -> (Arc<[u8]>, Option<f64>) {
    let empty: Arc<[u8]> = Arc::from(&b"{\"result\":[]}"[..]);
    if !sh.cfg.sz_enabled {
        return (empty, None);
    }
    let (last, body) = {
        let g = lock(&sh.sz);
        (g.last_ok, g.payload.clone())
    };
    if last == 0.0 {
        return (empty, None);
    }
    let age = now() - last;
    (if age <= sh.cfg.sz_hold_s { body } else { empty }, Some(age))
}

// ---------------------------------------------------------------- IDOL (Liberecký kraj)

const IDOL_EMPTY: &[u8] = b"{\"transmitters\":[]}";

fn idol_fetch(agent: &ureq::Agent, cfg: &Config) -> Result<String, String> {
    // výřez celé ČR: IDOL vrací všechny své vozy (i spoje mimo kraj), nic se neořezává
    let body = r#"{"w":12.0,"s":48.5,"e":19.0,"n":51.1,"zoom":10,"showStops":false,"mapFilterId":null}"#;
    let resp = agent
        .post(&cfg.idol_url)
        .set("User-Agent", UA)
        .set("Content-Type", "application/json; charset=utf-8")
        .set("Accept", "application/json")
        // bez Referer / Origin mapy odpovídá 400
        .set("Referer", "https://mpvnet.cz/idol/map")
        .set("Origin", "https://mpvnet.cz")
        .set("Accept-Encoding", "gzip")
        .send_string(body)
        .map_err(|e| e.to_string())?;
    let gz = resp.header("Content-Encoding").map_or(false, |v| v.to_ascii_lowercase().contains("gzip"));
    let body = read_body(resp)?;
    let body = if gz || body.starts_with(&[0x1f, 0x8b]) { inflate::gunzip(&body, 16 << 20)? } else { body };
    String::from_utf8(body).map_err(|e| e.to_string())
}

fn idol_poller(sh: Arc<Shared>) {
    let cfg = sh.cfg;
    let agent = make_agent(Duration::from_secs(15), 3);
    while !sh.stop.is_set() {
        let t0 = now();
        match idol_fetch(&agent, cfg).and_then(|src| idol::parse(&src, (now() * 1000.0) as i64)) {
            Ok(snap) => {
                let t = now();
                let mut g = lock(&sh.idol);
                g.vehicles = snap.vehicles.len();
                g.list = snap.vehicles;
                g.payload = Arc::from(snap.raw);
                g.list_ts = t;
                g.last_ok = t;
                g.failures = 0;
                g.last_error.clear();
                g.last_fetch_ms = ((t - t0) * 1000.0) as u64;
            }
            Err(e) => {
                let mut g = lock(&sh.idol);
                g.failures += 1;
                g.last_error = e.clone();
                let n = g.failures;
                if n == 1 || n == 6 || n % 60 == 0 {
                    warn!("IDOL selhal ({}x): {}", n, e);
                }
            }
        }
        if sh.stop.wait(Duration::from_secs_f64((cfg.idol_poll_s - (now() - t0)).max(0.0))) {
            break;
        }
    }
}

/// Poslední odpověď IDOL a její stáří; starší než IDOL_HOLD_S (nebo vypnuté / ještě žádná) = prázdná.
fn idol_body(sh: &Shared) -> (Arc<[u8]>, Option<f64>) {
    let empty: Arc<[u8]> = Arc::from(IDOL_EMPTY);
    if !sh.cfg.idol_enabled {
        return (empty, None);
    }
    let (last, body) = {
        let g = lock(&sh.idol);
        (g.last_ok, g.payload.clone())
    };
    if last == 0.0 {
        return (empty, None);
    }
    let age = now() - last;
    (if age <= sh.cfg.idol_hold_s { body } else { empty }, Some(age))
}

// ---------------------------------------------------------------- DPKV (Karlovy Vary)

const DPKV_EMPTY: &[u8] = b"{\"vehicles\":[]}";

/// Polohy (GetFeatures, JSON) a tabulka právě jedoucích spojů (HTML) z dopravního portálu DPKV.
fn dpkv_fetch(agent: &ureq::Agent, cfg: &Config) -> Result<(String, String), String> {
    let referer = format!("{}/", cfg.dpkv_base);
    // výřez celé ČR v S-JTSK (EPSG:5514): portál vrací všechny své vozy, nic se neořezává
    let body = r#"{"scale":40000,"resolution":10.58,"extent":{"xmin":-910000,"xmax":-430000,"ymin":-1230000,"ymax":-930000},"layerFilterList":[{"id":"RT spoje point"}]}"#;
    let read = |resp: ureq::Response| -> Result<String, String> {
        let gz = resp.header("Content-Encoding").map_or(false, |v| v.to_ascii_lowercase().contains("gzip"));
        let b = read_body(resp)?;
        let b = if gz || b.starts_with(&[0x1f, 0x8b]) { inflate::gunzip(&b, 16 << 20)? } else { b };
        String::from_utf8(b).map_err(|e| e.to_string())
    };
    let features = agent
        .post(&format!("{}/FeatureService/GetFeatures", cfg.dpkv_base))
        .set("User-Agent", UA)
        .set("Content-Type", "application/json; charset=utf-8")
        .set("Referer", &referer)
        .set("Accept-Encoding", "gzip")
        .send_string(body)
        .map_err(|e| e.to_string())
        .and_then(read)?;
    let table = agent
        .get(&format!("{}/TabularData/RTConn?order=undefined&dir=undefined", cfg.dpkv_base))
        .set("User-Agent", UA)
        .set("Referer", &referer)
        .set("Accept-Encoding", "gzip")
        .call()
        .map_err(|e| e.to_string())
        .and_then(read)?;
    Ok((features, table))
}

fn dpkv_poller(sh: Arc<Shared>) {
    let cfg = sh.cfg;
    let agent = make_agent(Duration::from_secs(15), 3);
    while !sh.stop.is_set() {
        let t0 = now();
        match dpkv_fetch(&agent, cfg).and_then(|(f, t)| dpkv::parse(&f, &t)) {
            Ok(snap) => {
                let t = now();
                let mut g = lock(&sh.dpkv);
                g.vehicles = snap.vehicles.len();
                g.list = snap.vehicles;
                g.payload = Arc::from(snap.raw);
                g.list_ts = t;
                g.last_ok = t;
                g.failures = 0;
                g.last_error.clear();
                g.last_fetch_ms = ((t - t0) * 1000.0) as u64;
            }
            Err(e) => {
                let mut g = lock(&sh.dpkv);
                g.failures += 1;
                g.last_error = e.clone();
                let n = g.failures;
                if n == 1 || n == 6 || n % 60 == 0 {
                    warn!("DPKV selhal ({}x): {}", n, e);
                }
            }
        }
        if sh.stop.wait(Duration::from_secs_f64((cfg.dpkv_poll_s - (now() - t0)).max(0.0))) {
            break;
        }
    }
}

fn dpkv_body(sh: &Shared) -> (Arc<[u8]>, Option<f64>) {
    let empty: Arc<[u8]> = Arc::from(DPKV_EMPTY);
    if !sh.cfg.dpkv_enabled {
        return (empty, None);
    }
    let (last, body) = {
        let g = lock(&sh.dpkv);
        (g.last_ok, g.payload.clone())
    };
    if last == 0.0 {
        return (empty, None);
    }
    let age = now() - last;
    (if age <= sh.cfg.dpkv_hold_s { body } else { empty }, Some(age))
}

// ---------------------------------------------------------------- PMDP (Plzeň), SignalR longPolling

const PMDP_EMPTY: &[u8] = b"{\"vehicles\":[]}";

/// Procentové kódování pro query string a formulář (vše kromě nerezervovaných znaků).
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Jedno spojení se SignalR hubem PMDP: negotiate → connect → start → registrace(null) → poll ve smyčce.
/// Vrátí se při zastavení serveru, jinak chybou (odpojení, výpadek, dlouho bez dat) a poller se připojí znovu.
fn pmdp_session(agent: &ureq::Agent, sh: &Shared) -> Result<(), String> {
    let base = &sh.cfg.pmdp_base;
    let referer = format!("{}/provoz", base);
    let cd = urlenc(r#"[{"name":"provozhub"}]"#);
    let call = |url: &str, form: Option<&str>| -> Result<serde_json::Value, String> {
        let req = if form.is_some() { agent.post(url) } else { agent.get(url) };
        let req = req.set("User-Agent", UA).set("Referer", &referer);
        let resp = match form {
            Some(b) => req.set("Content-Type", "application/x-www-form-urlencoded; charset=UTF-8").send_string(b),
            None => req.call(),
        }
        .map_err(|e| e.to_string())?;
        let body = read_body(resp)?;
        serde_json::from_slice(&body).map_err(|e| format!("JSON: {e}"))
    };
    let neg = call(&format!("{base}/signalr/negotiate?clientProtocol=1.5&connectionData={cd}"), None)?;
    let token = neg.get("ConnectionToken").and_then(|v| v.as_str()).ok_or("negotiate bez ConnectionToken")?;
    let q = format!("transport=longPolling&clientProtocol=1.5&connectionToken={}&connectionData={}", urlenc(token), cd);
    let c = call(&format!("{base}/signalr/connect?{q}"), Some(""))?;
    let mut mid = c.get("C").and_then(|v| v.as_str()).unwrap_or("").to_string();
    call(&format!("{base}/signalr/start?{q}"), None)?;
    // null = všechny linky (jako mapa bez výběru linek)
    let msg = r#"{"H":"provozhub","M":"registrace","A":[null],"I":0}"#;
    call(&format!("{base}/signalr/send?{q}"), Some(&format!("data={}", urlenc(msg))))?;
    let mut last_update = now();
    loop {
        if sh.stop.is_set() {
            return Ok(());
        }
        let r = call(&format!("{base}/signalr/poll?{q}&messageId={}", urlenc(&mid)), Some(""))?;
        let t = now();
        {
            let mut g = lock(&sh.pmdp);
            g.last_ok = t;
            g.failures = 0;
            g.last_error.clear();
        }
        // D = server spojení ukončil, T = klient se má připojit znovu
        if r.get("D").and_then(|v| v.as_i64()) == Some(1) || r.get("T").and_then(|v| v.as_i64()) == Some(1) {
            return Err("server požádal o nové spojení".into());
        }
        if let Some(c) = r.get("C").and_then(|v| v.as_str()) {
            mid = c.to_string();
        }
        for m in r.get("M").and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[]) {
            if m.get("M").and_then(|v| v.as_str()) != Some("updateTraffic") {
                continue;
            }
            if let Some(list) = m.get("A").and_then(|a| a.get(0)).and_then(|l| l.as_array()) {
                let vs = pmdp::parse_traffic(list, (t * 1000.0) as i64);
                let raw = serde_json::to_vec(&json!({ "vehicles": vs })).unwrap_or_default();
                let mut g = lock(&sh.pmdp);
                g.vehicles = vs.len();
                g.list = vs;
                g.payload = Arc::from(raw);
                g.list_ts = t;
                last_update = t;
            }
        }
        // stav chodí po 2–8 s; dlouho nic = spojení se zaseklo, připojit znovu
        if t - last_update > 90.0 {
            return Err("90 s bez updateTraffic".into());
        }
    }
}

fn pmdp_poller(sh: Arc<Shared>) {
    // long poll drží spojení až ~110 s (ConnectionTimeout serveru)
    let agent = make_agent(Duration::from_secs(130), 3);
    while !sh.stop.is_set() {
        if let Err(e) = pmdp_session(&agent, &sh) {
            let n = {
                let mut g = lock(&sh.pmdp);
                g.failures += 1;
                g.last_error = e.clone();
                g.failures
            };
            if n == 1 || n == 6 || n % 60 == 0 {
                warn!("PMDP selhal ({}x): {}", n, e);
            }
            let pause = (5 * n).min(60) as f64;
            if sh.stop.wait(Duration::from_secs_f64(pause)) {
                break;
            }
        }
    }
}

/// Relace webu PMDP pro dotazy na trasu spoje (token ze stránky /provoz + cookie ASP.NET) a cache tras.
struct PmdpApi {
    token: String,
    cookie: String,
    token_at: f64,
    /// ConnectionId → (kdy, odpověď {"trip": …})
    trips: HashMap<String, (f64, Arc<[u8]>)>,
}

const PMDP_TRIP_TTL_S: f64 = 600.0;
const PMDP_TOKEN_TTL_S: f64 = 1800.0;

fn pmdp_agent() -> &'static ureq::Agent {
    static A: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    A.get_or_init(|| make_agent(Duration::from_secs(12), 3))
}

/// Nová relace: stránka /provoz → token z hidden pole + cookies.
fn pmdp_new_session(base: &str) -> Result<(String, String), String> {
    let resp = pmdp_agent().get(&format!("{base}/provoz")).set("User-Agent", UA).call().map_err(|e| e.to_string())?;
    let cookie = resp
        .all("set-cookie")
        .iter()
        .filter_map(|c| c.split(';').next().map(|x| x.trim().to_string()))
        .filter(|c| c.contains('='))
        .collect::<Vec<_>>()
        .join("; ");
    let html = String::from_utf8_lossy(&read_body(resp)?).into_owned();
    let key = "name='X-API-Request-Verification-Token' value='";
    let i = html.find(key).ok_or("token na stránce není")? + key.len();
    let j = html[i..].find('\'').ok_or("token bez konce")? + i;
    Ok((html[i..j].to_string(), cookie))
}

fn pmdp_fetch_trip(sh: &Shared, conn: &str, token: &str, cookie: &str) -> Result<serde_json::Value, String> {
    let base = &sh.cfg.pmdp_base;
    let body = format!(r#"{{"SegmentId":{{"ConnectionId":{conn}}},"IncludeDetailedPath":true}}"#);
    let resp = pmdp_agent()
        .post(&format!("{base}/provoz/trasaspoje"))
        .set("User-Agent", UA)
        .set("Referer", &format!("{base}/provoz"))
        .set("Content-Type", "application/json; charset=UTF-8")
        .set("X-Requested-With", "XMLHttpRequest")
        .set("X-API-Request-Verification-Token", token)
        .set("Cookie", cookie)
        .send_string(&body)
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_slice(&read_body(resp)?).map_err(|e| format!("JSON: {e}"))?;
    pmdp::parse_path(&v)
}

/// /api/trip/pmdp?conn=<ConnectionId>: zastávky a tvar trasy spoje PMDP ve tvaru /api/vehicle-trip webu.
fn pmdp_trip(sh: &Shared, conn: &str) -> Result<Arc<[u8]>, (u16, String)> {
    if !sh.cfg.pmdp_enabled {
        return Err((404, "PMDP vypnuto".into()));
    }
    let ok_id = !conn.is_empty() && conn.len() <= 14 && conn.trim_start_matches('-').bytes().all(|b| b.is_ascii_digit());
    if !ok_id || conn.trim_start_matches('-').is_empty() {
        return Err((400, "conn musí být číslo spoje (ConnectionId)".into()));
    }
    let t = now();
    let (mut token, mut cookie) = {
        let g = lock(&sh.pmdp_api);
        if let Some((at, b)) = g.trips.get(conn) {
            if t - at < PMDP_TRIP_TTL_S {
                return Ok(b.clone());
            }
        }
        let fresh = !g.token.is_empty() && t - g.token_at < PMDP_TOKEN_TTL_S;
        if fresh { (g.token.clone(), g.cookie.clone()) } else { (String::new(), String::new()) }
    };
    let mut result = Err(String::new());
    // nejvýš dva pokusy: s uloženou relací a s novou (vypršelý token / cookie)
    for attempt in 0..2 {
        if token.is_empty() || attempt == 1 {
            let (tk, ck) = pmdp_new_session(&sh.cfg.pmdp_base).map_err(|e| (502, format!("relace PMDP: {e}")))?;
            let mut g = lock(&sh.pmdp_api);
            g.token = tk.clone();
            g.cookie = ck.clone();
            g.token_at = now();
            token = tk;
            cookie = ck;
        }
        result = pmdp_fetch_trip(sh, conn, &token, &cookie);
        if result.is_ok() {
            break;
        }
    }
    let trip = result.map_err(|e| (502, format!("trasa PMDP: {e}")))?;
    let body: Arc<[u8]> = Arc::from(serde_json::to_vec(&json!({ "trip": trip })).unwrap_or_default());
    let mut g = lock(&sh.pmdp_api);
    if g.trips.len() > 500 {
        g.trips.retain(|_, (at, _)| t - *at < PMDP_TRIP_TTL_S);
        if g.trips.len() > 500 {
            g.trips.clear();
        }
    }
    g.trips.insert(conn.to_string(), (t, body.clone()));
    Ok(body)
}

fn pmdp_body(sh: &Shared) -> (Arc<[u8]>, Option<f64>) {
    let empty: Arc<[u8]> = Arc::from(PMDP_EMPTY);
    if !sh.cfg.pmdp_enabled {
        return (empty, None);
    }
    let (last, body) = {
        let g = lock(&sh.pmdp);
        (g.list_ts, g.payload.clone())
    };
    if last == 0.0 {
        return (empty, None);
    }
    let age = now() - last;
    (if age <= sh.cfg.pmdp_hold_s { body } else { empty }, Some(age))
}

// ---------------------------------------------------------------- DPMD (Děčín)

const DPMD_EMPTY: &[u8] = b"{\"TimeStamp\":null,\"buspositions\":[]}";

fn dpmd_fetch(agent: &ureq::Agent, cfg: &Config) -> Result<String, String> {
    // v = cache buster: bez něj openresty vrací kopii starou i 5 minut
    let url = format!("{}?v={}", cfg.dpmd_url, (now() * 1000.0) as u64);
    let resp = agent
        .get(&url)
        .set("User-Agent", UA)
        .set("Accept-Encoding", "gzip")
        .call()
        .map_err(|e| e.to_string())?;
    let gz = resp.header("Content-Encoding").map_or(false, |v| v.to_ascii_lowercase().contains("gzip"));
    let body = read_body(resp)?;
    let body = if gz || body.starts_with(&[0x1f, 0x8b]) { inflate::gunzip(&body, 16 << 20)? } else { body };
    String::from_utf8(body).map_err(|e| e.to_string())
}

fn dpmd_poller(sh: Arc<Shared>) {
    let cfg = sh.cfg;
    let agent = make_agent(Duration::from_secs(15), 3);
    while !sh.stop.is_set() {
        let t0 = now();
        let res = dpmd_fetch(&agent, cfg).and_then(|src| {
            let snap = dpmd::parse(&src, (now() * 1000.0) as i64)?;
            // useknutý nebo cachovaný soubor: TimeStamp starší než 90 s se nebere
            if let Some(st) = snap.stamp_ms {
                let age = (now() * 1000.0) as i64 / 1000 - st / 1000;
                if age > 90 {
                    return Err(format!("zastaralá odpověď (TimeStamp {} s)", age));
                }
            }
            Ok(snap)
        });
        match res {
            Ok(snap) => {
                let t = now();
                let mut g = lock(&sh.dpmd);
                g.vehicles = snap.in_service;
                g.list = snap.vehicles;
                g.payload = Arc::from(snap.raw);
                g.list_ts = t;
                g.last_ok = t;
                g.failures = 0;
                g.last_error.clear();
                g.last_fetch_ms = ((t - t0) * 1000.0) as u64;
            }
            Err(e) => {
                let mut g = lock(&sh.dpmd);
                g.failures += 1;
                g.last_error = e.clone();
                let n = g.failures;
                if n == 1 || n == 6 || n % 60 == 0 {
                    warn!("DPMD selhal ({}x): {}", n, e);
                }
            }
        }
        if sh.stop.wait(Duration::from_secs_f64((cfg.dpmd_poll_s - (now() - t0)).max(0.0))) {
            break;
        }
    }
}

/// Poslední odpověď DPMD (JSON bez řidičů) a její stáří; starší než DPMD_HOLD_S (nebo vypnuté / ještě žádná) = prázdná.
fn dpmd_body(sh: &Shared) -> (Arc<[u8]>, Option<f64>) {
    let empty: Arc<[u8]> = Arc::from(DPMD_EMPTY);
    if !sh.cfg.dpmd_enabled {
        return (empty, None);
    }
    let (last, body) = {
        let g = lock(&sh.dpmd);
        (g.last_ok, g.payload.clone())
    };
    if last == 0.0 {
        return (empty, None);
    }
    let age = now() - last;
    (if age <= sh.cfg.dpmd_hold_s { body } else { empty }, Some(age))
}

// ---------------------------------------------------------------- PID (Golemio)

/// Chyba s HTTP stavem (je-li), aby se 401/403/429 dalo odlišit od výpadku sítě.
type PidErr = (Option<u16>, String);

fn pid_fetch(agent: &ureq::Agent, cfg: &Config) -> Result<Value, PidErr> {
    let resp = agent
        .get(&cfg.pid_url)
        .set("X-Access-Token", &cfg.pid_key)
        .set("Accept", "application/json")
        .set("Accept-Encoding", "gzip")
        .set("User-Agent", UA)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(c, _) => (Some(c), format!("HTTP {c}")),
            e => (None, e.to_string()),
        })?;
    let gz = resp.header("Content-Encoding").map_or(false, |v| v.to_ascii_lowercase().contains("gzip"));
    let body = read_body(resp).map_err(|e| (None, e))?;
    let body = if gz || body.starts_with(&[0x1f, 0x8b]) { inflate::gunzip(&body, 128 << 20).map_err(|e| (None, e))? } else { body };
    serde_json::from_slice(&body).map_err(|e| (None, e.to_string()))
}

fn pid_poller(sh: Arc<Shared>) {
    let cfg = sh.cfg;
    let agent = make_agent(Duration::from_secs(30), 0);
    while !sh.stop.is_set() {
        let t0 = now();
        let blocked = lock(&sh.pid).blocked_until > t0;
        if !blocked {
            let res = pid_fetch(&agent, cfg).map(|data| {
                let features = data.get("features").and_then(|f| f.as_array()).map_or(0, |a| a.len());
                (pid::parse(&data, (now() * 1000.0) as i64, cfg.pid_box.as_ref()), features)
            });
            let t = now();
            let mut g = lock(&sh.pid);
            match res {
                Ok((list, features)) => {
                    g.vehicles = list.len();
                    g.features = features;
                    g.list = list;
                    g.list_ts = t;
                    g.last_ok = t;
                    g.failures = 0;
                    g.last_error.clear();
                    g.last_fetch_ms = ((t - t0) * 1000.0) as u64;
                }
                Err((status, e)) => {
                    g.failures += 1;
                    // špatný klíč / překročený limit: nebušit, počkat
                    let pause = match status {
                        Some(401) | Some(403) => 300.0,
                        Some(429) => 30.0,
                        _ => 0.0,
                    };
                    if pause > 0.0 {
                        g.blocked_until = t + pause;
                    }
                    g.last_error = e.clone();
                    let n = g.failures;
                    if n == 1 || n == 6 || n % 60 == 0 {
                        warn!("PID selhal ({}x): {}{}", n, e, if pause > 0.0 { format!(", pauza {} s", pause as i64) } else { String::new() });
                    }
                }
            }
        }
        if sh.stop.wait(Duration::from_secs_f64((cfg.pid_poll_s - (now() - t0)).max(0.0))) {
            break;
        }
    }
}

/// /api/raw/pid: poslední rozebraná vozidla a stáří; starší než PID_HOLD_S (nebo vypnuté / ještě žádná) = prázdná.
fn pid_body(sh: &Shared) -> (Arc<[u8]>, Option<f64>) {
    let empty: Arc<[u8]> = Arc::from(&b"{\"vehicles\":[]}"[..]);
    if !sh.cfg.pid_enabled {
        return (empty, None);
    }
    let g = lock(&sh.pid);
    if g.last_ok == 0.0 {
        return (empty, None);
    }
    let age = now() - g.last_ok;
    if age > sh.cfg.pid_hold_s {
        return (empty, Some(age));
    }
    let body = serde_json::to_vec(&json!({"vehicles": &g.list})).map(Arc::from).unwrap_or(empty);
    (body, Some(age))
}

// ---------------------------------------------------------------- sloučená vozidla (/api/vehicles)

fn build_vehicles(sh: &Shared) -> Arc<[u8]> {
    let cfg = sh.cfg;
    let t = now();
    let mut inputs: Vec<SourceInput> = Vec::with_capacity(5);

    // DPMÚL: poslední odpověď se při výpadku drží nejvýš 60 s (stáří poloh roste)
    if cfg.dpmul_enabled {
        let g = lock(&sh.dpmul);
        let held_s = (t - g.list_ts).floor() as i64;
        let vehicles: Vec<Vehicle> = if g.list_ts > 0.0 && held_s <= 60 {
            g.list.iter().map(|v| Vehicle { age_sec: v.age_sec + held_s, ..v.clone() }).collect()
        } else {
            Vec::new()
        };
        inputs.push(SourceInput {
            id: "dpmul",
            name: "DPMÚL",
            segments: &["city"],
            health: merge::health_of(g.last_ok, t, 60.0),
            stale_vehicle_sec: 90,
            vehicles,
            held: 0,
            failures: g.failures,
            last_error: g.last_error.clone(),
        });
    }

    // DPMD (Děčín): vlastní číslování a žádný jiný zdroj tu nejezdí. Merge ho nikdy nezahazuje jako "ducha"
    // a prázdné segments zajistí, aby nemaskoval zastaralé vozy ostatních zdrojů.
    if cfg.dpmd_enabled {
        let g = lock(&sh.dpmd);
        let held_s = (t - g.list_ts).floor() as i64;
        let vehicles: Vec<Vehicle> = if g.list_ts > 0.0 && held_s <= 60 {
            g.list.iter().map(|v| Vehicle { age_sec: v.age_sec + held_s, ..v.clone() }).collect()
        } else {
            Vec::new()
        };
        inputs.push(SourceInput {
            id: "dpmd",
            name: "DPMD",
            segments: &[],
            health: merge::health_of(g.last_ok, t, 60.0),
            stale_vehicle_sec: 90,
            vehicles,
            held: 0,
            failures: g.failures,
            last_error: g.last_error.clone(),
        });
    }

    // IDOL (Liberecký kraj): autobusy mají vlastní identitu (evidenční číslo), s jinými zdroji se neslučují a nezahazují
    // se jako "duchové"; prázdné segments = nemaskuje zastaralé vozy jiných zdrojů. Vlaky se slučují podle čísla.
    if cfg.idol_enabled {
        let g = lock(&sh.idol);
        let held_s = (t - g.list_ts).floor() as i64;
        let vehicles: Vec<Vehicle> = if g.list_ts > 0.0 && held_s <= 60 {
            g.list.iter().map(|v| Vehicle { age_sec: v.age_sec + held_s, ..v.clone() }).collect()
        } else {
            Vec::new()
        };
        inputs.push(SourceInput {
            id: "idol",
            name: "IDOL",
            segments: &[],
            health: merge::health_of(g.last_ok, t, 60.0),
            stale_vehicle_sec: 90,
            vehicles,
            held: 0,
            failures: g.failures,
            last_error: g.last_error.clone(),
        });
    }

    // DPKV (Karlovy Vary): vlastní číslování, nikdo jiný tam nejezdí; jako DPMD se neslučuje, nemaskuje a nemaže
    if cfg.dpkv_enabled {
        let g = lock(&sh.dpkv);
        let held_s = (t - g.list_ts).floor() as i64;
        let vehicles: Vec<Vehicle> = if g.list_ts > 0.0 && held_s <= 60 {
            g.list.iter().map(|v| Vehicle { age_sec: v.age_sec + held_s, ..v.clone() }).collect()
        } else {
            Vec::new()
        };
        inputs.push(SourceInput {
            id: "dpkv",
            name: "DPKV",
            segments: &[],
            health: merge::health_of(g.last_ok, t, 60.0),
            stale_vehicle_sec: 90,
            vehicles,
            held: 0,
            failures: g.failures,
            last_error: g.last_error.clone(),
        });
    }

    // PMDP (Plzeň): vlastní číslování, nikdo jiný tam nejezdí; neslučuje se, nemaskuje a nemaže
    if cfg.pmdp_enabled {
        let g = lock(&sh.pmdp);
        let held_s = (t - g.list_ts).floor() as i64;
        let vehicles: Vec<Vehicle> = if g.list_ts > 0.0 && held_s <= 60 {
            g.list.iter().map(|v| Vehicle { age_sec: v.age_sec + held_s, ..v.clone() }).collect()
        } else {
            Vec::new()
        };
        inputs.push(SourceInput {
            id: "pmdp",
            name: "PMDP",
            segments: &[],
            health: merge::health_of(g.list_ts, t, 60.0),
            stale_vehicle_sec: 90,
            vehicles,
            held: 0,
            failures: g.failures,
            last_error: g.last_error.clone(),
        });
    }

    // DÚK: držená vozidla z feedu (stáří = stáří GPS při pozorování + jak dlouho ji držíme)
    let (duk_outage, duk_last_ok) = {
        let g = lock(&sh.duk);
        let mut vehicles = Vec::with_capacity(g.store.len());
        let mut held = 0usize;
        for (&vid, it) in g.store.iter() {
            let since = t - it.ts;
            if since > cfg.ttl(vid, it.state) {
                continue;
            }
            let limit = if merge::is_mhd(vid) { 45 * 60 } else { 10 * 60 };
            if it.gps > limit {
                continue;
            }
            if since > cfg.poll_s * 1.5 {
                held += 1;
            }
            if let Some(v) = merge::duk_vehicle(&it.e, it.gps + since.floor().max(0.0) as i64) {
                vehicles.push(v);
            }
        }
        let outage = held > 18 || g.last_ok <= 0.0 || (t - g.last_ok).floor() > 60.0;
        inputs.push(SourceInput {
            id: "duk",
            name: "DÚK",
            segments: &["city", "regional"],
            health: merge::health_of(g.last_ok, t, 30.0),
            stale_vehicle_sec: 40,
            vehicles,
            held,
            failures: g.failures,
            last_error: g.last_error.clone(),
        });
        (outage, g.last_ok)
    };

    // PID (Golemio): nepokrývá žádný segment (segments prázdné), takže nezahazuje zastaralé vozy jiných zdrojů;
    // jeho vlastní osamělé zastaralé vozy (regionální autobusy, vlaky) zahodí jiný živý zdroj stejného segmentu.
    if cfg.pid_enabled {
        let g = lock(&sh.pid);
        let held_s = (t - g.list_ts).floor() as i64;
        let vehicles: Vec<Vehicle> = if g.list_ts > 0.0 && held_s <= 60 {
            g.list.iter().map(|v| Vehicle { age_sec: v.age_sec + held_s, ..v.clone() }).collect()
        } else {
            Vec::new()
        };
        inputs.push(SourceInput {
            id: "pid",
            name: "PID",
            segments: &[],
            health: merge::health_of(g.last_ok, t, 40.0),
            stale_vehicle_sec: 90,
            vehicles,
            held: 0,
            failures: g.failures,
            last_error: g.last_error.clone(),
        });
    }

    // SŽ: při pauze / výpadku se poslední data drží jen 2 minuty
    if cfg.sz_enabled {
        let g = lock(&sh.sz);
        let held_s = (t - g.list_ts).floor() as i64;
        let vehicles: Vec<Vehicle> = if g.list_ts > 0.0 && held_s <= 120 {
            g.list.iter().map(|v| Vehicle { age_sec: held_s, ..v.clone() }).collect()
        } else {
            Vec::new()
        };
        inputs.push(SourceInput {
            id: "train",
            name: "Správa železnic",
            segments: &["regional"],
            health: merge::health_of(g.last_ok, t, 40.0),
            stale_vehicle_sec: 40,
            vehicles,
            held: 0,
            failures: g.failures,
            last_error: g.last_error.clone(),
        });
    }

    Arc::from(merge::snapshot(&inputs, t, duk_outage, duk_last_ok))
}

/// Hotový JSON se drží 1 s: zátěž nezávisí na počtu požadavků.
fn vehicles_json(sh: &Shared) -> Arc<[u8]> {
    let mut c = lock(&sh.vcache);
    let t = now();
    if c.1.is_empty() || t - c.0 > 1.0 {
        c.1 = build_vehicles(sh);
        c.0 = t;
    }
    c.1.clone()
}

// ---------------------------------------------------------------- HTTP

fn hdr(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn health(sh: &Shared) -> Arc<[u8]> {
    let t = now();
    let age = |last: f64| if last > 0.0 { Some(t - last) } else { None };
    let ok = |a: Option<f64>| a.map_or(false, |a| a < 60.0);

    let (duk, duk_ok) = {
        let g = lock(&sh.duk);
        let a = age(g.last_ok);
        (
            json!({"lastOk": g.last_ok, "failures": g.failures, "lastError": g.last_error,
                   "vehicles": g.vehicles, "held": g.held, "heldMhd": g.held_mhd,
                   "ok": ok(a), "upstreamAgeSec": a.map(|a| a as i64)}),
            ok(a),
        )
    };
    let (dpmul, dpmul_ok) = {
        let g = lock(&sh.dpmul);
        let a = age(g.last_ok);
        (
            json!({"enabled": sh.cfg.dpmul_enabled, "lastOk": g.last_ok, "failures": g.failures, "lastError": g.last_error,
                   "vehicles": g.vehicles, "lastFetchMs": g.last_fetch_ms,
                   "ok": ok(a), "upstreamAgeSec": a.map(|a| a as i64)}),
            ok(a),
        )
    };
    let sz = {
        let g = lock(&sh.sz);
        let a = age(g.last_ok);
        json!({"enabled": sh.cfg.sz_enabled, "lastOk": g.last_ok, "failures": g.failures, "lastError": g.last_error,
               "vehicles": g.vehicles, "blockedForSec": (g.blocked_until - t).max(0.0) as i64,
               "ok": ok(a), "upstreamAgeSec": a.map(|a| a as i64)})
    };
    let dpmd = {
        let g = lock(&sh.dpmd);
        let a = age(g.last_ok);
        json!({"enabled": sh.cfg.dpmd_enabled, "lastOk": g.last_ok, "failures": g.failures, "lastError": g.last_error,
               "vehicles": g.vehicles, "lastFetchMs": g.last_fetch_ms,
               "ok": ok(a), "upstreamAgeSec": a.map(|a| a as i64)})
    };
    let idol = {
        let g = lock(&sh.idol);
        let a = age(g.last_ok);
        json!({"enabled": sh.cfg.idol_enabled, "lastOk": g.last_ok, "failures": g.failures, "lastError": g.last_error,
               "vehicles": g.vehicles, "lastFetchMs": g.last_fetch_ms,
               "ok": ok(a), "upstreamAgeSec": a.map(|a| a as i64)})
    };
    let dpkv = {
        let g = lock(&sh.dpkv);
        let a = age(g.last_ok);
        json!({"enabled": sh.cfg.dpkv_enabled, "lastOk": g.last_ok, "failures": g.failures, "lastError": g.last_error,
               "vehicles": g.vehicles, "lastFetchMs": g.last_fetch_ms,
               "ok": ok(a), "upstreamAgeSec": a.map(|a| a as i64)})
    };
    let pmdp = {
        let g = lock(&sh.pmdp);
        let a = age(g.list_ts);
        json!({"enabled": sh.cfg.pmdp_enabled, "lastOk": g.last_ok, "lastUpdate": g.list_ts, "failures": g.failures,
               "lastError": g.last_error, "vehicles": g.vehicles, "ok": ok(a), "upstreamAgeSec": a.map(|a| a as i64)})
    };
    let pid = {
        let g = lock(&sh.pid);
        let a = age(g.last_ok);
        json!({"enabled": sh.cfg.pid_enabled, "configured": !sh.cfg.pid_key.is_empty(), "lastOk": g.last_ok,
               "failures": g.failures, "lastError": g.last_error, "vehicles": g.vehicles, "features": g.features,
               "blockedForSec": (g.blocked_until - t).max(0.0) as i64, "lastFetchMs": g.last_fetch_ms,
               "ok": ok(a), "upstreamAgeSec": a.map(|a| a as i64)})
    };
    // PID se do souhrnného "ok" nepočítá (jako SŽ a DPMD)
    // DPMD se do souhrnného "ok" nepočítá stejně jako SŽ (jeho výpadek nesmí shodit zdraví celého serveru)
    // SŽ se do souhrnného "ok" nepočítá: její 403 nesmí shodit zdraví celého serveru
    // DPMÚL se počítá jen když je zapnutý (DPMUL_URL); vypnutý nesmí shodit zdraví serveru
    let body = json!({"ok": duk_ok && (dpmul_ok || !sh.cfg.dpmul_enabled),
                      "uptimeMs": sh.start.elapsed().as_millis() as u64,
                      "duk": duk, "dpmul": dpmul, "sz": sz, "dpmd": dpmd, "idol": idol, "dpkv": dpkv, "pmdp": pmdp, "pid": pid,
                      "rateLimit": {"enabled": sh.limiter.is_some(), "perMin": sh.cfg.rate_per_min,
                                    "burst": sh.cfg.rate_burst,
                                    "limitedTotal": sh.limiter.as_ref().map_or(0, |l| l.limited_total())}});
    Arc::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// Některé = klient překročil limit (hodnota je Retry-After v sekundách).
fn rate_limited(rq: &tiny_http::Request, sh: &Shared) -> Option<u64> {
    let limiter = sh.limiter.as_ref()?;
    let xff = rq
        .headers()
        .iter()
        .find(|h| h.field.equiv("X-Forwarded-For"))
        .map(|h| h.value.as_str().to_string());
    let ip = client_ip(rq.remote_addr().map(|a| a.ip()), xff.as_deref(), sh.cfg.trust_proxy);
    let key = match ip {
        Some(ip) if sh.cfg.rate_exempt.contains(&ip) => return None,
        Some(ip) => ip_key(ip),
        None => "unknown".to_string(),
    };
    match limiter.take(&key) {
        Verdict::Ok => None,
        Verdict::Limited { retry_after_s } => {
            let n = limiter.limited_total();
            if n == 1 || n % 200 == 0 {
                warn!("rate limit: {} překročil limit (celkem {} zamítnutých)", key, n);
            }
            Some(retry_after_s)
        }
    }
}

fn handle(rq: tiny_http::Request, sh: &Shared) {
    if *rq.method() != Method::Get {
        let _ = rq.respond(Response::empty(501));
        return;
    }
    let path = rq.url().split('?').next().unwrap_or("").to_string();
    // /health se neomezuje (monitoring); všechno ostatní včetně 404 ano
    if path != "/health" && path != "/api/health" {
        if let Some(retry) = rate_limited(&rq, sh) {
            let body = json!({"error": "rate_limited", "retryAfterSec": retry}).to_string().into_bytes();
            let len = body.len();
            let resp = Response::new(
                StatusCode(429),
                vec![
                    hdr("Content-Type", "application/json; charset=utf-8"),
                    hdr("Retry-After", &retry.to_string()),
                    hdr("Cache-Control", "no-store"),
                ],
                Cursor::new(body),
                Some(len),
                None,
            );
            let _ = rq.respond(resp);
            return;
        }
    }
    let mut x_age: Option<i64> = None;
    let body: Arc<[u8]> = match path.as_str() {
        "/duk/traffic" => lock(&sh.duk).payload.clone(),
        "/dpmul/traffic" => {
            let (b, age) = dpmul_body(sh);
            x_age = Some(age.map_or(-1, |a| a as i64));
            b
        }
        "/health" | "/api/health" => health(sh),
        "/api" | "/api/" => Arc::from(
            serde_json::to_vec(&json!({
                "version": 2,
                "endpoints": {
                    "/api/vehicles": "všechna vozidla sloučená ze zdrojů (nejčerstvější poloha)",
                    "/api/raw/duk": "DÚK (Portabo) s držením zmizelých vozidel",
                    "/api/raw/dpmul": "DPMÚL, poslední odpověď",
                    "/api/raw/sz": "Správa železnic, poslední odpověď",
                    "/api/raw/dpmd": "DPMD (Děčín), vozidla ve službě bez řidičů",
                    "/api/raw/idol": "IDOL (Liberecký kraj), vozidla z mapy mpvnet.cz",
                    "/api/raw/dpkv": "DPKV (Karlovy Vary), rozebraná vozidla (WGS84)",
                    "/api/raw/pmdp": "PMDP (Plzeň), rozebraná vozidla (tramvaje, trolejbusy, autobusy)",
                    "/api/trip/pmdp?conn=<trip vozu>": "PMDP: zastávky a tvar trasy spoje (stejný tvar jako /api/vehicle-trip webu)",
                    "/api/raw/pid": "PID (Golemio), rozebraná vozidla, jedno na oběh",
                    "/api/health": "stav serveru a zdrojů"
                }
            }))
            .unwrap_or_default(),
        ),
        "/api/vehicles" => vehicles_json(sh),
        "/api/raw/duk" => lock(&sh.duk).payload.clone(),
        "/api/raw/dpmul" => {
            let (b, age) = dpmul_body(sh);
            x_age = Some(age.map_or(-1, |a| a as i64));
            b
        }
        "/api/raw/sz" => {
            let (b, age) = sz_body(sh);
            x_age = Some(age.map_or(-1, |a| a as i64));
            b
        }
        "/api/raw/dpmd" => {
            let (b, age) = dpmd_body(sh);
            x_age = Some(age.map_or(-1, |a| a as i64));
            b
        }
        "/api/raw/idol" => {
            let (b, age) = idol_body(sh);
            x_age = Some(age.map_or(-1, |a| a as i64));
            b
        }
        "/api/raw/dpkv" => {
            let (b, age) = dpkv_body(sh);
            x_age = Some(age.map_or(-1, |a| a as i64));
            b
        }
        "/api/trip/pmdp" => {
            let conn = rq
                .url()
                .split_once('?')
                .map(|(_, q)| q)
                .unwrap_or("")
                .split('&')
                .find_map(|kv| kv.strip_prefix("conn="))
                .unwrap_or("")
                .to_string();
            let (status, b, cache) = match pmdp_trip(sh, &conn) {
                Ok(b) => (200, b, "public, max-age=300"),
                Err((st, e)) => {
                    if st >= 500 {
                        warn!("{}", e);
                    }
                    (st, Arc::from(json!({ "error": e }).to_string().into_bytes()), "no-store")
                }
            };
            let len = b.len();
            let resp = Response::new(
                StatusCode(status),
                vec![hdr("Content-Type", "application/json; charset=utf-8"), hdr("Cache-Control", cache)],
                Cursor::new(b),
                Some(len),
                None,
            );
            let _ = rq.respond(resp);
            return;
        }
        "/api/raw/pmdp" => {
            let (b, age) = pmdp_body(sh);
            x_age = Some(age.map_or(-1, |a| a as i64));
            b
        }
        "/api/raw/pid" => {
            let (b, age) = pid_body(sh);
            x_age = Some(age.map_or(-1, |a| a as i64));
            b
        }
        _ => {
            let _ = rq.respond(Response::empty(404));
            return;
        }
    };
    let len = body.len();
    let mut resp = Response::new(
        StatusCode(200),
        vec![
            hdr("Content-Type", "application/json; charset=utf-8"),
            hdr("Cache-Control", "max-age=3"),
        ],
        Cursor::new(body),
        Some(len),
        None,
    );
    if let Some(a) = x_age {
        resp.add_header(hdr("X-Age-Sec", &a.to_string()));
    }
    let _ = rq.respond(resp);
}

fn worker(server: Arc<Server>, sh: Arc<Shared>) {
    while !sh.stop.is_set() {
        match server.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(rq)) => handle(rq, &sh),
            Ok(None) => {}
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

// ---------------------------------------------------------------- main

fn main() {
    let cfg: &'static Config = Box::leak(Box::new(Config::from_env()));
    let sh = Arc::new(Shared {
        cfg,
        duk: Mutex::new(Duk {
            store: HashMap::new(),
            payload: Arc::from(&b"{\"VehicleList\":[]}"[..]),
            last_ok: 0.0,
            failures: 0,
            last_error: String::new(),
            vehicles: 0,
            held: 0,
            held_mhd: 0,
        }),
        dpmul: Mutex::new(Dpmul {
            payload: Arc::from(&b"[]"[..]),
            list: Vec::new(),
            list_ts: 0.0,
            last_ok: 0.0,
            failures: 0,
            last_error: String::new(),
            vehicles: 0,
            last_fetch_ms: 0,
        }),
        sz: Mutex::new(Sz {
            payload: Arc::from(&b"{\"result\":[]}"[..]),
            list: Vec::new(),
            list_ts: 0.0,
            last_ok: 0.0,
            failures: 0,
            last_error: String::new(),
            vehicles: 0,
            blocked_until: 0.0,
            blocked_count: 0,
            last_request: 0.0,
        }),
        dpmd: Mutex::new(Dpmd {
            payload: Arc::from(DPMD_EMPTY),
            list: Vec::new(),
            list_ts: 0.0,
            last_ok: 0.0,
            failures: 0,
            last_error: String::new(),
            vehicles: 0,
            last_fetch_ms: 0,
        }),
        idol: Mutex::new(Idol {
            payload: Arc::from(IDOL_EMPTY),
            list: Vec::new(),
            list_ts: 0.0,
            last_ok: 0.0,
            failures: 0,
            last_error: String::new(),
            vehicles: 0,
            last_fetch_ms: 0,
        }),
        dpkv: Mutex::new(Idol {
            payload: Arc::from(DPKV_EMPTY),
            list: Vec::new(),
            list_ts: 0.0,
            last_ok: 0.0,
            failures: 0,
            last_error: String::new(),
            vehicles: 0,
            last_fetch_ms: 0,
        }),
        pmdp: Mutex::new(Idol {
            payload: Arc::from(PMDP_EMPTY),
            list: Vec::new(),
            list_ts: 0.0,
            last_ok: 0.0,
            failures: 0,
            last_error: String::new(),
            vehicles: 0,
            last_fetch_ms: 0,
        }),
        pmdp_api: Mutex::new(PmdpApi { token: String::new(), cookie: String::new(), token_at: 0.0, trips: HashMap::new() }),
        pid: Mutex::new(Pid {
            list: Vec::new(),
            list_ts: 0.0,
            last_ok: 0.0,
            failures: 0,
            last_error: String::new(),
            vehicles: 0,
            features: 0,
            blocked_until: 0.0,
            last_fetch_ms: 0,
        }),
        vcache: Mutex::new((0.0, Arc::from(&b""[..]))),
        limiter: (cfg.rate_per_min > 0.0).then(|| RateLimiter::new(cfg.rate_per_min, cfg.rate_burst)),
        stop: Stop { m: Mutex::new(false), cv: Condvar::new() },
        start: Instant::now(),
    });

    load(&sh);

    {
        let sh = sh.clone();
        ctrlc::set_handler(move || sh.stop.set()).expect("signal handler");
    }

    let small = |name: &str| thread::Builder::new().name(name.into()).stack_size(512 * 1024);
    {
        let s = sh.clone();
        small("duk").spawn(move || poller(s)).unwrap();
        if cfg.dpmul_enabled {
            let s = sh.clone();
            small("dpmul").spawn(move || dpmul_poller(s)).unwrap();
        }
        if cfg.sz_enabled {
            let s = sh.clone();
            small("sz").spawn(move || sz_poller(s)).unwrap();
        }
        if cfg.dpmd_enabled {
            let s = sh.clone();
            small("dpmd").spawn(move || dpmd_poller(s)).unwrap();
        }
        if cfg.idol_enabled {
            let s = sh.clone();
            small("idol").spawn(move || idol_poller(s)).unwrap();
        }
        if cfg.dpkv_enabled {
            let s = sh.clone();
            small("dpkv").spawn(move || dpkv_poller(s)).unwrap();
        }
        if cfg.pmdp_enabled {
            let s = sh.clone();
            small("pmdp").spawn(move || pmdp_poller(s)).unwrap();
        }
        if cfg.pid_enabled {
            let s = sh.clone();
            // větší zásobník: rozebírá se několik MB JSONu
            thread::Builder::new().name("pid".into()).stack_size(2 << 20).spawn(move || pid_poller(s)).unwrap();
        }
    }

    let server = Arc::new(
        Server::http((cfg.bind.as_str(), cfg.port)).unwrap_or_else(|e| {
            eprintln!("ERROR nelze naslouchat na {}:{}: {}", cfg.bind, cfg.port, e);
            std::process::exit(1);
        }),
    );
    if cfg.rate_per_min > 0.0 {
        info!("rate limit: {}/min na klienta, rezerva {}, proxy {}, výjimky {}", cfg.rate_per_min, cfg.rate_burst, cfg.trust_proxy, cfg.rate_exempt.len());
    } else {
        info!("rate limit vypnut (RATE_PER_MIN=0)");
    }
    info!(
        "PID (Golemio): {}",
        if cfg.pid_enabled { "zapnuto" } else if cfg.pid_key.is_empty() { "vypnuto (chybí GOLEMIO_API_KEY)" } else { "vypnuto (PID=0)" }
    );
    info!("poslouchám na {}:{}, upstream DÚK {}, DPMÚL {}, SŽ {}, DPMD {}", cfg.bind, cfg.port, cfg.upstream, if cfg.dpmul_enabled { "zapnut (DPMUL_URL)" } else { "vypnut" }, if cfg.sz_enabled { "zapnuta" } else { "vypnuta" }, if cfg.dpmd_enabled { cfg.dpmd_url.as_str() } else { "vypnut" });

    let workers: Vec<_> = (0..HTTP_WORKERS)
        .map(|i| {
            let (s, h) = (server.clone(), sh.clone());
            small(&format!("http{i}")).spawn(move || worker(s, h)).unwrap()
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    save(&sh);
}
