# Product

<!-- impeccable:product-schema 1 -->

## Platform

desktop

Aplicativo nativo em egui/eframe (Linux, Windows; captura X11, Wayland via portal e GDI). Não é web, iOS nem Android: as referências de plataforma móvel não se aplicam.

## Users
Duas frentes, confirmadas pelo dono do projeto:
- Pessoas que querem se divertir jogando online com os amigos: voz e compartilhamento de tela em salas, sem criar conta.
- Pessoas que se preocupam com anonimato: não querem que um servidor, diretório ou terceiro veja quem fala com quem nem o IP.

## Product Purpose
Chamadas de voz e de tela P2P, sem servidor nem diretório. Cada pessoa é o relay de si mesma; a mídia anda em circuitos de 2 saltos cifrados em camadas. Sucesso: ligar para um contato ou entrar numa sala e ser ouvido com boa qualidade, sem depender de infraestrutura que alguém opere e sem perder a privacidade por padrão.

## Positioning
Um app de chamadas no estilo do Discord (trilho de servidores, salas, contatos) que de fato não tem servidor: nenhum concorrente centralizado pode copiar a ausência de conta, de diretório e de ponto central.

## Operating Context
Uso doméstico em janela larga (mínimo 820×560), em chamada longa com áudio e tela ao mesmo tempo. O reencontro de contatos usa aviso direto, DHT e âncoras escolhidas pelo usuário. Quem atende é contato: chave fora da lista não recebe resposta.

## Capabilities and Constraints
- Rust, edition 2024; GUI em `examples/gui` (egui), biblioteca `clam` sem conhecer a GUI; GUI fala com o nó por `Cmd` e `Shared`.
- Privacidade é o padrão; perder proteção (cover, 2 saltos, IP escondido) exige ação explícita e aviso na interface. Sem fallback silencioso para caminho menos privado.
- Nenhum servidor, conta ou serviço central, nem como atalho.
- Texto da interface, comentários, commits e docs em português do Brasil, com acentuação correta.
- A GUI não roda sem display: validação por compilação e testes.

## Brand Commitments
Visual neomórfico (soft UI): uma cor base, relevo por duas sombras opostas, três estados (elevado, afundado, plano), um único acento, vermelho só para o destrutivo e verde só para "ao vivo/ok". Interface minimalista e centrada em ícones, desenhados com o `painter` quando a fonte pode não ter o glifo; explicação no hover. Temas escuro (padrão) e claro. Helpers e cores só em `examples/gui/theme.rs`. Nenhum outro compromisso de marca foi declarado.

## Evidence on Hand
Documentação técnica em `docs/` (COMUNICACAO, CIRCUITO, SEGURANCA, GUI, ENCONTRO, SERVIDORES, CHAMADAS) e `README.md`. Não há depoimentos, clientes, métricas de uso nem identidade visual externa (logo, nome registrado): não inventar.

## Product Principles
1. Privacidade por padrão; abrir mão dela é escolha explícita e visível.
2. Sem infraestrutura de ninguém: nada que alguém precise operar.
3. Ícone antes de texto; o que não é óbvio explica no hover.
4. Contraste legível (WCAG AA) vence o efeito de relevo; estado nunca depende só da sombra.
5. Um lugar para o visual (`theme.rs`); as telas só chamam os helpers.

## Accessibility & Inclusion
Contraste WCAG AA obrigatório nos dois temas (texto 4,5:1; ícones e cores de estado 3:1), verificado por teste. Estado importante (ativo, desativado, erro, quem fala) combina cor, ícone ou rótulo, nunca só sombra. Controle desativado fica opaco e sem relevo.
