//! Servidores: trilho sempre visível com os servidores, e a aba com salas de voz, quem está nelas e os membros online/offline.

use clam::sync::LockExt;
use std::sync::atomic::Ordering;

use clam::server::{Output, Role, ServerId, ServerView};
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, vec2};

use crate::{conf::*, engine::*, theme::*, ui::{App, Tab}};

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
    /// Texto do bate-papo ainda não enviado.
    pub(crate) chat_draft: String,
    /// O cartão "nova sala" está aceitando o nome.
    pub(crate) adding_room: bool,
    /// O cartão de convite está aberto.
    pub(crate) inviting: bool,
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
    pub(crate) fn servers_tab(&mut self, ui: &mut egui::Ui, offline: bool) {
        let hub = self.shared.servers.lock_unpoisoned().clone();
        let Some(hub) = hub.filter(|_| !offline) else {
            card(ui, "Servidores", "Grupos de pessoas com salas de voz, sem servidor no meio: cada membro guarda o grupo e fica de pé enquanto estiver online.", |ui| {
                note(ui, "Fique online (aba Configurações) para ver e usar os seus servidores.");
            });
            return;
        };
        let views = hub.read(|s, now| s.views(now));
        if self.srv.sel.is_some_and(|id| !views.iter().any(|v| v.id == id)) {
            self.srv.sel = None;
        }
        self.invitations_card(ui, &hub);
        match views.iter().find(|v| Some(v.id) == self.srv.sel) {
            Some(v) => self.server_view(ui, &hub, v),
            None => self.server_add(ui, &hub),
        }
        if !self.srv.info.is_empty() {
            ui.colored_label(GREEN, &self.srv.info);
        }
        if !self.srv.msg.is_empty() {
            ui.colored_label(Color32::from_rgb(0xf2, 0x7b, 0x7e), &self.srv.msg);
        }
    }

    /// Coluna estreita à esquerda da janela, sempre visível como a do Discord: um ícone por servidor e o "+".
    /// Clicar abre a aba Servidores naquele servidor.
    pub(crate) fn server_rail(&mut self, ctx: &egui::Context, offline: bool) {
        let hub = self.shared.servers.lock_unpoisoned().clone().filter(|_| !offline);
        let views = hub.as_ref().map(|h| h.read(|s, now| s.views(now))).unwrap_or_default();
        if self.srv.sel.is_some_and(|id| !views.iter().any(|v| v.id == id)) {
            self.srv.sel = None;
        }
        let here = self.tab == Tab::Servers;
        egui::SidePanel::left("trilho")
            .exact_width(76.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(12, 14)))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 14.0;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let on = self.tab == Tab::Contacts;
                    let people = server_icon(ui, "👤", on.then_some(BLURPLE), Color32::WHITE);
                    if people.on_hover_text("Contatos").clicked() {
                        self.tab = Tab::Contacts;
                    }
                    ui.separator();
                    for v in &views {
                        // Qualquer tela que não seja Contatos mantém a coluna de salas no servidor: o ícone segue marcado.
                        let on = self.tab != Tab::Contacts && self.srv.sel == Some(v.id);
                        let initial = v.name.chars().next().map_or("?".to_string(), |c| c.to_uppercase().to_string());
                        let reply = server_icon(ui, &initial, on.then_some(BLURPLE), Color32::WHITE);
                        if reply.on_hover_text(&v.name).clicked() {
                            self.tab = Tab::Servers;
                            self.srv.sel = Some(v.id);
                            self.srv.msg.clear();
                            self.srv.info.clear();
                            self.srv.invite_out.clear();
                        }
                    }
                    let on = here && self.srv.sel.is_none();
                    let plus = server_icon(ui, "+", on.then_some(GREEN), if on { Color32::WHITE } else { GREEN });
                    if plus.on_hover_text("Criar um servidor ou entrar com um convite").clicked() {
                        self.tab = Tab::Servers;
                        self.srv.sel = None;
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
            ui.check(&mut self.srv.join_ok, "Entendo que, ao entrar, os membros passam a ver o meu IP quando ligarmos na mesma sala");
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
            ui.check(&mut self.srv.join_ok, "Entendo que, ao entrar, os membros passam a ver o meu IP");
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
    }

    /// Tela central de um servidor: as telas da sala (quando há) e o bate-papo da sala em que estou. Presença e
    /// sair do servidor ficam no menu ⋮ do cabeçalho da coluna de salas.
    fn server_view(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView) {
        if v.joining {
            note(ui, "Aguardando o convidante responder para trazer o servidor (ele precisa estar online).");
        }
        let in_this_server = self.shared.room.lock_unpoisoned().at.is_some_and(|(server, _)| server == v.id);
        if in_this_server {
            self.screens_card(ui, v);
        }
        self.room_chat(ui, hub, v);
    }

    /// Coluna dos membros, à direita e de ponta a ponta da janela; o convite fica recolhido no fim do cartão de membros.
    pub(crate) fn members_panel(&mut self, ctx: &egui::Context, offline: bool) {
        if self.tab != Tab::Servers || offline {
            return;
        }
        let Some(hub) = self.shared.servers.lock_unpoisoned().clone() else { return };
        let Some(v) = hub.read(|s, now| s.views(now)).into_iter().find(|v| Some(v.id) == self.srv.sel) else { return };
        egui::SidePanel::right("membros")
            .exact_width(340.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(14, 26)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.members_card(ui, &hub, &v);
                });
            });
    }

    /// Bate-papo da sala em que estou, ocupando o centro. Quem está na sala, compartilhar e assistir tela ficam
    /// na coluna de salas, junto da própria sala.
    fn room_chat(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView) {
        let my_room = self.shared.room.lock_unpoisoned().at;
        let Some(room) = v.rooms.iter().find(|r| my_room == Some((v.id, r.id))) else {
            card(ui, "Bate-papo", "", |ui| {
                note(ui, "Clique numa sala da lista à esquerda para entrar e conversar. Quem está na mesma sala fala com todos (até 8 pessoas), em ligação direta entre cada par.");
            });
            return;
        };
        ui.label(RichText::new(format!("🔊  {}", room.name)).strong().size(18.0).color(Color32::WHITE));
        self.chat_box(ui, hub, v, room);
    }

    /// Menu aberto pelo ícone de sair do servidor: um único item vermelho, que serve de confirmação.
    fn server_menu(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView) {
        if ui.button(RichText::new("Confirmar saída").color(RED)).clicked() {
            if let Err(e) = hub.try_act(|s, now| s.leave(&v.id, now)) {
                self.srv.msg = e.to_string();
            }
            self.srv.sel = None;
            ui.close();
        }
    }

    /// Coluna de salas à esquerda (ao lado do trilho de servidores): clicar numa sala entra nela. No pé ficam o
    /// perfil, o mudo, a engrenagem com as demais telas e o desligar.
    pub(crate) fn rooms_panel(&mut self, ctx: &egui::Context, offline: bool, in_call: bool, status: &str) {
        let hub = self.shared.servers.lock_unpoisoned().clone().filter(|_| !offline);
        let view = hub.as_ref().and_then(|h| h.read(|s, now| s.views(now)).into_iter().find(|v| Some(v.id) == self.srv.sel));
        let my_room = self.shared.room.lock_unpoisoned().at;
        // (sala, servidor) em que estou: vira a subseção acima do perfil, com o sair da sala.
        let room_bar = hub.as_ref().zip(my_room).and_then(|(h, (sid, rid))| {
            let sv = h.read(|s, now| s.view(&sid, now))?;
            let name = sv.rooms.iter().find(|r| r.id == rid)?.name.clone();
            Some((name, sv.name))
        });
        egui::SidePanel::left("salas")
            .exact_width(300.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(12, 14)))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                // O perfil ocupa uma altura fixa no pé; a lista de salas rola no espaço que sobra.
                let footer: f32 = 74.0 + if room_bar.is_some() { 36.0 } else { 0.0 };
                if self.tab == Tab::Contacts {
                    let count = parse_contacts(&self.form.contacts).len();
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(RichText::new("Contatos").strong().size(17.0).color(Color32::WHITE));
                            ui.label(RichText::new(match count { 0 => "ninguém ainda".to_string(), 1 => "1 pessoa".to_string(), n => format!("{n} pessoas") }).size(13.0).color(MUTED));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let plus = plus_button(ui, "Novo contato");
                            // Fecha só ao clicar fora, para dar tempo de colar o código.
                            egui::Popup::menu(&plus).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| self.new_contact_form(ui));
                        });
                    });
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(2.0);
                    let height = (ui.available_height() - footer).max(60.0);
                    egui::ScrollArea::vertical().max_height(height).auto_shrink([false, false]).show(ui, |ui| {
                        // Folga lateral para as sombras dos contatos não serem cortadas pela rolagem.
                        egui::Frame::new().inner_margin(Margin::symmetric(8, 6)).show(ui, |ui| self.contacts_list(ui, !offline, in_call));
                    });
                } else {
                match (&hub, &view) {
                    (Some(hub), Some(v)) => {
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(RichText::new(&v.name).strong().size(17.0).color(Color32::WHITE));
                                ui.label(RichText::new(format!("{} online", v.members.iter().filter(|m| m.online).count())).size(13.0).color(MUTED));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let exit = exit_button(ui).on_hover_text("Sair do servidor");
                                egui::Popup::menu(&exit).show(|ui| self.server_menu(ui, hub, v));
                            });
                        });
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(2.0);
                        let height = (ui.available_height() - footer).max(60.0);
                        egui::ScrollArea::vertical().max_height(height).auto_shrink([false, false]).show(ui, |ui| {
                            // Folga lateral para as sombras das salas não serem cortadas pela rolagem.
                            egui::Frame::new().inner_margin(Margin::symmetric(8, 6)).show(ui, |ui| self.room_list(ui, hub, v, my_room, in_call));
                        });
                    }
                    _ => {
                        ui.add_space(8.0);
                        note(ui, "Escolha um servidor na coluna da esquerda.");
                    }
                }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    sunken_frame(ui, 12, 8, |ui| {
                        ui.set_width(ui.available_width());
                        self.user_panel(ctx, ui, offline, in_call, status);
                    });
                    if let Some((room, server)) = &room_bar {
                        // Colada no perfil (sem o espaço entre cartões) e de uma linha só: é um estado, não uma tela.
                        sunken_frame(ui, 12, 4, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("🔊 {room} · {server}")).size(12.0).color(GREEN));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let leave = egui::Button::new(RichText::new("✖").size(12.0).color(MUTED)).frame(false);
                                    if ui.add(leave).on_hover_text("Sair da sala").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                        self.send(Cmd::LeaveRoom);
                                    }
                                });
                            });
                        });
                    }
                });
            });
    }

    fn room_list(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView, my_room: Option<(ServerId, clam::server::RoomId)>, in_call: bool) {
        let contacts = parse_contacts(&self.form.contacts);
        let manages = v.my_role.is_some_and(|r| r.manages());
        let mut action: Option<(Option<[u8; 32]>, bool)> = None;
        for room in &v.rooms {
            let here = my_room == Some((v.id, room.id));
            let blocked = if here { None } else if my_room.is_some() { Some("Saia da sala atual (✖ no pé da coluna) antes de trocar") } else if in_call { Some("Encerre a chamada antes de entrar numa sala") } else { None };
            raised_frame(ui, 12, 8, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let title = RichText::new(format!("🔊  {}", room.name)).strong().color(if here { Color32::WHITE } else { TEXT });
                let width = ui.available_width() - if manages { 30.0 } else { 0.0 };
                let entry = egui::Button::new(title).fill(if here { SELECTED } else { Color32::TRANSPARENT }).min_size(vec2(width, 32.0));
                let reply = ui.add_enabled(blocked.is_none(), entry).on_hover_text("Entrar na sala").on_disabled_hover_text(blocked.unwrap_or_default());
                if reply.clicked() && !here {
                    self.send(Cmd::JoinRoom(v.id, room.id));
                }
                if manages && ui.add(egui::Button::new("🗑").frame(false)).on_hover_text("Apagar a sala").clicked()
                    && let Err(e) = hub.try_act(|s, now| s.delete_room(&v.id, room.id, now)) {
                        self.srv.msg = e.to_string();
                    }
            });
            let live = self.shared.room.lock_unpoisoned();
            let sharing = self.shared.screen.want_share.load(Ordering::Relaxed);
            for id in &room.people {
                let Some(m) = v.members.iter().find(|m| m.id == *id) else { continue };
                let speaking = live.speaking(&m.key);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.add_space(26.0);
                    let who = if m.name.is_empty() { member_name(&contacts, &m.key) } else { m.name.clone() };
                    avatar(ui, &who, 22.0, speaking.then_some(GREEN));
                    ui.label(RichText::new(who).size(14.0).color(if speaking { GREEN } else { MUTED }));
                    if m.sharing {
                        let (rect, tip) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, CornerRadius::same(2), RED);
                        tip.on_hover_text("transmitindo a tela");
                    }
                    // Compartilhar/assistir tela só valem na sala em que estou.
                    if here {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Mesma folga da borda que o avatar tem à esquerda (26 menos o enchimento do botão).
                            ui.add_space(22.0);
                            if m.is_me {
                                let (icon, tip) = if sharing { ("⏹", "Parar de compartilhar") } else { ("🖥", "Compartilhar tela") };
                                if ui.add(egui::Button::new(RichText::new(icon).color(if sharing { RED } else { TEXT })).frame(false)).on_hover_text(tip).clicked() {
                                    action = Some((None, !sharing));
                                }
                            } else if m.sharing {
                                let on = live.watching.contains(&m.key);
                                if ui.add(egui::Button::new(RichText::new(if on { "🚫" } else { "👁" }).color(GREEN)).frame(false)).on_hover_text(if on { "Parar de assistir" } else { "Assistir à tela" }).clicked() {
                                    action = Some((Some(m.key), !on));
                                }
                            }
                        });
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
            ui.add_space(8.0);
        }
        // Uma sala "fantasma": mesmo desenho das outras, mais apagada, com o + no meio. Ao clicar, o próprio
        // cartão vira o campo do nome: Enter cria, Esc (ou sair do campo vazio) cancela.
        if self.srv.adding_room {
            let mut done = None;
            raised_frame(ui, 12, 8, |ui| {
                ui.set_width(ui.available_width());
                let edit = egui::TextEdit::singleline(&mut self.srv.new_room).hint_text("Nome da sala").frame(false).desired_width(f32::INFINITY).margin(Margin::symmetric(4, 8));
                let reply = ui.add(edit);
                // Foco só na primeira vez, para não roubá-lo do bate-papo se a pessoa clicar em outro lugar.
                let focused = egui::Id::new("foco-nova-sala");
                if !ui.data(|d| d.get_temp::<bool>(focused)).unwrap_or(false) {
                    reply.request_focus();
                    ui.data_mut(|d| d.insert_temp(focused, true));
                }
                let name = self.srv.new_room.trim().to_string();
                if reply.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !name.is_empty() {
                    done = Some(Some(name));
                } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) || (reply.lost_focus() && name.is_empty()) {
                    done = Some(None);
                }
            });
            match done {
                Some(Some(name)) => match hub.try_run(|s, now| s.create_room(&v.id, &name, now)) {
                    Ok(_) => {
                        self.srv.new_room.clear();
                        self.srv.msg.clear();
                        self.srv.adding_room = false;
                    }
                    Err(e) => self.srv.msg = e.to_string(),
                },
                Some(None) => {
                    self.srv.new_room.clear();
                    self.srv.adding_room = false;
                }
                None => {}
            }
            if !self.srv.adding_room {
                ui.data_mut(|d| d.insert_temp(egui::Id::new("foco-nova-sala"), false));
            }
        } else {
            let (rect, ghost) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), egui::Sense::click());
            raised(ui.painter(), rect, 12, BG, 3);
            draw_plus(ui.painter(), rect.center(), 8.0, if ghost.hovered() { TEXT } else { MUTED.gamma_multiply(0.6) });
            if ghost.on_hover_text("Nova sala").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.srv.adding_room = true;
            }
        }
    }

    /// Telas que estou assistindo ou transmitindo na sala em que estou.
    fn screens_card(&mut self, ui: &mut egui::Ui, v: &ServerView) {
        let my_room = self.shared.room.lock_unpoisoned().at;
        if my_room.is_some_and(|(server, _)| server == v.id) {
            let (sharing, note_text) = (self.shared.screen.sharing.load(Ordering::Relaxed), self.shared.screen.note.lock_unpoisoned().clone());
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
                        if ui.button("🔍").on_hover_text("Ampliar: abre a tela numa janela maior").clicked() {
                            self.zoom = Some(*who);
                        }
                        if self.zoom != Some(*who) {
                            ui.add(egui::Image::new(tex).max_width(ui.available_width()).corner_radius(CornerRadius::same(8)).shrink_to_fit());
                        }
                    }
                });
            }
        }
    }

    /// Membros do servidor e, no fim, o convite (recolhido, para ocupar pouco).
    fn members_card(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView) {
        let contacts = parse_contacts(&self.form.contacts);
        let name_of = |key: &[u8; 32], given: &str| if given.is_empty() { member_name(&contacts, key) } else { given.to_string() };
        let manages = v.my_role.is_some_and(|r| r.manages());
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
                            let dots = dots_button(ui);
                            egui::Popup::menu(&dots).show(|ui| {
                                if v.my_role == Some(Role::Owner) {
                                    let (label, to) = if m.role == Role::Admin { ("Rebaixar", Role::Member) } else { ("Promover", Role::Admin) };
                                    if ui.button(label).on_hover_text("Muda o papel de admin deste membro").clicked() {
                                        if let Err(e) = hub.try_act(|s, now| s.set_role(&v.id, m.id, to, now)) {
                                            self.srv.msg = e.to_string();
                                        }
                                        ui.close();
                                    }
                                }
                                if ui.button(RichText::new("Remover").color(RED)).on_hover_text("Remover do servidor").clicked() {
                                    if let Err(e) = hub.try_act(|s, now| s.remove_member(&v.id, m.id, now)) {
                                        self.srv.msg = e.to_string();
                                    }
                                    ui.close();
                                }
                            });
                        });
                    }
                });
                ui.add_space(4.0);
            }
            ui.add_space(4.0);
            self.invite_card(ui, hub, v);
        });
    }

    /// Convite de um contato, compacto: recolhido até a pessoa querer convidar.
    fn invite_card(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView) {
        let contacts = parse_contacts(&self.form.contacts);
        // Como o cartão "nova sala": um cartão apagado com + que, ao clicar, vira o formulário no próprio lugar.
        if !self.srv.inviting {
            let (rect, ghost) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), egui::Sense::click());
            raised(ui.painter(), rect, 12, BG, 3);
            draw_plus(ui.painter(), rect.center(), 8.0, if ghost.hovered() { TEXT } else { MUTED.gamma_multiply(0.6) });
            if ghost.on_hover_text("Convidar alguém para o servidor").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.srv.inviting = true;
            }
            return;
        }
        {
          raised_frame(ui, 12, 12, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Convidar alguém").strong().color(Color32::WHITE));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new("✖").color(MUTED)).frame(false)).on_hover_text("Fechar").clicked() {
                        self.srv.inviting = false;
                    }
                });
            });
            note(ui, "O convite chega pela rede, cifrado, e só o contato escolhido consegue abri-lo. Qualquer membro pode convidar.");
            let shown = self.srv.pick.map_or("escolha um contato".to_string(), |k| member_name(&contacts, &k));
            egui::ComboBox::from_id_salt("convidar").selected_text(shown).width(260.0).show_ui(ui, |ui| {
                for (name, key) in &contacts {
                    if !v.members.iter().any(|m| m.key == *key) {
                        ui.selectable_value(&mut self.srv.pick, Some(*key), name);
                    }
                }
            });
            ui.check(&mut self.srv.ip_ok, "Entendo que quem entrar passa a ver o meu IP quando ligarmos na mesma sala").on_hover_text("As chamadas de sala são diretas entre os pares (por circuito de 2 saltos), mas o relay de cada um fica conhecido pelos membros.");
            let ready = self.srv.pick.is_some() && self.srv.ip_ok;
            if contacts.iter().all(|(_, k)| v.members.iter().any(|m| m.key == *k)) {
                note(ui, "Não há contato para convidar: todos já são membros (ou a lista está vazia). Adicione contatos pelo + da coluna de contatos.");
            }
            ui.horizontal(|ui| {
            if button(ui, ready, "Enviar convite", BLURPLE).on_disabled_hover_text("Escolha um contato e marque o aviso do IP para liberar").clicked() {
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
            if button(ui, ready, "Gerar só o texto", PANEL).on_disabled_hover_text("Escolha um contato e marque o aviso do IP para liberar").clicked() {
                let key = self.srv.pick.expect("botão só liga com contato escolhido");
                match hub.try_run(|s, now| s.invite(&v.id, key, now).map(|t| (t, Output::default()))) {
                    Ok(text) => {
                        self.srv.invite_out = text;
                        self.srv.msg.clear();
                    }
                    Err(e) => self.srv.msg = e.to_string(),
                }
            }
            });
            if !self.srv.invite_out.is_empty() {
                ui.add(egui::Label::new(RichText::new(&self.srv.invite_out).monospace().size(12.0)).wrap());
                if button(ui, true, "Copiar o convite", GREEN).clicked() {
                    ui.ctx().copy_text(self.srv.invite_out.clone());
                }
                note(ui, "Mande o texto por onde preferir; ele só abre na conta do convidado. Ele entra quando você estiver online. (Este caminho não envia nada pela rede.)");
            }
          });
        }
    }

    /// Bate-papo da sala em que estou: só existe enquanto a sala tem gente, e só quem está nela o vê.
    fn chat_box(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView, room: &clam::server::RoomView) {
        let contacts = parse_contacts(&self.form.contacts);
        ui.add_space(6.0);
        // O conteúdo central rola, então não há "altura disponível": vai até o pé da janela, menos a linha de envio e a folga da sombra do botão (senão a rolagem a corta).
        let room_height = (ui.clip_rect().bottom() - ui.cursor().top() - 104.0).max(160.0);
        egui::ScrollArea::vertical().id_salt(("bate-papo", room.id)).max_height(room_height).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
            if room.chat.is_empty() {
                ui.label(RichText::new("Sem mensagens. A conversa some quando a sala fica vazia.").size(13.0).color(MUTED));
            }
            for line in &room.chat {
                let who = v.members.iter().find(|m| m.id == line.from).map_or("?".to_string(), |m| if m.name.is_empty() { member_name(&contacts, &m.key) } else { m.name.clone() });
                let mine = v.me == line.from;
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(RichText::new(format!("{who}:")).strong().color(if mine { GREEN } else { Color32::WHITE }));
                    ui.label(RichText::new(&line.text).color(TEXT));
                });
            }
        });
        ui.horizontal(|ui| {
            let reply = ui.add(field(&mut self.srv.chat_draft, "Mensagem para a sala").desired_width((ui.available_width() - 90.0).max(80.0)));
            let enter = reply.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (button(ui, !self.srv.chat_draft.trim().is_empty(), "Enviar", BLURPLE).clicked() || enter) && !self.srv.chat_draft.trim().is_empty() {
                let text = std::mem::take(&mut self.srv.chat_draft);
                if let Err(e) = hub.try_act(|s, now| s.say(&v.id, &text, now)) {
                    self.srv.msg = e.to_string();
                }
                reply.request_focus();
            }
        });
        ui.add_space(16.0);
    }

    fn send(&self, c: Cmd) {
        if let Some(r) = &self.running {
            let _ = r.cmds.send(c);
        }
    }
}

/// Ícone quadrado de um servidor na coluna da esquerda, com a inicial desenhada no centro exato do retângulo
/// (o texto de um botão fica à esquerda quando a coluna é mais larga que ele). Elevado em repouso; o
/// selecionado (`active` traz a cor) fica afundado.
fn server_icon(ui: &mut egui::Ui, text: &str, active: Option<Color32>, ink: Color32) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(52.0, 52.0), egui::Sense::click());
    match active {
        Some(fill) => sunken(ui.painter(), rect, 18, fill),
        None => raised(ui.painter(), rect, 18, if reply.hovered() { BG.lerp_to_gamma(Color32::WHITE, 0.06) } else { BG }, 4),
    }
    if text == "+" {
        draw_plus(ui.painter(), rect.center(), 11.0, ink);
    } else {
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(24.0), ink);
    }
    reply.on_hover_cursor(egui::CursorIcon::PointingHand)
}
