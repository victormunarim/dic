//! Reputação medida dos relays, a partir do que o próprio cliente observa.
//!
//! O [`CircuitManager`](crate::circuit::manager::CircuitManager) emite
//! [`Event::Rtt`] (circuito saudável) e [`Event::CircuitFailed`] (circuito
//! caiu). Alimente [`Reputation::record`] com esses eventos e passe a lista de
//! candidatos por [`Reputation::adjust`] antes de entregá-la ao gerente:
//!
//! ```text
//! rep.record(&evento, agora_ms);
//! mgr.set_relays(rep.adjust(discovery.candidates(), agora_ms));
//! ```
//!
//! # Modelo
//!
//! Por relay (chave: endereço) guardamos, com decaimento exponencial
//! (meia-vida de 1 h):
//!
//! - **Acertos**: cada [`Event::Rtt`] soma [`RTT_CREDIT`] (50 pings saudáveis,
//!   ~5 s de circuito ativo, valem um acerto).
//! - **Erros**: cada falha soma 1, repartida conforme o que se sabe. Falha na
//!   montagem em `create` culpa só o primeiro salto; em `extend`, só o segundo.
//!   Silêncio, `Destroy` ou travamento em `verify` não dizem qual dos dois
//!   caiu: cada um leva 0,5. Um relay ruim aparece em muitas falhas com
//!   parceiros diferentes, os bons aparecem pouco, e a diferença se acumula.
//! - **Latência**: média móvel do RTT fim a fim dos circuitos de que o relay
//!   participou (o RTT de um circuito é de dois relays; um relay lento puxa
//!   para cima a média de todos os parceiros, e a dos parceiros diluída).
//!
//! A pontuação final é `pontuação_base × confiabilidade² × fator_de_latência`.
//! Relays desconhecidos começam em confiabilidade 2/3 (a priori, 2 acertos e 1
//! erro): podem ser escolhidos, mas perdem para quem já provou ser estável.
//! Quem acumula evidência suficiente de falha ([`ReputationConfig::ban_min_evidence`])
//! com taxa de acerto abaixo de [`ReputationConfig::ban_below`] sai da lista
//! até as falhas decaírem.
//!
//! # Limites
//!
//! - **Só experiência própria.** Nada de reputação trocada com outros nós, que
//!   seria um canal fácil de manipular. O custo é a partida a frio.
//! - Um relay pode se comportar bem sob ping e mal sob carga real; o ping do
//!   gerente já viaja pelo mesmo circuito da mídia, o que reduz (não elimina)
//!   isso.
//! - O estado vive na memória; para sobreviver a reinícios use
//!   [`Reputation::export`] / [`Reputation::import`] (veja "Persistência").
//!
//! # Persistência
//!
//! `export` gera um instantâneo binário (`clamrep1`, endereços e contagens
//! já decaídas, com a hora de gravação). `import` aplica o decaimento pelo
//! tempo que passou desde a gravação, de modo que parar por horas esquece
//! parte das falhas, como se o programa tivesse ficado rodando. O arquivo
//! revela quais relays você usou: [`save_file`] grava com permissão `0600`
//! (Unix), de forma atômica (arquivo temporário e `rename`). Dados lidos de
//! disco são tratados como não confiáveis: tamanho, endereços e valores são
//! validados e limitados, e um arquivo corrompido vira erro, nunca pânico.

