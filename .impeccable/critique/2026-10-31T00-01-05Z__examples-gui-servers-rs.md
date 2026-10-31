---
target: servidores e salas
total_score: 28
max_score: 40
na_heuristics: 
p0_count: 0
p1_count: 2
target_identity: "file:/home/victor/orca/workspaces/dic/clam/examples/gui/servers.rs"
target_fingerprint: "sha256:747af0258d9ae8b159dc1efbd19cbbd525c2d36447eb27bfa06f7e49953f1785"
target_path: /home/victor/orca/workspaces/dic/clam/examples/gui/servers.rs
timestamp: 2026-10-31T00-01-05Z
slug: examples-gui-servers-rs
---
Method: design review only (detector not applicable: egui, sem marcação web)
Alvo: tela do servidor (servers.rs): rooms_panel, room_list, room_chat, members_panel, invite_card, screen_cover.
Score 28/40 (todas aplicáveis). P0=0 P1=2 P2=3.

P1 Largura: 76+300+340=716 px de colunas; no mínimo 820 o bate-papo central fica com ~104 px.
P1 Emoji como ícone (🔊 📞 🔍 ✖) em servers.rs, contra o CLAUDE.md; mistura com ícones desenhados.
P2 screen_cover e screens_card usam botões de texto padrão do egui e parágrafo fixo, fora do tema.
P2 Convite: dois parágrafos fixos e "Gerar só o texto" visíveis; ação rara deveria ficar recolhida.
P2 Cartões fantasmas (nova sala, convidar) com + a 0,6 de opacidade: afordância fraca, abaixo de 3:1.
