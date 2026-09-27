use serde::{Deserialize, Serialize};

/// 频道。
///
/// `kind` 为 `inbox` 的那个频道是全局唯一且不可删除的默认捕获目标 ——
/// 这是整个产品「捕获时不做任何决策」这条原则在数据层的落点。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub sort_order: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// 未删除的消息条数，用于侧边栏显示
    pub message_count: i64,
}

/// 一条「消息」，也就是一条笔记。
///
/// `device_id` / `rev` 目前在单机版里没有被使用，但必须现在就存在：
/// 它们是后续多设备同步做冲突判定的依据，等有数据了再补列会非常痛苦。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub channel_id: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    pub items: Vec<Message>,
    pub has_more: bool,
}

/// 检索结果。把 Message 展平，前端可以直接复用消息组件渲染。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    #[serde(flatten)]
    pub message: Message,
    pub channel_name: String,
}

/// 一页检索结果。
///
/// 检索的翻页语义和时间线**不一样**：时间线是"往前翻更早的"，可以用
/// `(created_at, id)` 做键集游标；而检索是"更相关的"，排序键是 `bm25()` ——
/// 一个会随语料变化的浮点分数。拿它做游标是不稳的，所以这里用 **offset**：
/// 用户点"加载更多结果"，就是在同一次检索里往后多看几条。
///
/// 代价要知道：两次请求之间如果有新数据写入，偏移量可能让某一条重复出现或
/// 被跳过。对"刚搜完正在往下翻"这个瞬态场景可以接受；时间线那边不行，
/// 所以那边仍然用键集游标。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPage {
    pub items: Vec<SearchHit>,
    pub has_more: bool,
}


#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagCount {
    pub name: String,
    pub count: i64,
}

/// 侧边栏与筛选条需要的两个计数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineStats {
    /// 时间线里的全部消息
    pub total: i64,
    /// 还没归档到任何频道的 —— 仍在收件箱里等整理的那些
    pub unfiled: i64,
}

/// 同步服务端配置。存在本地 `meta` 表里，跟库一起走 ——
/// 换机器时直接复制数据库文件，不用重新配一遍。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncConfig {
    /// 形如 `https://notes.example.com`；末尾斜杠会被去掉
    pub url: String,
    /// 与服务端 `MESSAGENOTE_TOKEN` 一致的长期凭据
    pub token: String,
}
