//! SQLite 存储层（客户端）。
//!
//! 设计要点：
//!
//! 1. **所有写入都在事务里同时维护 `message` 和 `message_fts`。**
//!    这里没有用触发器，因为索引文本（bigram 展开）是 Rust 侧算出来的，
//!    SQL 触发器算不出来。代价是"忘了同步 FTS"会变成静默的检索失效，
//!    所以两处写入必须成对出现在同一个事务里。
//!
//! 2. **删除一律是软删除（`deleted_at`）。** 同步需要知道"这条被删了"，
//!    而不是"这条不见了" —— 后者和"还没同步过来"无法区分。
//!
//! 3. **每次修改都推进 HLC 并置 `dirty = 1`。** 时钟状态和 `dirty` 标记
//!    与数据写在同一个事务里：否则一次崩溃就可能让重启后的时钟回退，
//!    产出一批比服务端已有版本更旧的时间戳，那些修改会被静默判负。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};

use messagenote_core::hlc::Hlc;
use messagenote_core::models::{
    Channel, Message, MessagePage, SearchHit, SyncConfig, TagCount, TimelineStats,
};
use messagenote_core::search::{self, QueryPlan};

use crate::error::{AppError, AppResult};

/// 收件箱使用固定 ID（而不是随机 UUID）：
/// 每台设备都必须把「默认落点」认成同一个频道。
pub const INBOX_ID: &str = "inbox";

const SCHEMA_VERSION: i64 = 2;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS channel (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  kind       TEXT NOT NULL DEFAULT 'normal',   -- inbox | normal
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  device_id  TEXT NOT NULL DEFAULT '',
  rev        INTEGER NOT NULL DEFAULT 1,
  deleted_at INTEGER
);

CREATE TABLE IF NOT EXISTS message (
  id         TEXT PRIMARY KEY,
  channel_id TEXT NOT NULL REFERENCES channel(id),
  body       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  device_id  TEXT NOT NULL DEFAULT '',
  rev        INTEGER NOT NULL DEFAULT 1,
  deleted_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_message_channel_time
  ON message(channel_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_message_time
  ON message(created_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS tag (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS message_tag (
  message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
  tag_id     TEXT NOT NULL REFERENCES tag(id) ON DELETE CASCADE,
  PRIMARY KEY (message_id, tag_id)
);

CREATE INDEX IF NOT EXISTS idx_message_tag_tag ON message_tag(tag_id);

-- 独立 FTS5 表：search_text 存的是 Rust 侧算好的 bigram 文本。
-- 不是 external-content 表，因为索引内容需要应用层参与计算。
CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
  search_text,
  message_id UNINDEXED,
  tokenize = 'unicode61 remove_diacritics 2'
);
"#;

/// v2：为多设备同步改造 schema。
const SCHEMA_V2: &str = r#"
-- 1. channel / message 增加 HLC 与 dirty。
--    用 ADD COLUMN 而不是重建表：这两张表的约束没变，重建纯属多余风险。
ALTER TABLE channel ADD COLUMN hlc_wall    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE channel ADD COLUMN hlc_counter INTEGER NOT NULL DEFAULT 0;
ALTER TABLE channel ADD COLUMN dirty       INTEGER NOT NULL DEFAULT 1;

ALTER TABLE message ADD COLUMN hlc_wall    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE message ADD COLUMN hlc_counter INTEGER NOT NULL DEFAULT 0;
ALTER TABLE message ADD COLUMN dirty       INTEGER NOT NULL DEFAULT 1;

-- 2. 已有的本地数据必须能在首次同步时上传到服务端，所以标 dirty。
--    时间戳从 updated_at 回填，而不是从 0 开始 —— 否则这些历史记录
--    会以"比服务端任何版本都旧"的姿态出现，在冲突里必输。
UPDATE channel SET hlc_wall = updated_at, dirty = CASE WHEN kind = 'inbox' THEN 0 ELSE 1 END;
UPDATE message SET hlc_wall = updated_at;

-- 3. tag 改用「标签名即主键」。
--    原来的随机 UUID 主键在多设备下必然出问题：两台设备各自第一次打
--    「工作」这个标签会生成两个不同的 id、相同的 name，合并时撞唯一约束。
--    用名字做身份则两端天然一致，且 DB 里可读、无需哈希。
CREATE TABLE tag_v2 (
  name        TEXT PRIMARY KEY,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  device_id   TEXT NOT NULL DEFAULT '',
  hlc_wall    INTEGER NOT NULL DEFAULT 0,
  hlc_counter INTEGER NOT NULL DEFAULT 0,
  deleted_at  INTEGER,
  dirty       INTEGER NOT NULL DEFAULT 1
);

-- message_tag 刻意不加外键：同步批次里一条关联可能先于它的标签到达，
-- 外键会让整个批次回滚。引用完整性由应用层和同步顺序保证。
CREATE TABLE message_tag_v2 (
  message_id  TEXT NOT NULL,
  tag_name    TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  device_id   TEXT NOT NULL DEFAULT '',
  hlc_wall    INTEGER NOT NULL DEFAULT 0,
  hlc_counter INTEGER NOT NULL DEFAULT 0,
  deleted_at  INTEGER,
  dirty       INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (message_id, tag_name)
);

INSERT INTO tag_v2 (name, created_at, updated_at, hlc_wall, dirty)
SELECT name, created_at, created_at, created_at, 1 FROM tag;

-- 必须在删掉旧 tag 之前把关联迁过来（下面 JOIN 依赖它）
INSERT INTO message_tag_v2 (message_id, tag_name, created_at, updated_at, hlc_wall, hlc_counter, dirty)
SELECT mt.message_id, t.name, 0, 0, 0, 0, 1
  FROM message_tag mt JOIN tag t ON t.id = mt.tag_id;

DROP TABLE message_tag;
DROP TABLE tag;
ALTER TABLE tag_v2 RENAME TO tag;
ALTER TABLE message_tag_v2 RENAME TO message_tag;

CREATE INDEX IF NOT EXISTS idx_message_tag_tag ON message_tag(tag_name);
CREATE INDEX IF NOT EXISTS idx_message_tag_message ON message_tag(message_id);

-- 4. rev 是各设备自己自增的，跨设备毫无意义，已被 HLC 取代。
ALTER TABLE channel DROP COLUMN rev;
ALTER TABLE message DROP COLUMN rev;
"#;

/// 数据库句柄。用 `Mutex` 包一层足够：单用户、写入极少、每次操作都是毫秒级。
pub struct Db {
    inner: Mutex<Connection>,
}

impl Db {
    /// 取连接。锁中毒时返回错误而不是 panic —— 一个界面上点歪的操作不该让整个应用崩掉。
    pub fn conn(&self) -> AppResult<std::sync::MutexGuard<'_, Connection>> {
        self.inner
            .lock()
            .map_err(|_| AppError::Msg("数据库连接锁已损坏，请重启应用".into()))
    }
}

pub fn open(path: &Path) -> AppResult<Db> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    migrate(&conn)?;
    seed(&conn)?;
    Ok(Db {
        inner: Mutex::new(conn),
    })
}

fn migrate(conn: &Connection) -> AppResult<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    // 重建 tag / message_tag 期间必须关掉外键检查：要 DROP 掉被引用的父表。
    // 注意 `PRAGMA foreign_keys` 在事务内是空操作，必须在 BEGIN 之前执行。
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;

    if current < 1 {
        conn.execute_batch(SCHEMA_V1)?;
    }
    if current < 2 {
        conn.execute_batch(SCHEMA_V2)?;
    }

    // user_version 不支持参数绑定，只能拼字符串；拼的是编译期常量，无注入风险。
    conn.execute_batch(&format!(
        "PRAGMA user_version = {SCHEMA_VERSION};
         PRAGMA foreign_keys = ON;"
    ))?;
    Ok(())
}

