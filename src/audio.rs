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

use sonora::{AudioProcessing, Config, StreamConfig};

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

/// Meia-largura do filtro em cruzamentos por zero do sinc: mais é mais nítido, mas atrasa e custa mais.
const SINC_LOBES: f64 = 8.0;
/// Corte do filtro como fração da menor taxa (0,5 seria Nyquist): deixa folga para a banda de transição.
const SINC_CUTOFF: f64 = 0.45;

/// Interpolador de sinc com janela de Blackman, avaliado na posição fracionária exata de cada saída
/// (serve a qualquer razão de taxas, inclusive 44,1 kHz → 16 kHz). Atrasa a saída em `m` amostras de entrada.
struct Sinc {
    /// Corte em ciclos por amostra de entrada.
    fc: f64,
    /// Meia-largura em amostras de entrada.
    m: i64,
    /// Amostras de entrada por amostra de saída.
    step: f64,
    /// Fator sobre `step` (1.0 = taxa nominal), para absorver a diferença entre relógios.
    scale: f64,
    /// Posição (índice de entrada, fracionário) da próxima saída.
    t: f64,
    /// Índice de entrada de `hist[0]`.
    base: i64,
    hist: VecDeque<f32>,
}

impl Sinc {
    fn new(from: u32, to: u32) -> Self {
        let low = from.min(to) as f64 / from as f64;
        Self { fc: SINC_CUTOFF * low, m: (SINC_LOBES / low).ceil() as i64, step: from as f64 / to as f64, scale: 1.0, t: 0.0, base: 0, hist: VecDeque::new() }
    }

    /// Há entrada suficiente para calcular a próxima saída?
    fn ready(&self) -> bool {
        self.base + self.hist.len() as i64 > self.t.floor() as i64 + self.m
    }

    fn push(&mut self, x: f32) {
        self.hist.push_back(x);
    }

    /// Calcula a próxima saída (exige [`ready`](Self::ready)) e avança.
    fn next(&mut self) -> f32 {
        let (mut acc, mut norm) = (0.0f64, 0.0f64);
        let m = self.m as f64;
        for i in (self.t - m).ceil() as i64..=(self.t + m).floor() as i64 {
            let d = i as f64 - self.t;
            let a = std::f64::consts::PI * 2.0 * self.fc * d;
            let sinc = if a.abs() < 1e-9 { 1.0 } else { a.sin() / a };
            let w = 0.42 + 0.5 * (std::f64::consts::PI * d / m).cos() + 0.08 * (2.0 * std::f64::consts::PI * d / m).cos();
            let h = sinc * w;
            norm += h;
            // Antes do primeiro quadro, silêncio.
            if let Some(x) = i.checked_sub(self.base).filter(|k| *k >= 0).and_then(|k| self.hist.get(k as usize)) {
                acc += h * *x as f64;
            }
        }
        self.t += self.step * self.scale;
        let keep_from = (self.t - m).ceil() as i64;
        while self.base < keep_from && !self.hist.is_empty() {
            self.hist.pop_front();
            self.base += 1;
        }
        (acc / norm) as f32
    }
}

/// Reduz a taxa com filtro passa-baixas de sinc (polifásico por posição fracionária), sem aliasing audível. Mono, `i16`.
pub struct Downsampler(Sinc);

impl Downsampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self(Sinc::new(from, to))
    }

    /// Consome uma amostra (em -1.0..1.0); devolve uma de saída quando completa um período.
    pub fn push(&mut self, x: f32) -> Option<i16> {
        self.0.push(x);
        self.0.ready().then(|| (self.0.next().clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
    }
}

/// Aumenta a taxa por interpolação de sinc (mesmo filtro do [`Downsampler`]).
pub struct Upsampler(Sinc);

impl Upsampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self(Sinc::new(from, to))
    }

    /// Consome a entrada `scale` vezes mais rápido (1.0 = nominal). Perto de 1.0 não se ouve; serve para
    /// acompanhar um relógio de rede ligeiramente diferente do da placa de som.
    pub fn set_scale(&mut self, scale: f64) {
        self.0.scale = scale;
    }

    /// Próxima amostra de saída, ou `None` se a fila secou (o chamador toca silêncio).
    pub fn pull(&mut self, q: &mut VecDeque<i16>) -> Option<f32> {
        while !self.0.ready() {
            self.0.push(q.pop_front()? as f32 / i16::MAX as f32);
        }
        Some(self.0.next())
    }
}

