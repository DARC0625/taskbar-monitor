"""Original code-drawn gauge icon; requires Pillow only when regenerating."""
from pathlib import Path
from math import cos, sin, radians
from PIL import Image, ImageDraw

out = Path(__file__).resolve().parents[1] / "assets"
size = 1024
im = Image.new("RGBA", (size, size))
d = ImageDraw.Draw(im)
d.rounded_rectangle((24,24,1000,1000), 230, fill="#111e23")
d.arc((203,203,821,821), 135, 405, fill="#29454a", width=64)
d.arc((203,203,821,821), 135, 352, fill="#70eed2", width=64)
for angle in range(135, 406, 27):
    a = radians(angle)
    d.line([(512+cos(a)*378,512+sin(a)*378),(512+cos(a)*401,512+sin(a)*401)], fill="#789b9b", width=12)
a = radians(318)
d.line([(512,512),(512+cos(a)*218,512+sin(a)*218)], fill="#edfff9", width=40)
d.ellipse((475,475,549,549), fill="#edfff9")
d.rounded_rectangle((350,827,674,852), 12, fill="#70eed2")
im.save(out / "taskbar-monitor.ico", sizes=[(16,16),(20,20),(24,24),(32,32),(40,40),(48,48),(64,64),(128,128),(256,256)])
im.resize((256,256), Image.Resampling.LANCZOS).save(out / "taskbar-monitor.png")
