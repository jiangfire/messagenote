//! AI 标签建议的**模型调用**。
//!
//! ## 为什么是"OpenAI 兼容端点"而不是各家 SDK
//!
//! OpenAI 那个 `POST /v1/chat/completions` 的形状如今几乎人人在讲：
//! DeepSeek、Qwen、Kimi、GLM、Ollama、vLLM、LM Studio、llama.cpp server ——
//! 都是同一个请求体、同一套字段。走这一个形状，本地模型和云 API 都一样，
//! **不必为了换一个供应商改代码**。
//!
//! 代价是"只能支持这一种形状"。这是有意的取舍：本项目是**单人自用**笔记，
//! 引入一套多供应商抽象（要处理流式、tool call、思考预算、每家的错误码）
//! 换来的只是一个用不上的功能。
//!
//! ## 密钥只在本机
//!
//! 走桌面端自己的配置，不经过服务端、不进同步、不进变更日志。
//! 一条笔记正文是**高度私密**的东西（这正是这个产品存在的理由），把正文
//! 发到第三方之前必须是用户**自己主动配好的**一件事，而不是默认行为。
//!
//! ## 阻塞式调用，但不在主线程上
//!
//! 用 `ureq` 同步调（和 `http.rs` 同一个理由：不引 reqwest）。命令都标了
//! `#[tauri::command(async)]`，所以它们跑在线程池上，界面不会冻住。

use std::time::Duration;

use crate::error::{AppError, AppResult};

/// 连接超时。端点填错时用户等这么久。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 整体超时。**模型可能慢，而用户在等** —— 给一个上限，
/// 免得一次调用挂住界面十几分钟。
const OVERALL_TIMEOUT: Duration = Duration::from_secs(60);

/// 单次送进模型的正文字数上限。
///
/// **必须限。** 一条十万字的笔记全塞进去，一来慢、二来贵、三来模型多半
/// 直接报错。超出的部分从**尾部**截断并明确告诉调用方 —— 因为开头才是
/// "这条笔记讲什么"。
const MAX_BODY_CHARS: usize = 4000;

/// 最多要几个标签。
///
/// 6 个是"够用又不打扰"：标签是给人扫的，给 20 个等于没有给。
const MAX_TAGS: usize = 6;

/// 模型配置。**本机私有**，不进同步。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmConfig {
    /// 端点根地址，例如 `https://api.openai.com/v1` 或 `http://localhost:11434/v1`。
    ///
    /// 用户填的是**到 `/v1` 为止**：拼 `chat/completions` 是我们的事，
    /// 而各家对这个前缀的叫法不统一（有的叫 base_url，有的叫 api_base，
    /// 有的根本不要 `/v1`）。写死在哪一层比让用户猜要可靠。
    pub base_url: String,
    /// API key。**本地模型（Ollama / LM Studio）可以留空。**
    pub api_key: String,
    /// 模型名，例如 `gpt-4o-mini`、`deepseek-chat`、`qwen2.5:7b`。
    pub model: String,
}

impl LlmConfig {
    /// 配齐了吗。没配齐的话界面不该显示"生成建议"这个按钮 ——
    /// 给一个点了必然失败的入口是最差的交互。
    pub fn is_ready(&self) -> bool {
        let base = self.base_url.trim();
        !base.is_empty()
            && (base.starts_with("http://") || base.starts_with("https://"))
            && !self.model.trim().is_empty()
    }

    /// 没配齐时给用户的一句**可操作**的话（缺什么说什么）。
    pub fn missing_reason(&self) -> Option<&'static str> {
        let base = self.base_url.trim();
        if base.is_empty() {
            return Some("还没填模型地址");
        }
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Some("模型地址必须以 http:// 或 https:// 开头");
        }
        if self.model.trim().is_empty() {
            return Some("还没填模型名");
        }
        None
    }

    fn endpoint(&self) -> AppResult<String> {
        let base = self.base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(AppError::Msg("还没填模型地址".into()));
        }
        Ok(format!("{base}/chat/completions"))
    }
}

