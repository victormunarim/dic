//! Gerente de circuitos (*sans-IO*): mantém um circuito ativo e outro **reserva**
//! já montado por relays diferentes, detecta falhas por ping e troca sem refazer
//! o handshake, para o silêncio ficar abaixo de 500 ms.
//!
//! Uso: chame [`CircuitManager::start`], depois
//! - [`CircuitManager::tick`] a cada ~10–20 ms,
//! - [`CircuitManager::handle_cell`] para cada célula vinda de qualquer primeiro relay,
//! - [`CircuitManager::send`] para cada quadro de áudio.
//!
//! Todos devolvem `Vec<Out>`: células a enviar ao primeiro relay do circuito
//! ([`Out::Send`]) e eventos para o app ([`Out::Event`]).
//!
//! Observações:
//! - O reserva também abre o fluxo até o destino. O contato vê duas sessões e
//!   deve tratar a mais recente como a válida; dados que chegam por qualquer
//!   um dos dois circuitos prontos são entregues ao app ([`Event::Data`]), então
//!   o app deve tolerar duplicatas (número de sequência no pacote de mídia).
//! - `dead_after_ms` precisa superar a ida e volta do circuito mais o intervalo
//!   de ping (com 4 travessias de ~50 ms, 350 ms é folgado). Com latências bem
//!   maiores, aumente `dead_after_ms` (o silêncio no failover cresce junto).

use std::collections::{HashMap, HashSet, VecDeque};

use rand_core::{OsRng, RngCore};

use super::{
    Cell, Cmd,
    client::{ClientCircuit, Event as CircuitEvent},
};

/// Relay candidato, vindo do descritor/diretório.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Endereço onde o relay é alcançado (primeiro salto: para onde o cliente envia).
    pub addr: String,
    /// Chave do circuito (handshake estilo ntor).
    pub public: [u8; 32],
    /// Chave do link (Noise NK), distinta da do circuito.
    pub link_public: [u8; 32],
    /// Maior é melhor (reputação, latência etc.).
    pub score: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct ManagerConfig {
    /// Ping ao circuito ativo. Padrão: 100 ms.
    pub ping_interval_ms: u64,
    /// Sem nenhum sinal do circuito ativo por este tempo ele é dado como morto. Padrão: 350 ms.
    pub dead_after_ms: u64,
    /// Ping ao reserva (mantém o circuito vivo nos relays). Padrão: 1 s.
    pub standby_ping_interval_ms: u64,
    pub standby_dead_after_ms: u64,
    /// Tempo máximo de cada etapa da montagem. Padrão: 3 s.
    pub build_timeout_ms: u64,
    /// Espera entre tentativas de montagem que falharam. Padrão: 500 ms.
    pub retry_ms: u64,
    /// Um relay que falhou fica fora das escolhas por este tempo. Padrão: 30 s.
    pub cooldown_ms: u64,
    /// Cover de ponta a ponta no circuito ativo: exatamente uma célula a cada intervalo
    /// (mídia, ping ou `Drop`), e o relay de saída responde uma por uma. `None` desliga.
    /// Esconde dos relays quando há voz ou silêncio. O tique do gerente (10–20 ms) limita
    /// o menor intervalo útil. Padrão: `None`.
    pub cover_ms: Option<u64>,
    /// Com `cover_ms` ligado, o circuito reserva também manda uma célula por intervalo
    /// (`Drop` ou ping) e o relay de saída responde. Assim o reserva não se distingue do
    /// ativo, e a troca no failover não muda o padrão de tráfego. Custa o dobro de banda
    /// do cover. Sem efeito se `cover_ms` é `None`. Padrão: `true`.
    pub standby_cover: bool,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            ping_interval_ms: 100,
            dead_after_ms: 350,
            standby_ping_interval_ms: 1_000,
            standby_dead_after_ms: 4_000,
            build_timeout_ms: 3_000,
            retry_ms: 500,
            cooldown_ms: 30_000,
            cover_ms: None,
            standby_cover: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// A montagem travou na etapa indicada.
    BuildTimeout(&'static str),
    /// Pronto, mas sem resposta dentro de `dead_after_ms`.
    Silent,
    /// Um relay derrubou o circuito.
    Destroyed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// O circuito ativo está verificado de ponta a ponta.
    Ready { rtt_ms: u64 },
    /// Dados vindos do destino.
    Data(Vec<u8>),
    /// O ativo caiu e o reserva assumiu. `silence_ms`: tempo desde o último sinal do ativo antigo.
    FailedOver { silence_ms: u64 },
    /// Não há circuito utilizável; o gerente está remontando.
    NoCircuit,
    /// Um circuito falhou. Alimente a reputação dos relays com isto.
    CircuitFailed { first: String, second: String, reason: FailReason },
    /// Medida de ida e volta fim a fim do circuito.
    Rtt { first: String, second: String, rtt_ms: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Entregue `cell` à conexão com o relay `to` (primeiro salto).
    Send { to: String, cell: Cell },
    Event(Event),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    /// Sem circuito ativo pronto.
    Unavailable,
    Circuit(super::CircuitError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Creating,
    Extending,
    Verifying,
    Ready,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Creating => "create",
            Phase::Extending => "extend",
            Phase::Verifying => "verify",
            Phase::Ready => "ready",
        }
    }
}

struct Slot {
    circuit: ClientCircuit,
    first: Candidate,
    second: Candidate,
    phase: Phase,
    phase_started: u64,
    last_rx: u64,
    next_ping: u64,
    outstanding: Option<(u64, u64)>,
    rtt_ms: Option<f32>,
    /// Células prontas esperando o próximo instante do cover (só o ativo recebe mídia).
    queue: VecDeque<Cell>,
    /// Ping esperando um instante vago: só ocupa o lugar de um `Drop`, nunca o de mídia.
    ping_cell: Option<Cell>,
    next_cover: u64,
}

/// Células em espera no cover; passando disso, as mais antigas (áudio velho) caem.
const MAX_COVER_QUEUE: usize = 16;

/// Retrato de um circuito, para diagnóstico.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotInfo {
    pub first: String,
    pub second: String,
    pub ready: bool,
    pub rtt_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub active: Option<SlotInfo>,
    pub standby: Option<SlotInfo>,
}

