//! Omezení počtu požadavků na klienta (ochrana proti zahlcení). V paměti, bez závislostí.
//!
//! Každý klient (IP) má „kyblík“ žetonů: jeden požadavek = jeden žeton, žetony se průběžně doplňují.
//! Appka, která se ptá každých 5–10 s, se do limitu nikdy nedostane; navíc je v kyblíku rezerva (`burst`)
//! na požadavky mimo rozvrh (start appky, návrat z pozadí, „na požádání“, když appka nemá zdroj dat).
//! Teprve trvalý nadlimitní provoz dostane 429.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr},
    sync::Mutex,
    time::Instant,
};

const MAX_KEYS: usize = 20_000;
const PRUNE_EVERY_S: f64 = 60.0;

struct Bucket {
    tokens: f64,
    t: f64,
}

struct Inner {
    buckets: HashMap<String, Bucket>,
    last_prune: f64,
}

pub struct RateLimiter {
    rate: f64,  // žetonů za sekundu
    burst: f64, // velikost kyblíku
    start: Instant,
    inner: Mutex<Inner>,
    limited: std::sync::atomic::AtomicU64,
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Ok,
    /// Za kolik sekund to bude zase projde.
    Limited { retry_after_s: u64 },
}

impl RateLimiter {
    pub fn new(per_min: f64, burst: f64) -> Self {
        RateLimiter {
            rate: per_min.max(0.1) / 60.0,
            burst: burst.max(1.0),
            start: Instant::now(),
            inner: Mutex::new(Inner { buckets: HashMap::new(), last_prune: 0.0 }),
            limited: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn limited_total(&self) -> u64 {
        self.limited.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn take(&self, key: &str) -> Verdict {
        self.take_at(key, self.start.elapsed().as_secs_f64())
    }

    /// Jako `take`, jen s explicitním časem (kvůli testům).
    fn take_at(&self, key: &str, t: f64) -> Verdict {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if g.buckets.len() > MAX_KEYS || t - g.last_prune > PRUNE_EVERY_S {
            self.prune(&mut g, t);
        }

        let (rate, burst) = (self.rate, self.burst);
        let b = g.buckets.entry(key.to_string()).or_insert(Bucket { tokens: burst, t });
        b.tokens = (b.tokens + (t - b.t) * rate).min(burst);
        b.t = t;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Verdict::Ok
        } else {
            let wait = ((1.0 - b.tokens) / rate).ceil().max(1.0) as u64;
            self.limited.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Verdict::Limited { retry_after_s: wait }
        }
    }

    /// Plný kyblík je totéž co žádný záznam, takže se zahodí. Při útoku s mnoha adresami, kdy to nestačí,
    /// se paměť omezí tvrdě (klienti dostanou čerstvé kyblíky, což je lepší než neomezený růst).
    fn prune(&self, g: &mut Inner, t: f64) {
        let (rate, burst) = (self.rate, self.burst);
        g.buckets.retain(|_, b| b.tokens + (t - b.t) * rate < burst);
        if g.buckets.len() > MAX_KEYS {
            g.buckets.clear();
        }
        g.last_prune = t;
    }
}

/// Klíč klienta. IPv6 se sdružuje po /64 (jedno připojení dostává celou /64, jinak by útočník střídal adresy).
pub fn ip_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.to_string();
            }
            let s = v6.segments();
            format!("{}/64", Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
    }
}