/// 内存库。
///
/// 需要显式指定设备 id —— 同步相关的测试要在一个进程里造出多台"设备"。
/// 单元测试和 `tests/sync_e2e.rs` 都用它。
pub fn open_memory(device_id: &str) -> AppResult<Db> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrate(&conn)?;
    seed(&conn)?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('device_id', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![device_id],
    )?;
    Ok(Db {
        inner: Mutex::new(conn),
    })
}

fn seed(conn: &Connection) -> AppResult<()> {
    if get_meta(conn, "device_id")?.is_none() {
        set_meta(conn, "device_id", &uuid::Uuid::now_v7().to_string())?;
    }
    // 收件箱是**常量实体**：每台设备都独立创建同样的它，且 dirty = 0 永不推送。
    //
    // 时间戳刻意写死 0，而不是 now_ms()。用本机时间的话，两台设备的收件箱
    // 会长得不一样（created_at 不同）—— 功能上无害，但它破坏了"任意两台设备
    // 最终状态逐字节相同"这个性质，而那个性质正是收敛测试赖以断言的东西。
    // 常量就该长成常量。
    conn.execute(
        "INSERT OR IGNORE INTO channel
           (id, name, kind, sort_order, created_at, updated_at, device_id, hlc_wall, hlc_counter, dirty)
         VALUES (?1, '收件箱', 'inbox', 0, 0, 0, '', 0, 0, 0)",
        params![INBOX_ID],
    )?;
    Ok(())
}

fn get_meta(conn: &Connection, key: &str) -> AppResult<Option<String>> {
    let v = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(v)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> AppResult<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn device_id(conn: &Connection) -> AppResult<String> {
    Ok(get_meta(conn, "device_id")?.unwrap_or_default())
}

// ---------------------------------------------------------------- 同步配置

const SYNC_URL_KEY: &str = "sync_url";
const SYNC_TOKEN_KEY: &str = "sync_token";

pub fn get_sync_config(conn: &Connection) -> AppResult<SyncConfig> {
    Ok(SyncConfig {
        url: get_meta(conn, SYNC_URL_KEY)?.unwrap_or_default(),
        token: get_meta(conn, SYNC_TOKEN_KEY)?.unwrap_or_default(),
    })
}

pub fn set_sync_config(conn: &Connection, url: &str, token: &str) -> AppResult<()> {
    // 在这里归一化一次，免得后面每处拼接 URL 都要自己处理末尾斜杠
    let url = url.trim().trim_end_matches('/');
    set_meta(conn, SYNC_URL_KEY, url)?;
    set_meta(conn, SYNC_TOKEN_KEY, token.trim())?;
    Ok(())
}

// ---------------------------------------------------------------- 时钟

fn read_clock(conn: &Connection, device: &str) -> AppResult<Hlc> {
    let wall = get_meta(conn, "hlc_wall")?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let counter = get_meta(conn, "hlc_counter")?
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    Ok(Hlc::new(wall, counter, device))
}

fn write_clock(conn: &Connection, hlc: &Hlc) -> AppResult<()> {
    set_meta(conn, "hlc_wall", &hlc.wall.to_string())?;
    set_meta(conn, "hlc_counter", &hlc.counter.to_string())?;
    Ok(())
}

/// 推进本机时钟，返回本次变更应使用的时间戳。
///
/// 必须在调用方的**同一个事务内**调用：时钟前进和数据落盘要么一起成功、
/// 要么一起失败。
pub fn clock_next(conn: &Connection) -> AppResult<Hlc> {
    let device = device_id(conn)?;
    let mut hlc = read_clock(conn, &device)?;
    hlc.tick(messagenote_core::now_ms());
    write_clock(conn, &hlc)?;
    Ok(hlc)
}

/// 观察到远端时间戳后校正本机时钟。收到比本机更超前的时间戳时，
/// 把本地时钟拉前，避免此后持续落后、每次冲突都输。
#[allow(dead_code)] // 由同步引擎调用（下一步接入）
pub fn clock_observe(conn: &Connection, remote: &Hlc) -> AppResult<()> {
    let device = device_id(conn)?;
    let mut hlc = read_clock(conn, &device)?;
    hlc.observe(remote, messagenote_core::now_ms());
    write_clock(conn, &hlc)
}

/// 取某一行的 (HLC, dirty)，供同步合并判定使用。
#[allow(dead_code)] // 由同步引擎调用（下一步接入）
pub fn row_state(conn: &Connection, table: &str, id: &str) -> AppResult<Option<(Hlc, bool)>> {
    // table 只能来自调用方硬编码的白名单，不接受外部输入
    let sql = format!("SELECT hlc_wall, hlc_counter, device_id, dirty FROM {table} WHERE id = ?1");
    let row = conn
        .query_row(&sql, params![id], |r| {
            Ok((
                Hlc::new(
                    r.get::<_, i64>(0)?,
                    r.get::<_, u32>(1)?,
                    r.get::<_, String>(2)?,
                ),
                r.get::<_, i64>(3)? != 0,
            ))
        })
        .optional()?;
    Ok(row)
}

// ---------------------------------------------------------------- 读取辅助

fn row_to_message(row: &Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        channel_id: row.get(1)?,
        body: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        tags: Vec::new(),
    })
}

/// 批量补齐标签，避免每条消息一次查询（N+1）。
fn load_tags(conn: &Connection, ids: &[String]) -> AppResult<HashMap<String, Vec<String>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT mt.message_id, mt.tag_name
           FROM message_tag mt
           JOIN tag t ON t.name = mt.tag_name
          WHERE mt.message_id IN ({placeholders})
            AND mt.deleted_at IS NULL
            AND t.deleted_at IS NULL
          ORDER BY mt.tag_name"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(ids.iter()), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for r in rows {
        let (mid, name) = r?;
        map.entry(mid).or_default().push(name);
    }
    Ok(map)
}

fn attach_tags(conn: &Connection, items: &mut [Message]) -> AppResult<()> {
    let ids: Vec<String> = items.iter().map(|m| m.id.clone()).collect();
    let map = load_tags(conn, &ids)?;
    for m in items.iter_mut() {
        m.tags = map.get(&m.id).cloned().unwrap_or_default();
    }
    Ok(())
}

