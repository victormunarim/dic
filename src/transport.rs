//! Transporte TCP dos relays e do cliente (feature `net`).
//!
//! Liga os motores *sans-IO* ([`RelayNode`], [`CircuitManager`]) a sockets TCP:
//!
//! - [`RelayServer`]: escuta conexões, repassa células entre links e abre as
//!   conexões que o relay pede (`Action::Connect`). O relay de saída (segundo
//!   salto) abre o fluxo até o destino **só** se a [`ExitPolicy`] permitir.
//! - [`CircuitClient`]: conecta aos primeiros relays sob demanda, dirige o
//!   gerente de circuitos (relógio, ping, failover) e expõe envio/eventos.
//!
//! # Formato no fio
//!
//! - **Links entre nós**: Noise `NK` (veja [`crate::link`]) com uma célula
//!   ([`Cell::WIRE_LEN`] = 261 bytes) por mensagem cifrada. O iniciador
//!   autentica o relay pela chave do descritor; quem observa a rede não vê ids
//!   de circuito nem comandos. Cada célula ainda é cifrada em camadas.
//! - **Fluxo do relay de saída ao destino**: quadros `[u16 BE tamanho][dados]`,
//!   para preservar as fronteiras dos pacotes de mídia sobre o TCP.
//!
//! # Limites
//!
//! - O link oculta conteúdo, ids e comandos, mas não o tamanho nem a
//!   temporização dos quadros por si só (todos têm o mesmo tamanho). O cover
//!   traffic (um quadro por intervalo, real ou `Padding`) cobre a temporização;
//!   veja [`DEFAULT_COVER`].
//! - O trecho entre o relay de saída e o destino é TCP puro: o conteúdo de
//!   mídia já vai cifrado ponta a ponta pela camada acima, não por este módulo.
//! - Fechar um link não derruba na hora os circuitos que passavam por ele; o
//!   relay os expira por ociosidade e o cliente os detecta por silêncio.
//! - Células inválidas fecham o link (violação de protocolo).

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, tcp::OwnedReadHalf},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, MissedTickBehavior, interval, timeout},
};

use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::rand_core::OsRng;

use crate::link::{self, LinkReader, LinkWriter};
use crate::cover::CoverProfile;
use crate::reputation::{RelayStats, Reputation};
use crate::circuit::{
    Cell, Cmd,
    manager::{Candidate, CircuitManager, Event, Out, SendError, Status},
    relay::{Action, CircRef, LinkId, RelayNode},
};

/// Intervalo entre reaplicações da reputação à lista de candidatos.
pub const REPUTATION_REFRESH: Duration = Duration::from_secs(30);

/// Decide para onde o relay de saída pode abrir fluxos.
/// Devolve o endereço TCP real para o `dest` pedido, ou `None` para recusar.
/// A política padrão ([`deny_all`]) recusa tudo: sem ela o relay
/// seria um proxy aberto.
pub type ExitPolicy = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub fn deny_all() -> ExitPolicy {
    Arc::new(|_| None)
}

/// Permite somente os destinos da lista: `(nome pedido pelo cliente, endereço TCP)`.
pub fn allow_list(entries: Vec<(String, String)>) -> ExitPolicy {
    Arc::new(move |dest| entries.iter().find(|(n, _)| n == dest).map(|(_, a)| a.clone()))
}

#[derive(Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    /// Conexões de entrada simultâneas. Padrão: 512.
    pub max_links: usize,
    /// Prazo para conectar ao próximo salto ou ao destino. Padrão: 3 s.
    pub connect_timeout: Duration,
    pub exit: ExitPolicy,
    /// Chave de link (Noise NK) do relay, a mesma que vai no descritor como `link_key`.
    /// `None` gera uma chave nova a cada início; para manter a identidade de link entre
    /// reinícios, guarde o segredo e passe-o aqui. É distinta da chave do circuito.
    pub link_secret: Option<StaticSecret>,
    /// Cover traffic: intervalo entre quadros nos links com padding. `None` desliga.
    /// O relay liga o padding em links que ele mesmo abre e responde com padding nos links
    /// de entrada cujo par o usa (veja [`DEFAULT_COVER`]). Padrão: `Some(DEFAULT_COVER)`.
    pub cover: Option<Duration>,
    /// Links com padding simultâneos (cada um custa banda constante nos dois sentidos).
    /// Sem teto, um par poderia forçar o relay a gastar banda em todos os links. Padrão: 64.
    pub max_cover_links: usize,
    /// Perfil de custo do cover; quando presente, substitui `cover` (usa o intervalo de link
    /// do perfil). Troca viva com [`RelayServer::set_profile`]. Padrão: `None`.
    pub profile: Option<CoverProfile>,
}

