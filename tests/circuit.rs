//! Circuito de 2 saltos ponta a ponta, em rede simulada na memória.

use std::collections::{HashMap, VecDeque};

use clam::circuit::{
    CELL_LEN, Cell, Cmd, MAX_HOPS, max_data,
    client::{ClientCircuit, Event},
    relay::{Action, LinkId, RelayConfig, RelayNode},
};
use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};

struct Sim {
    nodes: HashMap<String, RelayNode>,
    wires: HashMap<(String, LinkId), (String, LinkId)>,
    next_in: LinkId,
    queue: VecDeque<(String, LinkId, Cell)>,
    /// Tudo que passou por qualquer link: (de, para, célula).
    observed: Vec<(String, String, Cell)>,
    delivered: Vec<(String, Vec<u8>)>,
    closed_streams: usize,
    client_inbox: VecDeque<Cell>,
    echo: bool,
    now: u64,
}

impl Sim {
    fn new(names: &[&str], cfg: RelayConfig) -> Self {
        let nodes = names
            .iter()
            .map(|n| (n.to_string(), RelayNode::new(StaticSecret::random_from_rng(OsRng), cfg, 0)))
            .collect();
        let mut sim = Sim {
            nodes,
            wires: HashMap::new(),
            next_in: 1,
            queue: VecDeque::new(),
            observed: vec![],
            delivered: vec![],
            closed_streams: 0,
            client_inbox: VecDeque::new(),
            echo: true,
            now: 0,
        };
        // O cliente fala com "A" pelo link 1.
        sim.wires.insert(("A".into(), 1), ("client".into(), 0));
        sim.next_in = 2;
        sim
    }

    fn pubkey(&self, n: &str) -> [u8; 32] {
        self.nodes[n].public_key()
    }

    fn client_send(&mut self, cell: Cell) {
        self.observed.push(("client".into(), "A".into(), cell.clone()));
        self.queue.push_back(("A".into(), 1, cell));
        self.pump();
    }

    fn pump(&mut self) {
        while let Some((node, link, cell)) = self.queue.pop_front() {
            let now = self.now;
            let actions = self.nodes.get_mut(&node).unwrap().handle_cell(link, cell, now);
            self.exec(&node, actions);
        }
    }

    fn exec(&mut self, node: &str, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Send { link, cell } => {
                    let (peer, plink) = self.wires[&(node.to_string(), link)].clone();
                    self.observed.push((node.into(), peer.clone(), cell.clone()));
                    if peer == "client" {
                        self.client_inbox.push_back(cell);
                    } else {
                        self.queue.push_back((peer, plink, cell));
                    }
                }
                Action::Connect { link, addr, .. } => {
                    let id = self.next_in;
                    self.next_in += 1;
                    self.wires.insert((node.into(), link), (addr.clone(), id));
                    self.wires.insert((addr, id), (node.into(), link));
                    let follow = self.nodes.get_mut(node).unwrap().link_established(link);
                    self.exec(node, follow);
                }
                Action::OpenStream { .. } => {}
                Action::Deliver { circ, payload } => {
                    self.delivered.push((node.into(), payload.clone()));
                    if self.echo {
                        let mut reply = b"eco:".to_vec();
                        reply.extend_from_slice(&payload);
                        let follow = self.nodes.get_mut(node).unwrap().stream_data(circ, &reply).unwrap();
                        self.exec(node, follow);
                    }
                }
                Action::CloseStream { .. } => self.closed_streams += 1,
            }
        }
    }

    fn client_recv(&mut self, cc: &mut ClientCircuit) -> Event {
        let cell = self.client_inbox.pop_front().expect("nenhuma célula para o cliente");
        cc.handle_cell(cell).unwrap()
    }

    /// Monta A → B e abre o fluxo para "contato:9".
    fn build(&mut self) -> ClientCircuit {
        let (mut cc, create) = ClientCircuit::create(1, &self.pubkey("A"));
        self.client_send(create);
        assert_eq!(self.client_recv(&mut cc), Event::HopReady(0));
        let ext = cc.extend("B", &self.pubkey("B"), &self.pubkey("B")).unwrap();
        self.client_send(ext);
        assert_eq!(self.client_recv(&mut cc), Event::HopReady(1));
        let begin = cc.begin("contato:9").unwrap();
        self.client_send(begin);
        cc
    }
}

fn sim() -> Sim {
    Sim::new(&["A", "B"], RelayConfig::default())
}

#[test]
fn data_flows_both_ways_through_two_hops() {
    let mut s = sim();
    let mut cc = s.build();
    assert_eq!(cc.hops(), MAX_HOPS);

    let cell = cc.send_data(b"ola contato").unwrap();
    s.client_send(cell);
    assert_eq!(s.delivered, vec![("B".to_string(), b"ola contato".to_vec())]);
    assert_eq!(s.client_recv(&mut cc), Event::Data(b"eco:ola contato".to_vec()));
}

#[test]
fn every_cell_on_every_link_has_fixed_size_and_hides_payload() {
    let mut s = sim();
    let mut cc = s.build();
    let secret = b"conteudo-secreto-da-chamada";
    let cell = cc.send_data(secret).unwrap();
    s.client_send(cell);
    s.client_recv(&mut cc);

    for (from, to, cell) in &s.observed {
        assert_eq!(cell.body.len(), CELL_LEN);
        assert_eq!(cell.to_bytes().len(), Cell::WIRE_LEN, "{from}->{to}");
        if cell.cmd == Cmd::Relay {
            let hay = &cell.body[..];
            assert!(!hay.windows(secret.len()).any(|w| w == secret), "payload em claro em {from}->{to}");
        }
    }
}

