//! Limite de pedidos por IP.

use super::*;

/// Limite de pedidos por IP: balde de fichas que enche `per_sec` por segundo até `burst`.
/// Pedidos baratos custam 1 ficha; os que verificam assinatura ou fazem conexões de volta custam mais.
#[derive(Clone, Copy, Debug)]
pub struct RateLimit {
    pub per_sec: f64,
    pub burst: f64,
}

impl Default for RateLimit {
    /// Folgado para vários nós atrás do mesmo IP (CGNAT, casa, empresa), apertado para quem faz flood.
    fn default() -> Self {
        Self { per_sec: 50.0, burst: 200.0 }
    }
}

/// IPs rastreados ao mesmo tempo; passou disso, IPs novos são recusados até alguns saírem.
pub(super) const MAX_TRACKED_IPS: usize = 65536;

pub(super) struct Limiter {
    cfg: RateLimit,
    buckets: Mutex<HashMap<u128, (f64, Instant)>>,
}

impl Limiter {
    pub(super) fn new(cfg: RateLimit) -> Self {
        Self { cfg, buckets: Mutex::default() }
    }

    /// Gasta `cost` fichas do IP; `false` se não há. IPv6 conta por /64 (um usuário recebe o bloco todo).
    pub(super) fn allow(&self, ip: std::net::IpAddr, cost: f64) -> bool {
        let key = match ip.to_canonical() {
            std::net::IpAddr::V4(a) => u32::from(a) as u128,
            std::net::IpAddr::V6(a) => (u128::from(a) >> 64) << 64 | 1 << 100,
        };
        let now = Instant::now();
        let mut b = self.buckets.lock().unwrap();
        if b.len() >= MAX_TRACKED_IPS && !b.contains_key(&key) {
            // Quem já recuperou o balde inteiro não precisa mais ser lembrado.
            let c = self.cfg;
            b.retain(|_, (t, at)| *t + at.elapsed().as_secs_f64() * c.per_sec < c.burst);
            if b.len() >= MAX_TRACKED_IPS {
                return false;
            }
        }
        let e = b.entry(key).or_insert((self.cfg.burst, now));
        e.0 = (e.0 + e.1.elapsed().as_secs_f64() * self.cfg.per_sec).min(self.cfg.burst);
        e.1 = now;
        if e.0 < cost {
            return false;
        }
        e.0 -= cost;
        true
    }
}

/// Ficha que cada tipo de pedido custa.
pub(super) fn cost(kind: Option<&u8>) -> f64 {
    match kind {
        Some(&(REQ_ROOM_JOIN | REQ_ROOM_LEAVE | REQ_ROOM_LIST | REQ_NOTICE_PUT | REQ_NOTICE_GET | REQ_PUNCH_LISTEN)) => 5.0,
        Some(&REQ_REACH) => 20.0,
        _ => 1.0,
    }
}
