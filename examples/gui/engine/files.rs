//! Arquivos do bate-papo 1:1 no laço do nó: oferta, resposta, pedaços e cancelamento. O protocolo e o disco estão
//! em `clam::transfer`; aqui só se liga isso ao estado da janela (`arquivos::Transfers`) e ao histórico.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use clam::{
    media::update::{FileFrame, MAX_FILE},
    node::Dialer,
    sync::LockExt,
    transfer::{self, Incoming},
};

use super::{Shared, Sink};
use crate::arquivos::Phase;

/// Quanto o envio espera a resposta do contato à oferta; a pessoa do outro lado pode estar longe da tela.
const ANSWER_WAIT: Duration = Duration::from_secs(300);
const CONTROL_WAIT: Duration = Duration::from_secs(8);
/// Ofertas abertas de um mesmo contato; passando disso, as novas são ignoradas.
const MAX_ASKING: usize = 8;

/// Arquivos que estão chegando, por (contato, número). Mora no laço do nó: o disco não passa pela janela.
#[derive(Default)]
pub(crate) struct Receiving {
    open: HashMap<([u8; 32], u64), Incoming>,
}

/// A pessoa escolheu um arquivo: confere, registra a oferta e a manda; o envio só segue com o aceite.
pub(crate) fn start_send(shared: &std::sync::Arc<Shared>, dialer: &Dialer, out: &Sink, peer: [u8; 32], path: PathBuf) {
    let size = std::fs::metadata(&path).ok().filter(std::fs::Metadata::is_file).map(|m| m.len());
    let name = path.file_name().map(|n| transfer::safe_name(&n.to_string_lossy()));
    let (Some(size), Some(name)) = (size, name) else {
        out.line(format!("não consegui ler {}", path.display()));
        return;
    };
    if size == 0 || size > MAX_FILE {
        out.line(format!("{name}: o arquivo precisa ter de 1 byte a {} MB", MAX_FILE / (1024 * 1024)));
        return;
    }
    let id = dialer.next_chat_id();
    shared.transfers.lock_unpoisoned().add_out(peer, id, name.clone(), size);
    // A mensagem do arquivo entra já no histórico; a conversa mostra o carregando ao lado enquanto anda.
    shared.chats.lock_unpoisoned().push_mine(peer, id, format!("📎 {name}"));
    (out.repaint)();
    let (sh, d, o) = (shared.clone(), dialer.clone(), out.clone());
    tokio::spawn(async move {
        let end = |phase: Phase| {
            let state = if matches!(phase, Phase::Done(_)) { crate::chats::State::Sent } else { crate::chats::State::Failed };
            sh.chats.lock_unpoisoned().set_state(&peer, id, state);
            sh.transfers.lock_unpoisoned().phase(&peer, id, true, phase);
            (o.repaint)();
        };
        let offer = FileFrame::Offer { id, size, name: name.clone() };
        if !d.send_file_frame(peer, &offer, CONTROL_WAIT).await {
            return end(Phase::Failed("contato fora do ar".into()));
        }
        let deadline = tokio::time::Instant::now() + ANSWER_WAIT;
        let accepted = loop {
            // O cadeado não atravessa o `await`: a decisão sai de um bloco só.
            let (cancelled, answer) = {
                let mut t = sh.transfers.lock_unpoisoned();
                (t.cancel_requested(&peer, id, true), t.take_answer(&peer, id))
            };
            if cancelled {
                let _ = d.send_file_frame(peer, &FileFrame::Cancel { id }, CONTROL_WAIT).await;
                return end(Phase::Cancelled);
            }
            if let Some(a) = answer {
                break a;
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = d.send_file_frame(peer, &FileFrame::Cancel { id }, CONTROL_WAIT).await;
                return end(Phase::Failed("sem resposta".into()));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        if !accepted {
            return end(Phase::Declined);
        }
        end(Phase::Active);
        let (sh2, sh3) = (sh.clone(), sh.clone());
        let result = transfer::stream(
            &d,
            peer,
            id,
            &path,
            size,
            move |bytes| {
                sh2.transfers.lock_unpoisoned().progress(&peer, id, true, bytes);
            },
            move || sh3.transfers.lock_unpoisoned().cancel_requested(&peer, id, true),
        )
        .await;
        match result {
            Ok(()) => {
                end(Phase::Done(None));
            }
            Err(transfer::TransferError::Cancelled) => {
                let _ = d.send_file_frame(peer, &FileFrame::Cancel { id }, CONTROL_WAIT).await;
                end(Phase::Cancelled);
            }
            Err(e) => end(Phase::Failed(e.to_string())),
        }
    });
}

/// Trata os quadros de arquivo que chegaram de contatos.
/// Devolve as ofertas novas (contato, número): o laço as aceita sozinho.
pub(crate) fn on_frames(shared: &std::sync::Arc<Shared>, rx: &mut Receiving, frames: Vec<([u8; 32], FileFrame)>) -> Vec<([u8; 32], u64)> {
    let mut fresh = Vec::new();
    for (peer, frame) in frames {
        let id = frame.id();
        match frame {
            FileFrame::Offer { size, name, .. } => {
                let mut t = shared.transfers.lock_unpoisoned();
                if t.asking(&peer) >= MAX_ASKING {
                    continue;
                }
                if t.add_in(peer, id, name.clone(), size) {
                    drop(t);
                    // Conta como mensagem nova: aparece na bolinha do contato.
                    shared.chats.lock_unpoisoned().push_received(peer, id, format!("📎 {name}"));
                    fresh.push((peer, id));
                }
            }
            FileFrame::Answer { accept, .. } => shared.transfers.lock_unpoisoned().answered(&peer, id, accept),
            FileFrame::Chunk { index, data, .. } => {
                let Some(inc) = rx.open.get_mut(&(peer, id)) else { continue };
                let mut t = shared.transfers.lock_unpoisoned();
                match inc.put(index, &data) {
                    Ok(_) => t.progress(&peer, id, false, inc.received()),
                    Err(e) => {
                        t.phase(&peer, id, false, Phase::Failed(e.to_string()));
                        drop(t);
                        if let Some(inc) = rx.open.remove(&(peer, id)) {
                            inc.abort();
                        }
                        continue;
                    }
                }
                if inc.complete() {
                    drop(t);
                    let Some(inc) = rx.open.remove(&(peer, id)) else { continue };
                    let phase = match inc.finish() {
                        Ok(p) => {
                            Phase::Done(Some(p))
                        }
                        Err(e) => Phase::Failed(e.to_string()),
                    };
                    shared.transfers.lock_unpoisoned().phase(&peer, id, false, phase);
                }
            }
            FileFrame::Cancel { .. } => {
                if let Some(inc) = rx.open.remove(&(peer, id)) {
                    inc.abort();
                    shared.transfers.lock_unpoisoned().phase(&peer, id, false, Phase::Cancelled);
                }
                // Oferta minha: o contato desistiu antes ou durante o envio.
                shared.transfers.lock_unpoisoned().request_cancel(&peer, id, true);
                // Oferta dele ainda sem resposta: ele a retirou.
                shared.transfers.lock_unpoisoned().phase(&peer, id, false, Phase::Cancelled);
            }
        }
    }
    fresh
}

/// Resposta a uma oferta (o laço aceita as de contato sozinho; quem cancela é a pessoa). Aceitar cria o arquivo parcial em `~/Downloads/clam`.
pub(crate) fn answer(shared: &std::sync::Arc<Shared>, dialer: &Dialer, out: &Sink, rx: &mut Receiving, peer: [u8; 32], id: u64, accept: bool) {
    let Some((name, size)) = shared.transfers.lock_unpoisoned().asked(&peer, id) else { return };
    let phase = if !accept {
        Phase::Declined
    } else {
        match Incoming::create(&crate::conf::downloads_dir(), id, size, &name) {
            Ok(inc) => {
                rx.open.insert((peer, id), inc);
                Phase::Active
            }
            Err(e) => {
                out.line(format!("não consegui criar o arquivo em {}: {e}", crate::conf::downloads_dir().display()));
                Phase::Failed(e.to_string())
            }
        }
    };
    let accepted = phase == Phase::Active;
    shared.transfers.lock_unpoisoned().phase(&peer, id, false, phase);
    (out.repaint)();
    let (sh, d, o) = (shared.clone(), dialer.clone(), out.clone());
    tokio::spawn(async move {
        if !d.send_file_frame(peer, &FileFrame::Answer { id, accept: accepted }, CONTROL_WAIT).await {
            sh.transfers.lock_unpoisoned().phase(&peer, id, false, Phase::Failed("contato fora do ar".into()));
            (o.repaint)();
        }
    });
}

/// A pessoa cancelou. Do lado que envia, a tarefa de envio vê o pedido; do que recebe, o parcial é apagado.
pub(crate) fn cancel(shared: &std::sync::Arc<Shared>, dialer: &Dialer, out: &Sink, rx: &mut Receiving, peer: [u8; 32], id: u64, mine: bool) {
    shared.transfers.lock_unpoisoned().request_cancel(&peer, id, mine);
    if mine {
        (out.repaint)();
        return;
    }
    if let Some(inc) = rx.open.remove(&(peer, id)) {
        inc.abort();
    }
    shared.transfers.lock_unpoisoned().phase(&peer, id, false, Phase::Cancelled);
    (out.repaint)();
    let d = dialer.clone();
    tokio::spawn(async move {
        let _ = d.send_file_frame(peer, &FileFrame::Cancel { id }, CONTROL_WAIT).await;
    });
}