impl ServerConfig {
    pub fn new(listen: SocketAddr) -> Self {
        Self { listen, max_links: 512, connect_timeout: Duration::from_secs(3), exit: deny_all(), link_secret: None, cover: Some(DEFAULT_COVER), max_cover_links: 64, profile: None }
    }
}

/// Intervalo padrão do cover traffic: 100 quadros/s (279 bytes) por sentido em cada link.
pub const DEFAULT_COVER: Duration = Duration::from_millis(10);

/// Intervalo padrão do cover de ponta a ponta: 50 células/s por sentido (um quadro Opus de 20 ms).
pub const DEFAULT_CIRCUIT_COVER: Duration = Duration::from_millis(20);

const CHANNEL: usize = 1024;
const WRITE_QUEUE: usize = 256;

// ---------------------------------------------------------------------------
// Leitura/escrita de links
// ---------------------------------------------------------------------------

/// Lê células do link até erro (EOF, autenticação falha ou célula inválida).
/// Células `Padding` não chegam a `on_cell`: só avisam que o par usa cover traffic.
async fn read_cells<F, Fut>(mut rd: LinkReader, peer_cover: Arc<AtomicBool>, mut on_cell: F)
where
    F: FnMut(Cell) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    while let Ok(cell) = rd.read_cell().await {
        if cell.cmd == Cmd::Padding {
            peer_cover.store(true, Ordering::Relaxed);
            continue;
        }
        if !on_cell(cell).await {
            return;
        }
    }
}

/// Política de cover traffic de um link.
#[derive(Clone)]
struct Cover {
    /// Intervalo entre quadros; `None` desliga. Mudar o valor vale para o link já aberto.
    interval: watch::Receiver<Option<Duration>>,
    /// Liga o padding por conta própria (links que nós abrimos).
    initiate: bool,
    /// O par já mandou padding: responde na mesma moeda.
    peer: Arc<AtomicBool>,
    /// Teto de links com padding ativo (só nos relays).
    slots: Option<Arc<Semaphore>>,
}

impl Cover {
    fn off() -> Self {
        Self::fixed(None)
    }

    /// Intervalo que nunca muda (relays).
    fn fixed(interval: Option<Duration>) -> Self {
        Self { interval: watch::channel(interval).1, initiate: false, peer: Arc::new(AtomicBool::new(false)), slots: None }
    }
}

/// Espera uma mudança de intervalo; sem emissor, nunca acorda.
async fn interval_changed(rx: &mut watch::Receiver<Option<Duration>>) -> Option<Duration> {
    if rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
    *rx.borrow_and_update()
}

fn new_ticker(every: Duration) -> tokio::time::Interval {
    let mut t = interval(every);
    t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    t
}

