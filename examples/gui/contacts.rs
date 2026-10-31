//! Contatos: lista na coluna da esquerda, adicionar por código, confiança para a tela e apresentações recebidas.

use clam::sync::LockExt;

use eframe::egui::{self, RichText, vec2};

use crate::{conf::*, engine::*, theme::*, ui::*};

/// Estado de um contato na lista. O ponto verde só diz "online"; os outros três estados ficam no texto do avatar,
/// para a cor não carregar sozinha a informação.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Presence {
    Online,
    Offline,
    /// Ainda sem resposta da sondagem: não dá para dizer que está offline.
    Checking,
    /// Quem olha está offline: nada se sabe dos contatos.
    SelfOffline,
}

impl Presence {
    fn of(session_online: bool, up: bool, probed: bool) -> Self {
        match (session_online, up, probed) {
            (false, ..) => Self::SelfOffline,
            (true, true, _) => Self::Online,
            (true, false, true) => Self::Offline,
            (true, false, false) => Self::Checking,
        }
    }

    fn tip(self) -> &'static str {
        match self {
            Self::Online => "Online",
            Self::Offline => "Offline",
            Self::Checking => "Verificando se está online…",
            Self::SelfOffline => "Você está offline",
        }
    }
}

/// Ordem das linhas: quem tem mensagem nova sobe (a ordem entre os demais se mantém). Com o ponteiro sobre a
/// lista (`frozen` = a ordem do quadro anterior) nada muda de lugar, para o botão que a pessoa ia clicar não
/// fugir quando chega uma mensagem; chaves novas entram no fim.
fn display_order(keys: &[[u8; 32]], unread: impl Fn(&[u8; 32]) -> usize, frozen: Option<&[[u8; 32]]>) -> Vec<[u8; 32]> {
    let mut order = keys.to_vec();
    match frozen {
        Some(prev) => order.sort_by_key(|k| prev.iter().position(|p| p == k).unwrap_or(usize::MAX)),
        None => order.sort_by_key(|k| std::cmp::Reverse(unread(k))),
    }
    order
}

/// Se o contato aparece na busca: o texto (minúsculo, não vazio ou vazio) está no nome ou no começo do código.
fn matches_search(name: &str, key: &[u8; 32], needle: &str) -> bool {
    needle.is_empty() || name.to_lowercase().contains(needle) || hex(key).starts_with(needle)
}

impl App {
    /// Quem se apresentou (o nó já o atende): se é novo, entra na lista e recebe de volta a apresentação desta
    /// pessoa, para ele ter o nome dela; se já era contato sem nome (foi adicionado só pelo código), ganha o nome
    /// que ele deu a si mesmo. Quem já tinha nome não muda. A apresentação de volta sai uma vez por execução, no laço do nó
    /// (senão as duas pontas ficariam se apresentando para sempre).
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
    pub(crate) fn new_contact_form(&mut self, ui: &mut egui::Ui, in_menu: bool) {
        ui.set_width(if in_menu { 260.0 } else { ui.available_width() });
        note(ui, "Peça o código de contato de um amigo.");
        ui.add(field(&mut self.form.new_code, "Código de contato"));
        if button(ui, !self.form.new_code.trim().is_empty(), "Adicionar", BLURPLE).clicked() {
            self.add_contact();
            if in_menu && self.error.is_empty() {
                ui.close();
            }
        }
        if !self.error.is_empty() {
            ui.colored_label(pal().error, &self.error);
        }
    }

    /// Contatos online agora: a sondagem do nó (`Shared::presence`) ou um servidor em comum que os mostra online.
    /// Quem ainda não foi sondado não está na lista, e o botão de ligar espera o resultado.
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

