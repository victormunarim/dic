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
    media::update::{self, Intro, MailItem, MAIL_SLOTS},
    phone,
    rendezvous,
};
use super::dialer::Dialer;
use super::util::{unix_millis, unix_secs};
use super::{INTRO_ACK_WAIT, Introduced, RelayInfo, UPDATE_GIVE_UP, UPDATE_MAX_WAIT};

/// Onde chega o que outros mandam sobre contatos (apresentação direta ou da caixa): a fila para o app, as
/// apresentações nossas ainda em entrega e as lápides das remoções.
#[derive(Clone, Default)]
pub(super) struct Inbox {
    pub(super) introduced: Arc<Mutex<Vec<Introduced>>>,
    pub(super) introducing: Arc<Mutex<HashMap<[u8; 32], tokio::task::JoinHandle<()>>>>,
    pub(super) tombs: Arc<Mutex<HashMap<[u8; 32], u64>>>,
}

impl Inbox {
    /// Registra que `peer` foi removido com o carimbo `stamp` (ms Unix); guarda o maior e esquece o que já venceu.
    pub(super) fn bury(&self, peer: [u8; 32], stamp: u64) {
        let mut tombs = self.tombs.lock_unpoisoned();
        let slot = tombs.entry(peer).or_insert(stamp);
        *slot = (*slot).max(stamp);
        let floor = (unix_millis() as u64).saturating_sub(update::MAIL_TTL_MS);
        tombs.retain(|_, s| *s >= floor);
    }

    /// As lápides vivas, para o app guardar entre execuções.
    pub(super) fn tombs(&self) -> Vec<([u8; 32], u64)> {
        self.tombs.lock_unpoisoned().iter().map(|(k, v)| (*k, *v)).collect()
    }

    /// Entrega a apresentação de `peer` ao app. `false`: o carimbo dela é anterior à remoção de `peer` (convite
    /// velho, de antes de a pessoa ser removida), e é descartada. Se aceita, `peer` já tem este contato: a
    /// apresentação nossa a ele, se ainda estava em entrega, para de insistir.
    pub(super) fn receive(&self, peer: [u8; 32], intro: Intro) -> bool {
        if self.tombs.lock_unpoisoned().get(&peer).is_some_and(|stamp| intro.seq <= *stamp) {
            return false;
        }
        if let Some(task) = self.introducing.lock_unpoisoned().remove(&peer) {
            task.abort();
        }
        let mut queue = self.introduced.lock_unpoisoned();
        // Quem se apresenta de novo (a confirmação se perdeu) não entra duas vezes na fila.
        queue.retain(|i| i.peer != peer);
        queue.push(Introduced { peer, name: intro.name, reach: intro.reach });
        true
    }
}

/// Quem entrega a apresentação desta pessoa a `peer`: direto, e sem resposta na caixa dele e com contatos de confiança.
/// Serve a [`Node::introduce`](super::Node::introduce) (quem adiciona) e à resposta de quem achou um convite na caixa
/// ([`mail_reader`]), que por isso não depende de um `&Node`.
#[derive(Clone)]
pub(super) struct Introducer {
    pub(super) dialer: Dialer,
    pub(super) own: Arc<Mutex<Option<RelayInfo>>>,
    pub(super) contacts: phone::Contacts,
    pub(super) name: String,
    pub(super) tasks: Arc<Mutex<HashMap<[u8; 32], tokio::task::JoinHandle<()>>>>,
    pub(super) retry: Duration,
}