/// Escreve as células da fila. Com cover ativo, emite exatamente um quadro por
/// `interval`: a célula seguinte da fila ou, se não houver, um `Padding`. Assim a taxa e a
/// temporização no fio não dependem de haver voz, silêncio ou controle.
async fn write_loop(mut wr: LinkWriter, mut rx: mpsc::Receiver<Cell>, mut cover: Cover) {
    let mut every = *cover.interval.borrow_and_update();
    let mut ticker = new_ticker(every.unwrap_or(Duration::from_secs(1)));
    // A vaga do teto fica retida enquanto o escritor existir.
    let mut _slot: Option<OwnedSemaphorePermit> = None;
    let mut active = false;
    loop {
        if every.is_none() {
            _slot = None;
            active = false;
            // Sem cover: repassa como chegar.
            tokio::select! {
                c = rx.recv() => match c {
                    Some(cell) => if wr.write_cell(&cell).await.is_err() { return; },
                    None => return,
                },
                new = interval_changed(&mut cover.interval) => {
                    every = new;
                    if let Some(d) = new { ticker = new_ticker(d); }
                }
            }
            continue;
        }
        if !active && (cover.initiate || cover.peer.load(Ordering::Relaxed)) {
            match &cover.slots {
                None => active = true,
                Some(s) => {
                    _slot = s.clone().try_acquire_owned().ok();
                    active = _slot.is_some();
                }
            }
        }
        if !active {
            // Sem cover ainda: repassa como chegar e reavalia a cada intervalo.
            tokio::select! {
                c = rx.recv() => match c {
                    Some(cell) => if wr.write_cell(&cell).await.is_err() { return; },
                    None => return,
                },
                _ = ticker.tick() => {}
                new = interval_changed(&mut cover.interval) => {
                    every = new;
                    if let Some(d) = new { ticker = new_ticker(d); }
                }
            }
            continue;
        }
        tokio::select! {
            _ = ticker.tick() => {}
            new = interval_changed(&mut cover.interval) => {
                every = new;
                if let Some(d) = new { ticker = new_ticker(d); }
                continue;
            }
        }
        let cell = match rx.try_recv() {
            Ok(c) => c,
            Err(mpsc::error::TryRecvError::Empty) => Cell::with_body(0, Cmd::Padding, &[]),
            Err(mpsc::error::TryRecvError::Disconnected) => return,
        };
        if wr.write_cell(&cell).await.is_err() {
            return;
        }
    }
}

/// Disca `addr` e cifra o link, autenticando o relay por `key`.
async fn dial(addr: String, key: [u8; 32], limit: Duration) -> Option<(LinkReader, LinkWriter)> {
    let s = timeout(limit, TcpStream::connect(addr)).await.ok()?.ok()?;
    // Sem isto o Nagle seguraria quadros pequenos de voz esperando o ACK do anterior.
    let _ = s.set_nodelay(true);
    link::connect(s, &key).await.ok()
}

// ---------------------------------------------------------------------------
// Relay
// ---------------------------------------------------------------------------

enum RelayEvent {
    Linked(LinkReader, LinkWriter, OwnedSemaphorePermit),
    Cell(LinkId, Cell),
    LinkClosed(LinkId),
    Connected(LinkId, Option<(LinkReader, LinkWriter)>),
    StreamOpened(CircRef, Option<TcpStream>),
    StreamData(CircRef, Vec<u8>),
    StreamClosed(CircRef),
}

/// Relay em execução. Larga-se com [`RelayServer::stop`] (ou ao descartar o valor).
pub struct RelayServer {
    addr: SocketAddr,
    link_public: [u8; 32],
    cover: watch::Sender<Option<Duration>>,
    task: JoinHandle<()>,
}

impl RelayServer {
    /// Abre o socket e inicia o relay. Devolve o endereço efetivo (útil com porta 0).
    pub async fn spawn(node: RelayNode, mut cfg: ServerConfig) -> io::Result<Self> {
        crate::timer::fine_timers();
        let listener = TcpListener::bind(cfg.listen).await?;
        let addr = listener.local_addr()?;
        let link = cfg.link_secret.take().unwrap_or_else(|| StaticSecret::random_from_rng(OsRng));
        let link_public = PublicKey::from(&link).to_bytes();
        let secret = Zeroizing::new(link.to_bytes());
        let (cover, cover_rx) = watch::channel(cfg.profile.map_or(cfg.cover, |p| p.link_interval()));
        let task = tokio::spawn(relay_actor(node, secret, listener, cfg, cover_rx));
        Ok(Self { addr, link_public, cover, task })
    }

