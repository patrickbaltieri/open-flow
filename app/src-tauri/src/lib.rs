use std::collections::HashSet;
use std::io::Cursor;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition};

mod audios;
mod ditados;
mod insertion_queue;
mod platform;
mod siwc;

const RULES: &str = "Você é um editor de ditado. Receberá a transcrição literal de uma fala em pt-BR.\n\
Invioláveis: NÃO adicione fatos, NÃO resuma, NÃO omita conteúdo.\n\
Edição — padrão conservador; o Estilo no fim pode pedir mais liberdade:\n\
1. Preserve as palavras do falante. Não troque expressões por sinônimos mais formais, não reordene nem reescreva frases que já se entendem. Corrija apenas ortografia, concordância e pontuação.\n\
2. Repetição de palavra ou frase é ênfase do falante: MANTENHA. Remova só hesitações e falsos inícios (\"é...\", \"hum\", \"a-a-a\").\n\
3. Descarte a versão anterior apenas quando o falante se retrata de forma explícita (\"não, na verdade é X\", \"me corrigindo, são 30\"). Conectores como \"quer dizer\", \"tipo\" e \"então\" sozinhos NÃO são correção.\n\
4. Estrutura: quando o falante marca a virada em voz alta, obedeça — é instrução, não licença para reformatar. \"outra coisa\", \"outro assunto\", \"mudando de assunto\", \"agora\" abrindo tema novo: parágrafo novo (linha em branco). \"primeiro/segundo/terceiro\", \"três coisas que eu queria\": uma linha por item, numerada. Isso vale igual em fala curta. Sem marca falada, entregue um bloco só: não force formato nem crie tópicos por conta própria.\n\
5. Fala longa (acima de ~400 caracteres) nunca sai em bloco único, mesmo que trate de um assunto só: divida em parágrafos de no máximo 4 frases, cortando no ponto final mais próximo da virada de foco.\n";

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
struct Profile {
    name: String,
    style: String,
    /// atalho próprio: dita já no estilo deste perfil, sem trocar o perfil ativo
    hotkey: String,
    /// "hold" (padrão) ou "toggle"
    hotkey_mode: String,
    /// Provedor de reescrita só deste perfil; vazio usa o global.
    ///
    /// Existe porque qualidade e latência estão em troca direta, e o ponto de
    /// equilíbrio é diferente em cada perfil. Medido em 07/10 no mesmo ditado
    /// clínico de 2.872 caracteres: `gpt-oss-120b` pela Groq respondeu em 8,4s
    /// mas escreveu "desobturção da restauração" onde a fala dizia "remoção da
    /// resina" — erro que não pode ir para prontuário; `gpt-5.6-terra` acertou
    /// as 8 seções do perfil sem erro, em 33,3s. Escolha global condenaria um
    /// dos dois: anotação de prontuário aceita esperar, ditado corrido não.
    rewrite_provider_name: String,
    /// Modelo de reescrita deste perfil. Só vale com `rewrite_provider_name`
    /// preenchido, e aí é obrigatório: o modelo global pertence a outro
    /// provedor e daria 404 sem explicar o motivo.
    rewrite_model: String,
    /// Transcreve só neste computador (sidecar local), sem nuvem e sem cair
    /// para a nuvem se o local falhar: ditado com dado de paciente, em que o
    /// áudio não pode sair da máquina (regra do laudo do TOMO, LGPD).
    stt_local_only: bool,
    /// Vocabulário deste perfil para quem transcreve (initial_prompt do
    /// Whisper). Vazio usa o dicionário global.
    stt_prompt: String,
    /// Não cola nem reescreve: o ditado vira `.wav` + `.json` com o texto cru
    /// na pasta `ditados` (ver ditados.rs), para outro programa buscar. Também
    /// fica fora do histórico e da pasta de áudios do app: é dado de paciente.
    save_only: bool,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Snippet {
    trigger: String,
    text: String,
}

/// Como o provedor se autentica, e por tabela em que rota ele atende.
///
/// `ChatgptOauth` não é "mais um compatível com a OpenAI": fala a Responses API
/// com forma própria, se autentica por token que expira de hora em hora, e
/// **não transcreve** — a documentação do fluxo recusa áudio e o endpoint de
/// transcrição. Por isso é tipo, não um `base_url` diferente.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
enum ProviderKind {
    /// Groq, OpenRouter, OmniRoute, Gemini — chave estática no cabeçalho
    #[default]
    ApiKey,
    /// Sign in with ChatGPT, gastando a cota do plano do usuário
    ChatgptOauth,
}

/// Provedor de LLM que fala o dialeto OpenAI (`/models`, `/chat/completions`).
/// Cobre Groq, OpenRouter, OmniRoute e o próprio Gemini pelo endpoint de
/// compatibilidade — cada um verificado antes de generalizar.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
struct Provider {
    /// apelido escolhido pelo usuário; é por ele que a escolha de modelo aponta
    name: String,
    /// endereço base, ex.: https://api.groq.com/openai/v1
    base_url: String,
    api_key: Option<String>,
    /// teto por chamada de reescrita, em segundos. 0 ou ausente = REWRITE_TIMEOUT.
    /// Existe porque a latência é característica do provedor, não do app: Groq e
    /// Gemini respondem abaixo de 1,1s, enquanto um gateway agregador pode levar
    /// 5s no caminho normal. Teto único condenaria um dos dois.
    timeout_secs: u32,
    /// como autentica; ausente no arquivo antigo vira `ApiKey`, o comportamento
    /// que já existia
    kind: ProviderKind,
}

impl Provider {
    /// junta o caminho ao endereço base tolerando barra sobrando ou faltando
    fn url(&self, caminho: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), caminho.trim_start_matches('/'))
    }

    /// A Anthropic exige `anthropic-version` nas rotas nativas. Medido em
    /// 14/10 contra a API real: `/chat/completions` — a camada de
    /// compatibilidade com o dialeto OpenAI — responde sem o header, mas
    /// `/models` devolve `400 anthropic-version: header is required`, e era
    /// isso que quebrava o botão de carregar modelos.
    ///
    /// Restrito ao host dela pelo mesmo motivo do fallback do Gemini: header
    /// que só um provedor entende não tem por que viajar para os outros.
    fn precisa_versao_anthropic(&self) -> bool {
        self.base_url.contains("api.anthropic.com")
    }

    /// teto desta chamada; sem valor próprio, usa o padrão
    fn timeout(&self) -> Duration {
        if self.timeout_secs == 0 {
            REWRITE_TIMEOUT
        } else {
            Duration::from_secs(self.timeout_secs as u64)
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    /// atalho de ditado no modo "segurar e falar"
    hotkey: String,
    /// atalho de ditado no modo "apertar para iniciar, apertar para parar"
    hotkey_toggle: String,
    /// atalho que avança para o próximo perfil da lista
    hotkey_cycle_profile: String,
    /// tecla que aborta o ditado em andamento; vazio desliga
    cancel_key: String,
    /// legado: antes escolhia o modo do atalho único. Só é lido na migração.
    mode: String,
    mic: Option<String>,
    stt_provider: String,
    groq_api_key: Option<String>,
    groq_model: String,
    speech_lang: String,
    output_lang: String,
    gemini_model: String,
    gemini_api_key: Option<String>,
    /// legado: "gemini" ou "groq". Só vale quando `rewrite_provider_name` está vazio.
    rewrite_provider: String,
    groq_rewrite_model: String,
    /// provedores cadastrados pelo usuário (aba Provedores)
    providers: Vec<Provider>,
    /// apelido do provedor usado na reescrita; vazio = comportamento legado
    rewrite_provider_name: String,
    rewrite_model: String,
    /// apelido do provedor usado na transcrição; vazio = comportamento legado
    /// (`stt_provider`, que continua valendo para o modo local)
    stt_provider_name: String,
    stt_model: String,
    wave_style: String,
    theme: String,
    profiles: Vec<Profile>,
    active_profile: String,
    dictionary: Vec<String>,
    snippets: Vec<Snippet>,
    history_enabled: bool,
    /// dias que a cópia em disco de cada ditado fica guardada; 0 = não grava.
    /// Vale também para a pasta `ditados` (ver `prazo_ditados`).
    audio_keep_days: u32,
    python: String,
    sidecar: String,
    stt_port: u16,
}

fn default_profiles() -> Vec<Profile> {
    let p = |name: &str, style: &str| Profile {
        name: name.into(),
        style: style.into(),
        hotkey: String::new(),
        hotkey_mode: "hold".into(),
        ..Default::default()
    };
    vec![
        p("Bruto corrigido", "coringa do dia a dia (resposta de conversa, WhatsApp, prompt para IA): texto natural com o tom e as palavras do falante preservados."),
        p("Jurídico formal", "certidão ou comunicação de oficial de justiça, linguagem jurídica formal brasileira (ex.: \"Certifico e dou fé que...\", \"genitor\", \"Ante o exposto, devolvo o presente mandado\")."),
        p("E-mail profissional", "e-mail profissional claro e cordial, com saudação e fecho quando fizer sentido. Aqui pode reescrever para deixar formal: cada ideia em sua própria linha, separadas por linha em branco, no padrão corporativo."),
        p("WhatsApp curto", "mensagem curta e informal de WhatsApp, direta, sem formalidade excessiva."),
        p("Roteiro de vídeo", "roteiro falado para vídeo do YouTube: frases curtas, ritmo de fala natural, tom de conversa."),
    ]
}

#[cfg(windows)]
fn default_hotkey() -> String {
    "Ctrl+Win".into()
}

// Ctrl+Option: a combinação mais "vazia" do macOS. Ctrl+Cmd foi descartado
// porque letra acidental durante o hold dispara combos do sistema
// (Ctrl+Cmd+Q trava a tela).
#[cfg(target_os = "macos")]
fn default_hotkey() -> String {
    "Ctrl+Option".into()
}

#[cfg(windows)]
fn default_sidecar_paths() -> (String, String) {
    (
        r"C:\dev\open-flow\.venv\Scripts\python.exe".into(),
        r"C:\dev\open-flow\sidecar\stt_server.py".into(),
    )
}

#[cfg(target_os = "macos")]
fn default_sidecar_paths() -> (String, String) {
    let home = dirs::home_dir().unwrap_or_default();
    (
        home.join("dev/open-flow/.venv/bin/python").to_string_lossy().into_owned(),
        home.join("dev/open-flow/sidecar/stt_server.py").to_string_lossy().into_owned(),
    )
}

impl Default for Settings {
    fn default() -> Self {
        let (python, sidecar) = default_sidecar_paths();
        Settings {
            hotkey: default_hotkey(),
            hotkey_toggle: String::new(),
            hotkey_cycle_profile: String::new(),
            // Esc e não Espaço: com o atalho segurado, Ctrl+Option+Espaço (mac) e
            // Win+Espaço (Windows) são atalhos do sistema que trocam a fonte de
            // entrada — o hook é listen-only, então o sistema recebe a combinação
            // junto e o layout do teclado muda no meio do ditado.
            cancel_key: "ESC".into(),
            mode: "hold".into(),
            mic: None,
            stt_provider: "groq".into(),
            groq_api_key: None,
            groq_model: "whisper-large-v3-turbo".into(),
            speech_lang: "pt".into(),
            output_lang: "same".into(),
            gemini_model: "gemini-3.5-flash-lite".into(),
            gemini_api_key: None,
            rewrite_provider: "gemini".into(),
            groq_rewrite_model: "llama-3.3-70b-versatile".into(),
            providers: Vec::new(),
            rewrite_provider_name: String::new(),
            rewrite_model: String::new(),
            stt_provider_name: String::new(),
            stt_model: String::new(),
            wave_style: "tech".into(),
            theme: "dark".into(),
            profiles: default_profiles(),
            active_profile: "Bruto corrigido".into(),
            dictionary: Vec::new(),
            snippets: Vec::new(),
            history_enabled: true,
            // 7 cobre fim de semana e feriado até o laudo buscar o ditado
            audio_keep_days: 7,
            // ponytail: caminhos desta máquina; viram recurso empacotado na fase 3
            python,
            sidecar,
            stt_port: 17765,
        }
    }
}

struct Shared {
    settings: Mutex<Settings>,
    bindings: Mutex<Vec<Binding>>,
    cancel: Mutex<Option<u32>>,
    sidecar: Mutex<Option<std::process::Child>>,
    /// preenchido no setup(); o hook de teclado precisa dele para trocar perfil e avisar na tela
    app: OnceLock<AppHandle>,
    /// último texto inserido: a colagem pode falhar sem erro (janela sem foco,
    /// campo não editável) e o clipboard volta ao valor antigo 300ms depois —
    /// sem esta cópia o ditado some sem deixar rastro
    last_text: Mutex<String>,
    /// cada exibição do overlay ganha um número; quem termina tarde só mexe na
    /// janela se ainda for o dono dela (ditar de novo enquanto o anterior processa)
    overlay_gen: AtomicU64,
    /// o último Start falhou ao abrir o microfone: o hook confere para não
    /// ficar preso numa gravação que nunca começou. É aviso explícito da thread
    /// de captura, não dedução por tempo — microfone Bluetooth leva mais de 1s
    /// para abrir, e tratar isso como falha liberava outro atalho no meio.
    falhou_ao_gravar: AtomicBool,
}

impl Settings {
    /// algum perfil transcreve só no sidecar local
    fn tem_perfil_local(&self) -> bool {
        self.profiles.iter().any(|p| p.stt_local_only || p.save_only)
    }

    fn groq_ready(&self) -> bool {
        self.groq_api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false)
    }
}

pub(crate) fn config_dir() -> std::path::PathBuf {
    // Teste nunca toca a pasta real: o teste da troca de perfil chama
    // `cycle_profile`, que grava o settings.json, e em 28/10/2026 isso apagou
    // as configurações de verdade (perfis, chaves, atalhos) no meio do uso.
    #[cfg(test)]
    return std::env::temp_dir().join(format!("openflow-testes-{}", std::process::id()));
    #[cfg(not(test))]
    dirs::config_dir().unwrap().join("OpenFlow")
}

fn settings_path() -> std::path::PathBuf {
    config_dir().join("settings.json")
}

// --- Segredos: DPAPI (Windows) ⇄ Keychain (macOS); no disco fica "enc:..." ---
const ENC_PREFIX: &str = "enc:";

/// Versão da API da Anthropic. Ela trata isso como contrato: é o que garante
/// que uma mudança futura no formato não quebre quem já está rodando.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `name` identifica o item no cofre da plataforma (o Keychain precisa; o DPAPI ignora)
pub(crate) fn decrypt_key(name: &str, v: &Option<String>) -> Option<String> {
    v.as_ref()
        .map(|s| match s.strip_prefix(ENC_PREFIX) {
            Some(stored) => platform::unprotect_secret(name, stored).unwrap_or_default(),
            None => s.clone(),
        })
        .filter(|s| !s.is_empty())
}

pub(crate) fn encrypt_key(name: &str, v: &Option<String>) -> Option<String> {
    v.as_ref().filter(|s| !s.is_empty()).map(|s| {
        if s.starts_with(ENC_PREFIX) {
            s.clone()
        } else {
            platform::protect_secret(name, s)
                .map(|b| format!("{ENC_PREFIX}{b}"))
                .unwrap_or_else(|| s.clone())
        }
    })
}

/// Primeira execução com a aba Provedores: transforma as chaves que já existiam
/// em cartões, e aponta a reescrita para o provedor que estava em uso. Sem isto
/// o usuário abriria a aba vazia e teria de recadastrar tudo à mão.
fn migrate_providers(s: &mut Settings) {
    if s.providers.is_empty() {
        if s.groq_api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false) {
            s.providers.push(Provider {
                name: "Groq".into(),
                base_url: "https://api.groq.com/openai/v1".into(),
                api_key: s.groq_api_key.clone(),
                timeout_secs: 0,
                kind: ProviderKind::ApiKey,
            });
        }
        if s.gemini_api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false) {
            // endpoint de compatibilidade: o /chat/completions responde, mas o
            // /models devolve 404 — a aba trata provedor sem catálogo
            s.providers.push(Provider {
                name: "Gemini".into(),
                base_url: "https://generativelanguage.googleapis.com/v1beta/openai".into(),
                api_key: s.gemini_api_key.clone(),
                timeout_secs: 0,
                kind: ProviderKind::ApiKey,
            });
        }
    }
    if s.stt_provider_name.is_empty() && s.stt_provider == "groq" {
        if let Some(p) = s.providers.iter().find(|p| p.name == "Groq") {
            s.stt_provider_name = p.name.clone();
            s.stt_model = s.groq_model.clone();
        }
    }
    if s.rewrite_provider_name.is_empty() && !s.providers.is_empty() {
        let (alvo, modelo) = if s.rewrite_provider == "groq" {
            ("Groq", s.groq_rewrite_model.clone())
        } else {
            ("Gemini", s.gemini_model.clone())
        };
        if s.providers.iter().any(|p| p.name == alvo) {
            s.rewrite_provider_name = alvo.into();
            s.rewrite_model = modelo;
        }
    }
}

/// grava no disco com as chaves criptografadas (em memória ficam em texto)
fn persist_settings(s: &Settings) -> Result<(), String> {
    let mut disk = s.clone();
    disk.gemini_api_key = encrypt_key("gemini_api_key", &disk.gemini_api_key);
    disk.groq_api_key = encrypt_key("groq_api_key", &disk.groq_api_key);
    for p in &mut disk.providers {
        p.api_key = encrypt_key("provider_api_key", &p.api_key);
    }
    let path = settings_path();
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&path, serde_json::to_string_pretty(&disk).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

fn history_path() -> std::path::PathBuf {
    config_dir().join("history.jsonl")
}

/// retorna (settings, primeira_execucao)
fn load_settings() -> (Settings, bool) {
    let path = settings_path();
    if let Ok(txt) = std::fs::read_to_string(&path) {
        // tolera BOM que editores/PowerShell inserem
        match serde_json::from_str::<Settings>(txt.trim_start_matches('\u{feff}')) {
            Ok(mut s) => {
                if s.profiles.is_empty() {
                    s.profiles = default_profiles();
                }
                // versões antigas tinham um atalho só, com o modo num campo à parte:
                // quem estava em "toggle" mantém o comportamento no atalho equivalente
                if s.mode == "toggle" && s.hotkey_toggle.is_empty() && !s.hotkey.is_empty() {
                    s.hotkey_toggle = std::mem::take(&mut s.hotkey);
                }
                s.mode = "hold".into();
                if s.rewrite_provider.is_empty() {
                    s.rewrite_provider = "gemini".into();
                }
                if s.groq_rewrite_model.is_empty() {
                    s.groq_rewrite_model = "llama-3.3-70b-versatile".into();
                }
                for p in &mut s.profiles {
                    if p.hotkey_mode.is_empty() {
                        p.hotkey_mode = "hold".into();
                    }
                }
                s.gemini_api_key = decrypt_key("gemini_api_key", &s.gemini_api_key);
                s.groq_api_key = decrypt_key("groq_api_key", &s.groq_api_key);
                for p in &mut s.providers {
                    p.api_key = decrypt_key("provider_api_key", &p.api_key);
                }
                migrate_providers(&mut s);
                return (s, false);
            }
            Err(e) => {
                // nunca descartar o arquivo do usuário em silêncio
                eprintln!("[settings] arquivo ilegível ({e}); backup em settings.json.bak");
                let _ = std::fs::copy(&path, path.with_extension("json.bak"));
            }
        }
    }
    let s = Settings::default();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&s).unwrap());
    (s, true)
}

