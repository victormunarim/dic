//! Áudio do computador para a transmissão de tela, sem o que o próprio programa toca.
//!
//! Captura o que o sistema manda ao alto-falante (Linux: o monitor da saída padrão do PulseAudio/PipeWire,
//! lido por um `gst-launch-1.0` filho; Windows: loopback do WASAPI na saída padrão, pelo `cpal`) e tira dele o que o programa mesmo tocou (voz do contato e áudio de
//! tela de outros), com um filtro adaptativo que só subtrai: a referência é o que saiu pelo alto-falante
//! do programa ([`Audio::speaker_reference`]). Sem isso, quem assiste ouviria a própria voz de volta.
//!
//! A subtração é adaptativa, não um isolamento por aplicativo: sobra um resto fraco da voz do contato e,
//! enquanto ele fala por cima de um som alto, o cancelador pode abaixar um pouco o som do sistema.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::{io::Read, process::{Child, Command, Stdio}};

#[cfg(windows)]
use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread::JoinHandle,
    time::Duration,
};

use crate::{
    audio::{AudioError, SpeakerReference},
    sync::LockExt,
};

/// Taxa do áudio da tela (a mesma do Opus da voz).
pub const RATE: u32 = 16_000;
/// Amostras de um quadro de 20 ms.
pub const FRAME: usize = (RATE / 50) as usize;
/// Áudio capturado e ainda não lido: mais que 1 s é atraso que ninguém quer ouvir, o mais antigo cai.
#[cfg(any(unix, windows))]
const MAX_QUEUED: usize = RATE as usize;

/// Filtros de 200 ms: cobrem o atraso entre o programa escrever na placa e o monitor devolver o som.
const TAPS: usize = (RATE as usize) / 5;
/// Passo de adaptação do NLMS (0 a 1): maior aprende mais rápido e erra mais no regime.
const MU: f32 = 0.25;
/// Referência a juntar antes de começar (40 ms): evita esvaziar e deslocar a linha do tempo.
const PRIME: usize = (RATE as usize) / 25;
/// Referência guardada no máximo (500 ms); passou disso o excesso antigo cai.
const MAX_PENDING: usize = (RATE as usize) / 2;

/// Filtro adaptativo NLMS: aprende o caminho linear da referência até o áudio capturado e o subtrai.
///
/// Não usa o cancelador de eco do WebRTC porque ele, além de subtrair, abaixa tudo que ouve enquanto a
/// referência fala (para o eco acústico não vazar), e aqui isso levaria junto a música e o jogo. O
/// caminho do monitor da placa é quase linear, então só subtrair basta.
struct Nlms {
    w: Vec<f32>,
    /// Histórico da referência em buffer duplo: a janela de `TAPS` amostras começa em `pos`, a mais nova primeiro.
    hist: Vec<f32>,
    pos: usize,
    energy: f32,
}

impl Nlms {
    fn new() -> Self {
        Self { w: vec![0.0; TAPS], hist: vec![0.0; 2 * TAPS], pos: 0, energy: 0.0 }
    }

    /// Entra com a próxima amostra da referência e a capturada na mesma hora; devolve a capturada sem o
    /// que o filtro reconhece como vindo da referência.
    fn step(&mut self, reference: f32, captured: f32) -> f32 {
        self.pos = if self.pos == 0 { TAPS - 1 } else { self.pos - 1 };
        let old = self.hist.get(self.pos + TAPS).copied().unwrap_or(0.0);
        self.energy = (self.energy + reference * reference - old * old).max(0.0);
        if let Some(h) = self.hist.get_mut(self.pos) {
            *h = reference;
        }
        if let Some(h) = self.hist.get_mut(self.pos + TAPS) {
            *h = reference;
        }
        let Some(window) = self.hist.get(self.pos..self.pos + TAPS) else { return captured };
        let est: f32 = self.w.iter().zip(window).map(|(w, x)| w * x).sum();
        let err = captured - est;
        let gain = MU * err / (self.energy + 1e-3 * TAPS as f32 + 1.0);
        for (w, x) in self.w.iter_mut().zip(window) {
            *w += gain * x;
        }
        err
    }
}

