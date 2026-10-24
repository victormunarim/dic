//! Telefone: liga a camada de mídia ao circuito (feature `net`).
//!
//! - **Chamador**: [`call`] espera o circuito, manda o convite, espera o aceite e devolve uma
//!   [`ActiveCall`].
//! - **Chamado** (o "papel de destino"): [`Phone`] escuta num TCP — é o endereço que a política
//!   de saída dos relays aponta (`contato:9`). Cada circuito chega como uma conexão que fala
//!   quadros `[u16 BE tamanho][dados]`. Convites de contatos conhecidos viram [`IncomingCall`].
//!
//! ```text
//! chamador ── circuito ──> relay A ─> relay B ──TCP──> Phone ──> IncomingCall::accept
//!    call()                                                      ActiveCall (PCM)
//! ```
//!
//! **Failover.** O circuito reserva abre uma segunda conexão com o `Phone`. Quando o ativo
//! cai, a voz passa a chegar nela, sem convite. O `Phone` tenta o pacote em cada chamada em
//! andamento; a chamada que o autenticar passa a responder por aquela conexão. Pacote repetido
//! (que a autenticação recusa) nunca troca a rota.
//!
//! PCM entra e sai por canais; o relógio de reprodução (`frame_ms`) é interno.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, tcp::OwnedReadHalf, tcp::OwnedWriteHalf},
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, timeout},
};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{
    circuit::manager::Event,
    media::{
        CallStats, MediaError, VoiceCall,
        codec::Codec,
        signal::{Answer, Caller, Incoming},
    },
    transport::CircuitClient,
};

/// Maior quadro `[u16][dados]` aceito do circuito; os de mídia e sinalização têm 200 B.
const MAX_FRAME: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum CallError {
    /// O circuito não ficou pronto a tempo.
    NoCircuit,
    /// Ninguém respondeu ao convite a tempo.
    NoAnswer,
    Rejected,
    /// O circuito fechou.
    Closed,
    Media(MediaError),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCircuit => write!(f, "circuito não ficou pronto"),
            Self::NoAnswer => write!(f, "sem resposta ao convite"),
            Self::Rejected => write!(f, "chamada recusada"),
            Self::Closed => write!(f, "circuito fechado"),
            Self::Media(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CallError {}

impl From<MediaError> for CallError {
    fn from(e: MediaError) -> Self {
        Self::Media(e)
    }
}

fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Uma chamada em andamento. Largar o valor (ou chamar [`hangup`](Self::hangup)) desliga.
pub struct ActiveCall {
    mic: mpsc::Sender<Vec<i16>>,
    speaker: mpsc::Receiver<Vec<i16>>,
    stats: Arc<Mutex<CallStats>>,
}

impl ActiveCall {
    /// Um quadro do microfone (o tamanho que o codec espera). `Err` = a chamada acabou.
    pub async fn send(&self, pcm: Vec<i16>) -> Result<(), CallError> {
        self.mic.send(pcm).await.map_err(|_| CallError::Closed)
    }

    /// Próximo quadro para o alto-falante, no ritmo do codec. `None` = a chamada acabou
    /// (o outro lado desligou ou o circuito caiu).
    pub async fn next_pcm(&mut self) -> Option<Vec<i16>> {
        self.speaker.recv().await
    }

    pub fn stats(&self) -> CallStats {
        *self.stats.lock().expect("estatísticas")
    }

    /// Desliga: manda o pacote de desligar e encerra.
    pub fn hangup(self) {}
}

/// Por onde os quadros de uma chamada entram e saem: `conn` identifica a conexão de origem
/// (sempre 0 no chamador, que tem um só circuito).
type WireIn = (u64, Vec<u8>);

enum WireOut {
    Frame { conn: u64, data: Vec<u8> },
    /// Um pacote autenticado chegou por `conn`, que não era a rota atual.
    Bound(u64),
}

/// Motor de uma chamada: cifra o microfone, decifra a rede, toca no ritmo do codec.
#[allow(clippy::too_many_arguments)]
async fn drive<C: Codec>(
    mut call: VoiceCall<C>,
    frame_ms: u64,
    mut current: u64,
    mut wire_in: mpsc::Receiver<WireIn>,
    wire_out: mpsc::Sender<WireOut>,
    mut mic: mpsc::Receiver<Vec<i16>>,
    speaker: mpsc::Sender<Vec<i16>>,
    stats: Arc<Mutex<CallStats>>,
) {
    let t0 = Instant::now();
    let now = || t0.elapsed().as_millis() as u64;
    let mut tick = interval(Duration::from_millis(frame_ms));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            pcm = mic.recv() => match pcm {
                Some(pcm) => {
                    if let Ok(p) = call.send(&pcm) {
                        let _ = wire_out.send(WireOut::Frame { conn: current, data: p.to_vec() }).await;
                    }
                }
                None => {
                    // Três cópias seguidas: a voz tolera perda, o desligar não deveria depender de uma célula.
                    for _ in 0..3 {
                        if let Ok(p) = call.hangup() {
                            let _ = wire_out.send(WireOut::Frame { conn: current, data: p.to_vec() }).await;
                        }
                        tokio::time::sleep(Duration::from_millis(frame_ms)).await;
                    }
                    break;
                }
            },
            inb = wire_in.recv() => match inb {
                Some((conn, data)) => {
                    if call.receive(now(), &data).is_ok() {
                        if conn != current {
                            current = conn;
                            let _ = wire_out.send(WireOut::Bound(conn)).await;
                        }
                        if call.is_hung_up() {
                            break;
                        }
                    }
                }
                None => break,
            },
            _ = tick.tick() => {
                for f in call.playout(now()) {
                    let _ = speaker.try_send(f);
                }
                *stats.lock().expect("estatísticas") = call.stats();
            }
        }
    }
    *stats.lock().expect("estatísticas") = call.stats();
}

