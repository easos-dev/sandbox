#!/usr/bin/env python3
"""字体样张：把镜像里适合排版的字体按中文、日文、英文分组，每个字体用对应语言的示范文字排一行（有粗体再排一行），
标出 CSS / LibreOffice 里该写的字体名，用 weasyprint 生成 PDF。挑字体、核对字体有没有装好都用它。

在沙箱容器里运行（要 weasyprint 与 fontconfig）：python3 font-specimen.py 输出.pdf
字体是否真的存在、有哪些字重，都现查 fontconfig：不存在的字体在样张里标红，不会被别的字体顶替而看不出来。
"""

import html
import subprocess
import sys

import weasyprint

# 示范文字：每组一句，覆盖常用字、数字与标点。
SAMPLE_ZH = "天地玄黄，宇宙洪荒。入住登记与房态月报 2026"
SAMPLE_JA = "いろはにほへと 宿泊者名簿のご案内 チェックイン 15:00"
SAMPLE_EN = "The quick brown fox jumps over the lazy dog — 0123456789"
# 等宽字体的示范：代码片段，混淆字符 0O 1lI 放在一起看区分度；比正文短，免得等宽字体折行。
SAMPLE_CODE = "total = sum(r.price for r in rooms)  # 0O 1lI -> !="

# (分组标题, 示范文字, [(字体名, 说明)])；字体名就是 CSS font-family 与 LibreOffice 里写的名字。
GROUPS = [
    ("中文 · 正文与标题", SAMPLE_ZH, [
        ("Noto Sans CJK SC", "思源黑体，Thin 至 Black 七档字重"),
        ("Noto Serif CJK SC", "思源宋体，ExtraLight 至 Black 七档字重"),
        ("LXGW WenKai", "霞鹜文楷，适合阅读类长文"),
        ("Zhuque Fangsong", "朱雀仿宋，国标字形，公文正文"),
        ("AR PL UKai CN", "文鼎 PL 中楷，对应 Windows 楷体"),
        ("AR PL UMing CN", "文鼎 PL 明体"),
        ("WenQuanYi Zen Hei", "文泉驿正黑"),
    ]),
    ("中文 · 展示与书法", SAMPLE_ZH, [
        ("Smiley Sans", "得意黑，现代感标题"),
        ("ZCOOL XiaoWei", "站酷小薇，文艺标题"),
        ("ZCOOL QingKe HuangYou", "站酷庆科黄油体，海报"),
        ("ZCOOL KuaiLe", "站酷快乐体，活泼"),
        ("Ma Shan Zheng", "马善政毛笔楷书，证书与贺卡"),
        ("Long Cang", "龙藏体，手写"),
    ]),
    ("日文", SAMPLE_JA, [
        ("Noto Sans CJK JP", "源ノ角ゴシック"),
        ("Noto Serif CJK JP", "源ノ明朝"),
        ("BIZ UDPGothic", "BIZ UD ゴシック，Windows 自带的商务字体"),
        ("BIZ UDPMincho", "BIZ UD 明朝"),
        ("IPAexGothic", "IPAex ゴシック"),
        ("IPAexMincho", "IPAex 明朝"),
        ("Zen Kaku Gothic New", "Zen 角ゴシック"),
        ("Zen Maru Gothic", "Zen 丸ゴシック"),
        ("Zen Old Mincho", "Zen オールド明朝"),
        ("Shippori Mincho", "しっぽり明朝"),
        ("Rounded Mplus 1c", "M PLUS Rounded 1c"),
        ("M PLUS 1", "M+ 1"),
        ("Sawarabi Mincho", "さわらび明朝"),
        ("Klee One", "クレー，手书风"),
        ("MotoyaLMaru", "モトヤ L マルベリ"),
        ("Dela Gothic One", "デラゴシック，展示用"),
    ]),
    ("英文 · 无衬线", SAMPLE_EN, [
        ("Inter", ""), ("Source Sans 3", ""), ("IBM Plex Sans", ""), ("Poppins", ""), ("Montserrat", ""),
        ("Raleway", ""), ("Nunito", ""), ("Work Sans", ""), ("DM Sans", ""), ("Manrope", ""), ("Quicksand", ""),
        ("Lato", ""), ("Open Sans", ""), ("Roboto", ""), ("Noto Sans", ""),
        ("Liberation Sans", "与 Arial 度量兼容"), ("Carlito", "与 Calibri 度量兼容"), ("TeX Gyre Heros", "Helvetica 风格"),
    ]),
    ("英文 · 衬线", SAMPLE_EN, [
        ("Playfair Display", "展示标题"), ("Merriweather", ""), ("Lora", ""), ("Source Serif 4", ""),
        ("Libre Baskerville", ""), ("Crimson Pro", ""), ("EB Garamond", ""), ("Linux Libertine O", ""),
        ("Vollkorn", ""), ("IBM Plex Serif", ""), ("Roboto Slab", ""), ("Gelasio", "与 Georgia 度量兼容"),
        ("TeX Gyre Pagella", "Palatino 风格"), ("TeX Gyre Termes", "Times 风格"), ("TeX Gyre Bonum", "Bookman 风格"),
        ("TeX Gyre Schola", "Century Schoolbook 风格"), ("Liberation Serif", "与 Times New Roman 度量兼容"),
        ("Caladea", "与 Cambria 度量兼容"), ("Noto Serif", ""),
    ]),
    ("英文 · 展示", SAMPLE_EN, [
        ("Oswald", ""), ("Bebas Neue", "只有大写"), ("League Spartan", ""), ("TeX Gyre Adventor", "Avant Garde 风格"),
        ("Comic Neue", "与 Comic Sans 度量兼容"),
    ]),
    ("等宽（代码）", SAMPLE_CODE, [
        ("JetBrains Mono", ""), ("Fira Code", "带连字"), ("IBM Plex Mono", ""),
        ("DejaVu Sans Mono", ""), ("Noto Sans Mono CJK SC", "中英等宽"),
    ]),
]

