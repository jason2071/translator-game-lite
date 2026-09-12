# Written by Game Translator Lite: replace the game's Latin-only UI fonts.
init -999 python:
    _gtl_thai_font = "tl/None/gtl_thai_font.ttf"
    _gtl_original_fonts = (
        "DejaVuSans.ttf",
        "Jersey10-Regular.ttf",
        "fonts/Jersey10-Regular.ttf",
        "PixelifySans-Medium.ttf",
        "fonts/PixelifySans-Medium.ttf",
    )
    for _gtl_original_font in _gtl_original_fonts:
        for _gtl_bold in (False, True):
            for _gtl_italic in (False, True):
                config.font_replacement_map[
                    _gtl_original_font, _gtl_bold, _gtl_italic
                ] = (_gtl_thai_font, _gtl_bold, _gtl_italic)

init 999 python:
    _gtl_dialogue_prefixes = ("say_", "nvl_")
    for _gtl_style_name, _gtl_style in list(renpy.style.styles.items()):
        if _gtl_style.font in _gtl_original_fonts and _gtl_style.size:
            _gtl_scale = (
                __GTL_DIALOGUE_FONT_SCALE__
                if str(_gtl_style_name).startswith(_gtl_dialogue_prefixes)
                else __GTL_UI_FONT_SCALE__
            )
            _gtl_style.size = int(_gtl_style.size * _gtl_scale)
