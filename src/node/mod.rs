//! Nó: o que cada pessoa roda para ligar e receber chamadas (feature `net`).
//!
//! Um [`Node`] junta o que antes era configurado à mão: um relay (que também serve de último salto para quem
//! liga para você), o telefone ([`Phone`]), a fila que entrega o endereço novo aos contatos e, se ligados, o
//! ponto de encontro na DHT e a âncora. Não há servidor: quem usa informa a identidade e os contatos; para ligar,
//! precisa da chave pública do outro e de um relay dele (o código de contato direto, o último endereço guardado,
//! o aviso que ele mandou, a DHT ou uma âncora).
//!
//! A chamada vai pelos relays dos próprios contatos, de ponta a ponta.

mod config;
mod deliver;
mod dialer;
mod lookup;
mod mail;
mod publish;
mod reach;
mod util;

pub use config::{DEFAULT_RELAY_PORT, NodeConfig, RelayKeys, UPDATE_RETRY};
pub use dialer::{DialError, Dialer};
use mail::{mail_keeper, mail_reader};
use publish::{outbox, rendezvous_publisher, server_publisher};
use reach::{Reachability, own_relay, relay_hint, track_address, watch_address};
use util::{push_note, unix_millis};

use crate::sync::LockExt;
use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::task::JoinHandle;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{
    mailbox::Custody,
    circuit::{
        manager::Candidate,
        relay::{RelayConfig, RelayNode},
    },
    upnp,
    media::{
        codec::Codec,
        update::{self, AddressRecord, Intro},
    },
    phone::{ActiveCall, IncomingCall, Introduction, Phone, ScreenShare},
    server::net::{ServerHub, restore},
    rendezvous::{self, ANCHOR_DEST, Anchor, GATEWAY_DEST, Rendezvous},
    transport::{RelayServer, ServerConfig, allow_list},
};

/// Nome do destino que o relay de saída mapeia para o telefone de `key`.
#[must_use]
pub fn dest_for(key: &[u8; 32]) -> String {
    let hex: String = key[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}:9")
}

/// Como alcançar um relay: endereço TCP e as duas chaves públicas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayInfo {
    pub addr: String,
    pub onion: [u8; 32],
    pub link: [u8; 32],
}

impl RelayInfo {
    #[must_use]
    pub fn candidate(&self) -> Candidate {
        Candidate { addr: self.addr.clone(), public: self.onion, link_public: self.link, score: 1.0 }
    }
}

/// Quantos endereços antigos de cada contato ficam guardados para tentar depois do atual.
pub const HISTORY: usize = 4;

/// Maior espera entre duas tentativas de entrega do endereço novo.
const UPDATE_MAX_WAIT: Duration = Duration::from_secs(2 * 3600);

/// Depois disto sem confirmação do contato, a entrega do endereço novo é abandonada.
const UPDATE_GIVE_UP: Duration = Duration::from_secs(3 * 24 * 3600);

/// O registro do endereço atual deste relay (veja [`track_address`]); vazio até o endereço ficar conhecido.
type Current = Arc<Mutex<Option<AddressRecord>>>;

type Known = Arc<Mutex<HashMap<[u8; 32], RelayInfo>>>;

/// Endereços antigos por contato, o mais novo primeiro (até [`HISTORY`]).
type History = Arc<Mutex<HashMap<[u8; 32], Vec<RelayInfo>>>>;

/// `relay` passa a ser o endereço guardado de `peer`; o que estava guardado vai para o histórico.
fn set_known(known: &Known, history: &History, peer: &[u8; 32], relay: RelayInfo) {
    let old = known.lock_unpoisoned().insert(*peer, relay.clone());
    let mut history = history.lock_unpoisoned();
    let list = history.entry(*peer).or_default();
    list.retain(|r| *r != relay);
    if let Some(old) = old.filter(|o| *o != relay) {
        list.retain(|r| *r != old);
        list.insert(0, old);
    }
    list.truncate(HISTORY);
    if list.is_empty() {
        history.remove(peer);
    }
}

/// Quem adicionou esta pessoa e se apresentou ([`Node::introduce`] do outro lado): ainda não é contato.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Introduced {
    pub peer: [u8; 32],
    pub name: String,
    pub reach: update::Reach,
}

