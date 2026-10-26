//! Captura e codec de vídeo da tela compartilhada.
//!
//! ```text
//! ScreenSource::grab ─> H264Encoder::encode ─> ScreenShare::push ─> [circuito de tela]
//! ScreenEvent::Frame ─> H264Decoder::decode ─> RawFrame
//! ```
//!
//! O codec trabalha em **taxa constante** dentro da capacidade do circuito de tela
//! ([`ScreenStream::capacity_bps`]): com taxa variável, o tamanho dos quadros vazaria o que
//! está na tela. O controle de taxa do openh264 não respeita o orçamento por quadro (sob rolagem
//! brusca gasta de 3 a 10 vezes mais), então o controle é nosso: o QP de cada quadro sai do tamanho
//! dos anteriores e de quanto a imagem mudou, e [`pump_all`] só codifica quando a fila do circuito
//! esvaziou, de modo que o que se envia é sempre a tela atual e a fila nunca acumula quadros velhos.
//! Um quadro que passa de [`FRAME_LIMIT`] é descartado e o próximo sai como quadro-chave.
//! A captura fica atrás de [`ScreenSource`]; o X11 está em [`crate::capture`].

use crate::media::screen::{ScreenStream, FRAGMENT_PAYLOAD, MAX_FRAME, MAX_QUEUE};
use crate::phone::{ScreenShare, SCREEN_SLOT};
use openh264::decoder::Decoder;
use openh264::encoder::{Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, QpRange, RateControlMode, UsageType};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};
use thiserror::Error;

/// Quadros por segundo da transmissão quando o circuito é estreito (com cover).
pub const FPS: f32 = 15.0;
/// Quadros por segundo quando o circuito aguenta pelo menos [`HIGH_FPS_BITRATE`] (sem privacidade).
pub const HIGH_FPS: f32 = 30.0;
/// Taxa de vídeo, em bits/s, a partir da qual a transmissão passa a [`HIGH_FPS`]: abaixo disso, mais
/// quadros por segundo só dividiriam os mesmos bits em quadros piores.
pub const HIGH_FPS_BITRATE: u32 = 1_500_000;

/// Quadros por segundo adequados a uma taxa de vídeo.
pub fn fps_for(bitrate: u32) -> f32 {
    if bitrate >= HIGH_FPS_BITRATE { HIGH_FPS } else { FPS }
}

/// Largura máxima enviada: telas maiores são reduzidas por média de blocos, senão o quadro-chave
/// passa de [`MAX_FRAME`] e nada chega a quem assiste.
pub const MAX_WIDTH: usize = 1920;
const MAX_SHRINK: usize = 8;

#[derive(Debug, Error)]
pub enum VideoError {
    #[error("codec de vídeo: {0}")]
    Codec(String),
    #[error("imagem {0}x{1} pequena demais ou com buffer de tamanho errado")]
    Size(usize, usize),
    #[error("o circuito de tela recusou o quadro")]
    Push,
}

fn codec_err(e: openh264::Error) -> VideoError {
    VideoError::Codec(e.to_string())
}

/// Imagem RGBA, 4 bytes por pixel, linha após linha.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl RawFrame {
    pub fn new(width: usize, height: usize, rgba: Vec<u8>) -> Result<Self, VideoError> {
        if width < 2 || height < 2 || rgba.len() != width * height * 4 {
            return Err(VideoError::Size(width, height));
        }
        Ok(Self { width, height, rgba })
    }

    /// Reduz por média de blocos `k`×`k` até caber em [`MAX_WIDTH`]; sem efeito se já cabe.
    fn shrunk(&self, min_k: usize) -> Option<RawFrame> {
        let k = self.width.div_ceil(MAX_WIDTH).max(min_k);
        if k <= 1 || self.rgba.len() != self.width * self.height * 4 {
            return None;
        }
        let (w, h) = (self.width / k, self.height / k);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let mut sum = [0u32; 4];
                for dy in 0..k {
                    let row = ((y * k + dy) * self.width + x * k) * 4;
                    for px in self.rgba[row..row + k * 4].as_chunks::<4>().0.iter() {
                        for c in 0..4 {
                            sum[c] += px[c] as u32;
                        }
                    }
                }
                rgba.extend(sum.map(|v| (v / (k * k) as u32) as u8));
            }
        }
        Some(RawFrame { width: w, height: h, rgba })
    }

    /// Corta para dimensões pares, como o H.264 (4:2:0) exige.
    fn even(&self) -> Option<std::borrow::Cow<'_, RawFrame>> {
        let (w, h) = (self.width & !1, self.height & !1);
        if w < 2 || h < 2 || self.rgba.len() != self.width * self.height * 4 {
            return None;
        }
        if (w, h) == (self.width, self.height) {
            return Some(std::borrow::Cow::Borrowed(self));
        }
        let mut rgba = Vec::with_capacity(w * h * 4);
        for row in self.rgba.chunks_exact(self.width * 4).take(h) {
            rgba.extend_from_slice(&row[..w * 4]);
        }
        Some(std::borrow::Cow::Owned(RawFrame { width: w, height: h, rgba }))
    }
}

