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
//! - Fragmento perdido: quem assiste pede de volta só o que faltou (flag `NACK`, num pacote de
//!   preenchimento: contagem no byte 2 e, depois do cabeçalho, pares `id do quadro (2) ‖ índice (2)`;
//!   índice `0xFFFF` = o quadro inteiro). O emissor guarda os últimos [`HISTORY`] fragmentos que saíram e
//!   os reenvia à frente da fila, em pacotes novos (número de sequência novo, cifra nova). Os quadros
//!   esperam o reparo e saem em ordem.
//! - Reparo que não veio em [`PATIENCE`]: o quadro é abandonado e a próxima entrega só acontece num
//!   quadro-chave; o receptor pede um (flag `WANT_KEY`, repetida enquanto precisar) e o emissor consulta
//!   [`take_keyframe_request`](ScreenStream::take_keyframe_request).
//! - Perda no caminho: quem assiste conta as lacunas no número de sequência dos pacotes que chegam e,
//!   a cada meio segundo, devolve a fração perdida (flag `REPORT`, em pacote de preenchimento). O
//!   emissor lê o relatório em [`take_report`](ScreenStream::take_report) e ajusta a taxa do codec.
//! - Fila do emissor limitada: se o codec produz mais do que o circuito leva, os quadros
//!   mais velhos são descartados e a fila só volta a aceitar a partir de um quadro-chave.

use std::{
    collections::{HashMap, VecDeque},
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
/// Quadros em remontagem ao mesmo tempo (a espera por um reparo acumula os que vêm depois); passou
/// disso, o mais velho é abandonado.
const MAX_PENDING: usize = 48;
/// Fragmentos que o emissor guarda para reenviar (~2 s a 333 pacotes/s).
pub const HISTORY: usize = 660;
/// Quanto o receptor espera por um reparo antes de abandonar o quadro e pedir quadro-chave. Conta desde o
/// último fragmento que o quadro recebeu: um quadro-chave ainda chegando não se esgota.
pub const PATIENCE: Duration = Duration::from_millis(800);
/// Intervalo mínimo entre dois pedidos do mesmo quadro (um pedido leva uma ida e volta no circuito).
const NACK_REPEAT: Duration = Duration::from_millis(250);
/// Intervalo mínimo entre dois reenvios do mesmo fragmento (pedidos repetidos já em trânsito).
const RETX_HOLD: Duration = Duration::from_millis(120);
/// Pares num pedido.
const MAX_NACK: usize = FRAGMENT_PAYLOAD / 4;
/// Reenvios esperando saída; o resto o receptor pede de novo.
const MAX_RETX: usize = 128;
/// Índice que pede o quadro inteiro.
const WHOLE_FRAME: u16 = u16::MAX;
/// Quadros no máximo à frente do último entregue que valem um pedido; mais que isso é desistir.
const NACK_SPAN: usize = 64;

/// Depois de enfileirar um quadro-chave, pedidos do receptor são ignorados por este tempo: o quadro
/// leva mais de um segundo para sair e os pedidos já em trânsito não valem para o que está a caminho.
const KEY_COOLDOWN: Duration = Duration::from_secs(3);

const KEYFRAME: u8 = 1;
const IDLE: u8 = 2;
const WANT_KEY: u8 = 4;
const STOP: u8 = 8;
/// Pacote de preenchimento com relatório de perda: byte 1 = fração perdida (0 a 255), bytes 3 e 4 = pacotes
/// esperados na janela.
const REPORT: u8 = 16;
/// Pacote de preenchimento com pedido de fragmentos (veja o topo do módulo).
const NACK: u8 = 32;

/// Janela de medição da perda, entre dois relatórios.
const REPORT_EVERY: Duration = Duration::from_millis(500);
/// Pacotes esperados mínimos para valer um relatório (poucos pacotes dão uma fração sem sentido).
const MIN_REPORT: u32 = 16;

/// Quadro de vídeo remontado, pronto para o decodificador.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ScreenFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Relatório de quem assiste sobre a janela de pacotes que acabou de medir.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LossReport {
    /// Fração dos pacotes que não chegou (0 a 1).
    pub loss: f32,
    /// Pacotes que deveriam ter chegado na janela.
    pub packets: u32,
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
    /// Pacotes com vídeo que saíram (sem contar preenchimento; os reenvios entram).
    pub packets_sent: u64,
    /// Fragmentos reenviados a pedido de quem assiste.
    pub retransmitted: u64,
}

