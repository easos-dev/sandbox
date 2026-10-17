//! 命令执行：以命名空间账号在会话目录里跑 `bash -c`，标准输出与错误合并成一条流，边写进 `out/<执行号>.txt`
//! 边在内存留 16 KiB 尾部；状态可长轮询。超时或被停止时按进程组先 TERM、两秒后 KILL。
//! 每条执行另存一份小 JSON 记录（归运行器自己的目录），运行器重启后凭执行号仍能回答“已结束（退出码）”或
//! “未知（重启时还在跑）”，输出尾部从 out 文件补读；不恢复进行中的命令。
//! 命令环境是干净的：只有 HOME、USER、PATH 等几个固定变量，运行器的密钥与任何 EasOS 凭证都不在里面。

use crate::account::Account;
use crate::config::Config;
use crate::paths::{self, Workspace};
use crate::server::{now_ms, ApiError, ApiResult};
use http::StatusCode;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe::Receiver;
use tokio::process::Child;
use tokio::sync::watch;

/// 单条命令的最长运行时间，也是 `timeout` 参数的上限；与助手客户端今天的 COMMAND_TIMEOUT 一致。
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// `wait` 参数的上限：一次长轮询最多挂 30 秒，助手客户端今天就是按 30 秒轮询的。
pub const MAX_WAIT: Duration = Duration::from_secs(30);
/// 状态应答里保留的输出尾部长度；更早的输出只在 out 文件里。
pub const TAIL_BYTES: usize = 16 * 1024;
/// 命令文本的长度上限（字节）：更长的多半是把文件内容塞进了命令，应该先写成文件再执行。
pub const MAX_COMMAND_BYTES: usize = 256 * 1024;
/// 发 SIGTERM 后等多久再 SIGKILL。
const TERM_GRACE: Duration = Duration::from_secs(2);
/// 进程结束后再给输出管道多少时间：它 fork 出来又没被信号波及的后代可能还握着写端，不能为它们无限等。
const DRAIN_GRACE: Duration = Duration::from_millis(500);
/// 已结束的执行在内存里保留多久（之后状态从记录与 out 文件补读）。
const KEEP_FINISHED: Duration = Duration::from_secs(3600);
/// 执行号的随机字节数（十六进制后 16 个字符）。
const ID_BYTES: usize = 8;

/// 一次执行的状态；`done`/`timeout`/`killed` 与 Open Terminal 相同，助手客户端已按它们解释结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// 还在跑。
    Running,
    /// 自己结束了，看 exit_code。
    Done,
    /// 超过 timeout 被运行器终止。
    Timeout,
    /// 被 `DELETE /execute/{id}`、删会话或运行器退出终止。
    Killed,
    /// 进程启动后等待结果出错（极少见）。
    Failed,
    /// 运行器在它结束前重启过，结果查不到了。
    Unknown,
}

/// 落盘的执行记录（`<状态目录>/executions/<账号>/<执行号>.json`），开始时写一次、结束时再写一次。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    conversation_id: String,
    status: Status,
    exit_code: Option<i64>,
    /// 开始时刻（Unix 毫秒）。
    started_at: u64,
    /// 结束时刻（Unix 毫秒），还在跑时为空。
    finished_at: Option<u64>,
    /// 到结束为止的输出总字节数。
    total_bytes: u64,
}

/// 执行的实时进展。
struct Progress {
    status: Status,
    exit_code: Option<i64>,
    /// 到现在为止输出的总字节数（含已经不在尾部里的）。
    total: u64,
    /// 输出尾部，最多 TAIL_BYTES。
    tail: Vec<u8>,
    finished_at: Option<u64>,
    /// 结束时刻（单调时钟），内存清理按它算年龄。
    finished_instant: Option<Instant>,
    /// 异常说明（例如等待进程失败、重启后结果未知）。
    note: Option<String>,
}

