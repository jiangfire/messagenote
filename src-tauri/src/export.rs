//! 把整个库导出成一棵 Markdown 目录树。
//!
//! **格式在 `messagenote_core::export` 里，这里只做 I/O**：枚举记录、建目录、
//! 写文件。这样桌面端和（将来的）网页端导出不会各长出一个格式。
//!
//! ## 字节不跨 IPC
//!
//! 附件字节由这里**直接从 SQLite 读出来写盘**，不经过前端。一份图多的库有几十
//! 上百 MB，让它们来回搬一遍 Tauri IPC 既慢又没必要 —— 前端只出一个目录路径
//! 和一个 UTC 偏移。
//!
//! ## 元数据和字节分两步查
//!
//! 先 `blob_meta`（轻，不含字节）决定每个引用的相对路径，之后再逐个读字节写盘。
//! 合成一步就得把所有附件同时捏在内存里 —— 一份图多的库能到几百 MB。
//!
//! ## 筛选发生在这一层，不下推到 `list_messages`
//!
//! 频道和标签能表达成共享层的 [`db::Scope`]（那套「频道管归属、标签管横切」的
//! 定义只有一份），**时间范围不能**：导出要的是一个**区间**，而 `list_messages`
//! 只认下界 `since`。两处各管一半的后果是"这个区间到底含不含端点"会有两个答案，
//! 而用户看到的只是"少了一条"。所以时间在这里一次筛完，端点都算数。
//!
//! 代价是**窄区间导出仍然会把整表翻一遍**（筛完才丢）。这是有意的取舍：
//! 正确性只有一份实现，而导出是一次性的用户动作，翻表的成本全量导出本来也要付。
//! 真要优化，把 `since` 下推是**安全**的（`list_messages` 的下界同样是闭的），
//! 但那会让"下界"重新变成两处各写一遍 —— 等它真的成为瓶颈再说。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use base64::Engine as _;
use messagenote_core::attachment;
use messagenote_core::export::{self, ExportItem};
use messagenote_core::models::Message;

use crate::db;
use crate::error::AppResult;

/// 附件目录名（导出根下）。
const ATTACHMENTS_DIR: &str = "attachments";

/// 枚举时一页取多少条。`list_messages` 自己会把上限收到 500。
const PAGE: i64 = 500;

/// 导出筛选。四个字段全空 = 全量导出。
///
/// `Deserialize` 是给 IPC 用的：前端直接传一个对象（或 `null`），而不是四个
/// 各自可为空的参数 —— 这四个字段是一件事（"导哪一部分"），不是一个签名。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportFilter {
    /// 只导这个频道。收件箱也是一个频道（id 固定为 `inbox`），不用特判。
    pub channel_id: Option<String>,
    /// 只导打了这个标签的记录。标签是横切的，与频道正交 —— 两个都给时取交集。
    pub tag: Option<String>,
    /// 时间范围下界（epoch 毫秒，**含端点**）。
    pub from_ms: Option<i64>,
    /// 时间范围上界（epoch 毫秒，**含端点**）。
    pub to_ms: Option<i64>,
}

impl ExportFilter {
    /// 时间范围在界面上可能就是同一个日期（起 = 止），这里不特判：
    /// `from == to` 自然就是"那一毫秒"，而调用方给的是当天的 0 点和 23:59:59.999。
    ///
    /// 频道**只在"标签也给了"的时候**才在这里补一刀：那时 SQL 走的是标签
    /// （见 `selected_messages`），频道条件就落到了这一层。
    fn matches(&self, m: &Message) -> bool {
        if let Some(from) = self.from_ms {
            if m.created_at < from {
                return false;
            }
        }
        if let Some(to) = self.to_ms {
            if m.created_at > to {
                return false;
            }
        }
        if self.tag.is_some() {
            if let Some(id) = self.channel_id.as_deref() {
                // `channel_id` 是非空列，所以这就是 SQL 里那句 `m.channel_id = ?`
                return m.channel_id == id;
            }
        }
        true
    }
}

