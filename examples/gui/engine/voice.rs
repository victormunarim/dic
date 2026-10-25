//! Tratamento da voz que sai do microfone: filtro de graves, porta de ruído e volume automático.
//!
//! Tudo roda em quadros de qualquer tamanho a 16 kHz, guardando o estado entre um quadro e o seguinte.
//! A ordem é: filtro de graves → medida do nível (a mesma que o medidor da janela mostra) → porta →
//! ganho do microfone × volume automático → limitador de picos.

use crate::conf::AudioPrefs;

/// Taxa de amostragem do áudio da chamada (G.722).
const RATE: f32 = 16_000.0;
/// Corte do filtro de graves: abaixo disso é ventilador, estrondo de mesa e sopro, não voz.
const HIGH_PASS_HZ: f32 = 100.0;
/// A porta olha blocos de 5 ms: reage ao começo de uma palavra sem esperar o quadro inteiro de 20 ms.
const BLOCK: usize = 80;
/// Depois que o som cai abaixo do limiar a porta continua aberta por 250 ms, para não cortar o fim das palavras.
const HOLD_SAMPLES: usize = 4000;
/// Tempos de abertura e fechamento da porta (constantes de tempo, em segundos).
const GATE_ATTACK_S: f32 = 0.002;
const GATE_RELEASE_S: f32 = 0.060;
/// Volume automático: nível médio buscado (dBFS), maior reforço e maior corte (dB).
const AGC_TARGET_DB: f32 = -22.0;
const AGC_MAX_BOOST_DB: f32 = 24.0;
const AGC_MAX_CUT_DB: f32 = -12.0;
/// Abaixo disso o quadro é ruído de fundo: o volume automático não o reforça.
const AGC_FLOOR_DB: f32 = -50.0;
/// O limitador começa a comprimir aqui, antes do fundo de escala.
const LIMIT_KNEE: f32 = 24_000.0;

/// Nível de `samples` em dBFS (RMS); silêncio total dá -120.
pub(crate) fn rms_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    let ms = samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32;
    (10.0 * (ms / (32768.0 * 32768.0)).max(1e-12).log10()).max(-120.0)
}

/// Nível de 0.0 (-60 dBFS ou menos) a 1.0 (fundo de escala), a escala do medidor da janela.
pub(crate) fn voice_meter(db: f32) -> f32 {
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// O que o tratamento mediu num quadro.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Measure {
    /// Nível do microfone depois do filtro de graves e antes de porta e ganhos, de 0.0 a 1.0.
    pub(crate) level: f32,
    /// Há voz (acima do limiar, ou ainda no tempo de espera da porta).
    pub(crate) speaking: bool,
}

#[derive(Default)]
pub(crate) struct Voice {
    /// Memória dos dois estágios do filtro de graves: (entrada anterior, saída anterior).
    hp: [(f32, f32); 2],
    /// Ganho atual da porta (1.0 aberta, até a redução escolhida fechada).
    gate_gain: Option<f32>,
    /// Amostras que faltam para a porta fechar.
    hold: usize,
    /// Ganho do volume automático em dB e o linear aplicado ao fim do quadro anterior.
    agc_db: f32,
    agc_applied: Option<f32>,
}

