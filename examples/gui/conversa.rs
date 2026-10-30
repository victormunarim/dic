//! Bate-papo 1:1 com um contato: histórico guardado aqui, envio só com o contato online (`docs/CHAMADAS.md`).

use clam::sync::LockExt;

use eframe::egui::{self, Color32, RichText, vec2};

use crate::{arquivos::{Phase, size_text}, chats::State, engine::*, theme::*, ui::*};

impl App {
    /// A conversa com `peer`. `online`: o contato está de pé agora (ou em chamada comigo), então dá para enviar.
    pub(crate) fn chat_panel(&mut self, ui: &mut egui::Ui, peer: [u8; 32], online: bool) {
        let name = member_name(&self.form.names(), &peer);
        let msgs = self.shared.chats.lock_unpoisoned().with(&peer).to_vec();
        self.shared.chats.lock_unpoisoned().read(&peer);
        let sendable = online && self.running.is_some();
        let mut close = false;
        // Arquivo solto na janela vira oferta ao contato (só com ele online).
        if sendable {
            let dropped = ui.ctx().input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect::<Vec<_>>());
            if let Some(r) = &self.running {
                for path in dropped {
                    let _ = r.cmds.send(Cmd::SendFile(peer, path));
                }
            }
        }
        let xfers = self.shared.transfers.lock_unpoisoned().of(&peer);
        let mut file_cmds: Vec<Cmd> = Vec::new();
        // Como `card`, mas com o X no fim da linha do título.
        ui.add_space(4.0);
        raised_frame(ui, R_CARD, 22, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Bate-papo com {name}")).strong().size(18.0).color(Color32::WHITE));
                // Em chamada a conversa é a da linha: fechar não faz sentido.
                if !self.shared.in_call.load(std::sync::atomic::Ordering::Relaxed) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if close_button(ui, "Fechar a conversa").clicked() {
                            close = true;
                        }
                    });
                }
            });
            note(ui, "Só vai para quem está online. Cada um guarda a sua cópia nesta máquina.");
            ui.add_space(6.0);
            for x in xfers.iter().rev().take(5).rev() {
                file_row(ui, x, &mut file_cmds);
            }
            let height = (ui.clip_rect().bottom() - ui.cursor().top() - 120.0).clamp(160.0, 420.0);
            egui::ScrollArea::vertical().id_salt(("conversa", peer)).max_height(height).min_scrolled_height(height).stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                if msgs.is_empty() {
                    note(ui, "Sem mensagens ainda.");
                }
                for m in &msgs {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let (who, color) = if m.mine() { ("Você", GREEN) } else { (name.as_str(), Color32::WHITE) };
                        ui.label(RichText::new(format!("{who}:")).strong().color(color));
                        ui.label(RichText::new(&m.text).color(TEXT));
                        match m.state {
                            State::Sending => ui.label(RichText::new("enviando…").size(12.0).color(MUTED)),
                            State::Failed => ui.label(RichText::new("não entregue").size(12.0).color(Color32::from_rgb(0xf2, 0x7b, 0x7e))).on_hover_text("O contato não confirmou: estava offline ou sem rota. Não há caixa de correio; envie de novo quando ele voltar."),
                            State::Sent | State::Received => ui.label(""),
                        };
                    });
                }
            });
            ui.add_space(4.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 36.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ready = sendable && !self.chat_draft.trim().is_empty();
                let sent = button(ui, ready, "Enviar", BLURPLE).clicked();
                if attach_button(ui, sendable).clicked()
                    && let Some(r) = &self.running
                {
                    // O diálogo do sistema bloqueia: roda fora da janela e devolve o caminho como comando.
                    let (tx, repaint) = (r.cmds.clone(), ui.ctx().clone());
                    std::thread::spawn(move || {
                        if let Some(path) = rfd::FileDialog::new().set_title("Enviar arquivo").pick_file() {
                            let _ = tx.send(Cmd::SendFile(peer, path));
                            repaint.request_repaint();
                        }
                    });
                }
                let hint = if sendable { "Mensagem" } else { "Contato offline: não dá para enviar" };
                let reply = ui.add_enabled(sendable, field(&mut self.chat_draft, hint).desired_width(ui.available_width().max(80.0)));
                let enter = reply.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (sent || enter) && ready {
                    let text = std::mem::take(&mut self.chat_draft);
                    if let Some(r) = &self.running {
                        let _ = r.cmds.send(Cmd::Chat(peer, text));
                    }
                    reply.request_focus();
                }
            });
        });
        ui.add_space(18.0);
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

/// Uma transferência na conversa: nome e tamanho, e o que a pessoa pode fazer agora.
fn file_row(ui: &mut egui::Ui, x: &crate::arquivos::Xfer, cmds: &mut Vec<Cmd>) {
    ui.horizontal(|ui| {
        let way = if x.mine { "Enviando" } else { "Recebendo" };
        ui.label(RichText::new(format!("{} ({})", x.name, size_text(x.size))).strong().color(Color32::WHITE));
        match &x.phase {
            // Aceito sozinho pelo laço; aqui só aparece no instante entre a oferta e o início.
            Phase::Asking | Phase::Offering => {
                ui.label(RichText::new(if x.mine { "esperando o contato…" } else { "começando…" }).color(MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if close_button(ui, "Cancelar").clicked() {
                        cmds.push(Cmd::FileCancel(x.peer, x.id, x.mine));
                    }
                });
            }
            Phase::Active => {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if close_button(ui, "Cancelar").clicked() {
                        cmds.push(Cmd::FileCancel(x.peer, x.id, x.mine));
                    }
                    let frac = x.bytes as f32 / x.size.max(1) as f32;
                    ui.add(egui::ProgressBar::new(frac).desired_width(ui.available_width().max(60.0)).text(format!("{way} {}%", (frac * 100.0) as u32)).fill(BLURPLE));
                });
            }
            Phase::Done(path) => {
                let r = ui.label(RichText::new(if x.mine { "enviado" } else { "recebido" }).color(GREEN));
                if let Some(p) = path {
                    r.on_hover_text(format!("Salvo em {}", p.display()));
                }
            }
            Phase::Failed(e) => {
                ui.label(RichText::new("falhou").color(RED)).on_hover_text(e);
            }
            Phase::Declined => {
                ui.label(RichText::new("recusado").color(MUTED));
            }
            Phase::Cancelled => {
                ui.label(RichText::new("cancelado").color(MUTED));
            }
        }
    });
}

/// Clipe de papel desenhado (a fonte pode não ter o glifo): abre o seletor de arquivos do sistema.
fn attach_button(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(34.0, 34.0), egui::Sense::click());
    let ink = if !enabled { MUTED.gamma_multiply(0.5) } else if reply.hovered() { Color32::WHITE } else { TEXT };
    let stroke = egui::Stroke::new(2.0_f32, ink);
    let c = rect.center();
    // Uma alça comprida e a volta interna, como um clipe visto de frente.
    let pts = [vec2(-3.0, 8.0), vec2(-6.0, 5.0), vec2(-6.0, -7.0), vec2(0.0, -10.0), vec2(6.0, -7.0), vec2(6.0, 6.0), vec2(2.0, 9.0), vec2(-2.0, 6.0), vec2(-2.0, -4.0)];
    ui.painter().add(egui::Shape::line(pts.iter().map(|p| c + *p).collect(), stroke));
    let tip = "Anexar arquivo: abre as pastas do computador (ou arraste o arquivo para a janela). Limite de 32 MB";
    let reply = reply.on_hover_text(tip);
    if enabled { reply.on_hover_cursor(egui::CursorIcon::PointingHand) } else { reply }
}
