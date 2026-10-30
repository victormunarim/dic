//! Contatos: lista na coluna da esquerda, adicionar por código, confiança para a tela e apresentações recebidas.

use clam::sync::LockExt;

use eframe::egui::{self, Color32, RichText, vec2};

use crate::{conf::*, engine::*, theme::*, ui::*};

/// Se o contato aparece na busca: o texto (minúsculo, não vazio ou vazio) está no nome ou no começo do código.
fn matches_search(name: &str, key: &[u8; 32], needle: &str) -> bool {
    needle.is_empty() || name.to_lowercase().contains(needle) || hex(key).starts_with(needle)
}

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

    /// Quem removeu esta pessoa (o nó já o esqueceu): sai da lista e perde a confiança. Sem aviso de volta, senão as
    /// duas pontas ficariam se avisando.
    pub(crate) fn absorb_removals(&mut self) {
        let removed: Vec<[u8; 32]> = std::mem::take(&mut *self.shared.removed.lock_unpoisoned());
        if removed.is_empty() {
            return;
        }
        for key in removed {
            while let Some(i) = parse_contacts(&self.form.contacts).iter().position(|(_, k)| *k == key) {
                self.form.contacts = remove_contact(&self.form.contacts, i);
            }
            self.form.set_trusted(key, false);
        }
        self.form.save();
    }

    /// Formulário de novo contato, dentro do menu do + do cabeçalho da coluna.
    pub(crate) fn new_contact_form(&mut self, ui: &mut egui::Ui) {
        ui.set_width(260.0);
        note(ui, "Peça o código de contato de um amigo.");
        ui.add(field(&mut self.form.new_code, "Código de contato"));
        if button(ui, !self.form.new_code.trim().is_empty(), "Adicionar", BLURPLE).clicked() {
            self.add_contact();
            if self.error.is_empty() {
                ui.close();
            }
        }
        if !self.error.is_empty() {
            ui.colored_label(Color32::from_rgb(0xf2, 0x7b, 0x7e), &self.error);
        }
    }

    /// Contatos online agora: a sondagem do nó (`Shared::presence`) ou um servidor em comum que os mostra online.
    /// Quem ainda não foi sondado não está na lista, e o 📞 espera o resultado.
    pub(crate) fn online_contacts(&self, contacts: &[(String, [u8; 32])]) -> std::collections::HashSet<[u8; 32]> {
        let keys: Vec<[u8; 32]> = contacts.iter().map(|(_, k)| *k).collect();
        {
            let mut watch = self.shared.watch.lock_unpoisoned();
            if *watch != keys {
                *watch = keys;
            }
        }
        let mut up: std::collections::HashSet<[u8; 32]> = self.shared.presence.lock_unpoisoned().iter().filter(|(_, on)| **on).map(|(k, _)| *k).collect();
        if let Some(hub) = self.shared.servers.lock_unpoisoned().clone() {
            for v in hub.read(|s, now| s.views(now)) {
                up.extend(v.members.iter().filter(|m| !m.is_me && m.online).map(|m| m.key));
            }
        }
        up
    }

    /// Lista de contatos na coluna da esquerda (o "início", como as mensagens diretas do Discord): um por linha,
    /// com ligar, a confiança para a tela (escudo) e remover (lixeira) à direita. No alto, a busca.
    pub(crate) fn contacts_list(&mut self, ui: &mut egui::Ui, online: bool, in_call: bool) {
        let contacts = parse_contacts(&self.form.contacts);
        // O texto da busca vive na memória temporária da janela: não vai para o disco nem precisa de campo em `App`.
        let search_id = egui::Id::new("busca-contatos");
        let mut query = ui.data_mut(|d| d.get_temp::<String>(search_id)).unwrap_or_default();
        ui.add(field(&mut query, "🔍 Buscar contato"));
        ui.data_mut(|d| d.insert_temp(search_id, query.clone()));
        let needle = query.trim().to_lowercase();
        if contacts.is_empty() {
            note(ui, "Nenhum contato ainda. Use o + do alto com o código de quem você quer ouvir.");
        } else if !needle.is_empty() && !contacts.iter().any(|(n, k)| matches_search(n, k, &needle)) {
            note(ui, "Ninguém com esse nome.");
        }
        let online_keys = self.online_contacts(&contacts);
        let probed = self.shared.presence.lock_unpoisoned().clone();
        let ringing = *self.shared.ringing.lock_unpoisoned();
        let mut remove = None;
        // Quem tem mensagem nova vem primeiro (a ordem entre os demais se mantém).
        let unread: std::collections::HashMap<[u8; 32], usize> = {
            let book = self.shared.chats.lock_unpoisoned();
            contacts.iter().map(|(_, k)| (*k, book.unread(k))).collect()
        };
        let mut shown: Vec<_> = contacts.iter().enumerate().filter(|(_, (n, k))| matches_search(n, k, &needle)).collect();
        shown.sort_by_key(|(_, (_, k))| std::cmp::Reverse(unread[k]));
        for (i, (name, key)) in shown {
            raised_frame(ui, 12, 8, |ui| {
              ui.set_width(ui.available_width());
              ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let face = avatar(ui, name, 32.0, None);
                count_badge(ui.painter(), face.rect, unread[key], PANEL);
                // Adicionado só pelo código: o nome chega quando o contato devolve a apresentação.
                let pending = *name == hex(key)[..8];
                ui.add_sized([ui.available_width() - 118.0 - if pending { 22.0 } else { 0.0 }, 32.0], egui::Label::new(RichText::new(name).strong().color(TEXT)).truncate())
                    .on_hover_text(format!("{}…", &hex(key)[..16]));
                if pending {
                    ui.label(RichText::new("❗").size(14.0).color(YELLOW)).on_hover_text("O nome do contato aparecerá assim que você e ele estiverem online ao mesmo tempo.");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    if trash_button(ui).on_hover_text("Remover contato").clicked() {
                        remove = Some(i);
                    }
                    let trusted = self.form.is_trusted(key);
                    let tip = if trusted {
                        "Contato de confiança (clique para tirar). A sua tela vai direto a ele, sem relay, e ele vê o seu IP."
                    } else {
                        "Marcar como contato de confiança. Só vale com a \"Privacidade na transmissão da tela\" desligada (aba Vídeo): a sua tela vai direto a este contato, sem relay no meio, e ele vê o seu IP. Os outros continuam recebendo por 2 relays. Marque só quem você conhece."
                    };
                    if shield_button(ui, trusted).on_hover_text(tip).clicked() {
                        // Marcar pede confirmação do aviso; desmarcar vale na hora.
                        if trusted {
                            self.set_trust(*key, false);
                        } else {
                            self.trust_ask = Some(*key);
                        }
                    }
                    // Os três ícones têm a mesma largura e ficam colados: o 📞 é texto, então sem o enchimento do botão.
                    let call = ui.scope(|ui| {
                        ui.spacing_mut().button_padding = vec2(0.0, 0.0);
                        ui.add_enabled(online && !in_call && online_keys.contains(key), egui::Button::new(RichText::new("📞").size(16.0)).frame(false).min_size(vec2(28.0, 34.0)))
                    }).inner;
                    let off_tip = if ringing == Some(*key) {
                        "Chamando…"
                    } else if online && !in_call {
                        if probed.contains_key(key) { "Contato offline" } else { "Verificando se o contato está online…" }
                    } else {
                        "Fique online e sem chamada para ligar"
                    };
                    if call.on_hover_text("Ligar").on_disabled_hover_text(off_tip).clicked() {
                        self.dial(*key);
                    }
                    let unread = unread[key];
                    let chat = ui.scope(|ui| {
                        ui.spacing_mut().button_padding = vec2(0.0, 0.0);
                        let ink = if unread > 0 { BLURPLE } else { TEXT };
                        ui.add(egui::Button::new(RichText::new("💬").size(16.0).color(ink)).frame(false).min_size(vec2(28.0, 34.0)))
                    }).inner;
                    let tip = if unread > 0 { format!("Bate-papo ({unread} nova(s))") } else { "Bate-papo".to_string() };
                    if chat.on_hover_text(tip).clicked() {
                        self.open_chat(*key);
                    }
                });
              });
            });
            ui.add_space(4.0);
        }
        // Remover é irreversível (o outro lado também te tira da lista dele): pede confirmação.
        if let Some(i) = remove {
            self.remove_ask = Some(contacts[i].1);
        }
    }

    /// Tira o contato da lista, avisa o outro lado e esquece o endereço.
    fn remove_contact_now(&mut self, key: [u8; 32]) {
        let Some(i) = parse_contacts(&self.form.contacts).iter().position(|(_, k)| *k == key) else { return };
        self.form.contacts = remove_contact(&self.form.contacts, i);
        // O endereço guardado do contato vai junto (a menos que a mesma chave ainda esteja na lista).
        if !parse_contacts(&self.form.contacts).iter().any(|(_, k)| *k == key) {
            // A confiança não volta sozinha se o contato for adicionado de novo.
            self.form.set_trusted(key, false);
            // O outro lado é avisado e também tira esta pessoa da lista dele. Offline, o aviso fica na fila
            // (`.clam-gui.removed`) e o endereço dele continua guardado até a entrega no próximo início.
            match &self.running {
                Some(r) => {
                    let _ = r.cmds.send(Cmd::Retire(key));
                }
                None => {
                    if let Err(e) = mark_pending(key, false).and_then(|()| mark_removal(key, true)) {
                        self.error = format!("não deu para guardar a remoção do contato em {}: {e}", removals_path().display());
                    }
                }
            }
        }
        self.form.save();
    }

    /// Marca ou desmarca o contato como de confiança, grava e avisa o nó. Quem marca já confirmou o aviso do IP.
    pub(crate) fn set_trust(&mut self, key: [u8; 32], on: bool) {
        self.form.set_trusted(key, on);
        self.form.save();
        if let Some(r) = &self.running {
            let _ = r.cmds.send(Cmd::Trust(key, on));
        }
    }

    /// Pergunta antes de remover o contato; só a confirmação remove.
    pub(crate) fn remove_window(&mut self, ctx: &egui::Context) {
        let Some(key) = self.remove_ask else { return };
        if !parse_contacts(&self.form.contacts).iter().any(|(_, k)| *k == key) {
            self.remove_ask = None;
            return;
        }
        let name = member_name(&self.form.names(), &key);
        let mut answer = None;
        egui::Window::new(format!("Remover {name}?"))
            .id(egui::Id::new("remover-contato"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(format!("{name} sai da sua lista de contatos e é avisado para tirar você da dele. Para voltar a falar, um dos dois precisa adicionar o outro de novo. O histórico do bate-papo fica guardado."));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if button(ui, true, "Remover", RED).clicked() {
                        answer = Some(true);
                    }
                    if button(ui, true, "Cancelar", GREY_BUTTON).clicked() {
                        answer = Some(false);
                    }
                });
            });
        if let Some(yes) = answer {
            self.remove_ask = None;
            if yes {
                self.remove_contact_now(key);
            }
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
                    if let Err(e) = relay.map_or(Ok(()), |r| remember_hint(key, r)).and_then(|()| mark_pending(key, true)).and_then(|()| mark_removal(key, false)) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_name_or_code_prefix_ignoring_case() {
        let key = [0xab; 32];
        assert!(matches_search("Ana Souza", &key, ""));
        assert!(matches_search("Ana Souza", &key, "souza"));
        assert!(matches_search("Ana", &key, "abab"));
        assert!(!matches_search("Ana", &key, "bruno"));
        assert!(!matches_search("Ana", &key, "baba"));
    }
}