impl Voice {
    /// Trata `frame` no lugar e devolve o que mediu. O estado de um recurso desligado é reiniciado, para
    /// religá-lo não herdar nada velho.
    pub(crate) fn process(&mut self, frame: &mut [i16], p: &AudioPrefs) -> Measure {
        let mut x: Vec<f32> = frame.iter().map(|s| *s as f32).collect();
        if p.high_pass {
            self.high_pass(&mut x);
        } else {
            self.hp = Default::default();
        }
        let db = rms_db(&x);
        let level = voice_meter(db);

        let open = self.gate(&mut x, p);
        let speaking = if p.gate { open } else { db >= p.gate_db };

        let user = p.mic_gain;
        let target = self.agc_target(p, db, user, speaking);
        let start = self.agc_applied.unwrap_or(target);
        let n = x.len().max(1) as f32;
        for (i, s) in x.iter_mut().enumerate() {
            let g = user * (start + (target - start) * (i as f32 + 1.0) / n);
            *s = if p.agc { limit(*s * g) } else { *s * g };
        }
        self.agc_applied = Some(target);

        for (o, s) in frame.iter_mut().zip(&x) {
            *o = s.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
        Measure { level, speaking }
    }

    /// Dois filtros passa-altas de primeira ordem em cascata (12 dB por oitava).
    fn high_pass(&mut self, x: &mut [f32]) {
        let rc = 1.0 / (std::f32::consts::TAU * HIGH_PASS_HZ);
        let dt = 1.0 / RATE;
        let a = rc / (rc + dt);
        for s in x.iter_mut() {
            let mut v = *s;
            for (prev_in, prev_out) in &mut self.hp {
                let out = a * (*prev_out + v - *prev_in);
                (*prev_in, *prev_out) = (v, out);
                v = out;
            }
            *s = v;
        }
    }

    /// Aplica a porta de ruído; devolve se ela terminou o quadro aberta.
    fn gate(&mut self, x: &mut [f32], p: &AudioPrefs) -> bool {
        if !p.gate {
            self.gate_gain = None;
            self.hold = 0;
            return true;
        }
        let closed = 10f32.powf(-p.gate_depth_db / 20.0);
        let mut g = self.gate_gain.unwrap_or(closed);
        let (attack, release) = (1.0 - (-1.0 / (GATE_ATTACK_S * RATE)).exp(), 1.0 - (-1.0 / (GATE_RELEASE_S * RATE)).exp());
        for block in x.chunks_mut(BLOCK) {
            if rms_db(block) >= p.gate_db {
                self.hold = HOLD_SAMPLES;
            } else {
                self.hold = self.hold.saturating_sub(block.len());
            }
            let (goal, k) = if self.hold > 0 { (1.0, attack) } else { (closed, release) };
            for s in block {
                g += (goal - g) * k;
                *s *= g;
            }
        }
        self.gate_gain = Some(g);
        self.hold > 0
    }

    /// Ganho linear do volume automático para o fim deste quadro (1.0 com ele desligado).
    fn agc_target(&mut self, p: &AudioPrefs, db: f32, user: f32, speaking: bool) -> f32 {
        if !p.agc {
            self.agc_db = 0.0;
            return 1.0;
        }
        // Só aprende com voz: em silêncio mantém o ganho, para o ruído de fundo não ser reforçado.
        if speaking && db > AGC_FLOOR_DB {
            let user_db = 20.0 * user.max(1e-3).log10();
            let want = (AGC_TARGET_DB - (db + user_db)).clamp(AGC_MAX_CUT_DB, AGC_MAX_BOOST_DB);
            // Desce rápido (voz alta), sobe devagar (voz baixa).
            let k = if want < self.agc_db { 0.3 } else { 0.03 };
            self.agc_db += (want - self.agc_db) * k;
        }
        10f32.powf(self.agc_db / 20.0)
    }
}

/// Comprime picos acima de `LIMIT_KNEE` suavemente, sem passar do fundo de escala.
fn limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= LIMIT_KNEE {
        return x;
    }
    let room = 32767.0 - LIMIT_KNEE;
    x.signum() * (LIMIT_KNEE + room * ((a - LIMIT_KNEE) / room).tanh())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 320;

    fn sine(amp: f32, hz: f32, n0: usize, len: usize) -> Vec<i16> {
        (0..len).map(|i| (amp * (std::f32::consts::TAU * hz * (n0 + i) as f32 / RATE).sin()) as i16).collect()
    }

    fn off() -> AudioPrefs {
        AudioPrefs { high_pass: false, gate: false, agc: false, ..AudioPrefs::default() }
    }

    /// Passa `frames` quadros de uma senóide e devolve o último quadro tratado e a última medida.
    fn run(v: &mut Voice, p: &AudioPrefs, amp: f32, hz: f32, frames: usize) -> (Vec<i16>, Measure) {
        let mut last = (Vec::new(), Measure { level: 0.0, speaking: false });
        for n in 0..frames {
            let mut f = sine(amp, hz, n * FRAME, FRAME);
            let m = v.process(&mut f, p);
            last = (f, m);
        }
        last
    }

    fn db_of(f: &[i16]) -> f32 {
        rms_db(&f.iter().map(|s| *s as f32).collect::<Vec<_>>())
    }

    #[test]
    fn everything_off_only_applies_the_microphone_gain() {
        let mut v = Voice::default();
        let p = AudioPrefs { mic_gain: 0.5, ..off() };
        let mut f = vec![4000i16; FRAME];
        v.process(&mut f, &p);
        assert!(f.iter().all(|x| *x == 2000));
    }