/// keycodes por plataforma (VK no Windows, CGKeyCode no mac) vêm de platform::
/// Cada elemento é um grupo: basta uma das teclas do grupo (ex.: Ctrl esquerdo ou direito).
/// `None` quando o texto está vazio ou cita uma tecla desconhecida — atalho inválido é
/// ignorado, nunca vira uma tecla arbitrária.
fn parse_hotkey(s: &str) -> Option<Vec<Vec<u32>>> {
    let mut groups = Vec::new();
    for token in s.split('+') {
        let name = token.trim().to_uppercase();
        if name.is_empty() {
            return None;
        }
        let group = platform::modifier_group(&name)
            .or_else(|| platform::key_from_name(&name).map(|k| vec![k]))?;
        groups.push(group);
    }
    if groups.is_empty() {
        None
    } else {
        Some(groups)
    }
}

/// O que um atalho faz quando disparado.
#[derive(Clone)]
enum Act {
    /// grava e dita; `profile` força o estilo daquele perfil sem mudar o perfil ativo.
    /// Todo atalho de ditado é misto (`SEGURAR`): toque curto liga e outro toque
    /// desliga; segurar grava até soltar.
    Record { profile: Option<String> },
    /// avança para o próximo perfil da lista
    CycleProfile,
}

#[derive(Clone)]
struct Binding {
    /// identidade estável do vínculo ("hold", "toggle", "cycle", "profile:<nome>")
    id: String,
    groups: Vec<Vec<u32>>,
    act: Act,
}

fn add_binding(v: &mut Vec<Binding>, id: String, keys: &str, act: Act) {
    if let Some(groups) = parse_hotkey(keys) {
        // um mesmo atalho em dois lugares: vale o primeiro cadastrado
        if v.iter().any(|b| b.groups == groups) {
            eprintln!("[hotkey] atalho duplicado ignorado em {id}: {keys}");
            return;
        }
        v.push(Binding { id, groups, act });
    }
}

fn build_bindings(s: &Settings) -> Vec<Binding> {
    let mut v = Vec::new();
    add_binding(&mut v, "hold".into(), &s.hotkey, Act::Record { profile: None });
    add_binding(&mut v, "toggle".into(), &s.hotkey_toggle, Act::Record { profile: None });
    add_binding(&mut v, "cycle".into(), &s.hotkey_cycle_profile, Act::CycleProfile);
    for p in &s.profiles {
        add_binding(
            &mut v,
            format!("profile:{}", p.name),
            &p.hotkey,
            Act::Record { profile: Some(p.name.clone()) },
        );
    }
    // mais teclas = mais específico. Sem isto, Ctrl+Win dispararia junto com Ctrl+Win+J.
    v.sort_by(|a, b| b.groups.len().cmp(&a.groups.len()));
    v
}

/// vazio = cancelamento desligado
fn parse_cancel_key(s: &str) -> Option<u32> {
    let name = s.trim().to_uppercase();
    if name.is_empty() {
        return None;
    }
    // key_from_name devolve None para nome desconhecido: tecla inválida desliga o
    // cancelamento em vez de virar F9 por engano
    platform::key_from_name(&name)
}

type LevelFn = Box<dyn FnMut(f32) + Send>;

struct Recorder {
    stream: Option<cpal::Stream>,
    buf: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    channels: u16,
}

impl Recorder {
    fn new() -> Self {
        Recorder { stream: None, buf: Arc::new(Mutex::new(Vec::new())), rate: 16000, channels: 1 }
    }

    fn start(&mut self, mic: Option<&str>, mut on_level: LevelFn) -> Result<(), String> {
        let host = cpal::default_host();
        let device = match mic {
            Some(name) => host
                .input_devices()
                .map_err(|e| e.to_string())?
                .find(|d| d.name().map(|n| n.contains(name)).unwrap_or(false))
                .ok_or_else(|| format!("microfone '{name}' não encontrado"))?,
            None => host.default_input_device().ok_or("sem microfone padrão")?,
        };
        let config = device.default_input_config().map_err(|e| e.to_string())?;
        self.rate = config.sample_rate().0;
        self.channels = config.channels();
        self.buf.lock().unwrap().clear();
        let buf = self.buf.clone();
        let err_fn = |e| eprintln!("[audio] erro no stream: {e}");
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device
                .build_input_stream(
                    &config.into(),
                    move |data: &[f32], _: &_| {
                        let rms = (data.iter().map(|s| s * s).sum::<f32>()
                            / data.len().max(1) as f32)
                            .sqrt();
                        on_level(rms);
                        buf.lock().unwrap().extend_from_slice(data);
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| e.to_string())?,
            cpal::SampleFormat::I16 => device
                .build_input_stream(
                    &config.into(),
                    move |data: &[i16], _: &_| {
                        let mut b = buf.lock().unwrap();
                        let start = b.len();
                        b.extend(data.iter().map(|s| *s as f32 / 32768.0));
                        let chunk = &b[start..];
                        let rms = (chunk.iter().map(|s| s * s).sum::<f32>()
                            / chunk.len().max(1) as f32)
                            .sqrt();
                        drop(b);
                        on_level(rms);
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| e.to_string())?,
            f => return Err(format!("formato de sample não suportado: {f}")),
        };
        stream.play().map_err(|e| e.to_string())?;
        self.stream = Some(stream);
        Ok(())
    }

    fn stop(&mut self) -> (Vec<f32>, u32) {
        self.stream = None;
        let raw = std::mem::take(&mut *self.buf.lock().unwrap());
        let ch = self.channels as usize;
        let mono: Vec<f32> = if ch > 1 { raw.chunks(ch).map(|c| c[0]).collect() } else { raw };
        (mono, self.rate)
    }
}

/// maior RMS em janelas de 100ms — proxy de "houve fala?"
fn peak_rms(samples: &[f32], rate: u32) -> f32 {
    let win = (rate as usize / 10).max(1);
    samples
        .chunks(win)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .fold(0.0f32, f32::max)
}

/// alucinações típicas do Whisper em áudio sem fala
fn is_hallucination(text: &str, audio_secs: f32) -> bool {
    if audio_secs >= 2.0 {
        return false;
    }
    let norm: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let norm = norm.trim();
    // sem frase anterior para servir de âncora, a assinatura de legenda vem
    // sozinha e o strip_credit não a alcança (histórico: 0,8s de áudio)
    norm.starts_with("legenda")
        || matches!(norm, "e aí" | "eaí" | "obrigado" | "obrigada" | "tchau" | "valeu" | "thank you")
}

/// O Whisper trabalha internamente a 16 kHz: qualquer coisa acima disso ele
/// reamostra antes de processar. Enviar 48 kHz é subir 3x mais bytes para o
/// servidor jogar dois terços fora — sem ganho nenhum de qualidade.
///
/// NÃO REMOVER EM MERGE COM O UPSTREAM. Sem isto, o teto de 25 MB da Groq cai
/// de ~13,7 min para ~4,6 min de fala, e a transcrição falha em ditado longo.
/// Já aconteceu no Mac; no Windows um ditado de 243 s chegou a 22,3 MB (89% do
/// limite). Ver docs/alteracoes-do-fork.md.
const STT_RATE: u32 = 16_000;

/// Razão inteira (48000/16000 = 3) cai na média por blocos, que além de
/// decimar funciona como filtro passa-baixa e evita aliasing. Razão quebrada
/// cai na interpolação linear, que é suficiente para voz.
fn resample(samples: &[f32], de: u32, para: u32) -> Vec<f32> {
    if de == para || de == 0 || para == 0 || samples.is_empty() {
        return samples.to_vec();
    }
    if de % para == 0 {
        let fator = (de / para) as usize;
        return samples
            .chunks(fator)
            .map(|c| c.iter().sum::<f32>() / c.len() as f32)
            .collect();
    }
    let razao = para as f64 / de as f64;
    let n = (samples.len() as f64 * razao).round() as usize;
    (0..n)
        .map(|i| {
            let pos = i as f64 / razao;
            let a = (pos.floor() as usize).min(samples.len() - 1);
            let b = (a + 1).min(samples.len() - 1);
            let f = (pos - a as f64) as f32;
            samples[a] * (1.0 - f) + samples[b] * f
        })
        .collect()
}

fn wav_bytes(samples: &[f32], rate: u32) -> Vec<u8> {
    let (samples, rate) = if rate > STT_RATE {
        (resample(samples, rate, STT_RATE), STT_RATE)
    } else {
        (samples.to_vec(), rate)
    };
    let samples = &samples[..];
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cursor, spec).unwrap();
        for s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16).unwrap();
        }
        w.finalize().unwrap();
    }
    cursor.into_inner()
}

/// Cliente HTTP único para as APIs de nuvem. Criar um por chamada custava um
/// handshake TCP+TLS a cada ditado e, pior, sorteava um endereço novo toda vez:
/// em rede com IPv6 anunciado mas sem rota (VPN que bloqueia IPv6, por exemplo)
/// a conexão fica pendurada até o timeout de 30s. Amarrar a saída em IPv4 e
/// reaproveitar a conexão elimina os dois problemas.
pub(crate) fn cloud_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("cliente HTTP de nuvem")
    })
}

/// Cliente da reescrita: teto curto. O `cloud_client` continua em 30s porque a
/// transcrição sobe o WAV e precisa do tempo; reescrita é só texto e, medido
/// aqui, 7 de 10 chamadas voltam abaixo de 1,1s. O que passa disso é travada
/// isolada do free tier, não chamada legitimamente longa.
fn rewrite_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            // sem teto no cliente: cada chamada define o seu, porque ele varia
            // por provedor (ver Provider::timeout)
            .connect_timeout(Duration::from_secs(5))
            .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("cliente HTTP da reescrita")
    })
}

fn sidecar_healthy(port: u16) -> bool {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()
        .and_then(|c| c.get(format!("http://127.0.0.1:{port}/health")).send().ok())
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn ensure_sidecar(shared: &Shared, s: &Settings) -> Result<(), String> {
    if sidecar_healthy(s.stt_port) {
        return Ok(());
    }
    {
        let mut guard = shared.sidecar.lock().unwrap();
        if sidecar_healthy(s.stt_port) {
            return Ok(());
        }
        if let Some(c) = guard.as_mut() {
            let _ = c.kill();
        }
        *guard = platform::spawn_sidecar(s);
        if guard.is_none() {
            return Err("não foi possível iniciar a transcrição local".into());
        }
    }
    println!("[stt] aquecendo transcrição local (primeira vez leva ~30s)...");
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if sidecar_healthy(s.stt_port) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err("transcrição local não ficou pronta em 120s".into())
}

/// Transcrição por qualquer provedor com `/audio/transcriptions` no formato
/// da OpenAI — mesmo multipart usado hoje com a Groq, só muda o endereço.
/// Dicionário do usuário como pista de vocabulário para quem transcreve.
///
/// Vai no campo `prompt`, e só nele. Medido em 07/10 contra os provedores
/// reais: `prompt` é aceito pela Groq e pelo OpenRouter, nos dois modelos
/// testados; `keywords` — o campo que o `gpt-transcribe` tem para termos de
/// domínio — devolve `HTTP 400 unknown param` na Groq. Um campo que derruba o
/// ditado em um provedor não compensa o ganho no outro.
///
/// Razão de existir: no ditado clínico de teste, `whisper-large-v3-turbo`
/// escreveu "dexometasona" e "clavonato" — nomes de medicamento errados num
/// texto que vai para prontuário — e ambos os modelos erraram "radix
/// entomolaris". São termos que o usuário já cadastra para a reescrita; usá-los
/// também aqui não custa nada.
fn vocabulario(s: &Settings) -> Option<String> {
    let termos: Vec<&str> =
        s.dictionary.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect();
    if termos.is_empty() {
        None
    } else {
        Some(termos.join(", "))
    }
}

fn transcribe_openai(
    wav: Vec<u8>,
    p: &Provider,
    model: &str,
    lang: &str,
    vocab: Option<&str>,
) -> Result<String, String> {
    if p.kind == ProviderKind::ChatgptOauth {
        // Não é limitação do app: o fluxo de uso do plano recusa áudio e o
        // endpoint de transcrição. Dizer isso é melhor que mandar o WAV e
        // receber um 400 sem explicação.
        return Err(format!(
            "'{}' é conexão com o ChatGPT, que não transcreve — escolha um provedor de chave",
            p.name
        ));
    }
    if model.trim().is_empty() {
        return Err(format!("nenhum modelo de transcrição escolhido para '{}'", p.name));
    }
    let part = reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let mut form = reqwest::blocking::multipart::Form::new()
        .part("file", part)
        .text("model", model.to_string())
        .text("response_format", "json");
    if lang != "auto" {
        form = form.text("language", lang.to_string());
    }
    if let Some(v) = vocab.filter(|v| !v.is_empty()) {
        form = form.text("prompt", v.to_string());
    }
    let mut req = cloud_client()
        .post(p.url("audio/transcriptions"))
        .header("accept", "application/json")
        .multipart(form);
    if let Some(k) = p.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let resp = req.send().map_err(|e| format!("{}: {e}", p.name))?;
    let status = resp.status();
    let corpo = resp.text().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&corpo)
        .map_err(|_| format!("{} respondeu HTTP {status} sem JSON: {}", p.name, corta(&corpo)))?;
    if let Some(msg) = v["error"]["message"].as_str() {
        return Err(format!("{}: {msg}", p.name));
    }
    if !status.is_success() {
        return Err(format!("{} respondeu HTTP {status}: {}", p.name, corta(&corpo)));
    }
    Ok(v["text"].as_str().unwrap_or("").trim().to_string())
}

fn transcribe_groq(wav: Vec<u8>, s: &Settings, vocab: Option<&str>) -> Result<String, String> {
    let key = s.groq_api_key.clone().filter(|k| !k.is_empty()).ok_or("chave Groq ausente")?;
    let part = reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let mut form = reqwest::blocking::multipart::Form::new()
        .part("file", part)
        .text("model", s.groq_model.clone())
        .text("response_format", "json");
    if s.speech_lang != "auto" {
        form = form.text("language", s.speech_lang.clone());
    }
    if let Some(v) = vocab {
        form = form.text("prompt", v.to_string());
    }
    let resp = cloud_client()
        .post("https://api.groq.com/openai/v1/audio/transcriptions")
        .bearer_auth(key)
        .multipart(form)
        .send()
        .map_err(|e| format!("Groq: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Groq HTTP {}: {}", resp.status(), resp.text().unwrap_or_default()));
    }
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    Ok(v["text"].as_str().unwrap_or("").trim().to_string())
}

/// O Whisper foi treinado em legendas e às vezes assina o fim da transcrição:
/// "Legenda Adriana Zanotto", "Legendas pela comunidade Amara.org". Não dá para
/// resolver no prompt — as regras mandam NÃO omitir conteúdo, então o Gemini
/// preserva a assinatura fielmente e ela cai no campo do usuário. 8 dos 282
/// ditados do histórico vieram assim, sempre coladas depois da última frase.
///
/// Só corta a cauda depois do último ponto final: "legenda" no meio da fala é
/// palavra legítima do falante (aparece no histórico em "com título, legenda,
/// nome das variáveis") e não pode sumir.
fn strip_credit(text: &str) -> &str {
    const MARCAS: [&str; 3] = ["legenda", "legendado", "subtitle"];
    let t = text.trim_end();
    let corpo = t.trim_end_matches(['.', '!', '?', ' ']);
    // fim de frase é pontuação seguida de espaço: o ponto de "Amara.org" não conta
    let Some(i) = corpo
        .char_indices()
        .filter(|(j, c)| {
            matches!(c, '.' | '!' | '?' | '\n')
                && corpo[j + c.len_utf8()..].chars().next().map_or(true, char::is_whitespace)
        })
        .map(|(j, _)| j)
        .next_back()
    else {
        return t;
    };
    let cauda = corpo[i + 1..].trim().to_lowercase();
    let curta = cauda.split_whitespace().count() <= 5;
    if curta && (MARCAS.iter().any(|m| cauda.starts_with(m)) || cauda.contains("amara.org")) {
        return t[..=i].trim_end();
    }
    t
}

fn run_stt(wav: Vec<u8>, shared: &Shared, s: &Settings, prof: Option<&Profile>) -> Result<String, String> {
    run_stt_com_origem(wav, shared, s, prof).map(|(t, _)| t)
}

/// Como `run_stt`, devolvendo também quem transcreveu. No caminho só local a
/// origem vem da resposta do sidecar (modelo, dispositivo e tempo): é a prova,
/// gravada no ditado do laudo, de que o áudio não saiu da máquina.
fn run_stt_com_origem(
    wav: Vec<u8>,
    shared: &Shared,
    s: &Settings,
    prof: Option<&Profile>,
) -> Result<(String, Option<Origem>), String> {
    let vocab = prof
        .map(|p| p.stt_prompt.trim())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| vocabulario(s));
    // "salvar para o laudo" implica só local: é áudio de paciente
    let texto = if prof.is_some_and(|p| p.stt_local_only || p.save_only) {
        // sem nuvem e sem fallback: falhar aqui é melhor que mandar o áudio para fora.
        // Beam 5, o mesmo do TOMO: no local a precisão custa ~1s, não dinheiro.
        ensure_sidecar(shared, s)
            .map_err(|e| format!("transcrição local indisponível ({e}); este perfil não usa a nuvem"))?;
        transcribe_local(wav, s.stt_port, &s.speech_lang, vocab.as_deref(), 5).map(|(t, o)| (t, Some(o)))
    } else {
        stt_provider(wav, shared, s, vocab.as_deref()).map(|t| (t, None))
    };
    // os dois provedores rodam Whisper, então os dois assinam legenda
    texto.map(|(t, o)| (strip_credit(&t).to_string(), o))
}

