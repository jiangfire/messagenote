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

use std::io::{Cursor, Write};

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
        attachments: tree.attachments.len(),
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