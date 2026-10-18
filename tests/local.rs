//! 不依赖 Docker 的集成测试：以当前用户在本机起守护进程（单用户模式），用真实 HTTP 走一遍
//! execute / status / 停止 / 超时 / 重启后查记录、download（本地起的上游服务，含跳转、超限、404）、
//! stat / prove / upload（本地起的接收端核对 Content-Length 与正文）、删会话、鉴权与路径校验。

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 测试用的密钥。
const KEY: &str = "test-key-7f3a";
/// 测试用的命名空间（账号名）。
const NS: &str = "user-test";

/// 一个跑着的运行器进程。
struct Runner {
    child: std::process::Child,
    base: String,
}

impl Runner {
    /// 以单用户模式起一个运行器，家目录根指向 `home`；等它打印监听地址再返回。
    fn start(home: &Path) -> Runner {
        std::fs::create_dir_all(home).unwrap();
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sandbox-runner"))
            .env("SANDBOX_SINGLE_USER", "1")
            .env("SANDBOX_LISTEN", "127.0.0.1:0")
            .env("SANDBOX_API_KEY", KEY)
            .env("SANDBOX_HOME_ROOT", home)
            .env("SANDBOX_COMMAND_ENV", "NODE_PATH=/opt/node-test")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("启动运行器");
        let stdout = child.stdout.take().unwrap();
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let addr = line
            .split("监听 ")
            .nth(1)
            .and_then(|rest| rest.split('，').next())
            .unwrap_or_else(|| panic!("第一行不是监听地址：{line:?}"))
            .trim()
            .to_owned();
        // 把剩下的日志排干，免得管道塞满把运行器卡住；顺带打印到测试输出便于排查。
        std::thread::spawn(move || {
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("[runner] {line}");
            }
        });
        Runner {
            child,
            base: format!("http://{addr}"),
        }
    }

    fn client(&self) -> Client {
        Client {
            base: self.base.clone(),
            http: reqwest::Client::new(),
        }
    }

    /// 用 SIGKILL 结束运行器：模拟它来不及收尾就没了。
    fn kill_hard(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 面向运行器的最小客户端。
struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    async fn call(&self, method: reqwest::Method, path: &str, ns: Option<&str>, key: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(key) = key {
            req = req.bearer_auth(key);
        }
        if let Some(ns) = ns {
            req = req.header("X-User-Id", ns);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let response = req.send().await.expect("请求运行器");
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("应答不是 JSON：{text}"));
        (status, value)
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, Some(NS), Some(KEY), Some(body)).await
    }
    async fn get(&self, path: &str) -> (u16, Value) {
        self.call(reqwest::Method::GET, path, Some(NS), Some(KEY), None).await
    }
    async fn delete(&self, path: &str) -> (u16, Value) {
        self.call(reqwest::Method::DELETE, path, Some(NS), Some(KEY), None).await
    }
    /// 像助手客户端那样执行一条命令：开始后按 offset 轮询到结束，把各次拿到的输出拼起来；返回最后一次的状态 JSON，
    /// 其中 output 换成拼好的全部输出、truncated 是各次的或。
    async fn run(&self, conversation: &str, command: &str) -> Value {
        let (status, mut view) = self
            .post("/execute?wait=20", json!({"conversation_id": conversation, "command": command}))
            .await;
        assert_eq!(status, 200, "{view}");
        let id = view["id"].as_str().unwrap().to_owned();
        let mut output = output_of(&view);
        let mut truncated = view["truncated"].as_bool().unwrap_or(false);
        let mut polls = 0;
        while view["status"] == "running" {
            polls += 1;
            assert!(polls < 60, "命令迟迟不结束：{command}");
            let offset = view["next_offset"].as_u64().unwrap();
            let (status, next) = self.get(&format!("/execute/{id}/status?wait=20&offset={offset}")).await;
            assert_eq!(status, 200, "{next}");
            output.push_str(&output_of(&next));
            truncated |= next["truncated"].as_bool().unwrap_or(false);
            view = next;
        }
        view["output"] = json!([{"stream": "combined", "data": output}]);
        view["truncated"] = json!(truncated);
        view
    }
}