/// Quem transcreveu, segundo o próprio sidecar.
struct Origem {
    modelo: String,
    dispositivo: String,
    segundos: f64,
}

fn stt_provider(wav: Vec<u8>, shared: &Shared, s: &Settings, vocab: Option<&str>) -> Result<String, String> {
    // provedor escolhido na aba Provedores tem prioridade
    if !s.stt_provider_name.is_empty() {
        match s.providers.iter().find(|p| p.name == s.stt_provider_name) {
            Some(p) => match transcribe_openai(
                wav.clone(),
                p,
                &s.stt_model,
                &s.speech_lang,
                vocab,
            ) {
                Ok(t) => {
                    println!("[stt] transcrito via {} ({})", p.name, s.stt_model);
                    return Ok(t);
                }
                Err(e) => eprintln!("[stt] {} falhou ({e}); tentando o local", p.name),
            },
            None => eprintln!(
                "[stt] provedor '{}' não existe mais; tentando o local",
                s.stt_provider_name
            ),
        }
    } else if s.stt_provider == "groq" && s.groq_ready() {
        match transcribe_groq(wav.clone(), s, vocab) {
            Ok(t) => {
                println!("[stt] transcrito via Groq (nuvem)");
                return Ok(t);
            }
            Err(e) => eprintln!("[stt] Groq falhou ({e}); caindo para o local"),
        }
    }
    match ensure_sidecar(shared, s) {
        Ok(()) => transcribe(wav, s.stt_port, &s.speech_lang, vocab, 1),
        Err(e) if s.stt_provider == "local" && s.groq_ready() => {
            eprintln!("[stt] local indisponível ({e}); caindo para o Groq");
            transcribe_groq(wav, s, vocab)
        }
        Err(e) => Err(e),
    }
}

fn transcribe(wav: Vec<u8>, port: u16, lang: &str, vocab: Option<&str>, beam: u32) -> Result<String, String> {
    transcribe_local(wav, port, lang, vocab, beam).map(|(t, _)| t)
}

fn transcribe_local(
    wav: Vec<u8>,
    port: u16,
    lang: &str,
    vocab: Option<&str>,
    beam: u32,
) -> Result<(String, Origem), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let beam = beam.to_string();
    let mut q = vec![("lang", lang), ("beam", beam.as_str())];
    if let Some(v) = vocab {
        q.push(("prompt", v));
    }
    let v: serde_json::Value = client
        .post(format!("http://127.0.0.1:{port}/stt"))
        .query(&q)
        .body(wav)
        .send()
        .map_err(|e| format!("sidecar STT: {e}"))?
        .json()
        .map_err(|e| e.to_string())?;
    let origem = Origem {
        // sidecar antigo não manda os dois campos: fica registrado que não se sabe
        modelo: v["model"].as_str().unwrap_or("?").to_string(),
        dispositivo: v["device"].as_str().unwrap_or("?").to_string(),
        segundos: v["stt_seconds"].as_f64().unwrap_or(-1.0),
    };
    Ok((v["text"].as_str().unwrap_or("").trim().to_string(), origem))
}

fn build_prompt(s: &Settings, profile: &str) -> String {
    let style = s
        .profiles
        .iter()
        .find(|p| p.name == profile)
        .map(|p| p.style.clone())
        .unwrap_or_else(|| "texto natural corrigido, mantendo o tom do falante.".into());
    let mut p = RULES.to_string();
    let dict: Vec<&str> = s.dictionary.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect();
    if !dict.is_empty() {
        p.push_str(&format!("6. Grafias obrigatórias quando essas palavras aparecerem: {}.\n", dict.join(", ")));
    }
    p.push_str("Responda SOMENTE com o texto final, sem comentários.\n");
    if s.output_lang != "same" && s.output_lang != s.speech_lang {
        let idioma = match s.output_lang.as_str() {
            "pt" => "português do Brasil",
            "en" => "inglês",
            "es" => "espanhol",
            other => other,
        };
        p.push_str(&format!(
            "Escreva o texto final em {idioma}, mantendo URLs, e-mails e nomes próprios intactos.\n"
        ));
    }
    p.push_str(&format!("Estilo: {style}\n\nTranscrição:\n"));
    p
}

/// Decide quem reescreve um perfil. `Ok(None)` significa "sem escolha própria,
/// usa o provedor global".
///
/// Separada do `rewrite` para poder ser testada: a reescrita em si faz chamada
/// de rede, a escolha de quem chamar não.
fn alvo_do_perfil<'a>(
    s: &'a Settings,
    profile: &str,
) -> Result<Option<(&'a Provider, &'a str)>, String> {
    let prof = match s.profiles.iter().find(|p| p.name == profile) {
        Some(p) => p,
        // perfil que não existe mais: o global resolve, não é motivo de erro
        None => return Ok(None),
    };
    if prof.rewrite_provider_name.is_empty() {
        return Ok(None);
    }
    if prof.rewrite_model.is_empty() {
        return Err(format!(
            "perfil '{}' tem provedor '{}' mas nenhum modelo escolhido",
            profile, prof.rewrite_provider_name
        ));
    }
    match s.providers.iter().find(|p| p.name == prof.rewrite_provider_name) {
        Some(p) => Ok(Some((p, prof.rewrite_model.as_str()))),
        None => Err(format!(
            "perfil '{}' aponta para o provedor '{}', que não existe mais na aba Provedores",
            profile, prof.rewrite_provider_name
        )),
    }
}

/// Modelos que já recusaram `temperature`, para não pagar a ida perdida de novo.
///
/// Cresce em uso, nunca encolhe: um modelo que recusa o campo hoje não volta a
/// aceitá-lo. Vive só na memória do processo — na próxima abertura do app a
/// primeira chamada daquele modelo descobre de novo, o que custa uma
/// retentativa e evita guardar no disco uma lista que envelhece sozinha.
static SEM_TEMPERATURA: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn sem_temperatura(model: &str) -> bool {
    SEM_TEMPERATURA
        .get_or_init(Default::default)
        .lock()
        .map(|c| c.contains(model))
        .unwrap_or(false)
}

fn marcar_sem_temperatura(model: &str) {
    if let Ok(mut c) = SEM_TEMPERATURA.get_or_init(Default::default).lock() {
        c.insert(model.to_string());
    }
}

/// Separa o esforço de raciocínio do nome do modelo: `gpt-5.6-luna:low`.
///
/// Vai no próprio nome em vez de virar campo novo porque atravessa tudo que já
/// existe — seletor de modelo, escolha por perfil, histórico — sem migração. É
/// a mesma convenção que o gateway usava em `codex/gpt-5.6-terra-medium`.
fn modelo_e_esforco(modelo: &str) -> (&str, &str) {
    match modelo.split_once(':') {
        Some((m, e)) => (m, e),
        None => (modelo, ""),
    }
}

/// Despacha para a rota que o provedor atende. Só o `ChatgptOauth` sai do
/// dialeto comum.
fn rewrite_no_provedor(
    raw: &str,
    s: &Settings,
    profile: &str,
    p: &Provider,
    modelo: &str,
) -> Result<String, String> {
    if p.kind != ProviderKind::ChatgptOauth {
        return rewrite_openai(raw, s, profile, p, modelo);
    }
    let (m, esforco) = modelo_e_esforco(modelo);
    // A Responses API recusa item `{role:"system"}`: a instrução vai em
    // `instructions` e só a transcrição vai em `input`.
    siwc::responder(m, &build_prompt(s, profile), raw, esforco, p.timeout())
        .map(|t| sem_raciocinio(&t).trim().to_string())
}

fn rewrite(raw: &str, s: &Settings, profile: &str) -> Result<String, String> {
    // Escolha do perfil ganha da global: é a mais específica, pela mesma lógica
    // que ordena os atalhos do mais específico para o mais genérico.
    if let Some((p, modelo)) = alvo_do_perfil(s, profile)? {
        return rewrite_no_provedor(raw, s, profile, p, modelo);
    }
    if !s.rewrite_provider_name.is_empty() {
        match s.providers.iter().find(|p| p.name == s.rewrite_provider_name) {
            Some(p) => return rewrite_no_provedor(raw, s, profile, p, &s.rewrite_model),
            // provedor removido sem trocar a seleção: avisa em vez de falhar calado
            None => {
                return Err(format!(
                    "provedor '{}' não existe mais na aba Provedores",
                    s.rewrite_provider_name
                ))
            }
        }
    }
    if s.rewrite_provider == "groq" {
        return rewrite_groq(raw, s, profile);
    }
    rewrite_gemini(raw, s, profile)
}

/// tira o bloco de raciocínio que alguns modelos devolvem antes da resposta
fn sem_raciocinio(t: &str) -> &str {
    match t.rsplit_once("</think>") {
        Some((_, depois)) => depois,
        None => t,
    }
}

/// Reescrita por qualquer provedor compatível com a API da OpenAI.
///
/// Mesma estratégia do `rewrite_gemini` do upstream: teto curto e UMA segunda
/// tentativa quando estoura. A travada do free tier é isolada — a segunda
/// chamada pega outra fila e costuma responder no tempo normal. Só cabe porque
/// o teto é REWRITE_TIMEOUT; com os 30s antigos a retentativa chegaria depois de
/// o usuário já ter desistido. Uma só: duas somariam o dobro do teto.
fn rewrite_openai(
    raw: &str,
    s: &Settings,
    profile: &str,
    p: &Provider,
    model: &str,
) -> Result<String, String> {
    match rewrite_openai_uma_vez(raw, s, profile, p, model) {
        Err(e) if e.contains("__TIMEOUT__") => {
            eprintln!("[{}] estourou {}s; segunda tentativa", p.name, p.timeout().as_secs());
            rewrite_openai_uma_vez(raw, s, profile, p, model)
                .map_err(|e2| e2.replace("__TIMEOUT__", "tempo esgotado"))
        }
        outro => outro.map_err(|e| e.replace("__TIMEOUT__", "tempo esgotado")),
    }
}

fn rewrite_openai_uma_vez(
    raw: &str,
    s: &Settings,
    profile: &str,
    p: &Provider,
    model: &str,
) -> Result<String, String> {
    if model.trim().is_empty() {
        return Err(format!("nenhum modelo escolhido para o provedor '{}'", p.name));
    }
    // guardado para a retentativa: ela repete a chamada inteira, e o sufixo de
    // esforço faz parte do que o usuário escolheu
    let model_original = model;
    let (model, esforco) = modelo_e_esforco(model);
    let mut body = serde_json::json!({
        "model": model,
        // sem isto o OmniRoute responde em SSE e o corpo deixa de ser JSON
        "stream": false,
        "messages": [{"role": "user", "content": format!("{}{}", build_prompt(s, profile), raw)}]
    });
    // `temperature` baixa mantém a reescrita previsível, mas modelo que pensa
    // antes de responder rejeita o campo — o claude-haiku-5-5 devolve
    // `400 temperature is deprecated for this model`. Quem já falhou uma vez
    // entra no conjunto e não paga a ida perdida de novo.
    if !sem_temperatura(model) {
        body["temperature"] = serde_json::json!(0.2);
    }
    // `reasoning_effort` é o campo do dialeto OpenAI, e é o que a camada de
    // compatibilidade da Anthropic entende. Medido em 14/10 no prompt clínico
    // com claude-haiku-5-5: `low` gastou 2.951 tokens de saída contra 3.681 do
    // `high`. O campo nativo dela (`output_config.effort`) é aceito mas
    // ignorado — a variação saiu na direção errada, que é ruído.
    //
    // Provedor que não conhece o campo ignora em silêncio, que é o
    // comportamento normal para campo desconhecido nesse dialeto.
    if !esforco.is_empty() {
        body["reasoning_effort"] = serde_json::Value::String(esforco.to_string());
    }
    let mut req = rewrite_client()
        .post(p.url("chat/completions"))
        .timeout(p.timeout())
        .header("accept", "application/json")
        .json(&body);
    // a camada de compatibilidade aceita sem, mas mandar protege se ela passar
    // a exigir, como as rotas nativas já exigem
    if p.precisa_versao_anthropic() {
        req = req.header("anthropic-version", ANTHROPIC_VERSION);
    }
    if let Some(k) = p.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let resp = req.send().map_err(|e| {
        // marcador interno: só o wrapper acima entende, e ele o troca por texto
        // legível antes de qualquer coisa chegar ao usuário
        let marca = if e.is_timeout() { "__TIMEOUT__ " } else { "" };
        format!("{marca}{} (reescrita): {e}", p.name)
    })?;
    let status = resp.status();
    // Mesmo remédio do caminho nativo do Gemini, que é onde ele vive no upstream.
    // Aqui é obrigatório repetir porque provedor cadastrado na aba Provedores NÃO
    // passa por rewrite_gemini() — sem isto o fallback não valeria justamente para
    // quem mais precisou dele. Só no Gemini: nos demais provedores o modelo de
    // reserva não existiria.
    let code = status.as_u16();
    if (code == 404 || code == 429 || code == 503)
        && p.base_url.contains("generativelanguage.googleapis.com")
        && model != FALLBACK_MODEL
    {
        eprintln!("[{}] {model} indisponível (HTTP {status}); usando {FALLBACK_MODEL}", p.name);
        // só 404 rebaixa a sessão inteira; 429 e 503 são passageiros e travar
        // neles condenaria o resto da sessão ao reserva por um pico
        if erro_e_definitivo(code) {
            USE_FALLBACK.store(true, Ordering::Relaxed);
        }
        return rewrite_openai(raw, s, profile, p, FALLBACK_MODEL);
    }
    let corpo = resp.text().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&corpo)
        .map_err(|_| format!("{} respondeu HTTP {status} sem JSON: {}", p.name, corta(&corpo)))?;
    if let Some(msg) = v["error"]["message"].as_str() {
        // Modelo que recusa `temperature`: anota e repete sem o campo. A segunda
        // chamada já sai correta, e da próxima vez o campo nem é enviado.
        // `!sem_temperatura` fecha a recursão: se o campo já não foi enviado, a
        // queixa é outra e repetir só daria o mesmo erro para sempre.
        if !status.is_success()
            && msg.to_lowercase().contains("temperature")
            && !sem_temperatura(model)
        {
            eprintln!("[{}] {model} recusa temperature; repetindo sem o campo", p.name);
            marcar_sem_temperatura(model);
            return rewrite_openai_uma_vez(raw, s, profile, p, model_original);
        }
        return Err(format!("{} (reescrita): {msg}", p.name));
    }
    if !status.is_success() {
        return Err(format!("{} respondeu HTTP {status}: {}", p.name, corta(&corpo)));
    }
    let text = sem_raciocinio(v["choices"][0]["message"]["content"].as_str().unwrap_or("")).trim();
    if text.is_empty() {
        return Err(format!("{} devolveu resposta vazia", p.name));
    }
    Ok(text.to_string())
}

fn corta(s: &str) -> String {
    let limpo = s.replace('\n', " ");
    limpo.chars().take(160).collect()
}

/// Reescrita pela Groq, com a mesma chave da transcrição. Latência bem mais
/// estável que o free tier do Gemini, que oscila de 0,5s a mais de 30s.
fn rewrite_groq(raw: &str, s: &Settings, profile: &str) -> Result<String, String> {
    let key = s.groq_api_key.clone().filter(|k| !k.is_empty()).ok_or("chave Groq ausente")?;
    let body = serde_json::json!({
        "model": s.groq_rewrite_model,
        "temperature": 0.2,
        "messages": [{"role": "user", "content": format!("{}{}", build_prompt(s, profile), raw)}]
    });
    let v: serde_json::Value = cloud_client()
        .post("https://api.groq.com/openai/v1/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .send()
        .map_err(|e| format!("Groq (reescrita): {e}"))?
        .json()
        .map_err(|e| e.to_string())?;
    if let Some(msg) = v["error"]["message"].as_str() {
        return Err(format!("Groq (reescrita): {msg}"));
    }
    let text = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
    // modelos com cadeia de raciocínio devolvem <think>...</think> antes da resposta
    let text = match text.rsplit_once("</think>") {
        Some((_, depois)) => depois,
        None => text,
    };
    if text.trim().is_empty() {
        return Err(format!("resposta vazia da Groq: {v}"));
    }
    Ok(text.trim().to_string())
}

/// Modelo de reserva: o acesso a modelos varia por conta/projeto, então a chave
/// de outro usuário pode não ter o modelo configurado (404) ou vê-lo saturado
/// (503/429). Cair aqui é melhor que devolver a transcrição crua — mas SÓ se o
/// reserva for confiável, e o antigo (`gemini-3.1-flash-lite`) não era: medido,
/// ele pendura 25% das chamadas até o timeout (3 de 12), o que transformou o
/// rebaixamento numa sessão inteira de ditados travando. O `-latest` fez 12/12
/// a 1,39s de mediana. Ele é um alias, então o modelo por baixo pode mudar sem
/// aviso — para um RESERVA isso é troca boa: nunca vira 404 quando o Google
/// aposenta uma versão, que é justamente o caso que traz alguém até aqui.
const FALLBACK_MODEL: &str = "gemini-flash-lite-latest";
/// Travado só depois de um 404: sem isso, uma conta sem o modelo preferido
/// pagaria uma chamada perdida em TODO ditado. 429 e 503 NÃO travam — são
/// congestionamento do minuto, e rebaixar a sessão inteira por causa de um pico
/// custaria caro (o 3.1-flash-lite mede 5,5s contra 1,4s do 3.5).
static USE_FALLBACK: AtomicBool = AtomicBool::new(false);

/// Só o 404 é veredito sobre o modelo ("essa chave não tem esse modelo"). 429 e
/// 503 falam do minuto, não do modelo — travar a sessão por causa deles trocaria
/// um pico de segundos por uma sessão inteira no modelo lento.
fn erro_e_definitivo(code: u16) -> bool {
    code == 404
}

fn modelo_efetivo(configurado: &str) -> &str {
    if USE_FALLBACK.load(Ordering::Relaxed) {
        FALLBACK_MODEL
    } else {
        configurado
    }
}

/// Teto por tentativa. O free tier trava requisições isoladas sem relação com o
/// tamanho da entrada — medido em ditados reais do history: 18,1s para 43
/// caracteres no mesmo lote em que 1729 caracteres saíram em 1,8s. E a travada
/// não gruda: refeita na hora, a chamada volta ao tempo normal (24/24 num teste
/// que cortava tudo acima de 2s). Por isso cortar cedo ganha do teto largo —
/// medido, o corte em 2s deu máximo de 4,1s contra 10,0s do corte em 8s.
/// 4s é o meio: acima do p95 real (~3,3s), então quase nenhuma chamada boa
/// morre, e ainda mata o estol antes de o ditado virar prejuízo.
/// Merge de 2026-10-28: o upstream (41839b1) subiu este teto para 8s e tirou a
/// retentativa, medindo p90 de 5,85s no Gemini. O fork manteve 4s + retentativa
/// porque tem teto por provedor (`timeout_secs`) e reescreve por outros provedores;
/// rever com bench/bench_rewrite.py antes de adotar o número do upstream.
const REWRITE_TIMEOUT: Duration = Duration::from_secs(4);

/// Err traz o status HTTP (0 = falha de rede, 408 = estourou o teto) para separar
/// "esse modelo não serve para esta chave" de "a internet caiu".
fn call_gemini(model: &str, key: &str, prompt: &str) -> Result<String, (u16, String)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(REWRITE_TIMEOUT)
        .build()
        .map_err(|e| (0, e.to_string()))?;
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={key}"
    );
    let body = serde_json::json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {"temperature": 0.2}
    });
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .map_err(|e| (if e.is_timeout() { 408 } else { 0 }, format!("Gemini: {e}")))?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        return Err((status, format!("Gemini {model}: HTTP {status}")));
    }
    let v: serde_json::Value = resp.json().map_err(|e| (status, e.to_string()))?;
    let parts = &v["candidates"][0]["content"]["parts"];
    let text = parts
        .as_array()
        .map(|a| a.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""))
        .unwrap_or_default();
    if text.trim().is_empty() {
        return Err((status, format!("resposta vazia do Gemini: {v}")));
    }
    Ok(text.trim().to_string())
}

