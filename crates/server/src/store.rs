//! 服务端存储层。
//!
//! 和客户端存储层**刻意不共享写入实现**：客户端有 `dirty` 标记和本地检索索引，
//! 服务端有的是单调递增的 `server_seq`。硬把两套表抽象成一个，只会得到一个
//! 到处是分支的怪物。
//!
//! 但**浏览和检索**是例外：那部分查询只碰两边都有的列，住在 `messagenote-store`
//! 里，两端跑的是同一份实现。理由见那个 crate 的文档 —— 简而言之，时间线
//! "怎么排、怎么翻页"如果两边各写一份，漂移是静默的。
//!
//! ## 服务端只多做一件事：分配 seq
//!
//! `server_seq` 是"我上次同步之后有哪些变化"的唯一可靠游标。用时间戳做游标
//! 的话，任何一台设备时钟偏慢，它写的数据就永远落在别的设备游标之前 ——
//! **永久漏拉，而且不报错**。
//!
//! 注意 seq 的分配和"谁赢"是两件事：即使一条变更被拒绝（HLC 更旧），
//! 也不分配新的 seq，因为服务端的状态没有变化。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};

use messagenote_core::hlc::Hlc;
use messagenote_core::merge;
use messagenote_core::models::{Channel, MessagePage, SearchHit, TagCount, TimelineStats};
use messagenote_core::payload::{
    message_tag_key, ChannelPayload, MessagePayload, MessageTagPayload, TagPayload,
};
use messagenote_core::search;
use messagenote_core::wire::{Change, EntityKind, PullResponse, PushOutcome, PushResponse};
use messagenote_store::{Cursor, Scope};

use crate::error::{ServerError, ServerResult};

const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = r#"
-- 单行计数器。seq 必须全局单调，不能按实体各自计数，
-- 否则 4 张表的 seq 会互相穿插、游标失去意义。
CREATE TABLE IF NOT EXISTS seq_counter (
  id    INTEGER PRIMARY KEY CHECK (id = 1),
  value INTEGER NOT NULL
);
INSERT OR IGNORE INTO seq_counter (id, value) VALUES (1, 0);

