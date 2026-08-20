# Alterações deste fork — o que não pode se perder num merge

Este arquivo existe para uma coisa só: quando o upstream (`mackswendhell/open-flow`)
lançar versão nova e formos mergear, conferir esta lista antes de dar o merge por
concluído. Já aconteceu de uma correção sumir sem ninguém perceber.

**Regra de ouro:** depois de todo merge, rodar a suíte. Cada item abaixo tem teste
que falha se a alteração for desfeita. Suíte verde = nada se perdeu.

```bash
cd app/src-tauri
cargo test --lib --release --no-run
cp testes.manifest "target/release/deps/<binario>.exe.manifest"
./target/release/deps/<binario>.exe
```

O manifesto é obrigatório no Windows: sem ele o binário de teste morre no
carregamento com `STATUS_ENTRYPOINT_NOT_FOUND`, porque o comctl32 5.82 não
exporta `TaskDialogIndirect`. O `app.exe` real recebe manifesto do `tauri-build`;
o de teste, não.

---

## 1. Áudio reamostrado para 16 kHz antes de subir

**Onde:** `wav_bytes()` e `resample()` em `lib.rs`, constante `STT_RATE`.

**Por quê:** o Whisper trabalha internamente a 16 kHz e reamostra tudo que chega.
Enviar os 48 kHz do microfone é subir 3x mais bytes para o servidor descartar dois
terços, sem ganho de qualidade. O upstream grava o WAV na taxa nativa do
dispositivo, sem reamostrar.

**O que quebra sem isso:** o teto de 25 MB da Groq (free tier) cai de ~13,7 min
para ~4,6 min de fala. Já falhou de verdade no Mac; no Windows um ditado de 243 s
gerou 22,3 MB — 89% do limite, passou raspando.

| | 48 kHz (upstream) | 16 kHz (aqui) |
|---|---|---|
| taxa | 94 KB/s | 31 KB/s |
| teto de 25 MB | 4,6 min | 13,7 min |
| ditado de 243 s | 22,3 MB | 7,4 MB |

Razão inteira (48000/16000 = 3) usa média por blocos, que decima e filtra ao mesmo
tempo, evitando aliasing. Razão quebrada cai na interpolação linear.

**Testes:** `audio_e_reamostrado_para_16khz_antes_de_subir`,
`taxa_igual_ou_menor_que_16khz_passa_intacta`, `reamostragem_preserva_o_sinal`.

---

## 2. Atalhos independentes

**Onde:** `Binding`, `Act`, `build_bindings()`, `handle_key()`, `Shared.bindings`.

O upstream tem um atalho só, com um campo `mode` escolhendo entre segurar e
apertar. Aqui cada atalho carrega o próprio comportamento:

- `hotkey` — segurar e falar
- `hotkey_toggle` — apertar para começar, apertar para parar
- `hotkey_cycle_profile` — avança o perfil ativo e mostra o nome na tela
- `Profile.hotkey` + `Profile.hotkey_mode` — atalho por perfil, que dita naquele
  estilo **sem** trocar o perfil ativo

A lista é ordenada do mais específico para o mais genérico, senão `Ctrl+Win`
dispara junto com `Ctrl+Win+J`. Enquanto um atalho está pressionado os demais são
ignorados, e só o vínculo que iniciou uma gravação pode encerrá-la.

**Cuidado no merge:** o `handle_key` do upstream mexe em `st.recording` como
`bool`; aqui é `Option<String>`. O cancelamento por Esc dele foi adaptado — zera
`recording` mas não mexe em `latched`, então o Release seguinte não dispara Stop.

**Testes:** `atalho_de_perfil_dita_naquele_estilo_sem_trocar_o_ativo`,
`atalho_mais_especifico_ganha_do_mais_curto`, `atalho_duplicado_nao_cria_dois_vinculos`,
`atalho_invalido_e_descartado_em_vez_de_virar_outra_tecla`,
`migracao_do_modo_toggle_preserva_o_comportamento`, mais os 3 de cancelamento do
upstream, que continuam passando sem alteração.

---

## 3. `key_from_name` devolve `Option`

**Onde:** `platform/windows.rs` e `platform/macos.rs`.

O upstream cobre ~15 teclas e devolve **F9 para qualquer nome desconhecido**. Um
atalho `Ctrl+Alt+P` viraria `Ctrl+Alt+F9` em silêncio. Aqui a tabela cobre letras,
dígitos, F1–F24, numpad, setas e navegação, e nome desconhecido devolve `None` —
o atalho inteiro é descartado.