/// Tira do áudio do sistema o que o programa tocou.
pub struct Excluder {
    nlms: Nlms,
    /// Referência que ainda não encontrou sua amostra capturada.
    pending: VecDeque<i16>,
    primed: bool,
    reference: SpeakerReference,
}

impl Excluder {
    #[must_use]
    pub fn new(reference: SpeakerReference) -> Self {
        Self { nlms: Nlms::new(), pending: VecDeque::new(), primed: false, reference }
    }

    /// Passa `pcm` pelo subtrator, no lugar. Cada amostra capturada casa com a próxima da referência, na
    /// ordem em que chegaram (as duas correm na mesma taxa, então o atraso fixo entre elas fica nos
    /// coeficientes). Faltando referência, usa silêncio, que não adapta nada.
    pub fn process(&mut self, pcm: &mut [i16]) {
        self.pending.extend(self.reference.take(MAX_PENDING));
        if self.pending.len() > MAX_PENDING {
            let extra = self.pending.len() - MAX_PENDING;
            self.pending.drain(..extra);
        }
        if !self.primed {
            self.primed = self.pending.len() >= PRIME;
        }
        for c in pcm.iter_mut() {
            let r = if self.primed { self.pending.pop_front().unwrap_or(0) } else { 0 };
            let out = self.nlms.step(f32::from(r), f32::from(*c));
            *c = out.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        }
    }
}

/// Áudio do sistema capturado, já sem o do programa, em quadros de 20 ms a 16 kHz mono.
pub struct SystemAudio {
    queue: Arc<Mutex<VecDeque<i16>>>,
    #[cfg(unix)]
    child: Child,
    #[cfg(windows)]
    capture: Loopback,
    excluder: Excluder,
}

/// Loopback do WASAPI: uma thread dona do `Stream` do `cpal` (que não precisa ser `Send`), parada por `stop`.
/// `dead` sobe se o dispositivo some ou o stream dá erro.
#[cfg(windows)]
struct Loopback {
    stop: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    /// Último erro do stream, para a janela dizer por que o som parou.
    error: Arc<Mutex<String>>,
    thread: Option<JoinHandle<()>>,
}

