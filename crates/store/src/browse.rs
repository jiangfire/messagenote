//! 浏览与检索查询。
//!
//! **这里的每一条 SQL 在客户端和服务端两种 schema 上都成立**（见 crate 文档）。
//! 改动之前先确认：它没有引用 `dirty`，也没有引用 `server_seq`。

use std::collections::HashMap;

use rusqlite::{params, params_from_iter, Connection, Row};

use messagenote_core::models::{
    Channel, Message, MessagePage, SearchHit, SearchPage, TagCount, TimelineStats,
};
use messagenote_core::search::{self, QueryPlan};

/// 收件箱使用固定 ID（而不是随机 UUID）：
/// 每台设备都必须把「默认落点」认成同一个频道。
pub const INBOX_ID: &str = "inbox";

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

/// [`Scope::parse`] 的失败原因。
///
/// 单独一个类型，是为了让两端各自把它包成自己的错误（桌面端是 `AppError`，
/// 服务端是 `ServerError`），而**"接受哪些字符串、映射到什么"只有一份**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeParseError {
    UnknownScope(String),
    MissingChannelId,
    MissingTag,
}

impl std::fmt::Display for ScopeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownScope(s) => write!(f, "未知的时间线范围：{s}"),
            Self::MissingChannelId => write!(f, "scope=channel 时必须提供 channel_id"),
            Self::MissingTag => write!(f, "scope=tag 时必须提供 tag"),
        }
    }
}

impl std::error::Error for ScopeParseError {}

impl<'a> Scope<'a> {
    /// 从查询参数解析筛选范围。
    ///
    /// **桌面端的命令层和服务端的 API 共用这一份。** 两边各写一个 `match` 的话，
    /// `"unfiled"` 这个词在哪一边改了含义，另一边不会报错 —— 只是时间线
    /// 悄悄换成了别的内容。
    pub fn parse(
        scope: &'a str,
        channel_id: Option<&'a str>,
        tag: Option<&'a str>,
    ) -> Result<Self, ScopeParseError> {
        match scope {
            "all" => Ok(Scope::All),
            "unfiled" => Ok(Scope::Unfiled),
            "channel" => channel_id
                .map(Scope::Channel)
                .ok_or(ScopeParseError::MissingChannelId),
            "tag" => tag.map(Scope::Tag).ok_or(ScopeParseError::MissingTag),
            other => Err(ScopeParseError::UnknownScope(other.to_string())),
        }
    }
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
) -> rusqlite::Result<MessagePage> {
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

// ---------------------------------------------------------------- 频道

pub fn list_channels(conn: &Connection) -> rusqlite::Result<Vec<Channel>> {
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

// ---------------------------------------------------------------- 标签

pub fn list_tags(conn: &Connection) -> rusqlite::Result<Vec<TagCount>> {
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

// ---------------------------------------------------------------- 统计

/// 侧边栏要的两个计数。
///
/// 单独一条查询，不去翻整个列表 —— 侧边栏只需要两个数字，
/// 没必要为此把消息拉一遍。
pub fn timeline_stats(conn: &Connection) -> rusqlite::Result<TimelineStats> {
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

/// 中文检索。规划查询用的是 `core::search`，两端因此走同一条路径。
///
/// **排序必须带次级键。** `bm25()` 分数相同时，FTS5 按 rowid 返回 ——
/// 而 rowid 取决于插入顺序，服务端和客户端必然不同。于是同一批数据在两端
/// 会搜出**不同的顺序**：同样的词，桌面端和网页端列表长得不一样。
/// 不报错，但用户会觉得"这两个东西不是一回事"。
///
/// 加 `(created_at, id)` 之后，顺序只由内容决定，与插入顺序无关。
///
/// `offset` 用于"加载更多结果"，分页那层的封套见 [`search_page`]。
pub fn search(
    conn: &Connection,
    query: &str,
    limit: i64,
    offset: i64,
) -> rusqlite::Result<Vec<SearchHit>> {
    let limit = limit.clamp(1, 200);
    let offset = offset.max(0);
    let Some(plan) = search::plan_query(query) else {
        return Ok(Vec::new());
    };

    // 粗筛窗口要够大：跳过 offset 之后还得能剩下 limit 条精确命中。
    // 注意这只是**窗口的目标**，不是返回条数 —— 返回的切片由下面的
    // `skip/take` 决定，而"还有没有更多"由调用方多要一条来判断
    // （见 [`search_page`]）。
    let want = offset + limit + 1;

    let raw: Vec<(Message, String)> = match plan {
        QueryPlan::Fts { match_expr, words } => {
            // **窗口要逐步放大。** SQL 的 LIMIT 作用在粗筛上，而精确过滤在
            // Rust 里 —— 固定的窗口（早先是 limit*5）在候选里假阳性多的时候会
            // **静默少返回**：明明还有结果，却因为粗筛窗口里凑不出足够的精确
            // 命中而报告"没有了"。所以放大到凑够 want 条、或确认没有候选为止。
            let mut window = (want * 5).max(50);
            loop {
                let rows = fts_candidates(conn, &match_expr, window)?;
                let exhausted = (rows.len() as i64) < window;
                let matched: Vec<(Message, String)> = rows
                    .into_iter()
                    .filter(|(m, _)| search::matches_all(&m.body, &words))
                    .collect();

                if matched.len() as i64 >= want || exhausted || window >= MAX_SCAN {
                    break matched;
                }
                window = (window * 4).min(MAX_SCAN);
            }
        }
        // LIKE 回退路径的过滤条件就在 SQL 里，取多少就是多少，不用再筛
        QueryPlan::Like { words } => like_candidates(conn, &words, want)?,
    };

    let items: Vec<SearchHit> = raw
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .map(|(message, channel_name)| SearchHit {
            message,
            channel_name,
        })
        .collect();

    // 标签只给最终这一页补齐 —— 给所有候选补是白费一次查询
    let ids: Vec<String> = items.iter().map(|h| h.message.id.clone()).collect();
    let map = load_tags(conn, &ids)?;
    Ok(items
        .into_iter()
        .map(|mut h| {
            h.message.tags = map.get(&h.message.id).cloned().unwrap_or_default();
            h
        })
        .collect())
}

/// 粗筛窗口的上限。
///
/// 到了这个数还凑不够精确命中就不再往下扫了 —— 宁可少返回几条，
/// 也不能让一次检索把整个库拖进内存。
const MAX_SCAN: i64 = 5000;

/// FTS 粗筛：按相关性取前 `window` 条候选。
fn fts_candidates(
    conn: &Connection,
    match_expr: &str,
    window: i64,
) -> rusqlite::Result<Vec<(Message, String)>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.channel_id, m.body, m.created_at, m.updated_at, c.name
           FROM message_fts
           JOIN message m ON m.id = message_fts.message_id
           JOIN channel c ON c.id = m.channel_id
          WHERE message_fts MATCH ?1
            AND m.deleted_at IS NULL
          ORDER BY bm25(message_fts), m.created_at DESC, m.id DESC
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![match_expr, window], |r| {
        Ok((row_to_message(r)?, r.get::<_, String>(5)?))
    })?;
    rows.collect()
}

/// LIKE 回退：单字 CJK 之类构不成 bigram 的查询。
fn like_candidates(
    conn: &Connection,
    words: &[String],
    window: i64,
) -> rusqlite::Result<Vec<(Message, String)>> {
    let mut sql = String::from(
        "SELECT m.id, m.channel_id, m.body, m.created_at, m.updated_at, c.name
           FROM message m
           JOIN channel c ON c.id = m.channel_id
          WHERE m.deleted_at IS NULL",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    for w in words {
        sql.push_str(" AND m.body LIKE ? ESCAPE '\\'");
        args.push(Box::new(search::like_pattern(w)));
    }
    sql.push_str(" ORDER BY m.created_at DESC, m.id DESC LIMIT ?");
    args.push(Box::new(window));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args.iter().map(|b| b.as_ref())), |r| {
        Ok((row_to_message(r)?, r.get::<_, String>(5)?))
    })?;
    rows.collect()
}

