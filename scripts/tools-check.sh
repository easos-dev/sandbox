#!/bin/bash
# 镜像工具核对：在沙箱容器里以命名空间账号执行（冒烟脚本经 /execute 把整段脚本交给 bash -c），
# 也可以手动跑：docker exec -u <账号> <容器> bash /path/tools-check.sh。
# 每项真用一遍而不只看命令在不在：转换出的文件再读回来核对内容，字体映射看 PDF 里实际嵌入的字体。
# 每行输出“通过 名称 说明”或“失败 名称 说明”，最后一行是汇总；有失败时退出码为 1。
# 在临时目录里做，结束删掉，不在会话目录留东西；“pip 装到用户目录”那项装完即卸载。
set -uo pipefail
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"
FAILS=0

pass() { echo "通过  $1  ${2:-}"; }
fail() { echo "失败  $1  ${2:-}"; FAILS=$((FAILS + 1)); }
# check 名称 命令…：命令成功即通过，失败时带上最后两行输出。
check() {
    local name="$1"; shift
    local out
    if out=$("$@" 2>&1); then pass "$name" "$(printf '%s' "$out" | tail -1 | cut -c1-120)"; else fail "$name" "$(printf '%s' "$out" | tail -2 | tr '\n' ' ' | cut -c1-200)"; fi
}

# 1. 工具文档里的清单逐项核对：命令找得到、系统包已安装、Python 包已安装、Python 模块能导入、Node 包在全局目录、字族在 fontconfig 里。
#    清单只在文档（镜像里的 /usr/local/share/sandbox/TOOLS.md，源文件 doc/镜像工具.md）里维护，这里不另写一份。
DOC=/usr/local/share/sandbox/TOOLS.md
# block 名称：取文档里 ```名称 代码块的内容，一行一项，空行忽略。
block() { awk -v b="$1" '$0 == "```" b {f = 1; next} f && $0 == "```" {exit} f && NF' "$DOC"; }
if [ ! -f "$DOC" ]; then
    fail "工具文档" "找不到 $DOC"
else
    pass "工具文档" "$DOC，$(wc -l < "$DOC" | tr -d ' ') 行"
    missing=""; n=0
    while IFS= read -r c; do n=$((n + 1)); command -v "$c" >/dev/null 2>&1 || missing="$missing $c"; done < <(block commands)
    [ $n -gt 0 ] && [ -z "$missing" ] && pass "文档清单：命令" "$n 个" || fail "文档清单：命令" "共 $n 个，缺:$missing"
    missing=""; n=0
    while IFS= read -r p; do n=$((n + 1)); [ "$(dpkg-query -W -f='${db:Status-Status}' "$p" 2>/dev/null)" = installed ] || missing="$missing $p"; done < <(block apt-packages)
    [ $n -gt 0 ] && [ -z "$missing" ] && pass "文档清单：系统包" "$n 个" || fail "文档清单：系统包" "共 $n 个，未安装:$missing"
    check "文档清单：Python 包" python3 -c '
import sys, importlib.metadata as md
names = sys.stdin.read().split()
bad = []
for n in names:
    try:
        md.version(n)
    except md.PackageNotFoundError:
        bad.append(n)
if not names or bad:
    raise SystemExit(f"共 {len(names)} 个，未安装: {bad}")
print(f"{len(names)} 个")' < <(block pip-packages)
    check "文档清单：Python 模块" python3 -W ignore -c '
import sys, importlib
names = sys.stdin.read().split()
bad = []
for n in names:
    try:
        importlib.import_module(n)
    except Exception as e:
        bad.append(f"{n}({type(e).__name__}: {e})")
if not names or bad:
    raise SystemExit(f"共 {len(names)} 个，导入失败: " + "; ".join(bad))
print(f"Python {sys.version.split()[0]}，{len(names)} 个模块")' < <(block python-modules)
    missing=""; n=0
    while IFS= read -r p; do n=$((n + 1)); [ -f "/usr/local/lib/node_modules/$p/package.json" ] || missing="$missing $p"; done < <(block node-packages)
    [ $n -gt 0 ] && [ -z "$missing" ] && pass "文档清单：Node 包" "$n 个" || fail "文档清单：Node 包" "共 $n 个，缺:$missing"
    families=$(fc-list --format '%{family}\n' | tr ',' '\n' | sort -u)
    missing=""; n=0
    while IFS= read -r f; do n=$((n + 1)); grep -qxF "$f" <<<"$families" || missing="$missing [$f]"; done < <(block fonts)
    [ $n -gt 0 ] && [ -z "$missing" ] && pass "文档清单：字体" "$n 个字族" || fail "文档清单：字体" "共 $n 个，缺:$missing"
