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

/// 把整库导出到 `dir`（不存在就建）。
pub fn export_to(
    conn: &Connection,
    dir: &Path,
    utc_offset_minutes: i32,
) -> AppResult<ExportSummary> {
    let channels = channel_names(conn)?;
    let messages = all_messages(conn)?;

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

/// 按 `(created_at, id)` 顺序把**全部**未删除记录翻完。
///
/// 用游标翻页而不是一次性 `SELECT *`：`list_messages` 的上限是 500，
/// 而且它维护的是和界面同一个查询 —— 这里就不该再写一份取数的 SQL。
fn all_messages(conn: &Connection) -> AppResult<Vec<Message>> {
    let mut out: Vec<Message> = Vec::new();
    let mut before: Option<db::Cursor> = None;
    loop {
        let page = db::list_messages(conn, db::Scope::All, PAGE, before.as_ref())?;
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
        let sum = export_to(&conn, &dir, 480).unwrap();
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
        let sum = export_to(&conn, &dir, 0).unwrap();
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
