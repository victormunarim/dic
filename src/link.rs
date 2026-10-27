//! Cifra de link entre nós (feature `net`): Noise `NK` sobre TCP.
//!
//! Protege o que o circuito não cobre: ids de circuito, comandos e
//! temporização das células ficam ocultos para quem observa a rede, e um
//! atacante no caminho não consegue injetar nem alterar células.
//!
//! - Padrão `Noise_NK_25519_ChaChaPoly_SHA256`: o iniciador (cliente ou relay
//!   anterior) é anônimo e conhece a chave estática do respondedor, a mesma
//!   chave X25519 do descritor do relay. Assim o link só fecha com o relay
//!   certo.
//! - Mensagens do handshake: `[u16 BE tamanho][mensagem]`. Depois, cada célula
//!   (261 bytes) vira uma mensagem Noise de 277 bytes, com o mesmo prefixo de
//!   tamanho, então todo quadro no fio tem tamanho fixo (279 bytes).
//! - Nonces são contadores por direção, exigidos em ordem: o TCP já entrega em
//!   ordem, qualquer repetição ou salto falha na autenticação e fecha o link.
//! - Handshake com prazo de [`HANDSHAKE_TIMEOUT`], para conexões lentas não
//!   segurarem recursos.
//!
//! A chave estática do relay também é usada no handshake do circuito (ntor).
//! Os dois protocolos têm rótulos de domínio distintos (prólogo `clam-link-v1`
//! aqui, HKDF próprio lá), então os usos não se misturam; separar em duas
//! chaves no descritor é melhoria futura.

// Tudo aqui lê bytes de quem está do outro lado do fio: nada de índice nem `unwrap` que possa derrubar o processo.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use std::{io, sync::Arc, time::Duration};

use snow::{Builder, StatelessTransportState};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    time::timeout,
};

use crate::circuit::Cell;

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

const PARAMS: &str = "Noise_NK_25519_ChaChaPoly_SHA256";
const PROLOGUE: &[u8] = b"clam-link-v1";
const TAG: usize = 16;
/// Tamanho do texto cifrado de uma célula.
const CT_LEN: usize = Cell::WIRE_LEN + TAG;

fn err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// `buf[..n]` sem pânico: `n` vem do `snow` e cabe no buffer, mas o parser não aposta nisso.
fn head(buf: &[u8], n: usize) -> io::Result<&[u8]> {
    buf.get(..n).ok_or_else(|| err("tamanho fora do buffer"))
}

async fn write_msg(wr: &mut OwnedWriteHalf, msg: &[u8]) -> io::Result<()> {
    let mut out = Vec::with_capacity(2 + msg.len());
    out.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    out.extend_from_slice(msg);
    wr.write_all(&out).await
}

async fn read_msg(rd: &mut OwnedReadHalf, max: usize) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    rd.read_exact(&mut len).await?;
    let n = u16::from_be_bytes(len) as usize;
    if n > max {
        return Err(err("mensagem grande demais"));
    }
    let mut buf = vec![0u8; n];
    rd.read_exact(&mut buf).await?;
    Ok(buf)
}

pub struct LinkReader {
    rd: OwnedReadHalf,
    st: Arc<StatelessTransportState>,
    nonce: u64,
}

pub struct LinkWriter {
    wr: OwnedWriteHalf,
    st: Arc<StatelessTransportState>,
    nonce: u64,
}

impl LinkReader {
    /// Lê e decifra a próxima célula. Erro = link inutilizável.
    pub async fn read_cell(&mut self) -> io::Result<Cell> {
        let ct = read_msg(&mut self.rd, CT_LEN).await?;
        if ct.len() != CT_LEN {
            return Err(err("tamanho de quadro inesperado"));
        }
        let mut pt = [0u8; CT_LEN];
        let n = self.st.read_message(self.nonce, &ct, &mut pt).map_err(err)?;
        self.nonce += 1;
        Cell::from_bytes(head(&pt, n)?).map_err(err)
    }
}

impl LinkWriter {
    pub async fn write_cell(&mut self, cell: &Cell) -> io::Result<()> {
        let mut ct = [0u8; CT_LEN];
        let n = self.st.write_message(self.nonce, &cell.to_bytes(), &mut ct).map_err(err)?;
        self.nonce += 1;
        write_msg(&mut self.wr, head(&ct, n)?).await
    }
}

