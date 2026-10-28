//! Entrega do endereço novo aos contatos: fila de saída, ponto de encontro na DHT e servidores.

use crate::sync::LockExt;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{
    sync::Semaphore,
    task::{AbortHandle, JoinSet},
};
use x25519_dalek::PublicKey;

use crate::{
    media::update::{self, AddressRecord},
    phone::{self},
    server::net::ServerHub,
    rendezvous::{self},
};
use super::dialer::{Dialer};
use super::{Current, UPDATE_GIVE_UP, UPDATE_MAX_WAIT};
use super::util::{unix_millis, unix_secs};

/// Quanto esperar a confirmação de um contato depois de entregar o endereço.
const UPDATE_ACK_WAIT: Duration = Duration::from_secs(8);

/// Entregas de endereço feitas ao mesmo tempo (cada uma monta um circuito).
const UPDATE_PARALLEL: usize = 4;

/// De quanto em quanto tempo o endereço é regravado na DHT (os itens somem em cerca de 2 h sem renovação).
const DHT_REPUBLISH: Duration = Duration::from_secs(3600);

/// Depois de uma gravação que falhou na DHT, quanto esperar para tentar de novo.
const DHT_RETRY: Duration = Duration::from_secs(60);

/// Contatos gravados ou consultados na DHT por rodada.
const DHT_CONTACTS: usize = 32;

/// Fila de saída: entrega o registro de `current` a cada contato, no endereço que se tem dele, e insiste com
/// espera crescente até a confirmação (ou [`UPDATE_GIVE_UP`]). Um registro novo troca o de todos.
pub(super) async fn outbox(dialer: Dialer, contacts: phone::Contacts, current: Current, retry: Duration) {
    let slots = Arc::new(Semaphore::new(UPDATE_PARALLEL));
    let mut tasks = JoinSet::new();
    let mut live: HashMap<[u8; 32], AbortHandle> = HashMap::new();
    let mut announced = 0u64;
    loop {
        let record = current.lock_unpoisoned().clone();
        if let Some(record) = record {
            if record.seq != announced {
                for h in live.values() {
                    h.abort();
                }
                live.clear();
                announced = record.seq;
            }
            let keys: Vec<[u8; 32]> = contacts.lock_unpoisoned().clone();
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
pub(super) async fn rendezvous_publisher(dialer: Dialer, contacts: phone::Contacts, current: Current) {
    let (secret, me) = (dialer.secret.clone(), PublicKey::from(&dialer.secret));
    // Por contato: o `seq` do registro e o dia gravados, e quando gravar de novo.
    let mut state: HashMap<[u8; 32], (u64, u64, Instant)> = HashMap::new();
    let mut put_seq = 0i64;
    loop {
        let record = current.lock_unpoisoned().clone();
        if let Some(record) = record {
            let keys: Vec<[u8; 32]> = contacts.lock_unpoisoned().iter().take(DHT_CONTACTS).copied().collect();
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

/// Grava o registro de `current` para os membros de cada servidor (um item por servidor e dia, cifrado com o
/// segredo dele), com o mesmo ritmo do reencontro por contato. Quem está invisível não grava nada.
pub(super) async fn server_publisher(dialer: Dialer, hub: ServerHub, current: Current) {
    let me = *PublicKey::from(&dialer.secret).as_bytes();
    // Por servidor e época: o `seq` do registro e o dia gravados, e quando gravar de novo.
    let mut state: HashMap<([u8; 16], u16), (u64, u64, Instant)> = HashMap::new();
    let mut put_seq = 0i64;
    loop {
        let record = current.lock_unpoisoned().clone();
        let (visible, secrets) = hub.read(|s, _| (s.visible(), s.secrets()));
        if let Some(record) = record.filter(|_| visible) {
            state.retain(|k, _| secrets.iter().any(|(id, epoch, _)| (*id, *epoch) == *k));
            let period = unix_secs() / 86400;
            let due: Vec<_> = secrets.iter().filter(|(id, epoch, _)| state.get(&(*id, *epoch)).is_none_or(|(seq, day, at)| *seq != record.seq || *day != period || Instant::now() >= *at)).collect();
            let base = (unix_millis() as i64).max(put_seq + 1);
            put_seq = base + due.len() as i64;
            let mut signed = Vec::new();
            let mut sent = Vec::new();
            for (i, (id, epoch, secret)) in due.into_iter().enumerate() {
                let (seed, salt) = update::server_rendezvous(secret, id, &me, period);
                let Ok(value) = update::seal_server_record(secret, id, &me, &record) else { continue };
                signed.push(rendezvous::sign(&seed, &salt, &value, base + i as i64));
                sent.push((*id, *epoch));
            }
            let done = if signed.is_empty() { Vec::new() } else { dialer.store_put(&signed).await };
            for (k, ok) in sent.into_iter().zip(done) {
                state.insert(k, (record.seq, period, Instant::now() + if ok { DHT_REPUBLISH } else { DHT_RETRY }));
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Entrega `record` a `peer` até ele confirmar, o contato sair da lista ou a entrega ser abandonada.
async fn push_address(dialer: Dialer, contacts: phone::Contacts, peer: [u8; 32], record: AddressRecord, retry: Duration, slots: Arc<Semaphore>) {
    let (mut wait, give_up) = (retry, Instant::now() + UPDATE_GIVE_UP);
    loop {
        if !contacts.lock_unpoisoned().contains(&peer) {
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