/// 频道名查不到时的落点。
///
/// 会走到这儿意味着记录指向了一个本地没有的频道（比如频道还没从对端同步过来）。
/// 用一句话说明，而不是丢掉这条记录 —— 导出里**少一条笔记**比多一个这样的目录
/// 严重得多。
const ORPHAN_CHANNEL: &str = "未归档";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
    pub messages: usize,
    pub attachments: usize,
    /// 正文引用了、但本地**还没有字节**的附件数。
    ///
    /// 这些引用在导出物里**原样保留** `attachment:<sha>`，不编一个指不到东西的
    /// 相对路径。界面上应该把它告诉用户（"有 N 张图本地还没有，没能带出来"），
    /// 否则他只会以为导出漏了。
    pub missing_attachments: usize,
    pub channels: usize,
}

/// 单条记录渲染成 Markdown（"复制这一条"用）。
///
/// `Ok(None)` 表示这条不在了（被删掉或 id 不对）—— 调用方据此给用户一句
/// "这条记录已经不在了"，而不是抛一句数据库错误。
///
/// **单条复制与全量导出的差别只有附件怎么放**：这里把图内联成 data URI
/// （剪贴板里没有"文件"这个概念，只有文本），导出那边写相对路径 + 单独的文件。
/// 格式、front-matter、消毒都走同一份实现。
pub fn render_one(
    conn: &Connection,
    id: &str,
    utc_offset_minutes: i32,
) -> AppResult<Option<String>> {
    let row = conn
        .query_row(
            "SELECT id, channel_id, body, created_at, updated_at
               FROM message WHERE id = ?1 AND deleted_at IS NULL",
            [id],
            db::row_to_message,
        )
        .optional()?;
    let Some(mut m) = row else {
        return Ok(None);
    };
    db::attach_tags(conn, std::slice::from_mut(&mut m))?;

    let channels = channel_names(conn)?;
    let channel = channels
        .get(&m.channel_id)
        .map(String::as_str)
        .unwrap_or(ORPHAN_CHANNEL);

    let item = ExportItem {
        channel,
        tags: &m.tags,
        created_at: m.created_at,
        updated_at: m.updated_at,
        body: &m.body,
    };
    Ok(Some(export::render_markdown(
        &item,
        utc_offset_minutes,
        // **单条复制把图内联成 data URI。** 剪贴板里带不走文件，而留着
        // `attachment:<sha>` 对别的程序就是一句看不懂的话 —— 复制出去贴到任何
        // 能画 Markdown 的地方（GitHub、Obsidian、LLM 对话框），图都还在。
        //
        // 全量导出那边**刻意不同**：它有磁盘可用，写相对路径、图单独成文件，
        // 所以导出物是可读、可 diff 的文本。两处的差别来自"有没有地方放字节"，
        // 不是两套格式。
        |sha| {
            let (_stored_mime, bytes) = db::read_attachment(conn, sha).ok()?;
            // 类型**由字节嗅探**，不采信库里存的那一列 —— 和上传路径同一条规矩。
            let mime = attachment::resolve_mime(&bytes);
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Some(format!("data:{mime};base64,{b64}"))
        },
    )))
}

