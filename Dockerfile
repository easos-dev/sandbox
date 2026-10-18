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

# 设计用字体（为了排出好看的 PDF），放在 Python 包之后：改字体清单不会让前面的大层重建。
# Debian 包：
#   中文  思源黑体/宋体的全部字重（Thin～Black，标题与正文分出层次）、霞鹜文楷（LXGW WenKai）、得意黑（Smiley Sans）
#   日文  BIZ UD 明朝/ゴシック（Windows 10 起自带的日文商务字体）、M+、さわらび明朝/ゴシック、モトヤ L シーダ/マルベリ（丸ゴシック）
#   英文  Inter、Source Sans 3、TeX Gyre 全套（Palatino/Times/Helvetica/Bookman/Century 风格）、Linux Libertine、
#         Montserrat、Roboto Slab、Vollkorn、Quicksand、Manrope、League Spartan、Comic Neue（Comic Sans 度量兼容）、
#         Fira Code 与 JetBrains Mono（代码）
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      fonts-noto-cjk-extra fonts-lxgw-wenkai fonts-smiley-sans \
      fonts-morisawa-bizud-gothic fonts-morisawa-bizud-mincho fonts-mplus fonts-sawarabi-gothic fonts-sawarabi-mincho \
      fonts-motoya-l-cedar fonts-motoya-l-maruberi \
      fonts-inter fonts-adobe-sourcesans3 fonts-texgyre fonts-linuxlibertine fonts-montserrat fonts-roboto-slab \
      fonts-vollkorn fonts-quicksand fonts-manrope fonts-league-spartan fonts-comic-neue fonts-firacode fonts-jetbrains-mono \
 && rm -rf /var/lib/apt/lists/*

# Google Fonts 官方仓库（固定到下面这个提交，git 按对象哈希校验内容）里 Debian 没有的常用字体，约 190 MB：
#   中文  站酷小薇、站酷庆科黄油体、站酷快乐体、马善政毛笔楷书、龙藏体（标题、海报、证书用）
#   日文  Zen 角ゴシック New、Zen 丸ゴシック、Zen オールド明朝、しっぽり明朝、M PLUS Rounded 1c、Dela Gothic One、Klee One
#   英文  Playfair Display、Merriweather、Lora、Source Serif 4、Libre Baskerville、Crimson Pro、EB Garamond、Poppins、Raleway、Nunito、
#         Work Sans、DM Sans、Oswald、Bebas Neue、IBM Plex Sans/Serif/Mono、Gelasio（Georgia 度量兼容）
# 只稀疏检出这些目录的 .ttf；可变字体由 image/static-fonts.py 拆成各字重的静态字体（原因见脚本开头）。
# EB Garamond 用这里的版本而不用 Debian 包：Debian 包把 08、12 两种光学尺寸挂在同一字族名下，weasyprint 选粗体时会选错。
COPY image/static-fonts.py /tmp/static-fonts.py
RUN set -e; dest=/usr/share/fonts/truetype/google-fonts; \
    dirs="zcoolxiaowei zcoolqingkehuangyou zcoolkuaile mashanzheng longcang \
          zenkakugothicnew zenmarugothic zenoldmincho shipporimincho mplusrounded1c delagothicone kleeone \
          playfairdisplay merriweather lora sourceserif4 librebaskerville crimsonpro ebgaramond poppins raleway nunito worksans dmsans \
          oswald bebasneue ibmplexsans ibmplexserif ibmplexmono gelasio"; \
    git init -q /tmp/gf && cd /tmp/gf && git remote add origin https://github.com/google/fonts.git \
 && git fetch -q --depth 1 --filter=blob:none origin 47d54ca1ebb14fd864695f1d39dc734f39017c33 \
 && git sparse-checkout init --no-cone \
 && for d in $dirs; do echo "/ofl/$d/*.ttf"; echo "/ofl/$d/OFL.txt"; done > .git/info/sparse-checkout \
 && git checkout -q FETCH_HEAD \
 && for d in $dirs; do test -d "ofl/$d" || { echo "缺 $d" >&2; exit 1; }; mkdir -p "$dest/$d"; cp ofl/$d/*.ttf "$dest/$d/"; if [ -f ofl/$d/OFL.txt ]; then cp ofl/$d/OFL.txt "$dest/$d/"; fi; done \
 && cd / && rm -rf /tmp/gf \
 && python3 /tmp/static-fonts.py "$dest" && rm /tmp/static-fonts.py

# 朱雀仿宋（OFL，TrionesType）：按国标字形设计的仿宋，比 cwTeX 仿宋（台湾字形）更适合简体公文；Windows 文档里的
# 仿宋、仿宋_GB2312、方正仿宋都映射到它。只有预发布版，固定 v0.212 并按 SHA-256 校验。
# 族名去掉“(technical preview)”“（预览测试版）”：weasyprint 拿族名拼 PDF 里的字体名，括号不转义，生成的 PDF 语法不合法
# （2026-10-18 qpdf 实测报错）。许可证是 OFL 1.1 且没有声明保留字体名，允许改名；PostScript 名本来就是 ZhuqueFangsong-Regular。
RUN cd /tmp \
 && curl -fsSL --retry 3 -o zhuque.zip https://github.com/TrionesType/zhuque/releases/download/v0.212/ZhuqueFangsong-v0.212.zip \
 && echo "bb8b661a7643d2296a72d9d10530a00949419c4e527fb61783f73c2ba1a8c062  zhuque.zip" | sha256sum -c - \
 && mkdir -p /usr/share/fonts/truetype/zhuque && unzip -q -j zhuque.zip '*.ttf' -d /usr/share/fonts/truetype/zhuque \
 && rm zhuque.zip \
 && python3 -c 'from fontTools.ttLib import TTFont; p = "/usr/share/fonts/truetype/zhuque/ZhuqueFangsong-Regular.ttf"; f = TTFont(p); [setattr(r, "string", r.toUnicode().replace(" (technical preview)", "").replace("（预览测试版）", "")) for r in f["name"].names if r.nameID in (1, 3, 4, 16)]; f.save(p)' \
 && test "$(fc-scan --format '%{family[0]}' /usr/share/fonts/truetype/zhuque/ZhuqueFangsong-Regular.ttf)" = "Zhuque Fangsong"

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