/// 取一页检索结果。
///
/// 这一层只做一件事：**多要一条**，用"多出来的那一条"回答"还有没有更多"，
/// 不必再查一次 count。
pub fn search_page(
    conn: &Connection,
    query: &str,
    limit: i64,
    offset: i64,
) -> rusqlite::Result<SearchPage> {
    let limit = limit.clamp(1, 200);

    // 这里必须显式要 `limit + 1`。`search` 自己只会按 offset/limit 切片，
    // **不会**替我们多取一条 —— 早先的版本想当然地以为它会，
    // 于是 `items.len() > limit` 恒为假，`has_more` 永远是 false：
    // 界面上的"加载更多"从来不出现，而结果明明还有。
    let mut items = search(conn, query, limit + 1, offset)?;
    let has_more = items.len() as i64 > limit;
    items.truncate(limit as usize);
    Ok(SearchPage { items, has_more })
}

// ---------------------------------------------------------------- 读取辅助

/// 把一行 `(id, channel_id, body, created_at, updated_at)` 解成 [`Message`]。
///
/// 公开是因为"改完一条消息之后把它读回来"这个动作两端都要做，
/// 而列顺序必须和 [`list_messages`] 的 SELECT 一致 —— 各写一份迟早会对不上。
/// 标签由调用方补（见 [`attach_tags`]）。
pub fn row_to_message(row: &Row) -> rusqlite::Result<Message> {
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
fn load_tags(conn: &Connection, ids: &[String]) -> rusqlite::Result<HashMap<String, Vec<String>>> {
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

/// 给一批消息补上标签。
pub fn attach_tags(conn: &Connection, items: &mut [Message]) -> rusqlite::Result<()> {
    let ids: Vec<String> = items.iter().map(|m| m.id.clone()).collect();
    let map = load_tags(conn, &ids)?;
    for m in items.iter_mut() {
        m.tags = map.get(&m.id).cloned().unwrap_or_default();
    }
    Ok(())
}
