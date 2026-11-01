//! Histórico do bate-papo 1:1: o que a pessoa trocou com cada contato, guardado nesta máquina (`~/.clam-gui.chats`).
//! Cada lado guarda a própria cópia; não há servidor nem caixa de correio (`docs/CHAMADAS.md`, "Bate-papo 1:1").

use std::collections::HashMap;
use std::path::PathBuf;

use crate::conf::{conf_path, hex, unhex, write_private};

/// Mensagens guardadas por contato; as mais antigas saem quando passa disso.
const KEEP: usize = 500;

/// Como a mensagem está.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// Minha, ainda a caminho (some ao reiniciar: vira `Failed`).
    Sending,
    /// Minha, e o contato confirmou que abriu.
    Sent,
    /// Minha, e o contato não confirmou (offline ou sem rota).
    Failed,
    /// Do contato.
    Received,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Msg {
    /// Carimbo do remetente (ms Unix): ordena a conversa e é o que o ack repete.
    pub(crate) id: u64,
    pub(crate) text: String,
    pub(crate) state: State,
}

impl Msg {
    pub(crate) fn mine(&self) -> bool {
        self.state != State::Received
    }
}

/// Junta as mensagens seguidas que são pedaços de um mesmo texto longo: mesmo autor e `id` consecutivo. `split_text`
/// parte o texto em quadros de 128 bytes e `Dialer::next_chat_id` dá `id + 1` a cada pedaço da rajada, enquanto duas
/// mensagens digitadas à mão ficam a segundos (milhares de ms) uma da outra. Devolve os intervalos `[início, fim)`.
pub(crate) fn groups(msgs: &[Msg]) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        let joins = i > 0 && msgs.get(i - 1).is_some_and(|p| p.mine() == m.mine() && p.id.checked_add(1) == Some(m.id));
        match out.last_mut() {
            Some(g) if joins => g.end = i + 1,
            _ => out.push(i..i + 1),
        }
    }
    out
}

/// Todas as conversas, mais o que ainda não foi lido e se há algo por gravar.
#[derive(Default)]
pub(crate) struct ChatBook {
    chats: HashMap<[u8; 32], Vec<Msg>>,
    unread: HashMap<[u8; 32], usize>,
    dirty: bool,
}

impl ChatBook {
    pub(crate) fn with(&self, peer: &[u8; 32]) -> &[Msg] {
        self.chats.get(peer).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn unread(&self, peer: &[u8; 32]) -> usize {
        self.unread.get(peer).copied().unwrap_or(0)
    }

    /// Não lidas de todas as conversas.
    pub(crate) fn unread_total(&self) -> usize {
        self.unread.values().sum()
    }

    pub(crate) fn read(&mut self, peer: &[u8; 32]) {
        self.unread.remove(peer);
    }

    /// Apaga a conversa inteira com `peer` (mensagens e não lidas): contato removido não deixa histórico.
    pub(crate) fn forget(&mut self, peer: &[u8; 32]) {
        self.unread.remove(peer);
        if self.chats.remove(peer).is_some() {
            self.dirty = true;
        }
    }

    /// Mensagem minha, a caminho.
    pub(crate) fn push_mine(&mut self, peer: [u8; 32], id: u64, text: String) {
        self.push(peer, Msg { id, text, state: State::Sending });
    }

    /// Mensagem do contato. Repetida (mesmo `id`) é ignorada.
    pub(crate) fn push_received(&mut self, peer: [u8; 32], id: u64, text: String) {
        if self.chats.get(&peer).is_some_and(|v| v.iter().any(|m| m.id == id && !m.mine())) {
            return;
        }
        *self.unread.entry(peer).or_insert(0) += 1;
        self.push(peer, Msg { id, text, state: State::Received });
    }

    pub(crate) fn set_state(&mut self, peer: &[u8; 32], id: u64, state: State) {
        if let Some(m) = self.chats.get_mut(peer).and_then(|v| v.iter_mut().find(|m| m.id == id && m.mine())) {
            m.state = state;
            self.dirty = true;
        }
    }

    fn push(&mut self, peer: [u8; 32], msg: Msg) {
        let v = self.chats.entry(peer).or_default();
        v.push(msg);
        // A ordem é a de chegada; relógios diferentes não reordenam o que já está na tela.
        if v.len() > KEEP {
            v.drain(..v.len() - KEEP);
        }
        self.dirty = true;
    }

    /// Texto para gravar, se algo mudou desde a última vez; zera a marca.
    pub(crate) fn take_dirty(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.dirty) {
            return None;
        }
        Some(self.render())
    }

    /// Uma linha por mensagem: `chave id estado texto` (`m` enviada, `f` falhou, `r` recebida). O texto não tem
    /// caractere de controle (o quadro o recusa), então a quebra de linha separa as mensagens sem escape.
    fn render(&self) -> String {
        let mut keys: Vec<_> = self.chats.keys().collect();
        keys.sort();
        let mut out = String::new();
        for k in keys {
            for m in &self.chats[k] {
                let s = match m.state {
                    State::Sent => 'm',
                    State::Sending | State::Failed => 'f',
                    State::Received => 'r',
                };
                out.push_str(&format!("{} {} {s} {}\n", hex(k), m.id, m.text));
            }
        }
        out
    }

