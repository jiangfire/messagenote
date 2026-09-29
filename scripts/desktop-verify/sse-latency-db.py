"""量「服务端写一条 → 桌面端本地库拿到」的延迟。

这才是「SSE 唤醒同步线程」那条验证项的**准确**测法：
  · SSE 生效  -> 几秒内本地库就有
  · 只有 45 秒轮询 -> 最坏 45 秒才有
界面上出现与否是**另一件事**（UI 刷新），不能混在一起量。

不合成按键、不抢焦点。
"""

import json
import os
import sqlite3
import time
import urllib.request

BASE = "http://127.0.0.1:8787"
TOKEN = "sse-verify-token-0123456789abcdefghijklmnop"
DB = os.path.join(os.environ["APPDATA"], "com.messagenote.desktop", "messagenote.sqlite")
TIMEOUT = 70.0
POLL = 0.25


def local_has(marker: str) -> bool:
    c = sqlite3.connect(DB, timeout=5)
    try:
        row = c.execute("SELECT 1 FROM message WHERE body = ?", (marker,)).fetchone()
        return row is not None
    finally:
        c.close()


def cursor():
    c = sqlite3.connect(DB, timeout=5)
    try:
        r = c.execute("SELECT value FROM meta WHERE key='sync_cursor'").fetchone()
        return r[0] if r else None
    finally:
        c.close()


marker = f"SSE-DB延迟-{int(time.time()*1000)}"
if local_has(marker):
    print("标记一开始就存在，测量无效")
    raise SystemExit(1)
print(f"标记: {marker}")
print(f"写入前 sync_cursor = {cursor()}")

t0 = time.monotonic()
req = urllib.request.Request(
    f"{BASE}/api/message",
    data=json.dumps({"body": marker}).encode(),
    headers={"Content-Type": "application/json", "Authorization": f"Bearer {TOKEN}"},
    method="POST",
)
with urllib.request.urlopen(req, timeout=10) as resp:
    print(f"服务端写入: HTTP {resp.status}（t0）")

found = None
while time.monotonic() - t0 < TIMEOUT:
    if local_has(marker):
        found = time.monotonic() - t0
        break
    time.sleep(POLL)

if found is None:
    print(f"!! {TIMEOUT:.0f} 秒内本地库一直没拿到 —— SSE 和轮询都没生效")
    raise SystemExit(1)

print(f"\n本地库拿到耗时: {found:.2f} s")
print(f"sync_cursor 现在 = {cursor()}")
if found < 10:
    print("判定: ✓ SSE 唤醒生效（远快于 45 秒轮询）")
    raise SystemExit(0)
print("判定: !! 像是只走了 45 秒轮询 —— SSE 没唤醒同步线程")
raise SystemExit(1)
