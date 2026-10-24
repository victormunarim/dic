# Clam no Linux

Guia para compilar, testar e fazer uma chamada de voz no Linux. Para o outro lado (Windows), veja `docs/WINDOWS.md`.

## 1. Pré-requisitos

- Rust estável (instale com `rustup`: https://rustup.rs). O crate usa a edição 2024, então precisa do Rust 1.85 ou mais novo.
- Compilador C e linker (`build-essential` no Debian/Ubuntu).
- Para a janela (feature `gui`): normalmente nada além do que o desktop já tem (X11 ou Wayland; as bibliotecas são carregadas em tempo de execução).
- Para microfone e alto-falante (feature `audio`): `pkg-config` e os cabeçalhos do ALSA.

```bash
# Debian/Ubuntu
sudo apt install build-essential pkg-config libasound2-dev
# Fedora
sudo dnf install gcc pkgconf-pkg-config alsa-lib-devel
# Arch
sudo pacman -S base-devel pkgconf alsa-lib
```

Em máquinas com PipeWire ou PulseAudio, o ALSA já encaminha para eles; nada extra é preciso.

## 2. Compilar e testar

```bash
cargo test                   # só o núcleo (sem rede)
cargo test --features net    # tudo, incluindo transporte, circuito e telefone
cargo test --features audio -- --ignored   # opcional: abre o microfone e o alto-falante reais
```

## 3. Chamada com a janela

A janela cobre as pontas (quem liga e quem atende). Os **relays** ainda sobem pelo terminal, com `measure relay`.

### 3.1. Abrir a janela

```bash
cargo run --release --features gui,audio --example gui
```

Sem `audio` (ou com a caixa desmarcada), cada lado envia um tom de teste (440 Hz quem liga, 660 Hz quem atende). **Use fones** quando usar o microfone: não há cancelamento de eco.

### 3.2. Sua identidade

Na janela, clique em *Gerar nova* e depois em *Copiar* na sua chave pública. Passe essa chave pública a quem vai atender. A chave secreta fica em `~/.clam-gui.conf` (permissão 0600); não a compartilhe.

### 3.3. Relay A (esta máquina)

Em outro terminal, com `IP_LINUX` o endereço desta máquina visível para o Windows e `IP_WIN:7000` o telefone do outro lado:

```bash
cargo build --release --features net --example measure
target/release/examples/measure relay 0.0.0.0:4001 IP_LINUX:4001 contato:9=IP_WIN:7000
```

Ele imprime uma linha `endereço,onion,link`: é o **Relay A** da janela. Libere a porta TCP 4001 no firewall (`sudo ufw allow 4001/tcp`, se usar o ufw).

### 3.4. Ligar

No Windows, o outro lado sobe o Relay B e o telefone (veja `docs/WINDOWS.md`) e lhe passa a linha do Relay B e a chave pública do telefone. Na janela, escolha *Ligar* e preencha:

- chave pública de quem vai atender;
- Relay A (a linha do passo 3.3) e Relay B (a que o Windows passou);
- duração em segundos.

Clique em *Ligar*. O log mostra, por segundo, pacotes recebidos, escondidos e o atraso. *Desligar* encerra.

### 3.5. Atender (Linux como destino)

Escolha *Atender*, informe `0.0.0.0:7000` e cole a chave pública de quem pode ligar (várias, separadas por `;`). O relay de saída do outro lado precisa mapear `contato:9=IP_LINUX:7000`. Libere a porta 7000.

## 4. Chamada só por scripts (sem a janela)

```bash
scripts/call.sh pub          # imprime a sua chave pública
scripts/call.sh dial IP_LINUX IP_WIN 'LINHA_RELAY_B' PUB_DO_TELEFONE [segundos] [porta-do-telefone]
```

O script sobe o relay A, liga com tom de teste e imprime as estatísticas. O lado Windows usa `scripts/call-windows.ps1`.

## 5. Problemas comuns

- **`pkg-config` não encontrado** ao compilar com `audio`: instale `pkg-config` e `libasound2-dev`.
- **"sem microfone padrão"**: configure um dispositivo de entrada padrão no sistema (`pavucontrol` ou as configurações de som).
- **Circuito não fica pronto em 15 s**: confira IPs, firewall (portas 4001 e 7000) e se as linhas dos relays foram coladas inteiras.
- **Ninguém atende**: o telefone só toca para as chaves públicas listadas; chamadas de outras chaves são ignoradas sem resposta.
- **Eco ou microfonia**: use fones.