/// Sobe o motor de uma chamada e devolve a ponta que o app usa.
fn start_call<C: Codec + Send + 'static>(
    voice: VoiceCall<C>,
    frame_ms: u64,
    conn: u64,
) -> (ActiveCall, mpsc::Sender<WireIn>, mpsc::Receiver<WireOut>) {
    let (in_tx, in_rx) = mpsc::channel(256);
    let (out_tx, out_rx) = mpsc::channel(256);
    let (mic_tx, mic_rx) = mpsc::channel(8);
    let (spk_tx, spk_rx) = mpsc::channel(32);
    let stats = Arc::new(Mutex::new(CallStats::default()));
    tokio::spawn(drive(voice, frame_ms, conn, in_rx, out_tx, mic_rx, spk_tx, stats.clone()));
    (ActiveCall { mic: mic_tx, speaker: spk_rx, stats }, in_tx, out_rx)
}

/// Liga para `peer` (a chave do pareamento) pelo circuito de `client`.
///
/// `wait` limita cada espera: o circuito ficar pronto (se `client` já estiver pronto, não espera)
/// e o aceite chegar.
pub async fn call<C: Codec + Send + 'static>(
    mut client: CircuitClient,
    my_static: &StaticSecret,
    peer: &PublicKey,
    codec: C,
    wait: Duration,
) -> Result<ActiveCall, CallError> {
    let ready = client.status().await.is_some_and(|s| s.active.is_some_and(|a| a.ready));
    timeout(wait, async {
        if ready {
            return Ok(());
        }
        loop {
            match client.next_event().await {
                Some(Event::Ready { .. }) => return Ok(()),
                Some(_) => {}
                None => return Err(CallError::Closed),
            }
        }
    })
    .await
    .map_err(|_| CallError::NoCircuit)??;

    let (caller, invite) = Caller::invite(my_static, peer, unix_secs())?;
    client.send(&invite).await.map_err(|_| CallError::NoCircuit)?;
    let keys = timeout(wait, async {
        loop {
            match client.next_event().await {
                Some(Event::Data(d)) => match caller.answer(my_static, &d) {
                    Ok(Answer::Accepted(k)) => return Ok(k),
                    Ok(Answer::Rejected) => return Err(CallError::Rejected),
                    Err(_) => {}
                },
                Some(_) => {}
                None => return Err(CallError::Closed),
            }
        }
    })
    .await
    .map_err(|_| CallError::NoAnswer)??;

    let frame_ms = codec.frame_ms();
    let (active, in_tx, mut out_rx) = start_call(VoiceCall::new(&keys, codec), frame_ms, 0);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                out = out_rx.recv() => match out {
                    // Sem circuito durante um failover a voz se perde; é o esperado.
                    Some(WireOut::Frame { data, .. }) => { let _ = client.send(&data).await; }
                    Some(WireOut::Bound(_)) => {}
                    None => break,
                },
                ev = client.next_event() => match ev {
                    Some(Event::Data(d)) => { if in_tx.send((0, d)).await.is_err() { break; } }
                    Some(_) => {}
                    None => break,
                },
            }
        }
        // `send` só enfileira: dá tempo de o último pacote (o desligar) sair antes de parar.
        tokio::time::sleep(Duration::from_millis(100)).await;
        client.stop();
    });
    Ok(active)
}

