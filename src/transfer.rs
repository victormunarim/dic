//! Transferência de arquivo entre dois contatos online: quem envia lê o arquivo em pedaços e os manda por circuitos
//! (direto ou de 2 saltos, `Dialer::open_file_pipe`); quem recebe junta os pedaços num arquivo parcial e, completo, dá o nome
//! final. Os quadros estão em [`crate::media::update::FileFrame`]; a decisão de aceitar é da pessoa (o app).
//!
//! - **Só contato, só online, só se a pessoa aceita.** A oferta não escreve nada no disco; o arquivo parcial só nasce
//!   depois do "aceitar" ([`Incoming::create`]).
//! - **Memória limitada.** Quem recebe guarda uma tabela de bits (até ~80 KB) e grava cada pedaço no lugar dele;
//!   quem envia lê uma janela por vez.
//! - **Nome seguro.** O nome que o contato manda nunca vira caminho: [`safe_name`] tira separadores, controles e pontos
//!   do começo, e [`Incoming::finish`] nunca sobrescreve um arquivo que já existe.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::media::update::{FILE_CHUNK, FileFrame, MAX_FILE, MAX_FILE_NAME};

/// Pedaços em voo ao mesmo tempo (~170 KB, uns 2 s no ritmo do circuito de tela): limita a memória e quanto se
/// reenvia se o circuito cair.
pub const WINDOW: usize = 1024;
/// Circuitos novos seguidos, sem nenhum ack, antes de desistir do contato.
const ROUNDS: usize = 3;
/// Pedaços por lote: a fila do cover guarda 16 células, então 8 de cada vez, no ritmo dele.
const BURST: usize = 8;
/// Um pedaço sem ack por este tempo sai de novo (o cover descarta ~1% das células quando a fila enche).
const RESEND: Duration = Duration::from_millis(3000);
/// Sem ack algum por este tempo, o circuito é dado como morto e outro é aberto.
const STALL: Duration = Duration::from_secs(10);

/// Por que uma transferência parou.
#[derive(Debug, PartialEq, Eq)]
pub enum TransferError {
    /// O contato não confirmou os pedaços (offline, sem rota, ou desistiu).
    Unreachable,
    /// A pessoa cancelou.
    Cancelled,
    /// Não deu para ler o arquivo.
    Read(String),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable => f.write_str("o contato parou de confirmar o arquivo"),
            Self::Cancelled => f.write_str("cancelado"),
            Self::Read(e) => write!(f, "não consegui ler o arquivo: {e}"),
        }
    }
}

/// Número de pedaços de um arquivo de `size` bytes.
pub fn chunk_count(size: u64) -> u64 {
    size.div_ceil(FILE_CHUNK as u64)
}