use std::{
    collections::HashMap,
    io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::circuit::manager::{Candidate, Event, FailReason};

/// Acerto parcial por [`Event::Rtt`].
pub const RTT_CREDIT: f64 = 0.02;

#[derive(Debug, Clone, Copy)]
pub struct ReputationConfig {
    /// Meia-vida das contagens de acertos e erros. Padrão: 1 h.
    pub half_life_ms: u64,
    /// RTT até o qual a latência não penaliza. Padrão: 100 ms.
    pub latency_free_ms: f32,
    /// A cada `latency_scale_ms` acima disso, o fator cai pela metade. Padrão: 200 ms.
    pub latency_scale_ms: f32,
    /// Evidência mínima (acertos + erros) para banir. Padrão: 4.
    pub ban_min_evidence: f64,
    /// Taxa de acerto abaixo da qual há banimento. Padrão: 0,2.
    pub ban_below: f64,
    /// Relays rastreados no máximo; os mais antigos saem. Padrão: 8192.
    pub max_entries: usize,
}

impl Default for ReputationConfig {
    fn default() -> Self {
        Self {
            half_life_ms: 3_600_000,
            latency_free_ms: 100.0,
            latency_scale_ms: 200.0,
            ban_min_evidence: 4.0,
            ban_below: 0.2,
            max_entries: 8192,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    good: f64,
    bad: f64,
    rtt_ms: Option<f32>,
    updated_ms: u64,
}

/// Retrato público de um relay, para diagnóstico.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelayStats {
    pub good: f64,
    pub bad: f64,
    pub rtt_ms: Option<f32>,
    pub reliability: f64,
    pub banned: bool,
}

pub struct Reputation {
    cfg: ReputationConfig,
    relays: HashMap<String, Entry>,
}

impl Reputation {
    pub fn new(cfg: ReputationConfig) -> Self {
        Self { cfg, relays: HashMap::new() }
    }

    /// Consome um evento do gerente de circuitos. Eventos sem relação são ignorados.
    pub fn record(&mut self, ev: &Event, now_ms: u64) {
        match ev {
            Event::Rtt { first, second, rtt_ms } => {
                for addr in [first, second] {
                    let e = self.entry(addr, now_ms);
                    e.good += RTT_CREDIT;
                    let r = *rtt_ms as f32;
                    e.rtt_ms = Some(e.rtt_ms.map_or(r, |old| old * 0.8 + r * 0.2));
                }
            }
            Event::CircuitFailed { first, second, reason } => {
                let (wf, ws) = match reason {
                    FailReason::BuildTimeout("create") => (1.0, 0.0),
                    FailReason::BuildTimeout("extend") => (0.0, 1.0),
                    _ => (0.5, 0.5),
                };
                self.entry(first, now_ms).bad += wf;
                self.entry(second, now_ms).bad += ws;
            }
            _ => {}
        }
    }

    /// Multiplica as pontuações pela reputação e remove os relays banidos.
    pub fn adjust(&mut self, candidates: Vec<Candidate>, now_ms: u64) -> Vec<Candidate> {
        candidates
            .into_iter()
            .filter_map(|mut c| {
                let Some(e) = self.relays.get_mut(&c.addr) else {
                    c.score *= (PRIOR_RELIABILITY * PRIOR_RELIABILITY) as f32;
                    return Some(c);
                };
                decay(e, now_ms, self.cfg.half_life_ms);
                let e = *e;
                if banned(&e, &self.cfg) {
                    return None;
                }
                let r = reliability(&e);
                c.score *= (r * r) as f32 * latency_factor(e.rtt_ms, &self.cfg);
                Some(c)
            })
            .collect()
    }

    pub fn stats(&mut self, addr: &str, now_ms: u64) -> Option<RelayStats> {
        let half = self.cfg.half_life_ms;
        let cfg = self.cfg;
        let e = self.relays.get_mut(addr)?;
        decay(e, now_ms, half);
        Some(RelayStats {
            good: e.good,
            bad: e.bad,
            rtt_ms: e.rtt_ms,
            reliability: reliability(e),
            banned: banned(e, &cfg),
        })
    }

    /// Instantâneo para gravar em disco. `now_ms` é o mesmo relógio de [`Self::record`].
    pub fn export(&mut self, now_ms: u64) -> Vec<u8> {
        self.export_at(now_ms, unix_now())
    }

    fn export_at(&mut self, now_ms: u64, unix_secs: u64) -> Vec<u8> {
        let half = self.cfg.half_life_ms;
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&unix_secs.to_be_bytes());
        out.extend_from_slice(&(self.relays.len() as u32).to_be_bytes());
        for (addr, e) in self.relays.iter_mut() {
            decay(e, now_ms, half);
            out.push(addr.len() as u8);
            out.extend_from_slice(addr.as_bytes());
            out.extend_from_slice(&e.good.to_be_bytes());
            out.extend_from_slice(&e.bad.to_be_bytes());
            out.extend_from_slice(&e.rtt_ms.unwrap_or(f32::NAN).to_be_bytes());
        }
        out
    }

    /// Reconstrói a partir de [`Self::export`]. `now_ms` é o relógio do novo processo.
    pub fn import(cfg: ReputationConfig, bytes: &[u8], now_ms: u64) -> Result<Self, ImportError> {
        Self::import_at(cfg, bytes, now_ms, unix_now())
    }

    fn import_at(cfg: ReputationConfig, bytes: &[u8], now_ms: u64, unix_secs: u64) -> Result<Self, ImportError> {
        let mut r = Reader(bytes);
        if r.take(MAGIC.len())? != MAGIC {
            return Err(ImportError::BadMagic);
        }
        let saved = u64::from_be_bytes(r.take(8)?.try_into().unwrap());
        let count = u32::from_be_bytes(r.take(4)?.try_into().unwrap()) as usize;
        if count > cfg.max_entries {
            return Err(ImportError::TooMany);
        }
        // Relógio que andou para trás: não rejuvenesce nem envelhece.
        let elapsed_ms = unix_secs.saturating_sub(saved).saturating_mul(1000);
        let f = 0.5f64.powf(elapsed_ms as f64 / cfg.half_life_ms as f64);
        let mut relays = HashMap::with_capacity(count);
        for _ in 0..count {
            let n = r.take(1)?[0] as usize;
            if n == 0 || n > crate::descriptor::MAX_ADDR_LEN {
                return Err(ImportError::BadEntry);
            }
            let addr = std::str::from_utf8(r.take(n)?).map_err(|_| ImportError::BadEntry)?.to_string();
            let good = f64::from_be_bytes(r.take(8)?.try_into().unwrap());
            let bad = f64::from_be_bytes(r.take(8)?.try_into().unwrap());
            let rtt = f32::from_be_bytes(r.take(4)?.try_into().unwrap());
            if !(good.is_finite() && bad.is_finite()) || !(0.0..=MAX_COUNT).contains(&good) || !(0.0..=MAX_COUNT).contains(&bad) {
                return Err(ImportError::BadEntry);
            }
            let rtt_ms = match rtt {
                x if x.is_nan() => None,
                x if (0.0..=60_000.0).contains(&x) => Some(x),
                _ => return Err(ImportError::BadEntry),
            };
            relays.insert(addr, Entry { good: good * f, bad: bad * f, rtt_ms, updated_ms: now_ms });
        }
        if !r.0.is_empty() {
            return Err(ImportError::BadEntry);
        }
        Ok(Self { cfg, relays })
    }

    pub fn len(&self) -> usize {
        self.relays.len()
    }

    pub fn is_empty(&self) -> bool {
        self.relays.is_empty()
    }

    fn entry(&mut self, addr: &str, now_ms: u64) -> &mut Entry {
        if !self.relays.contains_key(addr) && self.relays.len() >= self.cfg.max_entries {
            // Descarta o menos recentemente atualizado.
            if let Some(oldest) = self.relays.iter().min_by_key(|(_, e)| e.updated_ms).map(|(k, _)| k.clone()) {
                self.relays.remove(&oldest);
            }
        }
        let half = self.cfg.half_life_ms;
        let e = self
            .relays
            .entry(addr.to_string())
            .or_insert(Entry { good: 0.0, bad: 0.0, rtt_ms: None, updated_ms: now_ms });
        decay(e, now_ms, half);
        e
    }
}

const MAGIC: &[u8; 8] = b"clamrep1";
/// Teto aceito para contagens lidas de disco (evita arquivo forjado fixar uma pontuação extrema).
const MAX_COUNT: f64 = 10_000.0;

#[derive(Debug, PartialEq, Eq)]
pub enum ImportError {
    BadMagic,
    Truncated,
    TooMany,
    BadEntry,
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BadMagic => "não é um arquivo de reputação do clam",
            Self::Truncated => "arquivo de reputação truncado",
            Self::TooMany => "arquivo de reputação com entradas demais",
            Self::BadEntry => "arquivo de reputação com entrada inválida",
        })
    }
}