fn rewrite_gemini(raw: &str, s: &Settings, profile: &str) -> Result<String, String> {
    let key = s
        .gemini_api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| std::env::var("GEMINI_API_KEY").ok())
        .ok_or("chave do Gemini ausente (settings.json ou GEMINI_API_KEY)")?;
    let prompt = format!("{}{}", build_prompt(s, profile), raw);
    let escolhido = modelo_efetivo(&s.gemini_model);
    match call_gemini(escolhido, &key, &prompt) {
        Ok(t) => Ok(t),
        // Modelo fora do ar (404/503) ou cota do minuto estourada (429): o
        // fallback é outro modelo, logo outro balde de cota, então responde na
        // hora. Erro de rede não entra: erraria de novo.
        Err((code, e)) if (code == 404 || code == 429 || code == 503) && escolhido != FALLBACK_MODEL => {
            eprintln!("[gemini] {escolhido} indisponível ({e}); usando {FALLBACK_MODEL}");
            let t = call_gemini(FALLBACK_MODEL, &key, &prompt).map_err(|(_, e2)| e2)?;
            if erro_e_definitivo(code) {
                USE_FALLBACK.store(true, Ordering::Relaxed);
            }
            Ok(t)
        }
        Err((_, e)) => Err(e),
    }
}

fn apply_snippets(text: &str, snippets: &[Snippet]) -> String {
    let mut out = text.to_string();
    for sn in snippets {
        if sn.trigger.is_empty() {
            continue;
        }
        let lower_out = out.to_lowercase();
        let lower_trig = sn.trigger.to_lowercase();
        if let Some(pos) = lower_out.find(&lower_trig) {
            let mut end = pos + lower_trig.len();
            // consome pontuação final colada ao gatilho ("meu instagram." -> link)
            if out[end..].starts_with('.') || out[end..].starts_with(',') {
                end += 1;
            }
            out = format!("{}{}{}", &out[..pos], sn.text, &out[end..]);
        }
    }
    out
}

fn insert_text(text: &str, privado: bool) -> Result<(), String> {
    let mut clip = arboard::Clipboard::new().map_err(|e| {
        app_log("insert.log", &format!("clipboard indisponível: {e}"));
        e.to_string()
    })?;
    let backup = clip.get_text().ok();
    copiar(&mut clip, text, privado).map_err(|e| {
        app_log("insert.log", &format!("set_text falhou: {e}"));
        e.to_string()
    })?;
    std::thread::sleep(Duration::from_millis(60));
    // modificador físico ainda pressionado + V sintético formaria combos do sistema
    platform::wait_modifiers_released(Duration::from_secs(1));
    app_log(
        "insert.log",
        &format!(
            "colando {} chars | alvo={} | mods presos={} | backup={}",
            text.chars().count(),
            platform::foreground_window_desc(),
            platform::modifiers_down(),
            if backup.is_some() { "sim" } else { "não (clipboard vazio)" },
        ),
    );
    match platform::paste_shortcut() {
        Ok(()) => app_log("insert.log", "paste_shortcut retornou ok"),
        Err(e) => {
            app_log("insert.log", &format!("paste_shortcut FALHOU: {e}"));
            return Err(e);
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    if let Some(b) = backup {
        let _ = clip.set_text(b);
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Clone)]
struct HistoryEntry {
    ts: u64,
    profile: String,
    raw: String,
    r#final: String,
    audio_secs: f32,
    stt_secs: f32,
    total_secs: f32,
}

fn append_history(entry: &HistoryEntry) {
    let _ = std::fs::create_dir_all(config_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(history_path()) {
        let _ = writeln!(f, "{}", serde_json::to_string(entry).unwrap());
    }
}

fn app_log(file: &str, msg: &str) {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let _ = std::fs::create_dir_all(config_dir());
    if let Ok(mut f) =
        std::fs::OpenOptions::new().create(true).append(true).open(config_dir().join(file))
    {
        let _ = writeln!(f, "{ts} {msg}");
    }
}

fn overlay_log(msg: &str) {
    app_log("overlay.log", msg);
}

/// mostra o overlay e devolve o número desta exibição
fn overlay_show(app: &AppHandle) -> u64 {
    let shared = app.state::<Arc<Shared>>();
    let gen = shared.overlay_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let style = shared.settings.lock().unwrap().wave_style.clone();
    if let Some(o) = app.get_webview_window("overlay") {
        // monitor onde está o cursor (foco de digitação), não o primário
        let mon = o
            .cursor_position()
            .ok()
            .and_then(|p| o.monitor_from_point(p.x, p.y).ok().flatten())
            .or_else(|| o.primary_monitor().ok().flatten());
        if let (Some(mon), Ok(sz)) = (mon, o.outer_size()) {
            let m = mon.size();
            let x = mon.position().x + (m.width as i32 - sz.width as i32) / 2;
            // 72px fixos + 2% da altura do monitor: só o valor fixo encostava no
            // dock em telas grandes
            let margem = 72 + m.height as i32 * 2 / 100;
            let y = mon.position().y + m.height as i32 - sz.height as i32 - margem;
            if let Err(e) = o.set_position(PhysicalPosition::new(x, y)) {
                overlay_log(&format!("set_position falhou: {e}"));
            }
        }
        // A alternância é o que reergue a janela: o tao só chama SetWindowPos
        // quando a flag ALWAYS_ON_TOP muda de valor (apply_diff), e a janela já
        // nasce alwaysOnTop pelo tauri.conf.json — então `set_always_on_top(true)`
        // sozinho nunca fez nada. Sem reerguer, o overlay fica no fundo da faixa
        // topmost e qualquer outra janela always-on-top criada depois (outro app
        // de ditado, um HUD) o cobre para sempre: ele pinta a onda normalmente e
        // o usuário não vê nada. Medido com PrintWindow durante um ditado real.
        let _ = o.set_always_on_top(false);
        let _ = o.set_always_on_top(true);
        let _ = app.emit_to("overlay", "overlay_show", style);
    } else {
        overlay_log("janela overlay inexistente");
    }
    gen
}

/// só apaga se esta exibição ainda for a atual
///
/// A janela do overlay nasce visível e nunca é escondida: no Windows, esconder e
/// reexibir a janela deixa a webview sem pintar — a janela reaparecia no lugar
/// certo, no topo, do tamanho certo, e mesmo assim invisível, porque o conteúdo
/// nunca era apresentado. Quem "some" e "aparece" agora é só o conteúdo, do lado
/// do React; a janela em si fica parada, transparente e sem capturar cliques.
fn overlay_hide(app: &AppHandle, gen: u64) {
    if app.state::<Arc<Shared>>().overlay_gen.load(Ordering::SeqCst) != gen {
        return;
    }
    let _ = app.emit_to("overlay", "overlay_hide", ());
}

fn overlay_status(app: &AppHandle, state: &str, message: &str) {
    let _ = app.emit_to(
        "overlay",
        "overlay_status",
        serde_json::json!({ "state": state, "message": message }),
    );
}

/// mensagem técnica -> frase curta que cabe no overlay
fn friendly_error(e: &str) -> String {
    let low = e.to_lowercase();
    let msg = if low.contains("microfone") || low.contains("sem microfone") {
        "Microfone não encontrado"
    } else if low.contains("429") || low.contains("rate limit") || low.contains("quota") {
        "Limite da API atingido"
    } else if low.contains("401") || low.contains("403") || low.contains("chave") {
        "Chave de API inválida ou ausente"
    } else if low.contains("timed out")
        || low.contains("timeout")
        || low.contains("dns")
        || low.contains("connect")
    {
        "Sem conexão com o serviço"
    } else if low.contains("transcrição local") {
        "Transcrição local indisponível"
    } else if low.contains("alucinação") || low.contains("transcrição vazia") {
        "Nada foi dito"
    } else if low.contains("clipboard") || low.contains("área de transferência") {
        "Não consegui colar o texto"
    } else {
        return e.chars().take(70).collect();
    };
    msg.to_string()
}

/// mostra o motivo da falha por alguns segundos — sem isso o ditado some em silêncio
fn overlay_fail(app: &AppHandle, gen: u64, err: &str) {
    if app.state::<Arc<Shared>>().overlay_gen.load(Ordering::SeqCst) != gen {
        return;
    }
    overlay_status(app, "error", &friendly_error(err));
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(3200));
        overlay_hide(&app, gen);
    });
}

/// cada toast recebe um número; só o mais recente pode se apagar sozinho
static TOAST_SEQ: AtomicU64 = AtomicU64::new(0);

/// balão discreto acima do overlay, dizendo qual perfil está em uso
fn toast_show(app: &AppHandle, text: &str) {
    let Some(w) = app.get_webview_window("toast") else {
        overlay_log("janela toast inexistente");
        return;
    };
    let mon = w
        .cursor_position()
        .ok()
        .and_then(|p| w.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| w.primary_monitor().ok().flatten());
    if let (Some(mon), Ok(sz)) = (mon, w.outer_size()) {
        let m = mon.size();
        let x = mon.position().x + (m.width as i32 - sz.width as i32) / 2;
        // acima do overlay das ondas, para os dois poderem aparecer juntos
        let y = mon.position().y + m.height as i32 - sz.height as i32 - 128;
        let _ = w.set_position(PhysicalPosition::new(x, y));
    }
    let _ = w.set_always_on_top(true);
    let _ = w.show();
    let _ = app.emit_to("toast", "toast", text.to_string());

    let seq = TOAST_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1600));
        if TOAST_SEQ.load(Ordering::SeqCst) == seq {
            if let Some(w) = app.get_webview_window("toast") {
                let _ = w.hide();
            }
        }
    });
}

/// avança o perfil ativo e avisa na tela
fn cycle_profile(shared: &Arc<Shared>) {
    let s = {
        let mut g = shared.settings.lock().unwrap();
        if g.profiles.is_empty() {
            return;
        }
        let i = g.profiles.iter().position(|p| p.name == g.active_profile).unwrap_or(0);
        g.active_profile = g.profiles[(i + 1) % g.profiles.len()].name.clone();
        g.clone()
    };
    let _ = persist_settings(&s);
    println!("[hotkey] perfil ativo: {}", s.active_profile);
    if let Some(app) = shared.app.get() {
        rebuild_tray_menu(app, &s);
        let _ = app.emit("settings_changed", ());
        toast_show(app, &s.active_profile);
    }
}

enum Cmd {
    /// `Some(perfil)` dita naquele estilo sem mexer no perfil ativo
    Start(Option<String>),
    Stop,
    Cancel,
    /// transcreve de novo um áudio salvo e cola onde estiver o cursor
    Retranscribe(std::path::PathBuf),
}

/// "Não guardar" (0) vale para a aba Áudios, não para os ditados do laudo:
/// esses existem para outro programa buscar, e apagar tudo os perderia.
fn prazo_ditados(audio_keep_days: u32) -> u32 {
    if audio_keep_days == 0 { 7 } else { audio_keep_days }
}

/// Põe o texto na área de transferência. `privado` marca o conteúdo para não
/// entrar no histórico do Win+V nem na nuvem (no macOS, no histórico dos
/// gerenciadores de clipboard): é texto de paciente.
fn copiar(clip: &mut arboard::Clipboard, texto: &str, privado: bool) -> Result<(), arboard::Error> {
    if privado {
        #[cfg(windows)]
        {
            use arboard::SetExtWindows;
            // monitoring cobre histórico e nuvem; a doc do arboard pede um só
            return clip.set().exclude_from_monitoring().text(texto);
        }
        #[cfg(target_os = "macos")]
        {
            use arboard::SetExtApple;
            return clip.set().exclude_from_history().text(texto);
        }
    }
    clip.set_text(texto)
}

/// Texto pronto para colar.
struct Pronto {
    texto: String,
    /// motivo para o balão quando o texto saiu sem a correção esperada
    aviso: Option<String>,
    /// perfil do laudo: o ditado já está na pasta `ditados`; cola o cru fora
    /// do histórico do clipboard e não guarda para a bandeja
    salvo: bool,
}

type Resultado = Result<Pronto, String>;

/// Reserva a posição do ditado na fila de colagem e processa em thread própria:
/// a thread do atalho fica livre e o próximo Start é imediato.
#[allow(clippy::too_many_arguments)]
fn enfileirar(
    shared: &Arc<Shared>,
    app: &AppHandle,
    insert_tx: &Sender<(u64, Receiver<Resultado>)>,
    gen: u64,
    wav: Vec<u8>,
    secs: f32,
    forced_profile: Option<String>,
    snap: Option<Profile>,
    arquivo: Option<std::path::PathBuf>,
    ditado: Option<ditados::Ditado>,
) {
    // overlay segue no ar em "processando": soltar o atalho e ver a
    // janela sumir por 2-3s é indistinguível de ter falhado
    overlay_status(app, "processing", "");
    let s = shared.settings.lock().unwrap().clone();
    let profile = forced_profile.unwrap_or_else(|| s.active_profile.clone());
    let shared_w = shared.clone();
    // Reservar a posição na thread de captura, antes de processar:
    // ordem de conclusão das APIs nunca decide a ordem da colagem.
    let (result_tx, result_rx) = channel();
    if insert_tx.send((gen, result_rx)).is_err() {
        overlay_fail(app, gen, "Fila de inserção indisponível");
        return;
    }
    std::thread::spawn(move || {
        let result = match &ditado {
            Some(d) => transcrever_ditado(wav, secs, &shared_w, &s, d, snap.as_ref()),
            None => transcrever(wav, secs, &shared_w, &s, &profile, snap.as_ref()),
        };
        if let Some(a) = &arquivo {
            match &result {
                Ok(_) => audios::limpar_falha(a),
                Err(e) => audios::marcar_falha(a, e),
            }
        }
        let _ = result_tx.send(result);
    });
}

/// STT, reescrita, snippets e histórico. Devolve o texto final e, se a
/// reescrita falhou, o motivo para o balão.
/// `snap` é o perfil como estava no início da gravação: trocar ou renomear o
/// perfil no meio não pode mandar para a nuvem um ditado que começou só local.
fn transcrever(
    wav: Vec<u8>,
    secs: f32,
    shared: &Shared,
    s: &Settings,
    profile: &str,
    snap: Option<&Profile>,
) -> Resultado {
    let t0 = Instant::now();
    let prof = snap.or_else(|| s.profiles.iter().find(|p| p.name == profile));
    let raw = run_stt(wav, shared, s, prof)?;
    if raw.is_empty() {
        return Err("transcrição vazia".into());
    }
    if is_hallucination(&raw, secs) {
        return Err(format!("alucinação de silêncio descartada: {raw:?}"));
    }
    let t_stt = t0.elapsed().as_secs_f32();
    // Reescrita que falha NÃO aborta o ditado: o texto bruto do
    // Whisper ainda é útil. Mas falhar em silêncio já custou dias
    // duas vezes — modelo descontinuado e modelo inexistente na
    // conta — porque o único sintoma era "o texto saiu sem
    // correção". O motivo vai para o balão depois da colagem.
    let mut aviso: Option<String> = None;
    let final_text = match rewrite(&raw, s, profile) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[pipeline] reescrita falhou ({e}); usando texto bruto");
            aviso = Some(friendly_error(&e));
            raw.clone()
        }
    };
    let final_text = apply_snippets(&final_text, &s.snippets);
    let total = t0.elapsed().as_secs_f32();
    println!("[pipeline] áudio {secs:.1}s | stt {t_stt:.2}s | total {total:.2}s");
    if s.history_enabled {
        append_history(&HistoryEntry {
            ts: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            profile: profile.to_string(),
            raw,
            r#final: final_text.clone(),
            audio_secs: secs,
            stt_secs: t_stt,
            total_secs: total,
        });
    }
    Ok(Pronto { texto: final_text, aviso, salvo: false })
}

