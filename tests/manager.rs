//! Gerente de circuitos em rede simulada: montagem, reserva e failover.
//! Tempo virtual em ms; a rede entrega instantaneamente (a latência real
//! entra na configuração de `dead_after_ms`, não aqui).

use std::collections::{HashMap, HashSet, VecDeque};

use clam::circuit::{
    Cell,
    manager::{Candidate, CircuitManager, Event, FailReason, ManagerConfig, Out, SendError},
    relay::{Action, LinkId, RelayConfig, RelayNode},
};
use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};

const CLIENT_LINK: LinkId = 1;

struct Net {
    nodes: HashMap<String, RelayNode>,
    wires: HashMap<(String, LinkId), (String, LinkId)>,
    next_in: LinkId,
    queue: VecDeque<(String, LinkId, Cell)>,
    inbox: VecDeque<(String, Cell)>,
    down: HashSet<String>,
    now: u64,
    mgr: CircuitManager,
    /// (instante, evento) de tudo que o gerente reportou.
    events: Vec<(u64, Event)>,
    /// (instante, relay) de cada célula enviada pelo cliente e recebida dele.
    tx_log: Vec<(u64, String)>,
    rx_log: Vec<(u64, String)>,
}

impl Net {
    fn new(names: &[&str]) -> Self {
        Self::with_cfg(names, ManagerConfig::default())
    }

    fn with_cfg(names: &[&str], cfg: ManagerConfig) -> Self {
        let nodes: HashMap<String, RelayNode> = names
            .iter()
            .map(|n| (n.to_string(), RelayNode::new(StaticSecret::random_from_rng(OsRng), RelayConfig::default(), 0)))
            .collect();
        let relays = names
            .iter()
            .map(|n| Candidate { addr: n.to_string(), public: nodes[*n].public_key(), link_public: nodes[*n].public_key(), score: 1.0 })
            .collect();
        let mut wires = HashMap::new();
        for n in names {
            wires.insert((n.to_string(), CLIENT_LINK), ("client".to_string(), 0));
        }
        Net {
            nodes,
            wires,
            next_in: 2,
            queue: VecDeque::new(),
            inbox: VecDeque::new(),
            down: HashSet::new(),
            now: 0,
            mgr: CircuitManager::new(cfg, "contato:9", relays),
            events: vec![],
            tx_log: vec![],
            rx_log: vec![],
        }
    }

    fn deliver(&mut self, node: &str, link: LinkId, cell: Cell) {
        if !self.down.contains(node) {
            self.queue.push_back((node.into(), link, cell));
        }
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
                    if peer == "client" {
                        self.inbox.push_back((node.into(), cell));
                    } else {
                        self.deliver(&peer, plink, cell);
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
                Action::Deliver { circ, payload } => {
                    let mut reply = b"eco:".to_vec();
                    reply.extend_from_slice(&payload);
                    let follow = self.nodes.get_mut(node).unwrap().stream_data(circ, &reply).unwrap();
                    self.exec(node, follow);
                }
                Action::OpenStream { .. } | Action::CloseStream { .. } => {}
            }
        }
    }

    /// Executa saídas do gerente até a rede ficar quieta.
    fn drive(&mut self, mut outs: Vec<Out>) {
        loop {
            for o in outs.drain(..) {
                match o {
                    Out::Send { to, cell } => {
                        self.tx_log.push((self.now, to.clone()));
                        self.deliver(&to, CLIENT_LINK, cell);
                    }
                    Out::Event(e) => self.events.push((self.now, e)),
                }
            }
            self.pump();
            let Some((from, cell)) = self.inbox.pop_front() else { break };
            self.rx_log.push((self.now, from));
            outs = self.mgr.handle_cell(cell, self.now);
        }
    }

    fn start(&mut self) {
        let outs = self.mgr.start(self.now);
        self.drive(outs);
    }

    /// Avança `ms`, em passos de 10 ms; opcionalmente envia um quadro por passo.
    fn run(&mut self, ms: u64, send: bool) {
        for _ in 0..ms / 10 {
            self.now += 10;
            let outs = self.mgr.tick(self.now);
            self.drive(outs);
            if send
                && let Ok(outs) = self.mgr.send(b"voz") {
                    self.drive(outs);
                }
        }
    }

    fn data_times(&self) -> Vec<u64> {
        self.events.iter().filter(|(_, e)| matches!(e, Event::Data(_))).map(|(t, _)| *t).collect()
    }
}

