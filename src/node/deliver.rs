//! Entrega de avisos de endereço e de apresentações ao relay de um contato, e aplicação do aviso que chega.

use crate::sync::LockExt;
use std::time::Duration;

/// Quadros mandados de uma vez antes de esperar o cover esvaziar (a fila dele guarda 16 células).
const PACE_BURST: usize = 8;
/// Sem ack novo por este tempo, um lote com ritmo para de esperar o que falta.
const ACK_IDLE: Duration = Duration::from_millis(1500);

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
        let found = if self.two_relays { self.reachable_client(&peer, ClientOptions::default).await.0 } else { self.stored_client(&peer, stored, ClientOptions::default).await.map(|(c, u)| (c, Some(u))) };
        let Some((mut client, used)) = found else { return false };
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
            self.remember_used(&peer, used);
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

    /// Abre o circuito de tela que leva um arquivo a `peer`: um só para a transferência inteira, para não pagar a montagem
    /// nem a espera pelos últimos acks a cada janela. `None` se o contato não foi alcançado.
    pub async fn open_file_pipe(&self, peer: [u8; 32], id: u64) -> Option<FilePipe> {
        let pk = PublicKey::from(peer);
        let stream = update::FileStream::sending(&self.secret, &pk, id)?;
        let (found, _) = self.reachable_client(&peer, crate::phone::screen_client_options).await;
        let (client, used) = found?;
        Some(FilePipe { client, used, secret: self.secret.clone(), pk, stream, bound: false, slot: self.file_slot(), next_slot: tokio::time::Instant::now() })
    }

    /// Guarda `used` como o endereço que funcionou para `peer` (o arquivo chegou).
    pub fn remember_file_route(&self, peer: &[u8; 32], pipe: FilePipe) {
        self.remember_used(peer, pipe.used);
        pipe.client.stop();
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
            self.remember_used(&peer, used);
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
        let found = if search || self.two_relays { self.reachable_client(&peer, options).await.0 } else { self.stored_client(&peer, self.stored_for(&peer), options).await.map(|(c, u)| (c, Some(u))) };
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
        // Com ritmo (lote de arquivo), a fila do cover descarta algumas células e o ack delas nunca vem: esperar o prazo
        // inteiro por elas custava ~20 s por janela (6 KB/s em vez de ~60). Passado `ACK_IDLE` sem ack novo, desiste
        // do que falta e o chamador reenvia só isso.
        let idle = if pace.is_zero() { wait } else { wait.min(ACK_IDLE) };
        // O relógio do ocioso conta desde o último ack, não desde o último evento: o cover também gera eventos.
        let deadline = tokio::time::Instant::now() + wait;
        let mut last_ack = tokio::time::Instant::now();
        while got.iter().any(|g| !g) {
            let now = tokio::time::Instant::now();
            let left = deadline.saturating_duration_since(now).min(idle.saturating_sub(now.saturating_duration_since(last_ack)));
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, client.next_event()).await {
                Ok(Some(Event::Data(d))) => {
                    if let Some(i) = (0..frames.len()).find(|i| !got[*i] && acked(*i, &d)) {
                        got[i] = true;
                        last_ack = tokio::time::Instant::now();
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        client.stop();
        if got.iter().any(|g| *g) {
            self.remember_used(&peer, used);
        }
        got
    }
}

/// O circuito do arquivo caiu (ou o quadro não pôde ser cifrado): quem envia abre outro.
#[derive(Debug)]
pub struct PipeClosed;

/// Circuito de tela aberto para um arquivo: manda pedaços no ritmo do cover e devolve os acks que chegam enquanto isso.
pub struct FilePipe {
    client: crate::transport::CircuitClient,
    used: Option<RelayInfo>,
    secret: x25519_dalek::StaticSecret,
    pk: PublicKey,
    /// Cifra dos pedaços compactos desta transferência.
    stream: update::FileStream,
    /// O primeiro ack chegou: o telefone já ligou o circuito à transferência, e o `Bind` não precisa mais sair.
    bound: bool,
    /// Intervalo entre pedaços ([`Dialer::file_slot`]): o ritmo constante da tela ou o rápido.
    slot: Duration,
    /// Quando o próximo lote pode sair. Conta por prazo, não por espera fixa: o tempo gasto lendo o disco ou tratando
    /// acks não pode atrasar o ritmo (nem deixar a fila do cover esvaziar).
    next_slot: tokio::time::Instant,
}

impl FilePipe {
    /// Manda `chunks` (no máximo [`PACE_BURST`] por vez, a fila do cover guarda 16 células) e espera até o próximo
    /// espaço do ritmo, recolhendo os acks que chegam. `Err`: o circuito caiu.
    pub async fn push(&mut self, chunks: &[update::FileFrame]) -> Result<Vec<u64>, PipeClosed> {
        // Enquanto nenhum ack voltou, o `Bind` vai na frente de cada lote: o cover descarta ~1% das células e o pedaço
        // compacto sem vínculo é ignorado pelo telefone. Depois do primeiro ack o vínculo vale até o circuito fechar.
        let mut cells = 0;
        if !self.bound {
            let bind = update::seal_file(&self.secret, &self.pk, &update::FileFrame::Bind { id: self.stream.id() }).map_err(|_| PipeClosed)?;
            self.client.send(&bind).await.map_err(|_| PipeClosed)?;
            cells += 1;
        }
        for c in chunks {
            let update::FileFrame::Chunk { index, data, .. } = c else { continue };
            let frame = self.stream.seal(*index, data).map_err(|_| PipeClosed)?;
            self.client.send(&frame).await.map_err(|_| PipeClosed)?;
            cells += 1;
        }
        let step = self.slot * cells.clamp(1, PACE_BURST + 1) as u32;
        let now = tokio::time::Instant::now();
        // Atrasado mais que um lote: recomeça do agora em vez de despachar uma rajada para "compensar".
        self.next_slot = if self.next_slot + step < now { now + step } else { self.next_slot + step };
        self.collect(self.next_slot).await
    }

    /// Recolhe acks até `until`.
    pub async fn collect(&mut self, until: tokio::time::Instant) -> Result<Vec<u64>, PipeClosed> {
        let mut acks = Vec::new();
        loop {
            match tokio::time::timeout_at(until, self.client.next_event()).await {
                Ok(Some(Event::Data(d))) => {
                    if let Some(seq) = update::ack_seq_of(&self.secret, &self.pk, &d) {
                        self.bound = true;
                        acks.push(seq);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => return Err(PipeClosed),
                Err(_) => return Ok(acks),
            }
        }
    }
}
