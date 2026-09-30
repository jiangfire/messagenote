//! Tauri 命令层。
//!
//! 这一层刻意保持"薄"：只负责取锁、转发、返回。所有业务规则都住在 `db` 模块里，
//! 这样业务流程可以脱离 Tauri 运行时单独测试。

use tauri::State;

use crate::db::{self, Db};
use crate::error::{AppError, AppResult};
use crate::sync_worker::{SyncStatus, SyncWorker};
use messagenote_core::models::{
    Channel, Message, MessagePage, SearchPage, SyncConfig, TagCount, TimelineStats,
};
use messagenote_core::wire::HealthResponse;

#[tauri::command]
pub fn list_channels(db: State<'_, Db>) -> AppResult<Vec<Channel>> {
    let conn = db.conn()?;
    Ok(db::list_channels(&conn)?)
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
///
/// `since`（epoch 毫秒，含端点）是时间范围筛选：「今天 / 近 7 天」这些档位
/// 在前端折算成一个绝对时刻，命令层不解释它。
///
/// 参数多是**有意的**：这一层是薄转发，签名与 IPC 线协议 1:1 对应，
/// 收拢成结构体反而多出一层反序列化、还要动两端的调用方式。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn list_timeline(
    db: State<'_, Db>,
    scope: String,
    channel_id: Option<String>,
    tag: Option<String>,
    limit: Option<i64>,
    since: Option<i64>,
    before_created_at: Option<i64>,
    before_id: Option<String>,
) -> AppResult<MessagePage> {
    let conn = db.conn()?;
    let cursor = cursor_from(before_created_at, before_id);

    // 解析规则住在共享存储层，服务端 API 用的也是同一份 —— 两边各写一个
    // match 的话，"unfiled" 在哪一边改了含义，另一边只会静默地换内容。
    let scope = db::Scope::parse(&scope, channel_id.as_deref(), tag.as_deref())
        .map_err(|e| AppError::Msg(e.to_string()))?;

    Ok(db::list_messages(
        &conn,
        scope,
        limit.unwrap_or(120),
        cursor.as_ref(),
        since,
    )?)
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
    offset: Option<i64>,
) -> AppResult<SearchPage> {
    let conn = db.conn()?;
    Ok(db::search_page(
        &conn,
        &query,
        limit.unwrap_or(60),
        offset.unwrap_or(0),
    )?)
}

#[tauri::command]
pub fn list_tags(db: State<'_, Db>) -> AppResult<Vec<TagCount>> {
    let conn = db.conn()?;
    Ok(db::list_tags(&conn)?)
}

/// 侧边栏的两个计数：时间线总数、收件箱（未打标签）数。
#[tauri::command]
pub fn timeline_stats(db: State<'_, Db>) -> AppResult<TimelineStats> {
    let conn = db.conn()?;
    Ok(db::timeline_stats(&conn)?)
}

