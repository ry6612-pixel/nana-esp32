#!/usr/bin/env python3
"""娜娜的「好聽聲音」代理 —— 在家裡電腦上跑，讓裝置換掉機械感的預設語音。

板子預設用 Google 翻譯的免費 TTS，音色偏機械。ESP32-S3 本身跑不動神經網路
語音合成，所以做法是:板子把文字丟給這支程式 → 這支程式用微軟 Edge 的神經
語音合成 → 把 MP3 回傳給板子播。板子只多一次區網往返，延遲影響很小。

用法:
    pip install edge-tts
    python tools/tts_proxy.py

啟動後會印出網址，把它貼進控制台「設定 → 好聽音色 → TTS 代理網址」即可。
電腦關機時板子會自動退回 Google 語音，不會啞掉。

可用音色(--voice 或在控制台改):
    zh-TW-HsiaoChenNeural  台灣女聲，甜、偏年輕(預設)
    zh-TW-HsiaoYuNeural    台灣女聲，穩一點
    zh-CN-XiaoxiaoNeural   大陸女聲，情感最豐富
列出全部: python -m edge_tts --list-voices
"""
import argparse
import asyncio
import base64
import json
import socket
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

try:
    import edge_tts
except ImportError:
    raise SystemExit("需要 edge-tts:pip install edge-tts")

DEFAULT_VOICE = "zh-TW-HsiaoChenNeural"
MAX_CHARS = 400  # 板子那邊本來就有截斷，這裡只是防呆


async def synth(text, voice, rate, pitch):
    audio = bytearray()
    comm = edge_tts.Communicate(text, voice, rate=rate, pitch=pitch)
    async for chunk in comm.stream():
        if chunk["type"] == "audio":
            audio.extend(chunk["data"])
    return bytes(audio)


class Handler(BaseHTTPRequestHandler):
    voice = DEFAULT_VOICE
    rate = "+50%"   # 約 1.5 倍。實測 +20%=1.20x、+25%=1.24x、+35%=1.34x、+50%=1.49x
    pitch = "+0Hz"

    def _json(self, code, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            n = int(self.headers.get("Content-Length", 0))
            req = json.loads(self.rfile.read(n).decode("utf-8"))
        except Exception as e:
            return self._json(400, {"ok": False, "error": "bad request: %s" % e})

        text = (req.get("text") or "").strip()[:MAX_CHARS]
        if not text:
            return self._json(400, {"ok": False, "error": "empty text"})
        voice = req.get("voice") or self.voice

        try:
            mp3 = asyncio.run(synth(text, voice, self.rate, self.pitch))
        except Exception as e:
            # 回 200 + ok:false，板子看到就會退回 Google TTS，不會整個沒聲音
            print("  合成失敗:%s" % e)
            return self._json(200, {"ok": False, "error": str(e)})

        print("  %s | %d 字 -> %d bytes" % (voice, len(text), len(mp3)))
        self._json(200, {"ok": True,
                         "audio_b64": base64.b64encode(mp3).decode("ascii")})

    def log_message(self, *_):
        pass  # 自己印就好，不要 BaseHTTPServer 那種雜訊


def lan_ip():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("8.8.8.8", 80))
        return s.getsockname()[0]
    except Exception:
        return "127.0.0.1"
    finally:
        s.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=5050)
    ap.add_argument("--voice", default=DEFAULT_VOICE)
    ap.add_argument("--rate", default="+50%",
                    help="語速，預設 +50%%(約 1.5 倍)。要更快給 +35%%，長輩聽不清楚就降到 +10%%")
    ap.add_argument("--pitch", default="+0Hz", help="音高，例如 +20Hz 更可愛")
    args = ap.parse_args()

    Handler.voice, Handler.rate, Handler.pitch = args.voice, args.rate, args.pitch
    url = "http://%s:%d/tts" % (lan_ip(), args.port)
    print("娜娜語音代理已啟動")
    print("  音色:%s  語速:%s  音高:%s" % (args.voice, args.rate, args.pitch))
    print("  把這個網址貼到控制台 → 設定 → 好聽音色:")
    print("    %s" % url)
    print("  (電腦關掉的話板子會自動退回 Google 語音)")
    ThreadingHTTPServer(("0.0.0.0", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
