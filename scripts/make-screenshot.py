#!/usr/bin/env python3
"""Generates a synthetic demo screenshot of ai-commander (no real user data).

Renders the actual UI layout in the actual theme colors as a text grid.
Run:  python scripts/make-screenshot.py   ->  docs/screenshot.png
"""
from PIL import Image, ImageDraw, ImageFont
import os

COLS, ROWS = 104, 31
FONT_PATH = "C:/Windows/Fonts/consola.ttf"
FONT_SIZE = 17
SCALE = 2

# palette (mirrors src/ui/theme.rs)
BG       = (0x00, 0x00, 0x8A)   # NC blue
HEADER   = (0x00, 0x00, 0x6E)   # deep blue (top bar / status bar)
COMPOSER = (0x00, 0x00, 0x5A)   # deepest blue (input zone)
BORDER   = (0x4F, 0xBF, 0xBF)   # cyan frames
FG       = (0xE8, 0xEC, 0xEC)   # near-white
DIM      = (0x8E, 0x9C, 0xB8)
PLACE    = (0xC4, 0xCB, 0xD8)   # placeholder
ACCENT   = (0x55, 0xFF, 0xFF)   # bright cyan
YELLOW   = (0xFF, 0xE1, 0x4D)   # NC yellow
REASON   = (0x93, 0x9B, 0xC8)
CODEBG   = (0x00, 0x00, 0x64)
CODEFG   = (0xC7, 0xD0, 0xE0)

font = ImageFont.truetype(FONT_PATH, FONT_SIZE)
cw = font.getlength("M")            # monospace advance
ch = int(FONT_SIZE * 1.25)

W, H = int(cw * COLS), ch * ROWS
img = Image.new("RGB", (W, H), BG)
d = ImageDraw.Draw(img)

def cell(ch_, x, y, color, bgc=None):
    px = x * cw
    py = y * ch
    if bgc is not None:
        d.rectangle([px, py, px + cw - 1, py + ch - 1], fill=bgc)
    d.text((px, py), ch_, font=font, fill=color)

def span(row, col, text, color, bgc=None):
    for i, c in enumerate(text):
        cell(c, col + i, row, color, bgc)

def fill_row(row, x0, x1, bgc):
    d.rectangle([x0 * cw, row * ch, (x1 + 1) * cw - 1, (row + 1) * ch - 1], fill=bgc)

def hline(row, x0, x1, color, bgc, left="┌", right="┐", title=None):
    cell(left, x0, row, color, bgc)
    for x in range(x0 + 1, x1):
        cell("─", x, row, color, bgc)
    cell(right, x1, row, color, bgc)
    if title:
        t = f" {title} "
        for i, c in enumerate(t):
            cell(c, x0 + 2 + i, row, ACCENT, bgc)

def vlines(row0, row1, x0, x1, color, bgc):
    for y in range(row0, row1 + 1):
        cell("│", x0, y, color, bgc)
        cell("│", x1, y, color, bgc)

# ---------- header (row 0) ----------
fill_row(0, 0, COLS - 1, HEADER)
span(0, 1,  " ■ ", ACCENT, HEADER)
span(0, 4,  "ai-commander", FG, HEADER)
span(0, 17, "  │ OpenCode [oc]", DIM, HEADER)
span(0, 35, "  │ deepseek/deepseek-flash", ACCENT, HEADER)
span(0, 63, " ●", (0x7E, 0xE7, 0x87), HEADER)
span(0, 66, " idle", DIM, HEADER)
span(0, COLS - 16, " » @tsingular ", DIM, HEADER)

SB_X0, SB_X1 = 0, 33            # sidebar frame
CH_X0, CH_X1 = 35, COLS - 1     # chat frame

# ---------- sidebar ----------
hline(1, SB_X0, SB_X1, BORDER, BG, title="History")
vlines(2, ROWS - 3, SB_X0, SB_X1, BORDER, BG)
cell("└", SB_X0, ROWS - 2, BORDER, BG)
for x in range(SB_X0 + 1, SB_X1):
    cell("─", x, ROWS - 2, BORDER, BG)
cell("┘", SB_X1, ROWS - 2, BORDER, BG)

span(2, 2, "+ New chat", YELLOW, BG)
span(2, 14, " Ctrl+N", DIM, BG)

span(4, 2, "[oc] OpenCode", ACCENT, BG)
span(4, 16, " ►", ACCENT, BG)
span(5, 3, "Refactor parser", FG, BG)
span(5, 25, "12:31", DIM, BG)
span(6, 3, "Fix CI pipeline", FG, BG)
span(6, 25, "11:02", DIM, BG)
span(7, 3, "Docs: API notes", FG, BG)
span(7, 25, "friday", DIM, BG)
span(8, 3, "Deploy script", FG, BG)
span(8, 25, "thursday", DIM, BG)

