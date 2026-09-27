//! 客户端同步引擎。
//!
//! ## 两条时间轴，各管一件事
//!
//! - **`sync_cursor`（服务端分配的单调整数）** 回答"我上次同步之后，
//!   服务端有哪些变化"。用时间戳做游标的话，任何一台设备时钟偏慢，
//!   它写的数据就会永远落在别的设备游标之前 —— **永久漏拉，且不报错**。
//! - **HLC** 回答"同一行两边都改了，谁赢"。
//!
//! 两者不能混用：拿 seq 判胜负会变成"网络顺序赢"，跨设备不一致；
//! 拿时间戳做游标会漏数据。
//!
//! ## 四条不能违反的规则
//!
//! 1. **删除状态落库时必须由变更内容推导，不能用本机 now()。**
//!    否则两台设备对同一条墓碑会写出不同的 `deleted_at`，状态永远收敛不了。
//! 2. **清 `dirty` 必须带上 HLC 条件。** 推送在途时用户又改了同一行，
//!    无条件清零会把第二次修改误标成"已同步"，那条编辑就永远出不去了。
//! 3. **远端胜出而本地有未上传的改动时，先留副本再覆盖。**
//!    个人记忆工具里，少一条笔记比多一条重复内容严重得多。
//! 4. **网络往返期间绝不持有数据库锁。**
//!    [`sync_once`] 只通过 [`LocalStore`] 接触数据库，而 trait 的每个方法
//!    各自加锁、返回前释放，**网络调用一律夹在两次调用之间** ——
//!    那正是锁被放开的时刻。`sync_once` 拿不到 `&Connection`，
//!    所以这条约束由类型来保证，不靠注释和自觉。
//!
//!    为什么必须这样：连接超时 4 秒、整体兜底 30 秒。一旦把锁的持有期
//!    拉长到整个 HTTP 往返，用户在同步期间打字就会跟着卡住。更要命的是
//!    `std::sync::Mutex` **不可重入** —— 已有调用方习惯先 `db.conn()` 再操作，
//!    在 guard 还活着的时候调进来不是变慢，是**直接死锁**。

use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;

use messagenote_core::hlc::Hlc;
use messagenote_core::merge::{self, LocalState, Resolution};
use messagenote_core::payload::{
    message_tag_key, ChannelPayload, MessagePayload, MessageTagPayload, TagPayload,
};
use messagenote_core::search;
use messagenote_core::wire::{Change, EntityKind, PullResponse, PushResponse};

use crate::db::{self, Db};
use crate::error::{AppError, AppResult};

/// 单次推送/拉取的批量上限。取值只需要"不会让一次 HTTP 请求过大"，
/// 太小会让首次上传变得很慢。
const BATCH: i64 = 400;

const CURSOR_KEY: &str = "sync_cursor";

/// 远端服务的最小接口。
///
/// 抽成 trait 是为了让同步引擎能在一份进程里对着内存假服务端跑
/// **随机化收敛测试** —— 那才是同步 bug 真正藏身的地方，
/// 而它不需要任何网络。
pub trait ServerApi {
    fn pull(&self, since: i64, limit: i64) -> AppResult<PullResponse>;
    fn push(&self, changes: &[Change]) -> AppResult<PushResponse>;
}

/// 一轮同步对本地库提出的全部需求。**每个方法内部各自加锁、返回前释放。**
///
/// 这个 trait 存在的唯一理由，是让"锁的持有期"没法被写长。
/// [`sync_once`] 只认这个 trait、拿不到 `&Connection`，所以网络往返
/// 必然发生在两次调用之间 —— 那正是锁被放开的时刻。
///
/// 谁要是为了省事把 `&Connection` 加回 `sync_once` 的参数表，锁的持有期
/// 立刻从毫秒级变成整个 HTTP 往返（连接超时 4 秒、整体兜底 30 秒）。
/// 更糟的是 `std::sync::Mutex` **不可重入**：调用方习惯先 `db.conn()`
/// 再操作，在一个还活着的 guard 上再调进来就是直接死锁。
/// 这种约束必须由类型来保证，注释拦不住。
pub trait LocalStore {
    /// 阶段一：取出待上传的一批变更（加锁 → 读 → 释放）
    fn take_pending_batch(&self, limit: i64) -> AppResult<Vec<Change>>;

    /// 阶段三：落地推送结果（加锁 → 写 → 释放）。返回新建的冲突副本数。
    ///
    /// "应用落败者"和"清 dirty"必须在**同一次加锁**里做完：
    /// 中间若放开，另一条路径的写入会让 `clear_dirty` 基于过期的状态判断。
    fn commit_push_result(&self, sent: &[Change], resp: &PushResponse) -> AppResult<usize>;

    /// 当前拉取游标（加锁 → 读 → 释放）
    fn current_cursor(&self) -> AppResult<i64>;

    /// 阶段三：落地一批拉取结果并推进游标（加锁 → 写 → 释放）。
    ///
    /// 落库和推游标放在同一次加锁里。崩在中间的话，下次会重拉同一批，
    /// 靠 HLC 相等那条幂等路径兜住 —— 安全，但没必要留这个窗口。
    fn commit_pull_batch(&self, changes: &[Change], new_cursor: i64) -> AppResult<ApplyCount>;
}