/// Perfil "só salvar": STT e o `.json` na pasta `ditados`, sem reescrita, sem
/// histórico e sem colar. A falha também vira `.json` (status "falhou"), para
/// quem busca a pasta saber que aquele `.wav` não tem texto.
fn transcrever_ditado(
    wav: Vec<u8>,
    secs: f32,
    shared: &Shared,
    s: &Settings,
    d: &ditados::Ditado,
    snap: Option<&Profile>,
) -> Resultado {
    // perfil sumiu no meio: o ditado nasceu "salvar para o laudo", então segue só local
    let fallback = Profile { name: d.profile.clone(), save_only: true, ..Default::default() };
    let prof = snap.or_else(|| s.profiles.iter().find(|p| p.name == d.profile)).unwrap_or(&fallback);
    let mut stt = "sidecar local (sem resposta)".to_string();
    let mut stt_secs = None;
    let raw = run_stt_com_origem(wav, shared, s, Some(prof)).and_then(|(t, o)| {
        if let Some(o) = o {
            stt = format!("{} local {}", o.modelo, o.dispositivo);
            stt_secs = Some(o.segundos);
        }
        if t.is_empty() {
            Err("transcrição vazia".into())
        } else if is_hallucination(&t, secs) {
            Err(format!("alucinação de silêncio descartada: {t:?}"))
        } else {
            Ok(t)
        }
    });
    let gravou = d.salvar_json(secs, &stt, stt_secs, raw.as_deref().map_err(String::as_str));
    let raw = raw?;
    gravou.map_err(|e| format!("transcreveu, mas o ditado não foi salvo: {e}"))?;
    println!("[ditados] salvo ({secs:.1}s de áudio, {} caracteres)", raw.chars().count());
    Ok(Pronto { texto: raw, aviso: None, salvo: true })
}

fn spawn_pipeline(shared: Arc<Shared>, app: AppHandle) -> Sender<Cmd> {
    let (tx, rx) = channel::<Cmd>();
    let (insert_tx, insert_rx) = channel();
    let shared_insert = shared.clone();
    let app_insert = app.clone();
    std::thread::spawn(move || {
        insertion_queue::consume_in_order(insert_rx, |gen, result| {
            let result = result
                .map_err(|_| "Processamento do ditado interrompido".to_string())
                .and_then(|result: Resultado| result)
                .and_then(|p: Pronto| {
                    if p.salvo {
                        // texto de paciente: colado fora do Win+V e não guardado para a bandeja
                        return insert_text(&p.texto, true).map(|()| Some("ditado salvo para o laudo".to_string()));
                    }
                    // Preservar a recuperação pela bandeja mesmo se a colagem falhar.
                    *shared_insert.last_text.lock().unwrap() = p.texto.clone();
                    insert_text(&p.texto, false).map(|()| p.aviso.map(|m| format!("texto sem correção — {m}")))
                });
            match result {
                // colou, mas sem reescrita: o texto é o bruto do Whisper
                Ok(Some(balao)) => {
                    overlay_hide(&app_insert, gen);
                    toast_show(&app_insert, &balao);
                }
                Ok(None) => overlay_hide(&app_insert, gen),
                Err(e) => {
                    eprintln!("[pipeline] erro: {e}");
                    overlay_fail(&app_insert, gen, &e);
                }
            }
        });
    });
    std::thread::spawn(move || {
        let mut rec = Recorder::new();
        // perfil pedido pelo atalho que iniciou a gravação em curso
        let mut pending_profile: Option<String> = None;
        // perfil "só salvar": o nome do arquivo é a hora do início da gravação
        let mut pending_ditado: Option<ditados::Ditado> = None;
        // o perfil como estava no Start: é ele que decide nuvem ou local
        let mut pending_snap: Option<Profile> = None;
        let mut gen = 0u64;
        let mut gravando = false;
        loop {
            match rx.recv() {
                Ok(Cmd::Start(profile)) => {
                    if gravando {
                        eprintln!("[pipeline] Start com gravação em curso ignorado");
                        continue;
                    }
                    shared.falhou_ao_gravar.store(false, Ordering::SeqCst);
                    let (nome, snap) = {
                        let g = shared.settings.lock().unwrap();
                        let nome = profile.unwrap_or_else(|| g.active_profile.clone());
                        let snap = g.profiles.iter().find(|p| p.name == nome).cloned();
                        (nome, snap)
                    };
                    pending_ditado =
                        snap.as_ref().is_some_and(|p| p.save_only).then(|| ditados::Ditado::novo(nome.clone()));
                    pending_profile = Some(nome);
                    pending_snap = snap;
                    gravando = true;
                    gen = overlay_show(&app);
                    let mic = shared.settings.lock().unwrap().mic.clone();
                    let app_lvl = app.clone();
                    let mut last = Instant::now() - Duration::from_secs(1);
                    let on_level: LevelFn = Box::new(move |rms| {
                        if last.elapsed() >= Duration::from_millis(40) {
                            last = Instant::now();
                            let _ = app_lvl.emit("level", rms);
                        }
                    });
                    if let Err(e) = rec.start(mic.as_deref(), on_level) {
                        gravando = false;
                        shared.falhou_ao_gravar.store(true, Ordering::SeqCst);
                        eprintln!("[pipeline] gravação falhou: {e}");
                        overlay_fail(&app, gen, &e);
                    } else {
                        println!("[pipeline] gravando...");
                    }
                }
                Ok(Cmd::Cancel) => {
                    gravando = false;
                    pending_ditado = None;
                    pending_snap = None;
                    let _ = rec.stop();
                    overlay_hide(&app, gen);
                    println!("[pipeline] ditado cancelado");
                }
                Ok(Cmd::Stop) => {
                    // o overlay só sai depois da colagem: some antes = parece ter falhado
                    let forced_profile = pending_profile.take();
                    let ditado = pending_ditado.take();
                    let snap = pending_snap.take();
                    gravando = false;
                    let (samples, rate) = rec.stop();
                    let secs = samples.len() as f32 / rate as f32;
                    if secs < 0.4 {
                        println!("[pipeline] áudio curto demais ({secs:.1}s), ignorado");
                        overlay_hide(&app, gen);
                        continue;
                    }
                    let peak = peak_rms(&samples, rate);
                    if peak < 0.006 {
                        println!("[pipeline] sem fala detectada (pico rms {peak:.4}), ignorado");
                        overlay_hide(&app, gen);
                        continue;
                    }
                    let wav = wav_bytes(&samples, rate);
                    // cópia em disco ANTES do STT: se ele falhar, a fala ainda existe
                    // e o menu do ícone ou a aba Áudios transcrevem de novo
                    // "Salvar histórico localmente" desligado vale para o áudio também
                    if let Some(d) = &ditado {
                        if let Err(e) = d.salvar_wav(&wav) {
                            eprintln!("[ditados] áudio não foi salvo: {e}");
                            toast_show(&app, &format!("áudio do laudo não foi salvo — {e}"));
                        }
                    }
                    // o ditado do laudo já tem a cópia dele na pasta `ditados`, e o
                    // de perfil só local não pode ficar onde a aba Áudios o
                    // retranscreveria pela nuvem
                    let sensivel = snap.as_ref().is_some_and(|p| p.stt_local_only || p.save_only);
                    let guardar = ditado.is_none() && !sensivel && {
                        let g = shared.settings.lock().unwrap();
                        g.history_enabled && g.audio_keep_days > 0
                    };
                    let arquivo = if guardar {
                        audios::salvar(&wav)
                            .map_err(|e| {
                                eprintln!("[pipeline] áudio não foi salvo: {e}");
                                toast_show(&app, &format!("áudio não foi guardado — {e}"));
                            })
                            .ok()
                    } else {
                        None
                    };
                    enfileirar(&shared, &app, &insert_tx, gen, wav, secs, forced_profile, snap, arquivo, ditado);
                }
                Ok(Cmd::Retranscribe(arquivo)) => {
                    // durante uma gravação o overlay é dela: trocar o dono aqui
                    // deixaria o ditado em curso sem conseguir fechar a janela
                    if gravando {
                        toast_show(&app, "termine a gravação antes de transcrever o último áudio");
                        continue;
                    }
                    gen = overlay_show(&app);
                    match audios::carregar(&arquivo) {
                        Ok((wav, secs)) => {
                            enfileirar(&shared, &app, &insert_tx, gen, wav, secs, None, None, Some(arquivo), None)
                        }
                        Err(e) => overlay_fail(&app, gen, &e),
                    }
                }
                Err(_) => break,
            }
        }
    });
    tx
}

// Estado compartilhado do rastreador de atalho: alimentado pelo hook LL no
// Windows e pelo CGEventTap no macOS (ambos em platform::).
struct HookState {
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    pressed: HashSet<u32>,
    /// atalho fisicamente pressionado agora — enquanto existe, os outros são ignorados
    latched: Option<String>,
    /// atalho que iniciou a gravação em curso (só ele pode encerrá-la)
    recording: Option<String>,
    /// quando o atalho que iniciou a gravação foi apertado: decide, ao soltar,
    /// se foi toque (segue gravando) ou segurar (para)
    started_at: Option<Instant>,
    /// teclas cuja descida foi engolida: só a soltura delas é engolida também,
    /// senão o app em foco ficaria com a tecla presa (F2 antes do Alt)
    engolidas: HashSet<u32>,
}

/// Limiar do atalho misto, o mesmo do gravador do TOMO (`SEGURAR` em
/// tomo/gravar.py): soltar antes disso é toque e a gravação continua até o
/// próximo toque; soltar depois é segurar e a gravação para ali.
const SEGURAR: Duration = Duration::from_millis(600);

fn binding_satisfied(b: &Binding, pressed: &HashSet<u32>) -> bool {
    !b.groups.is_empty() && b.groups.iter().all(|g| g.iter().any(|k| pressed.contains(k)))
}

/// Processa uma tecla física. Devolve `true` quando ela é a tecla comum (não
/// modificador) de um atalho disparado e deve ser engolida: sem isso o F2 de
/// Alt+F2 chegaria também ao app em foco. A soltura só é engolida se a descida
/// foi: descida que passou e soltura engolida deixariam a tecla presa no app.
fn handle_key(st: &mut HookState, vk: u32, down: bool) -> bool {
    // repetição automática segue o que foi decidido na primeira descida: se ela
    // chegou ao app, as repetições e a soltura também chegam
    let repeticao = down && st.pressed.contains(&vk);
    processar_tecla(st, vk, down);
    if repeticao {
        return st.engolidas.contains(&vk);
    }
    if !down {
        return st.engolidas.remove(&vk);
    }
    let engolir = tecla_comum_do_atalho(st, vk);
    if engolir {
        st.engolidas.insert(vk);
    } else {
        // soltura perdida antes (UAC, tela de bloqueio): não engolir a próxima
        st.engolidas.remove(&vk);
    }
    engolir
}

fn processar_tecla(st: &mut HookState, vk: u32, down: bool) {
    if down {
        st.pressed.insert(vk);
    } else {
        st.pressed.remove(&vk);
    }

    // aborta o ditado em andamento sem inserir nada. No modo hold o atalho segue
    // pressionado, mas `latched` continua apontando para ele: ao soltar, o Release
    // não acha mais a gravação e não dispara Stop.
    if down && st.recording.is_some() && *st.shared.cancel.lock().unwrap() == Some(vk) {
        st.recording = None;
        st.started_at = None;
        let _ = st.tx.send(Cmd::Cancel);
        return;
    }

    enum Step {
        Nothing,
        /// o atalho pressionado foi solto
        Release,
        Fire { id: String, act: Act },
    }

    // decide com o cadeado dos vínculos na mão e age depois de soltá-lo:
    // agir aqui dentro travaria junto com o cadeado das configurações
    let step = {
        let bindings = st.shared.bindings.lock().unwrap();
        match &st.latched {
            Some(id) => match bindings.iter().find(|b| &b.id == id) {
                Some(b) if binding_satisfied(b, &st.pressed) => Step::Nothing,
                Some(_) => Step::Release,
                // o atalho sumiu (configuração mudou no meio): encerra o que estiver aberto
                None => {
                    st.started_at = Some(Instant::now() - SEGURAR);
                    Step::Release
                }
            },
            // a lista vem do mais específico para o mais genérico
            None => match bindings.iter().find(|b| binding_satisfied(b, &st.pressed)) {
                Some(b) => Step::Fire { id: b.id.clone(), act: b.act.clone() },
                None => Step::Nothing,
            },
        }
    };

    match step {
        Step::Nothing => {}
        Step::Release => {
            let id = st.latched.take();
            // toque curto: a gravação segue até o próximo toque do mesmo atalho
            let segurou = st.started_at.is_some_and(|t| t.elapsed() >= SEGURAR);
            if segurou && id.is_some() && st.recording == id {
                st.recording = None;
                st.started_at = None;
                let _ = st.tx.send(Cmd::Stop);
            }
        }
        Step::Fire { id, act } => {
            st.latched = Some(id.clone());
            match act {
                // fora da thread do hook: o WH_KEYBOARD_LL é removido pelo Windows se o
                // callback demorar, e isto grava arquivo e mexe em janela
                Act::CycleProfile => {
                    let shared = st.shared.clone();
                    std::thread::spawn(move || cycle_profile(&shared));
                }
                Act::Record { profile } => {
                    // a gravação não chegou a começar (microfone falhou): sem isto
                    // o próximo toque viraria um Stop mudo em vez de um Start
                    if st.recording.is_some() && st.shared.falhou_ao_gravar.swap(false, Ordering::SeqCst) {
                        st.recording = None;
                        st.started_at = None;
                    }
                    if st.recording.is_some() {
                        // gravando (depois de um toque): só o mesmo atalho encerra
                        if st.recording.as_deref() == Some(id.as_str()) {
                            st.recording = None;
                            st.started_at = None;
                            let _ = st.tx.send(Cmd::Stop);
                        }
                        return;
                    }
                    println!("[hotkey] {id} ativado");
                    if let (Some(p), Some(app)) = (profile.clone(), st.shared.app.get()) {
                        let app = app.clone();
                        std::thread::spawn(move || toast_show(&app, &p));
                    }
                    st.recording = Some(id);
                    st.started_at = Some(Instant::now());
                    // o aviso de falha passa a valer só para este Start
                    st.shared.falhou_ao_gravar.store(false, Ordering::SeqCst);
                    let _ = st.tx.send(Cmd::Start(profile));
                }
            }
        }
    }
}

/// A tecla faz parte do atalho travado agora e não é modificador? Modificador
/// vem de `modifier_group` com as variantes esquerda e direita (grupo com mais
/// de uma tecla); tecla comum é grupo de uma só.
fn tecla_comum_do_atalho(st: &HookState, vk: u32) -> bool {
    let Some(id) = &st.latched else { return false };
    let bindings = st.shared.bindings.lock().unwrap();
    bindings
        .iter()
        .find(|b| &b.id == id)
        .is_some_and(|b| b.groups.iter().any(|g| g.len() == 1 && g[0] == vk))
}

