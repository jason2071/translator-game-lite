"""Generate the Game Translator Lite app icon.

Draws a rounded-square tile with an indigo/violet gradient and two chat
bubbles: "A" (source language) and "ก" (Thai target language).

Outputs:
  assets/app.ico             multi-size Windows icon (embedded in the exe)
  ui/assets/app-icon.png     256px PNG used as the Slint window icon
"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
S = 1024          # master canvas
MARGIN = 40       # transparent margin around the tile
RADIUS = 235      # tile corner radius


def lerp(a: int, b: int, t: float) -> int:
    return int(a + (b - a) * t)


def make_tile() -> Image.Image:
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    # Diagonal gradient (indigo -> violet), masked by the rounded tile.
    top, bottom = (79, 70, 229), (139, 68, 237)
    gradient = Image.new("RGBA", (S, S))
    gdraw = ImageDraw.Draw(gradient)
    for y in range(S):
        t = y / S
        gdraw.line(
            [(0, y), (S, y)],
            fill=(
                lerp(top[0], bottom[0], t),
                lerp(top[1], bottom[1], t),
                lerp(top[2], bottom[2], t),
                255,
            ),
        )
        del t

    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [MARGIN, MARGIN, S - MARGIN, S - MARGIN], radius=RADIUS, fill=255
    )
    img.paste(gradient, (0, 0), mask)
    del draw
    return img


def bubble(
    img: Image.Image,
    box: tuple[int, int, int, int],
    tail: list[tuple[int, int]],
    fill: tuple[int, int, int, int],
    radius: int = 88,
) -> None:
    draw = ImageDraw.Draw(img)
    draw.polygon(tail, fill=fill)
    draw.rounded_rectangle(box, radius=radius, fill=fill)


def center_text(
    img: Image.Image,
    text: str,
    font: ImageFont.FreeTypeFont,
    center: tuple[int, int],
    fill: tuple[int, int, int, int],
) -> None:
    draw = ImageDraw.Draw(img)
    draw.text(center, text, font=font, fill=fill, anchor="mm")


def main() -> None:
    img = make_tile()

    # Bubble 1 (white, upper-left) with a tail pointing down-left: "A".
    bubble(
        img,
        box=(118, 168, 566, 560),
        tail=[(212, 520), (330, 520), (196, 690)],
        fill=(255, 255, 255, 242),
    )
    font_a = ImageFont.truetype(r"C:\Windows\Fonts\segoeuib.ttf", 265)
    center_text(img, "A", font_a, (342, 358), (67, 56, 202, 255))

    # Bubble 2 (amber, lower-right) with a tail pointing down-right: "ก".
    bubble(
        img,
        box=(462, 452, 906, 842),
        tail=[(702, 800), (826, 800), (888, 952)],
        fill=(251, 191, 36, 255),
    )
    font_th = ImageFont.truetype(r"C:\Windows\Fonts\tahoma.ttf", 235)
    center_text(img, "ก", font_th, (686, 646), (120, 53, 15, 255))

    out_dir = ROOT / "assets"
    out_dir.mkdir(exist_ok=True)
    ico_path = out_dir / "app.ico"
    img.save(
        ico_path,
        format="ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )

    ui_dir = ROOT / "ui" / "assets"
    ui_dir.mkdir(exist_ok=True)
    img.resize((256, 256), Image.LANCZOS).save(ui_dir / "app-icon.png")

    print(f"wrote {ico_path}")
    print(f"wrote {ui_dir / 'app-icon.png'}")


if __name__ == "__main__":
    main()
