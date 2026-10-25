//! Janela (egui): desenha o formulário e fala com o motor.

use std::{sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
}, time::Duration};

use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};
use eframe::egui;
use tokio::sync::mpsc;

use clam::room;

use crate::{conf::*, engine::*};


pub(crate) struct App {
    pub(crate) rt: tokio::runtime::Runtime,
    pub(crate) form: Form,
    pub(crate) log: Log,
    pub(crate) shared: Arc<Shared>,
    pub(crate) running: Option<Running>,
    pub(crate) done: Arc<AtomicBool>,
    pub(crate) error: String,
    /// Membro cujo "Remover" espera confirmação.
    pub(crate) confirm_remove: Option<[u8; 32]>,
    /// Microfones e alto-falantes que o sistema listou (atualizável na janela).
    #[cfg_attr(not(feature = "audio"), allow(dead_code))]
    pub(crate) devs: (Vec<String>, Vec<String>),
    /// Textura da tela de quem transmite para você e o número da imagem que ela mostra.
    /// Tela de quem está aberta numa janela própria, em tamanho grande.
    pub(crate) zoom: Option<[u8; 32]>,
    pub(crate) screen_tex: std::collections::HashMap<[u8; 32], (u64, egui::TextureHandle)>,
}

impl App {
    /// Salas guardadas neste computador: entrar de novo é um clique, sem combinar nada.
    pub(crate) fn rooms_ui(&mut self, ui: &mut egui::Ui, offline: bool, in_room: bool, in_call: bool) {
        ui.weak("Ficam salvas neste computador. Entre de novo quando quiser; o primeiro a entrar é o anfitrião.");
        // O dono trocou o segredo de alguma sala (alguém foi removido): guarda o novo.
        let changed: Vec<_> = std::mem::take(&mut *self.shared.rekeyed.lock().unwrap());
        if !changed.is_empty() {
            for (old, new) in changed {
                self.form.replace_room_secret(&old, &new);
            }
            self.form.save();
        }
        let rooms = parse_rooms(&self.form.rooms);
        let me = self.form.my_key();
        let mut forget = None;
        for (i, room) in rooms.iter().enumerate() {
            let SavedRoom { name, secret, owner } = room;
            ui.horizontal(|ui| {
                ui.label(name);
                if owner.is_some() && *owner == me {
                    ui.weak("(você é o dono)");
                }
                if ui.add_enabled(!offline && !in_call, egui::Button::new("Entrar")).clicked()
                    && let Some(r) = &self.running
                {
                    let _ = r.cmds.send(Cmd::Join(*secret, *owner, name.clone()));
                }
                if ui.button("Copiar código").clicked() {
                    let dir = self.form.directory_addr();
                    let code = room.code();
                    ui.ctx().copy_text(if dir.is_empty() { code } else { format!("{code}@{dir}") });
                }
                if ui.add_enabled(!in_room, egui::Button::new("Esquecer")).clicked() {
                    forget = Some(i);
                }
            });
        }
        if let Some(i) = forget {
            let kept: Vec<SavedRoom> = rooms.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, r)| r.clone()).collect();
            self.form.rooms = format_rooms(&kept);
            self.form.save();
        }
        ui.horizontal(|ui| {
            ui.label("Nova sala:");
            ui.add(egui::TextEdit::singleline(&mut self.form.room_name).hint_text("nome").desired_width(110.0));
            ui.label("Código:");
            ui.add(egui::TextEdit::singleline(&mut self.form.room_code).hint_text("vazio = criar; ou cole o código de uma sala").desired_width(280.0));
            if ui.add_enabled(!offline && !in_call, egui::Button::new("Criar / entrar")).clicked() {
                // Sala nova: você é o dono. Com código: o dono vem no código (se o código tiver um).
                let fresh = if self.form.room_code.trim().is_empty() {
                    me.map(|m| (room::new_secret(), Some(m)))
                } else {
                    split_room_code(&self.form.room_code)
                };
                match fresh {
                    Some((secret, owner)) => {
                        let name: String = self.form.room_name.chars().filter(|c| !matches!(c, ';' | '\n')).collect();
                        let name = if name.trim().is_empty() { format!("sala {}", rooms.len() + 1) } else { name.trim().to_string() };
                        if !rooms.iter().any(|r| r.secret == secret) {
                            if !self.form.rooms.trim().is_empty() {
                                self.form.rooms.push('\n');
                            }
                            self.form.rooms.push_str(&format!("{name} {}", SavedRoom { name: String::new(), secret, owner }.code()));
                        }
                        self.form.room_name.clear();
                        self.form.room_code.clear();
                        self.error.clear();
                        self.form.save();
                        if let Some(r) = &self.running {
                            let _ = r.cmds.send(Cmd::Join(secret, owner, name));
                        }
                    }
                    None => self.error = "código de sala inválido (64 hex, com @host:porta opcional)".into(),
                }
            }
            if ui.add_enabled(in_room, egui::Button::new("Sair da sala")).clicked()
                && let Some(r) = &self.running
            {
                let _ = r.cmds.send(Cmd::Leave);
            }
        });
        let status = self.shared.room.lock().unwrap().clone();
        if !status.is_empty() {
            ui.label(status);
        }
        self.members_ui(ui);
    }

    /// Escolha de microfone e alto-falante (vazio = padrão do sistema); vale na próxima chamada.
    #[cfg(feature = "audio")]
    fn devices_ui(&mut self, ui: &mut egui::Ui, offline: bool) {
        let _ = offline;
        let pick = |ui: &mut egui::Ui, label: &str, cur: &mut String, list: &[String]| {
            ui.horizontal(|ui| {
                ui.label(label);
                let shown = if cur.is_empty() { "padrão do sistema".to_string() } else { cur.clone() };
                egui::ComboBox::from_id_salt(label).selected_text(shown).width(320.0).show_ui(ui, |ui| {
                    ui.selectable_value(cur, String::new(), "padrão do sistema");
                    for d in list {
                        ui.selectable_value(cur, d.clone(), d);
                    }
                });
            });
        };
        let (ins, outs) = self.devs.clone();
        pick(ui, "Microfone", &mut self.form.input_dev, &ins);
        pick(ui, "Alto-falante", &mut self.form.output_dev, &outs);
        if ui.small_button("Atualizar lista de dispositivos").clicked() {
            self.devs = clam::audio::devices();
        }
    }

    /// Quem está na sala: indicador de fala, apelido e volume de cada pessoa.
    fn members_ui(&mut self, ui: &mut egui::Ui) {
        let members = self.shared.members.lock().unwrap().clone();
        let mesh = self.shared.mesh.lock().unwrap().clone();
        let me = *self.shared.me.lock().unwrap();
        if members.is_empty() {
            return;
        }
        let names = self.form.names();
        let i_own = self.shared.owner.load(Ordering::Relaxed);
        let mut nick_changes = Vec::new();
        let mut remove = None;
        for key in members {
            ui.horizontal(|ui| {
                let (live, speaking) = match (&mesh, key == me) {
                    (_, true) => (true, false),
                    (Some(m), false) => (m.has(&key), m.is_speaking(&key)),
                    _ => (false, false),
                };
                let color = if speaking {
                    egui::Color32::from_rgb(60, 190, 90)
                } else if live {
                    egui::Color32::GRAY
                } else {
                    egui::Color32::from_rgb(200, 150, 60)
                };
                let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), if speaking { 6.0 } else { 4.0 }, color);
                if key == me {
                    ui.label("você");
                    return;
                }
                let mut nick = self.form.nick(&key);
                let hint = member_name(&names, &key);
                if ui.add(egui::TextEdit::singleline(&mut nick).hint_text(hint).desired_width(130.0)).changed() {
                    nick_changes.push((key, nick));
                }
                if let Some(m) = &mesh {
                    let mut v = m.volume(&key);
                    if ui.add(egui::Slider::new(&mut v, 0.0..=room::MAX_VOLUME).show_value(false)).changed() {
                        m.set_volume(&key, v);
                    }
                    ui.label(if v == 0.0 { "mudo".to_string() } else { format!("{:.0}%", v * 100.0) });
                }
                if !live {
                    ui.label("conectando...");
                }
                if i_own {
                    if self.confirm_remove == Some(key) {
                        let who = member_name(&names, &key);
                        ui.colored_label(egui::Color32::from_rgb(200, 70, 60), format!("Remover {who}?"));
                        if ui.button("Sim, remover").clicked() {
                            remove = Some(key);
                        }
                        if ui.button("Cancelar").clicked() {
                            self.confirm_remove = None;
                        }
                    } else if ui.button("Remover").on_hover_text("Troca o segredo da sala: só quem fica recebe o novo").clicked() {
                        self.confirm_remove = Some(key);
                    }
                }
            });
        }
        if let Some(key) = remove {
            self.confirm_remove = None;
            if let Some(r) = &self.running {
                let _ = r.cmds.send(Cmd::Remove(key));
            }
        }
        if !nick_changes.is_empty() {
            for (k, n) in nick_changes {
                self.form.set_nick(&k, &n);
            }
            self.form.save();
        }
    }

    pub(crate) fn go_online(&mut self, ctx: &egui::Context) {
        self.error.clear();
        if let Err(e) = self.form.identity() {
            self.error = e;
            return;
        }
        // Sem o campo preenchido, vale o servidor que veio num código de contato (`chave@host:porta`).
        if self.form.directory.trim().is_empty() {
            self.form.directory = self.form.directory_addr();
        }
        if self.form.directory.trim().is_empty() {
            self.error = "Informe o servidor (diretório), no formato endereço:porta.".into();
            return;
        }
        if let Err(e) = self.form.save_checked() {
            self.error = format!("não deu para gravar a configuração em {}: {e}; o servidor e os contatos não ficam salvos", conf_path().display());
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
}

pub(crate) fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(title).strong().size(15.0));
        ui.add_space(4.0);
        add(ui);
    });
    ui.add_space(8.0);
}

