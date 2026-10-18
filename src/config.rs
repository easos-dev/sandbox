//! 运行配置：全部来自环境变量，启动时读一次。密钥支持 Docker secrets 惯用的 `<变量>_FILE` 形式。

use anyhow::{bail, Context, Result};
use std::net::SocketAddr;
use std::path::PathBuf;

/// 命令环境里 PATH 的默认值：只有系统目录。Dockerfile 把 Python 包装进系统目录，所以不需要额外的 venv 路径。
const DEFAULT_COMMAND_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// 运行器建立的命名空间账号从这个 UID 起分配：低于它的名字（root、daemon 等系统账号）即使撞名也拒绝使用。
const DEFAULT_UID_MIN: u32 = 10000;

/// 运行器的配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 监听地址（`SANDBOX_LISTEN`，默认 0.0.0.0:8000）；集成测试用 127.0.0.1:0 让系统分配端口。
    pub listen: SocketAddr,
    /// 调用方的密钥（`SANDBOX_API_KEY` 或 `SANDBOX_API_KEY_FILE`）；绝不进入命令环境。
    pub api_key: String,
    /// 命名空间账号的家目录根（`SANDBOX_HOME_ROOT`，默认 /home）：账号 `a` 的家是 `<根>/a`。
    pub home_root: PathBuf,
    /// 执行记录目录（`SANDBOX_STATE_DIR`，默认 `<家目录根>/.sandbox`）；归运行器自己，命令账号读不到。
    pub state_dir: PathBuf,
    /// 单用户模式（`SANDBOX_SINGLE_USER=1`）：不建系统账号、不切换账号，一切以当前用户执行；只用于本机开发与测试。
    pub single_user: bool,
    /// 命令环境里 PATH 的系统部分（`SANDBOX_COMMAND_PATH`）；运行器会在它前面加上账号的 `~/.local/bin`。
    pub command_path: String,
    /// 额外给每条命令设的环境变量（`SANDBOX_COMMAND_ENV`，分号或换行分隔的 `名=值`），例如镜像里 Node 全局包的
    /// NODE_PATH。只放与镜像布局有关的固定值，绝不放密钥；运行器固定设的 HOME、USER、PATH 等不能被它覆盖。
    pub command_env: Vec<(String, String)>,
    /// 命名空间账号的最小 UID（`SANDBOX_UID_MIN`，默认 10000）。
    pub uid_min: u32,
    /// 执行命令用的 shell（`SANDBOX_SHELL`，默认 /bin/bash），以 `-c` 方式执行。
    pub shell: PathBuf,
}

impl Config {
    /// 从环境变量读配置；缺密钥或值不合法时启动失败，不带默认密钥运行。
    pub fn from_env() -> Result<Self> {
        let listen = std::env::var("SANDBOX_LISTEN").unwrap_or_else(|_| "0.0.0.0:8000".into());
        let listen: SocketAddr = listen.parse().with_context(|| format!("SANDBOX_LISTEN 不是合法地址：{listen}"))?;
        let api_key = env_or_file("SANDBOX_API_KEY")?.unwrap_or_default();
        if api_key.is_empty() {
            bail!("缺少 SANDBOX_API_KEY（或 SANDBOX_API_KEY_FILE）");
        }
        let home_root = PathBuf::from(std::env::var("SANDBOX_HOME_ROOT").unwrap_or_else(|_| "/home".into()));
        let state_dir = std::env::var("SANDBOX_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_root.join(".sandbox"));
        let single_user = flag("SANDBOX_SINGLE_USER");
        let command_path = std::env::var("SANDBOX_COMMAND_PATH").unwrap_or_else(|_| DEFAULT_COMMAND_PATH.into());
        let command_env = parse_command_env(&std::env::var("SANDBOX_COMMAND_ENV").unwrap_or_default())?;
        let uid_min = match std::env::var("SANDBOX_UID_MIN") {
            Ok(text) => text.parse().with_context(|| format!("SANDBOX_UID_MIN 不是整数：{text}"))?,
            Err(_) => DEFAULT_UID_MIN,
        };
        let shell = PathBuf::from(std::env::var("SANDBOX_SHELL").unwrap_or_else(|_| "/bin/bash".into()));
        Ok(Self {
            listen,
            api_key,
            home_root,
            state_dir,
            single_user,
            command_path,
            command_env,
            uid_min,
            shell,
        })
    }
}

/// 解析 `SANDBOX_COMMAND_ENV`：分号或换行分隔的 `名=值`，空项忽略。名字只接受大写字母、数字与下划线且不以数字开头；
/// 以 `SANDBOX_` 开头的名字拒绝，免得把运行器自己的配置（含密钥文件路径）带进命令环境。
fn parse_command_env(text: &str) -> Result<Vec<(String, String)>> {
    let mut vars = Vec::new();
    for item in text.split([';', '\n']).map(str::trim).filter(|s| !s.is_empty()) {
        let Some((name, value)) = item.split_once('=') else {
            bail!("SANDBOX_COMMAND_ENV 的 \"{item}\" 不是 名=值");
        };
        let valid = !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            && !name.as_bytes()[0].is_ascii_digit();
        if !valid || name.starts_with("SANDBOX_") {
            bail!("SANDBOX_COMMAND_ENV 的变量名 \"{name}\" 不合法（只能大写字母、数字、下划线，不能以 SANDBOX_ 开头）");
        }
        vars.push((name.to_owned(), value.to_owned()));
    }
    Ok(vars)
}


/// 布尔开关：`1`、`true`、`yes`（不分大小写）为开。
fn flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// 读 `<变量>` 或 `<变量>_FILE`（文件内容去掉首尾空白）；两者都设视为配置错误，与官方 PostgreSQL 镜像的约定一致。
fn env_or_file(name: &str) -> Result<Option<String>> {
    let direct = std::env::var(name).ok().filter(|v| !v.is_empty());
    let file = std::env::var(format!("{name}_FILE")).ok().filter(|v| !v.is_empty());
    match (direct, file) {
        (Some(_), Some(_)) => bail!("{name} 与 {name}_FILE 只能设一个"),
        (Some(value), None) => Ok(Some(value)),
        (None, Some(path)) => {
            let text = std::fs::read_to_string(&path).with_context(|| format!("读 {name}_FILE 失败：{path}"))?;
            Ok(Some(text.trim().to_owned()))
        }
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 命令额外环境变量的解析() {
        assert_eq!(
            parse_command_env("NODE_PATH=/usr/local/lib/node_modules; A_1=x=y\n\n").unwrap(),
            vec![("NODE_PATH".into(), "/usr/local/lib/node_modules".into()), ("A_1".into(), "x=y".into())]
        );
        assert!(parse_command_env("").unwrap().is_empty());
        for bad in ["NOVALUE", "lower=1", "1A=1", "SANDBOX_API_KEY=x", "=x", "A B=1"] {
            assert!(parse_command_env(bad).is_err(), "{bad}");
        }
    }
}
