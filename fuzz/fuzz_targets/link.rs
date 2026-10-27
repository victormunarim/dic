#![no_main]
//! Link (Noise `NK` sobre TCP). O iniciador é anônimo, então qualquer um completa o handshake com o relay e
//! depois manda o que quiser; o alvo faz esse papel (e o de um relay hostil) num socket de loopback.
//!
//! O primeiro byte escolhe o cenário, o resto é o que o outro lado manda:
//! 0. handshake de verdade e um quadro cifrado válido com esse texto claro: `read_cell` aceita exatamente
//!    quando é uma célula, e a célula reproduz a entrada;
//! 1. handshake de verdade e bytes crus no lugar do quadro: `read_cell` recusa;
//! 2. bytes crus no lugar da primeira mensagem do handshake: `accept` recusa;
//! 3. relay hostil responde ao `connect` com esses bytes: `connect` recusa.
//!
//! Nada pode entrar em pânico nem travar (o socket é fechado depois de escrever, então esperar além de 2 s é bug).

use std::{net::SocketAddr, sync::OnceLock, time::Duration};

use clam::{circuit::Cell, link};
use libfuzzer_sys::fuzz_target;
use snow::{Builder, StatelessTransportState};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    runtime::Runtime,
    time::timeout,
};

const PARAMS: &str = "Noise_NK_25519_ChaChaPoly_SHA256";
const PROLOGUE: &[u8] = b"clam-link-v1";
const LIMIT: Duration = Duration::from_secs(2);
/// O que o fuzzer manda além disso é cortado: o link só lê quadros pequenos.
const MAX_INPUT: usize = 1024;

struct Env {
    rt: Runtime,
    listener: TcpListener,
    addr: SocketAddr,
    secret: [u8; 32],
    public: [u8; 32],
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).expect("porta de loopback");
        let addr = listener.local_addr().expect("endereço");
        let keys = Builder::new(PARAMS.parse().expect("parâmetros")).generate_keypair().expect("chaves");
        Env { rt, listener, addr, secret: keys.private.try_into().expect("32 bytes"), public: keys.public.try_into().expect("32 bytes") }
    })
}

async fn pair(env: &Env) -> (TcpStream, TcpStream) {
    let (c, s) = tokio::join!(TcpStream::connect(env.addr), env.listener.accept());
    (c.expect("conecta"), s.expect("aceita").0)
}

fn framed(msg: &[u8]) -> Vec<u8> {
    let mut v = (msg.len() as u16).to_be_bytes().to_vec();
    v.extend_from_slice(msg);
    v
}

async fn read_framed(s: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    s.read_exact(&mut len).await?;
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    s.read_exact(&mut buf).await?;
    Ok(buf)
}

/// O iniciador do protocolo, escrito à mão: é o que um par anônimo qualquer faria.
async fn initiate(c: &mut TcpStream, public: &[u8; 32]) -> StatelessTransportState {
    let mut hs = Builder::new(PARAMS.parse().expect("parâmetros"))
        .prologue(PROLOGUE)
        .expect("prólogo")
        .remote_public_key(public)
        .expect("chave do relay")
        .build_initiator()
        .expect("iniciador");
    let mut buf = [0u8; 128];
    let n = hs.write_message(&[], &mut buf).expect("primeira mensagem");
    c.write_all(&framed(&buf[..n])).await.expect("envia");
    let reply = read_framed(c).await.expect("resposta do relay legítimo");
    hs.read_message(&reply, &mut buf).expect("resposta válida");
    hs.into_stateless_transport_mode().expect("modo de transporte")
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else { return };
    let rest = rest.get(..MAX_INPUT).unwrap_or(rest);
    let env = env();
    env.rt.block_on(async {
        timeout(LIMIT, scenario(env, mode % 4, rest)).await.expect("o link travou");
    });
});

async fn scenario(env: &Env, mode: u8, rest: &[u8]) {
    let (mut c, s) = pair(env).await;
    match mode {
        0 | 1 => {
            let server = async {
                let (mut rd, _wr) = link::accept(s, &env.secret).await.expect("handshake legítimo");
                rd.read_cell().await
            };
            let client = async {
                let st = initiate(&mut c, &env.public).await;
                let wire = if mode == 0 {
                    let mut ct = vec![0u8; rest.len() + 16];
                    let n = st.write_message(0, rest, &mut ct).expect("cifra");
                    framed(&ct[..n])
                } else {
                    rest.to_vec()
                };
                // O relay pode largar o link antes de ler tudo; o que importa é o que ele concluiu.
                let _ = c.write_all(&wire).await;
                let _ = c.shutdown().await;
            };
            let (got, ()) = tokio::join!(server, client);
            if mode == 0 {
                match (Cell::from_bytes(rest), got) {
                    (Ok(want), Ok(cell)) => assert_eq!(cell, want, "célula lida difere da enviada"),
                    (Err(_), Err(_)) => {}
                    (want, got) => panic!("o link e o parser discordam: parser {want:?}, link {got:?}"),
                }
            } else {
                assert!(got.is_err(), "quadro cru sem autenticação foi aceito");
            }
        }
        2 => {
            let client = async {
                let _ = c.write_all(rest).await;
                let _ = c.shutdown().await;
            };
            let (got, ()) = tokio::join!(link::accept(s, &env.secret), client);
            assert!(got.is_err(), "handshake forjado foi aceito");
        }
        _ => {
            let mut s = s;
            let hostile = async {
                let _ = read_framed(&mut s).await;
                let _ = s.write_all(rest).await;
                let _ = s.shutdown().await;
            };
            let (got, ()) = tokio::join!(link::connect(c, &env.public), hostile);
            assert!(got.is_err(), "resposta forjada do relay foi aceita");
        }
    }
}