/// 把整库（或筛选出的那一部分）导出到 `dir`（不存在就建）。
pub fn export_to(
    conn: &Connection,
    dir: &Path,
    utc_offset_minutes: i32,
    filter: &ExportFilter,
) -> AppResult<ExportSummary> {
    let channels = channel_names(conn)?;
    let messages = selected_messages(conn, filter)?;

    // 第一遍：只碰元数据，决定每个被引用附件的相对路径。
    let mut paths: HashMap<String, String> = HashMap::new();
    let mut missing: HashSet<String> = HashSet::new();
    for m in &messages {
        for sha in attachment::referenced_shas(&m.body) {
            if paths.contains_key(&sha) || missing.contains(&sha) {
                continue;
            }
            match db::blob::blob_meta(conn, &sha)? {
                Some(meta) if meta.present => {
                    let name = format!("{sha}.{}", attachment::extension_for(&meta.mime));
                    // 记录落在 `<根>/<频道>/`，附件在 `<根>/attachments/`，
                    // 所以从记录回到附件是往上一级。
                    paths.insert(sha, format!("../{ATTACHMENTS_DIR}/{name}"));
                }
                // 只有占位行（bytes 为 NULL，等着下载）。
                _ => {
                    missing.insert(sha);
                }
            }
        }
    }

    // 第二遍：写记录。
    //
    // 同名去重是**必须的**：两条记录完全可能算出同一个文件名（同一分钟、
    // 开头那句又一样），覆盖就等于**静默丢掉一条笔记**。
    let mut used: HashSet<String> = HashSet::new();
    let mut written = 0usize;
    for m in &messages {
        let channel = channels
            .get(&m.channel_id)
            .map(String::as_str)
            .unwrap_or(ORPHAN_CHANNEL);
        let dir_name = export::sanitize_segment(channel);
        let sub = dir.join(&dir_name);
        fs::create_dir_all(&sub)?;

        let item = ExportItem {
            channel,
            tags: &m.tags,
            created_at: m.created_at,
            updated_at: m.updated_at,
            body: &m.body,
        };
        let base = export::file_name(&item, utc_offset_minutes);
        let name = unique_name(&mut used, &dir_name, &base);
        let md = export::render_markdown(&item, utc_offset_minutes, |sha| paths.get(sha).cloned());
        fs::write(sub.join(name), md)?;
        written += 1;
    }

    // 第三遍：拷字节。名字用第一遍算出来的那一份，**不在这里重算** ——
    // 重算就有可能和正文里写的路径对不上，而那种不一致只有打开文件才发现。
    let mut copied = 0usize;
    if !paths.is_empty() {
        let att_dir = dir.join(ATTACHMENTS_DIR);
        fs::create_dir_all(&att_dir)?;
        for (sha, rel) in &paths {
            let name = rel.rsplit('/').next().unwrap_or(sha.as_str());
            let (_mime, bytes) = db::read_attachment(conn, sha)?;
            fs::write(att_dir.join(name), bytes)?;
            copied += 1;
        }
    }

    Ok(ExportSummary {
        messages: written,
        attachments: copied,
        missing_attachments: missing.len(),
        channels: used
            .iter()
            .filter_map(|k| k.split('/').next())
            .collect::<HashSet<_>>()
            .len(),
    })
}

/// 在同一个目录里挑一个没被用过的文件名：重名就依次加 `-2`、`-3`。
///
/// `dir_name` 参与判重，因为**判重是按目录来的** —— 不同频道下的同名文件
/// 本来就该各自保留。
fn unique_name(used: &mut HashSet<String>, dir_name: &str, base: &str) -> String {
    if used.insert(format!("{dir_name}/{base}")) {
        return base.to_string();
    }
    let stem = base.strip_suffix(".md").unwrap_or(base);
    let mut n = 2;
    loop {
        let cand = format!("{stem}-{n}.md");
        if used.insert(format!("{dir_name}/{cand}")) {
            return cand;
        }
        n += 1;
    }
}

fn channel_names(conn: &Connection) -> AppResult<HashMap<String, String>> {
    Ok(db::list_channels(conn)?
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect())
}

