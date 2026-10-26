//! Transmissão de tela: quadros de vídeo já codificados, fatiados em pacotes de 200 B.
//!
//! Anda num circuito **só da tela**, com chaves próprias ([`CallKeys::sub`](super::keys::CallKeys::sub)).
//! Quem observa a rede vê um fluxo de taxa constante: a cada slot (definido por quem chama,
//! por exemplo 5 ms) sai exatamente um pacote, seja fragmento de quadro ou preenchimento.
//! Parar a tela, mexer o mouse ou trocar de janela não muda nada no fio. O codec deve
//! trabalhar em bitrate constante, ajustado à taxa de slots (ver [`ScreenStream::capacity_bps`]).
//!
//! O fluxo é *sans-IO* e simétrico: cada ponta cifra o que sai e decifra o que entra. Quem
//! só assiste manda preenchimento e pedidos de quadro-chave.
//!
//! Texto claro de cada pacote (dentro do AEAD de [`packet`](super::packet)):
//! `flags (1) ‖ id do quadro (2) ‖ índice (2) ‖ total (2) ‖ dados`.
//!
//! - Quadro perdido: a próxima entrega só acontece num quadro-chave; o receptor pede um
//!   (flag `WANT_KEY`, repetida enquanto precisar) e o emissor consulta
//!   [`take_keyframe_request`](ScreenStream::take_keyframe_request).
//! - Fila do emissor limitada: se o codec produz mais do que o circuito leva, os quadros
//!   mais velhos são descartados e a fila só volta a aceitar a partir de um quadro-chave.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use super::{
    MediaError,
    keys::CallKeys,
    packet::{MAX_PAYLOAD, Opened, Opener, PACKET_LEN, Sealer},
};

const HEADER: usize = 7;
/// Bytes de vídeo por pacote.
pub const FRAGMENT_PAYLOAD: usize = MAX_PAYLOAD - HEADER;
/// Maior quadro codificado aceito.
pub const MAX_FRAME: usize = 1000 * FRAGMENT_PAYLOAD;
/// Fragmentos na fila de saída antes de descartar quadros velhos (~2 s a 333 pacotes/s).
pub const MAX_QUEUE: usize = 660;
/// Quadros em remontagem ao mesmo tempo; passou disso, o mais velho é abandonado.
const MAX_PENDING: usize = 4;

/// Depois de enfileirar um quadro-chave, pedidos do receptor são ignorados por este tempo: o quadro
/// leva mais de um segundo para sair e os pedidos já em trânsito não valem para o que está a caminho.
const KEY_COOLDOWN: Duration = Duration::from_secs(3);

const KEYFRAME: u8 = 1;
const IDLE: u8 = 2;
const WANT_KEY: u8 = 4;
const STOP: u8 = 8;

/// Quadro de vídeo remontado, pronto para o decodificador.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ScreenFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Contadores do fluxo.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScreenStats {
    pub frames_sent: u64,
    pub frames_dropped: u64,
    pub frames_received: u64,
    /// Quadros incompletos abandonados ou descartados por falta de quadro-chave.
    pub frames_lost: u64,
    pub rejected: u64,
    /// Pacotes com vídeo que saíram (sem contar preenchimento).
    pub packets_sent: u64,
}

struct Pending {
    id: u16,
    keyframe: bool,
    total: u16,
    got: usize,
    parts: Vec<Option<Vec<u8>>>,
}

/// `a` vem antes de `b` na numeração circular de 16 bits.
fn older(a: u16, b: u16) -> bool {
    a != b && b.wrapping_sub(a) < 0x8000
}

/// Um lado do fluxo de tela.
pub struct ScreenStream {
    sealer: Sealer,
    opener: Opener,
    queue: VecDeque<(u8, u16, u16, u16, Vec<u8>)>,
    next_id: u16,
    /// Fila estourou: só aceita quadro-chave até limpar.
    need_key_out: bool,
    // Recepção.
    pending: Vec<Pending>,
    done: VecDeque<ScreenFrame>,
    last_id: Option<u16>,
    /// Faltou quadro: descarta até chegar um quadro-chave.
    broken: bool,
    peer_wants_key: bool,
    last_key: Option<Instant>,
    key_cooldown: Duration,
    peer_stopped: bool,
    stats: ScreenStats,
}

