//! Aba Chamada: ligar, chamada em andamento e a tela de quem transmite.

use clam::sync::LockExt;
use std::sync::atomic::Ordering;

use eframe::egui::{self, Color32, CornerRadius, Rect, RichText, vec2};

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
        self.call_bar(ui, ringing);
        if !ringing {
            // A tela vem antes da conversa: é o que a pessoa foi ver; o bate-papo fica logo abaixo.
            self.screens_ui(ui);
            // Na chamada a conversa é com quem está na linha, e dá para escrever sem checar presença.
            let peer = *self.shared.peer.lock_unpoisoned();
            if let Some(peer) = peer {
                self.chat_panel(ui, peer, true);
            }
        }
    }

    /// Mantém as texturas das telas que chegam em dia (a cada quadro da janela, qualquer que seja a aba).
    pub(crate) fn update_screen_textures(&mut self, ctx: &egui::Context) {
        let st = &self.shared.screen;
        let mut shown: Vec<([u8; 32], u64, egui::ColorImage)> = Vec::new();
        {
            let images = st.images.lock_unpoisoned();
            self.screen_tex.retain(|k, _| images.contains_key(k));
            self.screen_thumb.retain(|k, _| images.contains_key(k));
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
                // A miniatura é uma textura à parte, criada uma vez: fica parada no que a tela mostrava ao chegar.
                let thumb = ctx.load_texture(format!("mini-{}", who[0]), color.clone(), egui::TextureOptions::LINEAR);
                self.screen_thumb.insert(who, thumb);
                let tex = ctx.load_texture(format!("tela-{}", who[0]), color, egui::TextureOptions::LINEAR);
                self.screen_tex.insert(who, (seq, tex));
            }
        }
    }

    /// Barra única da chamada: estado e privacidade à esquerda; som da tela e desligar à direita. Quem transmite vê aqui,
    /// numa linha, que a tela está sendo vista; o diagnóstico fica recolhido. Nada disso ganha cartão próprio.
    fn call_bar(&mut self, ui: &mut egui::Ui, ringing: bool) {
        let st = &self.shared.screen;
        let (wanted, sharing) = (st.want_share.load(Ordering::Relaxed), st.sharing.load(Ordering::Relaxed));
        let note_text = st.note.lock_unpoisoned().clone();
        let diag = st.diagnostics();
        let link = self.shared.link.lock_unpoisoned().clone();
        ui.add_space(4.0);
        raised_frame(ui, R_CARD, 16, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let state = if ringing { status_dot(ui, pal().yellow, "Chamando…") } else { status_dot(ui, pal().green, "Em chamada") };
                if !link.is_empty() {
                    state.on_hover_text(format!("Linha: {link}"));
                }
                if !ringing {
                    privacy_badge(ui, "Privacidade da chamada", "Circuito de 2 saltos, cifrado em camadas, com tráfego constante: quem observa a rede não vê quando você fala. O primeiro salto é o relay de quem liga, então o contato vê o IP dele.");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if hangup_button(ui, if ringing { "Cancelar a chamada" } else { "Desligar a chamada" }).clicked() {
                        self.shared.hangup.store(true, Ordering::Relaxed);
                    }
                });
            });
            if sharing {
                // Ponto desenhado: o "●" da fonte não existe e saía como um quadrado vazio.
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 4.0, pal().green);
                    ui.label(RichText::new("sua tela está sendo vista").strong().color(pal().green));
                });
            } else if wanted {
                note(ui, "ligando o circuito de vídeo...");
            }
            if !wanted && !sharing && !note_text.is_empty() {
                ui.colored_label(pal().error, note_text);
            }
            if !diag.is_empty() {
                // Id fixo: o texto muda a cada segundo e não pode ser a chave do estado aberto/fechado.
                egui::CollapsingHeader::new(RichText::new("Detalhes da transmissão").size(14.0).color(pal().muted))
                    .id_salt("detalhes-transmissao")
                    .icon(chevron)
                    .show(ui, |ui| {
                        ui.label(RichText::new(diag).monospace().size(13.0).color(pal().muted));
                    });
            }
        });
        ui.add_space(14.0);
    }

    /// As telas de quem transmite para você: uma miniatura parada, pequena. Assistir é clicar nela (tela cheia), e só
    /// então o som dela toca: a miniatura não tem som nem botão de som.
    fn screens_ui(&mut self, ui: &mut egui::Ui) {
        let names = self.form.names();
        let mut order: Vec<_> = self.screen_thumb.iter().map(|(k, t)| (*k, t.clone())).collect();
        order.sort_by_key(|(k, _)| *k);
        for (who, tex) in order {
            let size = tex.size_vec2();
            let w = ui.available_width().min(260.0);
            let (rect, resp) = ui.allocate_exact_size(vec2(w, w * size.y / size.x.max(1.0)), egui::Sense::click());
            let radius = CornerRadius::same(12);
            let p = ui.painter();
            egui::Image::new(&tex).corner_radius(radius).paint_at(ui, rect);
            let hot = resp.hovered();
            p.rect_filled(rect, radius, Color32::from_black_alpha(if hot { 70 } else { 30 }));
            // Triângulo de "assistir" desenhado: o glifo da fonte pode faltar.
            let c = rect.center();
            p.circle_filled(c, 20.0, Color32::from_black_alpha(150));
            p.add(egui::Shape::convex_polygon(vec![c + vec2(-6.0, -10.0), c + vec2(-6.0, 10.0), c + vec2(10.0, 0.0)], Color32::WHITE, egui::Stroke::NONE));
            // Nome no canto de cima, sobre um fundo escuro para ler em qualquer imagem.
            let label = p.layout_no_wrap(member_name(&names, &who), egui::FontId::proportional(13.0), Color32::WHITE);
            let tag = Rect::from_min_size(rect.min + vec2(8.0, 8.0), label.size() + vec2(12.0, 6.0));
            p.rect_filled(tag, CornerRadius::same(8), Color32::from_black_alpha(150));
            p.galley(tag.min + vec2(6.0, 3.0), label, Color32::WHITE);
            let resp = resp.on_hover_text("Assistir a tela (Esc ou o X para sair)").on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                self.zoom = Some(who);
            }
            ui.add_space(14.0);
        }
    }
}