    /// Troca o perfil de cover do relay, inclusive nos links já abertos (por exemplo, um
    /// relay em banda cara passa a `Economy`). Vale para os dois sentidos de cada link que
    /// o relay escreve; o que o par manda depende do perfil do par.
    pub fn set_profile(&self, profile: CoverProfile) {
        let _ = self.cover.send(profile.link_interval());
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Chave pública do link, para o campo `link_key` do descritor.
    pub fn link_public_key(&self) -> [u8; 32] {
        self.link_public
    }

    /// Para o relay e fecha todas as conexões.
    pub fn stop(self) {
        self.task.abort();
    }
}

impl Drop for RelayServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct RelayIo {
    cover: watch::Receiver<Option<Duration>>,
    cover_slots: Arc<Semaphore>,
    cfg: ServerConfig,
    tx: mpsc::Sender<RelayEvent>,
    tasks: JoinSet<()>,
    writers: HashMap<LinkId, mpsc::Sender<Cell>>,
    streams: HashMap<CircRef, mpsc::Sender<Vec<u8>>>,
    next_in: LinkId,
    started: Instant,
}

impl RelayIo {
    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Registra um link cifrado e lança seus leitores/escritores.
    /// `permit` (links de entrada) fica retida até o link acabar.
    fn attach(&mut self, link: LinkId, rd: LinkReader, wr: LinkWriter, permit: Option<OwnedSemaphorePermit>) {
        let (wtx, wrx) = mpsc::channel(WRITE_QUEUE);
        self.writers.insert(link, wtx);
        let cover = Cover {
            initiate: permit.is_none(),
            slots: Some(self.cover_slots.clone()),
            interval: self.cover.clone(),
            ..Cover::off()
        };
        let peer_cover = cover.peer.clone();
        self.tasks.spawn(write_loop(wr, wrx, cover));
        let tx = self.tx.clone();
        self.tasks.spawn(async move {
            let _permit = permit;
            let cell_tx = tx.clone();
            read_cells(rd, peer_cover, move |c| {
                let tx = cell_tx.clone();
                async move { tx.send(RelayEvent::Cell(link, c)).await.is_ok() }
            })
            .await;
            let _ = tx.send(RelayEvent::LinkClosed(link)).await;
        });
    }

