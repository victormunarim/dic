//! Nó: o que cada pessoa roda para ligar e receber chamadas (feature `net`).
//!
//! Um [`Node`] junta o que antes era configurado à mão: um relay (que também serve de último salto para quem
//! liga para você), o telefone ([`Phone`]), a fila que entrega o endereço novo aos contatos e, se ligados, o
//! ponto de encontro na DHT e a âncora. Não há servidor: quem usa informa a identidade e os contatos; para ligar,
//! precisa da chave pública do outro e de um relay dele (o código de contato direto, o último endereço guardado,
//! o aviso que ele mandou, a DHT ou uma âncora).
//!
//! A chamada vai pelos relays dos próprios contatos, de ponta a ponta.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{
    sync::Semaphore,
    task::{AbortHandle, JoinHandle, JoinSet},
};
use rand_core::{OsRng, RngCore};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{
    circuit::{
        manager::{Candidate, CircuitManager, ManagerConfig},
        relay::{RelayConfig, RelayNode},
    },
    upnp,
    circuit::manager::Event,
    media::{
        codec::Codec,
        keys::CallKeys,
        signal::RelayHint,
        update::{self, AddressRecord, Intro, MAX_ADDR},
    },
    phone::{self, ActiveCall, AddressUpdate, CallError, IncomingCall, Introduction, Phone, PhoneLink, ScreenShare, call_linked, open_screen_client_options, screen_client_options, share_screen_slot, OPEN_SCREEN_SLOT_MS, SCREEN_SLOT_MS},
    rendezvous::{self, ANCHOR_DEST, Anchor, DhtOptions, GATEWAY_DEST, Remote, Rendezvous, Signed},
    transport::{CircuitClient, ClientOptions, RelayServer, ServerConfig, allow_list},
};

/// De quanto em quanto tempo o nó renova a porta aberta no roteador (UPnP) e confere se o endereço dele mudou.
const RENEW_EVERY: Duration = Duration::from_secs(40);
/// Sem porta aberta, o UPnP é tentado de novo a cada tantas conferências (o roteador pode ter demorado a subir).
const MAP_RETRY_EVERY: u32 = 3;

/// Nome do destino que o relay de saída mapeia para o telefone de `key`.
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
    pub fn candidate(&self) -> Candidate {
        Candidate { addr: self.addr.clone(), public: self.onion, link_public: self.link, score: 1.0 }
    }
}

/// Quantos endereços antigos de cada contato ficam guardados para tentar depois do atual.
pub const HISTORY: usize = 4;

/// Espera inicial entre as tentativas de entregar o endereço novo a um contato (dobra a cada falha, até 2 h).
pub const UPDATE_RETRY: Duration = Duration::from_secs(30);

/// Maior espera entre duas tentativas de entrega do endereço novo.
const UPDATE_MAX_WAIT: Duration = Duration::from_secs(2 * 3600);

/// Depois disto sem confirmação do contato, a entrega do endereço novo é abandonada.
const UPDATE_GIVE_UP: Duration = Duration::from_secs(3 * 24 * 3600);

/// Quanto esperar a confirmação de um contato depois de entregar o endereço.
const UPDATE_ACK_WAIT: Duration = Duration::from_secs(8);

/// Entregas de endereço feitas ao mesmo tempo (cada uma monta um circuito).
const UPDATE_PARALLEL: usize = 4;

/// De quanto em quanto tempo o endereço é regravado na DHT (os itens somem em cerca de 2 h sem renovação).
const DHT_REPUBLISH: Duration = Duration::from_secs(3600);

/// Depois de uma gravação que falhou na DHT, quanto esperar para tentar de novo.
const DHT_RETRY: Duration = Duration::from_secs(60);

/// Tempo máximo de uma consulta à DHT ao discar (montar o circuito, quando é o caso, e ler os três dias).
const DHT_LOOKUP_LIMIT: Duration = Duration::from_secs(45);

/// Contatos gravados ou consultados na DHT por rodada.
const DHT_CONTACTS: usize = 32;

/// Espera a confirmação de uma apresentação (como [`UPDATE_ACK_WAIT`]).
const INTRO_ACK_WAIT: Duration = Duration::from_secs(8);

/// Porta de relay preferida; se estiver ocupada, o sistema escolhe outra.
pub const DEFAULT_RELAY_PORT: u16 = 4001;

pub struct NodeConfig {
    pub secret: StaticSecret,
    /// Chaves públicas que podem ligar para você. Quem não está na lista não recebe resposta.
    pub contacts: Vec<[u8; 32]>,
    /// Porta TCP do relay (0 = qualquer). Padrão: [`DEFAULT_RELAY_PORT`].
    pub relay_port: u16,
    /// Endereço `host:porta` que os outros devem usar para chegar ao relay (um IP público, um nome DNS, a porta
    /// aberta no roteador). `None`: o que o UPnP abrir ou, na falta, o IP desta máquina na rede local.
    pub public_addr: Option<String>,
    /// Tenta abrir a porta do relay no roteador por UPnP quando nenhum endereço público foi informado. Padrão: sim.
    pub upnp: bool,
    /// Últimos endereços de relay conhecidos por contato (veja [`Node::known`]). Ligar para quem está
    /// aqui funciona enquanto o endereço não mudar.
    pub known: Vec<([u8; 32], RelayInfo)>,
    /// Relays que contatos informaram nos convites que mandaram (veja [`Node::hints`]). Ficam atrás de `known`
    /// na ordem de tentativa, e só viram `known` depois de uma chamada completada por eles.
    pub hints: Vec<([u8; 32], RelayInfo)>,
    /// Segredos do relay (circuito, link). `None` gera chaves novas a cada início, e então o endereço
    /// que os contatos guardaram de você (veja `known`) deixa de valer a cada reinício; gere o par uma vez
    /// com [`RelayKeys::random`], guarde-o e passe-o aqui.
    pub relay_keys: Option<RelayKeys>,
    /// Endereços antigos de relay por contato, do mais novo ao mais velho (veja [`Node::history`]). Ligar
    /// tenta estes depois do atual, para quem mudou de endereço sem avisar.
    pub history: Vec<([u8; 32], RelayInfo)>,
    /// Entrega o endereço deste relay aos contatos, cifrado, no endereço que se tem deles (veja
    /// [`crate::media::update`]), ao iniciar e sempre que o endereço mudar. Padrão: ligado.
    pub updates: bool,
    /// Espera inicial entre tentativas de entrega a um contato que não responde. Padrão: [`UPDATE_RETRY`].
    pub update_retry: Duration,
    /// Guarda o endereço deste relay na DHT do BitTorrent, cifrado para cada contato, e a consulta ao ligar
    /// para quem mudou de endereço sem avisar (veja [`crate::rendezvous`]). `None`: desligado, que é o padrão.
    pub dht: Option<DhtOptions>,
    /// Como esta pessoa quer ser chamada nas apresentações que manda ([`Node::introduce`]); vazio: sem nome.
    pub name: String,
    /// Aceita apresentações de quem adicionou esta pessoa (veja [`Node::take_introductions`]). Padrão: sim.
    pub introductions: bool,
    /// Serve de âncora: guarda os registros de endereço dos outros nós (veja [`crate::rendezvous`]). Padrão: não.
    pub anchor: bool,
    /// Âncoras onde este nó grava o endereço e consulta o dos contatos, em ordem de preferência. Padrão: nenhuma.
    pub anchors: Vec<RelayInfo>,
}

