//! 写入前的内容规范化与校验。
//!
//! 两端共用。这些规则看起来琐碎（去空格、去重、非空），但**只要两边写得不一样，
//! 就会变成"桌面端拒绝、网页端接受"** —— 用户看到的是"这个软件时好时坏"，
//! 而没有任何一处报错指向真正的原因。
//!
//! 刻意返回 `&'static str` 而不是定义错误类型：这一层只需要一句话说明
//! "哪里不合法"，由调用方包成自己的错误（桌面端 `AppError`、服务端
//! `ServerError::BadRequest`）。

/// 单条正文的字符数上限。
///
/// 定这个数的理由是**它是本项目唯一一个能一路撑爆请求体的字段**：附件走
/// 二进制通道、有自己的 25 MB 上限，而正文是 JSON 里的一个字符串，一条 5 MB
/// 的笔记会让 JSON 请求体轻松越过反代和 axum 的默认上限。
///
/// 放在这里（而不是只在服务端检查）是因为这个模块**两端共用** —— 桌面端走
/// 本地库、网页端走 HTTP，两边判得不一样的话用户看到的是「同一个内容在
/// 桌面端能存、在网页端被拒」，而没有任何一处报错指向真正的原因。
///
/// 取 64K 个**字符**而不是字节：中文一个字占 3 字节，按字节算的话实际能写
/// 的中文只有两万多，明显比用户以为的少。
pub const MAX_BODY_CHARS: usize = 64 * 1024;

/// 规范正文。
///
/// 去掉**尾随**空白而不是首尾：聊天式输入里，行尾空格是噪音，但开头的缩进
/// 往往是用户有意写的（代码、层级列表）。
pub fn body(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim_end();
    if trimmed.trim().is_empty() {
        return Err("内容不能为空");
    }
    // 长度按**字符**算：按字节的话中文用户会觉得莫名其妙地更早撞上限。
    if trimmed.chars().count() > MAX_BODY_CHARS {
        return Err("内容太长了");
    }
    Ok(trimmed.to_string())
}

/// 规范频道名。首尾空白一律去掉 —— 名字是身份，`"工作 "` 和 `"工作"`
/// 会被认成两个不同的频道，而用户完全看不出区别。
pub fn channel_name(input: &str) -> Result<String, &'static str> {
    let name = input.trim();
    if name.is_empty() {
        return Err("频道名不能为空");
    }
    Ok(name.to_string())
}

/// 规范标签列表：去空白、去空项、去重，并**保持用户给出的顺序**。
pub fn tags(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let n = n.trim().to_string();
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 超长正文必须被拒 —— 它是唯一能一路撑爆 HTTP 请求体的字段。
    ///
    /// 反向验证过：去掉那道 `chars().count()` 检查，这条立刻红。
    #[test]
    fn an_over_long_body_is_rejected() {
        let huge = "字".repeat(MAX_BODY_CHARS + 1);
        assert_eq!(
            body(&huge).unwrap_err(),
            "内容太长了",
            "超上限的正文要在这里就被拒，而不是等它变成一个 413"
        );
    }

    /// 恰好在上限上的正文要放行（差一个字符就是空跑）。
    #[test]
    fn a_body_exactly_at_the_limit_is_accepted() {
        let at_limit = "字".repeat(MAX_BODY_CHARS);
        assert!(body(&at_limit).is_ok(), "正好到上限不该被拒");
    }

    /// 长度按**字符**算，不是字节。
    ///
    /// 中文一个字 3 字节 —— 按字节算的话上限实际只有两万多字，
    /// 用户会莫名其妙地更早撞墙，而提示里只说「太长了」。
    #[test]
    fn the_limit_counts_characters_not_bytes() {
        // 这个长度按字节算远超上限（3 倍以上），按字符算正好在界内
        let chinese = "字".repeat(MAX_BODY_CHARS);
        assert!(chinese.len() > MAX_BODY_CHARS, "按字节确实超了");
        assert!(
            body(&chinese).is_ok(),
            "64K 个汉字必须放行 —— 按字节判的话中文用户实际只能写两万多字"
        );
    }

    /// 尾随空白照旧去掉，且**不**因此绕过长度上限。
    #[test]
    fn trimming_still_happens() {
        assert_eq!(body("  内容  \n").unwrap(), "  内容");
    }

    #[test]
    fn an_empty_body_is_rejected() {
        assert_eq!(body("   \n\t ").unwrap_err(), "内容不能为空");
    }
}
