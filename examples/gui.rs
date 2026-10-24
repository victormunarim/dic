//! Janela mínima para ligar e atender sem usar o terminal.
//! Roteiro no README, seção "Interface gráfica mínima".
//!
//! ```text
//! cargo run --release --features gui --example gui
//! ```
//! Sem `--features audio` (ou sem a opção marcada na janela), cada lado envia um tom
//! (440 Hz quem liga, 660 Hz quem atende). Com ela, usa o microfone e o alto-falante padrão:
//! `cargo run --release --features gui,audio --example gui`.
//! Os campos são gravados em `~/.clam-gui.conf` ao ligar/atender.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use clam::circuit::manager::{Candidate, CircuitManager, ManagerConfig};
use clam::media::codec::Mulaw;
use clam::phone::{ActiveCall, Phone, call};
use clam::transport::{CircuitClient, ClientOptions};
use clam::{
    rand_core::OsRng,
    x25519_dalek::{PublicKey, StaticSecret},
};
use eframe::egui;
use tokio::time::{Instant, MissedTickBehavior, interval};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<[u8; 32], String> {
    let s = s.trim();
    if s.len() != 64 || !s.is_ascii() {
        return Err(format!("chave deve ter 64 caracteres hex, veio {}", s.len()));
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn parse_candidate(spec: &str) -> Result<Candidate, String> {
    let mut p = spec.trim().split(',');
    let (Some(addr), Some(onion), Some(link)) = (p.next(), p.next(), p.next()) else {
        return Err(format!("relay '{spec}': esperado endereço,onion,link"));
    };
    Ok(Candidate { addr: addr.trim().into(), public: unhex(onion)?.into(), link_public: unhex(link)?, score: 1.0 })
}

#[cfg(feature = "audio")]
type Sound = clam::audio::Audio;

/// Sem a feature `audio` nenhum `Sound` existe: o tipo é vazio.
#[cfg(not(feature = "audio"))]
enum Sound {}

#[cfg(not(feature = "audio"))]
impl Sound {
    fn mic_frame(&self) -> Option<Vec<i16>> {
        match *self {}
    }
    fn play(&self, _: &[i16]) {
        match *self {}
    }
}

/// Abre microfone/alto-falante se pedido; sem eles, cai para o tom de teste.
fn open_sound(want: bool, out: &Sink) -> Option<Sound> {
    if !want {
        return None;
    }
    #[cfg(feature = "audio")]
    match Sound::open() {
        Ok(s) => {
            out.line("microfone e alto-falante abertos (use fones: não há cancelamento de eco)");
            return Some(s);
        }
        Err(e) => out.line(format!("{e}; usando tom de teste")),
    }
    #[cfg(not(feature = "audio"))]
    out.line("compilado sem a feature audio; usando tom de teste");
    None
}

fn tone(hz: f64, n: usize) -> Vec<i16> {
    (0..160).map(|i| (6000.0 * (std::f64::consts::TAU * hz * (n * 160 + i) as f64 / 8000.0).sin()) as i16).collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Dial,
    Answer,
}

#[derive(Default, Clone)]
struct Form {
    secret: String,
    peer: String,
    relay_a: String,
    relay_b: String,
    listen: String,
    contacts: String,
    seconds: String,
    use_audio: bool,
}

fn conf_path() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
    home.join(".clam-gui.conf")
}

impl Form {
    fn load() -> Self {
        let mut f = Form { listen: "0.0.0.0:7000".into(), seconds: "15".into(), ..Default::default() };
        let Ok(text) = std::fs::read_to_string(conf_path()) else { return f };
        let kv: HashMap<_, _> = text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let get = |k: &str, d: &str| kv.get(k).cloned().unwrap_or_else(|| d.to_string());
        f.secret = get("secret", "");
        f.peer = get("peer", "");
        f.relay_a = get("relay_a", "");
        f.relay_b = get("relay_b", "");
        f.listen = get("listen", &f.listen);
        f.contacts = get("contacts", "");
        f.seconds = get("seconds", &f.seconds);
        f.use_audio = get("audio", "0") == "1";
        f
    }

    fn save(&self) {
        let text = format!(
            "audio={}\nsecret={}\npeer={}\nrelay_a={}\nrelay_b={}\nlisten={}\ncontacts={}\nseconds={}\n",
            u8::from(self.use_audio),
            self.secret.trim(),
            self.peer.trim(),
            self.relay_a.trim(),
            self.relay_b.trim(),
            self.listen.trim(),
            self.contacts.replace('\n', ";").trim(),
            self.seconds.trim()
        );
        let path = conf_path();
        // Contém a chave secreta: no Unix, só o dono lê.
        if std::fs::write(&path, text).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }

    fn identity(&self) -> Result<StaticSecret, String> {
        unhex(&self.secret).map(StaticSecret::from).map_err(|e| format!("identidade: {e}"))
    }
}

type Log = Arc<Mutex<Vec<String>>>;

/// Escreve uma linha no log da janela e pede repintura.
#[derive(Clone)]
struct Sink {
    log: Log,
    ctx: egui::Context,
}

impl Sink {
    fn line(&self, s: impl Into<String>) {
        let mut l = self.log.lock().unwrap();
        l.push(s.into());
        if l.len() > 500 {
            l.drain(..100);
        }
        drop(l);
        self.ctx.request_repaint();
    }
}

/// Envia um tom a cada 20 ms e relata, por segundo, o que chegou. `secs = None`: até o outro lado desligar.
async fn talk(mut c: ActiveCall, hz: f64, secs: Option<u64>, stop: Arc<AtomicBool>, out: Sink, sound: Option<Sound>) {
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut report = interval(Duration::from_secs(1));
    let end = secs.map(|s| Instant::now() + Duration::from_secs(s));
    let (mut n, mut frames, mut energy) = (0usize, 0u64, 0f64);
    let mut prev = c.stats();
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if stop.load(Ordering::Relaxed) { break; }
                // Com microfone: manda o que já foi capturado (até 2 quadros, para absorver atraso do relógio).
                let frames: Vec<Vec<i16>> = match &sound {
                    Some(s) => std::iter::from_fn(|| s.mic_frame()).take(2).collect(),
                    None => vec![tone(hz, n)],
                };
                let mut closed = false;
                for f in frames {
                    closed |= c.send(f).await.is_err();
                }
                if closed { break; }
                n += 1;
                if end.is_some_and(|e| Instant::now() >= e) { break; }
            }
            f = c.next_pcm() => match f {
                Some(f) => {
                    if let Some(s) = &sound { s.play(&f); }
                    frames += 1;
                    energy += f.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / f.len() as f64;
                }
                None => break,
            },
            _ = report.tick() => {
                let s = c.stats();
                let rms = if frames > 0 { (energy / frames as f64).sqrt() } else { 0.0 };
                out.line(format!(
                    "recebidos +{:<3} (total {})  rejeitados {}  escondidos +{}  atraso {} ms  volume {:.0}",
                    s.received - prev.received, s.received, s.rejected, s.concealed - prev.concealed, s.delay_ms, rms
                ));
                prev = s;
                (frames, energy) = (0, 0.0);
            }
        }
    }
    let s = c.stats();
    out.line(format!("chamada encerrada: enviados {}, recebidos {}, escondidos {}, atrasados {}", s.sent, s.received, s.concealed, s.late));
    c.hangup();
    tokio::time::sleep(Duration::from_millis(500)).await;
}

