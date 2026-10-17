//! HTTP 层：hyper 1 的 HTTP/1.1 服务、路由、请求体读取、统一的 JSON 应答与错误形状 `{error, message}`。
//! 除 `GET /healthz` 外每个请求都要 Bearer 密钥与 `X-User-Id`；路由只做分发与参数校验，业务在 exec 与 files。
//! 接口的 JSON 形状与 Open Terminal 对齐（`/execute`、`/execute/{id}/status?wait=&offset=`、`DELETE /execute/{id}`），
//! 让助手客户端改动最小；文件接口是新的，全部 JSON、不传字节。

use crate::account::Accounts;
use crate::auth;
use crate::config::Config;
use crate::exec::{self, Executions};
use crate::files::{self, Files};
use crate::paths;
use bytes::Bytes;
use http::{header, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 请求体上限（字节）：上传计划带几千段也远小于它；更大的多半是误把文件内容发过来了。
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// 整个进程共享的状态。
pub struct State {
    pub cfg: Arc<Config>,
    pub accounts: Accounts,
    pub executions: Executions,
    pub files: Files,
}

impl State {
    pub fn new(cfg: Arc<Config>) -> anyhow::Result<Self> {
        // 执行记录目录归运行器自己：0700，命令账号读不到也改不了。
        std::fs::create_dir_all(&cfg.state_dir)?;
        std::fs::set_permissions(&cfg.state_dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        Ok(Self {
            accounts: Accounts::new(cfg.clone()),
            executions: Executions::new(cfg.clone()),
            files: Files::new(cfg.clone())?,
            cfg,
        })
    }
}

/// 接口错误：HTTP 状态码、稳定的错误代码、给人看的说明，以及可选的附加字段（例如上传失败到了哪一段）。
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub extra: serde_json::Map<String, Value>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            extra: serde_json::Map::new(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }
    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.extra.insert(key.to_owned(), value);
        self
    }
    fn to_json(&self) -> Value {
        let mut map = self.extra.clone();
        map.insert("error".into(), json!(self.code));
        map.insert("message".into(), json!(self.message));
        Value::Object(map)
    }
}

/// 当前 Unix 时间（毫秒）。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Unix 毫秒 → `YYYY-MM-DDTHH:MM:SSZ`（日志用，不引入日期库）。
pub fn iso8601(ms: u64) -> String {
    let secs = ms / 1000;
    let (year, month, day) = civil_from_days((secs / 86400) as i64);
    let rem = secs % 86400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// 1970-01-01 起的天数 → 公历年月日（Howard Hinnant 的算法）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 一个 HTTP 请求的完整处理：路由、执行、JSON 应答、一行日志。
pub async fn handle(state: Arc<State>, req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let namespace = req
        .headers()
        .get("x-user-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_owned();
    let (status, body) = match route(&state, req).await {
        Ok((status, body)) => (status, body),
        Err(error) => (error.status, error.to_json()),
    };
    if path != "/healthz" {
        let detail = if status.is_success() {
            String::new()
        } else {
            format!(
                " [{}] {}",
                body["error"].as_str().unwrap_or(""),
                body["message"].as_str().unwrap_or("")
            )
        };
        println!(
            "{} {method} {path} ns={namespace} -> {} {}ms{detail}",
            iso8601(now_ms()),
            status.as_u16(),
            started.elapsed().as_millis()
        );
    }
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    Ok(Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Full::new(Bytes::from(bytes)))
        .expect("固定的应答头合法"))
}

/// `POST /execute` 的请求体。
#[derive(serde::Deserialize)]
struct ExecuteRequest {
    conversation_id: String,
    command: String,
    /// 单条命令的最长秒数，默认也是上限 600。
    #[serde(default)]
    timeout: Option<u64>,
}

async fn route(state: &State, req: Request<Incoming>) -> ApiResult<(StatusCode, Value)> {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path();
    let query = parts.uri.query();
    if parts.method == http::Method::GET && path == "/healthz" {
        return Ok((StatusCode::OK, json!({"ok": true})));
    }
    if !auth::authorized(parts.headers.get(header::AUTHORIZATION), &state.cfg.api_key) {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "缺少或错误的 Bearer 密钥"));
    }
    let namespace = parts
        .headers
        .get("x-user-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::bad_request("缺少 X-User-Id 请求头"))?;
    let account = state.accounts.resolve(namespace).await?;
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (parts.method.as_str(), segments.as_slice()) {
        ("POST", ["execute"]) => {
            let req: ExecuteRequest = read_json(body).await?;
            conversation(&req.conversation_id)?;
            if req.command.trim().is_empty() {
                return Err(ApiError::bad_request("command 不能为空"));
            }
            if req.command.len() > exec::MAX_COMMAND_BYTES {
                return Err(ApiError::bad_request(format!("command 超过 {} 字节", exec::MAX_COMMAND_BYTES)));
            }
            let timeout = match req.timeout {
                None => exec::MAX_TIMEOUT,
                Some(0) => return Err(ApiError::bad_request("timeout 至少 1 秒")),
                Some(secs) => Duration::from_secs(secs).min(exec::MAX_TIMEOUT),
            };
            let wait = query_secs(query, "wait", exec::MAX_WAIT)?;
            let execution = state
                .executions
                .start(account, &req.conversation_id, &req.command, timeout)
                .await?;
            Ok((StatusCode::OK, execution.wait_view(0, wait).await))
        }
        ("GET", ["execute", id, "status"]) => {
            let offset = query_u64(query, "offset")?.unwrap_or(0);
            let wait = query_secs(query, "wait", exec::MAX_WAIT)?;
            Ok((StatusCode::OK, state.executions.status(&account, id, offset, wait).await?))
        }
        ("DELETE", ["execute", id]) => Ok((StatusCode::OK, state.executions.stop(&account, id).await?)),
        ("POST", ["files", "download"]) => {
            let req: files::DownloadRequest = read_json(body).await?;
            conversation(&req.conversation_id)?;
            Ok((StatusCode::OK, state.files.download(&account, req).await?))
        }
        ("POST", ["files", "stat"]) => {
            let req: files::PathRequest = read_json(body).await?;
            conversation(&req.conversation_id)?;
            Ok((StatusCode::OK, state.files.stat(&account, req).await?))
        }
        ("POST", ["files", "prove"]) => {
            let req: files::ProveRequest = read_json(body).await?;
            conversation(&req.conversation_id)?;
            Ok((StatusCode::OK, state.files.prove(&account, req).await?))
        }
        ("POST", ["files", "upload"]) => {
            let req: files::UploadRequest = read_json(body).await?;
            conversation(&req.conversation_id)?;
            Ok((StatusCode::OK, state.files.upload(&account, req).await?))
        }
        ("DELETE", ["conversations", id]) => {
            conversation(id)?;
            // 先停掉这个会话里还在跑的命令并清掉它们的执行记录，再删目录：否则命令可能边删边写。
            state.executions.end_conversation(&account, id).await;
            let deleted = state.files.delete_conversation(&account, id).await?;
            Ok((StatusCode::OK, json!({"conversation_id": id, "deleted": deleted})))
        }
        _ => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "route_not_found",
            format!("没有 {} {path} 这个接口", parts.method),
        )),
    }
}

