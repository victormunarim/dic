---
target: servidores e salas
total_score: 30
max_score: 40
na_heuristics: 
p0_count: 0
p1_count: 1
target_identity: "file:/home/victor/orca/workspaces/dic/clam/examples/gui/servers.rs"
target_fingerprint: "sha256:072449050d039b74bd01328e9f75d9b1f4776e331319fa4af1bd2d63042f218f"
target_path: /home/victor/orca/workspaces/dic/clam/examples/gui/servers.rs
timestamp: 2026-10-31T00-06-43Z
slug: examples-gui-servers-rs
---
Method: design review only (detector not applicable: egui, sem marcação web)
Alvo: tela do servidor (servers.rs) após ceb2f3c. Score 30/40. P0=0 P1=1 P2=3 (+P3).

P1 Ícones desenhados (dots, lixeira, sair, desligar, monitor, lupa, X, +, fantasma, alternar membros) sem indicador de foco do teclado; só o trilho tem aro, e o aro tem 2,74:1 no escuro.
P2 hangup_button (círculo com X) serve a quatro ações: sair da sala, desligar, recusar, parar de assistir.
P2 Em janela estreita o convite fica atrás do botão de membros, sem pista de que existe.
P2 Estado do botão de membros (aberto/fechado) só por cor.
P3 Erro de srv.msg longe da ação; sem atalhos de teclado.