/// IP klienta. `trust_proxy` je počet proxy před serverem (Caddy = 1). Při 0 se X-Forwarded-For ignoruje,
/// takže ho klient nemůže podvrhnout. Při N > 0 se bere N-tá položka zprava, tedy ta, kterou připsala nejbližší
/// důvěryhodná proxy; to, co poslal klient, je vlevo a nepoužije se. Bez použitelné hlavičky platí adresa spojení.
pub fn client_ip(peer: Option<IpAddr>, xff: Option<&str>, trust_proxy: usize) -> Option<IpAddr> {
    if trust_proxy > 0 {
        if let Some(h) = xff {
            let hops: Vec<&str> = h.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
            if hops.len() >= trust_proxy {
                let cand = hops[hops.len() - trust_proxy].trim_matches(|c| c == '[' || c == ']');
                if let Ok(ip) = cand.parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
    }
    peer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_limited_then_refill() {
        let rl = RateLimiter::new(30.0, 10.0); // 0,5 žetonu/s, rezerva 10
        for _ in 0..10 {
            assert_eq!(rl.take_at("a", 0.0), Verdict::Ok);
        }
        assert!(matches!(rl.take_at("a", 0.0), Verdict::Limited { .. }));
        assert_eq!(rl.take_at("a", 2.0), Verdict::Ok); // za 2 s je zase jeden žeton
        assert!(matches!(rl.take_at("a", 2.0), Verdict::Limited { .. }));
    }

    #[test]
    fn polling_every_5s_never_limited_even_with_on_demand_extras() {
        let rl = RateLimiter::new(30.0, 10.0);
        let mut t = 0.0;
        for i in 0..2000 {
            assert_eq!(rl.take_at("app", t), Verdict::Ok, "pravidelný dotaz č. {i}");
            if i % 7 == 0 {
                assert_eq!(rl.take_at("app", t + 0.3), Verdict::Ok, "dotaz navíc č. {i}"); // on demand
            }
            t += 5.0;
        }
    }

    #[test]
    fn clients_are_independent_and_retry_after_is_sane() {
        let rl = RateLimiter::new(30.0, 2.0);
        assert_eq!(rl.take_at("a", 0.0), Verdict::Ok);
        assert_eq!(rl.take_at("a", 0.0), Verdict::Ok);
        match rl.take_at("a", 0.0) {
            Verdict::Limited { retry_after_s } => assert_eq!(retry_after_s, 2),
            v => panic!("{v:?}"),
        }
        assert_eq!(rl.take_at("b", 0.0), Verdict::Ok);
        assert_eq!(rl.limited_total(), 1);
    }

    #[test]
    fn prune_drops_idle_buckets_and_caps_memory() {
        let rl = RateLimiter::new(30.0, 5.0);
        rl.take_at("idle", 0.0);
        rl.take_at("other", 1000.0); // spustí úklid: "idle" je dávno zase plný
        assert_eq!(rl.inner.lock().unwrap().buckets.len(), 1);
        for i in 0..(MAX_KEYS + 10) {
            rl.take_at(&format!("k{i}"), 1000.0);
        }
        assert!(rl.inner.lock().unwrap().buckets.len() <= MAX_KEYS + 1);
    }

    #[test]
    fn ip_keys() {
        assert_eq!(ip_key("1.2.3.4".parse().unwrap()), "1.2.3.4");
        assert_eq!(ip_key("::ffff:1.2.3.4".parse().unwrap()), "1.2.3.4");
        assert_eq!(ip_key("2001:db8:1:2:aaaa:bbbb:cccc:dddd".parse().unwrap()), "2001:db8:1:2::/64");
        assert_eq!(ip_key("2001:db8:1:2:1::1".parse().unwrap()), ip_key("2001:db8:1:2:2::2".parse().unwrap()));
    }

    #[test]
    fn client_ip_trust() {
        let peer: Option<IpAddr> = Some("127.0.0.1".parse().unwrap());
        // bez důvěry v proxy se hlavička ignoruje (nejde podvrhnout)
        assert_eq!(client_ip(peer, Some("9.9.9.9"), 0), peer);
        // jedna proxy: bere se poslední položka (tu přidala proxy), podvržené vlevo se ignoruje
        assert_eq!(client_ip(peer, Some("6.6.6.6, 5.6.7.8"), 1), Some("5.6.7.8".parse().unwrap()));
        assert_eq!(client_ip(peer, Some("[2001:db8::1]"), 1), Some("2001:db8::1".parse().unwrap()));
        // nesmyslná / chybějící hlavička → adresa spojení
        assert_eq!(client_ip(peer, Some("blbost"), 1), peer);
        assert_eq!(client_ip(peer, None, 1), peer);
    }
}
