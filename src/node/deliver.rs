//! Entrega de avisos de endereço e de apresentações ao relay de um contato, e aplicação do aviso que chega.

use crate::sync::LockExt;
use std::time::Duration;

/// Quadros mandados de uma vez antes de esperar o cover esvaziar (a fila dele guarda 16 células).
const PACE_BURST: usize = 8;

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

    /// Manda a `peer` um quadro de controle da transferência de arquivo (oferta, resposta, cancelamento) e espera a
    /// confirmação. Uma tentativa só; `false` se o contato está offline.
    pub async fn send_file_frame(&self, peer: [u8; 32], frame: &update::FileFrame, wait: Duration) -> bool {
        let Ok(sealed) = update::seal_file(&self.secret, &PublicKey::from(peer), frame) else { return false };
        self.deliver_confirmed(peer, &sealed, frame.ack_seq(), wait).await
    }

    /// Manda os pedaços `chunks` (só `Chunk`) por um circuito só, no ritmo do circuito de tela, e devolve quais o contato
    /// confirmou. O que não voltou o chamador reenvia numa rodada seguinte.
    pub async fn send_file_chunks(&self, peer: [u8; 32], chunks: &[update::FileFrame], wait: Duration) -> Vec<bool> {
        let pk = PublicKey::from(peer);
        let sealed: Vec<_> = chunks.iter().map(|c| update::seal_file(&self.secret, &pk, c)).collect();
        if sealed.iter().any(Result::is_err) {
            return vec![false; chunks.len()];
        }
        let frames: Vec<_> = sealed.into_iter().flatten().collect();
        let seqs: Vec<u64> = chunks.iter().map(update::FileFrame::ack_seq).collect();
        self.deliver_batch_with(peer, &frames, |i, d| seqs.get(i).is_some_and(|s| update::open_ack(&self.secret, &pk, *s, d)), wait, true, (crate::phone::screen_client_options, crate::phone::SCREEN_SLOT)).await
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
        self.deliver_batch_with(peer, frames, acked, wait, search, (ClientOptions::default, Duration::ZERO)).await.iter().filter(|g| **g).count()
    }

    /// Como [`Dialer::deliver_batch`], com as opções do circuito e o `pace` entre quadros (zero = sem espera) à escolha e o resultado de cada quadro (`true` = confirmado).
    pub(crate) async fn deliver_batch_with(&self, peer: [u8; 32], frames: &[crate::media::signal::Frame], acked: impl Fn(usize, &[u8]) -> bool, wait: Duration, search: bool, (options, pace): (fn() -> ClientOptions, Duration)) -> Vec<bool> {
        let found = if search { self.reachable_client(&peer, options).await.0 } else { self.stored_client(&peer, self.stored_for(&peer), options).await };
        let Some((mut client, used)) = found else { return vec![false; frames.len()] };
        let mut got = vec![false; frames.len()];
        for (n, f) in frames.iter().enumerate() {
            if client.send(f).await.is_err() {
                break;
            }
            // A fila do cover guarda poucas células e descarta as mais velhas; sem este ritmo, quase todo o lote se perderia.
            if !pace.is_zero() && n % PACE_BURST == PACE_BURST - 1 {
                tokio::time::sleep(pace * PACE_BURST as u32).await;
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
        if got.iter().any(|g| *g) {
            self.remember(&peer, used);
        }
        got
    }
}
