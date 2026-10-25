//! Relay voluntário (*sans-IO*): mantém a tabela de circuitos, aplica limites
//! de banda e devolve [`Action`]s para o transporte executar.
//!
//! Contrato com o transporte:
//! - `LinkId` identifica uma conexão com um vizinho. Os ids de entrada são
//!   atribuídos pelo transporte e devem ser **menores que** [`FIRST_OUTBOUND_LINK`].
//! - [`Action::Connect`] pede uma conexão **nova** (sempre um `LinkId` novo, usado
//!   só pelos circuitos que este nó iniciou) e traz a chave pública do próximo relay,
//!   para o transporte autenticar o link; ao conectar, chame
//!   [`RelayNode::link_established`], ou [`RelayNode::link_failed`] se falhar.
//! - [`Action::OpenStream`]/[`Action::Deliver`]/[`Action::CloseStream`] só ocorrem
//!   no último salto; devolva dados do destino com [`RelayNode::stream_data`].
//! - Chame [`RelayNode::tick`] de vez em quando para expirar circuitos ociosos.

use std::collections::{HashMap, VecDeque};

use x25519_dalek::{PublicKey, StaticSecret};

use super::{
    CELL_LEN, Cell, CircuitError, Cmd, MAX_HOPS, RelayCell, RelayCmd,
    crypto::{LayerReceiver, LayerSender, server_handshake},
    link_len, max_data, pad_to_cell, relay_cell_len,
};

pub type LinkId = u64;
pub const FIRST_OUTBOUND_LINK: LinkId = 1 << 63;

/// Circuito visto por este nó: o link/circ do lado do cliente (anterior).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CircRef {
    pub link: LinkId,
    pub circ: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // um `Cell` por variante: encaixotar custaria uma alocação por célula
pub enum Action {
    Send { link: LinkId, cell: Cell },
    Connect { link: LinkId, addr: String, key: [u8; 32] },
    OpenStream { circ: CircRef, dest: String },
    Deliver { circ: CircRef, payload: Vec<u8> },
    CloseStream { circ: CircRef },
}

#[derive(Debug, Clone, Copy)]
pub struct RelayConfig {
    /// Circuitos simultâneos aceitos. Padrão: 256.
    pub max_circuits: usize,
    /// Banda total do relay em bytes/s (células, nos dois sentidos). Padrão: 1 MB/s.
    pub relay_bytes_per_sec: u64,
    /// Teto por circuito em bytes/s. Padrão: 64 kB/s (sobra para voz).
    pub circuit_bytes_per_sec: u64,
    /// Rajada tolerada, em ms de banda. Padrão: 500.
    pub burst_ms: u64,
    /// Circuito sem tráfego por este tempo é derrubado. Padrão: 60 s.
    pub idle_ms: u64,
    /// Células que falham na autenticação antes de derrubar o circuito. Padrão: 16.
    pub max_auth_failures: u32,
    /// Criações de circuito aceitas por segundo, no relay todo. Cada `Create` custa um
    /// Diffie-Hellman; sem este teto, quem abre e derruba circuitos em loop gasta a CPU do
    /// relay sem nunca encher `max_circuits`. Padrão: 20.
    pub creates_per_sec: u32,
    /// Rajada de criações tolerada. Padrão: 40.
    pub create_burst: u32,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            max_circuits: 256,
            relay_bytes_per_sec: 1_000_000,
            circuit_bytes_per_sec: 64_000,
            burst_ms: 500,
            idle_ms: 60_000,
            max_auth_failures: 16,
            creates_per_sec: 20,
            create_burst: 40,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub dropped_rate: u64,
    pub dropped_auth: u64,
    pub dropped_replay: u64,
    pub dropped_unknown: u64,
    pub refused_full: u64,
    /// `Create` recusados por exceder `creates_per_sec`.
    pub refused_create_rate: u64,
}

struct Bucket {
    per_ms: f64,
    cap: f64,
    tokens: f64,
    last: u64,
}