/// 一条正在跑或刚结束的执行。
pub struct Execution {
    pub id: String,
    /// 所属账号名；查状态与停止都要求账号匹配，别的命名空间看不到。
    pub account: String,
    pub conversation_id: String,
    /// 开始时刻（Unix 毫秒）。
    pub started_at: u64,
    progress: Mutex<Progress>,
    /// 每次输出或状态变化加一，长轮询靠它醒来。
    changed: watch::Sender<u64>,
    /// 终止请求：Some(原因)，只接受第一次；由 supervise 消费。
    stop: watch::Sender<Option<Status>>,
    /// 子进程的进程组（等于子进程 PID），终止时整组发信号。
    pgid: Pid,
}

impl Execution {
    fn append(&self, chunk: &[u8]) {
        {
            let mut p = self.progress.lock().unwrap();
            p.total += chunk.len() as u64;
            p.tail.extend_from_slice(chunk);
            if p.tail.len() > TAIL_BYTES {
                let cut = p.tail.len() - TAIL_BYTES;
                p.tail.drain(..cut);
            }
        }
        self.changed.send_modify(|v| *v += 1);
    }

    fn finish(&self, status: Status, exit_code: Option<i64>, note: Option<String>) {
        {
            let mut p = self.progress.lock().unwrap();
            p.status = status;
            p.exit_code = exit_code;
            p.finished_at = Some(now_ms());
            p.finished_instant = Some(Instant::now());
            p.note = note;
        }
        self.changed.send_modify(|v| *v += 1);
    }

    /// 请求终止；已经有请求时忽略后来的。
    pub fn request_stop(&self, reason: Status) {
        self.stop.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(reason);
                true
            } else {
                false
            }
        });
    }

    fn signal(&self, signal: Signal) {
        match killpg(self.pgid, signal) {
            // 进程组已经不在了：正常，刚好自己结束。
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(e) => eprintln!("向执行 {} 的进程组发 {signal:?} 失败：{e}", self.id),
        }
    }

    fn is_running(&self) -> bool {
        self.progress.lock().unwrap().status == Status::Running
    }

    /// 结束后过了多久；还在跑为 None。
    fn finished_for(&self) -> Option<Duration> {
        self.progress.lock().unwrap().finished_instant.map(|t| t.elapsed())
    }

    /// 当前状态的 JSON 视图：`offset` 之后、尾部以内的输出。
    pub fn view(&self, offset: u64) -> Value {
        let p = self.progress.lock().unwrap();
        render(
            &self.id,
            &self.conversation_id,
            p.status,
            p.exit_code,
            p.total,
            &p.tail,
            offset,
            self.started_at,
            p.finished_at,
            p.note.as_deref(),
        )
    }

    /// 等到结束、或 `offset` 之后有新输出、或 `wait` 到期，再返回视图。
    pub async fn wait_view(&self, offset: u64, wait: Duration) -> Value {
        self.wait_until(wait, |p| p.status != Status::Running || p.total > offset)
            .await;
        self.view(offset)
    }

    /// 等到结束或 `wait` 到期；返回是否已结束。
    pub async fn wait_finished(&self, wait: Duration) -> bool {
        self.wait_until(wait, |p| p.status != Status::Running).await
    }

    async fn wait_until(&self, wait: Duration, ready: impl Fn(&Progress) -> bool) -> bool {
        let mut rx = self.changed.subscribe();
        let deadline = Instant::now() + wait;
        loop {
            let ready_now = {
                let p = self.progress.lock().unwrap();
                ready(&p)
            };
            if ready_now {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            if tokio::time::timeout(remaining, rx.changed()).await.is_err() {
                let p = self.progress.lock().unwrap();
                return ready(&p);
            }
        }
    }
}

/// 状态应答的形状（与 Open Terminal 对齐的字段：id、status、exit_code、output[].data、next_offset、truncated）。
#[allow(clippy::too_many_arguments)]
fn render(
    id: &str,
    conversation_id: &str,
    status: Status,
    exit_code: Option<i64>,
    total: u64,
    tail: &[u8],
    offset: u64,
    started_at: u64,
    finished_at: Option<u64>,
    note: Option<&str>,
) -> Value {
    // 尾部覆盖的是 [tail_start, total)；调用方的 offset 落在它之前时，中间那段只能去 out 文件里看，标 truncated。
    let tail_start = total - tail.len() as u64;
    let from = offset.clamp(tail_start, total);
    let data = String::from_utf8_lossy(&tail[(from - tail_start) as usize..]);
    let output = if data.is_empty() {
        json!([])
    } else {
        json!([{"stream": "combined", "data": data}])
    };
    json!({
        "id": id,
        "conversation_id": conversation_id,
        "status": status,
        "exit_code": exit_code,
        "output": output,
        "next_offset": total,
        "total_bytes": total,
        "truncated": offset < tail_start,
        "output_path": format!("out/{id}.txt"),
        "started_at": started_at,
        "finished_at": finished_at,
        "message": note,
    })
}

