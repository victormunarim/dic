//! Janela mínima para ligar e atender sem usar o terminal.
//! Roteiro no README, seção "Interface gráfica mínima".
//!
//! ```text
//! cargo run --release --features gui,audio --example gui
//! ```
//! Você informa a identidade, o endereço do diretório e os contatos; "Ficar online" sobe relay e
//! telefone e publica no diretório; "Ligar" só precisa da chave pública do outro. Chamadas de
//! contatos da lista são atendidas sozinhas. Sem `--features audio` (ou sem a opção marcada),
//! cada lado envia um tom (440 Hz quem liga, 660 Hz quem atende).
//! Os campos são gravados em `~/.clam-gui.conf` (inclui a chave secreta).

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use clam::media::codec::G722;
use clam::node::{Node, NodeConfig};
use clam::phone::ActiveCall;
use clam::{
    rand_core::OsRng,
    x25519_dalek::{PublicKey, StaticSecret},
};
use eframe::egui;
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval},
};

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
    fn played(&self) -> usize {
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
    match Sound::open_rate(G722::RATE) {
        Ok(s) => {
            out.line(format!("microfone e alto-falante abertos (use fones: não há cancelamento de eco): {}", s.info()));
            return Some(s);
        }
        Err(e) => out.line(format!("{e}; usando tom de teste")),
    }
    #[cfg(not(feature = "audio"))]
    out.line("compilado sem a feature audio; usando tom de teste");
    None
}

fn tone(hz: f64, n: usize) -> Vec<i16> {
    (0..320).map(|i| (6000.0 * (std::f64::consts::TAU * hz * (n * 320 + i) as f64 / 16000.0).sin()) as i16).collect()
}

#[derive(Default, Clone)]
struct Form {
    secret: String,
    directory: String,
    /// Uma linha por contato: `[nome] chave-pública-hex`.
    contacts: String,
    peer: String,
    public_addr: String,
    use_audio: bool,
    via_server: bool,
}

/// Código de contato: a chave pública (64 hex), com o diretório opcional depois de `@`
/// (`<chave>@host:porta`). Devolve a chave e o diretório, se houver.
fn split_code(t: &str) -> Option<([u8; 32], Option<String>)> {
    let (key, dir) = match t.trim().split_once('@') {
        Some((k, d)) => (k, Some(d.trim().to_string()).filter(|d| !d.is_empty())),
        None => (t.trim(), None),
    };
    unhex(key).ok().map(|k| (k, dir))
}

/// Contatos da lista: `(nome, chave)`. Linhas sem uma chave de 64 hex são ignoradas.
fn parse_contacts(text: &str) -> Vec<(String, [u8; 32])> {
    text.lines()
        .filter_map(|l| {
            let mut name = vec![];
            let mut key = None;
            for t in l.split_whitespace() {
                match split_code(t) {
                    Some((k, _)) => key = Some(k),
                    None => name.push(t),
                }
            }
            key.map(|k| (if name.is_empty() { hex(&k)[..8].to_string() } else { name.join(" ") }, k))
        })
        .collect()
}

fn conf_path() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
    home.join(".clam-gui.conf")
}