impl Bucket {
    fn new(bytes_per_sec: u64, burst_ms: u64, now: u64) -> Self {
        let cap = (bytes_per_sec as f64 * burst_ms as f64 / 1000.0).max(4.0 * CELL_LEN as f64);
        Self { per_ms: bytes_per_sec as f64 / 1000.0, cap, tokens: cap, last: now }
    }

    /// Verifica sem gastar; `spend` desconta.
    fn refill(&mut self, now: u64) {
        let dt = now.saturating_sub(self.last) as f64;
        self.tokens = (self.tokens + dt * self.per_ms).min(self.cap);
        self.last = now.max(self.last);
    }

    fn has(&self, n: usize) -> bool {
        self.tokens >= n as f64
    }

    fn spend(&mut self, n: usize) {
        self.tokens -= n as f64;
    }
}

struct Circ {
    pos: usize,
    recv: LayerReceiver,
    send: LayerSender,
    next: Option<CircRef>,
    extending: bool,
    stream: Option<String>,
    last_active: u64,
    auth_failures: u32,
    bucket: Bucket,
    /// O cliente manda um `Drop` por intervalo: cada célula dele (Data ou Drop) libera
    /// exatamente uma célula para o cliente, tirada de `down` ou de preenchimento.
    cover: bool,
    down: VecDeque<Vec<u8>>,
}

/// Quadros do destino esperando a vez no cover; os mais antigos caem primeiro.
const MAX_DOWN_QUEUE: usize = 64;

struct PendingExtend {
    prev: CircRef,
    next_circ: u32,
    create_body: [u8; 33],
}

pub struct RelayNode {
    secret: StaticSecret,
    public: [u8; 32],
    cfg: RelayConfig,
    circs: HashMap<CircRef, Circ>,
    /// Lado do próximo salto → circuito anterior.
    rev: HashMap<CircRef, CircRef>,
    connecting: HashMap<LinkId, PendingExtend>,
    next_link: LinkId,
    next_circ: u32,
    global: Bucket,
    creates: Bucket,
    pub stats: Stats,
}

impl RelayNode {
    pub fn new(secret: StaticSecret, cfg: RelayConfig, now_ms: u64) -> Self {
        let public = PublicKey::from(&secret).to_bytes();
        Self {
            secret,
            public,
            cfg,
            circs: HashMap::new(),
            rev: HashMap::new(),
            connecting: HashMap::new(),
            next_link: FIRST_OUTBOUND_LINK,
            next_circ: 1,
            global: Bucket::new(cfg.relay_bytes_per_sec, cfg.burst_ms, now_ms),
            creates: Bucket {
                per_ms: cfg.creates_per_sec as f64 / 1000.0,
                cap: cfg.create_burst as f64,
                tokens: cfg.create_burst as f64,
                last: now_ms,
            },
            stats: Stats::default(),
        }
    }

    /// Chave pública estática, publicada no descritor do relay.
    pub fn public_key(&self) -> [u8; 32] {
        self.public
    }

    pub fn circuit_count(&self) -> usize {
        self.circs.len()
    }

    pub fn handle_cell(&mut self, link: LinkId, cell: Cell, now: u64) -> Vec<Action> {
        let key = CircRef { link, circ: cell.circ };
        match cell.cmd {
            Cmd::Create => self.on_create(key, &cell, now),
            Cmd::Created => self.on_created(key, &cell, now),
            Cmd::Destroy => self.on_destroy(key),
            Cmd::Padding => vec![],
            Cmd::Relay => {
                if self.circs.contains_key(&key) {
                    self.forward(key, &cell.body, now)
                } else if let Some(prev) = self.rev.get(&key).copied() {
                    self.backward(prev, &cell.body, now)
                } else {
                    self.stats.dropped_unknown += 1;
                    vec![]
                }
            }
        }
    }