#[tauri::command]
fn get_settings(state: tauri::State<Arc<Shared>>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let m = app.autolaunch();
    if enabled {
        m.enable().map_err(|e| e.to_string())
    } else {
        m.disable().map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    state: tauri::State<Arc<Shared>>,
    settings: Settings,
) -> Result<(), String> {
    persist_settings(&settings)?;
    // prazo menor (ou "não guardar") vale na hora, sem esperar o ciclo de 6h
    if settings.audio_keep_days != state.settings.lock().unwrap().audio_keep_days {
        audios::limpar_antigos(settings.audio_keep_days);
        ditados::limpar_antigos(prazo_ditados(settings.audio_keep_days));
    }
    *state.bindings.lock().unwrap() = build_bindings(&settings);
    *state.cancel.lock().unwrap() = parse_cancel_key(&settings.cancel_key);
    let warm_local = settings.stt_provider == "local" || settings.tem_perfil_local();
    rebuild_tray_menu(&app, &settings);
    *state.settings.lock().unwrap() = settings;
    if warm_local {
        let shared = state.inner().clone();
        std::thread::spawn(move || {
            let s = shared.settings.lock().unwrap().clone();
            if let Err(e) = ensure_sidecar(&shared, &s) {
                eprintln!("[stt] falha ao aquecer local: {e}");
            }
        });
    }
    Ok(())
}

#[derive(Serialize)]
struct ProviderTest {
    ok: bool,
    /// modelos do catálogo, ou vazio quando o provedor não expõe `/models`
    models: Vec<String>,
    /// mensagem para o usuário: quantos modelos, ou o motivo da falha
    message: String,
    /// provedor respondeu mas sem catálogo (Gemini é assim): o modelo vai na mão
    no_catalog: bool,
    /// tem `/audio/transcriptions`, ou seja, serve para transcrever
    audio: bool,
    /// subconjunto de `models` que o catálogo declara como transcrição
    transcription_models: Vec<String>,
    /// como esse subconjunto foi obtido, para a interface ser honesta
    transcription_source: String,
}

/// O catálogo declara que este modelo transcreve?
///
/// Groq marca `output_modalities: ["transcription"]`; o OmniRoute usa
/// `type: "audio"` com `subtype: "transcription"`. Só aceito esses dois sinais
/// explícitos — "aceita áudio na entrada" não serve, porque no OpenRouter isso
/// pega 25 modelos de chat multimodal que não atendem em /audio/transcriptions.
fn declara_transcricao(m: &serde_json::Value) -> bool {
    let saida_transcricao = m["output_modalities"]
        .as_array()
        .map(|a| a.iter().any(|x| x.as_str() == Some("transcription")))
        .unwrap_or(false);
    let tipo_audio = m["type"].as_str() == Some("audio")
        && m["subtype"].as_str().map(|s| s.contains("transcri")).unwrap_or(false);
    saida_transcricao || tipo_audio
}

/// Último recurso quando o catálogo não classifica nada: nome do modelo.
fn parece_transcricao(id: &str) -> bool {
    let s = id.to_lowercase();
    ["whisper", "transcri", "speech-to-text", "voxtral", "-stt", "/stt"]
        .iter()
        .any(|p| s.contains(p))
}

/// Descobre se o provedor transcreve. Manda um multipart vazio de propósito:
/// 404 significa que a rota não existe; qualquer outra resposta (tipicamente
/// 400 reclamando do corpo) significa que existe. Não gasta tokens.
fn probe_audio(p: &Provider) -> bool {
    let mut req = cloud_client()
        .post(p.url("audio/transcriptions"))
        .multipart(reqwest::blocking::multipart::Form::new());
    if let Some(k) = p.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    match req.send() {
        Ok(r) => r.status() != reqwest::StatusCode::NOT_FOUND,
        Err(_) => false,
    }
}

/// Testa a conexão e lista os modelos. Só roda quando o usuário clica —
/// abrir a aba não gasta chamada de API.
/// Abre o navegador e espera a autorização. Bloqueia até cinco minutos — o
/// Tauri roda comandos fora da thread da interface, então a janela continua viva.
#[tauri::command]
fn siwc_conectar(app: AppHandle) -> Result<siwc::Conta, String> {
    siwc::conectar(|url| {
        use tauri_plugin_opener::OpenerExt;
        if let Err(e) = app.opener().open_url(url, None::<&str>) {
            eprintln!("[siwc] não consegui abrir o navegador: {e}");
        }
    })
}

#[tauri::command]
fn siwc_conta() -> Option<siwc::Conta> {
    siwc::conta()
}

#[tauri::command]
fn siwc_desconectar() -> Result<(), String> {
    siwc::desconectar()
}

/// Catálogo da assinatura. Devolve os modelos puros: o esforço virou seletor
/// próprio na interface, que monta o sufixo `modelo:esforço` sozinha.
#[tauri::command]
fn siwc_modelos() -> Result<Vec<String>, String> {
    siwc::modelos()
}

#[tauri::command]
fn test_provider(base_url: String, api_key: Option<String>) -> ProviderTest {
    let p = Provider {
        name: String::new(),
        base_url,
        api_key,
        timeout_secs: 0,
        kind: ProviderKind::ApiKey,
    };
    if p.base_url.trim().is_empty() {
        return ProviderTest {
            ok: false,
            models: vec![],
            message: "informe o endereço base".into(),
            no_catalog: false,
            audio: false,
            transcription_models: vec![],
            transcription_source: String::new(),
        };
    }
    let mut req = cloud_client().get(p.url("models")).header("accept", "application/json");
    if p.precisa_versao_anthropic() {
        req = req.header("anthropic-version", ANTHROPIC_VERSION);
    }
    if let Some(k) = p.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let resp = match req.send() {
        Ok(r) => r,
        Err(e) => {
            return ProviderTest {
                ok: false,
                models: vec![],
                message: format!("não consegui conectar: {e}"),
                no_catalog: false,
                audio: false,
                transcription_models: vec![],
                transcription_source: String::new(),
            }
        }
    };
    let status = resp.status();
    let corpo = resp.text().unwrap_or_default();
    if status == reqwest::StatusCode::NOT_FOUND {
        return ProviderTest {
            ok: true,
            models: vec![],
            message: "conectado, mas este provedor não publica catálogo — informe o modelo à mão".into(),
            no_catalog: true,
            audio: probe_audio(&p),
            transcription_models: vec![],
            transcription_source: "sem catálogo — informe o modelo à mão".into(),
        };
    }
    if !status.is_success() {
        let detalhe = serde_json::from_str::<serde_json::Value>(&corpo)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| corta(&corpo));
        return ProviderTest {
            ok: false,
            models: vec![],
            message: format!("HTTP {status}: {detalhe}"),
            no_catalog: false,
            audio: false,
            transcription_models: vec![],
            transcription_source: String::new(),
        };
    }
    let v: serde_json::Value = match serde_json::from_str(&corpo) {
        Ok(v) => v,
        Err(_) => {
            return ProviderTest {
                ok: false,
                models: vec![],
                message: format!("resposta não é JSON: {}", corta(&corpo)),
                no_catalog: false,
                audio: false,
                transcription_models: vec![],
                transcription_source: String::new(),
            }
        }
    };
    let itens = v["data"].as_array().cloned().unwrap_or_default();
    let mut models: Vec<String> =
        itens.iter().filter_map(|m| m["id"].as_str().map(|s| s.to_string())).collect();
    models.sort();

    // primeiro o que o catálogo declara; só se não declarar nada, cai no nome
    let mut transcription_models: Vec<String> = itens
        .iter()
        .filter(|m| declara_transcricao(m))
        .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
        .collect();
    let mut transcription_source = "declarado pelo provedor".to_string();
    if transcription_models.is_empty() {
        transcription_models = models.iter().filter(|id| parece_transcricao(id)).cloned().collect();
        transcription_source = if transcription_models.is_empty() {
            "o catálogo não identifica modelos de transcrição".into()
        } else {
            "deduzido pelo nome — o catálogo não classifica".into()
        };
    }
    transcription_models.sort();
    if models.is_empty() {
        return ProviderTest {
            ok: true,
            models,
            message: "conectado, mas o catálogo veio vazio — informe o modelo à mão".into(),
            no_catalog: true,
            audio: probe_audio(&p),
            transcription_models: vec![],
            transcription_source: "sem catálogo — informe o modelo à mão".into(),
        };
    }
    let audio = probe_audio(&p);
    ProviderTest {
        ok: true,
        message: format!(
            "conectado — {} modelos{}",
            models.len(),
            if audio {
                format!(", {} de transcrição", transcription_models.len())
            } else {
                ", sem endpoint de transcrição".to_string()
            }
        ),
        models,
        no_catalog: false,
        audio,
        transcription_models,
        transcription_source,
    }
}

#[tauri::command]
fn list_mics() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

#[tauri::command]
fn get_history(query: Option<String>, limit: Option<usize>) -> Vec<HistoryEntry> {
    let q = query.unwrap_or_default().to_lowercase();
    let limit = limit.unwrap_or(50);
    let Ok(txt) = std::fs::read_to_string(history_path()) else {
        return Vec::new();
    };
    txt.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
        .filter(|e| {
            q.is_empty()
                || e.r#final.to_lowercase().contains(&q)
                || e.raw.to_lowercase().contains(&q)
        })
        .take(limit)
        .collect()
}

#[tauri::command]
fn list_audios() -> Vec<audios::Audio> {
    audios::listar()
}

/// bytes crus do WAV para o player da aba Áudios (sem base64 no meio)
#[tauri::command]
fn read_audio(name: String) -> Result<tauri::ipc::Response, String> {
    let p = audios::caminho(&name).ok_or("nome de áudio inválido")?;
    std::fs::read(p).map(tauri::ipc::Response::new).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct Retranscricao {
    texto: String,
    /// motivo, quando a reescrita falhou e o texto é o bruto do STT
    aviso: Option<String>,
}

/// Retranscreve um áudio escolhido na aba Áudios. O texto vai só para o
/// clipboard: nessa hora o cursor está nesta janela, não onde se quer colar.
#[tauri::command(async)]
fn transcribe_audio(state: tauri::State<'_, Arc<Shared>>, name: String) -> Result<Retranscricao, String> {
    let p = audios::caminho(&name).ok_or("nome de áudio inválido")?;
    let (wav, secs) = audios::carregar(&p)?;
    let s = state.settings.lock().unwrap().clone();
    let result = transcrever(wav, secs, &state, &s, &s.active_profile, None);
    match &result {
        Ok(_) => audios::limpar_falha(&p),
        Err(e) => audios::marcar_falha(&p, e),
    }
    let Pronto { texto, aviso, .. } = result.map_err(|e| friendly_error(&e))?;
    *state.last_text.lock().unwrap() = texto.clone();
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(texto.clone()))
        .map_err(|e| format!("transcreveu, mas o clipboard falhou: {e}"))?;
    Ok(Retranscricao { texto, aviso })
}

