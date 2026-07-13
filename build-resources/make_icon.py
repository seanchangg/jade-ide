#!/usr/bin/env python3
"""Generate Jade's app icon: a rounded-square jade gem with a soft radial
gradient, a diagonal facet highlight, and a subtle inner bevel. Renders at
1024px, then builds the macOS .icns via iconutil."""
import math, os, subprocess
from PIL import Image, ImageDraw, ImageFilter

S = 1024
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
draw = ImageDraw.Draw(img)

# macOS-style rounded superellipse-ish square with margin.
margin = int(S * 0.11)
radius = int(S * 0.225)
box = [margin, margin, S - margin, S - margin]

# Base rounded rect (dark jade) as a mask.
mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle(box, radius=radius, fill=255)

# Radial gradient: bright jade center → deep jade edges.
grad = Image.new("RGBA", (S, S), (0, 0, 0, 0))
gpix = grad.load()
cx, cy = S * 0.42, S * 0.38          # light source upper-left
maxd = math.hypot(S, S) * 0.62
# jade palette
hi = (120, 214, 168)                  # bright jade highlight
mid = (54, 156, 110)                  # core jade (matches --chart-1 family)
lo = (18, 74, 54)                     # deep shadow jade
for y in range(S):
    for x in range(S):
        d = min(1.0, math.hypot(x - cx, y - cy) / maxd)
        if d < 0.5:
            t = d / 0.5
            r = int(hi[0] + (mid[0] - hi[0]) * t)
            g = int(hi[1] + (mid[1] - hi[1]) * t)
            b = int(hi[2] + (mid[2] - hi[2]) * t)
        else:
            t = (d - 0.5) / 0.5
            r = int(mid[0] + (lo[0] - mid[0]) * t)
            g = int(mid[1] + (lo[1] - mid[1]) * t)
            b = int(mid[2] + (lo[2] - mid[2]) * t)
        gpix[x, y] = (r, g, b, 255)

img = Image.composite(grad, img, mask)

# Diagonal facet highlight (a translucent quadrilateral, blurred).
facet = Image.new("RGBA", (S, S), (0, 0, 0, 0))
fd = ImageDraw.Draw(facet)
fd.polygon(
    [(margin, int(S * 0.30)), (int(S * 0.62), margin),
     (int(S * 0.50), int(S * 0.52)), (margin, int(S * 0.66))],
    fill=(255, 255, 255, 60),
)
facet = facet.filter(ImageFilter.GaussianBlur(24))
facet = Image.composite(facet, Image.new("RGBA", (S, S), (0, 0, 0, 0)), mask)
img = Image.alpha_composite(img, facet)

# Inner bevel: bright top edge + dark bottom edge along the rounded border.
bevel = Image.new("RGBA", (S, S), (0, 0, 0, 0))
bd = ImageDraw.Draw(bevel)
bd.rounded_rectangle([b + 6 for b in box[:2]] + [box[2] - 6, box[3] - 6],
                     radius=radius - 6, outline=(255, 255, 255, 70), width=6)
bevel = bevel.filter(ImageFilter.GaussianBlur(4))
bevel = Image.composite(bevel, Image.new("RGBA", (S, S), (0, 0, 0, 0)), mask)
img = Image.alpha_composite(img, bevel)

# Engraved "J" monogram, subtly darker for a carved look.
mono = Image.new("RGBA", (S, S), (0, 0, 0, 0))
md = ImageDraw.Draw(mono)
# Draw a chunky J with rectangles + arc.
jx, jw = int(S * 0.545), int(S * 0.055)
md.rectangle([jx, int(S * 0.33), jx + jw, int(S * 0.60)], fill=(10, 50, 36, 120))
md.rectangle([int(S * 0.40), int(S * 0.33), jx + jw, int(S * 0.385)], fill=(10, 50, 36, 120))
md.arc([int(S * 0.36), int(S * 0.50), jx + jw, int(S * 0.70)],
       start=20, end=170, fill=(10, 50, 36, 140), width=jw)
mono = mono.filter(ImageFilter.GaussianBlur(2))
mono = Image.composite(mono, Image.new("RGBA", (S, S), (0, 0, 0, 0)), mask)
img = Image.alpha_composite(img, mono)

here = os.path.dirname(os.path.abspath(__file__))
png = os.path.join(here, "icon-1024.png")
img.save(png)
print("wrote", png)

# Build .iconset then .icns.
iconset = os.path.join(here, "Jade.iconset")
os.makedirs(iconset, exist_ok=True)
specs = [(16, "16x16"), (32, "16x16@2x"), (32, "32x32"), (64, "32x32@2x"),
         (128, "128x128"), (256, "128x128@2x"), (256, "256x256"),
         (512, "256x256@2x"), (512, "512x512"), (1024, "512x512@2x")]
for size, name in specs:
    img.resize((size, size), Image.LANCZOS).save(
        os.path.join(iconset, f"icon_{name}.png"))
icns = os.path.join(here, "icon.icns")
subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns], check=True)
print("wrote", icns)