CREATE TABLE IF NOT EXISTS channel (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  kind        TEXT NOT NULL DEFAULT 'normal',
  sort_order  INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  device_id   TEXT NOT NULL DEFAULT '',
  hlc_wall    INTEGER NOT NULL,
  hlc_counter INTEGER NOT NULL,
  deleted_at  INTEGER,
  server_seq  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_channel_seq ON channel(server_seq);

CREATE TABLE IF NOT EXISTS message (
  id          TEXT PRIMARY KEY,
  channel_id  TEXT NOT NULL,
  body        TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  device_id   TEXT NOT NULL DEFAULT '',
  hlc_wall    INTEGER NOT NULL,
  hlc_counter INTEGER NOT NULL,
  deleted_at  INTEGER,
  server_seq  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_message_seq ON message(server_seq);

CREATE TABLE IF NOT EXISTS tag (
  name        TEXT PRIMARY KEY,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  device_id   TEXT NOT NULL DEFAULT '',
  hlc_wall    INTEGER NOT NULL,
  hlc_counter INTEGER NOT NULL,
  deleted_at  INTEGER,
  server_seq  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tag_seq ON tag(server_seq);

CREATE TABLE IF NOT EXISTS message_tag (
  message_id  TEXT NOT NULL,
  tag_name    TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  device_id   TEXT NOT NULL DEFAULT '',
  hlc_wall    INTEGER NOT NULL,
  hlc_counter INTEGER NOT NULL,
  deleted_at  INTEGER,
  server_seq  INTEGER NOT NULL,
  PRIMARY KEY (message_id, tag_name)
);
CREATE INDEX IF NOT EXISTS idx_message_tag_seq ON message_tag(server_seq);

-- 检索索引。定义和客户端那份**逐字一致** —— 两端用同一个 core::search 规划查询，
-- 索引文本也必须用同一个 core::search::to_index_text 生成，否则同样的关键词
-- 在一端搜得到、在另一端搜不到。
--
-- 独立 FTS5 表而不是 external-content 表：索引内容（bigram 展开）是 Rust 侧
-- 算出来的，SQL 触发器算不出来。代价是**忘了同步索引就是静默的检索失效**，
-- 所以写入路径必须成对出现在同一个事务里（见 upsert）。
CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
  search_text,
  message_id UNINDEXED,
  tokenize = 'unicode61 remove_diacritics 2'
);
"#;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> ServerResult<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;

        let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current < SCHEMA_VERSION {
            conn.execute_batch(SCHEMA)?;
            // v2 加了检索索引。已经有数据的服务端升级上来时，建了表却是空的 ——
            // 表现是"检索永远没有结果"，而且不报任何错。所以必须回填。
            if current < 2 {
                backfill_fts(&conn)?;
            }
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        }
        seed_constants(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 内存库。端到端测试要在一个进程里把**真实**服务端跑起来，
    /// 用临时文件不仅慢，还得处理清理和残留。
    pub fn in_memory() -> ServerResult<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        seed_constants(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> ServerResult<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| ServerError::Msg("数据库连接锁已损坏".into()))
    }

    /// 当前最大 seq。客户端首次同步时从这里开始，避免把整个历史重放一遍。
    pub fn max_seq(&self) -> ServerResult<i64> {
        let conn = self.conn()?;
        Ok(max_seq(&conn)?)
    }

    /// 拉取 `since` 之后的变更。
    pub fn pull(&self, since: i64, limit: i64) -> ServerResult<PullResponse> {
        let limit = limit.clamp(1, 1000);
        let conn = self.conn()?;

        let mut collected: Vec<(i64, Change)> = Vec::new();
        // 每个实体各取"大于 since 的最小 limit 条"，再在内存里归并取全局前 limit 条。
        // 这样是 4 次查询而不是 N+1；而且"每个 kind 各取最小的 limit 条"
        // 保证了全局前 limit 条一定落在取到的集合里，不会漏。
        collect_channels(&conn, since, limit, &mut collected)?;
        collect_messages(&conn, since, limit, &mut collected)?;
        collect_tags(&conn, since, limit, &mut collected)?;
        collect_message_tags(&conn, since, limit, &mut collected)?;

        collected.sort_by_key(|(seq, _)| *seq);
        let changes: Vec<Change> = collected
            .into_iter()
            .take(limit as usize)
            .map(|(_, c)| c)
            .collect();

        // 游标取本批实际返回的最大 seq。一条都没返回时保持不变，
        // 让客户端停在原地而不是跳到未知位置。
        let cursor = changes
            .iter()
            .filter_map(|c| c.seq)
            .max()
            .unwrap_or(since);

        // 注意：**不能**用 `collected.len() > limit` 判断还有没有更多。
        // 每个实体种类都各自被 LIMIT 截断过，被截掉的尾部是看不见的：
        // 3 条消息、limit=2 时 collected.len() 恰好等于 2，那个算法会得出
        // "没有更多了"，客户端从此再也拉不到第 3 条 —— 而且不报任何错。
        // 必须直接问一句"游标之后还有没有行"。
        let has_more = has_any_after(&conn, cursor)?;

        Ok(PullResponse {
            changes,
            cursor,
            has_more,
        })
    }

    /// 接收一批变更。
    ///
    /// 整批一个事务：要么全部落地，要么全部不落地。半途失败会让客户端
    /// 误以为"推过了"，而实际上部分变更从未被接受。
    pub fn push(&self, changes: &[Change]) -> ServerResult<PushResponse> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;
        let mut results = Vec::with_capacity(changes.len());

        for c in changes {
            let stored = read_stored_hlc(&tx, c.kind, &c.id)?;

            // 用 core 里那**同一个**函数做裁定。客户端和服务端必须对
            // "谁更新"得出完全一致的结论，否则两边各自认为自己的版本胜出，
            // 最终收敛不到同一个状态，而且不报任何错。
            if merge::should_accept_push(&c.hlc, stored.as_ref()) {
                let seq = next_seq(&tx)?;
                upsert(&tx, c, seq)?;
                results.push(PushOutcome {
                    kind: c.kind,
                    id: c.id.clone(),
                    accepted: true,
                    winner: None,
                });
            } else {
                results.push(PushOutcome {
                    kind: c.kind,
                    id: c.id.clone(),
                    accepted: false,
                    winner: read_change(&tx, c.kind, &c.id)?,
                });
            }
        }

        tx.commit()?;
        Ok(PushResponse { results })
    }

    // ---------------------------------------------------------------- 读取
    //
    // 全部委托给 `messagenote-store`，这里只负责加锁。
    //
    // 这一点很要紧：网页端看到的**不是**"一份和桌面端长得像的实现"，而是
    // 同一份。时间线怎么排、未归档怎么算、同一毫秒的兄弟行怎么翻页，
    // 这些语义如果两边各写一份，漂移起来是静默的 —— 差几条，不报错。

    pub fn list_channels(&self) -> ServerResult<Vec<Channel>> {
        let conn = self.conn()?;
        Ok(messagenote_store::list_channels(&conn)?)
    }

    pub fn list_tags(&self) -> ServerResult<Vec<TagCount>> {
        let conn = self.conn()?;
        Ok(messagenote_store::list_tags(&conn)?)
    }

    pub fn timeline_stats(&self) -> ServerResult<TimelineStats> {
        let conn = self.conn()?;
        Ok(messagenote_store::timeline_stats(&conn)?)
    }

    pub fn list_messages(
        &self,
        scope: Scope<'_>,
        limit: i64,
        before: Option<&Cursor>,
    ) -> ServerResult<MessagePage> {
        let conn = self.conn()?;
        Ok(messagenote_store::list_messages(&conn, scope, limit, before)?)
    }

    pub fn search(&self, query: &str, limit: i64) -> ServerResult<Vec<SearchHit>> {
        let conn = self.conn()?;
        Ok(messagenote_store::search(&conn, query, limit)?)
    }
}

// ---------------------------------------------------------------- seq

/// 游标之后是否还有任何变更。用 EXISTS 而不是 COUNT：
/// 只需要知道"有没有"，不必把整个后缀数一遍（每张表的 server_seq 上都有索引，
/// 但 EXISTS 能在第一行就短路返回）。
fn has_any_after(conn: &Connection, cursor: i64) -> ServerResult<bool> {
    let more: i64 = conn.query_row(
        "SELECT
           EXISTS(SELECT 1 FROM channel     WHERE server_seq > ?1)
         + EXISTS(SELECT 1 FROM message     WHERE server_seq > ?1)
         + EXISTS(SELECT 1 FROM tag         WHERE server_seq > ?1)
         + EXISTS(SELECT 1 FROM message_tag WHERE server_seq > ?1)",
        params![cursor],
        |r| r.get(0),
    )?;
    Ok(more > 0)
}

