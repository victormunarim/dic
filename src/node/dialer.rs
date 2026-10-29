//! [`Dialer`]: quem liga para os outros e entrega a tela. Guarda os endereços dos contatos e monta o circuito até eles.

use crate::sync::LockExt;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use x25519_dalek::{PublicKey, StaticSecret};

use crate::{
    circuit::manager::{Candidate, CircuitManager, ManagerConfig},
    media::{
        codec::Codec,
        keys::CallKeys,
    },
    path::{PathMode, PathRequest},
    phone::{ActiveCall, AddressUpdate, CallError, PhoneLink, ScreenShare, call_linked, direct_screen_client_options, open_screen_client_options, screen_client_options, share_screen_slot, OPEN_SCREEN_SLOT, SCREEN_SLOT},
    rendezvous::Rendezvous,
    transport::{CircuitClient, ClientOptions},
};
use super::{History, Known, RelayInfo, dest_for, set_known};
use super::util::ready_within;

#[derive(Debug, thiserror::Error)]
pub enum DialError {
    #[error("{}", if *.tried_stored { "o endereço guardado do contato não respondeu e não há outro (peça um código de contato novo)" } else { "sem endereço desse contato: use o código de contato direto dele" })]
    NoAddress { tried_stored: bool },
    #[error("{0}")]
    Call(#[from] CallError),
}

/// Quem liga para os outros: guarda o que o [`Node`](super::Node) usa para discar e pode ser clonado para tarefas.
#[derive(Clone)]
pub struct Dialer {
    pub(super) secret: StaticSecret,
    pub(super) mine: RelayInfo,
    pub(super) relay_port: u16,
    pub(super) known: Known,
    /// Relays que contatos mandaram nos convites (veja [`Node::hints`](super::Node::hints)): última opção antes da DHT e das âncoras.
    pub(super) hints: Known,
    /// Endereços antigos dos contatos (veja [`Node::history`](super::Node::history)): tentados depois do guardado e da dica.
    pub(super) history: History,
    /// Último `seq` aceito de cada contato: atualização igual ou mais velha é ignorada.
    pub(super) seen_seq: Arc<Mutex<HashMap<[u8; 32], u64>>>,
    /// Registra as chamadas que esta máquina faz no telefone dela (para receber a tela do outro lado).
    pub(super) link: PhoneLink,
    /// DHT onde os contatos deixam o endereço novo (veja [`NodeConfig::dht`]).
    pub(super) dht: Option<Arc<Rendezvous>>,
    /// Fala com a DHT pelos relays de dois contatos em vez do próprio IP (veja [`DhtOptions::circuit`]).
    pub(super) dht_circuit: bool,
    /// Âncoras do usuário (veja [`NodeConfig::anchors`]).
    pub(super) anchors: Vec<RelayInfo>,
    /// Contatos de confiança: os únicos para quem a tela sem privacidade vai direto (veja [`Dialer::set_trusted`]).
    pub(super) trusted: Arc<Mutex<HashSet<[u8; 32]>>>,
}

impl Dialer {
    /// Endereço de relay guardado de `peer` (da última ligação ou aviso, ou, na falta, do convite que ele
    /// mandou), se houver; é por ele que [`Dialer::dial`] tenta primeiro.
    #[must_use]
    pub fn stored_address(&self, peer: &[u8; 32]) -> Option<String> {
        self.stored_for(peer).first().map(|r| r.addr.clone())
    }

    /// Relays de `peer` para tentar antes da DHT e das âncoras, em ordem: o guardado de uma ligação ou aviso, o
    /// que o convite dele trouxe (que ninguém verificou) e os endereços antigos, do mais novo ao mais velho.
    pub(crate) fn stored_for(&self, peer: &[u8; 32]) -> Vec<RelayInfo> {
        let mut v: Vec<RelayInfo> = self.known.lock_unpoisoned().get(peer).cloned().into_iter().collect();
        if let Some(h) = self.hints.lock_unpoisoned().get(peer)
            && !v.contains(h)
        {
            v.push(h.clone());
        }
        for old in self.history.lock_unpoisoned().get(peer).into_iter().flatten() {
            if !v.contains(old) {
                v.push(old.clone());
            }
        }
        v
    }

