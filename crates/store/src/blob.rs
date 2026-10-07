//! 附件字节的存取。
//!
//! ## 为什么字节进 SQLite，而不是放一个文件夹
//!
//! README 推荐的自建服务端备份方式是 **Litestream**，它只跟**一个数据库文件**
//! 走。把图片放到 `attachments/` 目录里，备份会**静默漏掉它们** —— 恢复之后
//! 笔记都在，图全没了，而且是在你真正需要恢复的那天才发现。
//!
//! 客户端这边同理：`messagenote.sqlite` 一个文件就是全部状态，复制它就是备份。
//!
//! 代价是库文件变大。所以有 [`MAX_ATTACHMENT_BYTES`](messagenote_core::attachment::MAX_ATTACHMENT_BYTES)
//! 这个上限 —— 这条路径上每一层（HTTP 请求体、IPC、SQLite BLOB）都会把整个
//! 字节数组握在手里，真正的流式传输要改动每一层。
//!
//! ## 表在两端是同形的
//!
//! 这个模块的每个函数都只碰 `sha256 / size / mime / created_at / bytes`
//! 这五列。客户端多一个 `uploaded` 列（"还没传上去"的标记），但那是客户端
//! 自己的事，这里的 SQL 一个字都不提它 —— 和 `browse.rs` 遵循同一条契约：
//! **只写"在两种 schema 上都成立"的查询**。

use rusqlite::{params, Connection, OptionalExtension};

use messagenote_core::attachment;

/// 附件的一行元数据（不含字节）。
///
/// 列表和队列用得上它；真正取字节用 [`get_blob`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    pub sha256: String,
    pub size: i64,
    pub mime: String,
    pub created_at: i64,
    /// 字节在不在本地。`false` 表示"知道有这么个附件，但还没拿到"。
    pub present: bool,
}

/// 存一份字节。已存在则**什么都不做**并返回 `false`。
///
/// 幂等是内容寻址白送的性质：名字就是内容的哈希，所以"这个名字已经在了"
/// 必然意味着"同样的字节已经在了"，不需要比较、也不需要覆盖。
///
/// **例外只有一种**：那行存在但字节是空的（占位行）。这时候本地握着的
/// 这份字节要补进去 —— 原来 `DO NOTHING` 把这种也丢了，于是本地明明有、
/// 却永远不会被上传。见下面冲突子句上的说明。
pub fn put_blob(
    conn: &Connection,
    sha256: &str,
    bytes: &[u8],
    created_at: i64,
) -> rusqlite::Result<bool> {
    // 内容寻址的前提是名字真的是内容的哈希。不校验的话，一个算错的调用方
    // 会把错误的字节永久钉在一个名字上 —— 而所有设备都信任那个名字。
    debug_assert_eq!(
        attachment::sha256_hex(bytes),
        sha256,
        "存进来的字节和它的 sha256 对不上"
    );

    let mime = attachment::resolve_mime(bytes);
    // **冲突时只补空的那一半。**
    //
    // 原来这里是 `DO NOTHING`，为的是"迟到的、不带字节的登记别把已下好的图擦掉"
    // —— `register_placeholder` 那边确实是 `DO UPDATE ... WHERE bytes IS NULL`。
    // 但 `DO NOTHING` 在这里同样把**反过来的**一种情况也一并丢掉了：
    //
    //   别的设备先同步过来一条引用了这张图的消息 → 本地建了一行 `bytes IS NULL`
    //   的占位 → 用户在本机粘上**同一张图** → put_blob 撞上占位行，直接放弃。
    //
    // 本地明明握有逐字节相同的副本，却被丢弃，行永远空着。而 `pending_uploads`
    // 只选有字节的行，所以这份数据**永远不会被上传** —— 只能等源设备上传后
    // 由下载队列自愈；源设备要是没了，这张图在所有设备上永久损坏。
    //
    // `WHERE attachment.bytes IS NULL` 同时守住两半：占位行补上字节，
    // 而已经有字节的行不被后来者覆盖（内容寻址下两者必然相同，覆盖也无害，
    // 但守住"不覆盖"让意图写在代码里）。
    let changed = conn.execute(
        "INSERT INTO attachment (sha256, size, mime, created_at, bytes)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(sha256) DO UPDATE SET
             bytes = excluded.bytes,
             size  = excluded.size,
             mime  = excluded.mime
         WHERE attachment.bytes IS NULL",
        params![sha256, bytes.len() as i64, mime, created_at, bytes],
    )?;
    Ok(changed > 0)
}

/// 登记一个"知道有、但还没有字节"的附件。
///
/// 拉取时正文里出现了本地没有的 `attachment:<sha>`，就走这里建一行空壳，
/// 之后由 [`pending_downloads`] 排队去取。
///
/// 刻意**不覆盖已有的字节**：`DO NOTHING` 而不是 `DO UPDATE`。否则一个
/// 迟到的、不带字节的登记会把已经下好的图擦掉。
pub fn register_placeholder(
    conn: &Connection,
    sha256: &str,
    created_at: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO attachment (sha256, size, mime, created_at, bytes)
         VALUES (?1, 0, '', ?2, NULL)
         ON CONFLICT(sha256) DO UPDATE SET
             bytes = excluded.bytes,
             size  = excluded.size,
             mime  = excluded.mime
         WHERE attachment.bytes IS NULL",        params![sha256, created_at],
    )?;
    Ok(())
}

