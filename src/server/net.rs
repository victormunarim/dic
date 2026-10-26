//! Servidores na rede: liga o motor ([`Servers`]) ao telefone e ao discador do nó.
//!
//! O motor não conhece rede: recebe quadros e devolve quadros. Aqui os quadros que chegam pelo telefone
//! entram no motor (e a confirmação volta pela mesma conexão), os que o motor manda saem em lote, um circuito
//! por destinatário, e o relógio e o endereço do nó são entregues ao motor. Quadro perdido não é reenviado:
//! a presença e a sincronização do motor já se repetem sozinhas.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{sync::mpsc, task::JoinHandle};
use x25519_dalek::StaticSecret;

use super::{
    ServerError,
    engine::{Event, Outbound, Output, Servers},
    wire::INVITE_SERVER,
};
use crate::{
    media::{signal::Frame, update::Reach},
    node::{Dialer, RelayInfo},
    phone::{Phone, ServerHandler},
};

/// Quanto se espera pelas confirmações de um lote.
const ACK_WAIT: Duration = Duration::from_secs(6);
/// Entregas ao mesmo tempo.
const PARALLEL: usize = 6;
/// Quadros que esperam por um destinatário ocupado ou fora do ar; passando disto, os mais velhos caem.
const BACKLOG: usize = 64;
/// Intervalo mínimo entre duas buscas do endereço do mesmo membro.
const SEARCH_EVERY: Duration = Duration::from_secs(120);
/// Eventos que esperam a janela; passando disto, os mais velhos caem.
const EVENTS: usize = 512;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

struct Inner {
    servers: Mutex<Servers>,
    send: mpsc::UnboundedSender<Outbound>,
    events: Mutex<VecDeque<Event>>,
    dialer: Dialer,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Quando se procurou cada membro na DHT e nas âncoras pela última vez.
    searched: Mutex<HashMap<[u8; 32], Instant>>,
}

impl Inner {
    /// Põe na fila o que o motor mandou fazer e devolve a confirmação a dar, se houver.
    fn dispatch(&self, out: Output) -> Option<Frame> {
        for o in out.send {
            let _ = self.send.send(o);
        }
        let mut events = self.events.lock().unwrap();
        for e in out.events {
            match e {
                Event::Address { key, reach } => self.dialer.add_hint(key, RelayInfo { addr: reach.addr, onion: reach.onion, link: reach.link }),
                e => {
                    if events.len() >= EVENTS {
                        events.pop_front();
                    }
                    events.push_back(e);
                }
            }
        }
        out.ack
    }
}

/// Os servidores deste nó: o motor, a fila de saída e os eventos para a janela. Barato de clonar.
#[derive(Clone)]
pub struct ServerHub {
    inner: Arc<Inner>,
}

impl ServerHub {
    /// Põe `servers` para funcionar: passa a atender os quadros que o telefone recebe e a mandar os seus.
    /// `own` é o relay deste nó como os outros o alcançam (veja [`crate::node::Node::relay_info`]).
    pub fn start(servers: Servers, dialer: Dialer, phone: &Phone, own: Arc<Mutex<Option<RelayInfo>>>) -> Self {
        let contacts = phone.contacts();
        let (send, queue) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner { servers: Mutex::new(servers), send, events: Mutex::default(), dialer, tasks: Mutex::default(), searched: Mutex::default() });
        let handler: ServerHandler = {
            let inner = inner.clone();
            Arc::new(move |frame: &[u8]| {
                // Convites só vêm de contatos: a lista é copiada antes para não segurar as duas travas juntas.
                let known: Vec<[u8; 32]> = contacts.lock().unwrap().clone();
                let out = inner.servers.lock().unwrap().handle_with(frame, now(), &known);
                inner.dispatch(out).map(|f| f.to_vec())
            })
        };
        phone.set_server_handler(handler);
        let tasks = vec![tokio::spawn(deliver_loop(inner.clone(), queue)), tokio::spawn(clock(inner.clone(), own))];
        *inner.tasks.lock().unwrap() = tasks;
        Self { inner }
    }

    /// Muda o estado e manda o que a mudança pede.
    pub fn run(&self, f: impl FnOnce(&mut Servers, u64) -> Output) {
        let out = {
            let mut s = self.inner.servers.lock().unwrap();
            f(&mut s, now())
        };
        self.inner.dispatch(out);
    }

    /// Como [`run`](Self::run), para o que pode falhar e devolve algo (o servidor criado, a sala criada...).
    pub fn try_run<T>(&self, f: impl FnOnce(&mut Servers, u64) -> Result<(T, Output), ServerError>) -> Result<T, ServerError> {
        let result = {
            let mut s = self.inner.servers.lock().unwrap();
            f(&mut s, now())
        };
        let (value, out) = result?;
        self.inner.dispatch(out);
        Ok(value)
    }

    /// Como [`try_run`](Self::try_run), para o que só devolve o que fazer.
    pub fn try_act(&self, f: impl FnOnce(&mut Servers, u64) -> Result<Output, ServerError>) -> Result<(), ServerError> {
        self.try_run(|s, now| f(s, now).map(|out| ((), out)))
    }

    /// Lê o estado (visões, exportação...).
    pub fn read<R>(&self, f: impl FnOnce(&Servers, u64) -> R) -> R {
        f(&self.inner.servers.lock().unwrap(), now())
    }

    /// Eventos desde a última chamada, do mais velho ao mais novo.
    pub fn take_events(&self) -> Vec<Event> {
        self.inner.events.lock().unwrap().drain(..).collect()
    }

    /// Chave pública desta pessoa.
    pub fn my_key(&self) -> [u8; 32] {
        self.inner.servers.lock().unwrap().my_key()
    }

    /// Para as tarefas de rede (o nó chama ao parar).
    pub fn stop(&self) {
        for t in self.inner.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }
}