/// 全部执行的登记：内存里的活执行 + 磁盘上的记录。
pub struct Executions {
    cfg: Arc<Config>,
    live: Mutex<HashMap<String, Arc<Execution>>>,
}

impl Executions {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self {
            cfg,
            live: Mutex::new(HashMap::new()),
        }
    }

    /// 启动一条命令：建会话目录与 out 文件、起进程、登记、起监管任务。
    pub async fn start(
        &self,
        account: Arc<Account>,
        conversation: &str,
        command: &str,
        timeout: Duration,
    ) -> ApiResult<Arc<Execution>> {
        self.sweep();
        let cfg = self.cfg.clone();
        let workspace = paths::open_workspace(&cfg.home_root, &account.name, account.owner, conversation, true)
            .await
            .map_err(|e| ApiError::internal(format!("准备会话目录失败：{e}")))?
            .ok_or_else(|| ApiError::internal("会话目录建立后仍不存在"))?;
        let id = paths::random_hex(ID_BYTES);
        let out_file = {
            let mut pending = workspace
                .begin_write(&["out".to_owned(), format!("{id}.txt")])
                .map_err(|e| ApiError::internal(format!("建立输出文件失败：{e}")))?;
            let file = pending.take_file().expect("刚建立的文件句柄还在");
            // 输出文件是边跑边写的，不需要“写完才出现”，立刻放到最终名下。
            pending
                .commit()
                .map_err(|e| ApiError::internal(format!("放置输出文件失败：{e}")))?;
            file
        };
        let (reader, writer) = io::pipe().map_err(|e| ApiError::internal(format!("建立输出管道失败：{e}")))?;
        let stderr = writer
            .try_clone()
            .map_err(|e| ApiError::internal(format!("复制输出管道失败：{e}")))?;
        let child = {
            let mut cmd = tokio::process::Command::new(&cfg.shell);
            cmd.arg("-c")
                .arg(command)
                .current_dir(&workspace.path)
                .env_clear()
                .env("HOME", &account.home)
                .env("USER", &account.name)
                .env("LOGNAME", &account.name)
                .env("SHELL", &cfg.shell)
                .env("PATH", &cfg.command_path)
                .env("LANG", "C.UTF-8")
                .env("TERM", "dumb")
                // Python 对管道默认全缓冲，不设的话输出要到结束才看得到；matplotlib 没有显示器，固定无界面后端。
                .env("PYTHONUNBUFFERED", "1")
                .env("MPLBACKEND", "Agg")
                .stdin(Stdio::null())
                .stdout(Stdio::from(writer))
                .stderr(Stdio::from(stderr))
                // 自成进程组：超时或停止时整组发信号，命令 fork 出来的子进程也一起结束。
                .process_group(0);
            if let Some(owner) = account.owner {
                cmd.uid(owner.uid).gid(owner.gid);
            }
            cmd.spawn()
                .map_err(|e| ApiError::internal(format!("启动命令失败：{e}")))?
            // cmd 在这里析构，连带放掉我们手里的管道写端；不放的话读端永远等不到 EOF。
        };
        let pid = child
            .id()
            .ok_or_else(|| ApiError::internal("刚启动的进程没有 PID"))?;
        let reader = Receiver::from_owned_fd(OwnedFd::from(reader))
            .map_err(|e| ApiError::internal(format!("接管输出管道失败：{e}")))?;
        let execution = Arc::new(Execution {
            id: id.clone(),
            account: account.name.clone(),
            conversation_id: conversation.to_owned(),
            started_at: now_ms(),
            progress: Mutex::new(Progress {
                status: Status::Running,
                exit_code: None,
                total: 0,
                tail: Vec::new(),
                finished_at: None,
                finished_instant: None,
                note: None,
            }),
            changed: watch::channel(0).0,
            stop: watch::channel(None).0,
            pgid: Pid::from_raw(pid as i32),
        });
        self.live.lock().unwrap().insert(id, execution.clone());
        write_record(&cfg, &execution).await;
        tokio::spawn(supervise(cfg, execution.clone(), child, reader, out_file, timeout));
        Ok(execution)
    }

    /// 查状态：活执行走内存（可长轮询）；不在内存里的查记录，尾部从 out 文件补读。
    pub async fn status(&self, account: &Account, id: &str, offset: u64, wait: Duration) -> ApiResult<Value> {
        if let Some(execution) = self.find_live(account, id) {
            return Ok(execution.wait_view(offset, wait).await);
        }
        let record = self
            .read_record(account, id)
            .await?
            .ok_or_else(|| not_found(id))?;
        Ok(self.view_record(account, record, offset).await)
    }

    /// 停止一条执行，等它真的结束（TERM 两秒 + KILL）再返回终态；已结束的直接返回记录。
    pub async fn stop(&self, account: &Account, id: &str) -> ApiResult<Value> {
        let Some(execution) = self.find_live(account, id) else {
            let record = self
                .read_record(account, id)
                .await?
                .ok_or_else(|| not_found(id))?;
            return Ok(self.view_record(account, record, 0).await);
        };
        execution.request_stop(Status::Killed);
        execution
            .wait_finished(TERM_GRACE + Duration::from_secs(2))
            .await;
        Ok(execution.view(0))
    }

    /// 会话被删：停掉它还在跑的命令，忘掉它们，删掉它的执行记录（记录随会话消失，不另设清理任务）。
    pub async fn end_conversation(&self, account: &Account, conversation: &str) {
        let targets: Vec<Arc<Execution>> = self
            .live
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.account == account.name && e.conversation_id == conversation)
            .cloned()
            .collect();
        for execution in &targets {
            execution.request_stop(Status::Killed);
        }
        for execution in &targets {
            execution
                .wait_finished(TERM_GRACE + Duration::from_secs(2))
                .await;
        }
        {
            let mut live = self.live.lock().unwrap();
            for execution in &targets {
                live.remove(&execution.id);
            }
        }
        let dir = record_dir(&self.cfg, &account.name);
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Ok(bytes) = tokio::fs::read(&path).await else {
                continue;
            };
            if let Ok(record) = serde_json::from_slice::<Record>(&bytes) {
                if record.conversation_id == conversation {
                    let _ = tokio::fs::remove_file(&path).await;
                }
            }
        }
    }

    /// 进程退出前：终止所有还在跑的命令，让它们的记录落成终态。
    pub async fn shutdown(&self) {
        let running: Vec<Arc<Execution>> = self
            .live
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.is_running())
            .cloned()
            .collect();
        if running.is_empty() {
            return;
        }
        println!("退出前终止 {} 条还在跑的命令", running.len());
        for execution in &running {
            execution.request_stop(Status::Killed);
        }
        let deadline = Instant::now() + TERM_GRACE + Duration::from_secs(1);
        for execution in &running {
            execution
                .wait_finished(deadline.saturating_duration_since(Instant::now()))
                .await;
        }
    }

    fn find_live(&self, account: &Account, id: &str) -> Option<Arc<Execution>> {
        self.live
            .lock()
            .unwrap()
            .get(id)
            .filter(|e| e.account == account.name)
            .cloned()
    }

    /// 清掉结束已久的执行，内存不随历史增长；之后它们的状态从记录与 out 文件回答。
    fn sweep(&self) {
        self.live
            .lock()
            .unwrap()
            .retain(|_, e| e.finished_for().is_none_or(|age| age < KEEP_FINISHED));
    }

    async fn read_record(&self, account: &Account, id: &str) -> ApiResult<Option<Record>> {
        if !valid_id(id) {
            return Ok(None);
        }
        let path = record_dir(&self.cfg, &account.name).join(format!("{id}.json"));
        match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| ApiError::internal(format!("执行记录 {} 损坏：{e}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ApiError::internal(format!("读执行记录失败：{e}"))),
        }
    }

    async fn view_record(&self, account: &Account, record: Record, offset: u64) -> Value {
        // 记录还是 running 却不在内存里，只有一种可能：运行器在它结束前重启了。
        let (status, note) = if record.status == Status::Running {
            (
                Status::Unknown,
                Some("运行器在这条命令结束前重启过，结果未知；可以检查它该产生的文件。"),
            )
        } else {
            (record.status, None)
        };
        let (total, tail) = read_tail(&self.cfg, account, &record.conversation_id, &record.id)
            .await
            .unwrap_or((record.total_bytes, Vec::new()));
        render(
            &record.id,
            &record.conversation_id,
            status,
            record.exit_code,
            total,
            &tail,
            offset,
            record.started_at,
            record.finished_at,
            note,
        )
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == ID_BYTES * 2 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn not_found(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "execution_not_found",
        format!("没有执行号 {id} 的记录"),
    )
}

fn record_dir(cfg: &Config, account: &str) -> PathBuf {
    cfg.state_dir.join("executions").join(account)
}

/// 写执行记录：先写临时名再改名，读到的永远是完整的 JSON。失败只记日志，不影响命令本身。
async fn write_record(cfg: &Config, execution: &Execution) {
    let record = {
        let p = execution.progress.lock().unwrap();
        Record {
            id: execution.id.clone(),
            conversation_id: execution.conversation_id.clone(),
            status: p.status,
            exit_code: p.exit_code,
            started_at: execution.started_at,
            finished_at: p.finished_at,
            total_bytes: p.total,
        }
    };
    let dir = record_dir(cfg, &execution.account);
    let result: io::Result<()> = async {
        tokio::fs::create_dir_all(&dir).await?;
        let bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
        let path = dir.join(format!("{}.json", record.id));
        let temp = dir.join(format!("{}.json.tmp", record.id));
        tokio::fs::write(&temp, bytes).await?;
        tokio::fs::rename(&temp, &path).await
    }
    .await;
    if let Err(e) = result {
        eprintln!("写执行记录 {} 失败：{e}", record.id);
    }
}

/// 从 out 文件补读输出尾部：返回（文件长度, 尾部字节）。文件不存在或读不了返回 None。
async fn read_tail(cfg: &Config, account: &Account, conversation: &str, id: &str) -> Option<(u64, Vec<u8>)> {
    let root = cfg.home_root.clone();
    let name = account.name.clone();
    let owner = account.owner;
    let conversation = conversation.to_owned();
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || -> io::Result<(u64, Vec<u8>)> {
        use std::io::{Read, Seek, SeekFrom};
        let workspace = Workspace::open(&root, &name, owner, &conversation, false)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let mut file = workspace.open_read(&["out".to_owned(), format!("{id}.txt")])?;
        let len = file.metadata()?.len();
        let skip = len.saturating_sub(TAIL_BYTES as u64);
        file.seek(SeekFrom::Start(skip))?;
        let mut tail = Vec::with_capacity((len - skip) as usize);
        file.take(TAIL_BYTES as u64).read_to_end(&mut tail)?;
        Ok((len, tail))
    })
    .await
    .ok()?
    .ok()
}