impl App {
    /// Mudo, volume, ganho e medidor do microfone, mais o teste do microfone.
    fn audio_ui(&mut self, ui: &mut egui::Ui, in_call: bool, testing: bool) {
        let a = &self.shared.audio;
        let mut muted = a.muted.load(Ordering::Relaxed);
        if ui.checkbox(&mut muted, "Mudo (o outro lado não ouve você)").changed() {
            a.muted.store(muted, Ordering::Relaxed);
        }
        let (mut volume, mut gain) = (a.volume(), a.mic_gain());
        if ui.add(egui::Slider::new(&mut volume, 0.0..=2.0).text("Volume do alto-falante")).changed() {
            a.set_volume(volume);
        }
        if ui.add(egui::Slider::new(&mut gain, 0.0..=3.0).text("Ganho do microfone")).changed() {
            a.set_mic_gain(gain);
        }
        if in_call || testing {
            ui.add(egui::ProgressBar::new(a.level()).desired_width(260.0).text("Microfone"));
        }
        if testing {
            if ui.button("Parar o teste").clicked() {
                self.shared.mic_test.store(false, Ordering::Relaxed);
            }
        } else if ui.add_enabled(!in_call, egui::Button::new("Testar o microfone")).clicked()
            && let Some(r) = &self.running
        {
            let _ = r.cmds.send(Cmd::MicTest);
        }
    }

