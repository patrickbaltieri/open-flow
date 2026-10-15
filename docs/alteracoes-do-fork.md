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

**Manter alinhado com o upstream.** A regra de quando travar a sessão é dele
(`erro_e_definitivo`): só `404` rebaixa, porque significa que a chave não tem o
modelo. `429` e `503` tentam o reserva mas **não** travam — são passageiros, e
travar neles condenaria o resto da sessão por um pico. Quando ele mexer nessa
regra, replicar aqui.

---

## 6. Teto curto e retentativa também no caminho genérico

**Onde:** `rewrite_client()` e `rewrite_openai()` / `rewrite_openai_uma_vez()`.

Mesma razão do item 5: o teto de 4 s com segunda tentativa que o upstream pôs na
v0.1.15 vive no `call_gemini`, e provedor da aba Provedores não passa por ali —
ficava nos 30 s do `cloud_client`.

**Por que dois clientes:** o `cloud_client` continua em 30 s porque a transcrição
sobe o WAV e precisa do tempo. Reescrita é só texto.

**Medição que justifica:** 10 chamadas ao Gemini com a mesma entrada deram
`timeout(35s), 943, 1002, 19548, 643` e `746, 18298, 974, 642, 649` ms. Sete de
dez abaixo de 1,1 s; o resto é travada isolada do free tier, não chamada
legitimamente longa. Uma segunda tentativa pega outra fila.

O erro de timeout carrega um marcador interno `__TIMEOUT__` que o wrapper troca
por texto legível — se vazar, vira ruído no overlay.

**Testes:** `marcador_de_timeout_nao_chega_ao_usuario`,
`teto_da_reescrita_e_curto_o_bastante_para_retentativa`.

---

## 7. Teto por provedor e aviso de reescrita que falhou

**Onde:** `Provider.timeout_secs` / `Provider::timeout()`, campo na aba Provedores,
e o `aviso` no `spawn_pipeline`.

**Teto por provedor:** a latência é característica de quem responde, não do app.
Medido: Groq e Gemini abaixo de 1,1 s; `codex/gpt-5.6-terra-medium` pelo OmniRoute
leva 4,6 a 5,6 s. Um teto único condenaria um dos dois — com 4 s o terra estourava,
retentava e estourava de novo, gravando 8,00 s cravados no histórico e colando
texto bruto. `0` (ou ausente) usa o padrão; o teto vai por requisição, não no
cliente.

**Aviso de falha:** reescrita que falha não aborta o ditado — o texto bruto do
Whisper ainda serve. Mas falhar em silêncio já custou dias **duas vezes**: modelo
descontinuado (`gemini-2.5-flash`, 404) e modelo que sumiu da conta
(`llama-3.3-70b-versatile`, 404). Nos dois casos o único sintoma era "o texto saiu
sem correção". Agora o motivo aparece no balão depois da colagem, passando pelo
`friendly_error` do upstream.

**Testes:** `teto_por_provedor_sobrepoe_o_padrao`,
`teto_de_doze_segundos_cobre_o_gateway_lento`.

---

## 8. Cliente HTTP único

**Onde:** `cloud_client()`.

O upstream cria um cliente por chamada, pagando handshake TCP+TLS a cada ditado.
Aqui é um só, reaproveitado, com `connect_timeout` de 5 s.

Tem um `local_address` fixando IPv4 que **não** resolveu o que eu pensava — foi
adicionado sob hipótese errada de que o IPv6 causava travamento. A lentidão era do
free tier do Gemini. Se atrapalhar em outra rede, pode sair sem prejuízo.

---

## 9. Balão de perfil e diagnóstico de colagem

**Onde:** janela `toast` no `tauri.conf.json` (registrada em
`capabilities/default.json` — janela fora dali tem os eventos negados em
silêncio), `toast_show()`, `Toast.tsx`, e o log em `insert.log`.

O `insert.log` grava janela alvo, modificadores ainda pressionados e resultado do
`paste_shortcut`. Foi o que permitiu distinguir "o Ctrl+V falhou" de "a janela
recusou a colagem".

---

## 10. Provedor de reescrita por perfil

**Onde:** `Profile.rewrite_provider_name` / `Profile.rewrite_model`,
`alvo_do_perfil()`, o `<details class="profile-rewrite">` no `ProfilesTab`.

O upstream tem uma escolha só de reescrita, global. Aqui cada perfil pode
apontar para outro provedor e outro modelo; vazio usa o global, então
configuração antiga continua valendo sem migração.

**Por que existe.** Qualidade e latência estão em troca direta, e o ponto de
equilíbrio muda por perfil. Medido em 07/10/2026 no mesmo ditado clínico real
(2.872 caracteres de transcrição, prompt de 9.154 com o estilo do perfil):