/// 监管一条执行：读输出直到 EOF，等进程结束或超时或被要求停止，落终态与记录。
async fn supervise(
    cfg: Arc<Config>,
    execution: Arc<Execution>,
    mut child: Child,
    reader: Receiver,
    out: std::fs::File,
    timeout: Duration,
) {
    let pump = tokio::spawn(pump_output(
        execution.clone(),
        reader,
        tokio::fs::File::from_std(out),
    ));
    let mut stop = execution.stop.subscribe();
    let mut ended: Option<Status> = None;
    let exit = tokio::select! {
        status = child.wait() => status,
        _ = tokio::time::sleep(timeout) => {
            ended = Some(Status::Timeout);
            terminate(&execution, &mut child).await
        }
        // 在分支内部就把 watch::Ref 解成 Copy 的值：Ref 不是 Send，不能留在 select 的结果里跨 await。
        requested = async { stop.wait_for(|reason| reason.is_some()).await.ok().and_then(|reason| *reason) } => {
            ended = requested;
            terminate(&execution, &mut child).await
        }
    };
    tokio::pin!(pump);
    if tokio::time::timeout(DRAIN_GRACE, &mut pump).await.is_err() {
        pump.abort();
    }
    let (status, exit_code, note) = match (ended, exit) {
        (Some(reason), Ok(status)) => (reason, exit_code_of(status), None),
        (None, Ok(status)) => (Status::Done, exit_code_of(status), None),
        (reason, Err(e)) => (
            reason.unwrap_or(Status::Failed),
            None,
            Some(format!("等待进程结束失败：{e}")),
        ),
    };
    execution.finish(status, exit_code, note);
    write_record(&cfg, &execution).await;
}

