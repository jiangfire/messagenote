//! 同步线缆协议类型。
//!
//! 桌面端和服务端共用这些定义，字段名与语义只有一处。同步协议最怕的就是
//! "某一边悄悄少发了一个字段" —— 它不会报错，只会在几周后表现为
//! "有些笔记的标签不知道为什么丢了"。
//!
//! 全部使用 camelCase 序列化，和前端、以及 Tauri 命令层的命名约定保持一致。

use serde::{Deserialize, Serialize};

use crate::hlc::Hlc;

/// 协议版本。服务端与客户端不一致时应显式拒绝，而不是尽力而为地继续。
pub const PROTOCOL_VERSION: u32 = 1;

/// 会被同步的实体种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Channel,
    Message,
    Tag,
    MessageTag,
}

impl EntityKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntityKind::Channel => "channel",
            EntityKind::Message => "message",
            EntityKind::Tag => "tag",
            EntityKind::MessageTag => "message_tag",
        }
    }
}

/// 一条变更。
///
/// `data` 是实体的完整快照（而不是"改了哪个字段"的差分）。理由是单用户场景下
/// 行很小、行数不多，快照换取的是**无需维护字段级差分逻辑**，也就没有
/// "某次改动漏进差分" 这类静默 bug 的空间。
///
/// 删除用 `deleted: true` 表达 —— 服务端和所有客户端都必须保留墓碑，
/// 否则"这条被删了"和"这条还没同步过来"无法区分。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    /// 服务端分配的单调整数游标。仅拉取时存在；推送时客户端不填。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    pub kind: EntityKind,
    /// 实体 id。`message_tag` 的 id 是 `"{messageId}|{tagName}"`。
    pub id: String,
    pub hlc: Hlc,
    pub deleted: bool,
    /// 实体快照；`deleted` 为真时可以为 null
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// `GET /api/sync/pull` 的查询参数。
///
/// 用服务端分配的 `since` 而不是时间戳做游标：时间戳游标在任何一台设备
/// 时钟偏慢时都会导致**永久漏拉**，而单调序号不会。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullQuery {
    pub since: i64,
    pub limit: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullResponse {
    pub changes: Vec<Change>,
    /// 客户端下次应当传入的 `since`
    pub cursor: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushRequest {
    pub changes: Vec<Change>,
}

/// 单条推送的裁定结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushOutcome {
    pub kind: EntityKind,
    pub id: String,
    /// 服务端是否接受了这条变更
    pub accepted: bool,
    /// 未被接受时，服务端当前胜出的版本。
    /// 客户端据此保留自己的副本并采用服务端的版本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<Change>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushResponse {
    pub results: Vec<PushOutcome>,
}

/// `GET /api/health`：最小握手，用来确认地址、token 和协议版本都对。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub ok: bool,
    pub protocol: u32,
    pub server_time_ms: i64,
}

/// `GET /api/timeline` 的查询参数。
///
/// 放在线缆协议里而不是服务端本地类型，是因为**网页端也要构造它**。
/// 参数名各写一份的话，前端发 `channelId` 而服务端读 `channel_id`
/// 会静默地退化成"全部消息"—— 不报错，只是筛选没生效。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineQuery {
    /// `"all"` / `"unfiled"` / `"channel"` / `"tag"`
    pub scope: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// 往前翻的游标。**两个字段必须一起给** —— 只给时间戳等于退回单键游标，
    /// 会在同一毫秒的兄弟行处整批漏掉记录。
    #[serde(default)]
    pub before_created_at: Option<i64>,
    #[serde(default)]
    pub before_id: Option<String>,
}

/// `GET /api/search` 的查询参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub limit: Option<i64>,
    /// 往后看几条。"加载更多结果"用的 —— 检索没有键集游标，
    /// 因为它的排序键是会随语料变化的 bm25 分数。
    #[serde(default)]
    pub offset: Option<i64>,
}

// ---------------------------------------------------------------- 写入请求
//
// 网页端的写操作走这些端点，由**服务端代笔**：服务端以一台设备的身份生成
// HLC、写进变更日志、分配 seq，桌面端下次同步照常拉到。浏览器因此完全不需要
// HLC、合并和冲突处理 —— 裁定权仍然只有 `core::merge` 那一份。
//
// 这些类型放在线缆协议里，是因为**网页端也要构造它们**。字段名各写一份的话，
// 前端发 `channelId` 而服务端读 `channel_id` 会静默地退化成别的含义。

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateMessageRequest {
    pub body: String,
    /// 省略时落到收件箱 —— 这是「捕获不做决策」的入口。
    #[serde(default)]
    pub channel_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditMessageRequest {
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveMessageRequest {
    pub channel_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetTagsRequest {
    /// 整体替换语义：传 `[]` 就是清空标签。
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateChannelRequest {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameChannelRequest {
    pub name: String,
}

// ---------------------------------------------------------------- 会话

/// `POST /api/session`：用长期令牌换一个短期会话。
///
/// 为什么需要这一步：长期令牌放进浏览器的 localStorage，等于把整个库的读写
/// 权限交给任何一次 XSS —— 而正文是要渲染用户 Markdown 的。换来的会话可以
/// 过期、可以被单独吊销，长期令牌则一直待在用户手里（或者干脆不落盘）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginRequest {
    /// `MESSAGENOTE_TOKEN` 那个长期凭据
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResponse {
    /// 之后所有请求带这个（同样走 `Authorization: Bearer`）
    pub session: String,
    /// 过期时刻（毫秒时间戳）。客户端据此决定什么时候重新登录。
    pub expires_at: i64,
}
