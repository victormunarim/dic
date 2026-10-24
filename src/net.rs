//! Transporte libp2p (feature `net`): Kademlia como [`RecordStore`] e GossipSub
//! nos tópicos derivados do círculo.
//!
//! Privacidade: o GossipSub roda em modo anônimo (sem PeerId nem assinatura no
//! cabeçalho da mensagem), então quem observa um tópico não liga mensagens a um
//! nó. Autenticar o conteúdo é papel do payload (cifrado com a chave do círculo).

use std::{collections::HashMap, sync::mpsc as std_mpsc, time::Duration};

use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, SwarmBuilder, gossipsub, identify, kad,
    multiaddr::Protocol,
    noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::{
    descriptor::DirectoryConfig,
    discovery::RELAY_TOPIC,
    store::{RecordStore, TopicControl},
};

#[derive(Debug, Error)]
pub enum NetError {
    #[error("falha ao montar o nó libp2p: {0}")]
    Build(String),
    #[error("endereço inválido ({0}): falta o sufixo /p2p/<PeerId>")]
    MissingPeerId(Multiaddr),
    #[error("o nó foi encerrado")]
    Closed,
}

#[derive(Debug, Clone, Default)]
pub struct NetConfig {
    /// Endereços onde escutar, ex.: `/ip4/0.0.0.0/tcp/4001`. Vazio: nenhum.
    pub listen: Vec<Multiaddr>,
    /// Nós iniciais, no formato `/ip4/.../tcp/.../p2p/<PeerId>`.
    pub bootstrap: Vec<Multiaddr>,
    /// Tempo máximo de um `get` na DHT. Padrão: 10 s.
    pub get_timeout: Option<Duration>,
    /// Regras para repassar descritores de relays no GossipSub (veja [`crate::discovery`]).
    pub relay_policy: DirectoryConfig,
}

/// Mensagem recebida em um tópico GossipSub.
#[derive(Debug, Clone)]
pub struct GossipMessage {
    pub topic: String,
    pub data: Vec<u8>,
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    kad: kad::Behaviour<kad::store::MemoryStore>,
    gossipsub: gossipsub::Behaviour,
    identify: identify::Behaviour,
}

enum Command {
    Put { key: [u8; 32], value: Vec<u8> },
    Get { key: [u8; 32], reply: std_mpsc::Sender<Option<Vec<u8>>> },
    Subscribe { topic: String },
    Unsubscribe { topic: String },
    Publish { topic: String, data: Vec<u8> },
    Dial { addr: Multiaddr },
    Listeners { reply: oneshot::Sender<Vec<Multiaddr>> },
}

/// Alça clonável para assinar e publicar tópicos GossipSub.
#[derive(Clone)]
pub struct Gossip {
    cmd: mpsc::UnboundedSender<Command>,
}

impl Gossip {
    pub fn subscribe(&self, topic: &str) -> Result<(), NetError> {
        self.cmd.send(Command::Subscribe { topic: topic.into() }).map_err(|_| NetError::Closed)
    }

    pub fn publish(&self, topic: &str, data: Vec<u8>) -> Result<(), NetError> {
        self.cmd.send(Command::Publish { topic: topic.into(), data }).map_err(|_| NetError::Closed)
    }
}

/// Alça clonável para a DHT. Implementa [`RecordStore`].
///
/// `get` **bloqueia** a thread até a DHT responder (ou estourar o tempo).
/// Chame o serviço de presença a partir de `tokio::task::spawn_blocking`,
/// nunca direto de uma tarefa async.
#[derive(Clone)]
pub struct NetStore {
    cmd: mpsc::UnboundedSender<Command>,
    get_timeout: Duration,
}

impl RecordStore for NetStore {
    fn put(&self, key: [u8; 32], value: Vec<u8>) {
        let _ = self.cmd.send(Command::Put { key, value });
    }

    fn get(&self, key: &[u8; 32]) -> Option<Vec<u8>> {
        let (reply, rx) = std_mpsc::channel();
        self.cmd.send(Command::Get { key: *key, reply }).ok()?;
        rx.recv_timeout(self.get_timeout).ok().flatten()
    }
}

impl TopicControl for NetStore {
    fn subscribe(&self, topic: &str) {
        let _ = self.cmd.send(Command::Subscribe { topic: topic.into() });
    }

