import sqlite3, os

db = os.path.expandvars(r"%APPDATA%\translator-game-lite\data.db")
con = sqlite3.connect(db)
cur = con.cursor()
rows = cur.execute(
    "SELECT source_id, status, translated_text FROM translations "
    "WHERE translated_text IS NOT NULL ORDER BY rowid DESC LIMIT 15"
).fetchall()
for sid, st, t in rows:
    print(st, "|", repr(t[:60]))
print("total:", cur.execute(
    "SELECT COUNT(*) FROM translations WHERE translated_text IS NOT NULL").fetchone()[0])
con.close()
