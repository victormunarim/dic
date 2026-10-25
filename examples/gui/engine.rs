//! Motor: nó, chamadas e salas. Não conhece a janela; só recebe comandos e escreve no log.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use clam::media::codec::G722;
use clam::node::{Node, NodeConfig};
use clam::phone::ActiveCall;
use clam::room::{self, Mesh, RoomView};
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval},
};

use clam::x25519_dalek::StaticSecret;

use crate::conf::*;

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
    fn set_level(&self, v: f32) {
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

pub(crate) type Log = Arc<Mutex<Vec<String>>>;

/// Escreve uma linha no log da janela e pede repintura.
#[derive(Clone)]
pub(crate) struct Sink {
    pub(crate) log: Log,
    /// Pede repintura da janela (vazio nos testes).
    pub(crate) repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Sink {
    pub(crate) fn line(&self, s: impl Into<String>) {
        let mut l = self.log.lock().unwrap();
        l.push(s.into());
        if l.len() > 500 {
            l.drain(..100);
        }
        drop(l);
        (self.repaint)();
    }
}

/// Envia um tom (ou o microfone) a cada 20 ms e relata, por segundo, o que chegou. Termina ao desligar
/// (`stop`) ou quando o outro lado desliga.
/// Sem receber nada por tanto tempo, a ligação é dada como perdida (a voz segue em 50 quadros/s, então silêncio é queda).
const LOST_AFTER: Duration = Duration::from_secs(8);

/// Para onde pedir a religação e para quem.
pub(crate) type Redial = (mpsc::UnboundedSender<([u8; 32], u32)>, [u8; 32]);

/// Tentativas de religar depois de uma queda.
const REDIAL_TRIES: u32 = 5;

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

pub(crate) async fn talk(mut c: ActiveCall, hz: f64, stop: Arc<AtomicBool>, out: Sink, sound: Option<Sound>, shared: &Shared) -> End {
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
                // Com microfone: manda o que já foi capturado (até 2 quadros, para absorver atraso do relógio).
                let sent: Vec<Vec<i16>> = match &sound {
                    Some(s) => std::iter::from_fn(|| s.mic_frame()).take(2).collect(),
                    None => vec![tone(hz, n)],
                };
                let mut closed = false;
                for mut f in sent {
                    shared.audio.outgoing(&mut f);
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
                Some(mut f) => {
                    frames += 1;
                    energy += f.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / f.len() as f64;
                    if let Some(s) = &sound {
                        scale_pcm(&mut f, shared.audio.volume());
                        s.play(&f);
                    }
                }
                None => {
                    end = if c.peer_hung_up() { End::PeerHangup } else { End::Lost };
                    break;
                }
            },
            _ = report.tick() => {
                if let Some(e) = sound.as_ref().and_then(Sound::take_error) {
                    out.line(format!("áudio: {e}"));
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
    shared.audio.set_level(0.0);
    let s = c.stats();
    out.line(format!("chamada encerrada: enviados {}, recebidos {}, escondidos {}, atrasados {}", s.sent, s.received, s.concealed, s.late));
    c.hangup();
    tokio::time::sleep(Duration::from_millis(500)).await;
    end
}

pub(crate) enum Cmd {
    Dial([u8; 32]),
    /// Segredo, dono da sala (se conhecido) e nome.
    Join([u8; 32], Option<[u8; 32]>, String),
    /// O dono tira um membro: troca o segredo e avisa só quem fica.
    Remove([u8; 32]),
    Leave,
    /// Liga o microfone ao alto-falante para a pessoa conferir som e nível.
    MicTest,
}

/// Estado compartilhado entre a janela e a tarefa do nó.
#[derive(Default)]
pub(crate) struct Shared {
    /// Há uma chamada em andamento (entrando ou saindo).
    pub(crate) in_call: AtomicBool,
    /// Pedido de desligar a chamada atual.
    pub(crate) hangup: AtomicBool,
    /// Qualidade da linha na chamada atual (vazio fora de chamada).
    pub(crate) link: Mutex<String>,
    /// Mudo, volume, ganho e nível do microfone.
    pub(crate) audio: Controls,
    /// Teste do microfone em andamento (a pessoa se ouve); desligar este sinalizador encerra o teste.
    pub(crate) mic_test: AtomicBool,
    /// Texto de estado do nó ("online em ...").
    pub(crate) status: Mutex<String>,
    /// Está numa sala (as chamadas da sala não usam `in_call`, mas ele fica ligado).
    pub(crate) in_room: AtomicBool,
    /// Resumo da sala atual.
    pub(crate) room: Mutex<String>,
    /// Sala atual: a malha (volume e quem fala) e os membros vivos, você incluído.
    pub(crate) mesh: Mutex<Option<Mesh>>,
    pub(crate) members: Mutex<Vec<[u8; 32]>>,
    pub(crate) me: Mutex<[u8; 32]>,
    /// Sou o dono da sala atual (posso remover membros).
    pub(crate) owner: AtomicBool,
    /// Segredos de sala trocados pelo dono, `(antigo, novo)`, para a janela atualizar o que está salvo.
    pub(crate) rekeyed: Mutex<Vec<([u8; 32], [u8; 32])>>,
}

/// Roda o nó até `stop`: atende contatos sozinho e disca quando a janela pede.
pub(crate) async fn run_node(form: Form, mut rx: mpsc::UnboundedReceiver<Cmd>, shared: Arc<Shared>, stop: Arc<AtomicBool>, out: Sink) -> Result<(), String> {
    let secret = form.identity()?;
    let ident = secret.clone();
    if form.directory_addr().is_empty() {
        return Err("informe o diretório (host:porta), ou cole um código de contato com @host:porta".into());
    }
    // Só contatos são atendidos sozinhos; apelidos servem apenas para exibir nomes.
    let contacts: Vec<_> = parse_contacts(&form.contacts);
    let names = form.names();
    let mut cfg = NodeConfig::new(secret, form.directory_addr(), contacts.iter().map(|(_, k)| *k).collect());
    cfg.known = load_known();
    if !form.public_addr.trim().is_empty() {
        cfg.public_addr = Some(form.public_addr.trim().to_string());
    }
    out.line("conectando ao diretório...");
    let mut node = Node::start(cfg).await.map_err(|e| format!("não ficou online: {e}"))?;
    *shared.status.lock().unwrap() = format!("online em {}", node.published_addr());
    out.line(format!("online; os outros chegam em {}", node.published_addr()));
    if let Some(n) = node.network_note() {
        out.line(n);
    }
    let dialer = node.dialer();
    let (dialed_tx, mut dialed_rx) = mpsc::unbounded_channel::<([u8; 32], u32, Option<Result<ActiveCall, clam::node::DialError>>)>();
    let (redial_tx, mut redial_rx) = mpsc::unbounded_channel::<([u8; 32], u32)>();
    // Discar demora (furo de NAT, endereço guardado, espera de resposta): numa tarefa, para a janela seguir
    // atendendo comandos; "Desligar" cancela.
    let spawn_dial = |peer: [u8; 32], attempt: u32, delay: Duration| {
        let (dialer, sh, tx) = (dialer.clone(), shared.clone(), dialed_tx.clone());
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let r = until_hangup(dialer.dial(peer, G722::new(), Duration::from_secs(15)), &sh.hangup).await;
            let _ = tx.send((peer, attempt, r));
        });
    };
    let mut room: Option<RoomRun> = None;
    let mut room_tick = interval(Duration::from_millis(20));
    room_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            inc = node.next_incoming() => {
                let Some(inc) = inc else { break };
                if let Some(r) = &room
                    && let Some(view) = &r.view
                    && view.members.contains(inc.caller().as_bytes())
                {
                    if room::accept_member(inc, view, &r.mesh).await {
                        out.line("alguém da sala entrou na chamada");
                    }
                    continue;
                }
                let name = member_name(&names, inc.caller().as_bytes());
                if shared.in_call.swap(true, Ordering::Relaxed) {
                    out.line(format!("{name} ligou, mas já há uma chamada; recusada"));
                    inc.reject().await;
                    continue;
                }
                out.line(format!("chamada de {name}"));
                shared.hangup.store(false, Ordering::Relaxed);
                match inc.accept(G722::new()).await {
                    Ok(c) => start_talk(c, 660.0, &shared, &out, &form, None),
                    Err(e) => {
                        out.line(format!("não atendeu: {e}"));
                        shared.in_call.store(false, Ordering::Relaxed);
                    }
                }
            }
            Some((peer, attempt, r)) = dialed_rx.recv() => {
                save_known(&node.known());
                match (dial_step(attempt, r.as_ref().map(|r| r.is_ok())), r) {
                    (DialStep::Talk, Some(Ok(c))) => {
                        out.line(if attempt > 0 { "religada" } else { "atendida" });
                        start_talk(c, 440.0, &shared, &out, &form, Some((redial_tx.clone(), peer)));
                    }
                    (DialStep::Retry, Some(Err(e))) => {
                        out.line(format!("não religou ({attempt}/{REDIAL_TRIES}): {e}"));
                        let tx = redial_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            let _ = tx.send((peer, attempt + 1));
                        });
                    }
                    (_, Some(Err(e))) => {
                        out.line(format!("não completou: {e}"));
                        shared.in_call.store(false, Ordering::Relaxed);
                    }
                    _ => {
                        out.line("ligação cancelada");
                        shared.in_call.store(false, Ordering::Relaxed);
                    }
                }
            }
            Some((peer, attempt)) = redial_rx.recv() => {
                if shared.hangup.load(Ordering::Relaxed) {
                    out.line("ligação cancelada");
                    shared.in_call.store(false, Ordering::Relaxed);
                } else {
                    out.line(format!("religando ({attempt}/{REDIAL_TRIES})..."));
                    spawn_dial(peer, attempt, Duration::ZERO);
                }
            }
            cmd = rx.recv() => match cmd {
                Some(Cmd::Dial(peer)) => {
                    if shared.in_call.swap(true, Ordering::Relaxed) {
                        out.line("já há uma chamada em andamento");
                        continue;
                    }
                    shared.hangup.store(false, Ordering::Relaxed);
                    out.line("ligando...");
                    // Discar demora (furo de NAT, endereço guardado, espera de resposta): numa tarefa, para a
                    // janela seguir atendendo comandos; "Desligar" cancela a ligação.
                    spawn_dial(peer, 0, Duration::ZERO);
                }
                Some(Cmd::Join(mut secret, owner, name)) => {
                    if shared.in_call.swap(true, Ordering::Relaxed) {
                        out.line("saia da chamada atual antes de entrar numa sala");
                        continue;
                    }
                    out.line(format!("entrando na sala {name}..."));
                    let me = node.public_key();
                    // Se o dono já trocou o segredo (alguém foi removido), segue a troca antes de entrar.
                    if let Some(o) = owner.filter(|o| *o != me) {
                        let new = follow_chain(node.directory(), &ident, &o, secret, &shared).await;
                        if new != secret {
                            out.line(format!("sala {name}: o dono trocou o segredo; usando o novo"));
                            secret = new;
                        }
                    }
                    shared.owner.store(owner == Some(me), Ordering::Relaxed);
                    shared.in_room.store(true, Ordering::Relaxed);
                    let mesh = Mesh::new();
                    *shared.mesh.lock().unwrap() = Some(mesh.clone());
                    *shared.me.lock().unwrap() = node.public_key();
                    room = Some(RoomRun {
                        name,
                        secret,
                        owner,
                        key: room::room_key(&secret),
                        mesh,
                        view: None,
                        sound: open_sound(form.use_audio, &form, &out),
                        next_sync: tokio::time::Instant::now(),
                        next_dial: tokio::time::Instant::now(),
                        shown: (0, 0, [0; 32]),
                        n: 0,
                    });
                }
                Some(Cmd::Remove(gone)) => {
                    let Some(r) = room.as_mut() else { continue };
                    let (Some(view), true) = (r.view.as_ref(), r.owner == Some(node.public_key())) else {
                        out.line("só o dono da sala remove membros");
                        continue;
                    };
                    let keep: Vec<[u8; 32]> = view.members.iter().copied().filter(|m| *m != gone && *m != node.public_key()).collect();
                    match room::rotate(node.directory(), &ident, &r.secret, &keep).await {
                        Ok(new) => {
                            shared.rekeyed.lock().unwrap().push((r.secret, new));
                            r.secret = new;
                            r.key = room::room_key(&new);
                            r.mesh.retain(&keep);
                            r.view = None;
                            r.next_sync = tokio::time::Instant::now();
                            out.line(format!("{} foi removido da sala {}; o segredo mudou e só quem ficou recebe o novo", member_name(&names, &gone), r.name));
                        }
                        Err(e) => out.line(format!("não deu para remover: {e}")),
                    }
                }
                Some(Cmd::MicTest) => {
                    if shared.in_call.swap(true, Ordering::Relaxed) {
                        out.line("encerre a chamada antes de testar o microfone");
                        continue;
                    }
                    match open_sound(true, &form, &out) {
                        Some(sound) => {
                            shared.mic_test.store(true, Ordering::Relaxed);
                            out.line("teste do microfone: você se ouve com um pequeno atraso; use fones para não dar eco");
                            tokio::spawn(mic_test(sound, shared.clone(), stop.clone(), out.clone()));
                        }
                        None => shared.in_call.store(false, Ordering::Relaxed),
                    }
                }
                Some(Cmd::Leave) => {
                    if let Some(r) = room.take() {
                        leave_room(&node, r, &shared, &out).await;
                    }
                }
                None => break,
            },
            _ = room_tick.tick(), if room.is_some() => {
                let Some(r) = room.as_mut() else { continue };
                let frames: Vec<Vec<i16>> = match &r.sound {
                    Some(s) => std::iter::from_fn(|| s.mic_frame()).take(2).collect(),
                    None => vec![tone(440.0, r.n)],
                };
                r.n += 1;
                if r.n.is_multiple_of(50)
                    && let Some(e) = r.sound.as_ref().and_then(Sound::take_error)
                {
                    out.line(format!("áudio: {e}"));
                }
                for mut f in frames {
                    shared.audio.outgoing(&mut f);
                    r.mesh.send(&f);
                }
                if let Some(mut f) = r.mesh.mix()
                    && let Some(s) = &r.sound
                {
                    scale_pcm(&mut f, shared.audio.volume());
                    s.play(&f);
                }
                // Discagem em segundo plano e em paralelo; repete a cada 2 s, o que religa sozinho quem caiu.
                if tokio::time::Instant::now() >= r.next_dial {
                    r.next_dial = tokio::time::Instant::now() + Duration::from_secs(2);
                    if let Some(view) = &r.view {
                        room::connect_missing(&dialer, view, &r.mesh, Duration::from_secs(8));
                    }
                }
                if tokio::time::Instant::now() >= r.next_sync {
                    r.next_sync = tokio::time::Instant::now() + Duration::from_secs(10);
                    // Membro: o dono pode ter trocado o segredo para tirar alguém; passa para o novo.
                    if let Some(o) = r.owner.filter(|o| *o != node.public_key()) {
                        let new = follow_chain(node.directory(), &ident, &o, r.secret, &shared).await;
                        if new != r.secret {
                            out.line(format!("sala {}: o dono trocou o segredo; acompanhando", r.name));
                            r.secret = new;
                            r.key = room::room_key(&new);
                        }
                    }
                    match room::sync(&node, &r.key).await {
                        Ok(view) => {
                            room::connect_missing(&dialer, &view, &r.mesh, Duration::from_secs(8));
                            let shown = (view.members.len(), r.mesh.count(), view.host);
                            if shown != r.shown {
                                r.shown = shown;
                                let host = if view.i_am_host { "você".to_string() } else { member_name(&names, &view.host) };
                                let text = format!("sala {}: {} pessoa(s), {} conectada(s); anfitrião: {host}", r.name, shown.0, shown.1);
                                out.line(text.clone());
                                *shared.room.lock().unwrap() = text;
                            }
                            // Quem saiu da lista (removido pelo dono) perde também a chamada.
                            r.mesh.retain(&view.members);
                            *shared.members.lock().unwrap() = view.members.clone();
                            r.view = Some(view);
                        }
                        Err(e) => out.line(format!("sala {}: diretório: {e}", r.name)),
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        if let Some(n) = node.take_notice() {
            out.line(n);
        }
    }
    if let Some(r) = room.take() {
        leave_room(&node, r, &shared, &out).await;
    }
    shared.hangup.store(true, Ordering::Relaxed);
    Ok(())
}

/// Sala em andamento na tarefa do nó.
pub(crate) struct RoomRun {
    pub(crate) name: String,
    pub(crate) secret: [u8; 32],
    pub(crate) owner: Option<[u8; 32]>,
    pub(crate) key: clam::ed25519_dalek::SigningKey,
    pub(crate) mesh: Mesh,
    pub(crate) view: Option<RoomView>,
    pub(crate) sound: Option<Sound>,
    pub(crate) next_sync: tokio::time::Instant,
    pub(crate) next_dial: tokio::time::Instant,
    /// O que o último resumo mostrou (membros, conectados, anfitrião), para só repetir quando mudar.
    pub(crate) shown: (usize, usize, [u8; 32]),
    pub(crate) n: usize,
}

/// Segue as trocas de segredo que o dono `owner` deixou no diretório, até não haver mais (no máximo 8).
/// Cada troca vai para `shared.rekeyed`, para a janela salvar o segredo novo.
/// Devolve o microfone ao alto-falante até a janela encerrar o teste (`mic_test`) ou o nó parar.
async fn mic_test(sound: Sound, shared: Arc<Shared>, stop: Arc<AtomicBool>, out: Sink) {
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut n = 0u32;
    while shared.mic_test.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        while let Some(mut f) = sound.mic_frame() {
            shared.audio.outgoing(&mut f);
            scale_pcm(&mut f, shared.audio.volume());
            sound.play(&f);
        }
        n += 1;
        if n.is_multiple_of(50)
            && let Some(e) = sound.take_error()
        {
            out.line(format!("áudio: {e}"));
        }
    }
    shared.audio.set_level(0.0);
    shared.mic_test.store(false, Ordering::Relaxed);
    shared.in_call.store(false, Ordering::Relaxed);
    out.line("teste do microfone encerrado");
}

pub(crate) async fn follow_chain(directory: &str, me: &StaticSecret, owner: &[u8; 32], mut secret: [u8; 32], shared: &Shared) -> [u8; 32] {
    for _ in 0..8 {
        match room::follow(directory, me, owner, &secret).await {
            Ok(Some(new)) if new != secret => {
                shared.rekeyed.lock().unwrap().push((secret, new));
                secret = new;
            }
            _ => break,
        }
    }
    secret
}

pub(crate) fn member_name(contacts: &[(String, [u8; 32])], key: &[u8; 32]) -> String {
    contacts.iter().find(|(_, k)| k == key).map(|(n, _)| n.clone()).unwrap_or_else(|| hex(key)[..8].to_string())
}

pub(crate) async fn leave_room(node: &Node, r: RoomRun, shared: &Shared, out: &Sink) {
    r.mesh.hangup();
    let _ = room::leave(node, &r.key).await;
    out.line(format!("saiu da sala {}", r.name));
    shared.room.lock().unwrap().clear();
    shared.members.lock().unwrap().clear();
    *shared.mesh.lock().unwrap() = None;
    shared.in_room.store(false, Ordering::Relaxed);
    shared.in_call.store(false, Ordering::Relaxed);
}

/// Inicia a conversa numa tarefa própria; `hangup` (da janela) encerra.
/// `redial`: se a conexão cair, pede para religar a esse contato (só quem discou religa).
pub(crate) fn start_talk(c: ActiveCall, hz: f64, shared: &Arc<Shared>, out: &Sink, form: &Form, redial: Option<Redial>) {
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
        let end = talk(c, hz, stop.clone(), out.clone(), sound, &shared).await;
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

pub(crate) struct Running {
    pub(crate) cmds: mpsc::UnboundedSender<Cmd>,
    pub(crate) stop: Arc<AtomicBool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> Sink {
        Sink { log: Arc::default(), repaint: Arc::new(|| {}) }
    }

    #[test]
    fn log_keeps_the_latest_lines() {
        let s = sink();
        for i in 0..650 {
            s.line(format!("linha {i}"));
        }
        let l = s.log.lock().unwrap();
        assert!(l.len() <= 500);
        assert_eq!(l.last().unwrap(), "linha 649");
    }

    #[tokio::test]
    async fn follow_chain_walks_every_rotation_and_skips_removed_members() {
        use clam::{directory::DirectoryServer, rand_core::OsRng, x25519_dalek::PublicKey};
        let d = DirectoryServer::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = d.local_addr().to_string();
        let (owner, bob, eve) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
        let (opub, bpub, epub) = (PublicKey::from(&owner).to_bytes(), PublicKey::from(&bob).to_bytes(), PublicKey::from(&eve).to_bytes());
        let first = room::new_secret();
        let second = room::rotate(&addr, &owner, &first, &[bpub, epub]).await.unwrap();
        let third = room::rotate(&addr, &owner, &second, &[bpub]).await.unwrap();
        let shared = Shared::default();
        assert_eq!(follow_chain(&addr, &bob, &opub, first, &shared).await, third);
        assert_eq!(shared.rekeyed.lock().unwrap().len(), 2);
        // Eve foi removida na segunda troca: fica presa no segredo do meio.
        assert_eq!(follow_chain(&addr, &eve, &opub, first, &Shared::default()).await, second);
    }

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

    #[test]
    fn quality_grades_loss_and_delay() {
        assert_eq!(quality(0, 0, 0), "sem sinal");
        assert!(quality(50, 0, 40).starts_with("boa"));
        assert!(quality(47, 3, 40).starts_with("regular"));
        assert!(quality(50, 0, 250).starts_with("regular"));
        assert!(quality(30, 20, 40).starts_with("ruim"));
    }

    #[test]
    fn member_name_prefers_contact_names() {
        let contacts = vec![("ana".to_string(), [1u8; 32])];
        assert_eq!(member_name(&contacts, &[1; 32]), "ana");
        assert_eq!(member_name(&contacts, &[2; 32]), "02020202");
    }
}
