//! Codecs de voz. A trait é o ponto de encaixe; vêm o G.722 e o Opus (os dois em Rust puro, a
//! 16 kHz) e o G.711 µ-law, que tem quadro fixo de 160 bytes (cabe no pacote) e serve para
//! testar a camada.

use opus_pure::{Application, OpusDecoder, OpusEncoder};

use super::{MediaError, packet::MAX_PAYLOAD};

/// Qual codec uma chamada usa. Quem liga escolhe e diz no convite (veja
/// [`Caller::invite_codec`](super::signal::Caller::invite_codec)); convite sem a indicação
/// (telefone antigo) é G.722.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum CodecId {
    /// G.711 µ-law, 8 kHz.
    Mulaw,
    /// G.722, 16 kHz, 64 kbps.
    #[default]
    G722,
    /// Opus, 16 kHz, 32 kbps.
    Opus,
}

impl CodecId {
    /// Todos, para quem precisa tentar um a um.
    pub const ALL: [CodecId; 3] = [CodecId::Mulaw, CodecId::G722, CodecId::Opus];

    /// Número que entra na etiqueta do convite.
    pub(crate) fn wire(self) -> u8 {
        match self {
            CodecId::Mulaw => 1,
            CodecId::G722 => 2,
            CodecId::Opus => 3,
        }
    }

    /// Taxa de amostragem do áudio que o codec consome e produz.
    pub fn sample_rate(self) -> u32 {
        match self {
            CodecId::Mulaw => 8000,
            CodecId::G722 | CodecId::Opus => 16000,
        }
    }

    /// Um codec novo deste tipo.
    pub fn build(self) -> AnyCodec {
        match self {
            CodecId::Mulaw => AnyCodec::Mulaw(Mulaw::new()),
            CodecId::G722 => AnyCodec::G722(Box::default()),
            CodecId::Opus => AnyCodec::Opus(Box::default()),
        }
    }
}

/// Codec escolhido em tempo de execução (o app só sabe qual é depois de ler a configuração ou o convite).
pub enum AnyCodec {
    Mulaw(Mulaw),
    G722(Box<G722>),
    Opus(Box<Opus>),
}

impl Codec for AnyCodec {
    fn id(&self) -> CodecId {
        match self {
            AnyCodec::Mulaw(c) => c.id(),
            AnyCodec::G722(c) => c.id(),
            AnyCodec::Opus(c) => c.id(),
        }
    }

    fn frame_ms(&self) -> u64 {
        match self {
            AnyCodec::Mulaw(c) => c.frame_ms(),
            AnyCodec::G722(c) => c.frame_ms(),
            AnyCodec::Opus(c) => c.frame_ms(),
        }
    }

    fn frame_samples(&self) -> usize {
        match self {
            AnyCodec::Mulaw(c) => c.frame_samples(),
            AnyCodec::G722(c) => c.frame_samples(),
            AnyCodec::Opus(c) => c.frame_samples(),
        }
    }

    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, MediaError> {
        match self {
            AnyCodec::Mulaw(c) => c.encode(pcm),
            AnyCodec::G722(c) => c.encode(pcm),
            AnyCodec::Opus(c) => c.encode(pcm),
        }
    }

    fn decode(&mut self, data: Option<&[u8]>) -> Vec<i16> {
        match self {
            AnyCodec::Mulaw(c) => c.decode(data),
            AnyCodec::G722(c) => c.decode(data),
            AnyCodec::Opus(c) => c.decode(data),
        }
    }
}

