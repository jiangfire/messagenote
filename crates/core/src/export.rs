//! 导出：把记录变成"拿得出去"的 Markdown。
//!
//! 为什么放在 `core` 而不是各自的界面里：导出格式**一旦两份实现分歧就会静默
//! 出错** —— 同一条笔记在桌面端和网页端导出成不同的样子，而且不会报错。
//! 这和 `search` / `hlc` / `merge` 是同一条判据。
//!
//! 另外：**正文里的附件引用只在这里解析一次**。引用语法归
//! [`crate::attachment`] 所有（`SCHEME` / `referenced_shas`），这里复用，
//! 不另写一个正则 —— 两个解析器对"什么算一条引用"给出不同答案是典型的静默 bug。
//!
//! ## 为什么日期换算在这里做
//!
//! 文件名要**本地时间**（`2026-09-29-1146`），而 `core` 没有时区库，也不想为了
//! 一个文件名引入 `chrono`。所以调用方只给一个 **UTC 偏移（分钟，东为正）** ——
//! 那纯算术就能处理，而且那正是客户端唯一知道的本地信息。
//! 这样"什么是本地时间"就不必在 Rust 里重建一遍时区数据库。
//!
//! 代价要知道：整次导出用**同一个偏移**。跨夏令时切换的那一天里导出的文件，
//! 时间标签可能差一小时。对"把笔记导出来"这件事可以接受 —— 精确的那一份在
//! front-matter 的 `created` 里（带偏移，机器可解析），文件名只是给人扫读的。
//!
//! ## 附件
//!
//! 正文里的引用是 `attachment:<sha256>`，导出时必须变成**相对路径**
//! （`../attachments/<sha>.png`），否则导出物里的图全是悬空的。
//! 路径由调用方给 —— 只有它知道文件落在哪儿、以及那份字节本地到底有没有。

use crate::attachment;

/// 导出时一条记录需要提供的全部内容。
///
/// 刻意是个借用结构而不是 `Message`：`core` 不该知道数据库那一层的形状
/// （`Message` 里还有 `dirty`、`device_id` 这些只属于某一端的东西）。
#[derive(Debug, Clone)]
pub struct ExportItem<'a> {
    /// 频道名。收件箱也应该给一个能读的名字（比如"收件箱"）。
    pub channel: &'a str,
    pub tags: &'a [String],
    pub created_at: i64,
    pub updated_at: i64,
    pub body: &'a str,
}

/// 文件名里时间标签的长度上限之外，摘要部分的字符上限。
///
/// 定小一点是有意的：文件名是给人扫的，而且它还要和目录名一起留在
/// Windows 的路径长度预算里（深目录 + 长中文名很容易顶到 260）。
const EXCERPT_CHARS: usize = 40;

/// 单个路径段的字符上限（按 `char` 数，不是字节）。
const SEGMENT_CHARS: usize = 60;

/// Windows 不允许出现在文件名里的字符，外加控制字符。
///
/// 换成 `_` 而不是删掉：删掉会让 `a/b` 和 `ab` 变成同一个名字，
/// 而保持长度不收缩更容易看出原名是什么。
fn is_illegal_in_name(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20
}

/// Windows 上一批**保留设备名**。叫 `CON.md` 的文件在 Windows 上创建会失败，
/// 而且报的错和"名字非法"完全不像，所以这里直接给它加个前缀绕开。
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 把一段文本变成能当**一个路径段**（目录名或文件名主体）用的字符串。
///
/// 不处理 `.` 和 `..` 之外的点号语义 —— 调用方拼的是 `<目录>/<文件名>`，
/// 而这里产出的东西永远不会是纯 `.` 或 `..`（下面兜底成"未命名"）。
pub fn sanitize_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if is_illegal_in_name(c) {
            out.push('_');
        } else {
            out.push(c);
        }
    }

    // Windows 会**静默吃掉**结尾的点和空格：`名字.` 存下来变成 `名字`，
    // 于是两个不同的输入撞成同一个文件。
    let trimmed = out.trim().trim_end_matches(['.', ' ']).trim();
    let mut s = trimmed.to_string();

    if s.chars().count() > SEGMENT_CHARS {
        s = s.chars().take(SEGMENT_CHARS).collect();
        s = s.trim_end().trim_end_matches(['.', ' ']).to_string();
    }

    if s.is_empty() {
        return "未命名".to_string();
    }

    // 保留设备名：`CON`、`con.txt` 都不行，所以判的是**第一段**。
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        s.insert(0, '_');
    }

    s
}

