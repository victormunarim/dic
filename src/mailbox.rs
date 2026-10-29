//! Custódia da caixa de correio: o que um contato de confiança guarda para entregar uma apresentação a quem
//! estava offline (veja `docs/ENCONTRO.md`). É *sans-IO*: quem chama entrega os pedaços recebidos e o relógio.
//!
//! O guardião só tem bytes que não abre: o quadro da apresentação é fechado para o destinatário, e a chave da caixa
//! só leva ao lugar onde o quadro fica na DHT, não à chave do destinatário. Por isso a custódia tem cotas: um contato
//! de confiança malicioso não pode encher a memória de quem guarda.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use crate::media::{
    signal::Frame,
    update::{CUSTODY_DATA, CUSTODY_PARTS, CustodyPart, MAIL_TTL_MS, TAG_INTRO, join_custody},
};

/// Quantas apresentações cada contato pode deixar guardadas.
pub const PER_DEPOSITOR: usize = 8;
/// Quantas apresentações no total.
pub const MAX_HELD: usize = 64;
/// Depósitos pela metade (falta um pedaço) ao mesmo tempo.
pub const MAX_PARTIAL: usize = 16;

/// Uma apresentação guardada: de quem veio, o carimbo dela (`id`), a chave da caixa e o quadro.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub from: [u8; 32],
    pub id: u64,
    pub master: [u8; 32],
    pub mail: Frame,
}

#[derive(Clone, Debug)]
struct Partial {
    from: [u8; 32],
    id: u64,
    parts: [Option<[u8; CUSTODY_DATA]>; CUSTODY_PARTS],
}

/// As apresentações que este nó guarda por conta de contatos.
#[derive(Debug, Default)]
pub struct Custody {
    held: Vec<Held>,
    partial: Vec<Partial>,
}

/// O carimbo `id` ainda vale em `now_ms`? Nem velho demais (passou o prazo), nem no futuro além de uma hora.
fn fresh(id: u64, now_ms: u64) -> bool {
    id <= now_ms.saturating_add(3_600_000) && now_ms.saturating_sub(id) <= MAIL_TTL_MS
}

impl Custody {
    pub fn new() -> Self {
        Self::default()
    }

    /// Recebe um pedaço de `from`. `true`: o pedaço ficou (ou já estava) e o depositante deve receber o ack;
    /// `false`: recusado (fora do prazo, cota cheia ou conteúdo que não é uma apresentação).
    pub fn accept(&mut self, from: [u8; 32], part: &CustodyPart, now_ms: u64) -> bool {
        self.expire(now_ms);
        if !fresh(part.id, now_ms) {
            return false;
        }
        if self.held.iter().any(|h| h.from == from && h.id == part.id) {
            return true;
        }
        let idx = usize::from(part.idx);
        let at = match self.partial.iter().position(|p| p.from == from && p.id == part.id) {
            Some(at) => at,
            None => {
                if self.partial.len() >= MAX_PARTIAL || self.full(&from) {
                    return false;
                }
                self.partial.push(Partial { from, id: part.id, parts: [None; CUSTODY_PARTS] });
                self.partial.len() - 1
            }
        };
        let Some(slot) = self.partial.get_mut(at).and_then(|p| p.parts.get_mut(idx)) else {
            return false;
        };
        *slot = Some(part.data);
        let Some(done) = self.partial.get(at).and_then(|p| p.parts.iter().copied().collect::<Option<Vec<_>>>()) else {
            return true;
        };
        self.partial.swap_remove(at);
        let Ok(parts) = <[[u8; CUSTODY_DATA]; CUSTODY_PARTS]>::try_from(done) else {
            return false;
        };
        match join_custody(&parts) {
            Some((master, mail)) if mail.first() == Some(&TAG_INTRO) => {
                self.held.push(Held { from, id: part.id, master, mail });
                true
            }
            _ => false,
        }
    }

    fn full(&self, from: &[u8; 32]) -> bool {
        let mine = |f: &[u8; 32]| f == from;
        self.held.len() + self.partial.len() >= MAX_HELD
            || self.held.iter().filter(|h| mine(&h.from)).count() + self.partial.iter().filter(|p| mine(&p.from)).count() >= PER_DEPOSITOR
    }

    /// Descarta o que passou do prazo.
    pub fn expire(&mut self, now_ms: u64) {
        self.held.retain(|h| fresh(h.id, now_ms));
        self.partial.retain(|p| fresh(p.id, now_ms));
    }

    /// Esquece o que `from` deixou (quando deixa de ser contato).
    pub fn drop_from(&mut self, from: &[u8; 32]) {
        self.held.retain(|h| &h.from != from);
        self.partial.retain(|p| &p.from != from);
    }

    /// O que está guardado e ainda vale.
    pub fn held(&self, now_ms: u64) -> Vec<Held> {
        self.held.iter().filter(|h| fresh(h.id, now_ms)).cloned().collect()
    }

    /// Texto para salvar em disco, uma apresentação por linha: `depositante id chave quadro` (hexadecimal).
    pub fn export(&self) -> Vec<String> {
        self.held.iter().map(|h| format!("{} {:x} {} {}", hex(&h.from), h.id, hex(&h.master), hex(&h.mail))).collect()
    }

