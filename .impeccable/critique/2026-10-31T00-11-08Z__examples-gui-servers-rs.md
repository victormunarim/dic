---
target: servidores e salas
total_score: 31
max_score: 40
na_heuristics: 
p0_count: 0
p1_count: 0
target_identity: "file:/home/victor/orca/workspaces/dic/clam/examples/gui/servers.rs"
target_fingerprint: "sha256:82b1c3e2301960da3c948c6b515f7d96389d8abf1724e84b759e74ce7bbbf3e3"
target_path: /home/victor/orca/workspaces/dic/clam/examples/gui/servers.rs
timestamp: 2026-10-31T00-11-08Z
slug: examples-gui-servers-rs
---
Method: design review only (detector not applicable: egui, sem marcação web)
Alvo: tela do servidor (servers.rs) após 45ef499. Score 31/40. P0=0 P1=0 P2=3 (+P3).

P2 Convite fica atrás do botão de membros em janela estreita; a pista é só a dica no hover.
P2 Sem atalhos de teclado (trocar de sala, abrir convite, sair da sala).
P2 Emojis restantes (contatos 💬 📞, menu da engrenagem) quebram a linguagem dos ícones desenhados.
P3 Alvos de clique de 28 px nos ícones sem moldura; sem feedback de apertado neles.
P3 Erro de srv.msg longe da ação.
