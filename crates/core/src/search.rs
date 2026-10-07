//! 中文友好的检索层。
//!
//! ## 要解决的问题
//!
//! SQLite FTS5 自带的 `unicode61` 分词器按「字母/数字边界」切词。中文句子没有
//! 空格，于是整句会被当成**一个** token，任何子串查询都匹配不到 —— 也就是说
//! 中文检索直接失效。
//!
//! FTS5 另一个自带的 `trigram` 分词器能救一部分，但它要求查询串至少 3 个字符，
//! 而中文里最高频的查询恰恰是「笔记」「会议」这类**双字词**。所以 trigram 也不够。
//!
//! ## 方案
//!
//! 在写索引之前，把连续的 CJK 段切成**重叠的二元组（bigram）**：
//!
//! ```text
//! 今天开会讨论  ->  今天 天开 开会 会讨 讨论
//! ```
//!
//! 查询时用完全相同的规则切分，于是：
//! - 双字词「笔记」就是一个精确 token
//! - 多字词「今天开会」变成三个必须同时出现的 token（AND 语义）
//!
//! 标点会**切断** CJK 段，所以「拿起了笔，记下」不会产生「笔记」这个 bigram ——
//! 这一点很重要，否则跨标点的假阳性会非常多。
//!
//! ## 代价与补救
//!
//! bigram 天然会有「跨词边界」的假阳性（例如查询「笔记」可能命中包含「笔」+「记」
//! 相邻但语义无关的文本）。所以检索的最后一步一定用**原始子串**做精确过滤，
//! 保证「搜得到的，一定是真的包含」。
//!
//! 单字 CJK 查询（例如只输入「记」）无法构成 bigram，交给 `LIKE` 回退处理。

/// 判断是否为需要按 bigram 切分的表意文字（中日韩）。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF     // CJK 统一表意文字扩展 A
        | 0x4E00..=0x9FFF   // CJK 统一表意文字
        | 0xF900..=0xFAFF   // CJK 兼容表意文字
        | 0x3040..=0x30FF   // 日文平假名 / 片假名
        | 0xAC00..=0xD7AF   // 韩文音节
    )
}

/// 遍历输入，把 CJK 连续段与非 CJK 词分别交给回调。
///
/// `emit_single_cjk` 控制「长度为 1 的 CJK 段」是否产生 token：
/// - 建索引时为 `true`（保留孤立汉字，保证它至少能被单字搜到）
/// - 构造查询时为 `false`（单字无法构成 bigram，强行生成只会漏掉真正包含它的词）
fn scan(input: &str, emit_single_cjk: bool, mut on_token: impl FnMut(String)) {
    let mut cjk_run: Vec<char> = Vec::new();
    let mut word = String::new();

    fn flush_cjk(run: &mut Vec<char>, emit_single: bool, on_token: &mut impl FnMut(String)) {
        match run.len() {
            0 => {}
            1 => {
                if emit_single {
                    on_token(run[0].to_string());
                }
            }
            _ => {
                for i in 0..run.len() - 1 {
                    on_token(run[i..i + 2].iter().collect());
                }
            }
        }
        run.clear();
    }

    fn flush_word(word: &mut String, on_token: &mut impl FnMut(String)) {
        if !word.is_empty() {
            on_token(std::mem::take(word));
        }
    }

    for ch in input.chars() {
        if is_cjk(ch) {
            flush_word(&mut word, &mut on_token);
            cjk_run.push(ch);
        } else if ch.is_alphanumeric() || ch == '_' {
            flush_cjk(&mut cjk_run, emit_single_cjk, &mut on_token);
            word.extend(ch.to_lowercase());
        } else {
            // 标点/空白：同时切断 CJK 段和西文词
            flush_cjk(&mut cjk_run, emit_single_cjk, &mut on_token);
            flush_word(&mut word, &mut on_token);
        }
    }
    flush_cjk(&mut cjk_run, emit_single_cjk, &mut on_token);
    flush_word(&mut word, &mut on_token);
}

