// 直接读取本地数据库做体检。
//
// 为什么需要它：这是个本地优先应用，出问题时第一现场永远是磁盘上那个
// SQLite 文件，而不是界面。有了这个脚本就不用去装 sqlite3 CLI。
//
// 用法：node scripts/inspect-db.mjs

import { DatabaseSync } from "node:sqlite";
import path from "node:path";
import fs from "node:fs";

const dir = path.join(process.env.APPDATA ?? "", "dev.messagenote.app");
const file = path.join(dir, "messagenote.sqlite");

if (!fs.existsSync(file)) {
  console.error(`找不到数据库：${file}`);
  console.error("（应用至少启动过一次才会创建它）");
  process.exit(1);
}

// readOnly：应用可能正在运行并持有 WAL 写锁，只读连接不会干扰它
const db = new DatabaseSync(file, { readOnly: true });

const tables = db
  .prepare("SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name")
  .all();

console.log(`数据库：${file}`);
console.log(`大小：${(fs.statSync(file).size / 1024).toFixed(1)} KB\n`);

console.log("--- 对象 ---");
for (const t of tables) console.log(`  ${t.type}: ${t.name}`);

console.log("\n--- 频道 ---");
for (const c of db.prepare("SELECT id, name, kind, sort_order FROM channel ORDER BY sort_order").all()) {
  console.log(`  ${c.kind === "inbox" ? "📥" : "#"} ${c.name}  (${c.id})  order=${c.sort_order}`);
}

console.log("\n--- 计数 ---");
for (const t of ["message", "channel", "tag", "message_tag", "message_fts"]) {
  const n = db.prepare(`SELECT COUNT(*) AS n FROM ${t}`).get().n;
  console.log(`  ${t.padEnd(12)} ${n}`);
}

console.log("\n--- 最近 5 条 ---");
for (const m of db
  .prepare(
    `SELECT substr(body, 1, 50) AS body, channel_id, created_at, hlc_wall, dirty
       FROM message WHERE deleted_at IS NULL
      ORDER BY created_at DESC LIMIT 5`
  )
  .all()) {
  const t = new Date(m.created_at).toLocaleString("zh-CN");
  const flag = m.dirty ? "待上传" : "已同步";
  console.log(`  [${t}] ${flag} hlc=${m.hlc_wall} chan=${m.channel_id}  ${m.body.replace(/\n/g, " ")}`);
}

// 同步体检：待上传的行数。接入服务端后这里归零，才说明推和拉都成功了。
console.log("\n--- 待上传（dirty）计数 ---");
for (const t of ["channel", "message", "tag", "message_tag"]) {
  const n = db.prepare(`SELECT COUNT(*) AS n FROM ${t} WHERE dirty = 1`).get().n;
  const total = db.prepare(`SELECT COUNT(*) AS n FROM ${t}`).get().n;
  console.log(`  ${t.padEnd(12)} ${n} / ${total}`);
}
const cursor = db.prepare("SELECT value FROM meta WHERE key = 'sync_cursor'").get();
console.log(`  sync_cursor  ${cursor ? cursor.value : "(从未同步)"}`);

// 把实际存进 FTS 的文本打出来。
// 这是中文检索最容易出问题的地方（bigram 展开是否正确），
// 出检索问题时第一个要看的就是这里。
console.log("\n--- 检索索引内容（bigram 展开后的实际存储）---");
const ftsRows = db.prepare("SELECT search_text FROM message_fts LIMIT 3").all();
if (ftsRows.length === 0) {
  console.log("  (索引为空)");
}
for (const r of ftsRows) {
  const s = r.search_text;
  console.log(`  ${s.length > 100 ? `${s.slice(0, 100)}…` : s}`);
}

console.log("\n--- meta ---");
for (const m of db.prepare("SELECT key, value FROM meta").all()) {
  console.log(`  ${m.key} = ${m.value}`);
}

console.log(`\nuser_version = ${db.prepare("PRAGMA user_version").get().user_version}`);
db.close();