# profile block
cell("─", SB_X0 + 1, ROWS - 5, BORDER, BG)
for x in range(SB_X0 + 2, SB_X1):
    cell("─", x, ROWS - 5, BORDER, BG)
span(ROWS - 4, 2, " » ", DIM, BG)
span(ROWS - 4, 5, "@tsingular", FG, BG)
span(ROWS - 3, 2, " • Settings ", FG, BG)
span(ROWS - 3, 14, "F2", ACCENT, BG)
span(ROWS - 3, 17, " ds-flash", DIM, BG)

# ---------- chat ----------
hline(1, CH_X0, CH_X1, BORDER, BG, title="Refactor parser — idle")
vlines(2, 21, CH_X0, CH_X1, BORDER, BG)

r = 2
span(r, CH_X0 + 2, "▐", YELLOW, BG); span(r, CH_X0 + 4, "You", YELLOW, BG); r += 1
span(r, CH_X0 + 3, "why does the tokenizer drop escaped quotes?", FG, BG); r += 2

span(r, CH_X0 + 2, "▐", ACCENT, BG); span(r, CH_X0 + 4, "Assistant", ACCENT, BG); r += 1
span(r, CH_X0 + 3, "· scanning the lexer edge cases first…", REASON, BG); r += 1
span(r, CH_X0 + 3, "√ read", YELLOW, BG)
span(r, CH_X0 + 10, "src/lexer.rs", FG, BG); r += 1
span(r, CH_X0 + 3, "Found it: split_once('=') breaks on escaped separators.", FG, BG); r += 1
span(r, CH_X0 + 3, "Patched with a tiny state machine:", FG, BG); r += 1
span(r, CH_X0 + 3, "╭─ rust", DIM, BG); r += 1
code_lines = [
    "│ while let Some(q) = iter.next() {",
    "│     if q == '\\\\' { escaped = !escaped; }",
    "│     if q == '=' && !escaped { break; }",
    "╰─",
]
for cl in code_lines:
    fill_row(r, CH_X0 + 2, CH_X1 - 3, CODEBG)
    for i, c in enumerate(cl):
        cell(c, CH_X0 + 3 + i, r, CODEFG, CODEBG)
    r += 1
r += 1
span(r, CH_X0 + 3, "All 14 tests pass — ready for review.", FG, BG)

# chat bottom border
cell("└", CH_X0, 22, BORDER, BG)
for x in range(CH_X0 + 1, CH_X1):
    cell("─", x, 22, BORDER, BG)
cell("┘", CH_X1, 22, BORDER, BG)

# ---------- composer (deep blue zone) ----------
for y in range(23, ROWS - 2):
    fill_row(y, CH_X0, CH_X1, COMPOSER)
cell("┌", CH_X0, 23, BORDER, COMPOSER)
for x in range(CH_X0 + 1, CH_X1):
    cell("─", x, 23, BORDER, COMPOSER)
cell("┐", CH_X1, 23, BORDER, COMPOSER)
vlines(24, ROWS - 3, CH_X0, CH_X1, BORDER, COMPOSER)
cell("└", CH_X0, ROWS - 2, BORDER, COMPOSER)
for x in range(CH_X0 + 1, CH_X1):
    cell("─", x, ROWS - 2, BORDER, COMPOSER)
cell("┘", CH_X1, ROWS - 2, BORDER, COMPOSER)

span(23, CH_X0 + 2, " @ demo.png (12.3k)", YELLOW, COMPOSER)
span(23, CH_X0 + 23, " Ctrl+D x", DIM, COMPOSER)
span(24, CH_X0 + 2, " Type a message…", PLACE, COMPOSER)
span(26, CH_X0 + 2, " Enter to send · Alt+Enter = newline", DIM, COMPOSER)

# ---------- status bar ----------
fill_row(ROWS - 1, 0, COLS - 1, HEADER)
keys = [("F1", "Help"), ("F2", "Settings"), ("^N", "New"), ("^K", "Model"),
        ("^P", "File"), ("^H", "Harness"), ("^O", "Panels"), ("F10", "Quit")]
x = 1
for i, (k, label) in enumerate(keys):
    if i:
        span(ROWS - 1, x, "│ ", (0x2E, 0x74, 0x8F), HEADER); x += 2
    span(ROWS - 1, x, f" {k} ", YELLOW, HEADER); x += 2 + len(k)
    span(ROWS - 1, x, label, FG, HEADER); x += len(label) + 1

# upscale and save
img = img.resize((W * SCALE, H * SCALE), Image.LANCZOS)
out = os.path.join(os.path.dirname(__file__), "..", "docs", "screenshot.png")
os.makedirs(os.path.dirname(out), exist_ok=True)
img.save(out)
print("saved", os.path.normpath(out), img.size)
