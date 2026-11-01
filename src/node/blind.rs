//! Opção "2 relays" no nó, chamada de voz sem IP: quem atende espera em pontos de introdução e publica onde; quem
//! liga acha os pontos na presença v2 e chega por introdução e encontro ([`crate::meet`]). Aqui só se junta o que a
//! biblioteca já tem: os pontos vêm do `HiddenService`, o registro é o da [`crate::presence`] v2, e o armazenamento é o
//! do reencontro (DHT Mainline, âncoras e gateway), então nenhuma infraestrutura nova.

use crate::sync::LockExt;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rand_core::{OsRng, RngCore};
use x25519_dalek::PublicKey;

use crate::{
    DEFAULT_EPOCH_SECS,
    circuit::manager::Candidate,
    epoch_of,
    meet::{HiddenService, IntroTarget, ServiceConfig, meet_with},
    pairing::PresenceKeys,
    phone::Contacts,
    presence::{self, IntroPointV2},
    rendezvous,
    transport::{CircuitClient, ClientOptions},
};
use super::dialer::{DialError, Dialer};
use super::util::{unix_millis, unix_secs};

/// Onde quem atende está esperando agora (vazio até o serviço subir): o que o publicador põe na presença.
pub(super) type Points = Arc<Mutex<Vec<IntroTarget>>>;

/// Por contato: as `auth_key` e a época gravadas, e quando gravar de novo.
type Written = (Vec<[u8; 32]>, u64, Instant);

/// Menos que isto e não há como separar o primeiro salto do ponto: o serviço nem sobe e quem liga não liga.
const POOL_MIN: usize = 2;

/// Pontos de introdução que quem atende mantém (cabem 3 na presença v2).
const INTRO_POINTS: usize = 3;

/// De quanto em quanto tempo o serviço é refeito com pontos novos, para um ponto morto não ficar para sempre.
const SERVICE_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// Espera entre duas tentativas de subir o serviço (sem relays de terceiros ou sem ponto que responda).
const SERVICE_RETRY: Duration = Duration::from_secs(10);

/// De quanto em quanto tempo o registro é regravado (os itens da DHT somem em cerca de 2 h sem renovação).
const PUBLISH_EVERY: Duration = Duration::from_secs(20 * 60);

/// Depois de uma gravação que falhou, quanto esperar para tentar de novo.
const PUBLISH_RETRY: Duration = Duration::from_secs(60);

/// Contatos que recebem a presença por rodada.
const CONTACTS: usize = 32;

impl Dialer {
    /// Relays de terceiros para primeiro salto e para pontos: os guardados dos contatos, sem o de `except`
    /// (quem liga não pode entrar pelo relay de quem recebe, que veria o IP dele) e sem o próprio.
    pub(super) fn hidden_pool(&self, except: Option<&[u8; 32]>) -> Vec<Candidate> {
        let known = self.known.lock_unpoisoned();
        let mut seen = std::collections::HashSet::new();
        known
            .iter()
            .filter(|(k, r)| Some(*k) != except && r.onion != self.mine.onion && seen.insert(r.addr.clone()))
            .map(|(_, r)| r.candidate())
            .collect()
    }

    /// Os pontos de introdução que `peer` deixou para nós: consulta a época anterior, a atual e a seguinte (o relógio
    /// de um dos dois pode estar errado) e fica com a mais nova que abre.
    async fn hidden_points(&self, peer: &[u8; 32]) -> Vec<IntroPointV2> {
        let keys = PresenceKeys::derive(&self.secret, &PublicKey::from(*peer));
        let owner = keys.owner_signing_key(peer).verifying_key();
        let now = epoch_of(unix_secs(), DEFAULT_EPOCH_SECS);
        let epochs = [now, now.saturating_sub(1), now.saturating_add(1)];
        let targets: Vec<([u8; 32], [u8; 32])> = epochs
            .iter()
            .map(|e| {
                let (seed, salt) = keys.dht_target(*e, peer);
                (rendezvous::public_key(&seed), salt)
            })
            .collect();
        let values = self.lookup_values(&targets, peer).await;
        epochs
            .iter()
            .find_map(|e| {
                let id = keys.lookup_id(*e);
                values.iter().find_map(|v| presence::open_v2(&keys, &owner, &id, v).ok().filter(|p| !p.is_empty()))
            })
            .unwrap_or_default()
    }

