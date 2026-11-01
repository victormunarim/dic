//! Abas Configurações e Atividade: conexão, identidade, servidores de apoio e o log.

use clam::sync::LockExt;
use std::sync::atomic::Ordering;

use eframe::egui::{self, Margin, RichText};
use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};

use crate::{conf::*, theme::*, ui::*};

/// Marca (em dado temporário da interface, nunca em arquivo) de que a chave secreta está à mostra no campo.
pub(crate) fn show_secret_id() -> egui::Id {
    egui::Id::new("mostrar-chave")
}

/// Corpo de um cartão que só se edita offline: apagado online, com o motivo no hover e no nome acessível. Fica
/// dentro do cartão (e não em volta de tudo) para o que é só leitura, como o menu da chave, seguir valendo.
fn lock<R>(ui: &mut egui::Ui, offline: bool, reason: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let scope = ui.add_enabled_ui(offline, add);
    if !offline {
        scope.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, false, reason));
        scope.response.on_hover_text(reason);
    }
    scope.inner
}

/// Chave secreta nova (64 hex). A troca só acontece depois da confirmação no menu da identidade.
fn fresh_secret() -> String {
    hex(&StaticSecret::random_from_rng(OsRng).to_bytes())
}

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
                if big_button(ui, "Ficar online", BLURPLE).clicked() {
                    self.go_online(ctx);
                }
            } else if why_off(button(ui, !in_call, "Ficar offline", pal().bg), !in_call, "Desligue a chamada antes de ficar offline").clicked()
                && let Some(r) = &self.running
            {
                r.stop.store(true, Ordering::Relaxed);
            }
            if ui.check(&mut self.form.auto_online, "Tentar ficar online ao abrir o programa").changed() {
                self.form.save();
            }
            if ui
                .check(&mut self.form.auto_files, "Baixar todo arquivo recebido até 100 MB")
                .on_hover_text("Desligado, o contato só envia um pedido: imagens e vídeos de até 50 MB descem sozinhos, e qualquer outro arquivo (ou maior) só vem para este computador quando você clica em baixar. Ligado, tudo de até 100 MB desce sozinho, de contatos, para a pasta Downloads/clam.")
                .changed()
            {
                self.form.save();
            }
            if !self.error.is_empty() {
                ui.colored_label(pal().error, &self.error);
            }
            details(
                ui,
                "Onde fica a configuração",
                &format!("{} ({})", conf_path().display(), if self.form.loaded { "lida ao abrir" } else { "não encontrada ao abrir; será criada" }),
            );
        });
        let locked = "Fique offline para mudar estes ajustes.";
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
        // O aviso fica colado nos cartões que ele trava; o código de contato acima segue valendo online.
        if !offline {
            note(ui, locked);
        }
        split(ui, |ui, side| {
            if side == 0 {
                card(ui, "Identidade", "A chave secreta (64 hex) é você. Não compartilhe com ninguém.", |ui| {
                    if self.form.keyring_lost && self.form.secret.trim().is_empty() {
                        note(ui, "A identidade está no chaveiro do sistema e ele não respondeu. Destrave-o e reabra o programa: a configuração segue apontando para ela. Gerar outra ou colar uma chave aqui a substitui.");
                    }
                    let shown_id = show_secret_id();
                    let shown = ui.data(|d| d.get_temp::<bool>(shown_id)).unwrap_or(false);
                    ui.horizontal(|ui| {
                        ui.add_enabled(offline, field(&mut self.form.secret, "chave secreta").password(!shown).desired_width(field_beside(ui, 34.0)))
                            .on_disabled_hover_text(locked);
                        // Mostrar e copiar só leem a chave e valem online; trocar muda o código de contato e fica
                        // num menu que pergunta e escreve o efeito.
                        let dots = dots_button(ui, "Mais ações da identidade");
                        let ask = egui::Id::new("gerar-identidade");
                        let menu = menu(&dots).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                            ui.set_min_width(MENU_WIDTH);
                            let have = !self.form.secret.trim().is_empty();
                            let blocked = (!have).then_some("Não há chave para mostrar");
                            let label = if shown { "Esconder a chave" } else { "Mostrar a chave" };
                            if menu_row(ui, blocked, false, None, label, None, |p, c, s, k| eye_icon(p, c, s, k, shown))
                                .on_hover_text("Quem vê a chave vira você: mostre só onde ninguém olha, e não grave a tela com ela aberta.")
                                .clicked()
                            {
                                ui.data_mut(|d| d.insert_temp(shown_id, !shown));
                                ui.close();
                            }
                            let blocked = (!have).then_some("Não há chave para copiar");
                            if menu_row(ui, blocked, false, None, "Copiar a chave", None, copy_icon)
                                .on_hover_text("Guarda a chave num lugar seguro (gerenciador de senhas). A área de transferência pode ser lida por outros programas: cole e copie outra coisa em seguida.")
                                .clicked()
                            {
                                ui.ctx().copy_text(self.form.secret.trim().to_string());
                                ui.close();
                            }
                            if offline {
                                let effect = "O seu código de contato muda e os outros precisarão do novo. A chave atual se perde se você não a tiver guardado.";
                                if confirm_item(ui, ask, "Gerar nova identidade", "Pede confirmação antes de trocar a chave", "Gerar uma nova identidade?", effect, "Gerar", |p, c, s, k| shield_icon(p, c, s, k, false)) {
                                    self.form.secret = fresh_secret();
                                    ui.close();
                                }
                            } else {
                                menu_row(ui, Some(locked), false, None, "Gerar nova identidade", None, |p, c, s, k| shield_icon(p, c, s, k, false));
                            }
                        });
                        if menu.is_none() {
                            reset_confirm(ui, ask);
                        }
                        menu_focus_back(ui, &dots, menu.is_some());
                    });
                });
                card(ui, "Seu nome", "Como os outros veem você na lista deles.", |ui| {
                    lock(ui, offline, locked, |ui| {
                        ui.horizontal(|ui| {
                            ui.add(field(&mut self.form.my_name, "seu nome").char_limit(clam::media::update::MAX_NAME).desired_width(field_beside(ui, 24.0)))
                                .on_hover_text("Vazio, os outros veem só o código.");
                            reopen_mark(ui);
                        });
                        ui.check(&mut self.form.intros, "Aceitar adições e remoções de contatos")
                            .on_hover_text("Quando alguém adiciona o seu código, ele manda uma apresentação cifrada e passa a aparecer na sua lista, para os dois se ligarem. Quando alguém te remove, ele avisa e sai da sua lista também (só vale de quem já é seu contato). Desligado, só entra na lista quem você adicionar e ninguém te tira da lista. Só aceita de quem já tem o seu endereço; no máximo 20 por minuto.");
                    });
                });
                let blocked = self.form.blocked_keys();
                if !blocked.is_empty() {
                    card(ui, "Bloqueados", "Não voltam à sua lista sozinhos, por apresentação ou convite. Adicionar o código de novo também desbloqueia.", |ui| {
                        lock(ui, offline, locked, |ui| {
                            for key in blocked {
                                ui.horizontal(|ui| {
                                    let code = hex(&key);
                                    let short = code.get(..8).unwrap_or(&code);
                                    let shown = self.form.blocked_name(&key).unwrap_or_else(|| short.to_string());
                                    ui.label(RichText::new(shown).color(pal().text)).on_hover_text(format!("Código {code}"));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if button(ui, true, "Desbloquear", pal().bg).clicked() {
                                            self.set_block(key, false);
                                        }
                                    });
                                });
                            }
                        });
                    });
                }
                card(ui, "Endereço", "Só se o IP que os outros enxergam não serve, como atrás de NAT.", |ui| {
                    lock(ui, offline, locked, |ui| {
                        ui.add(field(&mut self.form.public_addr, "ip:porta"))
                            .on_hover_text("ip:porta; a porta precisa estar aberta no roteador, e o endereço pode ser um nome.");
                    });
                });
            } else {
                card(ui, "2 relays", "Esconde o seu IP de quem você liga.", |ui| {
                    details(ui, "Como funciona", "Desligado (padrão), a ligação é direta: o contato e quem observa a rede perto dele ou de você veem o seu IP. O conteúdo segue cifrado de ponta a ponta. Ligado, a ligação passa pelo relay de outro contato (qualquer um serve): ele vê o seu IP e o destino, mas não o conteúdo, e o contato com quem você fala só vê o IP desse relay. Vale para voz, tela, bate-papo e arquivos. Sem relay de outro contato disponível, a ligação falha com o motivo, e nunca cai para o caminho direto. Voz, tela, bate-papo e arquivos: se o contato também usa a opção, vão por pontos de introdução e nenhum dos dois aprende o relay do outro; ligada, seu endereço deixa de ser entregue e gravado, um contato com a opção desligada deixa de achar você e um contato offline não recebe bate-papo. Apresentação e caixa de correio vão sem o seu endereço, e o seu código de contato leva só a chave. Servidores criam, convidam e entram sem endereço, mas só alcançam membros que são seus contatos (veja docs/DOIS-RELAYS.md).");
                    lock(ui, offline, locked, |ui| {
                        ui.check(&mut self.form.two_relays, "Passar as ligações por relays de outros contatos")
                            .on_hover_text("Desligado, a ligação é direta e o contato vê o seu IP. Ligado, o primeiro salto é o relay de outro contato e o destino não vê o seu IP. Um pouco mais lento. Vale ao reiniciar a conexão.");
                    });
                    if !self.form.two_relays {
                        note(ui, "Desligado: o contato e quem observa a rede veem o seu IP.");
                    }
                });
                card(ui, "Encontrar contatos", "Como reencontrar um contato que mudou de endereço.", |ui| {
                    details(ui, "Como funciona", "Quando um contato muda de endereço (IP novo, outra rede), o programa avisa direto. Se vocês dois mudarem enquanto estavam separados, o aviso não chega, e o reencontro usa a DHT, as âncoras ou as duas. Não existe servidor: o que é guardado vai cifrado e só o contato lê.");
                    lock(ui, offline, locked, |ui| {
                        // Ligada por padrão; desligar perde só o reencontro pela DHT.
                        ui.check(&mut self.form.use_dht, "Usar a DHT (BitTorrent) para reencontrar contatos")
                            .on_hover_text("A DHT do BitTorrent é uma rede pública e descentralizada de milhões de computadores. Seu nó guarda nela o endereço atual dele, cifrado para cada contato, e consulta o dos contatos ao ligar. Serve quando você e um contato mudam de endereço ao mesmo tempo. Só o contato lê o conteúdo, mas, com a opção “2 relays” desligada, quem opera nós da DHT vê o seu IP; ligada, o pedido vai por relays de contatos e a DHT só vê o IP do último. Ligada, seu nó também atende a DHT para circuitos de outros nós. Desligada, sobram o aviso direto e as âncoras.");
                    });
                });
                card(ui, "Âncoras", "Computadores de pessoas de confiança que guardam o seu endereço.", |ui| {
                    details(ui, "Como funciona", "Uma âncora é um computador com endereço fixo que guarda o seu endereço atual, cifrado para cada contato, para eles o acharem quando a DHT está bloqueada ou não basta. Escolha âncoras de pessoas em quem você confia: elas veem o IP de quem as usa, mas não o conteúdo.");
                    lock(ui, offline, locked, |ui| {
                        // Servir de âncora vem ligado; desligar só tira o seu computador da rede de âncoras dos outros.
                        ui.check(&mut self.form.anchor, "Servir de âncora para os registros de endereço dos outros")
                            .on_hover_text("Seu computador guarda, só em memória, o endereço cifrado de quem o usa como âncora, para os contatos dessa pessoa a acharem. Não lê o conteúdo nem forja item. Precisa de um endereço que os outros alcancem. Quem usa a sua âncora vê o IP do seu relay; o código da sua âncora aparece abaixo, para você passar a quem quiser.");
                        ui.horizontal(|ui| {
                            ui.label("Âncoras que você usa");
                            reopen_mark(ui);
                        });
                        ui.add(egui::TextEdit::multiline(&mut self.form.anchors).hint_text("ip:porta#chave-do-circuito#chave-do-link, uma por linha").desired_rows(3).desired_width(f32::INFINITY).margin(Margin::symmetric(12, 10)))
                            .on_hover_text("Uma por linha, no código que o dono da âncora te passa: ip:porta#chave-do-circuito#chave-do-link");
                    });
                });
            }
        });
        if self.form.anchor
            && let Some(r) = self.shared.relay.lock_unpoisoned().clone()
        {
            card(ui, "Código da sua âncora", "", |ui| {
                ui.horizontal(|ui| {
                    ui.monospace(format!("{}...", &anchor_code(&r)[..24]));
                    if button(ui, true, "Copiar", BLURPLE).clicked() {
                        ui.ctx().copy_text(anchor_code(&r));
                    }
                });
            });
        }
    }

    /// Estado e diagnóstico da transmissão de tela (taxa, QP, som...), que não ocupam mais a tela da chamada.
    fn broadcast_card(&self, ui: &mut egui::Ui) {
        let st = &self.shared.screen;
        let diag = st.diagnostics();
        let note_text = st.note.lock_unpoisoned().clone();
        let (wanted, sharing) = (st.want_share.load(Ordering::Relaxed), st.sharing.load(Ordering::Relaxed));
        if diag.is_empty() && note_text.is_empty() && !wanted && !sharing {
            return;
        }
        card(ui, "Transmissão da tela", "", |ui| {
            if sharing {
                ui.colored_label(pal().green, "sua tela está sendo vista")
                    .on_hover_text("Cada espectador abre um circuito de vídeo próprio, cifrado, direto com você (até 3 por transmissão).");
            } else if wanted {
                note(ui, "Sua tela está disponível: quem está na sala ou na chamada vê o botão de assistir ao lado do seu nome.");
            } else if !note_text.is_empty() {
                ui.colored_label(pal().error, note_text);
            }
            if !diag.is_empty() {
                ui.label(RichText::new(diag).monospace().size(13.0).color(pal().muted));
            }
        });
    }

    pub(crate) fn log_tab(&mut self, ui: &mut egui::Ui) {
        self.broadcast_card(ui);
        // Sem linhas o poço de altura cheia só carregava uma frase no canto: um cartão curto diz o mesmo e o poço
        // volta quando houver o que listar.
        if self.log.lock_unpoisoned().is_empty() {
            card(ui, "Nada ainda", &format!("Fique online {ONLINE_HINT}; o que o programa faz aparece aqui."), |_| {});
            return;
        }
        sunken_frame(ui, 12, 14, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
                let lines = self.log.lock_unpoisoned();
                for l in lines.iter() {
                    ui.label(RichText::new(l).monospace().size(13.5));
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_secret_is_64_hex_and_never_repeats() {
        let a = fresh_secret();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, fresh_secret());
    }

    #[test]
    fn locking_a_card_body_leaves_its_siblings_usable() {
        let ctx = egui::Context::default();
        let mut seen = Vec::new();
        for offline in [true, false] {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let inside = lock(ui, offline, "motivo", |ui| ui.is_enabled());
                    seen.push((offline, inside, ui.is_enabled()));
                });
            });
        }
        assert_eq!(seen, vec![(true, true, true), (false, false, true)], "só o corpo trava; o menu da chave ao lado segue valendo");
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::{theme::CARD_RECTS, ui::Tab};

    fn app() -> App {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        App {
            rt,
            form: Default::default(),
            log: Default::default(),
            shared: Default::default(),
            running: None,
            done: Default::default(),
            error: String::new(),
            tab: Tab::Settings,
            audio_dirty: false,
            screen_tex: Default::default(),
            zoom: None,
            chat_with: None,
            chat_draft: String::new(),
            global_keys: Default::default(),
            srv: Default::default(),
            devs: Default::default(),
        }
    }

    #[test]
    fn settings_cards_never_overlap() {
        for width in [620.0_f32, 900.0, 1300.0] {
            for offline in [true, false] {
                let mut app = app();
                let ctx = egui::Context::default();
                crate::theme::apply(&ctx);
                CARD_RECTS.with(|r| r.borrow_mut().clear());
                for _ in 0..3 {
                    CARD_RECTS.with(|r| r.borrow_mut().clear());
                    let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 1400.0))), ..Default::default() };
                    let _ = ctx.run(input, |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| app.settings_tab(ctx, ui, offline, false, ""));
                    });
                }
                let rects = CARD_RECTS.with(|r| r.borrow().clone());
                assert!(rects.len() >= 3, "largura {width}: só {} cartões", rects.len());
                for (i, (a, ra)) in rects.iter().enumerate() {
                    for (b, rb) in &rects[i + 1..] {
                        assert!(!ra.intersects(*rb), "largura {width}, offline {offline}: '{a}' {ra:?} sobrepõe '{b}' {rb:?}");
                    }
                }
            }
        }
    }

    /// Renderiza `f` numa janela de `size` e devolve os cartões desenhados (título e retângulo).
    fn cards(size: egui::Vec2, f: impl FnMut(&mut egui::Ui)) -> Vec<(String, egui::Rect)> {
        let mut f = f;
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        for _ in 0..3 {
            CARD_RECTS.with(|r| r.borrow_mut().clear());
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| f(ui));
            });
        }
        CARD_RECTS.with(|r| r.borrow().clone())
    }

    /// Atividade sem linhas é um cartão curto, não um poço que ocupa a altura toda.
    #[test]
    fn an_empty_activity_log_is_a_short_card() {
        let mut app = app();
        let rects = cards(egui::vec2(1000.0, 800.0), |ui| app.log_tab(ui));
        let (_, rect) = rects.iter().find(|(t, _)| t == "Nada ainda").expect("cartão de log vazio");
        assert!(rect.height() < 160.0, "o cartão vazio tem {} px", rect.height());
    }

    /// O tratamento da voz vem em cartões por assunto, sem um sobre o outro, nas duas larguras.
    #[test]
    fn voice_settings_are_grouped_in_cards_that_do_not_overlap() {
        for width in [700.0_f32, 1300.0] {
            let mut app = app();
            let rects = cards(egui::vec2(width, 1800.0), |ui| app.audio_tab(ui, true, false, false));
            for title in ["Ruído", "Eco", "Nível e formato"] {
                assert!(rects.iter().any(|(t, _)| t == title), "largura {width}: falta o cartão '{title}'");
            }
            for (i, (a, ra)) in rects.iter().enumerate() {
                for (b, rb) in &rects[i + 1..] {
                    assert!(!ra.intersects(*rb), "largura {width}: '{a}' {ra:?} sobrepõe '{b}' {rb:?}");
                }
            }
        }
    }
}
