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

/// G.722 (modo 1, 64 kbps), 16 kHz, 20 ms: 320 amostras ↔ 160 bytes. Banda larga (50 Hz a 7 kHz),
/// Rust puro, no mesmo tamanho de pacote do µ-law.
pub struct G722 {
    enc: Qmf,
    dec: Qmf,
    eb: [Band; 2],
    db: [Band; 2],
    last: Vec<i16>,
    losses: u32,
}

#[derive(Default)]
struct Qmf {
    x: [i32; 24],
}

impl Qmf {
    fn push2(&mut self, a: i32, b: i32) {
        self.x.copy_within(2.., 0);
        self.x[22] = a;
        self.x[23] = b;
    }

    /// (soma ímpar, soma par) do filtro de análise/síntese.
    fn sums(&self) -> (i32, i32) {
        let (mut odd, mut even) = (0, 0);
        for i in 0..12 {
            odd += self.x[2 * i] * QMF[i];
            even += self.x[2 * i + 1] * QMF[11 - i];
        }
        (odd, even)
    }
}

const QMF: [i32; 12] = [3, -11, 12, 32, -210, 951, 3876, -805, 362, -156, 53, -11];
const Q6: [i32; 30] = [
    0, 35, 72, 110, 150, 190, 233, 276, 323, 370, 422, 473, 530, 587, 650, 714, 786, 858, 940, 1023, 1121, 1219, 1339,
    1458, 1612, 1765, 1980, 2195, 2557, 2919,
];
/// Níveis de reconstrução do quantizador de 6 bits, por magnitude (1..=30).
const LEVEL: [i32; 31] = [
    0, 136, 432, 728, 1040, 1360, 1688, 2032, 2400, 2776, 3168, 3576, 4008, 4464, 4944, 5456, 6000, 6576, 7192, 7856,
    8576, 9360, 10232, 11192, 12280, 13512, 14984, 16704, 19008, 21904, 24808,
];
const WL: [i32; 8] = [-60, -30, 58, 172, 334, 538, 1198, 3042];
const RL42: [usize; 16] = [0, 7, 6, 5, 4, 3, 2, 1, 7, 6, 5, 4, 3, 2, 1, 0];
const ILB: [i32; 32] = [
    2048, 2093, 2139, 2186, 2233, 2282, 2332, 2383, 2435, 2489, 2543, 2599, 2656, 2714, 2774, 2834, 2896, 2960, 3025,
    3091, 3158, 3228, 3298, 3371, 3444, 3520, 3597, 3676, 3756, 3838, 3922, 4008,
];
const QM4: [i32; 16] = [0, -20456, -12896, -8968, -6288, -4240, -2584, -1200, 20456, 12896, 8968, 6288, 4240, 2584, 1200, 0];
const QM2: [i32; 4] = [-7408, -1616, 7408, 1616];
const WH: [i32; 3] = [0, -214, 798];
const RH2: [usize; 4] = [2, 1, 2, 1];
const IHN: [usize; 3] = [0, 1, 0];
const IHP: [usize; 3] = [0, 3, 2];

fn sat(x: i32) -> i32 {
    x.clamp(-32768, 32767)
}

/// Código de 6 bits do nível `k` (1..=30) com o sinal dado.
fn code6(k: usize, negative: bool) -> u8 {
    (if !negative {
        62 - k
    } else if k < 3 {
        64 - k
    } else {
        34 - k
    }) as u8
}

/// Nível de reconstrução (com sinal) de um código de 6 bits.
fn dequant6(code: u8) -> i32 {
    let c = code as usize;
    match c {
        0..=3 => -LEVEL[1],
        4..=31 => -LEVEL[34 - c],
        32..=61 => LEVEL[62 - c],
        62 => -LEVEL[2],
        _ => -LEVEL[1],
    }
}

/// Estado do ADPCM de uma sub-banda.
struct Band {
    s: i32,
    sp: i32,
    sz: i32,
    r: [i32; 3],
    a: [i32; 3],
    ap: [i32; 3],
    p: [i32; 3],
    d: [i32; 7],
    b: [i32; 7],
    bp: [i32; 7],
    sg: [i32; 7],
    nb: i32,
    det: i32,
}

