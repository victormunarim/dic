//! Contatos: lista na coluna da esquerda, adicionar por código, confiança para a tela e apresentações recebidas.

use clam::sync::LockExt;

use eframe::egui::{self, Color32, RichText, vec2};

use crate::{conf::*, engine::*, theme::*, ui::*};

impl App {
    /// Quem se apresentou (o nó já o atende): se é novo, entra na lista e recebe de volta a apresentação desta
    /// pessoa, para ele ter o nome dela; se já era contato sem nome (foi adicionado só pelo código), ganha o nome
    /// que ele deu a si mesmo. Quem já tinha nome não muda, e não há apresentação de volta (senão as duas pontas
    /// ficariam se apresentando para sempre).
    pub(crate) fn absorb_introductions(&mut self) {
        let introduced: Vec<_> = std::mem::take(&mut *self.shared.introduced.lock_unpoisoned());
        let mut added = false;
        for i in introduced {
            if !parse_contacts(&self.form.contacts).iter().any(|(_, k)| *k == i.peer) {
                if !self.form.contacts.trim().is_empty() {
                    self.form.contacts.push('\n');
                }
                self.form.contacts.push_str(&intro_line(&i.name, &i.peer, &i.reach));
                if let Some(r) = &self.running {
                    let relay = clam::node::RelayInfo { addr: i.reach.addr.clone(), onion: i.reach.onion, link: i.reach.link };
                    let _ = r.cmds.send(Cmd::Contact(i.peer, Some(relay)));
                }
                added = true;
            } else if let Some(text) = name_contact(&self.form.contacts, &i.peer, &i.name) {
                self.form.contacts = text;
                added = true;
            }
        }
        if added {
            self.form.save();
        }
    }

