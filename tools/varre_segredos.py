"""Barreira contra vazamento de chave antes de qualquer push.

Lê as chaves reais do usuário (settings.json e chatgpt.json em %APPDATA%\\OpenFlow,
ou ~/Library/Application Support/OpenFlow no macOS) e procura cada valor, mais os
padrões conhecidos de chave de API, em todo o histórico git de todas as branches.
Nunca imprime o valor de uma chave: só o nome do campo.

Uso manual:   python -I tools/varre_segredos.py [settings.json extra ...]
Como hook:    .git/hooks/pre-push chama este script; saída 1 bloqueia o push.
"""
import base64, json, os, re, subprocess, sys

sys.stdout.reconfigure(encoding="utf-8")

if sys.platform == "win32":
    cfg = os.path.join(os.environ["APPDATA"], "OpenFlow")
else:
    cfg = os.path.expanduser("~/Library/Application Support/OpenFlow")
repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# escopos do OAuth são constantes públicas do código (siwc.rs), não segredo
PUBLICOS = {"scopes"}

segredos = {}
cifrados_sem_abrir = []


def abrir_dpapi(b64):
    """mesmo caminho do app (platform/windows.rs): "enc:" + base64 do blob DPAPI"""
    import ctypes
    from ctypes import wintypes

    class Blob(ctypes.Structure):
        _fields_ = [("cbData", wintypes.DWORD), ("pbData", ctypes.POINTER(ctypes.c_char))]

    dados = base64.b64decode(b64)
    entrada = Blob(len(dados), ctypes.cast(ctypes.create_string_buffer(dados, len(dados)), ctypes.POINTER(ctypes.c_char)))
    saida = Blob()
    if not ctypes.windll.crypt32.CryptUnprotectData(ctypes.byref(entrada), None, None, None, None, 0x1, ctypes.byref(saida)):
        return None
    try:
        return ctypes.string_at(saida.pbData, saida.cbData).decode("utf-8")
    finally:
        ctypes.windll.kernel32.LocalFree(saida.pbData)

def coleta(obj, caminho, forcar=False):
    if isinstance(obj, dict):
        for k, v in obj.items():
            if k in PUBLICOS:
                continue
            # api_key sim, hotkey/cancel_key não: atalho de teclado não é segredo
            sensivel = re.search(r"api_?key|token|secret|senha|password", k, re.I)
            coleta(v, f"{caminho}.{k}", forcar or bool(sensivel))
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            coleta(v, f"{caminho}[{i}]", forcar)
    elif isinstance(obj, str) and forcar and len(obj) >= 12:
        # o app guarda as chaves cifradas: comparar o texto cifrado não prova nada
        if obj.startswith("enc:"):
            aberto = abrir_dpapi(obj[4:]) if sys.platform == "win32" else None
            if aberto is None:
                cifrados_sem_abrir.append(caminho)
                return
            obj = aberto
        segredos[caminho] = obj

# Arquivos extras pela linha de comando: dentro do sandbox do Claude o %APPDATA%
# é redirecionado e não tem as chaves de verdade — aí passa-se a cópia real
# (open-flow-instalar/settings-real.json, gerada pelo coletar-estado.cmd).
arquivos = [os.path.join(cfg, n) for n in ("settings.json", "chatgpt.json")] + sys.argv[1:]
for p in arquivos:
    if os.path.exists(p):
        nome = os.path.basename(p)
        with open(p, encoding="utf-8-sig") as f:
            coleta(json.load(f), nome, forcar=("chatgpt" in nome))

for c in cifrados_sem_abrir:
    print(f"[varre_segredos] aviso: {c} está cifrado e não abriu aqui; só os padrões valem para ele")
if not segredos:
    print(f"[varre_segredos] aviso: nenhuma chave real lida em {cfg}; só os padrões genéricos valem")

# barreira que passa quando quebra é pior que nenhuma: git falhou = bloqueia
r = subprocess.run(
    ["git", "-C", repo, "log", "--all", "-p", "--full-history", "--no-color", "--format=COMMIT %H %s"],
    capture_output=True, text=True, encoding="utf-8", errors="replace",
)
historico = r.stdout
if r.returncode != 0 or "COMMIT " not in historico:
    print(f"[varre_segredos] BLOQUEADO — não consegui ler o histórico git: {r.stderr.strip()[:200]}")
    sys.exit(1)

print(f"[varre_segredos] {len(segredos)} valores reais conferidos em {len(historico):,} caracteres de histórico")

achados = [f"valor real do campo {c}" for c, v in segredos.items() if v in historico]

PADROES = {
    "chave Anthropic (sk-ant-)": r"sk-ant-[A-Za-z0-9_\-]{20,}",
    "chave OpenAI (sk-)": r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}",
    "chave Groq (gsk_)": r"\bgsk_[A-Za-z0-9]{20,}",
    "chave Google (AIza)": r"\bAIza[0-9A-Za-z_\-]{30,}",
    "token JWT (eyJ...)": r"\beyJ[A-Za-z0-9_\-]{20,}\.[A-Za-z0-9_\-]{20,}\.",
    "token GitHub (ghp_)": r"\bgh[pousr]_[A-Za-z0-9]{30,}",
}
for nome, rx in PADROES.items():
    n = len(re.findall(rx, historico))
    if n:
        achados.append(f"{n}x padrão de {nome}")

if achados:
    print("[varre_segredos] BLOQUEADO — encontrado no histórico:")
    for a in achados:
        print("   -", a)
    sys.exit(1)
print("[varre_segredos] limpo: nenhuma chave real nem padrão de chave no histórico")
