//! MessageNote 共享内核。
//!
//! 桌面端（Tauri）和服务端（axum）都依赖这个 crate。抽出来的判断标准是
//! **"一旦两份实现出现分歧就会静默出错"** 的那些逻辑：
//!
//! - `search`：中文 bigram 分词。写入端和读取端只要有一点不一致，
//!   检索就会静默失效 —— 不报错，就是搜不到。
//! - `hlc`：混合逻辑时钟。两端对"谁更新"必须得出完全一致的结论，
//!   否则同一批数据在不同设备上会收敛到不同状态。
//! - `merge`：合并决策。冲突时"留谁"的规则必须唯一。
//! - `wire`：线缆协议类型。字段名和语义漂移会让同步悄悄丢字段。
//! - `models`：实体 DTO。桌面端命令层和服务端 API 共用同一份定义。
//! - `export`：导出的 Markdown 长什么样。两端各写一份的话，同一条笔记会
//!   导出成不同的东西，而且不报错。
//!
//! **不**放进来的是各自的存储实现：客户端有 `dirty` 标记和本地索引，
//! 服务端有单调递增的 `server_seq`，两边的表和 SQL 本来就不同，
//! 强行抽象只会得到一个到处是分支的怪物。
//!
//! 不过有一条例外值得记下来：**浏览和检索用的 SQL 在两边确实通用** ——
//! 服务端的表是客户端同名表的超集，而浏览只碰两边都有的列。那部分被放在
//! `messagenote-store` 里，而不是这里。这个 crate 保持纯逻辑、不依赖 rusqlite，
//! 于是它能被任何地方使用（包括将来可能编到 wasm 的场景）。

pub mod attachment;
pub mod error;
pub mod export;
pub mod hlc;
pub mod merge;
pub mod models;
pub mod payload;
pub mod search;
pub mod wire;

pub use error::{CoreError, CoreResult};
pub use hlc::{now_ms, Hlc};
pub use wire::PROTOCOL_VERSION;
