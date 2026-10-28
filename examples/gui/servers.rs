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
    pub(crate) confirm_leave: bool,
    /// Sala cujo bate-papo está aberto: só um por vez.
    pub(crate) chat_open: Option<clam::server::RoomId>,
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

    /// Coluna estreita à esquerda da janela, sempre visível como a do Discord: um ícone por servidor e o "＋".
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
            .frame(egui::Frame::new().fill(DEEP).inner_margin(Margin::symmetric(12, 14)))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let on = self.tab == Tab::Contacts;
                    let people = server_icon(ui, "👤", if on { BLURPLE } else { PANEL }, Color32::WHITE, if on { 14 } else { 24 });
                    if people.on_hover_text("Contatos").clicked() {
                        self.tab = Tab::Contacts;
                    }
                    ui.separator();
                    for v in &views {
                        let on = here && self.srv.sel == Some(v.id);
                        let initial = v.name.chars().next().map_or("?".to_string(), |c| c.to_uppercase().to_string());
                        let reply = server_icon(ui, &initial, if on { BLURPLE } else { PANEL }, Color32::WHITE, if on { 14 } else { 24 });
                        if reply.on_hover_text(&v.name).clicked() {
                            self.tab = Tab::Servers;
                            self.srv.sel = Some(v.id);
                            self.srv.msg.clear();
                            self.srv.info.clear();
                            self.srv.invite_out.clear();
                            self.srv.confirm_leave = false;
                        }
                    }
                    let on = here && self.srv.sel.is_none();
                    let plus = server_icon(ui, "＋", if on { GREEN } else { PANEL }, if on { Color32::WHITE } else { GREEN }, if on { 14 } else { 24 });
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

    fn server_view(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView) {
        let online = v.members.iter().filter(|m| m.online).count();
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

        let in_this_server = self.shared.room.lock_unpoisoned().at.is_some_and(|(server, _)| server == v.id);
        if in_this_server {
            self.screens_card(ui, v);
        }
        self.rooms_card(ui, hub, v);
    }

    /// Coluna dos membros, à direita e de ponta a ponta da janela; o convite fica recolhido embaixo.
    pub(crate) fn members_panel(&mut self, ctx: &egui::Context, offline: bool) {
        if self.tab != Tab::Servers || offline {
            return;
        }
        let Some(hub) = self.shared.servers.lock_unpoisoned().clone() else { return };
        let Some(v) = hub.read(|s, now| s.views(now)).into_iter().find(|v| Some(v.id) == self.srv.sel) else { return };
        egui::SidePanel::right("membros")
            .exact_width(340.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(DEEP).inner_margin(Margin::symmetric(14, 26)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.members_card(ui, &hub, &v);
                    self.invite_card(ui, &hub, &v);
                });
            });
    }

    /// A sala em que estou: quem está nela, compartilhar/assistir tela e o bate-papo. A lista de salas fica no painel da esquerda.
    fn rooms_card(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView) {
        let contacts = parse_contacts(&self.form.contacts);
        let name_of = |key: &[u8; 32], given: &str| if given.is_empty() { member_name(&contacts, key) } else { given.to_string() };
        let my_room = self.shared.room.lock_unpoisoned().at;
        let Some(room) = v.rooms.iter().find(|r| my_room == Some((v.id, r.id))) else {
            card(ui, "Sala de voz", "", |ui| {
                note(ui, "Clique numa sala da lista à esquerda para entrar. Quem está na mesma sala fala com todos (até 8 pessoas), em ligação direta entre cada par.");
            });
            return;
        };
        card(ui, &format!("🔊  {}", room.name), "", |ui| {
            ui.horizontal(|ui| {
                if button(ui, true, "Sair", RED).clicked() {
                    self.send(Cmd::LeaveRoom);
                }
                let open = self.srv.chat_open == Some(room.id);
                if button(ui, true, if open { "Fechar chat" } else { "💬 Chat" }, if open { GREY_BUTTON } else { BLURPLE }).clicked() {
                    self.srv.chat_open = if open { None } else { Some(room.id) };
                }
            });
            ui.add_space(6.0);
            let live = self.shared.room.lock_unpoisoned();
            let (sharing, mut action) = (self.shared.screen.want_share.load(Ordering::Relaxed), None);
            for id in &room.people {
                let Some(m) = v.members.iter().find(|m| m.id == *id) else { continue };
                let speaking = live.speaking(&m.key);
                ui.horizontal(|ui| {
                    let who = name_of(&m.key, &m.name);
                    avatar(ui, &who, 26.0, speaking.then_some(GREEN));
                    let text = RichText::new(if m.is_me { format!("{who} (você)") } else { who }).color(if speaking { GREEN } else { TEXT });
                    ui.label(if speaking { text.strong() } else { text });
                    if m.is_me {
                        let (icon, tip, fill) = if sharing { ("⏹", "Parar de compartilhar", RED) } else { ("🖥", "Compartilhar tela", BLURPLE) };
                        if icon_button(ui, true, icon, tip, fill).clicked() {
                            action = Some((None, !sharing));
                        }
                    } else if m.sharing {
                        let on = live.watching.contains(&m.key);
                        let reply = icon_button(ui, true, "👁", if on { "Parar de assistir" } else { "Assistir" }, if on { GREY_BUTTON } else { GREEN });
                        if on {
                            // O mesmo olho com um risco por cima.
                            let r = reply.rect.shrink2(vec2(11.0, 9.0));
                            ui.painter().line_segment([r.left_bottom(), r.right_top()], egui::Stroke::new(2.5f32, Color32::WHITE));
                        }
                        if reply.clicked() {
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
            if self.srv.chat_open == Some(room.id) {
                self.chat_box(ui, hub, v, room);
            }
        });
    }

    /// Coluna de salas à esquerda (ao lado do trilho de servidores): clicar numa sala entra nela. No pé ficam o
    /// perfil, o mudo, a engrenagem com as demais telas e o desligar.
    pub(crate) fn rooms_panel(&mut self, ctx: &egui::Context, offline: bool, in_call: bool, status: &str) {
        let hub = self.shared.servers.lock_unpoisoned().clone().filter(|_| !offline);
        let view = hub.as_ref().and_then(|h| h.read(|s, now| s.views(now)).into_iter().find(|v| Some(v.id) == self.srv.sel));
        let my_room = self.shared.room.lock_unpoisoned().at;
        egui::SidePanel::left("salas")
            .exact_width(240.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(12, 14)))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                // O perfil ocupa uma altura fixa no pé; a lista de salas rola no espaço que sobra.
                const FOOTER: f32 = 74.0;
                match (&hub, &view) {
                    (Some(hub), Some(v)) => {
                        ui.add_space(2.0);
                        ui.label(RichText::new(&v.name).strong().size(17.0).color(Color32::WHITE));
                        ui.label(RichText::new(format!("{} online", v.members.iter().filter(|m| m.online).count())).size(13.0).color(MUTED));
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(2.0);
                        let height = (ui.available_height() - FOOTER).max(60.0);
                        egui::ScrollArea::vertical().max_height(height).auto_shrink([false, false]).show(ui, |ui| self.room_list(ui, hub, v, my_room, in_call));
                    }
                    _ => {
                        ui.add_space(8.0);
                        note(ui, "Escolha um servidor na coluna da esquerda.");
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    egui::Frame::new().fill(DEEP).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(8, 8)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.user_panel(ctx, ui, offline, in_call, status);
                    });
                });
            });
    }

    fn room_list(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView, my_room: Option<(ServerId, clam::server::RoomId)>, in_call: bool) {
        let contacts = parse_contacts(&self.form.contacts);
        let manages = v.my_role.is_some_and(|r| r.manages());
        for room in &v.rooms {
            let here = my_room == Some((v.id, room.id));
            let blocked = if here { None } else if my_room.is_some() { Some("Saia da sala atual (✖) antes de trocar") } else if in_call { Some("Encerre a chamada antes de entrar numa sala") } else { None };
            egui::Frame::new().stroke(egui::Stroke::new(1.0_f32, SELECTED)).corner_radius(CornerRadius::same(4)).inner_margin(6.0_f32).show(ui, |ui| {
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
                });
            }
            drop(live);
            });
            ui.add_space(8.0);
        }
        egui::CollapsingHeader::new(RichText::new("＋ Nova sala").size(14.0).color(MUTED)).id_salt("nova-sala").default_open(false).show(ui, |ui| {
            ui.add(field(&mut self.srv.new_room, "Nome da sala"));
            if button(ui, !self.srv.new_room.trim().is_empty(), "Criar", BLURPLE).clicked() {
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
                            // Três pontos um acima do outro, desenhados: a fonte pode não ter o glifo "⋮".
                            let (rect, dots) = ui.allocate_exact_size(vec2(28.0, 34.0), egui::Sense::click());
                            let ink = if dots.hovered() { Color32::WHITE } else { MUTED };
                            for dy in [-6.0, 0.0, 6.0] {
                                ui.painter().circle_filled(rect.center() + vec2(0.0, dy), 2.0, ink);
                            }
                            egui::Popup::menu(&dots.on_hover_cursor(egui::CursorIcon::PointingHand)).show(|ui| {
                                if v.my_role == Some(Role::Owner) {
                                    let (label, to) = if m.role == Role::Admin { ("Tirar admin", Role::Member) } else { ("Tornar admin", Role::Admin) };
                                    if ui.button(label).clicked() {
                                        if let Err(e) = hub.try_act(|s, now| s.set_role(&v.id, m.id, to, now)) {
                                            self.srv.msg = e.to_string();
                                        }
                                        ui.close();
                                    }
                                }
                                if ui.button(RichText::new("Remover do servidor").color(RED)).clicked() {
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
        });
    }

    /// Convite de um contato, compacto: recolhido até a pessoa querer convidar.
    fn invite_card(&mut self, ui: &mut egui::Ui, hub: &clam::server::net::ServerHub, v: &ServerView) {
        let contacts = parse_contacts(&self.form.contacts);
        {
          egui::CollapsingHeader::new(RichText::new("＋ Convidar alguém").size(14.0).color(MUTED)).id_salt("convidar-aberto").default_open(false).show(ui, |ui| {
            note(ui, "O convite chega pela rede, cifrado, e só o contato escolhido consegue abri-lo. Qualquer membro pode convidar.");
            let shown = self.srv.pick.map_or("escolha um contato".to_string(), |k| member_name(&contacts, &k));
            egui::ComboBox::from_id_salt("convidar").selected_text(shown).width(260.0).show_ui(ui, |ui| {
                for (name, key) in &contacts {
                    if !v.members.iter().any(|m| m.key == *key) {
                        ui.selectable_value(&mut self.srv.pick, Some(*key), name);
                    }
                }
            });
            ui.checkbox(&mut self.srv.ip_ok, "Entendo que quem entrar passa a ver o meu IP quando ligarmos na mesma sala").on_hover_text("As chamadas de sala são diretas entre os pares (por circuito de 2 saltos), mas o relay de cada um fica conhecido pelos membros.");
            let ready = self.srv.pick.is_some() && self.srv.ip_ok;
            ui.horizontal(|ui| {
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
        egui::ScrollArea::vertical().id_salt(("bate-papo", room.id)).max_height(360.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
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