#[test]
fn builds_active_and_disjoint_standby() {
    let mut n = Net::new(&["A", "B", "C", "D", "E", "F"]);
    n.start();
    n.run(300, false);
    let st = n.mgr.status();
    let (a, s) = (st.active.expect("ativo"), st.standby.expect("reserva"));
    assert!(a.ready && s.ready);
    let used: HashSet<_> = [&a.first, &a.second, &s.first, &s.second].into_iter().collect();
    assert_eq!(used.len(), 4, "reserva deve usar relays diferentes do ativo");
    assert!(n.events.iter().any(|(_, e)| matches!(e, Event::Ready { .. })));
}

#[test]
fn media_flows_through_active_circuit() {
    let mut n = Net::new(&["A", "B", "C", "D"]);
    n.start();
    n.run(300, false);
    let outs = n.mgr.send(b"oi").unwrap();
    n.drive(outs);
    assert!(n.events.iter().any(|(_, e)| *e == Event::Data(b"eco:oi".to_vec())));
}

#[test]
fn failover_keeps_silence_under_500ms() {
    for victim in ["first", "second"] {
        let mut n = Net::new(&["A", "B", "C", "D", "E", "F"]);
        n.start();
        n.run(500, true);
        let before = n.data_times().len();
        assert!(before > 10, "mídia deve fluir antes da falha");

        let st = n.mgr.status();
        let active = st.active.unwrap();
        let dead = if victim == "first" { active.first.clone() } else { active.second.clone() };
        n.down.insert(dead);
        let killed_at = n.now;
        let last_ok = *n.data_times().last().unwrap();

        n.run(2_000, true);
        let times = n.data_times();
        let resumed = *times.iter().find(|t| **t > killed_at && **t > last_ok + 100).expect("mídia retomou");
        let silence = resumed - last_ok;
        assert!(silence < 500, "silêncio de {silence} ms com falha no {victim} relay");

        let fo = n.events.iter().find_map(|(_, e)| match e {
            Event::FailedOver { silence_ms } => Some(*silence_ms),
            _ => None,
        });
        assert!(fo.expect("evento FailedOver") < 500);
        assert!(n.events.iter().any(|(_, e)| matches!(
            e,
            Event::CircuitFailed { reason: FailReason::Silent, .. }
        )));
    }
}

#[test]
fn standby_is_rebuilt_after_failover_without_the_dead_relays() {
    let mut n = Net::new(&["A", "B", "C", "D", "E", "F"]);
    n.start();
    n.run(300, false);
    let old = n.mgr.status().active.unwrap();
    n.down.insert(old.first.clone());
    n.run(1_500, true);
    let st = n.mgr.status();
    let (a, s) = (st.active.expect("ativo"), st.standby.expect("novo reserva"));
    assert!(a.ready && s.ready);
    for r in [&a.first, &a.second, &s.first, &s.second] {
        assert!(*r != old.first && *r != old.second, "relay punido reaparece");
    }
}

#[test]
fn reports_no_circuit_and_refuses_send_without_relays() {
    let mut n = Net::new(&["A"]);
    n.start();
    n.run(100, false);
    assert_eq!(n.mgr.send(b"x").unwrap_err(), SendError::Unavailable);
    assert!(n.mgr.status().active.is_none());
}

#[test]
fn build_timeout_punishes_and_retries_with_other_relays() {
    let mut n = Net::new(&["A", "B", "C", "D", "E", "F"]);
    // Dois relays mortos desde o início: a montagem que os tocar estoura o prazo e é refeita.
    n.down.insert("A".into());
    n.down.insert("B".into());
    n.start();
    n.run(12_000, false);
    let a = n.mgr.status().active.expect("acabou montando");
    assert!(a.ready);
    assert!(a.first != "A" && a.first != "B" && a.second != "A" && a.second != "B");
}

