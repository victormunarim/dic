//! Janela (egui): barra lateral com uma aba por assunto, no estilo do Discord, e o motor por trás.

use std::{sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
}, time::Duration};

use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};
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
}

/// Larguras a partir das quais o conteúdo se divide em duas colunas.
const TWO_COLUMNS: f32 = 780.0;

/// Chama `f` para a coluna da esquerda (0) e a da direita (1): lado a lado se couber, uma sobre a outra se não.
fn split(ui: &mut egui::Ui, mut f: impl FnMut(&mut egui::Ui, usize)) {
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
fn devices_ui(ui: &mut egui::Ui, form: &mut Form, devs: &mut (Vec<String>, Vec<String>)) {
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
        self.log.lock().unwrap().clear();
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

    fn dial(&mut self, key: [u8; 32]) {
        if let Some(r) = &self.running {
            self.error.clear();
            let _ = r.cmds.send(Cmd::Dial(key, self.form.relay_for(&key)));
        }
    }

    /// Texto e cor do estado da conexão.
    fn connection(&self, offline: bool, status: &str) -> (Color32, &'static str) {
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
        let in_room = self.shared.room.lock().unwrap().at.is_some();
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
            let in_room = self.shared.room.lock().unwrap().at.is_some();
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
// Contatos
// ---------------------------------------------------------------------------------------------------

impl App {
    /// Quem se apresentou (o nó já o atende): se é novo, entra na lista e recebe de volta a apresentação desta
    /// pessoa, para ele ter o nome dela; se já era contato sem nome (foi adicionado só pelo código), ganha o nome
    /// que ele deu a si mesmo. Quem já tinha nome não muda, e não há apresentação de volta (senão as duas pontas
    /// ficariam se apresentando para sempre).
    fn absorb_introductions(&mut self) {
        let introduced: Vec<_> = std::mem::take(&mut *self.shared.introduced.lock().unwrap());
        let mut added = false;
        for i in introduced {
            if !parse_contacts(&self.form.contacts).iter().any(|(_, k)| *k == i.peer) {
                if !self.form.contacts.trim().is_empty() {
                    self.form.contacts.push('\n');
                }
                self.form.contacts.push_str(&intro_line(&i.name, &i.peer, &i.reach));
                if let Some(r) = &self.running {
                    let relay = clam::node::RelayInfo { addr: i.reach.addr.clone(), onion: i.reach.onion, link: i.reach.link };
                    let _ = r.cmds.send(Cmd::Contact(i.peer, Some(relay)));
                }
                added = true;
            } else if let Some(text) = name_contact(&self.form.contacts, &i.peer, &i.name) {
                self.form.contacts = text;
                added = true;
            }
        }
        if added {
            self.form.save();
        }
    }

    fn contacts_tab(&mut self, ui: &mut egui::Ui, online: bool, in_call: bool) {
        card(ui, "Adicionar contato", "Peça o código de contato de um amigo. Ele também aparece na sua lista, se você deixar.", |ui| {
            let w = ui.available_width();
            let button_w = 170.0;
            ui.horizontal(|ui| {
                ui.add(field(&mut self.form.new_code, "Código de contato").desired_width((w - button_w - ui.spacing().item_spacing.x).max(160.0)));
                if button(ui, true, "Adicionar", BLURPLE).clicked() {
                    self.add_contact();
                }
            });
            if !self.error.is_empty() {
                ui.colored_label(Color32::from_rgb(0xf2, 0x7b, 0x7e), &self.error);
            }
        });

        let contacts = parse_contacts(&self.form.contacts);
        if contacts.is_empty() {
            card(ui, "Nenhum contato ainda", "", |ui| {
                note(ui, "Adicione acima o código de contato de quem você quer ouvir.");
            });
        }
        let mut remove = None;
        for (i, (name, key)) in contacts.iter().enumerate() {
            egui::Frame::new().fill(PANEL).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(18, 14)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    theme::avatar(ui, name, 48.0, None);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(name).size(18.0).strong().color(Color32::WHITE));
                            // Adicionado só pelo código: o nome chega quando o contato devolve a apresentação.
                            if *name == hex(key)[..8] {
                                ui.label(RichText::new("❗").size(15.0).color(YELLOW))
                                    .on_hover_text("O nome do contato aparecerá assim que você e ele estiverem online ao mesmo tempo.");
                            }
                        });
                        ui.label(RichText::new(&hex(key)[..16]).monospace().size(13.0).color(MUTED));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, true, "Remover", GREY_BUTTON).clicked() {
                            remove = Some(i);
                        }
                        if icon_button(ui, online && !in_call, "📞", "Ligar", GREEN).clicked() {
                            self.dial(*key);
                        }
                    });
                });
            });
            ui.add_space(2.0);
        }
        if let Some(i) = remove {
            let key = contacts[i].1;
            self.form.contacts = remove_contact(&self.form.contacts, i);
            // O endereço guardado do contato vai junto (a menos que a mesma chave ainda esteja na lista).
            if !parse_contacts(&self.form.contacts).iter().any(|(_, k)| *k == key) {
                match &self.running {
                    Some(r) => {
                        let _ = r.cmds.send(Cmd::Forget(key));
                    }
                    None => {
                        if let Err(e) = forget_known(&key).and_then(|()| mark_pending(key, false)) {
                            self.error = format!("não deu para tirar o endereço do contato de {}: {e}", known_path().display());
                        }
                    }
                }
            }
            self.form.save();
        }
    }

    fn add_contact(&mut self) {
        if split_code(&self.form.new_code).is_none() {
            self.error = "Esse código de contato não parece certo (são 64 letras e números, e o que vem depois deles tem de estar inteiro).".into();
            return;
        }
        if let Some((key, relay)) = split_direct(&self.form.new_code) {
            match (&self.running, relay) {
                (Some(r), relay) => {
                    let _ = r.cmds.send(Cmd::Contact(key, relay));
                }
                (None, relay) => {
                    // Com o nó parado: o endereço entra como dica e a apresentação fica na fila do próximo início.
                    if let Err(e) = relay.map_or(Ok(()), |r| remember_hint(key, r)).and_then(|()| mark_pending(key, true)) {
                        self.error = format!("não deu para guardar o contato em {}: {e}", known_path().display());
                    }
                }
            }
        }
        if !self.form.contacts.trim().is_empty() {
            self.form.contacts.push('\n');
        }
        self.form.contacts.push_str(self.form.new_code.trim());
        self.form.new_code.clear();
        self.error.clear();
        self.form.save();
    }
}

