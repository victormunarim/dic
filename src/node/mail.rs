//! Caixa de correio da apresentação: deixa o convite com contatos de confiança e na DHT para quem estava offline
//! (veja `docs/ENCONTRO.md`). Quem se apresenta deposita; o guardião regrava de hora em hora; o destinatário lê.

use crate::sync::LockExt;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use x25519_dalek::PublicKey;

use crate::{
    mailbox::Custody,
    media::update::{self, CUSTODY_PARTS, Intro, MAIL_SLOTS},
    phone,
    rendezvous,
};
use super::dialer::Dialer;
use super::util::{unix_millis, unix_secs};
use super::Introduced;

/// Quantos contatos de confiança guardam a mesma apresentação.
pub(super) const KEEPERS: usize = 10;

/// Quanto esperar o guardião confirmar os dois pedaços.
const CUSTODY_ACK_WAIT: Duration = Duration::from_secs(8);

/// De quanto em quanto tempo o guardião regrava (os itens da DHT somem em cerca de 2 h sem renovação).
const REPUBLISH: Duration = Duration::from_secs(3600);

/// Depois de uma gravação que falhou, quanto esperar para tentar de novo.
const RETRY: Duration = Duration::from_secs(60);

/// Esperas da leitura logo depois de abrir o nó (a DHT ainda está entrando na rede); depois vale `NodeConfig::mail_poll`.
const FIRST_READS: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(60)];

impl Dialer {
    /// Grava no item de hoje da caixa de `peer` o quadro `mail`, na DHT e nas âncoras. `true`: algum lugar guardou.
    pub(super) async fn put_mail(&self, master: &[u8; 32], mail: &crate::media::signal::Frame, seq: i64) -> bool {
        let (seed, salt) = update::mailbox_target(master, unix_secs() / 86400, update::mail_slot(mail));
        let item = rendezvous::sign(&seed, &salt, mail, seq);
        self.store_put(&[item]).await.first().copied().unwrap_or(false)
    }

    /// Deixa com `keeper` a apresentação `mail` e a chave da caixa, em dois quadros. `true`: ele confirmou os dois.
    pub(super) async fn deliver_custody(&self, keeper: [u8; 32], id: u64, master: &[u8; 32], mail: &crate::media::signal::Frame) -> bool {
        let pk = PublicKey::from(keeper);
        let Ok(frames) = update::seal_custody(&self.secret, &pk, id, master, mail) else { return false };
        let acked = |i: usize, d: &[u8]| update::open_ack(&self.secret, &pk, id.wrapping_add(i as u64), d);
        self.deliver_batch(keeper, &frames, acked, CUSTODY_ACK_WAIT, true).await == CUSTODY_PARTS
    }

    /// Deposita a apresentação `intro` para `peer` (que está offline): grava na caixa dele e deixa com até
    /// [`KEEPERS`] contatos de confiança. `kept` guarda quem já confirmou, para não repetir a cada tentativa.
    pub(super) async fn leave_mail(&self, peer: [u8; 32], intro: &Intro, contacts: &phone::Contacts, kept: &mut HashSet<[u8; 32]>) {
        let Ok(mail) = update::seal_intro(&self.secret, &PublicKey::from(peer), intro) else { return };
        let master = update::mailbox_master(&peer);
        self.put_mail(&master, &mail, unix_millis() as i64).await;
        let list: Vec<[u8; 32]> = contacts.lock_unpoisoned().clone();
        let todo: Vec<[u8; 32]> = list.into_iter().filter(|k| *k != peer && self.is_trusted(k) && !kept.contains(k)).take(KEEPERS.saturating_sub(kept.len())).collect();
        let done = futures::future::join_all(todo.iter().map(|k| self.deliver_custody(*k, intro.seq, &master, &mail))).await;
        kept.extend(todo.into_iter().zip(done).filter(|(_, ok)| *ok).map(|(k, _)| k));
    }

    /// As apresentações que alguém deixou na caixa desta pessoa (ontem, hoje e amanhã: o relógio de um dos dois pode
    /// estar errado) e que ainda valem.
    pub(super) async fn read_mail(&self) -> Vec<([u8; 32], Intro)> {
        let master = update::mailbox_master(PublicKey::from(&self.secret).as_bytes());
        let today = unix_secs() / 86400;
        let targets: Vec<([u8; 32], [u8; 32])> = [today.saturating_sub(1), today, today + 1]
            .into_iter()
            .flat_map(|period| (0..MAIL_SLOTS).map(move |slot| (period, slot)))
            .map(|(period, slot)| update::mailbox_target(&master, period, slot))
            .map(|(seed, salt)| (rendezvous::public_key(&seed), salt))
            .collect();
        let now = unix_millis() as u64;
        self.lookup_values(&targets, &[0u8; 32]).await.iter().filter_map(|v| update::open_mail(&self.secret, v, now)).collect()
    }
}

/// O guardião: regrava na DHT, a cada hora e quando o dia vira, o que contatos deixaram aqui. Também descarta o
/// que passou do prazo.
pub(super) async fn mail_keeper(dialer: Dialer, custody: Arc<Mutex<Custody>>) {
    // Por apresentação guardada: o dia gravado e quando gravar de novo.
    let mut state: HashMap<([u8; 32], u64), (u64, Instant)> = HashMap::new();
    let mut put_seq = 0i64;
    loop {
        let now = unix_millis() as u64;
        let held = {
            let mut c = custody.lock_unpoisoned();
            c.expire(now);
            c.held(now)
        };
        state.retain(|k, _| held.iter().any(|h| (h.from, h.id) == *k));
        let day = unix_secs() / 86400;
        let due: Vec<_> = held.iter().filter(|h| state.get(&(h.from, h.id)).is_none_or(|(d, at)| *d != day || Instant::now() >= *at)).collect();
        for h in due {
            // A DHT recusa o mesmo `seq` com outro valor, e o quadro é sempre o mesmo, mas outro guardião pode ter gravado antes: `seq` crescente.
            put_seq = (unix_millis() as i64).max(put_seq + 1);
            let ok = dialer.put_mail(&h.master, &h.mail, put_seq).await;
            state.insert((h.from, h.id), (day, Instant::now() + if ok { REPUBLISH } else { RETRY }));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// O destinatário: lê a própria caixa logo depois de abrir e a cada `poll`. O que acha vira apresentação recebida,
/// como a que chega pelo telefone, se o autor ainda não é contato.
pub(super) async fn mail_reader(dialer: Dialer, contacts: phone::Contacts, introduced: Arc<Mutex<Vec<Introduced>>>, poll: Duration) {
    let mut seen: HashSet<([u8; 32], u64)> = HashSet::new();
    let mut waits = FIRST_READS.into_iter().map(|d| d.min(poll)).chain(std::iter::repeat(poll));
    loop {
        tokio::time::sleep(waits.next().unwrap_or(poll)).await;
        for (peer, intro) in dialer.read_mail().await {
            if contacts.lock_unpoisoned().contains(&peer) || !seen.insert((peer, intro.seq)) {
                continue;
            }
            let mut queue = introduced.lock_unpoisoned();
            queue.retain(|i| i.peer != peer);
            queue.push(Introduced { peer, name: intro.name, reach: intro.reach });
        }
    }
}