/// Abre o loopback da saída padrão numa thread e espera dizer se deu certo. O `cpal` trata uma saída aberta
/// como entrada como loopback, e ele entrega o que o sistema toca, na taxa e canais do mixer (que a conversão
/// abaixo reduz a 16 kHz mono).
#[cfg(windows)]
fn open_loopback(queue: Arc<Mutex<VecDeque<i16>>>) -> Result<Loopback, AudioError> {
    type Dead = (Arc<AtomicBool>, Arc<Mutex<String>>);
    use cpal::{
        SampleFormat,
        traits::{DeviceTrait, HostTrait, StreamTrait},
    };

    fn build<T>(dev: &cpal::Device, cfg: cpal::StreamConfig, q: Arc<Mutex<VecDeque<i16>>>, dead: Dead) -> Result<cpal::Stream, AudioError>
    where
        T: cpal::SizedSample,
        f32: cpal::FromSample<T>,
    {
        let ch = usize::from(cfg.channels).max(1);
        let mut down = crate::audio::Downsampler::new(cfg.sample_rate, RATE);
        dev.build_input_stream(
            cfg,
            move |data: &[T], _| {
                let mut q = q.lock_unpoisoned();
                for frame in data.chunks(ch) {
                    let mono = frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / ch as f32;
                    if let Some(v) = down.push(mono) {
                        q.push_back(v);
                    }
                }
                if q.len() > MAX_QUEUED {
                    let extra = q.len() - MAX_QUEUED;
                    q.drain(..extra);
                }
            },
            move |e| {
                // Xrun (descontinuidade do loopback, comum quando o mixer pára e volta) e troca de rota são
                // avisos: o stream segue vivo. Tratá-los como morte congelava a captura no primeiro aviso.
                if matches!(e.kind(), cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged) {
                    return;
                }
                *dead.1.lock_unpoisoned() = e.to_string();
                dead.0.store(true, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| AudioError(format!("loopback do WASAPI: {e}")))
    }

    /// Toca silêncio na mesma saída: o WASAPI só entrega pacotes de loopback enquanto algo é renderizado, e
    /// sem isso um mixer parado deixa a captura muda (e, em alguns drivers, nunca mais acorda).
    fn keepalive<T>(dev: &cpal::Device, cfg: cpal::StreamConfig) -> Result<cpal::Stream, AudioError>
    where
        T: cpal::SizedSample,
    {
        dev.build_output_stream(cfg, |data: &mut [T], _| data.fill(T::EQUILIBRIUM), |_| {}, None)
            .map_err(|e| AudioError(format!("silêncio de apoio: {e}")))
    }

    let (stop, dead) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let error = Arc::new(Mutex::new(String::new()));
    let (stop_t, dead_t) = (stop.clone(), (dead.clone(), error.clone()));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), AudioError>>();
    let thread = std::thread::spawn(move || {
        let open = || -> Result<(cpal::Stream, Option<cpal::Stream>), AudioError> {
            let dev = cpal::default_host().default_output_device().ok_or_else(|| AudioError("não achei a saída de som padrão do sistema".into()))?;
            let cfg = dev.default_output_config().map_err(|e| AudioError(format!("saída de som padrão: {e}")))?;
            let sc: cpal::StreamConfig = cfg.clone().into();
            let stream = match cfg.sample_format() {
                SampleFormat::F32 => build::<f32>(&dev, sc, queue, dead_t.clone())?,
                SampleFormat::I16 => build::<i16>(&dev, sc, queue, dead_t.clone())?,
                SampleFormat::I32 => build::<i32>(&dev, sc, queue, dead_t.clone())?,
                SampleFormat::U16 => build::<u16>(&dev, sc, queue, dead_t.clone())?,
                f => return Err(AudioError(format!("formato da saída não suportado: {f}"))),
            };
            stream.play().map_err(|e| AudioError(format!("loopback do WASAPI: {e}")))?;
            let sc: cpal::StreamConfig = cfg.clone().into();
            let quiet = match cfg.sample_format() {
                SampleFormat::F32 => keepalive::<f32>(&dev, sc),
                SampleFormat::I16 => keepalive::<i16>(&dev, sc),
                SampleFormat::I32 => keepalive::<i32>(&dev, sc),
                SampleFormat::U16 => keepalive::<u16>(&dev, sc),
                _ => Err(AudioError(String::new())),
            }
            .ok()
            .filter(|s| s.play().is_ok());
            Ok((stream, quiet))
        };
        match open() {
            Ok(streams) => {
                let _ = ready_tx.send(Ok(()));
                while !stop_t.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                drop(streams);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    });
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(Loopback { stop, dead, error, thread: Some(thread) }),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err(AudioError("o WASAPI não respondeu ao abrir o loopback".into()))
        }
    }
}

/// Argumentos do `gst-launch-1.0` que leem o monitor `source` e escrevem PCM de 16 bits no stdout.
#[cfg(unix)]
fn gst_args(source: &str) -> Vec<String> {
    format!("-q pulsesrc device={source} ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate={RATE},channels=1 ! fdsink fd=1")
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Nome que o servidor de som resolve sozinho para o monitor da saída padrão: vale no PulseAudio e no PipeWire
/// (`pipewire-pulse`), sem precisar do `pactl`.
#[cfg(unix)]
const DEFAULT_MONITOR: &str = "@DEFAULT_MONITOR@";

/// Nome do monitor da saída padrão (`<saída>.monitor`), pelo `pactl`. Sem o `pactl` instalado cai no nome
/// especial `@DEFAULT_MONITOR@`, que o servidor de som resolve: o programa não exige um pacote só para isso.
#[cfg(unix)]
fn default_monitor() -> Result<String, AudioError> {
    let out = match Command::new("pactl").arg("get-default-sink").output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DEFAULT_MONITOR.to_string()),
        Err(e) => return Err(AudioError(format!("não deu para rodar o pactl: {e}"))),
    };
    let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || sink.is_empty() {
        return Err(AudioError("não achei a saída de som padrão do sistema".into()));
    }
    Ok(format!("{sink}.monitor"))
}

