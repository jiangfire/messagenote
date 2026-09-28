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
//!
//! ## 测试里的一条纪律
//!
//! `Store::conn()` 拿到的是 `std::sync::Mutex` 的 guard，而它**不可重入**。
//! 所以测试里一旦握住了 guard，就不要再调 `Store` 的方法 ——
//! 那些方法会再拿一次同一把锁。**那不是变慢，是直接挂住**，而且挂住的测试
//! 会被当成"跑得慢"，没人会去查。
//!
//! 要用共享查询就直接调接受 `&Connection` 的那些函数，比如
//! `messagenote_store::search(&conn, …)`、`messagenote_store::list_messages(&conn, …)`。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use tokio::sync::broadcast;

use rusqlite::{params, Connection, OptionalExtension};

use messagenote_core::hlc::{now_ms, Hlc};
use messagenote_core::merge;
use messagenote_core::models::{
    Channel, Message, MessagePage, SearchPage, TagCount, TimelineStats,
};
use messagenote_core::payload::{
    message_tag_key, ChannelPayload, MessagePayload, MessageTagPayload, TagPayload,
};
use messagenote_core::search;
use messagenote_core::wire::{
    Change, EntityKind, PullResponse, PushOutcome, PushResponse, SessionResponse,
};
use messagenote_store::{blob, clock, normalize, Cursor, Scope};

use crate::error::{ServerError, ServerResult};

const SCHEMA_VERSION: i64 = 5;

/// 写入广播的队列深度。
///
/// 它决定的不是"能推多少条"，而是"一个卡住的订阅者多久之后会收到 Lagged"。
/// 信号本身没有内容（客户端收到就去拉最新状态），所以丢几次**无害** ——
/// 收到 Lagged 时补发一个就行。见 `events` 端点。
const EVENT_BUFFER: usize = 64;

/// 网页端会话的有效期。
///
/// 这是"不用反复登录"和"令牌被偷之后的暴露窗口"之间的折中。改这一个常量
/// 就能调整。注意它**必须**存在：长期令牌放进 localStorage 是永不过期的，
/// 再怎么定都比那个强。
const SESSION_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

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