    /// Lista de contatos na coluna da esquerda (o "início", como as mensagens diretas do Discord): um por linha,
    /// com ligar à direita e, no menu de três pontos, a confiança para a tela e remover. Embaixo, o formulário
    /// para adicionar, recolhido.
    pub(crate) fn contacts_list(&mut self, ui: &mut egui::Ui, online: bool, in_call: bool) {
        let contacts = parse_contacts(&self.form.contacts);
        if contacts.is_empty() {
            note(ui, "Nenhum contato ainda. Adicione o código de quem você quer ouvir.");
        }
        let mut remove = None;
        for (i, (name, key)) in contacts.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                avatar(ui, name, 32.0, None);
                // Adicionado só pelo código: o nome chega quando o contato devolve a apresentação.
                let pending = *name == hex(key)[..8];
                ui.add_sized([ui.available_width() - 76.0 - if pending { 22.0 } else { 0.0 }, 32.0], egui::Label::new(RichText::new(name).strong().color(TEXT)).truncate())
                    .on_hover_text(format!("{}…", &hex(key)[..16]));
                if pending {
                    ui.label(RichText::new("❗").size(14.0).color(YELLOW)).on_hover_text("O nome do contato aparecerá assim que você e ele estiverem online ao mesmo tempo.");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let dots = dots_button(ui);
                    egui::Popup::menu(&dots).show(|ui| {
                        let mut trusted = self.form.is_trusted(key);
                        if ui
                            .checkbox(&mut trusted, "Contato de confiança")
                            .on_hover_text("Só vale com a \"Privacidade na transmissão da tela\" desligada (aba Vídeo): a sua tela vai direto a este contato, sem relay no meio, e ele vê o seu IP. Os outros continuam recebendo por 2 relays. Marque só quem você conhece.")
                            .changed()
                        {
                            // Marcar pede confirmação do aviso; desmarcar vale na hora.
                            if trusted {
                                self.trust_ask = Some(*key);
                            } else {
                                self.set_trust(*key, false);
                            }
                            ui.close();
                        }
                        if ui.button(RichText::new("Remover contato").color(RED)).clicked() {
                            remove = Some(i);
                            ui.close();
                        }
                    });
                    if ui.add_enabled(online && !in_call, egui::Button::new(RichText::new("📞").size(16.0)).frame(false)).on_hover_text("Ligar").on_disabled_hover_text("Fique online e sem chamada para ligar").clicked() {
                        self.dial(*key);
                    }
                });
            });
        }
        ui.add_space(4.0);
        egui::CollapsingHeader::new(RichText::new("＋ Adicionar contato").size(14.0).color(MUTED)).id_salt("novo-contato").default_open(false).show(ui, |ui| {
            note(ui, "Peça o código de contato de um amigo.");
            ui.add(field(&mut self.form.new_code, "Código de contato"));
            if button(ui, !self.form.new_code.trim().is_empty(), "Adicionar", BLURPLE).clicked() {
                self.add_contact();
            }
            if !self.error.is_empty() {
                ui.colored_label(Color32::from_rgb(0xf2, 0x7b, 0x7e), &self.error);
            }
        });
        if let Some(i) = remove {
            let key = contacts[i].1;
            self.form.contacts = remove_contact(&self.form.contacts, i);
            // O endereço guardado do contato vai junto (a menos que a mesma chave ainda esteja na lista).
            if !parse_contacts(&self.form.contacts).iter().any(|(_, k)| *k == key) {
                // A confiança não volta sozinha se o contato for adicionado de novo.
                self.form.set_trusted(key, false);
                match &self.running {
                    Some(r) => {
                        let _ = r.cmds.send(Cmd::Forget(key));
                    }
                    None => {
                        if let Err(e) = forget_known(&key).and_then(|()| mark_pending(key, false)) {
                            self.error = format!("não deu para tirar o endereço do contato de {}: {e}", known_path().display());
                        }
                    }
                }
            }
            self.form.save();
        }
    }

    /// Marca ou desmarca o contato como de confiança, grava e avisa o nó. Quem marca já confirmou o aviso do IP.
    pub(crate) fn set_trust(&mut self, key: [u8; 32], on: bool) {
        self.form.set_trusted(key, on);
        self.form.save();
        if let Some(r) = &self.running {
            let _ = r.cmds.send(Cmd::Trust(key, on));
        }
    }

    /// Aviso de que o contato de confiança vê o IP de quem transmite; a confiança só vale depois do "Confiar".
    pub(crate) fn trust_window(&mut self, ctx: &egui::Context) {
        let Some(key) = self.trust_ask else { return };
        if !parse_contacts(&self.form.contacts).iter().any(|(_, k)| *k == key) {
            self.trust_ask = None;
            return;
        }
        let name = member_name(&self.form.names(), &key);
        let mut answer = None;
        egui::Window::new(format!("Confiar em {name}?"))
            .id(egui::Id::new("confiar-contato"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(format!("Com a \"Privacidade na transmissão da tela\" desligada, a sua tela vai direto a {name}, sem relay no meio: {name} e quem observa a rede perto dele passam a ver o seu endereço IP."));
                ui.add_space(4.0);
                ui.label("Marque só quem você conhece. Quem não é de confiança continua recebendo a tela por 2 relays. O vídeo segue cifrado nos dois casos.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if button(ui, true, "Confiar", BLURPLE).clicked() {
                        answer = Some(true);
                    }
                    if button(ui, true, "Cancelar", GREY_BUTTON).clicked() {
                        answer = Some(false);
                    }
                });
            });
        if let Some(yes) = answer {
            self.trust_ask = None;
            if yes {
                self.set_trust(key, true);
            }
        }
    }

    pub(crate) fn add_contact(&mut self) {
        if split_code(&self.form.new_code).is_none() {
            self.error = "Esse código de contato não parece certo (são 64 letras e números, e o que vem depois deles tem de estar inteiro).".into();
            return;
        }
        if let Some((key, relay)) = split_direct(&self.form.new_code) {
            match (&self.running, relay) {
                (Some(r), relay) => {
                    let _ = r.cmds.send(Cmd::Contact(key, relay));
                }
                (None, relay) => {
                    // Com o nó parado: o endereço entra como dica e a apresentação fica na fila do próximo início.
                    if let Err(e) = relay.map_or(Ok(()), |r| remember_hint(key, r)).and_then(|()| mark_pending(key, true)) {
                        self.error = format!("não deu para guardar o contato em {}: {e}", known_path().display());
                    }
                }
            }
        }
        if !self.form.contacts.trim().is_empty() {
            self.form.contacts.push('\n');
        }
        self.form.contacts.push_str(self.form.new_code.trim());
        self.form.new_code.clear();
        self.error.clear();
        self.form.save();
    }
}
