//! 文件接口：全部 JSON、不传字节。
//! - download：运行器自己下载到会话目录（跟随至多 3 次跳转，超过 max_bytes 中止并删掉半成品）；
//! - stat：一遍流式算出大小、MIME、MD5、内容 ETag、两种 CRC64 与分片 MD5；
//! - prove：对给定抽段算 HMAC-SHA256(nonce, 段字节)，是 File 秒传的持有证明；
//! - upload：按助手交来的计划逐段 PUT（流式读文件范围，带 Content-Length，不整读进内存）；
//! - 删会话目录：以命名空间账号自己的身份 `rm -rf`，账号无权删的东西它也删不掉。
//!
//! 运行器以自己的身份读文件与联网：文件打开走 paths 的不跟随符号链接逐级打开，模型的命令摆不出能让运行器
//! 读到会话目录之外的路径；联网不受命令账号的私网限制（iptables 按 uid 区分），因为它只连助手交来的地址。

use crate::account::Account;
use crate::checksum::{Fingerprint, Proof, PART_SIZE};
use crate::config::Config;
use crate::mime;
use crate::paths::{self, Workspace};
use crate::server::{ApiError, ApiResult};
use bytes::Bytes;
use futures_util::StreamExt;
use http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

/// 连接超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 两次收到数据之间的最长间隔；整体不设上限，大文件要多久下多久。
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// 下载跟随跳转的次数上限（File 的最终地址经 CDN 可能 301 再 302）。
const MAX_REDIRECTS: usize = 3;
/// 上传时每次从文件读多少字节交给请求体。
const UPLOAD_CHUNK: u64 = 256 * 1024;
/// 一次 prove 最多几段、总共最多多少字节：File 的挑战是 1 段整文件（≤ 192 KiB）或 3 段各 64 KiB，这里留足余量。
const PROVE_MAX_RANGES: usize = 64;
const PROVE_MAX_TOTAL: u64 = 16 * 1024 * 1024;
/// 一个上传计划最多几段（S3 分片上限 10000）。
const UPLOAD_MAX_PARTS: usize = 10_000;
/// 错误应答里带回的上游正文长度。
const SNIPPET_BYTES: usize = 300;

/// `POST /files/download`。
#[derive(Deserialize)]
pub struct DownloadRequest {
    pub conversation_id: String,
    /// 下载地址，只接受 http/https。
    pub url: String,
    /// 保存到会话内的相对路径（文件名），中间目录自动建立。
    pub path: String,
    /// 大小上限（字节），超过即中止并删掉半成品。
    pub max_bytes: u64,
}

/// `POST /files/stat` 与其他只带路径的请求。
#[derive(Deserialize)]
pub struct PathRequest {
    pub conversation_id: String,
    pub path: String,
}

/// 文件内的一段。
#[derive(Deserialize, Clone, Copy)]
pub struct Range {
    pub offset: u64,
    pub length: u64,
}

/// `POST /files/prove`。
#[derive(Deserialize)]
pub struct ProveRequest {
    pub conversation_id: String,
    pub path: String,
    /// HMAC 密钥，标准 Base64（File 挑战里的 nonce）。
    pub nonce: String,
    /// 要证明的抽段，顺序即应答里 proof_list 的顺序（EasOS 契约命名：同类集合以 _list 结尾）。
    pub range_list: Vec<Range>,
}

