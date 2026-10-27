# CLAUDE.md

Chamadas de voz e de tela P2P em Rust (crate `clam`), sem servidor nem diretório. Cada pessoa é o relay de si mesma; a mídia anda em circuitos de 2 saltos cifrados em camadas. Visão geral em `README.md`; como tudo funciona em `docs/COMUNICACAO.md`.

## Stack

- Rust, edition 2024 (1.85+). Crate de biblioteca `clam` + exemplos (`examples/gui`, `call`, `measure`).
- Runtime: `tokio`. Rede: TCP próprio (relay na porta 4001), link Noise NK (`snow`), DHT BitTorrent (`mainline`), `libp2p` na camada de presença, UPnP (`igd-next`).
- Criptografia: `chacha20poly1305`, `x25519-dalek`, `ed25519-dalek`, `hkdf`, `hmac`, `sha2`, `subtle`, `zeroize`.
- Mídia: `opus-pure`, `sonora` (AEC/AGC), `nnnoiseless`, `openh264`, `cpal`; captura X11/Wayland (portal)/GDI.
- GUI: `egui`/`eframe`. Chaveiro do sistema: `keyring`.
- Features: `net` (transporte, nó, servidores), `video`, `capture`, `portal`, `audio`, `gui` (inclui `net`). Quase todo teste de rede exige `--features net`.
- Fuzz: `cargo-fuzz` em `fuzz/` (nightly).

## Comandos

```sh
cargo test --features net                         # biblioteca e rede
cargo test --features gui --example gui           # GUI (sem abrir janela)
cargo clippy --features net --all-targets
cargo clippy --features gui --all-targets
cargo check --features audio
cd fuzz && cargo +nightly fuzz run <alvo> -- -max_total_time=60
```

Não rode `cargo fmt` (o estilo do repositório é próprio). A GUI não roda sem display: valide por compilação e testes, e diga isso ao relatar.

## Princípios

### Sempre P2P
- Nenhum servidor, diretório, conta ou serviço central, nem como atalho ou reserva. Reencontro de contatos usa aviso direto, DHT e âncoras que o próprio usuário escolhe (`docs/ENCONTRO.md`).
- Quem atende é contato: chave fora da lista não recebe resposta. O relay de saída entrega só ao telefone local do dono (não é proxy aberto).
- Não crie dependência de infraestrutura que alguém precise operar.

### Segurança
- Leia `docs/SEGURANCA.md` antes de mexer em cripto, link, circuito, convites ou chaveiro; atualize-o quando uma garantia ou limite mudar.
- `unsafe_code = "deny"`. Exceção só em FFI do Windows e openh264, com `#[allow(unsafe_code)]` e `// SAFETY:`.
- Parser de dado da rede nunca usa índice, `unwrap`, `expect` nem aritmética que estoura: `#![deny(clippy::indexing_slicing, clippy::unwrap_used)]` nesses módulos; use `get`, `split_at_checked`, `checked_*`. Entrada de fora é hostil.
- Mutex: `LockExt::lock_unpoisoned`, nunca `lock().unwrap()`.
- Segredos em `zeroize`, comparação em tempo constante (`subtle`), nonces nunca reutilizados, AEAD com dado associado. Segredo nunca vai a log, erro ou arquivo sem permissão restrita (`write_private`).
- Privacidade é o padrão. Perder uma proteção (cover, 2 saltos, IP escondido) exige ação explícita do usuário, aviso na interface e, se expõe o IP, confiança por contato (`path::PathRequest::resolve`). Sem fallback silencioso para caminho menos privado.
- Mudança que reduz anonimato ou aumenta o que um observador vê: diga isso em texto claro ao usuário e documente em `docs/SEGURANCA.md`.

### Testes
- Todo comportamento novo ou corrigido ganha teste, no nível certo: unitário no módulo, de integração em `tests/` (rede em loopback, ponta a ponta).
- Teste não pode ser instável: portas livres reais (`free_port`), sem `sleep` fixo para sincronizar, espera por condição com prazo. Ambiente carregado não pode quebrá-lo.
- Código sans-IO (`RelayNode`, `CircuitManager`) é testado sem rede; o transporte fica fino.
- Código que lê bytes da rede ganha alvo de fuzz em `fuzz/fuzz_targets` e roda ao menos 60 s ao mudar.
- Antes de commitar: testes das features afetadas, `clippy --all-targets` sem aviso novo.

### Desempenho
- Célula fixa de 256 bytes; o tráfego constante é a privacidade, então não troque tamanho ou ritmo sem entender o que um observador passa a ver.
- Caminho quente (célula, pacote de mídia, quadro de vídeo): sem alocação desnecessária, sem lock mantido durante `await` ou I/O, sem log por pacote.
- Medir antes de otimizar (`examples/measure`, testes de ritmo). Orçamento de banda e latência estão em `docs/COMUNICACAO.md` e `docs/CIRCUITO.md`; mantenha-os verdadeiros.
- O ciclo de desenvolvimento usa `opt-level` alto nas dependências (ver `[profile.dev]`); não o reduza.

### Alta coesão, baixo acoplamento
- Um módulo, uma responsabilidade. Protocolo e criptografia (sans-IO) separados de transporte, que fica separado de nó/app, que fica separado da GUI.
- A biblioteca não conhece a GUI. A GUI fala com o nó por `Cmd` e `Shared`, sem alcançar o interior do protocolo.
- Dependência vai em uma direção (GUI → nó → telefone/circuito → link/cripto). Não crie ciclo nem atalho que pule camada.
- Decisão de política (quem é confiável, qual caminho) fica em um lugar (`path.rs`, `Dialer`), não espalhada em `if`s pela interface.
- Prefira tipos pequenos e funções puras; compartilhe estado por `Arc<Mutex<..>>` só quando preciso e atrás de métodos.

### Documentação
- Código, comentários, commits e docs em português do Brasil, com acentuação correta.
- Comentário explica o porquê e o invariante, não o que a linha faz. Item público tem `///`. Siga a densidade e o estilo do arquivo vizinho.
- Mudou comportamento, protocolo, limite ou texto da interface: atualize no mesmo commit o doc correspondente (`docs/`), o `README.md` se o mapa mudou e os textos da GUI. Doc e código não podem divergir.
- Limitação conhecida vai em "Limites conhecidos" do doc, não só na cabeça de quem implementou.

## Fluxo

- Trabalhe direto em `main`; commit pequeno e coerente, mensagem em pt-BR com o porquê. Nunca `git stash` simples.
- Commit termina com `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`.
- Interface retrocompatível: relay de versão antiga existe; se uma mudança quebra com ele, documente.
