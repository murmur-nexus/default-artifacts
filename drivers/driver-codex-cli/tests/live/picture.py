"""Draw a PNG showing a three-digit number on a coloured background.

The picture is what the live image check asks a model to read: the number and the colour are
picked at random unless given, and are written only to --expected (or stdout), never into the
PNG's path or metadata. Standard library only.

Usage: python3 -I picture.py --out <png> [--number NNN] [--background <colour>] [--expected <file>]
"""
import argparse
import binascii
import secrets
import struct
import zlib

WIDTH, HEIGHT = 320, 160
SCALE = 12
GAP = 1  # columns of the font grid between two digits

BACKGROUNDS = {
    "red": (200, 30, 30),
    "green": (30, 150, 50),
    "blue": (30, 60, 200),
    "yellow": (240, 220, 30),
}
WHITE, BLACK = (255, 255, 255), (0, 0, 0)

# A 5x7 bitmap font: seven rows of five columns per digit, '#' lit.
FONT = {
    "0": [".###.", "#...#", "#..##", "#.#.#", "##..#", "#...#", ".###."],
    "1": ["..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###."],
    "2": [".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####"],
    "3": ["#####", "...#.", "..#..", "...#.", "....#", "#...#", ".###."],
    "4": ["...#.", "..##.", ".#.#.", "#..#.", "#####", "...#.", "...#."],
    "5": ["#####", "#....", "####.", "....#", "....#", "#...#", ".###."],
    "6": ["..##.", ".#...", "#....", "####.", "#...#", "#...#", ".###."],
    "7": ["#####", "....#", "...#.", "..#..", ".#...", ".#...", ".#..."],
    "8": [".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###."],
    "9": [".###.", "#...#", "#...#", ".####", "....#", "...#.", ".##.."],
}


def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", binascii.crc32(body) & 0xFFFFFFFF)


def draw(number, background):
    bg = BACKGROUNDS[background]
    fg = BLACK if background == "yellow" else WHITE
    text = f"{number:03d}"
    cols = len(text) * 5 + (len(text) - 1) * GAP
    left = (WIDTH - cols * SCALE) // 2
    top = (HEIGHT - 7 * SCALE) // 2

    def lit(x, y):
        gx, gy = (x - left) // SCALE, (y - top) // SCALE
        if x < left or y < top or gy >= 7 or gx >= cols:
            return False
        digit, col = divmod(gx, 5 + GAP)
        return col < 5 and FONT[text[digit]][gy][col] == "#"

    raw = bytearray()
    for y in range(HEIGHT):
        raw.append(0)  # filter type: none
        for x in range(WIDTH):
            raw.extend(fg if lit(x, y) else bg)
    ihdr = struct.pack(">IIBBBBB", WIDTH, HEIGHT, 8, 2, 0, 0, 0)  # 8-bit RGB
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b""))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", required=True)
    parser.add_argument("--number", type=int)
    parser.add_argument("--background", choices=sorted(BACKGROUNDS))
    parser.add_argument("--expected")
    args = parser.parse_args()

    number = args.number if args.number is not None else 100 + secrets.randbelow(900)
    if not 100 <= number <= 999:
        parser.error("--number must have three digits")
    background = args.background or secrets.choice(sorted(BACKGROUNDS))

    with open(args.out, "wb") as f:
        f.write(draw(number, background))
    line = f"NUMBER={number} BACKGROUND={background}\n"
    if args.expected:
        with open(args.expected, "w") as f:
            f.write(line)
    else:
        print(line, end="")


if __name__ == "__main__":
    main()