    /// O `Connect` de `link` concluiu: envia o `Create` pendente.
    pub fn link_established(&mut self, link: LinkId) -> Vec<Action> {
        match self.connecting.remove(&link) {
            Some(p) => vec![Action::Send {
                link,
                cell: Cell::with_body(p.next_circ, Cmd::Create, &p.create_body),
            }],
            None => vec![],
        }
    }

    /// O `Connect` de `link` falhou: avisa o cliente derrubando o circuito.
    pub fn link_failed(&mut self, link: LinkId) -> Vec<Action> {
        let Some(p) = self.connecting.remove(&link) else { return vec![] };
        self.rev.remove(&CircRef { link, circ: p.next_circ });
        self.remove_circ(p.prev)
    }

    /// Dados vindos do destino para o cliente do circuito `circ`.
    pub fn stream_data(&mut self, circ: CircRef, payload: &[u8]) -> Result<Vec<Action>, CircuitError> {
        let c = self.circs.get_mut(&circ).ok_or(CircuitError::UnknownCircuit)?;
        if payload.len() > max_data(c.pos) {
            return Err(CircuitError::Oversize);
        }
        if c.cover {
            if c.down.len() >= MAX_DOWN_QUEUE {
                c.down.pop_front();
            }
            c.down.push_back(payload.to_vec());
            return Ok(vec![]);
        }
        let body = originate(c, RelayCell::new(RelayCmd::Data, payload.to_vec()))?;
        Ok(vec![Action::Send { link: circ.link, cell: Cell { circ: circ.circ, cmd: Cmd::Relay, body } }])
    }

    /// Derruba circuitos ociosos.
    pub fn tick(&mut self, now: u64) -> Vec<Action> {
        let idle: Vec<CircRef> = self
            .circs
            .iter()
            .filter(|(_, c)| now.saturating_sub(c.last_active) >= self.cfg.idle_ms)
            .map(|(k, _)| *k)
            .collect();
        idle.into_iter().flat_map(|k| self.remove_circ(k)).collect()
    }

    fn on_create(&mut self, key: CircRef, cell: &Cell, now: u64) -> Vec<Action> {
        let destroy = |link, circ| vec![Action::Send { link, cell: Cell::with_body(circ, Cmd::Destroy, &[]) }];
        if self.circs.contains_key(&key) {
            return destroy(key.link, key.circ);
        }
        if self.circs.len() >= self.cfg.max_circuits {
            self.stats.refused_full += 1;
            return destroy(key.link, key.circ);
        }
        self.creates.refill(now);
        if !self.creates.has(1) {
            self.stats.refused_create_rate += 1;
            return destroy(key.link, key.circ);
        }
        self.creates.spend(1);
        let pos = cell.body[32] as usize;
        let x: [u8; 32] = cell.body[..32].try_into().unwrap();
        let Some(reply) = (pos < MAX_HOPS).then(|| server_handshake(&self.secret, &x).ok()).flatten() else {
            return destroy(key.link, key.circ);
        };
        let (recv, send) = reply.keys.relay_layers();
        self.circs.insert(
            key,
            Circ {
                pos,
                recv,
                send,
                next: None,
                extending: false,
                stream: None,
                last_active: now,
                auth_failures: 0,
                bucket: Bucket::new(self.cfg.circuit_bytes_per_sec, self.cfg.burst_ms, now),
                cover: false,
                down: VecDeque::new(),
            },
        );
        let mut body = [0u8; 64];
        body[..32].copy_from_slice(&reply.y);
        body[32..].copy_from_slice(&reply.tag);
        vec![Action::Send { link: key.link, cell: Cell::with_body(key.circ, Cmd::Created, &body) }]
    }

    /// `Created` do próximo relay: devolve ao cliente como `Extended`.
    fn on_created(&mut self, key: CircRef, cell: &Cell, now: u64) -> Vec<Action> {
        let Some(prev) = self.rev.get(&key).copied() else {
            self.stats.dropped_unknown += 1;
            return vec![];
        };
        let Some(c) = self.circs.get_mut(&prev).filter(|c| c.extending) else {
            self.stats.dropped_unknown += 1;
            return vec![];
        };
        c.extending = false;
        c.next = Some(key);
        c.last_active = now;
        match originate(c, RelayCell::new(RelayCmd::Extended, cell.body[..64].to_vec())) {
            Ok(body) => vec![Action::Send { link: prev.link, cell: Cell { circ: prev.circ, cmd: Cmd::Relay, body } }],
            Err(_) => self.remove_circ(prev),
        }
    }

