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
#[derive(Default)]
pub(crate) struct Controls {
    pub(crate) muted: AtomicBool,
    prefs: Mutex<AudioPrefs>,
    /// Estado do tratamento da voz (filtro, porta, volume automático); só o laço de áudio mexe nele.
    voice: Mutex<Voice>,
    /// Nível do microfone, de 0.0 a 1.0, em bits de `f32`.
    level: AtomicU32,
    /// Há voz acima do limiar de sensibilidade (e o microfone não está mudo).
    speaking: AtomicBool,
}

impl Controls {
    pub(crate) fn prefs(&self) -> AudioPrefs {
        *self.prefs.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn set_prefs(&self, p: AudioPrefs) {
        *self.prefs.lock().unwrap_or_else(|e| e.into_inner()) = p;
    }
    pub(crate) fn volume(&self) -> f32 {
        self.prefs().volume
    }
    pub(crate) fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
    pub(crate) fn set_level(&self, v: f32) {
        self.level.store(v.to_bits(), Ordering::Relaxed);
        if v == 0.0 {
            self.speaking.store(false, Ordering::Relaxed);
        }
    }
    pub(crate) fn speaking(&self) -> bool {
        self.speaking.load(Ordering::Relaxed)
    }

    /// Prepara um quadro do microfone para enviar: trata a voz, mede o nível (depois do filtro de graves e
    /// antes de porta e ganhos, para o medidor mostrar que o microfone funciona), e zera se estiver mudo.
    pub(crate) fn outgoing(&self, f: &mut [i16]) {
        let prefs = self.prefs();
        let m = self.voice.lock().unwrap_or_else(|e| e.into_inner()).process(f, &prefs);
        self.level.store(m.level.to_bits(), Ordering::Relaxed);
        if self.muted.load(Ordering::Relaxed) {
            f.fill(0);
            self.speaking.store(false, Ordering::Relaxed);
        } else {
            self.speaking.store(m.speaking, Ordering::Relaxed);
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

    fn sine_frame(amp: f32) -> Vec<i16> {
        (0..320).map(|i| (amp * (std::f32::consts::TAU * 1000.0 * i as f32 / 16000.0).sin()) as i16).collect()
    }

    #[test]
    fn scale_pcm_clamps() {
        let mut f = vec![1000, -1000, 30000, -30000];
        scale_pcm(&mut f, 2.0);
        assert_eq!(f, vec![2000, -2000, i16::MAX, i16::MIN]);
    }

    #[test]
    fn outgoing_measures_before_mute_and_gain() {
        let c = Controls::default();
        let mut f = sine_frame(4000.0);
        c.muted.store(true, Ordering::Relaxed);
        c.outgoing(&mut f);
        assert!(f.iter().all(|x| *x == 0));
        assert!(c.level() > 0.5, "o medidor deve mostrar o microfone mesmo mudo");
        assert!(!c.speaking(), "mudo nunca aparece como falando");
        c.muted.store(false, Ordering::Relaxed);
        c.set_prefs(AudioPrefs { high_pass: false, mic_gain: 0.5, ..AudioPrefs::default() });
        let mut f = vec![4000i16; 320];
        c.outgoing(&mut f);
        assert!(f.iter().all(|x| *x == 2000));
    }

    #[test]
    fn sensitivity_decides_whether_a_quiet_voice_is_speaking() {
        let c = Controls::default();
        let quiet = |gate_db: f32| {
            c.set_prefs(AudioPrefs { gate_db, ..AudioPrefs::default() });
            c.outgoing(&mut sine_frame(300.0));
            c.speaking()
        };
        // 300 de amplitude ≈ -43 dBFS: acima de -55, abaixo de -30.
        assert!(quiet(-55.0));
        assert!(!quiet(-30.0));
    }

    #[test]
    fn zero_level_clears_speaking() {
        let c = Controls::default();
        c.outgoing(&mut sine_frame(8000.0));
        assert!(c.speaking());
        c.set_level(0.0);
        assert!(!c.speaking());
    }
}
