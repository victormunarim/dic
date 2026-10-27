//! Lado do cliente: conecta aos primeiros relays e dirige o gerente de circuitos.

use super::*;

/// Passo máximo do relógio do gerente de circuitos.
const MANAGER_TICK: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------
// Cliente
// ---------------------------------------------------------------------------

pub(super) enum ClientCmd {
    Send(Vec<u8>, oneshot::Sender<Result<(), SendError>>),
    Status(oneshot::Sender<Status>),
    SetRelays(Vec<Candidate>),
    Snapshot(oneshot::Sender<Option<Vec<u8>>>),
    Stats(String, oneshot::Sender<Option<RelayStats>>),
    SetProfile(CoverProfile),
}

pub(super) enum ClientEvent {
    Connected(String, Option<(LinkReader, LinkWriter)>),
    Cell(Cell),
    LinkClosed(String),
}

/// Opções de [`CircuitClient::spawn_with`].
pub struct ClientOptions {
    /// Reputação medida dos relays (veja [`crate::reputation`]).
    pub reputation: Option<Reputation>,
    /// Cover traffic nos links do cliente: um quadro a cada intervalo, real ou `Padding`.
    /// `None` desliga. Padrão: `Some(DEFAULT_COVER)`, que custa banda constante (veja `docs/CIRCUITO.md`).
    pub cover: Option<Duration>,
    /// Cover de ponta a ponta no circuito ativo: uma célula por intervalo, de mídia, ping ou
    /// `Drop`, e o relay de saída responde na mesma cadência. `None` desliga. Padrão:
    /// `Some(DEFAULT_CIRCUIT_COVER)`. Os relays não precisam de configuração.
    pub circuit_cover: Option<Duration>,
    /// Perfil de custo; quando presente, substitui `cover` e `circuit_cover`.
    pub profile: Option<CoverProfile>,
    /// Circuito direto, sem relay no meio (veja [`ManagerConfig::direct`](crate::circuit::manager::ManagerConfig::direct)).
    /// O gerente precisa ter `exit_addr`. Padrão: `false`.
    pub direct: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self { reputation: None, cover: Some(DEFAULT_COVER), circuit_cover: Some(DEFAULT_CIRCUIT_COVER), profile: None, direct: false }
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
        let ClientOptions { reputation: rep, mut cover, mut circuit_cover, profile, direct } = options;
        if let Some(p) = profile {
            (cover, circuit_cover) = (p.link_interval(), p.circuit_interval());
        }
        manager.set_cover(circuit_cover.map(|d| d.as_millis() as u64));
        manager.set_direct(direct);
        // O gerente emite no máximo uma célula de cover por tick: o tick precisa caber no intervalo.
        let tick = circuit_cover.map_or(MANAGER_TICK, |d| d.clamp(Duration::from_millis(1), MANAGER_TICK));
        let (cmd_tx, cmd_rx) = mpsc::channel(CHANNEL);
        let (ev_tx, ev_rx) = mpsc::channel(CHANNEL);
        let task = tokio::spawn(client_actor(manager, connect_timeout, rep, cover, tick, cmd_rx, ev_tx));
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

pub(super) async fn client_actor(
    mut mgr: CircuitManager,
    connect_timeout: Duration,
    mut rep: Option<Reputation>,
    cover_every: Option<Duration>,
    tick_every: Duration,
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
    let mut tick = interval(tick_every);
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
                            let l = dial(to.clone(), key, connect_timeout, false).await;
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