/// Codec de voz com quadros de duração fixa.
pub trait Codec {
    /// Qual é, para o convite dizer ao outro lado.
    fn id(&self) -> CodecId;
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

/// Quadros de perda seguidos até o som escondido chegar a zero.
const FADE_FRAMES: usize = 3;

/// Esconde quadros perdidos (PLC): continua a onda repetindo o último período de pitch do quadro anterior,
/// com um fade linear até o silêncio em [`FADE_FRAMES`] quadros. Quando o áudio volta, o início do primeiro
/// quadro bom é misturado com a continuação, para não haver degrau.
#[derive(Default)]
struct Concealer {
    /// Último quadro bom.
    last: Vec<i16>,
    /// Período repetido, em amostras (0 enquanto não houve perda desde o último quadro bom).
    period: usize,
    /// Posição dentro do período da próxima amostra escondida.
    phase: usize,
    /// Amostras escondidas desde o último quadro bom.
    lost: usize,
}

impl Concealer {
    /// Período do sinal em `x` (autocorrelação normalizada nos últimos `len/2` amostras, de 8 a 2 períodos por quadro).
    fn pitch(x: &[i16]) -> usize {
        let len = x.len();
        let (min_lag, max_lag, win) = ((len / 8).max(1), len / 2, len / 2);
        let mut best = (max_lag.max(1), 0.0f64);
        for lag in min_lag..=max_lag {
            let (mut corr, mut energy) = (0f64, 0f64);
            for n in len - win..len {
                let (a, b) = (x[n] as f64, x[n - lag] as f64);
                corr += a * b;
                energy += b * b;
            }
            let score = corr / (energy + 1.0).sqrt();
            if score > best.1 {
                best = (lag, score);
            }
        }
        best.0
    }

    /// `n` amostras da continuação, a partir do estado atual, sem alterá-lo.
    fn render(&self, n: usize) -> Vec<i16> {
        let (len, total) = (self.last.len(), (FADE_FRAMES * self.last.len()) as f64);
        (0..n)
            .map(|i| {
                let src = self.last[len - self.period + (self.phase + i) % self.period] as f64;
                let gain = (1.0 - (self.lost + i + 1) as f64 / total).max(0.0);
                (src * gain) as i16
            })
            .collect()
    }

    /// Um quadro perdido de `n` amostras.
    fn lost(&mut self, n: usize) -> Vec<i16> {
        if self.last.is_empty() {
            return vec![0; n];
        }
        if self.period == 0 {
            self.period = Self::pitch(&self.last);
            self.phase = 0;
        }
        let out = self.render(n);
        self.phase = (self.phase + n) % self.period;
        self.lost += n;
        out
    }

    /// Um quadro bom. Depois de perda, o começo dele é misturado com a continuação escondida.
    fn good(&mut self, mut frame: Vec<i16>) -> Vec<i16> {
        if self.lost > 0 && !self.last.is_empty() {
            let n = (frame.len() / 4).max(1).min(frame.len());
            let ext = self.render(n);
            for i in 0..n {
                let w = (i + 1) as f64 / (n + 1) as f64;
                frame[i] = (ext[i] as f64 * (1.0 - w) + frame[i] as f64 * w) as i16;
            }
        }
        // O quadro guardado é o que saiu (já misturado): a próxima perda continua dele.
        self.last.clone_from(&frame);
        self.period = 0;
        self.lost = 0;
        frame
    }
}

/// G.711 µ-law, 8 kHz, 20 ms: 160 amostras ↔ 160 bytes (64 kbps). Banda estreita; é o codec
/// de referência, não o de produção.
#[derive(Default)]
pub struct Mulaw {
    plc: Concealer,
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
    fn id(&self) -> CodecId {
        CodecId::Mulaw
    }

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
            Some(d) if d.len() == Self::SAMPLES => self.plc.good(d.iter().map(|b| mulaw_to_lin(*b)).collect()),
            // Quadro de tamanho errado conta como perda: não confiamos nele.
            _ => self.plc.lost(Self::SAMPLES),
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
    plc: Concealer,
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
            plc: Concealer::default(),
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
    fn id(&self) -> CodecId {
        CodecId::G722
    }

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
                let pcm = d
                    .iter()
                    .flat_map(|c| {
                        let (a, b) = self.decode_pair(*c);
                        [a, b]
                    })
                    .collect();
                self.plc.good(pcm)
            }
            _ => self.plc.lost(Self::SAMPLES),
        }
    }
}

/// Opus, 16 kHz, 20 ms: 320 amostras ↔ até [`MAX_PAYLOAD`] bytes, a 32 kbps (metade do G.722;
/// o codificador escolhe entre SILK, CELT e o híbrido conforme o sinal). Rust puro (`opus-pure`).
///
/// Sem DTX (o fluxo é contínuo, veja o módulo [`media`](super)) e sem FEC: o jitter entrega um
/// quadro por vez, sem olhar o seguinte, então a perda é escondida pelo PLC do próprio Opus.
pub struct Opus {
    enc: OpusEncoder,
    dec: OpusDecoder,
    /// Só para quadro que o decodificador recusa: segura o som com o esconderijo comum.
    plc: Concealer,
}