impl SystemAudio {
    /// Começa a capturar o áudio do sistema, subtraindo o que `reference` diz que o programa tocou.
    ///
    /// # Errors
    ///
    /// Sem `gst-launch-1.0`, sem saída de som padrão ou com o servidor de som recusando o monitor.
    #[cfg(unix)]
    pub fn open(reference: SpeakerReference) -> Result<Self, AudioError> {
        let source = default_monitor()?;
        let mut child = Command::new("gst-launch-1.0")
            .args(gst_args(&source))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| AudioError(format!("não deu para rodar o gst-launch-1.0 (instale gstreamer1.0-tools e gstreamer1.0-plugins-good): {e}")))?;
        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err(AudioError("sem saída do gst-launch-1.0".into()));
        };
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let q = queue.clone();
        std::thread::spawn(move || {
            let (mut buf, mut odd) = ([0u8; 4096], None::<u8>);
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut q = q.lock_unpoisoned();
                for &b in buf.iter().take(n) {
                    match odd.take() {
                        Some(lo) => q.push_back(i16::from_le_bytes([lo, b])),
                        None => odd = Some(b),
                    }
                }
                if q.len() > MAX_QUEUED {
                    let extra = q.len() - MAX_QUEUED;
                    q.drain(..extra);
                }
            }
        });
        Ok(Self { queue, child, excluder: Excluder::new(reference) })
    }

    /// Começa a capturar o áudio do sistema (loopback do WASAPI), subtraindo o que `reference` diz que o
    /// programa tocou.
    ///
    /// # Errors
    ///
    /// Sem saída de som padrão ou com o WASAPI recusando o loopback.
    #[cfg(windows)]
    pub fn open(reference: SpeakerReference) -> Result<Self, AudioError> {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let capture = open_loopback(queue.clone())?;
        Ok(Self { queue, capture, excluder: Excluder::new(reference) })
    }

    /// Demais sistemas: ainda não há captura do áudio do sistema.
    #[cfg(not(any(unix, windows)))]
    pub fn open(_: SpeakerReference) -> Result<Self, AudioError> {
        Err(AudioError("áudio da tela só existe no Linux e no Windows".into()))
    }

    /// O capturador morreu (sem saída de som, `gst-launch-1.0` caiu, dispositivo removido)?
    pub fn is_dead(&mut self) -> bool {
        #[cfg(unix)]
        return matches!(self.child.try_wait(), Ok(Some(_)) | Err(_));
        #[cfg(windows)]
        return self.capture.dead.load(Ordering::Relaxed);
        #[cfg(not(any(unix, windows)))]
        true
    }

    /// Por que a captura parou, se o sistema disse (só no Windows; no Linux o `gst-launch-1.0` morre mudo).
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        #[cfg(windows)]
        return Some(self.capture.error.lock_unpoisoned().clone()).filter(|e| !e.is_empty());
        #[cfg(not(windows))]
        None
    }

    /// Próximo quadro de 20 ms, já sem o áudio do programa, se já houver.
    pub fn frame(&mut self) -> Option<Vec<i16>> {
        let mut pcm: Vec<i16> = {
            let mut q = self.queue.lock_unpoisoned();
            if q.len() < FRAME {
                return None;
            }
            q.drain(..FRAME).collect()
        };
        self.excluder.process(&mut pcm);
        Some(pcm)
    }
}