fi

# 2. OCR 语言包
langs=$(tesseract --list-langs 2>/dev/null | tail -n +2 | tr '\n' ' ')
need_ok=1; for l in chi_sim chi_tra jpn eng; do case " $langs " in *" $l "*) ;; *) need_ok=0 ;; esac; done
[ $need_ok = 1 ] && pass "OCR 语言包" "$langs" || fail "OCR 语言包" "$langs"

# 2b. Node 全局包能直接 require（NODE_PATH 生效）
check "Node 常用包可 require" node -e '
const mods = ["pptxgenjs", "docx", "exceljs", "pdf-lib", "marked", "sharp", "csv-parse/sync", "csv-stringify/sync", "typescript"];
for (const m of mods) require(m);
console.log(`Node ${process.version}，${mods.length} 个包`);'

# 3. 字体映射：Windows / macOS 字体名落到预期的字体
font_map_ok=1; font_detail=""
while IFS='|' read -r name expect; do
    got=$(fc-match -f '%{family[0]}' "$name")
    font_detail="$font_detail $name→$got"
    case "$got" in "$expect"*) ;; *) font_map_ok=0; font_detail="$font_detail(应为 $expect)" ;; esac
done <<'EOF'
宋体|Noto Serif CJK SC
SimSun|Noto Serif CJK SC
微软雅黑|Noto Sans CJK SC
楷体|AR PL UKai CN
仿宋|Zhuque Fangsong
方正仿宋_GBK|Zhuque Fangsong
方正小标宋简体|Noto Serif CJK SC
標楷體|AR PL UKai TW
新細明體|Noto Serif CJK TC
ＭＳ 明朝|IPAexMincho
MS Gothic|IPAexGothic
メイリオ|Noto Sans CJK JP
맑은 고딕|Noto Sans CJK KR
Calibri|Carlito
Cambria|Caladea
Arial|Liberation Sans
Times New Roman|Liberation Serif
Courier New|Liberation Mono
Georgia|Gelasio
Garamond|EB Garamond
Palatino Linotype|TeX Gyre Pagella
Century Gothic|TeX Gyre Adventor
Comic Sans MS|Comic Neue
BIZ UDPGothic|BIZ UDPGothic
EOF
[ $font_map_ok = 1 ] && pass "字体名映射" "$font_detail" || fail "字体名映射" "$font_detail"
# 3b. 字重齐全；可变字体都已拆成静态字体（reportlab、matplotlib 只认静态字体的字重）。字体是否都在由第 1 节按文档清单核对。
check "字重齐全且无可变字体残留" bash -c '
for spec in "Noto Sans CJK SC:Thin" "Noto Sans CJK SC:Black" "Noto Serif CJK SC:ExtraLight" "Noto Serif CJK SC:Black" "Playfair Display:Bold" "Lora:Bold Italic" "Inter:Bold" "DM Sans:Bold" "Merriweather:Bold"; do
  [ -n "$(fc-list ":family=${spec%%:*}:style=${spec#*:}")" ] || { echo "缺 $spec"; exit 1; }
done
left=$(find /usr/share/fonts/truetype/google-fonts -name "*\[*" | head -3); [ -z "$left" ] || { echo "还有可变字体: $left"; exit 1; }
w=$(fc-match -f "%{weight}" "方正小标宋简体"); [ "$w" = 200 ] || { echo "方正小标宋简体 字重 $w，应为粗体 200"; exit 1; }
echo "思源黑体 7 档字重；可变字体已全部拆开；方正小标宋映射为粗体"'
check "彩色 Emoji 字体" bash -c 'fc-list | grep -q "Noto Color Emoji" && echo 有'

# 4. matplotlib 画中文不成方块
check "matplotlib 中文" python3 - <<'EOF'
import warnings
import matplotlib.pyplot as plt
from matplotlib import font_manager
with warnings.catch_warnings(record=True) as caught:
    warnings.simplefilter("always")
    fig, ax = plt.subplots()
    ax.set_title("入住率 稼働率 −5%")
    ax.plot([1, 2, 3], [3, 1, 2], label="中文图例")
    ax.legend()
    fig.savefig("chart.png")
missing = [str(w.message) for w in caught if "missing from" in str(w.message)]
if missing:
    raise SystemExit("缺字: " + missing[0])
cjk = sorted({f.name for f in font_manager.fontManager.ttflist if "CJK" in f.name})
print("后端", plt.get_backend(), "，认到的 CJK 字体", ", ".join(cjk))
EOF

# 5. HTML 转 PDF（weasyprint），中日文嵌入字体、可抽回文字。
#    页面必须声明 charset：weasyprint 读本地 HTML 文件时没有声明就按 windows-1252 解码，中文会成乱码（2026-10-18 踩过）。
check "HTML 转 PDF" bash -c '
cat > page.html <<HTML
<html><head><meta charset="utf-8"></head><body style="font-family: sans-serif"><h1>报表 レポート</h1><p>入住率 85%，稼働率 85%。</p></body></html>
HTML
python3 -c "import weasyprint; weasyprint.HTML(\"page.html\").write_pdf(\"page.pdf\")" 2>/dev/null
pdftotext page.pdf - | grep -q "稼働率" || { echo "抽不出文字"; exit 1; }
pdffonts page.pdf | grep -qi "CJK" || { echo "没有嵌入 CJK 字体"; exit 1; }
pdffonts page.pdf | awk "NR>2 {print \$1}" | sed "s/^[A-Z]*+//" | sort -u | tr "\n" " "'

# 5b. 网页里写设计字体 → PDF 里嵌入的就是它们；reportlab 直接用拆出来的静态粗体
check "设计字体进 PDF" bash -c '
cat > design.html <<HTML
<html><head><meta charset="utf-8"></head><body>
<h1 style="font-family: Playfair Display; font-weight: 700">Quarterly Report</h1>
<p style="font-family: LXGW WenKai">霞鹜文楷：本季度入住率稳步上升。</p>
<p style="font-family: Zen Old Mincho">宿泊者名簿のご案内</p>
<p style="font-family: Noto Sans CJK SC; font-weight: 900">思源黑体 Black 标题</p>
</body></html>
HTML
python3 -c "import weasyprint; weasyprint.HTML(\"design.html\").write_pdf(\"design.pdf\")" 2>/dev/null
fonts=$(pdffonts design.pdf | awk "NR>2 {print \$1}" | sed "s/^[A-Z]*+//" | sort -u | tr "\n" " ")
# weasyprint 用连字符分词写字体名（Playfair-Display-Bold），思源黑体 Black 写作 Heavy；去掉连字符再比对。
flat=$(printf "%s" "$fonts" | tr -d "-")
for want in PlayfairDisplayBold LXGWWenKai ZenOldMincho; do case "$flat" in *"$want"*) ;; *) echo "缺 $want，实际: $fonts"; exit 1;; esac; done
case "$flat" in *CJKSCHeavy*|*CJKSCBlack*) ;; *) echo "缺思源黑体 Black，实际: $fonts"; exit 1;; esac
python3 - <<PY
import subprocess
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.pdfgen import canvas
path = subprocess.check_output(["fc-match", "-f", "%{file}", "Lora:Bold"], text=True)
assert "Bold" in path, path
pdfmetrics.registerFont(TTFont("LoraBold", path))
c = canvas.Canvas("rl.pdf"); c.setFont("LoraBold", 24); c.drawString(72, 720, "Lora Bold via reportlab"); c.save()
PY
pdffonts rl.pdf | grep -q "Lora" || { echo "reportlab 没嵌入 Lora"; exit 1; }
echo "$fonts；reportlab 用 Lora Bold"'