type Queue = Arc<Mutex<VecDeque<i16>>>;

/// Maior desvio de velocidade aplicado à reprodução (0,5%: inaudível).
const MAX_DRIFT: f64 = 0.005;
/// Nível desejado da fila de reprodução, em quadros de 20 ms.
const TARGET_FRAMES: f64 = 2.0;

/// Mantém a fila de reprodução perto do alvo ajustando de leve a velocidade de consumo. Sem isso, o relógio
/// da placa de som e o da rede (nunca iguais) esvaziam a fila, que então toca silêncio, ou a enchem até o
/// teto, que então descarta o mais antigo de uma vez: ambos estalam.
struct Drift {
    /// Média móvel do tamanho da fila, em amostras.
    level: f64,
    target: f64,
}

impl Drift {
    fn new(rate: u32) -> Self {
        let target = rate as f64 / 50.0 * TARGET_FRAMES;
        Self { level: target, target }
    }

    /// Fator de velocidade para o nível atual da fila (`len` amostras). Fila acima do alvo → consome mais rápido.
    fn scale(&mut self, len: usize) -> f64 {
        self.level += (len as f64 - self.level) * 0.02;
        1.0 + ((self.level - self.target) / self.target * MAX_DRIFT).clamp(-MAX_DRIFT, MAX_DRIFT)
    }
}

/// Microfone + alto-falante padrão do sistema, abertos enquanto o valor existir.
/// Último erro dos fluxos de áudio (dispositivo removido, driver caiu), guardado pelos callbacks.
type Failure = Arc<Mutex<Option<String>>>;

pub struct Audio {
    capture: Queue,
    playback: Queue,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    info: String,
    frame: usize,
    rate: u32,
    apm: Mutex<(AudioProcessing, Processing)>,
    tap: Queue,
    played: Arc<AtomicUsize>,
    failure: Failure,
}

impl Audio {
    /// Abre os dispositivos padrão de entrada e saída. Erra se faltar algum ou se o formato não for suportado.
    pub fn open() -> Result<Self, AudioError> {
        Self::open_rate(RATE)
    }

    /// Como [`Audio::open`], mas entregando e recebendo PCM na taxa `rate` (quadros de 20 ms).
    pub fn open_rate(rate: u32) -> Result<Self, AudioError> {
        Self::open_with(rate, &Devices::default())
    }