    /// Recarrega o que [`export`](Self::export) escreveu; ignora linha malformada, vencida ou além das cotas.
    pub fn import<'a>(lines: impl IntoIterator<Item = &'a str>, now_ms: u64) -> Self {
        let mut out = Self::new();
        for line in lines {
            let mut f = line.split_whitespace();
            let (Some(from), Some(id), Some(master), Some(mail)) = (f.next(), f.next(), f.next(), f.next()) else { continue };
            let (Some(from), Ok(id), Some(master), Some(mail)) = (unhex(from), u64::from_str_radix(id, 16), unhex(master), unhex(mail)) else { continue };
            let (Ok(from), Ok(master), Ok(mail)) = (<[u8; 32]>::try_from(from), <[u8; 32]>::try_from(master), Frame::try_from(mail)) else { continue };
            if fresh(id, now_ms) && mail.first() == Some(&TAG_INTRO) && !out.full(&from) {
                out.held.push(Held { from, id, master, mail });
            }
        }
        out
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()).collect()
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::media::update::{Intro, Reach, mailbox_master, open_custody, seal_custody, seal_intro};
    use rand_core::OsRng;
    use x25519_dalek::{PublicKey, StaticSecret};

    const NOW: u64 = 1_800_000_000_000;

    fn parts(id: u64) -> ([u8; 32], Vec<CustodyPart>) {
        let (a, keeper, b) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
        let pb = PublicKey::from(&b);
        let intro = Intro { seq: id, name: "Ana".into(), reach: Reach { addr: "203.0.113.7:4001".into(), onion: [3; 32], link: [4; 32] } };
        let mail = seal_intro(&a, &pb, &intro).unwrap();
        let frames = seal_custody(&a, &PublicKey::from(&keeper), id, &mailbox_master(pb.as_bytes()), &mail).unwrap();
        let got: Vec<_> = frames.iter().map(|f| open_custody(&keeper, f).unwrap()).collect();
        (got[0].0, got.into_iter().map(|g| g.1).collect())
    }

    #[test]
    fn joins_both_parts_in_any_order_and_acks_repeats() {
        let (from, p) = parts(NOW);
        let mut c = Custody::new();
        assert!(c.accept(from, &p[1], NOW));
        assert!(c.held(NOW).is_empty(), "falta um pedaço");
        assert!(c.accept(from, &p[0], NOW));
        assert_eq!(c.held(NOW).len(), 1);
        assert!(c.accept(from, &p[0], NOW), "repetido: ack de novo, sem duplicar");
        assert_eq!(c.held(NOW).len(), 1);
    }

    #[test]
    fn refuses_what_is_old_or_from_the_future() {
        let mut c = Custody::new();
        let (from, old) = parts(NOW - MAIL_TTL_MS - 1);
        assert!(!c.accept(from, &old[0], NOW));
        let (from, future) = parts(NOW + 2 * 3_600_000);
        assert!(!c.accept(from, &future[0], NOW));
    }

    #[test]
    fn expires_after_a_week() {
        let (from, p) = parts(NOW);
        let mut c = Custody::new();
        p.iter().for_each(|x| assert!(c.accept(from, x, NOW)));
        assert_eq!(c.held(NOW + MAIL_TTL_MS).len(), 1);
        c.expire(NOW + MAIL_TTL_MS + 1);
        assert!(c.held(NOW).is_empty());
    }

    #[test]
    fn quotas_per_depositor_and_in_total() {
        let mut c = Custody::new();
        let from = [1u8; 32];
        for i in 0..PER_DEPOSITOR as u64 {
            let (_, p) = parts(NOW + i);
            assert!(c.accept(from, &p[0], NOW));
        }
        let (_, p) = parts(NOW + 100);
        assert!(!c.accept(from, &p[0], NOW), "o mesmo contato passou da cota");
        assert!(c.accept([2; 32], &p[0], NOW), "outro contato ainda cabe");
        c.drop_from(&from);
        assert!(c.accept(from, &p[0], NOW));
    }

    #[test]
    fn refuses_a_deposit_that_is_not_an_introduction() {
        let (from, mut p) = parts(NOW);
        p[0].data[32] = 0; // o primeiro byte do quadro deixa de ser TAG_INTRO
        let mut c = Custody::new();
        assert!(c.accept(from, &p[0], NOW));
        assert!(!c.accept(from, &p[1], NOW));
        assert!(c.held(NOW).is_empty());
    }

    #[test]
    fn export_and_import_round_trip_and_skip_garbage() {
        let (from, p) = parts(NOW);
        let mut c = Custody::new();
        p.iter().for_each(|x| assert!(c.accept(from, x, NOW)));
        let mut lines = c.export();
        lines.extend(["lixo".into(), "zz 1 2 3".into()]);
        let back = Custody::import(lines.iter().map(String::as_str), NOW);
        assert_eq!(back.held(NOW), c.held(NOW));
        assert!(Custody::import(lines.iter().map(String::as_str), NOW + MAIL_TTL_MS + 1).held(NOW).is_empty());
    }
}
