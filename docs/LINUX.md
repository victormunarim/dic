# Clam no Linux

## 1. Instalar

```sh
sudo apt install build-essential pkg-config libasound2-dev   # libasound2-dev só para áudio real
curl https://sh.rustup.rs -sSf | sh
git clone git@github.com:victormunarim/dic.git && cd dic
```

## 2. Servidor do diretório (uma máquina só, acessível pelos dois lados)

```sh
cargo run --release --features net --example directory -- 0.0.0.0:7777
```

Libere a porta TCP 7777. O diretório só guarda "chave pública → relay" por 120 s.

## 3. Cada pessoa

Libere a porta TCP 4001 (seu relay). Abra a janela:

```sh
cargo run --release --features gui,audio --example gui     # sem áudio: --features gui
```

1. *Gerar nova* na identidade e envie seu **código de contato** (botão *Copiar*; é a chave pública + `@diretório`) à outra pessoa.
2. Diretório: `IP-DO-SERVIDOR:7777`.
3. Contatos: o código de contato de quem pode ligar para você. Se o campo Diretório estiver vazio, ele é preenchido pelo código.
4. *Ficar online*.
5. Para ligar, cole a código do outro em *Ligar*. Use fones.

Atrás de roteador: com o diretório na internet, o programa tenta abrir a porta 4001 sozinho por UPnP e avisa o resultado no log. Se não der, ou redirecione a porta 4001 e preencha o endereço público (`ip:4001`). A chamada é sempre p2p: sem porta acessível (CGNAT, por exemplo) ninguém consegue ligar para você.
Config em `~/.clam-gui.conf` (contém a chave secreta, modo 0600).
