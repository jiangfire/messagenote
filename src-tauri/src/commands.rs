//! Tauri 命令层。
//!
//! 这一层刻意保持"薄"：只负责取锁、转发、返回。所有业务规则都住在 `db` 模块里，
//! 这样业务流程可以脱离 Tauri 运行时单独测试。

use tauri::State;

use crate::db::{self, Db};
use crate::error::{AppError, AppResult};
use crate::sync_worker::{SyncStatus, SyncWorker};
use messagenote_core::models::{
    Channel, Message, MessagePage, SearchHit, SyncConfig, TagCount, TimelineStats,
};
use messagenote_core::wire::HealthResponse;

#[tauri::command]
pub fn list_channels(db: State<'_, Db>) -> AppResult<Vec<Channel>> {
    let conn = db.conn()?;
    db::list_channels(&conn)
}

#[tauri::command]
pub fn create_channel(db: State<'_, Db>, name: String) -> AppResult<Channel> {
    let conn = db.conn()?;
    db::create_channel(&conn, &name)
}

#[tauri::command]
pub fn rename_channel(db: State<'_, Db>, id: String, name: String) -> AppResult<()> {
    let conn = db.conn()?;
    db::rename_channel(&conn, &id, &name)
}

#[tauri::command]
pub fn delete_channel(db: State<'_, Db>, id: String) -> AppResult<()> {
    let conn = db.conn()?;
    db::delete_channel(&conn, &id)
}

/// 时间线查询。
///
/// `scope` 取 `"all"` / `"unfiled"` / `"channel"` / `"tag"`：
/// - `unfiled` —— 还没归档到任何频道（仍在收件箱），也就是时间线上那个筛选
/// - `channel` 需要 `channel_id`
/// - `tag` 需要 `tag`
///
/// 往前翻页用 `before_created_at` + `before_id` 两个参数一起传 ——
/// 二者构成 `(created_at, id)` 复合游标。只传时间戳会在同一毫秒内的
/// 多条消息处漏掉整批记录。
#[tauri::command]
pub fn list_timeline(
    db: State<'_, Db>,
    scope: String,
    channel_id: Option<String>,
    tag: Option<String>,
    limit: Option<i64>,
    before_created_at: Option<i64>,
    before_id: Option<String>,
) -> AppResult<MessagePage> {
    let conn = db.conn()?;
    let cursor = cursor_from(before_created_at, before_id);

    let scope = match scope.as_str() {
        "all" => db::Scope::All,
        "unfiled" => db::Scope::Unfiled,
        "channel" => db::Scope::Channel(channel_id.as_deref().ok_or_else(|| {
            AppError::Msg("scope=channel 时必须提供 channel_id".into())
        })?),
        "tag" => db::Scope::Tag(
            tag.as_deref()
                .ok_or_else(|| AppError::Msg("scope=tag 时必须提供 tag".into()))?,
        ),
        other => {
            return Err(AppError::Msg(format!("未知的时间线范围：{other}")));
        }
    };

    db::list_messages(&conn, scope, limit.unwrap_or(120), cursor.as_ref())
}

/// 两个参数必须**一起**给：只给时间戳等于退回单键游标，会漏记录。
fn cursor_from(created_at: Option<i64>, id: Option<String>) -> Option<db::Cursor> {
    match (created_at, id) {
        (Some(created_at), Some(id)) => Some(db::Cursor { created_at, id }),
        _ => None,
    }
}

/// 追加一条消息。`channel_id` 省略时落到收件箱 —— 这是「捕获不做决策」的入口。
#[tauri::command]
pub fn append_message(
    db: State<'_, Db>,
    body: String,
    channel_id: Option<String>,
) -> AppResult<Message> {
    let conn = db.conn()?;
    db::append_message(&conn, &body, channel_id.as_deref())
}

#[tauri::command]
pub fn update_message(db: State<'_, Db>, id: String, body: String) -> AppResult<Message> {
    let conn = db.conn()?;
    db::update_message(&conn, &id, &body)
}

#[tauri::command]
pub fn delete_message(db: State<'_, Db>, id: String) -> AppResult<()> {
    let conn = db.conn()?;
    db::delete_message(&conn, &id)
}

#[tauri::command]
pub fn move_message(db: State<'_, Db>, id: String, channel_id: String) -> AppResult<()> {
    let conn = db.conn()?;
    db::move_message(&conn, &id, &channel_id)
}

#[tauri::command]
pub fn search_messages(
    db: State<'_, Db>,
    query: String,
    limit: Option<i64>,
) -> AppResult<Vec<SearchHit>> {
    let conn = db.conn()?;
    db::search(&conn, &query, limit.unwrap_or(60))
}

#[tauri::command]
pub fn list_tags(db: State<'_, Db>) -> AppResult<Vec<TagCount>> {
    let conn = db.conn()?;
    db::list_tags(&conn)
}

/// 侧边栏的两个计数：时间线总数、收件箱（未打标签）数。
#[tauri::command]
pub fn timeline_stats(db: State<'_, Db>) -> AppResult<TimelineStats> {
    let conn = db.conn()?;
    db::timeline_stats(&conn)
}

#[tauri::command]
pub fn set_message_tags(
    db: State<'_, Db>,
    message_id: String,
    tags: Vec<String>,
) -> AppResult<()> {
    let conn = db.conn()?;
    db::set_message_tags(&conn, &message_id, &tags)
}

/// 收起捕获浮层。
///
/// 让前端走命令而不是直接调 `getCurrentWindow().hide()`：
/// "如何收起浮层"只保留一个实现（Rust 侧那个），
/// 以后要加收起时的清理逻辑也不必再去前端找第二处。
#[tauri::command]
pub fn hide_capture(app: tauri::AppHandle) {
    crate::hide_capture(&app);
}

// ---------------------------------------------------------------- 同步

#[tauri::command]
pub fn get_sync_config(db: State<'_, Db>) -> AppResult<SyncConfig> {
    let conn = db.conn()?;
    db::get_sync_config(&conn)
}

#[tauri::command]
pub fn set_sync_config(db: State<'_, Db>, url: String, token: String) -> AppResult<()> {
    let conn = db.conn()?;
    db::set_sync_config(&conn, &url, &token)
}

/// 请求立刻同步一次。
///
/// 命令本身**立刻返回**，真正的同步跑在后台线程上 —— 否则服务端不可达时
/// 界面会卡到超时为止。结果通过 `sync://status` 事件推给前端。
#[tauri::command]
pub fn sync_now(worker: State<'_, SyncWorker>) -> AppResult<()> {
    worker.trigger();
    Ok(())
}

/// 最近一次同步的结果。
///
/// 前端挂载时主动读一次，补上"启动时那次同步的事件已经错过了"的缺口。
#[tauri::command]
pub fn get_sync_status(worker: State<'_, SyncWorker>) -> Option<SyncStatus> {
    worker.last()
}

/// 测试连接。只发一个很小的探活请求，不落任何数据。
///
/// 这个是同步命令（会阻塞），但它是用户明确点的动作、只发一个请求、
/// 而且不持有数据库锁，所以不会连累其它操作。
#[tauri::command]
pub fn test_sync_connection(url: String, token: String) -> AppResult<HealthResponse> {
    crate::http::HttpServerApi::new(&url, &token)?.handshake()
}