/// Fragmentos na fila (≈ 40 ms de circuito) acima dos quais [`pump_all`] adia o quadro: a imagem
/// seguinte só é capturada e codificada quando o circuito já escoou o quadro anterior.
const GATE_BACKLOG: usize = 20;
/// Fragmentos na fila a partir dos quais um quadro de camada alta é largado em vez de enfileirado.
const DROP_BACKLOG: usize = 40;
/// Maior quadro que o circuito aceita com a fila no limite do adiamento: acima disso `push` o recusaria
/// (a fila passaria de [`MAX_QUEUE`]) e o quadro-chave de recuperação seria igualmente recusado.
pub const FRAME_LIMIT: usize = {
    let by_queue = (MAX_QUEUE - GATE_BACKLOG) * FRAGMENT_PAYLOAD;
    if by_queue < MAX_FRAME { by_queue } else { MAX_FRAME }
};

/// Faixa de QP do controlador (quanto maior, mais grosseiro e menor o quadro).
const QP_MIN: f32 = 28.0;
const QP_MAX: f32 = 51.0;
/// QP do primeiro quadro-chave; se não couber, o seguinte sai em [`QP_MAX`].
const QP_START: f32 = 38.0;
/// A cada +10 de QP o quadro encolhe pela metade, aproximadamente.
const QP_PER_OCTAVE: f32 = 10.0;
/// Quanto o QP pode subir ou descer entre dois quadros: subir é rápido (o quadro grande já custou),
/// descer é gradual (a imagem vai ficando nítida em vários quadros em vez de um quadro-chave).
const STEP_UP: f32 = 12.0;
const STEP_DOWN: f32 = 2.0;
/// Salto máximo de QP antecipado quando a imagem muda de repente, antes de ver o tamanho do quadro.
const JUMP_UP: f32 = 14.0;
/// Fração de pixels mudados abaixo da qual a imagem conta como parada: o quadro só refina a nitidez.
const STILL: f32 = 0.02;
/// Refinar de um QP a outro numa imagem parada custa um pico de bytes (a imagem toda é refeita). Pico
/// acima de tantos orçamentos de quadro encerra o refinamento naquele QP até a imagem se mexer de novo.
const SPIKE: f32 = 8.0;
/// Quadros grandes seguidos (acima de [`HEAVY`] orçamentos, já no QP mais grosseiro) antes de reduzir a resolução.
const HEAVY_FRAMES: u32 = 3;
const HEAVY: f32 = 2.0;
/// Maior redução extra por movimento.
const MAX_MOTION_K: usize = 3;
/// Menor fração de pixels mudados considerada na antecipação (evita dividir por zero).
const MIN_CHANGE: f32 = 0.02;
/// Passo, em pixels, da amostragem usada para medir quanto a imagem mudou.
const SAMPLE_STEP: usize = 4;
/// Diferença de canal acima da qual um pixel amostrado conta como mudado.
const CHANGE_DELTA: u8 = 24;

/// Fonte de imagens da tela.
pub trait ScreenSource {
    /// Imagem atual; `None` quando não deu para capturar agora (a transmissão pula o quadro).
    fn grab(&mut self) -> Option<RawFrame>;
}

/// Quadro codificado, pronto para [`ScreenShare::push`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Camada temporal superior: nenhum outro quadro depende dele, então pode ser largado sem quebrar
    /// a imagem (só cai a taxa de quadros). [`pump_all`] larga estes quando a fila do circuito enche.
    pub droppable: bool,
}