fn output_of(view: &Value) -> String {
    view["output"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| item["data"].as_str().unwrap_or(""))
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sandbox-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 测试用上游服务记录到的一次上传。
#[derive(Debug, Clone)]
struct Upload {
    path: String,
    content_length: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

type Uploads = Arc<Mutex<Vec<Upload>>>;

/// 本地上游：提供下载用的文件与跳转链、超限文件、404，接收上传并记录。返回 `http://127.0.0.1:端口`。
async fn upstream(uploads: Uploads) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            let uploads = uploads.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| upstream_handle(req, uploads.clone()));
                let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
            });
        }
    });
    format!("http://{addr}")
}

fn full(status: u16, body: &'static [u8], content_type: &str) -> hyper::Response<BoxBody<Bytes, Infallible>> {
    hyper::Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Full::new(Bytes::from_static(body)).boxed())
        .unwrap()
}

fn redirect(to: &str) -> hyper::Response<BoxBody<Bytes, Infallible>> {
    hyper::Response::builder()
        .status(302)
        .header("location", to)
        .body(Full::new(Bytes::new()).boxed())
        .unwrap()
}

async fn upstream_handle(req: hyper::Request<Incoming>, uploads: Uploads) -> Result<hyper::Response<BoxBody<Bytes, Infallible>>, Infallible> {
    let path = req.uri().path().to_owned();
    let response = match (req.method().as_str(), path.as_str()) {
        ("GET", "/file.csv") => full(200, b"a,b\n1,2\n", "text/csv"),
        // 三跳到文件（允许）；四跳（r4 起）超过上限。
        ("GET", "/r1") => redirect("/file.csv"),
        ("GET", "/r2") => redirect("/r1"),
        ("GET", "/r3") => redirect("/r2"),
        ("GET", "/r4") => redirect("/r3"),
        ("GET", "/missing") => full(404, b"no such file", "text/plain"),
        // 声明了长度的大文件：运行器应在读正文前就拒绝。
        ("GET", "/big-declared") => {
            let body = vec![b'x'; 100_000];
            hyper::Response::builder()
                .status(200)
                .header("content-length", body.len())
                .body(Full::new(Bytes::from(body)).boxed())
                .unwrap()
        }
        // 分块传输的大文件：运行器只能边收边数，超限时中止并删掉半成品。
        ("GET", "/big-chunked") => {
            let chunks = (0..50).map(|_| Ok::<_, Infallible>(Frame::data(Bytes::from(vec![b'y'; 2000]))));
            hyper::Response::builder()
                .status(200)
                .body(StreamBody::new(futures_util::stream::iter(chunks)).boxed())
                .unwrap()
        }
        ("PUT", p) if p.starts_with("/upload/") => {
            let content_length = req.headers().get("content-length").map(|v| v.to_str().unwrap().to_owned());
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
                .collect();
            let body = req.into_body().collect().await.unwrap().to_bytes().to_vec();
            let index = uploads.lock().unwrap().len();
            uploads.lock().unwrap().push(Upload {
                path: p.to_owned(),
                content_length,
                headers,
                body,
            });
            hyper::Response::builder()
                .status(200)
                .header("etag", format!("\"etag-{index}\""))
                .body(Full::new(Bytes::new()).boxed())
                .unwrap()
        }
        ("PUT", "/upload-forbidden") => full(403, b"AccessDenied", "application/xml"),
        _ => full(404, b"unknown route", "text/plain"),
    };
    Ok(response)
}

