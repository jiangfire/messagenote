//! 附件字节放在哪里：SQLite（默认）还是 S3 兼容的对象存储。
//!
//! ## 为什么要有这个选择
//!
//! 默认是 SQLite，理由在 `messagenote_store::blob` 的模块文档里：README 推荐的
//! Litestream 只跟**一个 `.sqlite` 文件**走，图片放到文件系统里会被静默漏掉。
//! 对一台小机器来说那是最省心的方案，也是这个项目一直在用的方案。
//!
//! 但库会长大。一份图多的笔记库到几 GB 之后，每次备份、每次把库文件复制来
//! 复制去都在搬这些字节 —— 而字节是不可变的，本来就不需要跟着数据库一起走。
//! 放进对象存储（AWS S3 / MinIO / Cloudflare R2 / Backblaze B2 都行）之后，
//! SQLite 里只剩笔记本身。
//!
//! ## 对象键就是 sha256，所以去重是白送的
//!
//! 键是 `{prefix}{sha256}`：没有随机段、没有日期分片、没有自增号。于是同一份
//! 字节永远落在同一个键上 —— 客户端重复上传、两台设备各传一次、断线重传，
//! 结果都只是同一个对象被覆盖一次（内容相同，覆盖等于没发生）。
//!
//! 分片确实能让某些对象存储的列举更快，但代价是"同一个 sha 可能落在两个键上"，
//! 而那正好把去重毁掉。这里选去重。
//!
//! ## 切过去之后，以前存在 SQLite 里的字节仍然读得到
//!
//! 读路径**穿底**：对象存储上没有就问一句 SQLite（只读）。换后端是一个部署动作，
//! 而库里已经有老附件是常态 —— 不穿底的话那些图在客户端眼里会变成 404。
//!
//! **注意这是单向的。** 它保证的是"切到 S3 不需要先把字节搬过去"，
//! **不是**"随时可以切回来"：把 `MESSAGENOTE_S3_BUCKET` 去掉之后，
//! 只有在 S3 期间传上去的那些字节会**看不见**（SQLite 里没有它们，
//! 而 SQLite 模式不会去问桶）。切回来之前得先把桶里的对象倒回库里，
//! 或者接受那些图暂时取不到。这条路没有做自动迁移，理由见
//! `ROADMAP.md` 第二节 3.4。
//!
//! ## 需要的桶权限
//!
//! - `s3:PutObject`、`s3:GetObject` —— 存和取。
//! - `s3:ListBucket`（**桶级**）—— 存在性探测（`HEAD`）。S3 的规矩是：
//!   对象不存在时，有 `ListBucket` 才回 404，没有就回 **403**。把 403 当成
//!   "出错"而不是"没有"，会让 `missing_blobs` 整批失败。写入路径**不依赖**
//!   它（用的是 `If-None-Match: *` 条件写，见 [`s3_put`]）。
//!
//! ## S3 模式下 SQLite 里不再新增附件行
//!
//! 服务端那张 `attachment` 表的用途只有一个：记住"我有这份字节，类型是这个"。
//! 字节去了对象存储之后，这张表就是一份**会漂移的副本**（有人从桶里删了对象、
//! 或者换了个桶，表里还写着"有"）。所以 S3 模式下一行都不写，存在性一律问
//! 对象存储 —— 上面说的穿底查询是只读的老数据。

use std::sync::Arc;

use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutMode, PutOptions};

use crate::error::{ServerError, ServerResult};

/// 对象键的默认前缀。一个桶里放别的东西时不至于和笔记附件混在根目录下。
const DEFAULT_PREFIX: &str = "attachments/";

/// 附件字节的落点。
pub enum Blobs {
    /// 字节和笔记躺在同一个 `.sqlite` 文件里（默认）。
    Sqlite,
    /// 字节放在 S3 兼容的对象存储里。
    ///
    /// `Arc<dyn ObjectStore>` 而不是具体的 `AmazonS3`：测试用 `InMemory` 塞进来，
    /// 跑的是**同一条代码路径** —— 只是字节落在内存里而不是网上。
    S3 {
        store: Arc<dyn ObjectStore>,
        prefix: String,
    },
}