/// Manda o arquivo `path` (de `size` bytes, já combinado com o contato) em pedaços, na transferência `id`. `progress`
/// recebe os bytes já confirmados; `cancelled` é consultado a cada lote (alguns milissegundos).
///
/// Um circuito só leva o arquivo inteiro, no ritmo constante do circuito de tela: o que um observador vê é o mesmo de
/// uma tela sendo transmitida. Os pedaços ficam "em voo" até o ack voltar (no máximo [`WINDOW`], o que também limita a
/// memória); o que não voltou em [`RESEND`] sai de novo no meio do fluxo, sem parar para esperar uma janela acabar.
#[cfg(feature = "net")]
pub async fn stream(dialer: &crate::node::Dialer, peer: [u8; 32], id: u64, path: &Path, size: u64, progress: impl Fn(u64), cancelled: impl Fn() -> bool) -> Result<(), TransferError> {
    use std::{collections::BTreeMap, time::Instant};

    use crate::{media::update::file_chunk_ack, node::FilePipe};

    if size == 0 || size > MAX_FILE {
        return Err(TransferError::Read("tamanho fora do limite".into()));
    }
    let mut file = File::open(path).map_err(|e| TransferError::Read(e.to_string()))?;
    let total = chunk_count(size);
    let mut next: u64 = 0;
    let mut done: u64 = 0;
    // Em voo: ack esperado -> (pedaço, quando saiu pela última vez).
    let mut flying: BTreeMap<u64, (FileFrame, Instant)> = BTreeMap::new();
    let mut pipe: Option<FilePipe> = None;
    let mut last_ack = Instant::now();
    let mut reopened = 0usize;
    while done < total {
        if cancelled() {
            return Err(TransferError::Cancelled);
        }
        let Some(p) = pipe.as_mut() else {
            if reopened > ROUNDS {
                return Err(TransferError::Unreachable);
            }
            reopened += 1;
            // Abrir o circuito leva segundos: o cancelamento vale durante.
            let open = dialer.open_file_pipe(peer, id);
            tokio::pin!(open);
            pipe = loop {
                tokio::select! {
                    p = &mut open => break p,
                    () = tokio::time::sleep(Duration::from_millis(100)) => if cancelled() {
                        return Err(TransferError::Cancelled);
                    },
                }
            };
            last_ack = Instant::now();
            // O que estava em voo foi com o circuito antigo: vale reenviar já.
            let past = Instant::now().checked_sub(RESEND).unwrap_or_else(Instant::now);
            flying.values_mut().for_each(|(_, at)| *at = past);
            continue;
        };
        // Lote: primeiro o que passou do prazo do ack, depois pedaços novos enquanto couber na janela.
        let mut batch: Vec<FileFrame> = Vec::with_capacity(BURST);
        let now = Instant::now();
        for (frame, at) in flying.values_mut().filter(|(_, at)| now.duration_since(*at) >= RESEND).take(BURST) {
            *at = now;
            batch.push(frame.clone());
        }
        while batch.len() < BURST && next < total && flying.len() + batch.len() < WINDOW {
            let want = (size - next * FILE_CHUNK as u64).min(FILE_CHUNK as u64) as usize;
            let mut data = vec![0u8; want];
            file.read_exact(&mut data).map_err(|e| TransferError::Read(e.to_string()))?;
            batch.push(FileFrame::Chunk { id, index: next as u32, data });
            next += 1;
        }
        let sent_now = Instant::now();
        let fresh: Vec<(u64, FileFrame)> = batch.iter().filter_map(|f| if let FileFrame::Chunk { index, .. } = f { Some((file_chunk_ack(id, *index), f.clone())) } else { None }).collect();
        match p.push(&batch).await {
            Ok(acks) => {
                for (seq, frame) in fresh {
                    flying.entry(seq).or_insert((frame, sent_now));
                }
                let before = done;
                for seq in acks {
                    if flying.remove(&seq).is_some() {
                        done += 1;
                    }
                }
                if done != before {
                    last_ack = Instant::now();
                    reopened = 0;
                    progress((done * FILE_CHUNK as u64).min(size));
                }
                // Sem nenhum ack por muito tempo, o circuito morreu sem avisar: abre outro.
                if !flying.is_empty() && last_ack.elapsed() >= STALL {
                    pipe = None;
                }
            }
            Err(_) => {
                for (seq, frame) in fresh {
                    flying.entry(seq).or_insert((frame, sent_now));
                }
                pipe = None;
            }
        }
    }
    if let Some(p) = pipe {
        dialer.remember_file_route(&peer, p);
    }
    Ok(())
}

/// Nome que cabe como nome de arquivo em qualquer sistema: sem separadores, controles nem pontos ou espaços nas pontas, e com
/// o que o Windows recusa trocado por `_`. Nunca vazio.
pub fn safe_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim_end_matches(['.', ' ']);
    // O limite do quadro é em bytes, não em caracteres.
    let mut out = String::new();
    for c in trimmed.chars() {
        if out.len() + c.len_utf8() > MAX_FILE_NAME {
            break;
        }
        out.push(c);
    }
    if out.is_empty() {
        out = "arquivo".into();
    }
    out
}

