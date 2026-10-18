//! 命名空间账号：请求头 `X-User-Id` 的值就是系统账号名，首次出现时建账号（无 sudo、家目录 0700、互不可读），
//! 之后命令与新建文件都归它。语义与 Open Terminal 的多用户模式一致，助手客户端不用改命名方式。
//! 单用户模式不建账号、不切换，只在家目录根下建同名目录，供本机开发与集成测试。

use crate::config::Config;
use crate::paths::Owner;
use crate::server::ApiError;
use http::StatusCode;
use nix::unistd::{Gid, Group, Uid, User};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 账号名长度上限：Linux 用户名的上限。
pub const MAX_NAME: usize = 32;

/// 一个命名空间账号。
#[derive(Debug)]
pub struct Account {
    /// 账号名 = X-User-Id。
    pub name: String,
    /// 家目录 `<家目录根>/<账号名>`。
    pub home: PathBuf,
    /// 多用户模式下的 uid/gid；单用户模式为 None。
    pub owner: Option<Owner>,
}

/// 账号登记：已解析过的账号缓存在内存，建账号串行化，避免两个并发首次请求同时 useradd。
pub struct Accounts {
    cfg: Arc<Config>,
    cache: Mutex<HashMap<String, Arc<Account>>>,
    create: tokio::sync::Mutex<()>,
}

/// 账号名只接受小写字母、数字与连字符，首字符不能是连字符：它直接做系统用户名与目录名。
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl Accounts {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self {
            cfg,
            cache: Mutex::new(HashMap::new()),
            create: tokio::sync::Mutex::new(()),
        }
    }

    /// 解析 X-User-Id 对应的账号，不存在就建。
    pub async fn resolve(&self, name: &str) -> Result<Arc<Account>, ApiError> {
        if !valid_name(name) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "namespace_invalid",
                "X-User-Id 只能含小写字母、数字与连字符，不超过 32 字节，不能以连字符开头",
            ));
        }
        if let Some(account) = self.cache.lock().unwrap().get(name) {
            return Ok(account.clone());
        }
        let _guard = self.create.lock().await;
        if let Some(account) = self.cache.lock().unwrap().get(name) {
            return Ok(account.clone());
        }
        let account = if self.cfg.single_user {
            self.single(name)?
        } else {
            self.system(name).await?
        };
        let account = Arc::new(account);
        self.cache.lock().unwrap().insert(name.to_owned(), account.clone());
        Ok(account)
    }

    fn single(&self, name: &str) -> Result<Account, ApiError> {
        let home = self.cfg.home_root.join(name);
        std::fs::create_dir_all(&home)
            .map_err(|e| ApiError::internal(format!("建立目录 {} 失败：{e}", home.display())))?;
        Ok(Account {
            name: name.to_owned(),
            home,
            owner: None,
        })
    }

    async fn system(&self, name: &str) -> Result<Account, ApiError> {
        let home = self.cfg.home_root.join(name);
        let user = match lookup(name)? {
            Some(user) => user,
            None => {
                self.create(name, &home).await?;
                lookup(name)?.ok_or_else(|| ApiError::internal(format!("useradd 之后仍查不到账号 {name}")))?
            }
        };
        // 撞上系统账号（root、daemon…）或别人建的同名账号：不替它执行任何东西。
        if user.uid.as_raw() < self.cfg.uid_min || user.dir != home {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "namespace_reserved",
                format!("账号 {name} 不是沙箱建立的命名空间账号"),
            ));
        }
        Ok(Account {
            name: name.to_owned(),
            home,
            owner: Some(Owner {
                uid: user.uid.as_raw(),
                gid: user.gid.as_raw(),
            }),
        })
    }

    /// 建系统账号：同名组、家目录 0700、登录 shell 为 bash，UID 与 GID 在 [uid_min, uid_min + ID_SPAN] 内。
    ///
    /// 账号表（/etc/passwd、/etc/group）在容器层里，家目录在卷里：容器重建后账号没了，家目录还在。
    /// 所以家目录已存在时沿用它原来的属主与属组重建账号，模型才访问得到自己以前的文件；
    /// 新账号的 ID 则避开家目录根下所有目录的属主与属组（见 free_id）。
    async fn create(&self, name: &str, home: &Path) -> Result<(), ApiError> {
        let (min, max) = (self.cfg.uid_min, self.cfg.uid_min + ID_SPAN);
        let existing = match std::fs::symlink_metadata(home) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(ApiError::internal(format!("查看 {} 失败：{e}", home.display()))),
        };
        let (uid, gid, create_home) = match existing {
            Some(meta) => {
                if !meta.file_type().is_dir() {
                    return Err(ApiError::internal(format!("{} 已存在但不是目录", home.display())));
                }
                let (uid, gid) = (meta.uid(), meta.gid());
                if !(min..=max).contains(&uid) || !(min..=max).contains(&gid) {
                    return Err(ApiError::new(
                        StatusCode::FORBIDDEN,
                        "namespace_reserved",
                        format!("{} 的属主 {uid}:{gid} 不在沙箱账号的范围内，不是沙箱建立的家目录", home.display()),
                    ));
                }
                if let Some(other) = User::from_uid(Uid::from_raw(uid)).map_err(query_failed)? {
                    return Err(ApiError::internal(format!(
                        "{} 的属主 UID {uid} 已被账号 {} 占用",
                        home.display(),
                        other.name
                    )));
                }
                (uid, gid, false)
            }
            None => {
                let id = self.free_id(min, max)?;
                (id, id, true)
            }
        };
        match Group::from_name(name).map_err(query_failed)? {
            Some(group) if group.gid.as_raw() == gid => {}
            Some(group) => {
                return Err(ApiError::internal(format!("组 {name} 已存在，但 GID 是 {} 而不是 {gid}", group.gid)));
            }
            None => {
                if let Some(other) = Group::from_gid(Gid::from_raw(gid)).map_err(query_failed)? {
                    return Err(ApiError::internal(format!("GID {gid} 已被组 {} 占用", other.name)));
                }
                run("groupadd", &["--gid", &gid.to_string(), name]).await?;
            }
        }
        let home_text = home.to_string_lossy();
        let home_flag = if create_home { "--create-home" } else { "--no-create-home" };
        run(
            "useradd",
            &[
                "--uid", &uid.to_string(), "--gid", &gid.to_string(), "--home-dir", &home_text, home_flag,
                "--shell", "/bin/bash", name,
            ],
        )
        .await?;
        // login.defs 的 HOME_MODE 因发行版而异，这里明确收成 0700：家目录互不可读是隔离的基础。
        std::fs::set_permissions(home, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .map_err(|e| ApiError::internal(format!("设置 {} 权限失败：{e}", home.display())))?;
        if create_home {
            println!("已建立命名空间账号 {name}（UID {uid}）");
        } else {
            println!("已按卷里现有的家目录重建命名空间账号 {name}（UID {uid}）");
        }
        Ok(())
    }

    /// 给新账号挑一个 ID（UID 与 GID 取同一个数）：在分配范围内，系统里没有这个 UID 与 GID，
    /// 也不是家目录根下任何目录的属主或属组。最后一条针对容器重建：旧家目录的账号要等它自己的请求来才重建，
    /// 在那之前新账号不能先占走它的 UID，否则新账号就成了别人家目录的属主，读得到别人的文件。
    fn free_id(&self, min: u32, max: u32) -> Result<u32, ApiError> {
        let mut reserved = HashSet::new();
        let entries = std::fs::read_dir(&self.cfg.home_root)
            .map_err(|e| ApiError::internal(format!("读 {} 失败：{e}", self.cfg.home_root.display())))?;
        for entry in entries.flatten() {
            if let Ok(meta) = entry.path().symlink_metadata() {
                reserved.insert(meta.uid());
                reserved.insert(meta.gid());
            }
        }
        // 查询出错时按“已占用”处理：宁可跳过一个号，也不冒险复用。
        let taken = |id: u32| {
            User::from_uid(Uid::from_raw(id)).map(|u| u.is_some()).unwrap_or(true)
                || Group::from_gid(Gid::from_raw(id)).map(|g| g.is_some()).unwrap_or(true)
        };
        pick_id(min, max, &reserved, taken)
            .ok_or_else(|| ApiError::internal(format!("命名空间账号的 ID（{min}～{max}）已用完")))
    }
}