// ---------------------------------------------------------------- 频道

pub fn list_channels(conn: &Connection) -> AppResult<Vec<Channel>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.name, c.kind, c.sort_order, c.created_at, c.updated_at,
                (SELECT COUNT(*) FROM message m
                  WHERE m.channel_id = c.id AND m.deleted_at IS NULL) AS cnt
           FROM channel c
          WHERE c.deleted_at IS NULL
          ORDER BY (c.kind = 'inbox') DESC, c.sort_order ASC, c.created_at ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Channel {
            id: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            sort_order: r.get(3)?,
            created_at: r.get(4)?,
            updated_at: r.get(5)?,
            message_count: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn create_channel(conn: &Connection, name: &str) -> AppResult<Channel> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::Msg("频道名不能为空".into()));
    }
    let exists: Option<String> = conn
        .query_row(
            "SELECT id FROM channel WHERE name = ?1 AND deleted_at IS NULL",
            params![name],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Err(AppError::Msg(format!("频道「{name}」已存在")));
    }

    let now = messagenote_core::now_ms();
    let next_order: i64 = conn.query_row(
        "SELECT COALESCE(MAX(sort_order), 0) + 1 FROM channel",
        [],
        |r| r.get(0),
    )?;
    let id = uuid::Uuid::now_v7().to_string();

    let tx = conn.unchecked_transaction()?;
    let hlc = clock_next(&tx)?;
    tx.execute(
        "INSERT INTO channel
           (id, name, kind, sort_order, created_at, updated_at, device_id, hlc_wall, hlc_counter, dirty)
         VALUES (?1, ?2, 'normal', ?3, ?4, ?4, ?5, ?6, ?7, 1)",
        params![id, name, next_order, now, hlc.device, hlc.wall, hlc.counter],
    )?;
    tx.commit()?;

    Ok(Channel {
        id,
        name: name.to_string(),
        kind: "normal".into(),
        sort_order: next_order,
        created_at: now,
        updated_at: now,
        message_count: 0,
    })
}

pub fn rename_channel(conn: &Connection, id: &str, name: &str) -> AppResult<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::Msg("频道名不能为空".into()));
    }
    let tx = conn.unchecked_transaction()?;
    let hlc = clock_next(&tx)?;
    let n = tx.execute(
        "UPDATE channel
            SET name = ?2, updated_at = ?3, device_id = ?4, hlc_wall = ?5, hlc_counter = ?6, dirty = 1
          WHERE id = ?1 AND deleted_at IS NULL",
        params![id, name, messagenote_core::now_ms(), hlc.device, hlc.wall, hlc.counter],
    )?;
    tx.commit()?;
    if n == 0 {
        return Err(AppError::Msg("频道不存在".into()));
    }
    Ok(())
}