fn max_seq(conn: &Connection) -> ServerResult<i64> {
    Ok(conn.query_row("SELECT value FROM seq_counter WHERE id = 1", [], |r| {
        r.get(0)
    })?)
}

/// 取下一个 seq。
///
/// `UPDATE ... RETURNING` 在一条语句里完成"读-改-写"，不需要额外的锁；
/// 外层事务保证它和这一批的行写入一起提交。
fn next_seq(conn: &Connection) -> ServerResult<i64> {
    let seq = conn.query_row(
        "UPDATE seq_counter SET value = value + 1 WHERE id = 1 RETURNING value",
        [],
        |r| r.get::<_, i64>(0),
    )?;
    Ok(seq)
}

// ---------------------------------------------------------------- 读取

fn read_stored_hlc(conn: &Connection, kind: EntityKind, id: &str) -> ServerResult<Option<Hlc>> {
    let (sql, key): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match kind {
        EntityKind::Channel => (
            "SELECT hlc_wall, hlc_counter, device_id FROM channel WHERE id = ?1",
            vec![Box::new(id.to_string())],
        ),
        EntityKind::Message => (
            "SELECT hlc_wall, hlc_counter, device_id FROM message WHERE id = ?1",
            vec![Box::new(id.to_string())],
        ),
        EntityKind::Tag => (
            "SELECT hlc_wall, hlc_counter, device_id FROM tag WHERE name = ?1",
            vec![Box::new(id.to_string())],
        ),
        EntityKind::MessageTag => {
            let Some((mid, tname)) = message_tag_key::decode(id) else {
                return Ok(None);
            };
            (
                "SELECT hlc_wall, hlc_counter, device_id FROM message_tag
                  WHERE message_id = ?1 AND tag_name = ?2",
                vec![Box::new(mid.to_string()), Box::new(tname.to_string())],
            )
        }
    };

    let hlc = conn
        .query_row(sql, rusqlite::params_from_iter(key.iter().map(|b| b.as_ref())), |r| {
            Ok(Hlc::new(
                r.get::<_, i64>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .optional()?;
    Ok(hlc)
}

/// 读回某一行当前的完整变更（含它的 seq），用于把胜出者回传给客户端。
fn read_change(conn: &Connection, kind: EntityKind, id: &str) -> ServerResult<Option<Change>> {
    let change = match kind {
        EntityKind::Channel => conn
            .query_row(
                "SELECT name, kind, sort_order, created_at, updated_at, device_id,
                        hlc_wall, hlc_counter, deleted_at, server_seq
                   FROM channel WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Change {
                        seq: Some(r.get::<_, i64>(9)?),
                        kind: EntityKind::Channel,
                        id: id.to_string(),
                        hlc: Hlc::new(
                            r.get::<_, i64>(6)?,
                            r.get::<_, u32>(7)?,
                            r.get::<_, String>(5)?,
                        ),
                        deleted: r.get::<_, Option<i64>>(8)?.is_some(),
                        data: serde_json::to_value(ChannelPayload {
                            name: r.get(0)?,
                            kind: r.get(1)?,
                            sort_order: r.get(2)?,
                            created_at: r.get(3)?,
                            updated_at: r.get(4)?,
                        })
                        .ok(),
                    })
                },
            )
            .optional()?,

        EntityKind::Message => conn
            .query_row(
                "SELECT channel_id, body, created_at, updated_at, device_id,
                        hlc_wall, hlc_counter, deleted_at, server_seq
                   FROM message WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Change {
                        seq: Some(r.get::<_, i64>(8)?),
                        kind: EntityKind::Message,
                        id: id.to_string(),
                        hlc: Hlc::new(
                            r.get::<_, i64>(5)?,
                            r.get::<_, u32>(6)?,
                            r.get::<_, String>(4)?,
                        ),
                        deleted: r.get::<_, Option<i64>>(7)?.is_some(),
                        data: serde_json::to_value(MessagePayload {
                            channel_id: r.get(0)?,
                            body: r.get(1)?,
                            created_at: r.get(2)?,
                            updated_at: r.get(3)?,
                        })
                        .ok(),
                    })
                },
            )
            .optional()?,

        EntityKind::Tag => conn
            .query_row(
                "SELECT created_at, updated_at, device_id, hlc_wall, hlc_counter, deleted_at, server_seq
                   FROM tag WHERE name = ?1",
                params![id],
                |r| {
                    Ok(Change {
                        seq: Some(r.get::<_, i64>(6)?),
                        kind: EntityKind::Tag,
                        id: id.to_string(),
                        hlc: Hlc::new(
                            r.get::<_, i64>(3)?,
                            r.get::<_, u32>(4)?,
                            r.get::<_, String>(2)?,
                        ),
                        deleted: r.get::<_, Option<i64>>(5)?.is_some(),
                        data: serde_json::to_value(TagPayload {
                            created_at: r.get(0)?,
                            updated_at: r.get(1)?,
                        })
                        .ok(),
                    })
                },
            )
            .optional()?,

        EntityKind::MessageTag => {
            let Some((mid, tname)) = message_tag_key::decode(id) else {
                return Ok(None);
            };
            conn.query_row(
                "SELECT created_at, updated_at, device_id, hlc_wall, hlc_counter, deleted_at, server_seq
                   FROM message_tag WHERE message_id = ?1 AND tag_name = ?2",
                params![mid, tname],
                |r| {
                    Ok(Change {
                        seq: Some(r.get::<_, i64>(6)?),
                        kind: EntityKind::MessageTag,
                        id: id.to_string(),
                        hlc: Hlc::new(
                            r.get::<_, i64>(3)?,
                            r.get::<_, u32>(4)?,
                            r.get::<_, String>(2)?,
                        ),
                        deleted: r.get::<_, Option<i64>>(5)?.is_some(),
                        data: serde_json::to_value(MessageTagPayload {
                            message_id: mid.to_string(),
                            tag_name: tname.to_string(),
                            created_at: r.get(0)?,
                            updated_at: r.get(1)?,
                        })
                        .ok(),
                    })
                },
            )
            .optional()?
        }
    };
    Ok(change)
}