# fontconfig 的字重数值 → CSS font-weight（只列样张会用到的几档）。
FC_TO_CSS = {0: 100, 40: 200, 50: 300, 55: 350, 80: 400, 100: 500, 180: 600, 200: 700, 205: 800, 210: 900}


def weights(family: str) -> list[int]:
    """这个字族实际有的正体字重（CSS 数值，升序）；只有斜体的字族（得意黑本身就是斜体设计）取斜体的字重；字族不存在时为空。"""
    out = subprocess.run(
        ["fc-list", f":family={family}:slant=0", "weight"], capture_output=True, text=True, check=False
    ).stdout or subprocess.run(
        ["fc-list", f":family={family}", "weight"], capture_output=True, text=True, check=False
    ).stdout
    found = set()
    for line in out.splitlines():
        if "weight=" in line:
            value = line.split("weight=", 1)[1].strip().rstrip(":")
            # 可能是单个数或 [a b] 区间（可变字体），区间取两端。
            for part in value.strip("[]").split():
                try:
                    number = int(float(part))
                except ValueError:
                    continue
                found.add(FC_TO_CSS.get(number, min(FC_TO_CSS.values(), key=lambda css: abs(css - number * 4.5))))
    return sorted(found)


def row(family: str, note: str, sample: str) -> str:
    available = weights(family)
    name = html.escape(family)
    label = f'<div class="label"><span class="name">{name}</span> <span class="note">{html.escape(note)}</span></div>'
    if not available:
        return f'<div class="row">{label}<div class="missing">未安装</div></div>'
    regular = min(available, key=lambda w: abs(w - 400))
    lines = [f'<div class="sample" style="font-family: \'{name}\'; font-weight: {regular}">{html.escape(sample)}</div>']
    heavy = [w for w in available if w >= 600]
    if heavy:
        bold = 700 if 700 in heavy else heavy[0]
        lines.append(f'<div class="sample" style="font-family: \'{name}\'; font-weight: {bold}">{html.escape(sample)}</div>')
    span = f"字重 {available[0]}–{available[-1]}" if len(available) > 1 else f"字重 {available[0]}"
    label = label.replace("</div>", f' <span class="weights">{span}</span></div>', 1)
    return f'<div class="row">{label}{"".join(lines)}</div>'


def weight_ladder(family: str, sample: str) -> str:
    """把一个字族的全部字重从细到粗排一遍。"""
    items = "".join(
        f'<div class="ladder" style="font-family: \'{html.escape(family)}\'; font-weight: {w}">'
        f'<span class="w">{w}</span>{html.escape(sample)}</div>'
        for w in weights(family)
    )
    return f'<div class="row"><div class="label"><span class="name">{html.escape(family)}</span> 全部字重</div>{items}</div>'


def main() -> None:
    target = sys.argv[1] if len(sys.argv) > 1 else "font-specimen.pdf"
    body = ['<h1>沙箱镜像字体样张</h1><p class="intro">字体名即 CSS font-family 与 LibreOffice 里写的名字；'
            '每个字体先排常规，再排粗体（有的话）。标红的是没装上的字体。</p>']
    body.append("<h2>中文字重</h2>")
    body.append(weight_ladder("Noto Sans CJK SC", "入住登记与房态月报"))
    body.append(weight_ladder("Noto Serif CJK SC", "入住登记与房态月报"))
    for title, sample, fonts in GROUPS:
        body.append(f"<h2>{html.escape(title)}</h2>")
        body.extend(row(family, note, sample) for family, note in fonts)
    css = """
    @page { size: A4; margin: 16mm 14mm; @bottom-right { content: counter(page); font: 9pt 'Noto Sans CJK SC'; color: #888; } }
    body { font-family: 'Noto Sans CJK SC'; color: #222; }
    h1 { font: 900 22pt 'Noto Sans CJK SC'; margin: 0 0 4pt; }
    .intro { font-size: 9pt; color: #666; margin: 0 0 10pt; }
    h2 { font: 700 13pt 'Noto Sans CJK SC'; border-bottom: 1.5pt solid #333; padding-bottom: 2pt; margin: 14pt 0 6pt; }
    .row { break-inside: avoid; border-bottom: 0.5pt solid #ddd; padding: 5pt 0; }
    .label { font: 8pt 'Noto Sans CJK SC'; color: #888; margin-bottom: 2pt; }
    .label .name { font-family: 'DejaVu Sans Mono'; color: #1a5fb4; }
    .label .weights { color: #aaa; }
    .sample { font-size: 15pt; line-height: 1.35; }
    .ladder { font-size: 13pt; line-height: 1.4; }
    .ladder .w { display: inline-block; width: 30pt; font: 8pt 'DejaVu Sans Mono'; color: #aaa; }
    .missing { color: #c01c28; font-size: 11pt; }
    """
    page = f'<html><head><meta charset="utf-8"><style>{css}</style></head><body>{"".join(body)}</body></html>'
    weasyprint.HTML(string=page).write_pdf(target)
    print(f"已生成 {target}")


if __name__ == "__main__":
    main()
