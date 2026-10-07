//! AI 标签建议。
//!
//! ## 三条不可动摇的约束（ROADMAP 里那一节）
//!
//! 1. **绝不能静默改数据。** 建议只落在 [`crate::db`] 的 `tag_suggestion` 表里，
//!    那是**另一个东西**：它不进检索、不进导出、不进标签云、**也不进变更日志**。
//!    用户点一下之后才走 `set_message_tags`，那才会落进 `message_tag`。
//!    换句话说，"模型动了我的数据"这件事在本模块里是**写不出来**的。
//!
//! 2. **生成物要稳定。** 同一个模型 + 同一段正文，重复调用仍然可能给出不一样的
//!    答案。所以建议是**被记住的**：跑过一次就落库，重跑只做幂等合并，
//!    用户昨天看到的三个标签今天不会变成另外三个。
//!
//! 3. **建议要有出处。** 表里存了生成它的 `model`。换模型之后"为什么建议变了"
//!    才有答案 —— 否则那只是一次莫名其妙的变化。
//!
//! ## 为什么只做这一件（标签建议）
//!
//! 总结和 RAG 都要求"能引用、能跳回原消息"，那是另一个量级的工程（要向量化、
//! 要管索引生命周期）。标签建议是**最便宜、最可回滚**的那一件，先把它做扎实。

use rusqlite::{params, Connection};

use crate::error::AppResult;

/// 一条建议的完整信息。
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TagSuggestion {
    pub message_id: String,
    pub name: String,
    pub model: String,
    pub created_at: i64,
}