-- 服务端自己的身份与时钟。
--
-- 代笔写入需要它：服务端在这里**就是一台普通设备**，要有自己唯一的 device_id
-- 和一份会推进的 HLC。和客户端那张 `meta` 表同形，推进逻辑也是同一份
-- （`messagenote_store::clock`）—— 那段逻辑一旦两端分叉，"谁更新"的结论就会
-- 分叉，而且是静默的。
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- 网页端的短期会话。
--
-- **为什么是数据库表，而不是内存里的 Map**：重启就掉线，多进程部署也会失效。
-- 早期笔记里写的是"倾向签名令牌（无状态）"，这里改了主意，理由是签名令牌
-- **签发之后到期前无法撤销** —— "退出登录"会变成一个骗人的按钮，而
-- 撤销恰恰是引入会话机制的主要收益之一。签名的另一条路要引一整套对称加密
-- 依赖，而这条只需要一张表。
--
-- **令牌原文直接存，没有哈希。** 哈希只有在"库比令牌更容易泄露"时才有意义，
-- 而这份库里躺着用户的全部笔记 —— 能拿到库的人已经不需要令牌了。
CREATE TABLE IF NOT EXISTS session (
  token      TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_session_expires ON session(expires_at);

-- 附件字节。
--
-- 和客户端那张同名表**同形**，少了客户端的 `uploaded` 一列（服务端不需要
-- 知道"谁传没传上来"）。共享层的每个查询都只碰前五列，所以两边能跑同一份 SQL。
--
-- 字节进数据库而不是放一个附件目录：备份。Litestream 只跟这一个 .sqlite 文件
-- 走，图片放外面会被静默漏掉。详见 messagenote_store::blob 的模块文档。
--
-- 这张表**不参与变更日志**：内容寻址意味着不可变（换图必然换 sha），
-- 没有冲突要解决。客户端是从正文里的 attachment:<sha> 发现它的。
CREATE TABLE IF NOT EXISTS attachment (
  sha256     TEXT PRIMARY KEY,
  size       INTEGER NOT NULL,
  mime       TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  bytes      BLOB
);
-- 回收孤儿附件时要反查"哪些 sha 有字节"。没有这个索引就得全表扫 BLOB。
CREATE INDEX IF NOT EXISTS idx_attachment_present
  ON attachment(sha256) WHERE bytes IS NOT NULL;
"#;

pub struct Store {
    conn: Mutex<Connection>,
    /// 写入广播。SSE 客户端靠它知道"有东西变了"。
    ///
    /// **只广播一个信号，不带具体内容。** 带上内容就得在这里实现一遍
    /// "哪些变更该发给谁"，而拉取路径已经有一套带游标、有测试守着的逻辑了 ——
    /// 两份实现对同一件事给出不同答案，是同步类 bug 最经典的来源。
    /// 客户端收到信号，自己去拉。
    ///
    /// 用 `broadcast` 而不是 `watch`：watch 只保留最后一个值，一个还没被
    /// 调度的客户端会**漏掉中间的信号**；而这里"漏掉信号"意味着它要等下一次
    /// 有人写才会醒过来。broadcast 会为每个订阅者排队（队列满了给 `Lagged`，
    /// 那时补发一次即可，见 `events`）。
    events: broadcast::Sender<()>,
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
            events: broadcast::channel(EVENT_BUFFER).0,
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
            events: broadcast::channel(EVENT_BUFFER).0,
        })
    }

    fn conn(&self) -> ServerResult<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| ServerError::Msg("数据库连接锁已损坏".into()))
    }

    // ---------------------------------------------------------------- 实时推送

    /// 订阅写入信号。SSE 端点用。
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.events.subscribe()
    }

    /// 广播"有东西变了"。
    ///
    /// **没有订阅者时是正常的**（比如只有桌面端在同步），所以这里忽略返回的
    /// 发送失败 —— 它不是错误，只是"没人在听"。
    fn emit(&self) {
        let _ = self.events.send(());
    }

    /// 当前最大 seq。客户端首次同步时从这里开始，避免把整个历史重放一遍。
    pub fn max_seq(&self) -> ServerResult<i64> {
        let conn = self.conn()?;
        max_seq(&conn)
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
        let cursor = changes.iter().filter_map(|c| c.seq).max().unwrap_or(since);

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
                // 引用完整性。**必须整批拒绝，不能"接受下来以后再说"**：
                // 客户端的表有外键，一条引用不存在的行会让接收端**整批拉取回滚**，
                // 而且此后每次同步都在同一个地方失败 —— 那台客户端永久卡死。
                // 一个坏客户端能因此锁死所有好客户端。
                check_references(&tx, c)?;

                let seq = next_seq(&tx)?;
                upsert(&tx, c, seq)?;

                // **观察客户端的时间戳，把本机时钟拉到两者最大值。**
                //
                // 漏了这一步，服务端就永远学不到客户端的时间。一台时钟超前的
                // 设备（VPS 上 NTP 挂掉、容器时钟漂移都算）改过某一行之后，
                // 服务端此后**代笔的每一次修改**都会因为 HLC 更旧而被判负 ——
                // 表现是"新建能成功、改已有的永远失败，说被另一台设备覆盖了"，
                // 而且刷新也没用。
                //
                // 客户端之间本来就会互相 observe（拉取远端变更时），所以补上
                // 这一步只是让服务端不再落后于所有人，不会让时钟更容易被带偏。
                clock::clock_observe(&tx, &c.hlc)?;

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
        // 提交之后才广播：订阅者收到信号就会来拉，那时数据必须已经可见。
        // 反过来的话，一次"收到信号但拉不到东西"会让客户端认为已经追平了。
        self.emit();
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
        Ok(messagenote_store::list_messages(
            &conn, scope, limit, before,
        )?)
    }

    pub fn search(&self, query: &str, limit: i64, offset: i64) -> ServerResult<SearchPage> {
        let conn = self.conn()?;
        Ok(messagenote_store::search_page(&conn, query, limit, offset)?)
    }

    // ---------------------------------------------------------------- 附件

    /// 存一份附件字节。已经存在时返回 `false`（内容寻址 —— 同名字节必然相同）。
    ///
    /// **调用方必须先核对 sha256**，这里只信任传进来的名字。
    pub fn put_blob(&self, sha256: &str, bytes: &[u8]) -> ServerResult<bool> {
        let conn = self.conn()?;
        Ok(blob::put_blob(&conn, sha256, bytes, now_ms())?)
    }

    /// 取附件字节和它**由内容嗅探出来的**类型。
    pub fn get_blob(&self, sha256: &str) -> ServerResult<Option<(String, Vec<u8>)>> {
        let conn = self.conn()?;
        Ok(blob::get_blob(&conn, sha256)?)
    }

    /// 服务端手上有没有这份字节。上传方用它决定要不要真的发字节。
    pub fn has_blob(&self, sha256: &str) -> ServerResult<bool> {
        let conn = self.conn()?;
        Ok(blob::has_blob(&conn, sha256)?)
    }

    /// 一批 sha 里服务端**缺**哪些。上传方一次问清楚，而不是盲传。
    pub fn missing_blobs(&self, shas: &[String]) -> ServerResult<Vec<String>> {
        let conn = self.conn()?;
        let mut out = Vec::new();
        for sha in shas {
            // 名字明显不合法的直接算"缺"，让上传方走完整校验路径
            if !messagenote_core::attachment::is_sha256(sha) || !blob::has_blob(&conn, sha)? {
                out.push(sha.clone());
            }
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- 代笔写入
    //
    // 服务端在这里**就是一台普通设备**：拿自己的 HLC、写进变更日志、分配 seq。
    // 桌面端下次同步照常拉到，没有任何特殊路径 —— 也正因如此，裁定权仍然
    // 只有一份（`merge::should_accept_push`）。
    //
    // 为什么不给浏览器也做一套同步引擎：那意味着在 JS 里再实现一遍 HLC 和
    // 合并规则。两份实现漂移起来的症状是"两边各自认为自己赢"，不报错，
    // 只是最终收敛不到同一个状态。
    //
    // 代价很明确：**网页端写入需要联网**。离线捕获留给 S3 的 outbox。

    /// 落一条以服务端身份发起的变更。
    ///
    /// 走的是和 `push` 完全同一套 `should_accept_push` + `upsert`，
    /// 所以索引维护、墓碑规则、seq 分配都不会有第二份实现。
    fn author(
        &self,
        kind: EntityKind,
        id: String,
        deleted: bool,
        data: serde_json::Value,
    ) -> ServerResult<()> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;

        // 时钟推进必须和变更落库在同一个事务里：崩在中间会让重启后的时钟回退，
        // 此后代笔的每一条都比已有的更旧、在裁定里持续判负 ——
        // 表现成"网页端写的东西全都不见了"，而且不报任何错。
        let hlc = clock::clock_next(&tx)?;
        let change = Change {
            seq: None,
            kind,
            id,
            hlc,
            deleted,
            data: Some(data),
        };

        if !merge::should_accept_push(
            &change.hlc,
            read_stored_hlc(&tx, kind, &change.id)?.as_ref(),
        ) {
            // 极少见：某台设备的时钟比本机还超前，于是它那一版更"新"。
            // 按 LWW 保留它 —— 但**绝不静默**，否则用户会以为自己的修改保存了。
            let wall = read_change(&tx, kind, &change.id)?
                .map(|w| w.hlc.wall)
                .unwrap_or(0);
            tx.commit()?;
            return Err(ServerError::Msg(format!(
                "这次修改被另一台设备上更新的版本覆盖了（对方时间戳 {wall}），请刷新后重试"
            )));
        }

        let seq = next_seq(&tx)?;
        upsert(&tx, &change, seq)?;
        tx.commit()?;
        // 所有"代笔写入"都走这里（建/改/删消息、频道、标签……），
        // 所以这是唯一的广播点，不需要在每个 handler 里各写一遍。
        self.emit();
        Ok(())
    }

    /// 读出某条消息当前的完整 payload。
    ///
    /// 编辑/移动/删除都要在它基础上改：线上的变更传的是**整份快照**而不是差分，
    /// 少填一个字段，`upsert` 里对应的 `excluded.*` 就会把它写成空值。
    fn message_payload(&self, id: &str) -> ServerResult<MessagePayload> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT channel_id, body, created_at FROM message
                  WHERE id = ?1 AND deleted_at IS NULL",
            params![id],
            |r| {
                Ok(MessagePayload {
                    channel_id: r.get(0)?,
                    body: r.get(1)?,
                    created_at: r.get(2)?,
                    updated_at: 0,
                })
            },
        )
        .optional()?
        .ok_or_else(|| ServerError::bad_request("消息不存在"))
    }

    fn channel_payload(&self, id: &str) -> ServerResult<ChannelPayload> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT name, kind, sort_order, created_at FROM channel
                  WHERE id = ?1 AND deleted_at IS NULL",
            params![id],
            |r| {
                Ok(ChannelPayload {
                    name: r.get(0)?,
                    kind: r.get(1)?,
                    sort_order: r.get(2)?,
                    created_at: r.get(3)?,
                    updated_at: 0,
                })
            },
        )
        .optional()?
        .ok_or_else(|| ServerError::bad_request("频道不存在"))
    }

    fn message(&self, id: &str) -> ServerResult<Message> {
        let conn = self.conn()?;
        let mut msg = conn
            .query_row(
                "SELECT id, channel_id, body, created_at, updated_at
                   FROM message WHERE id = ?1 AND deleted_at IS NULL",
                params![id],
                messagenote_store::row_to_message,
            )
            .optional()?
            .ok_or_else(|| ServerError::bad_request("消息不存在"))?;
        messagenote_store::attach_tags(&conn, std::slice::from_mut(&mut msg))?;
        Ok(msg)
    }

    /// 按 id 读一条消息，**不管它是不是墓碑**。
    ///
    /// 和 [`Self::message`] 只差一个地方：不过滤 `deleted_at`。幂等检查要用它，
    /// 因为"已经删掉了"也必须算"这个 id 用过了"。
    fn existing_message(&self, id: &str) -> ServerResult<Option<Message>> {
        let conn = self.conn()?;
        let mut found = conn
            .query_row(
                "SELECT id, channel_id, body, created_at, updated_at
                   FROM message WHERE id = ?1",
                params![id],
                messagenote_store::row_to_message,
            )
            .optional()?;
        if let Some(msg) = found.as_mut() {
            messagenote_store::attach_tags(&conn, std::slice::from_mut(msg))?;
        }
        Ok(found)
    }

    fn channel(&self, id: &str) -> ServerResult<Channel> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT c.id, c.name, c.kind, c.sort_order, c.created_at, c.updated_at,
                        (SELECT COUNT(*) FROM message m
                          WHERE m.channel_id = c.id AND m.deleted_at IS NULL)
                   FROM channel c WHERE c.id = ?1 AND c.deleted_at IS NULL",
            params![id],
            |r| {
                Ok(Channel {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    sort_order: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                    message_count: r.get(6)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| ServerError::bad_request("频道不存在"))
    }

    fn require_channel(&self, id: &str) -> ServerResult<()> {
        let _ = self.channel_payload(id)?;
        Ok(())
    }

    // ---- 消息 ----

    /// 新建一条消息。
    ///
    /// `id` 是**幂等键**：给了它就保证"同一条请求重放多少遍，库里也只有一条"。
    /// 见 `CreateMessageRequest::id` 的说明。
    pub fn create_message(
        &self,
        body: &str,
        channel_id: Option<&str>,
        id: Option<&str>,
    ) -> ServerResult<Message> {
        let body = normalize::body(body).map_err(ServerError::bad_request)?;
        let channel_id = channel_id.unwrap_or(messagenote_store::INBOX_ID);
        self.require_channel(channel_id)?;

        let id = match id {
            Some(given) => {
                let given = validate_client_id(given)?;
                if let Some(existing) = self.existing_message(&given)? {
                    // 已经有这一行：**一个字都不写**，把当前那一版原样还回去。
                    //
                    // 这里判的是"有没有这一行"，**不是**"有没有没被删的那一行"。
                    // 已经删掉的也算数 —— 否则离线队列的一次重放会把用户删掉的
                    // 记录**复活**。宁可少写一条，也不能让删除失效。
                    return Ok(existing);
                }
                given
            }
            None => uuid::Uuid::now_v7().to_string(),
        };

        let now = now_ms();
        self.author(
            EntityKind::Message,
            id.clone(),
            false,
            to_json(MessagePayload {
                channel_id: channel_id.to_string(),
                body,
                created_at: now,
                updated_at: now,
            })?,
        )?;
        self.message(&id)
    }

    pub fn edit_message(&self, id: &str, body: &str) -> ServerResult<Message> {
        let body = normalize::body(body).map_err(ServerError::bad_request)?;
        let mut p = self.message_payload(id)?;
        p.body = body;
        p.updated_at = now_ms();
        self.author(EntityKind::Message, id.to_string(), false, to_json(p)?)?;
        self.message(id)
    }

    pub fn remove_message(&self, id: &str) -> ServerResult<()> {
        let mut p = self.message_payload(id)?;
        // 墓碑时间取自 payload 的 `updated_at`，所以这里必须设成"现在"。
        // 服务端另取一个时间的话，两台设备对同一条墓碑会写出不同的
        // `deleted_at`，状态永远收敛不了 —— 而且不报错。
        p.updated_at = now_ms();
        self.author(EntityKind::Message, id.to_string(), true, to_json(p)?)
    }

    pub fn move_message(&self, id: &str, channel_id: &str) -> ServerResult<()> {
        self.require_channel(channel_id)?;
        let mut p = self.message_payload(id)?;
        p.channel_id = channel_id.to_string();
        p.updated_at = now_ms();
        self.author(EntityKind::Message, id.to_string(), false, to_json(p)?)
    }

    /// 用「标签名列表」整体替换一条消息的标签。
    ///
    /// **只发必要的变更**：该加的加、该删的删、没动的不发。
    /// 按"全部标删再全部重建"来写会产生一堆无意义的变更，让所有设备白重放一遍。
    pub fn set_message_tags(&self, message_id: &str, names: &[String]) -> ServerResult<()> {
        let _ = self.message_payload(message_id)?; // 消息必须存在
        let wanted = normalize::tags(names);

        // 现有关系（带原始 created_at —— 墓碑不能改这个值，
        // 否则各设备对同一行的快照会不一致）
        let current: Vec<(String, i64, i64)> = {
            let conn = self.conn()?;
            let mut stmt = conn.prepare(
                "SELECT tag_name, created_at, updated_at FROM message_tag
                  WHERE message_id = ?1 AND deleted_at IS NULL",
            )?;
            let it = stmt.query_map(params![message_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };

        // 删掉不再需要的
        for (name, created_at, _) in &current {
            if wanted.contains(name) {
                continue;
            }
            self.author(
                EntityKind::MessageTag,
                message_tag_key::encode(message_id, name),
                true,
                to_json(MessageTagPayload {
                    message_id: message_id.to_string(),
                    tag_name: name.clone(),
                    created_at: *created_at,
                    updated_at: now_ms(),
                })?,
            )?;
        }

        // 加上新要的
        for name in &wanted {
            if current.iter().any(|(n, _, _)| n == name) {
                continue;
            }
            // 标签本身也得存在：`load_tags` 会 JOIN tag 表，
            // 缺了这一行的话，所有设备上都看不到这个标签。
            self.ensure_tag(name)?;
            self.author(
                EntityKind::MessageTag,
                message_tag_key::encode(message_id, name),
                false,
                to_json(MessageTagPayload {
                    message_id: message_id.to_string(),
                    tag_name: name.clone(),
                    created_at: now_ms(),
                    updated_at: now_ms(),
                })?,
            )?;
        }
        Ok(())
    }

    /// 保证标签行存在且未被删。已经好好的就什么都不发。
    fn ensure_tag(&self, name: &str) -> ServerResult<()> {
        let existing: Option<(i64, Option<i64>)> = {
            let conn = self.conn()?;
            conn.query_row(
                "SELECT created_at, deleted_at FROM tag WHERE name = ?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        };
        let now = now_ms();

        match existing {
            // 已经存在且活着 —— 发一条内容相同的变更只会让所有设备白重放一遍
            Some((_, None)) => Ok(()),
            // 是墓碑：复活它，并**沿用原来的 created_at**
            Some((created_at, Some(_))) => self.author(
                EntityKind::Tag,
                name.to_string(),
                false,
                to_json(TagPayload {
                    created_at,
                    updated_at: now,
                })?,
            ),
            // 全新的标签
            None => self.author(
                EntityKind::Tag,
                name.to_string(),
                false,
                to_json(TagPayload {
                    created_at: now,
                    updated_at: now,
                })?,
            ),
        }
    }

    // ---- 频道 ----

    pub fn create_channel(&self, name: &str) -> ServerResult<Channel> {
        let name = normalize::channel_name(name).map_err(ServerError::bad_request)?;
        {
            let conn = self.conn()?;
            let exists: Option<String> = conn
                .query_row(
                    "SELECT id FROM channel WHERE name = ?1 AND deleted_at IS NULL",
                    params![name],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_some() {
                return Err(ServerError::bad_request(format!("频道「{name}」已存在")));
            }
        }

        // 排在最后。用 MAX+1 而不是 COUNT+1：删过频道之后 COUNT 会撞上已有的
        // 值，两个频道排序相同、顺序变得不确定。
        let next_order: i64 = {
            let conn = self.conn()?;
            conn.query_row(
                "SELECT COALESCE(MAX(sort_order), 0) + 1 FROM channel",
                [],
                |r| r.get(0),
            )?
        };

        let now = now_ms();
        let id = uuid::Uuid::now_v7().to_string();
        self.author(
            EntityKind::Channel,
            id.clone(),
            false,
            to_json(ChannelPayload {
                name,
                // 服务端只代笔普通频道：收件箱是常量实体，不存在"创建"这个动作
                kind: "normal".into(),
                sort_order: next_order,
                created_at: now,
                updated_at: now,
            })?,
        )?;
        self.channel(&id)
    }

    pub fn rename_channel(&self, id: &str, name: &str) -> ServerResult<()> {
        let name = normalize::channel_name(name).map_err(ServerError::bad_request)?;
        let mut p = self.channel_payload(id)?;
        p.name = name;
        p.updated_at = now_ms();
        self.author(EntityKind::Channel, id.to_string(), false, to_json(p)?)
    }

    /// 删除频道，**把它里面的记录移回收件箱**。
    ///
    /// 和客户端 `delete_channel` 的语义必须一致 —— 不一致的话，同一个操作
    /// 在桌面端和网页端会留下不同的结果，而用户会以为其中一边丢了东西。
    ///
    /// 注意**不是**删掉里面的消息：界面上那句「其中的记录会回到收件箱」是承诺，
    /// 不是描述。一个"删除频道"顺带毁掉里面所有笔记，是这个项目最不能接受的事。
    pub fn remove_channel(&self, id: &str) -> ServerResult<()> {
        if id == messagenote_store::INBOX_ID {
            return Err(ServerError::bad_request("收件箱是默认捕获目标，不能删除"));
        }
        let _ = self.channel_payload(id)?;

        let ids: Vec<String> = {
            let conn = self.conn()?;
            let mut stmt = conn
                .prepare("SELECT id FROM message WHERE channel_id = ?1 AND deleted_at IS NULL")?;
            let it = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for mid in ids {
            self.move_message(&mid, messagenote_store::INBOX_ID)?;
        }

        let mut p = self.channel_payload(id)?;
        p.updated_at = now_ms();
        self.author(EntityKind::Channel, id.to_string(), true, to_json(p)?)
    }

    // ---------------------------------------------------------------- 会话

    /// 签发一个短期会话。
    ///
    /// 令牌用 **v4（纯随机）** 而不是 v7：v7 的前 48 位就是时间戳，
    /// 拿它当凭据等于把强度降到"猜时间戳"。
    pub fn create_session(&self) -> ServerResult<SessionResponse> {
        let conn = self.conn()?;
        let now = now_ms();
        let expires_at = now + SESSION_TTL_MS;

        // 顺手清掉过期的。这是这张表唯一会产生垃圾的地方，
        // 就在产生垃圾的地方清 —— 不需要额外的定时任务。
        conn.execute("DELETE FROM session WHERE expires_at <= ?1", params![now])?;

        let session = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO session (token, created_at, expires_at) VALUES (?1, ?2, ?3)",
            params![session, now, expires_at],
        )?;
        Ok(SessionResponse {
            session,
            expires_at,
        })
    }

    /// 这个会话现在有效吗。
    ///
    /// 注意这里的比较**不是常量时间的**（SQLite 的主键查找会在第一个不同的
    /// 字节处短路）。之所以可以接受：令牌是 122 位随机值，要通过 HTTP 观测到
    /// 亚微秒级的差异来逐字节还原它，不现实。长期令牌那条路径仍然走
    /// `constant_time_eq`，因为那个是人选的、熵低得多。
    pub fn session_is_valid(&self, token: &str) -> ServerResult<bool> {
        if token.is_empty() {
            return Ok(false);
        }
        let conn = self.conn()?;
        let now = now_ms();

        let expires_at: Option<i64> = conn
            .query_row(
                "SELECT expires_at FROM session WHERE token = ?1 AND expires_at > ?2",
                params![token, now],
                |r| r.get(0),
            )
            .optional()?;

        let Some(expires_at) = expires_at else {
            return Ok(false);
        };

        // **滑动续期**：剩余寿命不足一半时把它推满。
        //
        // 不这么做的话，一个每天都在用的人也会在第 7 天被踢回登录页 ——
        // 而且很可能是在他正写到一半的时候。会话该过期的是"不再使用的"，
        // 不是"用了很久的"。
        //
        // 只续到一半以下才写库：每个请求都 UPDATE 一次是纯粹的浪费。
        if expires_at - now < SESSION_TTL_MS / 2 {
            conn.execute(
                "UPDATE session SET expires_at = ?2 WHERE token = ?1",
                params![token, now + SESSION_TTL_MS],
            )?;
        }
        Ok(true)
    }

    /// 吊销一个会话。这是引入会话机制的主要收益 —— 签名令牌做不到这件事。
    pub fn drop_session(&self, token: &str) -> ServerResult<()> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM session WHERE token = ?1", params![token])?;
        Ok(())
    }
}

/// 序列化 payload。失败只可能是 serde 本身出了意外，不是为用户准备的消息。
fn to_json<T: serde::Serialize>(value: T) -> ServerResult<serde_json::Value> {
    serde_json::to_value(value).map_err(|e| ServerError::Msg(format!("payload 序列化失败：{e}")))
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
    Ok(
        conn.query_row("SELECT value FROM seq_counter WHERE id = 1", [], |r| {
            r.get(0)
        })?,
    )
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
        .query_row(
            sql,
            rusqlite::params_from_iter(key.iter().map(|b| b.as_ref())),
            |r| {
                Ok(Hlc::new(
                    r.get::<_, i64>(0)?,
                    r.get::<_, u32>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
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
    // 服务端也是一台设备，也要有自己唯一的 id ——
    // HLC 是 (wall, counter, device)，device 那一层用来打破平局。
    clock::ensure_device_id(conn)?;

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

/// 校验一条变更引用的行都存在。
///
/// **为什么值得单独做这一步**：我们自己的客户端不会触发它 ——
/// `pending_changes` 保证同一个批次里频道整体排在消息之前，而上一批推过的
/// 频道已经落库了。所以这里失败意味着**对端有 bug，或者根本不是我们的客户端**。
///
/// 那正是要拦住的东西：一条指向不存在频道的消息落到服务端之后，会被所有客户端
/// 拉到，而客户端的 `message` 表有指向 `channel` 的外键 —— 那条插入失败会让
/// **整批拉取回滚**，此后每次同步都从同一个地方失败。一个坏客户端能锁死所有
/// 好客户端，而且受害的那台看起来只是"同步一直失败"。
///
/// 注意判的是**存在性**，不是"没被删"：墓碑行仍然满足外键，客户端也照样能插入。
/// 判错了会把"频道被删了但消息还没同步到"这种正常时序当成错误。
fn check_references(conn: &Connection, c: &Change) -> ServerResult<()> {
    match c.kind {
        EntityKind::Channel | EntityKind::Tag => Ok(()),

        EntityKind::Message => {
            let p: MessagePayload = decode(c)?;
            if !row_exists(conn, "channel", "id", &p.channel_id)? {
                return Err(ServerError::bad_request(format!(
                    "消息 {} 指向不存在的频道「{}」",
                    c.id, p.channel_id
                )));
            }
            Ok(())
        }

        EntityKind::MessageTag => {
            let p: MessageTagPayload = decode(c)?;
            if !row_exists(conn, "message", "id", &p.message_id)? {
                return Err(ServerError::bad_request(format!(
                    "标签关联 {} 指向不存在的消息「{}」",
                    c.id, p.message_id
                )));
            }
            if !row_exists(conn, "tag", "name", &p.tag_name)? {
                return Err(ServerError::bad_request(format!(
                    "标签关联 {} 指向不存在的标签「{}」",
                    c.id, p.tag_name
                )));
            }
            Ok(())
        }
    }
}

/// `table` / `column` 只来自上面那个函数里的字面量，不接受外部输入。
fn row_exists(conn: &Connection, table: &str, column: &str, value: &str) -> ServerResult<bool> {
    let sql = format!("SELECT 1 FROM {table} WHERE {column} = ?1");
    let found: Option<i64> = conn
        .query_row(&sql, params![value], |r| r.get(0))
        .optional()?;
    Ok(found.is_some())
}

/// 客户端提供的幂等键只做最基本的形状校验。
///
/// 它直接当主键用，所以不能是空串、不能长到离谱。**刻意不要求它长得像 UUID**：
/// 将来换个 id 生成方式（比如带上设备前缀，便于排查"这条是谁写的"）
/// 不该让服务端突然开始拒绝写入。
fn validate_client_id(id: &str) -> ServerResult<String> {
    let id = id.trim();
    if id.is_empty() {
        return Err(ServerError::bad_request("消息 id 不能是空的"));
    }
    if id.len() > 128 {
        return Err(ServerError::bad_request("消息 id 太长（上限 128 字节）"));
    }
    Ok(id.to_string())
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
                    c.id,
                    p.name,
                    p.kind,
                    p.sort_order,
                    p.created_at,
                    p.updated_at,
                    h.device,
                    h.wall,
                    h.counter,
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
                    c.id,
                    p.channel_id,
                    p.body,
                    p.created_at,
                    p.updated_at,
                    h.device,
                    h.wall,
                    h.counter,
                    if c.deleted { Some(p.updated_at) } else { None },
                    seq
                ],
            )?;

            // 检索索引是派生数据，但必须和正文在**同一个事务**里更新。
            // 分开写的话，索引和数据会静默漂移 —— 表现成"搜得到但点不开"
            // 或者"明明写了却搜不到"，两种都极难查。
            //
            // 注意 `upsert` 拿到的 conn 就是 `push` 那个事务，所以这里天然同事务。
            conn.execute(
                "DELETE FROM message_fts WHERE message_id = ?1",
                params![c.id],
            )?;
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
                    p.message_id,
                    p.tag_name,
                    p.created_at,
                    p.updated_at,
                    h.device,
                    h.wall,
                    h.counter,
                    if c.deleted { Some(p.updated_at) } else { None },
                    seq
                ],
            )?;
        }
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(c: &Change) -> ServerResult<T> {
    let data = c
        .data
        .clone()
        .ok_or_else(|| ServerError::Msg(format!("变更缺少 data：{} {}", c.kind.as_str(), c.id)))?;
    serde_json::from_value(data)
        .map_err(|e| ServerError::Msg(format!("变更 data 解析失败（{}）：{e}", c.id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 共享存储层。这里是**服务端**第一次真的用它 —— 见 shared_browse_queries_…
    use messagenote_store::{Cursor, Scope};

    /// 测试只关心"搜到几条、是哪几条"，不关心分页 —— 统一取第一页。
    ///
    /// 返回 `Result` 是为了让调用点保持原来的 `….unwrap()` 形状。
    fn search_hits(
        s: &Store,
        q: &str,
        n: i64,
    ) -> ServerResult<Vec<messagenote_core::models::SearchHit>> {
        Ok(s.search(q, n, 0)?.items)
    }

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
            first
                .changes
                .iter()
                .filter_map(|c| c.seq)
                .collect::<Vec<_>>(),
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
            search_hits(&s, "好的", 10).unwrap().is_empty(),
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
        assert_eq!(
            search_hits(&s, "读了", 10).unwrap().len(),
            1,
            "刚推上来就该能搜到"
        );

        // 改正文
        s.push(&[msg("m1", 200, 0, "a", "今天去爬山了")]).unwrap();
        assert!(
            search_hits(&s, "读了", 10).unwrap().is_empty(),
            "正文改掉之后旧词不能再搜到 —— 还能搜到就说明索引和数据已经漂移"
        );
        assert_eq!(search_hits(&s, "爬山", 10).unwrap().len(), 1);

        // 删除
        let mut tomb = msg("m1", 300, 0, "a", "今天去爬山了");
        tomb.deleted = true;
        s.push(&[tomb]).unwrap();
        assert!(
            search_hits(&s, "爬山", 10).unwrap().is_empty(),
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

        let hits = search_hits(&s, "升级", 10).unwrap();
        assert_eq!(hits.len(), 1, "回填之后，升级前就有的数据也要搜得到");
        assert_eq!(hits[0].message.id, "old-1");
    }

    /// 代笔写入 = 服务端把自己当一台设备。
    ///
    /// 核心断言是**代笔产生的变更必须进变更流**：桌面端不需要知道这条笔记是
    /// "网页端写的"还是"另一台桌面端写的" —— 走的完全是同一条路。
    #[test]
    fn authoring_writes_into_the_change_log() {
        let s = store();

        let m = s.create_message("网页端记的一条", None, None).unwrap();
        assert_eq!(m.body, "网页端记的一条");
        assert_eq!(m.channel_id, "inbox", "省略频道时落到收件箱");

        // 拉取能看到它 —— 这就是"服务端当一台设备"的全部含义
        let pulled = s.pull(0, 100).unwrap();
        assert!(
            pulled.changes.iter().any(|c| c.id == m.id),
            "代笔的变更必须进变更流，否则桌面端永远看不到"
        );

        // 索引维护走的是和 push 同一条路径，所以立刻能搜到
        assert_eq!(search_hits(&s, "网页端", 10).unwrap().len(), 1);

        // 改正文：旧词消失、新词出现、变更流里多一条
        let edited = s.edit_message(&m.id, "改成了别的").unwrap();
        assert_eq!(edited.body, "改成了别的");
        assert!(
            search_hits(&s, "网页端", 10).unwrap().is_empty(),
            "旧正文要从索引里摘掉"
        );
        assert_eq!(search_hits(&s, "别的", 10).unwrap().len(), 1);
        assert_eq!(edited.created_at, m.created_at, "改正文不该动 created_at");

        // 删除：墓碑要进变更流，且带上 payload 的 updated_at
        s.remove_message(&m.id).unwrap();
        let tomb = s
            .pull(0, 100)
            .unwrap()
            .changes
            .into_iter()
            .find(|c| c.id == m.id && c.deleted)
            .expect("墓碑必须进变更流");
        let p: MessagePayload = decode(&tomb).unwrap();
        assert!(tomb.deleted);
        assert!(p.updated_at > 0, "墓碑时间取自 payload 的 updatedAt");
        assert!(
            search_hits(&s, "别的", 10).unwrap().is_empty(),
            "墓碑要从索引里摘掉"
        );
    }

    /// 校验规则和桌面端**共用同一份**（`messagenote_store::normalize`）。
    ///
    /// 不共用的话会变成"桌面端拒绝、网页端接受" —— 用户看到的是
    /// "这个软件时好时坏"，而没有任何一处报错指向真正的原因。
    #[test]
    fn authoring_rejects_what_the_desktop_rejects() {
        let s = store();

        assert!(s.create_message("   ", None, None).is_err(), "空内容要拒绝");
        assert!(
            s.create_message("\n\t\n", None, None).is_err(),
            "只有空白的也要拒绝"
        );
        assert!(s.create_channel("  ",).is_err(), "空频道名要拒绝");

        let m = s.create_message("正常内容", None, None).unwrap();
        assert!(
            s.move_message(&m.id, "不存在的频道").is_err(),
            "目标频道不存在要拒绝"
        );
        assert!(s.edit_message(&m.id, "  ").is_err(), "改成空要拒绝");
        assert!(
            s.rename_channel(&m.channel_id, "  ").is_err(),
            "频道名改成空要拒绝"
        );
        assert!(
            s.remove_channel(&m.channel_id).is_err(),
            "收件箱是默认捕获目标，不能删除"
        );
    }

    /// 标签是「整体替换」语义，而且**只发必要的变更**。
    ///
    /// 按"全部标删再全部重建"写会产生一堆无意义的变更让所有设备白重放一遍。
    #[test]
    fn setting_tags_emits_only_the_changes_that_are_needed() {
        let s = store();
        let m = s.create_message("带标签的一条", None, None).unwrap();
        let before = s.max_seq().unwrap();

        s.set_message_tags(&m.id, &["水果".into(), " 水果 ".into(), "".into()])
            .unwrap();
        let after_add = s.max_seq().unwrap();
        // 一个 tag 行 + 一个 message_tag 行
        assert_eq!(after_add - before, 2, "新增两个标签应当只产生两条变更");

        // 重复设置同样的标签：一条变更都不该产生
        s.set_message_tags(&m.id, &["水果".into()]).unwrap();
        assert_eq!(
            s.max_seq().unwrap(),
            after_add,
            "标签没变化时不该产生任何变更 —— 否则每次保存都会让所有设备重放一遍"
        );

        // 清空
        s.set_message_tags(&m.id, &[]).unwrap();
        assert_eq!(s.max_seq().unwrap(), after_add + 1, "清空只需一条墓碑");

        let tags = messagenote_store::list_tags(&s.conn().unwrap()).unwrap();
        assert!(
            tags.iter().all(|t| t.count == 0),
            "标签行还在（不回收孤儿标签），但计数要为 0"
        );
    }

    #[test]
    fn a_session_can_be_revoked() {
        let s = store();
        let sess = s.create_session().unwrap();
        assert!(s.session_is_valid(&sess.session).unwrap());
        assert!(sess.expires_at > now_ms(), "过期时间必须在将来");

        s.drop_session(&sess.session).unwrap();
        assert!(
            !s.session_is_valid(&sess.session).unwrap(),
            "吊销之后必须**立刻**失效 —— 这正是会话相对签名令牌的收益，\
             也决定了我们不能用无状态签名令牌"
        );
    }

    #[test]
    fn an_expired_session_is_rejected() {
        let s = store();
        let sess = s.create_session().unwrap();
        assert!(s.session_is_valid(&sess.session).unwrap());

        {
            let conn = s.conn().unwrap();
            conn.execute(
                "UPDATE session SET expires_at = 0 WHERE token = ?1",
                params![sess.session],
            )
            .unwrap();
        }
        assert!(
            !s.session_is_valid(&sess.session).unwrap(),
            "过期会话必须失效"
        );

        // 签发新会话时顺手清掉过期的 —— 否则这张表会一直长
        let _ = s.create_session().unwrap();
        let n: i64 = s
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "签发新会话时应当顺手把过期的删掉");
    }

    #[test]
    fn session_tokens_are_random_not_time_ordered() {
        let s = store();
        let a = s.create_session().unwrap().session;
        let b = s.create_session().unwrap().session;
        assert_ne!(a, b);

        // v7 UUID 的前 48 位就是时间戳，几乎同时生成的两个会共享前缀 ——
        // 拿它当凭据等于把令牌强度降到"猜时间戳"。这里钉住用的是 v4。
        for t in [&a, &b] {
            let u = uuid::Uuid::parse_str(t).expect("会话令牌应当是 UUID");
            assert_eq!(
                u.get_version_num(),
                4,
                "会话令牌必须用 v4（纯随机），不能用 v7"
            );
        }
    }

    /// 会话在**被使用**时向前滑动。
    ///
    /// 不滑动的话，一个每天都在用的人也会在第 7 天被踢回登录页 ——
    /// 而且很可能是在他正写到一半的时候。会话该过期的是"不再使用的"，
    /// 不是"用了很久的"。
    #[test]
    fn an_active_session_slides_forward() {
        let s = store();
        let sess = s.create_session().unwrap();

        // 把它改到"只剩一小时"
        let nearly_done = now_ms() + 60 * 60 * 1000;
        {
            let conn = s.conn().unwrap();
            conn.execute(
                "UPDATE session SET expires_at = ?2 WHERE token = ?1",
                params![sess.session, nearly_done],
            )
            .unwrap();
        }

        assert!(
            s.session_is_valid(&sess.session).unwrap(),
            "还没过期，应当有效"
        );

        let after: i64 = s
            .conn()
            .unwrap()
            .query_row(
                "SELECT expires_at FROM session WHERE token = ?1",
                params![sess.session],
                |r| r.get(0),
            )
            .unwrap();
        assert!(after > nearly_done, "用过的会话应当被续期");
        assert!(
            after >= now_ms() + SESSION_TTL_MS / 2,
            "续期应当推到接近满寿命，而不是只加一点点"
        );

        // 刚续过期的不该被反复写库
        let before_second = after;
        assert!(s.session_is_valid(&sess.session).unwrap());
        let after_second: i64 = s
            .conn()
            .unwrap()
            .query_row(
                "SELECT expires_at FROM session WHERE token = ?1",
                params![sess.session],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after_second, before_second, "寿命还很足时不该再写库");
    }

    /// **服务端必须观察客户端的时间戳。**
    ///
    /// 不观察的话，一台时钟超前的设备改过某一行之后，服务端此后代笔的每一次
    /// 修改都会因为 HLC 更旧而被判负 —— 表现是"新建能成功、改已有的永远失败，
    /// 说被另一台设备覆盖了"，刷新也没用。这种一半好一半坏的症状最难查。
    #[test]
    fn the_server_learns_from_client_clocks() {
        let s = store();
        let m = s.create_message("原始内容", None, None).unwrap();

        // 模拟一台时钟严重超前的设备改了这条
        let far_future = now_ms() + 86_400_000; // 一天之后
        s.push(&[msg(&m.id, far_future, 0, "fast-device", "来自未来的修改")])
            .unwrap();

        // 现在服务端代笔改这条 —— 必须成功
        let edited = s
            .edit_message(&m.id, "网页端改的")
            .expect("服务端代笔不该被自己的时钟拖累：它应当已经观察到客户端的时间戳");
        assert_eq!(edited.body, "网页端改的");

        // 而且代笔产出的 HLC 确实比那个未来时间戳更新
        let latest = s
            .pull(0, 1000)
            .unwrap()
            .changes
            .into_iter()
            .filter(|c| c.id == m.id)
            .max_by_key(|c| c.seq)
            .expect("应当能拉到这条");
        assert!(
            latest.hlc.wall >= far_future,
            "服务端的时钟应当已经跟上了客户端（当前 {} vs 客户端 {}）",
            latest.hlc.wall,
            far_future
        );
    }

    /// 删除频道**不能**把里面的笔记一起删掉。
    ///
    /// 界面上的确认框写的是「其中的记录会回到收件箱」—— 那是**承诺**。
    /// 早先的实现是把它们一起软删了：一个"删除频道"的动作毁掉所有相关笔记，
    /// 而这个项目的底线是绝不静默丢弃用户写下的内容。
    ///
    /// 这条是打开浏览器跑端到端时抓出来的：删频道前时间线 4 条、删完 3 条。
    #[test]
    fn deleting_a_channel_moves_its_messages_to_the_inbox() {
        let s = store();
        let ch = s.create_channel("临时频道").unwrap();
        let m = s
            .create_message("频道里的一条", Some(&ch.id), None)
            .unwrap();

        {
            let conn = s.conn().unwrap();
            assert_eq!(
                messagenote_store::list_messages(&conn, Scope::Channel(&ch.id), 50, None)
                    .unwrap()
                    .items
                    .len(),
                1,
                "测试不能空跑：记录得真的在频道里"
            );
        }

        s.remove_channel(&ch.id).unwrap();

        let conn = s.conn().unwrap();
        assert!(
            messagenote_store::list_channels(&conn)
                .unwrap()
                .iter()
                .all(|c| c.id != ch.id),
            "频道本身应当被删掉"
        );

        let unfiled = messagenote_store::list_messages(&conn, Scope::Unfiled, 50, None).unwrap();
        assert_eq!(unfiled.items.len(), 1, "记录必须还在，而且回到收件箱");
        assert_eq!(unfiled.items[0].id, m.id);
        assert_eq!(unfiled.items[0].channel_id, "inbox");

        // 正文没变，所以检索也还找得到。
        // 注意这里走 `messagenote_store::search(&conn, …)` 而不是 `search_hits(&s, …)`：
        // 上面那个 `conn` 还握着锁，`Store` 的方法会再拿一次同一把锁 ——
        // `std::sync::Mutex` 不可重入，那不是变慢，是直接死锁。
        assert_eq!(
            messagenote_store::search_page(&conn, "频道里", 10, 0)
                .unwrap()
                .items
                .len(),
            1
        );
    }

    /// 服务端必须拦住"指向不存在的频道"的消息。
    ///
    /// 危害比"存了一条脏数据"大得多：客户端的 `message` 表有指向 `channel` 的
    /// 外键，那条插入失败会让**整批拉取回滚**，此后每次同步都从同一个地方失败。
    /// 受害的那台客户端看起来只是"同步一直失败"，找不到原因。
    #[test]
    fn a_change_pointing_at_a_missing_row_is_rejected() {
        let s = store();

        // 刻意不先推频道
        let err = s.push(&[msg_in("m1", "ch-nope", 100, "孤儿消息")]);
        assert!(err.is_err(), "指向不存在频道的消息必须被拒绝：{err:?}");

        // **整批**拒绝：什么都没落库，seq 也没动
        assert_eq!(s.max_seq().unwrap(), 0, "被拒绝的批次不该分配 seq");
        assert_eq!(s.pull(0, 100).unwrap().changes.len(), 0);

        // 同一个批次里先把频道推上去，就是合法的 —— 正常路径正是这样
        let ok = s.push(&[
            channel_change("ch-ok", "正常频道", "normal", 1, 50),
            msg_in("m1", "ch-ok", 100, "有主的消息"),
        ]);
        assert!(ok.is_ok(), "同批次里先有频道就该接受：{ok:?}");

        // 标签关联也走同一套校验
        let err = s.push(&[msg_tag_change("不存在的消息", "不存在的标签", 200)]);
        assert!(err.is_err(), "指向不存在消息的标签关联必须被拒绝：{err:?}");

        // 频道被删了（墓碑还在）不算"不存在" —— 外键仍然满足，
        // 判成错误会把"频道删了但消息还没同步到"这种正常时序卡住
        let ch = s.create_channel("临时").unwrap();
        let m = s.create_message("里面的一条", Some(&ch.id), None).unwrap();
        s.remove_channel(&ch.id).unwrap();
        let again = s.push(&[msg_in("m2", &ch.id, 300, "往墓碑频道里写")]);
        assert!(
            again.is_ok(),
            "频道的墓碑行还在，外键满足，不该当成引用错误：{again:?}"
        );
        let _ = m;
    }

    // ------------------------------------------------------------ 幂等写入
    //
    // 这一组是给网页端的离线队列准备的：断网时把"要记什么"排队，联网后重放，
    // 而重放天然会重试。没有幂等键的话，一次重试就是一条重复记录。

    /// 同一个 id 的创建请求重放多少遍，库里也只有一条，而且**不产生额外的变更**。
    #[test]
    fn creating_with_the_same_id_twice_writes_once() {
        let s = store();
        let id = "client-supplied-1";

        let first = s.create_message("第一版内容", None, Some(id)).unwrap();
        assert_eq!(first.id, id, "要用客户端给的 id，而不是服务端另生成一个");

        let seq = s.max_seq().unwrap();
        let log_len = s.pull(0, 1000).unwrap().changes.len();

        // 原样重放
        let again = s.create_message("第一版内容", None, Some(id)).unwrap();
        assert_eq!(again.id, id);
        assert_eq!(again.body, "第一版内容");

        assert_eq!(s.max_seq().unwrap(), seq, "重放不该占用新的 seq");
        assert_eq!(
            s.pull(0, 1000).unwrap().changes.len(),
            log_len,
            "重放不该往变更日志里再写一条 —— 否则别的设备会看到两次改动"
        );
        assert_eq!(s.timeline_stats().unwrap().total, 1, "库里只该有一条");
    }

    /// **重放不能把用户删掉的记录复活。**
    ///
    /// 这是幂等检查里最容易写错的一处：如果判的是"有没有**没被删**的那一行"，
    /// 那么"删除 → 队列重放"就会把记录变回来。删除是用户明确表达过的意图，
    /// 任何让它失效的路径都是数据完整性问题。
    #[test]
    fn replaying_a_deleted_create_does_not_resurrect_it() {
        let s = store();
        let id = "client-supplied-2";

        s.create_message("准备被删掉的", None, Some(id)).unwrap();
        s.remove_message(id).unwrap();
        assert_eq!(s.timeline_stats().unwrap().total, 0, "先确认真的删掉了");

        // 队列重放同一条请求
        let replayed = s.create_message("准备被删掉的", None, Some(id)).unwrap();
        assert_eq!(replayed.id, id, "调用方仍然该拿回它要的那条记录");
        assert_eq!(
            s.timeline_stats().unwrap().total,
            0,
            "重放**不能**让删掉的记录复活"
        );
    }

    /// 不给 id 时每次都是新记录 —— 老行为不能被这次改动碰坏。
    #[test]
    fn creating_without_an_id_always_makes_a_new_message() {
        let s = store();
        let a = s.create_message("完全一样的内容", None, None).unwrap();
        let b = s.create_message("完全一样的内容", None, None).unwrap();
        assert_ne!(a.id, b.id, "没给 id 就该各生成一个");
        assert_eq!(s.timeline_stats().unwrap().total, 2);
    }

    /// 形状不合法的幂等键要**拒绝**，不能当成"没给"。
    ///
    /// 当成"没给"的话，一个传空串的客户端会得到**非幂等**的写入，
    /// 而它以为自己传了幂等键 —— 最坏的一类失败：行为与契约不符，且不报错。
    #[test]
    fn a_malformed_client_id_is_rejected_not_ignored() {
        let s = store();
        assert!(s.create_message("x", None, Some("")).is_err(), "空串要拒绝");
        assert!(
            s.create_message("x", None, Some("   ")).is_err(),
            "全是空白也算空"
        );
        assert!(
            s.create_message("x", None, Some(&"z".repeat(129))).is_err(),
            "过长要拒绝"
        );
        assert_eq!(
            s.timeline_stats().unwrap().total,
            0,
            "被拒绝的请求一条也不该落库"
        );
    }
}