/// Células trocadas com `relay` na janela `(from, to]`: (enviadas, recebidas).
fn window(n: &Net, relay: &str, from: u64, to: u64) -> (usize, usize) {
    let count = |log: &[(u64, String)]| log.iter().filter(|(t, r)| *t > from && *t <= to && r == relay).count();
    (count(&n.tx_log), count(&n.rx_log))
}

/// Com cover de ponta a ponta o relay vê o mesmo número de células por segundo no silêncio e
/// falando, nos dois sentidos, e a mídia continua chegando.
#[test]
fn end_to_end_cover_hides_talk_from_silence() {
    let mut n = Net::new(&["A", "B", "C", "D"]);
    n.mgr.set_cover(Some(20));
    n.start();
    n.run(500, false);
    let first = n.mgr.status().active.expect("ativo").first;

    let t0 = n.now;
    n.run(1000, false);
    let silence = window(&n, &first, t0, n.now);

    let t1 = n.now;
    let before = n.data_times().len();
    for i in 0..100 {
        n.now += 10;
        let outs = n.mgr.tick(n.now);
        n.drive(outs);
        if i % 2 == 0
            && let Ok(outs) = n.mgr.send(b"voz") {
                n.drive(outs);
            }
    }
    let talk = window(&n, &first, t1, n.now);

    assert!((49..=51).contains(&silence.0), "silêncio, subida: {silence:?}");
    assert!((49..=51).contains(&talk.0), "voz, subida: {talk:?}");
    assert!((48..=52).contains(&silence.1), "silêncio, descida: {silence:?}");
    assert!((48..=52).contains(&talk.1), "voz, descida: {talk:?}");
    assert!(n.data_times().len() > before + 40, "a mídia deve chegar de volta pelo cover: {} -> {}", before, n.data_times().len());
}

/// Sem cover, o silêncio quase não gera células (só ping): é a assinatura que o cover esconde.
#[test]
fn without_end_to_end_cover_silence_is_visible() {
    let mut n = Net::new(&["A", "B", "C", "D"]);
    n.start();
    n.run(500, false);
    let first = n.mgr.status().active.expect("ativo").first;
    let t0 = n.now;
    n.run(1000, false);
    let (up, _) = window(&n, &first, t0, n.now);
    assert!(up <= 12, "só pings esperados, veio {up}");
}

/// O circuito com cover morre e faz failover como antes: o cover também conta como sinal de vida.
#[test]
fn failover_works_with_end_to_end_cover() {
    let mut n = Net::new(&["A", "B", "C", "D", "E", "F"]);
    n.mgr.set_cover(Some(20));
    n.start();
    n.run(500, true);
    let st = n.mgr.status();
    let a = st.active.expect("ativo");
    n.down.insert(a.first);
    let t = n.now;
    n.run(1000, true);
    let gap = n.data_times().into_iter().filter(|x| *x > t).collect::<Vec<_>>();
    assert!(!gap.is_empty(), "mídia deve voltar pelo reserva");
    assert!(gap[0] - t < 500, "silêncio {} ms", gap[0] - t);
}

/// O reserva com cover manda e recebe o mesmo ritmo do ativo; sem `standby_cover` só pinga.
#[test]
fn standby_cover_matches_active_cadence() {
    let rate = |standby_cover: bool| {
        let mut n = Net::with_cfg(&["A", "B", "C", "D", "E", "F"], ManagerConfig { cover_ms: Some(20), standby_cover, ..Default::default() });
        n.start();
        n.run(1500, false);
        let st = n.mgr.status();
        let (a, s) = (st.active.expect("ativo").first, st.standby.expect("reserva").first);
        assert_ne!(a, s, "com 6 relays o reserva evita o primeiro do ativo");
        let t0 = n.now;
        n.run(1000, false);
        (window(&n, &s, t0, n.now), window(&n, &a, t0, n.now))
    };
    let (standby, active) = rate(true);
    assert!((48..=52).contains(&standby.0), "reserva, subida: {standby:?}");
    assert!((48..=52).contains(&standby.1), "reserva, descida: {standby:?}");
    assert!((48..=52).contains(&active.0), "ativo, subida: {active:?}");
    let (quiet, _) = rate(false);
    assert!(quiet.0 <= 3, "sem cover no reserva, só ping de 1 s: {quiet:?}");
}

