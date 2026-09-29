//! 附件：身份、正文引用、类型识别。
//!
//! ## 为什么用内容寻址（sha256 当主键）
//!
//! 三个好处，每一个都省掉一整类代码：
//!
//! 1. **不可变。** 字节的哈希就是它的名字，所以"这张图变了"这件事不存在 ——
//!    换图必然换名字。于是附件**没有任何冲突要解决**，不需要 HLC、不需要 LWW、
//!    不需要墓碑。这也是它**不进变更日志**的原因：变更日志整套机制是为了解决
//!    "两边都改了谁赢"，而这里根本没有"改"这个动作。硬塞进去只会得到一堆
//!    永远不参与冲突解决的 HLC 字段。
//! 2. **天然去重。** 同一张图粘两次、两台设备各粘一次，落库都是一份。
//! 3. **同步变成一句话**："确保两端都有这个 sha 的字节"。不需要 diff、不需要
//!    版本协商、不需要重传判断 —— 有就跳过，没有就传。
//!
//! ## 正文里的引用形式
//!
//! `attachment:<64 位小写 hex>`，放在正常的 Markdown 图片语法里：
//!
//! ```text
//! ![截图](attachment:3f79bb7b435b05321651daefd374cdc681dc06faa65e374e38337b88ca046dea)
//! ```
//!
//! 刻意**不做成独立的 attachment 表去关联 message**：正文就是笔记本身，
//! 这样编辑、检索、同步、导出全都不需要为附件加特例。代价是"哪些附件还在被
//! 引用"要靠扫正文回答（见 [`referenced_shas`]）—— 对一个个人笔记库这点成本
//! 可以忽略，而它换来的是**没有第二处需要保持一致的状态**。

use sha2::{Digest, Sha256};

/// 正文里引用附件的协议前缀。
pub const SCHEME: &str = "attachment:";

/// sha256 的十六进制长度。
pub const SHA256_HEX_LEN: usize = 64;

/// 单个附件的上限。
///
/// 定这个数不是怕存不下，而是因为这条路径上每一层都会把整个字节数组握在手里
/// （HTTP 请求体、IPC、SQLite BLOB）。真正的流式传输要改动每一层，
/// 而个人笔记里的图片不该有 25 MB —— 真有的话，正确的做法是压一下再放进来。
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

/// 算出内容的 sha256，小写十六进制。
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();

    // 手写十六进制而不是引 hex crate：18 行 vs 一个依赖，
    // 而且这段代码要两端算出**完全相同**的结果，简单到一眼能看完更让人放心。
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(SHA256_HEX_LEN);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 是不是一个合法的 sha256 名字。
///
/// 只认**小写**。大小写混用会让"同一个文件"在两端算出不同的名字，
/// 从而绕过去重、并且让 `WHERE sha256 = ?` 查不到已经存在的那份。
pub fn is_sha256(s: &str) -> bool {
    s.len() == SHA256_HEX_LEN
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 从正文里抽出所有被引用的附件 sha，按出现顺序、已去重。
///
/// 这是"哪些附件还在被使用"的唯一判据 —— 垃圾回收要靠它。
/// 扫到不认识的前缀就继续往后找，所以正文里随便写一句 `attachment: 说明`
/// 不会有副作用。
pub fn referenced_shas(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = body.as_bytes();
    let mut cursor = 0usize;

    while let Some(found) = body[cursor..].find(SCHEME) {
        let start = cursor + found + SCHEME.len();
        if let Some(sha) = sha_at(bytes, start) {
            if !out.iter().any(|s| s == &sha) {
                out.push(sha);
            }
        }
        // 无论认没认出来都往后走，否则同一个位置会被反复找到
        cursor = start;
        if cursor >= body.len() {
            break;
        }
    }
    out
}

/// 从 `start` 开始读 64 个小写十六进制字符。
///
/// 手工按字节比较而不是先切片再 `is_sha256`：`start` 只保证落在字符边界上
/// （`SCHEME` 是 ASCII，所以加上它的长度仍然在边界上），后面的内容可能是中文，
/// 直接 `&body[start..start+64]` 会在字符中间切断而 panic。
fn sha_at(bytes: &[u8], start: usize) -> Option<String> {
    let end = start.checked_add(SHA256_HEX_LEN)?;
    if end > bytes.len() {
        return None;
    }
    let slice = &bytes[start..end];
    if !slice
        .iter()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return None;
    }
    // 上一步已经确认全是 ASCII
    Some(String::from_utf8_lossy(slice).into_owned())
}

/// 附件在 Markdown 里的引用文本。
pub fn reference(sha: &str) -> String {
    format!("{SCHEME}{sha}")
}

/// 一整套 Markdown 图片引用。
pub fn image_markdown(sha: &str, alt: &str) -> String {
    format!("![{alt}]({})", reference(sha))
}

// ---------------------------------------------------------------- 类型识别

/// 按**内容**判断类型，绝不采信调用方给的 MIME。
///
/// 服务端如果把客户端声明的类型原样回给浏览器，一个声明成 `text/html` 的
/// 上传文件就变成了**存储型 XSS**（同源部署下尤其致命 —— 网页端和服务端
/// 是同一个 origin，脚本能直接读走会话）。所以类型只由字节决定，
/// 而且只认下面这几种白名单。
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // RIFF....WEBP
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"%PDF-") {
        return Some("application/pdf");
    }
    None
}

/// 认不出来的类型。
///
/// 仍然允许存（"发个文件"是合理需求），但服务端会用
/// `Content-Disposition: attachment` 发出去，绝不当成可渲染内容。
pub const FALLBACK_MIME: &str = "application/octet-stream";