// ---------------------------------------------------------------- pull 收集

fn push_row(out: &mut Vec<(i64, Change)>, r: rusqlite::Result<(i64, Change)>) -> ServerResult<()> {
    out.push(r?);
    Ok(())
}

fn collect_channels(
    conn: &Connection,
    since: i64,
    limit: i64,
    out: &mut Vec<(i64, Change)>,
) -> ServerResult<()> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, sort_order, created_at, updated_at, device_id,
                hlc_wall, hlc_counter, deleted_at, server_seq
           FROM channel WHERE server_seq > ?1 ORDER BY server_seq LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![since, limit], |r| {
        let seq: i64 = r.get(10)?;
        Ok((
            seq,
            Change {
                seq: Some(seq),
                kind: EntityKind::Channel,
                id: r.get(0)?,
                hlc: Hlc::new(
                    r.get::<_, i64>(7)?,
                    r.get::<_, u32>(8)?,
                    r.get::<_, String>(6)?,
                ),
                deleted: r.get::<_, Option<i64>>(9)?.is_some(),
                data: serde_json::to_value(ChannelPayload {
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    sort_order: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                })
                .ok(),
            },
        ))
    })?;
    for r in rows {
        push_row(out, r)?;
    }
    Ok(())
}

fn collect_messages(
    conn: &Connection,
    since: i64,
    limit: i64,
    out: &mut Vec<(i64, Change)>,
) -> ServerResult<()> {
    let mut stmt = conn.prepare(
        "SELECT id, channel_id, body, created_at, updated_at, device_id,
                hlc_wall, hlc_counter, deleted_at, server_seq
           FROM message WHERE server_seq > ?1 ORDER BY server_seq LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![since, limit], |r| {
        let seq: i64 = r.get(9)?;
        Ok((
            seq,
            Change {
                seq: Some(seq),
                kind: EntityKind::Message,
                id: r.get(0)?,
                hlc: Hlc::new(
                    r.get::<_, i64>(6)?,
                    r.get::<_, u32>(7)?,
                    r.get::<_, String>(5)?,
                ),
                deleted: r.get::<_, Option<i64>>(8)?.is_some(),
                data: serde_json::to_value(MessagePayload {
                    channel_id: r.get(1)?,
                    body: r.get(2)?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                })
                .ok(),
            },
        ))
    })?;
    for r in rows {
        push_row(out, r)?;
    }
    Ok(())
}

fn collect_tags(
    conn: &Connection,
    since: i64,
    limit: i64,
    out: &mut Vec<(i64, Change)>,
) -> ServerResult<()> {
    let mut stmt = conn.prepare(
        "SELECT name, created_at, updated_at, device_id, hlc_wall, hlc_counter, deleted_at, server_seq
           FROM tag WHERE server_seq > ?1 ORDER BY server_seq LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![since, limit], |r| {
        let seq: i64 = r.get(7)?;
        Ok((
            seq,
            Change {
                seq: Some(seq),
                kind: EntityKind::Tag,
                id: r.get(0)?,
                hlc: Hlc::new(
                    r.get::<_, i64>(4)?,
                    r.get::<_, u32>(5)?,
                    r.get::<_, String>(3)?,
                ),
                deleted: r.get::<_, Option<i64>>(6)?.is_some(),
                data: serde_json::to_value(TagPayload {
                    created_at: r.get(1)?,
                    updated_at: r.get(2)?,
                })
                .ok(),
            },
        ))
    })?;
    for r in rows {
        push_row(out, r)?;
    }
    Ok(())
}

