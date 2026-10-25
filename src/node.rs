//! Nó: o que cada pessoa roda para ligar e receber chamadas (feature `net`).
//!
//! Um [`Node`] junta três coisas que antes eram configuradas à mão:
//! um relay (que também serve de último salto para quem liga para você), o telefone ([`Phone`])
//! e a publicação no [diretório](crate::directory). Quem usa só informa a identidade, o
//! endereço do diretório e os contatos permitidos; para ligar, só a chave pública do outro.
//!
//! O diretório é usado só para achar o relay do outro e para sortear um primeiro salto.
//! A chamada vai pelos relays, de ponta a ponta, sem passar por ele.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::task::JoinHandle;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{
    circuit::{
        manager::{Candidate, CircuitManager, ManagerConfig},
        relay::{RelayConfig, RelayNode},
    },
    upnp,
    directory::{self, REPUBLISH, RelayInfo, dest_for},
    media::codec::Codec,
    phone::{self, ActiveCall, CallError, IncomingCall, Phone, call, call_over},
    punch::Punch,
    transport::{CircuitClient, ClientOptions, RelayServer, ServerConfig, allow_list},
};

/// Depois de um furo de NAT que falhou, quanto tempo a ligação para o mesmo contato nem tenta de novo.
const NO_PUNCH_FOR: Duration = Duration::from_secs(300);

/// A cada quantas renovações no diretório os endereços dos contatos são atualizados.
const REFRESH_KNOWN_EVERY: u32 = 5;

/// Porta de relay preferida; se estiver ocupada, o sistema escolhe outra.
pub const DEFAULT_RELAY_PORT: u16 = 4001;

pub struct NodeConfig {
    pub secret: StaticSecret,
    /// Endereço `host:porta` do diretório de primeiro contato.
    pub directory: String,
    /// Chaves públicas que podem ligar para você. Quem não está na lista não recebe resposta.
    pub contacts: Vec<[u8; 32]>,
    /// Porta TCP do relay (0 = qualquer). Padrão: [`DEFAULT_RELAY_PORT`].
    pub relay_port: u16,
    /// Endereço `ip:porta` que os outros devem usar para chegar ao relay, se o IP que o diretório
    /// enxerga não serve (NAT com redirecionamento de porta, VPN). `None`: o diretório decide.
    pub public_addr: Option<String>,
    /// Tenta abrir a porta do relay no roteador por UPnP quando o diretório está na internet. Padrão: sim.
    pub upnp: bool,
    /// Tenta furar o NAT por UDP para chamadas diretas (sem circuito). Padrão: sim.
    pub punch: bool,
    /// Últimos endereços de relay conhecidos por contato (veja [`Node::known`]). Ligar para quem está
    /// aqui funciona mesmo com o diretório fora do ar, enquanto o endereço não mudar.
    pub known: Vec<([u8; 32], RelayInfo)>,
}

