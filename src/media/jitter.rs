//! Buffer de jitter *sans-IO*: recebe quadros com o instante de chegada (ms), devolve-os
//! no ritmo do codec, em ordem, marcando as perdas.
//!
//! O atraso de reprodução sobe um quadro a cada quadro que chega tarde demais (já foi dado
//! como perdido) e desce um quadro depois de [`CALM_FRAMES`] quadros seguidos sem atraso.
//! Fica entre `min_ms` e `max_ms`.

use std::collections::BTreeMap;

/// Quadros pontuais sem atraso antes de reduzir o atraso de reprodução.
pub const CALM_FRAMES: u32 = 250;
/// Salto à frente (em quadros) que reinicia o buffer em vez de contar perda.
const MAX_AHEAD: u32 = 50;

#[derive(Debug, PartialEq, Eq)]
pub enum Playout {
    Frame(Vec<u8>),
    Lost,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitterStats {
    pub played: u64,
    pub lost: u64,
    /// Chegaram depois do prazo e foram descartados.
    pub late: u64,
    pub delay_ms: u64,
}

pub struct JitterBuffer {
    frame_ms: u64,
    min_ms: u64,
    max_ms: u64,
    delay_ms: u64,
    frames: BTreeMap<u32, Vec<u8>>,
    /// (seq, instante) do primeiro quadro: âncora do relógio de reprodução.
    anchor: Option<(u32, u64)>,
    next: u32,
    calm: u32,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(frame_ms: u64, min_ms: u64, max_ms: u64) -> Self {
        assert!(frame_ms > 0 && min_ms <= max_ms);
        Self { frame_ms, min_ms, max_ms, delay_ms: min_ms, frames: BTreeMap::new(), anchor: None, next: 0, calm: 0, stats: JitterStats { delay_ms: min_ms, ..Default::default() } }
    }

    pub fn stats(&self) -> JitterStats {
        JitterStats { delay_ms: self.delay_ms, ..self.stats }
    }

    fn deadline(&self, seq: u32) -> u64 {
        let (a_seq, a_at) = self.anchor.expect("âncora definida");
        a_at + u64::from(seq - a_seq) * self.frame_ms + self.delay_ms
    }

    /// Entrega um quadro recebido em `now`.
    pub fn push(&mut self, now: u64, seq: u32, data: Vec<u8>) {
        if self.anchor.is_none() || seq >= self.next.saturating_add(MAX_AHEAD) {
            // Primeiro quadro, ou o par ficou calado e voltou: recomeça.
            self.frames.clear();
            self.anchor = Some((seq, now));
            self.next = seq;
        } else if seq < self.next {
            self.stats.late += 1;
            self.calm = 0;
            self.delay_ms = (self.delay_ms + self.frame_ms).min(self.max_ms);
            return;
        }
        self.frames.insert(seq, data);
    }

    /// Próximo quadro vencido em `now`, ou `None` se ainda não é hora. Chame em laço.
    pub fn poll(&mut self, now: u64) -> Option<Playout> {
        self.anchor?;
        if now < self.deadline(self.next) {
            return None;
        }
        let out = match self.frames.remove(&self.next) {
            Some(d) => {
                self.stats.played += 1;
                Playout::Frame(d)
            }
            None => {
                self.stats.lost += 1;
                Playout::Lost
            }
        };
        self.next += 1;
        self.calm += 1;
        if self.calm >= CALM_FRAMES {
            self.calm = 0;
            self.delay_ms = self.delay_ms.saturating_sub(self.frame_ms).max(self.min_ms);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(j: &mut JitterBuffer, now: u64) -> Vec<Playout> {
        std::iter::from_fn(|| j.poll(now)).collect()
    }

    #[test]
    fn plays_in_order_at_codec_pace() {
        let mut j = JitterBuffer::new(20, 40, 200);
        j.push(1000, 0, vec![0]);
        j.push(1020, 1, vec![1]);
        assert!(drain(&mut j, 1039).is_empty());
        assert_eq!(drain(&mut j, 1040), vec![Playout::Frame(vec![0])]);
        assert_eq!(drain(&mut j, 1060), vec![Playout::Frame(vec![1])]);
    }

    #[test]
    fn reorders_inside_the_delay() {
        let mut j = JitterBuffer::new(20, 40, 200);
        j.push(1000, 0, vec![0]);
        j.push(1021, 2, vec![2]);
        j.push(1030, 1, vec![1]); // chegou depois do 2, antes do prazo
        let got = drain(&mut j, 1100);
        assert_eq!(got[..3], [Playout::Frame(vec![0]), Playout::Frame(vec![1]), Playout::Frame(vec![2])]);
    }

    #[test]
    fn marks_missing_frames_lost() {
        let mut j = JitterBuffer::new(20, 40, 200);
        j.push(1000, 0, vec![0]);
        j.push(1040, 2, vec![2]);
        let got = drain(&mut j, 1200);
        assert_eq!(got[..3], [Playout::Frame(vec![0]), Playout::Lost, Playout::Frame(vec![2])]);
        assert_eq!(j.stats().lost, got.iter().filter(|p| **p == Playout::Lost).count() as u64);
    }

    #[test]
    fn late_frame_grows_delay_and_is_dropped() {
        let mut j = JitterBuffer::new(20, 40, 100);
        j.push(1000, 0, vec![0]);
        j.push(1020, 2, vec![2]);
        drain(&mut j, 1100); // 0 tocou, 1 perdido, 2 tocou
        j.push(1101, 1, vec![1]);
        let s = j.stats();
        assert_eq!((s.late, s.delay_ms), (1, 60));
    }

    #[test]
    fn delay_is_capped_and_shrinks_when_calm() {
        let mut j = JitterBuffer::new(20, 40, 60);
        j.push(0, 0, vec![]);
        drain(&mut j, 1000); // toca 0, depois só perdas até o quadro 50
        for s in 0..10 {
            j.push(1000, s, vec![]); // todos atrasados
        }
        assert_eq!(j.stats().delay_ms, 60);
        // Fluxo em dia, relógio de 1 ms: cada quadro chega no seu horário.
        let start = j.next;
        for t in u64::from(start) * 20..u64::from(start + CALM_FRAMES * 2) * 20 {
            if t % 20 == 0 {
                j.push(t, (t / 20) as u32, vec![]);
            }
            while j.poll(t).is_some() {}
        }
        assert_eq!(j.stats().delay_ms, 40);
    }

    #[test]
    fn restarts_after_a_long_silence() {
        let mut j = JitterBuffer::new(20, 40, 200);
        j.push(0, 0, vec![0]);
        drain(&mut j, 100);
        j.push(60_000, 3000, vec![9]);
        assert_eq!(drain(&mut j, 60_040), vec![Playout::Frame(vec![9])]);
    }
}
