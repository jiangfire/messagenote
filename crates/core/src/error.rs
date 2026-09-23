//! 内核错误类型。
//!
//! 刻意**不包含数据库变体**。内核不知道存储引擎长什么样 —— 客户端有
//! `dirty` 标记和本地索引，服务端有类型完全不同的 `server_seq` 表，
//! 硬塞一个共同的 `Sqlite` 变体只会让内核反向依赖某个具体实现。
//!
//! 所以：内核只描述"协议不兼容""数据格式不对"这类跨端共通的失败，
//! 各存储层再定义自己的错误类型并把内核错误作为一个来源。

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("协议不兼容：{0}")]
    Protocol(String),

    #[error("数据格式错误：{0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Msg(String),
}

impl Serialize for CoreError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type CoreResult<T> = Result<T, CoreError>;