    pub(crate) fn contacts_ui(&mut self, ui: &mut egui::Ui, online: bool, in_call: bool) {
        let contacts = parse_contacts(&self.form.contacts);
        if contacts.is_empty() {
            ui.weak("Nenhum contato ainda. Peça o código de contato de um amigo e adicione abaixo.");
        }
        let mut remove = None;
        for (i, (name, key)) in contacts.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(name).size(14.0));
                ui.weak(&hex(key)[..8]);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Remover").clicked() {
                        remove = Some(i);
                    }
                    if ui.add_enabled(online && !in_call, egui::Button::new("📞 Ligar")).clicked()
                        && let Some(r) = &self.running
                    {
                        self.error.clear();
                        let _ = r.cmds.send(Cmd::Dial(*key));
                    }
                });
            });
        }
        if let Some(i) = remove {
            self.form.contacts = remove_contact(&self.form.contacts, i);
            self.form.save();
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.form.new_name).hint_text("Nome").desired_width(110.0));
            ui.add(egui::TextEdit::singleline(&mut self.form.new_code).hint_text("Código de contato").desired_width(260.0));
            if ui.button("Adicionar contato").clicked() {
                if split_code(&self.form.new_code).is_none() {
                    self.error = "Esse código de contato não parece certo (são 64 letras e números).".into();
                } else {
                    let name: String = self.form.new_name.chars().filter(|c| !matches!(c, ';' | '\n')).collect();
                    let line = format!("{} {}", name.trim(), self.form.new_code.trim());
                    if !self.form.contacts.trim().is_empty() {
                        self.form.contacts.push('\n');
                    }
                    self.form.contacts.push_str(line.trim());
                    self.form.new_name.clear();
                    self.form.new_code.clear();
                    self.error.clear();
                    self.form.save();
                }
            }
        });
    }
}