/// Codificador H.264 em taxa constante.
pub struct H264Encoder {
    inner: Encoder,
    size: Option<(usize, usize)>,
    /// Bytes que um quadro pode ter para que o vídeo caiba na taxa.
    target: f32,
    /// Quadros por segundo desta transmissão ([`fps_for`] da taxa).
    fps: f32,
    need_key: bool,
    /// Fator mínimo de redução; sobe quando o quadro-chave não cabe em [`FRAME_LIMIT`] (tela muito detalhada).
    min_k: usize,
    /// As duas camadas temporais já foram ligadas neste codificador.
    layered: bool,
    /// QP do próximo quadro, antes da antecipação por mudança brusca; ajustado pelo tamanho dos anteriores.
    qp: f32,
    /// QP que o codificador tem configurado agora.
    applied_qp: f32,
    /// Menor QP permitido enquanto a imagem está parada (ver [`SPIKE`]).
    floor: f32,
    /// Fator de redução extra durante movimento brusco que nem o QP mais grosseiro segura: menos pixels,
    /// menos bits por quadro, mais quadros por segundo. Volta a 1 depois de 1 s (`fps` quadros) de imagem parada.
    motion_k: usize,
    heavy: u32,
    calm: u32,
    /// Amostra (RGB de alguns pixels) do último quadro codificado e a fração que mudou em relação ao anterior.
    sample: Vec<u8>,
    prev_change: f32,
    /// Quadros que o codificador recebeu e não devolveu (pulados ou grandes demais).
    pub skipped: u64,
    /// Quadros largados da camada alta porque a fila do circuito estava cheia.
    pub layer_dropped: u64,
    /// Quadros adiados porque o circuito ainda escoava o anterior.
    pub held: u64,
    /// Média móvel, em ms, do tempo gasto em `encode`.
    pub encode_ms: f32,
}

impl H264Encoder {
    /// Taxa em bits/s do vídeo: 90% do que o circuito de tela carrega; a folga absorve a ponta do último
    /// fragmento de cada quadro, e o excesso de um pico vai para a fila (que larga a camada alta).
    pub fn default_bitrate() -> u32 {
        Self::bitrate_for(SCREEN_SLOT)
    }

    /// Como [`Self::default_bitrate`], para um circuito que manda um pacote a cada `slot`.
    pub fn bitrate_for(slot: std::time::Duration) -> u32 {
        (ScreenStream::capacity_bps(slot) * 9 / 10) as u32
    }

    pub fn new(bitrate: u32) -> Result<Self, VideoError> {
        let qp = QP_START;
        let fps = fps_for(bitrate);
        Ok(Self {
            inner: Self::open(qp, fps)?,
            size: None,
            target: bitrate as f32 / 8.0 / fps,
            fps,
            need_key: true,
            min_k: 1,
            layered: false,
            qp,
            applied_qp: qp,
            floor: QP_MIN,
            motion_k: 1,
            heavy: 0,
            calm: 0,
            sample: Vec::new(),
            prev_change: 0.0,
            skipped: 0,
            layer_dropped: 0,
            held: 0,
            encode_ms: 0.0,
        })
    }

    fn open(qp: f32, fps: f32) -> Result<Encoder, VideoError> {
        let qp = qp.round() as u8;
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            // O controle de taxa do openh264 estoura o orçamento sob movimento; sem ele o QP é o que
            // `set_qp` manda em cada quadro, e quem decide o tamanho é este módulo.
            .rate_control_mode(RateControlMode::Off)
            .qp(QpRange::new(qp, qp))
            .max_frame_rate(FrameRate::from_hz(fps))
            .skip_frames(false)
            // Quadro-chave grande (dezenas de kB) leva segundos no circuito e congela a imagem: só periódico
            // de longe em longe; perda e quem entra pedem o seu por `WANT_KEY`.
            .intra_frame_period(IntraFramePeriod::from_num_frames((fps * 30.0) as u32));
        Encoder::with_api_config(openh264::OpenH264API::from_source(), config).map_err(codec_err)
    }

    /// Quadros por segundo em que quem chama deve entregar imagens.
    pub fn fps(&self) -> f32 {
        self.fps
    }

    /// QP configurado no último quadro (diagnóstico): quanto maior, mais grosseira a imagem.
    pub fn qp(&self) -> u8 {
        self.applied_qp as u8
    }

    /// Fator de redução da imagem enviada (1 = tamanho cheio), para diagnóstico.
    pub fn scale(&self) -> usize {
        self.min_k.max(self.motion_k)
    }

    /// Fração (0 a 1) de pixels amostrados que mudaram em relação ao último quadro codificado; guarda a
    /// amostra deste quadro. Resolução nova conta como mudança total.
    fn measure_change(&mut self, frame: &RawFrame) -> f32 {
        let mut sample = Vec::with_capacity(self.sample.len());
        for y in (0..frame.height).step_by(SAMPLE_STEP) {
            let row = &frame.rgba[y * frame.width * 4..(y + 1) * frame.width * 4];
            for px in row.as_chunks::<{ 4 * SAMPLE_STEP }>().0 {
                sample.extend_from_slice(&px[..3]);
            }
        }
        let changed = if sample.len() == self.sample.len() {
            let (now, before) = (sample.as_chunks::<3>().0, self.sample.as_chunks::<3>().0);
            let differ = now.iter().zip(before).filter(|(a, b)| a.iter().zip(*b).any(|(x, y)| x.abs_diff(*y) > CHANGE_DELTA)).count();
            differ as f32 / now.len().max(1) as f32
        } else {
            1.0
        };
        self.sample = sample;
        changed
    }

