//! 各实体的同步快照（payload）。
//!
//! 同步传的是**实体快照**而不是字段级差分。单用户场景下行很小、行数不多，
//! 快照换来的是"不需要维护差分逻辑"，也就没有"某次改动漏进差分"这类
//! 静默 bug 的空间 —— 那种 bug 不会报错，只会让某个字段悄悄同步不过去。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelPayload {
    pub name: String,
    pub kind: String,
    pub sort_order: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePayload {
    pub channel_id: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagPayload {
    pub created_at: i64,
    pub updated_at: i64,
}

/// `id` 字段的值是 `"{message_id}|{tag_name}"`；这里再显式带上两个组成部分，
/// 免得接收端必须去解析字符串。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageTagPayload {
    pub message_id: String,
    pub tag_name: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `message_tag` 这类复合主键实体在线缆上的 id 编解码。
///
/// 分隔符用 `|`：message_id 是 UUID、tag_name 是用户输入，
/// 两者都不会包含它。
pub mod message_tag_key {
    pub const SEP: char = '|';

    pub fn encode(message_id: &str, tag_name: &str) -> String {
        format!("{message_id}{SEP}{tag_name}")
    }

    pub fn decode(id: &str) -> Option<(&str, &str)> {
        id.split_once(SEP)
    }
}

#[cfg(test)]
mod tests {
    use super::message_tag_key::{decode, encode};

    #[test]
    fn message_tag_key_roundtrips() {
        let id = encode("0199-abc", "工作");
        assert_eq!(id, "0199-abc|工作");
        assert_eq!(decode(&id), Some(("0199-abc", "工作")));
    }

    #[test]
    fn message_tag_key_keeps_separators_inside_the_tag_name() {
        // 标签名里出现分隔符时，split_once 只切第一个，标签名保持不变
        let id = encode("m1", "a|b");
        assert_eq!(decode(&id), Some(("m1", "a|b")));
    }
}
