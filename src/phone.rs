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
//!
//! **Tela.** A transmissão de tela anda num circuito *só dela* (veja [`crate::media::screen`]),
//! com cover de [`SCREEN_SLOT_MS`] e chaves derivadas das da chamada. Quem transmite abre o
//! circuito até o telefone de quem assiste ([`share_screen`]); quem assiste recebe
//! [`ScreenEvent`] em [`ActiveCall::next_screen`]. A chamada que ligou também precisa estar
//! registrada no telefone da própria máquina ([`PhoneLink`]) para receber esse circuito.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
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
        keys::CallKeys,
        screen::{ScreenFrame, ScreenStats, ScreenStream},
        signal::{Answer, Caller, Incoming, RelayHint},
        update::{self, AddressRecord, Intro},
    },
    transport::{CircuitClient, ClientOptions},
};

/// Intervalo entre pacotes do circuito da tela: 200 pacotes/s, ≈ 277 kbps de vídeo.
pub const SCREEN_SLOT_MS: u64 = 2;

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
            Self::NoAnswer => write!(f, "sem resposta ao convite (confira se a sua chave está nos Contatos do outro lado e se ele está online)"),
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
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Uma chamada em andamento. Largar o valor (ou chamar [`hangup`](Self::hangup)) desliga.
pub struct ActiveCall {
    mic: mpsc::Sender<Vec<i16>>,
    speaker: mpsc::Receiver<Vec<i16>>,
    stats: Arc<Mutex<CallStats>>,
    peer_hung_up: Arc<AtomicBool>,
    screen: mpsc::Receiver<ScreenEvent>,
    share_keys: CallKeys,
}

/// O que chega do circuito da tela de quem está transmitindo.
#[derive(Debug, PartialEq, Eq)]
pub enum ScreenEvent {
    /// Quadro de vídeo remontado, pronto para o decodificador.
    Frame(ScreenFrame),
    /// Quem transmitia parou.
    Stopped,
    /// Total de quadros que chegaram incompletos ou sem referência (diagnóstico).
    Lost(u64),
}

impl ActiveCall {
    /// Próximo evento da tela de quem transmite para você. `None` = a chamada acabou.
    pub async fn next_screen(&mut self) -> Option<ScreenEvent> {
        self.screen.recv().await
    }

    /// Tira o canal de eventos da tela para ouvi-lo ao lado de [`Self::next_pcm`] (que também pede `&mut`).
    /// Depois disso, [`Self::next_screen`] devolve `None`.
    pub fn take_screen(&mut self) -> mpsc::Receiver<ScreenEvent> {
        std::mem::replace(&mut self.screen, mpsc::channel(1).1)
    }

    /// Chaves do circuito de tela quando *você* transmite (use em [`share_screen`]).
    pub fn share_keys(&self) -> &CallKeys {
        &self.share_keys
    }

    /// Um quadro do microfone (o tamanho que o codec espera). `Err` = a chamada acabou.
    pub async fn send(&self, pcm: Vec<i16>) -> Result<(), CallError> {
        self.mic.send(pcm).await.map_err(|_| CallError::Closed)
    }

    /// Próximo quadro para o alto-falante, no ritmo do codec. `None` = a chamada acabou
    /// (o outro lado desligou ou o circuito caiu).
    pub async fn next_pcm(&mut self) -> Option<Vec<i16>> {
        self.speaker.recv().await
    }