impl Band {
    fn new(det: i32) -> Self {
        Self {
            s: 0, sp: 0, sz: 0, r: [0; 3], a: [0; 3], ap: [0; 3], p: [0; 3], d: [0; 7], b: [0; 7], bp: [0; 7],
            sg: [0; 7], nb: 0, det,
        }
    }

    /// Adapta o preditor com a diferença quantizada `d`.
    fn adapt(&mut self, d: i32) {
        self.d[0] = d;
        self.r[0] = sat(self.s + d);
        self.p[0] = sat(self.sz + d);
        for i in 0..3 {
            self.sg[i] = self.p[i] >> 15;
        }
        let wd1 = sat(self.a[1] << 2);
        let mut wd2 = if self.sg[0] == self.sg[1] { -wd1 } else { wd1 };
        wd2 = wd2.min(32767);
        let mut wd3 = if self.sg[0] == self.sg[2] { 128 } else { -128 };
        wd3 += wd2 >> 7;
        wd3 += (self.a[2] * 32512) >> 15;
        self.ap[2] = wd3.clamp(-12288, 12288);

        self.sg[0] = self.p[0] >> 15;
        self.sg[1] = self.p[1] >> 15;
        let wd1 = if self.sg[0] == self.sg[1] { 192 } else { -192 };
        let wd2 = (self.a[1] * 32640) >> 15;
        self.ap[1] = sat(wd1 + wd2);
        let wd3 = sat(15360 - self.ap[2]);
        self.ap[1] = self.ap[1].clamp(-wd3, wd3);

        let wd1 = if d == 0 { 0 } else { 128 };
        self.sg[0] = d >> 15;
        for i in 1..7 {
            self.sg[i] = self.d[i] >> 15;
            let wd2 = if self.sg[i] == self.sg[0] { wd1 } else { -wd1 };
            let wd3 = (self.b[i] * 32640) >> 15;
            self.bp[i] = sat(wd2 + wd3);
        }
        for i in (1..7).rev() {
            self.d[i] = self.d[i - 1];
            self.b[i] = self.bp[i];
        }
        for i in (1..3).rev() {
            self.r[i] = self.r[i - 1];
            self.p[i] = self.p[i - 1];
            self.a[i] = self.ap[i];
        }
        let wd1 = sat(self.r[1] + self.r[1]);
        let wd1 = (self.a[1] * wd1) >> 15;
        let wd2 = sat(self.r[2] + self.r[2]);
        let wd2 = (self.a[2] * wd2) >> 15;
        self.sp = sat(wd1 + wd2);
        let mut sz = 0;
        for i in (1..7).rev() {
            let wd1 = sat(self.d[i] + self.d[i]);
            sz += (self.b[i] * wd1) >> 15;
        }
        self.sz = sat(sz);
        self.s = sat(self.sp + self.sz);
    }

    /// Adapta o passo de quantização (`det`) com o índice reduzido `idx`.
    fn step(&mut self, w: i32, max: i32, shift: i32) {
        self.nb = ((self.nb * 127) >> 7) + w;
        self.nb = self.nb.clamp(0, max);
        let wd1 = ((self.nb >> 6) & 31) as usize;
        let wd2 = shift - (self.nb >> 11);
        let wd3 = if wd2 < 0 { ILB[wd1] << -wd2 } else { ILB[wd1] >> wd2 };
        self.det = wd3 << 2;
    }
}

impl G722 {
    pub const SAMPLES: usize = 320;
    pub const RATE: u32 = 16000;

    pub fn new() -> Self {
        Self {
            enc: Qmf::default(),
            dec: Qmf::default(),
            eb: [Band::new(32), Band::new(8)],
            db: [Band::new(32), Band::new(8)],
            last: Vec::new(),
            losses: 0,
        }
    }