/// 一批远端变更落库后的统计。
#[derive(Debug, Default, Clone, Copy)]
pub struct ApplyCount {
    /// 真正写进本地库的条数（HLC 判负、保持本地不动的那些不算）
    pub applied: usize,
    /// 为保住本地未上传的编辑而新建的冲突副本数量
    pub conflicts: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    pub pushed: usize,
    pub pulled: usize,
    /// 本轮为了保命而新建的冲突副本数量
    pub conflicts: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct Applied {
    applied: bool,
    conflict_copy: bool,
}

// ---------------------------------------------------------------- 游标

pub fn cursor(conn: &Connection) -> AppResult<i64> {
    let v = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![CURSOR_KEY],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
}

fn set_cursor(conn: &Connection, value: i64) -> AppResult<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![CURSOR_KEY, value.to_string()],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- 本地状态

struct LocalRow {
    hlc: Hlc,
    dirty: bool,
    deleted: bool,
    /// 内容指纹，用来判断"本地和远端是不是同一份内容"。
    /// 实体各自的判定字段不同（消息比正文+频道，频道比名字+排序……）。
    fingerprint: String,
}

fn local_row(conn: &Connection, kind: EntityKind, id: &str) -> AppResult<Option<LocalRow>> {
    let row = match kind {
        EntityKind::Channel => conn
            .query_row(
                "SELECT hlc_wall, hlc_counter, device_id, dirty, deleted_at, name, kind, sort_order
                   FROM channel WHERE id = ?1",
                params![id],
                |r| {
                    Ok(LocalRow {
                        hlc: Hlc::new(
                            r.get::<_, i64>(0)?,
                            r.get::<_, u32>(1)?,
                            r.get::<_, String>(2)?,
                        ),
                        dirty: r.get::<_, i64>(3)? != 0,
                        deleted: r.get::<_, Option<i64>>(4)?.is_some(),
                        fingerprint: format!(
                            "{}\u{1}{}\u{1}{}",
                            r.get::<_, String>(5)?,
                            r.get::<_, String>(6)?,
                            r.get::<_, i64>(7)?
                        ),
                    })
                },
            )
            .optional()?,

        EntityKind::Message => conn
            .query_row(
                "SELECT hlc_wall, hlc_counter, device_id, dirty, deleted_at, body, channel_id
                   FROM message WHERE id = ?1",
                params![id],
                |r| {
                    Ok(LocalRow {
                        hlc: Hlc::new(
                            r.get::<_, i64>(0)?,
                            r.get::<_, u32>(1)?,
                            r.get::<_, String>(2)?,
                        ),
                        dirty: r.get::<_, i64>(3)? != 0,
                        deleted: r.get::<_, Option<i64>>(4)?.is_some(),
                        fingerprint: format!(
                            "{}\u{1}{}",
                            r.get::<_, String>(5)?,
                            r.get::<_, String>(6)?
                        ),
                    })
                },
            )
            .optional()?,

        // tag 的身份就是名字，message_tag 的身份是那一对 —— 两者都没有
        // "名字之外的正文"，所以指纹留空：差异只可能来自删除状态。
        EntityKind::Tag => conn
            .query_row(
                "SELECT hlc_wall, hlc_counter, device_id, dirty, deleted_at
                   FROM tag WHERE name = ?1",
                params![id],
                |r| {
                    Ok(LocalRow {
                        hlc: Hlc::new(
                            r.get::<_, i64>(0)?,
                            r.get::<_, u32>(1)?,
                            r.get::<_, String>(2)?,
                        ),
                        dirty: r.get::<_, i64>(3)? != 0,
                        deleted: r.get::<_, Option<i64>>(4)?.is_some(),
                        fingerprint: String::new(),
                    })
                },
            )
            .optional()?,

        EntityKind::MessageTag => {
            let Some((mid, tname)) = message_tag_key::decode(id) else {
                return Ok(None);
            };
            conn.query_row(
                "SELECT hlc_wall, hlc_counter, device_id, dirty, deleted_at
                   FROM message_tag WHERE message_id = ?1 AND tag_name = ?2",
                params![mid, tname],
                |r| {
                    Ok(LocalRow {
                        hlc: Hlc::new(
                            r.get::<_, i64>(0)?,
                            r.get::<_, u32>(1)?,
                            r.get::<_, String>(2)?,
                        ),
                        dirty: r.get::<_, i64>(3)? != 0,
                        deleted: r.get::<_, Option<i64>>(4)?.is_some(),
                        fingerprint: String::new(),
                    })
                },
            )
            .optional()?
        }
    };
    Ok(row)
}

fn decode_payload<T: DeserializeOwned>(change: &Change) -> AppResult<T> {
    let data = change.data.clone().ok_or_else(|| {
        AppError::Msg(format!(
            "变更缺少 data：{} {}",
            change.kind.as_str(),
            change.id
        ))
    })?;
    serde_json::from_value(data)
        .map_err(|e| AppError::Msg(format!("变更 data 解析失败（{}）：{e}", change.id)))
}

fn fingerprint_of(change: &Change) -> AppResult<String> {
    Ok(match change.kind {
        EntityKind::Channel => {
            let p: ChannelPayload = decode_payload(change)?;
            format!("{}\u{1}{}\u{1}{}", p.name, p.kind, p.sort_order)
        }
        EntityKind::Message => {
            let p: MessagePayload = decode_payload(change)?;
            format!("{}\u{1}{}", p.body, p.channel_id)
        }
        EntityKind::Tag | EntityKind::MessageTag => String::new(),
    })
}

/// 远端这份变更，和本地这一行，算不算"内容不同"。
fn content_differs(change: &Change, local: &LocalRow) -> AppResult<bool> {
    // 删除状态本身的变化就算内容不同：远端删掉了，而本地有未上传的编辑，
    // 那次编辑必须留副本 —— 否则一次删除会把用户的修改静默吞掉。
    if change.deleted != local.deleted {
        return Ok(true);
    }
    if change.deleted {
        // 两边都是墓碑，正文是什么已经无所谓了
        return Ok(false);
    }
    Ok(local.fingerprint != fingerprint_of(change)?)
}

// ---------------------------------------------------------------- 收集待上传

/// 收集本地待上传的变更。
///
/// 按依赖顺序返回（channel → message → tag → message_tag）：服务端按接受顺序
/// 分配 seq，别的客户端就会按同样的顺序重放，于是"关联先于它引用的标签到达"
/// 这类乱序不会出现。
///
/// **四个种类共享同一个预算，并且按依赖顺序消耗。** 不能让它们各自独立取
/// `limit` 条 —— 那样一个批次里就可能出现"引用了本批未包含的频道的消息"。
/// 接收端的 `message` 表有指向 `channel` 的外键，那条消息会插入失败、整批回滚，
/// 而且**此后每次同步都会重演，变成永久性故障**。
/// 宁可这批少推几个种类，也不能推一条引用不全的记录。
pub fn pending_changes(conn: &Connection, limit: i64) -> AppResult<Vec<Change>> {
    let mut out: Vec<Change> = Vec::new();

    // 收件箱的 dirty 恒为 0，所以不需要在这里特判排除它
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, sort_order, created_at, updated_at,
                hlc_wall, hlc_counter, device_id, deleted_at
           FROM channel WHERE dirty = 1 ORDER BY hlc_wall, hlc_counter LIMIT ?1",
    )?;
    for r in stmt.query_map(params![limit], |r| {
        Ok(Change {
            seq: None,
            kind: EntityKind::Channel,
            id: r.get::<_, String>(0)?,
            hlc: Hlc::new(
                r.get::<_, i64>(6)?,
                r.get::<_, u32>(7)?,
                r.get::<_, String>(8)?,
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
        })
    })? {
        out.push(r?);
    }

    let remaining = limit - out.len() as i64;
    if remaining <= 0 {
        return Ok(out);
    }
    let mut stmt = conn.prepare(
        "SELECT id, channel_id, body, created_at, updated_at,
                hlc_wall, hlc_counter, device_id, deleted_at
           FROM message WHERE dirty = 1 ORDER BY hlc_wall, hlc_counter LIMIT ?1",
    )?;
    for r in stmt.query_map(params![remaining], |r| {
        Ok(Change {
            seq: None,
            kind: EntityKind::Message,
            id: r.get::<_, String>(0)?,
            hlc: Hlc::new(
                r.get::<_, i64>(5)?,
                r.get::<_, u32>(6)?,
                r.get::<_, String>(7)?,
            ),
            deleted: r.get::<_, Option<i64>>(8)?.is_some(),
            data: serde_json::to_value(MessagePayload {
                channel_id: r.get(1)?,
                body: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
            .ok(),
        })
    })? {
        out.push(r?);
    }

    let remaining = limit - out.len() as i64;
    if remaining <= 0 {
        return Ok(out);
    }
    let mut stmt = conn.prepare(
        "SELECT name, created_at, updated_at, hlc_wall, hlc_counter, device_id, deleted_at
           FROM tag WHERE dirty = 1 ORDER BY hlc_wall, hlc_counter LIMIT ?1",
    )?;
    for r in stmt.query_map(params![remaining], |r| {
        Ok(Change {
            seq: None,
            kind: EntityKind::Tag,
            id: r.get::<_, String>(0)?,
            hlc: Hlc::new(
                r.get::<_, i64>(3)?,
                r.get::<_, u32>(4)?,
                r.get::<_, String>(5)?,
            ),
            deleted: r.get::<_, Option<i64>>(6)?.is_some(),
            data: serde_json::to_value(TagPayload {
                created_at: r.get(1)?,
                updated_at: r.get(2)?,
            })
            .ok(),
        })
    })? {
        out.push(r?);
    }

    let remaining = limit - out.len() as i64;
    if remaining <= 0 {
        return Ok(out);
    }
    let mut stmt = conn.prepare(
        "SELECT message_id, tag_name, created_at, updated_at,
                hlc_wall, hlc_counter, device_id, deleted_at
           FROM message_tag WHERE dirty = 1 ORDER BY hlc_wall, hlc_counter LIMIT ?1",
    )?;
    for r in stmt.query_map(params![remaining], |r| {
        let message_id: String = r.get(0)?;
        let tag_name: String = r.get(1)?;
        Ok(Change {
            seq: None,
            kind: EntityKind::MessageTag,
            id: message_tag_key::encode(&message_id, &tag_name),
            hlc: Hlc::new(
                r.get::<_, i64>(4)?,
                r.get::<_, u32>(5)?,
                r.get::<_, String>(6)?,
            ),
            deleted: r.get::<_, Option<i64>>(7)?.is_some(),
            data: serde_json::to_value(MessageTagPayload {
                message_id,
                tag_name,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
            })
            .ok(),
        })
    })? {
        out.push(r?);
    }

    Ok(out)
}

/// 推送成功后清除 dirty。
///
/// **必须带上 HLC 条件。** 推送在途时用户完全可能又改了同一行，
/// 无条件清零会把那次修改误标成"已同步"，它再也不会被推送出去 ——
/// 表现为"这条笔记在另一台设备上永远是旧的"，而且不报任何错。
fn clear_dirty(conn: &Connection, sent: &[Change]) -> AppResult<()> {
    for c in sent {
        let h = &c.hlc;
        match c.kind {
            EntityKind::Channel => conn.execute(
                "UPDATE channel SET dirty = 0
                  WHERE id = ?1 AND hlc_wall = ?2 AND hlc_counter = ?3 AND device_id = ?4",
                params![c.id, h.wall, h.counter, h.device],
            )?,
            EntityKind::Message => conn.execute(
                "UPDATE message SET dirty = 0
                  WHERE id = ?1 AND hlc_wall = ?2 AND hlc_counter = ?3 AND device_id = ?4",
                params![c.id, h.wall, h.counter, h.device],
            )?,
            EntityKind::Tag => conn.execute(
                "UPDATE tag SET dirty = 0
                  WHERE name = ?1 AND hlc_wall = ?2 AND hlc_counter = ?3 AND device_id = ?4",
                params![c.id, h.wall, h.counter, h.device],
            )?,
            EntityKind::MessageTag => {
                let Some((mid, tname)) = message_tag_key::decode(&c.id) else {
                    continue;
                };
                conn.execute(
                    "UPDATE message_tag SET dirty = 0
                      WHERE message_id = ?1 AND tag_name = ?2
                        AND hlc_wall = ?3 AND hlc_counter = ?4 AND device_id = ?5",
                    params![mid, tname, h.wall, h.counter, h.device],
                )?
            }
        };
    }
    Ok(())
}

// ---------------------------------------------------------------- 应用远端变更

fn apply_one(conn: &Connection, change: &Change) -> AppResult<Applied> {
    // 先把本机时钟推到远端之后。漏了这一步，本机后续写入会产出比远端
    // 更旧的时间戳，于是自己的修改在冲突里持续判负。
    db::clock_observe(conn, &change.hlc)?;

    let local = local_row(conn, change.kind, &change.id)?;

    let differs = match &local {
        None => true,
        Some(l) => content_differs(change, l)?,
    };

    let resolution = merge::resolve(
        &change.hlc,
        local.as_ref().map(|l| LocalState {
            hlc: &l.hlc,
            dirty: l.dirty,
            differs,
        }),
    );

    let mut outcome = Applied::default();
    match resolution {
        Resolution::KeepLocal => {}
        Resolution::ApplyRemote => {
            write_remote(conn, change)?;
            outcome.applied = true;
        }
        Resolution::ApplyRemoteKeepCopy => {
            outcome.conflict_copy = keep_conflict_copy(conn, change, local.as_ref())?;
            write_remote(conn, change)?;
            outcome.applied = true;
        }
    }
    Ok(outcome)
}

/// 远端胜出、但本地那一版从未被服务端见过且内容不同 —— 先把它救下来。
///
/// 只有消息承载"用户写下的内容"。频道改名、标签增删的落败不算数据丢失，
/// 强行留副本只会制造噪音。
fn keep_conflict_copy(
    conn: &Connection,
    change: &Change,
    local: Option<&LocalRow>,
) -> AppResult<bool> {
    if change.kind != EntityKind::Message {
        return Ok(false);
    }
    let Some(local) = local else {
        return Ok(false);
    };
    if local.deleted {
        // 本地已经是墓碑，没有正文可救
        return Ok(false);
    }

    let (body, channel_id): (String, String) = conn.query_row(
        "SELECT body, channel_id FROM message WHERE id = ?1",
        params![change.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    // 副本走**正常的本地写入路径**：它会拿到自己的 id 和 HLC，
    // 并在下一轮同步里被推送到服务端。不需要为它单开一套特殊逻辑。
    let copy = db::append_message(conn, &merge::conflict_body(&body), Some(&channel_id))?;
    db::set_message_tags(conn, &copy.id, &[merge::CONFLICT_TAG.to_string()])?;
    Ok(true)
}

/// 把远端变更写进本地。
///
/// **所有时间字段都取自变更本身，不用本机 now()。** 尤其是墓碑的
/// `deleted_at`：如果两台设备各自用本地时间，同一条墓碑的 `deleted_at`
/// 会不同，两边状态永远收敛不了 —— 而且不会有任何报错。
fn write_remote(conn: &Connection, change: &Change) -> AppResult<()> {
    let h = &change.hlc;

    match change.kind {
        EntityKind::Channel => {
            let p: ChannelPayload = decode_payload(change)?;
            conn.execute(
                "INSERT INTO channel
                   (id, name, kind, sort_order, created_at, updated_at, device_id, hlc_wall, hlc_counter, deleted_at, dirty)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name, kind = excluded.kind, sort_order = excluded.sort_order,
                   created_at = excluded.created_at,
                   updated_at = excluded.updated_at, device_id = excluded.device_id,
                   hlc_wall = excluded.hlc_wall, hlc_counter = excluded.hlc_counter,
                   deleted_at = excluded.deleted_at, dirty = 0",
                params![
                    change.id, p.name, p.kind, p.sort_order, p.created_at, p.updated_at,
                    h.device, h.wall, h.counter,
                    if change.deleted { Some(p.updated_at) } else { None }
                ],
            )?;
        }

        EntityKind::Message => {
            let p: MessagePayload = decode_payload(change)?;
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO message
                   (id, channel_id, body, created_at, updated_at, device_id, hlc_wall, hlc_counter, deleted_at, dirty)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,0)
                 ON CONFLICT(id) DO UPDATE SET
                   channel_id = excluded.channel_id, body = excluded.body,
                   created_at = excluded.created_at,
                   updated_at = excluded.updated_at, device_id = excluded.device_id,
                   hlc_wall = excluded.hlc_wall, hlc_counter = excluded.hlc_counter,
                   deleted_at = excluded.deleted_at, dirty = 0",
                params![
                    change.id, p.channel_id, p.body, p.created_at, p.updated_at,
                    h.device, h.wall, h.counter,
                    if change.deleted { Some(p.updated_at) } else { None }
                ],
            )?;
            // 检索索引是本地派生数据，永远不参与同步，但必须跟着正文更新
            tx.execute(
                "DELETE FROM message_fts WHERE message_id = ?1",
                params![change.id],
            )?;
            if !change.deleted {
                tx.execute(
                    "INSERT INTO message_fts (search_text, message_id) VALUES (?1, ?2)",
                    params![search::to_index_text(&p.body), change.id],
                )?;
            }
            tx.commit()?;
        }

        EntityKind::Tag => {
            let p: TagPayload = decode_payload(change)?;
            conn.execute(
                "INSERT INTO tag
                   (name, created_at, updated_at, device_id, hlc_wall, hlc_counter, deleted_at, dirty)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,0)
                 ON CONFLICT(name) DO UPDATE SET
                   created_at = excluded.created_at,
                   updated_at = excluded.updated_at, device_id = excluded.device_id,
                   hlc_wall = excluded.hlc_wall, hlc_counter = excluded.hlc_counter,
                   deleted_at = excluded.deleted_at, dirty = 0",
                params![
                    change.id, p.created_at, p.updated_at, h.device, h.wall, h.counter,
                    if change.deleted { Some(p.updated_at) } else { None }
                ],
            )?;
        }

        EntityKind::MessageTag => {
            let p: MessageTagPayload = decode_payload(change)?;
            conn.execute(
                "INSERT INTO message_tag
                   (message_id, tag_name, created_at, updated_at, device_id, hlc_wall, hlc_counter, deleted_at, dirty)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0)
                 ON CONFLICT(message_id, tag_name) DO UPDATE SET
                   created_at = excluded.created_at,
                   updated_at = excluded.updated_at, device_id = excluded.device_id,
                   hlc_wall = excluded.hlc_wall, hlc_counter = excluded.hlc_counter,
                   deleted_at = excluded.deleted_at, dirty = 0",
                params![
                    p.message_id, p.tag_name, p.created_at, p.updated_at,
                    h.device, h.wall, h.counter,
                    if change.deleted { Some(p.updated_at) } else { None }
                ],
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- 一轮同步

/// `Db` 是**唯一**的 `LocalStore` 实现。
///
/// 刻意不为 `Connection` 实现它：那样调用方就能一边握着 guard 一边同步，
/// 把"网络期间持锁"重新引回来 —— 而且是死锁，不是变慢。
impl LocalStore for Db {
    fn take_pending_batch(&self, limit: i64) -> AppResult<Vec<Change>> {
        let conn = self.conn()?;
        pending_changes(&conn, limit)
    }

    fn commit_push_result(&self, sent: &[Change], resp: &PushResponse) -> AppResult<usize> {
        let conn = self.conn()?;
        let mut conflicts = 0;

        // 被判负的变更：服务端把胜出的版本回传给我们，落下来。
        // 注意这里走 apply_one 而不是直接覆盖 —— 本地那一版如果从未上传过，
        // 会先被存成冲突副本。
        for outcome in &resp.results {
            if outcome.accepted {
                continue;
            }
            if let Some(winner) = &outcome.winner {
                if apply_one(&conn, winner)?.conflict_copy {
                    conflicts += 1;
                }
            }
        }

        clear_dirty(&conn, sent)?;
        Ok(conflicts)
    }

    fn current_cursor(&self) -> AppResult<i64> {
        let conn = self.conn()?;
        cursor(&conn)
    }

    fn commit_pull_batch(&self, changes: &[Change], new_cursor: i64) -> AppResult<ApplyCount> {
        let conn = self.conn()?;
        let mut out = ApplyCount::default();

        for c in changes {
            let a = apply_one(&conn, c)?;
            if a.applied {
                out.applied += 1;
            }
            if a.conflict_copy {
                out.conflicts += 1;
            }
        }

        // 即使一条都没应用也要推进游标：那些变更确实已经"看过了"，
        // 反复拉同一批只会原地打转。
        if new_cursor > cursor(&conn)? {
            set_cursor(&conn, new_cursor)?;
        }

        Ok(out)
    }
}

/// 推一次、拉干净。返回本轮统计。
///
/// **网络调用一律发生在两次 `LocalStore` 调用之间** —— 那是锁被放开的时刻。
/// 这条性质由 `the_database_is_not_locked_while_the_network_is_busy` 守着。
pub fn sync_once(store: &dyn LocalStore, api: &dyn ServerApi) -> AppResult<SyncReport> {
    let mut report = SyncReport::default();

    // ---- 推 ----
    let outgoing = store.take_pending_batch(BATCH)?; // 锁：读 ─┐
    if !outgoing.is_empty() {
        report.pushed = outgoing.len();
        let resp = api.push(&outgoing)?; // 无锁 ◄──────────────┘
        report.conflicts += store.commit_push_result(&outgoing, &resp)?; // 锁：写
    }

    // ---- 拉 ----
    let mut since = store.current_cursor()?; // 锁：读
    loop {
        let resp = api.pull(since, BATCH)?; // 无锁 ◄────────────┐
        let a = store.commit_pull_batch(&resp.changes, resp.cursor)?; // 锁：写 ─┘
        report.pulled += a.applied;
        report.conflicts += a.conflicts;

        since = since.max(resp.cursor);

        if resp.changes.is_empty() || !resp.has_more {
            break;
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use messagenote_core::wire::PushOutcome;

    use crate::db::{self, Db};

    // ------------------------------------------------ 内存假服务端

    /// 服务端的内存等价物。
    ///
    /// 它刻意复用 `core::merge::should_accept_push` —— 真实的 axum 服务端
    /// 也必须用同一个函数。如果两边各写一套"谁更新"的判断，
    /// 会各自认为自己的版本胜出，最终**收敛不到同一个状态且不报错**。
    struct MemoryServer {
        inner: Mutex<ServerState>,
    }

    #[derive(Default)]
    struct ServerState {
        seq: i64,
        rows: HashMap<String, (i64, Change)>,
    }

    impl MemoryServer {
        fn new() -> Self {
            Self {
                inner: Mutex::new(ServerState::default()),
            }
        }

        fn key(kind: EntityKind, id: &str) -> String {
            format!("{}:{}", kind.as_str(), id)
        }
    }

    impl ServerApi for MemoryServer {
        fn push(&self, changes: &[Change]) -> AppResult<PushResponse> {
            let mut st = self.inner.lock().unwrap();
            let mut results = Vec::with_capacity(changes.len());

            for c in changes {
                let k = Self::key(c.kind, &c.id);
                let stored_hlc = st.rows.get(&k).map(|(_, ch)| ch.hlc.clone());

                if merge::should_accept_push(&c.hlc, stored_hlc.as_ref()) {
                    st.seq += 1;
                    let mut stored = c.clone();
                    stored.seq = Some(st.seq);
                    let seq = st.seq;
                    st.rows.insert(k, (seq, stored));
                    results.push(PushOutcome {
                        kind: c.kind,
                        id: c.id.clone(),
                        accepted: true,
                        winner: None,
                    });
                } else {
                    let winner = st.rows.get(&k).map(|(seq, ch)| {
                        let mut w = ch.clone();
                        w.seq = Some(*seq);
                        w
                    });
                    results.push(PushOutcome {
                        kind: c.kind,
                        id: c.id.clone(),
                        accepted: false,
                        winner,
                    });
                }
            }
            Ok(PushResponse { results })
        }

        fn pull(&self, since: i64, limit: i64) -> AppResult<PullResponse> {
            let st = self.inner.lock().unwrap();
            let mut all: Vec<&(i64, Change)> =
                st.rows.values().filter(|(seq, _)| *seq > since).collect();
            all.sort_by_key(|(seq, _)| *seq);

            let has_more = all.len() as i64 > limit;
            let changes: Vec<Change> = all
                .into_iter()
                .take(limit as usize)
                .map(|(_, c)| c.clone())
                .collect();
            let cursor = changes.iter().filter_map(|c| c.seq).max().unwrap_or(since);

            Ok(PullResponse {
                changes,
                cursor,
                has_more,
            })
        }
    }

    // ------------------------------------------------ 测试工具

    fn device(name: &str) -> Db {
        db::open_memory(name).expect("建内存库")
    }

    /// 反复同步直到双方都不再有变化。收敛测试必须先跑到静止。
    ///
    /// 注意这里**不持有 `db.conn()`** —— `sync_once` 自己按需加锁。
    /// 握着 guard 调它是死锁，不是变慢。
    fn sync_until_quiet(db: &Db, server: &dyn ServerApi) -> SyncReport {
        let mut total = SyncReport::default();
        for _ in 0..40 {
            let r = sync_once(db, server).expect("同步失败");
            total.pushed += r.pushed;
            total.pulled += r.pulled;
            total.conflicts += r.conflicts;
            if r.pushed == 0 && r.pulled == 0 {
                break;
            }
        }
        total
    }

    /// 把整个库dump成一组规范化字符串，用来断言两台设备状态完全一致。
    ///
    /// 刻意包含 `deleted_at` 和 HLC：只比"可见内容"的话，
    /// 墓碑时间和时钟状态的发散会被漏掉。
    fn snapshot(conn: &Connection) -> Vec<String> {
        let mut out = Vec::new();

        let mut s = conn
            .prepare(
                "SELECT id, name, kind, sort_order, created_at, updated_at, deleted_at,
                        hlc_wall, hlc_counter, device_id, dirty
                   FROM channel ORDER BY id",
            )
            .unwrap();
        for r in s
            .query_map([], |r| {
                Ok(format!(
                    "channel|{}|{}|{}|{}|{}|{}|{:?}|{}|{}|{}|{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, i64>(10)?,
                ))
            })
            .unwrap()
        {
            out.push(r.unwrap());
        }

        let mut s = conn
            .prepare(
                "SELECT id, channel_id, body, created_at, updated_at, deleted_at,
                        hlc_wall, hlc_counter, device_id, dirty
                   FROM message ORDER BY id",
            )
            .unwrap();
        for r in s
            .query_map([], |r| {
                Ok(format!(
                    "message|{}|{}|{}|{}|{}|{:?}|{}|{}|{}|{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, i64>(9)?,
                ))
            })
            .unwrap()
        {
            out.push(r.unwrap());
        }

        let mut s = conn
            .prepare(
                "SELECT name, created_at, updated_at, deleted_at, hlc_wall, hlc_counter, device_id, dirty
                   FROM tag ORDER BY name",
            )
            .unwrap();
        for r in s
            .query_map([], |r| {
                Ok(format!(
                    "tag|{}|{}|{}|{:?}|{}|{}|{}|{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            })
            .unwrap()
        {
            out.push(r.unwrap());
        }

        let mut s = conn
            .prepare(
                "SELECT message_id, tag_name, created_at, updated_at, deleted_at,
                        hlc_wall, hlc_counter, device_id, dirty
                   FROM message_tag ORDER BY message_id, tag_name",
            )
            .unwrap();
        for r in s
            .query_map([], |r| {
                Ok(format!(
                    "message_tag|{}|{}|{}|{}|{:?}|{}|{}|{}|{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                ))
            })
            .unwrap()
        {
            out.push(r.unwrap());
        }

        out.sort();
        out
    }

    /// 可复现的线性同余伪随机数。不引第三方 rand：测试要的是**可复现**，
    /// 失败时能靠同一个种子原样重跑。
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                0
            } else {
                (self.next() as usize) % n
            }
        }
    }

    fn live_message_ids(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT id FROM message WHERE deleted_at IS NULL")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    // ------------------------------------------------ 基础行为

    #[test]
    fn one_device_push_then_other_pulls_everything() {
        let server = MemoryServer::new();
        let a = device("device-a");
        let b = device("device-b");

        {
            let conn = a.conn().unwrap();
            let m = db::append_message(&conn, "第一条笔记", None).unwrap();
            db::set_message_tags(&conn, &m.id, &["工作".into()]).unwrap();
            let ch = db::create_channel(&conn, "项目").unwrap();
            db::append_message(&conn, "项目里的一条", Some(&ch.id)).unwrap();
        }

        sync_until_quiet(&a, &server);
        sync_until_quiet(&b, &server);

        let snap_a = snapshot(&a.conn().unwrap());
        let snap_b = snapshot(&b.conn().unwrap());
        assert_eq!(snap_a, snap_b, "两台设备在同步后必须完全一致");
        assert!(
            snap_a.iter().any(|s| s.starts_with("message|")),
            "测试不能空跑"
        );
        assert!(
            snap_a.iter().any(|s| s.starts_with("tag|")),
            "标签也要同步过去"
        );

        // B 上应该真的能搜到中文
        let conn = b.conn().unwrap();
        assert_eq!(
            db::search_page(&conn, "笔记", 10, 0).unwrap().items.len(),
            1
        );
        assert_eq!(
            db::search_page(&conn, "项目", 10, 0).unwrap().items.len(),
            1
        );
    }

    /// 待上传批次必须按依赖顺序消耗预算。
    ///
    /// 如果四个种类各自独立取 `limit` 条，一个批次里就会出现"引用了本批未包含的
    /// 频道的消息"。接收端的 `message` 表有指向 `channel` 的外键，那条消息插入
    /// 失败会让整批回滚 —— 而且此后每次同步都会重演，**永久性故障**。
    #[test]
    fn pending_batch_never_includes_a_message_without_its_channel() {
        let db = device("device-budget");
        let conn = db.conn().unwrap();

        // 5 个频道，每个里面一条消息
        for i in 0..5 {
            let ch = db::create_channel(&conn, &format!("频道{i}")).unwrap();
            db::append_message(&conn, &format!("记录{i}"), Some(&ch.id)).unwrap();
        }

        // 预算只够装 2 条：必须全被频道吃掉，不能混进消息
        let small = pending_changes(&conn, 2).unwrap();
        let kinds: Vec<EntityKind> = small.iter().map(|c| c.kind).collect();
        assert_eq!(small.len(), 2, "预算用完就停");
        assert!(
            kinds.iter().all(|k| *k == EntityKind::Channel),
            "预算被频道吃完后，批次里不该出现任何消息（它们引用的频道没带上）：{kinds:?}"
        );

        // 预算充足时，频道必须整体排在消息之前
        let full = pending_changes(&conn, 100).unwrap();
        let first_message = full
            .iter()
            .position(|c| c.kind == EntityKind::Message)
            .expect("应当有消息");
        let last_channel = full
            .iter()
            .rposition(|c| c.kind == EntityKind::Channel)
            .expect("应当有频道");
        assert!(
            first_message > last_channel,
            "同一个批次里，频道必须全部排在消息之前"
        );
    }

    /// 同一条变更重复推送必须是无操作。同步的重试、断线重连都会走到这里。
    #[test]
    fn pushing_the_same_change_twice_is_idempotent() {
        let server = MemoryServer::new();
        let a = device("device-a");
        {
            let conn = a.conn().unwrap();
            db::append_message(&conn, "只此一条", None).unwrap();
        }

        let first = sync_once(&a, &server).unwrap();
        assert!(first.pushed > 0);

        // 强行把 dirty 再置起来，模拟"网络超时后客户端重试"
        {
            let conn = a.conn().unwrap();
            conn.execute("UPDATE message SET dirty = 1", []).unwrap();
        }
        let second = sync_once(&a, &server).unwrap();

        let snap = snapshot(&a.conn().unwrap());
        assert_eq!(
            snap.iter().filter(|s| s.starts_with("message|")).count(),
            1,
            "重复推送不能产生第二条消息"
        );
        assert!(second.conflicts == 0, "HLC 相等不该被当成冲突");
    }

    #[test]
    fn long_offline_device_catches_up() {
        let server = MemoryServer::new();
        let online = device("device-online");
        let offline = device("device-offline");

        // 离线设备先攒一点本地内容
        {
            let conn = offline.conn().unwrap();
            db::append_message(&conn, "离线期间写的第一条", None).unwrap();
            db::append_message(&conn, "离线期间写的第二条", None).unwrap();
        }

        // 在线设备在此期间同步了很多次
        for i in 0..30 {
            {
                let conn = online.conn().unwrap();
                db::append_message(&conn, &format!("在线记录 {i}"), None).unwrap();
            }
            sync_until_quiet(&online, &server);
        }

        // 离线设备重连
        sync_until_quiet(&offline, &server);
        sync_until_quiet(&online, &server);

        assert_eq!(
            snapshot(&online.conn().unwrap()),
            snapshot(&offline.conn().unwrap()),
            "长时间离线后重连也必须收敛"
        );

        let conn = offline.conn().unwrap();
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 32, "离线期间的本地内容和在线期间的远端内容都要在");
    }

    /// 本地编辑**从未到达服务端**时，绝不能被远端静默覆盖 —— 必须留下副本。
    ///
    /// 构造方式：A 先改、B 后改（中间 sleep 保证 B 的 HLC 严格更大），
    /// 然后**让 B 先同步**。于是服务端已经持有 B 的版本，A 的推送会被拒绝 ——
    /// A 那一版从未被持久化到任何地方，丢了就是真丢了。
    #[test]
    fn a_rejected_local_edit_is_preserved_as_a_conflict_copy() {
        let server = MemoryServer::new();
        let a = device("device-a");
        let b = device("device-b");

        {
            let conn = a.conn().unwrap();
            db::append_message(&conn, "原始内容", None).unwrap();
        }
        sync_until_quiet(&a, &server);
        sync_until_quiet(&b, &server);

        let id = {
            let conn = b.conn().unwrap();
            live_message_ids(&conn)
                .into_iter()
                .next()
                .expect("B 上应该有这条消息")
        };

        // A 先改，B 后改
        {
            let conn = a.conn().unwrap();
            db::update_message(&conn, &id, "A 的版本").unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
        {
            let conn = b.conn().unwrap();
            db::update_message(&conn, &id, "B 的版本").unwrap();
        }

        // B 先同步：服务端从此持有 B 的版本
        sync_until_quiet(&b, &server);

        // A 再同步：推送被拒，本地那一版必须被救下来
        let report = sync_until_quiet(&a, &server);
        assert!(
            report.conflicts >= 1,
            "被服务端拒绝的本地编辑必须产生冲突副本，否则那次编辑就彻底消失了"
        );

        // 收敛性不能因为冲突而破掉
        sync_until_quiet(&a, &server);
        sync_until_quiet(&b, &server);
        sync_until_quiet(&a, &server);
        assert_eq!(
            snapshot(&a.conn().unwrap()),
            snapshot(&b.conn().unwrap()),
            "发生冲突后两台设备仍须收敛"
        );

        let conn = a.conn().unwrap();
        let bodies: Vec<String> = conn
            .prepare("SELECT body FROM message WHERE deleted_at IS NULL ORDER BY body")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let joined = bodies.join("\n---\n");

        assert!(
            bodies.iter().any(|x| x.contains("A 的版本")),
            "A 那一版被静默丢弃了：\n{joined}"
        );
        assert!(
            bodies.iter().any(|x| x.contains("B 的版本")),
            "B 那一版不见了：\n{joined}"
        );
        assert!(
            bodies.iter().any(|x| x.contains("冲突副本")),
            "副本必须带醒目标记，否则用户会以为是自己重复写了：\n{joined}"
        );
    }

    /// 有意固化的边界：**已经被服务端接受过**的版本，之后被更新的版本覆盖时
    /// **不留副本**。
    ///
    /// 那是 LWW 的正常语义 —— 和你在一台设备上先后改两次没有本质区别。
    /// 每次覆盖都留副本只会把库变成噪音场，反而让真正的冲突副本淹没在里头。
    ///
    /// 我们保证的是：**从未到达服务端的编辑绝不丢失**（见上一个测试）。
    #[test]
    fn an_accepted_then_superseded_edit_is_not_copied() {
        let server = MemoryServer::new();
        let a = device("device-a");
        let b = device("device-b");

        {
            let conn = a.conn().unwrap();
            db::append_message(&conn, "原始内容", None).unwrap();
        }
        sync_until_quiet(&a, &server);
        sync_until_quiet(&b, &server);

        let id = {
            let conn = b.conn().unwrap();
            live_message_ids(&conn).into_iter().next().unwrap()
        };

        // A 改成 A 版，并且**成功同步出去**
        {
            let conn = a.conn().unwrap();
            db::update_message(&conn, &id, "A 的版本").unwrap();
        }
        sync_until_quiet(&a, &server);

        // B 之后才改，并且成功同步（HLC 更大）
        std::thread::sleep(std::time::Duration::from_millis(8));
        {
            let conn = b.conn().unwrap();
            db::update_message(&conn, &id, "B 的版本").unwrap();
        }
        sync_until_quiet(&b, &server);

        // A 拉取到 B 的版本。A 那一版已经被服务端接受过，属于正常覆盖
        let report = sync_until_quiet(&a, &server);
        assert_eq!(
            report.conflicts, 0,
            "已被服务端接受过的版本被更新版本覆盖，是正常 LWW，不该制造副本"
        );

        let conn = a.conn().unwrap();
        let bodies: Vec<String> = conn
            .prepare("SELECT body FROM message WHERE deleted_at IS NULL")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(bodies.len(), 1, "不应产生副本：{bodies:?}");
        assert_eq!(bodies[0], "B 的版本");
    }

    // ------------------------------------------------ 锁的粒度

    /// 一个会反过来检查"本地库此刻能不能被打开"的假服务端。
    ///
    /// 它站在网络的另一端：每次收到请求，就回头敲一下本地数据库的门。
    /// 门开不开，直接反映同步线程有没有把锁攥在手里。
    struct LockProbingServer {
        db: Arc<Db>,
        probes: AtomicUsize,
    }

    impl LockProbingServer {
        fn new(db: &Arc<Db>) -> Self {
            Self {
                db: Arc::clone(db),
                probes: AtomicUsize::new(0),
            }
        }

        /// 站在"网络另一端"回头看一眼本地库能不能被打开。
        fn probe(&self) -> AppResult<()> {
            let conn = self.db.try_conn().ok_or_else(|| {
                AppError::Msg("网络往返期间数据库锁仍被同步线程持有 —— 界面上的写入会卡住".into())
            })?;
            // 真读一次，确认拿到的是能用的连接，而不只是"锁没被占"
            conn.query_row("SELECT COUNT(*) FROM message", [], |r| r.get::<_, i64>(0))?;
            self.probes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl ServerApi for LockProbingServer {
        fn push(&self, changes: &[Change]) -> AppResult<PushResponse> {
            self.probe()?;
            Ok(PushResponse {
                results: changes
                    .iter()
                    .map(|c| PushOutcome {
                        kind: c.kind,
                        id: c.id.clone(),
                        accepted: true,
                        winner: None,
                    })
                    .collect(),
            })
        }

        fn pull(&self, since: i64, _limit: i64) -> AppResult<PullResponse> {
            self.probe()?;
            Ok(PullResponse {
                changes: Vec::new(),
                cursor: since,
                has_more: false,
            })
        }
    }

    /// **同步引擎最要紧的一条结构性质：网络往返期间不持有数据库锁。**
    ///
    /// 锁要是被同步线程占着，`try_conn` 直接给 `None`，测试立刻红；
    /// 而在真实使用中，同一个退化只会表现成"用户打字卡住 4 到 30 秒"——
    /// 一种没有任何告警、也没人会去查的现象。
    ///
    /// 刻意用 `try_conn` 而不是 `conn`：一旦有人把锁的粒度退回去，
    /// 这个测试必须是**失败**，不能是**挂起**。挂起的测试会被当成"跑得慢"。
    #[test]
    fn the_database_is_not_locked_while_the_network_is_busy() {
        let db = Arc::new(device("device-lock-probe"));
        {
            let conn = db.conn().unwrap();
            db::append_message(&conn, "触发一次推送", None).unwrap();
        }

        let server = LockProbingServer::new(&db);
        let report = sync_once(&*db, &server).expect("网络期间不该有人持着数据库锁");

        assert!(report.pushed > 0, "测试不能空跑：必须有东西被推出去");
        assert!(
            server.probes.load(Ordering::SeqCst) >= 2,
            "推送和拉取都必须各自在网络里探测过一次"
        );
    }

    /// 反向验证：探针本身必须真的能发现"被占住的锁"。
    ///
    /// **没有这一条，上面那个测试可能因为探针失灵而永远绿。**
    /// 一个永远绿的守卫测试比没有测试更糟 —— 它让人以为性质已经被守住了。
    ///
    /// 顺带钉死两条语义：锁被占时**立刻返回 `None`**（不是阻塞等待），
    /// 以及放开之后又能正常拿到。
    #[test]
    fn the_lock_probe_detects_a_held_lock() {
        let db = Arc::new(device("device-lock-probe-negative"));
        let server = LockProbingServer::new(&db);

        server.probe().expect("没人持锁时探针不该报错");

        // 锁被占住 —— 这正是"网络期间持锁"退化以后的样子
        let guard = db.conn().unwrap();
        let err = server
            .probe()
            .expect_err("锁被占住时探针必须失败，而不是挂在那里等");
        assert!(
            err.to_string().contains("数据库锁"),
            "错误信息要让人一眼看出是锁的问题：{err}"
        );
        drop(guard);

        server.probe().expect("锁放开后又该正常了");
        assert_eq!(
            server.probes.load(Ordering::SeqCst),
            2,
            "只有两次成功的探测该被计数"
        );
    }

    /// 用随机操作序列压同步逻辑。
    ///
    /// 同步的 bug 几乎都藏在操作组合里，写死的用例覆盖不到 ——
    /// 随机化 + 固定种子（可复现）比手写用例有效得多。
    #[test]
    fn randomized_operations_converge() {
        let server = MemoryServer::new();
        let a = device("device-a");
        let b = device("device-b");
        let mut rng = Lcg::new(0xC0FFEE);

        for _ in 0..150 {
            let use_a = rng.next().is_multiple_of(2);
            let target: &Db = if use_a { &a } else { &b };
            {
                let conn = target.conn().unwrap();
                match rng.below(6) {
                    0 => {
                        let body = format!("随机记录 {}", rng.below(10_000));
                        db::append_message(&conn, &body, None).unwrap();
                    }
                    1 => {
                        let ids = live_message_ids(&conn);
                        if !ids.is_empty() {
                            let id = ids[rng.below(ids.len())].clone();
                            let body = format!("改过的 {}", rng.below(10_000));
                            db::update_message(&conn, &id, &body).unwrap();
                        }
                    }
                    2 => {
                        let ids = live_message_ids(&conn);
                        if !ids.is_empty() {
                            let id = ids[rng.below(ids.len())].clone();
                            let tag = format!("标签{}", rng.below(4));
                            db::set_message_tags(&conn, &id, &[tag]).unwrap();
                        }
                    }
                    3 => {
                        let ids = live_message_ids(&conn);
                        if !ids.is_empty() {
                            let id = ids[rng.below(ids.len())].clone();
                            db::delete_message(&conn, &id).unwrap();
                        }
                    }
                    4 => {
                        let name = format!("频道{}", rng.below(4));
                        let _ = db::create_channel(&conn, &name);
                    }
                    _ => {
                        let ids = live_message_ids(&conn);
                        let chans = db::list_channels(&conn).unwrap();
                        if !ids.is_empty() && chans.len() > 1 {
                            let id = ids[rng.below(ids.len())].clone();
                            let ch = &chans[rng.below(chans.len())];
                            let _ = db::move_message(&conn, &id, &ch.id);
                        }
                    }
                }
            }

            // 有一定概率同步其中一台，制造"两边进度不一致"的真实时序
            if rng.below(3) == 0 {
                let syncer: &Db = if rng.next().is_multiple_of(2) { &a } else { &b };
                let _ = sync_once(syncer, &server).unwrap();
            }
        }

        // 收尾：两边都同步到静止
        sync_until_quiet(&a, &server);
        sync_until_quiet(&b, &server);
        sync_until_quiet(&a, &server);

        let snap_a = snapshot(&a.conn().unwrap());
        let snap_b = snapshot(&b.conn().unwrap());

        assert!(!snap_a.is_empty(), "测试不能空跑");
        assert_eq!(
            snap_a.len(),
            snap_b.len(),
            "两台设备的行数必须一致：\nA={snap_a:#?}\nB={snap_b:#?}"
        );
        assert_eq!(snap_a, snap_b, "随机操作后必须收敛到完全一致的状态");
    }
}
