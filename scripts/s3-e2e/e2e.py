"""服务端「附件放对象存储」的端到端验证。

跑法见同目录的 `run.ps1`（它会起 moto 和真的服务端二进制，再调这个脚本）。

验的是单元测试证明不了的那部分：**真实的 HTTP + 真实的 SigV4 签名 + 真实的
路径式寻址**。单元测试用的是 `InMemory` 对象存储，它共享同一段逻辑，
但签名怎么算、端点怎么拼、桶名放路径还是放主机名，只有对着一个真的 S3 协议
实现才说得清。

**它证明不了什么**（别当成验过）：真实云厂商的桶（AWS / R2 / B2 的权限模型、
区域、限流都没碰）、以及"两台客户端同时上传同一个键"的竞态。
"""

import argparse
import hashlib
import json
import sqlite3
import sys
import time
import urllib.error
import urllib.request

import boto3

ap = argparse.ArgumentParser()
ap.add_argument("--db", required=True, help="服务端用的那个 .sqlite 路径")
ap.add_argument("--base", default="http://127.0.0.1:8799", help="服务端地址")
ap.add_argument("--endpoint", default="http://127.0.0.1:5099", help="S3 端点")
ap.add_argument("--token", default="e2e-s3-token-0123456789abcdefghijkl")
ap.add_argument("--bucket", default="mn-attachments")
args = ap.parse_args()

ENDPOINT = args.endpoint
BASE = args.base
TOKEN = args.token
BUCKET = args.bucket
DB = args.db

ok = 0
bad = 0


def check(label, cond, detail=""):
    global ok, bad
    if cond:
        ok += 1
        print(f"  PASS  {label}")
    else:
        bad += 1
        print(f"  FAIL  {label}  {detail}")


def s3():
    # 凭据是假的：moto 不校验签名内容，但**会校验请求确实签过**
    # （伪造/缺失 Authorization 头会被它拒掉），所以这条路径不是白跑的。
    return boto3.client(
        "s3",
        endpoint_url=ENDPOINT,
        aws_access_key_id="test",
        aws_secret_access_key="test",
        region_name="us-east-1",
    )


def post(path, body, ctype="application/octet-stream"):
    req = urllib.request.Request(
        BASE + path,
        data=body,
        method="POST",
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": ctype},
    )
    with urllib.request.urlopen(req) as r:
        return r.status, json.loads(r.read())


def get(path):
    req = urllib.request.Request(BASE + path, headers={"Authorization": f"Bearer {TOKEN}"})
    try:
        with urllib.request.urlopen(req) as r:
            return r.status, r.read(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read(), dict(e.headers)


def wait_health(timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(BASE + "/api/health", timeout=1) as r:
                if r.status == 200:
                    return True
        except Exception:
            time.sleep(0.3)
    return False


def objects():
    return s3().list_objects_v2(Bucket=BUCKET).get("Contents", [])


# 一张会被嗅探成 png 的最小字节
PNG = b"\x89PNG\r\n\x1a\n" + bytes(range(32)) * 4
SHA = hashlib.sha256(PNG).hexdigest()
KEY = f"attachments/{SHA}"
ABSENT = "0" * 64

print("== 0. 准备 ==")
if not wait_health():
    sys.exit(f"服务端没起来：{BASE}")
s3().create_bucket(Bucket=BUCKET)

print("== 1. 上传：字节进对象存储，类型由字节嗅探 ==")
status, up = post("/api/blob", PNG, ctype="text/html")
check("上传返回 200", status == 200, status)
check("服务端算出的 sha 与本地一致", up["sha256"] == SHA, up)
check("声明 text/html 也被嗅探成 image/png", up["mime"] == "image/png", up)

objs = objects()
check("桶里正好一个对象", len(objs) == 1, objs)
check("对象键是 attachments/<sha256>", bool(objs) and objs[0]["Key"] == KEY, objs)
check("对象大小一致", bool(objs) and objs[0]["Size"] == len(PNG), objs)

print("== 2. 去重：同样的字节再传一次，桶里还是一个对象 ==")
status, up2 = post("/api/blob", PNG)
check("第二次也是 200 且 sha 相同", status == 200 and up2["sha256"] == SHA, up2)
check("桶里仍然只有一个对象", len(objects()) == 1, objects())

print("== 3. 下载：字节原样回来 ==")
status, got, headers = get(f"/api/blob/{SHA}")
check("下载返回 200", status == 200, status)
check("字节逐字节一致", got == PNG, f"{len(got)} vs {len(PNG)}")
check("Content-Type 是 image/png", headers.get("content-type") == "image/png", headers)
check("带 nosniff", headers.get("x-content-type-options") == "nosniff", headers)

print("== 4. missing：有的说没有，没有的说没有 ==")
status, res = post(
    "/api/blob/missing",
    json.dumps({"shas": [SHA, ABSENT]}).encode(),
    ctype="application/json",
)
check("missing 返回 200", status == 200, status)
check("只报没的那一个", res["missing"] == [ABSENT], res)

print("== 5. 穿底读：切 S3 之前存在 SQLite 里的字节仍然取得到 ==")
legacy = b"\x89PNG\r\n\x1a\n" + b"legacy-bytes" * 8
legacy_sha = hashlib.sha256(legacy).hexdigest()
con = sqlite3.connect(DB)
con.execute(
    "INSERT OR REPLACE INTO attachment(sha256,size,mime,created_at,bytes) VALUES(?,?,?,?,?)",
    (legacy_sha, len(legacy), "image/png", 0, legacy),
)
con.commit()
con.close()
status, got, _ = get(f"/api/blob/{legacy_sha}")
check("老字节取得回来（200）", status == 200, status)
check("老字节逐字节一致", got == legacy, f"{len(got)} vs {len(legacy)}")
status, res = post(
    "/api/blob/missing",
    json.dumps({"shas": [legacy_sha]}).encode(),
    ctype="application/json",
)
check("老字节不算『缺』（否则客户端会反复重传）", res["missing"] == [], res)
check("穿底读不会往桶里写东西", len(objects()) == 1, objects())

print("== 6. 不存在的附件：404，不是 500 ==")
status, _, _ = get(f"/api/blob/{ABSENT}")
check("404", status == 404, status)

print(f"\n结果：{ok} 项通过，{bad} 项失败")
sys.exit(1 if bad else 0)