    /// QP do quadro que vai ser codificado. Parte do QP que o tamanho dos quadros anteriores pede e sobe
    /// de uma vez quando a imagem mudou muito mais do que no quadro anterior (rolagem brusca, troca de
    /// tela): o primeiro quadro após a mudança é o mais caro e o único que o retorno não alcança a tempo.
    /// Quadro-chave não sobe: já é o quadro de recomeço, e subir o QP à toa o deixaria borrado.
    fn plan_qp(&mut self, changed: f32, key: bool) -> f32 {
        let jump = if key { 0.0 } else { (QP_PER_OCTAVE * (changed.max(MIN_CHANGE) / self.prev_change.max(MIN_CHANGE)).log2()).clamp(0.0, JUMP_UP) };
        self.prev_change = changed;
        (self.qp + jump).clamp(QP_MIN, QP_MAX)
    }

    /// Aprende com o tamanho do quadro. Imagem parada: o quadro refina a nitidez, um QP a cada quadro, e
    /// um pico grande fixa o piso (o que pagou já está na tela). Imagem em movimento: acima do orçamento
    /// o QP sobe de uma vez, abaixo desce devagar; se nem o QP mais grosseiro segura, a resolução cai.
    fn learn(&mut self, used_qp: f32, size: usize, changed: f32) {
        let ratio = size.max(1) as f32 / self.target;
        if changed < STILL {
            self.heavy = 0;
            self.calm += 1;
            if self.calm >= self.fps as u32 {
                self.motion_k = 1;
            }
            if ratio > SPIKE {
                self.floor = used_qp;
            }
            self.qp = (used_qp - 1.0).max(self.floor).max(QP_MIN);
            return;
        }
        self.calm = 0;
        self.floor = QP_MIN;
        if used_qp >= QP_MAX - 3.0 && ratio > HEAVY {
            self.heavy += 1;
            if self.heavy >= HEAVY_FRAMES {
                self.motion_k = (self.scale() + 1).min(MAX_MOTION_K);
                self.heavy = 0;
            }
        } else {
            self.heavy = 0;
        }
        let step = (QP_PER_OCTAVE * ratio.log2()).clamp(-STEP_DOWN, STEP_UP);
        self.qp = (used_qp + step).clamp(QP_MIN, QP_MAX);
    }

    /// Codifica um quadro. `force_key` pede quadro-chave. `Ok(None)`: o codificador pulou o quadro
    /// ou ele passou de [`MAX_FRAME`] (o próximo sai como quadro-chave).
    pub fn encode(&mut self, frame: &RawFrame, force_key: bool) -> Result<Option<EncodedFrame>, VideoError> {
        let started = std::time::Instant::now();
        let out = self.encode_inner(frame, force_key);
        self.encode_ms += (started.elapsed().as_secs_f32() * 1000.0 - self.encode_ms) * 0.1;
        if matches!(out, Ok(None)) {
            self.skipped += 1;
        }
        out
    }

