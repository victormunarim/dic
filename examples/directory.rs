//! Servidor do diretório de primeiro contato. Roda em uma máquina alcançável pelos dois lados.
//!
//! ```text
//! directory [escuta]                              # só diretório (padrão 0.0.0.0:7777)
//! directory [escuta] --relay <escuta> <ip:porta>  # também relay para quem está atrás de NAT
//! ```
//! Exemplo: `directory 0.0.0.0:7777 --relay 0.0.0.0:4000 203.0.113.5:4000`.
//! Guarda só "chave pública -> relay" em memória, por 120 s. Com `--relay`, quem usa
//! "atrás de NAT" liga a este servidor por uma conexão de saída e o relay daqui entrega as chamadas;
//! o servidor então vê o IP e o tráfego (cifrado de ponta a ponta) desses nós.

use clam::directory::{DirectoryServer, HostConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let listen = args.first().filter(|a| *a != "--relay").cloned().unwrap_or_else(|| "0.0.0.0:7777".into());
    let host = match args.iter().position(|a| a == "--relay") {
        Some(i) => Some(HostConfig {
            relay_listen: args.get(i + 1).ok_or("--relay precisa de <escuta> <ip:porta público>")?.parse()?,
            relay_public: Some(args.get(i + 2).ok_or("--relay precisa de <escuta> <ip:porta público>")?.clone()),
        }),
        None => None,
    };
    let server = DirectoryServer::spawn_with(listen.parse()?, host).await?;
    eprintln!("diretório ouvindo em {}", server.local_addr());
    if let Some(r) = server.relay_addr() {
        eprintln!("relay ouvindo em {r}");
    }
    eprintln!("Ctrl-C encerra");
    std::future::pending::<()>().await;
    Ok(())
}
