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
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
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
    phone::{ActiveCall, CallError, IncomingCall, Phone, call},
    transport::{CircuitClient, ClientOptions, RelayServer, ServerConfig, allow_list},
};

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
    /// Atrás de NAT, sem porta aberta: usa o relay do próprio servidor (o servidor precisa ter sido
    /// iniciado com relay). O servidor passa a ver o seu IP e o tráfego cifrado das suas chamadas.
    pub via_server: bool,
    /// Tenta abrir a porta do relay no roteador por UPnP quando o diretório está na internet. Padrão: sim.
    pub upnp: bool,
}

impl NodeConfig {
    pub fn new(secret: StaticSecret, directory: impl Into<String>, contacts: Vec<[u8; 32]>) -> Self {
        Self { secret, directory: directory.into(), contacts, relay_port: DEFAULT_RELAY_PORT, public_addr: None, via_server: false, upnp: true }
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

pub struct Node {
    secret: StaticSecret,
    directory: String,
    phone: Phone,
    relay: RelayServer,
    mine: RelayInfo,
    last_error: Arc<Mutex<Option<String>>>,
    republish: JoinHandle<()>,
    note: Option<String>,
}

impl Node {
    /// Sobe relay e telefone e publica no diretório. Erra se o diretório não responder.
    pub async fn start(cfg: NodeConfig) -> io::Result<Self> {
        let me = PublicKey::from(&cfg.secret);
        let phone = Phone::bind("127.0.0.1:0".parse().unwrap(), cfg.secret.clone(), cfg.contacts).await?;
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
                return Self::finish(cfg.secret, cfg.directory, cfg.public_addr, cfg.via_server, cfg.upnp, phone, r, onion).await;
            }
            Err(e) => return Err(e),
        };
        Self::finish(cfg.secret, cfg.directory, cfg.public_addr, cfg.via_server, cfg.upnp, phone, relay, onion).await
    }

    async fn finish(
        secret: StaticSecret,
        directory: String,
        public_addr: Option<String>,
        via_server: bool,
        upnp: bool,
        phone: Phone,
        relay: RelayServer,
        onion: [u8; 32],
    ) -> io::Result<Self> {
        let (port, link) = (relay.local_addr().port(), relay.link_public_key());
        let last_error = Arc::new(Mutex::new(None));
        if via_server {
            let (addr, task) = directory::host(&directory, &secret, phone.local_addr(), last_error.clone()).await?;
            let mine = RelayInfo { addr, onion, link };
            return Ok(Self { secret, directory, phone, relay, mine, last_error, republish: task, note: None });
        }
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
        let addr = directory::publish(&directory, &secret, port, onion, link, public_addr.as_deref()).await?;
        let mine = RelayInfo { addr, onion, link };
        let republish = {
            let (directory, secret, last_error, public_addr) = (directory.clone(), secret.clone(), last_error.clone(), public_addr);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(REPUBLISH).await;
                    if let Some(m) = &mapping {
                        let _ = m.renew().await;
                    }
                    let r = directory::publish(&directory, &secret, port, onion, link, public_addr.as_deref()).await;
                    *last_error.lock().unwrap() = r.err().map(|e| e.to_string());
                }
            })
        };
        Ok(Self { secret, directory, phone, relay, mine, last_error, republish, note })
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
    pub fn published_addr(&self) -> &str {
        &self.mine.addr
    }

    /// Aviso sobre a rede (porta aberta por UPnP, ou por que não foi possível), se houver.
    pub fn network_note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Erro da última renovação no diretório, se houve. `None`: tudo certo.
    pub fn directory_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    /// Motivo de um convite descartado em silêncio, se houve (veja [`Phone::take_notice`]).
    pub fn take_notice(&self) -> Option<String> {
        self.phone.take_notice()
    }

    pub async fn next_incoming(&mut self) -> Option<IncomingCall> {
        self.phone.next_incoming().await
    }

    /// Liga para `peer`: acha o relay dele no diretório, sorteia primeiros saltos e chama.
    pub async fn dial<C: Codec + Send + 'static>(&self, peer: [u8; 32], codec: C, wait: Duration) -> Result<ActiveCall, DialError> {
        let exit = directory::lookup(&self.directory, &peer).await?.ok_or(DialError::NotFound)?;
        let me = self.public_key();
        let mut relays: Vec<Candidate> = directory::sample(&self.directory, 3, &[me, peer]).await?.iter().map(RelayInfo::candidate).collect();
        // Último recurso para o primeiro salto: o próprio relay (por loopback). Perde o anonimato
        // perante o outro lado, mas permite ligar quando só há duas pessoas online.
        let port = self.relay.local_addr().port();
        relays.push(Candidate { addr: format!("127.0.0.1:{port}"), public: self.mine.onion.into(), link_public: self.mine.link, score: 0.5 });
        relays.push(exit.candidate());
        let cfg = ManagerConfig { exit_addr: Some(exit.addr.clone()), ..Default::default() };
        let client = CircuitClient::spawn_with(CircuitManager::new(cfg, &dest_for(&peer), relays), Duration::from_secs(3), ClientOptions::default());
        Ok(call(client, &self.secret, &PublicKey::from(peer), codec, wait).await?)
    }

    /// Para relay, telefone e renovações. Descartar o `Node` faz o mesmo.
    pub fn stop(self) {}
}

impl Drop for Node {
    fn drop(&mut self) {
        self.republish.abort();
    }
}
