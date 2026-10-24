# Clam no Windows

Guia para compilar, testar e fazer uma chamada de voz no Windows 10/11. Para o outro lado (Linux), veja `docs/LINUX.md`. Os comandos são do PowerShell.

## 1. Pré-requisitos

1. **Rust**: baixe e rode o `rustup-init.exe` em https://rustup.rs. Ele pede as *Visual Studio Build Tools* (carga de trabalho "Desenvolvimento para desktop com C++"); aceite a instalação sugerida.
2. **Git** (opcional, para clonar o repositório).
3. Nenhuma biblioteca de áudio extra: o `cpal` usa o WASAPI do próprio Windows.

Confira em um PowerShell novo:

```powershell
rustc --version
cargo --version
```

Rust 1.85 ou mais novo (o crate usa a edição 2024).

## 2. Compilar e testar

Na pasta do projeto:

```powershell
cargo test --features net
```

Opcional, com microfone e alto-falante reais:

```powershell
cargo test --features audio -- --ignored
```

## 3. Chamada com a janela

A janela cobre as pontas. Os **relays** sobem pelo terminal, com `measure relay`.

### 3.1. Abrir a janela

```powershell
cargo run --release --features gui,audio --example gui
```

Na primeira vez o Windows pode perguntar sobre acesso ao **microfone** (Configurações > Privacidade > Microfone: permita para aplicativos de desktop). Sem `audio` (ou com a caixa desmarcada), cada lado envia um tom de teste. **Use fones**: não há cancelamento de eco.

### 3.2. Sua identidade

Clique em *Gerar nova* e depois em *Copiar* na chave pública; passe-a a quem vai ligar. A chave secreta fica em `%USERPROFILE%\.clam-gui.conf`. Esse arquivo contém a chave secreta e **no Windows não recebe restrição de permissão** automática: não o compartilhe nem o deixe em pasta sincronizada.

### 3.3. Relay B (esta máquina)

O telefone da janela escuta na porta 7000; o relay B entrega a chamada a ele por loopback. Em outro PowerShell, com `IP_WIN` o IP desta máquina visível para o Linux:

```powershell
cargo build --release --features net --example measure
.\target\release\examples\measure.exe relay 0.0.0.0:4001 IP_WIN:4001 contato:9=127.0.0.1:7000
```

Ele imprime uma linha `endereço,onion,link`: é o **Relay B**. Passe-a ao Linux, junto com a chave pública da janela.

### 3.4. Firewall

Em um PowerShell **como administrador**:

```powershell
New-NetFirewallRule -DisplayName "clam-4001" -Direction Inbound -Protocol TCP -LocalPort 4001 -Action Allow
New-NetFirewallRule -DisplayName "clam-7000" -Direction Inbound -Protocol TCP -LocalPort 7000 -Action Allow
```

Para remover depois: `Remove-NetFirewallRule -DisplayName "clam-4001","clam-7000"`.

### 3.5. Atender

Na janela, escolha *Atender*, deixe `0.0.0.0:7000` e cole a chave pública de quem vai ligar (várias, separadas por `;`). Clique em *Atender* e aguarde. O log mostra a chamada quando ela chega. O Linux liga seguindo `docs/LINUX.md` (seção 3.4), usando a sua linha de Relay B.

### 3.6. Ligar (Windows como chamador)

Escolha *Ligar*, cole a chave pública de quem atende, as linhas dos dois relays e a duração. O relay de saída do outro lado precisa mapear `contato:9` para o telefone dele.

## 4. Chamada só por script (sem a janela)

Sobe o telefone e o relay B de uma vez, abre o firewall e imprime o comando do lado Linux. Use no lugar dos passos 3.3 a 3.5 (não rode junto com a janela: ambos usam a porta 7000).

```powershell
# antes, no Linux: scripts/call.sh pub  (chave pública de quem liga)
Set-ExecutionPolicy -Scope Process Bypass
.\scripts\call-windows.ps1 -PublicIp IP_WIN -CallerPub CHAVE_DO_LINUX -OpenFirewall
```

Ctrl+C encerra os processos e remove as regras de firewall. Este script ainda não foi testado em uma máquina Windows real.

## 5. Problemas comuns

- **Testes `profile_switch_changes_link_cadence_live` / `relay_profile_sets_and_switches_link_cadence` falhando** (`standard 64`, `economy 31`): o relógio padrão do Windows tem passo de ~15,6 ms e a cadência de 10/20 ms saía a ~60%. O crate agora pede temporizadores de 1 ms (`clam::timer::fine_timers`, `timeBeginPeriod`) ao criar relay, cliente e telefone. Se ainda falhar, avise com a saída.

- **`link.exe` não encontrado**: faltam as Build Tools do Visual Studio; reinstale pelo `rustup-init.exe`.
- **Script bloqueado** ("execução de scripts desabilitada"): rode `Set-ExecutionPolicy -Scope Process Bypass` na mesma janela.
- **"sem microfone padrão"** ou silêncio: defina o microfone padrão em Configurações > Sistema > Som e permita o acesso em Privacidade.
- **Circuito não fica pronto em 15 s**: confira IPs, firewall (4001 e 7000) e se as linhas dos relays foram coladas inteiras. Redes com isolamento de clientes (Wi-Fi de convidados) bloqueiam o tráfego entre máquinas.
- **Eco ou microfonia**: use fones.