/// Fragmento: sinalizadores, id do quadro, índice, total, dados.
type Frag = (u8, u16, u16, u16, Vec<u8>);

/// Fragmento que já saiu, guardado para um pedido de reenvio.
struct Sent {
    frag: Frag,
    resent: Option<Instant>,
}

struct Pending {
    id: u16,
    keyframe: bool,
    total: u16,
    got: usize,
    parts: Vec<Option<Vec<u8>>>,
    /// Quando chegou o último fragmento deste quadro.
    last_at: Instant,
}

/// `a` vem antes de `b` na numeração circular de 16 bits.
fn older(a: u16, b: u16) -> bool {
    a != b && b.wrapping_sub(a) < 0x8000
}

/// Um lado do fluxo de tela.
pub struct ScreenStream {
    sealer: Sealer,
    opener: Opener,
    queue: VecDeque<Frag>,
    /// Reenvios pedidos, à frente da fila.
    retx: VecDeque<Frag>,
    history: VecDeque<Sent>,
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
    /// Quadros já pedidos de volta e quando (receptor).
    asked: HashMap<u16, Instant>,
    patience: Duration,
    last_key: Option<Instant>,
    key_cooldown: Duration,
    peer_stopped: bool,
    stats: ScreenStats,
    // Medição de perda de quem assiste: maior `seq` visto, início da janela e pacotes que chegaram nela.
    top: Option<u32>,
    win_base: u32,
    win_got: u32,
    win_start: Instant,
    report_every: Duration,
    // Relatório de quem assiste, do lado de quem transmite.
    report_in: Option<LossReport>,
    report_at: Option<Instant>,
    /// `packets_sent` quando o último relatório chegou.
    sent_at_report: u64,
}

impl ScreenStream {
    pub fn new(keys: &CallKeys) -> Self {
        Self {
            sealer: Sealer::new(keys),
            opener: Opener::new(keys),
            queue: VecDeque::new(),
            retx: VecDeque::new(),
            history: VecDeque::new(),
            next_id: 0,
            need_key_out: false,
            pending: Vec::new(),
            done: VecDeque::new(),
            last_id: None,
            broken: false,
            peer_wants_key: false,
            asked: HashMap::new(),
            patience: PATIENCE,
            last_key: None,
            key_cooldown: KEY_COOLDOWN,
            peer_stopped: false,
            stats: ScreenStats::default(),
            top: None,
            win_base: 0,
            win_got: 0,
            win_start: Instant::now(),
            report_every: REPORT_EVERY,
            report_in: None,
            report_at: None,
            sent_at_report: 0,
        }
    }