/// 按 `(created_at, id)` 顺序把筛选命中的记录翻完。
///
/// 用游标翻页而不是一次性 `SELECT *`：`list_messages` 的上限是 500，
/// 而且它维护的是和界面同一个查询 —— 这里就不该再写一份取数的 SQL。
///
/// 翻页拿到的每一页都可能是"被筛掉一部分"的，所以游标取的是**原始页的最后一条**，
/// 而不是筛完之后的 —— 否则筛空的那些页会让翻页提前停住，后面还有记录也不翻了。
fn selected_messages(conn: &Connection, filter: &ExportFilter) -> AppResult<Vec<Message>> {
    // **标签优先。** 两个筛选都给的时候 SQL 走标签、频道在 `matches` 里补 ——
    // 这样"什么算有这个标签"只有 `Scope::Tag` 一份定义。反过来（SQL 走频道、
    // 标签在 Rust 里比 `m.tags`）就成了两个谓词：`Scope::Tag` 只看
    // `message_tag.deleted_at`，而 `m.tags` 还要求 `tag.deleted_at IS NULL`，
    // 于是一条"标签行被软删、消息上的关联还在"的记录会在两种筛法下得到
    // **两个答案** —— 而用户看到的只是"少了一条"。
    let scope = if let Some(tag) = filter.tag.as_deref() {
        db::Scope::Tag(tag)
    } else if let Some(id) = filter.channel_id.as_deref() {
        db::Scope::Channel(id)
    } else {
        db::Scope::All
    };

    let mut out: Vec<Message> = Vec::new();
    let mut before: Option<db::Cursor> = None;
    loop {
        let page = db::list_messages(conn, scope, PAGE, before.as_ref(), None)?;
        if page.items.is_empty() {
            break;
        }
        before = page.items.last().map(db::Cursor::before);
        let more = page.has_more;
        out.extend(page.items);
        if !more {
            break;
        }
    }
    db::attach_tags(conn, &mut out)?;
    out.retain(|m| filter.matches(m));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mn-export-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 一张**真的会被嗅探成 png** 的最小字节。
    ///
    /// 不需要是能解码的图：这里验的是"类型由字节决定"这条路径，
    /// 而 `sniff_mime` 认的就是开头那 8 个字节。
    fn png_bytes() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0u8; 24]);
        v
    }

    fn only_md(dir: &Path) -> std::path::PathBuf {
        let mut found: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
            .collect();
        assert_eq!(found.len(), 1, "应当正好有一个 .md：{found:?}");
        found.pop().unwrap()
    }

    #[test]
    fn a_full_export_writes_a_tree_whose_attachment_path_really_resolves() {
        let db = db::open_memory("test-device").unwrap();
        let conn = db.conn().unwrap();

        let png = png_bytes();
        let sha = db::save_attachment(&conn, &png).unwrap();
        let ch = db::create_channel(&conn, "项目A").unwrap();
        let m = db::append_message(
            &conn,
            &format!("会议记录\n![截图](attachment:{sha})"),
            Some(&ch.id),
        )
        .unwrap();
        db::set_message_tags(&conn, &m.id, &["重要".to_string()]).unwrap();

        let dir = tmpdir("full");
        let sum = export_to(&conn, &dir, 480, &ExportFilter::default()).unwrap();
        assert_eq!(sum.messages, 1);
        assert_eq!(sum.attachments, 1);
        assert_eq!(sum.missing_attachments, 0);
        assert_eq!(sum.channels, 1);

        let md_path = only_md(&dir.join("项目A"));
        let md = fs::read_to_string(&md_path).unwrap();

        assert!(md.contains("channel: \"项目A\""), "实际：{md}");
        assert!(md.contains("tags: [\"重要\"]"), "实际：{md}");
        assert!(md.contains("![截图](../attachments/"), "实际：{md}");
        assert!(!md.contains("attachment:"), "旧的引用不该留下：{md}");

        // **路径真的指得到字节。** 只断言"字符串长这样"是不够的 ——
        // 正是那种断言在文件名算错时照样会通过。
        let rel = md
            .split("](../")
            .nth(1)
            .and_then(|s| s.split(')').next())
            .expect("正文里应当有一个相对路径");
        let bytes = fs::read(dir.join(rel)).unwrap();
        assert_eq!(bytes, png, "导出的附件字节必须和库里那份逐字节一致");
        assert!(rel.ends_with(".png"), "png 不该退化成别的扩展名：{rel}");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_attachment_whose_bytes_are_not_local_stays_a_reference_and_is_counted() {
        let db = db::open_memory("test-device").unwrap();
        let conn = db.conn().unwrap();

        // 一条**只存在于正文里**的引用：本地没有 attachment 行（对端还没传来）。
        let sha = "d".repeat(64);
        db::append_message(&conn, &format!("看图 attachment:{sha}"), None).unwrap();

        let dir = tmpdir("missing");
        let sum = export_to(&conn, &dir, 0, &ExportFilter::default()).unwrap();
        assert_eq!(sum.messages, 1);
        assert_eq!(sum.attachments, 0);
        assert_eq!(sum.missing_attachments, 1, "取不到的附件要被数出来告诉用户");

        let md = fs::read_to_string(only_md(&dir.join("收件箱"))).unwrap();
        assert!(
            md.contains(&format!("attachment:{sha}")),
            "取不到字节时应当保留原引用，而不是编一个死链：{md}"
        );
        assert!(
            !dir.join(ATTACHMENTS_DIR).exists(),
            "没有字节就不该建附件目录"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn colliding_file_names_get_a_suffix_instead_of_overwriting_a_note() {
        let mut used = HashSet::new();
        let base = "2026-01-01-0000 会议.md";
        assert_eq!(unique_name(&mut used, "项目A", base), base);
        assert_eq!(
            unique_name(&mut used, "项目A", base),
            "2026-01-01-0000 会议-2.md"
        );
        assert_eq!(
            unique_name(&mut used, "项目A", base),
            "2026-01-01-0000 会议-3.md"
        );
        // 判重按目录来：另一个频道下的同名文件本来就该各自保留
        assert_eq!(unique_name(&mut used, "项目B", base), base);
    }

    /// 直接改一条记录的 `created_at`。
    ///
    /// 时间范围筛选要验的是**端点算不算数**，那就必须能把一条记录钉在一个确切的
    /// 时刻上；靠 `append_message` 拿到的"现在"只能验个大概。
    fn pin_created_at(conn: &Connection, id: &str, ms: i64) {
        conn.execute(
            "UPDATE message SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![id, ms],
        )
        .unwrap();
    }

    /// 三个真实量级的时刻：相隔一天，而且都是 2023 年之后的毫秒值。
    /// 玩具数（1、2、3）测不出"把毫秒当成秒"这类单位错误。
    const T1: i64 = 1_700_000_000_000;
    const T2: i64 = 1_700_086_400_000;
    const T3: i64 = 1_700_172_800_000;

    fn md_files_in(dir: &Path) -> Vec<String> {
        if !dir.exists() {
            return Vec::new();
        }
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_export_filtered_by_channel_leaves_the_other_channels_out() {
        let db = db::open_memory("test-device").unwrap();
        let conn = db.conn().unwrap();
        let a = db::create_channel(&conn, "项目A").unwrap();
        let b = db::create_channel(&conn, "项目B").unwrap();
        db::append_message(&conn, "A 里的", Some(&a.id)).unwrap();
        db::append_message(&conn, "B 里的", Some(&b.id)).unwrap();
        db::append_message(&conn, "收件箱里的", None).unwrap();

        let dir = tmpdir("by-channel");
        let filter = ExportFilter {
            channel_id: Some(a.id.clone()),
            ..Default::default()
        };
        let sum = export_to(&conn, &dir, 0, &filter).unwrap();

        assert_eq!(sum.messages, 1);
        assert_eq!(sum.channels, 1);
        let md = fs::read_to_string(only_md(&dir.join("项目A"))).unwrap();
        assert!(md.contains("A 里的"), "实际：{md}");
        // "没导出"和"导出了但目录是空的"要分得清：别的频道目录根本不该被建出来
        assert!(!dir.join("项目B").exists(), "别的频道不该有目录");
        assert!(
            !dir.join("收件箱").exists(),
            "没收件箱的记录就不该建收件箱目录"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    /// 收件箱也是一个频道，`channel_id = "inbox"` 能直接筛。
    ///
    /// 这一条看着像废话，但导出面板里"收件箱"就是下拉里的一个选项，
    /// 而它的 id 是**固定字符串**而不是随机 UUID —— 哪天有人给它加个特判、
    /// 或者把固定 id 换掉，"只导未归档的"会**静默**变成导出一棵空目录树。
    #[test]
    fn the_inbox_is_selectable_as_a_channel() {
        let db = db::open_memory("test-device").unwrap();
        let conn = db.conn().unwrap();
        let a = db::create_channel(&conn, "项目A").unwrap();
        db::append_message(&conn, "已归档的", Some(&a.id)).unwrap();
        db::append_message(&conn, "还在收件箱的", None).unwrap();

        let dir = tmpdir("inbox");
        let filter = ExportFilter {
            channel_id: Some("inbox".into()),
            ..Default::default()
        };
        let sum = export_to(&conn, &dir, 0, &filter).unwrap();

        assert_eq!(sum.messages, 1);
        let md = fs::read_to_string(only_md(&dir.join("收件箱"))).unwrap();
        assert!(md.contains("还在收件箱的"), "实际：{md}");
        assert!(!dir.join("项目A").exists(), "别的频道不该被导出来");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn channel_and_tag_together_are_an_intersection() {
        let db = db::open_memory("test-device").unwrap();
        let conn = db.conn().unwrap();
        let a = db::create_channel(&conn, "项目A").unwrap();
        let b = db::create_channel(&conn, "项目B").unwrap();

        let in_a_tagged = db::append_message(&conn, "A 且重要", Some(&a.id)).unwrap();
        db::set_message_tags(&conn, &in_a_tagged.id, &["重要".into()]).unwrap();
        let in_a_plain = db::append_message(&conn, "A 但没标签", Some(&a.id)).unwrap();
        db::set_message_tags(&conn, &in_a_plain.id, &["次要".into()]).unwrap();
        let in_b_tagged = db::append_message(&conn, "B 且重要", Some(&b.id)).unwrap();
        db::set_message_tags(&conn, &in_b_tagged.id, &["重要".into()]).unwrap();

        let dir = tmpdir("channel-and-tag");
        let filter = ExportFilter {
            channel_id: Some(a.id.clone()),
            tag: Some("重要".into()),
            ..Default::default()
        };
        let sum = export_to(&conn, &dir, 0, &filter).unwrap();

        // `Scope` 表达不了"这个频道里带这个标签的"，所以这一条真的会被验到：
        // 少筛一半就会把"B 且重要"或"A 但没标签"也导出来
        assert_eq!(
            sum.messages,
            1,
            "实际：{:?}",
            md_files_in(&dir.join("项目A"))
        );
        let md = fs::read_to_string(only_md(&dir.join("项目A"))).unwrap();
        assert!(md.contains("A 且重要"), "实际：{md}");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_time_range_includes_both_endpoints() {
        let db = db::open_memory("test-device").unwrap();
        let conn = db.conn().unwrap();
        let a = db::create_channel(&conn, "项目A").unwrap();

        let first = db::append_message(&conn, "第一天", Some(&a.id)).unwrap();
        let middle = db::append_message(&conn, "第二天", Some(&a.id)).unwrap();
        let last = db::append_message(&conn, "第三天", Some(&a.id)).unwrap();
        pin_created_at(&conn, &first.id, T1);
        pin_created_at(&conn, &middle.id, T2);
        pin_created_at(&conn, &last.id, T3);

        // 起止都取在**已有记录的整点上**：区间是闭区间，两端那两条都要在
        let dir = tmpdir("range-inclusive");
        let filter = ExportFilter {
            from_ms: Some(T1),
            to_ms: Some(T2),
            ..Default::default()
        };
        let sum = export_to(&conn, &dir, 0, &filter).unwrap();

        assert_eq!(
            sum.messages,
            2,
            "实际：{:?}",
            md_files_in(&dir.join("项目A"))
        );
        let bodies: String = md_files_in(&dir.join("项目A"))
            .iter()
            .map(|n| fs::read_to_string(dir.join("项目A").join(n)).unwrap())
            .collect();
        assert!(bodies.contains("第一天"), "下界那一条要算数：{bodies}");
        assert!(bodies.contains("第二天"), "上界那一条要算数：{bodies}");
        assert!(!bodies.contains("第三天"), "上界之外的不该进来：{bodies}");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_filter_that_matches_nothing_writes_nothing_and_says_zero() {
        let db = db::open_memory("test-device").unwrap();
        let conn = db.conn().unwrap();
        db::append_message(&conn, "一条普通的记录", None).unwrap();

        // 谁都没有这个标签。空结果是一个**正常的答案**，不是错误 ——
        // 界面据此说"这个筛选下没有记录"，而不是摔一句失败。
        let dir = tmpdir("no-match");
        let filter = ExportFilter {
            tag: Some("不存在的标签".into()),
            ..Default::default()
        };
        let sum = export_to(&conn, &dir, 0, &filter).unwrap();

        assert_eq!(sum.messages, 0);
        assert_eq!(sum.channels, 0);
        assert_eq!(sum.attachments, 0);
        assert!(md_files_in(&dir.join("收件箱")).is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn render_one_inlines_attachments_so_the_copy_is_self_contained() {
        let db = db::open_memory("test-device").unwrap();
        let conn = db.conn().unwrap();

        let png = png_bytes();
        let sha = db::save_attachment(&conn, &png).unwrap();
        let m = db::append_message(&conn, &format!("截图 attachment:{sha}"), None).unwrap();

        let md = render_one(&conn, &m.id, 0).unwrap().expect("记录应当存在");
        // 剪贴板里带不走文件，而 `attachment:<sha>` 对别的程序是一句看不懂的话。
        assert!(
            !md.contains("attachment:"),
            "不该留下别的程序看不懂的引用：{md}"
        );

        // **把 data URI 解回来，断言字节一致。** 只断言"有个
        // data:image/png;base64," 是弱断言 —— base64 编码写错时它照样通过。
        let b64 = md
            .split("base64,")
            .nth(1)
            .expect("正文里应当有一个 data URI")
            .split_whitespace()
            .next()
            .unwrap()
            .trim_end_matches(')');
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(decoded, png, "解回来的字节必须和库里那份逐字节一致");
        assert!(md.contains("data:image/png;base64,"), "类型由字节嗅探决定");

        assert!(md.contains("channel: \"收件箱\""), "实际：{md}");
        assert!(md.contains("created: 20"), "实际：{md}");
    }

    #[test]
    fn render_one_keeps_the_raw_reference_when_the_bytes_are_not_local() {
        let db = db::open_memory("test-device").unwrap();
        let conn = db.conn().unwrap();

        // 只有占位行，字节还没下载到。编不出 data URI，那就**留着原引用** ——
        // 它至少诚实地说明"这里本来有张图"，而不是一条指向虚空的路径。
        let sha = "e".repeat(64);
        let m = db::append_message(&conn, &format!("看图 attachment:{sha}"), None).unwrap();

        let md = render_one(&conn, &m.id, 0).unwrap().expect("记录应当存在");
        assert!(md.contains(&format!("attachment:{sha}")), "实际：{md}");
    }

    #[test]
    fn render_one_returns_none_for_a_record_that_is_gone() {
        let db = db::open_memory("test-device").unwrap();
        let conn = db.conn().unwrap();
        assert!(render_one(&conn, "不存在的-id", 0).unwrap().is_none());

        // 软删除之后也算"不在了"
        let m = db::append_message(&conn, "待删", None).unwrap();
        db::delete_message(&conn, &m.id).unwrap();
        assert!(render_one(&conn, &m.id, 0).unwrap().is_none());
    }

    #[test]
    fn every_displayable_image_type_gets_a_real_extension_not_bin() {
        // 两个白名单一旦分叉，能渲染的类型会被导成 `.bin`，
        // 于是 Markdown 里的图渲染不出来 —— 而且不报错。
        for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            assert!(attachment::is_displayable_image(mime), "{mime} 应当能渲染");
            assert_ne!(
                attachment::extension_for(mime),
                "bin",
                "{mime} 不该退化成 .bin"
            );
        }
        assert_eq!(attachment::extension_for("application/pdf"), "bin");
    }
}
