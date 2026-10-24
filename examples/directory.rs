//! Servidor do diretório de primeiro contato. Roda em uma máquina alcançável pelos dois lados.
//!
//! ```text
//! cargo run --release --features net --example directory -- 0.0.0.0:7777
//! ```
//! Guarda só "chave pública -> relay" em memória, por 120 s. Não vê o conteúdo das chamadas.

use clam::directory::DirectoryServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listen = std::env::args().nth(1).unwrap_or_else(|| "0.0.0.0:7777".into());
    let server = DirectoryServer::spawn(listen.parse()?).await?;
    eprintln!("diretório ouvindo em {}; Ctrl-C encerra", server.local_addr());
    std::future::pending::<()>().await;
    Ok(())
}