/// 删除频道：连同其消息一起软删除，并把它们从检索索引里摘掉。
pub fn delete_channel(conn: &Connection, id: &str) -> AppResult<()> {
    if id == INBOX_ID {
        return Err(AppError::Msg("收件箱是默认捕获目标，不能删除".into()));
    }
    let now = messagenote_core::now_ms();

    let tx = conn.unchecked_transaction()?;
    let hlc = clock_next(&tx)?;

    tx.execute(
        "DELETE FROM message_fts WHERE message_id IN (SELECT id FROM message WHERE channel_id = ?1)",
        params![id],
    )?;
    tx.execute(
        "UPDATE message
            SET deleted_at = ?2, updated_at = ?2, device_id = ?3, hlc_wall = ?4, hlc_counter = ?5, dirty = 1
          WHERE channel_id = ?1 AND deleted_at IS NULL",
        params![id, now, hlc.device, hlc.wall, hlc.counter],
    )?;
    let n = tx.execute(
        "UPDATE channel
            SET deleted_at = ?2, updated_at = ?2, device_id = ?3, hlc_wall = ?4, hlc_counter = ?5, dirty = 1
          WHERE id = ?1 AND deleted_at IS NULL AND kind != 'inbox'",
        params![id, now, hlc.device, hlc.wall, hlc.counter],
    )?;
    tx.commit()?;
    if n == 0 {
        return Err(AppError::Msg("频道不存在".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------- 消息

/// 时间线的筛选范围。
///
/// 分工是明确的：**频道管归属（互斥），标签管横切（可多个）**。
/// 一条消息属于且仅属于一个频道；标签是"偶尔标的额外记号"。
///
/// 于是"收件箱"不是一个和频道平级的分类，而是**默认落点还没被替换掉**这个状态。
/// [`Scope::Unfiled`] 就是它：让你看清"还有哪些没归档"。
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// 全部消息 —— 时间线主视图
    All,
    /// 还没归档到任何频道（即仍在收件箱里的），等你去分主题
    Unfiled,
    /// 某个频道的全部消息
    Channel(&'a str),
    /// 某个标签下的消息。标签是横切的，与频道正交 ——
    /// 一条已归档的消息照样可以被这个范围查到。
    Tag(&'a str),
}

/// 时间线游标。
///
/// 用 `(created_at, id)` 复合键，而不是单个时间戳。**这不是洁癖**：
/// 同一毫秒内完全可能写入多条（批量导入、脚本、飞快地连打几句），
/// 只比时间戳的游标会在 `created_at < t` 这一步把这些"兄弟行"**整批跳过**，
/// 于是往前翻会凭空少掉一段记录 —— 而且不报任何错，只是有东西不见了。
///
/// `id` 是 UUIDv7，字典序即时间序，所以它天然就是第二排序键。
#[derive(Debug, Clone)]
pub struct Cursor {
    pub created_at: i64,
    pub id: String,
}

impl Cursor {
    /// 取某条消息"之前"的游标（用于继续往前翻）。
    pub fn before(m: &Message) -> Self {
        Self {
            created_at: m.created_at,
            id: m.id.clone(),
        }
    }
}

pub fn list_messages(
    conn: &Connection,
    scope: Scope<'_>,
    limit: i64,
    before: Option<&Cursor>,
) -> AppResult<MessagePage> {
    let limit = limit.clamp(1, 500);

    let mut sql = String::from(
        "SELECT m.id, m.channel_id, m.body, m.created_at, m.updated_at
           FROM message m
          WHERE m.deleted_at IS NULL",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    match scope {
        Scope::All => {}
        Scope::Unfiled => {
            sql.push_str(" AND m.channel_id = ?");
            args.push(Box::new(INBOX_ID.to_string()));
        }
        Scope::Channel(id) => {
            sql.push_str(" AND m.channel_id = ?");
            args.push(Box::new(id.to_string()));
        }
        Scope::Tag(name) => {
            // 用 EXISTS 而不是 LEFT JOIN：前者在命中/未命中时就能短路，
            // 而且不会因为一条消息有多个标签就把它重复返回。
            sql.push_str(
                " AND EXISTS (
                     SELECT 1 FROM message_tag mt
                      WHERE mt.message_id = m.id
                        AND mt.deleted_at IS NULL
                        AND mt.tag_name = ?)",
            );
            args.push(Box::new(name.to_string()));
        }
    }

    if let Some(c) = before {
        // 按 (created_at, id) 的字典序取"严格更早"的行。
        // 这样同一毫秒内的兄弟行也能被逐条翻到。
        sql.push_str(" AND (m.created_at < ? OR (m.created_at = ? AND m.id < ?))");
        args.push(Box::new(c.created_at));
        args.push(Box::new(c.created_at));
        args.push(Box::new(c.id.clone()));
    }
    sql.push_str(" ORDER BY m.created_at DESC, m.id DESC LIMIT ?");
    // 多取一条用来判断"还有没有更早的"，避免再发一次 count 查询
    args.push(Box::new(limit + 1));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args.iter().map(|b| b.as_ref())), row_to_message)?;

    let mut items = Vec::new();
    for r in rows {
        items.push(r?);
    }
    let has_more = items.len() as i64 > limit;
    if has_more {
        items.truncate(limit as usize);
    }

    attach_tags(conn, &mut items)?;
    Ok(MessagePage { items, has_more })
}

pub fn append_message(
    conn: &Connection,
    body: &str,
    channel_id: Option<&str>,
) -> AppResult<Message> {
    let body = body.trim_end();
    if body.trim().is_empty() {
        return Err(AppError::Msg("内容不能为空".into()));
    }
    let channel_id = channel_id.unwrap_or(INBOX_ID);

    let exists: Option<String> = conn
        .query_row(
            "SELECT id FROM channel WHERE id = ?1 AND deleted_at IS NULL",
            params![channel_id],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Err(AppError::Msg("目标频道不存在".into()));
    }

    let id = uuid::Uuid::now_v7().to_string();
    let now = messagenote_core::now_ms();

    let tx = conn.unchecked_transaction()?;
    let hlc = clock_next(&tx)?;
    tx.execute(
        "INSERT INTO message
           (id, channel_id, body, created_at, updated_at, device_id, hlc_wall, hlc_counter, dirty)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, 1)",
        params![id, channel_id, body, now, hlc.device, hlc.wall, hlc.counter],
    )?;
    tx.execute(
        "INSERT INTO message_fts (search_text, message_id) VALUES (?1, ?2)",
        params![search::to_index_text(body), id],
    )?;
    tx.commit()?;

    Ok(Message {
        id,
        channel_id: channel_id.to_string(),
        body: body.to_string(),
        created_at: now,
        updated_at: now,
        tags: Vec::new(),
    })
}

pub fn update_message(conn: &Connection, id: &str, body: &str) -> AppResult<Message> {
    let body = body.trim_end();
    if body.trim().is_empty() {
        return Err(AppError::Msg("内容不能为空".into()));
    }
    let now = messagenote_core::now_ms();

    let tx = conn.unchecked_transaction()?;
    let hlc = clock_next(&tx)?;
    let n = tx.execute(
        "UPDATE message
            SET body = ?2, updated_at = ?3, device_id = ?4, hlc_wall = ?5, hlc_counter = ?6, dirty = 1
          WHERE id = ?1 AND deleted_at IS NULL",
        params![id, body, now, hlc.device, hlc.wall, hlc.counter],
    )?;
    if n == 0 {
        return Err(AppError::Msg("消息不存在".into()));
    }
    // 索引必须同步重建，否则改了正文却搜不到新内容
    tx.execute("DELETE FROM message_fts WHERE message_id = ?1", params![id])?;
    tx.execute(
        "INSERT INTO message_fts (search_text, message_id) VALUES (?1, ?2)",
        params![search::to_index_text(body), id],
    )?;
    tx.commit()?;

    let mut msg = conn.query_row(
        "SELECT id, channel_id, body, created_at, updated_at FROM message WHERE id = ?1",
        params![id],
        row_to_message,
    )?;
    attach_tags(conn, std::slice::from_mut(&mut msg))?;
    Ok(msg)
}

pub fn delete_message(conn: &Connection, id: &str) -> AppResult<()> {
    let now = messagenote_core::now_ms();
    let tx = conn.unchecked_transaction()?;
    let hlc = clock_next(&tx)?;
    let n = tx.execute(
        "UPDATE message
            SET deleted_at = ?2, updated_at = ?2, device_id = ?3, hlc_wall = ?4, hlc_counter = ?5, dirty = 1
          WHERE id = ?1 AND deleted_at IS NULL",
        params![id, now, hlc.device, hlc.wall, hlc.counter],
    )?;
    tx.execute("DELETE FROM message_fts WHERE message_id = ?1", params![id])?;
    tx.commit()?;
    if n == 0 {
        return Err(AppError::Msg("消息不存在".into()));
    }
    Ok(())
}

pub fn move_message(conn: &Connection, id: &str, channel_id: &str) -> AppResult<()> {
    let exists: Option<String> = conn
        .query_row(
            "SELECT id FROM channel WHERE id = ?1 AND deleted_at IS NULL",
            params![channel_id],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Err(AppError::Msg("目标频道不存在".into()));
    }

    let tx = conn.unchecked_transaction()?;
    let hlc = clock_next(&tx)?;
    let n = tx.execute(
        "UPDATE message
            SET channel_id = ?2, updated_at = ?3, device_id = ?4, hlc_wall = ?5, hlc_counter = ?6, dirty = 1
          WHERE id = ?1 AND deleted_at IS NULL",
        params![
            id,
            channel_id,
            messagenote_core::now_ms(),
            hlc.device,
            hlc.wall,
            hlc.counter
        ],
    )?;
    tx.commit()?;
    if n == 0 {
        return Err(AppError::Msg("消息不存在".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------- 标签

pub fn list_tags(conn: &Connection) -> AppResult<Vec<TagCount>> {
    let mut stmt = conn.prepare(
        "SELECT t.name, COUNT(m.id)
           FROM tag t
           LEFT JOIN message_tag mt ON mt.tag_name = t.name AND mt.deleted_at IS NULL
           LEFT JOIN message m ON m.id = mt.message_id AND m.deleted_at IS NULL
          WHERE t.deleted_at IS NULL
          GROUP BY t.name
          ORDER BY COUNT(m.id) DESC, t.name ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(TagCount {
            name: r.get(0)?,
            count: r.get(1)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 用「标签名列表」整体替换一条消息的标签（前端交互就是几个 chip 的增删）。
///
/// 这里**不再回收孤儿标签**。P0 版本结尾有一句全局的
/// `DELETE FROM tag WHERE id NOT IN (SELECT tag_id FROM message_tag)`，
/// 单机没问题，但多设备下是一次危险操作：A 端因为本地没人用而删掉标签 X，
/// B 端离线期间刚好在给某条笔记打 X，同步后这次删除会覆盖到 B，
/// 而 B 那边的关联还指着它。
/// 标签是极小的数据，宁可让列表里出现使用数为 0 的条目。
pub fn set_message_tags(conn: &Connection, message_id: &str, names: &[String]) -> AppResult<()> {
    // 去重 + 去空 + 去首尾空格，同时保持用户给出的顺序
    let mut cleaned: Vec<String> = Vec::new();
    for n in names {
        let n = n.trim().to_string();
        if !n.is_empty() && !cleaned.contains(&n) {
            cleaned.push(n);
        }
    }

    let now = messagenote_core::now_ms();
    let tx = conn.unchecked_transaction()?;

    let exists: Option<String> = tx
        .query_row(
            "SELECT id FROM message WHERE id = ?1 AND deleted_at IS NULL",
            params![message_id],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Err(AppError::Msg("消息不存在".into()));
    }

    let hlc = clock_next(&tx)?;

    // 现在是"整体替换"语义：把当前关联全部标记删除，再重建要保留的。
    // 用软删除而不是 DELETE，这样同步时"取消打标签"也能传播出去。
    tx.execute(
        "UPDATE message_tag
            SET deleted_at = ?2, updated_at = ?2, device_id = ?3, hlc_wall = ?4, hlc_counter = ?5, dirty = 1
          WHERE message_id = ?1 AND deleted_at IS NULL",
        params![message_id, now, hlc.device, hlc.wall, hlc.counter],
    )?;

    for name in &cleaned {
        tx.execute(
            "INSERT INTO tag (name, created_at, updated_at, device_id, hlc_wall, hlc_counter, dirty)
             VALUES (?1, ?2, ?2, ?3, ?4, ?5, 1)
             ON CONFLICT(name) DO UPDATE SET
               updated_at = excluded.updated_at,
               device_id = excluded.device_id,
               hlc_wall = excluded.hlc_wall,
               hlc_counter = excluded.hlc_counter,
               deleted_at = NULL,
               dirty = 1",
            params![name, now, hlc.device, hlc.wall, hlc.counter],
        )?;
        tx.execute(
            "INSERT INTO message_tag
               (message_id, tag_name, created_at, updated_at, device_id, hlc_wall, hlc_counter, dirty)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, 1)
             ON CONFLICT(message_id, tag_name) DO UPDATE SET
               updated_at = excluded.updated_at,
               device_id = excluded.device_id,
               hlc_wall = excluded.hlc_wall,
               hlc_counter = excluded.hlc_counter,
               deleted_at = NULL,
               dirty = 1",
            params![message_id, name, now, hlc.device, hlc.wall, hlc.counter],
        )?;
    }

    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------- 统计

/// 侧边栏要的两个计数。
///
/// 单独一条查询，不去翻整个列表 —— 侧边栏只需要两个数字，
/// 没必要为此把消息拉一遍。
pub fn timeline_stats(conn: &Connection) -> AppResult<TimelineStats> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM message WHERE deleted_at IS NULL",
        [],
        |r| r.get(0),
    )?;

    // 这里的判定条件必须和 `Scope::Unfiled` **逐字一致**。
    // 两处写歪一点，筛选条显示"未归档 3"而点进去只有 2 条 ——
    // 这种小出入最伤信任。
    let unfiled: i64 = conn.query_row(
        "SELECT COUNT(*) FROM message WHERE deleted_at IS NULL AND channel_id = ?1",
        params![INBOX_ID],
        |r| r.get(0),
    )?;

    Ok(TimelineStats { total, unfiled })
}

// ---------------------------------------------------------------- 检索
pub fn search(conn: &Connection, query: &str, limit: i64) -> AppResult<Vec<SearchHit>> {
    let limit = limit.clamp(1, 200);
    let Some(plan) = search::plan_query(query) else {
        return Ok(Vec::new());
    };

    let mut hits: Vec<SearchHit> = Vec::new();

    match plan {
        QueryPlan::Fts { match_expr, words } => {
            // 多取一些候选，因为最后还有一步精确过滤会淘汰掉假阳性
            let fetch = limit.saturating_mul(5).max(50);
            let mut stmt = conn.prepare(
                "SELECT m.id, m.channel_id, m.body, m.created_at, m.updated_at, c.name
                   FROM message_fts
                   JOIN message m ON m.id = message_fts.message_id
                   JOIN channel c ON c.id = m.channel_id
                  WHERE message_fts MATCH ?1
                    AND m.deleted_at IS NULL
                  ORDER BY bm25(message_fts)
                  LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![match_expr, fetch], |r| {
                let msg = row_to_message(r)?;
                let channel_name: String = r.get(5)?;
                Ok((msg, channel_name))
            })?;
            for r in rows {
                let (msg, channel_name) = r?;
                if search::matches_all(&msg.body, &words) {
                    hits.push(SearchHit {
                        message: msg,
                        channel_name,
                    });
                    if hits.len() as i64 >= limit {
                        break;
                    }
                }
            }
        }
        QueryPlan::Like { words } => {
            // 单个汉字之类的查询无法用 bigram 表达，退回子串扫描。
            // 用全部词做 AND 粗筛（仍然是精确结果的超集），再逐条确认。
            let mut sql = String::from(
                "SELECT m.id, m.channel_id, m.body, m.created_at, m.updated_at, c.name
                   FROM message m
                   JOIN channel c ON c.id = m.channel_id
                  WHERE m.deleted_at IS NULL",
            );
            let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            for w in &words {
                sql.push_str(" AND m.body LIKE ? ESCAPE '\\'");
                args.push(Box::new(search::like_pattern(w)));
            }
            sql.push_str(" ORDER BY m.created_at DESC, m.id DESC LIMIT ?");
            args.push(Box::new(limit));

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(args.iter().map(|b| b.as_ref())), |r| {
                let msg = row_to_message(r)?;
                let channel_name: String = r.get(5)?;
                Ok((msg, channel_name))
            })?;
            for r in rows {
                let (msg, channel_name) = r?;
                hits.push(SearchHit {
                    message: msg,
                    channel_name,
                });
            }
        }
    }

    let ids: Vec<String> = hits.iter().map(|h| h.message.id.clone()).collect();
    let map = load_tags(conn, &ids)?;
    for h in hits.iter_mut() {
        h.message.tags = map.get(&h.message.id).cloned().unwrap_or_default();
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存库。这些测试同时承担两个职责：
    /// 1. 验证 SQLite 的 FTS5 扩展确实被编译进来了（`bundled` 特性的关键前提）
    /// 2. 验证中文检索的端到端行为，而不只是分词函数的单元行为
    fn mem() -> Db {
        open_memory("test-device").expect("建内存库")
    }

    fn hlc_of(conn: &Connection, table: &str, id: &str) -> Hlc {
        let sql = format!("SELECT hlc_wall, hlc_counter, device_id FROM {table} WHERE id = ?1");
        conn.query_row(&sql, params![id], |r| {
            Ok(Hlc::new(
                r.get::<_, i64>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .expect("读取 HLC")
    }

    #[test]
    fn fresh_database_lands_on_the_latest_schema_version() {
        let db = mem();
        let conn = db.conn().unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION, "新建库应当直接是最新版本");
    }

    #[test]
    fn fts5_is_available_and_chinese_two_char_query_works() {
        let db = mem();
        let conn = db.conn().unwrap();
        append_message(&conn, "今天开会，讨论笔记软件的架构", None).unwrap();
        append_message(&conn, "买菜：西红柿、鸡蛋", None).unwrap();

        // 双字词 —— 这正是 FTS5 的 trigram 分词器会失败、而 bigram 方案必须成功的场景
        let hits = search(&conn, "笔记", 10).unwrap();
        assert_eq!(hits.len(), 1, "「笔记」应命中 1 条，实际 {}", hits.len());

        let hits = search(&conn, "开会", 10).unwrap();
        assert_eq!(hits.len(), 1, "「开会」应命中 1 条");

        let hits = search(&conn, "笔记软件", 10).unwrap();
        assert_eq!(hits.len(), 1, "「笔记软件」应命中 1 条");

        let hits = search(&conn, "架构设计模式", 10).unwrap();
        assert!(hits.is_empty(), "无关词不应命中");

        // 单字退化为 LIKE 回退（bigram 无法表达单字）
        let hits = search(&conn, "蛋", 10).unwrap();
        assert_eq!(hits.len(), 1, "单字「蛋」应通过 LIKE 回退命中 1 条");
    }

    /// 明确固化一个**有意的**语义边界：检索是「连续子串」匹配，不是分词匹配。
    ///
    /// 原文写「开了个会」时搜「开会」是搜不到的 —— 因为中间隔着「了个」。
    /// 这是 bigram 方案的必然结果，也是它和"真分词"检索的核心区别。
    /// 把它写成测试，是为了避免以后有人误以为这是 bug 而"修"坏检索逻辑。
    #[test]
    fn search_is_contiguous_substring_not_word_segmentation() {
        let db = mem();
        let conn = db.conn().unwrap();
        append_message(&conn, "今天开了个会", None).unwrap();

        assert!(
            search(&conn, "开会", 10).unwrap().is_empty(),
            "「开了个会」中间隔着字，连续子串匹配不应命中「开会」"
        );
        assert_eq!(search(&conn, "开了个会", 10).unwrap().len(), 1);
        assert_eq!(search(&conn, "个会", 10).unwrap().len(), 1);
    }

    #[test]
    fn post_filter_rejects_disjoint_bigram_hits() {
        let db = mem();
        let conn = db.conn().unwrap();
        // 三个 bigram（笔记 / 记软 / 软件）在索引里都存在，但被标点切开，
        // 原文并不含连续的「笔记软件」。这是 bigram 方案的固有假阳性，
        // 必须由末尾的子串精确过滤挡掉。
        append_message(&conn, "笔记，记软件", None).unwrap();

        let hits = search(&conn, "笔记软件", 10).unwrap();
        assert!(hits.is_empty(), "FTS 粗筛会命中，但精确过滤必须剔除");

        append_message(&conn, "这是一份笔记软件的设计稿", None).unwrap();
        let hits = search(&conn, "笔记软件", 10).unwrap();
        assert_eq!(hits.len(), 1, "连续出现时应命中 1 条");
    }

    #[test]
    fn soft_delete_removes_message_from_search_and_list() {
        let db = mem();
        let conn = db.conn().unwrap();
        let msg = append_message(&conn, "临时记录一下待办事项", None).unwrap();

        assert_eq!(search(&conn, "待办", 10).unwrap().len(), 1);
        delete_message(&conn, &msg.id).unwrap();

        assert!(search(&conn, "待办", 10).unwrap().is_empty(), "删除后不应还能搜到");
        assert!(
            list_messages(&conn, Scope::All, 50, None).unwrap().items.is_empty(),
            "删除后不应出现在列表里"
        );
    }

    #[test]
    fn editing_body_rebuilds_the_search_index() {
        let db = mem();
        let conn = db.conn().unwrap();
        let msg = append_message(&conn, "旧的内容", None).unwrap();

        update_message(&conn, &msg.id, "全新的内容关于量子计算").unwrap();

        assert!(search(&conn, "旧的", 10).unwrap().is_empty(), "旧内容不应还能搜到");
        assert_eq!(search(&conn, "量子", 10).unwrap().len(), 1, "新内容必须立刻可检索");
    }

    #[test]
    fn inbox_is_the_default_capture_target() {
        let db = mem();
        let conn = db.conn().unwrap();
        let msg = append_message(&conn, "没指定频道", None).unwrap();
        assert_eq!(msg.channel_id, INBOX_ID, "省略频道时必须落到收件箱");
    }

    #[test]
    fn tag_roundtrip_uses_name_as_identity() {
        let db = mem();
        let conn = db.conn().unwrap();
        let msg = append_message(&conn, "一条带标签的记录", None).unwrap();

        set_message_tags(&conn, &msg.id, &["工作".into(), "工作".into(), " 重要 ".into()]).unwrap();

        let page = list_messages(&conn, Scope::All, 50, None).unwrap();
        assert_eq!(
            page.items[0].tags,
            vec!["工作".to_string(), "重要".to_string()],
            "应去重并去空白，且按名字排序"
        );

        assert_eq!(
            list_messages(&conn, Scope::Tag("工作"), 50, None).unwrap().items.len(),
            1
        );
    }

    /// 多设备下**不能**再回收孤儿标签：A 端的清理会通过同步删掉 B 端还在用的标签。
    #[test]
    fn unused_tags_are_kept_not_garbage_collected() {
        let db = mem();
        let conn = db.conn().unwrap();
        let msg = append_message(&conn, "记录", None).unwrap();

        set_message_tags(&conn, &msg.id, &["会被取消的标签".into()]).unwrap();
        set_message_tags(&conn, &msg.id, &[]).unwrap();

        assert!(
            list_tags(&conn).unwrap().iter().any(|t| t.name == "会被取消的标签"),
            "取消打标签后标签本身必须保留，否则会误删其它设备在用的同名标签"
        );
    }

    // ------------------------------------------------ 归档状态（频道）与标签的分工

    /// 「未归档」= 消息的 `channel_id` 还是收件箱。**移到频道它就离开。**
    ///
    /// 注意这和"打标签"完全无关 —— 那次改版把收件箱定义成"未打标签"，
    /// 结果只要用户不怎么打标签，收件箱和时间线的数字就几乎一样，
    /// 两个视图看起来就是重复的。归属归频道管，才是它该有的语义。
    #[test]
    fn unfiled_scope_means_still_in_the_inbox_channel() {
        let db = mem();
        let conn = db.conn().unwrap();

        let pending = append_message(&conn, "还没归档的", None).unwrap();
        let filed = append_message(&conn, "已经归档的", None).unwrap();
        let ch = create_channel(&conn, "项目").unwrap();
        move_message(&conn, &filed.id, &ch.id).unwrap();

        let unfiled = list_messages(&conn, Scope::Unfiled, 50, None).unwrap();
        assert_eq!(unfiled.items.len(), 1, "只有还留在收件箱的那条算未归档");
        assert_eq!(unfiled.items[0].id, pending.id);

        // 把剩下那条也归档，未归档就空了
        move_message(&conn, &pending.id, &ch.id).unwrap();
        assert!(
            list_messages(&conn, Scope::Unfiled, 50, None)
                .unwrap()
                .items
                .is_empty(),
            "全部归档之后未归档应当是空的"
        );

        // 它们并没有消失，只是换了归属
        assert_eq!(
            list_messages(&conn, Scope::All, 50, None).unwrap().items.len(),
            2,
            "时间线里两条都还在"
        );
        assert_eq!(
            list_messages(&conn, Scope::Channel(&ch.id), 50, None)
                .unwrap()
                .items
                .len(),
            2,
            "频道视图里两条都在"
        );
    }

    /// 标签是**横切**的：打标签不改变消息的归属，也不影响它算不算未归档。
    ///
    /// 这是"频道为主、标签为辅"在数据层的体现。上一版把两者混在一起
    /// （未归档 = 未打标签），于是两个概念互相绑架，界面上也就分不清了。
    #[test]
    fn tagging_does_not_change_filing_state() {
        let db = mem();
        let conn = db.conn().unwrap();
        let m = append_message(&conn, "打了个标签", None).unwrap();
        set_message_tags(&conn, &m.id, &["重要".into()]).unwrap();

        assert_eq!(
            list_messages(&conn, Scope::Unfiled, 50, None).unwrap().items.len(),
            1,
            "打标签不该让消息离开收件箱 —— 那是频道的职责"
        );
        assert_eq!(
            list_messages(&conn, Scope::Tag("重要"), 50, None).unwrap().items.len(),
            1
        );

        // 归档之后，标签仍然能查到它（横切）
        let ch = create_channel(&conn, "项目").unwrap();
        move_message(&conn, &m.id, &ch.id).unwrap();
        assert!(list_messages(&conn, Scope::Unfiled, 50, None).unwrap().items.is_empty());
        assert_eq!(
            list_messages(&conn, Scope::Tag("重要"), 50, None).unwrap().items.len(),
            1,
            "归档不该让标签失效 —— 频道和标签是正交的两个维度"
        );
    }

    /// 一条消息有多个标签时，按标签筛选不能把它重复返回。
    #[test]
    fn tag_scope_does_not_duplicate_multi_tagged_messages() {
        let db = mem();
        let conn = db.conn().unwrap();
        let m = append_message(&conn, "同时属于两个标签", None).unwrap();
        set_message_tags(&conn, &m.id, &["工作".into(), "重要".into()]).unwrap();

        assert_eq!(
            list_messages(&conn, Scope::Tag("工作"), 50, None).unwrap().items.len(),
            1
        );
        assert_eq!(
            list_messages(&conn, Scope::Tag("重要"), 50, None).unwrap().items.len(),
            1
        );
    }

    /// 统计口径必须和筛选口径一致，否则筛选条显示"未归档 3"而点进去只有 2 条。
    #[test]
    fn stats_agree_with_the_scopes_they_summarise() {
        let db = mem();
        let conn = db.conn().unwrap();
        append_message(&conn, "一", None).unwrap();
        let b = append_message(&conn, "二", None).unwrap();
        let ch = create_channel(&conn, "项目").unwrap();
        move_message(&conn, &b.id, &ch.id).unwrap();

        let stats = timeline_stats(&conn).unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.unfiled, 1);
        assert_eq!(
            stats.total,
            list_messages(&conn, Scope::All, 50, None).unwrap().items.len() as i64
        );
        assert_eq!(
            stats.unfiled,
            list_messages(&conn, Scope::Unfiled, 50, None).unwrap().items.len() as i64
        );
    }

    // ------------------------------------------------ 时间线翻页

    /// 往前翻时间线必须**不重不漏**。
    ///
    /// 要害是同一毫秒内的多条消息：只比时间戳的游标会在 `created_at < t`
    /// 这一步把它们整批跳过，于是往回翻会凭空少掉一段记录 ——
    /// 不报错，只是有东西不见了。这正是"翻不到旧记录"最隐蔽的成因。
    #[test]
    fn keyset_pagination_covers_same_millisecond_messages_exactly_once() {
        let db = mem();
        let conn = db.conn().unwrap();

        // 手工插入 5 条 created_at 完全一样的消息
        for i in 0..5u32 {
            let id = format!("0199aaaa-0000-7000-8000-00000000000{i}");
            conn.execute(
                "INSERT INTO message
                   (id, channel_id, body, created_at, updated_at, device_id, hlc_wall, hlc_counter, dirty)
                 VALUES (?1, ?2, ?3, 1000, 1000, 'test-device', 1000, ?4, 0)",
                params![id, INBOX_ID, format!("同毫秒第 {i} 条"), i],
            )
            .unwrap();
        }

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<Cursor> = None;
        // 每页 2 条 → 至少需要 3 页才装得下 5 条
        for _ in 0..10 {
            let page = list_messages(&conn, Scope::All, 2, cursor.as_ref()).unwrap();
            if page.items.is_empty() {
                break;
            }
            seen.extend(page.items.iter().map(|m| m.id.clone()));
            cursor = Some(Cursor::before(page.items.last().expect("已判非空")));
            if !page.has_more {
                break;
            }
        }

        assert_eq!(
            seen.len(),
            5,
            "5 条同毫秒消息必须全部翻到，实际只拿到 {} 条：{seen:?}",
            seen.len()
        );
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 5, "翻页不能重复返回同一条：{seen:?}");
    }

    /// 分页拼起来的结果，必须和一次性取回**完全一致**（含顺序）。
    #[test]
    fn paginated_and_bulk_listing_agree() {
        let db = mem();
        let conn = db.conn().unwrap();
        for i in 0..7 {
            append_message(&conn, &format!("记录 {i}"), None).unwrap();
        }

        let bulk: Vec<String> = list_messages(&conn, Scope::All, 100, None)
            .unwrap()
            .items
            .into_iter()
            .map(|m| m.id)
            .collect();

        let mut paged: Vec<String> = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = list_messages(&conn, Scope::All, 3, cursor.as_ref()).unwrap();
            if page.items.is_empty() {
                break;
            }
            paged.extend(page.items.iter().map(|m| m.id.clone()));
            cursor = Some(Cursor::before(page.items.last().expect("已判非空")));
            if !page.has_more {
                break;
            }
        }

        assert_eq!(paged, bulk, "分页结果必须和一次性取回逐条一致");
    }

    /// 按标签翻页走的是另一条 SQL，同样要能往前翻。
    #[test]
    fn pagination_works_for_tag_views_too() {
        let db = mem();
        let conn = db.conn().unwrap();
        for i in 0..5 {
            let m = append_message(&conn, &format!("带标签的记录 {i}"), None).unwrap();
            set_message_tags(&conn, &m.id, &["翻页".into()]).unwrap();
        }

        let mut seen = 0;
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = list_messages(&conn, Scope::Tag("翻页"), 2, cursor.as_ref()).unwrap();
            if page.items.is_empty() {
                break;
            }
            seen += page.items.len();
            cursor = Some(Cursor::before(page.items.last().expect("已判非空")));
            if !page.has_more {
                break;
            }
        }
        assert_eq!(seen, 5, "标签视图也必须能一页页翻完全部记录");
    }

    // ------------------------------------------------ 同步相关的 schema 行为

    #[test]
    fn fresh_database_marks_inbox_as_not_dirty() {
        let db = mem();
        let conn = db.conn().unwrap();
        let dirty: i64 = conn
            .query_row(
                "SELECT dirty FROM channel WHERE id = ?1",
                params![INBOX_ID],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dirty, 0, "收件箱是常量实体，不该被推送到服务端");
    }

    #[test]
    fn local_writes_are_dirty_and_advance_the_clock_monotonically() {
        let db = mem();
        let conn = db.conn().unwrap();

        let a = append_message(&conn, "第一条", None).unwrap();
        let b = append_message(&conn, "第二条", None).unwrap();

        let (dirty_a, dirty_b): (i64, i64) = (
            conn.query_row("SELECT dirty FROM message WHERE id=?1", params![a.id], |r| r.get(0))
                .unwrap(),
            conn.query_row("SELECT dirty FROM message WHERE id=?1", params![b.id], |r| r.get(0))
                .unwrap(),
        );
        assert_eq!((dirty_a, dirty_b), (1, 1), "本地写入必须标记为待上传");

        assert!(
            hlc_of(&conn, "message", &b.id) > hlc_of(&conn, "message", &a.id),
            "连续写入的 HLC 必须严格递增，否则后写的内容会在冲突中判负"
        );
    }

    #[test]
    fn editing_marks_dirty_and_bumps_hlc() {
        let db = mem();
        let conn = db.conn().unwrap();
        let msg = append_message(&conn, "原始内容", None).unwrap();
        conn.execute("UPDATE message SET dirty = 0 WHERE id = ?1", params![msg.id])
            .unwrap();
        let before = hlc_of(&conn, "message", &msg.id);

        update_message(&conn, &msg.id, "改过的内容").unwrap();

        let dirty: i64 = conn
            .query_row("SELECT dirty FROM message WHERE id=?1", params![msg.id], |r| r.get(0))
            .unwrap();
        assert_eq!(dirty, 1);
        assert!(hlc_of(&conn, "message", &msg.id) > before);
    }

    #[test]
    fn clock_observe_pulls_local_clock_forward() {
        let db = mem();
        let conn = db.conn().unwrap();
        let far_future = Hlc::new(messagenote_core::now_ms() + 600_000, 7, "other-device");

        clock_observe(&conn, &far_future).unwrap();
        let next = clock_next(&conn).unwrap();

        assert!(
            next > far_future,
            "观察到远端超前时间戳后，本地时钟必须被拉前，否则会持续判负"
        );
    }

    /// 迁移必须保住数据，并且把历史记录标记成待上传 ——
    /// 否则用户升级后，已有的笔记永远不会同步到服务端。
    #[test]
    fn migrates_v1_database_preserving_data_and_scheduling_upload() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();

        conn.execute(
            "INSERT INTO channel (id,name,kind,sort_order,created_at,updated_at,device_id,rev)
             VALUES ('c1','工作','normal',1,100,100,'dev-old',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id,channel_id,body,created_at,updated_at,device_id,rev)
             VALUES ('m1','c1','历史笔记',100,150,'dev-old',1)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO tag (id,name,created_at) VALUES ('t1','工作',100)", [])
            .unwrap();
        conn.execute("INSERT INTO message_tag (message_id,tag_id) VALUES ('m1','t1')", [])
            .unwrap();
        conn.execute_batch("PRAGMA user_version = 1;").unwrap();

        migrate(&conn).unwrap();

        let (body, wall, dirty): (String, i64, i64) = conn
            .query_row(
                "SELECT body, hlc_wall, dirty FROM message WHERE id='m1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(body, "历史笔记", "迁移不能丢数据");
        assert_eq!(wall, 150, "HLC 应从 updated_at 回填，而不是从 0 开始");
        assert_eq!(dirty, 1, "历史数据必须能在首次同步时上传");

        // 标签改用名字做主键，关联跟着迁过来
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM tag WHERE name='工作'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM message_tag WHERE tag_name='工作'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );

        assert!(
            conn.prepare("SELECT rev FROM message").is_err(),
            "rev 应当已被移除"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // 再跑一次不应报错，也不应重复施加 ALTER
        migrate(&conn).unwrap();
    }

    /// 拿一份**真实的旧库副本**跑迁移。
    ///
    /// 上面那个迁移测试用的 v1 库是手工造的，形状是"我以为的 v1"。
    /// 真实数据才可能带上我没想到的东西（WAL 里的未检查点数据、
    /// 意料之外的 NULL、被外键牵连的行……），所以这一步不能省。
    ///
    /// 用法：
    ///   $env:MESSAGENOTE_MIGRATE_TEST_DB = "…\副本.sqlite"
    ///   cargo test -p messagenote -- --ignored --nocapture
    #[test]
    #[ignore = "需要 MESSAGENOTE_MIGRATE_TEST_DB 指向一份真实旧库副本"]
    fn migrates_a_real_v1_database() {
        let path = std::env::var("MESSAGENOTE_MIGRATE_TEST_DB")
            .expect("请设置 MESSAGENOTE_MIGRATE_TEST_DB 指向旧库副本");

        // 迁移前先记录基线。注意用只读连接，避免这一步就把库给动了。
        let (v_before, msgs_before, chans_before) = {
            let before = Connection::open(&path).expect("打开旧库副本");
            (
                before
                    .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                    .expect("读 user_version"),
                before
                    .query_row("SELECT COUNT(*) FROM message", [], |r| r.get::<_, i64>(0))
                    .expect("数消息"),
                before
                    .query_row("SELECT COUNT(*) FROM channel", [], |r| r.get::<_, i64>(0))
                    .expect("数频道"),
            )
        };
        println!("迁移前：user_version={v_before} 消息={msgs_before} 频道={chans_before}");

        let db = open(Path::new(&path)).expect("打开并迁移真实库");
        let conn = db.conn().unwrap();

        let v_after: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        let msgs_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        let chans_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM channel", [], |r| r.get(0))
            .unwrap();

        assert_eq!(v_after, SCHEMA_VERSION, "迁移后应是最新版本");
        assert_eq!(msgs_after, msgs_before, "迁移不能丢消息");
        assert_eq!(chans_after, chans_before, "迁移不能丢频道");

        let stale: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message WHERE dirty = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0, "迁移后所有历史消息都必须是待上传状态");

        let no_hlc: i64 = conn
            .query_row("SELECT COUNT(*) FROM message WHERE hlc_wall = 0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(no_hlc, 0, "HLC 必须从 updated_at 回填，不能留在 0");

        println!("迁移后：user_version={v_after} 消息={msgs_after} 频道={chans_after}");
        for m in conn
            .prepare("SELECT id, substr(body,1,24), hlc_wall, dirty FROM message")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
        {
            let (id, body, wall, dirty) = m.unwrap();
            println!("  {id} hlc_wall={wall} dirty={dirty}  {body}");
        }
    }
}