    fn on_destroy(&mut self, key: CircRef) -> Vec<Action> {
        if self.circs.contains_key(&key) {
            return self.remove_circ_from(key, true);
        }
        if let Some(prev) = self.rev.get(&key).copied() {
            return self.remove_circ_from(prev, false);
        }
        vec![]
    }

    fn remove_circ(&mut self, prev: CircRef) -> Vec<Action> {
        self.remove_circ_from(prev, false)
    }

    /// Remove o circuito e avisa o(s) vizinho(s). `from_prev`: o aviso veio do cliente.
    fn remove_circ_from(&mut self, prev: CircRef, from_prev: bool) -> Vec<Action> {
        let Some(c) = self.circs.remove(&prev) else { return vec![] };
        let mut out = Vec::new();
        if let Some(next) = c.next {
            self.rev.remove(&next);
            out.push(Action::Send { link: next.link, cell: Cell::with_body(next.circ, Cmd::Destroy, &[]) });
        }
        if c.stream.is_some() {
            out.push(Action::CloseStream { circ: prev });
        }
        if !from_prev {
            out.push(Action::Send { link: prev.link, cell: Cell::with_body(prev.circ, Cmd::Destroy, &[]) });
        }
        out
    }

    /// Consome orçamento de banda; `false` descarta a célula.
    fn admit(&mut self, key: CircRef, now: u64) -> bool {
        self.global.refill(now);
        let Some(c) = self.circs.get_mut(&key) else { return false };
        c.bucket.refill(now);
        if !(self.global.has(CELL_LEN) && c.bucket.has(CELL_LEN)) {
            self.stats.dropped_rate += 1;
            return false;
        }
        self.global.spend(CELL_LEN);
        c.bucket.spend(CELL_LEN);
        c.last_active = now;
        true
    }

    fn forward(&mut self, key: CircRef, body: &[u8; CELL_LEN], now: u64) -> Vec<Action> {
        if !self.admit(key, now) {
            return vec![];
        }
        let max_failures = self.cfg.max_auth_failures;
        let c = self.circs.get_mut(&key).expect("admit confirmou");
        let pt = match c.recv.open(&body[..link_len(c.pos)]) {
            Ok(pt) => pt,
            Err(CircuitError::Replay) => {
                self.stats.dropped_replay += 1;
                return vec![];
            }
            Err(_) => {
                self.stats.dropped_auth += 1;
                c.auth_failures += 1;
                return if c.auth_failures >= max_failures { self.remove_circ(key) } else { vec![] };
            }
        };
        if pt[0] == 0 {
            // Camada de passagem: o resto segue para o próximo salto.
            let Some(next) = c.next else { return self.remove_circ(key) };
            return vec![Action::Send {
                link: next.link,
                cell: Cell { circ: next.circ, cmd: Cmd::Relay, body: pad_to_cell(&pt[1..]) },
            }];
        }
        match RelayCell::decode(&pt[1..]) {
            Ok(rc) => self.on_relay_cell(key, rc),
            Err(_) => self.remove_circ(key),
        }
    }

