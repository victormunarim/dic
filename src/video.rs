//! Captura e codec de vídeo da tela compartilhada.
//!
//! ```text
//! ScreenSource::grab ─> H264Encoder::encode ─> ScreenShare::push ─> [circuito de tela]
//! ScreenEvent::Frame ─> H264Decoder::decode ─> RawFrame
//! ```
//!
//! O codec trabalha em **taxa constante** dentro da capacidade do circuito de tela
//! ([`ScreenStream::capacity_bps`]): com taxa variável, o tamanho dos quadros vazaria o que
//! está na tela. Um quadro que passa de [`MAX_FRAME`] é descartado e o próximo sai como
//! quadro-chave. A captura fica atrás de [`ScreenSource`]; o X11 está em [`crate::capture`].

use crate::media::screen::{ScreenStream, MAX_FRAME};
use crate::phone::{ScreenShare, SCREEN_SLOT_MS};
use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};
use thiserror::Error;

/// Quadros por segundo da transmissão.
pub const FPS: f32 = 8.0;

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
}

/// Codificador H.264 em taxa constante.
pub struct H264Encoder {
    inner: Encoder,
    size: Option<(usize, usize)>,
    bitrate: u32,
    need_key: bool,
    /// Fator mínimo de redução; sobe quando o quadro-chave não cabe em [`MAX_FRAME`] (tela muito detalhada).
    min_k: usize,
}

impl H264Encoder {
    /// Taxa em bits/s do vídeo: 80% do que o circuito de tela carrega, sobra para o cabeçalho.
    pub fn default_bitrate() -> u32 {
        (ScreenStream::capacity_bps(SCREEN_SLOT_MS) * 8 / 10) as u32
    }

    pub fn new(bitrate: u32) -> Result<Self, VideoError> {
        Ok(Self { inner: Self::open(bitrate)?, size: None, bitrate, need_key: true, min_k: 1 })
    }

    fn open(bitrate: u32) -> Result<Encoder, VideoError> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(FPS))
            .skip_frames(true)
            .intra_frame_period(IntraFramePeriod::from_num_frames((FPS * 4.0) as u32));
        Encoder::with_api_config(openh264::OpenH264API::from_source(), config).map_err(codec_err)
    }

    /// Codifica um quadro. `force_key` pede quadro-chave. `Ok(None)`: o codificador pulou o quadro
    /// ou ele passou de [`MAX_FRAME`] (o próximo sai como quadro-chave).
    pub fn encode(&mut self, frame: &RawFrame, force_key: bool) -> Result<Option<EncodedFrame>, VideoError> {
        let small = frame.shrunk(self.min_k);
        let frame = small.as_ref().unwrap_or(frame);
        let frame = frame.even().ok_or(VideoError::Size(frame.width, frame.height))?;
        let dims = (frame.width, frame.height);
        // O H.264 do openh264 fixa a resolução na abertura: tela nova, codificador novo.
        if self.size.is_some_and(|s| s != dims) {
            self.inner = Self::open(self.bitrate)?;
            self.need_key = true;
        }
        self.size = Some(dims);
        if force_key || self.need_key {
            self.inner.force_intra_frame();
        }
        let yuv = YUVBuffer::from_rgba8_source(RgbaSliceU8::new(&frame.rgba, dims));
        let bits = self.inner.encode(&yuv).map_err(codec_err)?;
        let key = matches!(bits.frame_type(), FrameType::IDR | FrameType::I);
        if matches!(bits.frame_type(), FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let mut data = Vec::new();
        bits.write_vec(&mut data);
        if data.is_empty() {
            return Ok(None);
        }
        if data.len() > MAX_FRAME {
            if key {
                // Nem o quadro-chave cabe: menos pixels no próximo.
                self.min_k = (self.min_k + 1).min(MAX_SHRINK);
            }
            self.need_key = true;
            return Ok(None);
        }
        if key {
            self.need_key = false;
        }
        Ok(Some(EncodedFrame { data, keyframe: key }))
    }
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

/// Como [`pump`], para vários espectadores (sala): o mesmo quadro codificado vai a todos os circuitos.
/// `force_key` pede quadro-chave (por exemplo, entrou um espectador). Devolve para quantos o quadro seguiu.
pub fn pump_all(source: &mut dyn ScreenSource, encoder: &mut H264Encoder, shares: &[&ScreenShare], force_key: bool) -> Result<usize, VideoError> {
    // Todos precisam ser consultados: `any` pararia no primeiro e deixaria pedidos pendentes.
    let wanted = shares.iter().fold(force_key, |w, s| s.keyframe_wanted() | w);
    let Some(raw) = source.grab() else { return Ok(0) };
    let Some(frame) = encoder.encode(&raw, wanted)? else { return Ok(0) };
    let mut sent = 0;
    for share in shares {
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
        assert!(used < ScreenStream::capacity_bps(SCREEN_SLOT_MS), "{used} bps");
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
