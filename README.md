# EasOS 沙箱运行器

EasOS 助手（`mods/assistant`）里模型的"电脑"：跑命令、放附件、出产物。它替代原来的外部服务 Open Terminal，
是一个独立工程：一个 Rust 守护进程（沙箱运行器）加一个 Docker 镜像（Debian 上的 PDF、OCR、Office、图片音视频、数据分析、LaTeX、
Python 与 Node 常用库，以及中日英字体库）。镜像里有什么、怎么用，见 [doc/镜像工具.md](doc/镜像工具.md)，它也装进镜像供沙箱里的 AI 读取。

设计原则只有一条：**文件字节只在浏览器、对象存储与沙箱三者之间流动，助手后端与运行器之间只传 JSON。**
所以运行器没有读文件、写文件、列目录、取字节的接口；文件靠"按地址下载进工作区"与"按上传计划 PUT 出去"两个原语进出。
整体方案见 EasOS 主检出 `.checks/assistant-file-flow-20261018/方案.md` 第 5 节。

## 职责与语义

- **命名空间 = 系统账号。** 每个请求带 `X-User-Id`（小写字母、数字、连字符，≤ 32 字节），对应容器里一个同名系统账号，首次出现时建立：
  无 sudo、家目录 0700、互不可读。命令以该账号执行，新建的文件归它。与 Open Terminal 多用户模式的语义相同。
  账号表在容器层里，家目录在卷里：容器重建后，账号在它下一次请求时按卷里家目录原来的 UID/GID 重建；
  新账号分配 ID 时避开所有现存家目录的属主，不会抢走别人的 UID。
- **会话目录** `~/conversations/<会话 ID>/`，接口里的路径一律相对它，规范化后拒绝 `..`、绝对路径与反斜杠；
  运行器自己打开文件时每一级都不跟随符号链接，命令摆不出能让运行器读到会话目录之外的路径。
- **命令**：`bash -c`，工作目录是会话目录，环境只有 HOME、USER、LOGNAME、SHELL、PATH（账号的 `~/.local/bin` 在最前）、LANG、TERM、
  PYTHONUNBUFFERED、MPLBACKEND，以及 `SANDBOX_COMMAND_ENV` 配的变量（镜像里是 NODE_PATH、MATPLOTLIBRC）；运行器的密钥与任何 EasOS 凭证都不在里面。单条最长 600 秒，超时按进程组先 TERM 两秒后 KILL。
  标准输出与错误合并成一条流，完整写进会话目录 `out/<执行号>.txt`，状态接口只给最后 16 KiB 并标明是否截断。
  执行记录另存一份小 JSON；运行器重启后凭执行号仍能查到"已结束（退出码）"或"未知（重启时还在跑）"，不恢复进行中的命令。
- **文件接口**（全部 JSON）：下载到工作区、算指纹（大小、MIME、MD5、内容 ETag、CRC64-ECMA/NVME、分片 MD5）、
  算秒传持有证明（HMAC-SHA256）、按计划逐段 PUT、删整个会话目录。每个接口的形状与错误码见 [doc/接口.md](doc/接口.md)。
- **网络**：允许公网。命令账号拒绝 `SANDBOX_DENY_NETWORKS` 里的网段（默认是私网、运营商 NAT、链路本地、33 段与保留段，
  数据库、ES 等内部服务和 Docker 分配的容器网段都在其中），也禁止访问本机上运行器的端口；其余回环流量放行，模型自己起的本地服务可用。
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
| `image/` | 装进镜像的配置：字体名映射 `fonts.conf`、`matplotlibrc`、`pip.conf`；构建时拆可变字体的 `static-fonts.py`；`tex/` 是 ctex 默认字库 |
| `scripts/smoke.py`、`scripts/tools-check.sh` | 镜像冒烟；镜像工具逐项核对（按工具文档的清单，也装进镜像） |
| `scripts/font-specimen.py` | 生成字体样张 PDF（也装进镜像） |
| `doc/接口.md` | 接口文档（给助手后端的开发者） |
| `doc/镜像工具.md` | 镜像里的工具与用法（给沙箱里的 AI；装到镜像的 `/usr/local/share/sandbox/TOOLS.md`），文末清单由冒烟核对 |