impl Introducer {
    /// Começa a entrega a `peer` (não duplica uma em andamento). `reply_to`: o item da caixa que está sendo respondido;
    /// além da caixa de `peer` e dos contatos de confiança, a resposta vai ao buraco de resposta dele
    /// ([`update::reply_master`]), onde os guardiões do convite a pegam e a regravam. Uma resposta dura até a entrega
    /// confirmar, o prazo vencer ou `retire`/`forget` a cancelar: ela não exige `peer` já estar nos contatos (a
    /// aplicação o adiciona logo depois de [`Node::take_introductions`](super::Node::take_introductions)).
    pub(super) fn start(&self, peer: [u8; 32], reply_to: Option<Vec<u8>>) {
        let mut tasks = self.tasks.lock_unpoisoned();
        if tasks.contains_key(&peer) {
            return;
        }
        let Self { dialer, own, contacts, name, tasks: done, retry } = self.clone();
        tasks.insert(
            peer,
            tokio::spawn(async move {
                let (mut wait, give_up) = (retry, Instant::now() + UPDATE_GIVE_UP);
                let seq = unix_millis() as u64;
                let mut kept: HashSet<[u8; 32]> = HashSet::new();
                loop {
                    if reply_to.is_none() && !contacts.lock_unpoisoned().contains(&peer) {
                        break;
                    }
                    // O relay é o de agora (pode ter mudado entre as tentativas); sem endereço que alguém de fora use, espera.
                    let reach = own.lock_unpoisoned().clone().map(|r| update::Reach { addr: r.addr, onion: r.onion, link: r.link });
                    if let Some(reach) = reach {
                        let intro = Intro { seq, name: name.clone(), reach };
                        if dialer.deliver_intro(peer, &intro, INTRO_ACK_WAIT).await {
                            break;
                        }
                        // Sem resposta direta: deixa o convite na caixa dele e com contatos de confiança, que o entregam
                        // quando ele voltar (veja `mail`).
                        dialer.leave_mail(peer, &intro, &contacts, &mut kept, reply_to.as_deref()).await;
                    }
                    if Instant::now() >= give_up {
                        break;
                    }
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(UPDATE_MAX_WAIT);
                }
                done.lock_unpoisoned().remove(&peer);
            }),
        );
    }
}

/// Quantos contatos de confiança guardam a mesma apresentação.
pub(super) const KEEPERS: usize = 10;

/// Para quantos de seus contatos de confiança o guardião repassa o que recebeu direto do autor.
pub(super) const FORWARDS: usize = 3;

/// Estado do repasse de uma apresentação: quem já confirmou e quando tentar de novo.
type Passed = (HashSet<[u8; 32]>, Instant);

/// Entre tentativas de repasse: o vizinho pode não ter este guardião como contato e nunca confirmar.
const FORWARD_RETRY: Duration = Duration::from_secs(300);

/// Quanto esperar o guardião confirmar os dois pedaços.
const CUSTODY_ACK_WAIT: Duration = Duration::from_secs(8);

/// De quanto em quanto tempo o guardião regrava (os itens da DHT somem em cerca de 2 h sem renovação).
const REPUBLISH: Duration = Duration::from_secs(3600);

/// Depois de uma gravação que falhou, quanto esperar para tentar de novo.
const RETRY: Duration = Duration::from_secs(60);

/// Esperas da leitura logo depois de abrir o nó (a DHT ainda está entrando na rede); depois vale `NodeConfig::mail_poll`.
const FIRST_READS: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(60)];

impl Dialer {
    /// Grava no item de hoje da caixa de `peer` o item `mail`, na DHT e nas âncoras. `true`: algum lugar guardou.
    pub(super) async fn put_mail(&self, master: &[u8; 32], mail: &[u8], seq: i64) -> bool {
        let (seed, salt) = update::mailbox_target(master, unix_secs() / 86400, update::mail_slot(mail));
        let item = rendezvous::sign(&seed, &salt, mail, seq);
        self.store_put(&[item]).await.first().copied().unwrap_or(false)
    }

    /// Deixa com `keeper` o item `mail` e a chave da caixa, em vários quadros. `true`: ele confirmou todos.
    pub(super) async fn deliver_custody(&self, keeper: [u8; 32], id: u64, master: &[u8; 32], mail: &[u8]) -> bool {
        let pk = PublicKey::from(keeper);
        let Ok(frames) = update::seal_custody(&self.secret, &pk, id, master, mail) else { return false };
        let acked = |i: usize, d: &[u8]| update::open_ack(&self.secret, &pk, id.wrapping_add(i as u64), d);
        self.deliver_batch(keeper, &frames, acked, CUSTODY_ACK_WAIT, true).await == frames.len()
    }

