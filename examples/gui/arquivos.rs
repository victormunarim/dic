//! Transferências de arquivo do bate-papo 1:1 como a janela as vê: o que foi oferecido, o que espera resposta da
//! pessoa, o que anda e como terminou. Só estado de exibição; os bytes e o disco ficam no laço do nó
//! (`engine/node.rs`, via `clam::transfer`).

use std::path::{Path, PathBuf};

/// Imagem ou vídeo até este tamanho desce sozinho.
pub(crate) const AUTO_MEDIA_MAX: u64 = 50 << 20;
/// Teto do "baixar todo arquivo recebido" (a opção da configuração).
pub(crate) const AUTO_ALL_MAX: u64 = 100 << 20;

/// Um pedido de arquivo de contato vira download sem a pessoa clicar? Imagem e vídeo de até 50 MB, sempre; qualquer
/// outro arquivo só com a opção `all` ligada, até 100 MB. Antes do aceite nada é gravado neste computador.
pub(crate) fn auto_accept(name: &str, size: u64, all: bool) -> bool {
    (crate::midia::kind(name).is_some() && size <= AUTO_MEDIA_MAX) || (all && size <= AUTO_ALL_MAX)
}

/// O arquivo está na pasta dos recebidos, a que se apaga ao fechar? Compara caminhos já resolvidos: o nome que o contato
/// manda nunca tem separador (`transfer::safe_name`), então um arquivo recebido não sai da pasta por `..`.
pub(crate) fn is_temporary(dir: &Path, path: &Path) -> bool {
    path.starts_with(dir)
}

/// Apaga tudo o que a pasta dos recebidos tem: arquivos inteiros, parciais de um fechamento brusco e qualquer outra
/// coisa. Roda ao abrir (um fim brusco não deixa o arquivo para trás) e ao fechar. Não segue link: apaga o link, não
/// o alvo. O que não der para apagar (no Windows, arquivo aberto por outro programa) fica para a próxima abertura.
pub(crate) fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        let _ = if is_dir { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    }
}

/// Leva o arquivo para `dest`. Renomeia e, se `dest` está noutro disco, copia e apaga o original, de modo que nunca
/// sobram duas cópias nem se perde a única.
fn move_file(path: &Path, dest: &Path) -> std::io::Result<()> {
    if std::fs::rename(path, dest).is_ok() {
        return Ok(());
    }
    std::fs::copy(path, dest)?;
    std::fs::remove_file(path)
}

/// Baixar: leva o arquivo recebido de `id` para dentro de `dest_dir` (a pasta de baixados, sem perguntar) e o registra
/// como baixado. Sair da pasta dos recebidos é o que tira a marca de "apagar". Devolve o novo caminho (quem chama grava o
/// registro com `save_kept`). Erro, e o arquivo continua onde estava: nada se perde.
pub(crate) fn keep(t: &std::sync::Mutex<Transfers>, received: &Path, dest_dir: &Path, peer: [u8; 32], id: u64) -> Result<PathBuf, String> {
    use clam::sync::LockExt;
    let from = {
        let mut t = t.lock_unpoisoned();
        match t.find(&peer, id, false).map(|x| &x.phase) {
            Some(Phase::Done(Some(p))) if is_temporary(received, p) => p.clone(),
            _ => return Err("o arquivo não está mais entre os recebidos".into()),
        }
    };
    if is_temporary(received, dest_dir) {
        return Err(format!("a pasta de baixados não pode ser {}: o que está lá é apagado ao fechar", received.display()));
    }
    std::fs::create_dir_all(dest_dir).map_err(|e| format!("não consegui criar {}: {e}", dest_dir.display()))?;
    let name = from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let to = clam::transfer::unique_path(dest_dir, &name);
    move_file(&from, &to).map_err(|e| format!("não consegui guardar {}: {e}", to.display()))?;
    t.lock_unpoisoned().mark_kept(peer, id, to.clone());
    Ok(to)
}

/// Grava o registro dos baixados (`conf::kept_path`). Um de cada vez, e o texto é tirado já com a trava de gravação, para
/// que duas gravações seguidas nunca deixem o estado mais velho por último.
pub(crate) fn save_kept(t: &std::sync::Mutex<Transfers>) -> std::io::Result<()> {
    use clam::sync::LockExt;
    static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = WRITING.lock_unpoisoned();
    let text = t.lock_unpoisoned().render_kept();
    crate::conf::write_private(&crate::conf::kept_path(), &text)
}

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
    /// Onde está o arquivo que enviei (para a miniatura); o do contato vem em `Phase::Done`.
    source: Option<PathBuf>,
}