## 配置项（环境变量）

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `SANDBOX_API_KEY` / `SANDBOX_API_KEY_FILE` | 必填 | 调用方密钥，`Authorization: Bearer <密钥>`；两者只能设一个 |
| `SANDBOX_LISTEN` | `0.0.0.0:8000` | 监听地址 |
| `SANDBOX_HOME_ROOT` | `/home` | 命名空间账号的家目录根；镜像里把它做成卷 |
| `SANDBOX_STATE_DIR` | `<家目录根>/.sandbox` | 执行记录目录（0700，归运行器） |
| `SANDBOX_DENY_NETWORKS` | 见下 | 命令账号拒绝访问的网段，逗号分隔的 CIDR，IPv4 与 IPv6 可混写；设成空串表示不拒绝任何网段（运行器端口仍禁止）；格式不对时容器拒绝启动（entrypoint 读） |
| `SANDBOX_COMMAND_PATH` | 系统目录 | 命令环境里 PATH 的系统部分；运行器在它前面加上账号的 `~/.local/bin` |
| `SANDBOX_COMMAND_ENV` | 镜像里为 `NODE_PATH=/usr/local/lib/node_modules;MATPLOTLIBRC=/etc/matplotlibrc` | 额外给每条命令设的环境变量，分号或换行分隔的 `名=值`；名字只能大写字母、数字、下划线，不能以 `SANDBOX_` 开头，不能覆盖 HOME、USER、PATH 等固定变量；绝不放密钥 |
| `SANDBOX_UID_MIN` | `10000` | 命名空间账号从这个 UID 起分配；更低的名字视为系统账号拒绝 |
| `SANDBOX_SHELL` | `/bin/bash` | 执行命令的 shell |
| `SANDBOX_SINGLE_USER` | `0` | `1` 时不建账号、不切换，以当前用户执行；只用于本机开发与测试 |

`SANDBOX_DENY_NETWORKS` 的默认值与助手读网页的内网拒绝清单一致，只去掉回环：

```
10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,100.64.0.0/10,169.254.0.0/16,33.0.0.0/8,0.0.0.0/8,224.0.0.0/4,240.0.0.0/4,fc00::/7,fe80::/10
```

Docker 默认只从 172.17～172.31 与 192.168 里分配网段，所以容器自身所在的网段不必另加。

## 镜像里的工具

基础是 Debian trixie，Python 3.13、Node 20，镜像约 5.8 GB（系统包 2 GB、Python 包 1.5 GB、LaTeX 1.1 GB、设计用字体 0.75 GB、Node 包 0.2 GB）。
**工具清单、用法与已知的坑只在 [doc/镜像工具.md](doc/镜像工具.md) 维护**：它装进镜像的 `/usr/local/share/sandbox/TOOLS.md`，助手的系统提示里应告诉模型去读它。
文末的清单（命令、系统包、Python 包与模块、Node 包、字族）由冒烟逐项核对镜像，并与 Dockerfile 的安装列表比对，改镜像时两边同步改。

镜像构建里几处不显然的做法（原因写在对应文件开头）：

- `image/fonts.conf`：Windows / macOS 文档里的中日英字体名映射到镜像里风格对应的字体。
- `image/static-fonts.py`：Google Fonts 的可变字体拆成各字重的静态字体（reportlab、fpdf2、matplotlib 只认静态字体的字重）。
- 朱雀仿宋构建时改族名，去掉“(technical preview)”：weasyprint 拿族名拼 PDF 字体名时不转义括号，PDF 语法不合法；许可证是 OFL 1.1 且没有保留字体名。
- `image/matplotlibrc`：matplotlib 默认能画中文，运行器经 `SANDBOX_COMMAND_ENV` 的 `MATPLOTLIBRC` 指向它。
- `image/tex/`：ctex 默认字库改成思源宋体/黑体、文鼎楷体、朱雀仿宋；最后一层预热 LuaLaTeX 字体缓存。
- OCR 模型用 tessdata_best，Google Fonts 固定提交、朱雀仿宋与 OCR 模型按 SHA-256 校验下载。

## 部署（Docker）

