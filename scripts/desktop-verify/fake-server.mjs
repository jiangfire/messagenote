// R1 / R7 / R8 的替身服务端（验证用，产物落在 .scratch/，gitignore）
//
// 为什么不直接用 messagenote-server：R1 要"观察连接活多久"、R7 要"让上传
// 失败但拉取成功"，这两件事都需要服务端配合，而真服务端做不到。
// 这里只实现桌面端同步真正会打到的 5 个端点。
//
//   GET  /api/sync/handshake  -> 200
//   POST /api/sync/push       -> 200（空结果）
//   GET  /api/sync/pull       -> 200（带一条可辨认的变更）
//   POST /api/blob            -> 500（R7 专用：模拟慢上行导致的超时）
//   GET  /api/events          -> SSE，每 HEARTBEAT_MS 一次注释心跳
//
// 心跳间隔用环境变量 HEARTBEAT_MS 调小，这样"活过 25 秒"不用真等 25 秒。

import { createServer } from "node:http";

const PORT = Number(process.env.PORT || 8787);
const HEARTBEAT_MS = Number(process.env.HEARTBEAT_MS || 15_000);
const TOKEN = process.env.MESSAGENOTE_TOKEN || "";
const FAIL_BLOB = process.env.FAIL_BLOB === "1";
const MARKER = process.env.MARKER || "R7-PULL-MARKER";

const log = (...a) => console.log(new Date().toISOString().slice(11, 23), ...a);

// **每次启动一个唯一 runId**，并把它编进拉取消息的正文。
// 为什么必须有：验证脚本靠"这个标记从无到有地出现在界面上"来判断
// pull 真的执行了。上一轮的残留消息会留在库里（而且因为 id 固定、
// HLC 幂等，再拉也不会更新它），于是"标记一开始就存在"——
// 观察就没有意义了，脚本会拿着上一轮的残留判一个假通过。
const RUN_ID = `RUN${Date.now().toString(36).toUpperCase()}`;

// SSE 订阅者：改一次就醒一次，和真服务端一样只推"变了"不推内容。
const subs = new Set();
// R1 的判据靠它：记录每条连接活了多久、断了几次。修复前必然出现"约 25 秒"。
const stats = { runId: RUN_ID, opens: 0, heartbeats: 0, disconnects: [], failBlob: FAIL_BLOB, blobTries: 0, pullOk: 0 };
let sseOpenedAt = null;

function sseStream(req, res) {
  res.writeHead(200, {
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache",
    Connection: "keep-alive",
  });
  // 立刻写一个注释行，让客户端确认头已到（不然它还在等响应头阶段）
  res.write(": connected\n\n");
  sseOpenedAt = Date.now();
  stats.opens += 1;
  log(`SSE 连接建立（心跳每 ${HEARTBEAT_MS}ms）`);

  const beat = setInterval(() => {
    // 心跳是 SSE 注释行。**它不算数据**，客户端不会因此重连。
    res.write(`: beat ${Date.now()}\n\n`);
    stats.heartbeats += 1;
  }, HEARTBEAT_MS);

  subs.add(res);
  let closed = false;
  const drop = () => {
    // req 的 close 与 error 可能都触发，去重才能让存活时长只记一次
    if (closed) return;
    closed = true;
    clearInterval(beat);
    subs.delete(res);
    const aliveMs = sseOpenedAt ? Date.now() - sseOpenedAt : -1;
    stats.disconnects.push({ aliveMs });
    log(`SSE 连接断开（活了约 ${Math.round(aliveMs / 1000)} 秒）`);
  };
  req.on("close", drop);
  req.on("error", drop);
}

function json(res, code, obj) {
  const body = JSON.stringify(obj);
  res.writeHead(code, {
    "Content-Type": "application/json",
    "Content-Length": Buffer.byteLength(body),
  });
  res.end(body);
}