pub struct CircuitManager {
    cfg: ManagerConfig,
    dest: String,
    relays: Vec<Candidate>,
    active: Option<Slot>,
    standby: Option<Slot>,
    next_circ: u32,
    next_nonce: u64,
    next_active_attempt: u64,
    next_standby_attempt: u64,
    cooldown: HashMap<String, u64>,
    no_circuit_reported: bool,
    started: bool,
}

impl CircuitManager {
    /// `dest`: endereço que o último salto abre (o contato, via ponto de introdução).
    pub fn new(cfg: ManagerConfig, dest: &str, relays: Vec<Candidate>) -> Self {
        Self {
            cfg,
            dest: dest.into(),
            relays,
            active: None,
            standby: None,
            next_circ: 1,
            next_nonce: 1,
            next_active_attempt: 0,
            next_standby_attempt: 0,
            cooldown: HashMap::new(),
            no_circuit_reported: false,
            started: false,
        }
    }

    /// Atualiza a lista de relays (descoberta). Circuitos em uso não são afetados.
    pub fn set_relays(&mut self, relays: Vec<Candidate>) {
        self.relays = relays;
    }

    /// Liga ou desliga o cover de ponta a ponta (veja [`ManagerConfig::cover_ms`]).
    pub fn set_cover(&mut self, every_ms: Option<u64>) {
        self.cfg.cover_ms = every_ms;
    }

    pub fn status(&self) -> Status {
        let info = |s: &Slot| SlotInfo {
            first: s.first.addr.clone(),
            second: s.second.addr.clone(),
            ready: s.phase == Phase::Ready,
            rtt_ms: s.rtt_ms.map(|r| r as u64),
        };
        Status { active: self.active.as_ref().map(info), standby: self.standby.as_ref().map(info) }
    }