#[tauri::command]
fn open_ditados_dir(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let d = ditados::dir();
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    app.opener().open_path(d.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_audio_dir(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let d = audios::dir();
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    app.opener().open_path(d.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn clear_history() -> Result<(), String> {
    match std::fs::remove_file(history_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[derive(Serialize)]
struct Diagnostics {
    stt_provider: String,
    groq_key_set: bool,
    sidecar_ok: bool,
    sidecar_configured: bool,
    sidecar_device: String,
    default_mic: String,
    gemini_key_set: bool,
    gemini_model: String,
    recent: Vec<HistoryEntry>,
}

#[tauri::command]
fn get_diagnostics(state: tauri::State<Arc<Shared>>) -> Diagnostics {
    let s = state.settings.lock().unwrap().clone();
    let (sidecar_ok, sidecar_device) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()
        .and_then(|c| c.get(format!("http://127.0.0.1:{}/health", s.stt_port)).send().ok())
        .and_then(|r| r.json::<serde_json::Value>().ok())
        .map(|v| (true, v["device"].as_str().unwrap_or("?").to_string()))
        .unwrap_or((false, "offline".into()));
    let default_mic = cpal::default_host()
        .default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_else(|| "nenhum".into());
    // sem os dois caminhos no disco o modo local nunca sobe — e "descarregado"
    // parece normal, escondendo que ele simplesmente não existe nesta máquina
    let sidecar_configured =
        std::path::Path::new(&s.python).exists() && std::path::Path::new(&s.sidecar).exists();
    Diagnostics {
        stt_provider: s.stt_provider.clone(),
        groq_key_set: s.groq_ready(),
        sidecar_ok,
        sidecar_configured,
        sidecar_device,
        default_mic,
        gemini_key_set: s.gemini_api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false)
            || std::env::var("GEMINI_API_KEY").is_ok(),
        gemini_model: s.gemini_model.clone(),
        recent: get_history(None, Some(10)),
    }
}

fn as_menu_refs(v: &[CheckMenuItem<tauri::Wry>]) -> Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> {
    v.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect()
}

fn build_tray_menu(app: &AppHandle, s: &Settings) -> tauri::Result<Menu<tauri::Wry>> {
    let items: Vec<CheckMenuItem<tauri::Wry>> = s
        .profiles
        .iter()
        .map(|p| {
            CheckMenuItem::with_id(
                app,
                format!("profile:{}", p.name),
                &p.name,
                true,
                p.name == s.active_profile,
                None::<&str>,
            )
        })
        .collect::<Result<_, _>>()?;
    let refs = as_menu_refs(&items);
    let perfil = Submenu::with_items(app, "Perfil", true, &refs)?;

    let langs = |prefix: &str, atual: &str, opts: &[(&str, &str)]| -> tauri::Result<Vec<CheckMenuItem<tauri::Wry>>> {
        opts.iter()
            .map(|(code, label)| {
                CheckMenuItem::with_id(
                    app,
                    format!("{prefix}:{code}"),
                    *label,
                    true,
                    *code == atual,
                    None::<&str>,
                )
            })
            .collect()
    };
    let fala_items = langs(
        "speech",
        &s.speech_lang,
        &[
            ("pt", "Português (Brasil)"),
            ("en", "English"),
            ("es", "Español"),
            ("auto", "Detectar automaticamente"),
        ],
    )?;
    let saida_items = langs(
        "out",
        &s.output_lang,
        &[
            ("same", "Mesmo da fala"),
            ("pt", "Português (Brasil)"),
            ("en", "English"),
            ("es", "Español"),
        ],
    )?;
    let fala_refs = as_menu_refs(&fala_items);
    let saida_refs = as_menu_refs(&saida_items);
    let fala = Submenu::with_items(app, "Fala", true, &fala_refs)?;
    let saida = Submenu::with_items(app, "Saída", true, &saida_refs)?;
    let idioma = Submenu::with_items(app, "Idioma", true, &[&fala, &saida])?;

    // atalho de um clique para o par mais usado; desmarcar volta a saída para "mesmo da fala"
    let pt_en = CheckMenuItem::with_id(
        app,
        "lang_pt_en",
        "PT falado → EN escrito",
        true,
        s.speech_lang == "pt" && s.output_lang == "en",
        None::<&str>,
    )?;

    let copy_last =
        MenuItem::with_id(app, "copy_last", "Copiar último ditado", true, None::<&str>)?;
    let transcribe_last =
        MenuItem::with_id(app, "transcribe_last", "Transcrever último áudio", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Configurações", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Sair", true, None::<&str>)?;
    Menu::with_items(app, &[&perfil, &pt_en, &idioma, &copy_last, &transcribe_last, &show, &quit])
}

/// altera as configurações no estado, grava em disco e avisa tray e janela
fn update_settings(app: &AppHandle, f: impl FnOnce(&mut Settings)) {
    let s = {
        let shared = app.state::<Arc<Shared>>();
        let mut g = shared.settings.lock().unwrap();
        f(&mut g);
        g.clone()
    };
    let _ = persist_settings(&s);
    rebuild_tray_menu(app, &s);
    let _ = app.emit("settings_changed", ());
}

fn rebuild_tray_menu(app: &AppHandle, s: &Settings) {
    if let (Some(tray), Ok(menu)) = (app.tray_by_id("tray"), build_tray_menu(app, s)) {
        let _ = tray.set_menu(Some(menu));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// casos reais do history.jsonl: o Whisper assina o fim da transcrição
    #[test]
    fn assinatura_de_legenda_nao_chega_ao_texto_final() {
        for (entrada, esperado) in [
            (
                "Mas isso será em um segundo momento. Legenda Adriana Zanotto",
                "Mas isso será em um segundo momento.",
            ),
            (
                "Fica bom para você? Legenda Adriana Zanotto",
                "Fica bom para você?",
            ),
            (
                "Tem texto. Legenda: Adriana Zanotto.",
                "Tem texto.",
            ),
            (
                "Foi ótimo. Legendas pela comunidade Amara.org",
                "Foi ótimo.",
            ),
            // "legenda" dita pelo falante no meio da frase: preservar
            (
                "Quero as tabelas com título, legenda, nome das variáveis.",
                "Quero as tabelas com título, legenda, nome das variáveis.",
            ),
            // frase final curta que só começa parecido: preservar
            (
                "Terminei o vídeo. Legendei tudo ontem.",
                "Terminei o vídeo. Legendei tudo ontem.",
            ),
            ("Sem assinatura nenhuma aqui.", "Sem assinatura nenhuma aqui."),
        ] {
            assert_eq!(strip_credit(entrada), esperado, "entrada: {entrada:?}");
        }
        // sozinha, sem frase anterior, cai no guarda de alucinação
        assert!(is_hallucination("Legenda Adriana Zanotto", 0.8));
        assert!(!is_hallucination("Legenda Adriana Zanotto", 30.0));
    }

    #[test]
    fn fallback_so_troca_o_modelo_depois_de_travado() {
        USE_FALLBACK.store(false, Ordering::Relaxed);
        assert_eq!(modelo_efetivo("gemini-3.5-flash-lite"), "gemini-3.5-flash-lite");
        USE_FALLBACK.store(true, Ordering::Relaxed);
        assert_eq!(modelo_efetivo("gemini-3.5-flash-lite"), FALLBACK_MODEL);
        USE_FALLBACK.store(false, Ordering::Relaxed);
    }

    #[test]
    fn so_404_rebaixa_a_sessao_inteira() {
        assert!(erro_e_definitivo(404), "404 = a chave não tem o modelo, é definitivo");
        // 408 (teto estourado), 429 (cota do minuto), 503 (saturado) e 0 (rede)
        // passam; travar em qualquer um deles rebaixaria a sessão por um pico
        for transitorio in [0u16, 408, 429, 503, 500] {
            assert!(!erro_e_definitivo(transitorio), "{transitorio} não pode travar a sessão");
        }
    }

    /// `mode` deixou de ser um campo: agora o comportamento vem de qual atalho
    /// disparou. "hold" usa `hotkey`, "toggle" usa `hotkey_toggle`.
    fn hook(mode: &str, cancel: &str) -> (HookState, std::sync::mpsc::Receiver<Cmd>) {
        let mut s = Settings::default();
        if mode == "toggle" {
            s.hotkey_toggle = std::mem::take(&mut s.hotkey);
        }
        s.cancel_key = cancel.into();
        let shared = Arc::new(Shared {
            bindings: Mutex::new(build_bindings(&s)),
            cancel: Mutex::new(parse_cancel_key(cancel)),
            sidecar: Mutex::new(None),
            settings: Mutex::new(s),
            app: OnceLock::new(),
            last_text: Mutex::new(String::new()),
            overlay_gen: AtomicU64::new(0),
            falhou_ao_gravar: AtomicBool::new(false),
        });
        let (tx, rx) = channel::<Cmd>();
        (HookState {
            shared,
            tx,
            pressed: HashSet::new(),
            latched: None,
            recording: None,
            started_at: None,
            engolidas: HashSet::new(),
        }, rx)
    }

    fn key(name: &str) -> u32 {
        platform::key_from_name(name).expect("tecla conhecida")
    }

    /// modificadores vêm de modifier_group (têm variante esquerda e direita)
    fn modkey(name: &str) -> u32 {
        platform::modifier_group(name).expect("modificador conhecido")[0]
    }

    /// primeiro grupo do atalho de ditado (Ctrl) e segundo (Win/Cmd)
    fn hotkey_codes(st: &HookState) -> (u32, u32) {
        let b = st.shared.bindings.lock().unwrap();
        let g = &b[0].groups;
        (g[0][0], g[1][0])
    }

    #[test]
    fn cancelar_no_hold_nao_dispara_stop_ao_soltar() {
        let (mut st, rx) = hook("hold", "SPACE");
        let (ctrl, win) = hotkey_codes(&st);
        let space = key("SPACE");

        handle_key(&mut st, ctrl, true);
        handle_key(&mut st, win, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(_))));

        handle_key(&mut st, space, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Cancel)));

        // soltar o atalho depois do cancelamento não pode mandar o áudio para o STT
        handle_key(&mut st, space, false);
        handle_key(&mut st, win, false);
        handle_key(&mut st, ctrl, false);
        assert!(rx.try_recv().is_err(), "Stop fantasma depois de cancelar");

        // e o próximo ditado continua funcionando
        handle_key(&mut st, ctrl, true);
        handle_key(&mut st, win, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(_))));
    }

    #[test]
    fn cancelar_no_toggle_permite_novo_ditado() {
        let (mut st, rx) = hook("toggle", "ESC");
        let (ctrl, win) = hotkey_codes(&st);
        let esc = key("ESC");

        handle_key(&mut st, ctrl, true);
        handle_key(&mut st, win, true);
        handle_key(&mut st, win, false);
        handle_key(&mut st, ctrl, false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(_))));

        handle_key(&mut st, esc, true);
        handle_key(&mut st, esc, false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Cancel)));

        handle_key(&mut st, ctrl, true);
        handle_key(&mut st, win, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(_))), "toggle travado depois do cancelamento");
    }

    #[test]
    fn cancelamento_desativado_ignora_a_tecla() {
        let (mut st, rx) = hook("hold", "");
        let (ctrl, win) = hotkey_codes(&st);
        handle_key(&mut st, ctrl, true);
        handle_key(&mut st, win, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(_))));

        handle_key(&mut st, key("SPACE"), true);
        assert!(rx.try_recv().is_err());

        segurou(&mut st);
        handle_key(&mut st, win, false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Stop)));
    }

    /// simula que o atalho foi segurado além do limiar do modo misto
    fn segurou(st: &mut HookState) {
        st.started_at = Some(Instant::now() - SEGURAR - Duration::from_millis(1));
    }

    #[test]
    fn toque_curto_liga_e_outro_toque_desliga() {
        let (mut st, rx) = hook_com(|s| s.hotkey = "Alt+F2".into());
        let (alt, f2) = (modkey("ALT"), key("F2"));

        handle_key(&mut st, alt, true);
        handle_key(&mut st, f2, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(None))));
        handle_key(&mut st, f2, false);
        handle_key(&mut st, alt, false);
        assert!(rx.try_recv().is_err(), "toque curto não pode parar a gravação");

        handle_key(&mut st, alt, true);
        handle_key(&mut st, f2, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Stop)), "segundo toque tem que parar");
        handle_key(&mut st, f2, false);
        handle_key(&mut st, alt, false);
        assert!(rx.try_recv().is_err(), "soltar depois do segundo toque não pode reabrir nada");

        // e o próximo ditado começa normalmente
        handle_key(&mut st, alt, true);
        handle_key(&mut st, f2, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(None))));
    }

    #[test]
    fn segurar_grava_ate_soltar_em_qualquer_atalho() {
        // o atalho "toggle" e o de perfil também são mistos
        let (mut st, rx) = hook_com(|s| {
            s.hotkey = String::new();
            s.hotkey_toggle = "Alt+F2".into();
            s.profiles[1].hotkey = "Ctrl+Alt+J".into();
        });
        handle_key(&mut st, modkey("ALT"), true);
        handle_key(&mut st, key("F2"), true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(None))));
        segurou(&mut st);
        handle_key(&mut st, key("F2"), false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Stop)));
        handle_key(&mut st, modkey("ALT"), false);

        handle_key(&mut st, modkey("CTRL"), true);
        handle_key(&mut st, modkey("ALT"), true);
        handle_key(&mut st, key("J"), true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(Some(_)))));
        segurou(&mut st);
        handle_key(&mut st, key("J"), false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Stop)));
    }

    #[test]
    fn so_a_tecla_comum_do_atalho_e_engolida() {
        let (mut st, _rx) = hook_com(|s| s.hotkey = "Alt+F2".into());
        let (alt, f2) = (modkey("ALT"), key("F2"));
        assert!(!handle_key(&mut st, alt, true), "modificador sozinho chega ao app");
        assert!(handle_key(&mut st, f2, true), "F2 do atalho não pode chegar ao app");
        assert!(handle_key(&mut st, f2, true), "repetição automática também é engolida");
        assert!(handle_key(&mut st, f2, false), "soltar o F2 também é engolido");
        assert!(!handle_key(&mut st, alt, false));
        // F2 sem o Alt não é o atalho: passa
        assert!(!handle_key(&mut st, f2, true));
        assert!(!handle_key(&mut st, f2, false));
    }

    #[test]
    fn soltura_so_e_engolida_se_a_descida_foi() {
        // F2 apertado antes do Alt: a descida chegou ao app, então a soltura também
        // tem que chegar, senão o F2 fica preso lá
        let (mut st, rx) = hook_com(|s| s.hotkey = "Alt+F2".into());
        let (alt, f2) = (modkey("ALT"), key("F2"));
        assert!(!handle_key(&mut st, f2, true));
        assert!(!handle_key(&mut st, alt, true));
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(None))), "a combinação disparou");
        assert!(!handle_key(&mut st, f2, true), "repetição de descida não engolida também passa");
        assert!(!handle_key(&mut st, f2, false), "soltura de descida não engolida passa");
    }

    #[test]
    fn gravacao_que_nao_comecou_nao_trava_o_atalho() {
        // o microfone falhou: a thread de captura não está gravando, e o próximo
        // toque tem que começar de novo em vez de virar um Stop mudo
        let (mut st, rx) = hook_com(|s| s.hotkey = "Alt+F2".into());
        let (alt, f2) = (modkey("ALT"), key("F2"));
        handle_key(&mut st, alt, true);
        handle_key(&mut st, f2, true);
        handle_key(&mut st, f2, false);
        handle_key(&mut st, alt, false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(None))));
        st.shared.falhou_ao_gravar.store(true, Ordering::SeqCst);
        handle_key(&mut st, alt, true);
        handle_key(&mut st, f2, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(None))), "esperava novo Start, não Stop");

        // microfone lento mas sem falha: o segundo toque para, por mais tempo que passe
        handle_key(&mut st, f2, false);
        handle_key(&mut st, alt, false);
        st.started_at = Some(Instant::now() - Duration::from_secs(5));
        handle_key(&mut st, alt, true);
        handle_key(&mut st, f2, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Stop)));
    }

    /// monta um HookState com atalhos arbitrários, para os cenários de vários vínculos
    fn hook_com(ajusta: impl FnOnce(&mut Settings)) -> (HookState, std::sync::mpsc::Receiver<Cmd>) {
        let mut s = Settings::default();
        ajusta(&mut s);
        let shared = Arc::new(Shared {
            bindings: Mutex::new(build_bindings(&s)),
            cancel: Mutex::new(parse_cancel_key(&s.cancel_key)),
            sidecar: Mutex::new(None),
            settings: Mutex::new(s),
            app: OnceLock::new(),
            last_text: Mutex::new(String::new()),
            overlay_gen: AtomicU64::new(0),
            falhou_ao_gravar: AtomicBool::new(false),
        });
        let (tx, rx) = channel::<Cmd>();
        (HookState {
            shared,
            tx,
            pressed: HashSet::new(),
            latched: None,
            recording: None,
            started_at: None,
            engolidas: HashSet::new(),
        }, rx)
    }

    #[test]
    fn atalho_de_perfil_dita_naquele_estilo_sem_trocar_o_ativo() {
        let (mut st, rx) = hook_com(|s| {
            s.hotkey = "Ctrl+Win".into();
            s.profiles[1].hotkey = "Ctrl+Alt+J".into();
        });
        let alvo = st.shared.settings.lock().unwrap().profiles[1].name.clone();

        handle_key(&mut st, modkey("CTRL"), true);
        handle_key(&mut st, modkey("ALT"), true);
        handle_key(&mut st, key("J"), true);
        match rx.try_recv() {
            Ok(Cmd::Start(Some(p))) => assert_eq!(p, alvo, "ditou no perfil errado"),
            Ok(Cmd::Start(None)) => panic!("Start sem perfil: o vínculo do perfil não foi usado"),
            _ => panic!("esperava Start com o perfil do atalho"),
        }
        // o perfil ativo não pode ter mudado
        assert_eq!(st.shared.settings.lock().unwrap().active_profile, "Bruto corrigido");

        segurou(&mut st);
        handle_key(&mut st, key("J"), false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Stop)));
    }

    #[test]
    fn atalho_mais_especifico_ganha_do_mais_curto() {
        // Ctrl+Win e Ctrl+Win+J juntos: completar a combinação maior não pode
        // acionar a menor
        let (mut st, rx) = hook_com(|s| {
            s.hotkey = "Ctrl+Win".into();
            s.profiles[1].hotkey = "Ctrl+Win+J".into();
        });
        let b = st.shared.bindings.lock().unwrap();
        assert!(b[0].groups.len() >= b[b.len() - 1].groups.len(), "lista não está do mais específico para o mais genérico");
        drop(b);

        handle_key(&mut st, modkey("CTRL"), true);
        handle_key(&mut st, modkey("WIN"), true);
        handle_key(&mut st, key("J"), true);
        // o primeiro a ser satisfeito foi Ctrl+Win, e ele fica travado até soltar
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(None))));
        assert!(rx.try_recv().is_err(), "dois Start para um gesto só");
    }

    #[test]
    fn atalho_de_ditado_e_de_troca_de_perfil_nao_se_misturam() {
        let (mut st, rx) = hook_com(|s| {
            s.hotkey = "Ctrl+Win".into();
            s.hotkey_cycle_profile = "Ctrl+Alt+P".into();
        });
        handle_key(&mut st, modkey("CTRL"), true);
        handle_key(&mut st, modkey("ALT"), true);
        handle_key(&mut st, key("P"), true);
        // trocar de perfil não grava áudio
        assert!(rx.try_recv().is_err(), "atalho de perfil disparou gravação");
    }

    #[test]
    fn atalho_invalido_e_descartado_em_vez_de_virar_outra_tecla() {
        assert!(parse_hotkey("Ctrl+Tecla_Que_Nao_Existe").is_none());
        assert!(parse_hotkey("").is_none());
        assert!(parse_hotkey("Ctrl+Win").is_some());
        // cancelamento com tecla desconhecida desliga, não vira F9
        assert!(parse_cancel_key("xyz").is_none());
        assert!(parse_cancel_key("").is_none());
        assert_eq!(parse_cancel_key("ESC"), platform::key_from_name("ESC"));
    }

    #[test]
    fn atalho_duplicado_nao_cria_dois_vinculos() {
        let mut s = Settings::default();
        s.hotkey = "Ctrl+Win".into();
        s.hotkey_toggle = "Ctrl+Win".into();
        assert_eq!(build_bindings(&s).len(), 1, "o mesmo atalho ficou em dois vínculos");
    }

    #[test]
    fn migracao_do_modo_toggle_preserva_o_comportamento() {
        // versões antigas guardavam o modo num campo à parte
        let antigo = r#"{"hotkey":"Ctrl+Win","mode":"toggle"}"#;
        let mut s: Settings = serde_json::from_str(antigo).unwrap();
        if s.mode == "toggle" && s.hotkey_toggle.is_empty() && !s.hotkey.is_empty() {
            s.hotkey_toggle = std::mem::take(&mut s.hotkey);
        }
        assert_eq!(s.hotkey_toggle, "Ctrl+Win");
        assert!(s.hotkey.is_empty());
        let b = build_bindings(&s);
        assert_eq!(b.len(), 1);
        assert!(matches!(b[0].act, Act::Record { profile: None }));
    }

    /// Trava o comportamento contra merge com o upstream: se alguém remover a
    /// reamostragem, estes testes falham em vez de o app voltar a estourar o
    /// limite da Groq em silêncio.
    #[test]
    fn audio_e_reamostrado_para_16khz_antes_de_subir() {
        // 1 segundo de silêncio a 48 kHz vira 1 segundo a 16 kHz
        let wav = wav_bytes(&vec![0.0f32; 48_000], 48_000);
        // cabeçalho WAV: taxa de amostragem nos bytes 24..28
        let taxa = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
        assert_eq!(taxa, 16_000, "o WAV enviado precisa sair em 16 kHz");

        // 48 kHz mono 16 bits = 96 KB/s; 16 kHz = 32 KB/s. Um terço do tamanho.
        let dados = wav.len() - 44;
        assert_eq!(dados, 32_000, "1s a 16 kHz e 16 bits deve dar 32.000 bytes");
    }

    #[test]
    fn taxa_igual_ou_menor_que_16khz_passa_intacta() {
        let wav = wav_bytes(&vec![0.0f32; 16_000], 16_000);
        assert_eq!(u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]), 16_000);
        let wav = wav_bytes(&vec![0.0f32; 8_000], 8_000);
        assert_eq!(u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]), 8_000,
            "taxa menor nao pode ser esticada");
    }

    #[test]
    fn reamostragem_preserva_o_sinal() {
        // razao inteira: media por blocos
        let constante = vec![0.5f32; 4_800];
        let r = resample(&constante, 48_000, 16_000);
        assert_eq!(r.len(), 1_600);
        assert!(r.iter().all(|v| (v - 0.5).abs() < 1e-6), "sinal constante deve sobreviver");

        // razao quebrada: interpolacao linear
        let r = resample(&vec![0.25f32; 4_410], 44_100, 16_000);
        assert_eq!(r.len(), 1_600);
        assert!(r.iter().all(|v| (v - 0.25).abs() < 1e-6));

        assert_eq!(resample(&[1.0, 2.0], 16_000, 16_000), vec![1.0, 2.0], "mesma taxa nao mexe");
    }

    /// O marcador de timeout é interno: se vazar para o usuário, vira ruído no
    /// overlay. Este teste trava a troca por texto legível.
    #[test]
    fn marcador_de_timeout_nao_chega_ao_usuario() {
        let bruto = "__TIMEOUT__ Gemini (reescrita): operation timed out";
        let limpo = bruto.replace("__TIMEOUT__", "tempo esgotado");
        assert!(!limpo.contains("__TIMEOUT__"), "marcador interno vazou");
        assert!(limpo.starts_with("tempo esgotado"));
    }

    #[test]
    fn teto_da_reescrita_e_curto_o_bastante_para_retentativa() {
        // com 30s a segunda tentativa chegaria depois de o usuário desistir;
        // o par teto+retentativa precisa caber em menos de 10s no pior caso
        assert!(REWRITE_TIMEOUT.as_secs() <= 5, "teto alto demais para retentar");
        assert!(REWRITE_TIMEOUT.as_secs() * 2 < 10, "duas tentativas passam de 10s");
    }

    /// Provedor lento não pode ser condenado pelo teto calibrado no provedor
    /// rápido: a latência é característica de quem responde, não do app.
    #[test]
    fn teto_por_provedor_sobrepoe_o_padrao() {
        let padrao = Provider { name: "Groq".into(), base_url: "https://x/v1".into(),
                                api_key: None, timeout_secs: 0, ..Default::default() };
        assert_eq!(padrao.timeout(), REWRITE_TIMEOUT, "0 significa usar o padrão");

        let lento = Provider { name: "OmniRoute".into(), base_url: "https://y/v1".into(),
                               api_key: None, timeout_secs: 12, ..Default::default() };
        assert_eq!(lento.timeout(), Duration::from_secs(12));
        assert!(lento.timeout() > padrao.timeout());
    }

    /// Monta um Settings com dois provedores e um perfil que aponta para o
    /// segundo, para exercitar a escolha por perfil sem tocar a rede.
    fn settings_com_perfil(provedor: &str, modelo: &str) -> Settings {
        let mut s = Settings::default();
        s.providers = vec![
            Provider { name: "Groq".into(), base_url: "https://groq/v1".into(),
                       api_key: None, timeout_secs: 0, ..Default::default() },
            Provider { name: "Omniroute".into(), base_url: "https://omni/v1".into(),
                       api_key: None, timeout_secs: 12, ..Default::default() },
        ];
        s.rewrite_provider_name = "Groq".into();
        s.rewrite_model = "openai/gpt-oss-120b".into();
        s.profiles = vec![
            Profile { name: "Bruto corrigido".into(), hotkey_mode: "hold".into(),
                      ..Default::default() },
            Profile { name: "Anotção clínica".into(), hotkey_mode: "hold".into(),
                      rewrite_provider_name: provedor.into(),
                      rewrite_model: modelo.into(), ..Default::default() },
        ];
        s
    }

    /// A razão de existir da escolha por perfil. Medido em 07/10 no mesmo ditado
    /// clínico de 2.872 caracteres: a Groq responde em 8,4s mas escreveu
    /// "desobturção da restauração" onde a fala dizia "remoção da resina";
    /// gpt-5.6-terra acertou as 8 seções do perfil, em 33,3s. Um perfil aceita
    /// esperar, o outro não.
    #[test]
    fn perfil_com_provedor_proprio_ganha_do_global() {
        let s = settings_com_perfil("Omniroute", "codex/gpt-5.6-terra-medium");

        let (p, modelo) = alvo_do_perfil(&s, "Anotção clínica")
            .expect("escolha válida")
            .expect("perfil tem provedor próprio");
        assert_eq!(p.name, "Omniroute");
        assert_eq!(modelo, "codex/gpt-5.6-terra-medium");
        assert_eq!(p.timeout(), Duration::from_secs(12), "leva o teto do provedor do perfil");

        assert!(
            alvo_do_perfil(&s, "Bruto corrigido").unwrap().is_none(),
            "perfil sem escolha própria cai no global"
        );
    }

    /// Configuração antiga não tem os campos novos, e perfil apagado não pode
    /// derrubar o ditado: nos dois casos o global resolve.
    #[test]
    fn perfil_sem_escolha_ou_inexistente_usa_o_global() {
        let s = settings_com_perfil("", "");
        assert!(alvo_do_perfil(&s, "Anotção clínica").unwrap().is_none());
        assert!(alvo_do_perfil(&s, "perfil que não existe").unwrap().is_none());
        for prof in &Settings::default().profiles {
            assert!(prof.rewrite_provider_name.is_empty(), "padrão não força provedor");
        }
    }

    /// Falhar calado já custou dias duas vezes. Provedor removido sem limpar o
    /// perfil, ou provedor escolhido sem modelo, precisa dizer o que está errado
    /// — o texto bruto ainda é colado, mas com o motivo no balão.
    #[test]
    fn escolha_quebrada_no_perfil_explica_o_motivo() {
        // `unwrap_err` exigiria Debug em Provider, e Provider carrega a chave de
        // API: um panic imprimiria a chave. Casar à mão sai mais barato que isso.
        let erro = |s: &Settings| match alvo_do_perfil(s, "Anotção clínica") {
            Err(e) => e,
            Ok(_) => panic!("escolha quebrada tinha de falhar, não seguir calada"),
        };

        let e = erro(&settings_com_perfil("Provedor apagado", "qualquer-modelo"));
        assert!(e.contains("Anotção clínica") && e.contains("Provedor apagado"),
                "precisa nomear o perfil e o provedor: {e}");

        let e = erro(&settings_com_perfil("Omniroute", ""));
        assert!(e.contains("modelo"), "modelo vazio tem de ser dito, não virar 404: {e}");
    }

    /// O `claude-haiku-5-5` devolve `400 temperature is deprecated for this
    /// model`, e o app mandava `temperature: 0.2` em toda reescrita — ou seja,
    /// a reescrita pela Anthropic estava quebrada. Quem recusa entra no
    /// conjunto e não paga a ida perdida de novo.
    #[test]
    fn modelo_que_recusa_temperatura_e_lembrado() {
        let modelo = "modelo-de-teste-temperatura";
        assert!(!sem_temperatura(modelo), "não pode nascer no conjunto");
        marcar_sem_temperatura(modelo);
        assert!(sem_temperatura(modelo));
        // o conjunto é por modelo, não global
        assert!(!sem_temperatura("outro-modelo-qualquer"));
    }

    /// Medido em 14/10 contra a API real: `/chat/completions` da Anthropic
    /// responde sem o header, mas `/models` devolve
    /// `400 anthropic-version: header is required`. O header fica restrito ao
    /// host dela — os outros provedores não o entendem e não têm por que vê-lo.
    #[test]
    fn so_a_anthropic_recebe_o_header_de_versao() {
        let anthropic = Provider {
            base_url: "https://api.anthropic.com/v1".into(),
            ..Default::default()
        };
        assert!(anthropic.precisa_versao_anthropic());

        for outro in ["https://api.groq.com/openai/v1", "https://openrouter.ai/api/v1",
                      "https://api.openai.com/v1", "https://omniroute.ihub.click/v1"] {
            let p = Provider { base_url: outro.into(), ..Default::default() };
            assert!(!p.precisa_versao_anthropic(), "{outro} não deveria receber o header");
        }
    }

    /// `prompt` é o único campo de vocabulário que todos aceitam. Medido em
    /// 07/10 contra os provedores reais: `keywords` devolve
    /// `HTTP 400 unknown param` na Groq, e um campo que derruba o ditado num
    /// provedor não compensa o ganho no outro.
    #[test]
    fn dicionario_vira_pista_de_vocabulario() {
        let mut s = Settings::default();
        assert!(vocabulario(&s).is_none(), "dicionário vazio não manda campo nenhum");

        s.dictionary = vec![
            "radix entomolaris".into(),
            "  ".into(), // linha em branco que o usuário deixou
            "AH Plus Jet".into(),
            "dexametasona".into(),
        ];
        let v = vocabulario(&s).expect("com termos, vira pista");
        assert_eq!(v, "radix entomolaris, AH Plus Jet, dexametasona");
        assert!(!v.contains("  ,"), "entrada vazia não pode virar vírgula solta");
    }

    /// O esforço viaja no nome do modelo para atravessar seletor, perfil e
    /// histórico sem campo novo. Modelo sem `:` continua valendo como sempre.
    #[test]
    fn esforco_viaja_no_nome_do_modelo() {
        assert_eq!(modelo_e_esforco("gpt-5.6-luna:low"), ("gpt-5.6-luna", "low"));
        assert_eq!(modelo_e_esforco("gpt-5.6-terra:medium"), ("gpt-5.6-terra", "medium"));
        assert_eq!(
            modelo_e_esforco("openai/gpt-oss-120b"),
            ("openai/gpt-oss-120b", ""),
            "modelo de provedor de chave não pode ganhar esforço do nada"
        );
    }

    /// O fluxo de uso do plano recusa áudio e o endpoint de transcrição. Mandar
    /// o WAV mesmo assim devolveria um 400 sem explicação.
    #[test]
    fn chatgpt_recusa_transcricao_com_motivo() {
        let p = Provider {
            name: "ChatGPT".into(),
            base_url: siwc::RESOURCE.into(),
            api_key: None,
            timeout_secs: 60,
            kind: ProviderKind::ChatgptOauth,
        };
        let e = transcribe_openai(vec![0u8; 64], &p, "gpt-5.6-luna", "pt", None).unwrap_err();
        assert!(e.contains("não transcreve"), "precisa dizer o motivo: {e}");
        assert!(e.contains("ChatGPT"), "precisa nomear o provedor: {e}");
    }

    /// Configuração anterior não tem o campo; virar `ChatgptOauth` por engano
    /// mandaria todo provedor de chave para a rota errada.
    #[test]
    fn provedor_sem_tipo_continua_sendo_de_chave() {
        let p: Provider = serde_json::from_str(
            r#"{"name":"Groq","base_url":"https://api.groq.com/openai/v1"}"#,
        )
        .expect("arquivo antigo tem de continuar lendo");
        assert_eq!(p.kind, ProviderKind::ApiKey);
        assert_eq!(p.timeout_secs, 0);
    }

    /// Medido: gpt-5.6-terra-medium leva 4,6-5,6s pelo OmniRoute. Com o teto de
    /// 4s ele estourava, retentava e estourava de novo — 8,00s cravados no
    /// histórico e texto bruto colado sem aviso.
    #[test]
    fn teto_de_doze_segundos_cobre_o_gateway_lento() {
        let p = Provider { name: "OmniRoute".into(), base_url: "https://y/v1".into(),
                           api_key: None, timeout_secs: 12, ..Default::default() };
        assert!(p.timeout().as_millis() > 5_600, "precisa caber a pior medição do gateway");
    }

    #[test]
    fn url_do_provedor_tolera_barra_sobrando_ou_faltando() {
        let com = Provider { name: "x".into(), base_url: "https://a.b/v1/".into(), api_key: None, timeout_secs: 0, ..Default::default() };
        let sem = Provider { name: "x".into(), base_url: "https://a.b/v1".into(), api_key: None, timeout_secs: 0, ..Default::default() };
        assert_eq!(com.url("chat/completions"), "https://a.b/v1/chat/completions");
        assert_eq!(sem.url("/chat/completions"), "https://a.b/v1/chat/completions");
        assert_eq!(sem.url("models"), "https://a.b/v1/models");
    }

    #[test]
    fn migracao_transforma_chaves_existentes_em_provedores() {
        let mut s = Settings::default();
        s.groq_api_key = Some("gsk_x".into());
        s.gemini_api_key = Some("AQ.y".into());
        s.rewrite_provider = "groq".into();
        s.groq_rewrite_model = "llama-3.3-70b-versatile".into();
        migrate_providers(&mut s);

        assert_eq!(s.providers.len(), 2, "as duas chaves deviam virar cartões");
        assert_eq!(s.rewrite_provider_name, "Groq", "manteve o provedor que estava em uso");
        assert_eq!(s.rewrite_model, "llama-3.3-70b-versatile");
        let groq = s.providers.iter().find(|p| p.name == "Groq").unwrap();
        assert_eq!(groq.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(groq.api_key.as_deref(), Some("gsk_x"));
    }

    #[test]
    fn migracao_aponta_a_transcricao_para_o_provedor_groq() {
        let mut s = Settings::default();
        s.groq_api_key = Some("gsk_x".into());
        s.stt_provider = "groq".into();
        s.groq_model = "whisper-large-v3-turbo".into();
        migrate_providers(&mut s);
        assert_eq!(s.stt_provider_name, "Groq");
        assert_eq!(s.stt_model, "whisper-large-v3-turbo");
    }

    #[test]
    fn quem_transcreve_no_local_nao_e_movido_para_a_nuvem() {
        let mut s = Settings::default();
        s.groq_api_key = Some("gsk_x".into());
        s.stt_provider = "local".into();
        migrate_providers(&mut s);
        assert!(
            s.stt_provider_name.is_empty(),
            "modo local nao pode virar provedor de nuvem sozinho"
        );
    }

    #[test]
    fn migracao_nao_mexe_em_quem_ja_tem_provedores() {
        let mut s = Settings::default();
        s.groq_api_key = Some("gsk_x".into());
        s.providers = vec![Provider {
            name: "Meu gateway".into(),
            base_url: "https://omniroute.exemplo/v1".into(),
            api_key: Some("sk-1".into()), timeout_secs: 0, ..Default::default()
        }];
        s.rewrite_provider_name = "Meu gateway".into();
        s.rewrite_model = "algum/modelo".into();
        migrate_providers(&mut s);

        assert_eq!(s.providers.len(), 1, "não pode recriar provedor que o usuário removeu");
        assert_eq!(s.rewrite_provider_name, "Meu gateway");
    }

    #[test]
    fn sem_provedores_o_comportamento_legado_continua() {
        let mut s = Settings::default();
        migrate_providers(&mut s);
        assert!(s.providers.is_empty());
        assert!(s.rewrite_provider_name.is_empty(), "sem chave nenhuma, nada a migrar");
    }

    #[test]
    fn classifica_modelo_de_transcricao_pelo_catalogo() {
        // formatos reais, copiados da resposta de cada provedor
        let groq_whisper = serde_json::json!({
            "id": "whisper-large-v3-turbo",
            "input_modalities": ["audio"], "output_modalities": ["transcription"]
        });
        let groq_chat = serde_json::json!({
            "id": "llama-3.3-70b-versatile",
            "input_modalities": ["text"], "output_modalities": ["text"]
        });
        let omni_whisper = serde_json::json!({
            "id": "openrouter/openai/whisper-large-v3-turbo",
            "type": "audio", "subtype": "transcription"
        });
        let omni_chat = serde_json::json!({
            "id": "openrouter/google/gemini-3.5-flash-lite",
            "input_modalities": ["text", "image"], "output_modalities": ["text"]
        });
        // chat multimodal do OpenRouter: aceita áudio, mas NÃO transcreve
        let or_multimodal = serde_json::json!({
            "id": "google/gemini-3.6-flash",
            "architecture": { "input_modalities": ["text", "image", "audio"] }
        });

        assert!(declara_transcricao(&groq_whisper));
        assert!(declara_transcricao(&omni_whisper));
        assert!(!declara_transcricao(&groq_chat));
        assert!(!declara_transcricao(&omni_chat));
        assert!(
            !declara_transcricao(&or_multimodal),
            "aceitar áudio na entrada não é transcrever — isso arrastaria 25 modelos de chat"
        );
    }

    #[test]
    fn heuristica_de_nome_e_o_ultimo_recurso() {
        assert!(parece_transcricao("openai/whisper-large-v3"));
        assert!(parece_transcricao("mistralai/voxtral-mini-transcribe"));
        assert!(!parece_transcricao("llama-3.3-70b-versatile"));
        assert!(!parece_transcricao("gemini-3.5-flash-lite"));
    }

    #[test]
    fn bloco_de_raciocinio_e_descartado() {
        assert_eq!(sem_raciocinio("<think>penso muito</think>\nResposta final").trim(), "Resposta final");
        assert_eq!(sem_raciocinio("Resposta direta"), "Resposta direta");
    }

    #[test]
    fn provedor_de_reescrita_migra_para_gemini() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.rewrite_provider, "gemini", "quem já usava não pode mudar de provedor sozinho");
    }

    #[test]
    fn erros_viram_frases_curtas() {
        assert_eq!(friendly_error("Groq HTTP 401: {\"error\":...}"), "Chave de API inválida ou ausente");
        assert_eq!(friendly_error("Groq HTTP 429: rate limit"), "Limite da API atingido");
        assert_eq!(friendly_error("microfone 'Yeti' não encontrado"), "Microfone não encontrado");
        assert_eq!(friendly_error("transcrição vazia"), "Nada foi dito");
        // sem regra conhecida: mostra o original, curto o bastante para caber
        assert!(friendly_error(&"x".repeat(200)).chars().count() <= 70);
    }
}