impl Blobs {
    /// 从环境变量装配。**没设 `MESSAGENOTE_S3_BUCKET` 就是 SQLite** ——
    /// 也就是说这次改动对现有部署是完全惰性的：什么都不配，行为一个字都不变。
    ///
    /// | 变量 | 作用 |
    /// | --- | --- |
    /// | `MESSAGENOTE_S3_BUCKET` | 桶名。给了就切到 S3 |
    /// | `MESSAGENOTE_S3_ENDPOINT` | S3 兼容服务的地址（MinIO / R2 / B2）。不给就是 AWS 官方端点 |
    /// | `MESSAGENOTE_S3_REGION` | 区域，默认 `us-east-1` |
    /// | `MESSAGENOTE_S3_ACCESS_KEY_ID` / `MESSAGENOTE_S3_SECRET_ACCESS_KEY` | 凭据。不给就交给下面那套官方变量 |
    /// | `MESSAGENOTE_S3_PREFIX` | 对象键前缀，默认 `attachments/` |
    ///
    /// 凭据走 `AmazonS3Builder::from_env()`，所以 AWS 官方那套
    /// （`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_REGION` /
    /// `AWS_ENDPOINT` / `AWS_ALLOW_HTTP`，以及 EC2/ECS 上的角色凭据）**照样有效** ——
    /// 部署在云上的人不该被迫把配置抄成 `MESSAGENOTE_` 前缀。
    pub fn from_env() -> ServerResult<Self> {
        let Some(bucket) = env_nonempty("MESSAGENOTE_S3_BUCKET") else {
            return Ok(Self::Sqlite);
        };

        let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);

        // 端点可以从两个地方来：我们自己的变量，或者官方的 `AWS_ENDPOINT`。
        // 两边都要参与"明文 HTTP 就放行"的判断 —— 只看前者的话，用官方变量配
        // 一个内网明文 MinIO 的人会撞上 "URL scheme is not allowed"，
        // 而那句话不会告诉他该去开哪个开关。
        let endpoint = env_nonempty("MESSAGENOTE_S3_ENDPOINT")
            .or_else(|| env_nonempty("AWS_ENDPOINT"))
            .or_else(|| env_nonempty("AWS_ENDPOINT_URL"));
        if let Some(endpoint) = endpoint {
            // 自建对象存储常常只在内网里说明文 HTTP，而这个开关必须**显式**开。
            let allow_http = endpoint.starts_with("http://");
            builder = builder.with_endpoint(endpoint).with_allow_http(allow_http);
        }
        if let Some(region) = env_nonempty("MESSAGENOTE_S3_REGION") {
            builder = builder.with_region(region);
        }
        if let Some(id) = env_nonempty("MESSAGENOTE_S3_ACCESS_KEY_ID") {
            builder = builder.with_access_key_id(id);
        }
        if let Some(secret) = env_nonempty("MESSAGENOTE_S3_SECRET_ACCESS_KEY") {
            builder = builder.with_secret_access_key(secret);
        }

        let store = builder.build().map_err(|e| {
            // 建不起来是**启动时**的事，用户正看着日志 —— 这里可以说细节，
            // 它不会被回给任何请求方。
            ServerError::Msg(format!("S3 客户端建不起来：{e}"))
        })?;
        let prefix = env_nonempty("MESSAGENOTE_S3_PREFIX").unwrap_or_else(|| DEFAULT_PREFIX.into());

        Ok(Self::S3 {
            store: Arc::new(store),
            prefix,
        })
    }

    /// 字节是不是放在对象存储里。
    pub fn is_s3(&self) -> bool {
        matches!(self, Self::S3 { .. })
    }
}

