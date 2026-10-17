//! 路径安全：会话 ID 与相对路径的校验，以及“从家目录根出发逐级打开、每一级都不跟随符号链接”的目录与文件打开。
//!
//! 运行器以 root 读写文件，而会话目录归命名空间账号、内容由模型的命令任意改写：命令可以把 `in/x` 换成指向
//! /etc/shadow 的符号链接，或把 `conversations` 整个换成指向 / 的链接。按字符串规范化再 canonicalize 有竞态窗口，
//! 所以这里不走路径字符串，而是拿着上一级目录的 fd 用 openat(O_NOFOLLOW) 打开下一级，任何一级是符号链接就拒绝（ELOOP），
//! 最终打开的文件还要 fstat 确认是普通文件（FIFO 会让运行器卡住）。起点 `<家目录根>/<账号>` 这一级账号自己改不了：
//! 家目录根归 root，改名或删除自己的家目录需要家目录根的写权限。
//! 单用户模式下运行器就是普通用户，这些检查仍然执行，只是没有 chown。

use nix::fcntl::{fcntl, openat, renameat, FcntlArg, OFlag};
use nix::sys::stat::{fstat, mkdirat, Mode, SFlag};
use nix::unistd::{fchown, unlinkat, Gid, Uid, UnlinkatFlags};
use rand_core::RngCore;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

/// 相对路径的总长度上限（字节）；超过的多半是模型拼坏的路径，直接拒绝。
pub const MAX_PATH_BYTES: usize = 1024;
/// 单个路径分量的长度上限（字节）：ext4 等文件系统的文件名上限。
pub const MAX_COMPONENT_BYTES: usize = 255;
/// 会话 ID 的长度上限（字节）；助手的会话业务 ID 远短于它。
pub const MAX_CONVERSATION_ID: usize = 64;

/// 会话 ID 只接受字母、数字、连字符与下划线：它直接做目录名，不能含分隔符、点或空白。
pub fn valid_conversation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CONVERSATION_ID
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 会话内相对路径 → 干净的分量列表。拒绝绝对路径、`..`、反斜杠、NUL 与过长；`.` 与空分量（连续斜杠）忽略。
/// 这里不做“`..` 回退到上一级”的宽容处理：模型给出的路径里出现 `..` 几乎都是想越出会话目录。
pub fn normalize(path: &str) -> Result<Vec<String>, String> {
    if path.len() > MAX_PATH_BYTES {
        return Err(format!("路径超过 {MAX_PATH_BYTES} 字节"));
    }
    if path.contains('\0') {
        return Err("路径含 NUL".into());
    }
    if path.contains('\\') {
        return Err("路径不能含反斜杠，请用 /".into());
    }
    if path.starts_with('/') {
        return Err("不接受绝对路径，路径相对会话目录".into());
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err("路径不能含 ..".into()),
            other => {
                if other.len() > MAX_COMPONENT_BYTES {
                    return Err(format!("路径分量超过 {MAX_COMPONENT_BYTES} 字节"));
                }
                parts.push(other.to_owned());
            }
        }
    }
    if parts.is_empty() {
        return Err("需要文件路径".into());
    }
    Ok(parts)
}

/// 随机十六进制串（`bytes` 字节 → 2×bytes 个字符），执行号与临时文件名用。
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand_core::OsRng.fill_bytes(&mut buf);
    crate::checksum::hex(&buf)
}

/// 新建目录与文件的归属（多用户模式下是命名空间账号；单用户模式为 None，不 chown）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

/// 一个已打开的会话目录：所有文件操作都从这个 fd 出发。
pub struct Workspace {
    dir: OwnedFd,
    /// 会话目录的路径（给命令做工作目录；文件操作不用它）。
    pub path: PathBuf,
    owner: Option<Owner>,
}

