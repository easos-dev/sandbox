# EasOS 沙箱运行器

EasOS 助手（`mods/assistant`）里模型的"电脑"：跑命令、放附件、出产物。它替代原来的外部服务 Open Terminal，
是一个独立工程：一个 Rust 守护进程（沙箱运行器）加一个 Docker 镜像（Debian + Python 数据栈 + LibreOffice + 中日文字体）。

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
- **命令**：`bash -c`，工作目录是会话目录，环境只有 HOME、USER、LOGNAME、SHELL、PATH、LANG、TERM、PYTHONUNBUFFERED、MPLBACKEND；
  运行器的密钥与任何 EasOS 凭证都不在里面。单条最长 600 秒，超时按进程组先 TERM 两秒后 KILL。
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
| `image/` | 装进镜像的配置：字体名映射 `fonts.conf`、`matplotlibrc`、`pip.conf`；构建时拆可变字体的 `static-fonts.py` |
| `scripts/smoke.py`、`scripts/tools-check.sh` | 镜像冒烟；镜像工具逐项核对 |
| `scripts/font-specimen.py` | 在容器里生成字体样张 PDF |
| `doc/接口.md` | 接口文档 |

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

基础是 Debian trixie，Python 3.13、Node 20，镜像约 4.75 GB（系统包 2 GB、Python 包 1.5 GB、设计用字体 0.75 GB、Node 包 0.2 GB）。
下面的工具都已预装，命令里直接用；`scripts/tools-check.sh` 逐项真用一遍核对。

| 类别 | 命令行 | Python | Node |
| --- | --- | --- | --- |
| PDF | poppler（pdftotext、pdftoppm、pdfimages、pdffonts、pdfinfo）、qpdf、ghostscript、mutool | pypdf、pdfplumber、PyMuPDF（fitz）、pikepdf、pdf2image、reportlab、fpdf2、weasyprint（HTML 转 PDF）、img2pdf | pdf-lib |
| OCR | tesseract（官方高精度模型：简体、繁体、日文横排与竖排、英文）、ocrmypdf（给扫描 PDF 加文字层） | ocrmypdf | |
| Office | LibreOffice Writer/Calc/Impress/Draw（`soffice --headless --convert-to`）、pandoc | openpyxl、xlsxwriter、xlrd、pyxlsb、odfpy、python-docx、docxtpl、python-pptx、mammoth、msoffcrypto-tool、xlsx2csv、uno（驱动 LibreOffice） | pptxgenjs、docx、exceljs |
| 图片与音视频 | ImageMagick（magick）、vips、ffmpeg、exiftool、cwebp、optipng、pngquant、jpegoptim、rsvg-convert、graphviz（dot） | Pillow、pillow-heif、OpenCV（headless）、scikit-image、imageio、cairosvg、qrcode、pyzbar、graphviz | sharp |
| 数据分析 | sqlite3、csvkit（in2csv、csvsql、csvstat）、jq | pandas、numpy、scipy、scikit-learn、statsmodels、polars、pyarrow、duckdb、sympy、networkx、matplotlib、seaborn、plotly | csv-parse、csv-stringify |
| 文本与网络 | ripgrep（rg）、fd（fdfind）、tree、file、curl、wget、git | requests、httpx、beautifulsoup4、lxml、html5lib、markdown、jinja2、pyyaml、tabulate、chardet、python-dateutil、tqdm、jieba、wordcloud | marked、typescript、tsx |
| 压缩 | zip/unzip、7z、xz、bzip2、zstd、unar（含 rar） | | |

**字体**（共 371 个字族；`scripts/font-specimen.py` 在容器里生成按语言分组的样张 PDF，挑字体时看它）：

| 用途 | 中文 | 日文 | 英文 |
| --- | --- | --- | --- |
| 正文与标题 | 思源黑体 / 思源宋体（Noto Sans/Serif CJK SC，Thin～Black 七档字重）、霞鹜文楷、朱雀仿宋（国标字形，公文）、文鼎 PL 楷体与明体、文泉驿 | 源ノ角ゴシック / 源ノ明朝、BIZ UD ゴシック / 明朝、IPAex、Zen 角ゴシック、Zen 丸ゴシック、Zen オールド明朝、しっぽり明朝、M PLUS 1 / Rounded 1c、さわらび、モトヤ | 无衬线：Inter、Source Sans 3、IBM Plex Sans、Poppins、Montserrat、Raleway、Nunito、Work Sans、DM Sans、Manrope、Quicksand、Lato、Open Sans、Roboto；衬线：Playfair Display、Merriweather、Lora、Source Serif 4、Libre Baskerville、Crimson Pro、EB Garamond、Linux Libertine、Vollkorn、IBM Plex Serif、Roboto Slab、TeX Gyre 全套 |
| 展示与书法 | 得意黑、站酷小薇、站酷庆科黄油体、站酷快乐体、马善政毛笔楷书、龙藏体 | Klee One（手书风）、Dela Gothic One | Oswald、Bebas Neue、League Spartan |
| 度量兼容 Office 字体 | | | Liberation（Arial / Times New Roman / Courier New）、Carlito（Calibri）、Caladea（Cambria）、Gelasio（Georgia）、Comic Neue（Comic Sans） |
| 等宽与其他 | Noto Sans Mono CJK | | JetBrains Mono、Fira Code、IBM Plex Mono、DejaVu Sans Mono；数学 STIX、Latin Modern；彩色 Emoji Noto Color Emoji |

