import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import Overlay from "./Overlay";
import Toast from "./Toast";
import canalImg from "./assets/canal.png";
import pixQrImg from "./assets/pix-qr.png";
import "./App.css";

const isMac = navigator.userAgent.includes("Mac");

export interface Profile {
  name: string;
  style: string;
  hotkey: string;
  hotkey_mode: string;
  /// provedor de reescrita só deste perfil; vazio usa o global
  rewrite_provider_name: string;
  rewrite_model: string;
  /// transcreve só no sidecar local, sem nuvem nem fallback
  stt_local_only: boolean;
  /// vocabulário deste perfil para quem transcreve; vazio usa o dicionário
  stt_prompt: string;
  /// não cola: grava .wav + .json na pasta de ditados para outro programa
  save_only: boolean;
}

export interface Snippet {
  trigger: string;
  text: string;
}

export interface Provider {
  name: string;
  base_url: string;
  api_key: string | null;
  timeout_secs: number;
  /// ausente = provedor de chave, como sempre foi
  kind?: "api_key" | "chatgpt_oauth";
}

/// Conta do ChatGPT conectada. Tokens ficam só no backend — aqui chega o
/// mínimo para a interface dizer quem está conectado.
interface ContaChatgpt {
  email: string;
  usa_o_plano: boolean;
  scopes: string[];
}

const ehChatgpt = (p?: Provider) => p?.kind === "chatgpt_oauth";

/// Padrão da assinatura, medido em 07/10 no ditado clínico real: `luna:low`
/// acertou as 7 checagens de fidelidade em 18,3s, contra 27,2s do
/// `terra:medium` para o mesmo acerto. Esforço alto passou de 60s e piorou —
/// edição fiel não melhora com mais raciocínio.
const PADRAO_CHATGPT = "gpt-5.6-luna:low";

/// Valor sentinela da lista de modelos: cair no campo livre. Começa com
/// espaço para nunca colidir com um nome de modelo real.
const OUTRO = " __outro__";

/// O esforço viaja como sufixo do nome do modelo (`claude-haiku-5-5:low`), para
/// atravessar seletor, escolha por perfil e histórico sem campo novo. Estas
/// duas funções são o único lugar que sabe disso na interface.
const semEsforco = (m: string) => m.split(":")[0];
const soEsforco = (m: string) => m.split(":")[1] ?? "";
const comEsforco = (m: string, e: string) => (e ? `${semEsforco(m)}:${e}` : semEsforco(m));

const ESFORCOS = [
  { v: "", rotulo: "padrão do provedor" },
  { v: "low", rotulo: "baixo — mais rápido" },
  { v: "medium", rotulo: "médio" },
  { v: "high", rotulo: "alto — mais lento" },
];

interface ProviderTest {
  ok: boolean;
  models: string[];
  message: string;
  no_catalog: boolean;
  audio: boolean;
  transcription_models: string[];
  transcription_source: string;
}

export interface Settings {
  hotkey: string;
  hotkey_toggle: string;
  hotkey_cycle_profile: string;
  cancel_key: string;
  mode: string;
  mic: string | null;
  stt_provider: string;
  groq_api_key: string | null;
  groq_model: string;
  speech_lang: string;
  output_lang: string;
  gemini_model: string;
  gemini_api_key: string | null;
  rewrite_provider: string;
  groq_rewrite_model: string;
  providers: Provider[];
  rewrite_provider_name: string;
  rewrite_model: string;
  stt_provider_name: string;
  stt_model: string;
  wave_style: string;
  theme: string;
  profiles: Profile[];
  active_profile: string;
  dictionary: string[];
  snippets: Snippet[];
  history_enabled: boolean;
  audio_keep_days: number;
  python: string;
  sidecar: string;
  stt_port: number;
}

interface HistoryEntry {
  ts: number;
  profile: string;
  raw: string;
  final: string;
  audio_secs: number;
  stt_secs: number;
  total_secs: number;
}

interface AudioEntry {
  name: string;
  ts: number;
  secs: number;
  erro: string | null;
}

interface Retranscricao {
  texto: string;
  aviso: string | null;
}

interface Diagnostics {
  stt_provider: string;
  groq_key_set: boolean;
  sidecar_ok: boolean;
  sidecar_configured: boolean;
  sidecar_device: string;
  default_mic: string;
  gemini_key_set: boolean;
  gemini_model: string;
  recent: HistoryEntry[];
}

const TABS = ["Geral", "Provedores", "Insights", "Perfis", "Dicionário", "Snippets", "Histórico", "Áudios", "Diagnóstico", "Apoie"] as const;
type Tab = (typeof TABS)[number];

function App() {
  const params = new URLSearchParams(window.location.search);
  if (params.has("overlay")) {
    return <Overlay />;
  }
  if (params.has("toast")) {
    return <Toast />;
  }
  return <SettingsApp />;
}