impl Form {
    fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(conf_path()) else { return Form::default() };
        let kv: HashMap<_, _> = text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let get = |k: &str| kv.get(k).cloned().unwrap_or_default();
        Form {
            secret: get("secret"),
            directory: get("directory"),
            contacts: get("contacts").replace(';', "\n"),
            peer: get("peer"),
            public_addr: get("public_addr"),
            use_audio: get("audio") == "1",
            via_server: get("via_server") == "1",
        }
    }

    fn save(&self) {
        let text = format!(
            "audio={}\nsecret={}\ndirectory={}\ncontacts={}\npeer={}\npublic_addr={}\nvia_server={}\n",
            u8::from(self.use_audio),
            self.secret.trim(),
            self.directory.trim(),
            self.contacts.trim().replace('\n', ";"),
            self.peer.trim(),
            self.public_addr.trim(),
            u8::from(self.via_server)
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

    /// Resolve o campo "ligar para": uma chave de 64 hex ou o nome de um contato.
    /// Diretório a usar: o campo, ou o que veio num código de contato (`chave@host:porta`).
    fn directory_addr(&self) -> String {
        let d = self.directory.trim();
        if !d.is_empty() {
            return d.to_string();
        }
        let codes = self.contacts.split_whitespace().chain(self.peer.split_whitespace());
        codes.filter_map(|t| split_code(t).and_then(|(_, d)| d)).next().unwrap_or_default()
    }

    /// Meu código de contato: chave pública e, se conhecido, o diretório.
    fn my_code(&self) -> Option<String> {
        let key = hex(PublicKey::from(&self.identity().ok()?).as_bytes());
        let dir = self.directory_addr();
        Some(if dir.is_empty() { key } else { format!("{key}@{dir}") })
    }

    fn target(&self) -> Result<[u8; 32], String> {
        let p = self.peer.trim();
        if let Some((k, _)) = split_code(p) {
            return Ok(k);
        }
        parse_contacts(&self.contacts)
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(p))
            .map(|(_, k)| k)
            .ok_or_else(|| "informe o código de contato (ou chave de 64 hex) ou o nome de um contato".to_string())
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

/// Envia um tom (ou o microfone) a cada 20 ms e relata, por segundo, o que chegou. Termina ao desligar
/// (`stop`) ou quando o outro lado desliga.
async fn talk(mut c: ActiveCall, hz: f64, stop: Arc<AtomicBool>, out: Sink, sound: Option<Sound>) {
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut report = interval(Duration::from_secs(1));
    let (mut n, mut frames, mut energy) = (0usize, 0u64, 0f64);
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
                for f in sent {
                    closed |= c.send(f).await.is_err();
                }
                if closed { break; }
                n += 1;
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
                    "recebidos +{:<3} (total {})  escondidos +{}  atraso {} ms  volume {:.0}{}",
                    s.received - prev.received, s.received, s.concealed - prev.concealed, s.delay_ms, rms,
                    sound.as_ref().map(|s| format!("  tocadas {}", s.played())).unwrap_or_default()
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

enum Cmd {
    Dial([u8; 32]),
}

/// Estado compartilhado entre a janela e a tarefa do nó.
#[derive(Default)]
struct Shared {
    /// Há uma chamada em andamento (entrando ou saindo).
    in_call: AtomicBool,
    /// Pedido de desligar a chamada atual.
    hangup: AtomicBool,
    /// Texto de estado do nó ("online em ...").
    status: Mutex<String>,
}

/// Roda o nó até `stop`: atende contatos sozinho e disca quando a janela pede.
async fn run_node(form: Form, mut rx: mpsc::UnboundedReceiver<Cmd>, shared: Arc<Shared>, stop: Arc<AtomicBool>, out: Sink) -> Result<(), String> {
    let secret = form.identity()?;
    if form.directory_addr().is_empty() {
        return Err("informe o diretório (host:porta), ou cole um código de contato com @host:porta".into());
    }
    let contacts: Vec<_> = parse_contacts(&form.contacts);
    let mut cfg = NodeConfig::new(secret, form.directory_addr(), contacts.iter().map(|(_, k)| *k).collect());
    cfg.via_server = form.via_server;
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
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            inc = node.next_incoming() => {
                let Some(inc) = inc else { break };
                let name = contacts.iter().find(|(_, k)| k == inc.caller().as_bytes()).map(|(n, _)| n.clone()).unwrap_or_else(|| hex(inc.caller().as_bytes())[..8].to_string());
                if shared.in_call.swap(true, Ordering::Relaxed) {
                    out.line(format!("{name} ligou, mas já há uma chamada; recusada"));
                    inc.reject().await;
                    continue;
                }
                out.line(format!("chamada de {name}"));
                shared.hangup.store(false, Ordering::Relaxed);
                match inc.accept(G722::new()).await {
                    Ok(c) => start_talk(c, 660.0, &shared, &out, form.use_audio),
                    Err(e) => {
                        out.line(format!("não atendeu: {e}"));
                        shared.in_call.store(false, Ordering::Relaxed);
                    }
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
                    match node.dial(peer, G722::new(), Duration::from_secs(15)).await {
                        Ok(c) => {
                            out.line("atendida");
                            start_talk(c, 440.0, &shared, &out, form.use_audio);
                        }
                        Err(e) => {
                            out.line(format!("não completou: {e}"));
                            shared.in_call.store(false, Ordering::Relaxed);
                        }
                    }
                }
                None => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        if let Some(n) = node.take_notice() {
            out.line(n);
        }
    }
    shared.hangup.store(true, Ordering::Relaxed);
    Ok(())
}

/// Inicia a conversa numa tarefa própria; `hangup` (da janela) encerra.
fn start_talk(c: ActiveCall, hz: f64, shared: &Arc<Shared>, out: &Sink, want_audio: bool) {
    let sound = open_sound(want_audio, out);
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
        talk(c, hz, stop.clone(), out, sound).await;
        stop.store(true, Ordering::Relaxed);
        shared.in_call.store(false, Ordering::Relaxed);
    });
}

struct Running {
    cmds: mpsc::UnboundedSender<Cmd>,
    stop: Arc<AtomicBool>,
}

struct App {
    rt: tokio::runtime::Runtime,
    form: Form,
    log: Log,
    shared: Arc<Shared>,
    running: Option<Running>,
    done: Arc<AtomicBool>,
    error: String,
}

impl App {
    fn go_online(&mut self, ctx: &egui::Context) {
        self.error.clear();
        if let Err(e) = self.form.identity() {
            self.error = e;
            return;
        }
        if self.form.directory.trim().is_empty() {
            self.error = "informe o endereço do diretório (host:porta)".into();
            return;
        }
        self.form.save();
        self.log.lock().unwrap().clear();
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let out = Sink { log: self.log.clone(), ctx: ctx.clone() };
        let (form, shared, stop2, done2) = (self.form.clone(), self.shared.clone(), stop.clone(), done.clone());
        self.rt.spawn(async move {
            if let Err(e) = run_node(form, rx, shared, stop2, out.clone()).await {
                out.line(format!("ERRO: {e}"));
            }
            out.line("offline");
            done2.store(true, Ordering::Relaxed);
            out.ctx.request_repaint();
        });
        self.running = Some(Running { cmds: tx, stop });
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
        if self.done.swap(false, Ordering::Relaxed) {
            self.running = None;
            self.shared.in_call.store(false, Ordering::Relaxed);
            self.shared.status.lock().unwrap().clear();
        }
        let offline = self.running.is_none();
        let in_call = self.shared.in_call.load(Ordering::Relaxed);
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Clam: chamada de voz");
            egui::Grid::new("form").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                field(ui, "Identidade (chave secreta, 64 hex)", &mut self.form.secret, offline);
                ui.label("Meu código de contato");
                ui.horizontal(|ui| {
                    match self.form.my_code() {
                        Some(code) => {
                            ui.monospace(if code.len() > 40 { format!("{}...", &code[..40]) } else { code.clone() });
                            if ui.button("Copiar").clicked() {
                                ui.ctx().copy_text(code);
                            }
                        }
                        None => {
                            ui.weak("(informe ou gere a identidade)");
                        }
                    }
                    if ui.add_enabled(offline, egui::Button::new("Gerar nova")).clicked() {
                        self.form.secret = hex(&StaticSecret::random_from_rng(OsRng).to_bytes());
                    }
                });
                ui.end_row();
                field(ui, "Diretório (host:porta)", &mut self.form.directory, offline);
                ui.label("Contatos (um por linha:\n[nome] código de contato)");
                ui.add_enabled(offline, egui::TextEdit::multiline(&mut self.form.contacts).desired_rows(3).desired_width(f32::INFINITY));
                ui.end_row();
                ui.label("Opções");
                ui.vertical(|ui| {
                    ui.add_enabled(offline, egui::Checkbox::new(&mut self.form.use_audio, "Microfone e alto-falante (use fones)"));
                    ui.add_enabled(offline, egui::Checkbox::new(&mut self.form.via_server, "Atrás de NAT: usar o relay do servidor (sem abrir porta)"));
                    ui.add_enabled_ui(offline, |ui| {
                        ui.collapsing("Avançado", |ui| {
                            ui.label("Endereço público ip:porta (só se o diretório vir um IP que não serve, como em NAT):");
                            ui.text_edit_singleline(&mut self.form.public_addr);
                        });
                    });
                });
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if offline {
                    if ui.button("Ficar online").clicked() {
                        self.go_online(ctx);
                    }
                } else if ui.add_enabled(!in_call, egui::Button::new("Ficar offline")).clicked()
                    && let Some(r) = &self.running
                {
                    r.stop.store(true, Ordering::Relaxed);
                }
                let st = self.shared.status.lock().unwrap().clone();
                if !st.is_empty() {
                    ui.label(st);
                }
                if !self.error.is_empty() {
                    ui.colored_label(egui::Color32::LIGHT_RED, &self.error);
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Ligar para:");
                ui.add_enabled(!offline && !in_call, egui::TextEdit::singleline(&mut self.form.peer).hint_text("nome ou código de contato").desired_width(420.0));
                if ui.add_enabled(!offline && !in_call, egui::Button::new("Ligar")).clicked() {
                    match self.form.target() {
                        Ok(k) => {
                            self.error.clear();
                            self.form.save();
                            if let Some(r) = &self.running {
                                let _ = r.cmds.send(Cmd::Dial(k));
                            }
                        }
                        Err(e) => self.error = e,
                    }
                }
                if ui.add_enabled(in_call, egui::Button::new("Desligar")).clicked() {
                    self.shared.hangup.store(true, Ordering::Relaxed);
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
                for l in self.log.lock().unwrap().iter() {
                    ui.monospace(l);
                }
            });
        });
        ctx.request_repaint_after(Duration::from_millis(300));
    }
}

fn main() -> eframe::Result {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([820.0, 560.0]).with_title("Clam"),
        ..Default::default()
    };
    eframe::run_native(
        "Clam",
        opts,
        Box::new(|_| {
            Ok(Box::new(App {
                rt,
                form: Form::load(),
                log: Arc::default(),
                shared: Arc::default(),
                running: None,
                done: Arc::default(),
                error: String::new(),
            }))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contact_code_parses() {
        let k = "ab".repeat(32);
        assert_eq!(split_code(&k).unwrap().1, None);
        let (key, dir) = split_code(&format!(" {k}@10.0.0.1:7777 ")).unwrap();
        assert_eq!((key, dir.as_deref()), ([0xab; 32], Some("10.0.0.1:7777")));
        assert!(split_code("nome").is_none());
        let c = parse_contacts(&format!("ana {k}@h:1"));
        assert_eq!(c, vec![("ana".to_string(), [0xab; 32])]);
    }
}
