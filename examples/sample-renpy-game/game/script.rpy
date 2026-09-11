# Sample Ren'Py project for trying Game Translator Lite.
# Open the folder that contains this README-less structure:
# pick the "sample-renpy-game" folder itself in the app (it has game/ inside).

define e = Character("Eileen")
define m = Character("Master Sylvie")

# define config.name = "Sample Game"

label start:
    scene bg guild hall
    "Welcome to the Guild Hall, [player_name]!"

    e "Hello [player_name]! How are you today?"
    m "Fine. The {b}guild master{/b} is waiting for you."

    menu:
        "I'm going to the guild.":
            e "Great! Follow me."
        "Where are you going?":
            e "To the guild, of course."

    e "See you later."
    return