/// 把正文转换成用于建索引的文本（bigram 展开后用空格连接）。
pub fn to_index_text(body: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    scan(body, true, |t| out.push(t));
    out.join(" ")
}

/// 把用户查询转换成 FTS5 的 token 列表。
pub fn query_terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    scan(query, false, |t| out.push(t));
    out
}

/// 一次检索的执行计划。
#[derive(Debug, Clone)]
pub enum QueryPlan {
    /// 先用 FTS5 粗筛（快），再用 `words` 精确过滤（准）
    Fts {
        match_expr: String,
        words: Vec<String>,
    },
    /// 查询无法用 bigram 表达（例如只输入了一个汉字），退化为 LIKE 扫描
    Like { words: Vec<String> },
}

/// 为空查询返回 `None`。
pub fn plan_query(query: &str) -> Option<QueryPlan> {
    // 按空白切词：每个词都必须出现（AND 语义），这与"聊天记录检索"的直觉一致
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();

    if words.is_empty() {
        return None;
    }

    let mut terms: Vec<String> = Vec::new();
    for w in &words {
        terms.extend(query_terms(w));
    }
    terms.sort();
    terms.dedup();

    if terms.is_empty() {
        return Some(QueryPlan::Like { words });
    }

    // 用双引号包住每个 token，避免 token 里可能出现的 FTS5 语法字符
    // （如 NOT / OR / * / ^）被解释成操作符。
    let match_expr = terms
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" AND ");

    // **精确过滤用的不是原始的 `words`，而是剥掉标点后的那些。**
    //
    // 索引侧 `to_index_text` 会丢掉所有非字母数字字符（FTS 的 bigram token
    // 里也没有它们），所以搜「笔记。」时粗筛能命中正文含「笔记」的记录；
    // 但末尾的 `matches_all` 要求正文包含**带标点的字面串**「笔记。」——
    // 命中被整体剔掉，返回空。
    //
    // 中文输入法带出标点极其常见，而用户看到的是「搜不到」而不是报错。
    // 这是**假阴性**：内容明明在库里，界面却说没有。
    //
    // 所以这里的判据要和索引那一侧对齐。同样一批词，LIKE 分支仍然用字面
    // 语义不变（那条路径没有分词，用户的原话就是我们要的字面）。
    //
    // **纯标点的词要丢掉**：剥完变空串的话，`matches_all` 里的
    // `lower.contains("")` 恒为真 —— 那一个词就等于"不过滤"，
    // 整批候选都会被放行，假阳性全回来了。
    let filter_words: Vec<String> = words
        .iter()
        .map(|w| strip_punct(w))
        .filter(|w| !w.is_empty())
        .collect();

    Some(QueryPlan::Fts {
        match_expr,
        words: filter_words,
    })
}