    pub(crate) fn parse(text: &str) -> Self {
        let mut book = Self::default();
        for l in text.lines() {
            let mut p = l.splitn(4, ' ');
            let (Some(k), Some(id), Some(s), Some(t)) = (p.next(), p.next(), p.next(), p.next()) else { continue };
            let (Ok(k), Ok(id)) = (unhex(k), id.parse()) else { continue };
            let state = match s {
                "m" => State::Sent,
                "f" => State::Failed,
                "r" => State::Received,
                _ => continue,
            };
            book.chats.entry(k).or_default().push(Msg { id, text: t.to_string(), state });
        }
        book
    }

    pub(crate) fn load() -> Self {
        Self::parse(&std::fs::read_to_string(chats_path()).unwrap_or_default())
    }
}

pub(crate) fn chats_path() -> PathBuf {
    conf_path().with_file_name(".clam-gui.chats")
}

pub(crate) fn save_chats(text: &str) -> std::io::Result<()> {
    write_private(&chats_path(), text)
}

/// Parte `text` em pedaços de até `max` bytes sem cortar caractere, preferindo quebrar num espaço, que fica no fim
/// do pedaço: assim `concat` dos pedaços devolve o texto (é como a tela os junta de volta, ver `joined`). Controle
/// vira espaço; o que sobra de espaço nas pontas do texto sai. Vazio, nada.
pub(crate) fn split_text(text: &str, max: usize) -> Vec<String> {
    let clean: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let mut rest = clean.trim();
    let mut out = Vec::new();
    while !rest.is_empty() {
        if rest.len() <= max {
            out.push(rest.to_string());
            break;
        }
        let mut cut = max;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        // Quebra no último espaço se ele não deixar o pedaço minúsculo.
        if let Some(sp) = rest.get(..cut).and_then(|h| h.rfind(' ')).filter(|&i| i >= max / 2) {
            cut = sp + 1;
        }
        let (head, tail) = rest.split_at(cut);
        out.push(head.to_string());
        rest = tail;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forgetting_a_contact_drops_only_that_conversation_and_marks_it_for_saving() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let mut book = ChatBook::default();
        book.push_mine(a, 1, "oi".into());
        book.push_received(a, 2, "olá".into());
        book.push_received(b, 3, "e aí".into());
        book.take_dirty();
        book.forget(&a);
        assert!(book.with(&a).is_empty());
        assert_eq!(book.unread(&a), 0);
        assert_eq!(book.with(&b).len(), 1, "a conversa dos outros fica");
        let saved = book.take_dirty().expect("a remoção precisa chegar ao arquivo");
        assert!(!saved.contains(&hex(&a)) && saved.contains(&hex(&b)));
        book.forget(&a);
        assert!(book.take_dirty().is_none(), "sem conversa, nada a gravar");
    }

    #[test]
    fn splits_without_cutting_characters() {
        let text = "ção ".repeat(100);
        let parts = split_text(&text, 128);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.len() <= 128 && !p.is_empty()));
        assert_eq!(parts.concat(), text.trim());
    }

    #[test]
    fn pieces_of_one_text_group_together_and_separate_messages_do_not() {
        let m = |id, state| Msg { id, text: "x".into(), state };
        let msgs = [m(10, State::Sent), m(11, State::Sent), m(12, State::Sent), m(5000, State::Sent), m(5001, State::Received), m(5002, State::Received), m(9000, State::Sent)];
        assert_eq!(groups(&msgs), vec![0..3, 3..4, 4..6, 6..7]);
        assert!(groups(&[]).is_empty());
    }

    #[test]
    fn splits_a_long_word_and_drops_blank_text() {
        let parts = split_text(&"é".repeat(200), 128);
        assert_eq!(parts.concat(), "é".repeat(200));
        assert!(parts.iter().all(|p| p.len() <= 128));
        assert!(split_text(" \n\t ", 128).is_empty());
        assert_eq!(split_text("a\nb", 128), vec!["a b"]);
    }

    #[test]
    fn keeps_history_through_save_and_load() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let mut book = ChatBook::default();
        book.push_mine(a, 10, "oi tudo bem?".into());
        book.set_state(&a, 10, State::Sent);
        book.push_mine(a, 11, "sem rede".into());
        book.push_received(a, 12, "tudo!".into());
        book.push_received(b, 5, "outra pessoa".into());
        let text = book.take_dirty().unwrap();
        assert!(book.take_dirty().is_none());
        let back = ChatBook::parse(&text);
        let states: Vec<_> = back.with(&a).iter().map(|m| (m.id, m.state)).collect();
        // A que estava a caminho vira falha: nada a retoma depois do reinício.
        assert_eq!(states, vec![(10, State::Sent), (11, State::Failed), (12, State::Received)]);
        assert_eq!(back.with(&a)[0].text, "oi tudo bem?");
        assert_eq!(back.with(&b).len(), 1);
    }

    #[test]
    fn ignores_repeats_counts_unread_and_trims_history() {
        let a = [1u8; 32];
        let mut book = ChatBook::default();
        book.push_received(a, 1, "x".into());
        book.push_received(a, 1, "x".into());
        assert_eq!((book.with(&a).len(), book.unread(&a)), (1, 1));
        assert_eq!(book.unread_total(), 1);
        book.read(&a);
        assert_eq!(book.unread(&a), 0);
        for i in 2..2 * KEEP as u64 {
            book.push_mine(a, i, "m".into());
        }
        assert_eq!(book.with(&a).len(), KEEP);
    }

    #[test]
    fn parse_skips_garbage() {
        let book = ChatBook::parse("lixo\n00 1 m x\nzz 1 m x\n");
        assert!(book.chats.is_empty());
    }
}