    #[test]
    fn high_pass_removes_rumble_and_keeps_speech() {
        let p = AudioPrefs { high_pass: true, ..off() };
        let rumble = run(&mut Voice::default(), &p, 8000.0, 30.0, 30).0;
        let voice = run(&mut Voice::default(), &p, 8000.0, 1000.0, 30).0;
        let raw = db_of(&sine(8000.0, 1000.0, 0, FRAME));
        assert!(db_of(&rumble) < raw - 15.0, "30 Hz deveria cair bastante: {}", db_of(&rumble));
        assert!((db_of(&voice) - raw).abs() < 1.0, "1 kHz deveria passar: {}", db_of(&voice));
    }

    #[test]
    fn gate_silences_noise_below_the_threshold_and_passes_speech() {
        let p = AudioPrefs { gate: true, gate_db: -40.0, gate_depth_db: 60.0, ..off() };
        let mut v = Voice::default();
        // Ruído de ~-52 dBFS.
        let (quiet, m) = run(&mut v, &p, 120.0, 500.0, 30);
        assert!(!m.speaking);
        assert!(quiet.iter().all(|x| x.abs() <= 1), "o ruído deveria sumir");
        // Voz de ~-20 dBFS.
        let (loud, m) = run(&mut v, &p, 4600.0, 500.0, 5);
        assert!(m.speaking);
        assert!((db_of(&loud) - db_of(&sine(4600.0, 500.0, 0, FRAME))).abs() < 1.0);
    }

    #[test]
    fn gate_holds_after_the_voice_stops_then_closes() {
        let p = AudioPrefs { gate: true, gate_db: -40.0, gate_depth_db: 60.0, ..off() };
        let mut v = Voice::default();
        run(&mut v, &p, 4600.0, 500.0, 5);
        // 20 ms depois do fim da voz a porta ainda segura (250 ms de espera)…
        assert!(run(&mut v, &p, 100.0, 500.0, 1).1.speaking);
        // …e meio segundo depois fechou.
        assert!(!run(&mut v, &p, 100.0, 500.0, 25).1.speaking);
    }

    #[test]
    fn a_lower_threshold_is_more_sensitive() {
        let soft = |db: f32| {
            let p = AudioPrefs { gate: true, gate_db: db, ..off() };
            run(&mut Voice::default(), &p, 300.0, 500.0, 10).1.speaking
        };
        assert!(!soft(-30.0));
        assert!(soft(-55.0));
    }

    #[test]
    fn the_level_is_measured_before_gate_and_gain() {
        let p = AudioPrefs { gate: true, gate_db: -10.0, gate_depth_db: 60.0, mic_gain: 0.1, ..off() };
        let (out, m) = run(&mut Voice::default(), &p, 4000.0, 500.0, 5);
        assert!(m.level > 0.5, "o medidor mostra o microfone mesmo com a porta fechada");
        assert!(out.iter().all(|x| x.abs() <= 1));
    }

    #[test]
    fn auto_volume_raises_quiet_voices_and_lowers_loud_ones() {
        let p = AudioPrefs { agc: true, ..off() };
        let target = AGC_TARGET_DB;
        let quiet = run(&mut Voice::default(), &p, 600.0, 500.0, 400).0;
        let loud = run(&mut Voice::default(), &p, 20000.0, 500.0, 400).0;
        assert!(db_of(&quiet) > db_of(&sine(600.0, 500.0, 0, FRAME)) + 10.0);
        assert!((db_of(&quiet) - target).abs() < 3.0, "{}", db_of(&quiet));
        assert!(db_of(&loud) < db_of(&sine(20000.0, 500.0, 0, FRAME)) - 5.0);
    }

    #[test]
    fn auto_volume_does_not_boost_background_noise() {
        let p = AudioPrefs { agc: true, ..off() };
        let before = db_of(&sine(20.0, 500.0, 0, FRAME));
        let (out, _) = run(&mut Voice::default(), &p, 20.0, 500.0, 200);
        assert!((db_of(&out) - before).abs() < 1.0);
    }

    #[test]
    fn the_limiter_never_passes_full_scale() {
        for x in [0.0, 20_000.0, 30_000.0, 100_000.0, -100_000.0] {
            assert!(limit(x).abs() <= 32767.0, "{x}");
        }
        assert_eq!(limit(1234.0), 1234.0);
        assert!(limit(30_000.0) > 24_000.0 && limit(30_000.0) < 30_000.0);
    }

    #[test]
    fn turning_a_feature_off_forgets_its_state() {
        let mut v = Voice::default();
        run(&mut v, &AudioPrefs { gate: true, agc: true, ..off() }, 8000.0, 500.0, 20);
        v.process(&mut vec![0i16; FRAME], &off());
        assert_eq!((v.agc_db, v.gate_gain, v.hold), (0.0, None, 0));
    }
}