    fn unsubscribe(&self, topic: &str) {
        let _ = self.cmd.send(Command::Unsubscribe { topic: topic.into() });
    }
}

/// Nó libp2p em execução. Encerra a tarefa interna ao ser descartado.
pub struct Node {
    peer_id: PeerId,
    cmd: mpsc::UnboundedSender<Command>,
    messages: mpsc::UnboundedReceiver<GossipMessage>,
    descriptors: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    get_timeout: Duration,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Node {
    /// Sobe o nó, escuta nos endereços e disca para os nós iniciais.
    pub async fn start(cfg: NetConfig) -> Result<Node, NetError> {
        let mut swarm = SwarmBuilder::with_new_identity()
            .with_tokio()
            .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)
            .map_err(|e| NetError::Build(e.to_string()))?
            .with_behaviour(|key| {
                let peer = key.public().to_peer_id();
                let mut kad = kad::Behaviour::new(peer, kad::store::MemoryStore::new(peer));
                // Sem confirmação de endereço externo ainda: responde a consultas desde já.
                kad.set_mode(Some(kad::Mode::Server));

                let gs_cfg = gossipsub::ConfigBuilder::default()
                    .validation_mode(gossipsub::ValidationMode::Anonymous)
                    // O app valida antes de repassar: nada se propaga sem aceite.
                    .validate_messages()
                    .message_id_fn(|m| gossipsub::MessageId::from(Sha256::digest(&m.data).to_vec()))
                    .build()
                    .map_err(|e| e.to_string())?;
                let gossipsub = gossipsub::Behaviour::new(gossipsub::MessageAuthenticity::Anonymous, gs_cfg)
                    .map_err(|e| e.to_string())?;
                let identify = identify::Behaviour::new(identify::Config::new("/clam/1".into(), key.public()));
                Ok(Behaviour { kad, gossipsub, identify })
            })
            .map_err(|e| NetError::Build(e.to_string()))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        for addr in cfg.listen {
            swarm.listen_on(addr).map_err(|e| NetError::Build(e.to_string()))?;
        }
        for addr in &cfg.bootstrap {
            let peer = peer_of(addr)?;
            swarm.behaviour_mut().kad.add_address(&peer, addr.clone());
            let _ = swarm.dial(addr.clone());
        }
        if !cfg.bootstrap.is_empty() {
            let _ = swarm.behaviour_mut().kad.bootstrap();
        }

        let peer_id = *swarm.local_peer_id();
        let (cmd, cmd_rx) = mpsc::unbounded_channel();
        let (msg_tx, messages) = mpsc::unbounded_channel();
        let (desc_tx, descriptors) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(swarm, cmd_rx, msg_tx, desc_tx, cfg.relay_policy));
        Ok(Node {
            peer_id,
            cmd,
            messages,
            descriptors: Some(descriptors),
            get_timeout: cfg.get_timeout.unwrap_or(Duration::from_secs(10)),
            task,
        })
    }

    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Armazenamento para o serviço de presença (`service::publish`/`lookup`).
    pub fn store(&self) -> NetStore {
        NetStore { cmd: self.cmd.clone(), get_timeout: self.get_timeout }
    }

    /// Endereços em que o nó já está escutando (vazio até o `listen_on` concluir).
    pub async fn listen_addrs(&self) -> Result<Vec<Multiaddr>, NetError> {
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Command::Listeners { reply }).map_err(|_| NetError::Closed)?;
        rx.await.map_err(|_| NetError::Closed)
    }

    /// Disca para `addr` (com `/p2p/<PeerId>`) e o registra na tabela Kademlia.
    pub fn dial(&self, addr: Multiaddr) -> Result<(), NetError> {
        peer_of(&addr)?;
        self.cmd.send(Command::Dial { addr }).map_err(|_| NetError::Closed)
    }

    pub fn subscribe(&self, topic: &str) -> Result<(), NetError> {
        self.cmd.send(Command::Subscribe { topic: topic.into() }).map_err(|_| NetError::Closed)
    }

    pub fn unsubscribe(&self, topic: &str) -> Result<(), NetError> {
        self.cmd.send(Command::Unsubscribe { topic: topic.into() }).map_err(|_| NetError::Closed)
    }

    pub fn publish(&self, topic: &str, data: Vec<u8>) -> Result<(), NetError> {
        self.cmd.send(Command::Publish { topic: topic.into(), data }).map_err(|_| NetError::Closed)
    }

    /// Alça clonável para assinar e publicar no GossipSub.
    pub fn gossip(&self) -> Gossip {
        Gossip { cmd: self.cmd.clone() }
    }

    /// Fila dos descritores de relays já validados que chegam em [`RELAY_TOPIC`].
    /// Só pode ser retirada uma vez; essas mensagens não passam por [`Self::next_message`].
    pub fn take_descriptors(&mut self) -> Option<mpsc::UnboundedReceiver<Vec<u8>>> {
        self.descriptors.take()
    }

    /// Próxima mensagem de qualquer tópico assinado.
    pub async fn next_message(&mut self) -> Option<GossipMessage> {
        self.messages.recv().await
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn peer_of(addr: &Multiaddr) -> Result<PeerId, NetError> {
    addr.iter()
        .find_map(|p| if let Protocol::P2p(id) = p { Some(id) } else { None })
        .ok_or_else(|| NetError::MissingPeerId(addr.clone()))
}

async fn run(
    mut swarm: libp2p::Swarm<Behaviour>,
    mut cmds: mpsc::UnboundedReceiver<Command>,
    messages: mpsc::UnboundedSender<GossipMessage>,
    descriptors: mpsc::UnboundedSender<Vec<u8>>,
    relay_policy: DirectoryConfig,
) {
    let mut pending: HashMap<kad::QueryId, std_mpsc::Sender<Option<Vec<u8>>>> = HashMap::new();
    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return };
                handle_command(&mut swarm, &mut pending, cmd);
            }
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::OutboundQueryProgressed { id, result, step, .. })) => {
                    if let kad::QueryResult::GetRecord(res) = result {
                        match res {
                            Ok(kad::GetRecordOk::FoundRecord(r)) => {
                                if let Some(reply) = pending.remove(&id) {
                                    let _ = reply.send(Some(r.record.value));
                                }
                            }
                            Ok(_) | Err(_) if step.last => {
                                if let Some(reply) = pending.remove(&id) {
                                    let _ = reply.send(None);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. })) => {
                    for addr in info.listen_addrs {
                        swarm.behaviour_mut().kad.add_address(&peer_id, addr);
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source,
                    message_id,
                    message,
                })) => {
                    let topic = message.topic.into_string();
                    let is_relay_topic = topic == RELAY_TOPIC;
                    // Descritor inválido não é repassado nem entregue.
                    let valid = !is_relay_topic || relay_policy.check_bytes(&message.data, unix_now()).is_ok();
                    let verdict = if valid { gossipsub::MessageAcceptance::Accept } else { gossipsub::MessageAcceptance::Reject };
                    let _ = swarm.behaviour_mut().gossipsub.report_message_validation_result(
                        &message_id,
                        &propagation_source,
                        verdict,
                    );
                    if valid {
                        if is_relay_topic {
                            let _ = descriptors.send(message.data);
                        } else {
                            let _ = messages.send(GossipMessage { topic, data: message.data });
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn handle_command(
    swarm: &mut libp2p::Swarm<Behaviour>,
    pending: &mut HashMap<kad::QueryId, std_mpsc::Sender<Option<Vec<u8>>>>,
    cmd: Command,
) {
    let b = swarm.behaviour_mut();
    match cmd {
        Command::Put { key, value } => {
            let record = kad::Record::new(kad::RecordKey::new(&key), value);
            // Sem pares conhecidos o registro fica só no armazenamento local.
            let _ = b.kad.put_record(record, kad::Quorum::One);
        }
        Command::Get { key, reply } => {
            let id = b.kad.get_record(kad::RecordKey::new(&key));
            pending.insert(id, reply);
        }
        Command::Subscribe { topic } => {
            let _ = b.gossipsub.subscribe(&gossipsub::IdentTopic::new(topic));
        }
        Command::Unsubscribe { topic } => {
            let _ = b.gossipsub.unsubscribe(&gossipsub::IdentTopic::new(topic));
        }
        Command::Publish { topic, data } => {
            let _ = b.gossipsub.publish(gossipsub::IdentTopic::new(topic), data);
        }
        Command::Dial { addr } => {
            if let Ok(peer) = peer_of(&addr) {
                b.kad.add_address(&peer, addr.clone());
            }
            let _ = swarm.dial(addr);
        }
        Command::Listeners { reply } => {
            let _ = reply.send(swarm.listeners().cloned().collect());
        }
    }
}
