//! 网页端导出：把整棵导出树打成一个 zip。
//!
//! **为什么在服务端做而不是浏览器里做**
//!
//! 浏览器里当然也能生成 zip（`CompressionStream` 之类），但那意味着附件字节要
//! 一张一张从 `/api/blob` 拉回来、再在前端拼 zip：一份图多的库几百 MB，
//! 每张图一次请求，而服务端本来就在同一个库旁边。
//!
//! 组装逻辑（频道目录、重名去重、附件相对路径、缺字节怎么办）在
//! [`messagenote_core::export::build_tree`]，桌面端写磁盘、这里写 zip，
//! **两端共用同一份** —— 否则重名去重和附件路径总有一天会对不上，
//! 而那种不一致只有用户打开导出物、发现图裂了的时候才暴露。
//!
//! ## 内存
//!
//! zip 先在内存里攒好再一次性发出。**这是一个有意的取舍**：流式 zip 要求
//! 一边读字节一边压缩，而附件可能在对象存储里（`get_blob` 是 async），
//! 真要流起来就得引入临时文件或跨任务管道，两条路都各自多出一批失败模式
//! （磁盘满、临时文件残留）。自建服务端的内存按几百 MB 算是正常的，
//! 所以改成**先量、超了就明说**：
//!
//! 累计超过 [`MAX_EXPORT_BYTES`] 直接 413 并给出可操作的建议（缩小筛选范围，
//! 或者用桌面端导出）。**不能默默截断** —— 那会得到一个"看起来导出了、
//! 其实少了一半"的 zip，而用户没有任何办法发现。
//!
//! ## 并发
//!
//! [`MAX_EXPORT_BYTES`] 是**单个请求**的上限，而峰值内存是
//! 「N 个并发 × 上限」。N 个并发导出请求各自独立攒一份 zip，所以这里用
//! [`export_gate`] 把同时进行的导出限在 [`MAX_CONCURRENT_EXPORTS`] 个：
//! 没拿到名额的请求直接 429 并说清"现在有别的导出在进行"，而不是排队等着
//! 把内存慢慢堆上去 —— 排队只是把 OOM 推迟，并不会让它不发生。

use std::io::{Cursor, Write};
use std::sync::Mutex;

use messagenote_core::export::{self, ExportItem};
use messagenote_core::attachment;
use zip::write::SimpleFileOptions;

use crate::error::{ServerError, ServerResult};
use crate::store::Store;

/// 单次导出允许占用的内存上限。
///
/// 不是一个"随便挑的数"：它是**这个取舍的边界**。超过就明说，而不是硬扛到
/// 进程被 OOM 杀掉 —— 被杀掉的话用户看到的是 502，而真正的原因是"导出太大了"。
pub const MAX_EXPORT_BYTES: usize = 256 * 1024 * 1024;

/// 同时最多进行几个导出。
///
/// 取 1 是刻意的：自建服务端通常只有一个人在用，而一次全量导出已经能把内存
/// 吃到几百 MB。两个人同时点导出而把进程 OOM 杀掉，比第二个人多等几秒糟糕得多。
pub const MAX_CONCURRENT_EXPORTS: usize = 1;

/// 同时进行的导出计数（信号量的等价物，够用且不引新依赖）。
static EXPORT_INFLIGHT: Mutex<usize> = Mutex::new(0);

/// 拿到导出名额；返回的 guard 在**离开作用域时**自动归还名额。
///
/// 用 `Mutex<usize>` 而不是 `tokio::sync::Semaphore`：名额只用来做**计数**，
/// 拿不到就立刻返回 429 而不是等待，所以不需要异步信号量那套
/// `acquire_owned` + 异步唤醒。guard 靠 RAII 归还，避免"中途 return 忘了减"。
#[derive(Debug)]
pub struct ExportGate;

impl ExportGate {
    fn acquire() -> ServerResult<Self> {
        let mut n = EXPORT_INFLIGHT
            .lock()
            .map_err(|_| ServerError::Msg("导出并发计数已损坏，请重启服务端".into()))?;
        if *n >= MAX_CONCURRENT_EXPORTS {
            return Err(ServerError::too_many_exports());
        }
        *n += 1;
        Ok(Self)
    }
}

impl Drop for ExportGate {
    fn drop(&mut self) {
        if let Ok(mut n) = EXPORT_INFLIGHT.lock() {
            *n = n.saturating_sub(1);
        }
    }
}

/// 供 `api.rs` 使用的入口：先拿名额，再干活。
pub async fn gated_build_zip(store: &Store, query: &ExportQuery) -> ServerResult<ZipExport> {
    let _gate = ExportGate::acquire()?;
    build_zip(store, query).await
}