    /// Guarda o relay que `peer` informou (o código de contato direto) como dica: é o
    /// primeiro endereço que [`Dialer::dial`] tenta e só vira endereço de confiança depois de uma chamada completa.
    pub fn add_hint(&self, peer: [u8; 32], relay: RelayInfo) {
        self.hints.lock_unpoisoned().insert(peer, relay);
    }

    /// Primeiro relay de `stored` cujo circuito fica pronto em 5 s, com o cliente já aberto.
    /// Endereço velho (relay sumiu, ou outra máquina no mesmo IP) nunca completa o handshake.
    pub(super) async fn stored_client(&self, peer: &[u8; 32], stored: Vec<RelayInfo>, options: fn() -> ClientOptions) -> Option<(CircuitClient, RelayInfo)> {
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
    pub(super) fn remember(&self, peer: &[u8; 32], used: RelayInfo) {
        let mut hints = self.hints.lock_unpoisoned();
        if hints.get(peer) == Some(&used) {
            hints.remove(peer);
        }
        drop(hints);
        set_known(&self.known, &self.history, peer, used);
    }

    /// Primeiro relay de `peer` cujo circuito fica pronto: os guardados e, se nenhum responder, o que o
    /// contato deixou na DHT ou nas âncoras. O `bool` diz se algum endereço foi tentado (muda o motivo do erro de quem liga).
    pub(super) async fn reachable_client(&self, peer: &[u8; 32], options: fn() -> ClientOptions) -> (Option<(CircuitClient, RelayInfo)>, bool) {
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
        if options.direct {
            // Direto, o único salto é o relay do contato: nenhum outro candidato entra.
            relays.clear();
        } else {
            // Último recurso para o primeiro salto: o próprio relay (por loopback). Perde o anonimato
            // perante o outro lado, mas permite ligar quando só há duas pessoas online.
            relays.push(Candidate { addr: format!("127.0.0.1:{}", self.relay_port), public: self.mine.onion, link_public: self.mine.link, score: 0.5 });
        }
        relays.push(exit.candidate());
        let cfg = ManagerConfig { exit_addr: Some(exit.addr.clone()), direct: options.direct, ..Default::default() };
        CircuitClient::spawn_with(CircuitManager::new(cfg, &dest_for(peer), relays), Duration::from_secs(3), options)
    }

    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        *PublicKey::from(&self.secret).as_bytes()
    }

    /// Se o relay de `peer` está de pé agora: monta o circuito até ele e o larga sem chamar. Como cada pessoa é o
    /// relay de si mesma, relay de pé quer dizer contato online. Custa um circuito de 2 saltos (até 5 s por
    /// endereço), então o app sonda poucos contatos por vez e não a cada quadro.
    pub async fn is_online(&self, peer: &[u8; 32]) -> bool {
        match self.reachable_client(peer, ClientOptions::default).await.0 {
            Some((client, _)) => {
                client.stop();
                true
            }
            None => false,
        }
    }

    /// Liga para `peer`: acha um relay dele (o guardado, o que ele avisou, a DHT ou uma âncora) e chama por um
    /// circuito que passa pelos relays dos contatos.
    ///
    /// # Errors
    ///
    /// [`DialError::NoAddress`] se nenhum relay do contato foi achado; [`DialError::Call`] se a chamada falhou (circuito, sem resposta, recusada).
    pub async fn dial<C: Codec + Send + 'static>(&self, peer: [u8; 32], codec: C, wait: Duration) -> Result<ActiveCall, DialError> {
        let (found, tried_stored) = self.reachable_client(&peer, ClientOptions::default).await;
        let Some((client, k)) = found else { return Err(DialError::NoAddress { tried_stored }) };
        let call = call_linked(client, &self.secret, &PublicKey::from(peer), codec, wait, Some(&self.link)).await?;
        self.remember(&peer, k);
        Ok(call)
    }

    /// Transmite a tela da chamada `call` para `peer`, por um circuito só da tela (cover de
    /// [`SCREEN_SLOT`]). Privada: por circuito de 2 saltos, com cover.
    ///
    /// # Errors
    ///
    /// Como [`Self::dial`]: sem endereço do contato ou falha ao montar o circuito da tela.
    pub async fn share_screen(&self, peer: [u8; 32], call: &ActiveCall, wait: Duration) -> Result<ScreenShare, DialError> {
        self.share_screen_keys(peer, &call.next_share_keys(), wait).await
    }