/// 独立的位运算 CRC64（反射、初值与输出异或全 1），用来核对运行器查表实现的结果。
fn crc64(poly: u64, data: &[u8]) -> u64 {
    let mut crc = !0u64;
    for &byte in data {
        crc ^= u64::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ poly } else { crc >> 1 };
        }
    }
    !crc
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 鉴权_命名空间与路由校验() {
    let home = scratch("auth");
    let runner = Runner::start(&home);
    let client = runner.client();
    let (status, body) = client.call(reqwest::Method::GET, "/healthz", None, None, None).await;
    assert_eq!((status, body["ok"].as_bool()), (200, Some(true)));
    let (status, body) = client.call(reqwest::Method::GET, "/execute/x/status", Some(NS), None, None).await;
    assert_eq!((status, body["error"].as_str()), (401, Some("unauthorized")));
    let (status, _) = client.call(reqwest::Method::GET, "/execute/x/status", Some(NS), Some("wrong"), None).await;
    assert_eq!(status, 401);
    let (status, body) = client.call(reqwest::Method::GET, "/execute/x/status", None, Some(KEY), None).await;
    assert_eq!((status, body["error"].as_str()), (400, Some("bad_request")));
    let (status, body) = client.call(reqwest::Method::GET, "/execute/x/status", Some("User_1"), Some(KEY), None).await;
    assert_eq!((status, body["error"].as_str()), (400, Some("namespace_invalid")));
    let (status, body) = client.get("/nope").await;
    assert_eq!((status, body["error"].as_str()), (404, Some("route_not_found")));
    let (status, body) = client.post("/execute", json!({"conversation_id": "a/b", "command": "true"})).await;
    assert_eq!((status, body["error"].as_str()), (400, Some("conversation_invalid")));
    let (status, body) = client.post("/execute", json!({"conversation_id": "c1", "command": "  "})).await;
    assert_eq!((status, body["error"].as_str()), (400, Some("bad_request")));
    let (status, body) = client.post("/execute", json!({"conversation_id": "c1"})).await;
    assert_eq!((status, body["error"].as_str()), (400, Some("body_invalid")));
    let (status, body) = client.get("/execute/0123456789abcdef/status").await;
    assert_eq!((status, body["error"].as_str()), (404, Some("execution_not_found")));
    // 单用户模式下首个请求就建出了账号目录。
    assert!(home.join(NS).is_dir());
    drop(runner);
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 执行_输出尾部_停止_超时_重启后查记录() {
    let home = scratch("exec");
    let mut runner = Runner::start(&home);
    let client = runner.client();

    // 正常结束：合并的输出、退出码、out 文件、工作目录与干净环境。
    let view = client.run("c1", "echo hello; echo err 1>&2; pwd; env; exit 3").await;
    assert_eq!(view["status"], "done", "{view}");
    assert_eq!(view["exit_code"], 3);
    assert_eq!(view["conversation_id"], "c1");
    let out = output_of(&view);
    assert!(out.contains("hello\n") && out.contains("err\n"), "{out}");
    let conv_dir = home.join(NS).join("conversations").join("c1");
    assert!(out.contains(&format!("{}\n", conv_dir.display())), "工作目录是会话目录：{out}");
    assert!(out.contains(&format!("HOME={}\n", home.join(NS).display())));
    assert!(!out.contains(KEY) && !out.contains("SANDBOX_"), "密钥与运行器配置不得进入命令环境：{out}");
    assert!(out.contains("NODE_PATH=/opt/node-test\n"), "SANDBOX_COMMAND_ENV 里的变量进入命令环境：{out}");
    assert!(
        out.contains(&format!("PATH={}/.local/bin:", home.join(NS).display())),
        "PATH 最前面是账号的 ~/.local/bin：{out}"
    );
    let id = view["id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 16);
    assert_eq!(view["output_path"], format!("out/{id}.txt"));
    let on_disk = std::fs::read_to_string(conv_dir.join("out").join(format!("{id}.txt"))).unwrap();
    assert_eq!(on_disk, out, "out 文件与状态里的输出一致");
    // 结束后再查：同样的终态；不带 wait 立即返回。
    let (status, again) = client.get(&format!("/execute/{id}/status")).await;
    assert_eq!((status, again["status"].as_str(), again["exit_code"].as_i64()), (200, Some("done"), Some(3)));
    // 别的命名空间看不到。
    let (status, _) = client.call(reqwest::Method::GET, &format!("/execute/{id}/status"), Some("other"), Some(KEY), None).await;
    assert_eq!(status, 404);

    // 输出超过 16 KiB：按 offset 跟着轮询能拿全；事后从 0 开始查只有尾部，标 truncated；紧接着的 offset 不算截断。
    let view = client.run("c1", "head -c 40000 /dev/zero | tr '\\0' x").await;
    assert_eq!(view["status"], "done");
    assert_eq!(view["total_bytes"], 40000);
    assert_eq!(view["next_offset"], 40000);
    let id = view["id"].as_str().unwrap();
    let (_, from_zero) = client.get(&format!("/execute/{id}/status?offset=0")).await;
    assert_eq!(from_zero["truncated"], true);
    assert_eq!(output_of(&from_zero).len(), 16 * 1024);
    assert_eq!(std::fs::metadata(conv_dir.join("out").join(format!("{id}.txt"))).unwrap().len(), 40000, "完整输出在 out 文件里");
    let (_, tail) = client.get(&format!("/execute/{id}/status?offset=40000")).await;
    assert_eq!(tail["truncated"], false);
    assert_eq!(output_of(&tail), "");
    let (_, partial) = client.get(&format!("/execute/{id}/status?offset=30000")).await;
    assert_eq!(partial["truncated"], false);
    assert_eq!(output_of(&partial).len(), 10000);

    // 长命令：wait 到期时还在跑；DELETE 后终态 killed。
    let (status, running) = client
        .post("/execute?wait=1", json!({"conversation_id": "c1", "command": "echo start; sleep 30; echo never"}))
        .await;
    assert_eq!((status, running["status"].as_str()), (200, Some("running")), "{running}");
    assert_eq!(output_of(&running), "start\n");
    let id = running["id"].as_str().unwrap().to_owned();
    let offset = running["next_offset"].as_u64().unwrap();
    let (_, still) = client.get(&format!("/execute/{id}/status?wait=1&offset={offset}")).await;
    assert_eq!(still["status"], "running");
    assert_eq!(output_of(&still), "", "没有新输出");
    let started = std::time::Instant::now();
    let (status, killed) = client.delete(&format!("/execute/{id}")).await;
    assert_eq!((status, killed["status"].as_str()), (200, Some("killed")), "{killed}");
    assert!(started.elapsed() < Duration::from_secs(5), "停止应在 TERM 宽限内完成");
    assert!(!output_of(&killed).contains("never"));
    let (_, after) = client.get(&format!("/execute/{id}/status")).await;
    assert_eq!(after["status"], "killed");
    // 再停一次：已结束的直接返回终态。
    let (status, twice) = client.delete(&format!("/execute/{id}")).await;
    assert_eq!((status, twice["status"].as_str()), (200, Some("killed")));

    // 超时：1 秒的 timeout 到了就终止。
    let (status, timed) = client
        .post("/execute?wait=10", json!({"conversation_id": "c1", "command": "sleep 20", "timeout": 1}))
        .await;
    assert_eq!((status, timed["status"].as_str()), (200, Some("timeout")), "{timed}");

    // 重启：已结束的凭记录查到终态，尾部从 out 文件补读；重启时还在跑的变成 unknown。
    let done = client.run("c1", "echo persisted").await;
    let done_id = done["id"].as_str().unwrap().to_owned();
    let (_, orphan) = client
        .post("/execute?wait=1", json!({"conversation_id": "c1", "command": "sleep 15"}))
        .await;
    let orphan_id = orphan["id"].as_str().unwrap().to_owned();
    runner.kill_hard();
    let runner = Runner::start(&home);
    let client = runner.client();
    let (status, recovered) = client.get(&format!("/execute/{done_id}/status")).await;
    assert_eq!((status, recovered["status"].as_str(), recovered["exit_code"].as_i64()), (200, Some("done"), Some(0)), "{recovered}");
    assert_eq!(output_of(&recovered), "persisted\n");
    let (status, unknown) = client.get(&format!("/execute/{orphan_id}/status")).await;
    assert_eq!((status, unknown["status"].as_str()), (200, Some("unknown")), "{unknown}");
    assert!(unknown["message"].as_str().unwrap().contains("重启"));
    drop(runner);
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 文件接口_下载_指纹_证明_上传_删会话() {
    let home = scratch("files");
    let runner = Runner::start(&home);
    let client = runner.client();
    let uploads: Uploads = Arc::new(Mutex::new(Vec::new()));
    let up = upstream(uploads.clone()).await;

    // 准备一个文件：命令写进 out/，再由接口算指纹。
    let view = client.run("c1", "printf 'hello world' > out/a.txt && ln -s /etc/passwd out/link && ln -s / out/dirlink").await;
    assert_eq!(view["status"], "done", "{view}");
    let (status, stat) = client.post("/files/stat", json!({"conversation_id": "c1", "path": "out/a.txt"})).await;
    assert_eq!(status, 200, "{stat}");
    assert_eq!(stat["size"], 11);
    assert_eq!(stat["mime"], "text/plain");
    assert_eq!(stat["md5"], "5eb63bbbe01eeed093cb22bb8f5acdc3");
    assert_eq!(stat["etag"], stat["md5"], "单片的内容 ETag 就是 MD5");
    assert_eq!(stat["crc64_ecma"], crc64(0xC96C_5795_D787_0F42, b"hello world").to_string());
    assert_eq!(stat["crc64_nvme"], crc64(0x9A6C_9329_AC4B_C9B5, b"hello world").to_string());
    assert_eq!(stat["part_size"], 5 * 1024 * 1024);
    assert_eq!(stat["parts"][0]["md5"], stat["md5"]);
    assert_eq!(stat["parts"][0]["length"], 11);
    assert!(stat["modified"].as_u64().is_some());

    // 符号链接（文件或目录）一律拒绝；不存在 404；越出路径 400。
    for path in ["out/link", "out/dirlink/etc/passwd", "out/dirlink"] {
        let (status, body) = client.post("/files/stat", json!({"conversation_id": "c1", "path": path})).await;
        assert_eq!((status, body["error"].as_str()), (400, Some("path_invalid")), "{path}: {body}");
    }
    let (status, body) = client.post("/files/stat", json!({"conversation_id": "c1", "path": "out/none.txt"})).await;
    assert_eq!((status, body["error"].as_str()), (404, Some("not_found")));
    let (status, body) = client.post("/files/stat", json!({"conversation_id": "c1", "path": "../c2/x"})).await;
    assert_eq!((status, body["error"].as_str()), (400, Some("path_invalid")));
    let (status, body) = client.post("/files/stat", json!({"conversation_id": "c1", "path": "/etc/passwd"})).await;
    assert_eq!((status, body["error"].as_str()), (400, Some("path_invalid")));
    let (status, body) = client.post("/files/stat", json!({"conversation_id": "c9", "path": "x"})).await;
    assert_eq!((status, body["error"].as_str()), (404, Some("conversation_not_found")));

    // 持有证明：HMAC-SHA256(hex_decode(nonce), 段字节)。
    let nonce = hex(b"Jefe");
    let (status, proof) = client
        .post("/files/prove", json!({"conversation_id": "c1", "path": "out/a.txt", "nonce": nonce, "ranges": [{"offset": 0, "length": 11}, {"offset": 6, "length": 5}]}))
        .await;
    assert_eq!(status, 200, "{proof}");
    let expect = |data: &[u8]| {
        use hmac::Mac;
        let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(b"Jefe").unwrap();
        mac.update(data);
        hex(&mac.finalize().into_bytes())
    };
    assert_eq!(proof["proofs"][0], expect(b"hello world"));
    assert_eq!(proof["proofs"][1], expect(b"world"));
    assert_eq!(proof["size"], 11);
    let (status, body) = client
        .post("/files/prove", json!({"conversation_id": "c1", "path": "out/a.txt", "nonce": nonce, "ranges": [{"offset": 8, "length": 4}]}))
        .await;
    assert_eq!((status, body["error"].as_str()), (400, Some("range_invalid")), "{body}");
    let (status, _) = client
        .post("/files/prove", json!({"conversation_id": "c1", "path": "out/a.txt", "nonce": "zz", "ranges": [{"offset": 0, "length": 1}]}))
        .await;
    assert_eq!(status, 400);

    // 下载：直接、三跳；四跳、404、超限（声明长度 / 分块）、坏路径、坏协议都失败且不留半成品。
    let (status, dl) = client
        .post("/files/download", json!({"conversation_id": "c1", "url": format!("{up}/file.csv"), "path": "in/sub/data.csv", "max_bytes": 1000}))
        .await;
    assert_eq!(status, 200, "{dl}");
    assert_eq!(dl["size"], 8);
    assert_eq!(dl["mime"], "text/csv");
    assert_eq!(dl["content_type"], "text/csv");
    assert_eq!(dl["path"], "in/sub/data.csv");
    let conv_dir = home.join(NS).join("conversations").join("c1");
    assert_eq!(std::fs::read_to_string(conv_dir.join("in/sub/data.csv")).unwrap(), "a,b\n1,2\n");
    let (status, dl) = client
        .post("/files/download", json!({"conversation_id": "c1", "url": format!("{up}/r3"), "path": "in/hops.csv", "max_bytes": 1000}))
        .await;
    assert_eq!((status, dl["size"].as_u64()), (200, Some(8)), "{dl}");
    assert!(dl["final_url"].as_str().unwrap().ends_with("/file.csv"));
    let (status, body) = client
        .post("/files/download", json!({"conversation_id": "c1", "url": format!("{up}/r4"), "path": "in/toomany.csv", "max_bytes": 1000}))
        .await;
    assert_eq!((status, body["error"].as_str()), (502, Some("upstream_failed")), "{body}");
    let (status, body) = client
        .post("/files/download", json!({"conversation_id": "c1", "url": format!("{up}/missing"), "path": "in/missing.csv", "max_bytes": 1000}))
        .await;
    assert_eq!((status, body["error"].as_str(), body["upstream_status"].as_u64()), (502, Some("upstream_status"), Some(404)), "{body}");
    assert_eq!(body["upstream_body"], "no such file");
    let (status, body) = client
        .post("/files/download", json!({"conversation_id": "c1", "url": format!("{up}/big-declared"), "path": "in/big1.bin", "max_bytes": 50000}))
        .await;
    assert_eq!((status, body["error"].as_str()), (413, Some("too_large")), "{body}");
    let (status, body) = client
        .post("/files/download", json!({"conversation_id": "c1", "url": format!("{up}/big-chunked"), "path": "in/big2.bin", "max_bytes": 50000}))
        .await;
    assert_eq!((status, body["error"].as_str()), (413, Some("too_large")), "{body}");
    let leftovers: Vec<String> = std::fs::read_dir(conv_dir.join("in"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers.len(), 2, "只应有 sub 目录与 hops.csv，没有半成品：{leftovers:?}");
    let (status, body) = client
        .post("/files/download", json!({"conversation_id": "c1", "url": format!("{up}/file.csv"), "path": "../escape.csv", "max_bytes": 1000}))
        .await;
    assert_eq!((status, body["error"].as_str()), (400, Some("path_invalid")));
    let (status, _) = client
        .post("/files/download", json!({"conversation_id": "c1", "url": "ftp://example.com/x", "path": "in/x", "max_bytes": 1000}))
        .await;
    assert_eq!(status, 400);
    let (status, body) = client
        .post("/files/download", json!({"conversation_id": "c1", "url": format!("{up}/file.csv"), "path": "in/newdir/x.csv", "max_bytes": 1000}))
        .await;
    assert_eq!(status, 200, "不存在的中间目录由运行器建出来：{body}");
    assert!(conv_dir.join("in/newdir").is_dir());
    let (status, body) = client
        .post("/files/download", json!({"conversation_id": "c1", "url": format!("{up}/file.csv"), "path": "out/dirlink/x.csv", "max_bytes": 1000}))
        .await;
    assert_eq!((status, body["error"].as_str()), (400, Some("path_invalid")), "经过符号链接目录的写入拒绝：{body}");

    // 上传：两段计划、请求头原样带过去、Content-Length 正确；一段失败就停在那一段。
    let (status, uploaded) = client
        .post(
            "/files/upload",
            json!({"conversation_id": "c1", "path": "out/a.txt", "parts": [
                {"offset": 0, "length": 6, "url": format!("{up}/upload/1"), "headers": {"Content-Type": "text/plain", "x-test": "one", "Content-Length": "999"}},
                {"offset": 6, "length": 5, "method": "put", "url": format!("{up}/upload/2"), "headers": {"x-test": "two"}}
            ]}),
        )
        .await;
    assert_eq!(status, 200, "{uploaded}");
    assert_eq!(uploaded["parts"].as_array().unwrap().len(), 2);
    assert_eq!(uploaded["parts"][0]["etag"], "\"etag-0\"");
    assert_eq!(uploaded["parts"][1]["etag"], "\"etag-1\"");
    assert_eq!(uploaded["parts"][1]["status"], 200);
    let received = uploads.lock().unwrap().clone();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].path, "/upload/1");
    assert_eq!(received[0].body, b"hello ");
    assert_eq!(received[0].content_length.as_deref(), Some("6"), "Content-Length 由运行器算，不用计划里的 999");
    assert!(received[0].headers.contains(&("content-type".to_owned(), "text/plain".to_owned())));
    assert!(received[0].headers.contains(&("x-test".to_owned(), "one".to_owned())));
    assert_eq!(received[1].body, b"world");
    assert_eq!(received[1].content_length.as_deref(), Some("5"));
    let (status, failed) = client
        .post(
            "/files/upload",
            json!({"conversation_id": "c1", "path": "out/a.txt", "parts": [
                {"offset": 0, "length": 5, "url": format!("{up}/upload/3")},
                {"offset": 5, "length": 6, "url": format!("{up}/upload-forbidden")},
                {"offset": 0, "length": 1, "url": format!("{up}/upload/never")}
            ]}),
        )
        .await;
    assert_eq!((status, failed["error"].as_str(), failed["failed_part"].as_u64()), (502, Some("upload_failed"), Some(1)), "{failed}");
    assert_eq!(failed["upstream_status"], 403);
    assert_eq!(failed["parts"].as_array().unwrap().len(), 1, "带回已完成的第一段");
    assert_eq!(uploads.lock().unwrap().len(), 3, "第三段没有发出");
    let (status, body) = client
        .post("/files/upload", json!({"conversation_id": "c1", "path": "out/a.txt", "parts": [{"offset": 10, "length": 2, "url": format!("{up}/upload/x")}]}))
        .await;
    assert_eq!((status, body["error"].as_str()), (400, Some("range_invalid")), "{body}");
    let (status, _) = client
        .post("/files/upload", json!({"conversation_id": "c1", "path": "out/a.txt", "parts": [{"offset": 0, "length": 2, "method": "DELETE", "url": format!("{up}/upload/x")}]}))
        .await;
    assert_eq!(status, 400);

    // 删会话：目录与执行记录一起消失；再删返回 false。
    let exec_id = view["id"].as_str().unwrap().to_owned();
    let (status, deleted) = client.delete("/conversations/c1").await;
    assert_eq!((status, deleted["deleted"].as_bool()), (200, Some(true)), "{deleted}");
    assert!(!conv_dir.exists());
    let (status, body) = client.get(&format!("/execute/{exec_id}/status")).await;
    assert_eq!((status, body["error"].as_str()), (404, Some("execution_not_found")));
    let (status, deleted) = client.delete("/conversations/c1").await;
    assert_eq!((status, deleted["deleted"].as_bool()), (200, Some(false)));
    drop(runner);
    let _ = std::fs::remove_dir_all(&home);
}