impl Xfer {
    /// O arquivo no disco, quando já dá para mostrá-lo: o que enviei, ou o que terminou de chegar.
    pub(crate) fn media_path(&self) -> Option<&Path> {
        match (&self.phase, self.mine) {
            (Phase::Done(Some(p)), false) => Some(p),
            (Phase::Done(_), true) => self.source.as_deref(),
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Transfers {
    list: Vec<Xfer>,
    /// Arquivos recebidos que a pessoa baixou, por (contato, número da oferta). Sobrevivem às 20 transferências
    /// guardadas e ao fechamento do programa (`render_kept`/`load_kept`), para a conversa seguir mostrando o arquivo.
    kept: std::collections::BTreeMap<([u8; 32], u64), PathBuf>,
}

impl Transfers {
    fn find(&mut self, peer: &[u8; 32], id: u64, mine: bool) -> Option<&mut Xfer> {
        self.list.iter_mut().find(|x| x.id == id && x.peer == *peer && x.mine == mine)
    }

    /// Oferta minha, esperando resposta.
    pub(crate) fn add_out(&mut self, peer: [u8; 32], id: u64, name: String, size: u64) {
        self.push(Xfer { id, peer, name, size, mine: true, phase: Phase::Offering, bytes: 0, answer: None, cancel: false, source: None });
    }

    /// Guarda o caminho do arquivo que enviei, para mostrar a miniatura.
    pub(crate) fn set_source(&mut self, peer: &[u8; 32], id: u64, path: PathBuf) {
        if let Some(x) = self.find(peer, id, true) {
            x.source = Some(path);
        }
    }

    /// Oferta do contato. `false` se é repetida (a oferta é reenviada quando o ack se perde).
    pub(crate) fn add_in(&mut self, peer: [u8; 32], id: u64, name: String, size: u64) -> bool {
        if self.find(&peer, id, false).is_some() {
            return false;
        }
        self.push(Xfer { id, peer, name, size, mine: false, phase: Phase::Asking, bytes: 0, answer: None, cancel: false, source: None });
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
        let mut out: Vec<Xfer> = self.list.iter().filter(|x| x.peer == *peer).cloned().collect();
        // Baixados de outra sessão (ou já fora das 20 guardadas) voltam como recebidos prontos.
        for ((_, id), path) in self.kept.range((*peer, 0)..=(*peer, u64::MAX)) {
            if out.iter().any(|x| x.id == *id && !x.mine) {
                continue;
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let size = std::fs::metadata(path).map_or(0, |m| m.len());
            out.push(Xfer { id: *id, peer: *peer, name, size, mine: false, phase: Phase::Done(Some(path.clone())), bytes: size, answer: None, cancel: false, source: None });
        }
        out
    }

    /// O arquivo recebido foi baixado para `path`: a transferência passa a apontar para lá e o registro guarda isso.
    fn mark_kept(&mut self, peer: [u8; 32], id: u64, path: PathBuf) {
        if let Some(x) = self.find(&peer, id, false) {
            x.phase = Phase::Done(Some(path.clone()));
        }
        self.kept.insert((peer, id), path);
    }

    /// O registro dos baixados em texto, uma linha por arquivo: `chave número caminho`. Caminho com quebra de linha
    /// não se registra (o arquivo fica baixado, mas não reaparece depois de reabrir).
    pub(crate) fn render_kept(&self) -> String {
        self.kept.iter().filter(|(_, p)| !p.to_string_lossy().contains(['\n', '\r'])).map(|((k, id), p)| format!("{} {id} {}\n", crate::conf::hex(k), p.display())).collect()
    }

    /// Lê o registro de `render_kept`; o que não existe mais no disco fica de fora.
    pub(crate) fn load_kept(&mut self, text: &str) {
        for l in text.lines() {
            let mut p = l.splitn(3, ' ');
            let (Some(k), Some(id), Some(path)) = (p.next(), p.next(), p.next()) else { continue };
            let (Ok(k), Ok(id)) = (crate::conf::unhex(k), id.parse()) else { continue };
            if Path::new(path).is_file() {
                self.kept.insert((k, id), PathBuf::from(path));
            }
        }
    }

    /// Apagar a conversa com `peer`: tira da tela as transferências encerradas dele e apaga os recebidos que ainda
    /// estavam na pasta temporária (o histórico que os mostrava sumiu). Os baixados **continuam no disco**: são da
    /// pessoa, só deixam de aparecer na conversa. Devolve `true` se o registro dos baixados mudou. O que ainda anda fica.
    pub(crate) fn forget_peer(&mut self, peer: &[u8; 32], received: &Path) -> bool {
        self.list.retain(|x| {
            let gone = x.peer == *peer && !x.phase.live();
            if gone && let (false, Phase::Done(Some(p))) = (x.mine, &x.phase) && is_temporary(received, p) {
                let _ = std::fs::remove_file(p);
            }
            !gone
        });
        let before = self.kept.len();
        self.kept.retain(|(k, _), _| k != peer);
        self.kept.len() != before
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

/// Liga cada linha `📎 nome` do bate-papo da sala à transferência que a acompanha, para a sala mostrar o mesmo que a
/// conversa 1:1 (progresso, baixar, imagem, vídeo). `lines`: por linha, `Some(contato)` se veio de outro membro ou `None`
/// se é minha. Devolve, por linha, o índice em `xfers`. De outro membro: a transferência dele com aquele nome, casando as
/// linhas repetidas pela ordem (a mais nova com a mais nova). Minha: qualquer envio meu com aquele nome, preferindo o que
/// já tem arquivo à vista (o mesmo arquivo sai a vários contatos, então só um serve para mostrar).
pub(crate) fn pair_room_files(lines: &[(Option<[u8; 32]>, &str)], xfers: &[Xfer]) -> Vec<Option<usize>> {
    let mut out = vec![None; lines.len()];
    for (i, (from, text)) in lines.iter().enumerate() {
        let Some(name) = text.strip_prefix("📎 ") else { continue };
        let mut cands: Vec<usize> = xfers.iter().enumerate().filter(|(_, x)| x.name == name && match from {
            Some(k) => !x.mine && x.peer == *k,
            None => x.mine,
        }).map(|(j, _)| j).collect();
        cands.sort_by_key(|&j| xfers.get(j).map_or(0, |x| x.id));
        out[i] = match from {
            None => cands.iter().copied().find(|&j| xfers.get(j).is_some_and(|x| x.media_path().is_some())).or(cands.last().copied()),
            Some(k) => {
                // Posição desta linha entre as iguais do mesmo membro, contada do fim.
                let later = lines.iter().skip(i + 1).filter(|(f, t)| *f == Some(*k) && *t == *text).count();
                cands.len().checked_sub(1 + later).and_then(|p| cands.get(p).copied())
            }
        };
    }
    out
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
    fn only_small_media_downloads_alone_unless_the_option_is_on() {
        let mb = 1 << 20;
        assert!(auto_accept("foto.JPG", 50 * mb, false));
        assert!(auto_accept("filme.mp4", 10 * mb, false));
        assert!(!auto_accept("foto.png", 50 * mb + 1, false), "mídia grande espera o clique");
        assert!(!auto_accept("relatorio.pdf", mb, false), "outro tipo espera o clique");
        assert!(!auto_accept("sem-extensao", 1, false));
        assert!(auto_accept("relatorio.pdf", 100 * mb, true));
        assert!(!auto_accept("relatorio.pdf", 100 * mb + 1, true));
        assert!(auto_accept("filme.mp4", 80 * mb, true), "com a opção, mídia grande até 100 MB também");
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
    fn media_path_exists_only_when_the_file_is_there() {
        let (p, mut t) = ([1u8; 32], Transfers::default());
        t.add_out(p, 1, "a.png".into(), 1);
        t.set_source(&p, 1, "/x/a.png".into());
        assert!(t.of(&p)[0].media_path().is_none(), "em andamento ainda não");
        t.phase(&p, 1, true, Phase::Done(None));
        assert_eq!(t.of(&p)[0].media_path(), Some(Path::new("/x/a.png")));
        t.add_in(p, 2, "b.png".into(), 1);
        t.phase(&p, 2, false, Phase::Done(Some("/d/b.png".into())));
        assert_eq!(t.of(&p).iter().find(|x| !x.mine).and_then(|x| x.media_path()), Some(Path::new("/d/b.png")));
    }

    #[test]
    fn room_lines_pair_with_their_own_transfers() {
        let (a, b, mut t) = ([1u8; 32], [2u8; 32], Transfers::default());
        t.add_in(a, 1, "x.png".into(), 1);
        t.add_in(a, 2, "x.png".into(), 1);
        t.add_in(b, 3, "x.png".into(), 1);
        t.add_out(b, 9, "y.mp4".into(), 1);
        t.add_out(a, 8, "y.mp4".into(), 1);
        t.set_source(&a, 8, "/s/y.mp4".into());
        t.phase(&a, 8, true, Phase::Done(None));
        let mut all = t.of(&a);
        all.extend(t.of(&b));
        let lines = [(Some(a), "📎 x.png"), (Some(b), "oi"), (Some(a), "📎 x.png"), (None, "📎 y.mp4"), (Some(a), "📎 sumiu.txt")];
        let got = pair_room_files(&lines, &all);
        let id = |i: usize| got[i].map(|j| all[j].id);
        assert_eq!((id(0), id(2)), (Some(1), Some(2)), "linhas repetidas casam pela ordem");
        assert_eq!(id(1), None, "texto comum não tem arquivo");
        assert_eq!(id(3), Some(8), "o envio meu que já tem arquivo à vista");
        assert_eq!(id(4), None, "sem transferência");
        assert_eq!(pair_room_files(&[(Some(a), "📎 x.png")], &all[..1]), vec![Some(0)]);
        assert_eq!(pair_room_files(&[(Some(a), "📎 x.png"), (Some(a), "📎 x.png"), (Some(a), "📎 x.png")], &all[..2]), vec![None, Some(0), Some(1)], "linha antiga sem transferência guardada");
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("clam-recebidos-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sweep_empties_the_received_folder_including_leftovers_of_a_hard_close() {
        let d = scratch("varre");
        std::fs::write(d.join("foto.png"), b"1").unwrap();
        std::fs::write(d.join(".clam-7.parcial"), b"2").unwrap();
        std::fs::create_dir_all(d.join("sobra")).unwrap();
        std::fs::write(d.join("sobra").join("x"), b"3").unwrap();
        sweep(&d);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "nada fica");
        assert!(d.exists(), "a pasta em si fica");
        sweep(&d.join("nao-existe"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sweep_never_touches_what_was_kept_outside_the_folder() {
        let d = scratch("fora");
        let (recebidos, guardados) = (d.join("recebidos"), d.join("guardados"));
        std::fs::create_dir_all(&recebidos).unwrap();
        std::fs::create_dir_all(&guardados).unwrap();
        std::fs::write(recebidos.join("a.pdf"), b"a").unwrap();
        std::fs::write(recebidos.join("b.pdf"), b"b").unwrap();
        move_file(&recebidos.join("a.pdf"), &guardados.join("a.pdf")).unwrap();
        assert!(!recebidos.join("a.pdf").exists(), "baixar tira o arquivo da pasta dos recebidos");
        sweep(&recebidos);
        assert_eq!(std::fs::read(guardados.join("a.pdf")).unwrap(), b"a", "o baixado permanece");
        assert!(!recebidos.join("b.pdf").exists(), "o não baixado some");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn done_in(t: &mut Transfers, peer: [u8; 32], id: u64, at: PathBuf) {
        t.add_in(peer, id, "a.pdf".into(), 1);
        t.phase(&peer, id, false, Phase::Done(Some(at)));
    }

    #[test]
    fn keeping_moves_the_file_out_registers_it_and_survives_a_restart() {
        let d = scratch("baixar");
        let (recebidos, destino) = (d.join("recebidos"), d.join("baixados"));
        std::fs::create_dir_all(&recebidos).unwrap();
        std::fs::write(recebidos.join("a.pdf"), b"conteudo").unwrap();
        let (peer, t) = ([3u8; 32], std::sync::Mutex::new(Transfers::default()));
        done_in(&mut t.lock().unwrap(), peer, 9, recebidos.join("a.pdf"));
        let to = keep(&t, &recebidos, &destino, peer, 9).unwrap();
        let text = t.lock().unwrap().render_kept();
        assert_eq!(to, destino.join("a.pdf"));
        assert!(!recebidos.join("a.pdf").exists());
        assert_eq!(t.lock().unwrap().of(&peer).first().and_then(|x| x.media_path().map(Path::to_path_buf)), Some(to.clone()), "a conversa passa a apontar para o novo lugar");
        // Fechou e abriu: a pasta dos recebidos é varrida, o baixado fica e reaparece no mesmo lugar.
        sweep(&recebidos);
        let mut novo = Transfers::default();
        novo.load_kept(&text);
        let x = novo.of(&peer).into_iter().next().unwrap();
        assert_eq!((x.id, x.mine, x.media_path()), (9, false, Some(to.as_path())));
        assert_eq!(x.size, 8);
        // Segundo arquivo com o mesmo nome não sobrescreve o primeiro.
        std::fs::write(recebidos.join("a.pdf"), b"outro").unwrap();
        done_in(&mut t.lock().unwrap(), peer, 10, recebidos.join("a.pdf"));
        let to2 = keep(&t, &recebidos, &destino, peer, 10).unwrap();
        assert_eq!(to2, destino.join("a (2).pdf"));
        assert_eq!(std::fs::read(&to).unwrap(), b"conteudo");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keeping_refuses_what_it_should_and_loses_nothing() {
        let d = scratch("recusa");
        let (recebidos, destino) = (d.join("recebidos"), d.join("baixados"));
        std::fs::create_dir_all(&recebidos).unwrap();
        std::fs::write(recebidos.join("a.pdf"), b"x").unwrap();
        let (peer, t) = ([3u8; 32], std::sync::Mutex::new(Transfers::default()));
        assert!(keep(&t, &recebidos, &destino, peer, 1).is_err(), "sem transferência");
        done_in(&mut t.lock().unwrap(), peer, 1, recebidos.join("a.pdf"));
        assert!(keep(&t, &recebidos, &recebidos.join("sub"), peer, 1).is_err(), "baixar para dentro da pasta que se apaga");
        assert!(recebidos.join("a.pdf").exists());
        // Destino que não dá para criar (um arquivo no lugar da pasta): o arquivo fica onde estava.
        std::fs::write(d.join("arquivo"), b"").unwrap();
        assert!(keep(&t, &recebidos, &d.join("arquivo").join("x"), peer, 1).is_err());
        assert!(recebidos.join("a.pdf").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn forgetting_a_peer_drops_his_files_that_were_not_downloaded_and_keeps_the_downloaded_ones() {
        let d = scratch("esquece");
        let (recebidos, destino) = (d.join("recebidos"), d.join("baixados"));
        std::fs::create_dir_all(&recebidos).unwrap();
        std::fs::write(recebidos.join("a.pdf"), b"a").unwrap();
        std::fs::write(recebidos.join("b.pdf"), b"b").unwrap();
        let (ana, bia, t) = ([1u8; 32], [2u8; 32], std::sync::Mutex::new(Transfers::default()));
        done_in(&mut t.lock().unwrap(), ana, 1, recebidos.join("a.pdf"));
        done_in(&mut t.lock().unwrap(), ana, 2, recebidos.join("b.pdf"));
        let baixado = keep(&t, &recebidos, &destino, ana, 1).unwrap();
        std::fs::write(recebidos.join("c.pdf"), b"c").unwrap();
        done_in(&mut t.lock().unwrap(), bia, 3, recebidos.join("c.pdf"));
        let mudou = t.lock().unwrap().forget_peer(&ana, &recebidos);
        assert!(mudou);
        assert!(t.lock().unwrap().of(&ana).is_empty(), "a conversa não mostra mais nada");
        assert!(!recebidos.join("b.pdf").exists(), "o não baixado some");
        assert!(baixado.exists(), "o baixado é da pessoa e fica no disco");
        assert!(recebidos.join("c.pdf").exists(), "o de outro contato não é tocado");
        assert_eq!(t.lock().unwrap().of(&bia).len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_kept_record_ignores_missing_files_and_garbage() {
        let d = scratch("registro");
        std::fs::write(d.join("x.png"), b"1").unwrap();
        let mut t = Transfers::default();
        t.load_kept(&format!("{} 5 {}\n{} 6 {}\nlixo\nzz 1 /x\n", crate::conf::hex(&[4; 32]), d.join("x.png").display(), crate::conf::hex(&[4; 32]), d.join("sumiu.png").display()));
        assert_eq!(t.of(&[4; 32]).len(), 1, "só o que existe volta");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_files_inside_the_received_folder_count_as_temporary() {
        let dir = Path::new("/h/.clam-gui.recebidos");
        assert!(is_temporary(dir, &dir.join("foto.png")));
        assert!(!is_temporary(dir, Path::new("/h/Documentos/foto.png")), "o que eu enviei é o original da pessoa");
        assert!(!is_temporary(dir, Path::new("/h/.clam-gui.recebidos-velho/foto.png")), "compara por componente, não por prefixo de texto");
    }

    #[test]
    fn size_text_is_short() {
        assert_eq!(size_text(999), "999 B");
        assert_eq!(size_text(1536), "1,5 KB");
        assert_eq!(size_text(32 * 1024 * 1024), "32,0 MB");
    }
}