/// Segredos do relay do nó: o do circuito (`onion`) e o do link (Noise NK).
#[derive(Clone)]
pub struct RelayKeys {
    pub onion: StaticSecret,
    pub link: StaticSecret,
}

impl RelayKeys {
    pub fn random() -> Self {
        Self { onion: StaticSecret::random_from_rng(rand_core::OsRng), link: StaticSecret::random_from_rng(rand_core::OsRng) }
    }
}

impl NodeConfig {
    pub fn new(secret: StaticSecret, contacts: Vec<[u8; 32]>) -> Self {
        Self { secret, contacts, relay_port: DEFAULT_RELAY_PORT, public_addr: None, upnp: true, known: Vec::new(), hints: Vec::new(), relay_keys: None, history: Vec::new(), updates: true, update_retry: UPDATE_RETRY, dht: None, name: String::new(), introductions: true, anchor: false, anchors: Vec::new() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DialError {
    #[error("{}", if *.tried_stored { "o endereço guardado do contato não respondeu e não há outro (peça um código de contato novo)" } else { "sem endereço desse contato: use o código de contato direto dele" })]
    NoAddress { tried_stored: bool },
    #[error("{0}")]
    Call(#[from] CallError),
}

/// Quem liga para os outros: guarda o que o [`Node`] usa para discar e pode ser clonado para tarefas.
#[derive(Clone)]
pub struct Dialer {
    secret: StaticSecret,
    mine: RelayInfo,
    relay_port: u16,
    known: Known,
    /// Relays que contatos mandaram nos convites (veja [`Node::hints`]): última opção antes da DHT e das âncoras.
    hints: Known,
    /// Endereços antigos dos contatos (veja [`Node::history`]): tentados depois do guardado e da dica.
    history: History,
    /// Último `seq` aceito de cada contato: atualização igual ou mais velha é ignorada.
    seen_seq: Arc<Mutex<HashMap<[u8; 32], u64>>>,
    /// Registra as chamadas que esta máquina faz no telefone dela (para receber a tela do outro lado).
    link: PhoneLink,
    /// DHT onde os contatos deixam o endereço novo (veja [`NodeConfig::dht`]).
    dht: Option<Arc<Rendezvous>>,
    /// Fala com a DHT pelos relays de dois contatos em vez do próprio IP (veja [`DhtOptions::circuit`]).
    dht_circuit: bool,
    /// Âncoras do usuário (veja [`NodeConfig::anchors`]).
    anchors: Vec<RelayInfo>,
}

/// Quem adicionou esta pessoa e se apresentou ([`Node::introduce`] do outro lado): ainda não é contato.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Introduced {
    pub peer: [u8; 32],
    pub name: String,
    pub reach: update::Reach,
}

/// O registro do endereço atual deste relay (veja [`track_address`]); vazio até o endereço ficar conhecido.
type Current = Arc<Mutex<Option<AddressRecord>>>;

type Known = Arc<Mutex<HashMap<[u8; 32], RelayInfo>>>;

/// Endereços antigos por contato, o mais novo primeiro (até [`HISTORY`]).
type History = Arc<Mutex<HashMap<[u8; 32], Vec<RelayInfo>>>>;

/// `relay` passa a ser o endereço guardado de `peer`; o que estava guardado vai para o histórico.
fn set_known(known: &Known, history: &History, peer: &[u8; 32], relay: RelayInfo) {
    let old = known.lock().unwrap().insert(*peer, relay.clone());
    let mut history = history.lock().unwrap();
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

/// Relay a informar nos convites: o endereço público informado ou aberto por UPnP ou, na falta dele
/// (`published` falso: `mine.addr` é só o de loopback inventado aqui), o IP desta máquina na rede
/// local. `None` se não houver endereço que alguém de fora use.
fn relay_hint(mine: &RelayInfo, published: bool) -> Option<RelayHint> {
    let addr: SocketAddr = mine.addr.parse().ok()?;
    let addr = if !published && (addr.ip().is_loopback() || addr.ip().is_unspecified()) { SocketAddr::new(local_ip()?, addr.port()) } else { addr };
    Some(RelayHint { addr, onion: mine.onion, link: mine.link })
}

/// Relay para o código de contato direto: o endereço publicado como está (pode ser um nome, como
/// `casa.exemplo.com:4001`) ou, não publicado, o IP desta máquina na rede local (veja [`relay_hint`]).
fn own_relay(mine: &RelayInfo, published: bool) -> Option<RelayInfo> {
    if published {
        return Some(mine.clone());
    }
    relay_hint(mine, false).map(|h| RelayInfo { addr: h.addr.to_string(), onion: mine.onion, link: mine.link })
}

/// IP desta máquina na rota para fora. O "connect" de UDP só escolhe a rota: nenhum pacote sai.
fn unix_millis() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

fn unix_secs() -> u64 {
    (unix_millis() / 1000) as u64
}

fn local_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

/// Acrescenta um aviso à nota de rede, separando por `; `.
fn push_note(note: &mut Option<String>, add: String) {
    *note = Some(match note.take() {
        Some(n) => format!("{n}; {add}"),
        None => add,
    });
}

/// Embaralha `list` (Fisher-Yates com o gerador do sistema).
fn shuffle<T>(list: &mut [T]) {
    for i in (1..list.len()).rev() {
        list.swap(i, OsRng.next_u32() as usize % (i + 1));
    }
}

/// Espera o circuito do cliente ficar pronto, até `limit`.
async fn ready_within(client: &CircuitClient, limit: Duration) -> bool {
    tokio::time::timeout(limit, async {
        loop {
            if client.status().await.is_some_and(|s| s.active.is_some_and(|a| a.ready)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .is_ok()
}

impl Dialer {
    /// Endereço de relay guardado de `peer` (da última ligação ou aviso, ou, na falta, do convite que ele
    /// mandou), se houver; é por ele que [`Dialer::dial`] tenta primeiro.
    pub fn stored_address(&self, peer: &[u8; 32]) -> Option<String> {
        self.stored_for(peer).first().map(|r| r.addr.clone())
    }

    /// Relays de `peer` para tentar antes da DHT e das âncoras, em ordem: o guardado de uma ligação ou aviso, o
    /// que o convite dele trouxe (que ninguém verificou) e os endereços antigos, do mais novo ao mais velho.
    fn stored_for(&self, peer: &[u8; 32]) -> Vec<RelayInfo> {
        let mut v: Vec<RelayInfo> = self.known.lock().unwrap().get(peer).cloned().into_iter().collect();
        if let Some(h) = self.hints.lock().unwrap().get(peer)
            && !v.contains(h)
        {
            v.push(h.clone());
        }
        for old in self.history.lock().unwrap().get(peer).into_iter().flatten() {
            if !v.contains(old) {
                v.push(old.clone());
            }
        }
        v
    }

    /// Guarda o relay que `peer` informou (o código de contato direto) como dica: é o
    /// primeiro endereço que [`Dialer::dial`] tenta e só vira endereço de confiança depois de uma chamada completa.
    pub fn add_hint(&self, peer: [u8; 32], relay: RelayInfo) {
        self.hints.lock().unwrap().insert(peer, relay);
    }

    /// Primeiro relay de `stored` cujo circuito fica pronto em 5 s, com o cliente já aberto.
    /// Endereço velho (relay sumiu, ou outra máquina no mesmo IP) nunca completa o handshake.
    async fn stored_client(&self, peer: &[u8; 32], stored: Vec<RelayInfo>, options: fn() -> ClientOptions) -> Option<(CircuitClient, RelayInfo)> {
        for k in stored {
            let client = self.client_with(peer, &k, Vec::new(), options());
            if ready_within(&client, Duration::from_secs(5)).await {
                return Some((client, k));
            }
            client.stop();
        }
        None
    }

    /// Uma chamada completou por `used`: passa a ser o endereço guardado de `peer`. É aqui que uma dica de
    /// convite (sem prova de quem a mandou) vira endereço de confiança.
    fn remember(&self, peer: &[u8; 32], used: RelayInfo) {
        let mut hints = self.hints.lock().unwrap();
        if hints.get(peer) == Some(&used) {
            hints.remove(peer);
        }
        drop(hints);
        set_known(&self.known, &self.history, peer, used);
    }

    /// Aplica o endereço novo que `update.peer` mandou (já autenticado pelo telefone). `false`: o `seq` não é
    /// mais novo que o último visto e nada muda. O endereço antigo vai para o histórico e a dica de convite,
    /// que não era autenticada, perde o lugar.
    fn apply_update(&self, update: &AddressUpdate) -> bool {
        {
            let mut seen = self.seen_seq.lock().unwrap();
            if seen.get(&update.peer).is_some_and(|s| *s >= update.record.seq) {
                return false;
            }
            seen.insert(update.peer, update.record.seq);
        }
        let r = &update.record;
        self.hints.lock().unwrap().remove(&update.peer);
        set_known(&self.known, &self.history, &update.peer, RelayInfo { addr: r.addr.clone(), onion: r.onion, link: r.link });
        true
    }

    /// Entrega `record` a `peer` no endereço que se tem dele (o guardado, a dica e os antigos) e espera a
    /// confirmação cifrada. `true`: o contato abriu o registro. Quem confirmou prova o endereço, que passa a ser o guardado.
    async fn deliver_update(&self, peer: [u8; 32], record: &AddressRecord, wait: Duration) -> bool {
        let stored = self.stored_for(&peer);
        let pk = PublicKey::from(peer);
        let Ok(frame) = update::seal_update(&self.secret, &pk, record) else { return false };
        let Some((mut client, used)) = self.stored_client(&peer, stored, ClientOptions::default).await else { return false };
        let acked = client.send(&frame).await.is_ok()
            && tokio::time::timeout(wait, async {
                loop {
                    match client.next_event().await {
                        Some(Event::Data(d)) if update::open_ack(&self.secret, &pk, record.seq, &d) => return true,
                        Some(_) => {}
                        None => return false,
                    }
                }
            })
            .await
            .unwrap_or(false);
        client.stop();
        if acked {
            self.remember(&peer, used);
        }
        acked
    }

    /// Entrega a apresentação `intro` a `peer` e espera a confirmação cifrada. O endereço dele vem do código que
    /// se tem (o guardado ou a dica), da DHT ou das âncoras. `true`: ele abriu e confirmou.
    async fn deliver_intro(&self, peer: [u8; 32], intro: &Intro, wait: Duration) -> bool {
        let pk = PublicKey::from(peer);
        let Ok(frame) = update::seal_intro(&self.secret, &pk, intro) else { return false };
        let (found, _) = self.reachable_client(&peer, ClientOptions::default).await;
        let Some((mut client, used)) = found else { return false };
        let acked = ready_within(&client, wait).await
            && client.send(&frame).await.is_ok()
            && tokio::time::timeout(wait, async {
                loop {
                    match client.next_event().await {
                        Some(Event::Data(d)) if update::open_ack(&self.secret, &pk, intro.seq, &d) => return true,
                        Some(_) => {}
                        None => return false,
                    }
                }
            })
            .await
            .unwrap_or(false);
        client.stop();
        if acked {
            self.remember(&peer, used);
        }
        acked
    }

    /// Relays de contatos para montar o circuito até um gateway da DHT, sem o
    /// deste nó e sem os dos contatos em `exclude`.
    fn gateway_pool(&self, exclude: &[[u8; 32]]) -> Vec<RelayInfo> {
        let mut found: Vec<([u8; 32], RelayInfo)> = self.known.lock().unwrap().iter().map(|(k, r)| (*k, r.clone())).collect();
        found.extend(self.hints.lock().unwrap().iter().map(|(k, r)| (*k, r.clone())));
        for (k, list) in self.history.lock().unwrap().iter() {
            found.extend(list.iter().map(|r| (*k, r.clone())));
        }
        let mut pool: Vec<RelayInfo> = Vec::new();
        let mut add = |r: RelayInfo| {
            if r.addr != self.mine.addr && !pool.iter().any(|p| p.addr == r.addr) {
                pool.push(r);
            }
        };
        for (k, r) in found {
            if !exclude.contains(&k) {
                add(r);
            }
        }
        pool
    }

    /// Sessão com um gateway da DHT por um circuito de dois relays que não são deste nó: o primeiro salto vê
    /// o IP e não o pedido, a saída vê o pedido e não o IP. `None`: não há dois relays, ou nenhuma saída serve de gateway.
    async fn gateway(&self, exclude: &[[u8; 32]]) -> Option<Remote> {
        let mut pool = self.gateway_pool(exclude);
        if pool.len() < 2 {
            return None;
        }
        shuffle(&mut pool);
        for n in 0..pool.len().min(3) {
            let exit = pool[n].clone();
            let relays: Vec<Candidate> = pool.iter().filter(|r| r.addr != exit.addr).take(3).map(RelayInfo::candidate).collect();
            if let Some(remote) = self.remote_to(GATEWAY_DEST, &exit, relays).await {
                return Some(remote);
            }
        }
        None
    }

    /// Sessão com o serviço `dest` da saída `exit` por um circuito que passa por `relays` antes dela.
    async fn remote_to(&self, dest: &str, exit: &RelayInfo, mut relays: Vec<Candidate>) -> Option<Remote> {
        relays.push(exit.candidate());
        let cfg = ManagerConfig { exit_addr: Some(exit.addr.clone()), ..Default::default() };
        let client = CircuitClient::spawn_with(CircuitManager::new(cfg, dest, relays), Duration::from_secs(3), ClientOptions::default());
        if ready_within(&client, Duration::from_secs(8)).await {
            return Some(Remote::new(client));
        }
        client.stop();
        None
    }

    /// Sessão com a âncora `anchor`, por relays de contatos e, na falta deles, pelo relay deste nó (por
    /// loopback): nesse caso a âncora vê o IP de quem pergunta.
    async fn anchor_remote(&self, anchor: &RelayInfo, exclude: &[[u8; 32]]) -> Option<Remote> {
        let mut pool = self.gateway_pool(exclude);
        pool.retain(|r| r.addr != anchor.addr);
        shuffle(&mut pool);
        let mut relays: Vec<Candidate> = pool.iter().take(2).map(RelayInfo::candidate).collect();
        relays.push(Candidate { addr: format!("127.0.0.1:{}", self.relay_port), public: self.mine.onion, link_public: self.mine.link, score: 0.5 });
        self.remote_to(ANCHOR_DEST, anchor, relays).await
    }

    /// Valores guardados nas âncoras para os alvos `(chave, sal)`: todos os que alguma âncora devolveu.
    async fn anchor_values(&self, targets: &[([u8; 32], [u8; 32])], exclude: &[[u8; 32]]) -> Vec<Vec<u8>> {
        let sessions = futures::future::join_all(self.anchors.iter().map(|a| async move {
            let mut found = Vec::new();
            if let Some(mut remote) = self.anchor_remote(a, exclude).await {
                for (key, salt) in targets {
                    match remote.get(key, salt).await {
                        Ok(Some(value)) => found.push(value),
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
                remote.stop();
            }
            found
        }))
        .await;
        sessions.into_iter().flatten().collect()
    }

    /// Grava os itens em cada âncora; um `bool` por item: `true` se alguma âncora guardou.
    async fn anchor_put(&self, items: &[Signed]) -> Vec<bool> {
        let sessions = futures::future::join_all(self.anchors.iter().map(|a| async move {
            let mut done = vec![false; items.len()];
            if let Some(mut remote) = self.anchor_remote(a, &[]).await {
                for (i, item) in items.iter().enumerate() {
                    match remote.put(item).await {
                        Ok(ok) => done[i] = ok,
                        Err(_) => break,
                    }
                }
                remote.stop();
            }
            done
        }))
        .await;
        (0..items.len()).map(|i| sessions.iter().any(|s| s[i])).collect()
    }

    /// Grava os itens na DHT e nas âncoras ao mesmo tempo; um `bool` por item: `true` se algum lugar guardou.
    async fn store_put(&self, items: &[Signed]) -> Vec<bool> {
        let (dht, anchors) = futures::future::join(self.dht_put(items), self.anchor_put(items)).await;
        dht.into_iter().zip(anchors).map(|(a, b)| a || b).collect()
    }

    /// Lê os alvos `(chave, sal)` da DHT, um valor (ou `None`) por alvo, direto ou pelo circuito conforme a
    /// opção. Sem caminho pelo circuito não lê nada: nunca cai para o acesso direto.
    async fn dht_get(&self, targets: &[([u8; 32], [u8; 32])], exclude: &[[u8; 32]]) -> Vec<Option<Vec<u8>>> {
        let Some(dht) = &self.dht else { return vec![None; targets.len()] };
        if !self.dht_circuit {
            return futures::future::join_all(targets.iter().map(|(key, salt)| dht.get(key, salt))).await;
        }
        let mut out = vec![None; targets.len()];
        if let Some(mut remote) = self.gateway(exclude).await {
            for (i, (key, salt)) in targets.iter().enumerate() {
                match remote.get(key, salt).await {
                    Ok(value) => out[i] = value,
                    Err(_) => break,
                }
            }
            remote.stop();
        }
        out
    }

    /// Grava os itens na DHT, direto ou pelo circuito; um `bool` por item.
    async fn dht_put(&self, items: &[Signed]) -> Vec<bool> {
        let Some(dht) = &self.dht else { return vec![false; items.len()] };
        if !self.dht_circuit {
            return futures::future::join_all(items.iter().map(|item| async move { dht.put(item).await.is_ok() })).await;
        }
        let mut out = vec![false; items.len()];
        if let Some(mut remote) = self.gateway(&[]).await {
            for (i, item) in items.iter().enumerate() {
                match remote.put(item).await {
                    Ok(done) => out[i] = done,
                    Err(_) => break,
                }
            }
            remote.stop();
        }
        out
    }

    /// O endereço mais novo que `peer` deixou para nós na DHT e nas âncoras: consulta o dia anterior, o atual e o
    /// seguinte (o relógio de um dos dois pode estar errado) e fica com o registro de maior `seq`.
    async fn rendezvous_record(&self, peer: &[u8; 32]) -> Option<AddressRecord> {
        if self.dht.is_none() && self.anchors.is_empty() {
            return None;
        }
        let pk = PublicKey::from(*peer);
        let today = unix_secs() / 86400;
        let targets: Vec<([u8; 32], [u8; 32])> = [today, today.saturating_sub(1), today + 1]
            .into_iter()
            .filter_map(|period| update::rendezvous(&self.secret, &pk, &pk, period))
            .map(|(seed, salt)| (rendezvous::public_key(&seed), salt))
            .collect();
        let exclude = [*peer];
        let lookup = futures::future::join(self.dht_get(&targets, &exclude), self.anchor_values(&targets, &exclude));
        let (dht, anchors) = tokio::time::timeout(DHT_LOOKUP_LIMIT, lookup).await.ok()?;
        dht.into_iter().flatten().chain(anchors).filter_map(|v| update::open_record(&self.secret, &pk, &v)).max_by_key(|r| r.seq)
    }

    /// Primeiro relay de `peer` cujo circuito fica pronto: os guardados e, se nenhum responder, o que o
    /// contato deixou na DHT ou nas âncoras. O `bool` diz se algum endereço foi tentado (muda o motivo do erro de quem liga).
    async fn reachable_client(&self, peer: &[u8; 32], options: fn() -> ClientOptions) -> (Option<(CircuitClient, RelayInfo)>, bool) {
        let stored = self.stored_for(peer);
        let mut tried = !stored.is_empty();
        if let Some(found) = self.stored_client(peer, stored.clone(), options).await {
            return (Some(found), tried);
        }
        if let Some(record) = self.rendezvous_record(peer).await {
            let relay = RelayInfo { addr: record.addr.clone(), onion: record.onion, link: record.link };
            self.apply_update(&AddressUpdate { peer: *peer, record });
            if !stored.contains(&relay) {
                tried = true;
                if let Some(found) = self.stored_client(peer, vec![relay], options).await {
                    return (Some(found), tried);
                }
            }
        }
        (None, tried)
    }

    fn client_with(&self, peer: &[u8; 32], exit: &RelayInfo, mut relays: Vec<Candidate>, options: ClientOptions) -> CircuitClient {
        // Último recurso para o primeiro salto: o próprio relay (por loopback). Perde o anonimato
        // perante o outro lado, mas permite ligar quando só há duas pessoas online.
        relays.push(Candidate { addr: format!("127.0.0.1:{}", self.relay_port), public: self.mine.onion, link_public: self.mine.link, score: 0.5 });
        relays.push(exit.candidate());
        let cfg = ManagerConfig { exit_addr: Some(exit.addr.clone()), ..Default::default() };
        CircuitClient::spawn_with(CircuitManager::new(cfg, &dest_for(peer), relays), Duration::from_secs(3), options)
    }

    pub fn public_key(&self) -> [u8; 32] {
        *PublicKey::from(&self.secret).as_bytes()
    }

    /// Liga para `peer`: acha um relay dele (o guardado, o que ele avisou, a DHT ou uma âncora) e chama por um
    /// circuito que passa pelos relays dos contatos.
    pub async fn dial<C: Codec + Send + 'static>(&self, peer: [u8; 32], codec: C, wait: Duration) -> Result<ActiveCall, DialError> {
        let (found, tried_stored) = self.reachable_client(&peer, ClientOptions::default).await;
        let Some((client, k)) = found else { return Err(DialError::NoAddress { tried_stored }) };
        let call = call_linked(client, &self.secret, &PublicKey::from(peer), codec, wait, Some(&self.link)).await?;
        self.remember(&peer, k);
        Ok(call)
    }

    /// Transmite a tela da chamada `call` para `peer`, por um circuito só da tela (cover de
    /// [`phone::SCREEN_SLOT_MS`] ms). Sempre por circuito, com cover.
    pub async fn share_screen(&self, peer: [u8; 32], call: &ActiveCall, wait: Duration) -> Result<ScreenShare, DialError> {
        self.share_screen_keys(peer, call.share_keys(), wait).await
    }

    /// Como [`Self::share_screen`], com as chaves em vez da chamada (quem não tem mais a chamada em mãos
    /// guarda uma cópia de [`ActiveCall::share_keys`]).
    pub async fn share_screen_keys(&self, peer: [u8; 32], keys: &CallKeys, wait: Duration) -> Result<ScreenShare, DialError> {
        self.share_screen_keys_with(peer, keys, wait, true).await
    }

    /// Como [`Self::share_screen_keys`]. Com `private` falso o circuito abre mão do cover: não manda pacote
    /// nos instantes sem vídeo (quem observa a rede vê quando a tela se mexe) e o ritmo máximo dobra
    /// ([`OPEN_SCREEN_SLOT_MS`]). O vídeo e as chaves seguem cifrados. Quem transmite deve usar
    /// [`H264Encoder::bitrate_for`](crate::video::H264Encoder::bitrate_for) com o mesmo intervalo.
    pub async fn share_screen_keys_with(&self, peer: [u8; 32], keys: &CallKeys, wait: Duration, private: bool) -> Result<ScreenShare, DialError> {
        let (options, slot_ms): (fn() -> ClientOptions, u64) = if private { (screen_client_options, SCREEN_SLOT_MS) } else { (open_screen_client_options, OPEN_SCREEN_SLOT_MS) };
        let (found, tried_stored) = self.reachable_client(&peer, options).await;
        let Some((client, used)) = found else { return Err(DialError::NoAddress { tried_stored }) };
        let share = share_screen_slot(client, keys, wait, slot_ms).await?;
        self.remember(&peer, used);
        Ok(share)
    }
}

/// O endereço pelo qual os outros chegam a este relay, que pode mudar com o programa aberto (o provedor troca
/// o IP, a máquina troca de rede). Quem muda o endereço passa por aqui: o código de contato direto, o que os
/// convites levam e o aviso aos contatos (`track_address`) seguem o novo.
#[derive(Clone)]
struct Reachability {
    onion: [u8; 32],
    link: [u8; 32],
    port: u16,
    addr: Arc<Mutex<String>>,
    own: Arc<Mutex<Option<RelayInfo>>>,
    phone: PhoneLink,
    notice: Arc<Mutex<Option<String>>>,
}

impl Reachability {
    /// Passa a usar `addr` (`published`: alguém de fora o alcança; falso, é o IP da rede local). Devolve se mudou.
    fn set(&self, addr: String, published: bool) -> bool {
        let mine = RelayInfo { addr: addr.clone(), onion: self.onion, link: self.link };
        let own = own_relay(&mine, published);
        let mut now = self.own.lock().unwrap();
        if *now == own {
            return false;
        }
        if let Some(r) = &own {
            *self.notice.lock().unwrap() = Some(format!("seu endereço mudou: agora {}", r.addr));
        }
        *now = own;
        *self.addr.lock().unwrap() = addr;
        self.phone.set_relay_hint(relay_hint(&mine, published));
        true
    }
}

/// Renova a porta no roteador e confere o endereço do nó de tempos em tempos: com a porta aberta por UPnP, o IP
/// externo do roteador; sem ela, tenta abrir de novo e acompanha o IP da máquina na rede local. Só roda quando o
/// endereço público não foi informado (um endereço informado é do usuário, que o corrige se precisar).
async fn watch_address(reach: Reachability, mut mapping: Option<upnp::Mapping>, upnp_on: bool) {
    let mut round = 0u32;
    loop {
        tokio::time::sleep(RENEW_EVERY).await;
        round += 1;
        if let Some(m) = &mapping {
            let _ = m.renew().await;
            // Roteador sem resposta ou IP que ninguém de fora alcança: fica o endereço de antes.
            if let Ok(addr) = m.external_addr().await {
                reach.set(addr, true);
            }
            continue;
        }
        if upnp_on
            && round.is_multiple_of(MAP_RETRY_EVERY)
            && let Ok(m) = upnp::map(reach.port).await
        {
            reach.set(m.external.clone(), true);
            mapping = Some(m);
            continue;
        }
        reach.set(format!("127.0.0.1:{}", reach.port), false);
    }
}

/// Mantém em `current` o registro do endereço deste relay: um `seq` novo (a hora em ms, nunca menor que o
/// anterior) cada vez que o endereço muda. A fila de saída e a gravação na DHT entregam o mesmo registro,
/// então o contato vê o mesmo `seq` por qualquer caminho e descarta o que não for mais novo.
async fn track_address(own: Arc<Mutex<Option<RelayInfo>>>, current: Current) {
    let mut seq = 0u64;
    loop {
        let mine = own.lock().unwrap().clone().filter(|m| (1..=MAX_ADDR).contains(&m.addr.len()));
        if let Some(mine) = mine {
            let changed = current.lock().unwrap().as_ref().is_none_or(|c| c.addr != mine.addr || c.onion != mine.onion || c.link != mine.link);
            if changed {
                seq = (unix_millis() as u64).max(seq + 1);
                *current.lock().unwrap() = Some(AddressRecord { seq, addr: mine.addr, onion: mine.onion, link: mine.link });
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Fila de saída: entrega o registro de `current` a cada contato, no endereço que se tem dele, e insiste com
/// espera crescente até a confirmação (ou [`UPDATE_GIVE_UP`]). Um registro novo troca o de todos.
async fn outbox(dialer: Dialer, contacts: phone::Contacts, current: Current, retry: Duration) {
    let slots = Arc::new(Semaphore::new(UPDATE_PARALLEL));
    let mut tasks = JoinSet::new();
    let mut live: HashMap<[u8; 32], AbortHandle> = HashMap::new();
    let mut announced = 0u64;
    loop {
        let record = current.lock().unwrap().clone();
        if let Some(record) = record {
            if record.seq != announced {
                for h in live.values() {
                    h.abort();
                }
                live.clear();
                announced = record.seq;
            }
            let keys: Vec<[u8; 32]> = contacts.lock().unwrap().clone();
            live.retain(|k, _| keys.contains(k));
            for k in keys {
                live.entry(k).or_insert_with(|| tasks.spawn(push_address(dialer.clone(), contacts.clone(), k, record.clone(), retry, slots.clone())));
            }
        }
        while tasks.try_join_next().is_some() {}
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Grava o registro de `current` na DHT e nas âncoras para cada contato: ao mudar o endereço, ao entrar um contato novo, ao
/// virar o dia (o alvo muda) e a cada [`DHT_REPUBLISH`]. Uma gravação que falha é refeita em [`DHT_RETRY`].
async fn rendezvous_publisher(dialer: Dialer, contacts: phone::Contacts, current: Current) {
    let (secret, me) = (dialer.secret.clone(), PublicKey::from(&dialer.secret));
    // Por contato: o `seq` do registro e o dia gravados, e quando gravar de novo.
    let mut state: HashMap<[u8; 32], (u64, u64, Instant)> = HashMap::new();
    let mut put_seq = 0i64;
    loop {
        let record = current.lock().unwrap().clone();
        if let Some(record) = record {
            let keys: Vec<[u8; 32]> = contacts.lock().unwrap().iter().take(DHT_CONTACTS).copied().collect();
            state.retain(|k, _| keys.contains(k));
            let period = unix_secs() / 86400;
            let due: Vec<[u8; 32]> = keys.into_iter().filter(|k| state.get(k).is_none_or(|(seq, day, at)| *seq != record.seq || *day != period || Instant::now() >= *at)).collect();
            // A DHT recusa o mesmo `seq` com outro valor, e cada cifra sai com nonce novo: cada gravação leva um `seq` novo.
            let base = (unix_millis() as i64).max(put_seq + 1);
            put_seq = base + due.len() as i64;
            // Assinado aqui: a semente não sai do nó, nem quando a gravação vai por um gateway.
            let mut signed = Vec::new();
            let mut sent: Vec<[u8; 32]> = Vec::new();
            for (i, k) in due.iter().enumerate() {
                let pk = PublicKey::from(*k);
                let Some((seed, salt)) = update::rendezvous(&secret, &pk, &me, period) else { continue };
                let Ok(value) = update::seal_record(&secret, &pk, &record) else { continue };
                signed.push(rendezvous::sign(&seed, &salt, &value, base + i as i64));
                sent.push(*k);
            }
            let done = if signed.is_empty() { Vec::new() } else { dialer.store_put(&signed).await };
            for (k, ok) in sent.into_iter().zip(done) {
                let wait = if ok { DHT_REPUBLISH } else { DHT_RETRY };
                state.insert(k, (record.seq, period, Instant::now() + wait));
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Entrega `record` a `peer` até ele confirmar, o contato sair da lista ou a entrega ser abandonada.
async fn push_address(dialer: Dialer, contacts: phone::Contacts, peer: [u8; 32], record: AddressRecord, retry: Duration, slots: Arc<Semaphore>) {
    let (mut wait, give_up) = (retry, Instant::now() + UPDATE_GIVE_UP);
    loop {
        if !contacts.lock().unwrap().contains(&peer) {
            return;
        }
        let acked = {
            let Ok(_slot) = slots.acquire().await else { return };
            dialer.deliver_update(peer, &record, UPDATE_ACK_WAIT).await
        };
        if acked || Instant::now() >= give_up {
            return;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(UPDATE_MAX_WAIT);
    }
}

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
    /// Tarefas de fundo: a que aplica os avisos de endereço dos contatos e a fila de saída dos nossos.
    background: Vec<JoinHandle<()>>,
}

impl Node {
    /// Sobe relay e telefone. Recebe de quem tem o seu endereço (o código de contato direto, ou o aviso que
    /// você mandou) e liga para quem está em `known`, em `hints` ou na DHT e nas âncoras.
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
        let NodeConfig { secret, public_addr, upnp, known, hints, history, updates, update_retry, dht: dht_options, name, introductions, anchors, .. } = cfg;
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
        let dialer = Dialer { secret: secret.clone(), mine: mine.clone(), relay_port: port, known: known.clone(), hints: hints.clone(), history: history.clone(), seen_seq: Arc::default(), link: phone.link(), dht: dht_client.clone(), dht_circuit: dht_options.as_ref().is_some_and(|o| o.circuit), anchors };
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
                        *notice.lock().unwrap() = Some(format!("{hex}... mudou de endereço e avisou: agora {}", u.record.addr));
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
            background.push(tokio::spawn(rendezvous_publisher(dialer.clone(), phone.contacts(), current)));
        }
        background.extend(servers);
        phone.accept_introductions(introductions);
        let introduced: Arc<Mutex<Vec<Introduced>>> = Arc::default();
        if let Some(mut rx) = phone.take_introductions() {
            let introduced = introduced.clone();
            background.push(tokio::spawn(async move {
                while let Some(Introduction { peer, intro }) = rx.recv().await {
                    let mut queue = introduced.lock().unwrap();
                    // Quem se apresenta de novo (a confirmação se perdeu) não entra duas vezes na fila.
                    queue.retain(|i| i.peer != peer);
                    queue.push(Introduced { peer, name: intro.name, reach: intro.reach });
                }
            }));
        }
        Ok(Self { secret, name, retry: update_retry, phone, _relay: relay, renew, reach, note, dialer, known, hints, history, own, update_notice, background, addr: addr_now, introduced, introducing: Arc::default() })
    }

    /// Aceita convites de `keys` além dos contatos.
    pub fn allow(&self, keys: &[[u8; 32]]) {
        let c = self.phone.contacts();
        let mut c = c.lock().unwrap();
        for k in keys {
            if !c.contains(k) {
                c.push(*k);
            }
        }
    }

    /// Novo contato: passa a ser atendido e, se o código dele trouxe o relay, esse é o primeiro endereço para ligar a ele.
    pub fn add_contact(&self, peer: [u8; 32], relay: Option<RelayInfo>) {
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
        let mut tasks = self.introducing.lock().unwrap();
        if tasks.contains_key(&peer) {
            return;
        }
        let (dialer, own, contacts, name, tasks_done, retry) = (self.dialer.clone(), self.own.clone(), self.phone.contacts(), self.name.clone(), self.introducing.clone(), self.retry);
        tasks.insert(
            peer,
            tokio::spawn(async move {
                let (mut wait, give_up) = (retry, Instant::now() + UPDATE_GIVE_UP);
                let seq = unix_millis() as u64;
                loop {
                    if !contacts.lock().unwrap().contains(&peer) {
                        break;
                    }
                    // O relay é o de agora (pode ter mudado entre as tentativas); sem endereço que alguém de fora use, espera.
                    let reach = own.lock().unwrap().clone().map(|r| update::Reach { addr: r.addr, onion: r.onion, link: r.link });
                    if let Some(reach) = reach
                        && dialer.deliver_intro(peer, &Intro { seq, name: name.clone(), reach }, INTRO_ACK_WAIT).await
                    {
                        break;
                    }
                    if Instant::now() >= give_up {
                        break;
                    }
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(UPDATE_MAX_WAIT);
                }
                tasks_done.lock().unwrap().remove(&peer);
            }),
        );
    }

    /// Contatos a quem [`Node::introduce`] ainda não entregou (sem confirmação até agora), para o app guardar e
    /// chamar `introduce` de novo no próximo início.
    pub fn pending_introductions(&self) -> Vec<[u8; 32]> {
        self.introducing.lock().unwrap().keys().copied().collect()
    }

    /// Quem adicionou esta pessoa e se apresentou desde a última chamada. Cada um já foi confirmado a quem
    /// mandou, mas nada mudou aqui: o app decide se vira contato ([`Node::add_contact`]). Fica vazio com
    /// [`NodeConfig::introductions`] desligado.
    pub fn take_introductions(&self) -> Vec<Introduced> {
        std::mem::take(&mut *self.introduced.lock().unwrap())
    }

    /// Contato removido: deixa de ser atendido e some o endereço dele guardado (`known` e `hints`).
    pub fn forget(&self, peer: &[u8; 32]) {
        if let Some(task) = self.introducing.lock().unwrap().remove(peer) {
            task.abort();
        }
        self.phone.contacts().lock().unwrap().retain(|k| k != peer);
        self.known.lock().unwrap().remove(peer);
        self.hints.lock().unwrap().remove(peer);
        self.history.lock().unwrap().remove(peer);
    }

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
    pub fn published_addr(&self) -> String {
        self.addr.lock().unwrap().clone()
    }

    /// Relay desta máquina como os outros o alcançam (endereço público ou, na falta, o da rede local), para
    /// montar o código de contato direto. `None` se não há endereço que alguém de fora use.
    pub fn relay_info(&self) -> Option<RelayInfo> {
        self.own.lock().unwrap().clone()
    }

    /// Guarda o relay que `peer` informou (veja [`Dialer::add_hint`]).
    pub fn add_hint(&self, peer: [u8; 32], relay: RelayInfo) {
        self.dialer.add_hint(peer, relay);
    }

    /// Aviso sobre a rede (porta aberta por UPnP, ou por que não foi possível), se houver.
    pub fn network_note(&self) -> Option<String> {
        self.note.clone()
    }

    /// Últimos endereços de relay conhecidos por contato, para guardar e devolver em [`NodeConfig::known`].
    pub fn known(&self) -> Vec<([u8; 32], RelayInfo)> {
        self.known.lock().unwrap().iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Relays que contatos mandaram nos convites e que ainda não foram confirmados por uma chamada, para
    /// guardar e devolver em [`NodeConfig::hints`]. Ligar de volta para quem te ligou funciona sem outro endereço.
    pub fn hints(&self) -> Vec<([u8; 32], RelayInfo)> {
        self.hints.lock().unwrap().iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Endereços antigos de relay por contato, do mais novo ao mais velho (até [`HISTORY`] por contato), para
    /// guardar e devolver em [`NodeConfig::history`]. Quem mudou de endereço sem avisar ainda é achado por eles.
    pub fn history(&self) -> Vec<([u8; 32], RelayInfo)> {
        self.history.lock().unwrap().iter().flat_map(|(k, v)| v.iter().map(|r| (*k, r.clone()))).collect()
    }

    /// Motivo de um convite descartado em silêncio (veja [`Phone::take_notice`]), aviso de que um contato
    /// mandou o endereço novo dele ou de que o endereço deste nó mudou, se houve; uma vez só.
    pub fn take_notice(&self) -> Option<String> {
        self.phone.take_notice().or_else(|| self.update_notice.lock().unwrap().take())
    }

    pub async fn next_incoming(&mut self) -> Option<IncomingCall> {
        let inc = self.phone.next_incoming().await?;
        if let Some(h) = inc.relay_hint() {
            self.hints.lock().unwrap().insert(*inc.caller().as_bytes(), RelayInfo { addr: h.addr.to_string(), onion: h.onion, link: h.link });
        }
        Some(inc)
    }

    /// Liga para `peer` (veja [`Dialer::dial`]).
    pub async fn dial<C: Codec + Send + 'static>(&self, peer: [u8; 32], codec: C, wait: Duration) -> Result<ActiveCall, DialError> {
        self.dialer.dial(peer, codec, wait).await
    }

    /// Transmite a tela da chamada para `peer` (veja [`Dialer::share_screen`]).
    pub async fn share_screen(&self, peer: [u8; 32], call: &ActiveCall, wait: Duration) -> Result<ScreenShare, DialError> {
        self.dialer.share_screen(peer, call, wait).await
    }

    /// Cópia barata que sabe ligar sem emprestar o `Node`: serve para discar em tarefas em segundo plano.
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
        for t in self.introducing.lock().unwrap().values() {
            t.abort();
        }
        for t in &self.background {
            t.abort();
        }
    }
}
