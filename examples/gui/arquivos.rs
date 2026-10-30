//! Transferências de arquivo do bate-papo 1:1 como a janela as vê: o que foi oferecido, o que espera resposta da
//! pessoa, o que anda e como terminou. Só estado de exibição; os bytes e o disco ficam no laço do nó
//! (`engine/node.rs`, via `clam::transfer`).

use std::path::PathBuf;

/// Quantas transferências já encerradas se guardam para a tela.
const KEEP_DONE: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Minha: oferta feita, esperando o contato aceitar.
    Offering,
    /// Do contato: espera a minha resposta.
    Asking,
    /// Os pedaços estão indo ou vindo.
    Active,
    /// Terminou; em quem recebeu, onde o arquivo ficou.
    Done(Option<PathBuf>),
    Failed(String),
    Declined,
    Cancelled,
}

impl Phase {
    pub(crate) fn live(&self) -> bool {
        matches!(self, Self::Offering | Self::Asking | Self::Active)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Xfer {
    pub(crate) id: u64,
    pub(crate) peer: [u8; 32],
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) mine: bool,
    pub(crate) phase: Phase,
    /// Bytes já confirmados (enviados) ou gravados (recebidos).
    pub(crate) bytes: u64,
    /// Resposta do contato à minha oferta, ainda não vista pela tarefa de envio.
    answer: Option<bool>,
    cancel: bool,
}

#[derive(Default)]
pub(crate) struct Transfers {
    list: Vec<Xfer>,
}

impl Transfers {
    fn find(&mut self, peer: &[u8; 32], id: u64, mine: bool) -> Option<&mut Xfer> {
        self.list.iter_mut().find(|x| x.id == id && x.peer == *peer && x.mine == mine)
    }

    /// Oferta minha, esperando resposta.
    pub(crate) fn add_out(&mut self, peer: [u8; 32], id: u64, name: String, size: u64) {
        self.push(Xfer { id, peer, name, size, mine: true, phase: Phase::Offering, bytes: 0, answer: None, cancel: false });
    }

    /// Oferta do contato. `false` se é repetida (a oferta é reenviada quando o ack se perde).
    pub(crate) fn add_in(&mut self, peer: [u8; 32], id: u64, name: String, size: u64) -> bool {
        if self.find(&peer, id, false).is_some() {
            return false;
        }
        self.push(Xfer { id, peer, name, size, mine: false, phase: Phase::Asking, bytes: 0, answer: None, cancel: false });
        true
    }

    fn push(&mut self, x: Xfer) {
        self.list.push(x);
        let done = self.list.iter().filter(|x| !x.phase.live()).count();
        if done > KEEP_DONE {
            let mut drop = done - KEEP_DONE;
            self.list.retain(|x| {
                let old = drop > 0 && !x.phase.live();
                drop -= usize::from(old);
                !old
            });
        }
    }

    pub(crate) fn of(&self, peer: &[u8; 32]) -> Vec<Xfer> {
        self.list.iter().filter(|x| x.peer == *peer).cloned().collect()
    }

    /// Transferências à espera de resposta da pessoa, de qualquer contato.
    pub(crate) fn asking(&self, peer: &[u8; 32]) -> usize {
        self.list.iter().filter(|x| x.peer == *peer && x.phase == Phase::Asking).count()
    }

    pub(crate) fn phase(&mut self, peer: &[u8; 32], id: u64, mine: bool, phase: Phase) {
        if let Some(x) = self.find(peer, id, mine) {
            // Um fim não se desfaz: um quadro atrasado não ressuscita o que acabou.
            if x.phase.live() {
                x.phase = phase;
            }
        }
    }

    pub(crate) fn progress(&mut self, peer: &[u8; 32], id: u64, mine: bool, bytes: u64) {
        if let Some(x) = self.find(peer, id, mine) {
            x.bytes = bytes.min(x.size);
        }
    }

