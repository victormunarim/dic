//! Abas Configurações e Atividade: conexão, identidade, servidores de apoio e o log.

use clam::sync::LockExt;
use std::sync::atomic::Ordering;

use eframe::egui::{self, Margin, RichText, vec2};
use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};

use crate::{conf::*, theme::*, ui::*};

impl App {
    pub(crate) fn settings_tab(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, offline: bool, in_call: bool, status: &str) {
        let (color, text) = self.connection(offline, status);
        card(ui, "Conexão", "Ficar online sobe o seu relay e o telefone; é daí que as chamadas chegam e saem.", |ui| {
            status_dot(ui, color, text);
            if !status.is_empty() {
                note(ui, status);
            }
            ui.add_space(4.0);
            if offline {
                if big_button(ui, "Ficar online", pal().green).clicked() {
                    self.go_online(ctx);
                }
            } else if ui.add_enabled(!in_call, egui::Button::new(RichText::new("Ficar offline").strong().size(17.0)).min_size(vec2(180.0, 46.0))).clicked()
                && let Some(r) = &self.running
            {
                r.stop.store(true, Ordering::Relaxed);
            }
            if ui.check(&mut self.form.auto_online, "Tentar ficar online ao abrir o programa").changed() {
                self.form.save();
            }
            if !self.error.is_empty() {
                ui.colored_label(pal().error, &self.error);
            }
            note(
                ui,
                format!("Configuração: {} ({})", conf_path().display(), if self.form.loaded { "lida ao abrir" } else { "não encontrada ao abrir; será criada" }),
            );
        });
        if !offline {
            note(ui, "Fique offline para mudar estes ajustes.");
        }
        card(ui, "Meu código de contato", "Envie este código a quem vai ligar para você.", |ui| {
            let relay = self.shared.relay.lock_unpoisoned().clone();
            match self.form.my_code(relay.as_ref()) {
                Some(code) => {
                    sunken_frame(ui, 12, 12, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(RichText::new(if code.len() > 72 { format!("{}...", &code[..72]) } else { code.clone() }).monospace()).wrap());
                    });
                    if button(ui, true, "Copiar o código", BLURPLE).clicked() {
                        ui.ctx().copy_text(code);
                    }
                }
                None => {
                    note(ui, "(identidade inválida: veja Configurações)");
                }
            }
        });
        ui.add_enabled_ui(offline, |ui| {
            split(ui, |ui, side| {
                if side == 0 {
                    card(ui, "Identidade", "A chave secreta (64 hex) é você. Não compartilhe com ninguém.", |ui| {
                        if self.form.keyring_lost && self.form.secret.trim().is_empty() {
                            note(ui, "A identidade está no chaveiro do sistema e ele não respondeu. Destrave-o e reabra o programa: a configuração segue apontando para ela. Gerar outra ou colar uma chave aqui a substitui.");
                        }
                        ui.add(field(&mut self.form.secret, "chave secreta").password(true));
                        if ui.button("Gerar nova identidade").on_hover_text("O seu código de contato muda e os outros precisarão do novo.").clicked() {
                            self.form.secret = hex(&StaticSecret::random_from_rng(OsRng).to_bytes());
                        }
                    });
                    card(ui, "Seu nome", "Aparece na lista de contatos de quem você adiciona. Vazio, os outros veem só o código. Vale a partir da próxima vez que o programa abrir.", |ui| {
                        ui.add(field(&mut self.form.my_name, "seu nome").char_limit(clam::media::update::MAX_NAME));
                        ui.check(&mut self.form.intros, "Aceitar adições e remoções de contatos")
                            .on_hover_text("Quando alguém adiciona o seu código, ele manda uma apresentação cifrada e passa a aparecer na sua lista, para os dois se ligarem. Quando alguém te remove, ele avisa e sai da sua lista também (só vale de quem já é seu contato). Desligado, só entra na lista quem você adicionar e ninguém te tira da lista. Só aceita de quem já tem o seu endereço; no máximo 20 por minuto.");
                    });
                    let blocked = self.form.blocked_keys();
                    if !blocked.is_empty() {
                        card(ui, "Bloqueados", "Não voltam à sua lista sozinhos, por apresentação ou convite. Adicionar o código de novo também desbloqueia.", |ui| {
                            for key in blocked {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&hex(&key)[..8]).color(pal().text));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if close_button(ui, "Desbloquear").clicked() {
                                            self.set_block(key, false);
                                        }
                                    });
                                });
                            }
                        });
                    }
                    card(ui, "Endereço", "Só se o IP que os outros enxergam não serve, como atrás de NAT: ip:porta (a porta precisa estar aberta no roteador; pode ser um nome).", |ui| {
                        ui.add(field(&mut self.form.public_addr, "ip:porta"));
                    });
                } else {
                    card(ui, "Encontrar contatos", "Quando um contato muda de endereço (IP novo, outra rede), o programa avisa direto. Se vocês dois mudarem enquanto estavam separados, o aviso não chega, e o reencontro usa a DHT, as âncoras ou as duas. Não existe servidor: o que é guardado vai cifrado e só o contato lê.", |ui| {
                        // A DHT vem ligada: a caixa é para desligá-la.
                        let mut no_dht = !self.form.use_dht;
                        let off = ui
                            .check(&mut no_dht, "Não usar a DHT (BitTorrent) para reencontrar contatos")
                            .on_hover_text("A DHT do BitTorrent é uma rede pública e descentralizada de milhões de computadores. Seu nó guarda nela o endereço atual dele, cifrado para cada contato, e consulta o dos contatos ao ligar. Serve quando você e um contato mudam de endereço ao mesmo tempo. Só o contato lê o conteúdo, mas quem opera nós da DHT vê o seu IP (a opção abaixo esconde). Ligada, seu nó também atende a DHT para circuitos de outros nós. Desligada, sobram o aviso direto e as âncoras.");
                        if off.changed() {
                            self.form.use_dht = !no_dht;
                        }
                        ui.add_enabled_ui(self.form.use_dht, |ui| {
                            ui.check(&mut self.form.dht_circuit, "Falar com a DHT pelos relays dos contatos (esconde o seu IP)")
                                .on_hover_text("Em vez de falar com a DHT do seu IP, o pedido vai por um circuito de dois relays de contatos: o primeiro vê o seu IP mas não o pedido, o segundo vê o pedido mas não o seu IP, e a DHT só vê o IP do segundo. Precisa de ao menos dois contatos com endereço guardado; sem eles a DHT fica parada, e nunca cai para o acesso direto. Um pouco mais lento.");
                        });
                    });
                    card(ui, "Âncoras", "Uma âncora é um computador com endereço fixo que guarda o seu endereço atual, cifrado para cada contato, para eles o acharem quando a DHT está bloqueada ou não basta. Escolha âncoras de pessoas em quem você confia: elas veem o IP de quem as usa, mas não o conteúdo.", |ui| {
                        // Servir de âncora vem ligado: a caixa é para não servir.
                        let mut no_anchor = !self.form.anchor;
                        let off = ui
                            .check(&mut no_anchor, "Não servir de âncora para os registros de endereço dos outros")
                            .on_hover_text("Seu computador guarda, só em memória, o endereço cifrado de quem o usa como âncora, para os contatos dessa pessoa a acharem. Não lê o conteúdo nem forja item. Precisa de um endereço que os outros alcancem. Quem usa a sua âncora vê o IP do seu relay; o código da sua âncora aparece abaixo, para você passar a quem quiser.");
                        if off.changed() {
                            self.form.anchor = !no_anchor;
                        }
                        ui.label("Âncoras que você usa (uma por linha, no código que o dono da âncora te passa: ip:porta#chave-do-circuito#chave-do-link):");
                        ui.add(egui::TextEdit::multiline(&mut self.form.anchors).desired_rows(3).desired_width(f32::INFINITY).margin(Margin::symmetric(12, 10)));
                    });
                }
            });
        });
        if self.form.anchor
            && let Some(r) = self.shared.relay.lock_unpoisoned().clone()
        {
            card(ui, "Código da sua âncora", "", |ui| {
                ui.horizontal(|ui| {
                    ui.monospace(format!("{}...", &anchor_code(&r)[..24]));
                    if ui.button("Copiar").clicked() {
                        ui.ctx().copy_text(anchor_code(&r));
                    }
                });
            });
        }
    }

    pub(crate) fn log_tab(&mut self, ui: &mut egui::Ui) {
        sunken_frame(ui, 12, 14, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
                let lines = self.log.lock_unpoisoned();
                if lines.is_empty() {
                    note(ui, "Nada ainda. Fique online e o que o programa faz aparece aqui.");
                }
                for l in lines.iter() {
                    ui.label(RichText::new(l).monospace().size(13.5));
                }
            });
        });
    }
}
