#!/usr/bin/env python3
"""Real-font verification helper for the veil-render sub-cell renderers.

  render_real_font.py sample OUT.png            # synthetic 1920x1080 UI-text source image
  render_real_font.py grid GRID.txt OUT.png     # draw a `braille_preview --dump` grid with a real font

`grid` is the point of this script: glyph *geometry* (braille_preview --png) says
what the renderer meant, a real font says what a terminal would actually show.
That's how braille was caught rendering as literal dots in JetBrainsMono Nerd Font.
"""
import argparse
import subprocess
import sys

from PIL import Image, ImageDraw, ImageFont


def default_font() -> str:
    out = subprocess.run(
        ["fc-match", "-f", "%{file}", "JetBrainsMono Nerd Font"],
        capture_output=True, text=True,
    ).stdout.strip()
    if not out:
        sys.exit("no JetBrainsMono Nerd Font found via fc-match; pass --font PATH")
    return out


def cmd_sample(a: argparse.Namespace) -> None:
    img = Image.new("RGB", (1920, 1080), (26, 15, 46))
    d = ImageDraw.Draw(img)
    y = 40
    for size in (14, 16, 18, 24, 32, 48):
        f = ImageFont.truetype(a.font, size)
        d.text((60, y), f"{size}px  The quick brown fox jumps over the lazy dog 0123456789", font=f, fill=(230, 230, 240))
        y += size + 28
    d.rectangle((60, y + 20, 700, y + 140), outline=(138, 107, 196), width=3)
    d.text((80, y + 60), "[ Button ]   File  Edit  View", font=ImageFont.truetype(a.font, 24), fill=(138, 107, 196))
    img.save(a.out)
    print(f"wrote {a.out}")


def cmd_grid(a: argparse.Namespace) -> None:
    src = sys.stdin if a.grid == "-" else open(a.grid, encoding="utf-8")
    lines = src.read().splitlines()
    font = ImageFont.truetype(a.font, a.size)
    ascent, descent = font.getmetrics()
    cw, ch = round(font.getlength("M")), ascent + descent
    img = Image.new("RGB", (max(map(len, lines), default=1) * cw, len(lines) * ch), (0, 0, 0))
    d = ImageDraw.Draw(img)
    for row, line in enumerate(lines):
        for col, c in enumerate(line):
            if c != " ":
                d.text((col * cw, row * ch), c, font=font, fill=(230, 230, 230))
    img.save(a.out)
    print(f"wrote {a.out} ({img.width}x{img.height}, cell {cw}x{ch}, font {a.font})")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--font", default=None, help="TTF path (default: fc-match JetBrainsMono Nerd Font)")
    p.add_argument("--size", type=int, default=16, help="font size for `grid` (default 16)")
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("sample"); s.add_argument("out"); s.set_defaults(fn=cmd_sample)
    g = sub.add_parser("grid"); g.add_argument("grid", help="grid text file, or - for stdin"); g.add_argument("out"); g.set_defaults(fn=cmd_grid)
    a = p.parse_args()
    a.font = a.font or default_font()
    a.fn(a)


if __name__ == "__main__":
    main()