fn dir_flags() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// 以 `parent` 为基准打开子目录 `name`，不跟随符号链接。
fn open_dir_at(parent: Option<RawFd>, name: &str) -> io::Result<OwnedFd> {
    let fd = openat(parent, name, dir_flags(), Mode::empty())?;
    // openat 成功返回的就是一个新 fd，这里只是把它交给 OwnedFd 管生命周期。
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// 子目录不存在就建（0755，归 owner），存在就直接打开；存在但不是目录或是符号链接时报错。
fn ensure_dir_at(parent: &OwnedFd, name: &str, owner: Option<Owner>) -> io::Result<OwnedFd> {
    let created = match mkdirat(Some(parent.as_raw_fd()), name, Mode::from_bits_truncate(0o755)) {
        Ok(()) => true,
        Err(nix::errno::Errno::EEXIST) => false,
        Err(e) => return Err(e.into()),
    };
    let dir = open_dir_at(Some(parent.as_raw_fd()), name)?;
    if created {
        chown_fd(&dir, owner)?;
    }
    Ok(dir)
}

fn chown_fd(fd: &OwnedFd, owner: Option<Owner>) -> io::Result<()> {
    if let Some(owner) = owner {
        fchown(fd.as_raw_fd(), Some(Uid::from_raw(owner.uid)), Some(Gid::from_raw(owner.gid)))?;
    }
    Ok(())
}

impl Workspace {
    /// 打开（`create` 为真时顺带建立）账号 `account` 的会话目录 `conversation`，含 `out/`。
    /// `create` 为假且目录不存在时返回 None。`home_root` 是可信配置，可以跟随符号链接；之后每一级都不跟随。
    pub fn open(
        home_root: &Path,
        account: &str,
        owner: Option<Owner>,
        conversation: &str,
        create: bool,
    ) -> io::Result<Option<Workspace>> {
        let root: OwnedFd = std::fs::File::open(home_root)?.into();
        let home = ensure_dir_at(&root, account, owner)?;
        let conversations = match Self::child(&home, "conversations", owner, create)? {
            Some(dir) => dir,
            None => return Ok(None),
        };
        let dir = match Self::child(&conversations, conversation, owner, create)? {
            Some(dir) => dir,
            None => return Ok(None),
        };
        if create {
            ensure_dir_at(&dir, "out", owner)?;
        }
        Ok(Some(Workspace {
            dir,
            path: home_root.join(account).join("conversations").join(conversation),
            owner,
        }))
    }

    fn child(parent: &OwnedFd, name: &str, owner: Option<Owner>, create: bool) -> io::Result<Option<OwnedFd>> {
        if create {
            return ensure_dir_at(parent, name, owner).map(Some);
        }
        match open_dir_at(Some(parent.as_raw_fd()), name) {
            Ok(dir) => Ok(Some(dir)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 沿分量逐级进入目录，返回最后一个分量的父目录 fd 与分量名；`create` 为真时缺的中间目录都建出来。
    fn descend(&self, rel: &[String], create: bool) -> io::Result<(OwnedFd, String)> {
        let (last, dirs) = rel.split_last().ok_or_else(|| invalid("需要文件路径"))?;
        let mut current = self.dir.try_clone()?;
        for name in dirs {
            current = if create {
                ensure_dir_at(&current, name, self.owner)?
            } else {
                open_dir_at(Some(current.as_raw_fd()), name)?
            };
        }
        Ok((current, last.clone()))
    }

    /// 只读打开会话内的普通文件；路径上任何一级是符号链接、或目标不是普通文件都拒绝。
    pub fn open_read(&self, rel: &[String]) -> io::Result<std::fs::File> {
        let (dir, name) = self.descend(rel, false)?;
        // O_NONBLOCK：目标若是 FIFO，打开不会卡住，随后由 fstat 拒掉。
        let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
        let fd = openat(Some(dir.as_raw_fd()), name.as_str(), flags, Mode::empty())?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let stat = fstat(fd.as_raw_fd())?;
        let kind = SFlag::from_bits_truncate(stat.st_mode as nix::libc::mode_t) & SFlag::S_IFMT;
        if kind != SFlag::S_IFREG {
            return Err(invalid("不是普通文件"));
        }
        // 普通文件不需要非阻塞标志，清掉以免影响后续读取方式。
        fcntl(fd.as_raw_fd(), FcntlArg::F_SETFL(OFlag::empty()))?;
        Ok(std::fs::File::from(fd))
    }

    /// 开始写一个文件：在目标所在目录建随机名的临时文件（归 owner），写完 `commit` 才改名到目标名；
    /// 中途失败或没有 commit 就删掉临时文件，目标名下不会出现半成品。目标已存在时 commit 会覆盖它。
    pub fn begin_write(&self, rel: &[String]) -> io::Result<PendingWrite> {
        let (dir, name) = self.descend(rel, true)?;
        let temp = format!(".{name}.{}.part", random_hex(6));
        // O_EXCL：随机名撞上现有文件就失败，而不是打开模型预先放在那里的东西。
        let flags = OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let fd = openat(Some(dir.as_raw_fd()), temp.as_str(), flags, Mode::from_bits_truncate(0o644))?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        chown_fd(&fd, self.owner)?;
        Ok(PendingWrite {
            dir,
            temp,
            final_name: name,
            file: Some(std::fs::File::from(fd)),
            committed: false,
        })
    }
}

/// 一次进行中的写入，见 `Workspace::begin_write`。
pub struct PendingWrite {
    dir: OwnedFd,
    temp: String,
    final_name: String,
    file: Option<std::fs::File>,
    committed: bool,
}

impl PendingWrite {
    /// 取走文件句柄去写（只能取一次）。
    pub fn take_file(&mut self) -> Option<std::fs::File> {
        self.file.take()
    }

    /// 把临时文件改名成目标名。目标名是符号链接时替换的是链接本身，不碰它指向的东西。
    pub fn commit(mut self) -> io::Result<()> {
        renameat(
            Some(self.dir.as_raw_fd()),
            self.temp.as_str(),
            Some(self.dir.as_raw_fd()),
            self.final_name.as_str(),
        )?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        if !self.committed {
            let _ = unlinkat(Some(self.dir.as_raw_fd()), self.temp.as_str(), UnlinkatFlags::NoRemoveDir);
        }
    }
}

/// `Workspace::open` 的异步包装：逐级 openat 是阻塞系统调用，放到阻塞线程池里做。
pub async fn open_workspace(
    home_root: &Path,
    account: &str,
    owner: Option<Owner>,
    conversation: &str,
    create: bool,
) -> io::Result<Option<Workspace>> {
    let root = home_root.to_owned();
    let account = account.to_owned();
    let conversation = conversation.to_owned();
    tokio::task::spawn_blocking(move || Workspace::open(&root, &account, owner, &conversation, create))
        .await
        .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn 路径规范化只接受会话内的相对路径() {
        assert_eq!(normalize("in/a.txt").unwrap(), vec!["in", "a.txt"]);
        assert_eq!(normalize("./in//b/./c.csv").unwrap(), vec!["in", "b", "c.csv"]);
        assert_eq!(normalize("out/").unwrap(), vec!["out"]);
        for bad in ["", ".", "/etc/passwd", "../x", "in/../../x", "in/..", "a\\b", "a\0b"] {
            assert!(normalize(bad).is_err(), "{bad:?} 应被拒绝");
        }
        assert!(normalize(&"a/".repeat(600)).is_err(), "过长路径");
        assert!(normalize(&"x".repeat(256)).is_err(), "过长分量");
        assert!(valid_conversation_id("c0q7cx1gpb9y6") && valid_conversation_id("a-b_C9"));
        for bad in ["", "a/b", "a.b", "a b", "..", &"x".repeat(65)] {
            assert!(!valid_conversation_id(bad), "{bad:?}");
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sandbox-paths-{name}-{}", random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn 工作区写入经临时文件_读取拒绝符号链接与非普通文件() {
        let root = scratch("ws");
        assert!(Workspace::open(&root, "acct", None, "c1", false).unwrap().is_none(), "不建就不存在");
        let ws = Workspace::open(&root, "acct", None, "c1", true).unwrap().unwrap();
        assert!(root.join("acct/conversations/c1/out").is_dir());

        // 写：中间目录自动建立；commit 前目标名不存在。
        let rel = normalize("in/sub/a.txt").unwrap();
        let mut pending = ws.begin_write(&rel).unwrap();
        pending.take_file().unwrap().write_all(b"hello").unwrap();
        assert!(!root.join("acct/conversations/c1/in/sub/a.txt").exists());
        pending.commit().unwrap();
        let mut text = String::new();
        ws.open_read(&rel).unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello");
        // 不 commit 就没有半成品留下。
        let rel_b = normalize("in/b.txt").unwrap();
        let mut aborted = ws.begin_write(&rel_b).unwrap();
        aborted.take_file().unwrap().write_all(b"half").unwrap();
        drop(aborted);
        let leftovers: Vec<_> = std::fs::read_dir(root.join("acct/conversations/c1/in")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "只剩 sub 目录：{leftovers:?}");

        // 读：会话内指向外面的符号链接、链接目录、目录本身、FIFO 都拒绝。
        let outside = root.join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("acct/conversations/c1/in/link.txt")).unwrap();
        assert!(ws.open_read(&normalize("in/link.txt").unwrap()).is_err());
        std::os::unix::fs::symlink(&root, root.join("acct/conversations/c1/in/dirlink")).unwrap();
        assert!(ws.open_read(&normalize("in/dirlink/secret.txt").unwrap()).is_err());
        assert!(ws.open_read(&normalize("in").unwrap()).is_err(), "目录不是文件");
        assert!(ws.open_read(&normalize("in/none.txt").unwrap()).is_err());
        nix::unistd::mkfifo(&root.join("acct/conversations/c1/in/pipe"), Mode::from_bits_truncate(0o644)).unwrap();
        assert!(ws.open_read(&normalize("in/pipe").unwrap()).is_err(), "FIFO 不是普通文件");
        // 写到符号链接目录下也拒绝。
        assert!(ws.begin_write(&normalize("in/dirlink/x.txt").unwrap()).is_err());

        // 整个 conversations 被换成符号链接时打开会话失败。
        let other = scratch("ws2");
        std::fs::create_dir_all(other.join("acct")).unwrap();
        std::os::unix::fs::symlink(&root, other.join("acct/conversations")).unwrap();
        assert!(Workspace::open(&other, "acct", None, "c1", true).is_err());
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
    }
}
