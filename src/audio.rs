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
        atomic::{AtomicBool, Ordering},
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
const MAX_QUEUED: usize = RATE as usize * 4 / 10;

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
}

impl Audio {
    /// Abre os dispositivos padrão de entrada e saída. Erra se faltar algum ou se o formato não for suportado.
    pub fn open() -> Result<Self, AudioError> {
        let capture = Queue::default();
        let playback = Queue::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (c, p, s) = (capture.clone(), playback.clone(), stop.clone());
        let thread = std::thread::spawn(move || match open_streams(c, p) {
            Ok(streams) => {
                let _ = ready_tx.send(Ok(()));
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
            Ok(Ok(())) => Ok(Self { capture, playback, stop, thread: Some(thread) }),
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
        if q.len() < FRAME {
            return None;
        }
        Some(q.drain(..FRAME).collect())
    }

    /// Enfileira PCM recebido para o alto-falante.
    pub fn play(&self, pcm: &[i16]) {
        push_capped(&mut self.playback.lock().unwrap(), pcm.iter().copied());
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

fn push_capped(q: &mut VecDeque<i16>, it: impl Iterator<Item = i16>) {
    q.extend(it);
    if q.len() > MAX_QUEUED {
        let extra = q.len() - MAX_QUEUED;
        q.drain(..extra);
    }
}

fn open_streams(capture: Queue, playback: Queue) -> Result<(Stream, Stream), AudioError> {
    let host = cpal::default_host();
    let input = host.default_input_device().ok_or_else(|| AudioError("sem microfone padrão".into()))?;
    let output = host.default_output_device().ok_or_else(|| AudioError("sem alto-falante padrão".into()))?;
    let (inp, outp) = (input.default_input_config().map_err(err)?, output.default_output_config().map_err(err)?);

    let in_stream = match inp.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&input, inp.into(), capture)?,
        SampleFormat::I16 => build_input::<i16>(&input, inp.into(), capture)?,
        SampleFormat::U16 => build_input::<u16>(&input, inp.into(), capture)?,
        SampleFormat::I32 => build_input::<i32>(&input, inp.into(), capture)?,
        f => return Err(AudioError(format!("formato de entrada não suportado: {f}"))),
    };
    let out_stream = match outp.sample_format() {
        SampleFormat::F32 => build_output::<f32>(&output, outp.into(), playback)?,
        SampleFormat::I16 => build_output::<i16>(&output, outp.into(), playback)?,
        SampleFormat::U16 => build_output::<u16>(&output, outp.into(), playback)?,
        SampleFormat::I32 => build_output::<i32>(&output, outp.into(), playback)?,
        f => return Err(AudioError(format!("formato de saída não suportado: {f}"))),
    };
    in_stream.play().map_err(err)?;
    out_stream.play().map_err(err)?;
    Ok((in_stream, out_stream))
}

fn build_input<T>(dev: &cpal::Device, cfg: cpal::StreamConfig, q: Queue) -> Result<Stream, AudioError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = cfg.channels as usize;
    let mut down = Downsampler::new(cfg.sample_rate, RATE);
    dev.build_input_stream(
        cfg,
        move |data: &[T], _| {
            let mut out = Vec::with_capacity(data.len() / ch / 6 + 1);
            for frame in data.chunks(ch) {
                let mono = frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / ch as f32;
                out.extend(down.push(mono));
            }
            push_capped(&mut q.lock().unwrap(), out.into_iter());
        },
        |e| eprintln!("microfone: {e}"),
        None,
    )
    .map_err(err)
}

fn build_output<T>(dev: &cpal::Device, cfg: cpal::StreamConfig, q: Queue) -> Result<Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels as usize;
    let mut up = Upsampler::new(RATE, cfg.sample_rate);
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            let mut q = q.lock().unwrap();
            for frame in data.chunks_mut(ch) {
                let v = T::from_sample(up.pull(&mut q).unwrap_or(0.0));
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

    #[test]
    fn queue_is_capped() {
        let mut q = VecDeque::new();
        push_capped(&mut q, std::iter::repeat_n(1i16, MAX_QUEUED + 1000));
        assert_eq!(q.len(), MAX_QUEUED);
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
