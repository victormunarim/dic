# Clam no Windows

**Status: não testado em Windows.** Se algo falhar, trate como bug.

## 1. Instalar

- Rust (`rustup`) com `stable-x86_64-pc-windows-msvc` e as Build Tools do Visual Studio (C++).
- Git, e `git clone git@github.com:victormunarim/dic.git`.

## 2. Servidor do diretório

Uma máquina só, acessível pelos dois lados (pode ser a do Linux):

```powershell
cargo run --release --features net --example directory -- 0.0.0.0:7777
```

Libere a porta TCP 7777 (e a 4000, se usar o relay abaixo) no Firewall do Windows.

## 3. Cada pessoa

Libere a porta TCP 4001 (seu relay). Abra a janela:

```powershell
cargo run --release --features gui,audio --example gui
```

1. *Gerar nova* na identidade e envie seu **código de contato** (botão *Copiar*; é a chave pública + `@diretório`) à outra pessoa.
2. Diretório: `IP-DO-SERVIDOR:7777`.
3. Contatos: o código de contato de quem pode ligar para você. Se o campo Diretório estiver vazio, ele é preenchido pelo código.
4. *Ficar online*.
5. Para ligar, cole a código do outro em *Ligar*. Use fones.

Atrás de roteador: com o diretório na internet, o programa tenta abrir a porta 4001 sozinho por UPnP e avisa o resultado no log. Se não der, ou redirecione a porta 4001 e preencha o endereço público (`ip:4001`). A chamada é sempre p2p: sem porta acessível (CGNAT, por exemplo) ninguém consegue ligar para você.
Config em `%USERPROFILE%\.clam-gui.conf` (contém a chave secreta).
O programa pede timers de 1 ms ao Windows (`timeBeginPeriod`) para manter a cadência da voz.

Erro `AddrInUse (10048)`: outra instância já usa a porta 4001; feche-a.

---

## Notas técnicas (resumo do README antigo)

**Status: não testado.** Todo o desenvolvimento e os testes rodaram em Linux. Nada no código depende de Linux, mas ninguém compilou no Windows ainda. Se algo falhar, trate como bug a corrigir.

O que deve funcionar, pelo que o código mostra:

- **Biblioteca, testes e `measure`**: as dependências (`tokio`, `snow`, `x25519-dalek`, `libp2p`) são Rust e têm suporte a Windows. O código usa só TCP e `std::fs`. O único trecho específico de sistema é `#[cfg(unix)]` em `src/reputation.rs`, que ajusta a permissão do arquivo de reputação para `0600`.
- **Permissão do arquivo de reputação**: no Windows esse ajuste não existe, e o arquivo herda as permissões da pasta. Guarde-o em uma pasta do seu usuário (por exemplo `%LOCALAPPDATA%`), não em uma pasta compartilhada.
- **Firewall**: nas máquinas dos relays, libere a porta TCP de escuta no Windows Defender Firewall, senão o `measure` e os clientes não conectam.

Para compilar e testar (PowerShell):

```powershell
rustup default stable-x86_64-pc-windows-msvc   # precisa das Build Tools do Visual Studio (C++)
cargo test --features net
cargo run --release --features net --example measure -- direct 127.0.0.1:9000
```

**Fuzzing no Windows**: use o WSL2. O `cargo-fuzz` (libFuzzer) tem suporte estável em Linux e macOS, não no Windows. Dentro do WSL2 os comandos da seção de fuzzing valem como estão.