/// 账号 ID 的分配跨度：从 uid_min 起五万个，足够多，也远离 nobody（65534）。
const ID_SPAN: u32 = 50_000;

/// 在 [min, max] 里找第一个既不在 reserved 里、也没被 taken 判为已占用的 ID。
fn pick_id(min: u32, max: u32, reserved: &HashSet<u32>, taken: impl Fn(u32) -> bool) -> Option<u32> {
    (min..=max).find(|id| !reserved.contains(id) && !taken(*id))
}

fn lookup(name: &str) -> Result<Option<User>, ApiError> {
    User::from_name(name).map_err(|e| ApiError::internal(format!("查账号 {name} 失败：{e}")))
}

fn query_failed(e: nix::errno::Errno) -> ApiError {
    ApiError::internal(format!("查系统账号表失败：{e}"))
}

/// 运行一个管理命令（groupadd / useradd），失败时把 stderr 带进错误。
async fn run(program: &str, args: &[&str]) -> Result<(), ApiError> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| ApiError::internal(format!("运行 {program} 失败：{e}")))?;
    if !output.status.success() {
        return Err(ApiError::internal(format!(
            "{program} {} 失败（{}）：{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 新账号的_id_避开旧家目录与已占用的号() {
        // 旧家目录占着 10000 与 10002，系统里 10001 已被占用：新账号拿 10003。
        let reserved: HashSet<u32> = [0, 10000, 10002].into_iter().collect();
        assert_eq!(pick_id(10000, 10010, &reserved, |id| id == 10001), Some(10003));
        assert_eq!(pick_id(10000, 10000, &reserved, |_| false), None, "范围用完");
        assert_eq!(pick_id(10000, 10010, &HashSet::new(), |_| false), Some(10000));
    }

    #[test]
    fn 账号名只认小写字母数字连字符() {
        assert!(valid_name("user-0q7cx1gpb9y6") && valid_name("team-abc") && valid_name("a"));
        for bad in ["", "-a", "User", "a_b", "a.b", "a b", "a:b", &"x".repeat(33)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }
}