```sh
# 密钥只放在仓库之外、只有本人可读的文件里，不打印、不进命令行。
umask 077 && mkdir -p ~/.easos-sandbox && python3 -c 'import secrets;print(secrets.token_urlsafe(32),end="")' > ~/.easos-sandbox/key
docker build -t easos-sandbox .
docker run -d --name easos-sandbox --restart unless-stopped \
  --network easos-db -p 127.0.0.1:8003:8000 \
  --cap-add NET_ADMIN \
  -v easos-sandbox-home:/home \
  -v "$HOME/.easos-sandbox/key:/root/.sandbox-key:ro" \
  -e SANDBOX_API_KEY_FILE=/root/.sandbox-key \
  --memory 4g --cpus 2 \
  easos-sandbox
```

- **密钥文件必须挂在 `/root` 下。** Docker Desktop（macOS / Windows）的文件共享不执行宿主文件的权限位：挂进容器的 600 文件显示为 `root:root 600`，但任何 uid 都能读（2026-10-18 实测）。挂在 `/root` 下时，容器自己文件系统上 `/root` 的 700 挡住命令账号。入口脚本启动时以 nobody 身份试读密钥文件，读得到就拒绝启动。用 `-e SANDBOX_API_KEY=<密钥>` 也安全（命令账号读不到 root 进程的环境），只是值会出现在 `docker inspect` 里。
- `--cap-add NET_ADMIN` 只在启动时用来写 iptables 规则，入口脚本随即用 `capsh` 把它从 bounding set 里去掉；没有这个权限时只打警告照常启动，命令账号的网络不受限。
- 容器以 root 跑运行器（要 `useradd` 与切换账号），`tini` 做 PID 1 回收孤儿进程。
- 助手配置里填 `http://easos-sandbox:8000` 与密钥。三个 EasOS 环境共用一个沙箱时和 Open Terminal 的接法一样：挂到 `easos-db` 网络，`scripts/env.py` 再把它接进两端环境的网络。
- 要调整命令账号的拒绝清单时加 `-e SANDBOX_DENY_NETWORKS=<网段,…>`；放开全部网段用 `-e SANDBOX_DENY_NETWORKS=`。
- 升级镜像就是删掉容器再按上面的命令重起，卷保留：账号会在各自下一次请求时按卷里的家目录重建，UID 不变。

## 本机运行与测试

```sh
cargo test                      # 单元测试 + 集成测试（集成测试以单用户模式起真进程，不需要 Docker 与 root）
SANDBOX_SINGLE_USER=1 SANDBOX_API_KEY=dev SANDBOX_HOME_ROOT=/tmp/sandbox-home SANDBOX_LISTEN=127.0.0.1:8000 cargo run
curl -H 'Authorization: Bearer dev' -H 'X-User-Id: user-dev' -d '{"conversation_id":"c1","command":"python3 -c \"print(1+1)\""}' 'http://127.0.0.1:8000/execute?wait=10'
```

单用户模式下账号目录就是 `<家目录根>/<X-User-Id>`，命令以当前用户执行；路径与符号链接的检查照常生效。

镜像建好后用 `python3 scripts/smoke.py [镜像名]` 冒烟：先核对工具文档的清单与 Dockerfile 一致；再起容器，核对账号隔离、指纹、
公网可达与默认拒绝清单、密钥文件不可读、NET_ADMIN 已收回、超时、容器重建后账号沿用原 UID、删会话；以命名空间账号跑 `scripts/tools-check.sh`
（按文档清单逐项核对，并真用一遍 PDF、OCR、Office、图片、LaTeX、字体等）；另起容器核对拒绝清单设空与写错、密钥文件挂错位置。
最后删掉容器、临时卷与临时密钥文件。构建时如果卡在拉基础镜像，多半是 Docker 客户端的凭据助手挂住，用不含 credsStore 的临时 `DOCKER_CONFIG` 匿名拉。

## 边界与未做

- 用户隔离之外的加固（seccomp、资源配额、进程数上限）未做；命令用 `setsid` 脱离进程组的后代不会被超时或停止波及。
- 输出文件 `out/<执行号>.txt` 与工作区不自动清理；执行记录随 `DELETE /conversations/{id}` 一起删除，不另设定时清理。
- 没有文件面板接口、没有 WebSocket；运行器不对接 File，只认助手交来的地址与上传计划。
- 镜像不含 Java、浏览器内核与完整版 TeX Live，替代办法见 [doc/镜像工具.md](doc/镜像工具.md)“没有的东西”。