    /// Chave de link do relay em `addr`, se conhecido (o transporte a usa para autenticar o link).
    pub fn relay_key(&self, addr: &str) -> Option<[u8; 32]> {
        self.relays.iter().find(|c| c.addr == addr).map(|c| c.link_public)
    }

    pub fn start(&mut self, now: u64) -> Vec<Out> {
        self.started = true;
        self.tick(now)
    }

    /// Envia um quadro de mídia pelo circuito ativo.
    pub fn send(&mut self, payload: &[u8]) -> Result<Vec<Out>, SendError> {
        let slot = self.active.as_mut().filter(|s| s.phase == Phase::Ready).ok_or(SendError::Unavailable)?;
        let cell = slot.circuit.send_data(payload).map_err(SendError::Circuit)?;
        if self.cfg.cover_ms.is_some() {
            if slot.queue.len() >= MAX_COVER_QUEUE {
                slot.queue.pop_front();
            }
            slot.queue.push_back(cell);
            return Ok(vec![]);
        }
        Ok(vec![Out::Send { to: slot.first.addr.clone(), cell }])
    }

    pub fn tick(&mut self, now: u64) -> Vec<Out> {
        let mut out = Vec::new();
        if !self.started {
            return out;
        }
        self.cooldown.retain(|_, until| *until > now);

        if let Some(reason) = self.failure(self.active.as_ref(), now, true) {
            self.fail(true, reason, now, &mut out);
        }
        if let Some(reason) = self.failure(self.standby.as_ref(), now, false) {
            self.fail(false, reason, now, &mut out);
        }

        for (is_active, slot) in [(true, self.active.as_mut()), (false, self.standby.as_mut())] {
            let Some(slot) = slot else { continue };
            let covered = is_active || self.cfg.standby_cover;
            if slot.phase == Phase::Ready && now >= slot.next_ping {
                let every = if is_active { self.cfg.ping_interval_ms } else { self.cfg.standby_ping_interval_ms };
                slot.next_ping = now + every;
                let queued = covered && self.cfg.cover_ms.is_some();
                Self::ping(slot, &mut self.next_nonce, now, queued, &mut out);
            }
            if let (true, true, Some(every)) = (covered, slot.phase == Phase::Ready, self.cfg.cover_ms) {
                if now >= slot.next_cover {
                    // Atrasado mais que um intervalo: não recupera em rajada.
                    slot.next_cover = if now - slot.next_cover > every { now + every } else { slot.next_cover + every };
                    let cell = match slot.queue.pop_front().or_else(|| slot.ping_cell.take()) {
                        Some(c) => Some(c),
                        None => slot.circuit.cover().ok(),
                    };
                    if let Some(cell) = cell {
                        out.push(Out::Send { to: slot.first.addr.clone(), cell });
                    }
                }
            }
        }

        if self.active.is_none() && now >= self.next_active_attempt {
            match self.build(&HashSet::new(), now, &mut out) {
                Some(slot) => self.active = Some(slot),
                None => self.next_active_attempt = now + self.cfg.retry_ms,
            }
        }
        if self.standby.is_none() && now >= self.next_standby_attempt {
            if let Some(active) = &self.active {
                let (a, b) = (active.first.addr.clone(), active.second.addr.clone());
                // Do mais ao menos independente: evita ambos, depois só o primeiro, depois só o segundo.
                let tiers = [vec![a.clone(), b.clone()], vec![a], vec![b]];
                let slot = tiers
                    .into_iter()
                    .find_map(|avoid| self.build(&avoid.into_iter().collect(), now, &mut out));
                match slot {
                    Some(s) => self.standby = Some(s),
                    None => self.next_standby_attempt = now + self.cfg.retry_ms,
                }
            }
        }
        out
    }