/// 上传计划里的一段：把文件 [offset, offset+length) 以 `method` 发到 `url`，带 `headers`。
#[derive(Deserialize)]
pub struct UploadPart {
    pub offset: u64,
    pub length: u64,
    /// PUT（默认）或 POST。
    #[serde(default)]
    pub method: Option<String>,
    pub url: String,
    /// 原样带上的请求头（例如 Content-Type、Content-MD5、x-amz-checksum-crc64nvme）；Content-Length 由运行器自己算。
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// `POST /files/upload`。
#[derive(Deserialize)]
pub struct UploadRequest {
    pub conversation_id: String,
    pub path: String,
    /// 上传计划的各段。
    pub part_list: Vec<UploadPart>,
}

/// 文件接口的实现：两个 HTTP 客户端分别用于下载（跟随跳转）与上传（不跟随：流式请求体不能重放）。
pub struct Files {
    cfg: Arc<Config>,
    downloader: reqwest::Client,
    uploader: reqwest::Client,
}

impl Files {
    pub fn new(cfg: Arc<Config>) -> anyhow::Result<Self> {
        let base = || {
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .user_agent(concat!("easos-sandbox/", env!("CARGO_PKG_VERSION")))
        };
        Ok(Self {
            cfg,
            downloader: base()
                .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                .build()?,
            uploader: base().redirect(reqwest::redirect::Policy::none()).build()?,
        })
    }