impl Opus {
    pub const SAMPLES: usize = 320;
    pub const RATE: u32 = 16000;
    /// Taxa média pedida ao codificador, em bits por segundo.
    pub const BITRATE: i32 = 32000;

    pub fn new() -> Self {
        let mut enc = OpusEncoder::new(Self::RATE as i32, 1, Application::Voip).expect("16 kHz mono é válido");
        enc.bitrate_bps = Self::BITRATE;
        enc.use_dtx = false;
        enc.use_inband_fec = false;
        Self { enc, dec: OpusDecoder::new(Self::RATE as i32, 1).expect("16 kHz mono é válido"), plc: Concealer::default() }
    }
}

impl Default for Opus {
    fn default() -> Self {
        Self::new()
    }
}

impl Codec for Opus {
    fn id(&self) -> CodecId {
        CodecId::Opus
    }

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
        let mut out = vec![0u8; MAX_PAYLOAD];
        let n = self.enc.encode_s16(pcm, Self::SAMPLES, &mut out).map_err(|_| MediaError::Malformed)?;
        out.truncate(n);
        Ok(out)
    }

    fn decode(&mut self, data: Option<&[u8]>) -> Vec<i16> {
        let mut pcm = vec![0i16; Self::SAMPLES];
        // Quadro vazio faz o Opus esconder a perda. Quadro que ele recusa conta como perda também.
        let ok = match data {
            Some(d) if !d.is_empty() => self.dec.decode_s16(d, Self::SAMPLES, &mut pcm).is_ok_and(|n| n == Self::SAMPLES),
            _ => return self.conceal(),
        };
        if ok { self.plc.good(pcm) } else { self.conceal() }
    }
}

