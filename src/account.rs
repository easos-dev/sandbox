//! 命名空间账号：请求头 `X-User-Id` 的值就是系统账号名，首次出现时建账号（无 sudo、家目录 0700、互不可读），
//! 之后命令与新建文件都归它。语义与 Open Terminal 的多用户模式一致，助手客户端不用改命名方式。
//! 单用户模式不建账号、不切换，只在家目录根下建同名目录，供本机开发与集成测试。

use crate::config::Config;
use crate::server::ApiError;
use crate::paths::Owner;
use http::StatusCode;
use std::collections::HashMap;
use std::path::PathBuf;
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
                self.useradd(name, &home).await?;
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

    /// 建系统账号：自带同名组、家目录 0700、登录 shell 为 bash；UID/GID 从 uid_min 起分配，与系统账号区分。
    async fn useradd(&self, name: &str, home: &std::path::Path) -> Result<(), ApiError> {
        let min = self.cfg.uid_min;
        let max = min + 50_000;
        let output = tokio::process::Command::new("useradd")
            .args(["--create-home", "--home-dir"])
            .arg(home)
            .args(["--shell", "/bin/bash", "--user-group"])
            .args(["--key", &format!("UID_MIN={min}"), "--key", &format!("UID_MAX={max}")])
            .args(["--key", &format!("GID_MIN={min}"), "--key", &format!("GID_MAX={max}")])
            .arg(name)
            .output()
            .await
            .map_err(|e| ApiError::internal(format!("运行 useradd 失败：{e}")))?;
        if !output.status.success() {
            return Err(ApiError::internal(format!(
                "useradd {name} 失败（{}）：{}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        // login.defs 的 HOME_MODE 因发行版而异，这里明确收成 0700：家目录互不可读是隔离的基础。
        std::fs::set_permissions(home, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .map_err(|e| ApiError::internal(format!("设置 {} 权限失败：{e}", home.display())))?;
        println!("已建立命名空间账号 {name}");
        Ok(())
    }
}

fn lookup(name: &str) -> Result<Option<nix::unistd::User>, ApiError> {
    nix::unistd::User::from_name(name).map_err(|e| ApiError::internal(format!("查账号 {name} 失败：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 账号名只认小写字母数字连字符() {
        assert!(valid_name("user-0q7cx1gpb9y6") && valid_name("team-abc") && valid_name("a"));
        for bad in ["", "-a", "User", "a_b", "a.b", "a b", "a:b", &"x".repeat(33)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }
}
