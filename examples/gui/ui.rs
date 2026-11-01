//! Janela (egui): trilho de servidores, coluna de salas e engrenagem com as demais telas, no estilo do Discord, e o motor por trás.

use clam::sync::LockExt;
use std::{sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
}, time::Duration};

use eframe::egui::{self, Color32, Margin, vec2};
use tokio::sync::mpsc;

use crate::{conf::*, engine::*, theme::{self, *}};

/// Uma tela da janela (as de fora do servidor ficam no menu da engrenagem).
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
    /// Quadros por segundo que chegam de cada tela, para o selo da tela cheia.
    pub(crate) screen_rate: std::collections::HashMap<[u8; 32], crate::call::FrameRate>,
    /// Contato cuja conversa 1:1 está aberta no centro, e o texto ainda não enviado.
    pub(crate) chat_with: Option<[u8; 32]>,
    pub(crate) chat_draft: String,
    /// Atalhos de mudo e de fone que valem com a janela em segundo plano.
    pub(crate) global_keys: crate::atalhos::Global,
}

/// A engrenagem fica marcada nas telas que só se abrem por ela: sem isso a pessoa em Áudio ou Configurações não vê
/// onde está (o trilho marca a coluna de contatos, não a tela do centro).
pub(crate) fn gear_marks(tab: Tab) -> bool {
    matches!(tab, Tab::Video | Tab::Audio | Tab::Settings | Tab::Log)
}

/// Larguras a partir das quais o conteúdo se divide em duas colunas.
const TWO_COLUMNS: f32 = 780.0;
/// Quanto a pessoa espera o primeiro quadro depois de clicar em assistir antes de ver um aviso na Atividade.
const OPEN_SCREEN_WAIT: Duration = Duration::from_secs(10);

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
    if button(ui, true, "Atualizar a lista de dispositivos", pal().bg).clicked() {
        *devs = clam::audio::devices();
    }
}