/// zip 里用的默认压缩方式。
///
/// 选 `Stored`（不压缩）**是刻意的**：附件字节（png/jpg/webp）本来就已经压过，
/// 再 deflate 一遍纯属白烧 CPU，而 Markdown 那一点点文本压不压缩都无所谓。
/// 代价是导出物比理论最小值大一点 —— 对一个自用的笔记导出，这个取舍划算。
const COMPRESSION: zip::CompressionMethod = zip::CompressionMethod::Stored;

/// 导出用的筛选。四个都空 = 全量。
#[derive(Debug, Clone, Default)]
pub struct ExportQuery {
    pub channel_id: Option<String>,
    pub tag: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// 前端时区相对 UTC 的偏移（**东为正**，东八区 480）。
    ///
    /// 服务端不解释任何时区约定：文件名里要的是**本地**时间，而服务端根本
    /// 不知道用户在哪。传 480 就是传 480，不是"按服务端时区算"。
    pub utc_offset_minutes: i32,
}

impl ExportQuery {
    fn as_filter(&self) -> export::ExportFilter {
        export::ExportFilter {
            channel_id: self.channel_id.clone(),
            tag: self.tag.clone(),
            from_ms: self.from_ms,
            to_ms: self.to_ms,
        }
    }
}

/// 导出结果：zip 字节，以及**给用户看的一句摘要**。
///
/// 摘要不是装饰：`missing_attachments` 对应一件用户要决定的事
/// （那些图是没下载下来，还是真没了），不给数字的话用户只会以为导出漏了。
pub struct ZipExport {
    pub bytes: Vec<u8>,
    pub messages: usize,
    pub attachments: usize,
    pub missing_attachments: usize,
}

/// 把筛选命中的记录打成 zip。
pub async fn build_zip(store: &Store, query: &ExportQuery) -> ServerResult<ZipExport> {
    let filter = query.as_filter();

    // 取数走共享层 —— 和桌面端导出命中的是同一批记录。
    let messages = store.export_selection(&filter)?;

    // **先按原始体积卡一道，再往下走。**
    //
    // 下面三处 `written > MAX_EXPORT_BYTES` 都在"已经读进内存之后"才判：
    // 记录的正文、`build_tree` 渲染出的全文、以及最终那份 zip。也就是说
    // 一个 300 MB 的库会在返回 413 **之前**先把正文和渲染结果都吃进内存 ——
    // 那正是这个上限本来要防的场景。
    //
    // 所以这里先按 body 之和卡一道：它与渲染后的体积同量级（渲染只多出
    // front-matter 和附件引用），能挡住"库本身就已经超了"这个主要情形。
    // 剩下的（渲染放大、附件字节）由下面几处继续兜。
    let raw_bytes: usize = messages.iter().map(|m| m.body.len()).sum();
    if raw_bytes > MAX_EXPORT_BYTES {
        return Err(ServerError::payload_too_large(export_too_large_message()));
    }

    let channel_names = store
        .list_channels()
        .map_err(|e| ServerError::Msg(format!("读频道失败：{e}")))?
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect::<std::collections::HashMap<_, _>>();

    // 第一遍：哪些附件**真的**有字节。没有的不进 `available`，于是正文里
    // 原样保留 `attachment:<sha>` —— 诚实地说明"这里本来有张图"，
    // 而不是编一个指不到东西的路径。
    //
    // **这里会为每份附件读一次字节、只为拿它的 MIME。** 代价说清楚：默认的
    // SQLite 模式下那只是一次本地页读，很便宜；S3 模式下就是多一轮 GET，
    // 所以字节真正的写入是**第二遍**。不这么做的替代方案是把所有字节先捏在
    // 内存里，那等于把峰值从"一份 zip"变成"一份 zip + 整个附件库" ——
    // 那才是真的会撑爆内存的那个。
    let mut available: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut counted: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in &messages {
        for sha in attachment::referenced_shas(&m.body) {
            if !counted.insert(sha.clone()) {
                continue;
            }
            // 扩展名由**字节**决定（`extension_for`），不由文件名或请求头决定。
            if let Some((mime, _bytes)) = store.get_blob(&sha).await? {
                available.insert(sha, attachment::extension_for(&mime).to_string());
            }
        }
    }

    let items: Vec<ExportItem<'_>> = messages
        .iter()
        .map(|m| ExportItem {
            channel: channel_names
                .get(&m.channel_id)
                .map(String::as_str)
                .unwrap_or(export::ORPHAN_CHANNEL),
            tags: &m.tags,
            created_at: m.created_at,
            updated_at: m.updated_at,
            body: &m.body,
        })
        .collect();
    let tree = export::build_tree(&items, query.utc_offset_minutes, &available);

    let mut buf = Cursor::new(Vec::<u8>::new());
    // 声明在块外：`buf` 被 `zip` 独占借走的那段时间里，它也要活到最后。
    let mut written_attachments = 0usize;
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = SimpleFileOptions::default().compression_method(COMPRESSION);

        // 单独记一份累计长度：`buf` 此刻被 `zip` 独占借走了，不能去读它。
        let mut written: usize = 0;

        for f in &tree.messages {
            written += f.markdown.len();
            if written > MAX_EXPORT_BYTES {
                return Err(ServerError::payload_too_large(export_too_large_message()));
            }
            zip.start_file(f.path.replace('\\', "/"), opts)
                .map_err(|e| ServerError::Msg(format!("写 zip 条目失败：{e}")))?;
            zip.write_all(f.markdown.as_bytes())
                .map_err(|e| ServerError::Msg(format!("写 zip 条目失败：{e}")))?;
        }

        // **数真正写进去的**，`tree.attachments.len()` 会在第二遍跳过时多报：
        // 那会让"有 N 个附件没带出来"凭空多出几张图，而用户去导出物里找不到
        // 任何缺的地方 —— 报的数字对不上实物，等于没报。
        for a in &tree.attachments {
            let Some((_mime, bytes)) = store.get_blob(&a.sha).await? else {
                // 第一遍说有、第二遍没有：只有并发写入才可能。跳过而不是
                // 写一个 0 字节的文件 —— 后者会让用户以为图是空的。
                continue;
            };
            written += bytes.len();
            if written > MAX_EXPORT_BYTES {
                return Err(ServerError::payload_too_large(export_too_large_message()));
            }
            zip.start_file(a.path.replace('\\', "/"), opts)
                .map_err(|e| ServerError::Msg(format!("写 zip 条目失败：{e}")))?;
            zip.write_all(&bytes)
                .map_err(|e| ServerError::Msg(format!("写 zip 条目失败：{e}")))?;
            written_attachments += 1;
        }

        zip.finish()
            .map_err(|e| ServerError::Msg(format!("收尾 zip 失败：{e}")))?;
    }

    let bytes = buf.into_inner();
    if bytes.len() > MAX_EXPORT_BYTES {
        return Err(ServerError::payload_too_large(export_too_large_message()));
    }

    Ok(ZipExport {
        bytes,
        messages: tree.messages.len(),
        attachments: written_attachments,
        missing_attachments: tree.missing_attachments,
    })
}

