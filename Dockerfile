# 沙箱镜像：两个阶段。
#   build  编译运行器。与运行阶段同一个 Debian 版本（trixie），glibc 对齐，二进制直接拷过去就能跑。
#   运行   模型的“电脑”：PDF、Office、图片与音视频、数据分析、Python 与 Node 常用库、中日韩为主的字体库，
#          外加网络隔离要用的 iptables/capsh。以 root 启动运行器（它要 useradd 与切换账号）；tini 做 PID 1。
# 层按变动频率排：系统包与字体 → Node 包 → Python 包 → 配置 → 运行器，只改运行器时前面的大层都走缓存。
# 构建（上下文就是本目录，.dockerignore 排除 target 与文档）：
#   docker build -t easos-sandbox .
# 工具清单与用法见 README.md。

FROM rust:1.97-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# --locked：Cargo.lock 随仓库提交，镜像里装的依赖版本与本机测试过的完全相同。
RUN cargo build --release --locked

FROM debian:trixie-slim
ENV DEBIAN_FRONTEND=noninteractive
# 系统软件，按用途分组（都是 Debian trixie 官方包）：
#   基础与网络隔离  bash curl wget ca-certificates procps jq git tzdata tini iptables libcap2-bin（capsh 收回 NET_ADMIN）
#   命令行工具      ripgrep fd-find tree file less bc gawk sqlite3
#   压缩            zip unzip p7zip-full xz-utils bzip2 zstd unar（含 rar 解压）
#   PDF             poppler-utils（pdftotext/pdftoppm/pdfimages/pdffonts）qpdf ghostscript mupdf-tools（mutool）
#   OCR             tesseract-ocr（自带英文与版面方向模型），unpaper（ocrmypdf 清理扫描件）；中日文模型见下一层
#   Office          LibreOffice 的 Writer/Calc/Impress/Draw（无界面转换与渲染），python3-uno（用 Python 驱动 LibreOffice），
#                   pandoc（Markdown / HTML / docx / epub 互转）
#   图片与音视频    imagemagick libvips-tools（vips）ffmpeg exiftool webp optipng pngquant jpegoptim
#                   librsvg2-bin（rsvg-convert：SVG 转 PNG/PDF）graphviz（dot 画图）
#   Python 包的系统依赖  libpango/libharfbuzz-subset（weasyprint）libcairo2（cairosvg）libzbar0t64（pyzbar 识别二维码）
#   Python 与 Node  python3 + pip + venv、python-is-python3（命令 python 指向 python3）、nodejs + npm（Node 20）
#   字体            中文：Noto CJK（思源黑体/宋体，简繁日韩）、文鼎 PL 楷体/明体、文泉驿、cwTeX 仿宋/楷书；
#                   日文：IPAex / IPA 明朝与ゴシック；西文：Noto、DejaVu、Liberation（Arial/Times/Courier 度量兼容）、
#                   Carlito/Caladea（Calibri/Cambria 度量兼容）、URW base35（PostScript 标准字体）、Roboto、Open Sans、Lato、
#                   FreeFont；数学：STIX、Latin Modern；彩色 Emoji：Noto Color Emoji
# 不装 LaTeX（约 1.5 GB）、Java 与浏览器内核；HTML 转 PDF 用 weasyprint，Markdown 转 PDF 用 pandoc 配 weasyprint。
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash curl wget ca-certificates procps jq git tzdata tini iptables libcap2-bin \
      ripgrep fd-find tree file less bc gawk sqlite3 \
      zip unzip p7zip-full xz-utils bzip2 zstd unar \
      poppler-utils qpdf ghostscript mupdf-tools \
      tesseract-ocr unpaper \
      libreoffice-writer libreoffice-calc libreoffice-impress libreoffice-draw python3-uno pandoc \
      imagemagick libvips-tools ffmpeg libimage-exiftool-perl webp optipng pngquant jpegoptim librsvg2-bin graphviz \
      libpango-1.0-0 libpangoft2-1.0-0 libharfbuzz-subset0 libcairo2 libzbar0t64 \
      python3 python3-pip python3-venv python-is-python3 nodejs npm \
      fonts-noto-core fonts-noto-mono fonts-noto-cjk fonts-noto-color-emoji \
      fonts-arphic-ukai fonts-arphic-uming fonts-wqy-zenhei fonts-wqy-microhei fonts-cwtex-fs fonts-cwtex-kai \
      fonts-ipaexfont-gothic fonts-ipaexfont-mincho fonts-ipafont-gothic fonts-ipafont-mincho \
      fonts-dejavu fonts-liberation fonts-liberation2 fonts-crosextra-carlito fonts-crosextra-caladea \
      fonts-urw-base35 fonts-roboto fonts-open-sans fonts-lato fonts-freefont-ttf fonts-stix fonts-lmodern \
 && rm -rf /var/lib/apt/lists/*

# OCR 识别模型用 tesseract 官方的高精度版（tessdata_best 4.1.0）：简体、繁体、日文横排与竖排、英文，共约 67 MB。
# Debian 的语言包是精简版（每种约 2.4 MB），2026-10-18 对比实测：中文整页两者都对，日文“稼働率”只有高精度版认对。
# 按 SHA-256 校验后放进 tesseract 的模型目录（覆盖 Debian 自带的英文精简版），校验不过构建失败。
RUN cd /usr/share/tesseract-ocr/5/tessdata \
 && for lang in chi_sim chi_tra jpn jpn_vert eng; do \
      curl -fsSL --retry 3 -o "$lang.traineddata" "https://github.com/tesseract-ocr/tessdata_best/raw/4.1.0/$lang.traineddata"; \
    done \
 && printf '%s\n' \
      "4fef2d1306c8e87616d4d3e4c6c67faf5d44be3342290cf8f2f0f6e3aa7e735b  chi_sim.traineddata" \
      "1aa60488574cafa69486d919284f079ca9b68fcc7f6ad8dc1ff1b318dfd97028  chi_tra.traineddata" \
      "36bdf9ac823f5911e624c30d0553e890b8abc7c31a65b3ef14da943658c40b79  jpn.traineddata" \
      "1258be6eb2a9851f18043234ad18cca13ed32690bfff62b335c898bbea371548  jpn_vert.traineddata" \
      "8280aed0782fe27257a68ea10fe7ef324ca0f8d85bd2fd145d1c2b560bcb66ba  eng.traineddata" \
    | sha256sum -c -

# Node 常用包装成全局（/usr/local/lib/node_modules）；运行器经 SANDBOX_COMMAND_ENV 给命令设 NODE_PATH，
# 脚本里直接 require 就能用，不用每个会话 npm install。
#   pptxgenjs（生成 PPT）docx（生成 Word）exceljs（读写 Excel）pdf-lib（编辑 PDF）marked（Markdown 转 HTML）
#   sharp（图片处理）csv-parse / csv-stringify（CSV）typescript + tsx（直接运行 TypeScript）
RUN npm install -g --omit=dev \
      pptxgenjs docx exceljs pdf-lib marked sharp csv-parse csv-stringify typescript tsx \
 && npm cache clean --force

# Python 常用库，直接装进系统 Python（容器里没有别的 Python 使用者，不必 venv）：
#   数据与分析  pandas numpy scipy scikit-learn statsmodels polars pyarrow duckdb sympy networkx
#   画图        matplotlib seaborn plotly
#   Excel/Word/PPT  openpyxl xlsxwriter xlrd pyxlsb odfpy python-docx docxtpl python-pptx mammoth msoffcrypto-tool xlsx2csv
#   PDF         pypdf pdfplumber pymupdf pikepdf pdf2image reportlab fpdf2 weasyprint img2pdf ocrmypdf
#   图片        pillow pillow-heif opencv-python-headless scikit-image imageio cairosvg qrcode pyzbar graphviz
#   文本与网络  requests httpx beautifulsoup4 lxml html5lib markdown jinja2 pyyaml tabulate chardet
#               python-dateutil tqdm jieba（中文分词）wordcloud
#   命令行      csvkit（in2csv / csvsql / csvstat 等）
RUN pip install --no-cache-dir --break-system-packages \
      pandas numpy scipy scikit-learn statsmodels polars pyarrow duckdb sympy networkx \
      matplotlib seaborn plotly \
      openpyxl xlsxwriter xlrd pyxlsb odfpy python-docx docxtpl python-pptx mammoth msoffcrypto-tool xlsx2csv \
      pypdf pdfplumber pymupdf pikepdf pdf2image reportlab fpdf2 weasyprint img2pdf ocrmypdf \
      pillow pillow-heif opencv-python-headless scikit-image imageio cairosvg qrcode pyzbar graphviz \
      requests httpx beautifulsoup4 lxml html5lib markdown jinja2 pyyaml tabulate chardet \
      python-dateutil tqdm jieba wordcloud \
      csvkit

COPY image/fonts.conf /etc/fonts/local.conf
COPY image/pip.conf /etc/pip.conf
COPY image/matplotlibrc /etc/matplotlibrc
RUN fc-cache -f >/dev/null
COPY --from=build /src/target/release/sandbox-runner /usr/local/bin/sandbox-runner
COPY entrypoint.sh /usr/local/bin/sandbox-entrypoint
RUN chmod 755 /usr/local/bin/sandbox-entrypoint /usr/local/bin/sandbox-runner
# /home 是全部命名空间账号的家目录与执行记录（/home/.sandbox）所在，挂卷持久化。
# SANDBOX_COMMAND_ENV：运行器给每条命令额外设的环境变量，见 README：
#   NODE_PATH     Node 全局包的位置，脚本里直接 require
#   MATPLOTLIBRC  指向 /etc/matplotlibrc（中文字体回退、无界面后端），不改 matplotlib 安装目录里的文件
ENV SANDBOX_LISTEN=0.0.0.0:8000 \
    SANDBOX_HOME_ROOT=/home \
    SANDBOX_COMMAND_ENV="NODE_PATH=/usr/local/lib/node_modules;MATPLOTLIBRC=/etc/matplotlibrc"
VOLUME ["/home"]
EXPOSE 8000
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/sandbox-entrypoint"]