#[tauri::command]
pub fn set_message_tags(db: State<'_, Db>, message_id: String, tags: Vec<String>) -> AppResult<()> {
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

/// 本次启动记下的**非致命**警告（比如全局快捷键被别的程序占用）。
///
/// 前端挂载时读一次。这些警告产生于 `setup` 阶段 —— 那时界面还没加载，
/// 事件推送没人听得到，所以必须有一个"可查询"的入口。
#[tauri::command]
pub fn get_startup_warnings() -> Vec<String> {
    crate::fatal::startup_warnings()
}

// ---------------------------------------------------------------- 附件

/// 存一份附件（粘贴或拖进来的图片），返回它的 sha256。
///
/// 调用方拿到 sha 之后要把它以 `attachment:<sha>` 的形式写进正文 ——
/// 正文就是笔记本身，附件靠这个引用被发现。见 `messagenote_core::attachment`。
#[tauri::command]
pub fn save_attachment(db: State<'_, Db>, bytes: Vec<u8>) -> AppResult<String> {
    let conn = db.conn()?;
    db::save_attachment(&conn, &bytes)
}

/// 取附件字节，给 `<img>` 用。
///
/// 返回 `tauri::ipc::Response` 而不是 `Vec<u8>`：前者走**原始字节**通道，
/// 后者会被序列化成 JSON 数组 —— 一张 2 MB 的图会变成约 8 MB 的文本
/// （每个字节最多要 4 个字符），在 IPC 上来回搬。
#[tauri::command]
pub fn read_attachment(db: State<'_, Db>, sha256: String) -> AppResult<tauri::ipc::Response> {
    let conn = db.conn()?;
    let (_mime, bytes) = db::read_attachment(&conn, &sha256)?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// 附件字节在不在本地。界面据此决定是显示占位图还是直接渲染。
#[tauri::command]
pub fn has_attachment(db: State<'_, Db>, sha256: String) -> AppResult<bool> {
    let conn = db.conn()?;
    Ok(db::blob::has_blob(&conn, &sha256)?)
}

/// 回收不再被任何记录引用的附件字节，返回清掉的字节数。
///
/// 刻意做成**显式动作**而不是自动的：它是 O(附件 × 记录) 的扫描，
/// 而且判错的代价是永久删掉用户的图。见 `messagenote_store::blob`。
#[tauri::command]
pub fn collect_garbage_attachments(db: State<'_, Db>) -> AppResult<i64> {
    let conn = db.conn()?;
    Ok(db::blob::gc_unreferenced(&conn)?)
}

/// 把整个库导出成一棵 Markdown 目录树。
///
/// **附件的字节不经过前端**：这里直接从 SQLite 读出来写盘。一份图多的库有几十
/// 上百 MB，让它们来回搬一遍 IPC 既慢又没必要 —— 所以前端只给一个目录路径。
///
/// `utc_offset_minutes` 是本地时区相对 UTC 的偏移（**东为正**，东八区是 480）。
/// 前端传 `-new Date().getTimezoneOffset()`。文件名要的是本地时间，而 Rust 侧
/// 没有时区库、也不值得为一个文件名引入一个 —— 理由见
/// `messagenote_core::export` 的模块说明。
///
/// `filter` 省略（或 `null`）= 全量导出。给了就只导命中的那部分：频道 / 标签 /
/// 时间区间，三者可任意组合。**区间是闭区间**，理由见 `crate::export`。
#[tauri::command]
pub fn export_markdown(
    db: State<'_, Db>,
    dir: String,
    utc_offset_minutes: i32,
    filter: Option<crate::export::ExportFilter>,
) -> AppResult<crate::export::ExportSummary> {
    let conn = db.conn()?;
    crate::export::export_to(
        &conn,
        std::path::Path::new(&dir),
        utc_offset_minutes,
        &filter.unwrap_or_default(),
    )
}

/// 把一条记录渲染成 Markdown，给「复制这一条」用。
///
/// 返回 `null` 表示这条已经不在了（被删掉，或 id 不对）—— 界面据此说
/// "这条记录已经不在了"，而不是把一句数据库错误摔到用户脸上。
#[tauri::command]
pub fn render_message_markdown(
    db: State<'_, Db>,
    id: String,
    utc_offset_minutes: i32,
) -> AppResult<Option<String>> {
    let conn = db.conn()?;
    crate::export::render_one(&conn, &id, utc_offset_minutes)
}

/// 装完更新之后重启自己。
///
/// **不能直接用插件自带的重启。** `tauri::process::restart` 是"起一个新进程，
/// 然后立刻 `std::process::exit`"：新进程启动时会去问单实例插件"我是不是第一个"，
/// 而老进程往往还没退干净 —— 于是新进程把参数交出去、自己退掉，老进程同时也在退，
/// **结果一个都不剩**（表现是"更新完应用不见了"）。插件为此留了一个
/// `destroy`：先把自己占的那个名字放掉，新进程才会认为自己是第一个。
///
/// 顺序不能反：`destroy` 必须在 `restart` 之前。
#[tauri::command]
pub fn relaunch(app: tauri::AppHandle) {
    tauri_plugin_single_instance::destroy(&app);
    app.restart();
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
