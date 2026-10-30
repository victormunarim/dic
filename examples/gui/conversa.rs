//! Bate-papo 1:1 com um contato: histórico guardado aqui, envio só com o contato online (`docs/CHAMADAS.md`).

use clam::sync::LockExt;

use eframe::egui::{self, Color32, RichText, vec2};

use crate::{chats::State, engine::*, theme::*, ui::*};

impl App {
    /// A conversa com `peer`. `online`: o contato está de pé agora (ou em chamada comigo), então dá para enviar.
    pub(crate) fn chat_panel(&mut self, ui: &mut egui::Ui, peer: [u8; 32], online: bool) {
        let name = member_name(&self.form.names(), &peer);
        let msgs = self.shared.chats.lock_unpoisoned().with(&peer).to_vec();
        self.shared.chats.lock_unpoisoned().read(&peer);
        let sendable = online && self.running.is_some();
        let mut close = false;
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