| Provedor / modelo | Tempo | Fidelidade ao que foi falado |
|---|---|---|
| Groq `openai/gpt-oss-120b` | **8,4 s** | 2 de 5 checagens |
| OpenRouter `openai/gpt-5.6-luna` | 11,6 s | **5 de 5** |
| OpenRouter `openai/gpt-5.6-terra` | 12,4 s | **5 de 5** |
| OpenRouter `openai/gpt-6.1-sol` | 16,8 s | 5 de 5, mas estrutura pior |
| Codex CLI `gpt-5.6-terra` (assinatura) | 33,3 s | 5 de 5 |

A falha da Groq não é de estilo. Ela escreveu **"Desobturção completa da
restauração antiga"** onde a fala dizia "remoção completa da resina antiga" —
desobturação é de canal radicular, não de restauração, e a palavra saiu sem o
"a". Também omitiu a queixa que foi dita ("nenhuma queixa maior do paciente") e
a ressalva das trincas ("sem relação com bolsa periodontal no momento"), e
injeta hífen não separável (U+2011) em "guta‑percha", caractere invisível que
viaja para o prontuário.

Nenhum dos dois extremos serve para tudo: anotação que vai para prontuário
aceita esperar 12 s por um texto sem erro clínico; ditado corrido não aceita, e
a Groq resolve.

**Cuidado:** o teto é por **provedor** (item 7), não por perfil. Apontar um
perfil para um provedor lento sem subir o `timeout_secs` dele reproduz o
problema do gateway: estoura, retenta, estoura, e cola texto bruto.

**Separada do `rewrite` de propósito:** `alvo_do_perfil()` só decide quem
chamar, sem tocar a rede, e por isso é testável. `Provider` **não** deriva
`Debug` — carrega a chave de API, e um `unwrap_err` num teste a imprimiria.

**Testes:** `perfil_com_provedor_proprio_ganha_do_global`,
`perfil_sem_escolha_ou_inexistente_usa_o_global`,
`escolha_quebrada_no_perfil_explica_o_motivo`.

**Medições em** `open-flow-instalar/comparar-reescrita.txt` e
`testar-omni-terra.txt`; scripts ao lado, reprodutíveis por tarefa agendada.

---

## 11. Conexão com o ChatGPT pela assinatura (Sign in with ChatGPT)

**Onde:** `siwc.rs` inteiro, `ProviderKind::ChatgptOauth`, `rewrite_no_provedor()`,
`ContaChatgptCard` no `App.tsx`.

Usa o caminho de cliente open-source/local, que a OpenAI abriu sem lista de
espera: a primeira autorização vai com `client_id=dynamic_agent_client` e o
servidor emite um client próprio. Sem client secret, sem chave de parceiro.

**Não transcreve.** A documentação do fluxo recusa áudio e o endpoint de
transcrição, e isso foi **verificado com controle positivo**: o mesmo token
recém-renovado funciona em `/responses` e devolve `401` em
`/audio/transcriptions`. O provedor nem aparece no seletor de transcrição.

Três coisas só apareceram testando contra o servidor real, com o protótipo em
`open-flow-instalar/siwc-prototipo.py`:

- o `aud` do ID token é **lista**, não string;
- `/v1/models` devolve `{"models":[...]}` com `slug`/`display_name`, não o
  `{"data":[...]}` do dialeto comum;
- abrir o navegador por `cmd /c start` quebra a URL em cada `&`, e o servidor
  recusa com `invalid_authorize_request`.

**Decisão consciente sobre o ID token:** emissor, audiência, expiração e nonce
são conferidos; a assinatura **não**. O token chega na resposta do token
endpoint, por HTTPS direto nosso com `auth.openai.com`, sem passar pelo
navegador — o canal já autentica o emissor. Se um dia ele passar a vir pelo
navegador, a validação de assinatura vira obrigatória.

**Credenciais** ficam em `chatgpt.json`, fora do `settings.json`: o settings vai
inteiro para o frontend em `load_settings`, e token de acesso não pode viajar
para lá.

**Vale a pena?** Medido em 14/10 no mesmo prompt clínico, o `gpt-5.6-luna:low`
levou **20,9 s pela assinatura** e **7,0 s pelo OpenRouter**, com qualidade
indistinguível. A assinatura ficou como alternativa, não como caminho padrão.

**Testes:** `pkce_segue_o_vetor_do_rfc_7636`, `audiencia_aceita_lista_e_string`,
`host_id_e_opaco_e_estavel`, `erros_de_cota_viram_texto_legivel`,
`query_do_callback_decodifica_porcento`, `encode_escapa_o_que_precisa`,
`chatgpt_recusa_transcricao_com_motivo`, `provedor_sem_tipo_continua_sendo_de_chave`.

