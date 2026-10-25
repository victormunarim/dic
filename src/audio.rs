//! Microfone e alto-falante reais (feature `audio`, via `cpal`).
//!
//! O resto do crate fala PCM de 16 bits, mono, 8 kHz, em quadros de 20 ms (160 amostras).
//! Este módulo converte de/para o formato nativo do dispositivo (qualquer taxa, N canais).
//! Os streams do `cpal` não são `Send` em todas as plataformas, então ficam numa thread própria;
//! [`Audio`] só guarda filas compartilhadas e a encerra ao ser descartado.
//!
//! Sem cancelamento de eco: use fones, ou o alto-falante realimenta o microfone.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use cpal::{
    FromSample, SampleFormat, SizedSample, Stream,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

/// Taxa e tamanho de quadro do resto do crate.
pub const RATE: u32 = 8000;
pub const FRAME: usize = 160;

/// Latência máxima guardada em cada fila (400 ms): passou disso, descarta o mais antigo.
fn max_queued(rate: u32) -> usize {
    rate as usize * 4 / 10
}

#[derive(Debug, thiserror::Error)]
#[error("áudio: {0}")]
pub struct AudioError(pub String);

fn err(e: impl std::fmt::Display) -> AudioError {
    AudioError(e.to_string())
}

/// Reduz a taxa por média de caixa (filtro simples contra aliasing). Mono, `i16`.
pub struct Downsampler {
    from: u64,
    to: u64,
    phase: u64,
    acc: f64,
    n: u32,
}

impl Downsampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { from: from as u64, to: to as u64, phase: 0, acc: 0.0, n: 0 }
    }

    /// Consome uma amostra (em -1.0..1.0); devolve uma de saída quando completa um período.
    pub fn push(&mut self, x: f32) -> Option<i16> {
        self.acc += x as f64;
        self.n += 1;
        // Aritmética inteira: acumular 1/6 em ponto flutuante perderia uma saída a cada ~800.
        self.phase += self.to;
        if self.phase < self.from {
            return None;
        }
        self.phase -= self.from;
        let v = self.acc / self.n as f64;
        (self.acc, self.n) = (0.0, 0);
        Some((v.clamp(-1.0, 1.0) * i16::MAX as f64) as i16)
    }
}

/// Aumenta a taxa por interpolação linear.
pub struct Upsampler {
    step: f64,
    t: f64,
    a: f32,
    b: f32,
}

impl Upsampler {
    pub fn new(from: u32, to: u32) -> Self {
        // t começa em 1.0: a primeira saída já consome uma amostra da fila.
        Self { step: from as f64 / to as f64, t: 1.0, a: 0.0, b: 0.0 }
    }

    /// Próxima amostra de saída, ou `None` se a fila secou (o chamador toca silêncio).
    pub fn pull(&mut self, q: &mut VecDeque<i16>) -> Option<f32> {
        while self.t >= 1.0 {
            self.a = self.b;
            self.b = q.pop_front()? as f32 / i16::MAX as f32;
            self.t -= 1.0;
        }
        let v = self.a + (self.b - self.a) * self.t as f32;
        self.t += self.step;
        Some(v)
    }
}

type Queue = Arc<Mutex<VecDeque<i16>>>;

/// Microfone + alto-falante padrão do sistema, abertos enquanto o valor existir.
pub struct Audio {
    capture: Queue,
    playback: Queue,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    info: String,
    frame: usize,
    rate: u32,
    gain: Mutex<Agc>,
    played: Arc<AtomicUsize>,
}

impl Audio {
    /// Abre os dispositivos padrão de entrada e saída. Erra se faltar algum ou se o formato não for suportado.
    pub fn open() -> Result<Self, AudioError> {
        Self::open_rate(RATE)
    }