    pub fn handle_cell(&mut self, cell: Cell, now: u64) -> Vec<Out> {
        let mut out = Vec::new();
        let is_active = match (&self.active, &self.standby) {
            (Some(s), _) if s.circuit.circ_id() == cell.circ => true,
            (_, Some(s)) if s.circuit.circ_id() == cell.circ => false,
            _ => return out,
        };
        let slot = if is_active { self.active.as_mut() } else { self.standby.as_mut() }.expect("achado acima");
        let result = slot.circuit.handle_cell(cell);
        match result {
            Err(_) => {} // célula inválida ou repetida: ignora; o ping decide se o circuito morreu
            Ok(CircuitEvent::Destroyed) => self.fail(is_active, FailReason::Destroyed, now, &mut out),
            Ok(CircuitEvent::HopReady(0)) => {
                let (addr, key, link) = (slot.second.addr.clone(), slot.second.public, slot.second.link_public);
                if let Ok(c) = slot.circuit.extend(&addr, &key, &link) {
                    out.push(Out::Send { to: slot.first.addr.clone(), cell: c });
                    slot.phase = Phase::Extending;
                    slot.phase_started = now;
                }
            }
            Ok(CircuitEvent::HopReady(_)) => {
                if let Ok(c) = slot.circuit.begin(&self.dest) {
                    out.push(Out::Send { to: slot.first.addr.clone(), cell: c });
                }
                slot.phase = Phase::Verifying;
                slot.phase_started = now;
                Self::ping(slot, &mut self.next_nonce, now, false, &mut out);
            }
            Ok(CircuitEvent::Padding) => slot.last_rx = now,
            Ok(CircuitEvent::Pong { nonce, .. }) => {
                slot.last_rx = now;
                if let Some((n, sent)) = slot.outstanding.filter(|(n, _)| *n == nonce) {
                    let _ = n;
                    slot.outstanding = None;
                    let rtt = now.saturating_sub(sent);
                    slot.rtt_ms = Some(slot.rtt_ms.map_or(rtt as f32, |old| old * 0.7 + rtt as f32 * 0.3));
                    out.push(Out::Event(Event::Rtt {
                        first: slot.first.addr.clone(),
                        second: slot.second.addr.clone(),
                        rtt_ms: rtt,
                    }));
                    Self::mark_ready(slot, is_active, rtt, now, &mut out);
                }
            }
            Ok(CircuitEvent::Data(d)) => {
                slot.last_rx = now;
                let rtt = slot.rtt_ms.map_or(0, |r| r as u64);
                Self::mark_ready(slot, is_active, rtt, now, &mut out);
                if slot.phase == Phase::Ready {
                    out.push(Out::Event(Event::Data(d)));
                }
            }
            Ok(CircuitEvent::Ended) => {}
        }
        out
    }

    fn mark_ready(slot: &mut Slot, is_active: bool, rtt: u64, now: u64, out: &mut Vec<Out>) {
        if slot.phase == Phase::Verifying {
            slot.phase = Phase::Ready;
            slot.phase_started = now;
            slot.next_ping = now;
            if is_active {
                out.push(Out::Event(Event::Ready { rtt_ms: rtt }));
            }
        }
    }

    /// `queued`: no cover o ping espera um instante vago em vez de sair na hora.
    fn ping(slot: &mut Slot, nonce_ctr: &mut u64, now: u64, queued: bool, out: &mut Vec<Out>) {
        let nonce = *nonce_ctr;
        *nonce_ctr += 1;
        let last = slot.circuit.hops() - 1;
        if let Ok(cell) = slot.circuit.ping(last, nonce) {
            slot.outstanding = Some((nonce, now));
            if queued {
                slot.ping_cell = Some(cell);
            } else {
                out.push(Out::Send { to: slot.first.addr.clone(), cell });
            }
        }
    }

    fn failure(&self, slot: Option<&Slot>, now: u64, is_active: bool) -> Option<FailReason> {
        let s = slot?;
        if s.phase != Phase::Ready {
            return (now.saturating_sub(s.phase_started) > self.cfg.build_timeout_ms)
                .then(|| FailReason::BuildTimeout(s.phase.name()));
        }
        let limit = if is_active { self.cfg.dead_after_ms } else { self.cfg.standby_dead_after_ms };
        (now.saturating_sub(s.last_rx) > limit).then_some(FailReason::Silent)
    }

