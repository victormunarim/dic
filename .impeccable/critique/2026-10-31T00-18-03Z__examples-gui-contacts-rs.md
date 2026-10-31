---
target: contatos
total_score: 28
max_score: 40
na_heuristics: 
p0_count: 0
p1_count: 1
target_identity: "file:/home/victor/orca/workspaces/dic/clam/examples/gui/contacts.rs"
target_fingerprint: "sha256:1adffecd26a97523e41c9dc97a016f21027c0699a73aed21995ed534401d9b0e"
target_path: /home/victor/orca/workspaces/dic/clam/examples/gui/contacts.rs
timestamp: 2026-10-31T00-18-03Z
slug: examples-gui-contacts-rs
---
Method: design review only (detector not applicable: egui, sem marcação web)
Alvo: contatos (contacts.rs) após 3313323. Score 28/40. P0=0 P1=1 P2=3 (+P3).

P1 Presença invisível: o avatar da lista não leva ponto de online; só o botão de ligar apagado sugere "offline", e ele também apaga quando você está offline ou em chamada.
P2 Quatro ícones por linha com o mesmo peso (escudo de confiança, lixeira, ligar, bate-papo): destrutivo e privacidade ao lado das ações principais, contra a regra de menu de três pontos.
P2 Estado vazio e primeiro contato: nota solta "Use o + do alto", sem o caminho (onde está o seu código, o que o amigo faz).
P2 Janela de remover: três botões, dois vermelhos de mesmo peso e um parágrafo longo.
P3 Contato pendente aparece com 8 hex como nome.
P3 Linhas reordenam sozinhas ao chegar mensagem nova.
P3 Busca sempre visível mesmo com poucos contatos; comentário de online_contacts ainda cita o 📞.
