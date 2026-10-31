//! Bate-papo 1:1 com um contato: histórico guardado aqui, envio só com o contato online (`docs/CHAMADAS.md`).

use clam::sync::LockExt;

use eframe::egui::{self, RichText, vec2};

use crate::{arquivos::{Phase, size_text}, chats::State, engine::*, theme::*, ui::*};

impl App {
    /// A conversa com `peer`. `online`: o contato está de pé agora (ou em chamada comigo), então dá para enviar.
    pub(crate) fn chat_panel(&mut self, ui: &mut egui::Ui, peer: [u8; 32], online: bool) {
        let name = member_name(&self.form.names(), &peer);
        let msgs = self.shared.chats.lock_unpoisoned().with(&peer).to_vec();
        self.shared.chats.lock_unpoisoned().read(&peer);
        let sendable = online && self.running.is_some();
        let mut close = false;
        // Arquivo solto na janela fica em espera até o Enviar (só com o contato online).
        if sendable {
            let dropped = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect::<Vec<_>>());
            for path in dropped {
                crate::anexo::stage(ui.ctx(), ("conversa", peer), path);
            }
        }
        let xfers = self.shared.transfers.lock_unpoisoned().of(&peer);
        let mut file_cmds: Vec<Cmd> = Vec::new();
        // Sem cartão, como o bate-papo da sala: título, mensagens até o pé da janela e a linha de envio embaixo.
        ui.add_space(4.0);
        ui.scope(|ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Bate-papo com {name}")).strong().size(18.0).color(pal().strong))
                    .on_hover_text("Só vai para quem está online. Cada um guarda a sua cópia nesta máquina.");
                // Em chamada a conversa é a da linha: fechar não faz sentido.
                if !self.shared.in_call.load(std::sync::atomic::Ordering::Relaxed) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if close_button(ui, "Fechar a conversa").clicked() {
                            close = true;
                        }
                    });
                }
            });
            ui.add_space(6.0);
            let height = (ui.clip_rect().bottom() - ui.cursor().top() - 84.0 - crate::anexo::height(ui.ctx(), ("conversa", peer))).max(160.0);
            egui::ScrollArea::vertical().id_salt(("conversa", peer)).max_height(height).min_scrolled_height(height).stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                if msgs.is_empty() {
                    note(ui, "Sem mensagens ainda.");
                }
                for g in crate::chats::groups(&msgs) {
                    let parts = msgs.get(g).unwrap_or_default();
                    let Some(m) = parts.first() else { continue };
                    // Pedaços de um texto longo aparecem como uma mensagem só; o estado é o pior entre eles.
                    let text: String = parts.iter().map(|p| p.text.as_str()).collect();
                    let state = if parts.iter().any(|p| p.state == State::Failed) {
                        State::Failed
                    } else if parts.iter().any(|p| p.state == State::Sending) {
                        State::Sending
                    } else {
                        m.state
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let (who, color) = if m.mine() { ("Você", pal().green) } else { (name.as_str(), pal().strong) };
                        ui.label(RichText::new(format!("{who}:")).strong().color(color));
                        ui.label(RichText::new(text).color(pal().text));
                        if let Some(x) = xfers.iter().find(|x| x.id == m.id && x.mine == m.mine() && x.phase.live()) {
                            file_progress(ui, x, &mut file_cmds);
                            return;
                        }
                        match state {
                            State::Sending => ui.label(RichText::new("enviando…").size(12.0).color(pal().muted)),
                            State::Failed => {
                                // Discreto: um triângulo pequeno no tom do erro, sem texto; o motivo fica no hover.
                                let (rect, r) = ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::hover());
                                r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "Não entregue"));
                                alert_icon(ui.painter(), rect.center(), 13.0, pal().error.gamma_multiply(0.8));
                                r.on_hover_text("Não entregue: o contato não confirmou, estava offline ou sem rota. Não há caixa de correio; envie de novo quando ele voltar.")
                            }
                            State::Sent | State::Received => ui.label(""),
                        };
                    });
                    // Imagem à vista, vídeo com play e cartão com baixar, quando o arquivo já está no disco.
                    if let Some(x) = xfers.iter().find(|x| x.id == m.id && x.mine == m.mine() && x.media_path().is_some())
                        && let Some(path) = x.media_path()
                    {
                        crate::midia::show(ui, &x.name, x.size, path);
                    }
                }
            });
            ui.add_space(4.0);
            crate::anexo::chips(ui, ("conversa", peer));
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 36.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let waiting = crate::anexo::count(ui.ctx(), ("conversa", peer)) > 0;
                let ready = sendable && (!self.chat_draft.trim().is_empty() || waiting);
                let sent = button(ui, ready, "Enviar", BLURPLE).clicked();
                if attach_button(ui, sendable, ATTACH_TIP).clicked() {
                    // O diálogo do sistema bloqueia: roda fora da janela e deixa o arquivo em espera até o Enviar.
                    let repaint = ui.ctx().clone();
                    std::thread::spawn(move || {
                        if let Some(path) = rfd::FileDialog::new().set_title("Anexar arquivo").pick_file() {
                            crate::anexo::stage(&repaint, ("conversa", peer), path);
                        }
                    });
                }
                let hint = if sendable { "Mensagem" } else { "Contato offline: não dá para enviar" };
                let reply = ui.add_enabled(sendable, field(&mut self.chat_draft, hint).desired_width(field_beside(ui, 0.0).max(80.0)));
                let enter = reply.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (sent || enter) && ready {
                    if let Some(r) = &self.running {
                        // Os arquivos saem primeiro e entram na conversa já com o "carregando" até o contato receber.
                        for path in crate::anexo::take(ui.ctx(), ("conversa", peer)) {
                            let _ = r.cmds.send(Cmd::SendFile(peer, path));
                        }
                        let text = std::mem::take(&mut self.chat_draft);
                        if !text.trim().is_empty() {
                            let _ = r.cmds.send(Cmd::Chat(peer, text));
                        }
                    }
                    reply.request_focus();
                }
            });
        });
        ui.add_space(16.0);
        if let Some(r) = &self.running {
            for c in file_cmds {
                let _ = r.cmds.send(c);
            }
        }
        if close {
            self.chat_with = None;
        }
    }

    /// Abre a conversa com o contato no centro.
    pub(crate) fn open_chat(&mut self, key: [u8; 32]) {
        self.chat_with = Some(key);
        self.chat_draft.clear();
        self.tab = Tab::Call;
    }
}

