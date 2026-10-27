//! Aba de servidores: trilho com os servidores, salas de voz com quem está nelas e membros online/offline.

use std::sync::atomic::Ordering;

use clam::server::{Output, Role, ServerId, ServerView};
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, vec2};

use crate::{conf::*, engine::*, theme::*, ui::App};

/// O que a aba lembra entre um quadro e outro.
#[derive(Default)]
pub(crate) struct ServersUi {
    /// Servidor aberto; `None` mostra a tela de criar/aceitar.
    pub(crate) sel: Option<ServerId>,
    pub(crate) new_server: String,
    pub(crate) invite_in: String,
    pub(crate) new_room: String,
    /// Contato escolhido para convidar.
    pub(crate) pick: Option<[u8; 32]>,
    /// A pessoa leu o aviso de que o convidado passa a ver o IP dela.
    pub(crate) ip_ok: bool,
    /// A pessoa leu o aviso de que, ao entrar, os membros passam a ver o IP dela.
    pub(crate) join_ok: bool,
    /// Convite gerado, para copiar.
    pub(crate) invite_out: String,
    pub(crate) confirm_leave: bool,
    pub(crate) msg: String,
    /// Resultado que deu certo (aparece em verde).
    pub(crate) info: String,
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Owner => "dono",
        Role::Admin => "admin",
        Role::Member => "membro",
    }
}

