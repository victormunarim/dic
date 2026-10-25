//! Servidor do diretório de primeiro contato. Roda em uma máquina alcançável pelos dois lados.
//!
//! ```text
//! directory [escuta] [estado]    # padrão 0.0.0.0:7777 (TCP e UDP, a mesma porta); estado: arquivo opcional
//! ```
//! Guarda "chave pública -> relay" e as salas por 120 s. Com `estado`, grava isso em disco a cada 5 s e
//! relê ao subir, então um reinício não esvazia as salas. O UDP serve ao furo de NAT: o servidor
//! diz a cada nó o endereço externo dele e coordena o furo entre quem chama e quem atende.

use clam::directory::DirectoryServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listen = std::env::args().nth(1).unwrap_or_else(|| "0.0.0.0:7777".into());
    let server = DirectoryServer::spawn_with(listen.parse()?, Default::default(), std::env::args().nth(2).map(Into::into)).await?;
    eprintln!("diretório ouvindo em {}", server.local_addr());
    eprintln!("Ctrl-C encerra");
    std::future::pending::<()>().await;
    Ok(())
}