async fn run_dial(form: Form, stop: Arc<AtomicBool>, out: Sink) -> Result<(), String> {
    let me = form.identity()?;
    let peer = PublicKey::from(unhex(&form.peer).map_err(|e| format!("chave do contato: {e}"))?);
    let a = parse_candidate(&form.relay_a)?;
    let b = parse_candidate(&form.relay_b)?;
    let secs: u64 = form.seconds.trim().parse().map_err(|_| "segundos: número inválido")?;
    let mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", vec![a, b]);
    let client = CircuitClient::spawn_with(mgr, Duration::from_secs(3), ClientOptions::default());
    out.line("ligando...");
    let c = call(client, &me, &peer, Mulaw::new(), Duration::from_secs(15)).await.map_err(|e| format!("não completou: {e}"))?;
    let sound = open_sound(form.use_audio, &out);
    out.line(format!("atendida; {secs} s"));
    talk(c, 440.0, Some(secs), stop, out, sound).await;
    Ok(())
}

async fn run_answer(form: Form, stop: Arc<AtomicBool>, out: Sink) -> Result<(), String> {
    let me = form.identity()?;
    let contacts = form
        .contacts
        .split([';', '\n', ' ', ','])
        .filter(|s| !s.trim().is_empty())
        .map(unhex)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("contatos: {e}"))?;
    if contacts.is_empty() {
        return Err("liste ao menos uma chave de contato: sem contatos ninguém toca".into());
    }
    let listen = form.listen.trim().parse().map_err(|e| format!("endereço de escuta: {e}"))?;
    let mut phone = Phone::bind(listen, me, contacts).await.map_err(|e| format!("não abriu a porta: {e}"))?;
    out.line(format!("telefone ouvindo em {}", phone.local_addr()));
    loop {
        tokio::select! {
            inc = phone.next_incoming() => {
                let Some(inc) = inc else { break };
                out.line(format!("chamada de {}", &hex(inc.caller().as_bytes())[..16]));
                match inc.accept(Mulaw::new()).await {
                    Ok(c) => {
                        let sound = open_sound(form.use_audio, &out);
                        talk(c, 660.0, None, stop.clone(), out.clone(), sound).await
                    }
                    Err(e) => out.line(format!("não atendeu: {e}")),
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    phone.stop();
    Ok(())
}

struct App {
    rt: tokio::runtime::Runtime,
    form: Form,
    mode: Mode,
    log: Log,
    running: Option<Arc<AtomicBool>>,
    done: Arc<AtomicBool>,
    error: String,
}

impl App {
    fn start(&mut self, ctx: &egui::Context) {
        self.error.clear();
        if let Err(e) = self.form.identity() {
            self.error = e;
            return;
        }
        self.form.save();
        self.log.lock().unwrap().clear();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let out = Sink { log: self.log.clone(), ctx: ctx.clone() };
        let (form, mode) = (self.form.clone(), self.mode);
        let (stop2, done2) = (stop.clone(), done.clone());
        self.rt.spawn(async move {
            let r = match mode {
                Mode::Dial => run_dial(form, stop2, out.clone()).await,
                Mode::Answer => run_answer(form, stop2, out.clone()).await,
            };
            if let Err(e) = r {
                out.line(format!("ERRO: {e}"));
            }
            out.line("parado");
            done2.store(true, Ordering::Relaxed);
            out.ctx.request_repaint();
        });
        self.running = Some(stop);
        self.done = done;
    }
}

fn field(ui: &mut egui::Ui, label: &str, text: &mut String, enabled: bool) {
    ui.label(label);
    ui.add_enabled(enabled, egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
    ui.end_row();
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if self.done.load(Ordering::Relaxed) {
            self.running = None;
            self.done.store(false, Ordering::Relaxed);
        }
        let idle = self.running.is_none();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Clam: chamada de voz");
            ui.horizontal(|ui| {
                ui.add_enabled_ui(idle, |ui| {
                    ui.checkbox(&mut self.form.use_audio, "Microfone e alto-falante");
                    ui.separator();
                    ui.radio_value(&mut self.mode, Mode::Dial, "Ligar");
                    ui.radio_value(&mut self.mode, Mode::Answer, "Atender");
                });
            });
            egui::Grid::new("form").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                field(ui, "Identidade (chave secreta, 64 hex)", &mut self.form.secret, idle);
                ui.label("Minha chave pública");
                ui.horizontal(|ui| {
                    match self.form.identity() {
                        Ok(k) => {
                            let p = hex(PublicKey::from(&k).as_bytes());
                            ui.monospace(&p);
                            if ui.button("Copiar").clicked() {
                                ui.ctx().copy_text(p);
                            }
                        }
                        Err(_) => {
                            ui.weak("(informe ou gere a identidade)");
                        }
                    }
                    if ui.add_enabled(idle, egui::Button::new("Gerar nova")).clicked() {
                        self.form.secret = hex(&StaticSecret::random_from_rng(OsRng).to_bytes());
                    }
                });
                ui.end_row();
                match self.mode {
                    Mode::Dial => {
                        field(ui, "Chave pública de quem vou chamar", &mut self.form.peer, idle);
                        field(ui, "Relay A (endereço,onion,link)", &mut self.form.relay_a, idle);
                        field(ui, "Relay B (endereço,onion,link)", &mut self.form.relay_b, idle);
                        field(ui, "Duração (segundos)", &mut self.form.seconds, idle);
                    }
                    Mode::Answer => {
                        field(ui, "Escutar em (ip:porta)", &mut self.form.listen, idle);
                        field(ui, "Chaves públicas permitidas (separe por ;)", &mut self.form.contacts, idle);
                    }
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let label = if self.mode == Mode::Dial { "Ligar" } else { "Atender" };
                if ui.add_enabled(idle, egui::Button::new(label)).clicked() {
                    self.start(ctx);
                }
                if ui.add_enabled(!idle, egui::Button::new("Desligar")).clicked()
                    && let Some(s) = &self.running
                {
                    s.store(true, Ordering::Relaxed);
                }
                if !self.error.is_empty() {
                    ui.colored_label(egui::Color32::LIGHT_RED, &self.error);
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
                for l in self.log.lock().unwrap().iter() {
                    ui.monospace(l);
                }
            });
        });
    }
}

fn main() -> eframe::Result {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([760.0, 520.0]).with_title("Clam"),
        ..Default::default()
    };
    eframe::run_native(
        "Clam",
        opts,
        Box::new(|_| {
            Ok(Box::new(App {
                rt,
                form: Form::load(),
                mode: Mode::Dial,
                log: Arc::default(),
                running: None,
                done: Arc::default(),
                error: String::new(),
            }))
        }),
    )
}