/// 送给模型的提示词。
///
/// **刻意不承诺格式。** 要求"只回 JSON"在实践中经常失败，而一个格式没读对的
/// 回答会被 [`parse_tags`](super::parse_tags) 宽容地救回来。反过来，
/// 把规则写进提示词还能让**小模型**（本地 7B 那种）明显更稳。
const SYSTEM_PROMPT: &str = "\
你是一个给个人笔记打标签的助手。
规则：
1. 只输出标签本身，用 JSON 数组，例如 [\"项目A\", \"会议\"]。
2. 最多 6 个。宁缺毋滥 —— 想不出来就少给，不要凑数。
3. 不要解释、不要客套话、不要重复正文里的原句。
4. 标签要短（不超过 8 个字）、具体、可复用。
5. 这条笔记不需要标签时，输出 []。";

/// 把正文裁到 [`MAX_BODY_CHARS`]，并说清**裁没裁**。
///
/// 从**头部**截：开头才是"这条笔记讲什么"，而尾巴往往是引用、链接和零散补充。
fn clamp_body(body: &str) -> (String, bool) {
    let total = body.chars().count();
    if total <= MAX_BODY_CHARS {
        return (body.to_string(), false);
    }
    (body.chars().take(MAX_BODY_CHARS).collect(), true)
}

/// 让模型给这条正文提标签。
///
/// 返回 `(标签, 这条正文是否被截断过)` —— 后者要带回界面：用户有权知道
/// AI 看到的不是他写的全部。
pub fn suggest_tags(cfg: &LlmConfig, body: &str) -> AppResult<(Vec<String>, bool)> {
    if let Some(why) = cfg.missing_reason() {
        return Err(AppError::Msg(format!("AI 还没配好：{why}")));
    }

    let (clamped, was_truncated) = clamp_body(body);

    let answer = call(cfg, &clamped)?;

    // 只从模型的话里取标签 —— 取到几条就是几条，**不因为"不够 6 个"而补**。
    let mut tags = crate::suggest::parse_tags(&answer);
    tags.truncate(MAX_TAGS);

    Ok((tags, was_truncated))
}

/// 发一次请求，拿回模型的话。
fn call(cfg: &LlmConfig, body: &str) -> AppResult<String> {
    let url = cfg.endpoint()?;
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(OVERALL_TIMEOUT))
            // 和 `http.rs` 同一个理由：4xx/5xx 的响应体里有一句人话，
            // 丢掉它用户就只剩一个孤零零的状态码。
            .http_status_as_error(false)
            .build(),
    );

    let payload = serde_json::json!({
        "model": cfg.model.trim(),
        // **温度设 0**：这不是"让答案更对"，而是让同一段正文反复跑出来的
        // 建议尽量一致 —— ROADMAP 那条"生成物要稳定"从这一行开始。
        // 剩下的稳定性靠落库，不靠这个参数（它只是降低方差，不是消除）。
        "temperature": 0,
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": body },
        ],
    });

    let mut req = agent.post(&url);
    let key = cfg.api_key.trim();
    if !key.is_empty() {
        req = req.header("Authorization", &format!("Bearer {key}"));
    }
    let resp = req.send_json(payload).map_err(map_err)?;

    let code = resp.status().as_u16();
    let text = resp
        .into_body()
        .into_with_config()
        // 回答本来就短，但有的模型会先甩一段思考。给个上限，
        // 不然一个坏端点能把内存吃光。
        .limit(1024 * 1024)
        .read_to_string()
        .map_err(map_err)?;

    if !(200..300).contains(&code) {
        // **401/403 要说人话。** "模型地址不对"和"key 不对"是两件完全不同的事，
        // 而裸状态码分不出"没填 key"和"key 填错了"。
        return Err(AppError::Msg(match code {
            401 | 403 => "模型拒绝了这个请求 —— 多半是 API key 不对，或没填（本地模型可以不填）".into(),
            404 => "这个地址下没有 /chat/completions —— 检查地址末尾是不是已经带了 /v1".into(),
            429 => "模型端点限流了，过一会儿再试".into(),
            _ if text.trim().is_empty() => format!("模型返回 HTTP {code}"),
            _ => format!("模型返回 HTTP {code}：{}", text.trim()),
        }));
    }

    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Msg(format!("模型的回答不是 JSON：{e}（原文开头：{}）", head(&text))))?;

    Ok(v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

fn head(s: &str) -> String {
    s.chars().take(120).collect()
}

fn map_err(e: ureq::Error) -> AppError {
    match e {
        ureq::Error::StatusCode(c) => AppError::Msg(format!("模型返回 HTTP {c}")),
        other => AppError::Msg(format!("连不上模型端点：{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> LlmConfig {
        LlmConfig {
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-test".into(),
            model: "gpt-x".into(),
        }
    }

    #[test]
    fn a_filled_config_is_ready() {
        assert!(ready().is_ready());
        assert_eq!(ready().missing_reason(), None);
    }

    #[test]
    fn a_local_model_without_a_key_is_still_ready() {
        // Ollama / LM Studio 不需要 key —— 逼用户填一个假的才难用
        let c = LlmConfig {
            base_url: "http://localhost:11434/v1".into(),
            api_key: String::new(),
            model: "qwen2.5:7b".into(),
        };
        assert!(c.is_ready(), "本地模型不该被必须填 key 挡住");
    }

    #[test]
    fn each_missing_piece_names_itself() {
        let empty = LlmConfig::default();
        assert!(!empty.is_ready());
        assert_eq!(empty.missing_reason(), Some("还没填模型地址"));

        let no_model = LlmConfig {
            base_url: "https://x/v1".into(),
            ..Default::default()
        };
        assert_eq!(no_model.missing_reason(), Some("还没填模型名"));

        let bad_scheme = LlmConfig {
            base_url: "api.example.com".into(),
            model: "m".into(),
            ..Default::default()
        };
        assert_eq!(
            bad_scheme.missing_reason(),
            Some("模型地址必须以 http:// 或 https:// 开头"),
            "少了协议的话请求根本发不出去，要说清"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_produce_a_double_slash() {
        let c = LlmConfig {
            base_url: "https://api.example.com/v1/".into(),
            ..ready()
        };
        assert_eq!(
            c.endpoint().unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn an_unconfigured_config_fails_before_any_network_call() {
        let e = suggest_tags(&LlmConfig::default(), "正文").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("还没填模型地址"), "实际：{msg}");
    }

    /// 正文很长时要截断，而且**必须告诉调用方截断了** ——
    /// 用户有权知道 AI 看到的不是他写的全部。
    #[test]
    fn an_over_long_body_is_truncated_and_the_caller_is_told() {
        let long = "字".repeat(MAX_BODY_CHARS + 500);
        let (clamped, truncated) = clamp_body(&long);
        assert!(truncated, "超上限的正文必须说清楚被截断了");
        assert_eq!(clamped.chars().count(), MAX_BODY_CHARS);

        let short = "短短的一条";
        let (same, truncated) = clamp_body(short);
        assert!(!truncated, "没超上限就不该说截断了");
        assert_eq!(same, short);
    }

    /// 截断必须**按字符**而不是按字节 —— 按字节会把一个汉字劈成半个，
    /// 而半个字符进到 JSON 里就是乱码，甚至让整个请求失败。
    #[test]
    fn clamping_counts_characters_not_bytes() {
        let long = "字".repeat(MAX_BODY_CHARS + 1);
        let (clamped, _) = clamp_body(&long);
        assert_eq!(clamped.chars().count(), MAX_BODY_CHARS);
    }
}
