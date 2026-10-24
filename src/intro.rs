//! Escolha dos pontos de introdução: os relays por onde você é alcançado e que
//! entram no registro de presença.
//!
//! Política:
//! - só relays saudáveis (menos de `max_failures` falhas seguidas);
//! - **estável**: pontos já escolhidos e ainda saudáveis permanecem, porque cada
//!   troca obriga a republicar o registro;
//! - preferência por menor latência, sorteando entre os melhores para não
//!   concentrar todos os usuários nos mesmos relays;
//! - diversidade: evita dois pontos no mesmo grupo (região, ou host se não houver).

use std::collections::HashSet;

use rand_core::RngCore;
use thiserror::Error;

use crate::presence::{MAX_INTRO_LEN, MAX_INTRO_POINTS};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IntroError {
    #[error("endereço de relay vazio")]
    EmptyAddr,
    #[error("endereço de relay com mais de {MAX_INTRO_LEN} bytes")]
    AddrTooLong,
    #[error("linha {0} da lista de sementes inválida (use: endereço [região])")]
    BadSeedLine(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct IntroConfig {
    /// Quantos pontos publicar (1 a 8). Padrão: 3.
    pub count: usize,
    /// Falhas seguidas que tiram um relay da escolha. Padrão: 3.
    pub max_failures: u32,
    /// Latência presumida de relay ainda não medido. Padrão: 500 ms.
    pub default_rtt_ms: u32,
}

impl Default for IntroConfig {
    fn default() -> Self {
        Self { count: 3, max_failures: 3, default_rtt_ms: 500 }
    }
}

struct Entry {
    addr: String,
    region: Option<String>,
    rtt_ms: Option<f32>,
    failures: u32,
}

impl Entry {
    fn group(&self) -> &str {
        self.region.as_deref().unwrap_or_else(|| self.addr.rsplit_once(':').map_or(&self.addr, |(h, _)| h))
    }
}

/// Resultado de [`IntroSelector::select`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Pontos prontos para `PresenceRunner::set_intro_points`.
    pub points: Vec<Vec<u8>>,
    /// Se o conjunto mudou desde a chamada anterior (só então vale republicar).
    pub changed: bool,
}

pub struct IntroSelector {
    cfg: IntroConfig,
    relays: Vec<Entry>,
    current: Vec<String>,
}

impl IntroSelector {
    pub fn new(cfg: IntroConfig) -> Self {
        let count = cfg.count.clamp(1, MAX_INTRO_POINTS);
        Self { cfg: IntroConfig { count, ..cfg }, relays: Vec::new(), current: Vec::new() }
    }

    /// Adiciona um relay (ou atualiza a região de um já conhecido).
    pub fn add_relay(&mut self, addr: &str, region: Option<&str>) -> Result<(), IntroError> {
        if addr.is_empty() {
            return Err(IntroError::EmptyAddr);
        }
        if addr.len() > MAX_INTRO_LEN {
            return Err(IntroError::AddrTooLong);
        }
        let region = region.map(str::to_owned);
        match self.relays.iter_mut().find(|e| e.addr == addr) {
            Some(e) => e.region = region,
            None => self.relays.push(Entry { addr: addr.into(), region, rtt_ms: None, failures: 0 }),
        }
        Ok(())
    }

    pub fn remove_relay(&mut self, addr: &str) {
        self.relays.retain(|e| e.addr != addr);
    }

    /// Carrega a lista de sementes. Uma por linha: `endereço [região]`;
    /// linhas vazias e iniciadas por `#` são ignoradas. Devolve quantas entraram.
    pub fn load_seeds(&mut self, text: &str) -> Result<usize, IntroError> {
        let mut n = 0;
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(addr), region, None) = (parts.next(), parts.next(), parts.next()) else {
                return Err(IntroError::BadSeedLine(i + 1));
            };
            self.add_relay(addr, region)?;
            n += 1;
        }
        Ok(n)
    }

    /// Relay respondeu: zera as falhas e atualiza a latência (média móvel).
    pub fn report_success(&mut self, addr: &str, rtt_ms: u32) {
        if let Some(e) = self.relays.iter_mut().find(|e| e.addr == addr) {
            e.failures = 0;
            let rtt = rtt_ms as f32;
            e.rtt_ms = Some(e.rtt_ms.map_or(rtt, |old| old * 0.7 + rtt * 0.3));
        }
    }

    pub fn report_failure(&mut self, addr: &str) {
        if let Some(e) = self.relays.iter_mut().find(|e| e.addr == addr) {
            e.failures = e.failures.saturating_add(1);
        }
    }

    /// Escolhe os pontos. Chame periodicamente e após `report_*`; republique
    /// só quando `changed` for verdadeiro.
    pub fn select(&mut self, rng: &mut impl RngCore) -> Selection {
        let cfg = self.cfg;
        let healthy = |e: &Entry| e.failures < cfg.max_failures;
        let rtt = |e: &Entry| e.rtt_ms.unwrap_or(cfg.default_rtt_ms as f32);

        // Mantém os atuais que continuam saudáveis.
        let mut chosen: Vec<usize> = self
            .current
            .iter()
            .filter_map(|a| self.relays.iter().position(|e| &e.addr == a))
            .filter(|&i| healthy(&self.relays[i]))
            .take(cfg.count)
            .collect();

        // Candidatos fixados uma vez: os `2 * count` de menor latência entre os que ainda não foram escolhidos.
        let mut ranked: Vec<usize> =
            (0..self.relays.len()).filter(|i| healthy(&self.relays[*i]) && !chosen.contains(i)).collect();
        ranked.sort_by(|&a, &b| rtt(&self.relays[a]).total_cmp(&rtt(&self.relays[b])));
        ranked.truncate(cfg.count * 2);

        while chosen.len() < cfg.count {
            let used: HashSet<&str> = chosen.iter().map(|&i| self.relays[i].group()).collect();
            let pool: Vec<usize> = ranked.iter().copied().filter(|i| !chosen.contains(i)).collect();
            if pool.is_empty() {
                break;
            }
            let diverse: Vec<usize> =
                pool.iter().copied().filter(|&i| !used.contains(self.relays[i].group())).collect();
            let from = if diverse.is_empty() { &pool } else { &diverse };
            chosen.push(from[rng.next_u32() as usize % from.len()]);
        }

        let addrs: Vec<String> = chosen.iter().map(|&i| self.relays[i].addr.clone()).collect();
        let (mut a, mut b) = (addrs.clone(), self.current.clone());
        a.sort();
        b.sort();
        let changed = a != b;
        self.current = addrs;
        Selection { points: self.current.iter().map(|a| a.as_bytes().to_vec()).collect(), changed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    fn selector(n: usize) -> IntroSelector {
        let mut s = IntroSelector::new(IntroConfig::default());
        for i in 0..n {
            s.add_relay(&format!("10.0.0.{i}:4001"), None).unwrap();
        }
        s
    }

    fn addrs(sel: &Selection) -> Vec<String> {
        sel.points.iter().map(|p| String::from_utf8(p.clone()).unwrap()).collect()
    }

    #[test]
    fn picks_count_distinct_relays() {
        let mut s = selector(10);
        let sel = s.select(&mut OsRng);
        let set: HashSet<_> = addrs(&sel).into_iter().collect();
        assert_eq!(set.len(), 3);
        assert!(sel.changed);
    }

    #[test]
    fn fewer_relays_than_count_returns_all() {
        let mut s = selector(2);
        assert_eq!(s.select(&mut OsRng).points.len(), 2);
        assert!(selector(0).select(&mut OsRng).points.is_empty());
    }

    #[test]
    fn selection_is_sticky_until_something_fails() {
        let mut s = selector(10);
        let first = s.select(&mut OsRng);
        for _ in 0..20 {
            let again = s.select(&mut OsRng);
            assert!(!again.changed);
            assert_eq!(again.points, first.points);
        }
    }

    #[test]
    fn failing_relay_is_replaced_and_others_stay() {
        let mut s = selector(10);
        let first = addrs(&s.select(&mut OsRng));
        for _ in 0..3 {
            s.report_failure(&first[0]);
        }
        let next = s.select(&mut OsRng);
        let got = addrs(&next);
        assert!(next.changed);
        assert!(!got.contains(&first[0]));
        assert!(got.contains(&first[1]) && got.contains(&first[2]));
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn success_clears_failures() {
        let mut s = selector(1);
        s.report_failure("10.0.0.0:4001");
        s.report_failure("10.0.0.0:4001");
        s.report_success("10.0.0.0:4001", 40);
        s.report_failure("10.0.0.0:4001");
        assert_eq!(s.select(&mut OsRng).points.len(), 1);
    }

    #[test]
    fn draws_only_from_the_lowest_latency_pool() {
        // Pool = 2 * count = 6 melhores: os 3 rápidos e 3 dos lentos (empate estável).
        let fast: HashSet<String> = (0..6).map(|i| format!("10.0.0.{i}:4001")).collect();
        for _ in 0..50 {
            let mut s = selector(20);
            for i in 0..20 {
                s.report_success(&format!("10.0.0.{i}:4001"), if i < 3 { 20 } else { 400 });
            }
            assert!(addrs(&s.select(&mut OsRng)).iter().all(|a| fast.contains(a)));
        }
    }

    #[test]
    fn avoids_same_region_when_possible() {
        let mut s = IntroSelector::new(IntroConfig::default());
        for i in 0..4 {
            s.add_relay(&format!("a{i}:4001"), Some("sa-east")).unwrap();
        }
        s.add_relay("b0:4001", Some("us-east")).unwrap();
        s.add_relay("c0:4001", Some("eu-west")).unwrap();
        for _ in 0..30 {
            let mut fresh = IntroSelector::new(IntroConfig::default());
            for i in 0..4 {
                fresh.add_relay(&format!("a{i}:4001"), Some("sa-east")).unwrap();
            }
            fresh.add_relay("b0:4001", Some("us-east")).unwrap();
            fresh.add_relay("c0:4001", Some("eu-west")).unwrap();
            let got = addrs(&fresh.select(&mut OsRng));
            let groups: HashSet<_> = got.iter().map(|a| &a[..1]).collect();
            assert_eq!(groups.len(), 3, "{got:?}");
        }
    }

    #[test]
    fn rejects_bad_addresses() {
        let mut s = selector(0);
        assert_eq!(s.add_relay("", None), Err(IntroError::EmptyAddr));
        assert_eq!(s.add_relay(&"x".repeat(MAX_INTRO_LEN + 1), None), Err(IntroError::AddrTooLong));
        assert!(s.add_relay(&"x".repeat(MAX_INTRO_LEN), None).is_ok());
    }

    #[test]
    fn loads_seed_list() {
        let mut s = selector(0);
        let n = s.load_seeds("# sementes\n\nr1.example:4001 sa-east\nr2.example:4001\n").unwrap();
        assert_eq!(n, 2);
        assert_eq!(s.select(&mut OsRng).points.len(), 2);
        assert_eq!(s.load_seeds("ok:1\nruim a b\n"), Err(IntroError::BadSeedLine(2)));
    }

    #[test]
    fn count_is_clamped_to_record_limit() {
        let mut s = IntroSelector::new(IntroConfig { count: 99, ..Default::default() });
        for i in 0..20 {
            s.add_relay(&format!("h{i}:1"), None).unwrap();
        }
        let sel = s.select(&mut OsRng);
        assert_eq!(sel.points.len(), MAX_INTRO_POINTS);
        crate::presence::validate_intro_points(&sel.points).unwrap();
    }
}
