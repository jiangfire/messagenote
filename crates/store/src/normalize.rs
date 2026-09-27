//! 写入前的内容规范化与校验。
//!
//! 两端共用。这些规则看起来琐碎（去空格、去重、非空），但**只要两边写得不一样，
//! 就会变成"桌面端拒绝、网页端接受"** —— 用户看到的是"这个软件时好时坏"，
//! 而没有任何一处报错指向真正的原因。
//!
//! 刻意返回 `&'static str` 而不是定义错误类型：这一层只需要一句话说明
//! "哪里不合法"，由调用方包成自己的错误（桌面端 `AppError`、服务端
//! `ServerError::BadRequest`）。

/// 规范正文。
///
/// 去掉**尾随**空白而不是首尾：聊天式输入里，行尾空格是噪音，但开头的缩进
/// 往往是用户有意写的（代码、层级列表）。
pub fn body(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim_end();
    if trimmed.trim().is_empty() {
        return Err("内容不能为空");
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
