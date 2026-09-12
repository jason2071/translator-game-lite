# Written by Game Translator Lite for Nothing Weird Happens Here.
# This screen override only adjusts the game's inline HUD and quest sizes.

screen tooltip_display():
    if tooltip_text:
        frame:
            xpos 150
            ypos 70
            text tooltip_text color "#d2d0dd" size int(44 * __GTL_HUD_FONT_SCALE__)
            background Solid("#00000038")
            padding (10, 5)

screen day_time_display():
    frame:
        xpos 900
        ypos 20
        xsize 600
        ysize 70
        background Solid("#22222217")
        padding (20, 10)
        vbox:
            text "Day [day_number] ([days_of_week[day_number % 7]]), [parts_of_day[part_of_day]]" color "#fff" size int(40 * __GTL_HUD_FONT_SCALE__) outlines [(2, "#000000", 0, 0)]

screen quest_log():
    modal True
    add "bg phone 2.png"

    key "dismiss" action [SetVariable("selected_quest_name", ""), Hide("quest_log"), Show("phone_screen")]

    imagebutton:
        idle "button/p close.png"
        hover "button/p close h.png"
        action [SetVariable("selected_quest_name", ""), Hide("quest_log"), Show("phone_screen")]
        focus_mask True

    frame:
        background None
        xalign 0.5
        yalign 0.5

        has hbox
        spacing 6

        frame:
            background Frame(Solid("#1a1a2e1e"), 10, 10)
            xminimum 230
            xmaximum 230
            yminimum 720
            ymaximum 720
            padding (8, 10, 8, 10)

            has vbox
            spacing 4

            text "Quest Log":
                size int(30 * __GTL_QUEST_FONT_SCALE__)
                xalign 0.5
                color "#7daad4"
                outlines [(2, "#000000", 0, 0)]

            null height 4
            frame:
                background Solid("#7daad4aa")
                xfill True
                yminimum 2
                ymaximum 2
            null height 4

            viewport:
                mousewheel True
                draggable True
                xfill True
                ymaximum 700

                has vbox
                spacing 2

                for quest in quest_journal:
                    if not (quest.name == "Aunt Housework" and quest.completed):
                        if quest.should_show and not quest.completed:
                            $ _qn = quest.name
                            $ _sel = (selected_quest_name == _qn)
                            $ _col = "#ffe066" if _sel else "#ffffff"
                            $ _bg = "#2c3e6618" if _sel else "#ffffff11"
                            button:
                                background Frame(Solid(_bg), 6, 6)
                                xfill True
                                padding (8, 10, 8, 10)
                                action SetVariable("selected_quest_name", _qn)
                                has hbox
                                spacing 6
                                text "›":
                                    size int(30 * __GTL_QUEST_FONT_SCALE__)
                                    color "#7daad4"
                                    yalign 0.5
                                text _qn:
                                    size int(26 * __GTL_QUEST_FONT_SCALE__)
                                    color _col
                                    outlines [(1, "#000000", 0, 0)]
                                    yalign 0.5

                null height 6
                frame:
                    background Solid("#7daad433")
                    xfill True
                    yminimum 1
                    ymaximum 1
                null height 6

                for quest in quest_journal:
                    if quest.should_show and quest.completed:
                        $ _qn = quest.name
                        $ _sel = (selected_quest_name == _qn)
                        $ _col = "#ffe066" if _sel else "#90EE90"
                        $ _bg = "#2c3e6618" if _sel else "#ffffff11"
                        button:
                            background Frame(Solid(_bg), 6, 6)
                            xfill True
                            padding (8, 10, 8, 10)
                            action SetVariable("selected_quest_name", _qn)
                            has hbox
                            spacing 6
                            text "+":
                                size int(26 * __GTL_QUEST_FONT_SCALE__)
                                color "#90EE90"
                                yalign 0.5
                            text _qn:
                                size int(26 * __GTL_QUEST_FONT_SCALE__)
                                color _col
                                outlines [(1, "#000000", 0, 0)]
                                yalign 0.5

        frame:
            background Frame(Solid("#12122a1f"), 10, 10)
            xminimum 310
            xmaximum 310
            yminimum 720
            ymaximum 720
            padding (14, 12, 14, 12)

            has vbox
            spacing 6

            $ _sq = None
            for quest in quest_journal:
                if quest.name == selected_quest_name:
                    $ _sq = quest

            if _sq is not None:
                $ _hcol = "#90EE90" if _sq.completed else "#7daad4"
                text _sq.name:
                    size int(32 * __GTL_QUEST_FONT_SCALE__)
                    xalign 0.5
                    color _hcol
                    outlines [(2, "#000000", 0, 0)]
                    text_align 0.5

                null height 2
                text _sq.description:
                    size int(26 * __GTL_QUEST_FONT_SCALE__)
                    xalign 0.5
                    color "#aaaacc"
                    outlines [(1, "#000000", 0, 0)]
                    text_align 0.5

                null height 4
                frame:
                    background Solid("#7daad466")
                    xfill True
                    yminimum 2
                    ymaximum 2
                null height 6

                if _sq.completed:
                    null height 180
                    text "+  Quest Completed!":
                        size int(26 * __GTL_QUEST_FONT_SCALE__)
                        xalign 0.5
                        color "#90EE90"
                        outlines [(1, "#000000", 0, 0)]
                else:
                    viewport:
                        mousewheel True
                        draggable True
                        xfill True
                        ymaximum 700
                        has vbox
                        spacing 5
                        $ _sorted_objectives = sorted(
                            [(i, obj) for i, obj in enumerate(_sq.objectives) if obj["visible"]],
                            key=lambda x: (x[1]["completed"] or x[1]["failed"])
                        )
                        for i, objective in _sorted_objectives:
                            if objective["failed"]:
                                $ _sym = "−"
                                $ _tc = "#ff5c5c"
                            elif objective["completed"]:
                                $ _sym = "+"
                                $ _tc = "#90EE90"
                            else:
                                $ _sym = "•"
                                $ _tc = "#dddddd"
                            frame:
                                background Solid("#ffffff0c")
                                xfill True
                                padding (8, 5, 8, 5)
                                has hbox
                                spacing 8
                                text _sym:
                                    size int(26 * __GTL_QUEST_FONT_SCALE__)
                                    color _tc
                                    yalign 0.0
                                text objective["text"]:
                                    size int(26 * __GTL_QUEST_FONT_SCALE__)
                                    color _tc
                                    outlines [(1, "#000000", 0, 0)]
                                    yalign 0.0
            else:
                null height 200
                text "← Select a quest":
                    size int(26 * __GTL_QUEST_FONT_SCALE__)
                    xalign 0.5
                    color "#44445a"
                    text_align 0.5