pub fn run() {
    let (settings, first_run) = load_settings();
    // migra chaves em texto plano para DPAPI na primeira oportunidade
    let _ = persist_settings(&settings);
    // com Groq configurado como provedor, a GPU fica livre: o local só aquece sob
    // demanda — a não ser que um perfil dite só local (laudo), que não pode
    // esperar os ~30s de carga no primeiro ditado
    let start_local = settings.stt_provider != "groq" || !settings.groq_ready() || settings.tem_perfil_local();
    let shared = Arc::new(Shared {
        bindings: Mutex::new(build_bindings(&settings)),
        cancel: Mutex::new(parse_cancel_key(&settings.cancel_key)),
        sidecar: Mutex::new(if start_local { platform::spawn_sidecar(&settings) } else { None }),
        settings: Mutex::new(settings.clone()),
        app: OnceLock::new(),
        last_text: Mutex::new(String::new()),
        overlay_gen: AtomicU64::new(0),
            falhou_ao_gravar: AtomicBool::new(false),
    });

    let shared_exit = shared.clone();
    let shared_setup = shared.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // no boot o app pode subir duas vezes (autostart do registro + atalho em
            // shell:startup, ou o "reabrir apps ao entrar" do Windows). A segunda cai
            // aqui e abria as configurações na cara do usuário — quem vem do boot só sai.
            if args.iter().any(|a| a == "--autostart") {
                return;
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            list_mics,
            get_history,
            clear_history,
            list_audios,
            read_audio,
            transcribe_audio,
            open_audio_dir,
            open_ditados_dir,
            get_diagnostics,
            get_autostart,
            set_autostart,
            test_provider,
            siwc_conectar,
            siwc_conta,
            siwc_desconectar,
            siwc_modelos
        ])
        .setup(move |app| {
            // app de bandeja: no mac o ícone vive só na menu bar, sem Dock
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // impede o App Nap: sem janela visível o macOS suspende o processo e o
            // event tap do atalho para de responder até o app "acordar"
            #[cfg(target_os = "macos")]
            unsafe {
                use objc2::runtime::AnyObject;
                use objc2::{class, msg_send};
                let pi: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
                let reason: *mut AnyObject = msg_send![
                    class!(NSString),
                    stringWithUTF8String: c"hotkey e ditado globais".as_ptr()
                ];
                // NSActivityUserInitiatedAllowingIdleSystemSleep
                let opts: usize = 0x00FF_FFFF & !(1 << 20);
                let token: *mut AnyObject =
                    msg_send![pi, beginActivityWithOptions: opts, reason: reason];
                let _: *mut AnyObject = msg_send![token, retain]; // token vive para sempre
            }

            {
                use tauri_plugin_autostart::ManagerExt;
                let al = app.autolaunch();
                // re-habilitar reescreve a chave do registro com os args atuais
                // (--autostart), inclusive em quem já tinha o autostart de versões antigas
                if first_run || al.is_enabled().unwrap_or(false) {
                    let _ = al.enable();
                }
            }
            // o hook de teclado troca perfil e mostra o balão: precisa do handle
            let _ = shared_setup.app.set(app.handle().clone());
            let tx = spawn_pipeline(shared_setup.clone(), app.handle().clone());
            let tx_tray = tx.clone();
            platform::spawn_hotkey_listener(shared_setup.clone(), tx);

            // áudios guardados vencem em `audio_keep_days`: limpa no boot e a cada 6h,
            // porque o app passa dias aberto na bandeja sem reiniciar
            let shared_limpeza = shared_setup.clone();
            std::thread::spawn(move || loop {
                let (keep, keep_d) = {
                    let g = shared_limpeza.settings.lock().unwrap();
                    (g.audio_keep_days, prazo_ditados(g.audio_keep_days))
                };
                let n = audios::limpar_antigos(keep);
                if n > 0 {
                    println!("[audios] {n} arquivo(s) com mais de {keep} dias apagados");
                }
                let n = ditados::limpar_antigos(keep_d);
                if n > 0 {
                    println!("[ditados] {n} arquivo(s) com mais de {keep_d} dias apagados");
                }
                std::thread::sleep(Duration::from_secs(6 * 3600));
            });

            if let Some(o) = app.get_webview_window("overlay") {
                let _ = o.set_ignore_cursor_events(true);
                // Spaces: sem isto o overlay só existe na mesa onde nasceu (a das
                // configurações) e "some" quando se dita em outra mesa/tela cheia.
                // CanJoinAllSpaces (1<<0) | Stationary (1<<4) | FullScreenAuxiliary (1<<8)
                #[cfg(target_os = "macos")]
                if let Ok(nsw) = o.ns_window() {
                    let nsw = nsw as *mut objc2::runtime::AnyObject;
                    let behavior: usize = (1 << 0) | (1 << 4) | (1 << 8);
                    unsafe {
                        let _: () = objc2::msg_send![nsw, setCollectionBehavior: behavior];
                    }
                }
            }

            let menu_settings = shared_setup.settings.lock().unwrap().clone();
            let menu = build_tray_menu(app.handle(), &menu_settings)?;
            let tray = TrayIconBuilder::with_id("tray");
            // menu bar do mac usa ícone template: só o glifo da onda, monocromático,
            // e o sistema o adapta ao estilo/tema da barra (como os ícones nativos)
            #[cfg(target_os = "macos")]
            let tray = tray
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template.png"))?)
                .icon_as_template(true);
            #[cfg(not(target_os = "macos"))]
            let tray = tray.icon(app.default_window_icon().unwrap().clone());
            tray.menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(w) = tray.app_handle().get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .tooltip("Open Flow — segure o atalho para ditar")
                .on_menu_event(move |app, event| {
                    let id = event.id.as_ref();
                    if let Some(name) = id.strip_prefix("profile:") {
                        let name = name.to_string();
                        update_settings(app, |g| g.active_profile = name);
                        return;
                    }
                    if let Some(code) = id.strip_prefix("speech:") {
                        let code = code.to_string();
                        update_settings(app, |g| g.speech_lang = code);
                        return;
                    }
                    if let Some(code) = id.strip_prefix("out:") {
                        let code = code.to_string();
                        update_settings(app, |g| g.output_lang = code);
                        return;
                    }
                    match id {
                        "lang_pt_en" => update_settings(app, |g| {
                            let ligado = g.speech_lang == "pt" && g.output_lang == "en";
                            g.speech_lang = "pt".into();
                            g.output_lang = if ligado { "same".into() } else { "en".into() };
                        }),
                        "copy_last" => {
                            let text = app.state::<Arc<Shared>>().last_text.lock().unwrap().clone();
                            if !text.is_empty() {
                                if let Ok(mut c) = arboard::Clipboard::new() {
                                    let _ = c.set_text(text);
                                }
                            }
                        }
                        // mesmo fluxo de um ditado: overlay em "processando", o usuário
                        // clica onde quer o texto e ele cola ali quando terminar
                        "transcribe_last" => match audios::mais_recente() {
                            Some(p) => {
                                let _ = tx_tray.send(Cmd::Retranscribe(p));
                            }
                            None => toast_show(app, "nenhum áudio salvo"),
                        },
                        "show" => {
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                        "quit" => app.exit(0),
                        _ => {}
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("erro ao iniciar o Open Flow")
        .run(move |_app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(child) = shared_exit.sidecar.lock().unwrap().as_mut() {
                    let _ = child.kill();
                }
            }
        });
}