    /// O circuito até `peer` por introdução e encontro, pronto para `call_linked`. Quem liga nunca aprende o endereço de
    /// quem recebe e quem recebe nunca aprende o de quem liga. Sem os pontos dele não há outro caminho: não cai para o
    /// relay guardado, que mostraria o IP.
    ///
    /// # Errors
    ///
    /// [`DialError::NoRelays`] sem relay de terceiro; [`DialError::NoHiddenPoints`] se o contato não publicou pontos
    /// (ou a opção está desligada nele); [`DialError::Meet`] se nenhum ponto levou ao encontro.
    pub(super) async fn hidden_client(&self, peer: &[u8; 32], options: fn() -> ClientOptions) -> Result<CircuitClient, DialError> {
        let pool = self.hidden_pool(Some(peer));
        if pool.len() < POOL_MIN {
            return Err(DialError::NoRelays);
        }
        let points = self.hidden_points(peer).await;
        if points.is_empty() {
            return Err(DialError::NoHiddenPoints);
        }
        let callee = PublicKey::from(*peer);
        let mut last = None;
        for point in &points {
            let via = IntroTarget::from(point);
            // O encontro é em outro relay que o do ponto, escolhido entre os de quem liga.
            let others: Vec<&Candidate> = pool.iter().filter(|c| c.addr != via.relay.addr).collect();
            let Some(rv) = others.get(OsRng.next_u64() as usize % others.len().max(1)) else { continue };
            match meet_with(&self.secret, &callee, &via, rv, &pool, options()).await {
                Ok(client) => return Ok(client),
                Err(e) => last = Some(e),
            }
        }
        Err(last.map_or(DialError::NoHiddenPoints, DialError::Meet))
    }
}

/// Mantém o serviço de quem atende de pé: sobe nos relays de terceiros, expõe os pontos em `points` e o refaz a cada
/// [`SERVICE_LIFETIME`]. Largar o serviço derruba os circuitos de espera.
pub(super) async fn hidden_service(dialer: Dialer, phone: SocketAddr, contacts: Contacts, points: Points) {
    loop {
        let mut pool = dialer.hidden_pool(None);
        if pool.len() < POOL_MIN {
            tokio::time::sleep(SERVICE_RETRY).await;
            continue;
        }
        // Pontos diferentes a cada rodada: embaralha antes de tomar os primeiros.
        for i in (1..pool.len()).rev() {
            pool.swap(i, OsRng.next_u64() as usize % (i + 1));
        }
        let intros = pool.iter().take(INTRO_POINTS).cloned().collect();
        let cfg = ServiceConfig { secret: dialer.secret.clone(), phone, contacts: contacts.clone(), pool, intros };
        match HiddenService::start(cfg).await {
            Ok(service) => {
                *points.lock_unpoisoned() = service.points().to_vec();
                tokio::time::sleep(SERVICE_LIFETIME).await;
                points.lock_unpoisoned().clear();
                drop(service);
            }
            Err(_) => tokio::time::sleep(SERVICE_RETRY).await,
        }
    }
}

/// Grava a presença v2 para cada contato: ao mudarem os pontos, ao entrar um contato novo, ao virar a época (o alvo
/// muda) e a cada [`PUBLISH_EVERY`]. Cada gravação leva um `seq` novo, porque a DHT recusa o mesmo `seq` com outro valor.
pub(super) async fn hidden_publisher(dialer: Dialer, contacts: Contacts, points: Points) {
    let me = *PublicKey::from(&dialer.secret).as_bytes();
    let mut state: HashMap<[u8; 32], Written> = HashMap::new();
    let mut put_seq = 0i64;
    loop {
        let current: Vec<IntroTarget> = points.lock_unpoisoned().clone();
        if !current.is_empty() {
            let marks: Vec<[u8; 32]> = current.iter().map(|p| p.auth_key).collect();
            let epoch = epoch_of(unix_secs(), DEFAULT_EPOCH_SECS);
            let wire: Vec<IntroPointV2> = current.iter().map(IntroPointV2::from).collect();
            let keys: Vec<[u8; 32]> = contacts.lock_unpoisoned().iter().take(CONTACTS).copied().collect();
            state.retain(|k, _| keys.contains(k));
            let due: Vec<[u8; 32]> = keys.into_iter().filter(|k| state.get(k).is_none_or(|(m, e, at)| *m != marks || *e != epoch || Instant::now() >= *at)).collect();
            let base = (unix_millis() as i64).max(put_seq + 1);
            put_seq = base + due.len() as i64;
            let (mut signed, mut sent) = (Vec::new(), Vec::new());
            for (i, k) in due.iter().enumerate() {
                let pair = PresenceKeys::derive(&dialer.secret, &PublicKey::from(*k));
                let (seed, salt) = pair.dht_target(epoch, &me);
                let Ok(value) = presence::seal_v2(&pair, &pair.owner_signing_key(&me), &pair.lookup_id(epoch), &wire) else { continue };
                signed.push(rendezvous::sign(&seed, &salt, &value, base + i as i64));
                sent.push(*k);
            }
            let done = if signed.is_empty() { Vec::new() } else { dialer.store_put(&signed).await };
            for (k, ok) in sent.into_iter().zip(done) {
                state.insert(k, (marks.clone(), epoch, Instant::now() + if ok { PUBLISH_EVERY } else { PUBLISH_RETRY }));
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