/// 超限时给用户的话。**要指向真原因，也要给出路** ——
/// 只说"太大了"的话，用户只能干看着。
fn export_too_large_message() -> String {
    format!(
        "这次导出超过 {} MB，服务端不会硬拼（那会被系统直接杀掉，你会看到一个没头没尾的失败）。\
         缩小筛选范围再试，或者用桌面端的「导出记录…」——它写磁盘，不占内存。",
        MAX_EXPORT_BYTES / 1024 / 1024
    )
}

/// 下载用的文件名。带日期是为了用户在下载列表里分得清是哪一次。
pub fn zip_file_name(query: &ExportQuery) -> String {
    let day = export::stamp(now_ms(), query.utc_offset_minutes);
    format!("messagenote-{day}.zip")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 峰值内存是「N 个并发 × MAX_EXPORT_BYTES」，所以必须限流。
    ///
    /// 少了这道闸门，两个并发导出就是 2×256 MB —— 而那是**服务端被杀**，
    /// 用户看到的是一个没有头绪的 502 / 连接重置。
    #[test]
    fn only_one_export_may_run_at_a_time() {
        let first = ExportGate::acquire();
        assert!(first.is_ok(), "第一次导出必须拿得到名额");

        // 第二次在第一次还没结束时必须被拒，且要说清是"等一会儿"
        let second = ExportGate::acquire().expect_err("并发导出必须被闸门挡住");
        assert!(
            matches!(second, ServerError::TooManyExports(_)),
            "必须是 429（稍后重试），不能是 413（换个做法）"
        );
        let text = second.to_string();
        assert!(
            text.contains("等") || text.contains("重试"),
            "文案要指向'稍后重试'，而不是让用户去缩小筛选范围：{text}"
        );

        // 第一个 guard 离开作用域后名额必须归还 —— 否则一次失败就把闸门焊死了
        drop(first);
        assert!(
            ExportGate::acquire().is_ok(),
            "前一次结束后必须能再拿到名额"
        );
    }

    #[test]
    fn the_gate_is_released_even_when_the_export_fails() {
        let result: ServerResult<()> = (|| {
            let _gate = ExportGate::acquire()?;
            Err(ServerError::Msg("模拟导出失败".into()))
        })();
        assert!(result.is_err());
        assert!(
            ExportGate::acquire().is_ok(),
            "失败路径也必须归还名额（RAII，不是手动 unlock）"
        );
    }
}