    async fn workspace(&self, account: &Account, conversation: &str, create: bool) -> ApiResult<Workspace> {
        paths::open_workspace(&self.cfg.home_root, &account.name, account.owner, conversation, create)
            .await
            .map_err(|e| io_error("打开会话目录", e))?
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "conversation_not_found", "会话目录不存在"))
    }

    /// 下载到会话目录。中间状态只在临时文件里，成功才改名到目标路径。
    pub async fn download(&self, account: &Account, req: DownloadRequest) -> ApiResult<Value> {
        let rel = rel(&req.path)?;
        if req.max_bytes == 0 {
            return Err(ApiError::bad_request("max_bytes 至少为 1"));
        }
        let url = http_url(&req.url)?;
        let workspace = self.workspace(account, &req.conversation_id, true).await?;
        let response = self
            .downloader
            .get(url)
            .send()
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, "upstream_failed", format!("连接下载地址失败：{e}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = snippet(response).await;
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "upstream_status",
                format!("下载地址返回 HTTP {}", status.as_u16()),
            )
            .with("upstream_status", json!(status.as_u16()))
            .with("upstream_body", json!(body)));
        }
        if let Some(declared) = response.content_length() {
            if declared > req.max_bytes {
                return Err(too_large(declared, req.max_bytes));
            }
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let final_url = response.url().to_string();
        let mut pending = workspace
            .begin_write(&rel)
            .map_err(|e| io_error("建立目标文件", e))?;
        let mut file = tokio::fs::File::from_std(pending.take_file().expect("刚建立的文件句柄还在"));
        let mut stream = response.bytes_stream();
        let mut size = 0u64;
        let mut head = Vec::with_capacity(mime::HEAD_BYTES);
        while let Some(chunk) = stream.next().await {
            // 任何错误直接返回：pending 析构时删掉临时文件，目标路径不会出现半成品。
            let chunk = chunk.map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, "upstream_failed", format!("下载中断：{e}")))?;
            size += chunk.len() as u64;
            if size > req.max_bytes {
                return Err(too_large(size, req.max_bytes));
            }
            if head.len() < mime::HEAD_BYTES {
                let take = (mime::HEAD_BYTES - head.len()).min(chunk.len());
                head.extend_from_slice(&chunk[..take]);
            }
            file.write_all(&chunk)
                .await
                .map_err(|e| ApiError::internal(format!("写文件失败：{e}")))?;
        }
        file.flush()
            .await
            .map_err(|e| ApiError::internal(format!("写文件失败：{e}")))?;
        drop(file);
        pending.commit().map_err(|e| io_error("放置文件", e))?;
        let name = rel.last().expect("规范化后至少一个分量");
        Ok(json!({
            "path": rel.join("/"),
            "size": size,
            "mime": mime::guess(&head, name),
            "content_type": content_type,
            "final_url": final_url,
        }))
    }

    /// 文件指纹：一遍读完算出全部值。
    pub async fn stat(&self, account: &Account, req: PathRequest) -> ApiResult<Value> {
        let rel = rel(&req.path)?;
        let workspace = self.workspace(account, &req.conversation_id, false).await?;
        let (digests, head, modified, rel) = blocking(move || -> io::Result<_> {
            let mut file = workspace.open_read(&rel)?;
            let modified = file
                .metadata()?
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64);
            let mut fingerprint = Fingerprint::new();
            let mut head = Vec::with_capacity(mime::HEAD_BYTES);
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                if head.len() < mime::HEAD_BYTES {
                    let take = (mime::HEAD_BYTES - head.len()).min(n);
                    head.extend_from_slice(&buf[..take]);
                }
                fingerprint.update(&buf[..n]);
            }
            Ok((fingerprint.finish(), head, modified, rel))
        })
        .await?
        .map_err(|e| io_error("读文件", e))?;
        let name = rel.last().expect("规范化后至少一个分量");
        Ok(json!({
            "path": rel.join("/"),
            "size": digests.size,
            "mime": mime::guess(&head, name),
            "md5": digests.md5,
            "etag": digests.etag,
            "crc64_ecma": digests.crc64_ecma,
            "crc64_nvme": digests.crc64_nvme,
            "part_size": PART_SIZE,
            "part_list": digests.parts,
            "modified": modified,
        }))
    }

    /// 持有证明：每段 HMAC-SHA256(key = Base64 解码的 nonce, 段字节) 的标准 Base64，顺序与 range_list 相同。
    pub async fn prove(&self, account: &Account, req: ProveRequest) -> ApiResult<Value> {
        let rel = rel(&req.path)?;
        let key = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(&req.nonce)
                .ok()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| ApiError::bad_request("nonce 必须是非空的标准 Base64"))?
        };
        if req.range_list.is_empty() || req.range_list.len() > PROVE_MAX_RANGES {
            return Err(ApiError::bad_request(format!("range_list 需要 1～{PROVE_MAX_RANGES} 段")));
        }
        let mut total = 0u64;
        for range in &req.range_list {
            if range.length == 0 {
                return Err(ApiError::bad_request("每段 length 至少为 1"));
            }
            total = total
                .checked_add(range.length)
                .filter(|t| *t <= PROVE_MAX_TOTAL)
                .ok_or_else(|| ApiError::bad_request(format!("抽段总长超过 {PROVE_MAX_TOTAL} 字节")))?;
        }
        let workspace = self.workspace(account, &req.conversation_id, false).await?;
        let ranges = req.range_list;
        let (size, proofs, rel) = blocking(move || -> ApiResult<_> {
            let file = workspace.open_read(&rel).map_err(|e| io_error("读文件", e))?;
            let size = file.metadata().map_err(|e| io_error("读文件", e))?.len();
            let mut proofs = Vec::with_capacity(ranges.len());
            for (index, range) in ranges.iter().enumerate() {
                let end = range
                    .offset
                    .checked_add(range.length)
                    .filter(|end| *end <= size)
                    .ok_or_else(|| {
                        ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "range_invalid",
                            format!("第 {} 段超出文件（文件 {size} 字节）", index + 1),
                        )
                        .with("size", json!(size))
                    })?;
                let mut proof = Proof::new(&key);
                let mut buf = vec![0u8; (range.length.min(1 << 20)) as usize];
                let mut position = range.offset;
                while position < end {
                    let want = ((end - position) as usize).min(buf.len());
                    let n = file
                        .read_at(&mut buf[..want], position)
                        .map_err(|e| io_error("读文件", e))?;
                    if n == 0 {
                        return Err(ApiError::internal("文件比刚读到的长度短，可能正在被改写"));
                    }
                    proof.update(&buf[..n]);
                    position += n as u64;
                }
                proofs.push(proof.finish());
            }
            Ok((size, proofs, rel))
        })
        .await??;
        Ok(json!({"path": rel.join("/"), "size": size, "proof_list": proofs}))
    }

    /// 按计划逐段上传。任一段失败就停在那一段，应答里带已完成段的结果。
    pub async fn upload(&self, account: &Account, req: UploadRequest) -> ApiResult<Value> {
        let rel = rel(&req.path)?;
        if req.part_list.is_empty() || req.part_list.len() > UPLOAD_MAX_PARTS {
            return Err(ApiError::bad_request(format!("part_list 需要 1～{UPLOAD_MAX_PARTS} 段")));
        }
        let mut plan = Vec::with_capacity(req.part_list.len());
        for (index, part) in req.part_list.iter().enumerate() {
            let method = match part.method.as_deref().map(str::to_ascii_uppercase).as_deref() {
                None | Some("PUT") => Method::PUT,
                Some("POST") => Method::POST,
                Some(other) => {
                    return Err(ApiError::bad_request(format!("第 {} 段的 method 只能是 PUT 或 POST，不是 {other}", index + 1)))
                }
            };
            if part.length == 0 {
                return Err(ApiError::bad_request(format!("第 {} 段 length 至少为 1", index + 1)));
            }
            plan.push((method, http_url(&part.url)?, header_map(index, &part.headers)?));
        }
        let workspace = self.workspace(account, &req.conversation_id, false).await?;
        let file = blocking(move || workspace.open_read(&rel).map(|f| (Arc::new(f), rel)))
            .await?
            .map_err(|e| io_error("读文件", e))?;
        let (file, rel) = file;
        let size = file.metadata().map_err(|e| io_error("读文件", e))?.len();
        for (index, part) in req.part_list.iter().enumerate() {
            if part.offset.checked_add(part.length).is_none_or(|end| end > size) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "range_invalid",
                    format!("第 {} 段超出文件（文件 {size} 字节）", index + 1),
                )
                .with("size", json!(size)));
            }
        }
        let mut results: Vec<Value> = Vec::with_capacity(req.part_list.len());
        for (index, (part, (method, url, headers))) in req.part_list.iter().zip(plan).enumerate() {
            let body = reqwest::Body::wrap_stream(range_stream(file.clone(), part.offset, part.length));
            let sent = self
                .uploader
                .request(method, url)
                .headers(headers)
                .header(header::CONTENT_LENGTH, part.length)
                .body(body)
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(e) => {
                    return Err(ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        "upload_failed",
                        format!("第 {} 段上传失败：{e}", index + 1),
                    )
                    .with("failed_part", json!(index))
                    .with("part_list", json!(results)))
                }
            };
            let status = response.status();
            let etag = response
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            if !status.is_success() {
                let body = snippet(response).await;
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "upload_failed",
                    format!("第 {} 段上传返回 HTTP {}", index + 1, status.as_u16()),
                )
                .with("failed_part", json!(index))
                .with("upstream_status", json!(status.as_u16()))
                .with("upstream_body", json!(body))
                .with("part_list", json!(results)));
            }
            results.push(json!({
                "offset": part.offset,
                "length": part.length,
                "status": status.as_u16(),
                "etag": etag,
            }));
        }
        Ok(json!({"path": rel.join("/"), "size": size, "part_list": results}))
    }

    /// 删整个会话目录。多用户模式以命名空间账号的身份执行 rm，它删不掉不属于自己的东西。
    pub async fn delete_conversation(&self, account: &Account, conversation: &str) -> ApiResult<bool> {
        let path = account.home.join("conversations").join(conversation);
        if tokio::fs::symlink_metadata(&path).await.is_err() {
            return Ok(false);
        }
        match account.owner {
            None => tokio::fs::remove_dir_all(&path)
                .await
                .map_err(|e| ApiError::internal(format!("删除会话目录失败：{e}")))?,
            Some(owner) => {
                let output = tokio::process::Command::new("rm")
                    .arg("-rf")
                    .arg(&path)
                    .uid(owner.uid)
                    .gid(owner.gid)
                    .env_clear()
                    .env("PATH", &self.cfg.command_path)
                    .output()
                    .await
                    .map_err(|e| ApiError::internal(format!("运行 rm 失败：{e}")))?;
                if !output.status.success() {
                    return Err(ApiError::internal(format!(
                        "删除会话目录失败（{}）：{}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
            }
        }
        Ok(true)
    }
}

/// 从文件的一个范围流式产出请求体：每次在阻塞线程里 pread 一块，不整段读进内存。
fn range_stream(
    file: Arc<std::fs::File>,
    offset: u64,
    length: u64,
) -> impl futures_util::Stream<Item = io::Result<Bytes>> + Send + 'static {
    futures_util::stream::try_unfold((file, offset, length), |(file, position, remaining)| async move {
        if remaining == 0 {
            return Ok(None);
        }
        let want = remaining.min(UPLOAD_CHUNK) as usize;
        let reader = file.clone();
        let chunk = tokio::task::spawn_blocking(move || -> io::Result<Vec<u8>> {
            let mut buf = vec![0u8; want];
            let mut filled = 0;
            while filled < want {
                let n = reader.read_at(&mut buf[filled..], position + filled as u64)?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "文件比上传计划短，可能正在被改写"));
                }
                filled += n;
            }
            Ok(buf)
        })
        .await
        .map_err(io::Error::other)??;
        Ok(Some((Bytes::from(chunk), (file, position + want as u64, remaining - want as u64))))
    })
}