impl Drop for SystemAudio {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        #[cfg(windows)]
        {
            self.capture.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.capture.thread.take() {
                let _ = t.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ruído branco determinístico.
    fn noise(seed: u32, amp: f64) -> impl FnMut() -> f64 {
        let mut s = seed;
        move || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            ((s >> 16) as i16) as f64 / 32768.0 * amp
        }
    }

    fn rms(v: &[f64]) -> f64 {
        (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt()
    }

    /// O som do sistema (música) mais o que o programa tocou, atrasado como a placa o atrasa. Depois de
    /// aprender, sobra muito pouco do que o programa tocou e a música continua lá.
    #[test]
    fn what_the_app_played_is_taken_out_and_the_rest_stays() {
        let reference = SpeakerReference::detached();
        let mut ex = Excluder::new(reference.clone());
        let (mut app, mut music) = (noise(1, 0.5), noise(2, 0.1));
        let delay = 640; // 40 ms
        let total = RATE as usize * 8;
        let played: Vec<f64> = (0..total).map(|_| app()).collect();
        let music: Vec<f64> = (0..total).map(|_| music()).collect();
        let (mut left_app, mut left_loop, mut want_music) = (vec![], vec![], vec![]);
        for (n, chunk) in (0..total).step_by(FRAME).enumerate() {
            let ref_pcm: Vec<i16> = (chunk..chunk + FRAME).map(|i| (played[i] * 32767.0) as i16).collect();
            reference.feed(&ref_pcm);
            let mut mixed: Vec<i16> = (chunk..chunk + FRAME)
                .map(|i| ((music[i] + if i >= delay { played[i - delay] } else { 0.0 }) * 32767.0) as i16)
                .collect();
            let before: Vec<f64> = mixed.iter().map(|&x| f64::from(x) / 32768.0).collect();
            ex.process(&mut mixed);
            if n > 300 {
                left_loop.extend(before);
                left_app.extend(mixed.iter().map(|&x| f64::from(x) / 32768.0));
                want_music.extend((chunk..chunk + FRAME).map(|i| music[i]));
            }
        }
        let (r_in, r_out, r_music) = (rms(&left_loop), rms(&left_app), rms(&want_music));
        assert!(r_in > r_music * 3.0);
        assert!(r_out < r_music * 1.3, "o que o programa tocou quase não saiu: entrada {r_in:.4}, saída {r_out:.4}");
        assert!(r_out > r_music * 0.8, "o som do sistema sumiu junto: saída {r_out:.4}, música {r_music:.4}");
    }

    #[test]
    fn without_anything_played_the_system_audio_passes_untouched_in_level() {
        let mut ex = Excluder::new(SpeakerReference::detached());
        let mut music = noise(3, 0.2);
        let (mut a, mut b) = (vec![], vec![]);
        for n in 0..200 {
            let mut pcm: Vec<i16> = (0..FRAME).map(|_| (music() * 32767.0) as i16).collect();
            let before: Vec<f64> = pcm.iter().map(|&x| f64::from(x)).collect();
            ex.process(&mut pcm);
            if n > 50 {
                a.extend(before);
                b.extend(pcm.iter().map(|&x| f64::from(x)));
            }
        }
        let ratio = rms(&b) / rms(&a);
        assert!((0.7..1.3).contains(&ratio), "o nível mudou sem nada a tirar: {ratio:.2}");
    }

    #[cfg(unix)]
    #[test]
    fn gst_pipeline_asks_for_16k_mono_s16() {
        let a = gst_args("alto.monitor").join(" ");
        assert!(a.contains("device=alto.monitor") && a.contains("rate=16000,channels=1") && a.contains("S16LE"));
    }
}
