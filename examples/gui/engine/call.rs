//! Conversa 1:1: laço de áudio, queda de rede e religação.

use super::*;

/// Envia um tom (ou o microfone) a cada 20 ms e relata, por segundo, o que chegou. Termina ao desligar
/// (`stop`) ou quando o outro lado desliga.
/// Sem receber nada por tanto tempo, a ligação é dada como perdida (a voz segue em 50 quadros/s, então silêncio é queda).
const LOST_AFTER: Duration = Duration::from_secs(8);

/// Para onde pedir a religação e para quem.
pub(crate) type Redial = (mpsc::UnboundedSender<([u8; 32], u32)>, [u8; 32]);

/// Tentativas de religar depois de uma queda.
pub(crate) const REDIAL_TRIES: u32 = 5;

/// Vigia o silêncio da linha: a voz segue em 50 quadros/s, então um tempo longo sem receber nada é queda.
pub(crate) struct Silence {
    last_rx: tokio::time::Instant,
}

impl Silence {
    pub(crate) fn new(now: tokio::time::Instant) -> Self {
        Silence { last_rx: now }
    }

    /// Chamada uma vez por segundo; `grew` diz se chegou algo desde a última. `true` se a ligação foi dada como perdida.
    pub(crate) fn lost(&mut self, grew: bool, now: tokio::time::Instant) -> bool {
        if grew {
            self.last_rx = now;
            false
        } else {
            now.duration_since(self.last_rx) > LOST_AFTER
        }
    }
}

/// O que fazer com o resultado de uma discagem.
#[derive(Debug, PartialEq)]
pub(crate) enum DialStep {
    /// Atendeu: começar a conversa.
    Talk,
    /// Religação falhou e ainda há tentativas: tentar de novo.
    Retry,
    /// Desistir e liberar a linha.
    GiveUp,
    /// A pessoa cancelou.
    Cancelled,
}

/// `attempt` é 0 na primeira discagem e 1.. nas religações; `answered` é `None` se foi cancelada.
pub(crate) fn dial_step(attempt: u32, answered: Option<bool>) -> DialStep {
    match answered {
        None => DialStep::Cancelled,
        Some(true) => DialStep::Talk,
        Some(false) if attempt > 0 && attempt < REDIAL_TRIES => DialStep::Retry,
        Some(false) => DialStep::GiveUp,
    }
}

/// Espera `fut`, mas devolve `None` assim que `hangup` for ligado.
pub(crate) async fn until_hangup<T>(fut: impl std::future::Future<Output = T>, hangup: &AtomicBool) -> Option<T> {
    tokio::select! {
        r = fut => Some(r),
        _ = async {
            while !hangup.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } => None,
    }
}

/// Resumo da qualidade da linha no último segundo, para a janela.
pub(crate) fn quality(received: u64, concealed: u64, delay_ms: u64) -> String {
    let total = received + concealed;
    if received == 0 {
        return "sem sinal".into();
    }
    let loss = concealed * 100 / total;
    let label = if loss < 2 && delay_ms < 150 {
        "boa"
    } else if loss < 10 && delay_ms < 300 {
        "regular"
    } else {
        "ruim"
    };
    format!("{label} ({loss}% perdidos, atraso {delay_ms} ms)")
}

/// Como a conversa terminou.
#[derive(PartialEq)]
pub(crate) enum End {
    /// Quem está na janela desligou.
    Hangup,
    /// O outro lado desligou.
    PeerHangup,
    /// A rede caiu ou mudou: vale religar.
    Lost,
}

/// Com quem se fala e como abrir o circuito de tela.
pub(crate) struct Peer {
    pub(crate) key: [u8; 32],
    pub(crate) dialer: clam::node::Dialer,
}

pub(crate) async fn talk(mut c: ActiveCall, hz: f64, stop: Arc<AtomicBool>, out: Sink, sound: Option<Sound>, shared: &Arc<Shared>, peer: &Peer) -> End {
    let mut decoder = None;
    let mut screen_rx = c.take_screen();
    let mut screen_open = true;
    let mut silence = Silence::new(tokio::time::Instant::now());
    let mut end = End::Hangup;
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut report = interval(Duration::from_secs(1));
    let (mut n, mut frames, mut energy) = (0usize, 0u64, 0f64);
    let (mut sent_n, mut sent_energy) = (0u64, 0f64);
    let mut prev = c.stats();
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if stop.load(Ordering::Relaxed) { break; }
                let mut closed = false;
                for f in mic_frames(sound.as_ref(), hz, n, shared) {
                    sent_n += 1;
                    sent_energy += f.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / f.len() as f64;
                    closed |= c.send(f).await.is_err();
                }
                if closed {
                    end = if c.peer_hung_up() { End::PeerHangup } else { End::Lost };
                    break;
                }
                n += 1;
            }
            f = c.next_pcm() => match f {
                Some(f) => {
                    frames += 1;
                    energy += f.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / f.len() as f64;
                    play_scaled(sound.as_ref(), f, shared);
                }
                None => {
                    end = if c.peer_hung_up() { End::PeerHangup } else { End::Lost };
                    break;
                }
            },
            ev = screen_rx.recv(), if screen_open => match ev {
                Some(ev) => {
                    if !on_screen_event(&shared.screen, &mut decoder, ev) {
                        out.line("tela: quadro inválido; esperando o próximo quadro-chave");
                    }
                }
                None => screen_open = false,
            },
            _ = report.tick() => {
                report_error(sound.as_ref(), &out);
                if shared.screen.want_share.load(Ordering::Relaxed) && !shared.screen.share_task.swap(true, Ordering::Relaxed) {
                    tokio::spawn(share_task(peer.dialer.clone(), peer.key, c.share_keys().clone(), shared.clone(), out.clone()));
                }
                let s = c.stats();
                if silence.lost(s.received > prev.received, tokio::time::Instant::now()) {
                    out.line("nada chegou há 8 s: a rede caiu ou mudou");
                    end = End::Lost;
                    break;
                }
                *shared.link.lock().unwrap() = quality(s.received - prev.received, s.concealed - prev.concealed, s.delay_ms);
                let rms = if frames > 0 { (energy / frames as f64).sqrt() } else { 0.0 };
                out.line(format!(
                    "recebidos +{:<3} (total {})  escondidos +{}  atraso {} ms  volume {:.0}  mic {:.0}{}",
                    s.received - prev.received, s.received, s.concealed - prev.concealed, s.delay_ms, rms, if sent_n > 0 { (sent_energy / sent_n as f64).sqrt() } else { 0.0 },
                    sound.as_ref().map(|s| format!("  tocadas {}", s.played())).unwrap_or_default()
                ));
                prev = s;
                (frames, energy, sent_n, sent_energy) = (0, 0.0, 0, 0.0);
            }
        }
    }
    shared.link.lock().unwrap().clear();
    shared.screen.reset();
    shared.audio.set_level(0.0);
    let s = c.stats();
    out.line(format!("chamada encerrada: enviados {}, recebidos {}, escondidos {}, atrasados {}", s.sent, s.received, s.concealed, s.late));
    c.hangup();
    tokio::time::sleep(Duration::from_millis(500)).await;
    end
}