    /// Descarta o circuito, avisa os relays, aplica cooldown e promove o reserva.
    fn fail(&mut self, is_active: bool, reason: FailReason, now: u64, out: &mut Vec<Out>) {
        let slot = if is_active { self.active.take() } else { self.standby.take() };
        let Some(slot) = slot else { return };
        let until = now + self.cfg.cooldown_ms;
        match reason {
            FailReason::BuildTimeout("create") => {
                self.cooldown.insert(slot.first.addr.clone(), until);
            }
            FailReason::BuildTimeout("extend") => {
                self.cooldown.insert(slot.second.addr.clone(), until);
            }
            _ => {
                // Não dá para saber qual dos dois caiu: pune os dois.
                self.cooldown.insert(slot.first.addr.clone(), until);
                self.cooldown.insert(slot.second.addr.clone(), until);
            }
        }
        out.push(Out::Send { to: slot.first.addr.clone(), cell: Cell::with_body(slot.circuit.circ_id(), Cmd::Destroy, &[]) });
        out.push(Out::Event(Event::CircuitFailed {
            first: slot.first.addr.clone(),
            second: slot.second.addr.clone(),
            reason,
        }));

        if is_active {
            let was_ready = slot.phase == Phase::Ready;
            match self.standby.take() {
                Some(sb) => {
                    if sb.phase == Phase::Ready {
                        if was_ready {
                            out.push(Out::Event(Event::FailedOver { silence_ms: now.saturating_sub(slot.last_rx) }));
                        } else {
                            // O ativo morreu montando: o reserva pronto nunca anunciou `Ready`.
                            out.push(Out::Event(Event::Ready { rtt_ms: sb.rtt_ms.map_or(0, |r| r as u64) }));
                        }
                    }
                    self.active = Some(sb);
                    self.next_standby_attempt = now;
                }
                None => {
                    if !self.no_circuit_reported {
                        self.no_circuit_reported = true;
                        out.push(Out::Event(Event::NoCircuit));
                    }
                    self.next_active_attempt = now;
                }
            }
        } else {
            self.next_standby_attempt = now + self.cfg.retry_ms;
        }
    }

    /// Escolhe dois relays distintos fora de `avoid` e do cooldown e inicia o circuito.
    fn build(&mut self, avoid: &HashSet<String>, now: u64, out: &mut Vec<Out>) -> Option<Slot> {
        let mut pool: Vec<&Candidate> = self
            .relays
            .iter()
            .filter(|c| !avoid.contains(&c.addr) && !self.cooldown.contains_key(&c.addr))
            .collect();
        if pool.len() < 2 {
            return None;
        }
        let first = take_one(&mut pool).clone();
        let second = take_one(&mut pool).clone();

        let circ = self.next_circ;
        self.next_circ = self.next_circ.wrapping_add(1).max(1);
        let (circuit, create) = ClientCircuit::create(circ, &first.public);
        out.push(Out::Send { to: first.addr.clone(), cell: create });
        self.no_circuit_reported = false;
        Some(Slot {
            circuit,
            first,
            second,
            phase: Phase::Creating,
            phase_started: now,
            last_rx: now,
            next_ping: now,
            outstanding: None,
            rtt_ms: None,
            queue: VecDeque::new(),
            ping_cell: None,
            next_cover: 0,
        })
    }
}

/// Sorteia entre os 3 de maior pontuação (espalha a carga) e remove do conjunto.
fn take_one<'a>(pool: &mut Vec<&'a Candidate>) -> &'a Candidate {
    pool.sort_by(|a, b| b.score.total_cmp(&a.score));
    let top = pool.len().min(3);
    let i = OsRng.next_u32() as usize % top;
    pool.remove(i)
}