/// 写入下载到的字节，顺带补上从服务端拿到的类型。
pub fn fill_blob(
    conn: &Connection,
    sha256: &str,
    bytes: &[u8],
    mime: &str,
    created_at: i64,
) -> rusqlite::Result<()> {
    debug_assert_eq!(
        attachment::sha256_hex(bytes),
        sha256,
        "下载回来的字节和请求的 sha256 对不上"
    );

    // 类型以**字节嗅探**为准，服务端给的只是提示。这样即使服务端被换掉、
    // 或者它的 Content-Type 被中间人改过，本地也不会把一个 HTML 当成图片去渲染。
    let mime = if attachment::sniff_mime(bytes).is_some() {
        attachment::resolve_mime(bytes)
    } else if mime.is_empty() {
        attachment::FALLBACK_MIME
    } else {
        mime
    };

    conn.execute(
        "UPDATE attachment SET bytes = ?2, size = ?3, mime = ?4 WHERE sha256 = ?1",
        params![sha256, bytes, bytes.len() as i64, mime],
    )?;
    let _ = created_at;
    Ok(())
}

/// 取字节和类型。没有字节（或整行都不存在）时返回 `None`。
pub fn get_blob(conn: &Connection, sha256: &str) -> rusqlite::Result<Option<(String, Vec<u8>)>> {
    conn.query_row(
        "SELECT mime, bytes FROM attachment WHERE sha256 = ?1 AND bytes IS NOT NULL",
        params![sha256],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
    )
    .optional()
}

/// 字节在不在本地。
pub fn has_blob(conn: &Connection, sha256: &str) -> rusqlite::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM attachment WHERE sha256 = ?1 AND bytes IS NOT NULL",
            params![sha256],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// 元数据（不含字节）。
pub fn blob_meta(conn: &Connection, sha256: &str) -> rusqlite::Result<Option<BlobMeta>> {
    conn.query_row(
        "SELECT sha256, size, mime, created_at, bytes IS NOT NULL
           FROM attachment WHERE sha256 = ?1",
        params![sha256],
        |r| {
            Ok(BlobMeta {
                sha256: r.get(0)?,
                size: r.get(1)?,
                mime: r.get(2)?,
                created_at: r.get(3)?,
                present: r.get::<_, i64>(4)? != 0,
            })
        },
    )
    .optional()
}

/// 把正文里引用到的、本地**完全没有记录**的附件登记成空壳。
///
/// 为什么需要它：附件不进变更日志（内容不可变，套 HLC/LWW 没有意义），
/// 所以"对端有这么一个附件"这件事只能从**正文**里读出来。见
/// [`messagenote_core::attachment::referenced_shas`]。
///
/// 返回新登记的数量。
pub fn register_referenced(
    conn: &Connection,
    body: &str,
    created_at: i64,
) -> rusqlite::Result<usize> {
    let mut added = 0;
    for sha in attachment::referenced_shas(body) {
        // 已经有行就不动它 —— 尤其是别把已经下好的字节变成空壳
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM attachment WHERE sha256 = ?1",
                params![sha],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            register_placeholder(conn, &sha, created_at)?;
            added += 1;
        }
    }
    Ok(added)
}

/// 某个附件还被多少条没被删的记录引用着。
///
/// 判据是"完整 sha 出现在正文里"，而不是"正文里有 attachment: 前缀" ——
/// 前缀可能只是个自然语言的巧合，而 64 位十六进制串不会。
pub fn reference_count(conn: &Connection, sha256: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM message
          WHERE deleted_at IS NULL AND body LIKE '%' || ?1 || '%'",
        params![sha256],
        |r| r.get(0),
    )
}

/// 回收已经没有任何记录引用的附件字节。
///
/// **刻意不自动跑。** 它是 O(附件数 × 记录数) 的扫描，而且判错的代价是
/// 永久删掉用户的图。做成显式动作（命令 / 定时任务），并且：
///
/// - 只删**有字节**的行。空壳行是"待下载"，不是垃圾。
/// - 保留行本身（`bytes = NULL`）而不是 DELETE：这样它变成"待下载"，
///   万一判错了（比如引用它的记录还没同步过来），下次同步会把图取回来，
///   而不是永久消失。
///
/// 返回清掉的字节数。
pub fn gc_unreferenced(conn: &Connection) -> rusqlite::Result<i64> {
    // 先算总量，GC 完才知道省了多少
    let before: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size), 0) FROM attachment WHERE bytes IS NOT NULL",
        [],
        |r| r.get(0),
    )?;

    conn.execute(
        "UPDATE attachment SET bytes = NULL
          WHERE bytes IS NOT NULL
            AND NOT EXISTS (
              SELECT 1 FROM message
               WHERE message.deleted_at IS NULL
                 AND message.body LIKE '%' || attachment.sha256 || '%'
            )",
        [],
    )?;

    let after: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size), 0) FROM attachment WHERE bytes IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    Ok(before - after)
}