impl App {
    pub(crate) fn servers_tab(&mut self, ui: &mut egui::Ui, offline: bool, busy: bool) {
        let hub = self.shared.servers.lock().unwrap().clone();
        let Some(hub) = hub.filter(|_| !offline) else {
            card(ui, "Servidores", "Grupos de pessoas com salas de voz, sem servidor no meio: cada membro guarda o grupo e fica de pé enquanto estiver online.", |ui| {
                note(ui, "Fique online (aba Início) para ver e usar os seus servidores.");
            });
            return;
        };
        let views = hub.read(|s, now| s.views(now));
        if self.srv.sel.is_some_and(|id| !views.iter().any(|v| v.id == id)) {
            self.srv.sel = None;
        }
        self.invitations_card(ui, &hub);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(60.0);
                for v in &views {
                    let on = self.srv.sel == Some(v.id);
                    let initial = v.name.chars().next().map_or("?".to_string(), |c| c.to_uppercase().to_string());
                    let reply = server_icon(ui, &initial, if on { BLURPLE } else { PANEL }, Color32::WHITE, if on { 14 } else { 24 });
                    if reply.on_hover_text(&v.name).clicked() {
                        self.srv.sel = Some(v.id);
                        self.srv.msg.clear();
                        self.srv.info.clear();
                        self.srv.invite_out.clear();
                        self.srv.confirm_leave = false;
                    }
                    ui.add_space(4.0);
                }
                let plus = server_icon(ui, "＋", PANEL, GREEN, 24);
                if plus.on_hover_text("Criar ou entrar em um servidor").clicked() {
                    self.srv.sel = None;
                }
            });
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                match views.iter().find(|v| Some(v.id) == self.srv.sel) {
                    Some(v) => self.server_view(ui, &hub, v, busy),
                    None => self.server_add(ui, &hub),
                }
                if !self.srv.info.is_empty() {
                    ui.colored_label(GREEN, &self.srv.info);
                }
                if !self.srv.msg.is_empty() {
                    ui.colored_label(Color32::from_rgb(0xf2, 0x7b, 0x7e), &self.srv.msg);
                }
            });
        });
    }

    /// Convites que contatos mandaram pela rede.
    fn invitations_card(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub) {
        let pending = hub.read(|s, _| s.pending_invites());
        if pending.is_empty() {
            return;
        }
        let contacts = parse_contacts(&self.form.contacts);
        card(ui, "Convites recebidos", "Um contato chamou você para um servidor.", |ui| {
            ui.checkbox(&mut self.srv.join_ok, "Entendo que, ao entrar, os membros passam a ver o meu IP quando ligarmos na mesma sala");
            for p in &pending {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&p.name).strong());
                    ui.label(format!("de {}", member_name(&contacts, &p.from)));
                    if button(ui, self.srv.join_ok, "Aceitar", GREEN).clicked() {
                        match hub.try_run(|s, now| s.accept(&p.text, now)) {
                            Ok(id) => {
                                self.srv.sel = Some(id);
                                self.srv.msg.clear();
                            }
                            Err(e) => self.srv.msg = e.to_string(),
                        }
                    }
                    if button(ui, true, "Recusar", PANEL).clicked() {
                        hub.run(|s, _| {
                            s.decline_invite(&p.server);
                            Output::default()
                        });
                    }
                });
            }
        });
    }

    fn server_add(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub) {
        card(ui, "Criar um servidor", "Você vira o dono. Convide depois quem estiver nos seus contatos.", |ui| {
            ui.add(field(&mut self.srv.new_server, "Nome do servidor"));
            if button(ui, !self.srv.new_server.trim().is_empty(), "Criar", GREEN).clicked() {
                let name = self.srv.new_server.trim().to_string();
                match hub.try_run(|s, now| s.create(&name, now)) {
                    Ok(id) => {
                        self.srv.sel = Some(id);
                        self.srv.new_server.clear();
                        self.srv.msg.clear();
                    }
                    Err(e) => self.srv.msg = e.to_string(),
                }
            }
        });
        card(ui, "Entrar com um convite", "Convites de contatos chegam sozinhos (aparecem acima). Se alguém mandou o texto por fora, cole aqui (começa com clam-servidor:).", |ui| {
            ui.add(field(&mut self.srv.invite_in, "clam-servidor:..."));
            ui.checkbox(&mut self.srv.join_ok, "Entendo que, ao entrar, os membros passam a ver o meu IP");
            if button(ui, !self.srv.invite_in.trim().is_empty() && self.srv.join_ok, "Entrar", BLURPLE).clicked() {
                let text = self.srv.invite_in.trim().to_string();
                match hub.try_run(|s, now| s.accept(&text, now)) {
                    Ok(id) => {
                        self.srv.sel = Some(id);
                        self.srv.invite_in.clear();
                        self.srv.msg.clear();
                    }
                    Err(e) => self.srv.msg = e.to_string(),
                }
            }
            note(ui, "O servidor aparece com os membros quando o convidante responder (ele precisa estar online).");
        });
        let visible = hub.read(|s, _| s.visible());
        card(ui, "Presença", "", |ui| {
            let mut v = visible;
            if ui.checkbox(&mut v, "Aparecer online para os membros dos meus servidores").changed() {
                hub.run(|s, now| s.set_visible(v, now));
            }
            note(ui, "Desmarcado, você não avisa ninguém que está online e não aparece nas salas; ainda vê o que os outros anunciam.");
        });
    }

    fn server_view(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView, busy: bool) {
        let contacts = parse_contacts(&self.form.contacts);
        let name_of = |key: &[u8; 32], given: &str| if given.is_empty() { member_name(&contacts, key) } else { given.to_string() };
        let online = v.members.iter().filter(|m| m.online).count();
        let manages = v.my_role.is_some_and(|r| r.manages());
        let my_room = self.shared.room.lock().unwrap().at;
        card(ui, &v.name, &format!("{} membro(s), {online} online · você é {}", v.members.len(), v.my_role.map_or("convidado", role_name)), |ui| {
            if v.joining {
                note(ui, "Aguardando o convidante responder para trazer o servidor (ele precisa estar online).");
            }
            ui.horizontal(|ui| {
                let mut visible = hub.read(|s, _| s.visible());
                if ui.checkbox(&mut visible, "Aparecer online").on_hover_text("Desmarcado, ninguém vê que você está online").changed() {
                    hub.run(|s, now| s.set_visible(visible, now));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.srv.confirm_leave {
                        if button(ui, true, "Confirmar saída", RED).clicked() {
                            if let Err(e) = hub.try_act(|s, now| s.leave(&v.id, now)) {
                                self.srv.msg = e.to_string();
                            }
                            self.srv.confirm_leave = false;
                            self.srv.sel = None;
                        }
                    } else if button(ui, true, "Sair do servidor", GREY_BUTTON).clicked() {
                        self.srv.confirm_leave = true;
                    }
                });
            });
        });

        card(ui, "Salas de voz", "Quem entra numa sala fala com todos que estão nela (até 8 pessoas), ligação direta entre cada par.", |ui| {
            for room in &v.rooms {
                let here = my_room == Some((v.id, room.id));
                egui::Frame::new().fill(DEEP).corner_radius(CornerRadius::same(8)).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("🔊  {}", room.name)).strong().size(17.0).color(Color32::WHITE));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if here {
                                if button(ui, true, "Sair", RED).clicked() {
                                    self.send(Cmd::LeaveRoom);
                                }
                            } else if button(ui, !busy, "Entrar", GREEN).clicked() {
                                self.send(Cmd::JoinRoom(v.id, room.id));
                            }
                            if manages && ui.add(egui::Button::new("🗑").frame(false)).on_hover_text("Apagar a sala").clicked() {
                                if let Err(e) = hub.try_act(|s, now| s.delete_room(&v.id, room.id, now)) {
                                    self.srv.msg = e.to_string();
                                }
                            }
                        });
                    });
                    let live = self.shared.room.lock().unwrap();
                    let (sharing, mut action) = (self.shared.screen.want_share.load(Ordering::Relaxed), None);
                    for id in &room.people {
                        let Some(m) = v.members.iter().find(|m| m.id == *id) else { continue };
                        let speaking = live.speaking(&m.key);
                        ui.horizontal(|ui| {
                            ui.add_space(14.0);
                            let who = name_of(&m.key, &m.name);
                            avatar(ui, &who, 26.0, speaking.then_some(GREEN));
                            let text = RichText::new(if m.is_me { format!("{who} (você)") } else { who }).color(if speaking { GREEN } else { TEXT });
                            ui.label(if speaking { text.strong() } else { text });
                            if m.sharing {
                                ui.label(RichText::new("🖥 transmitindo").size(13.0).color(GREEN));
                            }
                            if here && m.is_me {
                                let (label, fill) = if sharing { ("Parar de compartilhar", RED) } else { ("🖥 Compartilhar tela", BLURPLE) };
                                if button(ui, true, label, fill).clicked() {
                                    action = Some((None, !sharing));
                                }
                            } else if here && m.sharing {
                                let on = live.watching.contains(&m.key);
                                if button(ui, true, if on { "Parar de assistir" } else { "Assistir" }, if on { GREY_BUTTON } else { GREEN }).clicked() {
                                    action = Some((Some(m.key), !on));
                                }
                            }
                        });
                    }
                    drop(live);
                    match action {
                        Some((None, on)) => self.send(Cmd::RoomShare(on)),
                        Some((Some(key), on)) => crate::engine::follow(&self.shared, key, on),
                        None => {}
                    }
                });
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                ui.add(field(&mut self.srv.new_room, "Nome da nova sala").desired_width(260.0));
                if button(ui, !self.srv.new_room.trim().is_empty(), "Criar sala", BLURPLE).clicked() {
                    let name = self.srv.new_room.trim().to_string();
                    match hub.try_run(|s, now| s.create_room(&v.id, &name, now)) {
                        Ok(_) => {
                            self.srv.new_room.clear();
                            self.srv.msg.clear();
                        }
                        Err(e) => self.srv.msg = e.to_string(),
                    }
                }
            });
            if my_room.is_some() {
                note(ui, "Para trocar de sala, saia da atual primeiro.");
            }
        });

        if my_room.is_some_and(|(server, _)| server == v.id) {
            let (sharing, note_text) = (self.shared.screen.sharing.load(Ordering::Relaxed), self.shared.screen.note.lock().unwrap().clone());
            let wanted = self.shared.screen.want_share.load(Ordering::Relaxed);
            if wanted || !self.screen_tex.is_empty() || !note_text.is_empty() {
                let names = self.form.names();
                let diag = self.shared.screen.diagnostics();
                card(ui, "Telas da sala", "Cada espectador abre um circuito de vídeo próprio, cifrado, direto com quem transmite (até 3 por transmissão).", |ui| {
                    if wanted {
                        ui.colored_label(GREEN, if sharing { "● sua tela está sendo vista" } else { "Sua tela está disponível: quem está na sala vê o botão Assistir ao lado do seu nome." });
                    }
                    if !wanted && !note_text.is_empty() {
                        ui.colored_label(Color32::from_rgb(0xf2, 0x7b, 0x7e), note_text);
                    }
                    if !diag.is_empty() {
                        ui.label(RichText::new(diag).monospace().size(13.0).color(MUTED));
                    }
                    let mut order: Vec<_> = self.screen_tex.iter().collect();
                    order.sort_by_key(|(k, _)| **k);
                    for (who, (_, tex)) in order {
                        ui.label(RichText::new(format!("Tela de {}", member_name(&names, who))).strong());
                        if ui.button("🔍  Ampliar").on_hover_text("Abre a tela numa janela maior").clicked() {
                            self.zoom = Some(*who);
                        }
                        if self.zoom != Some(*who) {
                            ui.add(egui::Image::new(tex).max_width(ui.available_width()).corner_radius(CornerRadius::same(8)).shrink_to_fit());
                        }
                    }
                });
            }
        }

        card(ui, "Membros", "", |ui| {
            for m in &v.members {
                ui.horizontal(|ui| {
                    let who = name_of(&m.key, &m.name);
                    avatar(ui, &who, 34.0, Some(if m.online { GREEN } else { GREY_BUTTON }));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(if m.is_me { format!("{who} (você)") } else { who }).strong().color(if m.online { Color32::WHITE } else { MUTED }));
                        let place = m.room.and_then(|r| v.rooms.iter().find(|x| x.id == r)).map_or(String::new(), |r| format!(" · na sala {}", r.name));
                        ui.label(RichText::new(format!("{}{}{place}", role_name(m.role), if m.online { "" } else { " · offline" })).size(13.0).color(MUTED));
                    });
                    if manages && !m.is_me && m.role != Role::Owner {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if button(ui, true, "Remover", GREY_BUTTON).clicked() {
                                if let Err(e) = hub.try_act(|s, now| s.remove_member(&v.id, m.id, now)) {
                                    self.srv.msg = e.to_string();
                                }
                            }
                            if v.my_role == Some(Role::Owner) {
                                let (label, to) = if m.role == Role::Admin { ("Tirar admin", Role::Member) } else { ("Tornar admin", Role::Admin) };
                                if button(ui, true, label, GREY_BUTTON).clicked() {
                                    if let Err(e) = hub.try_act(|s, now| s.set_role(&v.id, m.id, to, now)) {
                                        self.srv.msg = e.to_string();
                                    }
                                }
                            }
                        });
                    }
                });
                ui.add_space(4.0);
            }
        });

        card(ui, "Convidar", "Escolha um contato: o convite chega a ele pela rede, cifrado, e só ele consegue abri-lo. Qualquer membro pode convidar.", |ui| {
            let shown = self.srv.pick.map_or("escolha um contato".to_string(), |k| member_name(&contacts, &k));
            egui::ComboBox::from_id_salt("convidar").selected_text(shown).width(260.0).show_ui(ui, |ui| {
                for (name, key) in &contacts {
                    if !v.members.iter().any(|m| m.key == *key) {
                        ui.selectable_value(&mut self.srv.pick, Some(*key), name);
                    }
                }
            });
            ui.checkbox(&mut self.srv.ip_ok, "Entendo que quem entrar passa a ver o meu IP quando ligarmos na mesma sala");
            note(ui, "As chamadas de sala são diretas entre os pares (por circuito de 2 saltos), mas o relay de cada um fica conhecido pelos membros.");
            let ready = self.srv.pick.is_some() && self.srv.ip_ok;
            if button(ui, ready, "Enviar convite", BLURPLE).clicked() {
                let key = self.srv.pick.expect("botão só liga com contato escolhido");
                match hub.try_act(|s, now| s.invite_send(&v.id, key, now)) {
                    Ok(()) => {
                        self.srv.msg.clear();
                        self.srv.info = format!("Convite enviado a {}. Se ele estiver offline, o programa tenta de novo até ele receber.", member_name(&contacts, &key));
                    }
                    Err(e) => {
                        self.srv.info.clear();
                        self.srv.msg = e.to_string();
                    }
                }
            }
            if button(ui, ready, "Gerar só o texto", PANEL).clicked() {
                let key = self.srv.pick.expect("botão só liga com contato escolhido");
                match hub.try_run(|s, now| s.invite(&v.id, key, now).map(|t| (t, Output::default()))) {
                    Ok(text) => {
                        self.srv.invite_out = text;
                        self.srv.msg.clear();
                    }
                    Err(e) => self.srv.msg = e.to_string(),
                }
            }
            if !self.srv.invite_out.is_empty() {
                ui.add(egui::Label::new(RichText::new(&self.srv.invite_out).monospace().size(12.0)).wrap());
                if button(ui, true, "Copiar o convite", GREEN).clicked() {
                    ui.ctx().copy_text(self.srv.invite_out.clone());
                }
                note(ui, "Mande o texto por onde preferir; ele só abre na conta do convidado. Ele entra quando você estiver online. (Este caminho não envia nada pela rede.)");
            }
        });
    }

    fn send(&self, c: Cmd) {
        if let Some(r) = &self.running {
            let _ = r.cmds.send(c);
        }
    }
}

/// Ícone quadrado de um servidor na coluna da esquerda, com a inicial desenhada no centro exato do retângulo
/// (o texto de um botão fica à esquerda quando a coluna é mais larga que ele).
fn server_icon(ui: &mut egui::Ui, text: &str, fill: Color32, ink: Color32, radius: u8) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(52.0, 52.0), egui::Sense::click());
    let fill = if reply.hovered() && fill == PANEL { PANEL.gamma_multiply(1.4) } else { fill };
    ui.painter().rect_filled(rect, CornerRadius::same(radius), fill);
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(24.0), ink);
    reply.on_hover_cursor(egui::CursorIcon::PointingHand)
}