---

## 12. Esforço de raciocínio como seletor

**Onde:** `modelo_e_esforco()`, `reasoning_effort` no corpo de
`rewrite_openai_uma_vez()`, e o `<select>` de esforço no `ModelPicker`.

O esforço viaja como **sufixo do nome do modelo** (`claude-haiku-5-5:low`), o
que o faz atravessar seletor, escolha por perfil e histórico sem campo novo nem
migração. Quem monta o sufixo é a interface; o backend separa e manda
`reasoning_effort`.

Medido em 14/10 na camada de compatibilidade da Anthropic, `claude-haiku-5-5`
no prompt clínico: `low` gastou 2.951 tokens de saída, `high` 3.681. O campo
nativo dela (`output_config.effort`) é aceito mas **ignorado** — a variação saiu
na direção errada, que é ruído. `thinking` é rejeitado com mensagem explícita, o
que confirma que a camada conhece esses campos.

**Cuidado:** a primeira versão embutia as variantes no catálogo
(`modelo:low`, `modelo:medium`, `modelo:high`), o que triplicava uma lista de
465 modelos no OpenRouter e não valia para provedor de chave.

---

## 13. `temperature` condicional

**Onde:** `SEM_TEMPERATURA`, `sem_temperatura()`, `marcar_sem_temperatura()`, e a
retentativa em `rewrite_openai_uma_vez()`.

O app mandava `temperature: 0.2` em toda reescrita. O `claude-haiku-5-5`
responde `400 temperature is deprecated for this model` — **a reescrita pela
Anthropic estava quebrada**, e só apareceu quando se testou pelo caminho do app
em vez de um script que não mandava o campo.

Modelo que pensa antes de responder rejeita o parâmetro; a OpenAI fez o mesmo
nos modelos de raciocínio dela. Por isso a correção é genérica — detecta a
recusa em qualquer provedor — e não uma exceção para `api.anthropic.com`.

Quem recusa entra num conjunto em memória e a chamada é repetida sem o campo.
A condição `!sem_temperatura(model)` fecha a recursão: sem ela, um erro que
mencionasse "temperature" por outro motivo repetiria para sempre. O conjunto
não vai para o disco — na próxima abertura a primeira chamada descobre de novo,
o que custa uma retentativa e evita guardar uma lista que envelhece sozinha.

**Teste:** `modelo_que_recusa_temperatura_e_lembrado`.

---

## 14. Header `anthropic-version`

**Onde:** `Provider::precisa_versao_anthropic()`, `ANTHROPIC_VERSION`.

Medido contra a API real: `/chat/completions` — a camada de compatibilidade com
o dialeto OpenAI — responde **sem** o header; `/models` devolve
`400 anthropic-version: header is required`. Era isso que quebrava o botão de
carregar modelos.

Restrito a `api.anthropic.com` pelo mesmo motivo do fallback do Gemini: header
que só um provedor entende não viaja para os outros.

**Teste:** `so_a_anthropic_recebe_o_header_de_versao`.

---

## 15. Dicionário como pista de vocabulário na transcrição

**Onde:** `vocabulario()`, o campo `prompt` em `transcribe_openai()` e
`transcribe_groq()`.

O dicionário só influenciava a reescrita. Agora vai também no pedido de
transcrição.

**Por quê:** no ditado clínico de teste a Groq escreveu "dexometasona" e
"clavonato" — dois nomes de medicamento errados num texto que vai para
prontuário. Com o dicionário, 7/12 termos viraram **11/12**.

**Vai no `prompt` e só nele.** Testado contra os provedores reais:

| provedor / modelo | sem vocabulário | `prompt` | `keywords` |
|---|---|---|---|
| Groq `whisper-large-v3-turbo` | aceita | aceita | **HTTP 400** |
| OpenAI `gpt-transcribe` | aceita | aceita | HTTP 400 |
| OpenRouter `gpt-transcribe` | aceita | **sem efeito** | sem efeito |

`keywords` é o campo que o `gpt-transcribe` tem para termos de domínio, mas a
Groq devolve `unknown param` e a própria OpenAI recusa nessa rota. Um campo que
derruba o ditado num provedor não compensa o ganho no outro.

**Descoberta separada:** o **OpenRouter descarta o `prompt`**. O mesmo
`gpt-transcribe`, com a mesma pista, deu 12/12 direto na OpenAI e 10/12 pelo
OpenRouter, com texto idêntico ao da chamada sem pista. Ele aceita o campo e não
repassa.

**Teste:** `dicionario_vira_pista_de_vocabulario`.

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