    /// A chamada acabou porque o outro lado desligou (e não por queda de rede).
    pub fn peer_hung_up(&self) -> bool {
        self.peer_hung_up.load(Ordering::Relaxed)
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

/// Rótulo das chaves da tela: quem transmite define o sentido, então os dois lados nunca
/// reusam um nonce se transmitirem ao mesmo tempo.
fn screen_label(sharer_is_caller: bool) -> &'static [u8] {
    if sharer_is_caller { b"screen/caller" } else { b"screen/callee" }
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
    peer_hung_up: Arc<AtomicBool>,
    view_keys: CallKeys,
    screen_tx: mpsc::Sender<ScreenEvent>,
) {
    // Tela de quem transmite para cá: o circuito dela chega por outra conexão e se prende à chamada
    // pelo primeiro pacote que autenticar.
    let mut screen = ScreenStream::new(&view_keys);
    let mut screen_conn: Option<u64> = None;
    let mut stopped_told = false;
    let mut lost_told = 0u64;
    let mut ask = interval(Duration::from_millis(100));
    ask.set_missed_tick_behavior(MissedTickBehavior::Delay);
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
                    let on_screen = screen_conn == Some(conn);
                    if on_screen || call.receive(now(), &data).is_err() {
                        if screen.receive(&data).is_ok() {
                            if !on_screen {
                                screen_conn = Some(conn);
                                let _ = wire_out.send(WireOut::Bound(conn)).await;
                            }
                            while let Some(f) = screen.poll_frame() {
                                let _ = screen_tx.try_send(ScreenEvent::Frame(f));
                            }
                            let lost = screen.stats().frames_lost;
                            if lost != lost_told {
                                lost_told = lost;
                                let _ = screen_tx.try_send(ScreenEvent::Lost(lost));
                            }
                            if screen.peer_stopped() != stopped_told {
                                stopped_told = screen.peer_stopped();
                                if stopped_told {
                                    let _ = screen_tx.try_send(ScreenEvent::Stopped);
                                }
                            }
                        }
                    } else {
                        if conn != current {
                            current = conn;
                            let _ = wire_out.send(WireOut::Bound(conn)).await;
                        }
                        if call.is_hung_up() {
                            peer_hung_up.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
                None => break,
            },
            // Imagem parada por perda: pede um quadro-chave, no máximo 10 vezes por segundo.
            _ = ask.tick(), if screen_conn.is_some() && screen.needs_key() => {
                if let (Some(conn), Ok(p)) = (screen_conn, screen.next_packet()) {
                    let _ = wire_out.send(WireOut::Frame { conn, data: p.to_vec() }).await;
                }
            }
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
    keys: &CallKeys,
    caller: bool,
    frame_ms: u64,
    conn: u64,
) -> (ActiveCall, mpsc::Sender<WireIn>, mpsc::Receiver<WireOut>) {
    let (in_tx, in_rx) = mpsc::channel(256);
    let (out_tx, out_rx) = mpsc::channel(256);
    let (mic_tx, mic_rx) = mpsc::channel(8);
    let (spk_tx, spk_rx) = mpsc::channel(32);
    let stats = Arc::new(Mutex::new(CallStats::default()));
    let peer_hung_up = Arc::new(AtomicBool::new(false));
    let (scr_tx, scr_rx) = mpsc::channel(64);
    let (share_keys, view_keys) = (keys.sub(screen_label(caller)), keys.sub(screen_label(!caller)));
    tokio::spawn(drive(voice, frame_ms, conn, in_rx, out_tx, mic_rx, spk_tx, stats.clone(), peer_hung_up.clone(), view_keys, scr_tx));
    (ActiveCall { mic: mic_tx, speaker: spk_rx, stats, peer_hung_up, screen: scr_rx, share_keys }, in_tx, out_rx)
}

/// Liga para `peer` (a chave do pareamento) pelo circuito de `client`.
///
/// `wait` limita cada espera: o circuito ficar pronto (se `client` já estiver pronto, não espera)
/// e o aceite chegar.
pub async fn call<C: Codec + Send + 'static>(
    client: CircuitClient,
    my_static: &StaticSecret,
    peer: &PublicKey,
    codec: C,
    wait: Duration,
) -> Result<ActiveCall, CallError> {
    call_linked(client, my_static, peer, codec, wait, None).await
}

/// Como [`call`], mas registrada no telefone desta máquina (`link`), que passa a entregar à
/// chamada o circuito de tela de quem está do outro lado.
pub async fn call_linked<C: Codec + Send + 'static>(
    mut client: CircuitClient,
    my_static: &StaticSecret,
    peer: &PublicKey,
    codec: C,
    wait: Duration,
    link: Option<&PhoneLink>,
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