    /// Como [`Audio::open`], mas entregando e recebendo PCM na taxa `rate` (quadros de 20 ms).
    pub fn open_rate(rate: u32) -> Result<Self, AudioError> {
        let capture = Queue::default();
        let playback = Queue::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String, AudioError>>();
        let played = Arc::new(AtomicUsize::new(0));
        let (c, p, s, n) = (capture.clone(), playback.clone(), stop.clone(), played.clone());
        let thread = std::thread::spawn(move || match open_streams(c, p, n, rate) {
            Ok((streams, info)) => {
                let _ = ready_tx.send(Ok(info));
                while !s.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                drop(streams);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        });
        match ready_rx.recv() {
            Ok(Ok(info)) => Ok(Self { capture, playback, stop, thread: Some(thread), info, played, frame: (rate / 50) as usize, rate, gain: Mutex::new(Agc::new()) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(AudioError("thread de áudio caiu ao abrir".into())),
        }
    }

    /// Próximo quadro de 160 amostras capturado, se já houver. Se o consumidor atrasar, o excesso antigo é descartado.
    pub fn mic_frame(&self) -> Option<Vec<i16>> {
        let mut q = self.capture.lock().unwrap();
        if q.len() < self.frame {
            return None;
        }
        let mut pcm: Vec<i16> = q.drain(..self.frame).collect();
        drop(q);
        self.gain.lock().unwrap().process(&mut pcm);
        Some(pcm)
    }

    /// Dispositivos e formatos em uso, para diagnóstico.
    pub fn info(&self) -> &str {
        &self.info
    }

    /// Amostras de voz (8 kHz) já entregues ao alto-falante.
    pub fn played(&self) -> usize {
        self.played.load(Ordering::Relaxed)
    }

    /// Enfileira PCM recebido para o alto-falante.
    pub fn play(&self, pcm: &[i16]) {
        push_capped(&mut self.playback.lock().unwrap(), pcm.iter().copied(), max_queued(self.rate));
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Alvo de nível da voz (RMS) e ganho máximo do controle automático de ganho do microfone.
const AGC_TARGET: f32 = 3000.0;
const AGC_MAX_GAIN: f32 = 30.0;
/// Quadros (20 ms) que a porta de ruído fica aberta depois da última voz.
const AGC_HANG: u32 = 12;
/// Atenuação do que não é voz (fundo, chiado, teclado ao longe).
const AGC_FLOOR_ATTEN: f32 = 0.03;

/// Controle automático de ganho com porta de ruído. Estima o piso de ruído (desce rápido, sobe devagar);
/// só é voz o que passa bem do piso. Voz: o ganho leva o nível perto de [`AGC_TARGET`] (sem amplificar
/// o ruído além de ~600 de RMS). Sem voz: a porta fecha e o quadro é quase silenciado.
struct Agc {
    gain: f32,
    floor: f32,
    open: f32,
    hang: u32,
}

impl Agc {
    fn new() -> Self {
        Self { gain: 1.0, floor: 60.0, open: 0.0, hang: 0 }
    }

    fn process(&mut self, pcm: &mut [i16]) {
        let rms = (pcm.iter().map(|&x| (x as f32).powi(2)).sum::<f32>() / pcm.len().max(1) as f32).sqrt();
        let k = if rms < self.floor { 0.3 } else { 0.003 };
        self.floor += (rms - self.floor) * k;
        let voice = rms > (self.floor * 3.0).max(80.0);
        if voice {
            self.hang = AGC_HANG;
            let cap = AGC_MAX_GAIN.min(600.0 / self.floor.max(1.0)).max(1.0);
            let want = (AGC_TARGET / rms).clamp(1.0, cap);
            let k = if want < self.gain { 0.5 } else { 0.1 };
            self.gain += (want - self.gain) * k;
        } else if self.hang > 0 {
            self.hang -= 1;
        } else {
            self.gain += (1.0 - self.gain) * 0.1;
        }
        let target = if self.hang > 0 { 1.0 } else { AGC_FLOOR_ATTEN };
        let k = if target > self.open { 0.7 } else { 0.25 };
        self.open += (target - self.open) * k;
        let g = self.gain * self.open;
        for x in pcm {
            *x = (*x as f32 * g).clamp(-32768.0, 32767.0) as i16;
        }
    }
}

fn push_capped(q: &mut VecDeque<i16>, it: impl Iterator<Item = i16>, max: usize) {
    q.extend(it);
    if q.len() > max {
        let extra = q.len() - max;
        q.drain(..extra);
    }
}

fn open_streams(capture: Queue, playback: Queue, played: Arc<AtomicUsize>, rate: u32) -> Result<((Stream, Stream), String), AudioError> {
    let host = cpal::default_host();
    let input = host.default_input_device().ok_or_else(|| AudioError("sem microfone padrão".into()))?;
    let output = host.default_output_device().ok_or_else(|| AudioError("sem alto-falante padrão".into()))?;
    let (inp, outp) = (input.default_input_config().map_err(err)?, output.default_output_config().map_err(err)?);

    let info = format!(
        "entrada \"{}\" {:?} {} Hz {} canais; saída \"{}\" {:?} {} Hz {} canais",
        input.description().map(|d| d.to_string()).unwrap_or_default(), inp.sample_format(), inp.sample_rate(), inp.channels(),
        output.description().map(|d| d.to_string()).unwrap_or_default(), outp.sample_format(), outp.sample_rate(), outp.channels(),
    );
    let in_stream = match inp.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&input, inp.into(), capture, rate)?,
        SampleFormat::I16 => build_input::<i16>(&input, inp.into(), capture, rate)?,
        SampleFormat::U16 => build_input::<u16>(&input, inp.into(), capture, rate)?,
        SampleFormat::I32 => build_input::<i32>(&input, inp.into(), capture, rate)?,
        f => return Err(AudioError(format!("formato de entrada não suportado: {f}"))),
    };
    let out_stream = match outp.sample_format() {
        SampleFormat::F32 => build_output::<f32>(&output, outp.into(), playback, played, rate)?,
        SampleFormat::I16 => build_output::<i16>(&output, outp.into(), playback, played, rate)?,
        SampleFormat::U16 => build_output::<u16>(&output, outp.into(), playback, played, rate)?,
        SampleFormat::I32 => build_output::<i32>(&output, outp.into(), playback, played, rate)?,
        f => return Err(AudioError(format!("formato de saída não suportado: {f}"))),
    };
    in_stream.play().map_err(err)?;
    out_stream.play().map_err(err)?;
    Ok(((in_stream, out_stream), info))
}

fn build_input<T>(dev: &cpal::Device, cfg: cpal::StreamConfig, q: Queue, rate: u32) -> Result<Stream, AudioError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = cfg.channels as usize;
    let mut down = Downsampler::new(cfg.sample_rate, rate);
    dev.build_input_stream(
        cfg,
        move |data: &[T], _| {
            let mut out = Vec::with_capacity(data.len() / ch / 6 + 1);
            for frame in data.chunks(ch) {
                let mono = frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / ch as f32;
                out.extend(down.push(mono));
            }
            push_capped(&mut q.lock().unwrap(), out.into_iter(), max_queued(rate));
        },
        |e| eprintln!("microfone: {e}"),
        None,
    )
    .map_err(err)
}

fn build_output<T>(dev: &cpal::Device, cfg: cpal::StreamConfig, q: Queue, played: Arc<AtomicUsize>, rate: u32) -> Result<Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels as usize;
    let mut up = Upsampler::new(rate, cfg.sample_rate);
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            let mut q = q.lock().unwrap();
            for frame in data.chunks_mut(ch) {
                let before = q.len();
                let v = T::from_sample(up.pull(&mut q).unwrap_or(0.0));
                if q.len() < before {
                    played.fetch_add(1, Ordering::Relaxed);
                }
                frame.fill(v);
            }
        },
        |e| eprintln!("alto-falante: {e}"),
        None,
    )
    .map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_48k_yields_one_in_six() {
        let mut d = Downsampler::new(48000, 8000);
        let n = (0..4800).filter_map(|_| d.push(0.5)).count();
        assert_eq!(n, 800);
    }

    #[test]
    fn downsample_keeps_level() {
        let mut d = Downsampler::new(44100, 8000);
        let out: Vec<i16> = (0..44100).filter_map(|_| d.push(0.5)).collect();
        assert!((out.len() as i32 - 8000).abs() <= 1);
        assert!(out.iter().all(|&x| (x as i32 - 16383).abs() < 3));
    }

    #[test]
    fn upsample_interpolates_and_runs_dry() {
        let mut u = Upsampler::new(8000, 48000);
        let mut q: VecDeque<i16> = (0..100).map(|_| i16::MAX / 2).collect();
        let n = std::iter::from_fn(|| u.pull(&mut q)).count();
        // 100 amostras a 6x, menos o preenchimento do primeiro par.
        assert!((595..=605).contains(&n), "{n}");
        assert!(u.pull(&mut q).is_none());
    }

    #[test]
    fn roundtrip_preserves_tone_energy() {
        let tone: Vec<i16> = (0..8000).map(|i| (8000.0 * (std::f64::consts::TAU * 440.0 * i as f64 / 8000.0).sin()) as i16).collect();
        let mut up = Upsampler::new(8000, 48000);
        let mut q: VecDeque<i16> = tone.iter().copied().collect();
        let mut down = Downsampler::new(48000, 8000);
        let back: Vec<i16> = std::iter::from_fn(|| up.pull(&mut q)).filter_map(|x| down.push(x)).collect();
        let rms = |v: &[i16]| (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        let ratio = rms(&back[100..back.len() - 100]) / rms(&tone);
        assert!((0.85..1.05).contains(&ratio), "{ratio}");
    }

    fn rms(v: &[i16]) -> f32 {
        (v.iter().map(|&x| (x as f32).powi(2)).sum::<f32>() / v.len() as f32).sqrt()
    }

    #[test]
    fn agc_lifts_quiet_voice_and_gates_noise() {
        let mut agc = Agc::new();
        let noise = |n: usize| -> Vec<i16> { (0..160).map(|i| (((n * 160 + i) * 7919 % 61) as i16 - 30)).collect() };
        // 2 s de ruído (RMS ~17): o piso é aprendido e a saída fica quase muda.
        let mut out = Vec::new();
        for n in 0..100 {
            out = noise(n);
            agc.process(&mut out);
        }
        assert!(rms(&out) < 3.0, "{}", rms(&out));
        // Voz fraca (RMS ~300): sobe para perto do alvo.
        let voice: Vec<i16> = (0..160).map(|i| (420.0 * (i as f32 * 0.3).sin()) as i16).collect();
        for _ in 0..60 {
            out = voice.clone();
            agc.process(&mut out);
        }
        assert!(rms(&out) > 1500.0, "{}", rms(&out));
        // Volta o ruído: a porta fecha.
        for n in 0..60 {
            out = noise(n);
            agc.process(&mut out);
        }
        assert!(rms(&out) < 3.0, "{}", rms(&out));
    }

    #[test]
    fn queue_is_capped() {
        let mut q = VecDeque::new();
        push_capped(&mut q, std::iter::repeat_n(1i16, max_queued(RATE) + 1000), max_queued(RATE));
        assert_eq!(q.len(), max_queued(RATE));
    }

    /// Precisa de placa de som: `cargo test --features audio -- --ignored`.
    #[test]
    #[ignore]
    fn opens_default_devices_and_captures() {
        let a = Audio::open().expect("dispositivos padrão");
        std::thread::sleep(Duration::from_millis(500));
        assert!(a.mic_frame().is_some(), "microfone não entregou quadro em 500 ms");
        a.play(&[0; FRAME]);
    }
}
