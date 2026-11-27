# timepipe

Terminal time tracker built with Rust and ratatui.

```
./install.sh
```

Builds a release binary, installs it to `~/.local/bin/timepipe` and adds a **timepipe** launcher to your app menu (opens in your default terminal). Or just `cargo run --release`.

Data lives in `~/.local/share/timepipe/` (`entries.json`, `sheet.xlsx`); nothing is written into the project folder. Exports (`x` / `X`) go to `~/Downloads`.

## Quick input

`project: description #tag #tag2 1h30m`

- project is the part before the colon (defaults to `general`)
- tags start with `#`
- duration: `30m`, `1.5h`, `1h30m` (required for `a`)

## Keys

| key | action |
|---|---|
| s / n | start timer (↑ in the form recalls the last entry) |
| space | pause / resume selected |
| p / P | push selected / all today's tasks to Sheet (.xlsx) |
| a | add past time (needs a duration) |
| e / d / r | edit / delete / repeat selected |
| 1 2 3, Tab | Today / Week / Stats |
| j k g G | move |
| f | time format: `30m (0.50h)` → `1h 30m` → `0.50h` |
| x | export `~/Downloads/timepipe_export.csv` |
| X | export `~/Downloads/timepipe_export.xlsx` |
| q | quit |

## Sheet Tab (Workload file `.xlsx`)

Press `4` to switch to **Sheet** mode. The workbook is stored as `~/.local/share/timepipe/sheet.xlsx`; on first run an `.xlsx` from the current directory is imported once, otherwise press `U` to sync it from Google Drive.

| key | action |
|---|---|
| `←` / `→` (or `h` / `l`) | Select day |
| `↑` / `↓` (or `k` / `j`) | Select task |
| `[` / `]` | Previous / next month |
| `+` / `-` | Quick adjust ±0.5h Fact |
| `f` | Time format (same as on Today): `1.5 (1h30m)` → `1h30m` → `1.5` |
| `U` | Sync workbook from Google Drive |
| `Enter` | Input e.g. `0.5 fix login bug` → writes to local copy **AND copies to clipboard** |
| `y` | Copy (yank) current cell's comment + hours to clipboard |
| `d` | Clear Fact for selected task & day |

### Clipboard formatting
When you submit via `Enter` or press `y`, the formatted string is automatically copied to your clipboard (supporting `wl-copy`, `xclip`, `xsel`, and terminal OSC-52):
- Format: `0.5ч fix login bug`
- In Google Sheets, you just click the cell, press **Shift + F2** (or right-click → Add comment / Add note), and press **Ctrl + V**!

### Auto-sync from Tracker to Sheet
When tracking tasks in Tab 1 (Today):
If your `project:` name (or project + description/tag) matches a task in your sheet:
- When you pause a task (`space`), mark it done (`c`), or add past time (`a`), it automatically updates the corresponding cell in `.timepipe.xlsx` for today and copies formatted hours to clipboard!
- You can also press **`p`** to push the selected task, or **`P`** to push all today's tasks to the sheet.