enum Msg {
    Open(u64, mpsc::Sender<Vec<u8>>),
    Frame(u64, Vec<u8>),
    Closed(u64),
    Register { call: u64, conn: u64, tx: mpsc::Sender<WireIn> },
    Out(u64, WireOut),
    Reject(u64, Vec<u8>),
}

enum Bind {
    /// Convite entregue ao app, sem resposta ainda.
    Pending,
    Call(u64),
}

/// Convite de um contato conhecido. Responda com [`accept`](Self::accept) ou [`reject`](Self::reject).
pub struct IncomingCall {
    invite: Incoming,
    secret: StaticSecret,
    conn: u64,
    actor: mpsc::Sender<Msg>,
    ids: Arc<AtomicU64>,
}

impl IncomingCall {
    pub fn caller(&self) -> &PublicKey {
        self.invite.caller()
    }

    pub async fn accept<C: Codec + Send + 'static>(self, codec: C) -> Result<ActiveCall, CallError> {
        let (frame, keys) = self.invite.accept(&self.secret)?;
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let frame_ms = codec.frame_ms();
        let (active, in_tx, mut out_rx) = start_call(VoiceCall::new(&keys, codec), frame_ms, self.conn);
        let actor = self.actor.clone();
        tokio::spawn(async move {
            while let Some(o) = out_rx.recv().await {
                if actor.send(Msg::Out(id, o)).await.is_err() {
                    break;
                }
            }
        });
        let reg = Msg::Register { call: id, conn: self.conn, tx: in_tx };
        self.actor.send(reg).await.map_err(|_| CallError::Closed)?;
        let out = Msg::Out(id, WireOut::Frame { conn: self.conn, data: frame.to_vec() });
        self.actor.send(out).await.map_err(|_| CallError::Closed)?;
        Ok(active)
    }

    pub async fn reject(self) {
        let frame = self.invite.reject();
        let _ = self.actor.send(Msg::Reject(self.conn, frame.to_vec())).await;
    }
}

/// Ponta de destino: recebe os circuitos e atende as chamadas.
pub struct Phone {
    addr: SocketAddr,
    incoming: mpsc::Receiver<IncomingCall>,
    tasks: Vec<JoinHandle<()>>,
}

