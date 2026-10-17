# EasOS 沙箱运行器

EasOS 助手（`mods/assistant`）里模型的"电脑"：跑命令、放附件、出产物。它替代原来的外部服务 Open Terminal，
是一个独立工程：一个 Rust 守护进程（沙箱运行器）加一个 Docker 镜像（Debian + Python 数据栈 + LibreOffice + 中日文字体）。

设计原则只有一条：**文件字节只在浏览器、对象存储与沙箱三者之间流动，助手后端与运行器之间只传 JSON。**
所以运行器没有读文件、写文件、列目录、取字节的接口；文件靠"按地址下载进工作区"与"按上传计划 PUT 出去"两个原语进出。
整体方案见 EasOS 主检出 `.checks/assistant-file-flow-20261018/方案.md` 第 5 节。

## 职责与语义

- **命名空间 = 系统账号。** 每个请求带 `X-User-Id`（小写字母、数字、连字符，≤ 32 字节），对应容器里一个同名系统账号，首次出现时建立：
  无 sudo、家目录 0700、互不可读。命令以该账号执行，新建的文件归它。与 Open Terminal 多用户模式的语义相同。
- **会话目录** `~/conversations/<会话 ID>/`，接口里的路径一律相对它，规范化后拒绝 `..`、绝对路径与反斜杠；
  运行器自己打开文件时每一级都不跟随符号链接，命令摆不出能让运行器读到会话目录之外的路径。
- **命令**：`bash -c`，工作目录是会话目录，环境只有 HOME、USER、LOGNAME、SHELL、PATH、LANG、TERM、PYTHONUNBUFFERED、MPLBACKEND；
  运行器的密钥与任何 EasOS 凭证都不在里面。单条最长 600 秒，超时按进程组先 TERM 两秒后 KILL。
  标准输出与错误合并成一条流，完整写进会话目录 `out/<执行号>.txt`，状态接口只给最后 16 KiB 并标明是否截断。
  执行记录另存一份小 JSON；运行器重启后凭执行号仍能查到"已结束（退出码）"或"未知（重启时还在跑）"，不恢复进行中的命令。
- **文件接口**（全部 JSON）：下载到工作区、算指纹（大小、MIME、MD5、内容 ETag、CRC64-ECMA/NVME、分片 MD5）、
  算秒传持有证明（HMAC-SHA256）、按计划逐段 PUT、删整个会话目录。每个接口的形状与错误码见 [doc/接口.md](doc/接口.md)。
- **网络**：允许公网。命令账号默认拒绝私网、链路本地与容器自身网段（保护数据库、ES 等内部服务），也禁止访问本机上运行器的端口；
  运行器自己不受限（它只连助手交来的地址，其中可能有内网对象存储）。规则由 entrypoint 用 iptables 按 uid 配置，配完永久收回 NET_ADMIN。

## 目录

| 位置 | 内容 |
| --- | --- |
| `src/main.rs` | 启动：读配置、监听、分发连接、优雅退出 |
| `src/config.rs` | 环境变量配置 |
| `src/server.rs` | hyper HTTP/1.1 服务、路由、统一错误 `{error, message}` |
| `src/auth.rs` | Bearer 密钥核对（常量时间） |
| `src/account.rs` | 命名空间账号的建立与查找；单用户模式 |
| `src/paths.rs` | 会话 ID 与相对路径校验；不跟随符号链接的逐级打开、临时文件写入 |
| `src/exec.rs` | 命令执行、输出尾部、长轮询、停止/超时、执行记录 |
| `src/files.rs` | download / stat / prove / upload / 删会话 |
| `src/checksum.rs`、`src/mime.rs` | 指纹与 MIME 推断的纯计算 |
| `tests/local.rs` | 不依赖 Docker 的集成测试（单用户模式起真进程） |
| `Dockerfile`、`entrypoint.sh` | 镜像与入口（网络规则） |
| `doc/接口.md` | 接口文档 |