impl ScreenStream {
    pub fn new(keys: &CallKeys) -> Self {
        Self {
            sealer: Sealer::new(keys),
            opener: Opener::new(keys),
            queue: VecDeque::new(),
            next_id: 0,
            need_key_out: false,
            pending: Vec::new(),
            done: VecDeque::new(),
            last_id: None,
            broken: false,
            peer_wants_key: false,
            last_key: None,
            key_cooldown: KEY_COOLDOWN,
            peer_stopped: false,
            stats: ScreenStats::default(),
        }
    }

    /// Bits por segundo de vídeo que cabem com um pacote a cada `slot_ms` (já sem cabeçalhos).
    pub fn capacity_bps(slot_ms: u64) -> u64 {
        FRAGMENT_PAYLOAD as u64 * 8 * 1000 / slot_ms.max(1)
    }

    /// Enfileira um quadro codificado. `Ok(false)` = descartado (fila cheia aguardando quadro-chave).
    pub fn push_frame(&mut self, data: &[u8], keyframe: bool) -> Result<bool, MediaError> {
        if data.is_empty() || data.len() > MAX_FRAME {
            return Err(MediaError::TooLarge(data.len()));
        }
        if self.need_key_out && !keyframe {
            self.stats.frames_dropped += 1;
            return Ok(false);
        }
        let total = data.len().div_ceil(FRAGMENT_PAYLOAD);
        if keyframe {
            // Quadro-chave dispensa tudo que está na fila: quem decodificar começa dele.
            self.queue.clear();
            self.need_key_out = false;
            self.last_key = Some(Instant::now());
        } else if self.queue.len() + total > MAX_QUEUE {
            self.queue.clear();
            self.need_key_out = true;
            self.stats.frames_dropped += 1;
            return Ok(false);
        }
        let id = self.next_id;
        self.next_id = id.wrapping_add(1);
        let flags = if keyframe { KEYFRAME } else { 0 };
        for (i, chunk) in data.chunks(FRAGMENT_PAYLOAD).enumerate() {
            self.queue.push_back((flags, id, i as u16, total as u16, chunk.to_vec()));
        }
        self.stats.frames_sent += 1;
        Ok(true)
    }

