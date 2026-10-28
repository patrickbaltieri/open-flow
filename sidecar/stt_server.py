"""Sidecar STT do Open Flow: servidor HTTP local com faster-whisper residente na GPU.

POST /stt?lang=pt&beam=5&prompt=... (corpo = WAV bytes) -> {"text": "..."}
GET  /health -> 200 quando o modelo está quente.

`prompt` é o vocabulário do perfil (initial_prompt do Whisper) e `beam` o beam
size; os dois são opcionais e o padrão segue o de antes (sem prompt, beam 1).
"""

import ctypes
import io
import json
import os
import struct
import sys
import threading
import time
import wave
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path


def _watch_parent():
    """Encerra este processo quando o app (pai) morrer — evita sidecars órfãos."""
    ppid = int(os.environ.get("OPENFLOW_PARENT_PID", "0"))
    if not ppid:
        return
    if sys.platform == "win32":
        SYNCHRONIZE = 0x00100000
        h = ctypes.windll.kernel32.OpenProcess(SYNCHRONIZE, False, ppid)
        if not h:
            os._exit(0)
        ctypes.windll.kernel32.WaitForSingleObject(h, 0xFFFFFFFF)
        os._exit(0)
    # POSIX: sinal 0 só checa existência; reparentado ao launchd (ppid 1) = pai morreu
    while True:
        if os.getppid() == 1:
            os._exit(0)
        try:
            os.kill(ppid, 0)
        except OSError:
            os._exit(0)
        time.sleep(2)


threading.Thread(target=_watch_parent, daemon=True).start()

VENV_NVIDIA = Path(sys.prefix) / "Lib" / "site-packages" / "nvidia"
for sub in ("cublas", "cudnn"):
    d = VENV_NVIDIA / sub / "bin"
    if d.is_dir():
        os.add_dll_directory(str(d))
        os.environ["PATH"] = str(d) + os.pathsep + os.environ["PATH"]

import numpy as np
from faster_whisper import WhisperModel

PORT = int(os.environ.get("OPENFLOW_STT_PORT", "17765"))

print("[stt] carregando large-v3-turbo...", flush=True)
t0 = time.perf_counter()
try:
    MODEL = WhisperModel("large-v3-turbo", device="cuda", compute_type="float16")
    DEVICE = "cuda/float16"
except Exception as e:
    print(f"[stt] CUDA indisponível ({e}); usando CPU int8", flush=True)
    MODEL = WhisperModel("large-v3-turbo", device="cpu", compute_type="int8")
    DEVICE = "cpu/int8"


def _silence_wav(seconds: float = 1.0, rate: int = 16000) -> bytes:
    n = int(seconds * rate)
    data = b"\x00\x00" * n
    hdr = struct.pack("<4sI4s4sIHHIIHH4sI", b"RIFF", 36 + len(data), b"WAVE", b"fmt ",
                      16, 1, 1, rate, rate * 2, 2, 16, b"data", len(data))
    return hdr + data


def _amostras(wav: bytes) -> np.ndarray:
    """WAV PCM 16-bit do app -> float32 mono em 16 kHz, sem passar pelo PyAV.

    O decodificador do faster-whisper abre o áudio pela `av`, e a `av` 19 recusa o
    `metadata_errors` que o faster-whisper 1.2.1 passa: o sidecar caía já no
    aquecimento (medido em 28/10/2026 no venv do TOMO). O app sempre manda WAV
    16-bit (`wav_bytes` em lib.rs), então ler aqui dispensa a `av` por inteiro.
    """
    with wave.open(io.BytesIO(wav)) as w:
        if w.getsampwidth() != 2:
            raise ValueError(f"WAV de {8 * w.getsampwidth()} bits; esperado 16")
        canais, taxa = w.getnchannels(), w.getframerate()
        x = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").astype(np.float32) / 32768.0
    if canais > 1:
        x = x.reshape(-1, canais).mean(axis=1)
    if taxa != 16000 and len(x):
        n = int(round(len(x) * 16000 / taxa))
        x = np.interp(np.linspace(0, len(x) - 1, n), np.arange(len(x)), x).astype(np.float32)
    return x


list(MODEL.transcribe(_amostras(_silence_wav(30.0)), language="pt", beam_size=1)[0])
print(f"[stt] pronto em {time.perf_counter() - t0:.1f}s ({DEVICE}) na porta {PORT}", flush=True)


class Handler(BaseHTTPRequestHandler):
    def _reply(self, code: int, body: bytes) -> None:
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self._reply(200, json.dumps({"status": "ok", "device": DEVICE}).encode())

    def do_POST(self):
        from urllib.parse import parse_qs, urlparse

        wav = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        q = parse_qs(urlparse(self.path).query)
        lang = q.get("lang", ["pt"])[0]
        kwargs = {} if lang == "auto" else {"language": lang}
        prompt = q.get("prompt", [""])[0].strip()
        if prompt:
            kwargs["initial_prompt"] = prompt
        try:
            beam = max(1, min(10, int(q.get("beam", ["1"])[0])))
            amostras = _amostras(wav)
        except (ValueError, EOFError, wave.Error) as e:
            self._reply(400, json.dumps({"error": str(e)}).encode())
            return
        t = time.perf_counter()
        # Contra texto inventado em silêncio: ditado do laudo terminou com "E aí E aí
        # E aí..." num trecho sem fala antes de soltar o atalho (28/10/2026), e outra
        # passada do mesmo áudio inventou outra coisa ali. hallucination_silence_threshold
        # pula segmento suspeito cercado de silêncio (exige word_timestamps), e sem
        # condition_on_previous_text a alucinação não puxa a próxima.
        segments, info = MODEL.transcribe(amostras, beam_size=beam, vad_filter=True,
                                          word_timestamps=True, hallucination_silence_threshold=2.0,
                                          condition_on_previous_text=False, **kwargs)
        text = " ".join(s.text.strip() for s in segments)
        dt = time.perf_counter() - t
        print(f"[stt] {info.duration:.1f}s de áudio em {dt:.2f}s: {text[:80]}", flush=True)
        # modelo e dispositivo vão na resposta: o app grava no ditado como prova de
        # que a transcrição foi feita aqui, e não na nuvem
        self._reply(200, json.dumps({"text": text, "stt_seconds": round(dt, 2),
                                     "model": "large-v3-turbo", "device": DEVICE}).encode())

    def log_message(self, *a):
        pass


class Server(HTTPServer):
    allow_reuse_address = False  # porta ocupada = outro sidecar vivo; falha em vez de duplicar


Server(("127.0.0.1", PORT), Handler).serve_forever()
