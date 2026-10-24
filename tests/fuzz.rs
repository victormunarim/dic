//! Fuzzing determinístico dos parsers e das máquinas de estado que lidam com
//! bytes de fora. Sem dependências: um PRNG fixo (xorshift) gera entradas
//! aleatórias e mutações de entradas válidas, e cada teste afirma o que deve
//! valer para qualquer entrada: nada entra em pânico, as tabelas ficam
//! limitadas e células adulteradas não dessincronizam um circuito saudável.
//!
//! Para variar a semente: `CLAM_FUZZ_SEED=123 CLAM_FUZZ_ITERS=1000000 cargo test --test fuzz`.

use clam::circuit::{
    CELL_LEN, Cell, Cmd, RelayCmd, RelayCell,
    client::{ClientCircuit, Event},
    manager::{Candidate, CircuitManager, ManagerConfig},
    relay::{Action, RelayConfig, RelayNode},
};
use clam::descriptor::RelayDescriptor;
use clam::reputation::{Reputation, ReputationConfig};
use clam::{ed25519_dalek::SigningKey, rand_core::OsRng, x25519_dalek::StaticSecret};

struct Rng(u64);

impl Rng {
    fn new(salt: u64) -> Self {
        let seed = std::env::var("CLAM_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0xC1A0_5EED);
        Self((seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
    /// Bytes aleatórios com tamanho aleatório abaixo de `max`.
    fn blob(&mut self, max: usize) -> Vec<u8> {
        let n = self.below(max);
        self.bytes(n)
    }
    /// Copia `src` com alguns bits trocados, bytes zerados/saturados e corte ou extensão.
    fn mutate(&mut self, src: &[u8]) -> Vec<u8> {
        let mut v = src.to_vec();
        for _ in 0..1 + self.below(4) {
            if v.is_empty() {
                break;
            }
            let i = self.below(v.len());
            match self.below(4) {
                0 => v[i] ^= 1 << self.below(8),
                1 => v[i] = 0,
                2 => v[i] = 0xFF,
                _ => v[i] = self.next() as u8,
            }
        }
        match self.below(6) {
            0 => v.truncate(self.below(v.len() + 1)),
            1 => {
                let extra = self.blob(8);
                v.extend(extra);
            }
            _ => {}
        }
        v
    }
}

fn iters(default: usize) -> usize {
    std::env::var("CLAM_FUZZ_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn random_cell(rng: &mut Rng, circs: u32) -> Cell {
    let cmd = [Cmd::Create, Cmd::Created, Cmd::Relay, Cmd::Destroy, Cmd::Padding][rng.below(5)];
    let mut body = [0u8; CELL_LEN];
    body.copy_from_slice(&rng.bytes(CELL_LEN));
    Cell { circ: rng.below(circs as usize) as u32, cmd, body }
}

fn relay(cfg: RelayConfig) -> RelayNode {
    RelayNode::new(StaticSecret::random_from_rng(OsRng), cfg, 0)
}

// ---------------------------------------------------------------------------
// Parsers puros
// ---------------------------------------------------------------------------

#[test]
fn cell_parser_never_panics_and_round_trips() {
    let mut rng = Rng::new(1);
    for _ in 0..iters(50_000) {
        let n = if rng.below(2) == 0 { Cell::WIRE_LEN } else { rng.below(2 * Cell::WIRE_LEN) };
        let buf = rng.bytes(n);
        if let Ok(c) = Cell::from_bytes(&buf) {
            assert_eq!(c.to_bytes(), buf);
        }
    }
}

#[test]
fn relay_cell_decoder_never_panics() {
    let mut rng = Rng::new(2);
    let valid = RelayCell::new(RelayCmd::Data, b"carga".to_vec()).encode(200).unwrap();
    for _ in 0..iters(50_000) {
        let buf = if rng.below(2) == 0 { rng.blob(CELL_LEN + 8) } else { rng.mutate(&valid) };
        let _ = RelayCell::decode(&buf);
    }
}

#[test]
fn descriptor_parser_rejects_mutations_without_panicking() {
    let mut rng = Rng::new(3);
    let key = SigningKey::generate(&mut OsRng);
    let good = RelayDescriptor::sign(&key, [1; 32], [2; 32], "203.0.113.9:4001", 500, 1_000, 3_600).unwrap().to_bytes();
    for _ in 0..iters(3_000) {
        let buf = if rng.below(4) == 0 { rng.blob(300) } else { rng.mutate(&good) };
        if let Ok(d) = RelayDescriptor::from_bytes(&buf) {
            // Só passa na verificação se for byte a byte o descritor assinado.
            if d.verify(1_000, 300).is_ok() {
                assert_eq!(buf, good, "descritor adulterado passou na verificação");
            }
        }
    }
}

#[test]
fn reputation_import_rejects_mutations_without_panicking() {
    let mut rng = Rng::new(4);
    let cfg = ReputationConfig::default();
    let mut rep = Reputation::new(cfg);
    for i in 0..5 {
        rep.record(
            &clam::circuit::manager::Event::Rtt { first: format!("10.0.0.{i}:1"), second: "10.0.0.9:1".into(), rtt_ms: 80 },
            0,
        );
    }
    let good = rep.export(0);
    for _ in 0..iters(30_000) {
        let buf = if rng.below(4) == 0 { rng.blob(200) } else { rng.mutate(&good) };
        if let Ok(mut r) = Reputation::import(cfg, &buf, 0) {
            assert!(r.len() <= cfg.max_entries);
            // Qualquer coisa aceita tem de produzir pontuações finitas e não negativas.
            let cand = |a: &str| Candidate { addr: a.into(), public: [0; 32], link_public: [0; 32], score: 1.0 };
            for c in r.adjust(vec![cand("10.0.0.1:1"), cand("10.0.0.9:1"), cand("x:1")], 1) {
                assert!(c.score.is_finite() && c.score >= 0.0, "{c:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Relay
// ---------------------------------------------------------------------------

#[test]
fn relay_survives_random_cells() {
    let mut rng = Rng::new(5);
    let cfg = RelayConfig::default();
    let mut node = relay(cfg);
    let mut now = 0;
    for i in 0..iters(30_000) {
        now += 1;
        let cell = random_cell(&mut rng, 64);
        let link = rng.below(8) as u64;
        let _ = node.handle_cell(link, cell, now);
        if i % 1000 == 0 {
            let _ = node.tick(now);
        }
        assert!(node.circuit_count() <= cfg.max_circuits);
    }
}

#[test]
fn create_flood_is_bounded() {
    let mut rng = Rng::new(6);
    let cfg = RelayConfig::default();
    let mut node = relay(cfg);
    let public = node.public_key();
    let created = |actions: &[Action]| {
        actions.iter().filter(|a| matches!(a, Action::Send { cell, .. } if cell.cmd == Cmd::Created)).count()
    };
    // 20 000 Creates válidos, de muitos links e ids, no mesmo instante: só a rajada passa
    // (cada um aceito custa um Diffie-Hellman).
    let mut accepted = 0;
    for i in 0..20_000u32 {
        let (_cc, create) = ClientCircuit::create(i % 5000 + 1, &public);
        accepted += created(&node.handle_cell((i / 5000) as u64, create, 0));
        assert!(node.circuit_count() <= cfg.max_circuits);
    }
    assert_eq!(accepted, cfg.create_burst as usize);
    assert_eq!(node.stats.refused_create_rate, 20_000 - accepted as u64);

    // Abrir e derrubar em loop não contorna o teto: em 10 s entram ~creates_per_sec por segundo.
    let mut node = relay(cfg);
    let mut accepted = 0;
    for ms in 0..10_000u64 {
        let (_cc, create) = ClientCircuit::create(1, &public);
        let got = created(&node.handle_cell(0, create, ms));
        accepted += got;
        if got > 0 {
            let _ = node.handle_cell(0, Cell::with_body(1, Cmd::Destroy, &[]), ms);
        }
    }
    let ceiling = cfg.create_burst as usize + 10 * cfg.creates_per_sec as usize;
    assert!(accepted <= ceiling, "{accepted} criações em 10 s (teto {ceiling})");
    assert!(accepted >= 10 * cfg.creates_per_sec as usize, "limite apertado demais: {accepted}");
    let _ = &mut rng;
}

#[test]
fn tampered_cells_do_not_desync_a_healthy_circuit() {
    let mut rng = Rng::new(7);
    // Sem limite de banda: aqui se testa a autenticação, não o token bucket.
    let cfg = RelayConfig {
        max_auth_failures: u32::MAX,
        relay_bytes_per_sec: u64::MAX / 4096,
        circuit_bytes_per_sec: u64::MAX / 4096,
        ..Default::default()
    };
    let mut node = relay(cfg);
    // Resposta do relay a uma célula, entregue ao cliente na ordem: um tamper que só atinge
    // o padding ainda é uma célula válida e o relay responde a ela.
    fn feed(cc: &mut ClientCircuit, actions: Vec<Action>) -> Vec<Event> {
        actions
            .into_iter()
            .filter_map(|a| match a {
                Action::Send { cell, .. } => cc.handle_cell(cell).ok(),
                _ => None,
            })
            .collect()
    }
    let (mut cc, create) = ClientCircuit::create(1, &node.public_key());
    let reply = node.handle_cell(7, create, 0);
    let Action::Send { cell, .. } = &reply[0] else { panic!("sem Created") };
    assert_eq!(cc.handle_cell(cell.clone()).unwrap(), Event::HopReady(0));

    let mut now = 1;
    for round in 0..iters(2_000) {
        now += 1;
        // Lixo e adulterações de uma célula válida, no mesmo circuito.
        let ping = cc.ping(0, round as u64).unwrap();
        let mut bad = ping.clone();
        let mut body = rng.mutate(&ping.body);
        body.resize(CELL_LEN, 0);
        bad.body.copy_from_slice(&body);
        if bad.body != ping.body {
            feed(&mut cc, node.handle_cell(7, bad, now));
        }
        let mut junk = random_cell(&mut rng, 2);
        junk.circ = 1;
        junk.cmd = Cmd::Relay;
        feed(&mut cc, node.handle_cell(7, junk, now));

        // A célula legítima seguinte ainda tem de ser entendida.
        let ping = cc.ping(0, 1_000_000 + round as u64).unwrap();
        let events = feed(&mut cc, node.handle_cell(7, ping, now));
        assert_eq!(
            events,
            vec![Event::Pong { hop: 0, nonce: 1_000_000 + round as u64 }],
            "rodada {round}: circuito saudável perdeu o sincronismo após células adulteradas"
        );
    }
}

#[test]
fn repeated_auth_failures_drop_the_circuit() {
    let cfg = RelayConfig::default();
    let mut node = relay(cfg);
    let (mut cc, create) = ClientCircuit::create(1, &node.public_key());
    let Action::Send { cell, .. } = node.handle_cell(7, create, 0).remove(0) else { panic!() };
    cc.handle_cell(cell).unwrap();
    let mut rng = Rng::new(8);
    for i in 0..cfg.max_auth_failures + 2 {
        let mut junk = random_cell(&mut rng, 2);
        junk.circ = 1;
        junk.cmd = Cmd::Relay;
        let _ = node.handle_cell(7, junk, 1 + i as u64);
    }
    assert_eq!(node.circuit_count(), 0, "circuito com autenticação falhando não foi derrubado");
}

// ---------------------------------------------------------------------------
// Cliente e gerente
// ---------------------------------------------------------------------------

#[test]
fn client_circuit_survives_random_and_mutated_cells() {
    let mut rng = Rng::new(9);
    let mut node = relay(RelayConfig::default());
    let (mut cc, create) = ClientCircuit::create(1, &node.public_key());
    let Action::Send { cell: created, .. } = node.handle_cell(7, create, 0).remove(0) else { panic!() };
    for _ in 0..iters(30_000) {
        let mut c = if rng.below(2) == 0 { random_cell(&mut rng, 3) } else { created.clone() };
        c.circ = 1;
        if rng.below(2) == 0 {
            let m = rng.mutate(&c.body);
            let n = m.len().min(CELL_LEN);
            c.body[..n].copy_from_slice(&m[..n]);
        }
        let _ = cc.handle_cell(c);
    }
}

#[test]
fn manager_survives_random_cells_and_stale_ticks() {
    let mut rng = Rng::new(10);
    let nodes: Vec<RelayNode> = (0..4).map(|_| relay(RelayConfig::default())).collect();
    let cands: Vec<Candidate> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| Candidate {
            addr: format!("n{i}"),
            public: n.public_key(),
            link_public: n.public_key(),
            score: 1.0,
        })
        .collect();
    let mut mgr = CircuitManager::new(ManagerConfig::default(), "dest:1", cands);
    let mut now = 0;
    let _ = mgr.start(now);
    for _ in 0..iters(20_000) {
        now += rng.below(40) as u64;
        let _ = mgr.tick(now);
        let mut c = random_cell(&mut rng, 6);
        c.circ = 1 + rng.below(4) as u32; // ids que o gerente realmente usa
        let _ = mgr.handle_cell(c, now);
        let _ = mgr.send(&rng.blob(100));
    }
}