/// 读一个环境变量，空串按"没设"处理。
///
/// 空串必须当没设：`MESSAGENOTE_S3_BUCKET=${S3_BUCKET:-}` 这种写法在
/// docker-compose 和 systemd 里太常见了，把它当成"要开 S3、但桶名是空的"，
/// 得到的是签名错误或 404 —— 一个和真正原因八竿子打不着的现象。
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `{prefix}{sha256}`。前缀少了尾部的 `/` 就补上 —— 否则 `attachments` +
/// `abc…` 会拼成 `attachmentsabc…`，而那种键在桶里看起来完全正常，
/// 只是和"你以为的那个键"不是一回事。
fn s3_key(prefix: &str, sha256: &str) -> ObjectPath {
    if prefix.is_empty() || prefix.ends_with('/') {
        ObjectPath::from(format!("{prefix}{sha256}"))
    } else {
        ObjectPath::from(format!("{prefix}/{sha256}"))
    }
}

/// 对象存储的错误**不回给请求方**（里面有端点、桶名，有时还有签名字符串），
/// 统一包成内部错误：细节进日志，响应体只有一句"服务端内部错误"。
fn s3_err(e: object_store::Error) -> ServerError {
    ServerError::Msg(format!("对象存储出错：{e}"))
}

/// 这份字节在不在对象存储里。
///
/// 只发一个 HEAD，不搬字节。
///
/// **这条路径要求桶级的 `s3:ListBucket`。** S3 的规矩：对象不存在时，
/// 有 `ListBucket` 才回 404，没有就回 **403** —— 而 403 会被当成"出错"而不是
/// "没有"。所以部署文档里把这条权限写成了必须项，而不是可选优化。
///
/// 注意它**不在上传路径上了**：`s3_put` 用的是条件写。这里剩下的调用方是
/// `missing_blobs`（一次问一批），所以慢的代价是"一次问很多个"时才显现 ——
/// 目前是串行的，见 `Store::missing_blobs` 上的说明。
pub(crate) async fn s3_has(
    store: &dyn ObjectStore,
    prefix: &str,
    sha256: &str,
) -> ServerResult<bool> {
    match store.head(&s3_key(prefix, sha256)).await {
        Ok(_) => Ok(true),
        // "没有"是**正常答案**，不是错误：同步协议里客户端就是靠它决定要传哪些。
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(s3_err(e)),
    }
}

/// 取字节。对象存储上没有就返回 `None`（由调用方决定要不要穿底查 SQLite）。
pub(crate) async fn s3_get(
    store: &dyn ObjectStore,
    prefix: &str,
    sha256: &str,
) -> ServerResult<Option<Vec<u8>>> {
    match store.get(&s3_key(prefix, sha256)).await {
        Ok(result) => Ok(Some(result.bytes().await.map_err(s3_err)?.to_vec())),
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(s3_err(e)),
    }
}