    /// Fragmentos esperando saída.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Há fragmento esperando saída (quem não tem nada a mandar deixa o cover do circuito preencher).
    pub fn has_data(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Faltou quadro e a imagem está parada à espera de um quadro-chave: vale mandar um pedido.
    pub fn needs_key(&self) -> bool {
        self.broken
    }

    /// O outro lado avisou que parou de transmitir.
    pub fn peer_stopped(&self) -> bool {
        self.peer_stopped
    }

    /// Pacote de "parei de transmitir" (mesmo tamanho e cifra dos outros).
    pub fn stop_packet(&mut self) -> Result<[u8; PACKET_LEN], MediaError> {
        let mut buf = [0u8; HEADER];
        buf[0] = STOP | IDLE;
        self.sealer.seal(&buf)
    }

    /// O emissor deve forçar um quadro-chave no codec (o outro lado pediu, ou a fila estourou).
    pub fn take_keyframe_request(&mut self) -> bool {
        let wanted = std::mem::take(&mut self.peer_wants_key) && self.last_key.is_none_or(|t| t.elapsed() >= self.key_cooldown);
        // Um quadro-chave ainda saindo já atende o pedido; outro o esvaziaria no meio e, como ele
        // leva centenas de ms para sair, os pedidos repetidos do receptor nunca o deixariam chegar.
        if self.queue.iter().any(|q| q.0 & KEYFRAME != 0) {
            return false;
        }
        wanted | self.need_key_out
    }

    /// O pacote do próximo slot: fragmento da fila ou preenchimento. Sempre 200 B.
    pub fn next_packet(&mut self) -> Result<[u8; PACKET_LEN], MediaError> {
        let want = if self.broken { WANT_KEY } else { 0 };
        let mut buf = [0u8; MAX_PAYLOAD];
        let len = match self.queue.pop_front() {
            Some((flags, id, idx, total, data)) => {
                self.stats.packets_sent += 1;
                buf[0] = flags | want;
                buf[1..3].copy_from_slice(&id.to_be_bytes());
                buf[3..5].copy_from_slice(&idx.to_be_bytes());
                buf[5..7].copy_from_slice(&total.to_be_bytes());
                buf[HEADER..HEADER + data.len()].copy_from_slice(&data);
                HEADER + data.len()
            }
            None => {
                buf[0] = IDLE | want;
                HEADER
            }
        };
        self.sealer.seal(&buf[..len])
    }

    /// Pacote que chegou do circuito.
    pub fn receive(&mut self, packet: &[u8]) -> Result<(), MediaError> {
        let frame = match self.opener.open(packet) {
            Ok((_, Opened::Voice(f))) => f,
            Ok((_, Opened::Hangup)) => return Ok(()),
            Err(e) => {
                self.stats.rejected += 1;
                return Err(e);
            }
        };
        if frame.len() < HEADER {
            self.stats.rejected += 1;
            return Err(MediaError::Malformed);
        }
        let flags = frame[0];
        if flags & WANT_KEY != 0 {
            self.peer_wants_key = true;
        }
        if flags & STOP != 0 {
            self.peer_stopped = true;
            return Ok(());
        }
        if flags & IDLE != 0 {
            return Ok(());
        }
        self.peer_stopped = false;
        let id = u16::from_be_bytes([frame[1], frame[2]]);
        let (idx, total) = (u16::from_be_bytes([frame[3], frame[4]]) as usize, u16::from_be_bytes([frame[5], frame[6]]) as usize);
        if total == 0 || idx >= total {
            self.stats.rejected += 1;
            return Err(MediaError::Malformed);
        }
        if self.last_id.is_some_and(|l| !older(l, id)) {
            return Ok(()); // quadro velho ou já entregue
        }
        let slot = match self.pending.iter().position(|p| p.id == id) {
            Some(i) => i,
            None => {
                self.pending.push(Pending { id, keyframe: flags & KEYFRAME != 0, total: total as u16, got: 0, parts: vec![None; total] });
                if self.pending.len() > MAX_PENDING {
                    let oldest = (0..self.pending.len()).fold(0, |o, i| if older(self.pending[i].id, self.pending[o].id) { i } else { o });
                    self.pending.remove(oldest);
                    self.stats.frames_lost += 1;
                    self.broken = true;
                }
                self.pending.iter().position(|p| p.id == id).expect("acabou de entrar")
            }
        };
        let p = &mut self.pending[slot];
        if p.total as usize != total {
            self.stats.rejected += 1;
            return Err(MediaError::Malformed);
        }
        if p.parts[idx].is_none() {
            p.parts[idx] = Some(frame[HEADER..].to_vec());
            p.got += 1;
        }
        if p.got == p.total as usize {
            let p = self.pending.remove(slot);
            self.finish(p);
        }
        Ok(())
    }

    fn finish(&mut self, p: Pending) {
        // Tudo que ficou para trás desse quadro nunca chegará inteiro.
        let before = self.pending.len();
        self.pending.retain(|q| !older(q.id, p.id));
        let abandoned = (before - self.pending.len()) as u64;
        let skipped = self.last_id.is_some_and(|l| p.id != l.wrapping_add(1));
        if abandoned > 0 || skipped {
            self.stats.frames_lost += abandoned.max(1);
            self.broken = true;
        }
        self.last_id = Some(p.id);
        if p.keyframe {
            self.broken = false;
        }
        if self.broken {
            return; // sem referência: espera o quadro-chave
        }
        let data: Vec<u8> = p.parts.into_iter().flatten().flatten().collect();
        self.stats.frames_received += 1;
        self.done.push_back(ScreenFrame { data, keyframe: p.keyframe });
    }

    /// Próximo quadro completo e decodificável, em ordem.
    pub fn poll_frame(&mut self) -> Option<ScreenFrame> {
        self.done.pop_front()
    }

    pub fn stats(&self) -> ScreenStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::keys::Ephemeral;
    use rand_core::OsRng;
    use x25519_dalek::{PublicKey, StaticSecret};

    fn pair() -> (ScreenStream, ScreenStream) {
        let (a, b) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
        let (ea, eb) = (Ephemeral::generate(), Ephemeral::generate());
        let ka = CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &eb.public()).unwrap().sub(b"screen");
        let kb = CallKeys::responder(&b, &eb, &PublicKey::from(&a), &ea.public()).unwrap().sub(b"screen");
        (ScreenStream::new(&ka), ScreenStream::new(&kb))
    }

