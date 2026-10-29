//! Aba Chamada: ligar, chamada em andamento e a tela de quem transmite.

use clam::sync::LockExt;
use std::sync::atomic::Ordering;

use eframe::egui::{self, Color32, CornerRadius, RichText};

use crate::{engine::*, theme::*, ui::*};

impl App {
    /// Cartão para ligar a um código de contato ou ao nome de um contato.
    pub(crate) fn dial_card(&mut self, ui: &mut egui::Ui, offline: bool, busy: bool) {
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

    pub(crate) fn call_tab(&mut self, ui: &mut egui::Ui, offline: bool, busy: bool, in_call: bool) {
        if !in_call {
            self.dial_card(ui, offline, busy);
            card(ui, "Nenhuma chamada em andamento", "Quando você ligar ou atender, ela aparece aqui, com a tela compartilhada.", |ui| {
                note(ui, "Para ligar a um contato, use o 📞 ao lado dele na lista de contatos (👤, na coluna da esquerda).");
            });
            return;
        }
        card(ui, "Em chamada", "", |ui| {
            status_dot(ui, GREEN, "Conversando agora");
            let link = self.shared.link.lock_unpoisoned().clone();
            if !link.is_empty() {
                note(ui, format!("Linha: {link}"));
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let muted = self.shared.audio.muted.load(Ordering::Relaxed);
                if icon_button(ui, true, if muted { "🔇" } else { "🎤" }, if muted { "Tirar do mudo" } else { "Silenciar" }, if muted { RED } else { GREY_BUTTON }).clicked() {
                    self.shared.audio.muted.store(!muted, Ordering::Relaxed);
                }
                if hangup_button(ui).on_hover_text("Desligar a chamada").clicked() {
                    self.shared.hangup.store(true, Ordering::Relaxed);
                }
            });
        });
        self.screen_ui(ui);
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
    pub(crate) fn screen_ui(&mut self, ui: &mut egui::Ui) {
        let st = &self.shared.screen;
        let (wanted, sharing) = (st.want_share.load(Ordering::Relaxed), st.sharing.load(Ordering::Relaxed));
        let note_text = st.note.lock_unpoisoned().clone();
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