    fn run(&mut self, actions: Vec<Action>) {
        let mut pending = actions;
        while let Some(a) = pending.pop() {
            match a {
                Action::Send { link, cell } => {
                    // Fila cheia = peer lento: descarta (voz tolera perda).
                    if let Some(w) = self.writers.get(&link) {
                        let _ = w.try_send(cell);
                    }
                }
                Action::Connect { link, addr, key } => {
                    let (tx, to) = (self.tx.clone(), self.cfg.connect_timeout);
                    self.tasks.spawn(async move {
                        let l = dial(addr, key, to).await;
                        let _ = tx.send(RelayEvent::Connected(link, l)).await;
                    });
                }
                Action::OpenStream { circ, dest } => {
                    let (tx, to) = (self.tx.clone(), self.cfg.connect_timeout);
                    let target = (self.cfg.exit)(&dest);
                    self.tasks.spawn(async move {
                        let s = match target {
                            Some(t) => timeout(to, TcpStream::connect(t)).await.ok().and_then(Result::ok),
                            None => None,
                        };
                        let _ = tx.send(RelayEvent::StreamOpened(circ, s)).await;
                    });
                }
                Action::Deliver { circ, payload } => {
                    if let Some(s) = self.streams.get(&circ) {
                        let _ = s.try_send(frame(&payload));
                    }
                }
                Action::CloseStream { circ } => {
                    self.streams.remove(&circ);
                }
            }
        }
    }
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + payload.len());
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

async fn read_frames(mut rd: OwnedReadHalf, circ: CircRef, tx: mpsc::Sender<RelayEvent>) {
    let mut len = [0u8; 2];
    while rd.read_exact(&mut len).await.is_ok() {
        let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
        if rd.read_exact(&mut buf).await.is_err() {
            break;
        }
        if tx.send(RelayEvent::StreamData(circ, buf)).await.is_err() {
            return;
        }
    }
    let _ = tx.send(RelayEvent::StreamClosed(circ)).await;
}

async fn relay_actor(mut node: RelayNode, secret: Zeroizing<[u8; 32]>, listener: TcpListener, cfg: ServerConfig, cover: watch::Receiver<Option<Duration>>) {
    let slots = Arc::new(Semaphore::new(cfg.max_links));
    let (tx, mut rx) = mpsc::channel(CHANNEL);
    let mut io = RelayIo {
        cover,
        cover_slots: Arc::new(Semaphore::new(cfg.max_cover_links)),
        cfg,
        tx: tx.clone(),
        tasks: JoinSet::new(),
        writers: HashMap::new(),
        streams: HashMap::new(),
        next_in: 1,
        started: Instant::now(),
    };
    let mut tick = interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                // Erros de accept (ex.: sem descritores) não derrubam o relay.
                // Sem vaga livre (`max_links`), a conexão é fechada na hora.
                if let Ok((s, _)) = accepted {
                    let _ = s.set_nodelay(true);
                    if let Ok(permit) = slots.clone().try_acquire_owned() {
                        let (tx, secret) = (tx.clone(), secret.clone());
                        io.tasks.spawn(async move {
                            if let Ok((rd, wr)) = link::accept(s, &secret).await {
                                let _ = tx.send(RelayEvent::Linked(rd, wr, permit)).await;
                            }
                        });
                    }
                }
            }
            _ = tick.tick() => {
                let now = io.now();
                let acts = node.tick(now);
                io.run(acts);
            }
            Some(ev) = rx.recv() => match ev {
                RelayEvent::Linked(rd, wr, permit) => {
                    let link = io.next_in;
                    io.next_in += 1; // < FIRST_OUTBOUND_LINK na prática
                    io.attach(link, rd, wr, Some(permit));
                }
                RelayEvent::Cell(link, cell) => {
                    let now = io.now();
                    let acts = node.handle_cell(link, cell, now);
                    io.run(acts);
                }
                RelayEvent::LinkClosed(link) => {
                    io.writers.remove(&link);
                }
                RelayEvent::Connected(link, Some((rd, wr))) => {
                    io.attach(link, rd, wr, None);
                    let acts = node.link_established(link);
                    io.run(acts);
                }
                RelayEvent::Connected(link, None) => {
                    let acts = node.link_failed(link);
                    io.run(acts);
                }
                RelayEvent::StreamOpened(circ, Some(s)) => {
                    let _ = s.set_nodelay(true);
                    let (rd, mut wr) = s.into_split();
                    let (stx, mut srx) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE);
                    io.streams.insert(circ, stx);
                    io.tasks.spawn(async move {
                        while let Some(b) = srx.recv().await {
                            if wr.write_all(&b).await.is_err() {
                                return;
                            }
                        }
                    });
                    io.tasks.spawn(read_frames(rd, circ, tx.clone()));
                }
                RelayEvent::StreamOpened(_, None) => {} // recusado ou inalcançável: o cliente vê silêncio
                RelayEvent::StreamData(circ, data) => {
                    if let Ok(acts) = node.stream_data(circ, &data) {
                        io.run(acts);
                    }
                }
                RelayEvent::StreamClosed(circ) => {
                    io.streams.remove(&circ);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cliente
// ---------------------------------------------------------------------------

enum ClientCmd {
    Send(Vec<u8>, oneshot::Sender<Result<(), SendError>>),
    Status(oneshot::Sender<Status>),
    SetRelays(Vec<Candidate>),
    Snapshot(oneshot::Sender<Option<Vec<u8>>>),
    Stats(String, oneshot::Sender<Option<RelayStats>>),
    SetProfile(CoverProfile),
}

enum ClientEvent {
    Connected(String, Option<(LinkReader, LinkWriter)>),
    Cell(Cell),
    LinkClosed(String),
}

/// Opções de [`CircuitClient::spawn_with`].
pub struct ClientOptions {
    /// Reputação medida dos relays (veja [`crate::reputation`]).
    pub reputation: Option<Reputation>,
    /// Cover traffic nos links do cliente: um quadro a cada intervalo, real ou `Padding`.
    /// `None` desliga. Padrão: `Some(DEFAULT_COVER)`, que custa banda constante (veja o README).
    pub cover: Option<Duration>,
    /// Cover de ponta a ponta no circuito ativo: uma célula por intervalo, de mídia, ping ou
    /// `Drop`, e o relay de saída responde na mesma cadência. `None` desliga. Padrão:
    /// `Some(DEFAULT_CIRCUIT_COVER)`. Os relays não precisam de configuração.
    pub circuit_cover: Option<Duration>,
    /// Perfil de custo; quando presente, substitui `cover` e `circuit_cover`.
    pub profile: Option<CoverProfile>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self { reputation: None, cover: Some(DEFAULT_COVER), circuit_cover: Some(DEFAULT_CIRCUIT_COVER), profile: None }
    }
}

/// Cliente de circuitos: dirige o [`CircuitManager`] sobre TCP.
pub struct CircuitClient {
    cmd: mpsc::Sender<ClientCmd>,
    events: mpsc::Receiver<Event>,
    task: JoinHandle<()>,
}

impl CircuitClient {
    /// Inicia o gerente. Os eventos (`Ready`, `Data`, `FailedOver`...) chegam por [`Self::next_event`].
    pub fn spawn(manager: CircuitManager, connect_timeout: Duration) -> Self {
        Self::spawn_with(manager, connect_timeout, ClientOptions::default())
    }