    /// Lista vazia: o caminho inteiro na própria coluna. Os dois lados precisam do código um do outro, então o
    /// formulário de adicionar já vem aberto e um atalho leva ao código de quem está aqui.
    fn empty_contacts(&mut self, ui: &mut egui::Ui) {
        sunken_frame(ui, 12, 12, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Ninguém por aqui ainda").strong().color(pal().strong));
            note(ui, "Para falar com alguém, os dois trocam o código de contato.");
            ui.add_space(4.0);
            self.new_contact_form(ui, false);
            ui.add_space(4.0);
            if button(ui, true, "Ver o meu código", pal().bg).on_hover_text("Fica em Configurações, em Meu código de contato").clicked() {
                self.tab = Tab::Settings;
            }
        });
    }

    /// Lista de contatos na coluna da esquerda (o "início", como as mensagens diretas do Discord): um por linha,
    /// com ligar e bate-papo à direita e, num menu de três pontos, a confiança para a tela e remover. Quem é de confiança
    /// mostra o escudo cheio na própria linha (expõe o IP: não pode ficar escondido). No alto, a busca.
    pub(crate) fn contacts_list(&mut self, ui: &mut egui::Ui, online: bool, in_call: bool) {
        let contacts = parse_contacts(&self.form.contacts);
        // O texto da busca vive na memória temporária da janela: não vai para o disco nem precisa de campo em `App`.
        let search_id = egui::Id::new("busca-contatos");
        let mut query = ui.data_mut(|d| d.get_temp::<String>(search_id)).unwrap_or_default();
        // Sem contatos a busca não tem o que achar, e o formulário de adicionar é o que importa.
        if !contacts.is_empty() {
            ui.add(field(&mut query, "Buscar contato"));
            ui.data_mut(|d| d.insert_temp(search_id, query.clone()));
        }
        let needle = query.trim().to_lowercase();
        if contacts.is_empty() {
            self.empty_contacts(ui);
        } else if !needle.is_empty() && !contacts.iter().any(|(n, k)| matches_search(n, k, &needle)) {
            note(ui, "Ninguém com esse nome.");
        }
        let online_keys = self.online_contacts(&contacts);
        let probed = self.shared.presence.lock_unpoisoned().clone();
        let ringing = *self.shared.ringing.lock_unpoisoned();
        let mut remove = None;
        let unread: std::collections::HashMap<[u8; 32], usize> = {
            let book = self.shared.chats.lock_unpoisoned();
            contacts.iter().map(|(_, k)| (*k, book.unread(k))).collect()
        };
        let order_id = egui::Id::new("ordem-contatos");
        let hover_id = egui::Id::new("ponteiro-na-lista");
        let frozen = ui.data(|d| d.get_temp::<bool>(hover_id)).unwrap_or(false).then(|| ui.data(|d| d.get_temp::<Vec<[u8; 32]>>(order_id))).flatten();
        let mut shown: Vec<_> = contacts.iter().enumerate().filter(|(_, (n, k))| matches_search(n, k, &needle)).collect();
        let keys: Vec<[u8; 32]> = shown.iter().map(|(_, (_, k))| *k).collect();
        let order = display_order(&keys, |k| unread[k], frozen.as_deref());
        shown.sort_by_key(|(_, (_, k))| order.iter().position(|o| o == k));
        let list_top = ui.cursor().min;
        for (i, (name, key)) in shown {
            raised_frame(ui, 12, 8, |ui| {
              ui.set_width(ui.available_width());
              ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let up = online_keys.contains(key);
                let status = Presence::of(online, up, probed.contains_key(key));
                // Offline conhecido apaga o avatar e o nome: o ponto verde sozinho some no meio de uma lista longa.
                let dim = status == Presence::Offline;
                let face = ui
                    .scope(|ui| {
                        if dim {
                            ui.set_opacity(0.5);
                        }
                        avatar(ui, name, 32.0, up.then_some(pal().green))
                    })
                    .inner
                    .on_hover_text(status.tip());
                count_badge(ui.painter(), face.rect, unread[key], pal().bg);
                // Adicionado só pelo código: o nome chega quando o contato devolve a apresentação.
                let pending = *name == hex(key)[..8];
                let trusted = self.form.is_trusted(key);
                let marks = 22.0 * (usize::from(pending) + usize::from(trusted)) as f32;
                let ink = if dim { pal().muted } else { pal().text };
                let label = ui.add_sized([ui.available_width() - 110.0 - marks, 32.0], egui::Label::new(RichText::new(name).strong().color(ink)).truncate().sense(egui::Sense::click()));
                let label = label.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{}…\nAbrir o bate-papo", &hex(key)[..16]));
                if label.clicked() || face.clicked() {
                    self.open_chat(*key);
                }
                // O escudo cheio fica à vista: quem vê o seu IP não pode ser um detalhe escondido no menu. É só uma
                // marca (sem clique), mas alcançável por teclado e com nome para leitor de tela.
                if trusted {
                    let (mark, _) = ui.allocate_exact_size(vec2(18.0, 18.0), egui::Sense::hover());
                    let reply = ui.interact(mark, egui::Id::new(("confianca", key)), egui::Sense::focusable_noninteractive());
                    reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "Contato de confiança: vê o seu IP"));
                    shield_icon(ui.painter(), mark.center(), 16.0, pal().muted, true);
                    focus_ring(ui, &reply, 6);
                    reply.on_hover_text("Contato de confiança: a sua tela vai direto a ele, sem relay, e ele vê o seu IP. Tire no menu de três pontos.");
                }
                if pending {
                    let (mark, _) = ui.allocate_exact_size(vec2(18.0, 18.0), egui::Sense::hover());
                    alert_icon(ui.painter(), mark.center(), 16.0, pal().yellow);
                    ui.interact(mark, egui::Id::new(("pendente", key)), egui::Sense::hover()).on_hover_text("Contato pendente: ainda sem nome. Não existe servidor: cada pessoa fala direto com a outra, cifrado. Quando o contato se apresenta, o nome e o endereço dele chegam sozinhos e esta marca some. A apresentação só é entregue com ele ligado, e o programa insiste até conseguir.");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let dots = dots_button(ui).on_hover_text("Mais ações");
                    egui::Popup::menu(&dots).show(|ui| {
                        let tip = if trusted {
                            "A sua tela vai direto a ele, sem relay, e ele vê o seu IP."
                        } else {
                            "Só vale com a \"Privacidade na transmissão da tela\" desligada (aba Vídeo): a sua tela vai direto a este contato, sem relay no meio, e ele vê o seu IP. Os outros continuam recebendo por 2 relays."
                        };
                        let label = if trusted { "Tirar a confiança" } else { "Marcar como de confiança" };
                        if menu_item(ui, false, None, label, |p, c, s, k| shield_icon(p, c, s, k, false)).on_hover_text(tip).clicked() {
                            // Marcar pede confirmação do aviso; desmarcar vale na hora.
                            if trusted {
                                self.set_trust(*key, false);
                            } else {
                                self.trust_ask = Some(*key);
                            }
                            ui.close();
                        }
                        if menu_item(ui, false, Some(pal().error), "Remover contato", trash_icon).on_hover_text("Pede confirmação antes de remover").clicked() {
                            remove = Some(i);
                            ui.close();
                        }
                    });
                    let unread = unread[key];
                    let chat = glyph_button(ui, "Bate-papo", (unread > 0).then_some(pal().focus), chat_icon);
                    let tip = if unread > 0 { format!("Bate-papo ({unread} nova(s))") } else { "Bate-papo".to_string() };
                    if chat.on_hover_text(tip).clicked() {
                        self.open_chat(*key);
                    }
                    let call = ui.add_enabled_ui(online && !in_call && up, |ui| glyph_button(ui, "Ligar", None, phone_icon)).inner;
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
                });
              });
            });
            ui.add_space(4.0);
        }
        let list = egui::Rect::from_min_max(list_top, egui::pos2(list_top.x + ui.available_width(), ui.cursor().min.y));
        let over = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| list.contains(p));
        ui.data_mut(|d| {
            d.insert_temp(hover_id, over);
            d.insert_temp(order_id, order);
        });
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

    /// Bloqueia ou desbloqueia `key`, grava e avisa o nó.
    pub(crate) fn set_block(&mut self, key: [u8; 32], on: bool) {
        self.form.set_blocked(key, on);
        self.form.save();
        if let Some(r) = &self.running {
            let _ = r.cmds.send(Cmd::Block(key, on));
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
                ui.label(format!("{name} sai da sua lista e é avisado para tirar você da dele. O histórico do bate-papo fica guardado."));
                ui.add_space(4.0);
                let block_id = egui::Id::new("remover-e-bloquear");
                let mut block = ui.data(|d| d.get_temp::<bool>(block_id)).unwrap_or(false);
                ui.check(&mut block, "Bloquear também").on_hover_text(format!("Impede {name} de voltar sozinho à sua lista (por apresentação ou convite) até você adicioná-lo de novo ou desbloqueá-lo nas configurações."));
                ui.data_mut(|d| d.insert_temp(block_id, block));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if button(ui, true, "Remover", pal().red).clicked() {
                        answer = Some(if block { Remove::AndBlock } else { Remove::Only });
                    }
                    if button(ui, true, "Cancelar", pal().bg).clicked() {
                        answer = Some(Remove::Cancel);
                    }
                });
            });
        if let Some(choice) = answer {
            self.remove_ask = None;
            ctx.data_mut(|d| d.remove::<bool>(egui::Id::new("remover-e-bloquear")));
            if choice != Remove::Cancel {
                self.remove_contact_now(key);
            }
            if choice == Remove::AndBlock {
                self.set_block(key, true);
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
                    if button(ui, true, "Cancelar", pal().bg).clicked() {
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
        // Adicionar de propósito desbloqueia.
        if let Some(key) = parse_contacts(&self.form.new_code).first().map(|(_, k)| *k).filter(|k| self.form.blocked_keys().contains(k)) {
            self.set_block(key, false);
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

/// A resposta do diálogo de remoção.
#[derive(Clone, Copy, PartialEq)]
enum Remove {
    Only,
    AndBlock,
    Cancel,
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

    #[test]
    fn presence_separates_offline_from_not_yet_checked() {
        assert_eq!(Presence::of(true, true, true), Presence::Online);
        assert_eq!(Presence::of(true, false, true), Presence::Offline);
        assert_eq!(Presence::of(true, false, false), Presence::Checking);
        // Um servidor em comum que o mostra online vale mesmo sem sondagem.
        assert_eq!(Presence::of(true, true, false), Presence::Online);
        // Sem estar online, nenhum contato pode ser dado como online nem como offline.
        assert_eq!(Presence::of(false, true, true), Presence::SelfOffline);
        assert_eq!(Presence::of(false, false, false).tip(), "Você está offline");
    }

    #[test]
    fn unread_rises_unless_the_order_is_frozen() {
        let (a, b, c) = ([1; 32], [2; 32], [3; 32]);
        let unread = |k: &[u8; 32]| usize::from(*k == c) * 2;
        assert_eq!(display_order(&[a, b, c], unread, None), vec![c, a, b]);
        // Com o ponteiro na lista, a ordem anterior vale e a chave nova vai para o fim.
        assert_eq!(display_order(&[a, b, c], unread, Some(&[a, b])), vec![a, b, c]);
        assert_eq!(display_order(&[b, a], unread, Some(&[a, b])), vec![a, b]);
    }
}