/// 先 TERM，宽限期内没结束再 KILL，然后等到它真的结束。
async fn terminate(execution: &Execution, child: &mut Child) -> io::Result<ExitStatus> {
    execution.signal(Signal::SIGTERM);
    match tokio::time::timeout(TERM_GRACE, child.wait()).await {
        Ok(result) => result,
        Err(_) => {
            execution.signal(Signal::SIGKILL);
            child.wait().await
        }
    }
}

/// 被信号结束时按 shell 惯例给 128+信号号，让模型能从退出码看出是被杀的。
fn exit_code_of(status: ExitStatus) -> Option<i64> {
    status
        .code()
        .map(i64::from)
        .or_else(|| status.signal().map(|s| 128 + i64::from(s)))
}

/// 把管道里的输出同时写进 out 文件与内存尾部，直到 EOF。
async fn pump_output(execution: Arc<Execution>, mut reader: Receiver, mut file: tokio::fs::File) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = file.write_all(&buf[..n]).await {
                    eprintln!("写执行 {} 的输出文件失败：{e}", execution.id);
                }
                execution.append(&buf[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let _ = file.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 状态视图只给尾部并标出被截掉的部分() {
        let total = (TAIL_BYTES + 100) as u64;
        let tail: Vec<u8> = (0..TAIL_BYTES).map(|i| b'a' + (i % 26) as u8).collect();
        // 调用方从 0 开始要：只能拿到尾部，标 truncated。
        let v = render("x", "c", Status::Running, None, total, &tail, 0, 1, None, None);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["next_offset"], total);
        assert_eq!(v["output"][0]["data"].as_str().unwrap().len(), TAIL_BYTES);
        // 调用方紧跟着上次的位置要：拿到增量，不算截断。
        let v = render("x", "c", Status::Running, None, total, &tail, total - 10, 1, None, None);
        assert_eq!(v["truncated"], false);
        assert_eq!(v["output"][0]["data"].as_str().unwrap().len(), 10);
        // 没有新输出：output 为空数组。
        let v = render("x", "c", Status::Done, Some(0), total, &tail, total, 1, Some(2), None);
        assert_eq!(v["output"].as_array().unwrap().len(), 0);
        assert_eq!(v["status"], "done");
        assert_eq!(v["output_path"], "out/x.txt");
        // 多字节字符被尾部边界切开时换成替换符，不 panic。
        let text = "中文输出".repeat(3000);
        let bytes = text.as_bytes();
        let cut = &bytes[bytes.len() - TAIL_BYTES..];
        let v = render("x", "c", Status::Running, None, bytes.len() as u64, cut, 0, 1, None, None);
        assert!(v["output"][0]["data"].as_str().unwrap().ends_with("中文输出"));
    }

    #[test]
    fn 执行号只认_16_位小写十六进制() {
        assert!(valid_id("0123456789abcdef"));
        assert!(!valid_id("0123456789ABCDEF"));
        assert!(!valid_id("0123456789abcde"));
        assert!(!valid_id("../../etc/passwd"));
        assert_eq!(exit_code_of(ExitStatus::from_raw(0)), Some(0));
        assert_eq!(exit_code_of(ExitStatus::from_raw(9)), Some(137), "被 SIGKILL");
        assert_eq!(exit_code_of(ExitStatus::from_raw(3 << 8)), Some(3));
    }
}