    /// Como [`Audio::open_rate`], escolhendo os dispositivos por nome (ver [`devices`]). Nome vazio = padrão;
    /// um nome que não existe mais (fone desconectado) erra em vez de cair no padrão sem avisar.
    pub fn open_with(rate: u32, devices: &Devices) -> Result<Self, AudioError> {
        let capture = Queue::default();
        let playback = Queue::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String, AudioError>>();
        let played = Arc::new(AtomicUsize::new(0));
        let tap = Queue::default();
        let failure = Failure::default();
        let (c, p, s, n, tap2, f2) = (capture.clone(), playback.clone(), stop.clone(), played.clone(), tap.clone(), failure.clone());
        let devices = devices.clone();
        let thread = std::thread::spawn(move || match open_streams(c, p, n, rate, tap2, &devices, f2) {
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
            Ok(Ok(info)) => Ok(Self { capture, playback, stop, thread: Some(thread), info, played, frame: (rate / 50) as usize, rate, apm: Mutex::new((voice_processor(rate, Processing::default()), Processing::default())), tap, failure }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(AudioError("thread de áudio caiu ao abrir".into())),
        }
    }

    /// Erro que um fluxo de áudio sofreu desde a última chamada (por exemplo, o fone foi desconectado).
    pub fn take_error(&self) -> Option<String> {
        self.failure.lock().unwrap().take()
    }

    /// Próximo quadro de 160 amostras capturado, se já houver. Se o consumidor atrasar, o excesso antigo é descartado.
    pub fn mic_frame(&self) -> Option<Vec<i16>> {
        let mut q = self.capture.lock().unwrap();
        if q.len() < self.frame {
            return None;
        }
        let mut pcm: Vec<i16> = q.drain(..self.frame).collect();
        drop(q);
        let mut guard = self.apm.lock().unwrap();
        clean_capture(&mut guard.0, &mut pcm, &mut self.tap.lock().unwrap(), self.frame / 2);
        Some(pcm)
    }

    /// Liga ou desliga estágios do tratamento do microfone. Só refaz o processador (e o cancelador de eco
    /// recomeça a aprender) quando algo mudou.
    pub fn set_processing(&self, on: Processing) {
        let mut guard = self.apm.lock().unwrap();
        if guard.1 != on {
            *guard = (voice_processor(self.rate, on), on);
        }
    }

    /// O que o cancelador de eco mede agora: quantos dB ele tira do eco (só tem valor depois que o
    /// alto-falante tocou algo) e o atraso que achou entre tocar e o microfone ouvir.
    pub fn echo_stats(&self) -> EchoStats {
        let guard = self.apm.lock().unwrap();
        let st = guard.0.statistics();
        EchoStats { erle_db: st.echo_return_loss_enhancement, delay_ms: st.delay_ms }
    }

    /// Tratamento do microfone em uso.
    pub fn processing(&self) -> Processing {
        self.apm.lock().unwrap().1
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

/// Quais estágios do tratamento WebRTC do microfone ficam ligados. Quem já faz um deles por conta própria
/// (a GUI tem filtro de graves, ganho automático e remoção de ruído próprios) desliga o daqui para não
/// tratar duas vezes: um ganho automático antes de outro reforça o ruído, e uma supressão antes de outra
/// distorce a voz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Processing {
    pub high_pass: bool,
    pub echo_cancel: bool,
    pub noise_suppression: bool,
    pub auto_gain: bool,
}

/// Medidas do cancelador de eco, para diagnóstico.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EchoStats {
    pub erle_db: Option<f64>,
    pub delay_ms: Option<i32>,
}

impl Default for Processing {
    fn default() -> Self {
        Processing { high_pass: true, echo_cancel: true, noise_suppression: true, auto_gain: true }
    }
}

/// Tratamento da voz do microfone (WebRTC, em Rust puro): filtro passa-altas, cancelamento de eco (usa o
/// que foi tocado como referência), supressão de ruído e ganho automático, conforme `on`.
fn voice_processor(rate: u32, on: Processing) -> AudioProcessing {
    use sonora::config::{AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, NoiseSuppression, NoiseSuppressionLevel};
    let config = Config {
        high_pass_filter: on.high_pass.then(HighPassFilter::default),
        echo_canceller: on.echo_cancel.then(EchoCanceller::default),
        noise_suppression: on.noise_suppression.then(|| NoiseSuppression { level: NoiseSuppressionLevel::High, ..Default::default() }),
        gain_controller2: on.auto_gain.then(|| GainController2 { adaptive_digital: Some(AdaptiveDigital::default()), ..Default::default() }),
        ..Default::default()
    };
    AudioProcessing::builder()
        .config(config)
        .capture_config(StreamConfig::new(rate, 1))
        .render_config(StreamConfig::new(rate, 1))
        .build()
}

/// Passa `pcm` (microfone) pelo tratamento, dando ao cancelador de eco o que o alto-falante tocou como
/// referência. `tap` é a fila do que o alto-falante já consumiu e `chunk` o tamanho do passo do tratamento
/// (10 ms).
///
/// A referência entra só em pedaços inteiros, quando existem, e nunca é preenchida com zeros: o alto-falante
/// e o microfone entregam em rajadas diferentes, e zeros no meio da referência a deslocariam no tempo em
/// relação ao eco, que o cancelador teria de reaprender a cada vez.
fn clean_capture(apm: &mut AudioProcessing, pcm: &mut [i16], tap: &mut VecDeque<i16>, chunk: usize) {
    // Atrasou demais (alto-falante parado e voltando): fica só com os dois pedaços mais novos.
    if tap.len() > chunk * 4 {
        let extra = (tap.len() / chunk - 2) * chunk;
        tap.drain(..extra);
    }
    let (mut render, mut sink) = (vec![0i16; chunk], vec![0i16; chunk]);
    for c in pcm.chunks_mut(chunk) {
        if tap.len() >= chunk {
            for (r, t) in render.iter_mut().zip(tap.drain(..chunk)) {
                *r = t;
            }
            let _ = apm.process_render_i16(&render, &mut sink);
        }
        let src = c.to_vec();
        let _ = apm.process_capture_i16(&src, c);
    }
}

fn push_capped(q: &mut VecDeque<i16>, it: impl Iterator<Item = i16>, max: usize) {
    q.extend(it);
    if q.len() > max {
        let extra = q.len() - max;
        q.drain(..extra);
    }
}

/// Dispositivos de áudio a abrir, pelo nome que [`devices`] lista. Vazio = o padrão do sistema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    pub input: String,
    pub output: String,
}

fn name_of(d: &cpal::Device) -> String {
    clean_name(&d.description().map(|d| d.to_string()).unwrap_or_default())
}

/// O ALSA descreve placas sem nome de porta como `"placa, "`; tira a vírgula e o espaço sobrando, para
/// o nome da lista e o salvo na configuração serem os mesmos.
fn clean_name(n: &str) -> String {
    n.trim().trim_end_matches(',').trim_end().to_string()
}

/// Dispositivo que não é microfone nem alto-falante de verdade (o "null" do ALSA, que descarta o som).
fn is_pseudo(name: &str) -> bool {
    name.starts_with("Discard all samples")
}

/// Nomes dos microfones e dos alto-falantes disponíveis agora: `(entradas, saídas)`.
pub fn devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let list = |it: Result<Vec<cpal::Device>, _>| -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for n in it.map(|d| d.iter().map(name_of).collect::<Vec<_>>()).unwrap_or_default() {
            // O ALSA repete a mesma placa em várias entradas (hw, plughw, surround…): uma só basta.
            if !n.is_empty() && !is_pseudo(&n) && !v.contains(&n) {
                v.push(n);
            }
        }
        v
    };
    (list(host.input_devices().map(Iterator::collect)), list(host.output_devices().map(Iterator::collect)))
}