# 6. docx 里写宋体、楷体 → LibreOffice 转 PDF → 嵌入的是映射后的字体
check "Office 字体映射转 PDF" bash -c '
python3 - <<PY
from docx import Document
from docx.oxml.ns import qn
d = Document()
p = d.add_paragraph()
for text, font in (("宋体正文", "宋体"), (" 楷体正文", "楷体"), (" ＭＳ明朝の本文", "ＭＳ 明朝")):
    r = p.add_run(text)
    r.font.name = font
    r._element.rPr.rFonts.set(qn("w:eastAsia"), font)
d.save("fonts.docx")
PY
soffice --headless --convert-to pdf fonts.docx >/dev/null 2>&1
fonts=$(pdffonts fonts.pdf | awk "NR>2 {print \$1}" | sed "s/^[A-Z]*+//" | sort -u | tr "\n" " ")
for want in NotoSerifCJK UKai IPAexMincho; do case "$fonts" in *"$want"*) ;; *) echo "缺 $want，实际: $fonts"; exit 1 ;; esac; done
echo "$fonts"'

# 7. pandoc：Markdown → docx → 读回
check "pandoc 转 docx" bash -c '
printf "# 标题\n\n- 第一条\n- 第二条\n" > note.md
pandoc note.md -o note.docx && python3 -c "import docx; print(len(docx.Document(\"note.docx\").paragraphs), \"段\")"'

