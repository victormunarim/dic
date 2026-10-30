//! Entrega de avisos de endereço e de apresentações ao relay de um contato, e aplicação do aviso que chega.

use crate::sync::LockExt;
use std::time::Duration;

use x25519_dalek::PublicKey;

use crate::{
    circuit::manager::Event,
    media::update::{self, AddressRecord, Intro},
    phone::AddressUpdate,
    transport::ClientOptions,
};
use super::dialer::{Dialer};
use super::{RelayInfo, set_known};
use super::util::{ready_within};

impl Dialer {
    /// Aplica o endereço novo que `update.peer` mandou (já autenticado pelo telefone). `false`: o `seq` não é
    /// mais novo que o último visto e nada muda. O endereço antigo vai para o histórico e a dica de convite,
    /// que não era autenticada, perde o lugar.
    pub(super) fn apply_update(&self, update: &AddressUpdate) -> bool {
        {
            let mut seen = self.seen_seq.lock_unpoisoned();
            if seen.get(&update.peer).is_some_and(|s| *s >= update.record.seq) {
                return false;
            }
            seen.insert(update.peer, update.record.seq);
        }
        let r = &update.record;
        self.hints.lock_unpoisoned().remove(&update.peer);
        set_known(&self.known, &self.history, &update.peer, RelayInfo { addr: r.addr.clone(), onion: r.onion, link: r.link });
        true
    }

    /// Entrega `record` a `peer` no endereço que se tem dele (o guardado, a dica e os antigos) e espera a
    /// confirmação cifrada. `true`: o contato abriu o registro. Quem confirmou prova o endereço, que passa a ser o guardado.
    pub(super) async fn deliver_update(&self, peer: [u8; 32], record: &AddressRecord, wait: Duration) -> bool {
        let stored = self.stored_for(&peer);
        let pk = PublicKey::from(peer);
        let Ok(frame) = update::seal_update(&self.secret, &pk, record) else { return false };
        let Some((mut client, used)) = self.stored_client(&peer, stored, ClientOptions::default).await else { return false };
        let acked = client.send(&frame).await.is_ok()
            && tokio::time::timeout(wait, async {
                loop {
                    match client.next_event().await {
                        Some(Event::Data(d)) if update::open_ack(&self.secret, &pk, record.seq, &d) => return true,
                        Some(_) => {}
                        None => return false,
                    }
                }
            })
            .await
            .unwrap_or(false);
        client.stop();
        if acked {
            self.remember(&peer, used);
        }
        acked
    }

    /// Entrega a apresentação `intro` a `peer` e espera a confirmação cifrada. O endereço dele vem do código que
    /// se tem (o guardado ou a dica), da DHT ou das âncoras. `true`: ele abriu e confirmou.
    pub(super) async fn deliver_intro(&self, peer: [u8; 32], intro: &Intro, wait: Duration) -> bool {
        let Ok(frame) = update::seal_intro(&self.secret, &PublicKey::from(peer), intro) else { return false };
        self.deliver_confirmed(peer, &frame, intro.seq, wait).await
    }

    /// Manda a `peer` a mensagem de bate-papo `id` (de [`Dialer::next_chat_id`]) e espera a confirmação cifrada.
    /// `true`: ele abriu e confirmou. Uma tentativa só: o contato offline não recebe (não há caixa de correio
    /// para bate-papo), e quem chama mostra que não foi entregue. O texto passa de [`update::MAX_CHAT`] bytes ou
    /// tem caractere de controle: `false` sem rede.
    pub async fn send_chat(&self, peer: [u8; 32], id: u64, text: &str, wait: Duration) -> bool {
        let chat = update::Chat { id, text: text.to_string() };
        let Ok(frame) = update::seal_chat(&self.secret, &PublicKey::from(peer), &chat) else { return false };
        self.deliver_confirmed(peer, &frame, id, wait).await
    }

    /// Avisa `peer` de que esta pessoa o removeu (carimbo `stamp`, milissegundos Unix) e espera a confirmação.
    pub(super) async fn deliver_removal(&self, peer: [u8; 32], stamp: u64, wait: Duration) -> bool {
        let Ok(frame) = update::seal_removal(&self.secret, &PublicKey::from(peer), stamp) else { return false };
        self.deliver_confirmed(peer, &frame, stamp, wait).await
    }

    /// Manda `frame` a `peer` e espera o ack cifrado de `seq`.
    async fn deliver_confirmed(&self, peer: [u8; 32], frame: &crate::media::signal::Frame, seq: u64, wait: Duration) -> bool {
        let pk = PublicKey::from(peer);
        let (found, _) = self.reachable_client(&peer, ClientOptions::default).await;
        let Some((mut client, used)) = found else { return false };
        let acked = ready_within(&client, wait).await
            && client.send(frame).await.is_ok()
            && tokio::time::timeout(wait, async {
                loop {
                    match client.next_event().await {
                        Some(Event::Data(d)) if update::open_ack(&self.secret, &pk, seq, &d) => return true,
                        Some(_) => {}
                        None => return false,
                    }
                }
            })
            .await
            .unwrap_or(false);
        client.stop();
        if acked {
            self.remember(&peer, used);
        }
        acked
    }

    /// Entrega `frames` a `peer` por um circuito só, no endereço que se tem dele, e espera a confirmação de cada
    /// um (`acked(i, resposta)`). Devolve quantos foram confirmados. Quem confirmou prova o endereço, que passa a
    /// ser o guardado. Com `search` falso não procura na DHT nem nas âncoras (quem usa já tem o endereço do membro);
    /// verdadeiro, procura como numa chamada (para um contato que ainda não se falou).
    pub(crate) async fn deliver_batch(&self, peer: [u8; 32], frames: &[crate::media::signal::Frame], acked: impl Fn(usize, &[u8]) -> bool, wait: Duration, search: bool) -> usize {
        let found = if search { self.reachable_client(&peer, ClientOptions::default).await.0 } else { self.stored_client(&peer, self.stored_for(&peer), ClientOptions::default).await };
        let Some((mut client, used)) = found else { return 0 };
        let mut got = vec![false; frames.len()];
        for f in frames {
            if client.send(f).await.is_err() {
                break;
            }
        }
        let _ = tokio::time::timeout(wait, async {
            while got.iter().any(|g| !g) {
                match client.next_event().await {
                    Some(Event::Data(d)) => {
                        if let Some(i) = (0..frames.len()).find(|i| !got[*i] && acked(*i, &d)) {
                            got[i] = true;
                        }
                    }
                    Some(_) => {}
                    None => return,
                }
            }
        })
        .await;
        client.stop();
        let n = got.iter().filter(|g| **g).count();
        if n > 0 {
            self.remember(&peer, used);
        }
        n
    }
}