impl Phone {
    /// Escuta em `listen`. Só convites de `contacts` (chaves estáticas do pareamento) tocam;
    /// os outros são descartados sem resposta, para não revelar que o telefone existe.
    pub async fn bind(listen: SocketAddr, my_static: StaticSecret, contacts: Vec<[u8; 32]>) -> io::Result<Self> {
        crate::timer::fine_timers();
        let listener = TcpListener::bind(listen).await?;
        let addr = listener.local_addr()?;
        let (actor_tx, actor_rx) = mpsc::channel(1024);
        let (inc_tx, inc_rx) = mpsc::channel(8);
        let ids = Arc::new(AtomicU64::new(1));

        let accept_tx = actor_tx.clone();
        let accept = tokio::spawn(async move {
            let mut conn = 0u64;
            while let Ok((stream, _)) = listener.accept().await {
                conn += 1;
                let _ = stream.set_nodelay(true);
                let (rd, wr) = stream.into_split();
                let (w_tx, w_rx) = mpsc::channel(256);
                if accept_tx.send(Msg::Open(conn, w_tx)).await.is_err() {
                    break;
                }
                tokio::spawn(write_frames(wr, w_rx));
                tokio::spawn(read_frames(rd, conn, accept_tx.clone()));
            }
        });
        let actor = tokio::spawn(run_actor(actor_rx, actor_tx, inc_tx, my_static, contacts, ids));
        Ok(Self { addr, incoming: inc_rx, tasks: vec![accept, actor] })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Próximo convite de um contato. `None` = o telefone parou.
    pub async fn next_incoming(&mut self) -> Option<IncomingCall> {
        self.incoming.recv().await
    }

    pub fn stop(self) {}
}

impl Drop for Phone {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn read_frames(mut rd: OwnedReadHalf, conn: u64, actor: mpsc::Sender<Msg>) {
    let mut len = [0u8; 2];
    while rd.read_exact(&mut len).await.is_ok() {
        let n = u16::from_be_bytes(len) as usize;
        if n > MAX_FRAME {
            break;
        }
        let mut buf = vec![0u8; n];
        if rd.read_exact(&mut buf).await.is_err() || actor.send(Msg::Frame(conn, buf)).await.is_err() {
            break;
        }
    }
    let _ = actor.send(Msg::Closed(conn)).await;
}

async fn write_frames(mut wr: OwnedWriteHalf, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(data) = rx.recv().await {
        let mut f = (data.len() as u16).to_be_bytes().to_vec();
        f.extend_from_slice(&data);
        if wr.write_all(&f).await.is_err() {
            break;
        }
    }
}

async fn run_actor(
    mut rx: mpsc::Receiver<Msg>,
    me: mpsc::Sender<Msg>,
    incoming: mpsc::Sender<IncomingCall>,
    secret: StaticSecret,
    contacts: Vec<[u8; 32]>,
    ids: Arc<AtomicU64>,
) {
    let mut writers: HashMap<u64, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut binds: HashMap<u64, Bind> = HashMap::new();
    let mut calls: HashMap<u64, mpsc::Sender<WireIn>> = HashMap::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Open(c, w) => {
                writers.insert(c, w);
            }
            Msg::Closed(c) => {
                writers.remove(&c);
                binds.remove(&c);
            }
            Msg::Register { call, conn, tx } => {
                calls.insert(call, tx);
                binds.insert(conn, Bind::Call(call));
            }
            Msg::Reject(conn, data) => {
                binds.remove(&conn);
                if let Some(w) = writers.get(&conn) {
                    let _ = w.try_send(data);
                }
            }
            Msg::Out(_, WireOut::Frame { conn, data }) => {
                if let Some(w) = writers.get(&conn) {
                    let _ = w.try_send(data);
                }
            }
            Msg::Out(call, WireOut::Bound(conn)) => {
                binds.insert(conn, Bind::Call(call));
            }
            Msg::Frame(conn, data) => match binds.get(&conn) {
                Some(Bind::Call(c)) => {
                    let c = *c;
                    if calls.get(&c).is_some_and(|t| t.try_send((conn, data)).is_err_and(|e| matches!(e, mpsc::error::TrySendError::Closed(_)))) {
                        calls.remove(&c);
                        binds.remove(&conn);
                    }
                }
                Some(Bind::Pending) => {}
                None => {
                    let invite = Incoming::parse(&secret, unix_secs(), &data).ok().filter(|i| contacts.contains(i.caller().as_bytes()));
                    if let Some(invite) = invite {
                        let call = IncomingCall { invite, secret: secret.clone(), conn, actor: me.clone(), ids: ids.clone() };
                        if incoming.try_send(call).is_ok() {
                            binds.insert(conn, Bind::Pending);
                        }
                    } else {
                        // Voz que chegou por uma conexão nova (failover): vale a chamada que a autenticar.
                        calls.retain(|_, t| !t.is_closed());
                        for t in calls.values() {
                            let _ = t.try_send((conn, data.clone()));
                        }
                    }
                }
            },
        }
    }
}