/// Ao lado da mensagem do arquivo, enquanto ele não terminou. Quem envia vê o carregando e o X de cancelar (só ele
/// cancela). Quem recebe vê o pedido com o ícone de baixar e o X de recusar; depois de baixar só espera.
fn file_progress(ui: &mut egui::Ui, x: &crate::arquivos::Xfer, cmds: &mut Vec<Cmd>) {
    if x.phase == Phase::Asking {
        ui.label(RichText::new(format!("pedido de arquivo · {}", size_text(x.size))).size(12.0).color(pal().muted));
        if download_button(ui, "Baixar: o arquivo só vem para este computador depois do clique").clicked() {
            cmds.push(Cmd::FileAnswer(x.peer, x.id, true));
        }
        if close_button(ui, "Recusar o pedido").clicked() {
            cmds.push(Cmd::FileAnswer(x.peer, x.id, false));
        }
        return;
    }
    ui.add(egui::Spinner::new().size(14.0).color(BLURPLE));
    let text = match x.phase {
        Phase::Active => format!("{}% de {}", (x.bytes * 100 / x.size.max(1)).min(100), size_text(x.size)),
        _ if x.mine => "esperando o contato…".to_string(),
        _ => "começando…".to_string(),
    };
    ui.label(RichText::new(text).size(12.0).color(pal().muted));
    if x.mine && close_button(ui, "Cancelar o envio").clicked() {
        cmds.push(Cmd::FileCancel(x.peer, x.id, true));
    }
}

pub(crate) const ATTACH_TIP: &str = "Anexar arquivo: abre as pastas do computador (ou arraste o arquivo para a janela). O arquivo só sai quando você clica em Enviar. Limite de 100 MB";

/// Clipe de papel desenhado (a fonte pode não ter o glifo): abre o seletor de arquivos do sistema.
pub(crate) fn attach_button(ui: &mut egui::Ui, enabled: bool, tip: &str) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(34.0, 34.0), if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    let ink = if !enabled { pal().muted.gamma_multiply(0.5) } else if reply.hovered() { pal().strong } else { pal().text };
    let stroke = egui::Stroke::new(2.0_f32, ink);
    let c = rect.center();
    // Uma alça comprida e a volta interna, como um clipe visto de frente.
    let pts = [vec2(-3.0, 8.0), vec2(-6.0, 5.0), vec2(-6.0, -7.0), vec2(0.0, -10.0), vec2(6.0, -7.0), vec2(6.0, 6.0), vec2(2.0, 9.0), vec2(-2.0, 6.0), vec2(-2.0, -4.0)];
    ui.painter().add(egui::Shape::line(pts.iter().map(|p| c + *p).collect(), stroke));
    reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, "Anexar arquivo"));
    let reply = reply.on_hover_text(tip);
    if enabled { reply.on_hover_cursor(egui::CursorIcon::PointingHand) } else { reply }
}