# 8. OCR：渲染一页三行中文与一行日文再识别（tesseract 默认版面分析面向整页，单行小图会认乱，所以做成页面）；
#     ocrmypdf 给图片 PDF 加文字层。
check "中日文 OCR" bash -c '
python3 - <<PY
import subprocess
from PIL import Image, ImageDraw, ImageFont
def font(name, size):
    # Noto CJK 是字体集合（.ttc），按 fontconfig 给出的文件与字面序号取对应地区的那一面。
    path, index = subprocess.check_output(["fc-match", "-f", "%{file}|%{index}", name], text=True).split("|")
    return ImageFont.truetype(path, size, index=int(index))
img = Image.new("RGB", (1600, 600), "white")
draw = ImageDraw.Draw(img)
for n, line in enumerate(["沙箱文字识别测试：入住登记表", "房间号 302，入住日期 2026年10月18日", "备注：客人需要提前退房"]):
    draw.text((80, 80 + n * 120), line, font=font("Noto Sans CJK SC", 48), fill="black")
img.save("zh.png")
img = Image.new("RGB", (1600, 300), "white")
ImageDraw.Draw(img).text((80, 100), "宿泊者名簿 チェックイン 稼働率", font=font("Noto Sans CJK JP", 48), fill="black")
img.save("ja.png")
PY
zh=$(tesseract zh.png - -l chi_sim 2>/dev/null | tr -d " \n")
ja=$(tesseract ja.png - -l jpn 2>/dev/null | tr -d " \n")
case "$zh" in *入住登记表*房间号302*提前退房*) ;; *) echo "中文认错：$zh"; exit 1;; esac
case "$ja" in *宿泊者名簿*チェックイン*稼働率*) ;; *) echo "日文认错：$ja"; exit 1;; esac
img2pdf zh.png -o scan.pdf && ocrmypdf -q -l chi_sim scan.pdf scan-ocr.pdf
pdftotext scan-ocr.pdf - | tr -d " \n" | grep -q "入住登记表" || { echo "ocrmypdf 没加上文字层"; exit 1; }
echo "中文：$zh；日文：$ja；ocrmypdf 文字层可抽取"'

# 9. PDF 工具链：拆页、转图、合并
check "PDF 处理" bash -c '
qpdf --empty --pages page.pdf fonts.pdf -- merged.pdf
n=$(pdfinfo merged.pdf | awk "/^Pages:/ {print \$2}"); [ "$n" = 2 ] || { echo "合并后 $n 页"; exit 1; }
pdftoppm -r 50 -png -f 1 -l 1 merged.pdf pg && magick merged.pdf[1] -resize 50% pg2.png && mutool info merged.pdf >/dev/null
python3 -c "import pymupdf, pdfplumber; d=pymupdf.open(\"merged.pdf\"); t=pdfplumber.open(\"merged.pdf\").pages[0].extract_text(); assert \"稼働率\" in t; print(d.page_count, \"页，PyMuPDF 与 pdfplumber 可读\")"'

# 10. 图片与音视频
check "图片处理" bash -c '
magick -size 64x64 xc:skyblue logo.png && vips copy logo.png logo.webp && cwebp -quiet logo.png -o logo2.webp
printf "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"40\" height=\"40\"><circle cx=\"20\" cy=\"20\" r=\"18\" fill=\"red\"/></svg>" > c.svg
rsvg-convert c.svg -o c.png && python3 -c "import cairosvg; cairosvg.svg2png(url=\"c.svg\", write_to=\"c2.png\")"
echo "digraph{a->b}" | dot -Tpng -o g.png
python3 -c "import cv2, pillow_heif, qrcode; from pyzbar.pyzbar import decode; from PIL import Image; qrcode.make(\"easos\").save(\"qr.png\"); assert decode(Image.open(\"qr.png\"))[0].data == b\"easos\"; print(\"OpenCV\", cv2.__version__, \"，二维码生成与识别\")"'
check "音视频" bash -c '
ffmpeg -v error -f lavfi -i testsrc=duration=1:size=160x120:rate=10 -y clip.mp4 && ffprobe -v error -show_entries format=duration -of csv=p=0 clip.mp4'