    let (caller, invite) = Caller::invite_with_hint(my_static, peer, unix_secs(), PhoneLink::current_hint(link).as_ref())?;
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
    let (active, in_tx, mut out_rx) = start_call(VoiceCall::new(&keys, codec), &keys, true, frame_ms, 0);
    let linked = link.cloned().map(|l| (l.ids.fetch_add(1, Ordering::Relaxed), l));
    if let Some((id, l)) = &linked {
        let _ = l.actor.send(Msg::Listen { call: *id, tx: in_tx.clone() }).await;
    }
    tokio::spawn(async move {
        loop {
            tokio::select! {
                out = out_rx.recv() => match out {
                    // Sem circuito durante um failover a voz se perde; é o esperado.
                    Some(WireOut::Frame { conn: 0, data }) => { let _ = client.send(&data).await; }
                    Some(o) => forward_extra(&linked, o).await,
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
    /// Chamada feita por esta máquina: só recebe o que chegar por conexões novas (a tela).
    Listen { call: u64, tx: mpsc::Sender<WireIn> },
    Out(u64, WireOut),
    Reject(u64, Vec<u8>),
}

enum Bind {
    /// Convite entregue ao app, sem resposta ainda.
    Pending,
    Call(u64),
}

/// Ligação de uma chamada feita por esta máquina com o telefone dela: sem isso, só o lado que
/// atendeu consegue receber o circuito de tela do outro.
#[derive(Clone)]
pub struct PhoneLink {
    actor: mpsc::Sender<Msg>,
    ids: Arc<AtomicU64>,
    hint: Arc<Mutex<Option<RelayHint>>>,
}

impl PhoneLink {
    /// Relay desta máquina que os convites das chamadas ligadas a este telefone passam a levar, para o
    /// outro lado poder ligar de volta (`None`: não leva). Vale também para o que já foi criado
    /// por [`Phone::link`].
    pub fn set_relay_hint(&self, hint: Option<RelayHint>) {
        *self.hint.lock().unwrap() = hint;
    }

    fn current_hint(link: Option<&PhoneLink>) -> Option<RelayHint> {
        link.and_then(|l| l.hint.lock().unwrap().clone())
    }
}

/// Saída por uma conexão do telefone (tudo que não é o circuito próprio da chamada, conexão 0).
async fn forward_extra(linked: &Option<(u64, PhoneLink)>, out: WireOut) {
    if let Some((id, l)) = linked {
        let _ = l.actor.send(Msg::Out(*id, out)).await;
    }
}

/// Opções do cliente de circuito da tela: cover de circuito e de link no ritmo dos slots.
pub fn screen_client_options() -> ClientOptions {
    let every = Duration::from_millis(SCREEN_SLOT_MS);
    ClientOptions { cover: Some(every), circuit_cover: Some(every), ..Default::default() }
}

/// Quem transmite a tela: dono do circuito de tela até o telefone de quem assiste.
///
/// Largar o valor interrompe sem avisar; [`stop`](Self::stop) avisa quem assiste.
pub struct ScreenShare {
    stream: Arc<Mutex<ScreenStream>>,
    stopping: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl ScreenShare {
    /// Entrega um quadro codificado. `Ok(false)`: descartado porque a fila encheu e falta um quadro-chave.
    pub fn push(&self, data: &[u8], keyframe: bool) -> Result<bool, MediaError> {
        self.stream.lock().expect("tela").push_frame(data, keyframe)
    }

    /// Quem assiste perdeu imagem (ou a fila encheu): force um quadro-chave no codec. Consome o pedido.
    pub fn keyframe_wanted(&self) -> bool {
        self.stream.lock().expect("tela").take_keyframe_request()
    }

    pub fn stats(&self) -> ScreenStats {
        self.stream.lock().expect("tela").stats()
    }

    /// Fragmentos esperando saída no circuito.
    pub fn queued(&self) -> usize {
        self.stream.lock().expect("tela").queued()
    }

    /// Avisa quem assiste e encerra o circuito.
    pub async fn stop(self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _ = timeout(Duration::from_secs(2), async { while !self.task.is_finished() { tokio::time::sleep(Duration::from_millis(10)).await } }).await;
    }
}

impl Drop for ScreenShare {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Transmite a tela por `client`, um cliente de circuito até o telefone de quem assiste
/// (crie-o com [`screen_client_options`]). `keys` vêm de [`ActiveCall::share_keys`].
///
/// Espera o circuito ficar pronto (até `wait`) e manda um pacote de abertura, que prende o circuito
/// à chamada do outro lado. Depois sai um pacote por slot enquanto houver vídeo na fila; nos
/// demais instantes o cover do circuito mantém o fluxo no mesmo ritmo.
pub async fn share_screen(mut client: CircuitClient, keys: &CallKeys, wait: Duration) -> Result<ScreenShare, CallError> {
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

    let stream = Arc::new(Mutex::new(ScreenStream::new(keys)));
    let stopping = Arc::new(AtomicBool::new(false));
    let hello = stream.lock().expect("tela").next_packet()?;
    client.send(&hello).await.map_err(|_| CallError::NoCircuit)?;
    let task = {
        let (stream, stopping) = (stream.clone(), stopping.clone());
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_millis(SCREEN_SLOT_MS));
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if stopping.load(Ordering::Relaxed) {
                            for _ in 0..3 {
                                let p = stream.lock().expect("tela").stop_packet();
                                if let Ok(p) = p {
                                    let _ = client.send(&p).await;
                                }
                                tokio::time::sleep(Duration::from_millis(SCREEN_SLOT_MS)).await;
                            }
                            break;
                        }
                        let p = {
                            let mut s = stream.lock().expect("tela");
                            s.has_data().then(|| s.next_packet())
                        };
                        if let Some(Ok(p)) = p {
                            let _ = client.send(&p).await;
                        }
                    }
                    ev = client.next_event() => match ev {
                        Some(Event::Data(d)) => { let _ = stream.lock().expect("tela").receive(&d); }
                        Some(_) => {}
                        None => break,
                    },
                }
            }
            // `send` só enfileira: dá tempo de o último pacote sair antes de parar.
            tokio::time::sleep(Duration::from_millis(100)).await;
            client.stop();
        })
    };
    Ok(ScreenShare { stream, stopping, task })
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

    /// Relay de quem ligou, se o convite o trouxe (veja [`RelayHint`]): serve para ligar de volta.
    /// Não é autenticado; use só como último recurso.
    pub fn relay_hint(&self) -> Option<&RelayHint> {
        self.invite.relay_hint()
    }

    pub async fn accept<C: Codec + Send + 'static>(self, codec: C) -> Result<ActiveCall, CallError> {
        let (frame, keys) = self.invite.accept(&self.secret)?;
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let frame_ms = codec.frame_ms();
        let (active, in_tx, mut out_rx) = start_call(VoiceCall::new(&keys, codec), &keys, false, frame_ms, self.conn);
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
    notice: Arc<Mutex<Option<String>>>,
    contacts: Contacts,
    ids: Arc<AtomicU64>,
    link_tx: mpsc::Sender<Msg>,
    hint: Arc<Mutex<Option<RelayHint>>>,
    updates: Option<mpsc::Receiver<AddressUpdate>>,
    introductions: Option<mpsc::Receiver<Introduction>>,
    accept_intros: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
}

/// Apresentação de alguém que adicionou esta pessoa ([`crate::media::update`]): a chave dele está provada pelo
/// quadro, mas ele ainda não é contato; quem decide é o dono do telefone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Introduction {
    pub peer: [u8; 32],
    pub intro: Intro,
}