    fn encode_inner(&mut self, frame: &RawFrame, force_key: bool) -> Result<Option<EncodedFrame>, VideoError> {
        let small = frame.shrunk(self.min_k.max(self.motion_k));
        let frame = small.as_ref().unwrap_or(frame);
        let frame = frame.even().ok_or(VideoError::Size(frame.width, frame.height))?;
        let dims = (frame.width, frame.height);
        // O H.264 do openh264 fixa a resolução na abertura: tela nova, codificador novo.
        if self.size.is_some_and(|s| s != dims) {
            self.inner = Self::open(self.qp, self.fps)?;
            self.applied_qp = self.qp;
            self.layered = false;
            self.need_key = true;
        }
        self.size = Some(dims);
        let want_key = force_key || self.need_key;
        if want_key {
            self.inner.force_intra_frame();
        }
        let changed = self.measure_change(&frame);
        let qp = self.plan_qp(changed, want_key).round();
        let yuv = YUVBuffer::from_rgba8_source(RgbaSliceU8::new(&frame.rgba, dims));
        if !self.layered {
            // O openh264 só inicializa no primeiro quadro; as camadas temporais entram depois disso.
            self.inner.encode(&yuv).map_err(codec_err)?;
            configure(&mut self.inner, qp, true)?;
            self.applied_qp = qp;
            self.inner.force_intra_frame();
            self.layered = true;
        } else if qp != self.applied_qp {
            configure(&mut self.inner, qp, false)?;
            self.applied_qp = qp;
        }
        let bits = self.inner.encode(&yuv).map_err(codec_err)?;
        let key = matches!(bits.frame_type(), FrameType::IDR | FrameType::I);
        let droppable = !key && bits.num_layers().checked_sub(1).and_then(|i| bits.layer(i)).is_some_and(|l| l.raw_info().uiTemporalId > 0);
        if matches!(bits.frame_type(), FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let mut data = Vec::new();
        bits.write_vec(&mut data);
        if data.is_empty() {
            return Ok(None);
        }
        if data.len() > FRAME_LIMIT {
            // Grosseiro ao máximo e ainda grande: menos pixels no próximo; senão, só mais grosseiro, e
            // sem refinar de volta até este QP (o pico seria o mesmo).
            if qp >= QP_MAX {
                self.min_k = (self.min_k + 1).min(MAX_SHRINK);
            } else if !key {
                self.floor = (qp + 3.0).min(QP_MAX);
            }
            self.qp = QP_MAX;
            self.need_key = true;
            return Ok(None);
        }
        if key {
            self.need_key = false;
        }
        if want_key {
            // Quadro-chave pedido é sempre bem maior que o orçamento: não serve de medida para os seguintes.
            self.qp = qp;
        } else {
            self.learn(qp, data.len(), changed);
        }
        Ok(Some(EncodedFrame { data, keyframe: key, droppable }))
    }
}

/// Ajusta o codificador já inicializado: o QP do quadro seguinte e, uma vez, as duas camadas temporais
/// (quadros pares, camada 0, servem de referência; os ímpares, camada 1, não). O crate `openh264` não
/// expõe nenhum dos dois; entram pela API crua, sem reiniciar o fluxo nem gerar quadro-chave.
fn configure(enc: &mut Encoder, qp: f32, layers: bool) -> Result<(), VideoError> {
    use openh264_sys2::{ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, SEncParamExt};
    // SAFETY: `SEncParamExt` é só dados; o codificador preenche a estrutura inteira em `get_option`,
    // e `set_option` a lê sem guardar o ponteiro.
    unsafe {
        let mut params: SEncParamExt = std::mem::zeroed();
        let raw = enc.raw_api();
        if raw.get_option(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, (&raw mut params).cast()) != 0 {
            return Err(VideoError::Codec("não deu para ler os parâmetros do codificador".into()));
        }
        if layers {
            params.iTemporalLayerNum = 2;
        }
        // Sem controle de taxa, o QP do quadro é o da camada espacial.
        params.sSpatialLayers[0].iDLayerQp = qp as i32;
        params.iMinQp = qp as i32;
        params.iMaxQp = qp as i32;
        if raw.set_option(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, (&raw mut params).cast()) != 0 {
            return Err(VideoError::Codec("não deu para ajustar o codificador".into()));
        }
    }
    Ok(())
}

/// Decodificador H.264.
pub struct H264Decoder {
    inner: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self, VideoError> {
        Ok(Self { inner: Decoder::new().map_err(codec_err)? })
    }

    /// Decodifica um quadro. `Ok(None)`: ainda sem imagem (falta o quadro-chave).
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<RawFrame>, VideoError> {
        let Some(yuv) = self.inner.decode(data).map_err(codec_err)? else { return Ok(None) };
        let (width, height) = yuv.dimensions();
        let mut rgba = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut rgba);
        Ok(Some(RawFrame { width, height, rgba }))
    }
}

/// Um passo da transmissão: captura, codifica e entrega ao circuito. Chame a cada `1/FPS` s.
/// Devolve `true` se um quadro seguiu. Atende o pedido de quadro-chave de quem assiste.
pub fn pump(source: &mut dyn ScreenSource, encoder: &mut H264Encoder, share: &ScreenShare) -> Result<bool, VideoError> {
    Ok(pump_all(source, encoder, &[share], false)? > 0)
}

