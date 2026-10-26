"""Render the checked-in MouseMic SVG to preview PNG and multi-size Windows ICO.

Only the Python standard library and Pillow are needed. Geometry mirrors mousemic.svg
so distribution builds use the checked-in ICO without invoking Python.
"""

from pathlib import Path
from PIL import Image, ImageDraw

BASE = Path(__file__).parent
SCALE = 4
CANVAS = 512 * SCALE


def px(value):
    return round(value * SCALE)


def box(*values):
    return tuple(px(value) for value in values)


image = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
draw = ImageDraw.Draw(image)

# Render the blue app tile with a top-to-bottom gradient. Rounded corners come
# from a separate alpha mask so every edge is antialiased when downsampled.
mask = Image.new("L", image.size, 0)
ImageDraw.Draw(mask).rounded_rectangle(box(16, 16, 496, 496), radius=px(112), fill=255)
blue = Image.new("RGBA", image.size, (0, 0, 0, 0))
blue_pixels = blue.load()
for y in range(px(16), px(496)):
    t = (y - px(16)) / px(480)
    color = tuple(round(a + (b - a) * t) for a, b in zip((50, 104, 245), (29, 67, 203)))
    for x in range(px(16), px(496)):
        blue_pixels[x, y] = (*color, 255)
image.paste(blue, (0, 0), mask)
draw = ImageDraw.Draw(image)
white = (255, 255, 255, 255)
cut = (36, 82, 221, 255)

# The mouse silhouette doubles as a microphone capsule. At 16px the scroll
# marker remains visible while the outer U-shaped cradle reads as a mic stand.
draw.rounded_rectangle(box(174, 88, 338, 348), radius=px(82), fill=white)
draw.rounded_rectangle(box(245, 120, 267, 182), radius=px(11), fill=cut)
def cubic(start, first, second, end, steps=128):
    points = []
    for i in range(1, steps + 1):
        t = i / steps
        u = 1 - t
        points.append(tuple(u**3 * start[j] + 3 * u**2 * t * first[j]
                            + 3 * u * t**2 * second[j] + t**3 * end[j]
                            for j in range(2)))
    return points


outer = [(130, 258), (130, 306)]
outer += cubic((130, 306), (130, 376), (186, 412), (256, 412))
outer += cubic((256, 412), (326, 412), (382, 376), (382, 306))
outer += [(382, 258)]
draw.line([tuple(map(px, p)) for p in outer], fill=white, width=px(24), joint="curve")
for point in (outer[0], outer[-1]):
    x, y = map(px, point)
    draw.ellipse((x - px(12), y - px(12), x + px(12), y + px(12)), fill=white)
draw.line([tuple(map(px, p)) for p in ((256, 412), (256, 452))], fill=white, width=px(24))
draw.line([tuple(map(px, p)) for p in ((206, 452), (306, 452))], fill=white, width=px(24))
for point in ((206, 452), (306, 452)):
    x, y = map(px, point)
    draw.ellipse((x - px(12), y - px(12), x + px(12), y + px(12)), fill=white)

preview = image.resize((512, 512), Image.Resampling.LANCZOS)
preview.save(BASE / "mousemic.png")
preview.save(
    BASE / "mousemic.ico",
    format="ICO",
    sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
)
print("Generated mousemic.png and mousemic.ico")