## 配置项（环境变量）

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `SANDBOX_API_KEY` / `SANDBOX_API_KEY_FILE` | 必填 | 调用方密钥，`Authorization: Bearer <密钥>`；两者只能设一个 |
| `SANDBOX_LISTEN` | `0.0.0.0:8000` | 监听地址 |
| `SANDBOX_HOME_ROOT` | `/home` | 命名空间账号的家目录根；镜像里把它做成卷 |
| `SANDBOX_STATE_DIR` | `<家目录根>/.sandbox` | 执行记录目录（0700，归运行器） |
| `SANDBOX_ALLOW_PRIVATE` | `0` | `1` 时命令账号可访问私网（entrypoint 读） |
| `SANDBOX_COMMAND_PATH` | 系统目录 | 命令环境里的 PATH |
| `SANDBOX_UID_MIN` | `10000` | 命名空间账号从这个 UID 起分配；更低的名字视为系统账号拒绝 |
| `SANDBOX_SHELL` | `/bin/bash` | 执行命令的 shell |
| `SANDBOX_SINGLE_USER` | `0` | `1` 时不建账号、不切换，以当前用户执行；只用于本机开发与测试 |

## 部署（Docker）

```sh
docker build -t easos-sandbox .
docker run -d --name easos-sandbox --restart unless-stopped \
  --network easos-db -p 127.0.0.1:8003:8000 \
  --cap-add NET_ADMIN \
  -v easos-sandbox-home:/home \
  -e SANDBOX_API_KEY=<密钥> \
  --memory 4g --cpus 2 \
  easos-sandbox
```

- `--cap-add NET_ADMIN` 只在启动时用来写 iptables 规则，入口脚本随即用 `capsh` 把它从 bounding set 里去掉；没有这个权限时只打警告照常启动，命令账号的网络不受限。
- 容器以 root 跑运行器（要 `useradd` 与切换账号），`tini` 做 PID 1 回收孤儿进程。
- 助手配置里填 `http://easos-sandbox:8000` 与密钥。三个 EasOS 环境共用一个沙箱时和 Open Terminal 的接法一样：挂到 `easos-db` 网络，`scripts/env.py` 再把它接进两端环境的网络。
- 需要命令访问内网时加 `-e SANDBOX_ALLOW_PRIVATE=1`。

## 本机运行与测试

```sh
cargo test                      # 单元测试 + 集成测试（集成测试以单用户模式起真进程，不需要 Docker 与 root）
SANDBOX_SINGLE_USER=1 SANDBOX_API_KEY=dev SANDBOX_HOME_ROOT=/tmp/sandbox-home SANDBOX_LISTEN=127.0.0.1:8000 cargo run
curl -H 'Authorization: Bearer dev' -H 'X-User-Id: user-dev' -d '{"conversation_id":"c1","command":"python3 -c \"print(1+1)\""}' 'http://127.0.0.1:8000/execute?wait=10'
```

单用户模式下账号目录就是 `<家目录根>/<X-User-Id>`，命令以当前用户执行；路径与符号链接的检查照常生效。

镜像建好后用 `python3 scripts/smoke.py [镜像名]` 冒烟：起容器，核对 Python 数据栈、账号隔离、指纹、公网可达与私网拒绝、
NET_ADMIN 已收回、LibreOffice 转 PDF、超时与删会话，打印镜像大小与各步耗时，最后删容器。

## 边界与未做

- 用户隔离之外的加固（seccomp、资源配额、进程数上限）未做；命令用 `setsid` 脱离进程组的后代不会被超时或停止波及。
- 输出文件 `out/<执行号>.txt` 与工作区不自动清理；执行记录随 `DELETE /conversations/{id}` 一起删除，不另设定时清理。
- 没有文件面板接口、没有 WebSocket；运行器不对接 File，只认助手交来的地址与上传计划。
- 镜像不含 LaTeX 与 Node（Open Terminal 完整镜像有）；Python 是 Debian trixie 的 3.13。