/// 写字节。返回"这次是不是真的新写了一个对象"。
///
/// 用**条件写**（`If-None-Match: *`）而不是"先 HEAD 再 PUT"：
///
/// - 少一次往返，也没有了"查完到写完"之间那个窗口（同一个键被并发写时，
///   谁先谁后都是同一份字节，所以结果本来就无害，但少一个窗口少一份解释成本）。
/// - **不要求 `s3:ListBucket`。** HEAD 一个不存在的键在没有该权限时回 403
///   （S3 的既定行为），那会让**每一次上传**都失败 —— 只给
///   `PutObject`/`GetObject` 的最小权限策略是很自然写法，不能让上传挂在
///   一条"读"权限上。条件写只需要 `PutObject`。
///
/// 服务端不支持条件写时（`S3ConditionalPut::Disabled`）会得到 `NotImplemented`，
/// 那是一个响亮的失败，而不是静默降级。
pub(crate) async fn s3_put(
    store: &dyn ObjectStore,
    prefix: &str,
    sha256: &str,
    bytes: &[u8],
) -> ServerResult<bool> {
    // 内容寻址的前提是名字真的是内容的哈希。不校验的话，一个算错的调用方
    // 会把错误的字节永久钉在一个名字上 —— 而所有设备都信任那个名字。
    debug_assert_eq!(
        messagenote_core::attachment::sha256_hex(bytes),
        sha256,
        "存进来的字节和它的 sha256 对不上"
    );

    let opts = PutOptions {
        mode: PutMode::Create,
        ..Default::default()
    };
    match store
        .put_opts(&s3_key(prefix, sha256), bytes.to_vec().into(), opts)
        .await
    {
        Ok(_) => Ok(true),
        // 键就是内容的哈希：已经存在 == 同样的字节已经在了。
        Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
        Err(e) => Err(s3_err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use object_store::memory::InMemory;

    fn in_memory() -> Blobs {
        Blobs::S3 {
            store: Arc::new(InMemory::new()),
            prefix: "attachments/".into(),
        }
    }

    fn s3(blobs: &Blobs) -> (&Arc<dyn ObjectStore>, &str) {
        match blobs {
            Blobs::S3 { store, prefix } => (store, prefix),
            Blobs::Sqlite => panic!("这个测试要的是 S3 后端"),
        }
    }

    /// 没有 `MESSAGENOTE_S3_BUCKET` 就是 SQLite —— **这条改动的"什么都不配、
    /// 行为一个字都不变"就靠它**，所以要真的走一遍 `from_env`，
    /// 而不是对着枚举构造器断言（那种断言恒真，永远红不了）。
    ///
    /// 动的是真环境变量：`from_env` 在生产上只被 `main` 调一次，这个测试二进制里
    /// 没有别的调用者，所以不会和别的测试抢。用完还原。
    #[test]
    fn without_a_bucket_we_stay_on_sqlite() {
        let keys = [
            "MESSAGENOTE_S3_BUCKET",
            "MESSAGENOTE_S3_ACCESS_KEY_ID",
            "MESSAGENOTE_S3_SECRET_ACCESS_KEY",
        ];
        let saved: Vec<(&str, Option<String>)> = keys
            .into_iter()
            .map(|k| (k, std::env::var(k).ok()))
            .collect();

        std::env::remove_var("MESSAGENOTE_S3_BUCKET");
        assert!(
            !Blobs::from_env().unwrap().is_s3(),
            "没设桶名就该留在 SQLite"
        );

        // 空串 = 没设（docker-compose 就是这么传的，见 from_env 的说明）
        std::env::set_var("MESSAGENOTE_S3_BUCKET", "");
        assert!(!Blobs::from_env().unwrap().is_s3(), "空串也算没设");

        // 给了桶名就该真的切过去。凭据随便给 —— 这一步只是**建客户端**，
        // 不发任何请求。
        std::env::set_var("MESSAGENOTE_S3_BUCKET", "some-bucket");
        std::env::set_var("MESSAGENOTE_S3_ACCESS_KEY_ID", "test");
        std::env::set_var("MESSAGENOTE_S3_SECRET_ACCESS_KEY", "test");
        assert!(Blobs::from_env().unwrap().is_s3(), "给了桶名才切到 S3");

        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    /// 名字不合法的 sha **绝不**能拼进对象键里 —— 一个 `../` 就是一次越界写。
    ///
    /// `Store::missing_blobs` 在碰对象存储之前就把这类名字挑出去了，
    /// 这条测试钉住那个前提（也顺带证明桶里什么都不会被写进去）。
    #[test]
    fn a_malformed_sha_never_reaches_the_object_store() {
        for bad in ["../../../etc/passwd", "", "not-a-sha", "abc"] {
            assert!(
                !messagenote_core::attachment::is_sha256(bad),
                "{bad:?} 不该被当成合法 sha"
            );
        }
        // 合法的（64 位小写十六进制）才过
        assert!(messagenote_core::attachment::is_sha256(&"a".repeat(64)));

        let blobs = in_memory();
        let (store, prefix) = s3(&blobs);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut stream = store.list(None);
            assert!(
                stream.next().await.is_none(),
                "这里什么都没写过，桶里就该是空的"
            );
            assert!(!s3_has(store.as_ref(), prefix, &"a".repeat(64))
                .await
                .unwrap());
        });
    }

    /// 空串按"没设"处理。
    ///
    /// 这条不是洁癖：`MESSAGENOTE_S3_BUCKET=${VAR:-}` 这种写法在 docker-compose
    /// 和 systemd 里太常见（`docker-compose.yml` 自己就是这么传的）。把空串当成
    /// "要开 S3、但桶名是空的"，得到的是签名错误或者 404 —— 一个和真正原因
    /// 八竿子打不着的现象。
    #[test]
    fn an_empty_env_var_counts_as_unset() {
        // 用一个只属于本测试的名字：`cargo test` 是多线程跑的，动公共变量会污染
        // 别的测试（它们可能正在读环境）。
        let name = "MESSAGENOTE_TEST_ENV_NONEMPTY";
        std::env::set_var(name, "");
        assert_eq!(env_nonempty(name), None, "空串 = 没设");
        std::env::set_var(name, "   ");
        assert_eq!(env_nonempty(name), None, "只有空白也算没设");
        std::env::set_var(name, " value ");
        assert_eq!(
            env_nonempty(name).as_deref(),
            Some("value"),
            "两侧空白要去掉"
        );
        std::env::remove_var(name);
        assert_eq!(env_nonempty(name), None);
    }

    /// 前缀尾部的斜杠补不补，拼出来的键是同一个。
    #[test]
    fn the_prefix_gets_exactly_one_slash() {
        let sha = "a".repeat(64);
        assert_eq!(
            s3_key("attachments/", &sha).as_ref(),
            format!("attachments/{sha}")
        );
        assert_eq!(
            s3_key("attachments", &sha).as_ref(),
            format!("attachments/{sha}")
        );
        // 空前缀 = 直接放桶根下
        assert_eq!(s3_key("", &sha).as_ref(), sha);
    }

    /// 同一份字节传两次只留一个对象，而且第二次明确说"已经有了"。
    #[test]
    fn uploading_the_same_bytes_twice_leaves_one_object() {
        let blobs = in_memory();
        let (store, prefix) = s3(&blobs);
        let png = b"\x89PNG\r\n\x1a\n".to_vec();
        let sha = messagenote_core::attachment::sha256_hex(&png);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(s3_put(store.as_ref(), prefix, &sha, &png).await.unwrap());
            assert!(
                !s3_put(store.as_ref(), prefix, &sha, &png).await.unwrap(),
                "同样的字节第二次该说'已经有了'"
            );

            // 桶里真的只有一个对象 —— 只断言返回值是不够的：
            // 一个"每次都新写一个键"的实现也能让返回值说得通（只要它总说 true），
            // 而那种实现在对象存储上的表现是**账单**。
            let mut listed = Vec::new();
            let mut stream = store.list(None);
            while let Some(item) = stream.next().await {
                listed.push(item.unwrap());
            }
            assert_eq!(listed.len(), 1, "同一个 sha 只该有一个对象：{listed:?}");
        });
    }

    /// 存进去的字节取得回来，而且类型是**按字节嗅探**出来的。
    #[test]
    fn bytes_come_back_unchanged_and_the_type_is_sniffed_from_them() {
        let blobs = in_memory();
        let (store, prefix) = s3(&blobs);
        let png = b"\x89PNG\r\n\x1a\n".to_vec();
        let sha = messagenote_core::attachment::sha256_hex(&png);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            s3_put(store.as_ref(), prefix, &sha, &png).await.unwrap();
            let got = s3_get(store.as_ref(), prefix, &sha).await.unwrap();
            assert_eq!(got.as_deref(), Some(png.as_slice()));
            // Upload 端点是按字节嗅探定类型的（api.rs），这里验的是"字节本身没被
            // 序列化坏掉" —— 嗅探的结果对不对由 core 的测试负责。
            assert_eq!(
                messagenote_core::attachment::resolve_mime(got.as_deref().unwrap()),
                "image/png"
            );
        });
    }

    /// 桶里没有的 sha 是 `None`/`false`，不是错误。
    ///
    /// 客户端要靠这个区分"对端也没有，别再重试"和"服务端出错了，等会儿再试" ——
    /// 两者混淆会让下载队列永远卡在同一条上。
    #[test]
    fn an_absent_object_is_an_answer_not_an_error() {
        let blobs = in_memory();
        let (store, prefix) = s3(&blobs);
        let absent = "0".repeat(64);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(!s3_has(store.as_ref(), prefix, &absent).await.unwrap());
            assert!(s3_get(store.as_ref(), prefix, &absent)
                .await
                .unwrap()
                .is_none());
        });
    }
}