    /// Deposita a apresentação `intro` para `peer` (que está offline): grava na caixa dele e deixa com até
    /// [`KEEPERS`] contatos de confiança. `kept` guarda quem já confirmou, para não repetir a cada tentativa.
    /// `reply_to`: o item que esta apresentação responde (veja [`Introducer::start`]); grava também no buraco de resposta dele.
    pub(super) async fn leave_mail(&self, peer: [u8; 32], intro: &Intro, contacts: &phone::Contacts, kept: &mut HashSet<[u8; 32]>, reply_to: Option<&[u8]>) {
        let Ok(mail) = update::seal_intro(&self.secret, &PublicKey::from(peer), intro) else { return };
        if let Some(item) = reply_to {
            self.put_reply(item, &mail).await;
        }
        self.leave_item(peer, intro.seq, &mail, true, contacts, kept).await;
    }

    /// Grava `mail`, a resposta ao item `item`, no buraco de resposta de `item`, hoje e na DHT e nas âncoras.
    async fn put_reply(&self, item: &[u8], mail: &[u8]) -> bool {
        let (seed, salt) = update::mailbox_target(&update::reply_master(item), unix_secs() / 86400, update::mail_slot(mail));
        let signed = rendezvous::sign(&seed, &salt, mail, unix_millis() as i64);
        self.store_put(&[signed]).await.first().copied().unwrap_or(false)
    }

    /// As respostas que alguém deixou no buraco de resposta de `item` (ontem, hoje e amanhã, como na caixa) e que são
    /// apresentações inteiras. O guardião não as abre: só o autor de `item` abre.
    async fn read_replies(&self, item: &[u8]) -> Vec<Vec<u8>> {
        let master = update::reply_master(item);
        let today = unix_secs() / 86400;
        let targets: Vec<([u8; 32], [u8; 32])> = [today.saturating_sub(1), today, today + 1]
            .into_iter()
            .flat_map(|period| (0..MAIL_SLOTS).map(move |slot| (period, slot)))
            .map(|(period, slot)| update::mailbox_target(&master, period, slot))
            .map(|(seed, salt)| (rendezvous::public_key(&seed), salt))
            .collect();
        self.lookup_values(&targets, &[0u8; 32]).await
    }

    /// Deposita o convite de servidor `raw` (veja `server::invite`) para `peer`, como [`leave_mail`](Self::leave_mail).
    /// `id` é o carimbo (ms) do convite. `put`: gravar também na DHT (o guardião regrava depois de hora em hora).
    pub(crate) async fn leave_invite_mail(&self, peer: [u8; 32], id: u64, raw: &[u8], put: bool, contacts: &phone::Contacts, kept: &mut HashSet<[u8; 32]>) {
        let Some(item) = update::invite_mail_item(raw) else { return };
        self.leave_item(peer, id, &item, put, contacts, kept).await;
    }

    async fn leave_item(&self, peer: [u8; 32], id: u64, mail: &[u8], put: bool, contacts: &phone::Contacts, kept: &mut HashSet<[u8; 32]>) {
        let master = update::mailbox_master(&peer);
        if put {
            self.put_mail(&master, mail, unix_millis() as i64).await;
        }
        let list: Vec<[u8; 32]> = contacts.lock_unpoisoned().clone();
        let todo: Vec<[u8; 32]> = list.into_iter().filter(|k| *k != peer && self.is_trusted(k) && !kept.contains(k)).take(KEEPERS.saturating_sub(kept.len())).collect();
        let done = futures::future::join_all(todo.iter().map(|k| self.deliver_custody(*k, id, &master, mail))).await;
        kept.extend(todo.into_iter().zip(done).filter(|(_, ok)| *ok).map(|(k, _)| k));
    }

    /// O que alguém deixou na caixa desta pessoa (ontem, hoje e amanhã: o relógio de um dos dois pode estar errado) e
    /// ainda vale: apresentações e convites de servidor.
    pub(super) async fn read_mail(&self) -> Vec<(Vec<u8>, MailItem)> {
        let master = update::mailbox_master(PublicKey::from(&self.secret).as_bytes());
        let today = unix_secs() / 86400;
        let targets: Vec<([u8; 32], [u8; 32])> = [today.saturating_sub(1), today, today + 1]
            .into_iter()
            .flat_map(|period| (0..MAIL_SLOTS).map(move |slot| (period, slot)))
            .map(|(period, slot)| update::mailbox_target(&master, period, slot))
            .map(|(seed, salt)| (rendezvous::public_key(&seed), salt))
            .collect();
        let now = unix_millis() as u64;
        self.lookup_values(&targets, &[0u8; 32]).await.into_iter().filter_map(|v| update::open_mail_item(&self.secret, &v, now).map(|item| (v, item))).collect()
    }
}