/// Atalhos de teclado fixos (Ctrl; Cmd no macOS); mudo e fone são configuráveis (`atalhos`). Entrar numa sala não tem atalho de propósito: é o gesto que mostra o
/// IP aos membros, e uma tecla perdida não pode fazer isso.
pub(crate) const INVITE_KEY: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::I);
pub(crate) const SEARCH_KEY: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::K);
pub(crate) const MEMBERS_KEY: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::B);

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
            (pal().muted, "Offline")
        } else if status.is_empty() {
            (pal().yellow, "Conectando...")
        } else {
            (pal().green, "Online")
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// Coluna de salas
// ---------------------------------------------------------------------------------------------------

impl App {
    /// Pé da coluna de salas: avatar com o estado (clicar liga e desliga), mudo, engrenagem (as demais telas) e desligar.
    pub(crate) fn user_panel(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, offline: bool, in_call: bool, status: &str) {
        let (color, text) = self.connection(offline, status);
        let name = if self.form.my_name.trim().is_empty() { "Você".to_string() } else { self.form.my_name.trim().to_string() };
        let in_room = self.shared.room.lock_unpoisoned().at.is_some();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            // A bolinha de estado também liga/desliga o "aparecer online" para os membros dos servidores.
            let hub = self.shared.servers.lock_unpoisoned().clone().filter(|_| !offline);
            let hidden = hub.as_ref().is_some_and(|h| !h.read(|s, _| s.visible()));
            let me = theme::avatar(ui, &name, 40.0, Some(if hidden { pal().deep } else { color }));
            if !offline && status.is_empty() {
                // Conectando: um arco gira em volta do avatar até o nó dar sinal de vida.
                let t = ui.input(|i| i.time) as f32;
                let radius = 40.0 / 2.0 + 3.0;
                let start = t * 4.0;
                let arc: Vec<egui::Pos2> = (0..=16).map(|k| {
                    let a = start + k as f32 * (std::f32::consts::PI * 1.2 / 16.0);
                    me.rect.center() + vec2(a.cos(), a.sin()) * radius
                }).collect();
                ui.painter().add(egui::Shape::line(arc, egui::Stroke::new(2.5_f32, pal().yellow)));
                ctx.request_repaint();
            }
            if let Some(hub) = &hub {
                let r = 40.0 * 0.17;
                let at = me.rect.right_bottom() - vec2(r * 0.9, r * 0.9);
                if hidden {
                    ui.painter().circle_stroke(at, r - 1.0, egui::Stroke::new(2.0_f32, color));
                }
                let spot = egui::Rect::from_center_size(at, vec2(2.0 * r + 6.0, 2.0 * r + 6.0));
                let dot = ui.interact(spot, ui.id().with("bolinha-presenca"), egui::Sense::click());
                let tip = if hidden { "Invisível para os membros dos servidores. Clique para aparecer online" } else { "Aparecendo online para os membros dos servidores. Clique para ficar invisível" };
                dot.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, !hidden, tip));
                if dot.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    hub.run(|s, now| s.set_visible(hidden, now));
                }
            }
            // Em chamada não dá para ficar offline: o botão da aba Configurações também fica travado.
            let can_toggle = offline || !in_call;
            if can_toggle && me.hovered() {
                ui.painter().circle_filled(me.rect.center(), me.rect.width() / 2.0, Color32::from_black_alpha(150));
                theme::power_icon(ui, me.rect, if offline { pal().focus } else { pal().red });
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
            // Offline é o primeiro bloqueio de quem abre o app: o estado vira texto e a ação fica ao lado do avatar.
            if offline {
                ui.add_space(8.0);
                let go = ui.add(egui::Label::new(egui::RichText::new("Ficar online").size(14.0).color(pal().strong)).sense(egui::Sense::click()));
                if go.on_hover_text("Você está offline. Clique para ficar online").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.go_online(ctx);
                }
            }
            // Sair da sala e desligar a chamada 1:1 ficam na subseção acima deste painel.
            // Engrenagem e mudo ficam juntos na ponta direita da linha (a engrenagem por último, na borda).
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // A engrenagem marca a tela aberta quando ela mora no menu (o trilho ao lado só diz qual coluna está à esquerda).
                let here = gear_marks(self.tab);
                let gear_tip = if here { format!("{} (aberta). Chamada, vídeo, áudio, configurações e atividade", self.title().0) } else { "Chamada, vídeo, áudio, configurações e atividade".to_string() };
                let gear = glyph_button_active(ui, "Menu", &gear_tip, here, gear_icon);
                let calling_1to1 = in_call && !in_room;
                let menu = theme::menu(&gear).show(|ui| {
                    ui.set_min_width(theme::MENU_WIDTH);
                    type Icon = fn(&egui::Painter, egui::Pos2, f32, egui::Color32);
                    let mic: Icon = |p, c, s, k| mic_icon(p, c, s, k, false);
                    for (tab, icon, label) in [
                        (Tab::Call, phone_icon as Icon, if calling_1to1 { "Chamada em curso" } else { "Chamada" }),
                        (Tab::Video, monitor_icon as Icon, "Vídeo"),
                        (Tab::Audio, mic, "Áudio"),
                        (Tab::Settings, gear_icon as Icon, "Configurações"),
                        (Tab::Log, log_icon as Icon, "Atividade"),
                    ] {
                        // A chamada em curso também vai no acento do ícone, além do rótulo.
                        let live = tab == Tab::Call && calling_1to1;
                        let drawn = move |p: &egui::Painter, c: egui::Pos2, s: f32, k: egui::Color32| icon(p, c, s, if live { pal().focus } else { k });
                        if menu_item(ui, self.tab == tab, None, label, drawn).clicked() {
                            self.tab = tab;
                            ui.close();
                        }
                    }
                });
                theme::menu_focus_back(ui, &gear, menu.is_some());
                let a = &self.shared.audio;
                let (muted, deaf) = (a.is_muted(), a.is_deafened());
                let mute_key = crate::atalhos::resolve(&self.form.mute_key, crate::atalhos::MUTE_DEFAULT);
                let deaf_key = crate::atalhos::resolve(&self.form.deafen_key, crate::atalhos::DEAFEN_DEFAULT);
                // Layout da direita para a esquerda: o fone entra primeiro e fica à direita do microfone.
                let tip = if deaf { format!("Fone desligado: sem microfone e sem som. Clique para voltar ({})", ctx.format_shortcut(&deaf_key)) } else { format!("Desligar microfone e som ({})", ctx.format_shortcut(&deaf_key)) };
                let ear = glyph_button(ui, "Fone", &tip, deaf.then_some(pal().red), |p, c, s, k| headphones_icon(p, c, s, k, deaf));
                let tip = if muted { format!("Você está no mudo: clique para voltar a falar ({})", ctx.format_shortcut(&mute_key)) } else { format!("Silenciar o microfone ({})", ctx.format_shortcut(&mute_key)) };
                let mic = glyph_button(ui, "Microfone", &tip, muted.then_some(pal().red), |p, c, s, k| mic_icon(p, c, s, k, muted));
                self.shared.auto_files.store(self.form.auto_files, Ordering::Relaxed);
                self.global_keys.sync(ctx, &self.shared, &mute_key, &deaf_key);
                // Com o sistema entregando as teclas, a janela não as trata também.
                let keys = !crate::atalhos::capturing(ctx) && !self.global_keys.active();
                if mic.clicked() || (keys && ctx.input_mut(|i| i.consume_shortcut(&mute_key))) {
                    self.shared.audio.toggle_muted();
                }
                if ear.clicked() || (keys && ctx.input_mut(|i| i.consume_shortcut(&deaf_key))) {
                    self.shared.audio.toggle_deafened();
                }
            });
        });
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
        let due = LAST_NOTE.with(std::cell::Cell::get).is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if due {
            LAST_NOTE.with(|l| l.set(Some(std::time::Instant::now())));
            self.log.lock_unpoisoned().push(line);
        }
    }

    /// Tela cheia da transmissão de `zoom`: a janela do app vai a tela cheia e a imagem ocupa tudo, com a
    /// proporção mantida. Fecha pelo X no canto ou pelo Esc, e sozinha se a tela some. Fica no mesmo
    /// viewport: um segundo (outra janela do sistema com contexto GL próprio) travava a janela principal
    /// no Wayland, que esperava a troca de buffers das duas.
    fn zoom_window(&mut self, ctx: &egui::Context) {
        let flag = egui::Id::new("tela-cheia");
        // Clique no monitor de quem transmite na sala: abre quando o primeiro quadro chega; desiste se parou de seguir.
        if let Some(who) = self.srv.open_screen {
            if self.screen_tex.contains_key(&who) {
                self.zoom = Some(who);
                self.srv.open_screen = None;
            } else if !self.shared.room.lock_unpoisoned().watching.contains(&who) {
                self.srv.open_screen = None;
            } else if self.srv.open_screen_since.is_some_and(|t| t.elapsed() >= OPEN_SCREEN_WAIT) {
                // Sem este aviso o clique em assistir parece não fazer nada: quem transmite pode não ter aberto
                // o circuito de vídeo (porta 4001 inalcançável, relay antigo) ou nem estar capturando.
                self.log.lock_unpoisoned().push(format!("tela: {}s sem quadro de quem transmite; o circuito de vídeo dele não abriu ou ele não está capturando (veja a Atividade dele)", OPEN_SCREEN_WAIT.as_secs()));
                self.srv.open_screen = None;
                self.srv.open_screen_since = None;
            }
        }
        let was = ctx.data_mut(|d| d.get_temp::<bool>(flag)).unwrap_or(false);
        let tex = self.zoom.and_then(|who| self.screen_tex.get(&who).map(|(_, tex)| tex.clone()));
        let Some(tex) = tex else {
            self.stop_following_zoom();
            self.zoom = None;
            if was {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                ctx.data_mut(|d| d.insert_temp(flag, false));
            }
            return;
        };
        if !was {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
            ctx.data_mut(|d| d.insert_temp(flag, true));
        }
        let screen = ctx.screen_rect();
        let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        egui::Area::new(egui::Id::new("tela-ampliada")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            // Cobre a interface toda e come os cliques, para nada por baixo reagir.
            ui.allocate_rect(screen, egui::Sense::click_and_drag());
            ui.painter().rect_filled(screen, 0.0, Color32::BLACK);
            let size = tex.size_vec2();
            let fit = (screen.width() / size.x).min(screen.height() / size.y);
            let image = egui::Rect::from_center_size(screen.center(), size * fit);
            ui.put(image, egui::Image::new(&tex).fit_to_exact_size(image.size()));
            // Selo no canto: o que está chegando de verdade (resolução decodificada e quadros por segundo).
            let fps = self.zoom.and_then(|w| self.screen_rate.get(&w)).map_or(0.0, |r| r.fps());
            let label = format!("{}×{} · {:.0} fps", tex.size()[0], tex.size()[1], fps);
            ui.painter().text(egui::pos2(screen.min.x + 16.0, screen.min.y + 16.0), egui::Align2::LEFT_TOP, label, egui::FontId::proportional(14.0), Color32::from_white_alpha(200));
            let corner = egui::Rect::from_min_size(egui::pos2(screen.max.x - 48.0, screen.min.y + 12.0), vec2(32.0, 34.0));
            if ui.put(corner, |ui: &mut egui::Ui| theme::close_button(ui, "Sair da tela cheia (Esc)")).clicked() {
                close = true;
            }
            // Ouvir a tela: a chave mora aqui, onde a pessoa está assistindo.
            let ear = egui::Rect::from_min_size(egui::pos2(screen.max.x - 92.0, screen.min.y + 12.0), vec2(34.0, 34.0));
            let on = self.form.screen_hear;
            let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(ear));
            if theme::sound_toggle(&mut inner, on, false, "Ouvir o som da tela", "Ouvir o som da tela. Toca o som que vem junto da tela que você assiste, no volume do alto-falante. Só toca com a tela aberta. Vale na hora.") {
                self.form.screen_hear = !on;
                self.form.save();
            }
        });
        if close {
            self.stop_following_zoom();
            self.zoom = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            ctx.data_mut(|d| d.insert_temp(flag, false));
        }
    }

    /// Fechar a tela cheia de uma transmissão da sala para de segui-la: sem miniatura, não há por que receber.
    fn stop_following_zoom(&self) {
        if let Some(who) = self.zoom
            && self.shared.room.lock_unpoisoned().watching.contains(&who)
        {
            crate::engine::follow(&self.shared, who, false);
        }
    }

    fn title(&self) -> (&'static str, String) {
        match self.tab {
            Tab::Contacts | Tab::Call => ("Chamada", "A conversa e a tela compartilhada.".into()),
            Tab::Servers => ("Servidores", "Grupos de pessoas com salas de voz.".into()),
            Tab::Video => ("Vídeo", "Como a sua tela é transmitida.".into()),
            Tab::Audio => ("Áudio", "Dispositivos, volume e tratamento da voz.".into()),
            Tab::Settings => ("Configurações", "Conexão, identidade, rede e privacidade.".into()),
            Tab::Log => ("Atividade", "O que o programa está fazendo agora.".into()),
        }
    }
}