    /// Bits por segundo de vídeo que cabem com um pacote a cada `slot` (já sem cabeçalhos).
    pub fn capacity_bps(slot: std::time::Duration) -> u64 {
        (FRAGMENT_PAYLOAD as u128 * 8 * 1_000_000 / slot.as_micros().max(1)) as u64
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
            // Os quadros anteriores não servem a quem decodifica a partir deste.
            self.retx.clear();
            self.history.clear();
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
        !self.queue.is_empty() || !self.retx.is_empty()
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

    /// Quem assiste: o relatório de perda da janela, se já deu o tempo e chegaram pacotes bastantes, e o
    /// pedido dos fragmentos que faltam, se houver. Vai num pacote de preenchimento (e leva o pedido de
    /// quadro-chave, se houver). Fecha a janela. Também entrega, pela fila de [`poll_frame`](Self::poll_frame),
    /// os quadros que desistiram de esperar: chame de tempos em tempos.
    pub fn report_packet(&mut self) -> Option<Result<[u8; PACKET_LEN], MediaError>> {
        let nacks = self.collect_nacks();
        let window = self.close_window();
        if nacks.is_empty() && window.is_none() {
            return None;
        }
        let mut buf = [0u8; MAX_PAYLOAD];
        let mut len = HEADER;
        buf[0] = IDLE | if self.broken { WANT_KEY } else { 0 };
        if let Some((lost, expected)) = window {
            buf[0] |= REPORT;
            buf[1] = lost;
            buf[3..5].copy_from_slice(&expected.to_be_bytes());
        }
        if !nacks.is_empty() {
            buf[0] |= NACK;
            buf[2] = nacks.len() as u8;
            for (id, idx) in nacks {
                buf[len..len + 2].copy_from_slice(&id.to_be_bytes());
                buf[len + 2..len + 4].copy_from_slice(&idx.to_be_bytes());
                len += 4;
            }
        }
        Some(self.sealer.seal(&buf[..len]))
    }

    /// Fecha a janela de medição se deu o tempo: fração perdida (0 a 255) e pacotes esperados.
    fn close_window(&mut self) -> Option<(u8, u16)> {
        let top = self.top?;
        let expected = top.checked_sub(self.win_base).map_or(0, |d| d + 1);
        if self.win_start.elapsed() < self.report_every || expected < MIN_REPORT {
            return None;
        }
        let lost = expected.saturating_sub(self.win_got);
        self.win_base = top.saturating_add(1);
        (self.win_got, self.win_start) = (0, Instant::now());
        Some(((lost as f32 / expected as f32 * 255.0).round() as u8, expected.min(u16::MAX as u32) as u16))
    }

    /// Os fragmentos que faltam e já valem um pedido: lacunas dentro do quadro que está chegando, o que
    /// falta nos quadros mais velhos e quadros que nem começaram a chegar. Cada quadro só é pedido de novo
    /// depois de [`NACK_REPEAT`].
    fn collect_nacks(&mut self) -> Vec<(u16, u16)> {
        self.drain();
        let Some(newest) = self.pending.iter().map(|p| p.id).fold(None, |n: Option<u16>, id| match n {
            Some(n) if !older(n, id) => Some(n),
            _ => Some(id),
        }) else {
            return Vec::new();
        };
        let start = self.last_id.map_or_else(|| self.pending.iter().map(|p| p.id).fold(newest, |o, id| if older(id, o) { id } else { o }), |l| l.wrapping_add(1));
        let span = newest.wrapping_sub(start) as usize;
        if span > NACK_SPAN {
            return Vec::new(); // `drain` vai desistir
        }
        let now = Instant::now();
        let mut out = Vec::new();
        for step in 0..=span {
            let id = start.wrapping_add(step as u16);
            if self.asked.get(&id).is_some_and(|t| now.duration_since(*t) < NACK_REPEAT) {
                continue;
            }
            let before = out.len();
            match self.pending.iter().find(|p| p.id == id) {
                None => out.push((id, WHOLE_FRAME)),
                Some(p) => {
                    // No quadro mais novo só vale o que ficou para trás de um fragmento que já chegou.
                    let upto = if id == newest { p.parts.iter().rposition(Option::is_some).unwrap_or(0) } else { p.parts.len() };
                    out.extend((0..upto).filter(|i| p.parts[*i].is_none()).map(|i| (id, i as u16)));
                }
            }
            if out.len() > before {
                self.asked.insert(id, now);
            }
            if out.len() >= MAX_NACK {
                out.truncate(MAX_NACK);
                break;
            }
        }
        out
    }

    /// Quem transmite: põe na fila de reenvio os fragmentos que o relatório de quem assiste pediu.
    fn schedule_retx(&mut self, frame: &[u8]) {
        let count = frame[2] as usize;
        let now = Instant::now();
        for pair in frame[HEADER..].as_chunks::<4>().0.iter().take(count) {
            let (id, idx) = (u16::from_be_bytes([pair[0], pair[1]]), u16::from_be_bytes([pair[2], pair[3]]));
            for sent in self.history.iter_mut().filter(|s| s.frag.1 == id && (idx == WHOLE_FRAME || s.frag.2 == idx)) {
                if self.retx.len() >= MAX_RETX {
                    return;
                }
                if sent.resent.is_some_and(|t| now.duration_since(t) < RETX_HOLD) {
                    continue;
                }
                sent.resent = Some(now);
                self.retx.push_back(sent.frag.clone());
                self.stats.retransmitted += 1;
            }
        }
    }

    /// Entrega, em ordem, os quadros completos; desiste do que não veio no prazo.
    fn drain(&mut self) {
        while let Some(i) = (0..self.pending.len()).fold(None, |o: Option<usize>, i| match o {
            Some(o) if !older(self.pending[i].id, self.pending[o].id) => Some(o),
            _ => Some(i),
        }) {
            let (id, key, full, last_at) = {
                let p = &self.pending[i];
                (p.id, p.keyframe, p.got == p.total as usize, p.last_at)
            };
            let next = self.last_id.map(|l| l.wrapping_add(1));
            let in_order = key || next.is_none_or(|n| n == id);
            if full && in_order {
                let p = self.pending.remove(i);
                self.finish(p);
                continue;
            }
            if self.pending.len() <= MAX_PENDING && last_at.elapsed() < self.patience {
                break;
            }
            // O reparo não veio: o que falta nunca chegará, e o que vem depois não decodifica.
            self.broken = true;
            match next {
                Some(n) if !in_order => {
                    self.stats.frames_lost += id.wrapping_sub(n) as u64;
                    self.last_id = Some(id.wrapping_sub(1));
                }
                _ => {
                    self.pending.remove(i);
                    self.stats.frames_lost += 1;
                    self.last_id = Some(id);
                }
            }
        }
        if let Some(l) = self.last_id {
            self.asked.retain(|id, _| older(l, *id));
        }
    }

    /// Quem transmite: o último relatório que chegou, uma vez só.
    pub fn take_report(&mut self) -> Option<LossReport> {
        self.report_in.take()
    }

    /// Quem transmite: há quanto tempo chegou o último relatório (`None` se nunca chegou: quem assiste
    /// pode ser uma versão sem relatórios) e quantos pacotes de vídeo saíram desde então.
    pub fn report_age(&self) -> Option<(Duration, u64)> {
        self.report_at.map(|t| (t.elapsed(), self.stats.packets_sent - self.sent_at_report))
    }

    fn note_seq(&mut self, seq: u32) {
        match self.top {
            None => (self.top, self.win_base, self.win_got, self.win_start) = (Some(seq), seq, 1, Instant::now()),
            Some(top) => {
                self.win_got += 1;
                self.top = Some(top.max(seq));
            }
        }
    }

    /// O pacote do próximo slot: fragmento da fila ou preenchimento. Sempre 200 B.
    pub fn next_packet(&mut self) -> Result<[u8; PACKET_LEN], MediaError> {
        let want = if self.broken { WANT_KEY } else { 0 };
        let mut buf = [0u8; MAX_PAYLOAD];
        let (next, fresh) = match self.retx.pop_front() {
            Some(f) => (Some(f), false),
            None => (self.queue.pop_front(), true),
        };
        let len = match next {
            Some((flags, id, idx, total, data)) => {
                self.stats.packets_sent += 1;
                buf[0] = flags | want;
                buf[1..3].copy_from_slice(&id.to_be_bytes());
                buf[3..5].copy_from_slice(&idx.to_be_bytes());
                buf[5..7].copy_from_slice(&total.to_be_bytes());
                buf[HEADER..HEADER + data.len()].copy_from_slice(&data);
                if fresh {
                    if self.history.len() >= HISTORY {
                        self.history.pop_front();
                    }
                    self.history.push_back(Sent { frag: (flags, id, idx, total, data.clone()), resent: None });
                }
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
            Ok((seq, Opened::Voice(f))) => {
                self.note_seq(seq);
                f
            }
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
        if flags & REPORT != 0 {
            let packets = u16::from_be_bytes([frame[3], frame[4]]) as u32;
            self.report_in = Some(LossReport { loss: frame[1] as f32 / 255.0, packets });
            self.report_at = Some(Instant::now());
            self.sent_at_report = self.stats.packets_sent;
        }
        if flags & NACK != 0 && frame.len() > HEADER {
            self.schedule_retx(&frame);
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
                let keyframe = flags & KEYFRAME != 0;
                if keyframe {
                    // O emissor largou tudo que era anterior ao quadro-chave: não há o que esperar.
                    self.pending.retain(|q| !older(q.id, id));
                }
                self.pending.push(Pending { id, keyframe, total: total as u16, got: 0, parts: vec![None; total], last_at: Instant::now() });
                self.pending.len() - 1
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
            p.last_at = Instant::now();
        }
        self.drain();
        Ok(())
    }

    /// Quadro completo e na vez dele (veja [`drain`](Self::drain)).
    fn finish(&mut self, p: Pending) {
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
        b.patience = Duration::ZERO; // sem esperar reparo
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
        b.patience = PATIENCE; // o quadro-chave leva dois fragmentos para chegar
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

    /// Quadro-chave de 1 fragmento, quadro de 3 fragmentos e quadro de 1: sai tudo, perdendo o fragmento `lose` (se houver).
    fn send_three(a: &mut ScreenStream, b: &mut ScreenStream, lose: Option<usize>) {
        a.push_frame(&frame(100, 0), true).unwrap();
        a.push_frame(&frame(400, 1), false).unwrap();
        a.push_frame(&frame(100, 2), false).unwrap();
        let mut n = 0;
        while a.queued() > 0 {
            slot(a, b, lose == Some(n));
            n += 1;
        }
    }

    /// B devolve o que tem a pedir; A o recebe e reenvia tudo que ficou na fila.
    fn repair(a: &mut ScreenStream, b: &mut ScreenStream) {
        let ask = b.report_packet().expect("pedido").unwrap();
        a.receive(&ask).unwrap();
        while a.has_data() {
            slot(a, b, false);
        }
    }

    #[test]
    fn a_lost_fragment_is_asked_for_and_the_frames_come_out_whole_and_in_order() {
        let (mut a, mut b) = pair();
        send_three(&mut a, &mut b, Some(2)); // o 2º fragmento do quadro 1
        assert_eq!(b.poll_frame().unwrap().data, frame(100, 0));
        assert_eq!(b.poll_frame(), None, "o quadro 2 espera o 1");
        repair(&mut a, &mut b);
        assert_eq!(b.poll_frame().unwrap().data, frame(400, 1));
        assert_eq!(b.poll_frame().unwrap().data, frame(100, 2));
        assert!(!b.needs_key());
        assert_eq!(b.stats().frames_lost, 0);
        assert_eq!(a.stats().retransmitted, 1);
        assert!(b.report_packet().is_none(), "nada mais a pedir");
    }

    #[test]
    fn a_frame_that_never_arrived_is_asked_for_whole() {
        let (mut a, mut b) = pair();
        a.push_frame(&frame(100, 0), true).unwrap();
        a.push_frame(&frame(400, 1), false).unwrap();
        a.push_frame(&frame(100, 2), false).unwrap();
        let mut n = 0;
        while a.queued() > 0 {
            slot(&mut a, &mut b, (1..=3).contains(&n)); // os 3 fragmentos do quadro 1
            n += 1;
        }
        assert_eq!(b.poll_frame().unwrap().data, frame(100, 0));
        assert_eq!(b.poll_frame(), None);
        repair(&mut a, &mut b);
        assert_eq!(b.poll_frame().unwrap().data, frame(400, 1));
        assert_eq!(b.poll_frame().unwrap().data, frame(100, 2));
        assert_eq!(a.stats().retransmitted, 3);
    }

    #[test]
    fn the_same_frame_is_not_asked_again_before_the_repeat_interval_and_not_resent_twice_in_a_row() {
        let (mut a, mut b) = pair();
        send_three(&mut a, &mut b, Some(2));
        let first = b.report_packet().expect("pedido").unwrap();
        assert!(b.report_packet().is_none(), "ainda dentro do intervalo");
        a.receive(&first).unwrap();
        a.receive(&first).unwrap_err(); // o mesmo pacote é repetição e não vale
        let again = {
            b.asked.clear();
            b.report_packet().expect("pedido repetido").unwrap()
        };
        a.receive(&again).unwrap();
        assert_eq!(a.stats().retransmitted, 1, "o reenvio recente segura o segundo pedido");
        // Passado o intervalo, o fragmento sai de novo.
        for s in a.history.iter_mut() {
            s.resent = s.resent.map(|t| t - RETX_HOLD);
        }
        b.asked.clear();
        a.receive(&b.report_packet().unwrap().unwrap()).unwrap();
        assert_eq!(a.stats().retransmitted, 2);
    }

    #[test]
    fn a_repair_that_never_comes_falls_back_to_a_keyframe_request() {
        let (mut a, mut b) = pair();
        b.patience = Duration::from_millis(30);
        send_three(&mut a, &mut b, Some(2));
        assert!(!b.needs_key(), "ainda espera o reparo");
        std::thread::sleep(Duration::from_millis(60));
        b.report_packet();
        assert!(b.needs_key());
        assert_eq!(b.stats().frames_lost, 1);
        assert_eq!(b.poll_frame().unwrap().data, frame(100, 0));
        assert_eq!(b.poll_frame(), None);
    }

    #[test]
    fn a_keyframe_drops_the_history_and_pending_repairs() {
        let (mut a, mut b) = pair();
        send_three(&mut a, &mut b, Some(2));
        let ask = b.report_packet().expect("pedido").unwrap();
        a.push_frame(&frame(50, 9), true).unwrap();
        a.receive(&ask).unwrap();
        assert_eq!(a.stats().retransmitted, 0, "o quadro-chave largou o que era anterior");
        while a.queued() > 0 {
            slot(&mut a, &mut b, false);
        }
        assert_eq!(b.poll_frame().unwrap().data, frame(100, 0));
        assert_eq!(b.poll_frame().unwrap().data, frame(50, 9));
        assert!(!b.needs_key());
    }

    #[test]
    fn retransmissions_look_like_any_other_packet_and_go_first() {
        let (mut a, mut b) = pair();
        send_three(&mut a, &mut b, Some(2));
        a.push_frame(&frame(2000, 7), false).unwrap();
        assert_eq!(b.poll_frame().unwrap().data, frame(100, 0));
        a.receive(&b.report_packet().unwrap().unwrap()).unwrap();
        let p = a.next_packet().unwrap();
        assert_eq!(p.len(), PACKET_LEN);
        b.receive(&p).unwrap();
        assert_eq!(b.poll_frame().unwrap().data, frame(400, 1), "o reenvio saiu antes do quadro novo");
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
        b.patience = Duration::ZERO;
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

    /// `n` slots de A para B (um a cada `lose` é perdido, se for > 0); depois B fecha a janela e A lê o relatório.
    fn report_after(n: usize, lose: usize) -> Option<LossReport> {
        let (mut a, mut b) = pair();
        b.report_every = Duration::ZERO;
        for i in 0..n {
            slot(&mut a, &mut b, lose > 0 && i % lose == 1);
        }
        let p = b.report_packet()?.unwrap();
        a.receive(&p).unwrap();
        a.take_report()
    }

    #[test]
    fn the_viewer_reports_the_share_of_packets_lost() {
        let r = report_after(200, 4).expect("relatório");
        assert!((r.loss - 0.25).abs() < 0.02, "{r:?}");
        assert!(r.packets >= 190, "{r:?}");
        let clean = report_after(200, 0).expect("relatório");
        assert_eq!(clean.loss, 0.0);
    }

    #[test]
    fn no_report_before_enough_packets_or_before_the_window_ends() {
        assert!(report_after(MIN_REPORT as usize - 1, 0).is_none());
        let (mut a, mut b) = pair();
        for _ in 0..100 {
            slot(&mut a, &mut b, false);
        }
        assert!(b.report_packet().is_none(), "a janela de 500 ms ainda não acabou");
    }

    #[test]
    fn the_window_restarts_after_each_report() {
        let (mut a, mut b) = pair();
        b.report_every = Duration::ZERO;
        // O último pacote da janela chega: perda na borda só se vê quando o seguinte chega, e cai na janela dele.
        for i in 0..101 {
            slot(&mut a, &mut b, i % 2 == 1);
        }
        assert!(b.report_packet().is_some());
        for _ in 0..100 {
            slot(&mut a, &mut b, false);
        }
        let p = b.report_packet().expect("segundo relatório").unwrap();
        a.receive(&p).unwrap();
        assert_eq!(a.take_report().unwrap().loss, 0.0);
        assert!(a.take_report().is_none(), "o relatório é entregue uma vez só");
    }

    #[test]
    fn report_age_counts_the_packets_sent_since_it() {
        let (mut a, mut b) = pair();
        b.report_every = Duration::ZERO;
        assert!(a.report_age().is_none());
        for _ in 0..40 {
            slot(&mut a, &mut b, false);
        }
        a.receive(&b.report_packet().unwrap().unwrap()).unwrap();
        assert_eq!(a.report_age().map(|(_, n)| n), Some(0));
        a.push_frame(&frame(1000, 1), true).unwrap();
        for _ in 0..3 {
            a.next_packet().unwrap();
        }
        assert_eq!(a.report_age().map(|(_, n)| n), Some(3));
    }

    #[test]
    fn capacity_matches_the_slot_rate() {
        // 5 ms = 200 pacotes/s.
        assert_eq!(ScreenStream::capacity_bps(Duration::from_millis(5)), FRAGMENT_PAYLOAD as u64 * 8 * 200);
    }
}