// ---------------------------------------------------------------------------------------------------
// Chamada e tela
// ---------------------------------------------------------------------------------------------------

impl App {
    /// Cartão para ligar a um código de contato ou ao nome de um contato.
    fn dial_card(&mut self, ui: &mut egui::Ui, offline: bool, busy: bool) {
        card(ui, "Ligar para um código", "Cole o código de contato de alguém, ou o nome de um contato.", |ui| {
            let live = !offline && !busy;
            ui.add_enabled(live, field(&mut self.form.peer, "código de contato ou nome de um contato"));
            if icon_button(ui, live, "📞", "Ligar", GREEN).clicked() {
                match self.form.target() {
                    Ok(k) => {
                        self.form.save();
                        self.dial(k);
                    }
                    Err(e) => self.error = e,
                }
            }
            if offline {
                note(ui, "Fique online primeiro.");
            }
        });
    }

    fn call_tab(&mut self, ui: &mut egui::Ui, offline: bool, busy: bool, in_call: bool) {
        if !in_call {
            self.dial_card(ui, offline, busy);
            card(ui, "Nenhuma chamada em andamento", "Quando você ligar ou atender, ela aparece aqui, com a tela compartilhada.", |ui| {
                if big_button(ui, "Escolher um contato", BLURPLE).clicked() {
                    self.tab = Tab::Contacts;
                }
            });
            return;
        }
        card(ui, "Em chamada", "", |ui| {
            status_dot(ui, GREEN, "Conversando agora");
            let link = self.shared.link.lock().unwrap().clone();
            if !link.is_empty() {
                note(ui, format!("Linha: {link}"));
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let muted = self.shared.audio.muted.load(Ordering::Relaxed);
                if icon_button(ui, true, if muted { "🔇" } else { "🎤" }, if muted { "Tirar do mudo" } else { "Silenciar" }, if muted { RED } else { GREY_BUTTON }).clicked() {
                    self.shared.audio.muted.store(!muted, Ordering::Relaxed);
                }
                if icon_button(ui, true, "✖", "Desligar a chamada", RED).clicked() {
                    self.shared.hangup.store(true, Ordering::Relaxed);
                }
            });
        });
        self.screen_ui(ui);
    }

    /// Mantém as texturas das telas que chegam em dia (a cada quadro da janela, qualquer que seja a aba).
    fn update_screen_textures(&mut self, ctx: &egui::Context) {
        let st = &self.shared.screen;
        let mut shown: Vec<([u8; 32], u64, egui::ColorImage)> = Vec::new();
        {
            let images = st.images.lock().unwrap();
            self.screen_tex.retain(|k, _| images.contains_key(k));
            for (who, (seq, img)) in images.iter() {
                if self.screen_tex.get(who).is_none_or(|(n, _)| n != seq) {
                    shown.push((*who, *seq, egui::ColorImage::from_rgba_unmultiplied([img.width, img.height], &img.rgba)));
                }
            }
        }
        for (who, seq, color) in shown {
            // Atualiza a textura que já existe: criar uma nova na GPU a cada quadro (15 por segundo, em
            // 1080p) enche a memória de vídeo de texturas à espera de serem liberadas.
            match self.screen_tex.get_mut(&who) {
                Some((n, tex)) => {
                    tex.set(color, egui::TextureOptions::LINEAR);
                    *n = seq;
                }
                None => {
                    let tex = ctx.load_texture(format!("tela-{}", who[0]), color, egui::TextureOptions::LINEAR);
                    self.screen_tex.insert(who, (seq, tex));
                }
            }
        }
    }

    /// Compartilhar a própria tela e ver a dos outros, na chamada.
    fn screen_ui(&mut self, ui: &mut egui::Ui) {
        let st = &self.shared.screen;
        let (wanted, sharing) = (st.want_share.load(Ordering::Relaxed), st.sharing.load(Ordering::Relaxed));
        let note_text = st.note.lock().unwrap().clone();
        let diag = st.diagnostics();

        card(ui, "Compartilhar a minha tela", "O vídeo segue cifrado de ponta a ponta, em um circuito só dele.", |ui| {
            ui.horizontal(|ui| {
                let (icon, tip, fill) = if wanted { ("⏹", "Parar de compartilhar", RED) } else { ("🖥", "Compartilhar minha tela", BLURPLE) };
                if icon_button(ui, true, icon, tip, fill).clicked() {
                    st.want_share.store(!wanted, Ordering::Relaxed);
                }
                if sharing {
                    ui.colored_label(GREEN, RichText::new("● sua tela está sendo vista").strong());
                } else if wanted {
                    note(ui, "ligando o circuito de vídeo...");
                }
            });
            if !wanted && !sharing && !note_text.is_empty() {
                ui.colored_label(Color32::from_rgb(0xf2, 0x7b, 0x7e), note_text);
            }
            if !diag.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new(diag).monospace().size(13.0).color(MUTED));
            }
        });

        let names = self.form.names();
        let mut order: Vec<_> = self.screen_tex.iter().collect();
        order.sort_by_key(|(k, _)| **k);
        for (who, (_, tex)) in order {
            card(ui, &format!("Tela de {}", member_name(&names, who)), "", |ui| {
                if ui.button("🔍").on_hover_text("Ampliar: abre a tela numa janela maior").clicked() {
                    self.zoom = Some(*who);
                }
                // Ampliada, a miniatura sai: a imagem só aparece na janela ampliada (e não é desenhada duas vezes).
                if self.zoom != Some(*who) {
                    ui.add(egui::Image::new(tex).max_width(ui.available_width()).corner_radius(CornerRadius::same(8)).shrink_to_fit());
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// Vídeo
// ---------------------------------------------------------------------------------------------------

impl App {
    /// Preferências da transmissão de tela: valem da próxima transmissão em diante, na chamada e nas salas.
    fn video_tab(&mut self, ui: &mut egui::Ui) {
        let st = &self.shared.screen;
        let busy = st.want_share.load(Ordering::Relaxed) || st.sharing.load(Ordering::Relaxed);
        card(ui, "Transmissão de tela", "Valem a partir da próxima transmissão e ficam salvas.", |ui| {
            if busy {
                note(ui, "Há uma transmissão em andamento: pare de compartilhar para mudar.");
            }
            ui.add_enabled_ui(!busy, |ui| {
                let changed = ui
                    .checkbox(&mut self.form.screen_private, "Privacidade na transmissão da tela")
                    .on_hover_text("Ligada, a tela sai num ritmo constante, com tráfego de enchimento, e quem observa a rede não vê quando a imagem se mexe. Desligada, só sai o que há para enviar e o ritmo máximo dobra: a imagem fica mais nítida e fluida, mas o vídeo segue cifrado. Vale na próxima transmissão.")
                    .changed();
                if changed {
                    self.form.save();
                }
                ui.horizontal(|ui| {
                    ui.label("Priorizar:");
                    let mut changed = ui
                        .radio_value(&mut self.form.screen_sharp, false, "Fluidez")
                        .on_hover_text("Mais quadros por segundo (o dobro): bom para jogo e vídeo. A imagem fica um pouco mais grosseira quando se mexe.")
                        .changed();
                    changed |= ui
                        .radio_value(&mut self.form.screen_sharp, true, "Nitidez")
                        .on_hover_text("Metade dos quadros por segundo, cada um com o dobro de bits: bom para texto, código e slides. O movimento fica menos suave.")
                        .changed();
                    if changed {
                        self.form.save();
                    }
                });
                if ui
                    .checkbox(&mut self.form.screen_hardware, "Codificar a tela pela placa de vídeo (experimental)")
                    .on_hover_text("Usa a GPU (VA-API, NVENC ou Quick Sync, via GStreamer) em vez do processador. Precisa de gstreamer1.0-tools e gstreamer1.0-plugins-bad com o driver da GPU; sem isso, ou se ela parar, o programa volta sozinho ao openh264. Menos CPU, mas sem o controle fino de nitidez por quadro, e a imagem pode demorar um pouco mais a reagir. Não funciona no Windows. Vale na próxima transmissão.")
                    .changed()
                {
                    self.form.save();
                }
            });
        });
    }
}

// ---------------------------------------------------------------------------------------------------
// Áudio
// ---------------------------------------------------------------------------------------------------

impl App {
    /// Dispositivos, mudo, volume, sensibilidade e tratamento da voz, medidor do microfone e teste do microfone.
    fn audio_tab(&mut self, ui: &mut egui::Ui, offline: bool, in_call: bool, testing: bool) {
        let shared = self.shared.clone();
        let a = &shared.audio;
        let mut muted = a.muted.load(Ordering::Relaxed);
        let before = a.prefs();
        let mut p = before;
        let level = a.level();
        let speaking = a.speaking() && !muted;

        split(ui, |ui, side| {
            if side == 0 {
                card(ui, "Dispositivos", "", |ui| {
                    ui.add_enabled(offline, egui::Checkbox::new(&mut self.form.use_audio, "Usar microfone e alto-falante"))
                        .on_hover_text("Desmarcado, cada lado manda só um tom de teste. Vale ao ficar online.");
                    #[cfg(feature = "audio")]
                    devices_ui(ui, &mut self.form, &mut self.devs);
                });
                card(ui, "Volume e microfone", "", |ui| {
                    ui.spacing_mut().slider_width = (ui.available_width() - 110.0).max(120.0);
                    if ui.checkbox(&mut muted, "Mudo (o outro lado não ouve você)").changed() {
                        a.muted.store(muted, Ordering::Relaxed);
                    }
                    ui.label("Volume do alto-falante");
                    ui.add(egui::Slider::new(&mut p.volume, AudioPrefs::VOLUME));
                    ui.label("Ganho do microfone");
                    ui.add(egui::Slider::new(&mut p.mic_gain, AudioPrefs::MIC_GAIN));
                    ui.label("Nível do microfone");
                    // Medidor: a linha marca o limiar de sensibilidade; o texto diz se a voz passa dele.
                    let meter = ui.add(
                        egui::ProgressBar::new(level)
                            .desired_width(ui.available_width())
                            .desired_height(26.0)
                            .corner_radius(CornerRadius::same(6))
                            .fill(if speaking { GREEN } else { BLURPLE })
                            .text(if speaking { "voz" } else { "" }),
                    );
                    let x = meter.rect.left() + meter.rect.width() * voice_meter(p.gate_db);
                    ui.painter().vline(x, meter.rect.y_range(), egui::Stroke::new(2.5_f32, Color32::from_rgb(230, 160, 40)));
                    ui.label("Sensibilidade (limiar da voz)");
                    ui.add(egui::Slider::new(&mut p.gate_db, AudioPrefs::GATE_DB).suffix(" dB"))
                        .on_hover_text("A linha laranja no medidor. Quanto mais para a esquerda, mais sensível: sons baixos já contam como voz.");
                    if in_call || testing {
                        note(ui, "Fale e veja o medidor: a linha laranja deve ficar abaixo da sua voz e acima do ruído.");
                    }
                    if testing {
                        if button(ui, true, "Parar o teste", RED).clicked() {
                            self.shared.mic_test.store(false, Ordering::Relaxed);
                        }
                    } else if button(ui, !in_call && !offline, "Testar o microfone", BLURPLE).clicked()
                        && let Some(r) = &self.running
                    {
                        let _ = r.cmds.send(Cmd::MicTest);
                    }
                    if offline {
                        note(ui, "Fique online para testar o microfone.");
                    }
                });
            } else {
                card(ui, "Tratamento da voz", "O que o programa faz com o som antes de mandá-lo.", |ui| {
                    ui.spacing_mut().slider_width = (ui.available_width() - 110.0).max(120.0);
                    ui.checkbox(&mut p.denoise, "Remover ruído (rede neural)")
                        .on_hover_text("Tira teclado, ventilador, estática e vozes ao fundo (RNNoise). Com ela ligada, a porta e o indicador \"voz\" só contam o que a rede reconhece como fala. Adiciona ~10 ms de atraso.");
                    ui.checkbox(&mut p.echo_cancel, "Cancelar eco do alto-falante")
                        .on_hover_text("Tira do microfone o que saiu pelo alto-falante, para o outro lado não se ouvir de volta. Leva alguns segundos de conversa para aprender o ambiente. Com fones não é preciso.");
                    ui.checkbox(&mut p.echo_suppress, "Abaixar o microfone enquanto só há eco")
                        .on_hover_text("Reforço para o eco que o cancelador deixa passar: com o outro lado falando, o microfone cai 18 dB enquanto estiver mais de 20 dB abaixo do que sai pelo alto-falante. Quem fala baixinho por cima do outro pode ser cortado; com fones, desligue.");
                    if let Some(erle) = a.echo_erle().filter(|_| p.echo_cancel && (in_call || testing)) {
                        note(ui, format!("Eco cancelado: {erle:.0} dB"));
                    }
                    ui.checkbox(&mut p.gate, "Silenciar o que não é voz")
                        .on_hover_text("Quando o microfone fica abaixo do limiar, o som enviado é reduzido. Ajuste a sensibilidade até a linha ficar logo acima do ruído.");
                    ui.add_enabled_ui(p.gate, |ui| {
                        ui.label("Redução do ruído");
                        ui.add(egui::Slider::new(&mut p.gate_depth_db, AudioPrefs::GATE_DEPTH_DB).suffix(" dB"));
                    });
                    ui.checkbox(&mut p.high_pass, "Cortar graves (ventilador, mesa, sopro)");
                    ui.checkbox(&mut p.agc, "Volume automático (nivela voz baixa e alta)")
                        .on_hover_text("Reforça quem fala baixo e contém quem fala alto. Não reforça o ruído de fundo.");
                    ui.add_enabled_ui(!in_call, |ui| {
                        let mut opus = self.form.voice_opus;
                        if ui
                            .checkbox(&mut opus, "Voz em Opus (metade da banda do G.722)")
                            .on_hover_text("Vale para as chamadas que você inicia; ao atender, vale o que o outro escolheu. O Opus usa 32 kbps contra 64 kbps do G.722 e esconde melhor a perda de pacotes, mas só funciona se o outro lado também for desta versão; contra um telefone antigo, desmarque. Vale na próxima ligação.")
                            .changed()
                        {
                            self.form.voice_opus = opus;
                            self.form.save();
                        }
                    });
                    self.shared.opus.store(self.form.voice_opus, Ordering::Relaxed);
                    if ui.button("Restaurar padrões do áudio").clicked() {
                        p = AudioPrefs::default();
                    }
                });
            }
        });

        if p != before {
            a.set_prefs(p);
            self.audio_dirty = true;
        }
        // Salva ao soltar o controle, não a cada quadro do arrasto.
        if self.audio_dirty && !ui.input(|i| i.pointer.any_down()) {
            self.audio_dirty = false;
            self.form.audio = p;
            self.form.save();
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// Configurações e atividade
// ---------------------------------------------------------------------------------------------------

impl App {
    fn settings_tab(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, offline: bool, in_call: bool, status: &str) {
        let (color, text) = self.connection(offline, status);
        card(ui, "Conexão", "Ficar online sobe o seu relay e o telefone; é daí que as chamadas chegam e saem.", |ui| {
            status_dot(ui, color, text);
            if !status.is_empty() {
                note(ui, status);
            }
            ui.add_space(4.0);
            if offline {
                if big_button(ui, "Ficar online", GREEN).clicked() {
                    self.go_online(ctx);
                }
            } else if ui.add_enabled(!in_call, egui::Button::new(RichText::new("Ficar offline").strong().size(17.0)).min_size(vec2(180.0, 46.0))).clicked()
                && let Some(r) = &self.running
            {
                r.stop.store(true, Ordering::Relaxed);
            }
            if ui.checkbox(&mut self.form.auto_online, "Tentar ficar online ao abrir o programa").changed() {
                self.form.save();
            }
            if !self.error.is_empty() {
                ui.colored_label(Color32::from_rgb(0xf2, 0x7b, 0x7e), &self.error);
            }
            note(
                ui,
                format!("Configuração: {} ({})", conf_path().display(), if self.form.loaded { "lida ao abrir" } else { "não encontrada ao abrir; será criada" }),
            );
        });
        if !offline {
            note(ui, "Fique offline para mudar estes ajustes.");
        }
        card(ui, "Meu código de contato", "Envie este código a quem vai ligar para você.", |ui| {
            let relay = self.shared.relay.lock().unwrap().clone();
            match self.form.my_code(relay.as_ref()) {
                Some(code) => {
                    egui::Frame::new().fill(DEEP).corner_radius(CornerRadius::same(8)).inner_margin(Margin::same(12)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(RichText::new(if code.len() > 72 { format!("{}...", &code[..72]) } else { code.clone() }).monospace()).wrap());
                    });
                    if button(ui, true, "Copiar o código", BLURPLE).clicked() {
                        ui.ctx().copy_text(code);
                    }
                }
                None => {
                    note(ui, "(identidade inválida: veja Configurações)");
                }
            }
        });
        ui.add_enabled_ui(offline, |ui| {
            split(ui, |ui, side| {
                if side == 0 {
                    card(ui, "Identidade", "A chave secreta (64 hex) é você. Não compartilhe com ninguém.", |ui| {
                        ui.add(field(&mut self.form.secret, "chave secreta").password(true));
                        if ui.button("Gerar nova identidade").on_hover_text("O seu código de contato muda e os outros precisarão do novo.").clicked() {
                            self.form.secret = hex(&StaticSecret::random_from_rng(OsRng).to_bytes());
                        }
                    });
                    card(ui, "Seu nome", "Aparece na lista de contatos de quem você adiciona.", |ui| {
                        ui.add(field(&mut self.form.my_name, "seu nome").char_limit(clam::media::update::MAX_NAME));
                        ui.checkbox(&mut self.form.intros, "Aceitar quem me adiciona como contato")
                            .on_hover_text("Quando alguém adiciona o seu código, ele manda uma apresentação cifrada e passa a aparecer na sua lista, para os dois se ligarem. Desligado, só entra na lista quem você adicionar. Só aceita de quem já tem o seu endereço; no máximo 20 por minuto.");
                    });
                } else {
                    card(ui, "Endereço", "Só se o IP que os outros enxergam não serve, como atrás de NAT: ip:porta (a porta precisa estar aberta no roteador; pode ser um nome).", |ui| {
                        ui.add(field(&mut self.form.public_addr, "ip:porta"));
                    });
                    card(ui, "Encontrar contatos", "", |ui| {
                        // A DHT vem ligada: a caixa é para desligá-la.
                        let mut no_dht = !self.form.use_dht;
                        let off = ui
                            .checkbox(&mut no_dht, "Não usar a DHT (BitTorrent) para reencontrar contatos")
                            .on_hover_text("Quando você e um contato mudam de endereço ao mesmo tempo, os dois se acham por ela, sem servidor. O conteúdo vai cifrado só para o contato, mas quem opera nós da DHT vê o seu IP. Com a DHT ligada, seu nó também atende a DHT para circuitos de outros nós.");
                        if off.changed() {
                            self.form.use_dht = !no_dht;
                        }
                        ui.add_enabled_ui(self.form.use_dht, |ui| {
                            ui.checkbox(&mut self.form.dht_circuit, "Falar com a DHT pelos relays dos contatos (esconde o seu IP)")
                                .on_hover_text("Usa um circuito de dois relays de contatos: o primeiro vê o seu IP mas não o pedido, o segundo vê o pedido mas não o seu IP. Precisa de ao menos dois contatos com endereço guardado; sem eles a DHT não é usada.");
                        });
                    });
                    card(ui, "Âncoras", "Guardam o endereço cifrado de quem as usa, para os contatos o acharem.", |ui| {
                        // Servir de âncora vem ligado: a caixa é para não servir.
                        let mut no_anchor = !self.form.anchor;
                        let off = ui
                            .checkbox(&mut no_anchor, "Não servir de âncora para os registros de endereço dos outros")
                            .on_hover_text("Guarda, em memória, o endereço cifrado de quem usa este computador como âncora, para os contatos dele o acharem. Não lê o conteúdo nem forja item. Precisa de um endereço que os outros alcancem; quem a usa vê o IP do relay dela.");
                        if off.changed() {
                            self.form.anchor = !no_anchor;
                        }
                        ui.label("Âncoras que você usa (uma por linha: ip:porta#chave-do-circuito#chave-do-link):");
                        ui.add(egui::TextEdit::multiline(&mut self.form.anchors).desired_rows(3).desired_width(f32::INFINITY).margin(Margin::symmetric(12, 10)));
                    });
                }
            });
        });
        if self.form.anchor
            && let Some(r) = self.shared.relay.lock().unwrap().clone()
        {
            card(ui, "Código da sua âncora", "", |ui| {
                ui.horizontal(|ui| {
                    ui.monospace(format!("{}...", &anchor_code(&r)[..24]));
                    if ui.button("Copiar").clicked() {
                        ui.ctx().copy_text(anchor_code(&r));
                    }
                });
            });
        }
    }

    fn log_tab(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new().fill(DEEP).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(14)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
                let lines = self.log.lock().unwrap();
                if lines.is_empty() {
                    note(ui, "Nada ainda. Fique online e o que o programa faz aparece aqui.");
                }
                for l in lines.iter() {
                    ui.label(RichText::new(l).monospace().size(13.5));
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
        let due = LAST_NOTE.with(|l| l.get()).is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if due {
            LAST_NOTE.with(|l| l.set(Some(std::time::Instant::now())));
            self.log.lock().unwrap().push(line);
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
        let watching = !self.shared.screen.images.lock().unwrap().is_empty();
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
            self.shared.room.lock().unwrap().clear();
            self.shared.status.lock().unwrap().clear();
            *self.shared.relay.lock().unwrap() = None;
        }
        self.absorb_introductions();
        self.update_screen_textures(ctx);
        self.zoom_window(ctx);
        let offline = self.running.is_none();
        let in_call = self.shared.in_call.load(Ordering::Relaxed);
        let testing = self.shared.mic_test.load(Ordering::Relaxed);
        let calling = in_call && !testing;
        // Quando a chamada começa, a aba dela abre sozinha.
        let in_room = self.shared.room.lock().unwrap().at.is_some();
        if calling && !self.was_in_call && !in_room {
            self.tab = Tab::Call;
        }
        self.was_in_call = calling;
        // As preferências de tela são lidas ao começar a transmissão, esteja a pessoa em que aba estiver.
        let st = &self.shared.screen;
        st.open_path.store(!self.form.screen_private, Ordering::Relaxed);
        st.sharp.store(self.form.screen_sharp, Ordering::Relaxed);
        st.hardware.store(self.form.screen_hardware, Ordering::Relaxed);
        let status = self.shared.status.lock().unwrap().clone();

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