/// 能否内联展示（也就是 `<img>` 能画出来的）。
pub fn is_displayable_image(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// 归一化类型：嗅探优先，认不出来就是兜底类型。
pub fn resolve_mime(bytes: &[u8]) -> &'static str {
    sniff_mime(bytes).unwrap_or(FALLBACK_MIME)
}

/// 导出时给这个类型用的文件扩展名。
///
/// 认不出来的一律 `.bin`：扩展名是**给别人看的**（文件管理器、Markdown 阅读器），
/// 编一个 `.png` 出来只会让打不开的图看起来像本来就坏了。
///
/// 白名单必须和 [`is_displayable_image`] 保持一致 —— 差一个的话，能渲染的类型
/// 会被写成 `.bin`，于是 Markdown 里的图渲染不出来，而且不报错。
pub fn extension_for(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        // 标准测试向量。算错了整条内容寻址就是错的，而且错得很安静：
        // 每台设备都"算得出"一个值，只是各不相同，表现为永远在重传。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn is_sha256_is_strict_about_case_and_length() {
        assert!(is_sha256(&sha256_hex(b"x")));
        assert!(!is_sha256(""), "空串不是");
        assert!(!is_sha256(&sha256_hex(b"x").to_uppercase()), "大写不算");
        assert!(!is_sha256(&sha256_hex(b"x")[..63]), "少一位不算");
        assert!(!is_sha256(&format!("{}z", &sha256_hex(b"x")[..63])));
        // 'g' 不在十六进制里 —— 只检查长度的话这里会漏
        assert!(!is_sha256(&format!("{}g", &sha256_hex(b"x")[..63])));
    }

    #[test]
    fn referenced_shas_finds_every_reference() {
        let a = sha256_hex(b"a");
        let b = sha256_hex(b"b");
        let body = format!(
            "看图\n\n{}\n\ntext\n\n{}",
            image_markdown(&a, "截图"),
            reference(&b)
        );

        assert_eq!(referenced_shas(&body), vec![a, b]);
    }

    #[test]
    fn referenced_shas_dedups_keeping_order() {
        let a = sha256_hex(b"a");
        let b = sha256_hex(b"b");
        let body = format!("{} {} {}", reference(&b), reference(&a), reference(&b));
        assert_eq!(referenced_shas(&body), vec![b, a]);
    }

    #[test]
    fn referenced_shas_ignores_malformed() {
        // 短了、长了、有大写、有非十六进制字符 —— 一个都不能收
        assert!(referenced_shas("attachment:abc").is_empty());
        assert!(referenced_shas(&format!("attachment:{}", "z".repeat(64))).is_empty());
        assert!(referenced_shas("attachment:").is_empty());

        let sha = sha256_hex(b"x");
        // 正好 64 位后面还跟着别的十六进制字符：应当只取前 64 位
        assert_eq!(
            referenced_shas(&format!("attachment:{sha}ff")),
            vec![sha],
            "多出来的字符不该让整条引用失效"
        );
    }

    #[test]
    fn referenced_shas_does_not_panic_on_multibyte_boundaries() {
        // 手工按下标扫描时最容易踩的坑：在字符中间切片会 panic。
        // 这里让中文紧贴着引用，逼出这个问题。
        let sha = sha256_hex(b"x");
        for body in [
            format!("中文{SCHEME}"),
            format!("中文{SCHEME}{sha}"),
            format!("{SCHEME}中文"),
            format!("中文{SCHEME}短"),
            "attachment:中".to_string(),
            format!("中attachment:文{sha}中"),
        ] {
            let _ = referenced_shas(&body); // 不 panic 就算过
        }

        // 中文在引用**两侧**时，引用本身仍然完整
        assert_eq!(
            referenced_shas(&format!("中{}文", reference(&sha))),
            vec![sha.clone()]
        );

        // 而中文落在 `attachment:` 和 sha 之间时，它就不是一条引用了 ——
        // 前缀后面必须紧跟 64 位十六进制。这不是缺陷：正文里写
        // "attachment: 见附件" 这类自然语言不该被当成引用。
        assert!(referenced_shas(&format!("中{SCHEME}文{sha}中")).is_empty());
    }

    #[test]
    fn sniff_mime_reads_magic_bytes() {
        assert_eq!(
            sniff_mime(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0]),
            Some("image/png")
        );
        assert_eq!(sniff_mime(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_mime(b"GIF87a...."), Some("image/gif"));
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_mime(b"%PDF-1.7"), Some("application/pdf"));

        assert_eq!(sniff_mime(b"<html>"), None);
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WAVE"), None, "RIFF 但不是 WEBP");
        assert_eq!(sniff_mime(b"GIF"), None, "太短，不能靠前缀猜");
        assert_eq!(sniff_mime(b""), None);
    }

    #[test]
    fn a_declared_html_type_is_never_believed() {
        // 这条是整个类型识别的理由：上传一份 HTML，就算调用方说它是 image/png，
        // 也只能得到兜底类型。
        let html = b"<script>fetch('/api/timeline')</script>";
        assert_eq!(sniff_mime(html), None);
        assert_eq!(resolve_mime(html), FALLBACK_MIME);
        assert!(!is_displayable_image(resolve_mime(html)));
    }

    #[test]
    fn displayable_is_a_whitelist_not_a_prefix_check() {
        assert!(is_displayable_image("image/png"));
        assert!(is_displayable_image("image/webp"));
        assert!(!is_displayable_image("image/svg+xml"), "SVG 能带脚本");
        assert!(!is_displayable_image(FALLBACK_MIME));
        assert!(!is_displayable_image("text/html"));
    }
}