fn finish(rd: OwnedReadHalf, wr: OwnedWriteHalf, hs: snow::HandshakeState) -> io::Result<(LinkReader, LinkWriter)> {
    let st = Arc::new(hs.into_stateless_transport_mode().map_err(err)?);
    Ok((LinkReader { rd, st: st.clone(), nonce: 0 }, LinkWriter { wr, st, nonce: 0 }))
}

/// Lado que disca: autentica o relay pela chave pública `remote`.
pub async fn connect(stream: TcpStream, remote: &[u8; 32]) -> io::Result<(LinkReader, LinkWriter)> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let _ = stream.set_nodelay(true);
        let (mut rd, mut wr) = stream.into_split();
        let mut hs = Builder::new(PARAMS.parse().map_err(err)?)
            .prologue(PROLOGUE)
            .map_err(err)?
            .remote_public_key(remote)
            .map_err(err)?
            .build_initiator()
            .map_err(err)?;
        let mut buf = [0u8; 128];
        let n = hs.write_message(&[], &mut buf).map_err(err)?;
        write_msg(&mut wr, head(&buf, n)?).await?;
        let reply = read_msg(&mut rd, 128).await?;
        hs.read_message(&reply, &mut buf).map_err(err)?;
        finish(rd, wr, hs)
    })
    .await
    .map_err(|_| err("handshake estourou o prazo"))?
}

/// Lado que aceita: prova a posse da chave privada `secret`.
pub async fn accept(stream: TcpStream, secret: &[u8; 32]) -> io::Result<(LinkReader, LinkWriter)> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let _ = stream.set_nodelay(true);
        let (mut rd, mut wr) = stream.into_split();
        let mut hs = Builder::new(PARAMS.parse().map_err(err)?)
            .prologue(PROLOGUE)
            .map_err(err)?
            .local_private_key(secret)
            .map_err(err)?
            .build_responder()
            .map_err(err)?;
        let mut buf = [0u8; 128];
        let first = read_msg(&mut rd, 128).await?;
        hs.read_message(&first, &mut buf).map_err(err)?;
        let n = hs.write_message(&[], &mut buf).map_err(err)?;
        write_msg(&mut wr, head(&buf, n)?).await?;
        finish(rd, wr, hs)
    })
    .await
    .map_err(|_| err("handshake estourou o prazo"))?
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::circuit::Cmd;
    use rand_core::OsRng;
    use tokio::net::TcpListener;
    use x25519_dalek::{PublicKey, StaticSecret};

    async fn pair(client_key: [u8; 32], server_secret: [u8; 32]) -> (io::Result<(LinkReader, LinkWriter)>, io::Result<(LinkReader, LinkWriter)>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            accept(s, &server_secret).await
        });
        let c = connect(TcpStream::connect(addr).await.unwrap(), &client_key).await;
        (c, srv.await.unwrap())
    }

    fn keypair() -> ([u8; 32], [u8; 32]) {
        let s = StaticSecret::random_from_rng(OsRng);
        (s.to_bytes(), PublicKey::from(&s).to_bytes())
    }

    #[tokio::test]
    async fn cells_round_trip_in_both_directions() {
        let (sk, pk) = keypair();
        let (c, s) = pair(pk, sk).await;
        let ((mut cr, mut cw), (mut sr, mut sw)) = (c.unwrap(), s.unwrap());
        let cell = Cell::with_body(7, Cmd::Relay, b"segredo");
        for _ in 0..3 {
            cw.write_cell(&cell).await.unwrap();
            assert_eq!(sr.read_cell().await.unwrap(), cell);
            sw.write_cell(&cell).await.unwrap();
            assert_eq!(cr.read_cell().await.unwrap(), cell);
        }
    }

    #[tokio::test]
    async fn wrong_relay_key_fails_the_handshake() {
        let (sk, _) = keypair();
        let (_, other_pk) = keypair();
        let (c, s) = pair(other_pk, sk).await;
        assert!(c.is_err() || s.is_err());
    }
}
