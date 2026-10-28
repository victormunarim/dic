//! Janela (egui): barra lateral com uma aba por assunto, no estilo do Discord, e o motor por trás.

use clam::sync::LockExt;
use std::{sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
}, time::Duration};

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, vec2};
use tokio::sync::mpsc;

use crate::{conf::*, engine::*, theme::{self, *}};

/// Uma aba da barra lateral.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Contacts,
    Call,
    Servers,
    Video,
    Audio,
    Settings,
    Log,
}

pub(crate) struct App {
    pub(crate) rt: tokio::runtime::Runtime,
    pub(crate) form: Form,
    pub(crate) log: Log,
    pub(crate) shared: Arc<Shared>,
    pub(crate) running: Option<Running>,
    pub(crate) done: Arc<AtomicBool>,
    pub(crate) error: String,
    /// Aba aberta.
    pub(crate) tab: Tab,
    /// Havia chamada no quadro anterior (para abrir a aba da chamada quando ela começa).
    pub(crate) was_in_call: bool,
    /// Ajustes de áudio mudaram e ainda não foram salvos (salva ao soltar o controle).
    pub(crate) audio_dirty: bool,
    /// Microfones e alto-falantes que o sistema listou (atualizável na janela).
    #[cfg_attr(not(feature = "audio"), allow(dead_code))]
    pub(crate) devs: (Vec<String>, Vec<String>),
    /// Tela de quem está aberta numa janela própria, em tamanho grande.
    pub(crate) zoom: Option<[u8; 32]>,
    /// Aba de servidores.
    pub(crate) srv: crate::servers::ServersUi,
    /// Textura da tela de quem transmite para você e o número da imagem que ela mostra.
    pub(crate) screen_tex: std::collections::HashMap<[u8; 32], (u64, egui::TextureHandle)>,
    /// Contato que a pessoa acabou de marcar como de confiança e ainda não confirmou o aviso do IP.
    pub(crate) trust_ask: Option<[u8; 32]>,
}

/// Larguras a partir das quais o conteúdo se divide em duas colunas.
const TWO_COLUMNS: f32 = 780.0;

/// Chama `f` para a coluna da esquerda (0) e a da direita (1): lado a lado se couber, uma sobre a outra se não.
pub(crate) fn split(ui: &mut egui::Ui, mut f: impl FnMut(&mut egui::Ui, usize)) {
    if ui.available_width() >= TWO_COLUMNS {
        ui.columns(2, |c| {
            f(&mut c[0], 0);
            f(&mut c[1], 1);
        });
    } else {
        f(ui, 0);
        f(ui, 1);
    }
}

/// Escolha de microfone e alto-falante (vazio = padrão do sistema); vale na próxima chamada.
#[cfg(feature = "audio")]
pub(crate) fn devices_ui(ui: &mut egui::Ui, form: &mut Form, devs: &mut (Vec<String>, Vec<String>)) {
    let pick = |ui: &mut egui::Ui, label: &str, cur: &mut String, list: &[String]| {
        ui.label(label);
        let shown = if cur.is_empty() { "padrão do sistema".to_string() } else { cur.clone() };
        egui::ComboBox::from_id_salt(label).selected_text(shown).width(ui.available_width()).show_ui(ui, |ui| {
            ui.selectable_value(cur, String::new(), "padrão do sistema");
            for d in list {
                ui.selectable_value(cur, d.clone(), d);
            }
        });
    };
    let (ins, outs) = devs.clone();
    pick(ui, "Microfone", &mut form.input_dev, &ins);
    pick(ui, "Alto-falante", &mut form.output_dev, &outs);
    note(ui, format!("Sistema de áudio: {}", clam::audio::host_name()));
    if ui.button("Atualizar a lista de dispositivos").clicked() {
        *devs = clam::audio::devices();
    }
}

impl App {
    pub(crate) fn go_online(&mut self, ctx: &egui::Context) {
        self.error.clear();
        if let Err(e) = self.form.identity() {
            self.error = e;
            return;
        }
        if let Err(e) = self.form.save_checked() {
            self.error = format!("não deu para gravar a configuração em {}: {e}; os contatos não ficam salvos", conf_path().display());
        }
        self.log.lock_unpoisoned().clear();
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let out = Sink {
            log: self.log.clone(),
            repaint: {
                let ctx = ctx.clone();
                Arc::new(move || ctx.request_repaint())
            },
        };
        let (form, shared, stop2, done2) = (self.form.clone(), self.shared.clone(), stop.clone(), done.clone());
        self.rt.spawn(async move {
            if let Err(e) = run_node(form, rx, shared, stop2, out.clone()).await {
                out.line(format!("ERRO: {e}"));
            }
            out.line("offline");
            done2.store(true, Ordering::Relaxed);
            (out.repaint)();
        });
        self.running = Some(Running { cmds: tx, stop });
        self.done = done;
    }

    pub(crate) fn dial(&mut self, key: [u8; 32]) {
        if let Some(r) = &self.running {
            self.error.clear();
            let _ = r.cmds.send(Cmd::Dial(key, self.form.relay_for(&key)));
        }
    }

