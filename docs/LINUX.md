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

1. *Gerar nova* na identidade e envie sua **chave pública** à outra pessoa.
2. Diretório: `IP-DO-SERVIDOR:7777`.
3. Contatos: a chave pública de quem pode ligar para você.
4. *Ficar online*.
5. Para ligar, cole a chave pública do outro em *Ligar*. Use fones.

Atrás de NAT: redirecione a porta 4001 e preencha o endereço público (`ip:4001`).
Config em `~/.clam-gui.conf` (contém a chave secreta, modo 0600).