# 11. 数据分析与 Office 生成
check "数据分析" python3 - <<'EOF'
import duckdb, pandas as pd, polars as pl
df = pd.DataFrame({"房间": ["101", "102", "101"], "金额": [100, 200, 300]})
df.to_csv("data.csv", index=False)
total = duckdb.sql("select sum(金额) from 'data.csv'").fetchone()[0]
assert total == 600 and pl.read_csv("data.csv")["金额"].sum() == 600
df.to_excel("data.xlsx", index=False)
assert pd.read_excel("data.xlsx")["金额"].sum() == 600
print("duckdb / polars / pandas 读写 CSV 与 Excel")
EOF
check "Node 生成 PPT" bash -c '
node -e "const P=require(\"pptxgenjs\"); const p=new P(); p.addSlide().addText(\"季度报告\", {x:1,y:1}); p.writeFile({fileName:\"deck.pptx\"}).then(()=>{})" &&
python3 -c "import pptx; print(len(pptx.Presentation(\"deck.pptx\").slides), \"页，python-pptx 可读\")"'

# 12. LaTeX：XeLaTeX + ctex 写中文（默认字库应是思源宋体/黑体、文鼎楷体、朱雀仿宋，见 image/tex/）、公式；
#     XeLaTeX 的 PDF 里思源字体显示为 NotoSerifCJKjp 是 xdvipdfmx 的命名方式，字形是简体，只比对前缀 NotoSerifCJK。
#     LuaLaTeX + luatexja 写日文；pandoc 经 XeLaTeX 把 Markdown 转 PDF。
check "XeLaTeX 中文与公式" bash -c '
cat > zh.tex <<TEX
\documentclass{ctexart}
\usepackage{amsmath}
\begin{document}
\section{入住统计}
本月入住率为 85\%，较上月提升。{\heiti 黑体强调}，{\kaishu 楷体备注}，{\fangsong 仿宋落款}。
\[ \sum_{i=1}^{n} i = \frac{n(n+1)}{2} \]
\end{document}
TEX
xelatex -interaction=nonstopmode -halt-on-error zh.tex >/dev/null 2>&1 || { grep -m3 "^!" zh.log; exit 1; }
pdftotext zh.pdf - | tr -d " \n" | grep -q "入住统计" || { echo "抽不出中文"; exit 1; }
fonts=$(pdffonts zh.pdf | awk "NR>2 {print \$1}" | sed "s/^[A-Z]*+//" | sort -u | tr "\n" " ")
for want in NotoSerifCJK NotoSansCJK UKai ZhuqueFangsong; do case "$fonts" in *"$want"*) ;; *) echo "缺 $want，实际: $fonts"; exit 1;; esac; done
echo "$fonts"'
check "LuaLaTeX 日文" bash -c '
cat > ja.tex <<TEX
\documentclass{ltjsarticle}
\begin{document}
\section{宿泊者名簿}
チェックインは15時からです。
\end{document}
TEX
lualatex -interaction=nonstopmode -halt-on-error ja.tex >/dev/null 2>&1 || { grep -m3 "^!" ja.log; exit 1; }
pdftotext ja.pdf - | tr -d " \n" | grep -q "宿泊者名簿" || { echo "抽不出日文"; exit 1; }
pdffonts ja.pdf | awk "NR>2 {print \$1}" | sed "s/^[A-Z]*+//" | sort -u | tr "\n" " "'
check "pandoc 经 XeLaTeX 出 PDF" bash -c '
printf "# 月度报告\n\n入住率 **85%%**，公式 \$a^2+b^2=c^2\$。\n" > report.md
pandoc report.md -o report.pdf --pdf-engine=xelatex -V CJKmainfont="Noto Serif CJK SC" 2>&1 | tail -2
pdftotext report.pdf - | tr -d " \n" | grep -q "月度报告" || { echo "抽不出中文"; exit 1; }
echo "$(pdfinfo report.pdf | awk "/^Pages:/ {print \$2}") 页"'

# 13. 模型自己 pip install：系统目录不可写时装到 ~/.local，命令在 PATH 里
check "pip 装到用户目录" bash -c '
pip install -q --no-warn-script-location cowsay >/dev/null 2>&1 || { echo "pip install 失败（无网络？）"; exit 1; }
out=$(cowsay -t hi 2>&1); pip uninstall -q -y cowsay >/dev/null 2>&1
case "$out" in *hi*) echo "装在 $(python3 -m site --user-site 2>/dev/null | sed "s#$HOME#~#")，命令可直接运行";; *) echo "$out"; exit 1;; esac'

if [ $FAILS -eq 0 ]; then echo "工具核对全部通过"; else echo "工具核对失败 $FAILS 项"; exit 1; fi