    /// Texto e cor do estado da conexão.
    pub(crate) fn connection(&self, offline: bool, status: &str) -> (Color32, &'static str) {
        if offline {
            (MUTED, "Offline")
        } else if status.is_empty() {
            (YELLOW, "Conectando...")
        } else {
            (GREEN, "Online")
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// Barra lateral
// ---------------------------------------------------------------------------------------------------

impl App {
    fn sidebar(&mut self, ctx: &egui::Context, offline: bool, in_call: bool, status: &str) {
        let in_room = self.shared.room.lock_unpoisoned().at.is_some();
        egui::SidePanel::left("abas")
            .exact_width(76.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(12, 14)))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                let items = [
                    (Tab::Contacts, "👥", "Contatos", None),
                    (Tab::Call, "📞", "Chamada", (in_call && !in_room).then_some(GREEN)),
                ];
                for (tab, icon, label, badge) in items {
                    if nav_item(ui, self.tab == tab, icon, label, badge).clicked() {
                        self.tab = tab;
                    }
                }
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                for (tab, icon, label) in [(Tab::Video, "🖥", "Vídeo"), (Tab::Audio, "🎤", "Áudio"), (Tab::Settings, "⚙", "Configurações"), (Tab::Log, "📜", "Atividade")] {
                    if nav_item(ui, self.tab == tab, icon, label, None).clicked() {
                        self.tab = tab;
                    }
                }

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    self.user_panel(ctx, ui, offline, in_call, status);
                });
            });
    }

    /// Pé da barra, de baixo para cima: o avatar com o estado (clicar liga e desliga), o mudo e o desligar.
    fn user_panel(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, offline: bool, in_call: bool, status: &str) {
        let (color, text) = self.connection(offline, status);
        let name = if self.form.my_name.trim().is_empty() { "Você".to_string() } else { self.form.my_name.trim().to_string() };
        let me = theme::avatar(ui, &name, 46.0, Some(color));
        // Em chamada não dá para ficar offline: o botão da aba Configurações também fica travado.
        let can_toggle = offline || !in_call;
        if can_toggle && me.hovered() {
            ui.painter().circle_filled(me.rect.center(), me.rect.width() / 2.0, Color32::from_black_alpha(150));
            theme::power_icon(ui, me.rect, if offline { GREEN } else { RED });
        }
        let tip = format!("{name}: {text}{}", if !can_toggle { "" } else if offline { "\nClique para ficar online" } else { "\nClique para ficar offline" });
        let me = me.on_hover_text(tip);
        if can_toggle && me.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            if offline {
                self.go_online(ctx);
            } else if let Some(r) = &self.running {
                r.stop.store(true, Ordering::Relaxed);
            }
        }
        let muted = self.shared.audio.muted.load(Ordering::Relaxed);
        let mic = egui::Button::new(RichText::new(if muted { "🔇" } else { "🎤" }).size(20.0).color(if muted { RED } else { TEXT }))
            .frame(false)
            .min_size(vec2(48.0, 40.0));
        if ui.add(mic).on_hover_text(if muted { "Você está no mudo: clique para voltar a falar" } else { "Silenciar o microfone" }).clicked() {
            self.shared.audio.muted.store(!muted, Ordering::Relaxed);
        }
        if in_call {
            let in_room = self.shared.room.lock_unpoisoned().at.is_some();
            let hang = egui::Button::new(RichText::new("✖").strong().size(18.0).color(Color32::WHITE)).fill(RED).corner_radius(CornerRadius::same(10)).min_size(vec2(48.0, 40.0));
            if ui.add(hang).on_hover_text(if in_room { "Sair da sala" } else { "Desligar a chamada" }).clicked() {
                if in_room {
                    if let Some(r) = &self.running {
                        let _ = r.cmds.send(Cmd::LeaveRoom);
                    }
                } else {
                    self.shared.hangup.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// Janela
// ---------------------------------------------------------------------------------------------------

thread_local! {
    /// Fim do último quadro desenhado, para notar quando a janela fica parada.
    static LAST_FRAME_END: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

impl App {
    /// Registra no log da janela uma lentidão da própria janela, no máximo uma vez por segundo.
    fn note_slow(&mut self, line: String) {
        thread_local!(static LAST_NOTE: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) });
        let due = LAST_NOTE.with(|l| l.get()).is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if due {
            LAST_NOTE.with(|l| l.set(Some(std::time::Instant::now())));
            self.log.lock_unpoisoned().push(line);
        }
    }

    /// Janela própria com a tela de `zoom`, ajustada ao tamanho da janela; fecha sozinha se a tela some.
    fn zoom_window(&mut self, ctx: &egui::Context) {
        let Some(who) = self.zoom else { return };
        let Some((_, tex)) = self.screen_tex.get(&who) else {
            self.zoom = None;
            return;
        };
        let title = format!("Tela de {}", member_name(&self.form.names(), &who));
        let size = tex.size_vec2();
        // Janela flutuante dentro da janela do app: um segundo viewport (outra janela do sistema com
        // contexto GL próprio) travava a janela principal no Wayland, que esperava a troca de buffers das duas.
        let screen = ctx.screen_rect().size();
        let fit = (screen.x * 0.9 / size.x).min(screen.y * 0.9 / size.y).min(1.0);
        let mut open = true;
        egui::Window::new(title)
            .id(egui::Id::new("tela-ampliada"))
            .open(&mut open)
            .resizable(true)
            .default_size(size * fit)
            .show(ctx, |ui| {
                ui.add(egui::Image::new(tex).fit_to_exact_size(ui.available_size()).maintain_aspect_ratio(true));
            });
        if !open {
            self.zoom = None;
        }
    }

    fn title(&self) -> (&'static str, String) {
        match self.tab {
            Tab::Contacts => ("Contatos", format!("{} na sua lista.", match parse_contacts(&self.form.contacts).len() {
                0 => "Ninguém".to_string(),
                1 => "1 pessoa".to_string(),
                n => format!("{n} pessoas"),
            })),
            Tab::Call => ("Chamada", "A conversa e a tela compartilhada.".into()),
            Tab::Servers => ("Servidores", "Grupos de pessoas com salas de voz.".into()),
            Tab::Video => ("Vídeo", "Como a sua tela é transmitida.".into()),
            Tab::Audio => ("Áudio", "Dispositivos, volume e tratamento da voz.".into()),
            Tab::Settings => ("Configurações", "Conexão, identidade, rede e privacidade.".into()),
            Tab::Log => ("Atividade", "O que o programa está fazendo agora.".into()),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let began = std::time::Instant::now();
        let watching = !self.shared.screen.images.lock_unpoisoned().is_empty();
        if let Some(end) = LAST_FRAME_END.with(|l| l.get()) {
            // Vendo tela a janela repinta a cada 16 ms: bem mais que isso sem atualizar é a janela travada fora da
            // nossa conta (troca de buffers, eventos do sistema).
            let gap = began.duration_since(end);
            if watching && gap > Duration::from_millis(400) {
                self.note_slow(format!("janela: {} ms sem atualizar entre dois quadros", gap.as_millis()));
            }
        }
        if self.done.swap(false, Ordering::Relaxed) {
            self.running = None;
            self.shared.in_call.store(false, Ordering::Relaxed);
            self.shared.room.lock_unpoisoned().clear();
            self.shared.status.lock_unpoisoned().clear();
            *self.shared.relay.lock_unpoisoned() = None;
        }
        self.absorb_introductions();
        self.update_screen_textures(ctx);
        self.zoom_window(ctx);
        self.trust_window(ctx);
        let offline = self.running.is_none();
        let in_call = self.shared.in_call.load(Ordering::Relaxed);
        let testing = self.shared.mic_test.load(Ordering::Relaxed);
        let calling = in_call && !testing;
        // Quando a chamada começa, a aba dela abre sozinha.
        let in_room = self.shared.room.lock_unpoisoned().at.is_some();
        if calling && !self.was_in_call && !in_room {
            self.tab = Tab::Call;
        }
        self.was_in_call = calling;
        // As preferências de tela são lidas ao começar a transmissão, esteja a pessoa em que aba estiver.
        let st = &self.shared.screen;
        st.open_path.store(!self.form.screen_private, Ordering::Relaxed);
        st.sharp.store(self.form.screen_sharp, Ordering::Relaxed);
        st.hardware.store(self.form.screen_hardware, Ordering::Relaxed);
        let status = self.shared.status.lock_unpoisoned().clone();

        self.server_rail(ctx, offline);
        self.sidebar(ctx, offline, calling, &status);
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(36, 26))).show(ctx, |ui| {
            let (title, subtitle) = self.title();
            page_header(ui, title, &subtitle);
            if self.tab == Tab::Log {
                self.log_tab(ui);
                return;
            }
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(1500.0));
                match self.tab {
                    Tab::Contacts => self.contacts_tab(ui, !offline, calling),
                    Tab::Call => self.call_tab(ui, offline, calling, calling && !in_room),
                    Tab::Servers => self.servers_tab(ui, offline, calling),
                    Tab::Video => self.video_tab(ui),
                    Tab::Audio => self.audio_tab(ui, offline, calling, testing),
                    Tab::Settings => self.settings_tab(ctx, ui, offline, calling, &status),
                    Tab::Log => {}
                }
            });
        });
        // Com tela chegando a janela repinta a cada ~16 ms; a 300 ms o vídeo parecia ter 3 quadros/s.
        ctx.request_repaint_after(Duration::from_millis(if watching { 16 } else { 300 }));
        let took = began.elapsed();
        if took > Duration::from_millis(100) {
            self.note_slow(format!("janela: um quadro levou {} ms para desenhar", took.as_millis()));
        }
        LAST_FRAME_END.with(|l| l.set(Some(std::time::Instant::now())));
    }
}
