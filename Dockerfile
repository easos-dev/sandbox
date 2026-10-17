# 沙箱镜像：两个阶段。
#   build  编译运行器。与运行阶段同一个 Debian 版本（trixie），glibc 对齐，二进制直接拷过去就能跑。
#   运行   模型的“电脑”：Debian + Python 数据栈 + LibreOffice + 中日文字体，外加网络隔离要用的 iptables/capsh。
#          以 root 启动运行器（它要 useradd 与切换账号）；tini 做 PID 1 转发信号、回收孤儿进程。
# 构建（上下文就是本目录，.dockerignore 排除 target 与文档）：
#   docker build -t easos-sandbox .
# 运行方式见 README.md。

FROM rust:1.97-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# --locked：Cargo.lock 随仓库提交，镜像里装的依赖版本与本机测试过的完全相同。
RUN cargo build --release --locked

FROM debian:trixie-slim
ENV DEBIAN_FRONTEND=noninteractive
# 运行时软件清单（与 Open Terminal 完整镜像的常用部分对齐，不含 LaTeX 与 Node）：
#   基础工具      bash curl ca-certificates procps jq unzip zip git
#   网络隔离      iptables（出站规则）libcap2-bin（capsh 收回 NET_ADMIN）iproute2（读容器网段）
#   PID 1         tini
#   Python 数据栈 python3 + pip 装的包（下一层）
#   办公文档      LibreOffice 的 Calc/Writer/Impress（无界面转换 PDF/xlsx/docx），不装 Java
#   字体          fonts-noto-cjk 中日韩；fonts-liberation 与 Arial/Times 等度量兼容，DejaVu 兜底
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash curl ca-certificates procps jq unzip zip git \
      iptables libcap2-bin iproute2 tini \
      python3 python3-pip python3-venv \
      libreoffice-calc libreoffice-writer libreoffice-impress \
      fonts-noto-cjk fonts-liberation fonts-dejavu-core \
 && rm -rf /var/lib/apt/lists/*
# 直接装进系统 Python（容器里没有别的 Python 用户，不必 venv），这样命令里的 python3 就带数据栈。
RUN pip install --no-cache-dir --break-system-packages \
      pandas numpy openpyxl xlsxwriter xlrd matplotlib \
      python-docx python-pptx pypdf reportlab pillow \
      requests beautifulsoup4 lxml tabulate pyyaml
COPY --from=build /src/target/release/sandbox-runner /usr/local/bin/sandbox-runner
COPY entrypoint.sh /usr/local/bin/sandbox-entrypoint
RUN chmod 755 /usr/local/bin/sandbox-entrypoint /usr/local/bin/sandbox-runner
# /home 是全部命名空间账号的家目录与执行记录（/home/.sandbox）所在，挂卷持久化。
ENV SANDBOX_LISTEN=0.0.0.0:8000 \
    SANDBOX_HOME_ROOT=/home
VOLUME ["/home"]
EXPOSE 8000
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/sandbox-entrypoint"]