/// Apresentações aceitas por minuto, de todos juntos; passando disto, o excesso é descartado sem resposta.
const INTRO_PER_MINUTE: usize = 20;

/// Endereço novo que um contato mandou ([`crate::media::update`]), já aberto e autenticado.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressUpdate {
    pub peer: [u8; 32],
    pub record: AddressRecord,
}

/// Chaves aceitas pelo telefone; pode mudar com ele ligado (contatos entram e saem).
pub type Contacts = Arc<Mutex<Vec<[u8; 32]>>>;

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
        let notice: Arc<Mutex<Option<String>>> = Arc::default();
        let contacts: Contacts = Arc::new(Mutex::new(contacts));

        let accept_tx = actor_tx.clone();
        let accept = tokio::spawn(async move {
            let mut conn = 0u64;
            loop {
                // No Windows o accept pode falhar por uma conexão que já caiu; isso não encerra o telefone.
                let Ok((stream, _)) = listener.accept().await else {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    continue;
                };
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
        let (upd_tx, upd_rx) = mpsc::channel(64);
        let (intro_tx, intro_rx) = mpsc::channel(64);
        let accept_intros = Arc::new(AtomicBool::new(true));
        let out = ActorOut { incoming: inc_tx, updates: upd_tx, introductions: intro_tx, accept_intros: accept_intros.clone(), notice: notice.clone() };
        let actor = tokio::spawn(run_actor(actor_rx, actor_tx.clone(), out, my_static, contacts.clone(), ids.clone()));
        Ok(Self { addr, incoming: inc_rx, notice, contacts, ids, link_tx: actor_tx, hint: Arc::default(), updates: Some(upd_rx), introductions: Some(intro_rx), accept_intros, tasks: vec![accept, actor] })
    }

    /// Para registrar neste telefone as chamadas que esta máquina faz ([`call_linked`]).
    pub fn link(&self) -> PhoneLink {
        PhoneLink { actor: self.link_tx.clone(), ids: self.ids.clone(), hint: self.hint.clone() }
    }

    /// Lista viva de chaves aceitas.
    pub fn contacts(&self) -> Contacts {
        self.contacts.clone()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Motivo do último convite descartado em silêncio (remetente fora dos contatos, ou relógios
    /// diferentes), uma vez só. Só para diagnóstico local: o remetente não recebe resposta.
    pub fn take_notice(&self) -> Option<String> {
        self.notice.lock().unwrap().take()
    }

    /// Fila das atualizações de endereço que contatos mandam (cada uma já foi confirmada ao contato). Só se
    /// retira uma vez; sem quem a leia, as atualizações novas se perdem quando a fila enche.
    pub fn take_updates(&mut self) -> Option<mpsc::Receiver<AddressUpdate>> {
        self.updates.take()
    }

    /// Fila das apresentações de quem adicionou esta pessoa (cada uma já foi confirmada a quem mandou). Só se retira
    /// uma vez; sem quem a leia, as novas se perdem quando a fila enche.
    pub fn take_introductions(&mut self) -> Option<mpsc::Receiver<Introduction>> {
        self.introductions.take()
    }

    /// Liga ou desliga o recebimento de apresentações (ligado de início). Desligado, o telefone as descarta sem
    /// resposta, como faz com qualquer outro quadro de estranho.
    pub fn accept_introductions(&self, on: bool) {
        self.accept_intros.store(on, Ordering::Relaxed);
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

/// Quantos convites recentes o telefone lembra para recusar repetição.
const SEEN_INVITES: usize = 1024;

/// Efêmeras dos últimos convites aceitos para tocar: um convite gravado e reenviado não toca duas vezes.
#[derive(Default)]
struct SeenInvites {
    set: std::collections::HashSet<[u8; 32]>,
    order: std::collections::VecDeque<[u8; 32]>,
}

impl SeenInvites {
    /// `true` na primeira vez que vê `nonce`.
    fn first_time(&mut self, nonce: [u8; 32]) -> bool {
        if !self.set.insert(nonce) {
            return false;
        }
        self.order.push_back(nonce);
        if self.order.len() > SEEN_INVITES
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }
}

/// Para onde o telefone entrega o que recebe: convites, avisos de endereço e o motivo do último descarte.
struct ActorOut {
    incoming: mpsc::Sender<IncomingCall>,
    updates: mpsc::Sender<AddressUpdate>,
    introductions: mpsc::Sender<Introduction>,
    accept_intros: Arc<AtomicBool>,
    notice: Arc<Mutex<Option<String>>>,
}

async fn run_actor(
    mut rx: mpsc::Receiver<Msg>,
    me: mpsc::Sender<Msg>,
    out: ActorOut,
    secret: StaticSecret,
    contacts: Contacts,
    ids: Arc<AtomicU64>,
) {
    let ActorOut { incoming, updates, introductions, accept_intros, notice } = out;
    let mut intro_window: (Instant, usize) = (Instant::now(), 0);
    let mut writers: HashMap<u64, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut binds: HashMap<u64, Bind> = HashMap::new();
    let mut calls: HashMap<u64, mpsc::Sender<WireIn>> = HashMap::new();
    let mut seen = SeenInvites::default();
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
            Msg::Listen { call, tx } => {
                calls.insert(call, tx);
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
                    // Apresentação de quem adicionou esta pessoa: confirma e entrega, sem tocar o telefone.
                    if accept_intros.load(Ordering::Relaxed)
                        && let Some((peer, intro)) = update::open_intro(&secret, &data)
                    {
                        if intro_window.0.elapsed() >= Duration::from_secs(60) {
                            intro_window = (Instant::now(), 0);
                        }
                        intro_window.1 += 1;
                        if intro_window.1 <= INTRO_PER_MINUTE
                            && introductions.try_send(Introduction { peer, intro: intro.clone() }).is_ok()
                            && let Ok(ack) = update::seal_ack(&secret, &PublicKey::from(peer), intro.seq)
                            && let Some(w) = writers.get(&conn)
                        {
                            let _ = w.try_send(ack.to_vec());
                        }
                        continue;
                    }
                    // Aviso de endereço de um contato: confirma e entrega, sem tocar o telefone.
                    let known: Vec<[u8; 32]> = contacts.lock().unwrap().clone();
                    if let Some((peer, record)) = update::open_update(&secret, known, &data) {
                        if let Ok(ack) = update::seal_ack(&secret, &PublicKey::from(peer), record.seq)
                            && let Some(w) = writers.get(&conn)
                        {
                            let _ = w.try_send(ack.to_vec());
                        }
                        let _ = updates.try_send(AddressUpdate { peer, record });
                        continue;
                    }
                    let invite = Incoming::parse(&secret, &data).ok().filter(|i| {
                        let known = contacts.lock().unwrap().contains(i.caller().as_bytes());
                        if !known {
                            let hex: String = i.caller().as_bytes()[..4].iter().map(|b| format!("{b:02x}")).collect();
                            *notice.lock().unwrap() = Some(format!("convite de {hex}... ignorado: essa chave não está em Contatos"));
                            return false;
                        }
                        // O convite não leva prazo que dependa de relógio: o mesmo convite repetido não toca de novo.
                        seen.first_time(i.nonce())
                    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_invite_does_not_ring_twice() {
        let mut seen = SeenInvites::default();
        assert!(seen.first_time([1; 32]));
        assert!(!seen.first_time([1; 32]));
        assert!(seen.first_time([2; 32]));
    }

    #[test]
    fn the_oldest_invites_are_forgotten_past_the_limit() {
        let mut seen = SeenInvites::default();
        let id = |i: usize| {
            let mut n = [0u8; 32];
            n[..8].copy_from_slice(&(i as u64).to_be_bytes());
            n
        };
        for i in 0..=SEEN_INVITES {
            assert!(seen.first_time(id(i)));
        }
        assert_eq!((seen.set.len(), seen.order.len()), (SEEN_INVITES, SEEN_INVITES));
        assert!(seen.first_time(id(0)), "o mais antigo saiu da memória");
        assert!(!seen.first_time(id(SEEN_INVITES)), "o mais novo continua lembrado");
    }
}
