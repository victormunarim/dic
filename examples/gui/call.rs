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
            self.screen_rate.retain(|k, _| images.contains_key(k));
            for (who, (seq, img)) in images.iter() {
                self.screen_rate.entry(*who).or_insert_with(|| FrameRate::new(*seq)).seen(*seq);
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

/// Mede quantos quadros por segundo uma tela entrega: conta quantos `seq` novos chegaram numa janela
/// de ~1 s e guarda o último resultado (a janela é curta para o selo reagir, longa para não tremer).
pub(crate) struct FrameRate {
    since: std::time::Instant,
    first: u64,
    last: u64,
    fps: f32,
}

impl FrameRate {
    const WINDOW: std::time::Duration = std::time::Duration::from_millis(1000);

    pub(crate) fn new(seq: u64) -> Self {
        Self { since: std::time::Instant::now(), first: seq, last: seq, fps: 0.0 }
    }

    /// Registra o `seq` do quadro mais novo; fecha a janela quando passou o tempo.
    pub(crate) fn seen(&mut self, seq: u64) {
        self.seen_at(seq, std::time::Instant::now());
    }

    fn seen_at(&mut self, seq: u64, now: std::time::Instant) {
        self.last = seq;
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed >= Self::WINDOW {
            self.fps = self.last.saturating_sub(self.first) as f32 / elapsed.as_secs_f32();
            self.since = now;
            self.first = self.last;
        }
    }

    pub(crate) fn fps(&self) -> f32 {
        self.fps
    }
}

#[cfg(test)]
mod frame_rate_tests {
    use super::FrameRate;
    use std::time::{Duration, Instant};

    #[test]
    fn counts_new_sequence_numbers_per_second() {
        let t0 = Instant::now();
        let mut r = FrameRate::new(10);
        r.since = t0;
        r.seen_at(20, t0 + Duration::from_millis(500));
        assert_eq!(r.fps(), 0.0, "janela ainda aberta");
        r.seen_at(40, t0 + Duration::from_secs(2));
        assert_eq!(r.fps(), 15.0, "30 quadros em 2 s");
    }
}