impl eframe::App for App {
    /// Fundo da janela atrás dos painéis: com o padrão (preto), uma fresta de 1 px entre dois painéis, por
    /// arredondamento de escala fracionária (comum no Linux), aparecia como uma linha preta.
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        pal().bg.to_normalized_gamma_f32()
    }

    /// Fechar a janela estando online: manda o aviso de saída aos membros dos servidores (`Msg::Bye`) antes de
    /// o processo morrer; sem isso eles me veem online até o prazo da presença vencer.
    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        let Some(r) = &self.running else { return };
        r.stop.store(true, Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !self.done.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let began = std::time::Instant::now();
        let watching = !self.shared.screen.images.lock_unpoisoned().is_empty();
        if let Some(end) = LAST_FRAME_END.with(std::cell::Cell::get) {
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
        self.absorb_removals();
        self.update_screen_textures(ctx);
        self.zoom_window(ctx);
        crate::midia::overlay(ctx);
        self.share_window(ctx);
        let offline = self.running.is_none();
        let in_call = self.shared.in_call.load(Ordering::Relaxed);
        let testing = self.shared.mic_test.load(Ordering::Relaxed);
        let calling = in_call && !testing;
        // A chamada não muda de aba: a coluna de contatos continua onde está e a subseção acima do perfil mostra
        // "Chamando…" ou a conversa.
        let in_room = self.shared.room.lock_unpoisoned().at.is_some();
        // As preferências de tela são lidas ao começar a transmissão, esteja a pessoa em que aba estiver.
        let st = &self.shared.screen;
        st.hardware.store(self.form.screen_hardware, Ordering::Relaxed);
        st.audio.store(self.form.screen_audio, Ordering::Relaxed);
        // O som da tela só toca com ela aberta (tela cheia), na chamada e na sala: miniatura e quadro solto são mudos,
        // e a chave de ouvir mora só na tela cheia.
        st.silenced.store(!self.form.screen_hear || self.zoom.is_none(), Ordering::Relaxed);
        let status = self.shared.status.lock_unpoisoned().clone();

        self.server_rail(ctx, offline);
        self.rooms_panel(ctx, offline, calling, &status);
        self.members_panel(ctx, offline);
        egui::CentralPanel::default().frame(egui::Frame::new().fill(pal().bg).inner_margin(Margin::symmetric(36, 26))).show(ctx, |ui| {
            // Chamada e conversa não levam cabeçalho de página: o que está abaixo já diz o que é.
            if !matches!(self.tab, Tab::Contacts | Tab::Call) {
                let (title, subtitle) = self.title();
                page_header(ui, title, &subtitle);
            }
            if self.tab == Tab::Log {
                self.log_tab(ui);
                return;
            }
            // A chave secreta só fica à mostra enquanto a pessoa está em Configurações.
            if self.tab != Tab::Settings {
                ctx.data_mut(|d| d.remove_temp::<bool>(crate::settings::show_secret_id()));
            }
            // A chamada não rola: o único que rola ali é o bate-papo, dentro do próprio painel.
            if matches!(self.tab, Tab::Contacts | Tab::Call) {
                ui.set_max_width(ui.available_width().min(1500.0));
                self.call_tab(ui, offline, calling, calling && !in_room);
                return;
            }
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(1500.0));
                match self.tab {
                    Tab::Contacts | Tab::Call => {}
                    Tab::Servers => self.servers_tab(ui, offline),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A engrenagem marca só as telas que moram no menu dela; a inicial, a chamada e os servidores ficam de fora.
    #[test]
    fn the_gear_marks_only_the_screens_it_opens() {
        for tab in [Tab::Video, Tab::Audio, Tab::Settings, Tab::Log] {
            assert!(gear_marks(tab));
        }
        for tab in [Tab::Contacts, Tab::Call, Tab::Servers] {
            assert!(!gear_marks(tab));
        }
    }
}