/// 把正文压成一行短摘要，给文件名用。
///
/// **这刻意不是 `src/lib/format.ts` 里的 `plainSummary`**：那个是给检索结果用的
/// （160 字、保留可读性），这个是给文件名用的（40 字、还要能过 `sanitize_segment`）。
/// 两者的取舍不同，不该被"统一"成一个 —— 真要统一也得先想清楚为谁让步。
fn excerpt(body: &str) -> String {
    // **只取第一行**：一条笔记的第一行通常就是它的"标题"。把后面的行也拼进来
    // 会让文件名变长，而且混进不相干的内容 —— 文件名只需要一个能认出来的标签，
    // 全文本来就在文件里。
    let first = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let t = first.to_string();

    // 去掉图片和链接，只留下文字。**顺序要紧**：先图后链接 —— 链接那条规则
    // 会把图片的 `![](...)` 吃掉一半，剩下一个孤零零的 `!`。
    let t = strip_inline(&t, "![", "](");
    let t = strip_inline(&t, "[", "](");
    let t: String = t
        .chars()
        .filter(|c| !matches!(c, '#' | '*' | '_' | '`' | '>' | '~' | '|'))
        .collect();

    let collapsed = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.chars().count() > EXCERPT_CHARS {
        let cut: String = trimmed.chars().take(EXCERPT_CHARS).collect();
        return format!("{}…", cut.trim_end());
    }
    trimmed.to_string()
}