impl App {
    /// Compartilhar a própria tela e ver a dos outros, na chamada ou na sala.
    fn screen_ui(&mut self, ui: &mut egui::Ui) {
        let st = &self.shared.screen;
        let (wanted, sharing) = (st.want_share.load(Ordering::Relaxed), st.sharing.load(Ordering::Relaxed));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let label = if wanted { "⏹ Parar de compartilhar a tela" } else { "🖥 Compartilhar minha tela" };
            if ui.button(label).clicked() {
                st.want_share.store(!wanted, Ordering::Relaxed);
            }
            if sharing {
                ui.colored_label(egui::Color32::LIGHT_GREEN, "● sua tela está sendo vista");
            } else if wanted {
                ui.weak("ligando o circuito de vídeo...");
            }
        });
        if !wanted && !sharing {
            let note = st.note.lock().unwrap().clone();
            if !note.is_empty() {
                ui.colored_label(egui::Color32::LIGHT_RED, note);
            }
        }
        let diag = st.diagnostics();
        if !diag.is_empty() {
            ui.weak(diag);
        }
        let names = self.form.names();
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
            let tex = ui.ctx().load_texture(format!("tela-{}", who[0]), color, egui::TextureOptions::LINEAR);
            self.screen_tex.insert(who, (seq, tex));
        }
        let mut order: Vec<_> = self.screen_tex.iter().collect();
        order.sort_by_key(|(k, _)| **k);
        for (who, (_, tex)) in order {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(format!("Tela de {}:", crate::engine::member_name(&names, who)));
                if ui.button("🔍 Ampliar").on_hover_text("Abre a tela numa janela maior").clicked() {
                    self.zoom = Some(*who);
                }
            });
            ui.add(egui::Image::new(tex).max_width(ui.available_width()).shrink_to_fit());
        }
    }
}