function SettingsApp() {
  const [tab, setTab] = useState<Tab>("Geral");
  const [settings, setSettings] = useState<Settings | null>(null);
  const [saved, setSaved] = useState(true);
  const timer = useRef<number | undefined>(undefined);
  const first = useRef(true);

  useEffect(() => {
    invoke<Settings>("get_settings").then(setSettings);
    // troca de perfil pela bandeja: recarrega para a UI não sobrescrever com estado velho
    const un = listen("settings_changed", () => {
      invoke<Settings>("get_settings").then(setSettings);
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  useEffect(() => {
    document.documentElement.dataset.theme = settings?.theme === "light" ? "light" : "dark";
  }, [settings?.theme]);

  useEffect(() => {
    if (!settings) {
      return;
    }
    if (first.current) {
      first.current = false;
      return;
    }
    setSaved(false);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(async () => {
      await invoke("save_settings", { settings });
      setSaved(true);
    }, 600);
  }, [settings]);

  const patch = useCallback((p: Partial<Settings>) => {
    setSettings((s) => (s ? { ...s, ...p } : s));
  }, []);

  if (!settings) {
    return <div className="loading">Carregando...</div>;
  }

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand">
          <svg className="brand-mark" width="22" height="13" viewBox="0 0 28 16" fill="currentColor" aria-hidden>
            <rect x="1" y="7.4" width="26" height="1.2" rx="0.6" />
            <rect x="4" y="6" width="2.4" height="4" rx="1.2" />
            <rect x="6.93" y="4.75" width="2.4" height="6.5" rx="1.2" />
            <rect x="9.87" y="3" width="2.4" height="10" rx="1.2" />
            <rect x="12.8" y="1.5" width="2.4" height="13" rx="1.2" />
            <rect x="15.73" y="3" width="2.4" height="10" rx="1.2" />
            <rect x="18.67" y="4.75" width="2.4" height="6.5" rx="1.2" />
            <rect x="21.6" y="6" width="2.4" height="4" rx="1.2" />
          </svg>{" "}
          Open Flow
        </div>
        <nav>
          {TABS.map((t) => (
            <button key={t} className={t === tab ? "active" : ""} onClick={() => setTab(t)}>
              {t}
            </button>
          ))}
        </nav>
        <div className="save-state">{saved ? "✓ salvo" : "salvando..."}</div>
      </aside>
      <main className="content">
        {tab === "Geral" && <GeneralTab s={settings} patch={patch} />}
        {tab === "Provedores" && <ProvidersTab s={settings} patch={patch} />}
        {tab === "Insights" && <InsightsTab historyEnabled={settings.history_enabled} />}
        {tab === "Perfis" && <ProfilesTab s={settings} patch={patch} />}
        {tab === "Dicionário" && <DictTab s={settings} patch={patch} />}
        {tab === "Snippets" && <SnippetsTab s={settings} patch={patch} />}
        {tab === "Histórico" && <HistoryTab s={settings} patch={patch} />}
        {tab === "Áudios" && <AudiosTab s={settings} patch={patch} />}
        {tab === "Diagnóstico" && <DiagTab />}
        {tab === "Apoie" && <ApoieTab />}
      </main>
    </div>
  );
}

interface TabProps {
  s: Settings;
  patch: (p: Partial<Settings>) => void;
}

function SecretInput({
  value,
  onChange,
}: {
  value: string;
  onChange: (v: string) => void;
}) {
  const [show, setShow] = useState(false);
  return (
    <div className="secret-input">
      <input
        type={show ? "text" : "password"}
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
      <button
        type="button"
        className="eye"
        title={show ? "Ocultar" : "Revelar"}
        onClick={() => setShow(!show)}
      >
        {show ? (
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94" />
            <path d="M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19" />
            <path d="M14.12 14.12a3 3 0 1 1-4.24-4.24" />
            <line x1="1" y1="1" x2="23" y2="23" />
          </svg>
        ) : (
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z" />
            <circle cx="12" cy="12" r="3" />
          </svg>
        )}
      </button>
    </div>
  );
}

const MODS = isMac
  ? { ctrl: "Ctrl", alt: "Option", shift: "Shift", meta: "Cmd" }
  : { ctrl: "Ctrl", alt: "Alt", shift: "Shift", meta: "Win" };

/// nomes aceitos pelo Rust (platform::key_from_name)
function mainKeyName(code: string): string | null {
  if (/^Key[A-Z]$/.test(code)) {
    return code.slice(3);
  }
  if (/^Digit[0-9]$/.test(code)) {
    return code.slice(5);
  }
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) {
    return code;
  }
  if (/^Numpad[0-9]$/.test(code)) {
    return `NUMPAD${code.slice(6)}`;
  }
  const named: Record<string, string> = {
    Space: "SPACE", Enter: "ENTER", NumpadEnter: "ENTER", Tab: "TAB", Escape: "ESC",
    Backspace: "BACKSPACE", Delete: "DELETE", Insert: "INSERT", Home: "HOME", End: "END",
    PageUp: "PAGEUP", PageDown: "PAGEDOWN", ArrowUp: "UP", ArrowDown: "DOWN",
    ArrowLeft: "LEFT", ArrowRight: "RIGHT", CapsLock: "CAPSLOCK", ScrollLock: "SCROLLLOCK",
    Pause: "PAUSE", NumLock: "NUMLOCK",
  };
  return named[code] ?? null;
}

/// Campo que grava a combinação teclada. Combos só de modificadores (Ctrl+Win)
/// são fechados ao soltar; com tecla comum, no próprio keydown.
function HotkeyInput({
  value,
  onChange,
  placeholder,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
}) {
  const [capturing, setCapturing] = useState(false);
  const [pending, setPending] = useState<string[]>([]);
  const pendingRef = useRef<string[]>([]);
  const doneRef = useRef(false);

  const setP = (v: string[]) => {
    pendingRef.current = v;
    setPending(v);
  };

  const modsOf = (e: React.KeyboardEvent) => {
    const out: string[] = [];
    if (e.ctrlKey) out.push(MODS.ctrl);
    if (e.altKey) out.push(MODS.alt);
    if (e.shiftKey) out.push(MODS.shift);
    if (e.metaKey) out.push(MODS.meta);
    return out;
  };

  const commit = (combo: string, el: HTMLInputElement) => {
    doneRef.current = true;
    onChange(combo);
    el.blur();
  };

  return (
    <div className="hotkey-input">
      <input
        readOnly
        value={capturing ? pending.join("+") : value}
        placeholder={capturing ? "tecle a combinação..." : (placeholder ?? "sem atalho")}
        onFocus={() => {
          setCapturing(true);
          setP([]);
          doneRef.current = false;
        }}
        onBlur={() => {
          setCapturing(false);
          setP([]);
        }}
        onKeyDown={(e) => {
          e.preventDefault();
          const el = e.target as HTMLInputElement;
          const mods = modsOf(e);
          // Esc sozinho desiste da captura; com modificador, é atalho válido
          if (e.code === "Escape" && mods.length === 0) {
            el.blur();
            return;
          }
          setP(mods);
          const main = mainKeyName(e.code);
          if (main) {
            commit([...mods, main].join("+"), el);
          }
        }}
        onKeyUp={(e) => {
          if (doneRef.current) {
            return;
          }
          const held = modsOf(e);
          // último modificador solto: fecha o combo com o que estava pressionado
          if (held.length === 0 && pendingRef.current.length > 0) {
            commit(pendingRef.current.join("+"), e.target as HTMLInputElement);
          }
        }}
      />
      {value && !capturing && (
        <button type="button" className="hotkey-clear" title="Remover atalho" onClick={() => onChange("")}>
          ×
        </button>
      )}
    </div>
  );
}

function GeneralTab({ s, patch }: TabProps) {
  const [mics, setMics] = useState<string[]>([]);
  const [autostart, setAutostart] = useState(false);
  useEffect(() => {
    invoke<string[]>("list_mics").then(setMics);
    invoke<boolean>("get_autostart").then(setAutostart);
  }, []);

  return (
    <section>
      <h1>Geral</h1>
      <label className="field">
        <span>Atalho de ditado</span>
        <HotkeyInput value={s.hotkey} onChange={(v) => patch({ hotkey: v })} />
        <small>
          Dois jeitos no mesmo atalho: um toque rápido começa a gravar e outro toque termina; segurar
          grava enquanto estiver apertado e transcreve ao soltar. Vale também para o atalho de cada perfil.
        </small>
      </label>

      <label className="field">
        <span>Segundo atalho de ditado (opcional)</span>
        <HotkeyInput value={s.hotkey_toggle} onChange={(v) => patch({ hotkey_toggle: v })} />
        <small>
          Outra combinação para o mesmo ditado, com o mesmo funcionamento. Deixe em branco se não
          precisar; repetir aqui o atalho de cima não muda nada.
        </small>
      </label>

      <label className="field">
        <span>Atalho para trocar de perfil</span>
        <HotkeyInput
          value={s.hotkey_cycle_profile}
          onChange={(v) => patch({ hotkey_cycle_profile: v })}
        />
        <small>
          Avança para o próximo perfil da lista e mostra o nome dele na tela. Clique no campo e
          tecle a combinação — comece pelos modificadores.{" "}
          {isMac ? "Ex.: Ctrl+Option+P." : "Ex.: Ctrl+Alt+P."}
        </small>
      </label>

      <label className="field">
        <span>Cancelar ditado</span>
        <select
          value={s.cancel_key}
          onChange={(e) => patch({ cancel_key: e.target.value })}
        >
          <option value="">Desativado</option>
          <option value="ESC">Esc</option>
        </select>
        <small>Durante a gravação, essa tecla descarta o ditado sem inserir nada.</small>
      </label>

      <div className="field">
        <span>Tema</span>
        <label className="switch-row">
          <span className="switch">
            <input
              type="checkbox"
              checked={s.theme === "light"}
              onChange={(e) => patch({ theme: e.target.checked ? "light" : "dark" })}
            />
            <span className="slider" />
          </span>
          {s.theme === "light" ? "Claro" : "Escuro"}
        </label>
      </div>

      <div className="field">
        <span>Iniciar com o Windows</span>
        <label className="switch-row">
          <span className="switch">
            <input
              type="checkbox"
              checked={autostart}
              onChange={async (e) => {
                const enabled = e.target.checked;
                setAutostart(enabled);
                await invoke("set_autostart", { enabled });
              }}
            />
            <span className="slider" />
          </span>
          {autostart ? "Ativado" : "Desativado"}
        </label>
      </div>

      <label className="field">
        <span>Microfone</span>
        <select
          value={s.mic ?? ""}
          onChange={(e) => patch({ mic: e.target.value === "" ? null : e.target.value })}
        >
          <option value="">Padrão do sistema</option>
          {mics.map((m) => (
            <option key={m} value={m}>
              {m}
            </option>
          ))}
        </select>
      </label>

      <label className="field">
        <span>Estilo da onda no overlay</span>
        <select value={s.wave_style} onChange={(e) => patch({ wave_style: e.target.value })}>
          <option value="tech">Tech (curvas fluidas)</option>
          <option value="classic">Clássica (barras)</option>
        </select>
        <small>Aplicado na próxima gravação.</small>
      </label>

      <h2>Transcrição — voz → texto</h2>
      <p className="hint">
        Quem converte sua fala em texto bruto. Se o provedor escolhido falhar (limite, sem
        chave, GPU ocupada), o outro assume automaticamente.
      </p>
      <ModelPicker
        s={s}
        patch={patch}
        rotulo="Provedor da transcrição"
        provedor={s.stt_provider_name}
        modelo={s.stt_model}
        aoTrocarProvedor={(v, m) => patch({ stt_provider_name: v, stt_model: m })}
        aoTrocarModelo={(v) => patch({ stt_model: v })}
        opcaoVazia="— Local (GPU, sem nuvem) —"
        somenteTranscricao
        ajuda="Se o provedor escolhido falhar, o modo local assume automaticamente."
      />
      <label className="field">
        <span>Python do modo local</span>
        <input value={s.python} onChange={(e) => patch({ python: e.target.value })} />
        <small>
          Caminho do executável Python com o faster-whisper instalado. Sem ele (e sem o arquivo
          abaixo), o modo local não funciona nem como fallback da nuvem.
        </small>
      </label>
      <label className="field">
        <span>Script do sidecar</span>
        <input value={s.sidecar} onChange={(e) => patch({ sidecar: e.target.value })} />
        <small>Caminho do stt_server.py. Confira o estado dos dois na aba Diagnóstico.</small>
      </label>
      <label className="field">
        <span>Idioma da fala</span>
        <select
          value={s.speech_lang}
          onChange={(e) => patch({ speech_lang: e.target.value })}
        >
          <option value="pt">Português (Brasil)</option>
          <option value="en">English</option>
          <option value="es">Español</option>
          <option value="auto">Detectar automaticamente</option>
        </select>
        <small>Idioma fixo transcreve melhor que a detecção automática em frases curtas.</small>
      </label>

      <h2>Reescrita — texto → texto final</h2>
      <p className="hint">
        Quem limpa a narração (hesitações, autocorreções) e aplica o perfil de escrita ativo.
      </p>
      <label className="field">
        <span>Idioma de saída</span>
        <select
          value={s.output_lang}
          onChange={(e) => patch({ output_lang: e.target.value })}
        >
          <option value="same">Mesmo da fala</option>
          <option value="pt">Português (Brasil)</option>
          <option value="en">English</option>
          <option value="es">Español</option>
        </select>
        <small>
          Diferente do idioma da fala = o texto final sai traduzido (requer a chave Gemini).
        </small>
      </label>
      <ModelPicker
        s={s}
        patch={patch}
        rotulo="Provedor da reescrita"
        provedor={s.rewrite_provider_name}
        modelo={s.rewrite_model}
        aoTrocarProvedor={(v, m) => patch({ rewrite_provider_name: v, rewrite_model: m })}
        aoTrocarModelo={(v) => patch({ rewrite_model: v })}
        opcaoVazia="— nenhum (cola o texto bruto, sem reescrever) —"
      />
    </section>
  );
}

/// Escolha de provedor + modelo, usada tanto pela transcrição quanto pela
/// reescrita. Os modelos só são buscados quando o usuário clica, para não
/// gastar chamada de API ao abrir a aba.
function ModelPicker({
  s,
  rotulo,
  provedor,
  modelo,
  aoTrocarProvedor,
  aoTrocarModelo,
  opcaoVazia,
  somenteTranscricao,
  ajuda,
}: {
  s: Settings;
  patch: (p: Partial<Settings>) => void;
  rotulo: string;
  provedor: string;
  modelo: string;
  /// provedor e modelo juntos de propósito: dois `patch` seguidos correm o
  /// risco de um sobrescrever o outro antes do React reconciliar
  aoTrocarProvedor: (v: string, modeloSugerido: string) => void;
  aoTrocarModelo: (v: string) => void;
  opcaoVazia: string;
  /// mostra só os modelos que o provedor declara como transcrição
  somenteTranscricao?: boolean;
  ajuda?: string;
}) {
  const [resultado, setResultado] = useState<ProviderTest | null>(null);
  const [carregando, setCarregando] = useState(false);
  const [verTodos, setVerTodos] = useState(false);
  /// volta ao campo livre quando o modelo que ele quer não está no catálogo
  const [digitando, setDigitando] = useState(false);
  const atual = s.providers.find((p) => p.name === provedor);
  const idLista = `modelos-${rotulo.replace(/\W+/g, "-")}`;

  useEffect(() => {
    setResultado(null);
    setVerTodos(false);
    setDigitando(false);
  }, [provedor]);

  const carregar = async () => {
    if (!atual) return;
    setCarregando(true);
    if (ehChatgpt(atual)) {
      // catálogo da assinatura: envelope próprio e sem chave, por isso não
      // passa pelo test_provider
      try {
        const modelos = await invoke<string[]>("siwc_modelos");
        setResultado({
          ok: true,
          models: modelos,
          message: `${modelos.length} opções da sua assinatura`,
          no_catalog: false,
          // a assinatura não transcreve: o fluxo recusa áudio
          audio: false,
          transcription_models: [],
          transcription_source: "",
        });
      } catch (e) {
        setResultado({
          ok: false,
          models: [],
          message: String(e),
          no_catalog: false,
          audio: false,
          transcription_models: [],
          transcription_source: "",
        });
      }
    } else {
      setResultado(
        await invoke<ProviderTest>("test_provider", {
          baseUrl: atual.base_url,
          apiKey: atual.api_key,
        })
      );
    }
    setCarregando(false);
  };

  const filtrando = !!somenteTranscricao && !verTodos;
  const modelos = !resultado
    ? []
    : filtrando
      ? resultado.transcription_models
      : resultado.models;

  if (s.providers.length === 0) {
    return (
      <div className="field">
        <span>{rotulo}</span>
        <p className="hint">
          Nenhum provedor cadastrado — vá na aba <b>Provedores</b> e adicione um.
        </p>
      </div>
    );
  }

  return (
    <>
      <label className="field">
        <span>{rotulo}</span>
        <select
          value={provedor}
          onChange={(e) => {
            const novo = e.target.value;
            const alvo = s.providers.find((x) => x.name === novo);
            aoTrocarProvedor(novo, ehChatgpt(alvo) ? PADRAO_CHATGPT : "");
          }}
        >
          <option value="">{opcaoVazia}</option>
          {s.providers
            // conexão com o ChatGPT não aparece na transcrição: o fluxo de uso
            // do plano recusa áudio, então oferecê-la só levaria a um erro
            .filter((p) => !(somenteTranscricao && ehChatgpt(p)))
            .map((p) => (
              <option key={p.name} value={p.name}>
                {p.name}
              </option>
            ))}
        </select>
        <small>{ajuda ?? "Cadastre e remova provedores na aba Provedores."}</small>
      </label>

      {atual && (
        <label className="field">
          <span>Modelo</span>
          <div className="modelo-linha">
            {modelos.length > 0 && !digitando ? (
              // Com catálogo na mão, uma lista de verdade. O `datalist` filtra
              // pelo texto que já está no campo, então um modelo escolhido
              // escondia todos os outros — parecia catálogo truncado.
              <select
                value={modelos.includes(semEsforco(modelo)) ? semEsforco(modelo) : ""}
                onChange={(e) => {
                  if (e.target.value === OUTRO) {
                    setDigitando(true);
                  } else {
                    // preserva o esforço já escolhido ao trocar de modelo
                    aoTrocarModelo(comEsforco(e.target.value, soEsforco(modelo)));
                  }
                }}
              >
                {!modelos.includes(semEsforco(modelo)) && (
                  <option value="">
                    {modelo ? `${modelo} (fora da lista)` : "— escolha um modelo —"}
                  </option>
                )}
                {modelos.map((m) => (
                  <option key={m} value={m}>
                    {m}
                  </option>
                ))}
                <option value={OUTRO}>outro — digitar à mão</option>
              </select>
            ) : (
              <input
                list={idLista}
                value={modelo}
                placeholder="digite ou carregue a lista"
                onChange={(e) => aoTrocarModelo(e.target.value)}
              />
            )}
            <button onClick={carregar} disabled={carregando}>
              {carregando ? "..." : modelos.length > 0 ? "Recarregar" : "Carregar modelos"}
            </button>
          </div>
          <div className="modelo-linha esforco-linha">
            <span className="unidade">Esforço</span>
            <select
              value={soEsforco(modelo)}
              onChange={(e) => aoTrocarModelo(comEsforco(modelo, e.target.value))}
            >
              {ESFORCOS.map((e) => (
                <option key={e.v} value={e.v}>
                  {e.rotulo}
                </option>
              ))}
            </select>
          </div>
          <datalist id={idLista}>
            {modelos.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
          {!resultado && (
            <small>Buscar a lista consome uma chamada da sua conta — só acontece no clique.</small>
          )}
          {resultado && (
            <small>
              {resultado.message}
              {somenteTranscricao && resultado.transcription_source && (
                <> · {resultado.transcription_source}</>
              )}
              {somenteTranscricao && resultado.models.length > 0 && (
                <>
                  {" · "}
                  <button type="button" className="link" onClick={() => setVerTodos(!verTodos)}>
                    {verTodos
                      ? `só transcrição (${resultado.transcription_models.length})`
                      : `ver todos (${resultado.models.length})`}
                  </button>
                </>
              )}
            </small>
          )}
        </label>
      )}
    </>
  );
}

const PRESETS: { name: string; base_url: string; nota: string }[] = [
  { name: "Groq", base_url: "https://api.groq.com/openai/v1", nota: "console.groq.com/keys" },
  { name: "OpenAI", base_url: "https://api.openai.com/v1", nota: "platform.openai.com/api-keys" },
  { name: "OpenRouter", base_url: "https://openrouter.ai/api/v1", nota: "openrouter.ai/keys" },
  { name: "Gemini", base_url: "https://generativelanguage.googleapis.com/v1beta/openai", nota: "aistudio.google.com/apikey" },
  { name: "OmniRoute", base_url: "https://omniroute.ihub.click/v1", nota: "seu gateway" },
];

/// Conectar, mostrar quem está conectado e desconectar. Os tokens nunca passam
/// por aqui: o backend guarda protegido e devolve só o e-mail e os escopos.
function ContaChatgptCard() {
  const [conta, setConta] = useState<ContaChatgpt | null>(null);
  const [ocupado, setOcupado] = useState(false);
  const [erro, setErro] = useState("");

  useEffect(() => {
    invoke<ContaChatgpt | null>("siwc_conta").then(setConta).catch(() => {});
  }, []);

  const conectar = async () => {
    setOcupado(true);
    setErro("");
    try {
      setConta(await invoke<ContaChatgpt>("siwc_conectar"));
    } catch (e) {
      setErro(String(e));
    }
    setOcupado(false);
  };

  const sair = async () => {
    await invoke("siwc_desconectar");
    setConta(null);
  };

  if (!conta) {
    return (
      <div className="field">
        <button onClick={conectar} disabled={ocupado}>
          {ocupado ? "aguardando a autorização no navegador..." : "Continue with ChatGPT"}
        </button>
        <small>
          Abre o navegador para você autorizar. A reescrita passa a sair da cota do seu plano
          ChatGPT, sem chave de API. <b>Não serve para transcrição</b> — esse fluxo não aceita
          áudio.
        </small>
        {erro && <p className="prov-erro">✗ {erro}</p>}
      </div>
    );
  }

  return (
    <div className="field">
      <div className="modelo-linha">
        <span className="conta-chatgpt">
          Conectado como <b>{conta.email}</b>
        </span>
        <button onClick={sair}>Desconectar</button>
      </div>
      {conta.usa_o_plano ? (
        <small>
          A cota do seu plano paga a reescrita. Acompanhe o consumo em{" "}
          <b>ChatGPT → Configurações → Uso</b>.
        </small>
      ) : (
        <p className="prov-erro">
          ✗ Você entrou, mas não autorizou o uso do plano — a reescrita por aqui não vai
          funcionar. Desconecte e conecte de novo, aceitando a permissão de uso do plano.
        </p>
      )}
    </div>
  );
}

function ProvidersTab({ s, patch }: TabProps) {
  const [estado, setEstado] = useState<Record<number, { testando?: boolean; r?: ProviderTest }>>({});

  const update = (i: number, p: Partial<Provider>) =>
    patch({ providers: s.providers.map((pr, j) => (j === i ? { ...pr, ...p } : pr)) });

  const remover = (i: number) => {
    const alvo = s.providers[i];
    const emUso = alvo.name === s.rewrite_provider_name;
    if (emUso && !confirm(`"${alvo.name}" está em uso pela reescrita.\n\nRemovendo, o ditado passa a colar o texto bruto até você escolher outro provedor. Remover mesmo assim?`)) {
      return;
    }
    patch({
      providers: s.providers.filter((_, j) => j !== i),
      ...(emUso ? { rewrite_provider_name: "", rewrite_model: "" } : {}),
    });
    setEstado({});
  };

  const testar = async (i: number) => {
    const p = s.providers[i];
    setEstado((e) => ({ ...e, [i]: { testando: true } }));
    const r = await invoke<ProviderTest>("test_provider", {
      baseUrl: p.base_url,
      apiKey: p.api_key,
    });
    setEstado((e) => ({ ...e, [i]: { r } }));
  };

  const adicionar = (preset?: (typeof PRESETS)[number]) => {
    const base = preset ?? { name: "Novo provedor", base_url: "" };
    let nome = base.name;
    for (let n = 2; s.providers.some((p) => p.name === nome); n++) {
      nome = `${base.name} ${n}`;
    }
    patch({
      providers: [
        ...s.providers,
        { name: nome, base_url: base.base_url, api_key: null, timeout_secs: 0, kind: "api_key" },
      ],
    });
  };

  /// Medido em 07/10 no ditado clínico real: `luna:low` em 18,3s acertou as 7
  /// checagens de fidelidade; `terra:medium` levou 27,2s para o mesmo acerto, e
  /// o esforço alto passou de 60s **e piorou**. Por isso o padrão já vem pronto
  /// em vez de deixar o campo vazio.
  const adicionarChatgpt = () => {
    let nome = "ChatGPT";
    for (let n = 2; s.providers.some((p) => p.name === nome); n++) nome = `ChatGPT ${n}`;
    patch({
      providers: [
        ...s.providers,
        {
          name: nome,
          base_url: "https://api.openai.com/v1",
          api_key: null,
          // a Responses API pensa antes de responder; com o padrão de 4s nunca
          // chegaria ao fim
          timeout_secs: 60,
          kind: "chatgpt_oauth",
        },
      ],
    });
  };

  return (
    <section>
      <h1>Provedores</h1>
      <p className="hint">
        Qualquer serviço compatível com a API da OpenAI serve — basta o endereço base e a chave.
        O botão <b>Testar</b> conecta e lista os modelos disponíveis; é a única hora em que o app
        consome uma chamada da sua conta.
      </p>

      {s.providers.length === 0 && (
        <p className="hint">Nenhum provedor cadastrado. A reescrita está usando o modo antigo.</p>
      )}

      {s.providers.map((p, i) => {
        const st = estado[i];
        const emUso = p.name === s.rewrite_provider_name;
        return (
          <div key={i} className={`card ${emUso ? "card-active" : ""}`}>
            <div className="card-head">
              <input
                className="card-title"
                value={p.name}
                placeholder="apelido"
                onChange={(e) => {
                  const antigo = p.name;
                  update(i, { name: e.target.value });
                  if (s.rewrite_provider_name === antigo) {
                    patch({ rewrite_provider_name: e.target.value });
                  }
                }}
              />
              {emUso && <span className="badge-uso">reescrita</span>}
              {!ehChatgpt(p) && (
                <button onClick={() => testar(i)} disabled={st?.testando}>
                  {st?.testando ? "testando..." : "Testar"}
                </button>
              )}
              <button className="danger" onClick={() => remover(i)}>
                remover
              </button>
            </div>

            {ehChatgpt(p) ? (
              <ContaChatgptCard />
            ) : (
              <>
                <label className="field">
                  <span>Endereço base</span>
                  <input
                    value={p.base_url}
                    placeholder="https://exemplo.com/v1"
                    onChange={(e) => update(i, { base_url: e.target.value })}
                  />
                </label>

                <label className="field">
                  <span>Chave da API</span>
                  <SecretInput
                    value={p.api_key ?? ""}
                    onChange={(v) => update(i, { api_key: v || null })}
                  />
                </label>
              </>
            )}

            <label className="field">
              <span>Tempo limite da reescrita</span>
              <div className="modelo-linha">
                <input
                  type="number"
                  min={0}
                  max={60}
                  value={p.timeout_secs || ""}
                  placeholder="4 (padrão)"
                  onChange={(e) => update(i, { timeout_secs: Number(e.target.value) || 0 })}
                />
                <span className="unidade">segundos</span>
              </div>
              <small>
                Vazio usa o padrão de 4s, que serve para Groq e Gemini. Gateway agregador
                costuma precisar de mais: se o ditado sair sem correção e o tempo bater o
                dobro deste valor, foi timeout — a chamada é repetida uma vez antes de
                desistir.
              </small>
            </label>

            {st?.r && (
              <p className={st.r.ok ? "prov-ok" : "prov-erro"}>
                {st.r.ok ? "✓ " : "✗ "}
                {st.r.message}
              </p>
            )}
            {st?.r?.models && st.r.models.length > 0 && (
              <details className="prov-modelos">
                <summary>ver modelos ({st.r.models.length})</summary>
                <div>{st.r.models.join("  ·  ")}</div>
              </details>
            )}
          </div>
        );
      })}

      <div className="prov-add">
        <button onClick={() => adicionar()}>+ Adicionar provedor</button>
        {!s.providers.some(ehChatgpt) && (
          <button onClick={adicionarChatgpt} title="usa a cota do seu plano ChatGPT">
            + Continue with ChatGPT
          </button>
        )}
        {PRESETS.filter((p) => !s.providers.some((x) => x.base_url === p.base_url)).map((p) => (
          <button key={p.name} className="preset" onClick={() => adicionar(p)} title={p.nota}>
            + {p.name}
          </button>
        ))}
      </div>
    </section>
  );
}

function ProfilesTab({ s, patch }: TabProps) {
  const update = (i: number, p: Partial<Profile>) => {
    const profiles = s.profiles.map((pr, j) => (j === i ? { ...pr, ...p } : pr));
    const renamed = p.name !== undefined && s.profiles[i].name === s.active_profile;
    patch({ profiles, ...(renamed ? { active_profile: p.name! } : {}) });
  };
  const remove = (i: number) => {
    const profiles = s.profiles.filter((_, j) => j !== i);
    patch({
      profiles,
      active_profile:
        s.profiles[i].name === s.active_profile
          ? (profiles[0]?.name ?? "")
          : s.active_profile,
    });
  };

  return (
    <section>
      <h1>Perfis de escrita</h1>
      <p className="hint">
        O perfil ativo define o estilo que a IA aplica ao texto ditado. As regras de limpeza
        (remover hesitações e autocorreções) valem para todos. O campo de texto de cada perfil
        é a instrução dada à IA — descreva ali a personalidade, o tom e o formato que o texto
        final deve ter.
      </p>
      {s.profiles.map((p, i) => (
        <div key={i} className={`card ${p.name === s.active_profile ? "card-active" : ""}`}>
          <div className="card-head">
            <label className="radio-active">
              <input
                type="radio"
                checked={p.name === s.active_profile}
                onChange={() => patch({ active_profile: p.name })}
              />
              ativo
            </label>
            <input
              className="card-title"
              value={p.name}
              onChange={(e) => update(i, { name: e.target.value })}
            />
            <button className="danger" onClick={() => remove(i)}>
              remover
            </button>
          </div>
          <textarea
            rows={2}
            value={p.style}
            placeholder='Instrução de estilo para a IA — personalidade, tom e formato do texto. Ex.: "e-mail curto e cordial, com saudação e fecho"'
            onChange={(e) => update(i, { style: e.target.value })}
          />
          <div className="profile-hotkey">
            <span>Atalho próprio</span>
            <HotkeyInput value={p.hotkey} onChange={(v) => update(i, { hotkey: v })} />
          </div>
          <details className="profile-rewrite">
            <summary>
              Transcrição e destino:{" "}
              {[(p.stt_local_only || p.save_only) && "só local", p.save_only && "salva para o laudo"]
                .filter(Boolean)
                .join(", ") || "como o resto do app"}
            </summary>
            <label className="check">
              <input
                type="checkbox"
                checked={!!p.stt_local_only || !!p.save_only}
                disabled={!!p.save_only}
                onChange={(e) => update(i, { stt_local_only: e.target.checked })}
              />
              Transcrever só neste computador (o áudio não vai para a nuvem; a reescrita segue o provedor do perfil)
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={!!p.save_only}
                onChange={(e) =>
                  // laudo é dado de paciente: ligar o laudo liga o "só local" junto
                  update(i, e.target.checked ? { save_only: true, stt_local_only: true } : { save_only: false })
                }
              />
              Salvar para o laudo: pasta de ditados, texto cru, sem reescrita
            </label>
            <textarea
              rows={2}
              value={p.stt_prompt ?? ""}
              placeholder="Vocabulário para a transcrição deste perfil (termos que o Whisper deve reconhecer). Vazio usa o dicionário."
              onChange={(e) => update(i, { stt_prompt: e.target.value })}
            />
            {p.save_only && (
              <div className="history-controls">
                <button onClick={() => invoke("open_ditados_dir")}>Abrir pasta de ditados</button>
              </div>
            )}
            <p className="hint">
              "Salvar para o laudo" grava cada ditado como .wav e .json (texto cru) na pasta de ditados, para
              outro programa buscar — é o caminho do laudo do TOMO. O texto cru também é colado, fora do
              histórico do Win+V, e não entra no histórico do app nem na aba Áudios. Fica pelo prazo da aba
              Áudios, mesmo com o histórico desligado (com "Não guardar" lá, fica 7 dias).
            </p>
          </details>
          <details className="profile-rewrite">
            <summary>
              Reescrita:{" "}
              {p.rewrite_provider_name
                ? `${p.rewrite_provider_name} / ${p.rewrite_model || "sem modelo"}`
                : "igual ao resto do app"}
            </summary>
            <ModelPicker
              s={s}
              patch={patch}
              rotulo="Provedor só deste perfil"
              provedor={p.rewrite_provider_name}
              modelo={p.rewrite_model}
              aoTrocarProvedor={(v, m) =>
                // provedor em branco volta ao global, e aí o modelo não faz sentido
                update(i, { rewrite_provider_name: v, rewrite_model: v ? m : "" })
              }
              aoTrocarModelo={(v) => update(i, { rewrite_model: v })}
              opcaoVazia="usar o provedor escolhido em Reescrita"
              ajuda="Troca qualidade por tempo neste perfil só. Anotação que vai para prontuário aceita esperar por um modelo mais cuidadoso; ditado corrido, não."
            />
          </details>
        </div>
      ))}
      <p className="hint">
        O atalho próprio dita direto no estilo daquele perfil, sem trocar o perfil ativo. Clique
        no campo e tecle a combinação; comece pelos modificadores. Todo atalho de ditado é misto:
        um toque rápido começa e outro toque termina; segurar grava até soltar. Em <b>Reescrita</b>, cada
        perfil pode usar um provedor diferente do resto do app.
      </p>
      <button
        onClick={() =>
          patch({
            profiles: [
              ...s.profiles,
              {
                name: "Novo perfil",
                style: "",
                hotkey: "",
                hotkey_mode: "hold",
                rewrite_provider_name: "",
                rewrite_model: "",
                stt_local_only: false,
                stt_prompt: "",
                save_only: false,
              },
            ],
          })
        }
      >
        + Adicionar perfil
      </button>
    </section>
  );
}

function DictTab({ s, patch }: TabProps) {
  const update = (i: number, v: string) =>
    patch({ dictionary: s.dictionary.map((d, j) => (j === i ? v : d)) });

  return (
    <section>
      <h1>Dicionário pessoal</h1>
      <p className="hint">
        Nomes próprios e termos que devem ser grafados exatamente assim — com espaços, acentos e
        maiúsculas. Ex.: Macks Wendhell, Wispr Flow.
      </p>
      {s.dictionary.map((term, i) => (
        <div key={i} className="snippet-row">
          <input
            placeholder="termo ou nome próprio"
            value={term}
            onChange={(e) => update(i, e.target.value)}
          />
          <button
            className="danger"
            onClick={() => patch({ dictionary: s.dictionary.filter((_, j) => j !== i) })}
          >
            ×
          </button>
        </div>
      ))}
      <button onClick={() => patch({ dictionary: [...s.dictionary, ""] })}>
        + Adicionar termo
      </button>
    </section>
  );
}

function SnippetsTab({ s, patch }: TabProps) {
  const update = (i: number, p: Partial<Snippet>) =>
    patch({ snippets: s.snippets.map((sn, j) => (j === i ? { ...sn, ...p } : sn)) });

  return (
    <section>
      <h1>Snippets</h1>
      <p className="hint">
        Diga o gatilho durante o ditado e ele vira o texto completo. Ex.: "link do meu canal" →
        https://www.youtube.com/@mackswendhell
      </p>
      {s.snippets.map((sn, i) => (
        <div key={i} className="snippet-row">
          <input
            placeholder="gatilho falado"
            value={sn.trigger}
            onChange={(e) => update(i, { trigger: e.target.value })}
          />
          <span>→</span>
          <input
            placeholder="texto inserido"
            value={sn.text}
            onChange={(e) => update(i, { text: e.target.value })}
          />
          <button
            className="danger"
            onClick={() => patch({ snippets: s.snippets.filter((_, j) => j !== i) })}
          >
            ×
          </button>
        </div>
      ))}
      <button onClick={() => patch({ snippets: [...s.snippets, { trigger: "", text: "" }] })}>
        + Adicionar snippet
      </button>
    </section>
  );
}

function HistoryTab({ s, patch }: TabProps) {
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [query, setQuery] = useState("");

  const load = useCallback(
    (q: string) => invoke<HistoryEntry[]>("get_history", { query: q, limit: 50 }).then(setEntries),
    [],
  );
  // sem espera, cada tecla relê e reparseia o history.jsonl inteiro do disco
  useEffect(() => {
    const t = window.setTimeout(() => load(query), 250);
    return () => window.clearTimeout(t);
  }, [query, load]);

  return (
    <section>
      <h1>Histórico</h1>
      <div className="history-controls">
        <label className="check">
          <input
            type="checkbox"
            checked={s.history_enabled}
            onChange={(e) => patch({ history_enabled: e.target.checked })}
          />
          Salvar histórico localmente
        </label>
        <button
          className="danger"
          onClick={async () => {
            await invoke("clear_history");
            load(query);
          }}
        >
          Apagar tudo
        </button>
      </div>
      <input
        className="search"
        placeholder="Buscar nas transcrições..."
        value={query}
        onChange={(e) => setQuery(e.target.value)}
      />
      {entries.length === 0 && <p className="hint">Nenhuma transcrição.</p>}
      {entries.map((e, i) => (
        <div key={i} className="card">
          <div className="entry-meta">
            {new Date(e.ts * 1000).toLocaleString("pt-BR")} · {e.profile} ·{" "}
            {e.total_secs.toFixed(1)}s
          </div>
          <div className="entry-final">{e.final}</div>
          <details>
            <summary>transcrição bruta</summary>
            <div className="entry-raw">{e.raw}</div>
          </details>
        </div>
      ))}
    </section>
  );
}

// estado de cada linha da aba Áudios, pela chave `name`
type AudioRow =
  | { st: "transcrevendo" }
  | { st: "ok"; texto: string; aviso: string | null }
  | { st: "erro"; msg: string };

function AudiosTab({ s, patch }: TabProps) {
  const [audios, setAudios] = useState<AudioEntry[]>([]);
  const [rows, setRows] = useState<Record<string, AudioRow>>({});
  const [tocando, setTocando] = useState<{ name: string; url: string } | null>(null);

  const load = useCallback(() => invoke<AudioEntry[]>("list_audios").then(setAudios), []);
  useEffect(() => {
    load();
  }, [load]);
  // prazo menor apaga arquivos ao salvar (debounce de 600ms): relê depois disso
  const keepAnterior = useRef(s.audio_keep_days);
  useEffect(() => {
    if (keepAnterior.current === s.audio_keep_days) return;
    keepAnterior.current = s.audio_keep_days;
    const t = window.setTimeout(load, 1200);
    return () => window.clearTimeout(t);
  }, [s.audio_keep_days, load]);
  // o object URL segura o WAV inteiro na memória até ser revogado
  useEffect(() => () => {
    if (tocando) URL.revokeObjectURL(tocando.url);
  }, [tocando]);

  const ouvir = async (name: string) => {
    try {
      const buf = await invoke<ArrayBuffer>("read_audio", { name });
      setTocando({ name, url: URL.createObjectURL(new Blob([buf], { type: "audio/wav" })) });
    } catch (e) {
      setRows((r) => ({ ...r, [name]: { st: "erro", msg: `não abriu o áudio: ${String(e)}` } }));
    }
  };

  const transcrever = async (name: string) => {
    setRows((r) => ({ ...r, [name]: { st: "transcrevendo" } }));
    try {
      const res = await invoke<Retranscricao>("transcribe_audio", { name });
      setRows((r) => ({ ...r, [name]: { st: "ok", texto: res.texto, aviso: res.aviso } }));
    } catch (e) {
      setRows((r) => ({ ...r, [name]: { st: "erro", msg: String(e) } }));
    }
    load();
  };

  return (
    <section>
      <h1>Áudios</h1>
      <p className="hint">
        Cada ditado fica guardado aqui, sem criptografia, enquanto "Salvar histórico localmente" estiver ligado.
        Se a transcrição falhar, transcreva de novo: o texto vai para a área de transferência. Pelo ícone da
        bandeja, "Transcrever último áudio" cola direto onde estiver o cursor.
      </p>
      <div className="history-controls">
        <label className="field">
          <span>Guardar por</span>
          <select
            value={s.audio_keep_days}
            onChange={(e) => patch({ audio_keep_days: Number(e.target.value) })}
          >
            {[1, 3, 7, 15, 30].map((d) => (
              <option key={d} value={d}>
                {d} {d === 1 ? "dia" : "dias"}
              </option>
            ))}
            <option value={0}>Não guardar</option>
          </select>
        </label>
        <button onClick={() => invoke("open_audio_dir")}>Abrir pasta</button>
      </div>
      {audios.length === 0 && <p className="hint">Nenhum áudio guardado.</p>}
      {audios.map((a) => {
        const row = rows[a.name];
        return (
          <div key={a.name} className="card">
            <div className="card-head">
              <div className="card-title">
                {new Date(a.ts * 1000).toLocaleString("pt-BR")} · {a.secs.toFixed(1)}s
                {a.erro && <span className="audio-falhou"> · falhou</span>}
              </div>
              <button onClick={() => ouvir(a.name)}>▶ Ouvir</button>
              <button disabled={row?.st === "transcrevendo"} onClick={() => transcrever(a.name)}>
                {row?.st === "transcrevendo" ? "Transcrevendo..." : "Transcrever"}
              </button>
            </div>
            {tocando?.name === a.name && <audio className="audio-player" src={tocando.url} controls autoPlay />}
            {a.erro && row?.st !== "ok" && <div className="entry-raw">{a.erro}</div>}
            {row?.st === "ok" && (
              <>
                <div className="entry-meta">
                  copiado para a área de transferência{row.aviso ? ` · sem correção: ${row.aviso}` : ""}
                </div>
                <div className="entry-final">{row.texto}</div>
              </>
            )}
            {row?.st === "erro" && <div className="audio-falhou">{row.msg}</div>}
          </div>
        );
      })}
    </section>
  );
}

function countWords(text: string): number {
  return text.trim().split(/\s+/).filter(Boolean).length;
}

function dayKey(d: Date): string {
  return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
}

function InsightsTab({ historyEnabled }: { historyEnabled: boolean }) {
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  useEffect(() => {
    invoke<HistoryEntry[]>("get_history", { limit: 5000 }).then(setEntries);
  }, []);

  const totalWords = entries.reduce((a, e) => a + countWords(e.final), 0);
  const audioSecs = entries.reduce((a, e) => a + e.audio_secs, 0);
  const wpm = audioSecs > 0 ? totalWords / (audioSecs / 60) : 0;
  const avgLatency =
    entries.length > 0 ? entries.reduce((a, e) => a + e.total_secs, 0) / entries.length : 0;

  const byDay = new Map<string, number>();
  for (const e of entries) {
    const k = dayKey(new Date(e.ts * 1000));
    byDay.set(k, (byDay.get(k) ?? 0) + 1);
  }

  let streak = 0;
  const cursor = new Date();
  if (!byDay.has(dayKey(cursor))) {
    cursor.setDate(cursor.getDate() - 1);
  }
  while (byDay.has(dayKey(cursor))) {
    streak++;
    cursor.setDate(cursor.getDate() - 1);
  }

  const WEEKS = 20;
  const today = new Date();
  const start = new Date(today);
  start.setDate(start.getDate() - start.getDay() - (WEEKS - 1) * 7);
  const cells: { count: number; future: boolean }[] = [];
  for (let w = 0; w < WEEKS; w++) {
    for (let d = 0; d < 7; d++) {
      const day = new Date(start);
      day.setDate(start.getDate() + w * 7 + d);
      cells.push({ count: byDay.get(dayKey(day)) ?? 0, future: day > today });
    }
  }
  const lvl = (c: number) => (c === 0 ? 0 : c < 3 ? 1 : c < 8 ? 2 : 3);

  const byProfile = new Map<string, { words: number; count: number }>();
  for (const e of entries) {
    const p = byProfile.get(e.profile) ?? { words: 0, count: 0 };
    p.words += countWords(e.final);
    p.count += 1;
    byProfile.set(e.profile, p);
  }

  return (
    <section>
      <h1>Insights</h1>
      {!historyEnabled && (
        <p className="hint">O histórico está desativado — os insights só contam ditados gravados nele.</p>
      )}
      <div className="diag-grid">
        <div className="card stat">
          <span className="stat-big">{Math.round(wpm)}</span>
          <span className="stat-label">palavras por minuto</span>
        </div>
        <div className="card stat">
          <span className="stat-big">{totalWords}</span>
          <span className="stat-label">palavras ditadas</span>
        </div>
        <div className="card stat">
          <span className="stat-big">{entries.length}</span>
          <span className="stat-label">ditados</span>
        </div>
        <div className="card stat">
          <span className="stat-big">{avgLatency.toFixed(1)}s</span>
          <span className="stat-label">latência média</span>
        </div>
      </div>
      <div className="card streak-card">
        <b>
          {streak > 0 ? `${streak} ${streak === 1 ? "dia seguido" : "dias seguidos"}` : "sem sequência ativa"}
        </b>
        <div className="heatmap">
          {cells.map((c, i) => (
            <span
              key={i}
              className={`hm hm-${lvl(c.count)} ${c.future ? "hm-future" : ""}`}
              title={`${c.count} ditado(s)`}
            />
          ))}
        </div>
      </div>
      {byProfile.size > 0 && (
        <div className="card">
          <b>Por perfil</b>
          {[...byProfile.entries()]
            .sort((a, b) => b[1].words - a[1].words)
            .map(([name, p]) => (
              <div key={name} className="profile-stat">
                {name}: {p.words} palavras em {p.count} ditado(s)
              </div>
            ))}
        </div>
      )}
    </section>
  );
}

function DiagTab() {
  const [d, setD] = useState<Diagnostics | null>(null);
  useEffect(() => {
    invoke<Diagnostics>("get_diagnostics").then(setD);
  }, []);

  if (!d) {
    return (
      <section>
        <h1>Diagnóstico</h1>
        <p className="hint">Verificando...</p>
      </section>
    );
  }
  const avg =
    d.recent.length > 0
      ? d.recent.reduce((a, e) => a + e.total_secs, 0) / d.recent.length
      : null;

  return (
    <section>
      <h1>Diagnóstico</h1>
      <div className="diag-grid">
        <div className="card">
          <b>Transcrição (provedor: {d.stt_provider === "groq" ? "Groq nuvem" : "Local GPU"})</b>
          <div>Groq: {d.groq_key_set ? "● chave configurada" : "○ sem chave"}</div>
          <div>
            Local:{" "}
            {d.sidecar_ok
              ? `● quente (${d.sidecar_device})`
              : d.sidecar_configured
                ? "○ descarregado (sob demanda)"
                : "○ não configurado"}
          </div>
          {!d.sidecar_configured && (
            <div className="hint">
              Sem Python e sidecar válidos o modo local nunca sobe — nem como fallback. Configure
              os caminhos em Geral.
            </div>
          )}
        </div>
        <div className="card">
          <b>Microfone padrão</b>
          <div>{d.default_mic}</div>
        </div>
        <div className="card">
          <b>Gemini</b>
          <div>
            {d.gemini_key_set ? "● chave configurada" : "○ sem chave"} · {d.gemini_model}
          </div>
        </div>
        <div className="card">
          <b>Latência média (últimos {d.recent.length})</b>
          <div>{avg !== null ? `${avg.toFixed(2)}s do soltar ao texto` : "sem dados"}</div>
        </div>
      </div>
    </section>
  );
}

const PIX_KEY = "mackspls@hotmail.com";

function ApoieTab() {
  const [copied, setCopied] = useState(false);
  return (
    <section>
      <h1>Apoie o Open Flow</h1>
      <p className="hint">
        O Open Flow é gratuito e open source, criado para tornar a escrita por voz mais
        acessível, prática e inteligente.
      </p>
      <div className="diag-grid apoie">
        <div className="card">
          <b>Quem faz</b>
          <img src={canalImg} alt="Canal Macks Wendhell — Inteligência Aplicada" />
          <p>
            Conteúdos práticos sobre IA, trabalho e produtividade no canal do criador do app.
          </p>
          <button onClick={() => openUrl("https://www.youtube.com/@mackswendhell")}>
            Conhecer o canal
          </button>
        </div>
        <div className="card">
          <b>Apoio voluntário</b>
          <img src={pixQrImg} alt="QR code Pix" className="apoie-qr" />
          <p>
            Se o app facilita seu trabalho, você pode apoiar o desenvolvimento com uma
            contribuição via Pix — qualquer valor ajuda a manter o projeto ativo. O apoio é
            totalmente opcional: o Open Flow continuará gratuito e aberto para todos.
          </p>
          <button
            onClick={() =>
              navigator.clipboard.writeText(PIX_KEY).then(() => {
                setCopied(true);
                window.setTimeout(() => setCopied(false), 2000);
              })
            }
          >
            {copied ? "✓ chave copiada" : `Copiar chave Pix (${PIX_KEY})`}
          </button>
        </div>
      </div>
    </section>
  );
}

export default App;