/// Caminho livre em `dir` para `name`: o próprio nome, ou `nome (2).ext`, `nome (3).ext`...
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let name = safe_name(name);
    let first = dir.join(&name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Arquivo que está chegando: o parcial no disco e a tabela do que já veio.
pub struct Incoming {
    file: File,
    part: PathBuf,
    dir: PathBuf,
    name: String,
    size: u64,
    got: Vec<u64>,
    count: u64,
}

impl Incoming {
    /// Cria (e esvazia) o arquivo parcial em `dir`, que passa a existir. `size` entre 1 e [`MAX_FILE`].
    pub fn create(dir: &Path, id: u64, size: u64, name: &str) -> io::Result<Self> {
        if size == 0 || size > MAX_FILE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "tamanho fora do limite"));
        }
        std::fs::create_dir_all(dir)?;
        let part = dir.join(format!(".clam-{id}.parcial"));
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let file = opts.open(&part)?;
        file.set_len(size)?;
        Ok(Self { file, part, dir: dir.to_path_buf(), name: safe_name(name), size, got: vec![0; chunk_count(size).div_ceil(64) as usize], count: 0 })
    }

    /// Grava o pedaço `index`. `Ok(true)` se é novo, `Ok(false)` se já tinha (reenvio). Pedaço fora da tabela ou com tamanho
    /// errado é recusado: o contato não faz o arquivo crescer nem escreve fora do lugar.
    pub fn put(&mut self, index: u32, data: &[u8]) -> io::Result<bool> {
        let at = u64::from(index) * FILE_CHUNK as u64;
        let expect = self.size.saturating_sub(at).min(FILE_CHUNK as u64) as usize;
        if at >= self.size || data.len() != expect {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "pedaço fora do arquivo"));
        }
        let (word, bit) = ((index / 64) as usize, 1u64 << (index % 64));
        let Some(w) = self.got.get_mut(word) else { return Err(io::Error::new(io::ErrorKind::InvalidData, "pedaço fora da tabela")) };
        if *w & bit != 0 {
            return Ok(false);
        }
        self.file.seek(SeekFrom::Start(at))?;
        self.file.write_all(data)?;
        *w |= bit;
        self.count += 1;
        Ok(true)
    }

    /// Bytes já gravados.
    pub fn received(&self) -> u64 {
        (self.count * FILE_CHUNK as u64).min(self.size)
    }

    pub fn complete(&self) -> bool {
        self.count == chunk_count(self.size)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Dá o nome final ao arquivo completo (sem sobrescrever outro) e devolve o caminho. Antes de completar, recusa.
    pub fn finish(self) -> io::Result<PathBuf> {
        if !self.complete() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "arquivo incompleto"));
        }
        self.file.sync_all()?;
        let to = unique_path(&self.dir, &self.name);
        std::fs::rename(&self.part, &to)?;
        Ok(to)
    }

    /// Joga fora o parcial (recusa, cancelamento ou falha).
    pub fn abort(self) {
        let _ = std::fs::remove_file(&self.part);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("clam-transfer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn safe_name_never_leaves_the_directory() {
        assert_eq!(safe_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_name("a\\b:c*.txt"), "a_b_c_.txt");
        assert_eq!(safe_name("  .oculto. "), "oculto");
        assert_eq!(safe_name("..."), "arquivo");
        assert_eq!(safe_name("a\nb"), "a_b");
        assert_eq!(safe_name("relatório ção.pdf"), "relatório ção.pdf");
        assert!(safe_name(&"x".repeat(500)).len() <= MAX_FILE_NAME);
        assert!(safe_name(&"ç".repeat(500)).len() <= MAX_FILE_NAME);
    }

    #[test]
    fn unique_path_never_overwrites() {
        let d = dir("unico");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("foto.png"), b"1").unwrap();
        std::fs::write(d.join("foto (2).png"), b"2").unwrap();
        std::fs::write(d.join("sem_ext"), b"3").unwrap();
        assert_eq!(unique_path(&d, "foto.png"), d.join("foto (3).png"));
        assert_eq!(unique_path(&d, "sem_ext"), d.join("sem_ext (2)"));
        assert_eq!(unique_path(&d, "novo.txt"), d.join("novo.txt"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn incoming_assembles_out_of_order_with_repeats() {
        let d = dir("junta");
        let size = (FILE_CHUNK * 3 + 17) as u64;
        let content: Vec<u8> = (0..size).map(|i| (i * 7 % 251) as u8).collect();
        let mut inc = Incoming::create(&d, 42, size, "dados.bin").unwrap();
        let piece = |i: usize| content[i * FILE_CHUNK..((i + 1) * FILE_CHUNK).min(content.len())].to_vec();
        for (i, new) in [(3, true), (1, true), (1, false), (0, true)] {
            assert_eq!(inc.put(i as u32, &piece(i)).unwrap(), new);
        }
        assert!(!inc.complete());
        assert!(inc.put(2, &piece(2)).unwrap());
        assert!(!inc.put(2, &piece(2)).unwrap(), "repetido");
        assert!(inc.complete());
        assert_eq!(inc.received(), size);
        let path = inc.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), content);
        assert!(std::fs::read_dir(&d).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".parcial")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn incoming_refuses_pieces_that_do_not_fit() {
        let d = dir("recusa");
        let size = (FILE_CHUNK + 5) as u64;
        let mut inc = Incoming::create(&d, 1, size, "a").unwrap();
        assert!(inc.put(2, &[0; 5]).is_err(), "índice além do fim");
        assert!(inc.put(0, &[0; 10]).is_err(), "tamanho errado");
        assert!(inc.put(1, &[0; FILE_CHUNK]).is_err(), "último pedaço longo demais");
        assert!(inc.put(1, &[0; 5]).unwrap());
        assert!(Incoming::create(&d, 2, 0, "a").is_err());
        assert!(Incoming::create(&d, 3, MAX_FILE + 1, "a").is_err());
        inc.abort();
        let _ = std::fs::remove_dir_all(&d);
    }
}