    fn encode_pair(&mut self, a: i16, b: i16) -> u8 {
        self.enc.push2(a as i32, b as i32);
        let (odd, even) = self.enc.sums();
        let xlow = (even + odd) >> 14;
        let xhigh = (even - odd) >> 14;

        let lo = &mut self.eb[0];
        let el = sat(xlow - lo.s);
        let wd = if el >= 0 { el } else { -(el + 1) };
        let mut k = 1;
        while k < 30 {
            if wd < ((Q6[k] * lo.det) >> 12) {
                break;
            }
            k += 1;
        }
        let ilow = code6(k, el < 0);
        let ril = (ilow >> 2) as usize;
        let dlow = (lo.det * QM4[ril]) >> 15;
        lo.step(WL[RL42[ril]], 18432, 8);
        lo.adapt(dlow);

        let hi = &mut self.eb[1];
        let eh = sat(xhigh - hi.s);
        let wd = if eh >= 0 { eh } else { -(eh + 1) };
        let mih = if wd >= ((564 * hi.det) >> 12) { 2 } else { 1 };
        let ihigh = if eh < 0 { IHN[mih] } else { IHP[mih] };
        let dhigh = (hi.det * QM2[ihigh]) >> 15;
        hi.step(WH[RH2[ihigh]], 22528, 10);
        hi.adapt(dhigh);

        ((ihigh as u8) << 6) | ilow
    }

    fn decode_pair(&mut self, code: u8) -> (i16, i16) {
        let ilow = code & 0x3f;
        let ihigh = ((code >> 6) & 3) as usize;

        let lo = &mut self.db[0];
        let rlow = (lo.s + ((lo.det * dequant6(ilow)) >> 15)).clamp(-16384, 16383);
        let ril = (ilow >> 2) as usize;
        let dlowt = (lo.det * QM4[ril]) >> 15;
        lo.step(WL[RL42[ril]], 18432, 8);
        lo.adapt(dlowt);

        let hi = &mut self.db[1];
        let dhigh = (hi.det * QM2[ihigh]) >> 15;
        let rhigh = (dhigh + hi.s).clamp(-16384, 16383);
        hi.step(WH[RH2[ihigh]], 22528, 10);
        hi.adapt(dhigh);

        self.dec.push2(rlow + rhigh, rlow - rhigh);
        let (odd, even) = self.dec.sums();
        (sat(even >> 11) as i16, sat(odd >> 11) as i16)
    }
}

impl Default for G722 {
    fn default() -> Self {
        Self::new()
    }
}

impl Codec for G722 {
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
        Ok(pcm.chunks(2).map(|p| self.encode_pair(p[0], p[1])).collect())
    }

    fn decode(&mut self, data: Option<&[u8]>) -> Vec<i16> {
        match data {
            Some(d) if d.len() == Self::SAMPLES / 2 => {
                self.losses = 0;
                self.last = d
                    .iter()
                    .flat_map(|c| {
                        let (a, b) = self.decode_pair(*c);
                        [a, b]
                    })
                    .collect();
                self.last.clone()
            }
            _ => {
                // PLC simples, como no µ-law: repete o último quadro com metade da amplitude; depois de 3, silêncio.
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

    /// Tom de 300 Hz e outro de 3 kHz (acima da banda do µ-law) passam pelo G.722 com SNR razoável.
    #[test]
    fn g722_roundtrip_has_good_snr() {
        let (mut e, mut d) = (G722::new(), G722::new());
        let sig: Vec<i16> = (0..16000)
            .map(|i| {
                let t = i as f64 / 16000.0;
                (6000.0 * (std::f64::consts::TAU * 300.0 * t).sin() + 4000.0 * (std::f64::consts::TAU * 3000.0 * t).sin()) as i16
            })
            .collect();
        let mut out = Vec::new();
        for f in sig.chunks(320) {
            let enc = e.encode(f).unwrap();
            assert_eq!(enc.len(), 160);
            out.extend(d.decode(Some(&enc)));
        }
        // Compensa o atraso do filtro (23 amostras) procurando o melhor alinhamento.
        let best = (0..40)
            .map(|lag| {
                let (mut sig_e, mut err_e) = (0f64, 0f64);
                for i in 4000..12000 {
                    let (a, b) = (sig[i] as f64, out[i + lag] as f64);
                    sig_e += a * a;
                    err_e += (a - b) * (a - b);
                }
                10.0 * (sig_e / err_e).log10()
            })
            .fold(f64::MIN, f64::max);
        assert!(best > 20.0, "SNR {best:.1} dB");
    }

    #[test]
    fn g722_loss_fades_to_silence() {
        let mut c = G722::new();
        let enc = c.encode(&vec![5000; 320]).unwrap();
        c.decode(Some(&enc));
        for _ in 0..4 {
            c.decode(None);
        }
        assert!(c.decode(None).iter().all(|&x| x == 0));
        assert!(matches!(c.encode(&[0; 160]), Err(MediaError::FrameLen { got: 160, want: 320 })));
    }
}