const server = createServer((req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);

  // R8 用：慢端点已在 /api/sync/handshake 上实现（见下），这里保留旧路径
  // 只是为了不破坏早先的调用方式。

  // R1 脚本读它来自动判定连接存活时长
  if (url.pathname === "/__stats") return json(res, 200, stats);
  if (url.pathname === "/__reset") {    stats.opens = 0;
    stats.heartbeats = 0;
    stats.disconnects = [];
    stats.blobTries = 0;
    stats.pullOk = 0;
    return json(res, 200, { ok: true });
  }

  // **速率闸门**：一旦某个端点被打出异常频率，说明验证脚本或替身实现
  // 出了反馈回路（实测踩过：拉取时推 changed -> 唤醒同步 -> 再拉取，
  // 60 秒 16000+ 次 pull，顺带耗尽本机临时端口，客户端报 os error 10048，
  // 验证结论全被污染）。这里宁可让它响亮地失败，也不要给出一个假的结论。
  if (url.pathname === "/api/sync/pull") {
    stats.pullOk += 1;
    if (stats.pullOk > 200) {
      log("!! 拉取次数异常，判定为反馈回路，拒绝继续应答");
      return json(res, 503, { error: "fake: pull storm, aborting" });
    }
  }

  const auth = req.headers.authorization || "";
  const ok = !TOKEN || auth === `Bearer ${TOKEN}`;
  if (!ok) {
    log(`${req.method} ${url.pathname} -> 401`);
    return json(res, 401, { error: "unauthorized" });
  }

  if (url.pathname === "/api/events") return sseStream(req, res);

  if (url.pathname === "/api/sync/handshake") {
    // 字段对着 crates/core/src/wire.rs 的 HealthResponse：ok / protocol /
    // serverTimeMs。少一个字段桌面端就会报 "missing field `ok`"，
    // 那个报错与 R8 无关，别让它混进验证输出里。
    const reply = () => json(res, 200, { ok: true, protocol: 1, serverTimeMs: Date.now() });
    // R8 用：SLOW_MS 毫秒内不答。桌面端 test_sync_connection 会一直等着，
    // 这条命令因此真的在主线程上"飞"好几秒，R8 才量得出冻结与否。
    const delay = Number(process.env.SLOW_MS || 0);
    if (delay > 0) {
      log(`handshake -> 故意延迟 ${delay}ms`);
      return setTimeout(() => {
        log("handshake -> 200（延迟结束）");
        reply();
      }, delay);
    }
    log("handshake -> 200");
    return reply();
  }

  if (url.pathname === "/api/sync/push" && req.method === "POST") {
    let n = 0;
    req.on("data", (c) => (n += c.length));
    return req.on("end", () => {
      log(`push（${n} 字节）-> 200 空结果`);
      for (const s of subs) s.write("event: changed\ndata: 1\n\n");
      json(res, 200, { results: [], cursor: 1 });
    });
  }

  if (url.pathname === "/api/sync/pull") {
    // 每次都回一条新的、带可辨认标记的变更，好让脚本能确认"拉取真的跑了"。
    // 字段名对着 crates/core/src/wire.rs 的 Change / PullResponse 写：
    // kind / id / hlc{wall,counter,device} / deleted / data，外加 hasMore。
    //
    // **这里绝不推 changed。** 拉取是被动读取，给订阅者推信号会形成
    // 「拉取 -> 唤醒同步 -> 再拉取」的反馈回路：实测能把替身服务端在
    // 60 秒内打到 16000+ 次 pull，顺带耗尽本机临时端口（客户端报
    // os error 10048），验证结果全被污染。真服务端也只在**写入**时推。
    // **固定 id**：桌面端按 HLC 幂等落库，固定 id 只会反复命中同一行，
    // 不会一轮一轮往界面里灌新消息（那样判读"标记出现了"会变得含糊）。
    const now = Date.now();
    const id = "r7-fixed-probe";
    log(`pull -> 200（变更 ${id}）`);
    return json(res, 200, {
      changes: [
        {
          seq: now,
          kind: "message",
          id,
          hlc: { wall: now, counter: 0, device: "fake-server" },
          deleted: false,
          data: {
            id,
            // **必须是本地真实存在的频道 id。** 桌面端把拉到的变更直接
            // INSERT 进 message 表，而 channel_id 有外键约束 —— 指向一个
            // 不存在的频道时整条插入失败，于是"拉取执行了"却看不到任何
            // 痕迹，验证脚本会误判成"pull 被饿死"。默认频道是 inbox。
            channelId: process.env.CHANNEL_ID || "inbox",
            body: `${MARKER} ${RUN_ID} ${id}`,
            createdAt: Math.floor(now / 1000),
            updatedAt: Math.floor(now / 1000),
            archived: false,
          },
        },
      ],
      cursor: now,
      hasMore: false,
    });
  }
  if (url.pathname === "/api/blob" && req.method === "POST") {
    stats.blobTries += 1;
    if (FAIL_BLOB) {
      log("blob -> 500（故意让上传失败）");
      return json(res, 500, { error: "fake: upload deliberately failing" });
    }
    let n = 0;
    req.on("data", (c) => (n += c.length));
    return req.on("end", () => {
      log(`blob（${n} 字节）-> 200`);
      json(res, 200, { sha256: "0".repeat(64), stored: true });
    });
  }

  log(`${req.method} ${url.pathname} -> 404`);
  json(res, 404, { error: "not found" });
});

server.listen(PORT, "127.0.0.1", () => {
  log(`替身服务端已启动 http://127.0.0.1:${PORT}`);
  log(`心跳 ${HEARTBEAT_MS}ms；上传失败 ${FAIL_BLOB ? "开" : "关"}`);
});