/// Failover com cover no reserva: o reserva já é cover, então a mídia volta rápido.
#[test]
fn failover_keeps_cadence_with_standby_cover() {
    let mut n = Net::with_cfg(&["A", "B", "C", "D", "E", "F"], ManagerConfig { cover_ms: Some(20), ..Default::default() });
    n.start();
    n.run(1500, true);
    let st = n.mgr.status();
    let (a, s) = (st.active.expect("ativo").first, st.standby.expect("reserva").first);
    n.down.insert(a);
    let t = n.now;
    n.run(1000, true);
    let gap = n.data_times().into_iter().filter(|x| *x > t).collect::<Vec<_>>();
    assert!(!gap.is_empty() && gap[0] - t < 500, "mídia deve voltar pelo reserva");
    let t1 = n.now;
    n.run(1000, true);
    let w = window(&n, &s, t1, n.now);
    assert!((48..=52).contains(&w.0), "novo ativo, subida: {w:?}");
}

fn direct_cfg(exit: &str) -> ManagerConfig {
    ManagerConfig { direct: true, exit_addr: Some(exit.into()), ..Default::default() }
}

/// Direto: só o relay de saída vê o cliente, sem extend, sem reserva e sem outro relay no caminho.
#[test]
fn a_direct_circuit_uses_only_the_exit_relay() {
    let mut n = Net::with_cfg(&["A", "B", "C"], direct_cfg("B"));
    n.start();
    n.run(500, true);
    let st = n.mgr.status();
    let active = st.active.expect("ativo");
    assert!(active.ready);
    assert_eq!((active.first.as_str(), active.second.as_str()), ("B", "B"));
    assert!(st.standby.is_none(), "não há outro caminho para um reserva");
    assert!(n.events.iter().any(|(_, e)| matches!(e, Event::Ready { .. })));
    assert!(n.events.iter().any(|(_, e)| *e == Event::Data(b"eco:voz".to_vec())), "o eco volta");
    assert!(n.tx_log.iter().chain(&n.rx_log).all(|(_, r)| r == "B"), "só o relay de saída fala com o cliente");
    assert_eq!(n.wires.len(), 3, "o relay não abriu link para nenhum outro relay");
}

#[test]
fn a_direct_circuit_needs_the_exit_among_the_candidates() {
    let mut n = Net::with_cfg(&["A", "B"], direct_cfg("Z"));
    n.start();
    n.run(300, false);
    assert!(n.mgr.status().active.is_none());
    assert_eq!(n.mgr.send(b"x").unwrap_err(), SendError::Unavailable);
}

/// O relay de saída é o único caminho: sem cooldown, a nova tentativa acontece assim que ele volta.
#[test]
fn a_direct_circuit_is_rebuilt_when_the_exit_comes_back() {
    let mut n = Net::with_cfg(&["A", "B"], direct_cfg("B"));
    n.start();
    n.run(300, true);
    assert!(n.mgr.status().active.is_some_and(|a| a.ready));
    n.down.insert("B".into());
    n.run(1_000, true);
    assert!(n.events.iter().any(|(_, e)| matches!(e, Event::CircuitFailed { reason: FailReason::Silent, .. })));
    assert!(n.events.iter().any(|(_, e)| *e == Event::NoCircuit));
    n.down.remove("B");
    let before = n.data_times().len();
    n.run(5_000, true);
    assert!(n.mgr.status().active.is_some_and(|a| a.ready), "refeito sem esperar o cooldown");
    assert!(n.data_times().len() > before, "a mídia voltou");
}