fn open_streams(capture: Queue, playback: Queue, played: Arc<AtomicUsize>, rate: u32, tap: Queue, want: &Devices, fail: Failure) -> Result<((Stream, Stream), String), AudioError> {
    let host = cpal::default_host();
    let input = if want.input.is_empty() {
        host.default_input_device().ok_or_else(|| AudioError("sem microfone padrão".into()))?
    } else {
        host.input_devices().map_err(err)?.find(|d| name_of(d) == clean_name(&want.input)).ok_or_else(|| AudioError(format!("microfone \"{}\" não encontrado", want.input)))?
    };
    let output = if want.output.is_empty() {
        host.default_output_device().ok_or_else(|| AudioError("sem alto-falante padrão".into()))?
    } else {
        host.output_devices().map_err(err)?.find(|d| name_of(d) == clean_name(&want.output)).ok_or_else(|| AudioError(format!("alto-falante \"{}\" não encontrado", want.output)))?
    };
    let (inp, outp) = (input.default_input_config().map_err(err)?, output.default_output_config().map_err(err)?);

    let info = format!(
        "entrada \"{}\" {:?} {} Hz {} canais; saída \"{}\" {:?} {} Hz {} canais",
        input.description().map(|d| d.to_string()).unwrap_or_default(), inp.sample_format(), inp.sample_rate(), inp.channels(),
        output.description().map(|d| d.to_string()).unwrap_or_default(), outp.sample_format(), outp.sample_rate(), outp.channels(),
    );
    let in_stream = match inp.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&input, inp.into(), capture, rate, fail.clone())?,
        SampleFormat::I16 => build_input::<i16>(&input, inp.into(), capture, rate, fail.clone())?,
        SampleFormat::U16 => build_input::<u16>(&input, inp.into(), capture, rate, fail.clone())?,
        SampleFormat::I32 => build_input::<i32>(&input, inp.into(), capture, rate, fail.clone())?,
        f => return Err(AudioError(format!("formato de entrada não suportado: {f}"))),
    };
    let out_stream = match outp.sample_format() {
        SampleFormat::F32 => build_output::<f32>(&output, outp.into(), playback, played, rate, tap.clone(), fail.clone())?,
        SampleFormat::I16 => build_output::<i16>(&output, outp.into(), playback, played, rate, tap.clone(), fail.clone())?,
        SampleFormat::U16 => build_output::<u16>(&output, outp.into(), playback, played, rate, tap.clone(), fail.clone())?,
        SampleFormat::I32 => build_output::<i32>(&output, outp.into(), playback, played, rate, tap.clone(), fail.clone())?,
        f => return Err(AudioError(format!("formato de saída não suportado: {f}"))),
    };
    in_stream.play().map_err(err)?;
    out_stream.play().map_err(err)?;
    Ok(((in_stream, out_stream), info))
}