/// 去掉所有非字母数字字符。
///
/// 和 `to_index_text` 丢掉字符的那一步保持一致 —— 过滤必须和索引用同一套
/// 字符集合，否则「索引里有、过滤时找不到」，也就是假阴性。
pub fn strip_punct(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// 精确过滤：要求 `words` 中每一个都作为**原始子串**出现在正文里。
///
/// 这一步是 bigram 方案的收尾保险，去掉跨词边界的假阳性。
pub fn matches_all(body: &str, words: &[String]) -> bool {
    let lower = body.to_lowercase();
    words.iter().all(|w| lower.contains(w.as_str()))
}

/// 生成用于 `LIKE ? ESCAPE '\'` 的模式串。
pub fn like_pattern(word: &str) -> String {
    let escaped = word
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigram_expands_cjk_runs() {
        assert_eq!(to_index_text("今天开会"), "今天 天开 开会");
    }

    #[test]
    fn punctuation_breaks_cjk_runs() {
        // 「笔」和「记」被逗号隔开，绝不能生成「笔记」这个 bigram
        let idx = to_index_text("拿起了笔，记下");
        assert!(!idx.contains("笔记"), "索引不应包含跨标点的 bigram: {idx}");
    }

    #[test]
    fn two_char_query_is_expressible() {
        // 中文最高频的查询形态：双字词，必须是可表达的 token
        assert_eq!(query_terms("笔记"), vec!["笔记".to_string()]);
    }

    #[test]
    fn single_char_query_falls_back_to_like() {
        let plan = plan_query("记").expect("非空查询应有计划");
        assert!(
            matches!(plan, QueryPlan::Like { .. }),
            "单字查询应退化为 LIKE，实际为 {plan:?}"
        );
    }

    #[test]
    fn mixed_query_keeps_single_char_as_filter() {
        // 「记 hello」中 hello 可走 FTS，而「记」留在 words 里做子串过滤
        let plan = plan_query("记 hello").expect("非空查询应有计划");
        match plan {
            QueryPlan::Fts { match_expr, words } => {
                assert_eq!(match_expr, "\"hello\"");
                assert_eq!(words, vec!["记".to_string(), "hello".to_string()]);
            }
            other => panic!("应走 FTS 分支，实际为 {other:?}"),
        }
    }

    #[test]
    fn latin_is_lowercased() {
        assert_eq!(query_terms("Hello World"), vec!["hello", "world"]);
    }

    #[test]
    fn exact_filter_rejects_cross_boundary_false_positive() {
        // 索引里「案纪」和「纪要」都可能命中包含「记」的查询，
        // 但精确过滤要求原文真的含「笔记」
        assert!(!matches_all("这个议案纪要", &["笔记".to_string()]));
        assert!(matches_all("今天的笔记软件", &["笔记".to_string()]));
    }

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("50%_x"), "%50\\%\\_x%");
    }
/// **搜带标点的词不能返回空。**
    ///
    /// 这是**假阴性**：内容明明在库里，界面却说搜不到。中文输入法带出标点
    /// 极其常见（「笔记。」「会议,」），而用户看到的不是报错，是"没有结果"。
    ///
    /// 机制：FTS 的 bigram token 在 `to_index_text` 里丢掉了标点，所以粗筛能
    /// 命中正文含「笔记」的记录；但末尾的精确过滤要求正文包含**带标点的
    /// 字面串**「笔记。」—— 命中被整体剔掉。
    ///
    /// 反向验证过：把 `filter_words` 改回原始 `words`（不剥标点），
    /// 这两条立刻红。
    #[test]
    fn a_query_with_trailing_punctuation_still_matches() {
        let QueryPlan::Fts { words, .. } = plan_query("笔记。").expect("非空查询") else {
            panic!("双字以上应走 FTS");
        };
        assert_eq!(
            words,
            vec!["笔记".to_string()],
            "过滤用的词必须和索引一样剥掉标点",
        );
        assert!(
            matches_all("这是一条笔记。", &words),
            "正文含「笔记」就该命中 —— 用户搜的是「笔记。」，不是那个句号"
        );
    }

    /// 纯标点的词剥完是空串，而空串 `contains` 恒真 —— 那等于"不过滤"，
    /// 假阳性会全回来。必须丢掉。
    #[test]
    fn a_punctuation_only_word_is_dropped_from_the_filter() {
        let Some(QueryPlan::Fts { words, .. }) = plan_query("笔记 。") else {
            panic!("双字以上应走 FTS");
        };
        assert_eq!(
            words,
            vec!["笔记".to_string()],
            "空串必须被丢掉，否则整批候选都放行"
        );
    }

    /// 剥标点之后**假阳性仍然被挡住** —— 「笔」「记」被逗号隔开时不该
    /// 合成「笔记」。这是 `matches_all` 存在的全部理由，不能为了修
    /// 假阴性顺手把它放松掉。
    #[test]
    fn stripping_punctuation_still_rejects_cross_boundary_matches() {
        assert!(
            !matches_all("拿起了笔，记下", &["笔记".to_string()]),
            "跨标点合成的「笔记」仍必须被拒绝",
        );
    }
}
