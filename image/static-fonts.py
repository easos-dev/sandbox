#!/usr/bin/env python3
"""把目录里的可变字体拆成各命名字重的静态字体，原可变字体文件删掉（构建镜像时运行一次）。

为什么要拆：Google Fonts 的很多字族只发可变字体（一个文件含全部字重）。fontconfig、LibreOffice、weasyprint 能认出
其中的命名实例，但 reportlab、fpdf2、matplotlib 只能用到默认那一档，选不了粗体。拆成 Regular / Bold / Light 等
独立文件后，所有工具都按普通字体用，字族名与字重名也和静态字体一致。

除字重以外的轴（opsz、wdth 等）只取一组取值：有落在默认值上的命名实例就取默认值那组，否则取命名实例最多的那组
（DM Sans 的命名实例都不在 opsz 默认值上）。例如 IBM Plex Sans 的 Condensed 宽度、Source Serif 4 的 Display 光学尺寸
不拆，免得字族里冒出一堆同名不同宽的字体。
用法：python3 static-fonts.py <字体目录>；目录里的静态字体原样保留。依赖 fontTools（matplotlib 的依赖，镜像里已有）。
"""

import pathlib
import re
import sys

from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

# 光学尺寸标签（如 “14pt”“18pt”）：带 opsz 轴的字族（DM Sans、Merriweather）拆出来后，STAT 会把它写进字重名，
# 变成 “14pt Bold”。只拆了一种光学尺寸，这个标签没有区分作用，从名字里去掉。
OPSZ_LABEL = re.compile(r"(?<![A-Za-z0-9])\d+pt ?")
# 要清理的 name 表条目：2 字重名、3 唯一标识、4 全名、6 PostScript 名、17 排版用字重名。
NAME_IDS = (2, 3, 4, 6, 17)


def strip_opsz_labels(font: TTFont) -> None:
    """去掉名字里的光学尺寸标签；字重名清空时补成 Regular。"""
    for record in font["name"].names:
        if record.nameID not in NAME_IDS:
            continue
        text = record.toUnicode()
        cleaned = re.sub(r"\s{2,}", " ", OPSZ_LABEL.sub("", text)).strip()
        if record.nameID == 6:
            # PostScript 名没有空格，标签直接贴在字重后面（Merriweather-Light18pt），单独去掉。
            cleaned = re.sub(r"\d+pt", "", text).replace(" ", "")
            if cleaned.endswith("-"):
                cleaned += "Regular"
        if record.nameID in (2, 17) and not cleaned:
            cleaned = "Regular"
        if cleaned != text:
            record.string = cleaned


def split(path: pathlib.Path) -> int:
    """拆一个可变字体，返回生成的静态字体个数；不是可变字体返回 0。"""
    font = TTFont(path)
    if "fvar" not in font:
        return 0
    defaults = {axis.axisTag: axis.defaultValue for axis in font["fvar"].axes}
    instances = [
        {tag: instance.coordinates.get(tag, default) for tag, default in defaults.items()}
        for instance in font["fvar"].instances
    ]
    if not instances:
        raise SystemExit(f"{path.name} 没有命名实例")

    def others(coords: dict) -> tuple:
        """除字重外各轴的取值（取整到千分位，作分组键）。"""
        return tuple(round(value, 3) for tag, value in sorted(coords.items()) if tag != "wght")

    default_key = others(defaults)
    groups: dict[tuple, int] = {}
    for coords in instances:
        groups[others(coords)] = groups.get(others(coords), 0) + 1
    chosen = default_key if default_key in groups else max(groups, key=groups.get)
    made = []
    for coords in instances:
        if others(coords) != chosen:
            continue
        # 每个实例重新读一次原文件：instantiateVariableFont 会改动传入的字体对象。
        static = instantiateVariableFont(TTFont(path), coords, updateFontNames=True)
        strip_opsz_labels(static)
        postscript = static["name"].getDebugName(6)
        if not postscript:
            raise SystemExit(f"{path.name} 的实例 {coords} 没有 PostScript 名")
        target = path.parent / f"{postscript}.ttf"
        static.save(target)
        made.append(target.name)
    if not made:
        raise SystemExit(f"{path.name} 没有可拆的命名实例")
    path.unlink()
    print(f"{path.name} → {len(made)} 个：{' '.join(made)}")
    return len(made)


def main() -> None:
    root = pathlib.Path(sys.argv[1])
    total = sum(split(path) for path in sorted(root.rglob("*.ttf")))
    print(f"共生成 {total} 个静态字体")


if __name__ == "__main__":
    main()