/// Inicia a conversa numa tarefa própria; `hangup` (da janela) encerra.
/// `redial`: se a conexão cair, pede para religar a esse contato (só quem discou religa).
pub(crate) fn start_talk(c: ActiveCall, hz: f64, shared: &Arc<Shared>, out: &Sink, form: &Form, redial: Option<Redial>, peer: Peer) {
    let sound = open_sound(form.use_audio, form, out);
    let (shared, out) = (shared.clone(), out.clone());
    // `talk` olha este sinalizador local; um vigia o liga quando a janela pede para desligar.
    let stop = Arc::new(AtomicBool::new(false));
    let (watch, sh) = (stop.clone(), shared.clone());
    tokio::spawn(async move {
        while !watch.load(Ordering::Relaxed) {
            if sh.hangup.load(Ordering::Relaxed) {
                watch.store(true, Ordering::Relaxed);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    tokio::spawn(async move {
        let end = talk(c, hz, stop.clone(), out.clone(), sound, &shared, &peer).await;
        stop.store(true, Ordering::Relaxed);
        if end == End::Lost
            && !shared.hangup.load(Ordering::Relaxed)
            && let Some((tx, peer)) = redial
        {
            // `in_call` fica ligado: nada mais entra na linha enquanto tenta religar.
            out.line("conexão perdida; religando...");
            let _ = tx.send((peer, 1));
            return;
        }
        if end == End::Lost && !shared.hangup.load(Ordering::Relaxed) {
            out.line("conexão perdida; aguardando o outro lado religar");
        }
        shared.in_call.store(false, Ordering::Relaxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_declares_loss_only_after_the_limit() {
        let t0 = tokio::time::Instant::now();
        let mut s = Silence::new(t0);
        assert!(!s.lost(false, t0 + LOST_AFTER));
        assert!(s.lost(false, t0 + LOST_AFTER + Duration::from_secs(1)));
        // Chegou algo: a contagem recomeça.
        assert!(!s.lost(true, t0 + Duration::from_secs(9)));
        assert!(!s.lost(false, t0 + Duration::from_secs(9) + LOST_AFTER));
        assert!(s.lost(false, t0 + Duration::from_secs(10) + LOST_AFTER));
    }

    #[test]
    fn dial_step_retries_only_redials_within_the_limit() {
        assert_eq!(dial_step(0, Some(true)), DialStep::Talk);
        assert_eq!(dial_step(3, Some(true)), DialStep::Talk);
        assert_eq!(dial_step(0, Some(false)), DialStep::GiveUp);
        assert_eq!(dial_step(1, Some(false)), DialStep::Retry);
        assert_eq!(dial_step(REDIAL_TRIES - 1, Some(false)), DialStep::Retry);
        assert_eq!(dial_step(REDIAL_TRIES, Some(false)), DialStep::GiveUp);
        assert_eq!(dial_step(2, None), DialStep::Cancelled);
    }

    #[tokio::test]
    async fn hangup_cancels_a_pending_dial() {
        let hangup = Arc::new(AtomicBool::new(false));
        let h = hangup.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            h.store(true, Ordering::Relaxed);
        });
        let r = tokio::time::timeout(Duration::from_secs(5), until_hangup(std::future::pending::<u8>(), &hangup)).await;
        assert_eq!(r, Ok(None));
        // Sem pedido de desligar, o resultado passa.
        assert_eq!(until_hangup(async { 7 }, &AtomicBool::new(false)).await, Some(7));
    }

    #[test]
    fn quality_grades_loss_and_delay() {
        assert_eq!(quality(0, 0, 0), "sem sinal");
        assert!(quality(50, 0, 40).starts_with("boa"));
        assert!(quality(47, 3, 40).starts_with("regular"));
        assert!(quality(50, 0, 250).starts_with("regular"));
        assert!(quality(30, 20, 40).starts_with("ruim"));
    }
}