/// 相对路径 → 分量；不合法为 400 path_invalid。
fn rel(path: &str) -> ApiResult<Vec<String>> {
    paths::normalize(path).map_err(|message| ApiError::new(StatusCode::BAD_REQUEST, "path_invalid", message))
}

/// 只接受 http/https 的地址。
fn http_url(text: &str) -> ApiResult<reqwest::Url> {
    let url = reqwest::Url::parse(text).map_err(|e| ApiError::bad_request(format!("url 不合法：{e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("url 只支持 http 与 https"));
    }
    Ok(url)
}

/// 上传计划里的请求头 → HeaderMap。Content-Length 与 Transfer-Encoding 由运行器决定，计划里给了也忽略。
fn header_map(index: usize, headers: &BTreeMap<String, String>) -> ApiResult<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| ApiError::bad_request(format!("第 {} 段的请求头名不合法：{name}", index + 1)))?;
        if name == header::CONTENT_LENGTH || name == header::TRANSFER_ENCODING {
            continue;
        }
        let value = HeaderValue::from_str(value)
            .map_err(|_| ApiError::bad_request(format!("第 {} 段的请求头 {name} 的值不合法", index + 1)))?;
        map.append(name, value);
    }
    Ok(map)
}

/// 文件系统错误 → 接口错误：不存在是 404，经过符号链接、不是普通文件或路径不合法是 400，其余 500。
fn io_error(what: &str, e: io::Error) -> ApiError {
    let raw = e.raw_os_error();
    let invalid = e.kind() == io::ErrorKind::InvalidInput
        || raw == Some(nix::libc::ELOOP)
        || raw == Some(nix::libc::ENOTDIR)
        || raw == Some(nix::libc::EISDIR);
    if invalid {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            "path_invalid",
            format!("{what}：路径经过符号链接、不是普通文件或不合法（{e}）"),
        );
    }
    match e.kind() {
        io::ErrorKind::NotFound => ApiError::new(StatusCode::NOT_FOUND, "not_found", format!("{what}：不存在")),
        _ => ApiError::internal(format!("{what}失败：{e}")),
    }
}

fn too_large(size: u64, max: u64) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!("文件超过 max_bytes（{max} 字节）"),
    )
    .with("size", json!(size))
    .with("max_bytes", json!(max))
}

/// 上游错误正文的开头一段，给错误应答用。
async fn snippet(mut response: reqwest::Response) -> String {
    let mut buf = Vec::new();
    while buf.len() < SNIPPET_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            _ => break,
        }
    }
    buf.truncate(SNIPPET_BYTES);
    String::from_utf8_lossy(&buf).into_owned()
}

/// 阻塞的文件工作放到阻塞线程池。
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ApiError::internal(format!("后台任务失败：{e}")))
}

use tokio::io::AsyncWriteExt;
