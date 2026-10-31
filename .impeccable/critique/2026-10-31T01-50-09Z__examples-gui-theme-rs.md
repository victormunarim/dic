---
target: botões
total_score: 29
max_score: 40
na_heuristics: 
p0_count: 1
p1_count: 1
target_identity: "file:/home/victor/orca/workspaces/dic/clam/examples/gui/theme.rs"
target_fingerprint: "sha256:1528c250997eb1df36185a7cd6bb5a518370eb236682e1243ac8d3d43d557259"
target_path: /home/victor/orca/workspaces/dic/clam/examples/gui/theme.rs
timestamp: 2026-10-31T01-50-09Z
slug: examples-gui-theme-rs
---
# Crítica de Botões, 1ª rodada (29/40)

Method: design review only (detector não se aplica: GUI egui nativa, sem display).

P0: botões neutros (`button(.., pal().bg)`: Cancelar, Recusar, Desbloquear, Ficar offline, Ver o meu código) usam texto branco fixo; no tema claro o contraste é ~1,2:1. O teste de AA só cobre acento, verde e vermelho.
P1: seis botões de ícone sem moldura (mais, fechar, lupa, desligar, monitor, parar de assistir) e o fone não têm nome acessível; só o hover, que o leitor de tela não lê.
P2: verde em ações que não são "ao vivo" (Criar, Aceitar, Copiar o convite); "Copiar" ora azul, ora verde. Dois `ui.button` crus (media.rs, ui.rs). O mesmo círculo com X serve para recusar, desligar, sair da sala e sair da tela cheia.
P3: três xis diferentes; `icon_button` morto; botão desligado sem motivo na tela.