impl Opus {
    /// Perda: o Opus esconde sozinho e o esconderijo comum acompanha, para o caso de o decodificador falhar depois.
    fn conceal(&mut self) -> Vec<i16> {
        let mut pcm = vec![0i16; Self::SAMPLES];
        match self.dec.decode_s16(&[], Self::SAMPLES, &mut pcm) {
            Ok(n) if n == Self::SAMPLES => self.plc.good(pcm),
            _ => self.plc.lost(Self::SAMPLES),
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
        assert!(peak(&l1) <= peak(&first) && peak(&l2) < peak(&l1));
        let l3 = c.decode(None);
        assert!(peak(&l3) < peak(&l2));
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
        for _ in 0..3 {
            c.decode(None);
        }
        assert!(c.decode(None).iter().all(|&x| x == 0));
        assert!(matches!(c.encode(&[0; 160]), Err(MediaError::FrameLen { got: 160, want: 320 })));
    }

    /// Dois quadros escondidos de um tom continuam a onda: erro pequeno contra o tom que seguiria, sem degrau na emenda.
    #[test]
    fn concealment_continues_the_waveform() {
        let hz = 300.0;
        let sig: Vec<i16> = (0..320 * 6).map(|i| (8000.0 * (std::f64::consts::TAU * hz * i as f64 / 16000.0).sin()) as i16).collect();
        let mut c = Concealer::default();
        c.good(sig[..320 * 3].to_vec());
        let hidden = c.lost(320);
        // Primeira metade do quadro escondido (ainda com ganho alto) ≈ onda original.
        let (mut err, mut energy) = (0f64, 0f64);
        for i in 0..160 {
            let (a, b) = (sig[960 + i] as f64, hidden[i] as f64);
            err += (a - b).powi(2);
            energy += a * a;
        }
        let snr = 10.0 * (energy / err).log10();
        assert!(snr > 15.0, "SNR {snr:.1} dB");
        // A volta do áudio não faz salto na emenda.
        let back = c.good(sig[1280..1600].to_vec());
        let jump = (back[0] as i32 - hidden[319] as i32).abs();
        assert!(jump < 3000, "salto {jump}");
    }

    #[test]
    fn pitch_finds_the_period_of_a_tone() {
        let x: Vec<i16> = (0..320).map(|i| (8000.0 * (std::f64::consts::TAU * 200.0 * i as f64 / 16000.0).sin()) as i16).collect();
        let p = Concealer::pitch(&x);
        // 200 Hz a 16 kHz: 80 amostras (ou um múltiplo).
        assert_eq!(p % 80, 0, "{p}");
    }

    /// Fala sintética (vogal com harmônicos) para o Opus, que trata tom puro de forma diferente.
    fn voice(n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| {
                let t = i as f64 / 16000.0;
                let f0 = 140.0;
                let ph = std::f64::consts::TAU * f0 * t;
                (3000.0 * ph.sin() + 2000.0 * (2.0 * ph).sin() + 1200.0 * (3.0 * ph).sin() + 600.0 * (5.0 * ph).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn opus_round_trip_keeps_the_voice_and_fits_the_packet() {
        let (mut e, mut d) = (Opus::new(), Opus::new());
        let sig = voice(16000 * 2);
        let (mut out, mut biggest, mut total) = (Vec::new(), 0, 0);
        for f in sig.chunks(320) {
            let enc = e.encode(f).unwrap();
            biggest = biggest.max(enc.len());
            total += enc.len();
            out.extend(d.decode(Some(&enc)));
        }
        assert!(biggest <= MAX_PAYLOAD, "pacote de {biggest} B");
        // 32 kbps = 80 B por quadro, com folga para a variação.
        assert!(total / 100 < 120, "média de {} B por quadro", total / 100);
        // O Opus codifica pelo que se ouve, não pela forma de onda (a fase de uma vogal sintética
        // não é preservada): compara a energia da banda de fala, que tem de sobreviver.
        let rms = |x: &[i16]| (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
        let (rin, rout) = (rms(&sig[8000..24000]), rms(&out[8000..24000]));
        assert!((rout / rin - 1.0).abs() < 0.25, "energia {rin:.0} -> {rout:.0}");
        // Tom fundamental (120 a 160 Hz) continua lá: a correlação com a entrada, no melhor atraso, é alta.
        let best = (0..320)
            .map(|lag| {
                let (mut ab, mut aa, mut bb) = (0f64, 0f64, 0f64);
                for i in 8000..24000 {
                    let (a, b) = (sig[i] as f64, out[i + lag] as f64);
                    ab += a * b;
                    aa += a * a;
                    bb += b * b;
                }
                ab / (aa * bb).sqrt()
            })
            .fold(f64::MIN, f64::max);
        assert!(best > 0.9, "correlação {best:.2}");
    }

    #[test]
    fn opus_hides_a_lost_frame_and_recovers() {
        let (mut e, mut d) = (Opus::new(), Opus::new());
        let sig = voice(320 * 20);
        let mut rms = Vec::new();
        for (i, f) in sig.chunks(320).enumerate() {
            let enc = e.encode(f).unwrap();
            let pcm = if i == 10 || i == 11 { d.decode(None) } else { d.decode(Some(&enc)) };
            assert_eq!(pcm.len(), 320);
            rms.push((pcm.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / 320.0).sqrt());
        }
        assert!(rms[10] > 100.0, "o quadro escondido tem som: {}", rms[10]);
        assert!(rms[15] > 1000.0, "depois da perda a voz volta: {}", rms[15]);
    }

    #[test]
    fn opus_rejects_wrong_frame_length_and_garbage() {
        let mut c = Opus::new();
        assert!(matches!(c.encode(&[0; 160]), Err(MediaError::FrameLen { got: 160, want: 320 })));
        assert_eq!(c.decode(Some(&[0xff; 5])).len(), 320);
        assert_eq!(c.decode(Some(&[])).len(), 320);
    }

    #[test]
    fn any_codec_builds_what_it_names() {
        for id in CodecId::ALL {
            let mut c = id.build();
            assert_eq!(c.id(), id);
            let pcm = vec![0i16; c.frame_samples()];
            let enc = c.encode(&pcm).unwrap();
            assert_eq!(c.decode(Some(&enc)).len(), c.frame_samples());
        }
    }
}
