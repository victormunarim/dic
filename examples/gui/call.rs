//! Aba Chamada: ligar, chamada em andamento e a tela de quem transmite.

use clam::sync::LockExt;
use std::sync::atomic::Ordering;

use eframe::egui;

use crate::{conf::parse_contacts, engine::*, theme::*, ui::*};

impl App {
    /// Cartão para ligar a um código de contato ou ao nome de um contato.
    pub(crate) fn dial_card(&mut self, ui: &mut egui::Ui, offline: bool, busy: bool) {
        card(ui, "Ligar para um código", "Cole o código de alguém, ou o nome de um contato. Também dá para ligar pelo telefone ao lado do contato.", |ui| {
            let live = !offline && !busy;
            // O botão fica na linha do campo, à direita: é dele que a ação sai, e abaixo ficava solto e apagado.
            let dial = ui.horizontal(|ui| {
                ui.add_enabled(live, field(&mut self.form.peer, "código de contato ou nome de um contato").desired_width(field_beside(ui, 44.0)));
                phone_button(ui, live, "Ligar", pal().green).clicked()
            });
            if dial.inner {
                match self.form.target() {
                    Ok(k) => {
                        self.form.save();
                        self.dial(k);
                    }
                    Err(e) => self.error = e,
                }
            }
            if offline {
                note(ui, format!("Fique online {ONLINE_HINT}."));
            }
        });
    }

    pub(crate) fn call_tab(&mut self, ui: &mut egui::Ui, offline: bool, busy: bool, in_call: bool) {
        if !in_call {
            if let Some(peer) = self.chat_with {
                let contacts = parse_contacts(&self.form.contacts);
                let online = self.online_contacts(&contacts).contains(&peer);
                self.chat_panel(ui, peer, online);
                return;
            }
            self.dial_card(ui, offline, busy);
            return;
        }
        let incoming = *self.shared.incoming.lock_unpoisoned();
        if let Some(who) = incoming {
            let name = member_name(&self.form.names(), &who);
            card(ui, "Chamada recebida", "", |ui| {
                status_dot(ui, pal().yellow, &format!("{name} está ligando para você"));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if phone_button(ui, true, "Atender", pal().green).clicked() {
                        self.shared.answer.store(ANSWER_ACCEPT, Ordering::Relaxed);
                    }
                    if hangup_button(ui, "Recusar").clicked() {
                        self.shared.answer.store(ANSWER_REJECT, Ordering::Relaxed);
                    }
                });
            });
            return;
        }
        // Só a conversa: estado, privacidade, desligar e transmissão estão na coluna da esquerda (barra do pé) e o
        // diagnóstico da transmissão em Atividade. Na chamada dá para escrever sem checar presença.
        if self.shared.ringing.lock_unpoisoned().is_some() {
            return;
        }
        let peer = *self.shared.peer.lock_unpoisoned();
        if let Some(peer) = peer {
            self.chat_panel(ui, peer, true);
        }
    }

    /// Mantém as texturas das telas que chegam em dia (a cada quadro da janela, qualquer que seja a aba).
    pub(crate) fn update_screen_textures(&mut self, ctx: &egui::Context) {
        let st = &self.shared.screen;
        let mut shown: Vec<([u8; 32], u64, egui::ColorImage)> = Vec::new();
        {
            let images = st.images.lock_unpoisoned();
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
            if let Some((n, tex)) = self.screen_tex.get_mut(&who) {
                tex.set(color, egui::TextureOptions::LINEAR);
                *n = seq;
            } else {
                let tex = ctx.load_texture(format!("tela-{}", who[0]), color, egui::TextureOptions::LINEAR);
                self.screen_tex.insert(who, (seq, tex));
            }
        }
    }
}