/// Estado vazio de `secret`, ou o que [`Servers::export`] guardou.
pub fn restore(secret: StaticSecret, saved: Option<&str>) -> Servers {
    match saved {
        Some(text) => Servers::import(secret, text),
        None => Servers::new(secret),
    }
}

/// Relógio do motor e acompanhamento do endereço do nó.
async fn clock(inner: Arc<Inner>, own: Arc<Mutex<Option<RelayInfo>>>) {
    let mut last: Option<Option<RelayInfo>> = None;
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    loop {
        ticks.tick().await;
        let mine = own.lock().unwrap().clone();
        let first = last.is_none();
        let out = {
            let mut s = inner.servers.lock().unwrap();
            let mut out = Output::default();
            if last.as_ref() != Some(&mine) {
                let reach = mine.clone().map(|r| Reach { addr: r.addr, onion: r.onion, link: r.link });
                out = s.set_reach(reach, millis(), now());
                last = Some(mine);
            }
            if first {
                let online = s.online(now());
                out.send.extend(online.send);
                out.events.extend(online.events);
            }
            let tick = s.tick(now());
            out.send.extend(tick.send);
            out.events.extend(tick.events);
            out
        };
        inner.dispatch(out);
    }
}

/// Entrega a fila: um circuito por destinatário com o que ele tem à espera, no máximo [`PARALLEL`] ao mesmo tempo.
async fn deliver_loop(inner: Arc<Inner>, mut queue: mpsc::UnboundedReceiver<Outbound>) {
    let mut backlog: HashMap<[u8; 32], VecDeque<Outbound>> = HashMap::new();
    let mut busy: HashSet<[u8; 32]> = HashSet::new();
    let (done_tx, mut done) = mpsc::unbounded_channel::<[u8; 32]>();
    loop {
        tokio::select! {
            Some(o) = queue.recv() => {
                let q = backlog.entry(o.to).or_default();
                if q.len() >= BACKLOG {
                    q.pop_front();
                }
                q.push_back(o);
            }
            Some(peer) = done.recv() => {
                busy.remove(&peer);
            }
            else => break,
        }
        let ready: Vec<[u8; 32]> = backlog.iter().filter(|(k, q)| !q.is_empty() && !busy.contains(*k)).map(|(k, _)| *k).collect();
        for peer in ready {
            if busy.len() >= PARALLEL {
                break;
            }
            let batch: Vec<Outbound> = backlog.get_mut(&peer).map(|q| q.drain(..).collect()).unwrap_or_default();
            busy.insert(peer);
            let (inner, done_tx) = (inner.clone(), done_tx.clone());
            tokio::spawn(async move {
                deliver(&inner, peer, batch).await;
                let _ = done_tx.send(peer);
            });
        }
        backlog.retain(|_, q| !q.is_empty());
    }
}

async fn deliver(inner: &Inner, peer: [u8; 32], batch: Vec<Outbound>) {
    // Quem ainda não tem endereço guardado usa o último que anunciou.
    if inner.dialer.stored_for(&peer).is_empty()
        && let Some(reach) = batch.iter().rev().find_map(|o| o.via.clone())
    {
        inner.dialer.add_hint(peer, RelayInfo { addr: reach.addr, onion: reach.onion, link: reach.link });
    }
    let frames: Vec<Frame> = batch.iter().map(|o| o.frame).collect();
    // Convite para um contato: ele pode não ter falado com este nó ainda, então vale procurar o endereço dele.
    let invites: Vec<usize> = (0..batch.len()).filter(|i| batch[*i].server == INVITE_SERVER).collect();
    let got = Mutex::new(HashSet::new());
    let acked = |i: usize, reply: &[u8]| {
        let ok = inner.servers.lock().unwrap().is_ack(&batch[i], reply);
        if ok {
            got.lock().unwrap().insert(i);
        }
        ok
    };
    let confirmed = inner.dialer.deliver_batch(peer, &frames, acked, ACK_WAIT, !invites.is_empty()).await;
    if !invites.is_empty() && invites.iter().all(|i| got.lock().unwrap().contains(i)) {
        inner.servers.lock().unwrap().invite_delivered(&peer, now());
    }
    if confirmed > 0 {
        return;
    }
    // Nada confirmado: o membro pode ter mudado de endereço. Procura o registro que ele deixou para os membros.
    let shared = inner.servers.lock().unwrap().shared_secrets(&peer);
    let due = {
        let mut searched = inner.searched.lock().unwrap();
        searched.retain(|_, at| at.elapsed() < SEARCH_EVERY);
        !shared.is_empty() && searched.insert(peer, Instant::now()).is_none()
    };
    if due && inner.dialer.find_member(&peer, &shared).await {
        inner.dialer.deliver_batch(peer, &frames, acked, ACK_WAIT, false).await;
    }
}
