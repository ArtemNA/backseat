"""Builds crates/app/icons/icon.ico (16..256 px) and icon.png (512 px) from a square
source picture of the app tile (transparent or plain light background around it): crops to the tile, makes
everything outside its rounded corners transparent, and downsamples with a sharpening
step for the small sizes.

usage: python tools/make-icon.py crates/app/icons/source.png
"""
import sys
from PIL import Image, ImageDraw, ImageFilter
import numpy as np

src = Image.open(sys.argv[1]).convert("RGBA")
a = np.asarray(src).astype(np.int32)
# The tile: opaque pixels (a transparent background), or clearly darker than a white one.
tile = (a[:, :, 3] > 128) if a[:, :, 3].min() < 128 else (a[:, :, :3].sum(axis=2) < 600)
ys, xs = np.nonzero(tile)
box = (xs.min(), ys.min(), xs.max() + 1, ys.max() + 1)
side = max(box[2] - box[0], box[3] - box[1])
cx, cy = (box[0] + box[2]) / 2, (box[1] + box[3]) / 2
crop = src.crop((int(cx - side / 2), int(cy - side / 2), int(cx + side / 2), int(cy + side / 2)))

# Rounded-square mask, drawn large and scaled down for smooth corners. The radius matches
# the tile's (about 22% of its side).
S = 2048
mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle((0, 0, S - 1, S - 1), radius=int(S * 0.22), fill=255)
big = crop.resize((S, S), Image.LANCZOS)
# Keep the picture's own transparency and cut it to the rounded square as well.
alpha = np.minimum(np.asarray(big)[:, :, 3], np.asarray(mask))
big.putalpha(Image.fromarray(alpha))

out = "crates/app/icons/"
big.resize((512, 512), Image.LANCZOS).save(out + "icon.png")
sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
frames = []
for s in sizes:
    im = big.resize((s, s), Image.LANCZOS)
    if s <= 32:
        im = im.filter(ImageFilter.UnsharpMask(radius=0.6, percent=60, threshold=0))
    frames.append(im)
frames[-1].save(out + "icon.ico", sizes=[(s, s) for s in sizes], append_images=frames[:-1])
# A preview strip of the small sizes on dark and light backgrounds, for checking.
strip = Image.new("RGBA", (sum(sizes[:6]) + 70, 120), (49, 51, 56, 255))
x = 10
for s, im in zip(sizes[:6], frames[:6]):
    strip.alpha_composite(im, (x, 10))
    light = Image.new("RGBA", (s, s), (240, 240, 240, 255))
    light.alpha_composite(im)
    strip.alpha_composite(light, (x, 60))
    x += s + 10
strip.resize((strip.width * 3, strip.height * 3), Image.NEAREST).save(".scratch/icon-preview.png")
print("tile", box, "->", [im.size for im in frames])