impl std::error::Error for ImportError {}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ImportError> {
        if self.0.len() < n {
            return Err(ImportError::Truncated);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Grava `bytes` de forma atômica (temporário + `rename`), com `0600` no Unix.
pub fn save_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    // O modo só vale na criação: um temporário que sobrou de antes, com modo mais largo, não pode ser reaproveitado.
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Lê e valida o arquivo. Arquivo ausente vira reputação vazia.
pub fn load_file(cfg: ReputationConfig, path: &Path, now_ms: u64) -> io::Result<Reputation> {
    match std::fs::read(path) {
        Ok(b) => Reputation::import(cfg, &b, now_ms).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Reputation::new(cfg)),
        Err(e) => Err(e),
    }
}

/// Confiabilidade a priori de um relay sem histórico: (2 acertos, 1 erro).
const PRIOR_RELIABILITY: f64 = 2.0 / 3.0;

fn decay(e: &mut Entry, now_ms: u64, half_life_ms: u64) {
    let dt = now_ms.saturating_sub(e.updated_ms);
    if dt > 0 {
        let f = 0.5f64.powf(dt as f64 / half_life_ms as f64);
        e.good *= f;
        e.bad *= f;
        e.updated_ms = now_ms;
    }
}

fn reliability(e: &Entry) -> f64 {
    (e.good + 2.0) / (e.good + e.bad + 3.0)
}

fn banned(e: &Entry, cfg: &ReputationConfig) -> bool {
    let total = e.good + e.bad;
    total >= cfg.ban_min_evidence && e.good / total < cfg.ban_below
}

fn latency_factor(rtt_ms: Option<f32>, cfg: &ReputationConfig) -> f32 {
    match rtt_ms {
        Some(r) if r > cfg.latency_free_ms => 0.5f32.powf((r - cfg.latency_free_ms) / cfg.latency_scale_ms),
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    fn cand(addr: &str) -> Candidate {
        Candidate { addr: addr.into(), public: [0; 32], link_public: [0; 32], score: 1.0 }
    }

    fn score(rep: &mut Reputation, addr: &str, now: u64) -> Option<f32> {
        rep.adjust(vec![cand(addr)], now).first().map(|c| c.score)
    }

    fn healthy(rep: &mut Reputation, a: &str, b: &str, pings: usize, rtt: u64, now: u64) {
        for _ in 0..pings {
            rep.record(&Event::Rtt { first: a.into(), second: b.into(), rtt_ms: rtt }, now);
        }
    }

    fn failed(rep: &mut Reputation, a: &str, b: &str, reason: FailReason, now: u64) {
        rep.record(&Event::CircuitFailed { first: a.into(), second: b.into(), reason }, now);
    }

    #[test]
    fn unknown_relay_is_usable_but_ranks_below_a_proven_one() {
        let mut rep = Reputation::new(ReputationConfig::default());
        healthy(&mut rep, "bom", "bom2", 500, 60, 0);
        let (known, unknown) = (score(&mut rep, "bom", 0).unwrap(), score(&mut rep, "novo", 0).unwrap());
        assert!(unknown > 0.0 && known > unknown, "{known} vs {unknown}");
    }

    #[test]
    fn repeated_culprit_sinks_while_its_partners_recover() {
        let mut rep = Reputation::new(ReputationConfig::default());
        // "ruim" falha com parceiros diferentes; os parceiros também têm circuitos bons entre si.
        for i in 0..8 {
            let partner = format!("p{i}");
            failed(&mut rep, "ruim", &partner, FailReason::Silent, 0);
            healthy(&mut rep, &partner, "estavel", 100, 80, 0);
        }
        let bad = score(&mut rep, "ruim", 0);
        let good = score(&mut rep, "p3", 0).unwrap();
        assert!(bad.is_none_or(|b| b < good * 0.2), "ruim={bad:?} bom={good}");
    }

    #[test]
    fn build_timeout_blames_only_the_stalled_hop() {
        let mut rep = Reputation::new(ReputationConfig::default());
        failed(&mut rep, "a", "b", FailReason::BuildTimeout("create"), 0);
        failed(&mut rep, "c", "d", FailReason::BuildTimeout("extend"), 0);
        assert_eq!(rep.stats("a", 0).unwrap().bad, 1.0);
        assert_eq!(rep.stats("b", 0).unwrap().bad, 0.0);
        assert_eq!(rep.stats("c", 0).unwrap().bad, 0.0);
        assert_eq!(rep.stats("d", 0).unwrap().bad, 1.0);
    }

    #[test]
    fn ambiguous_failures_split_the_blame() {
        let mut rep = Reputation::new(ReputationConfig::default());
        failed(&mut rep, "a", "b", FailReason::Destroyed, 0);
        assert_eq!(rep.stats("a", 0).unwrap().bad, 0.5);
        assert_eq!(rep.stats("b", 0).unwrap().bad, 0.5);
    }

    #[test]
    fn banned_relay_leaves_the_list_and_returns_after_decay() {
        let mut rep = Reputation::new(ReputationConfig::default());
        for _ in 0..6 {
            failed(&mut rep, "x", "y", FailReason::BuildTimeout("create"), 0);
        }
        assert!(rep.stats("x", 0).unwrap().banned);
        assert!(score(&mut rep, "x", 0).is_none());
        // 6 erros decaem para < 4 de evidência em ~1 meia-vida; tira o banimento.
        assert!(score(&mut rep, "x", 90 * MIN).is_some());
    }

    #[test]
    fn slow_relays_are_penalised_and_fast_ones_are_not() {
        let mut rep = Reputation::new(ReputationConfig::default());
        healthy(&mut rep, "rapido", "rapido2", 500, 60, 0);
        healthy(&mut rep, "lento", "lento2", 500, 500, 0);
        let (fast, slow) = (score(&mut rep, "rapido", 0).unwrap(), score(&mut rep, "lento", 0).unwrap());
        assert!(slow < fast * 0.4, "lento={slow} rapido={fast}");
    }

    #[test]
    fn old_failures_fade() {
        let mut rep = Reputation::new(ReputationConfig::default());
        failed(&mut rep, "a", "b", FailReason::Silent, 0);
        let fresh = score(&mut rep, "a", 0).unwrap();
        let later = score(&mut rep, "a", 6 * 60 * MIN).unwrap();
        assert!(later > fresh);
    }

    #[test]
    fn table_is_bounded() {
        let mut rep = Reputation::new(ReputationConfig { max_entries: 10, ..Default::default() });
        for i in 0..50u64 {
            failed(&mut rep, &format!("r{i}"), &format!("s{i}"), FailReason::Silent, i);
        }
        assert!(rep.len() <= 10);
    }

    #[test]
    fn export_import_round_trips_and_decays_with_wall_time() {
        let mut rep = Reputation::new(ReputationConfig::default());
        healthy(&mut rep, "a:1", "b:2", 300, 150, 0);
        failed(&mut rep, "a:1", "b:2", FailReason::Silent, 0);
        let bytes = rep.export_at(0, 1_000);
        let mut same = Reputation::import_at(ReputationConfig::default(), &bytes, 0, 1_000).unwrap();
        assert_eq!(same.stats("a:1", 0), rep.stats("a:1", 0));
        // Uma meia-vida depois (3600 s), as contagens caem pela metade.
        let mut later = Reputation::import_at(ReputationConfig::default(), &bytes, 0, 1_000 + 3_600).unwrap();
        let (s0, s1) = (rep.stats("a:1", 0).unwrap(), later.stats("a:1", 0).unwrap());
        assert!((s1.bad - s0.bad / 2.0).abs() < 1e-9 && (s1.good - s0.good / 2.0).abs() < 1e-9);
        assert_eq!(s1.rtt_ms, s0.rtt_ms);
    }

    #[test]
    fn import_rejects_garbage_without_panicking() {
        let cfg = ReputationConfig::default();
        let mut rep = Reputation::new(cfg);
        healthy(&mut rep, "a:1", "b:2", 10, 80, 0);
        let good = rep.export_at(0, 0);
        assert_eq!(Reputation::import(cfg, b"", 0).err(), Some(ImportError::Truncated));
        assert_eq!(Reputation::import(cfg, b"outro-arquivo-qualquer", 0).err(), Some(ImportError::BadMagic));
        for cut in 0..good.len() {
            assert!(Reputation::import(cfg, &good[..cut], 0).is_err(), "corte em {cut}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(Reputation::import(cfg, &trailing, 0).err(), Some(ImportError::BadEntry));
        // Valor absurdo forjado em `good` do primeiro relay.
        let mut forged = good.clone();
        let at = 8 + 8 + 4 + 1 + "a:1".len();
        forged[at..at + 8].copy_from_slice(&f64::INFINITY.to_be_bytes());
        assert_eq!(Reputation::import(cfg, &forged, 0).err(), Some(ImportError::BadEntry));
        let small = ReputationConfig { max_entries: 1, ..cfg };
        assert_eq!(Reputation::import(small, &good, 0).err(), Some(ImportError::TooMany));
    }

    #[test]
    fn file_round_trip_is_atomic_and_private() {
        let dir = std::env::temp_dir().join(format!("clam-rep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rep.bin");
        let cfg = ReputationConfig::default();
        assert!(load_file(cfg, &path, 0).unwrap().is_empty());
        let mut rep = Reputation::new(cfg);
        healthy(&mut rep, "a:1", "b:2", 10, 80, 0);
        save_file(&path, &rep.export(0)).unwrap();
        assert_eq!(load_file(cfg, &path, 0).unwrap().len(), 2);
        assert!(!path.with_extension("tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::write(&path, b"lixo").unwrap();
        assert!(load_file(cfg, &path, 0).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unrelated_events_are_ignored() {
        let mut rep = Reputation::new(ReputationConfig::default());
        rep.record(&Event::NoCircuit, 0);
        rep.record(&Event::Data(vec![1]), 0);
        assert!(rep.is_empty());
    }
}
