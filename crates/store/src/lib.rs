//! 两端共享的 SQLite 访问层。
//!
//! ## 为什么它不在 `core` 里
//!
//! `core` 是**纯逻辑**：HLC、合并裁定、分词规划、线缆协议。它不依赖 rusqlite，
//! 测试是毫秒级的、没有 I/O。把 SQL 混进去，会让 `core` 从"任何地方都能用的
//! 规则"退化成"必须有一份 SQLite 才能用"。
//!
//! ## 为什么它又确实可以共享
//!
//! `crates/core/src/lib.rs` 里写过一条判断：
//!
//! > **不**放进来的是各自的存储实现……两边的表和 SQL 本来就不同，
//! > 强行抽象只会得到一个到处是分支的怪物。
//!
//! **那条判断对写入成立，对浏览不成立。** 服务端的 `message` / `channel` /
//! `tag` / `message_tag` 是客户端同名表的**超集**（多个 `server_seq`、
//! 少个 `dirty`），而浏览只碰两边都有的列。
//!
//! 所以这个 crate 的契约很窄、也很硬：**只放"在两种 schema 上都成立"的查询**。
//! 任何需要 `dirty` 或 `server_seq` 的语句都不属于这里 —— 它应该留在各自那边，
//! 哪怕那意味着看起来相似的两段 SQL。
//!
//! ## 为什么值得单独成层
//!
//! "时间线怎么排、未归档怎么算、同一毫秒的兄弟行怎么翻页、`has_more` 怎么判定"
//! 这些语义如果两端各写一份，漂移是**静默**的：桌面端对、网页端差几条，
//! 不报任何错。这和当初把 `search` 抽进 `core` 是同一个理由。
//!
//! ## 错误
//!
//! 每个函数一律返回 [`rusqlite::Result`]，由调用方包成自己的错误类型。
//! 这一层不定义错误，也不需要知道谁在调用它。

pub mod blob;
pub mod browse;
pub mod clock;
pub mod normalize;

// `blob` 刻意**不**扁平转出：它有十来个函数，`blob::get_blob(..)` 比一长串
// `pub use` 更好读，也不会在将来和 browse 的短名字撞车。
pub use browse::{
    attach_tags, list_channels, list_messages, list_tags, row_to_message, search, search_page,
    timeline_stats, Cursor, Scope, ScopeParseError, INBOX_ID,
};