- **来源**：大部分是 Debian 官方包；Debian 没有的取自 Google Fonts 官方仓库（固定提交，git 按对象哈希校验）与朱雀仿宋的发布页（按 SHA-256 校验），都是 OFL 许可。
  Google Fonts 的可变字体由 `image/static-fonts.py` 拆成各字重的静态字体：reportlab、fpdf2、matplotlib 只认静态字体的字重。
- **朱雀仿宋改了族名**：原名带“(technical preview)”，weasyprint 拿族名拼 PDF 里的字体名时不转义括号，生成的 PDF 语法不合法。
  构建时改成 `Zhuque Fangsong`（OFL 1.1，没有声明保留字体名，允许改名）。
- **字体名映射**（`image/fonts.conf`）：Windows / macOS 文档里的宋体、微软雅黑、楷体、仿宋、新細明體、標楷體、ＭＳ 明朝、メイリオ、游ゴシック、맑은 고딕、
  Georgia、Garamond、Palatino、Century Gothic 等字体名，映射到上面字形与用途最接近的字体，LibreOffice 转出的 PDF 不会乱换字体；
  公文常用的方正小标宋映射到思源宋体粗体，方正仿宋到朱雀仿宋。没指定语言的汉字默认用简体字形。
- **matplotlib 默认就能画中文**：`image/matplotlibrc` 装到 `/etc/matplotlibrc`，运行器经 `SANDBOX_COMMAND_ENV` 设 `MATPLOTLIBRC` 指向它。
  matplotlib 只在 `font.family` 列出的字体之间回退，所以那里写的是 `sans-serif, Noto Sans CJK SC`；画衬线中文时改成 `serif, Noto Serif CJK SC`。
- **OCR 用官方高精度模型**（tessdata_best 4.1.0，构建时按 SHA-256 校验下载）：Debian 自带的是精简版，日文“稼働率”会认错。
  tesseract 默认的版面分析面向整页，只有一行字的小图要加 `--psm 7`。
- **HTML 转 PDF 要声明编码**：weasyprint 读本地 HTML 文件时，没有 `<meta charset="utf-8">` 就按 windows-1252 解码，中文成乱码。
- **Node 全局包直接 `require`**：运行器经 `SANDBOX_COMMAND_ENV` 给命令设了 `NODE_PATH`。
- **模型可以自己装包**：`pip install x` 会自动装到账号的 `~/.local`（`/etc/pip.conf` 放开了 PEP 668 的限制），`~/.local/bin` 在 PATH 最前面；
  `npm install x` 装在当前目录。两者都要访问公网。

没装的：LaTeX（XeLaTeX 加中日文支持装好后约 1.0 GB，完整版 8.4 GB；Markdown 转 PDF 用 `pandoc --pdf-engine=weasyprint`）、
Java（tabula、pdftk 不可用，用 pdfplumber、qpdf 代替）、浏览器内核（plotly 只能出 HTML，静态图用 matplotlib）。

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

镜像建好后用 `python3 scripts/smoke.py [镜像名]` 冒烟：起容器，核对 Python 数据栈、账号隔离、指纹、公网可达与默认拒绝清单、
密钥文件不可读、NET_ADMIN 已收回、LibreOffice 转 PDF、超时、容器重建后账号沿用原 UID、删会话，另起容器核对拒绝清单设空与写错、
密钥文件挂错位置；打印镜像大小与各步耗时，最后删掉容器、临时卷与临时密钥文件。

## 边界与未做

- 用户隔离之外的加固（seccomp、资源配额、进程数上限）未做；命令用 `setsid` 脱离进程组的后代不会被超时或停止波及。
- 输出文件 `out/<执行号>.txt` 与工作区不自动清理；执行记录随 `DELETE /conversations/{id}` 一起删除，不另设定时清理。
- 没有文件面板接口、没有 WebSocket；运行器不对接 File，只认助手交来的地址与上传计划。
- 镜像不含 LaTeX、Java 与浏览器内核，见“镜像里的工具”最后一段。