impl NodeConfig {
    pub fn new(secret: StaticSecret, directory: impl Into<String>, contacts: Vec<[u8; 32]>) -> Self {
        Self { secret, directory: directory.into(), contacts, relay_port: DEFAULT_RELAY_PORT, public_addr: None, upnp: true, punch: true, known: Vec::new() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DialError {
    #[error("diretório: {0}")]
    Directory(#[from] io::Error),
    #[error("essa chave não está online no diretório")]
    NotFound,
    #[error("{0}")]
    Call(#[from] CallError),
}

/// Quem liga para os outros: guarda o que o [`Node`] usa para discar e pode ser clonado para tarefas.
#[derive(Clone)]
pub struct Dialer {
    secret: StaticSecret,
    directory: String,
    mine: RelayInfo,
    relay_port: u16,
    punch: Option<Arc<Punch>>,
    known: Known,
    /// Contatos que não deixaram furar o NAT há pouco: pula o furo até a data indicada.
    no_punch: Arc<Mutex<HashMap<[u8; 32], Instant>>>,
}

type Known = Arc<Mutex<HashMap<[u8; 32], RelayInfo>>>;

/// Consulta o relay de cada contato (até 32) e guarda o endereço. Para na primeira falha do diretório.
async fn refresh_known(directory: &str, contacts: &phone::Contacts, known: &Known) {
    let keys: Vec<[u8; 32]> = contacts.lock().unwrap().iter().take(32).copied().collect();
    for k in keys {
        match directory::lookup(directory, &k).await {
            Ok(Some(r)) => {
                known.lock().unwrap().insert(k, r);
            }
            Ok(None) => {}
            Err(_) => return,
        }
    }
}

/// Acrescenta um aviso à nota de rede, separando por `; `.
fn push_note(note: &mut Option<String>, add: String) {
    *note = Some(match note.take() {
        Some(n) => format!("{n}; {add}"),
        None => add,
    });
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
    /// Cliente de circuito até `exit`. Sem outros relays, o primeiro salto é o próprio relay (por loopback).
    fn client(&self, peer: &[u8; 32], exit: &RelayInfo, mut relays: Vec<Candidate>) -> CircuitClient {
        // Último recurso para o primeiro salto: o próprio relay (por loopback). Perde o anonimato
        // perante o outro lado, mas permite ligar quando só há duas pessoas online.
        relays.push(Candidate { addr: format!("127.0.0.1:{}", self.relay_port), public: self.mine.onion, link_public: self.mine.link, score: 0.5 });
        relays.push(exit.candidate());
        let cfg = ManagerConfig { exit_addr: Some(exit.addr.clone()), ..Default::default() };
        CircuitClient::spawn_with(CircuitManager::new(cfg, &dest_for(peer), relays), Duration::from_secs(3), ClientOptions::default())
    }
    pub fn public_key(&self) -> [u8; 32] {
        *PublicKey::from(&self.secret).as_bytes()
    }

    /// Liga para `peer`: tenta o furo de NAT; se não der, acha o relay dele no diretório, sorteia primeiros saltos e chama.
    pub async fn dial<C: Codec + Send + 'static>(&self, peer: [u8; 32], codec: C, wait: Duration) -> Result<ActiveCall, DialError> {
        // Caminho direto por furo de NAT; se não der, cai no circuito.
        let skip = self.no_punch.lock().unwrap().get(&peer).is_some_and(|until| *until > Instant::now());
        let codec = match &self.punch {
            Some(p) if !skip => match p.dial(&peer, Duration::from_secs(6)).await {
                Ok(c) => return Ok(call_over(c.tx, c.rx, &self.secret, &PublicKey::from(peer), codec, wait).await?),
                Err(_) => {
                    self.no_punch.lock().unwrap().insert(peer, Instant::now() + NO_PUNCH_FOR);
                    codec
                }
            },
            _ => codec,
        };
        let me = *PublicKey::from(&self.secret).as_bytes();
        // Endereço guardado da última vez: se o circuito fica pronto, o diretório nem entra na conversa.
        // Endereço velho (relay sumiu, ou outra máquina no mesmo IP) nunca completa o handshake.
        let cached = self.known.lock().unwrap().get(&peer).cloned();
        if let Some(k) = cached {
            let client = self.client(&peer, &k, Vec::new());
            if ready_within(&client, Duration::from_secs(5)).await {
                return Ok(call(client, &self.secret, &PublicKey::from(peer), codec, wait).await?);
            }
            client.stop();
        }
        let exit = directory::lookup(&self.directory, &peer).await?.ok_or(DialError::NotFound)?;
        self.known.lock().unwrap().insert(peer, exit.clone());
        let relays = directory::sample(&self.directory, 3, &[me, peer]).await?.iter().map(RelayInfo::candidate).collect();
        let client = self.client(&peer, &exit, relays);
        Ok(call(client, &self.secret, &PublicKey::from(peer), codec, wait).await?)
    }
}

pub struct Node {
    secret: StaticSecret,
    directory: String,
    phone: Phone,
    _relay: RelayServer,
    /// Endereço publicado; vale o local até o diretório responder.
    addr: Arc<Mutex<String>>,
    last_error: Arc<Mutex<Option<String>>>,
    republish: JoinHandle<()>,
    note: Option<String>,
    dialer: Dialer,
    known: Known,
}

impl Node {
    /// Sobe relay e telefone e publica no diretório. Se o diretório não responder, sobe assim mesmo
    /// (veja [`Node::directory_error`]): recebe de quem já tem o seu endereço e liga para quem está em `known`.
    pub async fn start(cfg: NodeConfig) -> io::Result<Self> {
        let me = PublicKey::from(&cfg.secret);
        let phone = Phone::bind("127.0.0.1:0".parse().unwrap(), cfg.secret.clone(), cfg.contacts.clone()).await?;
        let relay_node = RelayNode::new(StaticSecret::random_from_rng(rand_core::OsRng), RelayConfig::default(), 0);
        let onion = relay_node.public_key();
        let mut scfg = ServerConfig::new(SocketAddr::from(([0, 0, 0, 0], cfg.relay_port)));
        scfg.exit = allow_list(vec![(dest_for(&me.to_bytes()), phone.local_addr().to_string())]);
        let relay = match RelayServer::spawn(relay_node, scfg.clone()).await {
            Ok(r) => r,
            // Porta ocupada: o sistema escolhe outra. `RelayNode` foi consumido, então recria.
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && cfg.relay_port != 0 => {
                let n = RelayNode::new(StaticSecret::random_from_rng(rand_core::OsRng), RelayConfig::default(), 0);
                let onion = n.public_key();
                scfg.listen = SocketAddr::from(([0, 0, 0, 0], 0));
                let r = RelayServer::spawn(n, scfg).await?;
                return Self::finish(cfg, phone, r, onion).await;
            }
            Err(e) => return Err(e),
        };
        Self::finish(cfg, phone, relay, onion).await
    }

    async fn finish(cfg: NodeConfig, phone: Phone, relay: RelayServer, onion: [u8; 32]) -> io::Result<Self> {
        let NodeConfig { secret, directory, public_addr, upnp, punch, known, .. } = cfg;
        let known: Known = Arc::new(Mutex::new(known.into_iter().collect()));
        let (port, link) = (relay.local_addr().port(), relay.link_public_key());
        let last_error = Arc::new(Mutex::new(None));
        // Diretório na internet e nenhum endereço público informado: tenta abrir a porta no roteador.
        let (mut public_addr, mut mapping, mut note) = (public_addr, None, None);
        if upnp && public_addr.is_none() && !upnp::directory_is_local(&directory).await {
            match upnp::map(port).await {
                Ok(m) => {
                    note = Some(format!("porta {port} aberta no roteador (UPnP)"));
                    public_addr = Some(m.external.clone());
                    mapping = Some(m);
                }
                Err(e) => note = Some(format!("não deu para abrir a porta automaticamente: {e}. Libere a porta {port} no roteador/firewall ou informe o endereço público")),
            }
        }
        let punch = if punch {
            match Punch::start(&directory, &secret, phone.datagram_port()).await {
                Ok(p) => Some(Arc::new(p)),
                Err(e) => {
                    push_note(&mut note, format!("sem furo de NAT: {e}"));
                    None
                }
            }
        } else {
            None
        };
        let (addr, published) = match directory::publish(&directory, &secret, port, onion, link, public_addr.as_deref()).await {
            Ok(a) => (a, true),
            Err(e) => {
                *last_error.lock().unwrap() = Some(e.to_string());
                (public_addr.clone().unwrap_or_else(|| format!("127.0.0.1:{port}")), false)
            }
        };
        let addr_now = Arc::new(Mutex::new(addr.clone()));
        let mine = RelayInfo { addr, onion, link };
        // Pede ao diretório para conectar de fora ao relay publicado: avisa cedo se ninguém consegue ligar para você.
        if published
            && !upnp::directory_is_local(&directory).await
            && let Ok(false) = directory::reachable(&directory, &secret).await
        {
            let warn = if punch.is_some() {
                format!("o relay {} não é alcançável de fora; chamadas diretas por furo de NAT ainda funcionam, mas libere a porta {port} para o resto", mine.addr)
            } else {
                format!("o relay {} não é alcançável de fora: ninguém consegue ligar para você. Libere a porta {port} no roteador/firewall ou informe o endereço público", mine.addr)
            };
            push_note(&mut note, warn);
        }
        let dialer = Dialer { secret: secret.clone(), directory: directory.clone(), mine: mine.clone(), relay_port: port, punch, known: known.clone(), no_punch: Arc::default() };
        let republish = {
            let (directory, secret, last_error, public_addr, addr_now) = (directory.clone(), secret.clone(), last_error.clone(), public_addr, addr_now.clone());
            let (contacts, known) = (phone.contacts(), known.clone());
            tokio::spawn(async move {
                let mut round = 0u32;
                loop {
                    // Guarda o endereço dos contatos agora (e de tempos em tempos) para ligar a eles com o diretório fora.
                    if round.is_multiple_of(REFRESH_KNOWN_EVERY) {
                        refresh_known(&directory, &contacts, &known).await;
                    }
                    round = round.wrapping_add(1);
                    tokio::time::sleep(REPUBLISH).await;
                    if let Some(m) = &mapping {
                        let _ = m.renew().await;
                    }
                    let r = directory::publish(&directory, &secret, port, onion, link, public_addr.as_deref()).await;
                    match r {
                        Ok(a) => {
                            *addr_now.lock().unwrap() = a;
                            *last_error.lock().unwrap() = None;
                        }
                        Err(e) => *last_error.lock().unwrap() = Some(e.to_string()),
                    }
                }
            })
        };
        Ok(Self { secret, directory, phone, _relay: relay, last_error, republish, note, dialer, known, addr: addr_now })
    }

    /// Aceita convites de `keys` além dos contatos (membros de uma sala).
    pub fn allow(&self, keys: &[[u8; 32]]) {
        let c = self.phone.contacts();
        let mut c = c.lock().unwrap();
        for k in keys {
            if !c.contains(k) {
                c.push(*k);
            }
        }
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub fn public_key(&self) -> [u8; 32] {
        *PublicKey::from(&self.secret).as_bytes()
    }

    /// Endereço publicado no diretório (o que os outros usam para chegar ao seu relay).
    pub fn published_addr(&self) -> String {
        self.addr.lock().unwrap().clone()
    }

    /// Aviso sobre a rede (porta aberta por UPnP, ou por que não foi possível), se houver.
    /// Inclui o aviso de diretório fora do ar enquanto durar.
    pub fn network_note(&self) -> Option<String> {
        let down = self.directory_error().map(|e| {
            format!("diretório fora do ar ({e}): só chegam a você os que já têm o seu endereço; tentando de novo a cada {}s", REPUBLISH.as_secs())
        });
        match (self.note.clone(), down) {
            (Some(n), Some(d)) => Some(format!("{n}; {d}")),
            (n, d) => n.or(d),
        }
    }

    /// Erro da última renovação no diretório, se houve. `None`: tudo certo.
    pub fn directory_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    /// Últimos endereços de relay conhecidos por contato, para guardar e devolver em [`NodeConfig::known`].
    pub fn known(&self) -> Vec<([u8; 32], RelayInfo)> {
        self.known.lock().unwrap().iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Motivo de um convite descartado em silêncio, se houve (veja [`Phone::take_notice`]).
    pub fn take_notice(&self) -> Option<String> {
        self.phone.take_notice()
    }

    pub async fn next_incoming(&mut self) -> Option<IncomingCall> {
        self.phone.next_incoming().await
    }

    /// Liga para `peer` (veja [`Dialer::dial`]).
    pub async fn dial<C: Codec + Send + 'static>(&self, peer: [u8; 32], codec: C, wait: Duration) -> Result<ActiveCall, DialError> {
        self.dialer.dial(peer, codec, wait).await
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
        self.republish.abort();
    }
}