#[test]
fn relay_a_never_learns_the_destination() {
    let mut s = sim();
    s.build();
    // O destino só aparece em texto dentro da camada de B; nenhuma célula o expõe.
    for (_, _, cell) in &s.observed {
        assert!(!cell.body.windows(6).any(|w| w == b"contat"));
    }
}

#[test]
fn ping_measures_each_hop() {
    let mut s = sim();
    let mut cc = s.build();
    for hop in 0..2 {
        let cell = cc.ping(hop, 100 + hop as u64).unwrap();
        s.client_send(cell);
        assert_eq!(s.client_recv(&mut cc), Event::Pong { hop, nonce: 100 + hop as u64 });
    }
    assert!(cc.ping(2, 0).is_err());
}

#[test]
fn replayed_and_tampered_cells_are_dropped() {
    let mut s = sim();
    let mut cc = s.build();
    let cell = cc.send_data(b"uma vez").unwrap();
    s.client_send(cell.clone());
    s.client_send(cell.clone()); // replay
    assert_eq!(s.delivered.len(), 1);
    assert_eq!(s.nodes["A"].stats.dropped_replay, 1);

    let mut bad = cc.send_data(b"adulterada").unwrap();
    bad.body[10] ^= 0xff;
    s.client_send(bad);
    assert_eq!(s.delivered.len(), 1);
    assert_eq!(s.nodes["A"].stats.dropped_auth, 1);
}

#[test]
fn wrong_relay_key_is_detected_by_the_client() {
    let mut s = sim();
    let wrong = StaticSecret::random_from_rng(OsRng);
    let wrong_pub = clam::x25519_dalek::PublicKey::from(&wrong).to_bytes();
    let (mut cc, create) = ClientCircuit::create(1, &wrong_pub);
    s.client_send(create);
    let cell = s.client_inbox.pop_front().unwrap();
    assert!(cc.handle_cell(cell).is_err());
}

#[test]
fn circuit_cannot_grow_past_max_hops_or_send_oversize() {
    let mut s = sim();
    let mut cc = s.build();
    assert!(cc.extend("C", &[7; 32], &[7; 32]).is_err());
    assert!(cc.send_data(&vec![0; max_data(1)]).is_ok());
    assert!(cc.send_data(&vec![0; max_data(1) + 1]).is_err());
}

#[test]
fn circuit_limit_refuses_new_circuits() {
    let mut s = Sim::new(&["A", "B"], RelayConfig { max_circuits: 1, ..Default::default() });
    let _first = s.build();
    let (mut cc2, create) = ClientCircuit::create(2, &s.pubkey("A"));
    s.client_send(create);
    assert_eq!(s.client_recv(&mut cc2), Event::Destroyed);
    assert_eq!(s.nodes["A"].stats.refused_full, 1);
}

#[test]
fn rate_limit_drops_floods_but_voice_rate_passes() {
    let cfg = RelayConfig { circuit_bytes_per_sec: 30_000, burst_ms: 500, ..Default::default() };
    let mut s = Sim::new(&["A", "B"], cfg);
    let mut cc = s.build();

    // Voz: 1 célula a cada 20 ms em cada sentido (25,6 kB/s com CELL_LEN=256) passa.
    let before = s.delivered.len();
    for i in 0..100 {
        s.now = 1_000 + i * 20;
        let cell = cc.send_data(b"voz").unwrap();
        s.client_send(cell);
    }
    assert_eq!(s.delivered.len() - before, 100);
    assert_eq!(s.nodes["A"].stats.dropped_rate, 0);

    // Inundação: 1000 células no mesmo instante.
    s.now = 10_000;
    for _ in 0..1000 {
        let cell = cc.send_data(b"x").unwrap();
        s.client_send(cell);
    }
    assert!(s.nodes["A"].stats.dropped_rate > 900);
    assert!(s.delivered.len() < before + 100 + 100);
}

#[test]
fn idle_circuits_expire_and_destroy_propagates() {
    let cfg = RelayConfig { idle_ms: 1000, ..Default::default() };
    let mut s = Sim::new(&["A", "B"], cfg);
    let mut cc = s.build();
    assert_eq!(s.nodes["A"].circuit_count(), 1);
    s.now = 5_000;
    let actions = s.nodes.get_mut("A").unwrap().tick(5_000);
    s.exec("A", actions);
    s.pump();
    assert_eq!(s.nodes["A"].circuit_count(), 0);
    assert_eq!(s.nodes["B"].circuit_count(), 0);
    assert_eq!(s.closed_streams, 1);
    assert_eq!(s.client_recv(&mut cc), Event::Destroyed);
}

#[test]
fn destroy_from_client_tears_down_both_relays() {
    let mut s = sim();
    let _cc = s.build();
    s.client_send(Cell::with_body(1, Cmd::Destroy, &[]));
    assert_eq!(s.nodes["A"].circuit_count(), 0);
    assert_eq!(s.nodes["B"].circuit_count(), 0);
}

#[test]
fn failed_connect_destroys_the_circuit() {
    let mut s = sim();
    let (mut cc, create) = ClientCircuit::create(1, &s.pubkey("A"));
    s.client_send(create);
    s.client_recv(&mut cc);
    let ext = cc.extend("B", &s.pubkey("B"), &s.pubkey("B")).unwrap();
    // Entrega o Extend ao nó A, mas "B" não conecta.
    let actions = s.nodes.get_mut("A").unwrap().handle_cell(1, ext, 0);
    let link = match &actions[0] {
        Action::Connect { link, .. } => *link,
        other => panic!("esperava Connect, veio {other:?}"),
    };
    let follow = s.nodes.get_mut("A").unwrap().link_failed(link);
    s.exec("A", follow);
    assert_eq!(s.client_recv(&mut cc), Event::Destroyed);
}