/// Espera a confirmação de uma apresentação (como [`UPDATE_ACK_WAIT`]).
const INTRO_ACK_WAIT: Duration = Duration::from_secs(8);

/// O que a DHT e a âncora precisam de `Node::start` (o relay é criado lá, com os serviços na política de saída)
/// em `finish`.
#[derive(Default)]
struct Services {
    client: Option<Arc<Rendezvous>>,
    servers: Vec<JoinHandle<()>>,
    note: Option<String>,
}

pub struct Node {
    secret: StaticSecret,
    phone: Phone,
    _relay: RelayServer,
    /// Endereço informado ou aberto por UPnP; na falta, o local.
    addr: Arc<Mutex<String>>,
    /// Renova a porta aberta no roteador e acompanha o endereço do nó, se ele não foi informado.
    renew: Option<JoinHandle<()>>,
    reach: Reachability,
    note: Option<String>,
    dialer: Dialer,
    known: Known,
    hints: Known,
    history: History,
    /// Relay desta máquina como os outros o alcançam (o que vai no código de contato direto).
    own: Arc<Mutex<Option<RelayInfo>>>,
    /// Último aviso de endereço novo recebido de um contato, para o app mostrar (veja [`Node::take_notice`]).
    update_notice: Arc<Mutex<Option<String>>>,
    /// Como esta pessoa se apresenta (veja [`NodeConfig::name`]).
    name: String,
    /// Espera inicial entre tentativas de entrega (veja [`NodeConfig::update_retry`]).
    retry: Duration,
    /// Apresentações recebidas que o app ainda não retirou ([`Node::take_introductions`]).
    introduced: Arc<Mutex<Vec<Introduced>>>,
    /// Apresentações em entrega: por contato, a tarefa que insiste até a confirmação ([`Node::introduce`]).
    introducing: Arc<Mutex<HashMap<[u8; 32], JoinHandle<()>>>>,
    /// Avisos de remoção em entrega: por ex-contato, a tarefa que insiste até a confirmação ([`Node::retire`]).
    retiring: Arc<Mutex<HashMap<[u8; 32], JoinHandle<()>>>>,
    /// Quem removeu esta pessoa e já foi tirado dos contatos do telefone ([`Node::take_removals`]).
    removed: Arc<Mutex<Vec<[u8; 32]>>>,
    /// Os servidores desta pessoa (veja [`Node::servers`]).
    servers: ServerHub,
    /// Tarefas de fundo: a que aplica os avisos de endereço dos contatos e a fila de saída dos nossos.
    background: Vec<JoinHandle<()>>,
}

