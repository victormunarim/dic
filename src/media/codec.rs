//! Codecs de voz. A trait é o ponto de encaixe do Opus; o G.711 µ-law vem pronto porque é
//! Rust puro, tem quadro fixo de 160 bytes (cabe no pacote) e serve para testar a camada.

use super::{MediaError, packet::MAX_PAYLOAD};

/// Codec de voz com quadros de duração fixa.
pub trait Codec {
    /// Duração de um quadro, em ms.
    fn frame_ms(&self) -> u64;
    /// Amostras PCM (mono, `i16`) por quadro.
    fn frame_samples(&self) -> usize;
    /// Codifica um quadro; o resultado deve ter no máximo [`MAX_PAYLOAD`] bytes.
    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, MediaError>;
    /// Decodifica um quadro. `None` = quadro perdido: devolve o que o codec tem para
    /// esconder a perda (PLC).
    fn decode(&mut self, data: Option<&[u8]>) -> Vec<i16>;
}

/// G.711 µ-law, 8 kHz, 20 ms: 160 amostras ↔ 160 bytes (64 kbps). Banda estreita; é o codec
/// de referência, não o de produção.
#[derive(Default)]
pub struct Mulaw {
    last: Vec<i16>,
    losses: u32,
}

impl Mulaw {
    pub const SAMPLES: usize = 160;

    pub fn new() -> Self {
        Self::default()
    }
}

const BIAS: i32 = 0x84;
const CLIP: i32 = 32635;

fn lin_to_mulaw(sample: i16) -> u8 {
    let mut s = sample as i32;
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    let s = s.min(CLIP) + BIAS;
    let mut exp = 7;
    let mut mask = 0x4000;
    while exp > 0 && s & mask == 0 {
        exp -= 1;
        mask >>= 1;
    }
    let mant = (s >> (exp + 3)) & 0x0F;
    !(sign | ((exp << 4) | mant) as u8)
}

fn mulaw_to_lin(byte: u8) -> i16 {
    let b = !byte;
    let sign = b & 0x80;
    let exp = ((b >> 4) & 0x07) as i32;
    let mant = (b & 0x0F) as i32;
    let s = (((mant << 3) + BIAS) << exp) - BIAS;
    (if sign != 0 { -s } else { s }) as i16
}

impl Codec for Mulaw {
    fn frame_ms(&self) -> u64 {
        20
    }

    fn frame_samples(&self) -> usize {
        Self::SAMPLES
    }

    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, MediaError> {
        if pcm.len() != Self::SAMPLES {
            return Err(MediaError::FrameLen { got: pcm.len(), want: Self::SAMPLES });
        }
        let out: Vec<u8> = pcm.iter().map(|s| lin_to_mulaw(*s)).collect();
        debug_assert!(out.len() <= MAX_PAYLOAD);
        Ok(out)
    }

    fn decode(&mut self, data: Option<&[u8]>) -> Vec<i16> {
        match data {
            Some(d) if d.len() == Self::SAMPLES => {
                self.losses = 0;
                self.last = d.iter().map(|b| mulaw_to_lin(*b)).collect();
                self.last.clone()
            }
            // Quadro de tamanho errado conta como perda: não confiamos nele.
            _ => {
                // PLC simples: repete o último quadro com metade da amplitude a cada perda
                // seguida; depois de 3, silêncio.
                self.losses += 1;
                if self.losses > 3 || self.last.is_empty() {
                    return vec![0; Self::SAMPLES];
                }
                for s in &mut self.last {
                    *s /= 2;
                }
                self.last.clone()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: f64) -> Vec<i16> {
        (0..n).map(|i| (amp * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 8000.0).sin()) as i16).collect()
    }

    #[test]
    fn mulaw_round_trip_keeps_the_signal() {
        let mut c = Mulaw::new();
        let pcm = tone(160, 12000.0);
        let enc = c.encode(&pcm).unwrap();
        assert_eq!(enc.len(), 160);
        let dec = c.decode(Some(&enc));
        let (sig, err) = pcm.iter().zip(&dec).fold((0f64, 0f64), |(s, e), (a, b)| {
            (s + (*a as f64).powi(2), e + (*a as f64 - *b as f64).powi(2))
        });
        let snr_db = 10.0 * (sig / err.max(1.0)).log10();
        assert!(snr_db > 30.0, "SNR {snr_db:.1} dB");
    }

    #[test]
    fn mulaw_handles_extremes() {
        for s in [i16::MIN, -1, 0, 1, i16::MAX] {
            let back = mulaw_to_lin(lin_to_mulaw(s)) as i32;
            assert!((back - s as i32).abs() <= 1100, "{s} -> {back}");
        }
    }

    #[test]
    fn rejects_wrong_frame_length() {
        assert!(matches!(Mulaw::new().encode(&[0; 10]), Err(MediaError::FrameLen { got: 10, want: 160 })));
    }

    #[test]
    fn concealment_fades_to_silence() {
        let mut c = Mulaw::new();
        let enc = c.encode(&tone(160, 12000.0)).unwrap();
        let first = c.decode(Some(&enc));
        let peak = |v: &[i16]| v.iter().map(|x| x.unsigned_abs()).max().unwrap();
        let l1 = c.decode(None);
        let l2 = c.decode(None);
        assert!(peak(&l1) < peak(&first) && peak(&l2) < peak(&l1));
        c.decode(None);
        assert_eq!(peak(&c.decode(None)), 0);
        // Voltou o áudio: sem resíduo da perda.
        assert!(peak(&c.decode(Some(&enc))) > 10000);
    }
}
