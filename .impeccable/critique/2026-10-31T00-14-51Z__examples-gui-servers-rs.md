---
target: servidores e salas
total_score: 33
max_score: 40
na_heuristics: 
p0_count: 0
p1_count: 0
target_identity: "file:/home/victor/orca/workspaces/dic/clam/examples/gui/servers.rs"
target_fingerprint: "sha256:b8964b5ae292aa88a8b85e169d910594d9c3c2e3ac9f992626d286d1c0770481"
target_path: /home/victor/orca/workspaces/dic/clam/examples/gui/servers.rs
timestamp: 2026-10-31T00-14-51Z
slug: examples-gui-servers-rs
---
Method: design review only (detector not applicable: egui, sem marcação web)
Alvo: tela do servidor (servers.rs) após 9619272. Score 33/40. P0=0 P1=0 P2=2 (+P3).

P2 Erros e avisos (srv.msg/srv.info) aparecem no topo da aba, longe do cartão de convite e das ações que os geraram.
P2 Atalhos só aparecem na dica de cada botão; sem lista única nem pista para quem usa só o teclado.
P3 Alvos de clique de 28x34 nos ícones sem moldura; sem feedback de apertado.
P3 Itens de menu e de sala usam preenchimento plano para o selecionado, fora do vocabulário afundado/elevado.
P3 O ponto "●" ainda é glifo de fonte (estado ao vivo, Chamada ●).
