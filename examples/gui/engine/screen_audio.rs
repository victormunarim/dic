//! Som do computador na tela de quem transmite: captura (sem o que o programa toca), Opus e envio.

use super::*;

/// Abaixo disto (pico, em amostras de 16 bits) um quadro é silêncio: não vale gastar slot nem banda.
#[cfg_attr(not(feature = "audio"), allow(dead_code))]
const SILENT_PEAK: i16 = 16;
/// Quadros mandados depois do último com som, para a cauda do som não ser cortada.
#[cfg_attr(not(feature = "audio"), allow(dead_code))]
const HANG: u32 = 10;

/// Fração dos slots que o som toma quando toca sem parar (um pacote a cada 20 ms): o vídeo cede essa parte.
pub(crate) fn audio_share(slot: Duration) -> f64 {
    (slot.as_secs_f64() / 0.020).min(0.5)
}

/// Um quadro é silêncio?
#[cfg_attr(not(feature = "audio"), allow(dead_code))]
fn is_silent(pcm: &[i16]) -> bool {
    pcm.iter().all(|&x| x.saturating_abs() < SILENT_PEAK)
}

/// Decide quais quadros saem: os com som e os `HANG` seguintes.
#[derive(Default)]
#[cfg_attr(not(feature = "audio"), allow(dead_code))]
struct Gate {
    hang: u32,
}

impl Gate {
    #[cfg_attr(not(feature = "audio"), allow(dead_code))]
    fn pass(&mut self, pcm: &[i16]) -> bool {
        if is_silent(pcm) {
            self.hang = self.hang.saturating_sub(1);
            self.hang > 0
        } else {
            self.hang = HANG;
            true
        }
    }
}

#[cfg(feature = "audio")]
pub(crate) use real::AudioTx;

#[cfg(feature = "audio")]
mod real {
    use super::*;
    use clam::{media::codec::{Codec, Opus}, sysaudio::SystemAudio};

    /// A captura do som do computador e o codificador.
    pub(crate) struct AudioTx {
        sys: SystemAudio,
        opus: Opus,
        gate: Gate,
    }

    impl AudioTx {
        /// Abre a captura se a pessoa pediu som na tela. Diz no log (e na nota da janela) por que não abriu.
        pub(crate) fn open(shared: &Shared, out: &Sink) -> Option<Self> {
            if !shared.screen.audio.load(Ordering::Relaxed) {
                return None;
            }
            // Sem áudio aberto o programa não toca nada, e não há o que tirar.
            let reference = shared.screen.speaker.lock_unpoisoned().clone().unwrap_or_else(clam::audio::SpeakerReference::detached);
            match SystemAudio::open(reference) {
                Ok(sys) => {
                    out.line("tela: levando o som do computador (sem o que o programa toca)");
                    Some(Self { sys, opus: Opus::new(), gate: Gate::default() })
                }
                Err(e) => {
                    out.line(format!("tela: sem som do computador ({e}); só a imagem"));
                    None
                }
            }
        }

        /// Codifica o que a captura juntou e entrega cada quadro a `push`. Devolve `false` se a captura morreu.
        pub(crate) fn pump(&mut self, mut push: impl FnMut(&[u8])) -> bool {
            while let Some(pcm) = self.sys.frame() {
                if self.gate.pass(&pcm)
                    && let Ok(data) = self.opus.encode(&pcm)
                {
                    push(&data);
                }
            }
            !self.sys.is_dead()
        }
    }
}

/// Sem a feature `audio` a tela vai sem som: o tipo é vazio.
#[cfg(not(feature = "audio"))]
pub(crate) enum AudioTx {}

#[cfg(not(feature = "audio"))]
impl AudioTx {
    pub(crate) fn open(shared: &Shared, out: &Sink) -> Option<Self> {
        if shared.screen.audio.load(Ordering::Relaxed) {
            out.line("tela: sem som do computador (compilado sem a feature audio); só a imagem");
        }
        None
    }

    pub(crate) fn pump(&mut self, _: impl FnMut(&[u8])) -> bool {
        match *self {}
    }
}

/// Dorme até `until` em fatias curtas, chamando `tick` entre elas: o som sai a cada 20 ms enquanto o laço
/// de vídeo espera o próximo quadro (que pode estar a 60 ms ou mais).
pub(crate) fn nap_until(until: std::time::Instant, mut tick: impl FnMut()) {
    loop {
        tick();
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(10)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_dropped_after_the_tail_and_sound_reopens_the_gate() {
        let (quiet, loud) = (vec![0i16; 320], vec![2000i16; 320]);
        let mut g = Gate::default();
        assert!(!g.pass(&quiet));
        assert!(g.pass(&loud));
        let tail = (0..40).filter(|_| g.pass(&quiet)).count();
        assert_eq!(tail, (HANG - 1) as usize);
        assert!(g.pass(&loud));
    }

    #[test]
    fn video_gives_up_the_share_audio_takes() {
        assert!((audio_share(Duration::from_millis(2)) - 0.1).abs() < 1e-9);
        assert!((audio_share(Duration::from_micros(500)) - 0.025).abs() < 1e-9);
        assert_eq!(audio_share(Duration::from_millis(40)), 0.5);
    }
}