fn build_input<T>(dev: &cpal::Device, cfg: cpal::StreamConfig, q: Queue, rate: u32, fail: Failure) -> Result<Stream, AudioError>
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
        move |e| *fail.lock().unwrap() = Some(format!("microfone: {e}")),
        None,
    )
    .map_err(err)
}

fn build_output<T>(dev: &cpal::Device, cfg: cpal::StreamConfig, q: Queue, played: Arc<AtomicUsize>, rate: u32, tap: Queue, fail: Failure) -> Result<Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels as usize;
    let mut up = Upsampler::new(rate, cfg.sample_rate);
    let mut drift = Drift::new(rate);
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            let mut q = q.lock().unwrap();
            up.set_scale(drift.scale(q.len()));
            for frame in data.chunks_mut(ch) {
                let (before, front) = (q.len(), q.front().copied());
                let v = T::from_sample(up.pull(&mut q).unwrap_or(0.0));
                if q.len() < before {
                    played.fetch_add(1, Ordering::Relaxed);
                    if let Some(f) = front {
                        push_capped(&mut tap.lock().unwrap(), std::iter::once(f), max_queued(rate));
                    }
                }
                frame.fill(v);
            }
        },
        move |e| *fail.lock().unwrap() = Some(format!("alto-falante: {e}")),
        None,
    )
    .map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_lose_alsa_trailing_separators_and_pseudo_devices_are_known() {
        assert_eq!(clean_name("sof-hda-dsp, "), "sof-hda-dsp");
        assert_eq!(clean_name("sof-hda-dsp, HDMI 1"), "sof-hda-dsp, HDMI 1");
        assert_eq!(clean_name("  PipeWire Sound Server "), "PipeWire Sound Server");
        assert!(is_pseudo("Discard all samples (playback) or generate zero samples (capture)"));
        assert!(!is_pseudo("PipeWire Sound Server"));
    }

    #[test]
    fn downsample_48k_yields_one_in_six() {
        let mut d = Downsampler::new(48000, 8000);
        let n = (0..4800).filter_map(|_| d.push(0.5)).count();
        // Uma a cada seis, menos as que esperam a meia-largura do filtro.
        assert!((790..=800).contains(&n), "{n}");
    }

    #[test]
    fn downsample_keeps_level() {
        let mut d = Downsampler::new(44100, 8000);
        let out: Vec<i16> = (0..44100).filter_map(|_| d.push(0.5)).collect();
        assert!((7985..=8000).contains(&out.len()), "{}", out.len());
        assert!(out.iter().skip(20).all(|&x| (x as i32 - 16383).abs() < 40), "{:?}", &out[20..30]);
    }

    #[test]
    fn downsample_rejects_aliasing_and_keeps_passband() {
        let rms = |v: &[i16]| (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        let run = |hz: f64, from: u32| -> f64 {
            let mut d = Downsampler::new(from, 8000);
            let out: Vec<i16> = (0..from as usize).filter_map(|i| d.push((0.5 * (std::f64::consts::TAU * hz * i as f64 / from as f64).sin()) as f32)).collect();
            rms(&out[50..]) / (0.5 * i16::MAX as f64 / 2f64.sqrt())
        };
        for from in [48000, 44100] {
            // 7 kHz dobraria para 1 kHz sem filtro; 1 kHz passa quase inteiro.
            assert!(run(7000.0, from) < 0.05, "{from}: {}", run(7000.0, from));
            assert!((0.95..1.05).contains(&run(1000.0, from)), "{from}: {}", run(1000.0, from));
        }
    }

    #[test]
    fn drift_speeds_up_a_full_queue_and_slows_a_starved_one() {
        let mut d = Drift::new(16000);
        for _ in 0..500 {
            d.scale(3200);
        }
        assert!(d.scale(3200) > 1.0);
        let mut d = Drift::new(16000);
        for _ in 0..500 {
            d.scale(0);
        }
        let s = d.scale(0);
        assert!((1.0 - MAX_DRIFT..1.0).contains(&s), "{s}");
    }

    /// Rede 0,4% mais rápida ou mais lenta que a placa de som (bem mais que a deriva real), por 20 s: a fila
    /// não chega ao teto de 400 ms nem seca.
    #[test]
    fn drift_keeps_the_queue_bounded_when_clocks_differ() {
        for skew in [1.004, 0.996] {
            let (rate, out_rate) = (16000u32, 48000u32);
            let mut up = Upsampler::new(rate, out_rate);
            let mut drift = Drift::new(rate);
            let mut q: VecDeque<i16> = VecDeque::new();
            let (mut produced, mut dry, mut peak) = (0.0f64, 0usize, 0usize);
            for ms in 0..20_000u32 {
                // Chega de 20 em 20 ms, em rajada, no ritmo da rede.
                if ms % 20 == 0 {
                    produced += 320.0 * skew;
                    let n = produced as usize;
                    produced -= n as f64;
                    q.extend(std::iter::repeat_n(1000i16, n));
                }
                up.set_scale(drift.scale(q.len()));
                for _ in 0..48 {
                    if up.pull(&mut q).is_none() {
                        dry += 1;
                    }
                }
                peak = peak.max(q.len());
            }
            assert!(peak < max_queued(rate), "skew {skew}: pico {peak}");
            assert!(dry < 500, "skew {skew}: {dry} saídas sem dado");
        }
    }

    #[test]
    fn upsample_interpolates_and_runs_dry() {
        let mut u = Upsampler::new(8000, 48000);
        let mut q: VecDeque<i16> = (0..100).map(|_| i16::MAX / 2).collect();
        let n = std::iter::from_fn(|| u.pull(&mut q)).count();
        // 100 amostras a 6x, menos a meia-largura do filtro.
        assert!((540..=560).contains(&n), "{n}");
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
    fn voice_processor_suppresses_steady_noise_and_keeps_speech_level() {
        let mut apm = voice_processor(16000, Processing::default());
        let rms = |v: &[i16]| (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        let (mut seed, mut sink, mut out) = (12345u32, vec![0i16; 160], vec![0i16; 160]);
        let mut noise = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            ((seed >> 16) as i16) / 40
        };
        let (mut last_in, mut last_out) = (0.0, 0.0);
        for n in 0..300 {
            let frame: Vec<i16> = (0..160).map(|_| noise()).collect();
            let silence = [0i16; 160];
            apm.process_render_i16(&silence, &mut sink).unwrap();
            apm.process_capture_i16(&frame, &mut out).unwrap();
            if n >= 250 {
                last_in += rms(&frame);
                last_out += rms(&out);
            }
        }
        assert!(last_out < last_in, "in {last_in} out {last_out}");
    }

    #[test]
    fn processing_stages_can_be_turned_off_one_by_one() {
        let rms = |v: &[i16]| (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        let run = |on: Processing| {
            let mut apm = voice_processor(16000, on);
            let (mut seed, mut sink, mut out) = (12345u32, vec![0i16; 160], vec![0i16; 160]);
            let (mut last_in, mut last_out) = (0.0, 0.0);
            for n in 0..300 {
                let frame: Vec<i16> = (0..160)
                    .map(|_| {
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        ((seed >> 16) as i16) / 40
                    })
                    .collect();
                apm.process_render_i16(&[0i16; 160], &mut sink).unwrap();
                apm.process_capture_i16(&frame, &mut out).unwrap();
                if n >= 250 {
                    last_in += rms(&frame);
                    last_out += rms(&out);
                }
            }
            last_out / last_in
        };
        let none = Processing { high_pass: false, echo_cancel: false, noise_suppression: false, auto_gain: false };
        assert!((run(none) - 1.0).abs() < 0.01, "tudo desligado deveria deixar passar");
        assert!(run(Processing { noise_suppression: true, ..none }) < 0.95, "a supressão de ruído ligada deveria baixar o ruído");
    }

    /// Eco simulado: o que toca volta ao microfone atrasado e atenuado. Devolve quantos dB o cancelador
    /// tira do eco depois de convergir (ERLE).
    fn erle(delay_ms: usize, with_aec: bool) -> f64 {
        let on = Processing { high_pass: false, echo_cancel: with_aec, noise_suppression: false, auto_gain: false };
        let mut apm = voice_processor(16000, on);
        let rms = |v: &[i16]| (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        let delay = delay_ms * 16;
        let mut seed = 99u32;
        let mut far: Vec<i16> = Vec::new();
        let (mut sink, mut out) = (vec![0i16; 160], vec![0i16; 160]);
        let (mut e_in, mut e_out) = (0.0, 0.0);
        let mut smooth = 0.0f64;
        for n in 0..1500 {
            // Voz distante: ruído colorido com envelope lento.
            let mut render = vec![0i16; 160];
            for r in render.iter_mut() {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                smooth = 0.7 * smooth + 0.3 * (((seed >> 16) as i16) as f64);
                *r = (smooth * 0.5) as i16;
            }
            far.extend_from_slice(&render);
            // Microfone: só o eco (sala simples: atraso, ganho 0,6, um eco curto a mais).
            let at = |i: isize| if i >= 0 { far.get(i as usize).copied().unwrap_or(0) as f64 } else { 0.0 };
            let base = (n * 160) as isize;
            let capture: Vec<i16> = (0..160).map(|i| {
                let t = base + i - delay as isize;
                (0.6 * at(t) + 0.2 * at(t - 40)) as i16
            }).collect();
            apm.process_render_i16(&render, &mut sink).unwrap();
            apm.process_capture_i16(&capture, &mut out).unwrap();
            if n >= 1300 {
                e_in += rms(&capture);
                e_out += rms(&out);
            }
        }
        20.0 * (e_in / e_out.max(1e-9)).log10()
    }

    #[test]
    fn echo_canceller_removes_a_delayed_echo() {
        assert!(erle(80, false) < 1.0, "sem o cancelador não há ganho");
        let e = erle(80, true);
        assert!(e > 12.0, "ERLE de {e:.1} dB");
    }

    /// Eco com o alto-falante e o microfone entregando em rajadas irregulares, como os dispositivos reais.
    /// `padded` refaz o jeito antigo (referência completada com zeros) para comparar.
    fn erle_with_bursts(padded: bool) -> f64 {
        const CH: usize = 160;
        let mut apm = voice_processor(16000, Processing { high_pass: false, echo_cancel: true, noise_suppression: false, auto_gain: false });
        let rms = |v: &[i16]| (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        let (mut seed, mut smooth) = (4242u32, 0.0f64);
        let mut next = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            smooth = 0.7 * smooth + 0.3 * (((seed >> 16) as i16) as f64);
            (smooth * 0.5) as i16
        };
        let mut far: Vec<i16> = Vec::new();
        let mut tap: VecDeque<i16> = VecDeque::new();
        let mut cap_q: VecDeque<i16> = VecDeque::new();
        let mut jitter = 17u32;
        let (mut e_in, mut e_out) = (0.0, 0.0);
        for step in 0..2000usize {
            // A cada 10 ms toca um pedaço, que chega à fila do eco em rajadas (0 a 3 pedaços de uma vez).
            let chunk: Vec<i16> = (0..CH).map(|_| next()).collect();
            far.extend_from_slice(&chunk);
            let played = &far[far.len() - CH..];
            tap.extend(played.iter().copied());
            jitter = jitter.wrapping_mul(1103515245).wrapping_add(12345);
            // O microfone ouve o eco a 80 ms.
            let base = (step * CH) as isize - 1280;
            let echo: Vec<i16> = (0..CH as isize).map(|i| 0.6 * far.get((base + i).max(0) as usize).copied().unwrap_or(0) as f64).map(|x| x as i16).collect();
            cap_q.extend(echo.iter().copied());
            // O consumidor só aparece a cada 20 ms.
            if step % 2 == 1 {
                let mut pcm: Vec<i16> = cap_q.drain(..2 * CH).collect();
                let input = pcm.clone();
                if padded {
                    let mut render: Vec<i16> = tap.drain(..tap.len().min(2 * CH)).collect();
                    // Em rajadas, o que chegou até agora é só parte do intervalo.
                    if (jitter >> 16).is_multiple_of(3) {
                        let keep = render.len() / 2;
                        for r in render.drain(keep..).collect::<Vec<_>>().into_iter().rev() {
                            tap.push_front(r);
                        }
                    }
                    render.resize(2 * CH, 0);
                    let mut sink = vec![0i16; CH];
                    for (r, c) in render.chunks(CH).zip(pcm.chunks_mut(CH)) {
                        let src = c.to_vec();
                        apm.process_render_i16(r, &mut sink).unwrap();
                        apm.process_capture_i16(&src, c).unwrap();
                    }
                } else {
                    // Metade das vezes o alto-falante só entregou parte do que tocou neste intervalo.
                    let hold = if (jitter >> 16).is_multiple_of(3) { tap.len().min(CH) } else { 0 };
                    let kept: Vec<i16> = tap.drain(tap.len() - hold..).collect();
                    clean_capture(&mut apm, &mut pcm, &mut tap, CH);
                    tap.extend(kept);
                }
                if step >= 1700 {
                    e_in += rms(&input);
                    e_out += rms(&pcm);
                }
            }
        }
        20.0 * (e_in / e_out.max(1e-9)).log10()
    }

    #[test]
    fn echo_canceller_holds_up_when_the_speaker_reference_arrives_in_bursts() {
        let (old, new) = (erle_with_bursts(true), erle_with_bursts(false));
        assert!(new > 10.0, "ERLE de {new:.1} dB");
        assert!(new > old + 6.0, "sem zeros {new:.1} dB, com zeros {old:.1} dB");
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