/// 把 `开头…](…)` 这一整块去掉。找不到闭合就原样返回（正文不必是合法 Markdown）。
fn strip_inline(s: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(open) {
        out.push_str(&rest[..i]);
        match rest[i..].find(close) {
            Some(j) => {
                // 跳过 close1 和它后面的 close2（`](`）
                let after = i + j + close.len();
                match rest[after..].find(')') {
                    Some(k) => rest = &rest[after + k + 1..],
                    None => {
                        rest = "";
                    }
                }
            }
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// 一个 epoch 毫秒 + UTC 偏移换算成本地的 `(年, 月, 日, 时, 分)`。
///
/// 民用日期那一步用的是 Howard Hinnant 的 `civil_from_days`（把"天数"
/// 直接搬到公历上，不需要查表，也不需要闰年分支）。
fn civil(ms: i64, utc_offset_minutes: i32) -> (i64, u32, u32, u32, u32) {
    let local = ms + i64::from(utc_offset_minutes) * 60_000;
    let days = local.div_euclid(86_400_000);
    let rem = local.rem_euclid(86_400_000);
    let (h, mi) = (rem / 3_600_000, (rem % 3_600_000) / 60_000);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, h as u32, mi as u32)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 文件名里的时间标签：`2026-09-29-1146`。
///
/// 用 `-` 而不是空格或 `:`：空格在命令行里要引号，`:` 在 Windows 上非法。
/// 这个形状**按字典序排就是时间序**，所以在文件管理器里排一下就是时间线。
pub fn stamp(ms: i64, utc_offset_minutes: i32) -> String {
    let (y, m, d, h, mi) = civil(ms, utc_offset_minutes);
    format!("{y:04}-{m:02}-{d:02}-{h:02}{mi:02}")
}

/// front-matter 里的本地时间：`2026-09-29T11:46:00+08:00`。
///
/// 带偏移而不是转成 UTC：读这份 Markdown 的是人，他记笔记时脑子里就是本地时间。
/// 偏移仍然写在里面，所以机器解析也不会歧义。
pub fn iso_local(ms: i64, utc_offset_minutes: i32) -> String {
    let (y, m, d, h, mi) = civil(ms, utc_offset_minutes);
    let sec = (ms + i64::from(utc_offset_minutes) * 60_000).rem_euclid(60_000) / 1000;
    let sign = if utc_offset_minutes < 0 { '-' } else { '+' };
    let abs = utc_offset_minutes.abs();
    format!(
        "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{sec:02}{sign}{:02}:{:02}",
        abs / 60,
        abs % 60
    )
}

/// 一条记录导出成的文件名（不含目录）：`<时间标签> <摘要>.md`。
pub fn file_name(item: &ExportItem, utc_offset_minutes: i32) -> String {
    let label = sanitize_segment(&excerpt(item.body));
    format!(
        "{} {}.md",
        stamp(item.created_at, utc_offset_minutes),
        label
    )
}

/// YAML 字符串：一律加双引号并转义。
///
/// 不加引号的话，一个叫 `重要: 待办` 的标签会把 front-matter 变成另一个结构，
/// 而解析失败**不会报错** —— 读的人只会发现标签莫名其妙没了。
fn yaml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 一条记录导出成的 Markdown 全文。
///
/// `attachment_path` 把 sha 映射成**相对于这条 `.md`** 的路径。
/// 返回 `None` 表示这份字节本地还没有（是"待下载"的占位行）——
/// 那就**原样保留 `attachment:<sha>`**，而不是编一个指不到东西的路径：
/// 前者至少诚实地说明"这里本来有张图"，后者会让读者以为图丢了。
pub fn render_markdown(
    item: &ExportItem,
    utc_offset_minutes: i32,
    attachment_path: impl Fn(&str) -> Option<String>,
) -> String {
    let mut body = item.body.to_string();
    // 复用 `attachment` 的解析：它只认 64 位小写 hex，所以正文里
    // 随便写的 "attachment: 说明" 不会被当成引用去替换。
    for sha in attachment::referenced_shas(item.body) {
        if let Some(path) = attachment_path(&sha) {
            body = body.replace(&format!("{}{}", attachment::SCHEME, sha), &path);
        }
    }

    let tags = item
        .tags
        .iter()
        .map(|t| yaml_string(t))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "---\nchannel: {}\ntags: [{}]\ncreated: {}\nupdated: {}\n---\n\n{}\n",
        yaml_string(item.channel),
        tags,
        iso_local(item.created_at, utc_offset_minutes),
        iso_local(item.updated_at, utc_offset_minutes),
        body.trim_end(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item<'a>(body: &'a str, tags: &'a [String]) -> ExportItem<'a> {
        ExportItem {
            channel: "项目A",
            tags,
            created_at: 0,
            updated_at: 0,
            body,
        }
    }

    #[test]
    fn epoch_and_a_known_timestamp_format_correctly() {
        assert_eq!(stamp(0, 0), "1970-01-01-0000");
        assert_eq!(stamp(86_400_000, 0), "1970-01-02-0000");
        // 1e12 ms 是众所周知的那个时刻：2001-09-09T01:46:40Z
        assert_eq!(stamp(1_000_000_000_000, 0), "2001-09-09-0146");
    }

    #[test]
    fn the_offset_shifts_local_time_and_can_roll_the_day_over() {
        // 东八区：UTC 的 00:00 是本地 08:00
        assert_eq!(stamp(0, 480), "1970-01-01-0800");
        // 差一毫秒到第二天的地方，加上 8 小时就是第二天早上 —— 日期必须跟着进位
        assert_eq!(stamp(86_400_000 - 1, 480), "1970-01-02-0759");
        // 西五区：UTC 的 00:00 还是**前一天** 19:00
        assert_eq!(stamp(0, -300), "1969-12-31-1900");
    }

    #[test]
    fn iso_carries_the_offset_so_it_stays_machine_parseable() {
        assert_eq!(iso_local(0, 480), "1970-01-01T08:00:00+08:00");
        assert_eq!(iso_local(0, -300), "1969-12-31T19:00:00-05:00");
        assert_eq!(iso_local(0, 0), "1970-01-01T00:00:00+00:00");
        // 半小时时区（比如印度 +05:30）不能被压成整点
        assert_eq!(iso_local(0, 330), "1970-01-01T05:30:00+05:30");
    }

    #[test]
    fn illegal_characters_become_underscores_not_vanishing_text() {
        // 换成 `_` 而不是删掉：删掉会让 `a/b` 和 `ab` 撞成同一个名字
        assert_eq!(sanitize_segment("a/b"), "a_b");
        assert_eq!(sanitize_segment("a\\b"), "a_b");
        assert_eq!(sanitize_segment("a:b*c?d\"e<f>g|h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize_segment("tab\there"), "tab_here");
        // 中文、空格、常见标点都该原样留着
        assert_eq!(sanitize_segment("会议记录（周一）"), "会议记录（周一）");
    }

    #[test]
    fn trailing_dots_and_spaces_are_stripped_because_windows_silently_eats_them() {
        // Windows 存 `名字.` 会变成 `名字` —— 两个不同输入撞成一个文件
        assert_eq!(sanitize_segment("名字."), "名字");
        assert_eq!(sanitize_segment("名字..."), "名字");
        assert_eq!(sanitize_segment("名字   "), "名字");
        assert_eq!(sanitize_segment("  名字 . . "), "名字");
    }

    #[test]
    fn reserved_device_names_get_a_prefix_instead_of_failing_to_create() {
        // `CON.md` 在 Windows 上创建会失败，而且报的错和"名字非法"毫无关系
        assert_eq!(sanitize_segment("CON"), "_CON");
        assert_eq!(sanitize_segment("con.txt"), "_con.txt");
        assert_eq!(sanitize_segment("LPT9"), "_LPT9");
        // 不是保留名的别乱改
        assert_eq!(sanitize_segment("CONTENT"), "CONTENT");
        assert_eq!(sanitize_segment("console"), "console");
    }

    #[test]
    fn an_empty_or_wholly_illegal_segment_falls_back_to_something_usable() {
        assert_eq!(sanitize_segment(""), "未命名");
        assert_eq!(sanitize_segment("   "), "未命名");
        // 全是不合法字符时不能返回空串 —— 那会拼出一个指向目录本身的路径
        assert_eq!(sanitize_segment("///"), "___");
    }

    #[test]
    fn long_segments_are_cut_to_keep_paths_within_budget() {
        let s = sanitize_segment(&"很长的名字".repeat(40));
        assert!(
            s.chars().count() <= SEGMENT_CHARS,
            "实际 {} 字",
            s.chars().count()
        );
        // 截断之后不能再以点或空格结尾（否则又踩回上面那个坑）
        assert!(!s.ends_with('.') && !s.ends_with(' '));
    }

    #[test]
    fn file_name_is_time_label_plus_a_readable_excerpt() {
        let tags: Vec<String> = vec![];
        let it = item("会议记录：讨论了下一步计划\n第二行不该出现", &tags);
        let name = file_name(&it, 480);
        assert_eq!(name, "1970-01-01-0800 会议记录：讨论了下一步计划.md");
        // 冒号在文件名里非法，但这里它是**全角**的，必须保留
        assert!(name.contains('：'));
    }

    #[test]
    fn file_name_strips_markdown_markers_without_leaving_debris() {
        let tags: Vec<String> = vec![];
        let it = item("## 标题与**重点**内容", &tags);
        // 残留的 `#`、`*`、`!` 是最难看的 —— 它们不该出现在文件名里
        assert_eq!(file_name(&it, 0), "1970-01-01-0000 标题与重点内容.md");
    }

    #[test]
    fn only_the_first_line_goes_into_the_file_name() {
        // 这是个**有意的**取舍：第一行当标题。多行拼起来会让文件名变长、
        // 并混进不相干的内容，而全文本来就在文件里。
        let tags: Vec<String> = vec![];
        let it = item("第一行是标题\n\n第二行不该进文件名", &tags);
        assert_eq!(file_name(&it, 0), "1970-01-01-0000 第一行是标题.md");
        // 开头是空行也不能被跳过 —— 取的是**第一个非空行**
        let it2 = item("\n\n  真正的第一行  \n后面", &tags);
        assert_eq!(file_name(&it2, 0), "1970-01-01-0000 真正的第一行.md");
    }

    #[test]
    fn file_name_of_an_image_only_note_still_says_something() {
        let tags: Vec<String> = vec![];
        let sha = "a".repeat(64);
        let body = format!("![截图](attachment:{sha})");
        let it = item(&body, &tags);
        // 图被剥掉之后正文就空了 —— 不能因此得到一个只有时间戳的文件名
        let name = file_name(&it, 0);
        assert_eq!(name, "1970-01-01-0000 未命名.md");
    }

    #[test]
    fn front_matter_quotes_values_so_a_colon_in_a_tag_cannot_break_it() {
        let tags = vec!["重要: 待办".to_string(), "带\"引号\"".to_string()];
        let md = render_markdown(&item("正文", &tags), 480, |_| None);
        assert!(md.starts_with("---\n"), "必须以 front-matter 开头");
        assert!(
            md.contains(r#"tags: ["重要: 待办", "带\"引号\""]"#),
            "实际：{md}"
        );
        assert!(md.contains("channel: \"项目A\""));
        assert!(md.contains("created: 1970-01-01T08:00:00+08:00"));
        assert!(md.ends_with("正文\n"));
    }

    #[test]
    fn referenced_attachments_become_relative_paths() {
        let sha = "b".repeat(64);
        let body = format!("看这个 ![截图](attachment:{sha}) 很清楚");
        let md = render_markdown(&item(&body, &[]), 0, |s| {
            assert_eq!(s, sha);
            Some("../attachments/img.png".to_string())
        });
        assert!(md.contains("![截图](../attachments/img.png)"), "实际：{md}");
        assert!(!md.contains("attachment:"), "旧的引用不该留下");
    }

    #[test]
    fn a_missing_attachment_keeps_the_original_reference_instead_of_a_dead_path() {
        // 字节还没下载到（占位行）。编一个指不到东西的路径会让读者以为图丢了；
        // 留着 `attachment:<sha>` 至少诚实说明了"这里本来有张图"。
        let sha = "c".repeat(64);
        let body = format!("![截图](attachment:{sha})");
        let md = render_markdown(&item(&body, &[]), 0, |_| None);
        assert!(md.contains(&format!("attachment:{sha}")));
    }

    #[test]
    fn natural_language_that_merely_starts_with_attachment_is_not_touched() {
        // `referenced_shas` 只认 64 位小写 hex，所以这句不是引用。
        // 这条守的是"别为了导出再写一个宽松的正则"。
        let md = render_markdown(&item("attachment: 见附件说明", &[]), 0, |_| {
            panic!("不该被当成引用去解析")
        });
        assert!(md.contains("attachment: 见附件说明"));
    }

    #[test]
    fn an_uppercase_sha_is_not_a_reference() {
        // 大小写混用会让"同一个文件"在两端算出不同的名字，所以只认小写。
        let body = format!("![x](attachment:{})", "A".repeat(64));
        let md = render_markdown(&item(&body, &[]), 0, |_| panic!("大写 sha 不该被解析"));
        assert!(md.contains(&"A".repeat(64)));
    }
}