**Cuidado no merge:** o `parse_cancel_key()` do upstream chama essa função
esperando `u32`. Com `Option`, tecla desconhecida desliga o cancelamento em vez de
virar F9.

---

## 4. Aba Provedores

**Onde:** `Provider`, `providers`, `rewrite_openai()`, `transcribe_openai()`,
`test_provider()`, `ProvidersTab` e `ModelPicker` no `App.tsx`.

Qualquer serviço compatível com a API da OpenAI, cadastrado pela interface:
endereço base, chave e botão de testar. Vale para transcrição e para reescrita.
Verificado contra Groq, OpenRouter, OmniRoute e o Gemini pelo endpoint
`/v1beta/openai`.

Três detalhes que só apareceram testando contra os serviços reais:

- **OmniRoute responde SSE por padrão.** Sem `stream: false` e
  `Accept: application/json` o corpo deixa de ser JSON e a reescrita quebra.
- **Gemini não publica catálogo:** `/models` devolve 404 mesmo com o chat
  funcionando. A interface trata como "conectado, sem catálogo".
- **Modelos de transcrição vêm do metadado**, não do nome: `output_modalities`
  contendo `transcription` (Groq) ou `type: audio` + `subtype: transcription`
  (OmniRoute). Deliberadamente **não** se usa "aceita áudio na entrada" — no
  OpenRouter isso arrastaria 25 modelos de chat multimodal que não atendem em
  `/audio/transcriptions`.

**Testes:** `migracao_transforma_chaves_existentes_em_provedores`,
`migracao_nao_mexe_em_quem_ja_tem_provedores`, `migracao_aponta_a_transcricao_para_o_provedor_groq`,
`quem_transcreve_no_local_nao_e_movido_para_a_nuvem`,
`url_do_provedor_tolera_barra_sobrando_ou_faltando`, `bloco_de_raciocinio_e_descartado`.

---

## 5. Fallback de modelo também no caminho genérico

**Onde:** `rewrite_openai()`, o bloco que testa `404`/`503`.

O fallback é do upstream (v0.1.14) e vive no `rewrite_gemini()`. Só que quem usa
a aba Provedores **não passa por ali** — vai pelo caminho genérico. Sem estender,
o remédio não valeria justamente para quem mais precisou dele.

Restrito ao `base_url` do Gemini: nos demais provedores o modelo de reserva não
existiria.

**Motivo de existir:** em 13/08 o `gemini-2.5-flash` foi descontinuado e passou a
responder 404. O app caía no texto bruto sem avisar, e a reescrita ficou dias
desligada sem ninguém perceber.

---

## 6. Cliente HTTP único

**Onde:** `cloud_client()`.

O upstream cria um cliente por chamada, pagando handshake TCP+TLS a cada ditado.
Aqui é um só, reaproveitado, com `connect_timeout` de 5 s.

Tem um `local_address` fixando IPv4 que **não** resolveu o que eu pensava — foi
adicionado sob hipótese errada de que o IPv6 causava travamento. A lentidão era do
free tier do Gemini. Se atrapalhar em outra rede, pode sair sem prejuízo.

---

## 7. Balão de perfil e diagnóstico de colagem

**Onde:** janela `toast` no `tauri.conf.json` (registrada em
`capabilities/default.json` — janela fora dali tem os eventos negados em
silêncio), `toast_show()`, `Toast.tsx`, e o log em `insert.log`.

O `insert.log` grava janela alvo, modificadores ainda pressionados e resultado do
`paste_shortcut`. Foi o que permitiu distinguir "o Ctrl+V falhou" de "a janela
recusou a colagem".

---

## Procedimento de merge

1. `git fetch --tags origin` e ver o que mudou: `git log --oneline HEAD..origin/main`
2. Criar branch de retorno: `git branch -f backup-antes-merge-<versao> HEAD`
3. Fazer backup do que funciona hoje (`open-flow-instalar/backup-hoje.cmd`, via
   tarefa agendada — escrita direta em `%APPDATA%` é redirecionada pelo sandbox)
4. `git merge origin/main --no-commit` e resolver preservando os dois lados
5. `cargo check` e `npx tsc --noEmit`
6. Rodar a suíte com o manifesto (ver topo deste arquivo)
7. Conferir esta lista, item por item
8. Build **só** pelo CLI do Tauri: `npm run tauri build -- --no-bundle`.
   Rodar `cargo build --release` depois sobrescreve o exe sem os assets do
   frontend e o app abre com `ERR_CONNECTION_REFUSED`. Conferir tamanho (~14,3 MB)
   e a presença dos assets do Vite dentro do binário.
9. Instalar por tarefa agendada (`trocar-exe.cmd`), nunca copiando daqui
