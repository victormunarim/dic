//! Microfone, alto-falante e os ajustes de áudio da janela.

use super::*;

#[cfg(feature = "audio")]
pub(crate) type Sound = clam::audio::Audio;

/// Sem a feature `audio` nenhum `Sound` existe: o tipo é vazio.
#[cfg(not(feature = "audio"))]
pub(crate) enum Sound {}

#[cfg(not(feature = "audio"))]
impl Sound {
    pub(crate) fn mic_frame(&self) -> Option<Vec<i16>> {
        match *self {}
    }
    pub(crate) fn played(&self) -> usize {
        match *self {}
    }
    pub(crate) fn take_error(&self) -> Option<String> {
        match *self {}
    }
    pub(crate) fn play(&self, _: &[i16]) {
        match *self {}
    }
}

/// Abre microfone/alto-falante se pedido; sem eles, cai para o tom de teste.
pub(crate) fn open_sound(want: bool, form: &Form, out: &Sink) -> Option<Sound> {
    if !want {
        return None;
    }
    #[cfg(feature = "audio")]
    match Sound::open_with(G722::RATE, &form.devices()) {
        Ok(s) => {
            out.line(format!("microfone e alto-falante abertos (use fones: não há cancelamento de eco): {}", s.info()));
            // Bipe de 0,3 s: se não soar, o problema é a saída do sistema, não a chamada.
            let beep: Vec<i16> = (0..4800).map(|i| (8000.0 * (std::f64::consts::TAU * 440.0 * i as f64 / 16000.0).sin()) as i16).collect();
            s.play(&beep);
            out.line("tocando um bipe de teste: se você não ouvir, a saída de áudio do sistema é a causa");
            return Some(s);
        }
        Err(e) => out.line(format!("{e}; usando tom de teste")),
    }
    #[cfg(not(feature = "audio"))]
    let _ = form;
    #[cfg(not(feature = "audio"))]
    out.line("compilado sem a feature audio; usando tom de teste");
    None
}

/// Ajustes de áudio da janela, lidos pelo motor a cada quadro.
pub(crate) struct Controls {
    pub(crate) muted: AtomicBool,
    /// Volume do alto-falante e ganho do microfone (1.0 = sem alteração), em bits de `f32`.
    volume: AtomicU32,
    mic_gain: AtomicU32,
    /// Nível do microfone, de 0.0 a 1.0.
    level: AtomicU32,
}

impl Default for Controls {
    fn default() -> Self {
        Controls { muted: AtomicBool::new(false), volume: AtomicU32::new(1f32.to_bits()), mic_gain: AtomicU32::new(1f32.to_bits()), level: AtomicU32::new(0) }
    }
}