    /// Como [`Self::spawn`], mas com reputação medida: o cliente alimenta
    /// `reputation` com `Rtt` e `CircuitFailed` e reaplica as pontuações aos
    /// candidatos de [`Self::set_relays`] a cada falha e a cada
    /// [`REPUTATION_REFRESH`]. Os candidatos só valem depois do primeiro
    /// [`Self::set_relays`]; antes disso o gerente usa a lista com que foi criado.
    pub fn spawn_with_reputation(manager: CircuitManager, connect_timeout: Duration, reputation: Reputation) -> Self {
        Self::spawn_with(manager, connect_timeout, ClientOptions { reputation: Some(reputation), ..Default::default() })
    }

    /// Inicia o gerente com opções (reputação, cover traffic).
    pub fn spawn_with(mut manager: CircuitManager, connect_timeout: Duration, options: ClientOptions) -> Self {
        crate::timer::fine_timers();
        let ClientOptions { reputation: rep, mut cover, mut circuit_cover, profile } = options;
        if let Some(p) = profile {
            (cover, circuit_cover) = (p.link_interval(), p.circuit_interval());
        }
        manager.set_cover(circuit_cover.map(|d| d.as_millis() as u64));
        let (cmd_tx, cmd_rx) = mpsc::channel(CHANNEL);
        let (ev_tx, ev_rx) = mpsc::channel(CHANNEL);
        let task = tokio::spawn(client_actor(manager, connect_timeout, rep, cover, cmd_rx, ev_tx));
        Self { cmd: cmd_tx, events: ev_rx, task }
    }

    /// Troca o perfil de cover em uso, inclusive nos links já abertos (por exemplo, ao passar
    /// de Wi-Fi para dados móveis). A troca de intervalo é visível a quem observa o link.
    pub async fn set_profile(&self, profile: CoverProfile) {
        let _ = self.cmd.send(ClientCmd::SetProfile(profile)).await;
    }

    /// Envia um quadro de mídia pelo circuito ativo.
    pub async fn send(&self, payload: &[u8]) -> Result<(), SendError> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(ClientCmd::Send(payload.to_vec(), tx)).await.map_err(|_| SendError::Unavailable)?;
        rx.await.unwrap_or(Err(SendError::Unavailable))
    }

    /// Troca a lista de relays candidatos (descoberta + reputação). Circuitos em uso não mudam.
    pub async fn set_relays(&self, relays: Vec<Candidate>) {
        let _ = self.cmd.send(ClientCmd::SetRelays(relays)).await;
    }

    /// Reputação atual de um relay; `None` sem reputação ativa ou sem histórico.
    pub async fn relay_stats(&self, addr: &str) -> Option<RelayStats> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(ClientCmd::Stats(addr.to_string(), tx)).await.ok()?;
        rx.await.ok().flatten()
    }

    /// Instantâneo da reputação para gravar com [`crate::reputation::save_file`]; `None` sem reputação ativa.
    pub async fn reputation_snapshot(&self) -> Option<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(ClientCmd::Snapshot(tx)).await.ok()?;
        rx.await.ok().flatten()
    }

    pub async fn status(&self) -> Option<Status> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(ClientCmd::Status(tx)).await.ok()?;
        rx.await.ok()
    }

    /// Próximo evento do gerente; `None` após [`Self::stop`].
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