impl Node {
    /// Sobe relay e telefone. Recebe de quem tem o seu endereço (o código de contato direto, ou o aviso que
    /// você mandou) e liga para quem está em `known`, em `hints` ou na DHT e nas âncoras.
    ///
    /// # Errors
    ///
    /// Erro de E/S ao abrir a porta do relay ou a do telefone.
    pub async fn start(cfg: NodeConfig) -> io::Result<Self> {
        let me = PublicKey::from(&cfg.secret);
        let phone = Phone::bind("127.0.0.1:0".parse().unwrap(), cfg.secret.clone(), cfg.contacts.clone()).await?;
        let keys = cfg.relay_keys.clone().unwrap_or_else(RelayKeys::random);
        let relay_node = RelayNode::new(keys.onion.clone(), RelayConfig::default(), 0);
        let onion = relay_node.public_key();
        let mut scfg = ServerConfig::new(SocketAddr::from(([0, 0, 0, 0], cfg.relay_port)));
        scfg.link_secret = Some(keys.link.clone());
        let mut exit = vec![(dest_for(&me.to_bytes()), phone.local_addr().to_string())];
        // Com a DHT ligada, o nó também é gateway dela para quem monta um circuito até aqui (veja `rendezvous`).
        let mut dht = Services::default();
        if let Some(options) = &cfg.dht {
            match Rendezvous::start(options).await {
                Ok(client) => {
                    let client = Arc::new(client);
                    match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                        Ok(listener) => {
                            exit.push((GATEWAY_DEST.to_string(), listener.local_addr()?.to_string()));
                            dht.servers.push(tokio::spawn(rendezvous::serve_gateway(listener, client.clone())));
                        }
                        Err(e) => dht.note = Some(format!("sem gateway da DHT: {e}")),
                    }
                    dht.client = Some(client);
                }
                Err(e) => dht.note = Some(format!("sem DHT: {e}")),
            }
        }
        if cfg.anchor {
            match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                Ok(listener) => {
                    exit.push((ANCHOR_DEST.to_string(), listener.local_addr()?.to_string()));
                    dht.servers.push(tokio::spawn(rendezvous::serve_anchor(listener, Arc::new(Anchor::new()))));
                }
                Err(e) => push_note(&mut dht.note, format!("sem âncora: {e}")),
            }
        }
        scfg.exit = allow_list(exit);
        let relay = match RelayServer::spawn(relay_node, scfg.clone()).await {
            Ok(r) => r,
            // Porta ocupada: o sistema escolhe outra. `RelayNode` foi consumido, então recria.
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && cfg.relay_port != 0 => {
                let n = RelayNode::new(keys.onion.clone(), RelayConfig::default(), 0);
                let onion = n.public_key();
                scfg.listen = SocketAddr::from(([0, 0, 0, 0], 0));
                let r = RelayServer::spawn(n, scfg).await?;
                return Self::finish(cfg, phone, r, onion, dht).await;
            }
            Err(e) => return Err(e),
        };
        Self::finish(cfg, phone, relay, onion, dht).await
    }

    async fn finish(cfg: NodeConfig, mut phone: Phone, relay: RelayServer, onion: [u8; 32], dht: Services) -> io::Result<Self> {
        let NodeConfig { secret, public_addr, upnp, known, hints, history, updates, update_retry, dht: dht_options, name, introductions, anchors, servers: saved_servers, custody: saved_custody, mail_poll, .. } = cfg;
        let known: Known = Arc::new(Mutex::new(known.into_iter().collect()));
        let hints: Known = Arc::new(Mutex::new(hints.into_iter().collect()));
        // A lista guarda do mais novo ao mais velho de cada contato; ao reler, a ordem se mantém.
        let mut by_peer: HashMap<[u8; 32], Vec<RelayInfo>> = HashMap::new();
        for (peer, r) in history {
            let list = by_peer.entry(peer).or_default();
            if list.len() < HISTORY && !list.contains(&r) {
                list.push(r);
            }
        }
        let history: History = Arc::new(Mutex::new(by_peer));
        let (port, link) = (relay.local_addr().port(), relay.link_public_key());
        // Nenhum endereço público informado: tenta abrir a porta no roteador.
        let informed = public_addr.is_some();
        let (mut public_addr, mut mapping, mut note) = (public_addr, None, None);
        if upnp && public_addr.is_none() {
            match upnp::map(port).await {
                Ok(m) => {
                    note = Some(format!("porta {port} aberta no roteador (UPnP)"));
                    public_addr = Some(m.external.clone());
                    mapping = Some(m);
                }
                Err(e) => note = Some(format!("não deu para abrir a porta automaticamente: {e}. Libere a porta {port} no roteador/firewall ou informe o endereço público")),
            }
        }
        let published = public_addr.is_some();
        // Informado pela pessoa, o endereço fica como está; senão o nó o vigia (UPnP ou IP local).
        let watched = !informed;
        let addr = public_addr.unwrap_or_else(|| format!("127.0.0.1:{port}"));
        let addr_now = Arc::new(Mutex::new(addr.clone()));
        let mine = RelayInfo { addr, onion, link };
        let Services { client: dht_client, servers, note: dht_note } = dht;
        if let Some(n) = dht_note {
            push_note(&mut note, n);
        }
        let dialer = Dialer { secret: secret.clone(), mine: mine.clone(), relay_port: port, known: known.clone(), hints: hints.clone(), history: history.clone(), seen_seq: Arc::default(), link: phone.link(), dht: dht_client.clone(), dht_circuit: dht_options.as_ref().is_some_and(|o| o.circuit), anchors, trusted: Arc::default() };
        // Os convites levam este relay, para o outro lado ligar de volta.
        let link_to_phone = phone.link();
        let own = Arc::new(Mutex::new(own_relay(&mine, published)));
        link_to_phone.set_relay_hint(relay_hint(&mine, published));
        let update_notice: Arc<Mutex<Option<String>>> = Arc::default();
        let reach = Reachability { onion, link, port, addr: addr_now.clone(), own: own.clone(), phone: link_to_phone, notice: update_notice.clone() };
        let renew = watched.then(|| tokio::spawn(watch_address(reach.clone(), mapping, upnp)));
        let mut background = Vec::new();
        if let Some(mut rx) = phone.take_updates() {
            let (dialer, notice) = (dialer.clone(), update_notice.clone());
            background.push(tokio::spawn(async move {
                while let Some(u) = rx.recv().await {
                    if dialer.apply_update(&u) {
                        let hex: String = u.peer[..4].iter().map(|b| format!("{b:02x}")).collect();
                        *notice.lock_unpoisoned() = Some(format!("{hex}... mudou de endereço e avisou: agora {}", u.record.addr));
                    }
                }
            }));
        }
        let current: Current = Arc::default();
        let publishes = dht_client.is_some() || !dialer.anchors.is_empty();
        if updates || publishes {
            background.push(tokio::spawn(track_address(own.clone(), current.clone())));
        }
        if updates {
            background.push(tokio::spawn(outbox(dialer.clone(), phone.contacts(), current.clone(), update_retry)));
        }
        if publishes {
            background.push(tokio::spawn(rendezvous_publisher(dialer.clone(), phone.contacts(), current.clone())));
        }
        background.extend(servers);
        phone.accept_introductions(introductions);
        let server_hub = ServerHub::start(restore(secret.clone(), saved_servers.as_deref()), dialer.clone(), &phone, own.clone());
        if publishes {
            background.push(tokio::spawn(server_publisher(dialer.clone(), server_hub.clone(), current.clone())));
        }
        let introduced: Arc<Mutex<Vec<Introduced>>> = Arc::default();
        if publishes {
            *phone.custody().lock_unpoisoned() = Custody::import(saved_custody.iter().map(String::as_str), unix_millis() as u64);
            background.push(tokio::spawn(mail_keeper(dialer.clone(), phone.custody())));
            if introductions {
                background.push(tokio::spawn(mail_reader(dialer.clone(), phone.contacts(), introduced.clone(), mail_poll)));
            }
        }
        if let Some(mut rx) = phone.take_introductions() {
            let introduced = introduced.clone();
            background.push(tokio::spawn(async move {
                while let Some(Introduction { peer, intro }) = rx.recv().await {
                    let mut queue = introduced.lock_unpoisoned();
                    // Quem se apresenta de novo (a confirmação se perdeu) não entra duas vezes na fila.
                    queue.retain(|i| i.peer != peer);
                    queue.push(Introduced { peer, name: intro.name, reach: intro.reach });
                }
            }));
        }
        let removed: Arc<Mutex<Vec<[u8; 32]>>> = Arc::default();
        if let Some(mut rx) = phone.take_removals() {
            let removed = removed.clone();
            background.push(tokio::spawn(async move {
                while let Some(peer) = rx.recv().await {
                    let mut queue = removed.lock_unpoisoned();
                    if !queue.contains(&peer) {
                        queue.push(peer);
                    }
                }
            }));
        }
        Ok(Self { secret, name, retry: update_retry, phone, _relay: relay, renew, reach, note, dialer, known, hints, history, own, update_notice, background, addr: addr_now, introduced, introducing: Arc::default(), retiring: Arc::default(), removed, servers: server_hub })
    }

    /// Aceita convites de `keys` além dos contatos.
    pub fn allow(&self, keys: &[[u8; 32]]) {
        let c = self.phone.contacts();
        let mut c = c.lock_unpoisoned();
        for k in keys {
            if !c.contains(k) {
                c.push(*k);
            }
        }
    }

    /// Quem está na minha sala de voz e pode ligar sem ser contato (substitui a lista anterior).
    pub fn set_room_peers(&self, keys: &[[u8; 32]]) {
        *self.phone.room_peers().lock_unpoisoned() = keys.to_vec();
    }

    /// Novo contato: passa a ser atendido e, se o código dele trouxe o relay, esse é o primeiro endereço para ligar a ele.
    pub fn add_contact(&self, peer: [u8; 32], relay: Option<RelayInfo>) {
        if let Some(task) = self.retiring.lock_unpoisoned().remove(&peer) {
            task.abort();
        }
        self.allow(&[peer]);
        if let Some(r) = relay {
            self.add_hint(peer, r);
        }
    }

    /// Apresenta esta pessoa a `peer`, que ela acabou de adicionar: manda, cifrada para ele, a chave (provada pelo
    /// quadro), o nome e o relay deste nó, para ele ter este contato também (veja [`Node::take_introductions`]).
    /// Insiste com espera crescente até a confirmação ou `UPDATE_GIVE_UP` (3 dias); o endereço dele vem do código que se
    /// tem, da DHT ou das âncoras. Chamar de novo para o mesmo contato não duplica a entrega.
    pub fn introduce(&self, peer: [u8; 32]) {
        let mut tasks = self.introducing.lock_unpoisoned();
        if tasks.contains_key(&peer) {
            return;
        }
        let (dialer, own, contacts, name, tasks_done, retry) = (self.dialer.clone(), self.own.clone(), self.phone.contacts(), self.name.clone(), self.introducing.clone(), self.retry);
        tasks.insert(
            peer,
            tokio::spawn(async move {
                let (mut wait, give_up) = (retry, Instant::now() + UPDATE_GIVE_UP);
                let seq = unix_millis() as u64;
                let mut kept: HashSet<[u8; 32]> = HashSet::new();
                loop {
                    if !contacts.lock_unpoisoned().contains(&peer) {
                        break;
                    }
                    // O relay é o de agora (pode ter mudado entre as tentativas); sem endereço que alguém de fora use, espera.
                    let reach = own.lock_unpoisoned().clone().map(|r| update::Reach { addr: r.addr, onion: r.onion, link: r.link });
                    if let Some(reach) = reach {
                        let intro = Intro { seq, name: name.clone(), reach };
                        if dialer.deliver_intro(peer, &intro, INTRO_ACK_WAIT).await {
                            break;
                        }
                        // Sem resposta direta: deixa o convite na caixa dele e com contatos de confiança, que o entregam
                        // quando ele voltar (veja `mail`).
                        dialer.leave_mail(peer, &intro, &contacts, &mut kept).await;
                    }
                    if Instant::now() >= give_up {
                        break;
                    }
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(UPDATE_MAX_WAIT);
                }
                tasks_done.lock_unpoisoned().remove(&peer);
            }),
        );
    }

    /// As apresentações que contatos deixaram aqui para quem estava offline, para o app guardar em disco e passar de
    /// volta em [`NodeConfig::custody`] no próximo início. Cada linha é opaca (hexadecimal).
    #[must_use]
    pub fn custody_export(&self) -> Vec<String> {
        self.phone.custody().lock_unpoisoned().export()
    }

    /// Contatos a quem [`Node::introduce`] ainda não entregou (sem confirmação até agora), para o app guardar e
    /// chamar `introduce` de novo no próximo início.
    #[must_use]
    pub fn pending_introductions(&self) -> Vec<[u8; 32]> {
        self.introducing.lock_unpoisoned().keys().copied().collect()
    }

    /// Quem adicionou esta pessoa e se apresentou desde a última chamada. Cada um já foi confirmado a quem
    /// mandou, mas nada mudou aqui: o app decide se vira contato ([`Node::add_contact`]). Fica vazio com
    /// [`NodeConfig::introductions`] desligado.
    #[must_use]
    pub fn take_introductions(&self) -> Vec<Introduced> {
        std::mem::take(&mut *self.introduced.lock_unpoisoned())
    }

    /// Remove o contato `peer` e avisa a ele (cifrado, só ele abre) para que o tire da lista dele também. Deixa de
    /// atender a chave na hora; o aviso insiste com espera crescente até a confirmação ou `UPDATE_GIVE_UP`, e o
    /// endereço guardado só some quando a entrega termina. Para só esquecer, sem avisar, use [`Node::forget`].
    /// Adicionar o contato de novo ([`Node::add_contact`]) cancela o aviso.
    pub fn retire(&self, peer: [u8; 32]) {
        if let Some(task) = self.introducing.lock_unpoisoned().remove(&peer) {
            task.abort();
        }
        self.phone.contacts().lock_unpoisoned().retain(|k| *k != peer);
        self.dialer.set_trusted(peer, false);
        self.phone.custody().lock_unpoisoned().drop_from(&peer);
        let mut tasks = self.retiring.lock_unpoisoned();
        if tasks.contains_key(&peer) {
            return;
        }
        let (node_dialer, known, hints, history, tasks_done, retry) = (self.dialer.clone(), self.known.clone(), self.hints.clone(), self.history.clone(), self.retiring.clone(), self.retry);
        tasks.insert(
            peer,
            tokio::spawn(async move {
                let (mut wait, give_up) = (retry, Instant::now() + UPDATE_GIVE_UP);
                loop {
                    // Carimbo novo a cada tentativa: quem recebe só aceita o recente.
                    if node_dialer.deliver_removal(peer, unix_millis() as u64, INTRO_ACK_WAIT).await || Instant::now() >= give_up {
                        break;
                    }
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(UPDATE_MAX_WAIT);
                }
                known.lock_unpoisoned().remove(&peer);
                hints.lock_unpoisoned().remove(&peer);
                history.lock_unpoisoned().remove(&peer);
                tasks_done.lock_unpoisoned().remove(&peer);
            }),
        );
    }

    /// Ex-contatos a quem [`Node::retire`] ainda não entregou o aviso, para o app guardar e chamar `retire` de novo no
    /// próximo início.
    #[must_use]
    pub fn pending_removals(&self) -> Vec<[u8; 32]> {
        self.retiring.lock_unpoisoned().keys().copied().collect()
    }

    /// Quem removeu esta pessoa desde a última chamada. Cada um já foi tirado dos contatos do telefone e confirmado
    /// a quem avisou; o app tira da lista dele ([`Node::forget`], sem avisar de volta). Só vale de quem era contato.
    /// Fica vazio com [`NodeConfig::introductions`] desligado.
    #[must_use]
    pub fn take_removals(&self) -> Vec<[u8; 32]> {
        std::mem::take(&mut *self.removed.lock_unpoisoned())
    }

    /// Contato removido: deixa de ser atendido e some o endereço dele guardado (`known` e `hints`).
    pub fn forget(&self, peer: &[u8; 32]) {
        if let Some(task) = self.introducing.lock_unpoisoned().remove(peer) {
            task.abort();
        }
        self.phone.contacts().lock_unpoisoned().retain(|k| k != peer);
        self.phone.custody().lock_unpoisoned().drop_from(peer);
        self.known.lock_unpoisoned().remove(peer);
        self.hints.lock_unpoisoned().remove(peer);
        self.history.lock_unpoisoned().remove(peer);
        self.dialer.set_trusted(*peer, false);
    }

    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        *PublicKey::from(&self.secret).as_bytes()
    }

    /// O app avisa que o endereço público mudou (um nome ou `ip:porta` que os outros alcançam): o código direto, os
    /// convites e o aviso aos contatos passam a usá-lo. O nó já acompanha sozinho o IP aberto por UPnP; isto serve
    /// a quem informou o endereço à mão.
    pub fn set_public_addr(&self, addr: &str) {
        let addr = addr.trim();
        if !addr.is_empty() {
            self.reach.set(addr.to_string(), true);
        }
    }

    /// Endereço do relay como foi informado ou aberto por UPnP (na falta, o local).
    #[must_use]
    pub fn published_addr(&self) -> String {
        self.addr.lock_unpoisoned().clone()
    }

    /// Relay desta máquina como os outros o alcançam (endereço público ou, na falta, o da rede local), para
    /// montar o código de contato direto. `None` se não há endereço que alguém de fora use.
    #[must_use]
    pub fn relay_info(&self) -> Option<RelayInfo> {
        self.own.lock_unpoisoned().clone()
    }

    /// Guarda o relay que `peer` informou (veja [`Dialer::add_hint`]).
    pub fn add_hint(&self, peer: [u8; 32], relay: RelayInfo) {
        self.dialer.add_hint(peer, relay);
    }

    /// Aviso sobre a rede (porta aberta por UPnP, ou por que não foi possível), se houver.
    #[must_use]
    pub fn network_note(&self) -> Option<String> {
        self.note.clone()
    }

    /// Últimos endereços de relay conhecidos por contato, para guardar e devolver em [`NodeConfig::known`].
    #[must_use]
    pub fn known(&self) -> Vec<([u8; 32], RelayInfo)> {
        self.known.lock_unpoisoned().iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Relays que contatos mandaram nos convites e que ainda não foram confirmados por uma chamada, para
    /// guardar e devolver em [`NodeConfig::hints`]. Ligar de volta para quem te ligou funciona sem outro endereço.
    #[must_use]
    pub fn hints(&self) -> Vec<([u8; 32], RelayInfo)> {
        self.hints.lock_unpoisoned().iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Endereços antigos de relay por contato, do mais novo ao mais velho (até [`HISTORY`] por contato), para
    /// guardar e devolver em [`NodeConfig::history`]. Quem mudou de endereço sem avisar ainda é achado por eles.
    #[must_use]
    pub fn history(&self) -> Vec<([u8; 32], RelayInfo)> {
        self.history.lock_unpoisoned().iter().flat_map(|(k, v)| v.iter().map(|r| (*k, r.clone()))).collect()
    }

    /// Motivo de um convite descartado em silêncio (veja [`Phone::take_notice`]), aviso de que um contato
    /// mandou o endereço novo dele ou de que o endereço deste nó mudou, se houve; uma vez só.
    #[must_use]
    pub fn take_notice(&self) -> Option<String> {
        self.phone.take_notice().or_else(|| self.update_notice.lock_unpoisoned().take())
    }

    pub async fn next_incoming(&mut self) -> Option<IncomingCall> {
        let inc = self.phone.next_incoming().await?;
        if let Some(h) = inc.relay_hint() {
            self.hints.lock_unpoisoned().insert(*inc.caller().as_bytes(), RelayInfo { addr: h.addr.to_string(), onion: h.onion, link: h.link });
        }
        Some(inc)
    }

    /// Liga para `peer` (veja [`Dialer::dial`]).
    ///
    /// # Errors
    ///
    /// Veja [`Dialer::dial`].
    pub async fn dial<C: Codec + Send + 'static>(&self, peer: [u8; 32], codec: C, wait: Duration) -> Result<ActiveCall, DialError> {
        self.dialer.dial(peer, codec, wait).await
    }

    /// Transmite a tela da chamada para `peer` (veja [`Dialer::share_screen`]).
    ///
    /// # Errors
    ///
    /// Veja [`Dialer::share_screen`].
    pub async fn share_screen(&self, peer: [u8; 32], call: &ActiveCall, wait: Duration) -> Result<ScreenShare, DialError> {
        self.dialer.share_screen(peer, call, wait).await
    }

    /// Marca `peer` como contato de confiança para a tela sem privacidade (veja [`Dialer::set_trusted`]).
    pub fn set_trusted(&self, peer: [u8; 32], trusted: bool) {
        self.dialer.set_trusted(peer, trusted);
    }

    /// Os servidores e salas desta pessoa: criar, convidar, presença, eventos. Cópia barata.
    #[must_use]
    pub fn servers(&self) -> ServerHub {
        self.servers.clone()
    }

    /// Cópia barata que sabe ligar sem emprestar o `Node`: serve para discar em tarefas em segundo plano.
    #[must_use]
    pub fn dialer(&self) -> Dialer {
        self.dialer.clone()
    }

    /// Para relay, telefone e renovações. Descartar o `Node` faz o mesmo.
    pub fn stop(self) {}
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(t) = &self.renew {
            t.abort();
        }
        for t in self.introducing.lock_unpoisoned().values().chain(self.retiring.lock_unpoisoned().values()) {
            t.abort();
        }
        for t in &self.background {
            t.abort();
        }
        self.servers.stop();
    }
}