/// 这条记录上**还没采纳**的建议。
///
/// 只取未采纳的：已经变成真标签的建议再显示一遍，等于让用户对着同一个标签
/// 点第二次（而第二次会把它删掉）。
pub fn open_suggestions(conn: &Connection, message_id: &str) -> AppResult<Vec<TagSuggestion>> {
    let mut stmt = conn.prepare(
        "SELECT message_id, name, model, created_at
           FROM tag_suggestion
          WHERE message_id = ?1 AND accepted_at IS NULL
          ORDER BY created_at, name",
    )?;
    let rows = stmt.query_map(params![message_id], |r| {
        Ok(TagSuggestion {
            message_id: r.get(0)?,
            name: r.get(1)?,
            model: r.get(2)?,
            created_at: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 把一批建议**幂等**地记到这条记录上，返回其中**新**出现的条数。
///
/// 幂等靠 `(message_id, name)` 主键 + `DO NOTHING`：同一个建议重跑多少次
/// 都只有一行，所以"重复点一次重新生成"不会让 chip 越堆越多。
///
/// **刻意不碰 `message_tag`。** 这个函数只写建议表 —— 它没有能力改标签，
/// 这正是"绝不能静默改数据"在代码里的形状。
pub fn put_suggestions(
    conn: &Connection,
    message_id: &str,
    names: &[String],
    model: &str,
    now_ms: i64,
) -> AppResult<usize> {
    let mut added = 0;
    for name in names {
        let n = conn.execute(
            "INSERT INTO tag_suggestion (message_id, name, model, created_at)
                  VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(message_id, name) DO NOTHING",
            params![message_id, name, model, now_ms],
        )?;
        added += n;
    }
    Ok(added)
}

/// 把一条建议标记成已采纳。**返回它是不是真的存在且未被采纳过。**
///
/// 返回 false 是有意义的：界面据此说"这条建议已经用过了"，而不是静默成功 ——
/// 否则用户会以为自己刚点的那个 chip 生效了，而实际上它早就不是建议态了。
pub fn accept_suggestion(
    conn: &Connection,
    message_id: &str,
    name: &str,
    now_ms: i64,
) -> AppResult<bool> {
    let n = conn.execute(
        "UPDATE tag_suggestion SET accepted_at = ?3
          WHERE message_id = ?1 AND name = ?2 AND accepted_at IS NULL",
        params![message_id, name, now_ms],
    )?;
    Ok(n > 0)
}

// ---------------------------------------------------------------- 模型返回的解析

/// 标签的字符上限。
///
/// **不是为了好看，是为了让模型的客套话露馅。** 一个真标签不会有 32 个字；
/// 而"好的，以下是我根据这条笔记整理出来的标签："有。
///
/// 没有这道检查的话，那句客套话会变成一个 25 字的长 chip，界面上和真标签
/// 长得一模一样，用户只能点下去才发现不对。
const MAX_TAG_CHARS: usize = 32;

/// 一段文本**像不像一个标签**。
///
/// 判据是"排除句子的标点"而不是"匹配允许的字符" —— 后者会把中日韩标签、
/// emoji 标签之类全判死，而标签是用户自己起的，规则越宽越好。
fn looks_like_a_tag(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= MAX_TAG_CHARS
        // 句读。出现它们就说明这是一句话被误当成了标签。
        && !s.chars().any(|c| "，。；：！？、,.!?;:\n\t".contains(c))
        // **不含空白。** 这是挡住英文散文的那一道：模型很爱回一句
        // "Sure! Here are some tags: ..." —— 按标点它会被挡掉，但去掉标点
        // 再按空格拆的话，剩下的每个单词都"像"一个标签。
        //
        // 代价：带空格的标签（用户自己打的那种"重要 项目"）不会被建议出来。
        // 这是**刻意的** —— 建议本来就是短的单词，而真标签用户自己会打。
        && !s.chars().any(char::is_whitespace)
}

/// 从模型的回答里取出标签。
///
/// **必须容错。** 一个本地模型、少一个引号、或者多一句"好的，以下是建议："，
/// 都可能让回答不是严格的 JSON —— 而这里要是直接失败，用户看到的就是
/// "AI 功能坏了"，尽管模型的回答其实是对的。
///
/// 做法是**宽松地扫 + 严格地筛**：先把
/// `["标签"]`、`- 标签`、`{"tags": [...]}`、`#项目 #会议` 四种形状都扫成候选，
/// 再用 [`looks_like_a_tag`] 把客套话剔掉，最后交给共享的
/// [`messagenote_store::normalize::tags`] 去重。
///
/// **不走共享规范化就会分叉** —— 一个模型建议出一个带前导空格的标签，
/// 落库和界面显示就会是两个东西。
///
/// 顺序保留 —— chip 的顺序就是用户看到的建议顺序，而模型给的顺序通常
/// 就是它的置信度顺序。
pub fn parse_tags(answer: &str) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    for line in answer.lines() {
        // 记下这行**是不是一个列表项**，再决定要不要按空格拆 ——
        // 对散文按空格拆会把 "Here are some tags" 拆成三个"标签"。
        let trimmed = line.trim();
        let is_item = trimmed.starts_with(['-', '*', '\u{2022}']) || trimmed.starts_with('#');
        let line = trimmed.trim_start_matches(['-', '*', '\u{2022}']);
        let line = line.trim().trim_start_matches('`').trim_start_matches("json");
        let line = line.trim();
        // 行首的键：`{"tags": [...]} / {"tags": [...]}` → `[...]`
        let body = match line.find(':') {
            Some(i) if line[..i].trim_start_matches(['{', '"']).ends_with("tags\"") => {
                line[i + 1..].trim()
            }
            _ => line,
        };
        // JSON 分隔符一定要拆；空格**只对列表项**拆（理由见 `looks_like_a_tag`）。
        let mut seps: Vec<char> = vec![',', '[', ']', '{', '}', '"', '\''];
        if is_item {
            seps.push(' ');
        }
        for piece in body.split(seps.as_slice()) {
            let p = piece.trim().trim_start_matches('#').trim();
            if !looks_like_a_tag(p) {
                continue;
            }
            if !raw.contains(&p.to_string()) {
                raw.push(p.to_string());
            }
        }
    }
    messagenote_store::normalize::tags(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    // 一条记录 + 它的连接守卫。
    fn fresh() -> (crate::db::Db, String) {
        let d = db::open_memory("suggest-test").unwrap();
        let m = db::append_message(&*d.conn().unwrap(), "今天开了个项目会", None).unwrap();
        (d, m.id)
    }

    #[test]
    fn a_suggestion_is_stored_and_shown_as_pending() {
        let (d, id) = fresh();
        let c = d.conn().unwrap();
        let added = put_suggestions(&*c, &id, &["项目".into(), "会议".into()], "gpt-x", 100)
            .unwrap();
        assert_eq!(added, 2);

        let open = open_suggestions(&*c, &id).unwrap();
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].model, "gpt-x");
    }

    /// 这条是整个模块最重要的一条：**建议不能变成标签。**
    #[test]
    fn a_suggestion_never_becomes_a_real_tag_on_its_own() {
        let (d, id) = fresh();
        {
            let c = d.conn().unwrap();
            put_suggestions(&*c, &id, &["项目".into()], "m", 1).unwrap();
        }
        let c = d.conn().unwrap();
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM message_tag WHERE message_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "建议只是建议：它不该出现在真实标签表里");
    }

    /// "每次重跑都变的结果会毁掉信任" —— 所以重跑必须是幂等的。
    #[test]
    fn running_the_suggestion_twice_does_not_duplicate_it() {
        let (d, id) = fresh();
        let c = d.conn().unwrap();
        let first = put_suggestions(&*c, &id, &["项目".into(), "会议".into()], "m", 100).unwrap();
        let second = put_suggestions(&*c, &id, &["项目".into(), "会议".into()], "m", 200)
            .unwrap();
        assert_eq!(first, 2);
        assert_eq!(second, 0, "第二次不该新增任何一条");
        assert_eq!(open_suggestions(&*c, &id).unwrap().len(), 2);
        // 时间戳也不该被刷新：用户看到的那条建议必须是原来那条
        assert_eq!(open_suggestions(&*c, &id).unwrap()[0].created_at, 100);
    }

    #[test]
    fn an_accepted_suggestion_stops_showing_as_pending() {
        let (d, id) = fresh();
        let c = d.conn().unwrap();
        put_suggestions(&*c, &id, &["项目".into()], "m", 1).unwrap();

        assert!(accept_suggestion(&*c, &id, "项目", 2).unwrap());
        assert!(open_suggestions(&*c, &id).unwrap().is_empty());
        let total: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM tag_suggestion WHERE message_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 1, "采纳之后它仍在表里，只是不再作为待办 chip 出现");

        // 再点一次要说"已经用过了"，而不是静默成功
        assert!(
            !accept_suggestion(&*c, &id, "项目", 3).unwrap(),
            "重复采纳必须返回 false，界面据此说'已经用过了'"
        );
    }

    #[test]
    fn accepting_something_that_was_never_suggested_fails_loudly() {
        let (d, id) = fresh();
        let c = d.conn().unwrap();
        assert!(!accept_suggestion(&*c, &id, "凭空来的", 1).unwrap());
    }

    #[test]
    fn suggestions_are_scoped_to_one_message() {
        let (d, a) = fresh();
        let b = db::append_message(&*d.conn().unwrap(), "另一条", None).unwrap().id;
        let c = d.conn().unwrap();
        put_suggestions(&*c, &a, &["甲".into()], "m", 1).unwrap();
        put_suggestions(&*c, &b, &["乙".into()], "m", 1).unwrap();
        assert_eq!(open_suggestions(&*c, &a).unwrap()[0].name, "甲");
        assert_eq!(open_suggestions(&*c, &b).unwrap()[0].name, "乙");
    }

    // ------------------------------------------------------------ 解析

    #[test]
    fn a_plain_json_array_is_parsed() {
        assert_eq!(parse_tags(r#"["项目", "会议"]"#), vec!["项目", "会议"]);
    }

    #[test]
    fn a_fenced_json_block_with_a_preamble_is_parsed() {
        let answer = "好的，以下是建议：\n```json\n{\"tags\": [\"项目\", \"会议\"]}\n```";
        assert_eq!(parse_tags(answer), vec!["项目", "会议"]);
    }

    #[test]
    fn a_bullet_list_is_parsed() {
        assert_eq!(
            parse_tags("- 项目\n- 会议\n- 待办"),
            vec!["项目", "会议", "待办"]
        );
    }

    /// 模型不听话是常态，不是异常 —— 这里不能失败。
    #[test]
    fn conversational_noise_does_not_become_a_tag() {
        let answer = "Sure! Here are some tags:\n- 项目\n- 会议\n\nLet me know if you need more.";
        assert_eq!(parse_tags(answer), vec!["项目", "会议"]);
    }

    #[test]
    fn duplicates_are_collapsed() {
        assert_eq!(parse_tags(r#"["项目", "项目", "会议"]"#), vec!["项目", "会议"]);
    }

    #[test]
    fn an_empty_answer_yields_nothing_rather_than_a_fake_tag() {
        assert!(parse_tags("").is_empty());
        assert!(parse_tags("[]").is_empty());
        assert!(parse_tags("这条笔记不需要标签。").is_empty());
    }

    #[test]
    fn a_hash_prefix_is_stripped() {
        assert_eq!(parse_tags("#项目 #会议"), vec!["项目", "会议"]);
    }

    #[test]
    fn an_over_long_tag_is_rejected_by_the_shared_normalizer() {
        // 标签长度上限是共享规则（`normalize::tags`），这里不许绕开它
        let too_long = "长".repeat(200);
        assert!(parse_tags(&format!("[\"{too_long}\"]")).is_empty());
    }
}
