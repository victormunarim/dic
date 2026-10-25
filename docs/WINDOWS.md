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
