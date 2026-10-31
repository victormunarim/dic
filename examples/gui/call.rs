//! Aba Chamada: ligar, chamada em andamento e a tela de quem transmite.

use clam::sync::LockExt;
use std::sync::atomic::Ordering;

use eframe::egui::{self, CornerRadius, RichText};

use crate::{conf::parse_contacts, engine::*, theme::*, ui::*};

impl App {
    /// Cartão para ligar a um código de contato ou ao nome de um contato.
    pub(crate) fn dial_card(&mut self, ui: &mut egui::Ui, offline: bool, busy: bool) {
        card(ui, "Ligar para um código", "Cole o código de contato de alguém, ou o nome de um contato.", |ui| {
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
            card(ui, "Nenhuma chamada em andamento", "Ligue pelo telefone ao lado do contato; a chamada e a tela compartilhada aparecem aqui.", |_| {});
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
        let ringing = self.shared.ringing.lock_unpoisoned().is_some();
        card(ui, if ringing { "Chamando" } else { "Em chamada" }, "", |ui| {
            if ringing {
                status_dot(ui, pal().yellow, "Chamando… aguardando a pessoa atender");
            } else {
                ui.horizontal(|ui| {
                    status_dot(ui, pal().green, "Conversando agora");
                    privacy_badge(ui, "Privacidade da chamada", "Circuito de 2 saltos, cifrado em camadas, com tráfego constante: quem observa a rede não vê quando você fala. O primeiro salto é o relay de quem liga, então o contato vê o IP dele.");
                });
            }
            let link = self.shared.link.lock_unpoisoned().clone();
            if !link.is_empty() {
                note(ui, format!("Linha: {link}"));
            }
            ui.add_space(4.0);
            if hangup_button(ui, if ringing { "Cancelar a chamada" } else { "Desligar a chamada" }).clicked() {
                self.shared.hangup.store(true, Ordering::Relaxed);
            }
        });
        if !ringing {
            // Na chamada a conversa é com quem está na linha, e dá para escrever sem checar presença.
            let peer = *self.shared.peer.lock_unpoisoned();
            if let Some(peer) = peer {
                self.chat_panel(ui, peer, true);
            }
            self.screen_ui(ui);
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

    /// As duas chaves do som da tela, à mão na própria transmissão: `send` (estou transmitindo) mostra "Enviar o
    /// som do computador"; `hear` (estou assistindo alguém) mostra "Ouvir o som das telas". Valem na hora e ficam salvas.
    pub(crate) fn screen_sound_toggles(&mut self, ui: &mut egui::Ui, send: bool, hear: bool) {
        let mut changed = false;
        if send {
            changed |= ui
                .check(&mut self.form.screen_audio, "Enviar o som do computador")
                .on_hover_text("Quem assiste ouve o que toca no seu computador (música, jogo, vídeo, notificações), menos o que o próprio programa toca: a voz do contato e o som de tela de outros. A exclusão não é perfeita, sobra um resto fraco da voz do contato; com fones some. Usa cerca de 10% da banda do vídeo (2,5% sem privacidade) e, sem privacidade, quem observa a rede vê quando há som. Só no Linux (precisa de gstreamer1.0-tools e pactl). Vale na hora; a imagem se ajusta à banda na próxima transmissão.")
                .changed();
        }
        if hear {
            changed |= ui
                .check(&mut self.form.screen_hear, "Ouvir o som das telas")
                .on_hover_text("Toca o som que vem junto das telas que você assiste, no volume do alto-falante. Desmarcado, o som é descartado e não é nem decodificado. Vale na hora.")
                .changed();
        }
        if changed {
            self.form.save();
        }
    }

    /// Compartilhar a própria tela e ver a dos outros, na chamada.
    pub(crate) fn screen_ui(&mut self, ui: &mut egui::Ui) {
        let st = &self.shared.screen;
        let (wanted, sharing) = (st.want_share.load(Ordering::Relaxed), st.sharing.load(Ordering::Relaxed));
        let note_text = st.note.lock_unpoisoned().clone();
        let diag = st.diagnostics();

        // Compartilhar e silenciar ficam na barra do pé da coluna; aqui só o estado e o diagnóstico da transmissão.
        if sharing || wanted || !diag.is_empty() || !note_text.is_empty() {
            card(ui, "Sua transmissão", "O vídeo segue cifrado de ponta a ponta, em um circuito só dele.", |ui| {
                if sharing {
                    ui.colored_label(pal().green, RichText::new("● sua tela está sendo vista").strong());
                } else if wanted {
                    note(ui, "ligando o circuito de vídeo...");
                }
                if !wanted && !sharing && !note_text.is_empty() {
                    ui.colored_label(pal().error, note_text);
                }
                self.screen_sound_toggles(ui, true, false);
                if !diag.is_empty() {
                    ui.label(RichText::new(diag).monospace().size(13.0).color(pal().muted));
                }
            });
        }

        let names = self.form.names();
        if !self.screen_tex.is_empty() {
            card(ui, "Som das telas", "", |ui| self.screen_sound_toggles(ui, false, true));
        }
        let mut order: Vec<_> = self.screen_tex.iter().collect();
        order.sort_by_key(|(k, _)| **k);
        for (who, (_, tex)) in order {
            card(ui, &format!("Tela de {}", member_name(&names, who)), "", |ui| {
                if magnifier_button(ui, "Tela cheia (Esc ou o X para sair)").clicked() {
                    self.zoom = Some(*who);
                }
                // Em tela cheia a miniatura sai: a imagem não é desenhada duas vezes.
                if self.zoom != Some(*who) {
                    ui.add(egui::Image::new(tex).max_width(ui.available_width()).corner_radius(CornerRadius::same(8)).shrink_to_fit());
                }
            });
        }
    }
}