fn collect_message_tags(
    conn: &Connection,
    since: i64,
    limit: i64,
    out: &mut Vec<(i64, Change)>,
) -> ServerResult<()> {
    let mut stmt = conn.prepare(
        "SELECT message_id, tag_name, created_at, updated_at, device_id,
                hlc_wall, hlc_counter, deleted_at, server_seq
           FROM message_tag WHERE server_seq > ?1 ORDER BY server_seq LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![since, limit], |r| {
        let seq: i64 = r.get(8)?;
        let message_id: String = r.get(0)?;
        let tag_name: String = r.get(1)?;
        Ok((
            seq,
            Change {
                seq: Some(seq),
                kind: EntityKind::MessageTag,
                id: message_tag_key::encode(&message_id, &tag_name),
                hlc: Hlc::new(
                    r.get::<_, i64>(5)?,
                    r.get::<_, u32>(6)?,
                    r.get::<_, String>(4)?,
                ),
                deleted: r.get::<_, Option<i64>>(7)?.is_some(),
                data: serde_json::to_value(MessageTagPayload {
                    message_id,
                    tag_name,
                    created_at: r.get(2)?,
                    updated_at: r.get(3)?,
                })
                .ok(),
            },
        ))
    })?;
    for r in rows {
        push_row(out, r)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- 写入

/// 建库时就要存在的常量行，且**不参与同步**。
///
/// 收件箱是**常量实体**：每台设备（包括服务端）都独立创建同样的它。
/// 客户端不推送它（`dirty` 恒为 0），服务端也不把它算进变更流
/// （`server_seq = 0`，任何 `pull(since >= 0)` 都拉不到）。
///
/// 服务端也必须有这一份，否则网页端的频道列表里**没有收件箱** —— 而"未归档"
/// 正是收件箱的另一个名字，网页端会缺少最主要的那个入口。
///
/// 这一条是被 `desktop_and_server_agree_on_browse_and_search` 抓出来的：
/// 那个测试逐字段比对两端的频道列表，桌面端两个、服务端只有一个。
fn seed_constants(conn: &Connection) -> ServerResult<()> {
    conn.execute(
        "INSERT OR IGNORE INTO channel
           (id, name, kind, sort_order, created_at, updated_at, device_id,
            hlc_wall, hlc_counter, deleted_at, server_seq)
         VALUES (?1, '收件箱', 'inbox', 0, 0, 0, '', 0, 0, NULL, 0)",
        params![messagenote_store::INBOX_ID],
    )?;
    Ok(())
}

/// 为 `message` 表里已有的行重建检索索引。
///
/// 索引文本（bigram 展开）只有 Rust 侧算得出来，SQL 算不出来 —— 这既是索引
/// 维护没做成触发器的原因，也是这里必须把正文读出来、算一遍、再写回去的原因。
///
/// 只在"从没有索引的旧版本升上来"这条路径上跑。
fn backfill_fts(conn: &Connection) -> ServerResult<()> {
    // 先把行读进内存，再开事务写。不然 `stmt` 借用着 conn，事务就借不到了。
    let rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT id, body FROM message WHERE deleted_at IS NULL")?;
        let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        it.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let tx = conn.unchecked_transaction()?;
    // 这条路径上索引本来就该是空的，清一次是幂等的
    tx.execute("DELETE FROM message_fts", [])?;
    for (id, body) in &rows {
        tx.execute(
            "INSERT INTO message_fts (search_text, message_id) VALUES (?1, ?2)",
            params![search::to_index_text(body), id],
        )?;
    }
    tx.commit()?;

    if !rows.is_empty() {
        tracing::info!(count = rows.len(), "升级：检索索引已回填");
    }
    Ok(())
}

/// 把一条已接受的变更落库，并打上新的 seq。
///
/// 时间字段一律取自变更本身（含墓碑的 `deleted_at`，用 payload 的
/// `updated_at` 推导）。用服务端本地时间的话，不同客户端拉到的
/// `deleted_at` 会不一致，状态永远收敛不了 —— 而且不报任何错。
fn upsert(conn: &Connection, c: &Change, seq: i64) -> ServerResult<()> {
    let h = &c.hlc;

    match c.kind {
        EntityKind::Channel => {
            let p: ChannelPayload = decode(c)?;
            conn.execute(
                "INSERT INTO channel
                   (id, name, kind, sort_order, created_at, updated_at, device_id,
                    hlc_wall, hlc_counter, deleted_at, server_seq)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name, kind = excluded.kind, sort_order = excluded.sort_order,
                   created_at = excluded.created_at, updated_at = excluded.updated_at,
                   device_id = excluded.device_id,
                   hlc_wall = excluded.hlc_wall, hlc_counter = excluded.hlc_counter,
                   deleted_at = excluded.deleted_at, server_seq = excluded.server_seq",
                params![
                    c.id, p.name, p.kind, p.sort_order, p.created_at, p.updated_at,
                    h.device, h.wall, h.counter,
                    if c.deleted { Some(p.updated_at) } else { None },
                    seq
                ],
            )?;
        }

        EntityKind::Message => {
            let p: MessagePayload = decode(c)?;
            conn.execute(
                "INSERT INTO message
                   (id, channel_id, body, created_at, updated_at, device_id,
                    hlc_wall, hlc_counter, deleted_at, server_seq)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(id) DO UPDATE SET
                   channel_id = excluded.channel_id, body = excluded.body,
                   created_at = excluded.created_at, updated_at = excluded.updated_at,
                   device_id = excluded.device_id,
                   hlc_wall = excluded.hlc_wall, hlc_counter = excluded.hlc_counter,
                   deleted_at = excluded.deleted_at, server_seq = excluded.server_seq",
                params![
                    c.id, p.channel_id, p.body, p.created_at, p.updated_at,
                    h.device, h.wall, h.counter,
                    if c.deleted { Some(p.updated_at) } else { None },
                    seq
                ],
            )?;

            // 检索索引是派生数据，但必须和正文在**同一个事务**里更新。
            // 分开写的话，索引和数据会静默漂移 —— 表现成"搜得到但点不开"
            // 或者"明明写了却搜不到"，两种都极难查。
            //
            // 注意 `upsert` 拿到的 conn 就是 `push` 那个事务，所以这里天然同事务。
            conn.execute("DELETE FROM message_fts WHERE message_id = ?1", params![c.id])?;
            if !c.deleted {
                conn.execute(
                    "INSERT INTO message_fts (search_text, message_id) VALUES (?1, ?2)",
                    params![search::to_index_text(&p.body), c.id],
                )?;
            }
        }

        EntityKind::Tag => {
            let p: TagPayload = decode(c)?;
            conn.execute(
                "INSERT INTO tag
                   (name, created_at, updated_at, device_id, hlc_wall, hlc_counter, deleted_at, server_seq)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(name) DO UPDATE SET
                   created_at = excluded.created_at, updated_at = excluded.updated_at,
                   device_id = excluded.device_id,
                   hlc_wall = excluded.hlc_wall, hlc_counter = excluded.hlc_counter,
                   deleted_at = excluded.deleted_at, server_seq = excluded.server_seq",
                params![
                    c.id, p.created_at, p.updated_at, h.device, h.wall, h.counter,
                    if c.deleted { Some(p.updated_at) } else { None },
                    seq
                ],
            )?;
        }

        EntityKind::MessageTag => {
            let p: MessageTagPayload = decode(c)?;
            conn.execute(
                "INSERT INTO message_tag
                   (message_id, tag_name, created_at, updated_at, device_id,
                    hlc_wall, hlc_counter, deleted_at, server_seq)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(message_id, tag_name) DO UPDATE SET
                   created_at = excluded.created_at, updated_at = excluded.updated_at,
                   device_id = excluded.device_id,
                   hlc_wall = excluded.hlc_wall, hlc_counter = excluded.hlc_counter,
                   deleted_at = excluded.deleted_at, server_seq = excluded.server_seq",
                params![
                    p.message_id, p.tag_name, p.created_at, p.updated_at,
                    h.device, h.wall, h.counter,
                    if c.deleted { Some(p.updated_at) } else { None },
                    seq
                ],
            )?;
        }
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(c: &Change) -> ServerResult<T> {
    let data = c.data.clone().ok_or_else(|| {
        ServerError::Msg(format!("变更缺少 data：{} {}", c.kind.as_str(), c.id))
    })?;
    serde_json::from_value(data)
        .map_err(|e| ServerError::Msg(format!("变更 data 解析失败（{}）：{e}", c.id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 共享存储层。这里是**服务端**第一次真的用它 —— 见 shared_browse_queries_…
    use messagenote_store::{Cursor, Scope};

    fn store() -> Store {
        Store::in_memory().unwrap()
    }

    fn msg(id: &str, wall: i64, counter: u32, device: &str, body: &str) -> Change {
        Change {
            seq: None,
            kind: EntityKind::Message,
            id: id.into(),
            hlc: Hlc::new(wall, counter, device),
            deleted: false,
            data: Some(json!({
                "channelId": "inbox",
                "body": body,
                "createdAt": wall,
                "updatedAt": wall
            })),
        }
    }

    fn msg_in(id: &str, channel_id: &str, wall: i64, body: &str) -> Change {
        let mut c = msg(id, wall, 0, "a", body);
        c.data = Some(json!({
            "channelId": channel_id,
            "body": body,
            "createdAt": wall,
            "updatedAt": wall
        }));
        c
    }

    fn channel_change(id: &str, name: &str, kind: &str, sort: i64, wall: i64) -> Change {
        Change {
            seq: None,
            kind: EntityKind::Channel,
            id: id.into(),
            hlc: Hlc::new(wall, 0, "a"),
            deleted: false,
            data: Some(json!({
                "name": name,
                "kind": kind,
                "sortOrder": sort,
                "createdAt": wall,
                "updatedAt": wall
            })),
        }
    }

    fn tag_change(name: &str, wall: i64) -> Change {
        Change {
            seq: None,
            kind: EntityKind::Tag,
            id: name.into(),
            hlc: Hlc::new(wall, 0, "a"),
            deleted: false,
            data: Some(json!({ "createdAt": wall, "updatedAt": wall })),
        }
    }

    fn msg_tag_change(message_id: &str, tag_name: &str, wall: i64) -> Change {
        Change {
            seq: None,
            kind: EntityKind::MessageTag,
            id: messagenote_core::payload::message_tag_key::encode(message_id, tag_name),
            hlc: Hlc::new(wall, 0, "a"),
            deleted: false,
            data: Some(json!({
                "messageId": message_id,
                "tagName": tag_name,
                "createdAt": wall,
                "updatedAt": wall
            })),
        }
    }

    /// **验证 `messagenote-store` 的立身之本：那些浏览查询在服务端的 schema 上
    /// 同样成立。**
    ///
    /// `crates/store` 存在的全部理由，是"服务端的表是客户端同名表的超集，
    /// 所以浏览用的 SQL 两边都能跑"。这句话如果只是读一遍 schema 得出的推断，
    /// 那这个 crate 就建在沙子上 —— 而它承载的是"桌面端和网页端看到的时间线
    /// 完全一致"这件事，漂移起来是静默的（差几条，不报错）。
    ///
    /// 所以这里用**真实的 `Store`** 落一批数据，再把共享查询原样跑一遍。
    /// 任何一条语句哪天碰到了服务端没有的列，这个测试会立刻红。
    #[test]
    fn shared_browse_queries_work_on_the_server_schema() {
        let s = store();
        // 刻意**不推**收件箱：客户端从不推送它（dirty 恒为 0），服务端
        // 建库时就自己种了一份。这里要验证的正是"服务端那份够用"。
        s.push(&[
            channel_change("ch-work", "工作", "normal", 1, 11),
            msg("m1", 100, 0, "a", "收件箱里的一条"),
            msg_in("m2", "ch-work", 101, "工作里的一条"),
            tag_change("重要", 102),
            msg_tag_change("m1", "重要", 103),
        ])
        .unwrap();

        let conn = s.conn().unwrap();

        // 频道列表：收件箱必须排在最前 —— 排序表达式 `(kind = 'inbox') DESC`
        // 是 SQLite 特有的写法，值得单独确认它在服务端表上也成立。
        let chans = messagenote_store::list_channels(&conn).unwrap();
        assert_eq!(chans.len(), 2, "服务端种下的收件箱 + 推上来的工作频道");
        assert_eq!(chans[0].id, "inbox", "收件箱要排在第一位");
        assert_eq!(chans[0].kind, "inbox");
        assert_eq!(chans[0].message_count, 1);
        assert_eq!(chans[1].id, "ch-work");
        assert_eq!(chans[1].message_count, 1);

        // 时间线主视图
        let all = messagenote_store::list_messages(&conn, Scope::All, 50, None).unwrap();
        assert_eq!(all.items.len(), 2);
        assert_eq!(all.items[0].id, "m2", "按 created_at 倒序");
        assert!(!all.has_more);

        // 未归档 —— 也就是时间线上那个筛选条
        let unfiled = messagenote_store::list_messages(&conn, Scope::Unfiled, 50, None).unwrap();
        assert_eq!(unfiled.items.len(), 1);
        assert_eq!(unfiled.items[0].id, "m1");

        // 按频道
        let work =
            messagenote_store::list_messages(&conn, Scope::Channel("ch-work"), 50, None).unwrap();
        assert_eq!(work.items.len(), 1);
        assert_eq!(work.items[0].id, "m2");

        // 按标签：标签是横切的，未归档的那条照样查得到
        let tagged = messagenote_store::list_messages(&conn, Scope::Tag("重要"), 50, None).unwrap();
        assert_eq!(tagged.items.len(), 1);
        assert_eq!(tagged.items[0].id, "m1");
        assert_eq!(
            tagged.items[0].tags,
            vec!["重要".to_string()],
            "标签要跟着消息一起带出来，否则网页端渲染不出 chip"
        );

        // 统计与筛选必须对得上 —— 侧边栏显示"未归档 3"而点进去只有 2 条，
        // 这种小出入最伤信任
        let stats = messagenote_store::timeline_stats(&conn).unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.unfiled, 1);
        assert_eq!(stats.unfiled as usize, unfiled.items.len());

        let tags = messagenote_store::list_tags(&conn).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "重要");
        assert_eq!(tags[0].count, 1);

        // 键集分页：同一毫秒的兄弟行不漏，翻到底 has_more 要变成 false
        let page1 = messagenote_store::list_messages(&conn, Scope::All, 1, None).unwrap();
        assert_eq!(page1.items.len(), 1);
        assert_eq!(page1.items[0].id, "m2");
        assert!(page1.has_more, "还有更早的");

        let cursor = Cursor::before(&page1.items[0]);
        let page2 = messagenote_store::list_messages(&conn, Scope::All, 1, Some(&cursor)).unwrap();
        assert_eq!(page2.items.len(), 1);
        assert_eq!(page2.items[0].id, "m1");
        assert!(!page2.has_more, "已经翻到底了，不该再说还有");
    }

    #[test]
    fn accepts_a_new_row_and_assigns_a_seq() {
        let s = store();
        let resp = s.push(&[msg("m1", 100, 0, "a", "第一条")]).unwrap();
        assert!(resp.results[0].accepted);
        assert_eq!(s.max_seq().unwrap(), 1);

        let pulled = s.pull(0, 100).unwrap();
        assert_eq!(pulled.changes.len(), 1);
        assert_eq!(pulled.cursor, 1);
        assert!(!pulled.has_more);
        assert_eq!(pulled.changes[0].id, "m1");
    }

    #[test]
    fn rejects_an_older_change_and_returns_the_winner() {
        let s = store();
        s.push(&[msg("m1", 200, 0, "a", "新的")]).unwrap();
        let resp = s.push(&[msg("m1", 100, 0, "b", "旧的")]).unwrap();

        let outcome = &resp.results[0];
        assert!(!outcome.accepted, "更旧的变更不能覆盖更新的");
        let winner = outcome.winner.as_ref().expect("必须回传胜出者");
        assert_eq!(winner.hlc, Hlc::new(200, 0, "a"));
        // 被拒绝时不应分配新 seq，服务端状态没变
        assert_eq!(s.max_seq().unwrap(), 1);
    }

    #[test]
    fn equal_hlc_is_rejected_so_retries_are_idempotent() {
        let s = store();
        s.push(&[msg("m1", 100, 0, "a", "内容")]).unwrap();
        let again = s.push(&[msg("m1", 100, 0, "a", "内容")]).unwrap();
        assert!(!again.results[0].accepted, "重复推送必须被拒，保持幂等");
        assert_eq!(s.max_seq().unwrap(), 1, "重复推送不应推进 seq");
    }

    #[test]
    fn pull_is_incremental_and_ordered_across_entity_kinds() {
        let s = store();
        s.push(&[msg("m1", 100, 0, "a", "一")]).unwrap();
        s.push(&[msg("m2", 200, 0, "a", "二")]).unwrap();
        s.push(&[msg("m3", 300, 0, "a", "三")]).unwrap();

        let first = s.pull(0, 2).unwrap();
        assert_eq!(first.changes.len(), 2);
        assert!(first.has_more, "还有第 3 条没拉");
        assert_eq!(first.cursor, 2);
        // 跨种类也必须严格按 seq 升序，客户端才能靠"后到的赢"正确重放
        assert_eq!(
            first.changes.iter().filter_map(|c| c.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );

        let second = s.pull(first.cursor, 2).unwrap();
        assert_eq!(second.changes.len(), 1);
        assert_eq!(second.changes[0].id, "m3");
        assert!(!second.has_more);

        let none = s.pull(second.cursor, 2).unwrap();
        assert!(none.changes.is_empty());
        assert_eq!(none.cursor, second.cursor, "没有新变更时游标不该乱跳");
    }

    #[test]
    fn deletion_is_a_tombstone_not_a_missing_row() {
        let s = store();
        s.push(&[msg("m1", 100, 0, "a", "要删的")]).unwrap();

        let mut del = msg("m1", 200, 0, "a", "要删的");
        del.deleted = true;
        s.push(&[del]).unwrap();

        let pulled = s.pull(0, 100).unwrap();
        assert_eq!(pulled.changes.len(), 1, "墓碑仍然要能拉到");
        assert!(pulled.changes[0].deleted);
        // 墓碑时间戳取自 payload 的 updatedAt，而不是服务端本地时间，
        // 否则不同客户端拉到的 deleted_at 会不一致
        let p: MessagePayload = decode(&pulled.changes[0]).unwrap();
        assert_eq!(p.updated_at, 200);
    }

    #[test]
    fn push_is_all_or_nothing() {
        let s = store();
        // 第二条故意缺 data，整批必须回滚
        let bad = Change {
            seq: None,
            kind: EntityKind::Message,
            id: "m2".into(),
            hlc: Hlc::new(200, 0, "a"),
            deleted: false,
            data: None,
        };
        let err = s.push(&[msg("m1", 100, 0, "a", "好的"), bad]);
        assert!(err.is_err(), "含非法变更的批次应当整体失败");

        assert_eq!(
            s.pull(0, 100).unwrap().changes.len(),
            0,
            "批次失败后不能留下半截数据"
        );
        assert_eq!(s.max_seq().unwrap(), 0, "回滚后 seq 也不该前进");

        // 检索索引和正文在同一个事务里，所以它同样不能留下半截内容 ——
        // 否则会出现"搜得到一条根本不存在的消息"
        assert!(
            s.search("好的", 10).unwrap().is_empty(),
            "整批回滚时，检索索引里也不能留下那条消息"
        );
    }

    /// 检索索引必须跟着正文走。
    ///
    /// 索引文本（bigram 展开）是 Rust 算出来的，SQL 触发器算不出来，所以
    /// "忘了更新索引"是一个完全可能的退化 —— 而且它**不报错**。轻则搜不到，
    /// 重则搜得到一条正文已经改掉的消息，点开发现内容对不上。
    #[test]
    fn the_search_index_follows_the_body() {
        let s = store();
        // 注意检索是**连续子串**语义（见 core::search），不是分词：
        // 「读了」连续所以命中，「读书」不连续所以不该命中。
        s.push(&[msg("m1", 100, 0, "a", "今天读了点书")]).unwrap();
        assert_eq!(s.search("读了", 10).unwrap().len(), 1, "刚推上来就该能搜到");

        // 改正文
        s.push(&[msg("m1", 200, 0, "a", "今天去爬山了")]).unwrap();
        assert!(
            s.search("读了", 10).unwrap().is_empty(),
            "正文改掉之后旧词不能再搜到 —— 还能搜到就说明索引和数据已经漂移"
        );
        assert_eq!(s.search("爬山", 10).unwrap().len(), 1);

        // 删除
        let mut tomb = msg("m1", 300, 0, "a", "今天去爬山了");
        tomb.deleted = true;
        s.push(&[tomb]).unwrap();
        assert!(
            s.search("爬山", 10).unwrap().is_empty(),
            "墓碑必须从索引里摘掉"
        );
    }

    /// 升级路径：从"还没有索引表"的版本升上来时，已有数据必须回填。
    ///
    /// 不回填的话，服务端建了索引却是空的 —— 表现是"老笔记永远搜不到，
    /// 新写的能搜到"。不报任何错，而且很容易被当成"我没记过这个"。
    #[test]
    fn upgrading_backfills_the_search_index_for_existing_rows() {
        let s = store();
        {
            let conn = s.conn().unwrap();
            // 模拟旧版本的库：有数据，但没有索引表
            conn.execute("DROP TABLE message_fts", []).unwrap();
            conn.execute(
                "INSERT INTO message
                   (id, channel_id, body, created_at, updated_at, hlc_wall, hlc_counter, server_seq)
                 VALUES ('old-1', 'inbox', '升级前就存在的笔记', 50, 50, 50, 0, 1)",
                [],
            )
            .unwrap();
        }

        // 重新建表 + 回填 —— 这就是 `open()` 在 current < 2 时做的事
        {
            let conn = s.conn().unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            backfill_fts(&conn).unwrap();
        }

        let hits = s.search("升级", 10).unwrap();
        assert_eq!(hits.len(), 1, "回填之后，升级前就有的数据也要搜得到");
        assert_eq!(hits[0].message.id, "old-1");
    }
}