/// Como [`pump`], para vários espectadores: o mesmo quadro codificado vai a todos os circuitos.
/// `force_key` pede quadro-chave (por exemplo, entrou um espectador). Devolve para quantos o quadro seguiu.
pub fn pump_all(source: &mut dyn ScreenSource, encoder: &mut H264Encoder, shares: &[&ScreenShare], force_key: bool) -> Result<usize, VideoError> {
    // Todos precisam ser consultados: `any` pararia no primeiro e deixaria pedidos pendentes.
    let wanted = shares.iter().fold(force_key, |w, s| s.keyframe_wanted() | w);
    // O circuito ainda escoa o quadro anterior: capturar agora só criaria um quadro velho na fila.
    // O pedido de quadro-chave fica guardado para o próximo quadro que sair.
    if shares.iter().any(|s| s.queued() > GATE_BACKLOG) {
        encoder.need_key |= wanted;
        encoder.held += 1;
        return Ok(0);
    }
    let Some(raw) = source.grab() else { return Ok(0) };
    let Some(frame) = encoder.encode(&raw, wanted)? else { return Ok(0) };
    let mut sent = 0;
    for share in shares {
        // Fila cheia: larga o quadro que ninguém usa de referência em vez de atrasar todos os seguintes.
        if frame.droppable && share.queued() >= DROP_BACKLOG {
            encoder.layer_dropped += 1;
            continue;
        }
        match share.push(&frame.data, frame.keyframe) {
            Ok(true) => sent += 1,
            Ok(false) => encoder.need_key = true,
            Err(_) => return Err(VideoError::Push),
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gradiente que se mexe com `t`, com algum ruído para dar trabalho ao codec.
    fn scene(w: usize, h: usize, t: usize) -> RawFrame {
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let n = (((x * 7 + y * 13 + t * 31) * 2654435761usize) >> 24) as u8;
                rgba.extend_from_slice(&[((x + t * 4) % 256) as u8, (y % 256) as u8, n / 8 + 100, 255]);
            }
        }
        RawFrame::new(w, h, rgba).unwrap()
    }

    #[test]
    fn first_frame_is_a_keyframe_and_decodes_to_the_same_size() {
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let mut dec = H264Decoder::new().unwrap();
        let f = enc.encode(&scene(320, 240, 0), false).unwrap().expect("quadro");
        assert!(f.keyframe);
        assert!(f.data.len() <= MAX_FRAME);
        let out = dec.decode(&f.data).unwrap().expect("imagem");
        assert_eq!((out.width, out.height), (320, 240));
    }

    #[test]
    fn decoded_image_resembles_the_source() {
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let mut dec = H264Decoder::new().unwrap();
        let src = RawFrame::new(64, 64, [200u8, 30, 30, 255].repeat(64 * 64)).unwrap();
        let mut out = None;
        for _ in 0..5 {
            if let Some(f) = enc.encode(&src, false).unwrap() {
                out = dec.decode(&f.data).unwrap().or(out);
            }
        }
        let out = out.expect("imagem");
        let px = &out.rgba[(32 * 64 + 32) * 4..][..3];
        assert!(px[0] > 150 && px[1] < 80 && px[2] < 80, "{px:?}");
    }

    #[test]
    fn odd_sizes_are_cropped_to_even() {
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let mut dec = H264Decoder::new().unwrap();
        let f = enc.encode(&scene(101, 75, 0), false).unwrap().expect("quadro");
        let out = dec.decode(&f.data).unwrap().expect("imagem");
        assert_eq!((out.width, out.height), (100, 74));
    }

    #[test]
    fn wide_screens_are_shrunk_to_fit_the_frame_limit() {
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let f = enc.encode(&scene(2560, 1440, 0), false).unwrap().expect("quadro-chave cabe");
        assert!(f.keyframe && f.data.len() <= MAX_FRAME);
        let mut dec = H264Decoder::new().unwrap();
        let img = dec.decode(&f.data).unwrap().expect("imagem");
        assert_eq!((img.width, img.height), (1280, 720)); // 2560 / 2: redução inteira por blocos
    }

    #[test]
    fn a_keyframe_that_does_not_fit_makes_the_encoder_send_fewer_pixels() {
        let detailed = |t: usize| {
            let rgba = (0..1920 * 1080).flat_map(|i| {
                let n = (((i + t) * 2654435761usize) >> 24) as u8;
                [n, n.wrapping_mul(3), n ^ 90, 255]
            });
            RawFrame::new(1920, 1080, rgba.collect()).unwrap()
        };
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let key = (0..10).find_map(|t| enc.encode(&detailed(t), false).unwrap()).expect("um quadro-chave acaba cabendo");
        assert!(key.keyframe && key.data.len() <= MAX_FRAME);
        assert!(enc.min_k > 1);
    }

    /// Página de texto sintético, `w` de largura por `h` de altura.
    fn text_page(w: usize, h: usize, seed: u64) -> Vec<u8> {
        let mut px = vec![255u8; w * h * 4];
        let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut rnd = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 33) as usize
        };
        for line in 0..h / 18 {
            let y0 = line * 18 + 3;
            let mut x = 20;
            while x < w - 40 {
                let len = 4 + rnd() % 40;
                for y in y0..y0 + 11 {
                    for x in x..(x + len).min(w) {
                        if rnd() % 3 != 0 {
                            px[(y * w + x) * 4..][..3].copy_from_slice(&[20, 20, 30]);
                        }
                    }
                }
                x += len + 8;
            }
        }
        px
    }

    /// Simula o circuito de tela: um quadro por tick de 1/FPS, a fila escoando `1 / SCREEN_SLOT / FPS`
    /// fragmentos por tick e o quadro só sendo codificado com a fila abaixo de `GATE_BACKLOG`, como em
    /// [`pump_all`]. Devolve (quadros enviados, maior fila em fragmentos, quadros que o circuito recusaria).
    fn run_ticks(enc: &mut H264Encoder, queue: &mut f32, ticks: usize, view: impl Fn(usize) -> RawFrame) -> (usize, f32, usize) {
        let drain = (1.0 / SCREEN_SLOT.as_secs_f32()) / FPS;
        let (mut sent, mut max, mut refused) = (0, 0f32, 0);
        for t in 0..ticks {
            *queue = (*queue - drain).max(0.0);
            if *queue > GATE_BACKLOG as f32 {
                enc.held += 1;
                continue;
            }
            let Some(f) = enc.encode(&view(t), false).unwrap() else { continue };
            let frags = f.data.len().div_ceil(FRAGMENT_PAYLOAD) as f32;
            if !f.keyframe && *queue + frags > MAX_QUEUE as f32 {
                refused += 1;
                continue;
            }
            *queue = if f.keyframe { frags } else { *queue + frags };
            max = max.max(*queue);
            sent += 1;
        }
        (sent, max, refused)
    }

    #[test]
    fn brusque_movement_never_piles_up_frames_in_the_circuit_queue() {
        let (w, h) = (960usize, 540usize);
        let (a, b) = (text_page(w, 3000, 1), text_page(w, 3000, 2));
        let view = |page: &Vec<u8>, off: usize| RawFrame::new(w, h, page[off * w * 4..(off + h) * w * 4].to_vec()).unwrap();
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let mut queue = 0.0;
        let limit = (MAX_QUEUE / 2) as f32; // ≈ 0,66 s de circuito
        type Scene = Box<dyn Fn(usize) -> (u8, usize)>;
        let scenes: [(&str, usize, Scene); 5] = [
            ("parada", 20, Box::new(|_| (0, 0))),
            ("rolagem rápida", 30, Box::new(|t| (0, t * 40))),
            ("rolagem brusca", 30, Box::new(|t| (0, t * 150 % 2400))),
            ("troca de página", 30, Box::new(|t| (u8::from(t / 5 % 2 == 1), 100))),
            ("parada", 30, Box::new(|_| (0, 100))),
        ];
        for (name, ticks, scene) in scenes {
            let (sent, max, refused) = run_ticks(&mut enc, &mut queue, ticks, |t| {
                let (which, off) = scene(t);
                view(if which == 0 { &a } else { &b }, off)
            });
            assert_eq!(refused, 0, "{name}: o circuito recusaria quadros");
            assert!(max <= limit, "{name}: fila de {max} fragmentos");
            assert!(sent >= 3, "{name}: só {sent} quadros em {ticks}");
        }
    }

    #[test]
    fn a_still_screen_refines_without_spikes_that_block_the_circuit() {
        let (w, h) = (960usize, 540usize);
        let page = text_page(w, h, 3);
        let still = RawFrame::new(w, h, page).unwrap();
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let mut queue = 0.0;
        let (sent, max, refused) = run_ticks(&mut enc, &mut queue, 60, |_| still.clone());
        assert_eq!(refused, 0);
        assert!(sent > 10);
        // O refinamento de uma imagem parada paga em picos; nenhum pode ocupar o circuito por mais de ≈ 0,7 s.
        assert!(max <= (MAX_QUEUE / 2) as f32, "pico de {max} fragmentos");
        assert!(enc.qp() < QP_START as u8, "a imagem não ficou mais nítida: qp {}", enc.qp());
    }

    #[test]
    fn sustained_heavy_frames_reduce_the_resolution_and_a_calm_screen_restores_it() {
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        assert_eq!(enc.scale(), 1);
        let (w, h) = (1280usize, 720usize);
        let page = text_page(w, 6000, 4);
        let view = |t: usize| RawFrame::new(w, h, page[(t * 200 % 5000) * w * 4..][..w * h * 4].to_vec()).unwrap();
        let mut t = 0;
        while enc.scale() == 1 && t < 60 {
            enc.encode(&view(t), false).unwrap();
            t += 1;
        }
        assert!(enc.scale() >= 2, "a resolução não caiu em {t} quadros");
        let still = view(0);
        for _ in 0..FPS as u32 + 2 {
            enc.encode(&still, false).unwrap();
        }
        assert_eq!(enc.scale(), 1);
    }

    #[test]
    fn odd_frames_are_droppable_and_the_rest_still_decodes_without_them() {
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let mut full = H264Decoder::new().unwrap();
        let mut base = H264Decoder::new().unwrap();
        let (mut droppable, mut kept) = (0, 0);
        for t in 0..30 {
            let Some(f) = enc.encode(&scene(640, 360, t), false).unwrap() else { continue };
            assert!(full.decode(&f.data).unwrap().is_some() || t == 0);
            if f.droppable {
                assert!(!f.keyframe);
                droppable += 1;
            } else {
                kept += 1;
                let img = base.decode(&f.data).unwrap();
                assert!(img.is_some(), "quadro {t} não decodificou sem a camada alta");
            }
        }
        assert!(droppable >= 10 && kept >= 10, "{droppable} largáveis, {kept} mantidos");
    }

    #[test]
    fn force_key_makes_a_keyframe_and_p_frames_follow() {
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        assert!(enc.encode(&scene(160, 120, 0), false).unwrap().unwrap().keyframe);
        let mut saw_p = false;
        for t in 1..4 {
            if let Some(f) = enc.encode(&scene(160, 120, t), false).unwrap() {
                saw_p |= !f.keyframe;
            }
        }
        assert!(saw_p);
        let forced = enc.encode(&scene(160, 120, 9), true).unwrap().unwrap();
        assert!(forced.keyframe);
    }

    #[test]
    fn decoder_without_a_keyframe_shows_nothing() {
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let mut dec = H264Decoder::new().unwrap();
        enc.encode(&scene(160, 120, 0), false).unwrap();
        let p = (1..6).find_map(|t| enc.encode(&scene(160, 120, t), false).unwrap().filter(|f| !f.keyframe)).expect("quadro P");
        assert!(dec.decode(&p.data).ok().flatten().is_none());
    }

    #[test]
    fn bitrate_stays_inside_the_screen_circuit_capacity() {
        let bps = H264Encoder::default_bitrate();
        let mut enc = H264Encoder::new(bps).unwrap();
        let (mut bytes, mut max) = (0usize, 0usize);
        let frames = FPS as usize * 10;
        for t in 0..frames {
            if let Some(f) = enc.encode(&scene(640, 360, t), false).unwrap() {
                bytes += f.data.len();
                max = max.max(f.data.len());
            }
        }
        let used = bytes as u64 * 8 / 10;
        assert!(max <= MAX_FRAME);
        assert!(used < ScreenStream::capacity_bps(SCREEN_SLOT), "{used} bps");
    }

    #[test]
    fn open_circuit_gets_a_higher_bitrate_and_30_fps() {
        let (private, open) = (H264Encoder::bitrate_for(SCREEN_SLOT), H264Encoder::bitrate_for(crate::phone::OPEN_SCREEN_SLOT));
        assert!(open >= HIGH_FPS_BITRATE && private < HIGH_FPS_BITRATE, "{private} / {open}");
        assert_eq!(fps_for(private), FPS);
        assert_eq!(fps_for(open), HIGH_FPS);
        let mut enc = H264Encoder::new(open).unwrap();
        assert_eq!(enc.fps(), HIGH_FPS);
        // O orçamento por quadro é o da taxa dividida pelos 30 quadros, e o quadro cabe no circuito.
        let f = enc.encode(&scene(320, 180, 0), false).unwrap().unwrap();
        assert!(f.keyframe && f.data.len() <= MAX_FRAME);
    }

    #[test]
    fn resolution_change_reopens_the_encoder_with_a_keyframe() {
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let mut dec = H264Decoder::new().unwrap();
        enc.encode(&scene(160, 120, 0), false).unwrap();
        let f = enc.encode(&scene(200, 100, 1), false).unwrap().unwrap();
        assert!(f.keyframe);
        let out = dec.decode(&f.data).unwrap().unwrap();
        assert_eq!((out.width, out.height), (200, 100));
    }

    #[test]
    fn raw_frame_rejects_wrong_buffer_size() {
        assert!(RawFrame::new(10, 10, vec![0; 10]).is_err());
        assert!(RawFrame::new(1, 10, vec![0; 40]).is_err());
    }
}
