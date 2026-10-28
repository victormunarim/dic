//! Configuração do [`Node`](super::Node).

use std::time::Duration;

use x25519_dalek::StaticSecret;

use crate::{
    rendezvous::DhtOptions,
};
use super::{RelayInfo};

/// Porta de relay preferida; se estiver ocupada, o sistema escolhe outra.
pub const DEFAULT_RELAY_PORT: u16 = 4001;

/// Espera inicial entre as tentativas de entregar o endereço novo a um contato (dobra a cada falha, até 2 h).
pub const UPDATE_RETRY: Duration = Duration::from_secs(30);

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
    /// Últimos endereços de relay conhecidos por contato (veja [`Node::known`](super::Node::known)). Ligar para quem está
    /// aqui funciona enquanto o endereço não mudar.
    pub known: Vec<([u8; 32], RelayInfo)>,
    /// Relays que contatos informaram nos convites que mandaram (veja [`Node::hints`](super::Node::hints)). Ficam atrás de `known`
    /// na ordem de tentativa, e só viram `known` depois de uma chamada completada por eles.
    pub hints: Vec<([u8; 32], RelayInfo)>,
    /// Segredos do relay (circuito, link). `None` gera chaves novas a cada início, e então o endereço
    /// que os contatos guardaram de você (veja `known`) deixa de valer a cada reinício; gere o par uma vez
    /// com [`RelayKeys::random`], guarde-o e passe-o aqui.
    pub relay_keys: Option<RelayKeys>,
    /// Endereços antigos de relay por contato, do mais novo ao mais velho (veja [`Node::history`](super::Node::history)). Ligar
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
    /// Como esta pessoa quer ser chamada nas apresentações que manda ([`Node::introduce`](super::Node::introduce)); vazio: sem nome.
    pub name: String,
    /// Aceita apresentações de quem adicionou esta pessoa (veja [`Node::take_introductions`](super::Node::take_introductions)). Padrão: sim.
    pub introductions: bool,
    /// Serve de âncora: guarda os registros de endereço dos outros nós (veja [`crate::rendezvous`]). Padrão: não.
    pub anchor: bool,
    /// Âncoras onde este nó grava o endereço e consulta o dos contatos, em ordem de preferência. Padrão: nenhuma.
    pub anchors: Vec<RelayInfo>,
    /// Estado dos servidores guardado por [`crate::server::Servers::export`]; `None`: nenhum servidor.
    pub servers: Option<String>,
}

/// Segredos do relay do nó: o do circuito (`onion`) e o do link (Noise NK).
#[derive(Clone)]
pub struct RelayKeys {
    pub onion: StaticSecret,
    pub link: StaticSecret,
}

impl RelayKeys {
    #[must_use]
    pub fn random() -> Self {
        Self { onion: StaticSecret::random_from_rng(rand_core::OsRng), link: StaticSecret::random_from_rng(rand_core::OsRng) }
    }
}

impl NodeConfig {
    #[must_use]
    pub fn new(secret: StaticSecret, contacts: Vec<[u8; 32]>) -> Self {
        Self { secret, contacts, relay_port: DEFAULT_RELAY_PORT, public_addr: None, upnp: true, known: Vec::new(), hints: Vec::new(), relay_keys: None, history: Vec::new(), updates: true, update_retry: UPDATE_RETRY, dht: None, name: String::new(), introductions: true, anchor: false, anchors: Vec::new(), servers: None }
    }
}