    fn on_relay_cell(&mut self, key: CircRef, rc: RelayCell) -> Vec<Action> {
        let c = self.circs.get_mut(&key).expect("existe");
        match rc.cmd {
            RelayCmd::Extend => {
                if c.pos + 1 >= MAX_HOPS || c.next.is_some() || c.extending || rc.data.len() <= 64 {
                    return self.remove_circ(key);
                }
                let Ok(addr) = String::from_utf8(rc.data[64..].to_vec()) else { return self.remove_circ(key) };
                let mut link_key = [0u8; 32];
                link_key.copy_from_slice(&rc.data[32..64]);
                let link = self.next_link;
                self.next_link += 1;
                let next_circ = self.next_circ;
                self.next_circ = self.next_circ.wrapping_add(1).max(1);
                let mut create_body = [0u8; 33];
                create_body[..32].copy_from_slice(&rc.data[..32]);
                create_body[32] = (c.pos + 1) as u8;
                c.extending = true;
                self.rev.insert(CircRef { link, circ: next_circ }, key);
                self.connecting.insert(link, PendingExtend { prev: key, next_circ, create_body });
                vec![Action::Connect { link, addr, key: link_key }]
            }
            RelayCmd::Begin => {
                if c.pos + 1 != MAX_HOPS || c.stream.is_some() {
                    return self.remove_circ(key);
                }
                let Ok(dest) = String::from_utf8(rc.data) else { return self.remove_circ(key) };
                c.stream = Some(dest.clone());
                vec![Action::OpenStream { circ: key, dest }]
            }
            RelayCmd::Data => match c.stream {
                Some(_) => {
                    let mut acts = vec![Action::Deliver { circ: key, payload: rc.data }];
                    acts.extend(self.cover_reply(key));
                    acts
                }
                None => vec![],
            },
            RelayCmd::Drop => {
                if c.pos + 1 != MAX_HOPS {
                    return vec![];
                }
                c.cover = true;
                self.cover_reply(key)
            }
            RelayCmd::End => {
                if c.stream.take().is_some() {
                    vec![Action::CloseStream { circ: key }]
                } else {
                    vec![]
                }
            }
            RelayCmd::Ping => match originate(c, RelayCell::new(RelayCmd::Pong, rc.data)) {
                Ok(body) => vec![Action::Send { link: key.link, cell: Cell { circ: key.circ, cmd: Cmd::Relay, body } }],
                Err(_) => self.remove_circ(key),
            },
            RelayCmd::Extended | RelayCmd::Pong => vec![],
        }
    }

    /// Responde a uma célula do cliente no modo cover: um quadro da fila ou um `Drop`.
    fn cover_reply(&mut self, key: CircRef) -> Vec<Action> {
        let Some(c) = self.circs.get_mut(&key).filter(|c| c.cover) else { return vec![] };
        let rc = match c.down.pop_front() {
            Some(d) => RelayCell::new(RelayCmd::Data, d),
            None => RelayCell::new(RelayCmd::Drop, Vec::new()),
        };
        match originate(c, rc) {
            Ok(body) => vec![Action::Send { link: key.link, cell: Cell { circ: key.circ, cmd: Cmd::Relay, body } }],
            Err(_) => self.remove_circ(key),
        }
    }

    /// Célula vinda do próximo salto: acrescenta a nossa camada e devolve ao cliente.
    fn backward(&mut self, prev: CircRef, body: &[u8; CELL_LEN], now: u64) -> Vec<Action> {
        if !self.admit(prev, now) {
            return vec![];
        }
        let c = self.circs.get_mut(&prev).expect("admit confirmou");
        let inner = &body[..link_len(c.pos + 1)];
        let mut pt = Vec::with_capacity(inner.len() + 1);
        pt.push(0);
        pt.extend_from_slice(inner);
        match c.send.seal(&pt) {
            Ok(sealed) => vec![Action::Send {
                link: prev.link,
                cell: Cell { circ: prev.circ, cmd: Cmd::Relay, body: pad_to_cell(&sealed) },
            }],
            Err(_) => self.remove_circ(prev),
        }
    }
}

/// Cifra uma célula originada neste salto, rumo ao cliente.
fn originate(c: &mut Circ, rc: RelayCell) -> Result<[u8; CELL_LEN], CircuitError> {
    let mut pt = vec![1u8];
    pt.extend_from_slice(&rc.encode(relay_cell_len(c.pos))?);
    Ok(pad_to_cell(&c.send.seal(&pt)?))
}