/// 读 JSON 请求体，超过上限或不是预期形状都算 400。
async fn read_json<T: DeserializeOwned>(body: Incoming) -> ApiResult<T> {
    let bytes = Limited::new(body, MAX_BODY_BYTES)
        .collect()
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "body_invalid",
                format!("读请求体失败（上限 {MAX_BODY_BYTES} 字节）：{e}"),
            )
        })?
        .to_bytes();
    serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "body_invalid", format!("请求体不是预期的 JSON：{e}")))
}

fn conversation(id: &str) -> ApiResult<()> {
    if paths::valid_conversation_id(id) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "conversation_invalid",
            "conversation_id 只能含字母、数字、连字符与下划线，不超过 64 字节",
        ))
    }
}

fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == key).then_some(v)
    })
}

fn query_u64(query: Option<&str>, key: &str) -> ApiResult<Option<u64>> {
    match query_param(query, key) {
        None | Some("") => Ok(None),
        Some(text) => text
            .parse()
            .map(Some)
            .map_err(|_| ApiError::bad_request(format!("查询参数 {key} 必须是非负整数"))),
    }
}

/// 秒数查询参数（缺省 0），超过 `max` 按 `max`。
fn query_secs(query: Option<&str>, key: &str, max: Duration) -> ApiResult<Duration> {
    Ok(query_u64(query, key)?
        .map(Duration::from_secs)
        .unwrap_or(Duration::ZERO)
        .min(max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 时间戳与查询参数解析() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_760_745_600_000), "2025-10-18T00:00:00Z");
        assert_eq!(iso8601(951_782_400_000), "2000-02-29T00:00:00Z", "闰日");
        assert_eq!(query_param(Some("wait=10&offset=5"), "offset"), Some("5"));
        assert_eq!(query_param(Some("wait"), "wait"), Some(""));
        assert_eq!(query_u64(Some("offset=42"), "offset").unwrap(), Some(42));
        assert!(query_u64(Some("offset=-1"), "offset").is_err());
        assert_eq!(
            query_secs(Some("wait=99"), "wait", Duration::from_secs(30)).unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(query_secs(None, "wait", Duration::from_secs(30)).unwrap(), Duration::ZERO);
    }
}
