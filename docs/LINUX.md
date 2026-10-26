# Clam no Linux

## 1. Instalar

```sh
sudo apt install build-essential pkg-config libasound2-dev   # libasound2-dev só para áudio real
curl https://sh.rustup.rs -sSf | sh
git clone git@github.com:victormunarim/dic.git && cd dic
```

## 2. Cada pessoa

Libere a porta TCP 4001 (seu relay). Abra a janela:

```sh
cargo run --release --features gui,audio --example gui     # sem áudio: --features gui
```

1. *Ficar online* (a identidade já nasce pronta; *Gerar nova* troca por outra).
2. Envie seu **código de contato** (botão *Copiar*; leva a chave pública e o seu relay, que só existe com o nó online) à outra pessoa.
3. Contatos: o código de contato de quem pode ligar para você (os dois lados se adicionam; quem é adicionado aparece sozinho na lista do outro).
4. Para ligar, cole o código do outro em *Ligar para um código* (aba *Início*) ou use o botão *Ligar* do contato (aba *Contatos*). Use fones.

Atrás de roteador: o programa tenta abrir a porta 4001 sozinho por UPnP e avisa o resultado no log. Se não der, redirecione a porta 4001 e preencha o endereço público (`ip:4001`, ou um nome) na aba *Configurações*. A chamada é sempre p2p: sem porta acessível (CGNAT, por exemplo) ninguém consegue ligar para você.
Config em `~/.clam-gui.conf` (contém a chave secreta, modo 0600).