impl Drop for CircuitClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn client_actor(
    mut mgr: CircuitManager,
    connect_timeout: Duration,
    mut rep: Option<Reputation>,
    cover_every: Option<Duration>,
    mut cmd: mpsc::Receiver<ClientCmd>,
    events: mpsc::Sender<Event>,
) {
    let (cover_tx, cover_rx) = watch::channel(cover_every);
    let started = Instant::now();
    let now = || started.elapsed().as_millis() as u64;
    let (tx, mut rx) = mpsc::channel::<ClientEvent>(CHANNEL);
    let mut tasks = JoinSet::new();
    let mut writers: HashMap<String, mpsc::Sender<Cell>> = HashMap::new();
    let mut dialing: HashMap<String, Vec<Cell>> = HashMap::new();
    // Lista como veio da descoberta, antes do ajuste pela reputação.
    let mut base: Vec<Candidate> = Vec::new();
    let mut last_refresh = 0u64;
    let mut tick = interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut outs = mgr.start(now());
    loop {
        for out in outs.drain(..) {
            match out {
                Out::Event(e) => {
                    if let Some(r) = rep.as_mut() {
                        r.record(&e, now());
                        if matches!(e, Event::CircuitFailed { .. }) && !base.is_empty() {
                            mgr.set_relays(r.adjust(base.clone(), now()));
                        }
                    }
                    let _ = events.try_send(e);
                }
                Out::Send { to, cell } => {
                    if let Some(w) = writers.get(&to) {
                        let _ = w.try_send(cell);
                    } else if let Some(q) = dialing.get_mut(&to) {
                        q.push(cell);
                    } else if let Some(key) = mgr.relay_key(&to) {
                        dialing.insert(to.clone(), vec![cell]);
                        let tx = tx.clone();
                        tasks.spawn(async move {
                            let l = dial(to.clone(), key, connect_timeout).await;
                            let _ = tx.send(ClientEvent::Connected(to, l)).await;
                        });
                    }
                }
            }
        }
        tokio::select! {
            _ = tick.tick() => {
                if let Some(r) = rep.as_mut() {
                    let t = now();
                    if !base.is_empty() && t.saturating_sub(last_refresh) >= REPUTATION_REFRESH.as_millis() as u64 {
                        last_refresh = t;
                        mgr.set_relays(r.adjust(base.clone(), t));
                    }
                }
                outs = mgr.tick(now());
            }
            c = cmd.recv() => match c {
                None => return,
                Some(ClientCmd::Send(p, reply)) => match mgr.send(&p) {
                    Ok(o) => { outs = o; let _ = reply.send(Ok(())); }
                    Err(e) => { let _ = reply.send(Err(e)); }
                },
                Some(ClientCmd::SetProfile(p)) => {
                    let _ = cover_tx.send(p.link_interval());
                    mgr.set_cover(p.circuit_interval().map(|d| d.as_millis() as u64));
                }
                Some(ClientCmd::Status(reply)) => { let _ = reply.send(mgr.status()); }
                Some(ClientCmd::Snapshot(reply)) => {
                    let _ = reply.send(rep.as_mut().map(|r| r.export(now())));
                }
                Some(ClientCmd::Stats(addr, reply)) => {
                    let _ = reply.send(rep.as_mut().and_then(|r| r.stats(&addr, now())));
                }
                Some(ClientCmd::SetRelays(list)) => match rep.as_mut() {
                    Some(r) => {
                        base = list;
                        last_refresh = now();
                        mgr.set_relays(r.adjust(base.clone(), last_refresh));
                    }
                    None => mgr.set_relays(list),
                },
            },
            Some(ev) = rx.recv() => match ev {
                ClientEvent::Connected(addr, Some((rd, wr))) => {
                    let (wtx, wrx) = mpsc::channel(WRITE_QUEUE);
                    for b in dialing.remove(&addr).unwrap_or_default() {
                        let _ = wtx.try_send(b);
                    }
                    writers.insert(addr.clone(), wtx);
                    let cover = Cover { interval: cover_rx.clone(), initiate: true, ..Cover::off() };
                    let peer_cover = cover.peer.clone();
                    tasks.spawn(write_loop(wr, wrx, cover));
                    let tx = tx.clone();
                    tasks.spawn(async move {
                        let cell_tx = tx.clone();
                        read_cells(rd, peer_cover, move |c| {
                            let tx = cell_tx.clone();
                            async move { tx.send(ClientEvent::Cell(c)).await.is_ok() }
                        }).await;
                        let _ = tx.send(ClientEvent::LinkClosed(addr)).await;
                    });
                }
                // Sem conexão: descarta o que esperava; o gerente estoura o prazo da montagem.
                ClientEvent::Connected(addr, None) => { dialing.remove(&addr); }
                ClientEvent::Cell(c) => outs = mgr.handle_cell(c, now()),
                ClientEvent::LinkClosed(addr) => { writers.remove(&addr); }
            },
        }
    }
}
