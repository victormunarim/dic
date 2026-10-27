//! Lado do relay: escuta links, repassa células e abre os fluxos de saída.

use super::*;

// ---------------------------------------------------------------------------
// Relay
// ---------------------------------------------------------------------------

pub(super) enum RelayEvent {
    /// O último campo diz se o outro lado está nesta máquina (o cliente do próprio dono do relay).
    Linked(LinkReader, LinkWriter, OwnedSemaphorePermit, bool),
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

pub(super) struct RelayIo {
    cover: watch::Receiver<Option<Duration>>,
    cover_slots: Arc<Semaphore>,
    cfg: ServerConfig,
    tx: mpsc::Sender<RelayEvent>,
    tasks: JoinSet<()>,
    writers: HashMap<LinkId, mpsc::Sender<Cell>>,
    /// Links de entrada que vêm desta máquina. Só o `Extend` deles pode apontar para endereço privado.
    local: HashSet<LinkId>,
    streams: HashMap<CircRef, mpsc::Sender<Vec<u8>>>,
    next_in: LinkId,
    started: Instant,
}

impl RelayIo {
    pub(super) fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Registra um link cifrado e lança seus leitores/escritores.
    /// `permit` (links de entrada) fica retida até o link acabar.
    pub(super) fn attach(&mut self, link: LinkId, rd: LinkReader, wr: LinkWriter, permit: Option<OwnedSemaphorePermit>) {
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

    pub(super) fn run(&mut self, actions: Vec<Action>) {
        let mut pending = actions;
        while let Some(a) = pending.pop() {
            match a {
                Action::Send { link, cell } => {
                    // Fila cheia = peer lento: descarta (voz tolera perda).
                    if let Some(w) = self.writers.get(&link) {
                        let _ = w.try_send(cell);
                    }
                }
                Action::Connect { link, addr, key, via } => {
                    let (tx, to) = (self.tx.clone(), self.cfg.connect_timeout);
                    let public_only = !self.local.contains(&via);
                    self.tasks.spawn(async move {
                        let l = dial(addr, key, to, public_only).await;
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

pub(super) fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + payload.len());
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

pub(super) async fn read_frames(mut rd: OwnedReadHalf, circ: CircRef, tx: mpsc::Sender<RelayEvent>) {
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

pub(super) async fn relay_actor(mut node: RelayNode, secret: Zeroizing<[u8; 32]>, listener: TcpListener, cfg: ServerConfig, cover: watch::Receiver<Option<Duration>>) {
    let slots = Arc::new(Semaphore::new(cfg.max_links));
    let (tx, mut rx) = mpsc::channel(CHANNEL);
    let mut io = RelayIo {
        cover,
        cover_slots: Arc::new(Semaphore::new(cfg.max_cover_links)),
        cfg,
        tx: tx.clone(),
        tasks: JoinSet::new(),
        writers: HashMap::new(),
        local: HashSet::new(),
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
                if let Ok((s, peer)) = accepted {
                    let _ = s.set_nodelay(true);
                    let local = peer.ip().to_canonical().is_loopback();
                    if let Ok(permit) = slots.clone().try_acquire_owned() {
                        let (tx, secret) = (tx.clone(), secret.clone());
                        io.tasks.spawn(async move {
                            if let Ok((rd, wr)) = link::accept(s, &secret).await {
                                let _ = tx.send(RelayEvent::Linked(rd, wr, permit, local)).await;
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
                RelayEvent::Linked(rd, wr, permit, local) => {
                    let link = io.next_in;
                    io.next_in += 1; // < FIRST_OUTBOUND_LINK na prática
                    if local {
                        io.local.insert(link);
                    }
                    io.attach(link, rd, wr, Some(permit));
                }
                RelayEvent::Cell(link, cell) => {
                    let now = io.now();
                    let acts = node.handle_cell(link, cell, now);
                    io.run(acts);
                }
                RelayEvent::LinkClosed(link) => {
                    io.writers.remove(&link);
                    io.local.remove(&link);
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