/// O guardião: regrava na DHT, a cada hora e quando o dia vira, o que contatos deixaram aqui. Também descarta o
/// que passou do prazo e recolhe as respostas que o destinatário deixou no buraco de cada apresentação segurada (elas
/// passam a ser regravadas na caixa do autor, mesmo que o destinatário e os contatos dele saiam do ar). `reply_poll`: de
/// quanto em quanto tempo olhar o buraco de cada apresentação segurada ([`NodeConfig::mail_poll`](super::NodeConfig::mail_poll)).
pub(super) async fn mail_keeper(dialer: Dialer, custody: Arc<Mutex<Custody>>, contacts: phone::Contacts, reply_poll: Duration) {
    // Por apresentação guardada: o dia gravado e quando gravar de novo.
    let mut state: HashMap<([u8; 32], u64), (u64, Instant)> = HashMap::new();
    let mut put_seq = 0i64;
    // Repasse (um salto): por apresentação, quem já confirmou e quando tentar de novo.
    let mut passed: HashMap<([u8; 32], u64), Passed> = HashMap::new();
    // Por apresentação guardada: quando olhar o buraco de resposta de novo.
    let mut replies: HashMap<([u8; 32], u64), Instant> = HashMap::new();
    loop {
        let now = unix_millis() as u64;
        let held = {
            let mut c = custody.lock_unpoisoned();
            c.expire(now);
            c.held(now)
        };
        state.retain(|k, _| held.iter().any(|h| (h.from, h.id) == *k));
        passed.retain(|k, _| held.iter().any(|h| (h.from, h.id) == *k));
        replies.retain(|k, _| held.iter().any(|h| (h.from, h.id) == *k));
        let polls: Vec<_> = held.iter().filter(|h| h.mail.first() == Some(&update::TAG_INTRO) && replies.get(&(h.from, h.id)).is_none_or(|at| Instant::now() >= *at)).collect();
        for h in polls {
            replies.insert((h.from, h.id), Instant::now() + reply_poll);
            // A resposta é para o autor do item, que é quem tem a caixa; o guardião a regrava lá.
            let Some(author) = update::mail_author(&h.mail) else { continue };
            let master = update::mailbox_master(&author);
            for reply in dialer.read_replies(&h.mail).await {
                custody.lock_unpoisoned().accept_reply(h.from, h.id, master, &reply, unix_millis() as u64);
            }
        }
        let held = custody.lock_unpoisoned().held(unix_millis() as u64);
        let day = unix_secs() / 86400;
        let due: Vec<_> = held.iter().filter(|h| state.get(&(h.from, h.id)).is_none_or(|(d, at)| *d != day || Instant::now() >= *at)).collect();
        for h in due {
            // A DHT recusa o mesmo `seq` com outro valor, e o quadro é sempre o mesmo, mas outro guardião pode ter gravado antes: `seq` crescente.
            put_seq = (unix_millis() as i64).max(put_seq + 1);
            let ok = dialer.put_mail(&h.master, &h.mail, put_seq).await;
            state.insert((h.from, h.id), (day, Instant::now() + if ok { REPUBLISH } else { RETRY }));
        }
        // Repasse: só o que veio direto do autor, para não passar de um salto (o guardião de segunda mão vê `from` ≠ autor).
        for h in held.iter().filter(|h| update::mail_author(&h.mail) == Some(h.from)) {
            let entry = passed.entry((h.from, h.id)).or_insert_with(|| (HashSet::new(), Instant::now()));
            if entry.0.len() >= FORWARDS || Instant::now() < entry.1 {
                continue;
            }
            let list: Vec<[u8; 32]> = contacts.lock_unpoisoned().clone();
            let todo: Vec<[u8; 32]> = list.into_iter().filter(|k| *k != h.from && dialer.is_trusted(k) && !entry.0.contains(k)).take(FORWARDS - entry.0.len()).collect();
            let done = futures::future::join_all(todo.iter().map(|k| dialer.deliver_custody(*k, h.id, &h.master, &h.mail))).await;
            entry.0.extend(todo.into_iter().zip(done).filter(|(_, ok)| *ok).map(|(k, _)| k));
            entry.1 = Instant::now() + FORWARD_RETRY;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// O destinatário: lê a própria caixa logo depois de abrir e a cada `poll`. O que acha vira apresentação recebida,
/// como a que chega pelo telefone (vale também de quem já é contato: é a aceitação de volta). Convite de servidor vai
/// para `invites` (que confere o autor e o destinatário).
pub(super) async fn mail_reader(dialer: Dialer, inbox: Inbox, introducer: Introducer, poll: Duration, invites: impl Fn(&[u8])) {
    let mut seen: HashSet<([u8; 32], u64)> = HashSet::new();
    let mut seen_invites: HashSet<Vec<u8>> = HashSet::new();
    let mut waits = FIRST_READS.into_iter().map(|d| d.min(poll)).chain(std::iter::repeat(poll));
    loop {
        tokio::time::sleep(waits.next().unwrap_or(poll)).await;
        for (raw, item) in dialer.read_mail().await {
            match item {
                // Quem já é contato também vale: é a aceitação de volta de quem recebeu o nosso convite.
                MailItem::Intro(peer, intro) => {
                    if seen.insert((peer, intro.seq)) {
                        let known = introducer.contacts.lock_unpoisoned().contains(&peer);
                        // Quem ainda não era contato e foi aceito: responde, para o autor saber que o convite chegou (se
                        // já era, esta é a resposta dele, ou os dois se adicionaram: não há o que responder).
                        if inbox.receive(peer, intro) && !known {
                            introducer.start(peer, Some(raw));
                        }
                    }
                }
                MailItem::Invite(raw) => {
                    if !seen_invites.contains(&raw) {
                        invites(&raw);
                        seen_invites.insert(raw);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intro(seq: u64) -> Intro {
        Intro { seq, name: "Ana".into(), reach: update::Reach { addr: "127.0.0.1:4001".into(), onion: [1; 32], link: [2; 32] } }
    }

    #[test]
    fn a_tombstone_refuses_an_older_or_equal_invitation() {
        let (inbox, peer, now) = (Inbox::default(), [7; 32], unix_millis() as u64);
        inbox.bury(peer, now);
        assert!(!inbox.receive(peer, intro(now - 1)));
        assert!(!inbox.receive(peer, intro(now)));
        assert!(inbox.introduced.lock_unpoisoned().is_empty());
        assert!(inbox.receive(peer, intro(now + 1)), "convite posterior à remoção vale");
        assert_eq!(inbox.introduced.lock_unpoisoned().len(), 1);
    }

    #[test]
    fn the_tombstone_keeps_the_latest_stamp_and_only_for_that_peer() {
        let (inbox, now) = (Inbox::default(), unix_millis() as u64);
        inbox.bury([1; 32], now);
        inbox.bury([1; 32], now - 5);
        assert_eq!(inbox.tombs(), vec![([1; 32], now)]);
        assert!(inbox.receive([2; 32], intro(1)));
    }

    #[test]
    fn an_expired_tombstone_is_forgotten() {
        let inbox = Inbox::default();
        inbox.bury([1; 32], 1);
        inbox.bury([2; 32], unix_millis() as u64);
        assert_eq!(inbox.tombs().len(), 1, "a de 1970 venceu");
    }

    #[tokio::test]
    async fn receiving_the_introduction_back_stops_ours() {
        let (inbox, peer) = (Inbox::default(), [3; 32]);
        let task = tokio::spawn(std::future::pending::<()>());
        inbox.introducing.lock_unpoisoned().insert(peer, task);
        assert!(inbox.receive(peer, intro(10)));
        assert!(inbox.introducing.lock_unpoisoned().is_empty());
    }
}