    fn frame(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_add(seed)).collect()
    }

    /// Um slot de A para B; `drop` perde o pacote.
    fn slot(a: &mut ScreenStream, b: &mut ScreenStream, drop: bool) {
        let p = a.next_packet().unwrap();
        assert_eq!(p.len(), PACKET_LEN);
        if !drop {
            b.receive(&p).unwrap();
        }
    }

    #[test]
    fn repeated_key_requests_do_not_restart_a_keyframe_still_going_out() {
        let (mut a, mut b) = pair();
        assert!(a.push_frame(&frame(5000, 1), true).unwrap());
        slot(&mut a, &mut b, false);
        b.broken = true;
        // O receptor pede quadro-chave enquanto o primeiro ainda sai.
        let ask = b.next_packet().unwrap();
        a.receive(&ask).unwrap();
        assert!(!a.take_keyframe_request());
        while a.has_data() {
            slot(&mut a, &mut b, false);
        }
        assert!(b.poll_frame().is_some());
    }

    #[test]
    fn small_and_large_frames_round_trip_in_order() {
        let (mut a, mut b) = pair();
        let frames = [frame(10, 1), frame(FRAGMENT_PAYLOAD, 2), frame(FRAGMENT_PAYLOAD + 1, 3), frame(5000, 4)];
        for (i, f) in frames.iter().enumerate() {
            assert!(a.push_frame(f, i == 0).unwrap());
        }
        for _ in 0..a.queued() {
            slot(&mut a, &mut b, false);
        }
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(b.poll_frame().unwrap(), ScreenFrame { data: f.clone(), keyframe: i == 0 });
        }
        assert_eq!(b.poll_frame(), None);
    }

    #[test]
    fn idle_slots_are_full_size_and_deliver_nothing() {
        let (mut a, mut b) = pair();
        for _ in 0..10 {
            slot(&mut a, &mut b, false);
        }
        assert_eq!(b.poll_frame(), None);
        assert_eq!(b.stats().rejected, 0);
    }

    #[test]
    fn packets_are_indistinguishable_in_size_with_or_without_video() {
        let (mut a, _) = pair();
        a.push_frame(&frame(3000, 0), true).unwrap();
        let sizes: Vec<usize> = (0..30).map(|_| a.next_packet().unwrap().len()).collect();
        assert!(sizes.iter().all(|s| *s == PACKET_LEN));
    }

    #[test]
    fn fragments_arriving_out_of_order_still_assemble() {
        let (mut a, mut b) = pair();
        let f = frame(1000, 9);
        a.push_frame(&f, true).unwrap();
        let mut ps: Vec<_> = (0..a.queued()).map(|_| a.next_packet().unwrap()).collect();
        ps.reverse();
        for p in ps {
            b.receive(&p).unwrap();
        }
        assert_eq!(b.poll_frame().unwrap().data, f);
    }

    #[test]
    fn key_requests_right_after_a_keyframe_are_ignored() {
        let (mut a, _) = pair();
        a.push_frame(&frame(100, 0), true).unwrap();
        a.peer_wants_key = true;
        assert!(!a.take_keyframe_request());
        a.queue.clear();
        a.last_key = Some(Instant::now() - KEY_COOLDOWN);
        a.peer_wants_key = true;
        assert!(a.take_keyframe_request());
    }

    #[test]
    fn lost_fragment_blocks_delta_frames_and_asks_for_a_keyframe() {
        let (mut a, mut b) = pair();
        a.key_cooldown = Duration::ZERO;
        a.push_frame(&frame(100, 0), true).unwrap();
        a.push_frame(&frame(400, 1), false).unwrap(); // 3 fragmentos
        a.push_frame(&frame(100, 2), false).unwrap();
        // Quadro 0 ok; perde o 2º fragmento do quadro 1; quadro 2 chega inteiro.
        let mut n = 0;
        while a.queued() > 0 {
            slot(&mut a, &mut b, n == 2);
            n += 1;
        }
        assert_eq!(b.poll_frame().unwrap().data, frame(100, 0));
        assert_eq!(b.poll_frame(), None, "quadro 2 não decodifica sem o 1");
        assert!(b.stats().frames_lost >= 1);
        // B (que só assiste) pede quadro-chave no preenchimento; A recebe o pedido.
        let p = b.next_packet().unwrap();
        a.receive(&p).unwrap();
        assert!(a.take_keyframe_request());
        assert!(!a.take_keyframe_request(), "pedido é consumido");
        a.push_frame(&frame(200, 3), true).unwrap();
        while a.queued() > 0 {
            slot(&mut a, &mut b, false);
        }
        let k = b.poll_frame().unwrap();
        assert!(k.keyframe);
        assert_eq!(k.data, frame(200, 3));
        // Depois do quadro-chave, os quadros delta voltam a passar.
        a.push_frame(&frame(50, 4), false).unwrap();
        slot(&mut a, &mut b, false);
        assert_eq!(b.poll_frame().unwrap().data, frame(50, 4));
        // E o pedido cessa.
        slot(&mut b, &mut a, false);
        assert!(!a.take_keyframe_request());
    }

    #[test]
    fn full_queue_drops_old_frames_and_wants_a_keyframe() {
        let (mut a, mut b) = pair();
        a.push_frame(&frame(100, 0), true).unwrap();
        let big = frame(FRAGMENT_PAYLOAD * 100, 1);
        let mut accepted = 0;
        for _ in 0..10 {
            accepted += a.push_frame(&big, false).unwrap() as usize;
        }
        assert!(accepted <= 7 && a.queued() <= MAX_QUEUE);
        assert!(a.take_keyframe_request());
        assert!(!a.push_frame(&big, false).unwrap(), "delta recusado até haver quadro-chave");
        assert!(a.push_frame(&frame(300, 5), true).unwrap());
        assert!(a.queued() <= 2, "quadro-chave limpa a fila");
        while a.queued() > 0 {
            slot(&mut a, &mut b, false);
        }
        assert_eq!(b.poll_frame().unwrap().data, frame(300, 5));
        assert!(a.stats().frames_dropped > 0);
    }

    #[test]
    fn replay_foreign_and_malformed_packets_are_rejected() {
        let (mut a, mut b) = pair();
        a.push_frame(&frame(10, 0), true).unwrap();
        let p = a.next_packet().unwrap();
        b.receive(&p).unwrap();
        assert_eq!(b.receive(&p).err(), Some(MediaError::Replay));
        let (mut x, _) = pair();
        x.next_packet().unwrap();
        assert_eq!(b.receive(&x.next_packet().unwrap()).err(), Some(MediaError::Decrypt));
        let mut bad = p;
        bad[50] ^= 1;
        assert!(b.receive(&bad).is_err());
        assert_eq!(b.stats().rejected, 3);
        // Cabeçalho incoerente (índice >= total) autenticado mas malformado.
        let (mut s, mut r) = pair();
        let mut raw = [0u8; HEADER];
        raw[4] = 5;
        raw[6] = 2;
        let pkt = s.sealer.seal(&raw).unwrap();
        assert_eq!(r.receive(&pkt).err(), Some(MediaError::Malformed));
    }

    #[test]
    fn frame_ids_wrap_around() {
        let (mut a, mut b) = pair();
        a.next_id = u16::MAX - 1;
        b.last_id = Some(u16::MAX - 2);
        for i in 0..4u8 {
            a.push_frame(&frame(20, i), i == 0).unwrap();
            slot(&mut a, &mut b, false);
            assert_eq!(b.poll_frame().unwrap().data, frame(20, i));
        }
    }

    #[test]
    fn oversized_and_empty_frames_are_errors() {
        let (mut a, _) = pair();
        assert!(a.push_frame(&[], true).is_err());
        assert!(a.push_frame(&vec![0; MAX_FRAME + 1], true).is_err());
        assert!(a.push_frame(&vec![0; MAX_FRAME], true).unwrap());
    }

    #[test]
    fn stop_is_signalled_and_cleared_by_new_video() {
        let (mut a, mut b) = pair();
        b.receive(&a.stop_packet().unwrap()).unwrap();
        assert!(b.peer_stopped());
        a.push_frame(&frame(10, 0), true).unwrap();
        slot(&mut a, &mut b, false);
        assert!(!b.peer_stopped());
        assert_eq!(b.poll_frame().unwrap().data, frame(10, 0));
    }

    #[test]
    fn viewer_asks_only_while_it_needs_a_keyframe() {
        let (mut a, mut b) = pair();
        assert!(!b.needs_key() && !a.has_data());
        a.push_frame(&frame(100, 0), true).unwrap();
        a.push_frame(&frame(400, 1), false).unwrap();
        a.push_frame(&frame(100, 2), false).unwrap();
        let mut n = 0;
        while a.has_data() {
            slot(&mut a, &mut b, n == 2);
            n += 1;
        }
        assert!(b.needs_key());
    }

    #[test]
    fn capacity_matches_the_slot_rate() {
        // 5 ms = 200 pacotes/s.
        assert_eq!(ScreenStream::capacity_bps(5), FRAGMENT_PAYLOAD as u64 * 8 * 200);
    }
}