impl App {
    /// Janela própria com a tela de `zoom`, ajustada ao tamanho da janela; fecha sozinha se a tela some.
    fn zoom_window(&mut self, ctx: &egui::Context) {
        let Some(who) = self.zoom else { return };
        let Some((_, tex)) = self.screen_tex.get(&who) else {
            self.zoom = None;
            return;
        };
        let title = format!("Tela de {}", crate::engine::member_name(&self.form.names(), &who));
        let size = tex.size_vec2();
        let mut close = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("tela-ampliada"),
            egui::ViewportBuilder::default().with_title(title).with_inner_size([size.x.min(1600.0), size.y.min(900.0)]),
            |ctx, _| {
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
                    ui.add(egui::Image::new(tex).fit_to_exact_size(ui.available_size()).maintain_aspect_ratio(true));
                });
                close = ctx.input(|i| i.viewport().close_requested());
            },
        );
        if close {
            self.zoom = None;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if self.done.swap(false, Ordering::Relaxed) {
            self.running = None;
            self.shared.in_call.store(false, Ordering::Relaxed);
            self.shared.in_room.store(false, Ordering::Relaxed);
            self.shared.status.lock().unwrap().clear();
            self.shared.room.lock().unwrap().clear();
        }
        self.zoom_window(ctx);
        let offline = self.running.is_none();
        let in_call = self.shared.in_call.load(Ordering::Relaxed);
        let in_room = self.shared.in_room.load(Ordering::Relaxed);
        let status = self.shared.status.lock().unwrap().clone();
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Clam");
                    ui.weak("chamadas de voz entre pessoas, sem servidor no meio");
                });
                ui.add_space(6.0);

                card(ui, "Conexão", |ui| {
                    ui.horizontal(|ui| {
                        let (color, text) = if offline {
                            (egui::Color32::GRAY, "Offline".to_string())
                        } else if status.is_empty() {
                            (egui::Color32::YELLOW, "Conectando...".to_string())
                        } else {
                            (egui::Color32::LIGHT_GREEN, "Online".to_string())
                        };
                        ui.colored_label(color, format!("● {text}"));
                        if !status.is_empty() {
                            ui.weak(&status);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Servidor (diretório):");
                        ui.add_enabled(
                            offline,
                            egui::TextEdit::singleline(&mut self.form.directory).hint_text("exemplo: meu-servidor.com:7000").desired_width(260.0),
                        );
                    });
                    ui.weak(format!(
                        "Configuração: {} ({})",
                        conf_path().display(),
                        if self.form.loaded { "lida ao abrir" } else { "não encontrada ao abrir; será criada" }
                    ));
                    ui.add_enabled(offline, egui::Checkbox::new(&mut self.form.use_audio, "Usar microfone e alto-falante (use fones para não dar eco)"));
                    #[cfg(feature = "audio")]
                    self.devices_ui(ui, offline);
                    ui.add_space(4.0);
                    if offline {
                        if ui.add_sized([160.0, 30.0], egui::Button::new("Ficar online")).clicked() {
                            self.go_online(ctx);
                        }
                    } else if ui.add_enabled(!in_call, egui::Button::new("Ficar offline")).clicked()
                        && let Some(r) = &self.running
                    {
                        r.stop.store(true, Ordering::Relaxed);
                    }
                    if !self.error.is_empty() {
                        ui.colored_label(egui::Color32::LIGHT_RED, &self.error);
                    }
                });

                let testing = self.shared.mic_test.load(Ordering::Relaxed);
                if in_call && !in_room && !testing {
                    card(ui, "Em chamada", |ui| {
                        ui.label("Conversando agora.");
                        let link = self.shared.link.lock().unwrap().clone();
                        if !link.is_empty() {
                            ui.label(format!("Linha: {link}"));
                        }
                        if ui.add_sized([160.0, 30.0], egui::Button::new("Desligar")).clicked() {
                            self.shared.hangup.store(true, Ordering::Relaxed);
                        }
                        self.screen_ui(ui);
                    });
                }

                if !offline && self.form.use_audio {
                    card(ui, "Áudio", |ui| self.audio_ui(ui, in_call, testing));
                }

                card(ui, "Meu código de contato", |ui| {
                    ui.weak("Envie este código a quem vai ligar para você.");
                    match self.form.my_code() {
                        Some(code) => {
                            ui.horizontal(|ui| {
                                ui.monospace(if code.len() > 36 { format!("{}...", &code[..36]) } else { code.clone() });
                                if ui.button("Copiar").clicked() {
                                    ui.ctx().copy_text(code);
                                }
                            });
                        }
                        None => {
                            ui.weak("(identidade inválida: veja Configurações)");
                        }
                    }
                });

                card(ui, "Contatos", |ui| self.contacts_ui(ui, !offline, in_call));

                card(ui, "Ligar para um código", |ui| {
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            !offline && !in_call,
                            egui::TextEdit::singleline(&mut self.form.peer).hint_text("cole o código de contato ou o nome de um contato").desired_width(380.0),
                        );
                        if ui.add_enabled(!offline && !in_call, egui::Button::new("📞 Ligar")).clicked() {
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
                    });
                    if offline {
                        ui.weak("Fique online primeiro.");
                    }
                });

                card(ui, "Salas", |ui| {
                    self.rooms_ui(ui, offline, in_room, in_call);
                    if in_room {
                        self.screen_ui(ui);
                    }
                });

                egui::CollapsingHeader::new("Configurações avançadas").show(ui, |ui| {
                    ui.add_enabled_ui(offline, |ui| {
                        ui.label("Identidade (chave secreta, 64 hex). Não compartilhe com ninguém.");
                        ui.add(egui::TextEdit::singleline(&mut self.form.secret).password(true).desired_width(f32::INFINITY));
                        if ui.button("Gerar nova identidade").on_hover_text("O seu código de contato muda e os outros precisarão do novo.").clicked() {
                            self.form.secret = hex(&StaticSecret::random_from_rng(OsRng).to_bytes());
                        }
                        ui.separator();
                        ui.label("Endereço público ip:porta (só se o servidor enxergar um IP que não serve, como atrás de NAT; a porta precisa estar aberta no roteador):");
                        ui.text_edit_singleline(&mut self.form.public_addr);
                    });
                });

                egui::CollapsingHeader::new("Registro de atividade").default_open(true).show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(220.0).stick_to_bottom(true).show(ui, |ui| {
                        for l in self.log.lock().unwrap().iter() {
                            ui.monospace(l);
                        }
                    });
                });
            });
        });
        // Com tela chegando a janela repinta a cada ~16 ms; a 300 ms o vídeo parecia ter 3 quadros/s.
        let watching = !self.shared.screen.images.lock().unwrap().is_empty();
        ctx.request_repaint_after(Duration::from_millis(if watching { 16 } else { 300 }));
    }
}