    /// Resposta do contato à minha oferta.
    pub(crate) fn answered(&mut self, peer: &[u8; 32], id: u64, accept: bool) {
        if let Some(x) = self.find(peer, id, true) {
            x.answer = Some(accept);
        }
    }

    pub(crate) fn take_answer(&mut self, peer: &[u8; 32], id: u64) -> Option<bool> {
        self.find(peer, id, true).and_then(|x| x.answer.take())
    }

    /// Pede para parar (a pessoa, ou o contato que cancelou): a tarefa de envio olha `cancel_requested`.
    pub(crate) fn request_cancel(&mut self, peer: &[u8; 32], id: u64, mine: bool) {
        if let Some(x) = self.find(peer, id, mine) {
            x.cancel = true;
        }
    }

    pub(crate) fn cancel_requested(&mut self, peer: &[u8; 32], id: u64, mine: bool) -> bool {
        self.find(peer, id, mine).is_none_or(|x| x.cancel)
    }

    /// Nome e tamanho de uma oferta do contato que espera resposta.
    pub(crate) fn asked(&mut self, peer: &[u8; 32], id: u64) -> Option<(String, u64)> {
        self.find(peer, id, false).filter(|x| x.phase == Phase::Asking).map(|x| (x.name.clone(), x.size))
    }
}

/// Tamanho para a tela: `1,5 MB`.
pub(crate) fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let (mut v, mut u) = (bytes as f64, 0);
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    let unit = UNITS.get(u).copied().unwrap_or("GB");
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {unit}").replace('.', ",") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_flows_and_ends_stay_ended() {
        let (p, mut t) = ([1u8; 32], Transfers::default());
        t.add_out(p, 5, "a.txt".into(), 100);
        assert!(t.add_in(p, 5, "b.txt".into(), 50), "mesmo id, direção diferente");
        assert!(!t.add_in(p, 5, "b.txt".into(), 50), "oferta repetida");
        assert_eq!(t.asking(&p), 1);
        assert_eq!(t.take_answer(&p, 5), None);
        t.answered(&p, 5, true);
        assert_eq!(t.take_answer(&p, 5), Some(true));
        assert_eq!(t.take_answer(&p, 5), None, "a resposta se vê uma vez");
        t.phase(&p, 5, true, Phase::Active);
        t.progress(&p, 5, true, 1000);
        t.phase(&p, 5, true, Phase::Done(None));
        t.phase(&p, 5, true, Phase::Failed("tarde".into()));
        let x = t.of(&p).into_iter().find(|x| x.mine).unwrap();
        assert_eq!((x.phase, x.bytes), (Phase::Done(None), 100));
        assert!(t.of(&p).iter().any(|x| !x.mine && x.phase.live()));
        assert_eq!(t.asked(&p, 5), Some(("b.txt".into(), 50)));
    }

    #[test]
    fn cancel_is_requested_and_unknown_counts_as_cancelled() {
        let (p, mut t) = ([1u8; 32], Transfers::default());
        t.add_out(p, 1, "a".into(), 1);
        assert!(!t.cancel_requested(&p, 1, true));
        t.request_cancel(&p, 1, true);
        assert!(t.cancel_requested(&p, 1, true));
        assert!(t.cancel_requested(&p, 99, true));
    }

    #[test]
    fn only_a_few_finished_transfers_are_kept() {
        let (p, mut t) = ([1u8; 32], Transfers::default());
        t.add_out(p, 0, "viva".into(), 1);
        for i in 1..=2 * KEEP_DONE as u64 {
            t.add_out(p, i, "x".into(), 1);
            t.phase(&p, i, true, Phase::Cancelled);
        }
        let all = t.of(&p);
        assert!(all.iter().any(|x| x.id == 0), "a viva fica");
        assert!(all.len() <= KEEP_DONE + 2);
    }

    #[test]
    fn size_text_is_short() {
        assert_eq!(size_text(999), "999 B");
        assert_eq!(size_text(1536), "1,5 KB");
        assert_eq!(size_text(32 * 1024 * 1024), "32,0 MB");
    }
}