impl Controls {
    pub(crate) fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }
    pub(crate) fn set_volume(&self, v: f32) {
        self.volume.store(v.to_bits(), Ordering::Relaxed);
    }
    pub(crate) fn mic_gain(&self) -> f32 {
        f32::from_bits(self.mic_gain.load(Ordering::Relaxed))
    }
    pub(crate) fn set_mic_gain(&self, v: f32) {
        self.mic_gain.store(v.to_bits(), Ordering::Relaxed);
    }
    pub(crate) fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
    pub(crate) fn set_level(&self, v: f32) {
        self.level.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Prepara um quadro do microfone para enviar: mede o nível (antes de mudo e ganho, para o medidor
    /// mostrar que o microfone funciona), depois zera se mudo ou aplica o ganho.
    pub(crate) fn outgoing(&self, f: &mut [i16]) {
        self.set_level(level(f));
        if self.muted.load(Ordering::Relaxed) {
            f.fill(0);
        } else {
            scale_pcm(f, self.mic_gain());
        }
    }
}

/// Multiplica as amostras por `gain`, sem estourar o `i16`.
pub(crate) fn scale_pcm(f: &mut [i16], gain: f32) {
    if gain == 1.0 {
        return;
    }
    for x in f {
        *x = (*x as f32 * gain).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

/// Nível de um quadro de 0.0 (silêncio, -60 dBFS ou menos) a 1.0 (fundo de escala).
pub(crate) fn level(f: &[i16]) -> f32 {
    if f.is_empty() {
        return 0.0;
    }
    let rms = (f.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / f.len() as f64).sqrt();
    ((20.0 * (rms / 32768.0).max(1e-9).log10() + 60.0) / 60.0).clamp(0.0, 1.0) as f32
}

pub(crate) fn tone(hz: f64, n: usize) -> Vec<i16> {
    (0..320).map(|i| (6000.0 * (std::f64::consts::TAU * hz * (n * 320 + i) as f64 / 16000.0).sin()) as i16).collect()
}

/// Quadros a enviar neste tick, já com mudo e ganho: do microfone (até 2, para absorver atraso do relógio)
/// ou, sem áudio, o tom de teste.
pub(crate) fn mic_frames(sound: Option<&Sound>, hz: f64, n: usize, shared: &Shared) -> Vec<Vec<i16>> {
    let mut frames: Vec<Vec<i16>> = match sound {
        Some(s) => std::iter::from_fn(|| s.mic_frame()).take(2).collect(),
        None => vec![tone(hz, n)],
    };
    frames.iter_mut().for_each(|f| shared.audio.outgoing(f));
    frames
}

/// Toca um quadro recebido no volume escolhido na janela (sem áudio, descarta).
pub(crate) fn play_scaled(sound: Option<&Sound>, mut f: Vec<i16>, shared: &Shared) {
    if let Some(s) = sound {
        scale_pcm(&mut f, shared.audio.volume());
        s.play(&f);
    }
}

/// Mostra no log o último erro do dispositivo de áudio, se houve.
pub(crate) fn report_error(sound: Option<&Sound>, out: &Sink) {
    if let Some(e) = sound.and_then(Sound::take_error) {
        out.line(format!("áudio: {e}"));
    }
}

/// Devolve o microfone ao alto-falante até a janela encerrar o teste (`mic_test`) ou o nó parar.
pub(crate) async fn mic_test(sound: Sound, shared: Arc<Shared>, stop: Arc<AtomicBool>, out: Sink) {
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut n = 0u32;
    while shared.mic_test.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        while let Some(mut f) = sound.mic_frame() {
            shared.audio.outgoing(&mut f);
            play_scaled(Some(&sound), f, &shared);
        }
        n += 1;
        if n.is_multiple_of(50) {
            report_error(Some(&sound), &out);
        }
    }
    shared.audio.set_level(0.0);
    shared.mic_test.store(false, Ordering::Relaxed);
    shared.in_call.store(false, Ordering::Relaxed);
    out.line("teste do microfone encerrado");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_pcm_clamps_and_level_follows_loudness() {
        let mut f = vec![1000, -1000, 30000, -30000];
        scale_pcm(&mut f, 2.0);
        assert_eq!(f, vec![2000, -2000, i16::MAX, i16::MIN]);
        assert_eq!(level(&[0; 320]), 0.0);
        assert!(level(&[1000; 320]) > level(&[100; 320]));
        assert!(level(&[i16::MAX; 320]) > 0.99);
    }

    #[test]
    fn outgoing_measures_before_mute_and_gain() {
        let c = Controls::default();
        let mut f = vec![4000i16; 320];
        c.muted.store(true, Ordering::Relaxed);
        c.outgoing(&mut f);
        assert!(f.iter().all(|x| *x == 0));
        assert!(c.level() > 0.5, "o medidor deve mostrar o microfone mesmo mudo");
        c.muted.store(false, Ordering::Relaxed);
        c.set_mic_gain(0.5);
        let mut f = vec![4000i16; 320];
        c.outgoing(&mut f);
        assert!(f.iter().all(|x| *x == 2000));
    }
}