    /// Como [`Self::share_screen`], com as chaves em vez da chamada (quem não tem mais a chamada em mãos
    /// guarda uma cópia de [`ActiveCall::next_share_keys`]).
    ///
    /// # Errors
    ///
    /// Como [`Self::share_screen`].
    pub async fn share_screen_keys(&self, peer: [u8; 32], keys: &CallKeys, wait: Duration) -> Result<ScreenShare, DialError> {
        self.share_screen_keys_with(peer, keys, wait, true).await
    }

    /// Marca (ou desmarca) `peer` como contato de confiança: a única pessoa para quem a tela sem privacidade
    /// ([`Self::share_screen_keys_with`] com `private` falso) vai direto, mostrando o IP de quem transmite.
    /// Quem chama deve ter avisado o usuário dessa exposição antes de marcar. Fora da lista, a tela segue
    /// por 2 relays. Vale a partir da próxima transmissão.
    pub fn set_trusted(&self, peer: [u8; 32], trusted: bool) {
        let mut set = self.trusted.lock_unpoisoned();
        if trusted {
            set.insert(peer);
        } else {
            set.remove(&peer);
        }
    }

    /// Se `peer` está marcado como contato de confiança (veja [`Self::set_trusted`]).
    #[must_use]
    pub fn is_trusted(&self, peer: &[u8; 32]) -> bool {
        self.trusted.lock_unpoisoned().contains(peer)
    }

    /// Como [`Self::share_screen_keys`]. Com `private` falso a tela abre mão do cover: não manda pacote nos
    /// instantes sem vídeo (quem observa a rede vê quando a tela se mexe) e o ritmo máximo quadruplica
    /// ([`OPEN_SCREEN_SLOT`]). O caminho depende de quem assiste (veja [`PathRequest::resolve`]): para um contato
    /// de confiança ([`Self::set_trusted`]) vai direto ao relay dele, sem relay no meio, e ele vê o seu IP; para
    /// os demais segue por 2 relays, que escondem o seu IP. O relay do contato de confiança precisa ser desta
    /// versão ou mais nova. O vídeo e as chaves seguem cifrados. Quem transmite deve usar
    /// [`H264Encoder::bitrate_for`](crate::video::H264Encoder::bitrate_for) com o mesmo intervalo.
    ///
    /// # Errors
    ///
    /// Como [`Self::share_screen`]. Contato fora da confiança nunca vai direto: com `private` falso segue por 2 relays, sem falhar.
    pub async fn share_screen_keys_with(&self, peer: [u8; 32], keys: &CallKeys, wait: Duration, private: bool) -> Result<ScreenShare, DialError> {
        let (options, slot): (fn() -> ClientOptions, Duration) = match (private, self.screen_path(&peer, private)) {
            (true, _) => (screen_client_options, SCREEN_SLOT),
            (false, PathMode::Direct) => (direct_screen_client_options, OPEN_SCREEN_SLOT),
            (false, _) => (open_screen_client_options, OPEN_SCREEN_SLOT),
        };
        let (found, tried_stored) = self.reachable_client(&peer, options).await;
        let Some((client, used)) = found else { return Err(DialError::NoAddress { tried_stored }) };
        let share = share_screen_slot(client, keys, wait, slot).await?;
        self.remember(&peer, used);
        Ok(share)
    }

    /// O caminho da tela para `peer`: sempre [`PathMode::TwoHop`] com `private`; sem ele, [`PathMode::Direct`] só
    /// para contato de confiança ([`Self::set_trusted`]) e [`PathMode::TwoHop`] para os demais. Marcar alguém de
    /// confiança pressupõe o aviso de exposição do IP já confirmado.
    #[must_use]
    pub fn screen_path(&self, peer: &[u8; 32], private: bool) -> PathMode {
        if private {
            return PathMode::TwoHop;
        }
        PathRequest { mode: PathMode::Direct, contact_trusted: self.is_trusted(peer), ip_exposure_warning_acked: true }.resolve().unwrap_or_default()
    }
}
