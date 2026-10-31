//! [`Dialer`]: quem liga para os outros e entrega a tela. Guarda os endereços dos contatos e monta o circuito até eles.

use crate::sync::LockExt;
use std::{
    collections::HashMap,
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
    phone::{ActiveCall, AddressUpdate, CallError, PhoneLink, ScreenShare, call_linked, screen_client_options, share_screen_slot, SCREEN_SLOT},
    rendezvous::Rendezvous,
    transport::{CircuitClient, ClientOptions},
};
use super::{History, Known, RelayInfo, dest_for, set_known};
use super::util::ready_within;

#[derive(Debug, thiserror::Error)]
pub enum DialError {
    #[error("{}", if *.tried_stored { "o endereço guardado do contato não respondeu e não há outro (peça um código de contato novo)" } else { "sem endereço desse contato: use o código de contato direto dele" })]
    NoAddress { tried_stored: bool },
    #[error("a opção 2 relays está ligada, mas não há relay de outro contato para servir de primeiro salto: nunca se cai para um caminho que mostre o seu IP")]
    NoRelays,
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
    /// Opção "2 relays" (veja [`NodeConfig::two_relays`]): ligada, o primeiro salto é o relay de outro contato; desligada, a ligação é direta.
    pub(super) two_relays: bool,
    /// Último carimbo de bate-papo usado (veja [`Dialer::next_chat_id`]).
    pub(super) chat_stamp: Arc<std::sync::atomic::AtomicU64>,
}

impl Dialer {
    /// Identificador da próxima mensagem de bate-papo: o relógio em ms Unix, e sempre maior que o anterior (duas
    /// mensagens no mesmo milissegundo não repetem o número, que é o que o ack e o descarte de repetidas usam).
    #[must_use]
    pub fn next_chat_id(&self) -> u64 {
        use std::sync::atomic::Ordering;
        let now = super::util::unix_millis() as u64;
        let mut last = self.chat_stamp.load(Ordering::Relaxed);
        loop {
            let id = now.max(last.saturating_add(1));
            match self.chat_stamp.compare_exchange(last, id, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return id,
                Err(seen) => last = seen,
            }
        }
    }

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
            let client = self.client_with(peer, &k, options());
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

    /// Relays de terceiros para o primeiro salto: os relays guardados dos outros contatos, nunca o de `peer`, o `exit`
    /// nem o próprio. Qualquer contato serve; ele vê o seu IP e o do destino, mas não o conteúdo (cifrado de ponta a ponta).
    pub(crate) fn third_party_relays(&self, peer: &[u8; 32], exit: &RelayInfo) -> Vec<Candidate> {
        let known = self.known.lock_unpoisoned();
        let mut seen = std::collections::HashSet::new();
        known
            .iter()
            .filter(|(k, r)| *k != peer && r.addr != exit.addr && r.onion != self.mine.onion && seen.insert(r.addr.clone()))
            .map(|(_, r)| r.candidate())
            .collect()
    }

    /// Monta o cliente do circuito até `exit`. Opção desligada: ligação direta, um salto só, sem relay no meio
    /// (nem o próprio, por loopback). Ligada: o primeiro salto é sempre o relay de um terceiro; sem nenhum, o
    /// circuito não monta, em vez de cair para um caminho que mostre o IP.
    fn client_with(&self, peer: &[u8; 32], exit: &RelayInfo, mut options: ClientOptions) -> CircuitClient {
        options.direct = !self.two_relays;
        let mut relays = if self.two_relays { self.third_party_relays(peer, exit) } else { Vec::new() };
        relays.push(exit.candidate());
        let cfg = ManagerConfig { exit_addr: Some(exit.addr.clone()), direct: options.direct, ..Default::default() };
        CircuitClient::spawn_with(CircuitManager::new(cfg, &dest_for(peer), relays), Duration::from_secs(3), options)
    }

    /// Falha cedo, com o motivo, quando a opção 2 relays está ligada e não há relay de terceiro (veja [`DialError::NoRelays`]).
    fn check_relays(&self, peer: &[u8; 32]) -> Result<(), DialError> {
        if !self.two_relays {
            return Ok(());
        }
        let any = self.stored_for(peer).iter().any(|exit| !self.third_party_relays(peer, exit).is_empty());
        if any || self.stored_for(peer).is_empty() { Ok(()) } else { Err(DialError::NoRelays) }
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
        self.check_relays(&peer)?;
        let (found, tried_stored) = self.reachable_client(&peer, ClientOptions::default).await;
        let Some((client, k)) = found else { return Err(DialError::NoAddress { tried_stored }) };
        let call = call_linked(client, &self.secret, &PublicKey::from(peer), codec, wait, Some(&self.link)).await?;
        self.remember(&peer, k);
        Ok(call)
    }

    /// Transmite a tela da chamada `call` para `peer`, por um circuito só da tela (cover de
    /// [`SCREEN_SLOT`]). O caminho é o da opção "2 relays": ligada, por um relay de terceiro; desligada, direto.
    ///
    /// # Errors
    ///
    /// Como [`Self::dial`]: sem endereço do contato, sem relay de terceiro ou falha ao montar o circuito da tela.
    pub async fn share_screen(&self, peer: [u8; 32], call: &ActiveCall, wait: Duration) -> Result<ScreenShare, DialError> {
        self.share_screen_keys(peer, &call.next_share_keys(), wait).await
    }

    /// Como [`Self::share_screen`], com as chaves em vez da chamada (quem não tem mais a chamada em mãos
    /// guarda uma cópia de [`ActiveCall::next_share_keys`]). Quem transmite deve usar
    /// [`H264Encoder::bitrate_for`](crate::video::H264Encoder::bitrate_for) com o mesmo intervalo.
    ///
    /// # Errors
    ///
    /// Como [`Self::share_screen`].
    pub async fn share_screen_keys(&self, peer: [u8; 32], keys: &CallKeys, wait: Duration) -> Result<ScreenShare, DialError> {
        self.check_relays(&peer)?;
        let (found, tried_stored) = self.reachable_client(&peer, screen_client_options).await;
        let Some((client, used)) = found else { return Err(DialError::NoAddress { tried_stored }) };
        let share = share_screen_slot(client, keys, wait, SCREEN_SLOT).await?;
        self.remember(&peer, used);
        Ok(share)
    }

    /// Se a opção "2 relays" está ligada (veja [`NodeConfig::two_relays`]).
    #[must_use]
    pub fn two_relays(&self) -> bool {
        self.two_relays
    }
}
