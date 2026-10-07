//! 导出时**挑出哪些记录**。
//!
//! 组装（目录、重名去重、附件相对路径）在 [`messagenote_core::export`]，
//! 写出（磁盘目录树 / zip）各归各家。夹在中间的"取数 + 筛选"放这里，
//! 因为桌面端和服务端用的是**同一个 SQLite 库结构**，这一层没有必要分叉 ——
//! 而一旦分叉，"这个筛选到底包含哪几条"就会有两个答案，用户看到的只是"少了一条"。

use crate::browse::{self, Cursor, Scope};
use messagenote_core::export::ExportFilter;
use messagenote_core::models::{Message, MessagePage};
use rusqlite::Connection;

/// 枚举时一页取多少条。`list_messages` 自己会把上限收到 500。
const PAGE: i64 = 500;

/// 按 `(created_at, id)` 顺序把筛选命中的记录翻完。
///
/// 用游标翻页而不是一次性 `SELECT *`：`list_messages` 的上限是 500，
/// 而且它维护的是和界面同一个查询 —— 这里就不该再写一份取数的 SQL。
///
/// 翻页拿到的每一页都可能是"被筛掉一部分"的，所以游标取的是**原始页的最后一条**，
/// 而不是筛完之后的 —— 否则筛空的那些页会让翻页提前停住，后面还有记录也不翻了。
pub fn selected_messages(
    conn: &Connection,
    filter: &ExportFilter,
) -> rusqlite::Result<Vec<Message>> {
    // **标签优先。** 两个筛选都给的时候 SQL 走标签、频道在 `matches` 里补 ——
    // 这样"什么算有这个标签"只有 `Scope::Tag` 一份定义。反过来（SQL 走频道、
    // 标签在 Rust 里比 `m.tags`）就成了两个谓词：`Scope::Tag` 只看
    // `message_tag.deleted_at`，而 `m.tags` 还要求 `tag.deleted_at IS NULL`，
    // 于是一条"标签行被软删、消息上的关联还在"的记录会在两种筛法下得到
    // **两个答案** —— 而用户看到的只是"少了一条"。
    let scope = if let Some(tag) = filter.tag.as_deref() {
        Scope::Tag(tag)
    } else if let Some(id) = filter.channel_id.as_deref() {
        Scope::Channel(id)
    } else {
        Scope::All
    };

    let mut out: Vec<Message> = Vec::new();
    let mut before: Option<Cursor> = None;
    loop {
        let page: MessagePage = browse::list_messages(conn, scope, PAGE, before.as_ref(), None)?;
        if page.items.is_empty() {
            break;
        }
        before = page.items.last().map(Cursor::before);
        let more = page.has_more;
        out.extend(page.items);
        if !more {
            break;
        }
    }
    browse::attach_tags(conn, &mut out)?;
    out.retain(|m| filter.matches(m));
    Ok(out)
}