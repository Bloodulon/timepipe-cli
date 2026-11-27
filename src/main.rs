use std::{collections::BTreeMap, fs, io, path::PathBuf, time::Duration as StdDuration};

use chrono::{DateTime, Duration, Local, NaiveDate};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Tabs,
    },
    Frame,
};
use serde::{Deserialize, Serialize};

mod sheet;
mod sync;

const DAILY_GOAL_MIN: i64 = 8 * 60;
const ACCENT: Color = Color::Rgb(137, 180, 250);
const GREEN: Color = Color::Rgb(166, 227, 161);
const YELLOW: Color = Color::Rgb(249, 226, 175);
const RED: Color = Color::Rgb(243, 139, 168);
const DIM: Color = Color::Rgb(108, 112, 134);
const SURFACE: Color = Color::Rgb(49, 50, 68);
const PALETTE: [Color; 7] = [
    Color::Rgb(137, 180, 250),
    Color::Rgb(166, 227, 161),
    Color::Rgb(250, 179, 135),
    Color::Rgb(203, 166, 247),
    Color::Rgb(245, 194, 231),
    Color::Rgb(148, 226, 213),
    Color::Rgb(249, 226, 175),
];

// ───────────────────────── data ─────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    project: String,
    description: String,
    tags: Vec<String>,
    start: DateTime<Local>,
    end: Option<DateTime<Local>>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    accumulated_sec: i64,
    #[serde(default)]
    first_start: Option<DateTime<Local>>,
}

impl Entry {
    fn duration(&self) -> Duration {
        if self.running() {
            Duration::seconds(self.accumulated_sec) + (Local::now() - self.start)
        } else if self.accumulated_sec > 0 {
            Duration::seconds(self.accumulated_sec)
        } else {
            self.end.unwrap_or_else(Local::now) - self.start
        }
    }
    fn running(&self) -> bool {
        self.end.is_none()
    }
    fn started_at(&self) -> DateTime<Local> {
        self.first_start.unwrap_or(self.start)
    }
    fn as_input(&self) -> String {
        let mut s = format!("{}: {}", self.project, self.description);
        for t in &self.tags {
            s.push_str(&format!(" #{}", t));
        }
        s
    }
    fn as_edit_input(&self) -> String {
        let mut s = self.as_input();
        if let Some(dur) = fmt_dur_compact(self.duration()) {
            if !s.ends_with(' ') {
                s.push(' ');
            }
            s.push_str(&dur);
        }
        s
    }
}

fn data_path() -> PathBuf {
    let mut p = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("timepipe");
    let _ = fs::create_dir_all(&p);
    p.push("entries.json");
    p
}

/// Exports go to ~/Downloads (or home), never into the current directory.
fn export_path(name: &str) -> PathBuf {
    let mut p = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));
    p.push(name);
    p
}

fn project_color(name: &str) -> Color {
    let h = name.bytes().fold(7usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize));
    PALETTE[h % PALETTE.len()]
}

// ───────────────────── parsing & formatting ─────────────────────

struct Parsed {
    project: String,
    description: String,
    tags: Vec<String>,
    dur: Option<Duration>,
}

/// "1h30m", "1.5h", "30m", "1ч30м", "0.5ч", "0.5", "1,5" -> Duration
fn parse_dur(t: &str) -> Option<Duration> {
    let t = t.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }

    // Normalize Russian and English unit words to 'h' and 'm'
    let s = t
        .replace("часов", "h")
        .replace("часа", "h")
        .replace("час", "h")
        .replace('ч', "h")
        .replace("hours", "h")
        .replace("hour", "h")
        .replace("hrs", "h")
        .replace("hr", "h")
        .replace("минут", "m")
        .replace("минуты", "m")
        .replace("минута", "m")
        .replace("мин", "m")
        .replace('м', "m")
        .replace("minutes", "m")
        .replace("minute", "m")
        .replace("mins", "m")
        .replace("min", "m");

    let mut total_mins = 0.0f64;
    let mut num = String::new();
    let mut any_unit = false;

    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' || c == ',' {
            num.push(if c == ',' { '.' } else { c });
        } else if c == 'h' || c == 'm' {
            let v: f64 = num.parse().ok()?;
            total_mins += if c == 'h' { v * 60.0 } else { v };
            num.clear();
            any_unit = true;
        } else if c.is_whitespace() {
            continue;
        } else {
            return None;
        }
    }

    if any_unit && num.is_empty() {
        return Some(Duration::seconds((total_mins * 60.0).round() as i64));
    }

    // If no unit, but string has decimal point (e.g. "0.5", "1.5", "0,5")
    if !any_unit && !num.is_empty() {
        if (num.contains('.') || num.contains(',')) && num.parse::<f64>().is_ok() {
            let v: f64 = num.parse().unwrap();
            if v > 0.0 {
                return Some(Duration::seconds((v * 3600.0).round() as i64));
            }
        }
    }

    None
}

/// "project: description #tag #tag2 1h30m"
fn parse_input(line: &str) -> Parsed {
    let line = line.trim();
    let (project, rest) = match line.split_once(':') {
        Some((p, r)) if !p.contains('#') && !p.trim().is_empty() => (p.trim().to_string(), r),
        _ => ("general".to_string(), line),
    };
    let mut words = Vec::new();
    let mut tags = Vec::new();
    let mut dur: Option<Duration> = None;
    for tok in rest.split_whitespace() {
        if let Some(tag) = tok.strip_prefix('#') {
            if !tag.is_empty() {
                tags.push(tag.to_string());
                continue;
            }
        }
        if let Some(d) = parse_dur(tok) {
            dur = Some(dur.unwrap_or_else(Duration::zero) + d);
            continue;
        }
        words.push(tok);
    }

    // If dur is still None, and words has a single standalone number (e.g. "1" or "2" in "Полигон: 1 #баги")
    if dur.is_none() && words.len() == 1 {
        if let Ok(v) = words[0].replace(',', ".").parse::<f64>() {
            if v > 0.0 && v <= 24.0 {
                dur = Some(Duration::seconds((v * 3600.0).round() as i64));
                words.clear();
            }
        }
    }

    Parsed { project, description: words.join(" "), tags, dur }
}

#[derive(Clone, Copy)]
enum Fmt {
    Both,
    HoursMin,
    Decimal,
}

impl Fmt {
    fn next(self) -> Fmt {
        match self {
            Fmt::Both => Fmt::HoursMin,
            Fmt::HoursMin => Fmt::Decimal,
            Fmt::Decimal => Fmt::Both,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Fmt::Both => "30m (0.50h)",
            Fmt::HoursMin => "1h 30m",
            Fmt::Decimal => "0.50h",
        }
    }
}

fn fmt_dur(d: Duration, f: Fmt) -> String {
    let mins = d.num_seconds().max(0) / 60;
    let hm = if mins >= 60 { format!("{}h {:02}m", mins / 60, mins % 60) } else { format!("{}m", mins) };
    let dec = format!("{:.2}h", mins as f64 / 60.0);
    match f {
        Fmt::Both => format!("{} ({})", hm, dec),
        Fmt::HoursMin => hm,
        Fmt::Decimal => dec,
    }
}

fn fmt_dur_compact(d: Duration) -> Option<String> {
    let mins = d.num_seconds().max(0) / 60;
    if mins == 0 {
        return None;
    }
    let h = mins / 60;
    let m = mins % 60;
    Some(match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m}m"),
    })
}

fn fmt_clock(d: Duration) -> String {
    let s = d.num_seconds().max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

// ───────────────────────── app ─────────────────────────

enum Mode {
    Normal,
    Start,
    Manual,
    Edit(usize),
    SheetEdit,
    ConfirmDelete(usize),
}

struct App {
    entries: Vec<Entry>,
    view: usize, // 0 today, 1 week, 2 stats, 3 sheet
    sel: usize,
    mode: Mode,
    input: String,
    cursor: usize,
    fmt: [Fmt; 4],
    status: String,
    error: bool,
    quit: bool,
    sheet: Option<sheet::Sheet>,
    sheet_err: String,
}

impl App {
    fn load() -> App {
        let entries = fs::read_to_string(data_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let (sheet, sheet_err) = open_sheet();
        App {
            entries,
            view: 0,
            sel: 0,
            mode: Mode::Normal,
            input: String::new(),
            cursor: 0,
            fmt: [Fmt::Both; 4],
            status: String::new(),
            error: false,
            quit: false,
            sheet,
            sheet_err,
        }
    }

    fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(&self.entries) {
            let _ = fs::write(data_path(), s);
        }
    }

    fn say(&mut self, s: impl Into<String>) {
        self.status = s.into();
        self.error = false;
    }

    fn cur_fmt(&self) -> Fmt {
        self.fmt[self.view.min(3)]
    }

    fn cycle_fmt(&mut self) {
        let idx = self.view.min(3);
        self.fmt[idx] = self.fmt[idx].next();
        self.say(format!("format: {}", self.fmt[idx].name()));
    }

    fn visible(&self) -> Vec<usize> {
        let today = Local::now().date_naive();
        let from = if self.view == 0 { today } else { today - Duration::days(6) };
        let mut v: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                e.started_at().date_naive() >= from || e.start.date_naive() >= from
            })
            .collect();
        // open tasks first, then done; newest first inside each group
        v.sort_by_key(|&i| (self.entries[i].done, std::cmp::Reverse(self.entries[i].started_at())));
        v
    }

    fn clamp(&mut self) {
        let n = self.visible().len();
        if n == 0 {
            self.sel = 0;
        } else if self.sel >= n {
            self.sel = n - 1;
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.visible().get(self.sel).copied()
    }

    fn stop(&mut self) -> (bool, Option<usize>) {
        let mut stopped = false;
        let mut last_idx = None;
        let now = Local::now();
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.running() {
                e.accumulated_sec += (now - e.start).num_seconds().max(0);
                e.end = Some(now);
                stopped = true;
                last_idx = Some(i);
            }
        }
        (stopped, last_idx)
    }

    fn sync_entry_to_sheet(&mut self, entry_idx: usize, verbose: bool) -> bool {
        let Some(e) = self.entries.get(entry_idx).cloned() else {
            return false;
        };
        let Some(sh) = self.sheet.as_mut() else {
            if verbose {
                self.status = "no .xlsx sheet loaded".into();
                self.error = true;
            }
            return false;
        };

        let row = match sh.find_task(&e.project, &e.description, &e.tags) {
            Some(r) => r,
            None => {
                if verbose {
                    self.status = format!("project '{}' not found in sheet", e.project);
                    self.error = true;
                }
                return false;
            }
        };

        let entry_date = e.started_at().date_naive();
        let day_idx = match sh.days.iter().position(|d| d.date == entry_date) {
            Some(idx) => idx,
            None => {
                if verbose {
                    self.status = format!("date {} not found in current sheet month", entry_date.format("%d.%m"));
                    self.error = true;
                }
                return false;
            }
        };

        let total_secs: i64 = self
            .entries
            .iter()
            .filter(|x| {
                x.started_at().date_naive() == entry_date
                    && x.project.trim().to_lowercase() == e.project.trim().to_lowercase()
            })
            .map(|x| x.duration().num_seconds())
            .sum();

        let hours = (total_secs as f64 / 3600.0 * 100.0).round() / 100.0;
        if hours < 0.01 && !verbose {
            return false;
        }

        let mut notes: Vec<String> = self
            .entries
            .iter()
            .filter(|x| {
                x.started_at().date_naive() == entry_date
                    && x.project.trim().to_lowercase() == e.project.trim().to_lowercase()
            })
            .map(|x| {
                let mins = x.duration().num_minutes();
                let dur = if mins >= 60 {
                    format!("{}h {:02}m", mins / 60, mins % 60)
                } else if mins > 0 {
                    format!("{}m", mins)
                } else {
                    String::new()
                };
                let mut text = x.description.clone();
                for t in &x.tags {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(&format!("#{t}"));
                }
                if dur.is_empty() {
                    text
                } else if text.is_empty() {
                    dur
                } else {
                    format!("{} {}", dur, text)
                }
            })
            .filter(|s| !s.trim().is_empty())
            .collect();
        notes.dedup();
        let comment = if notes.is_empty() {
            None
        } else {
            Some(notes.join("; "))
        };

        let proj_name = sh.text(row, 2).to_string();
        let task_name = sh.text(row, 4).to_string();

        match sh.set_fact_at(row, day_idx, Some(hours), false, comment.clone()) {
            Ok(_) => {
                let clip = clip_text(Some(hours), comment.as_deref().unwrap_or(""));
                let via = copy_clipboard(&clip);
                self.say(format!("✔ sheet: {}h → {} · {} · copied ({via})", h1(hours), proj_name, task_name));
                true
            }
            Err(err) => {
                if verbose {
                    self.status = format!("sheet sync error: {err}");
                    self.error = true;
                }
                false
            }
        }
    }

    fn restore_entries_to_sheet(&mut self) -> usize {
        let mut seen = std::collections::HashSet::new();
        let mut indices = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            let date = e.started_at().date_naive();
            let proj = e.project.trim().to_lowercase();
            if proj.is_empty() || proj == "general" {
                continue;
            }
            if seen.insert((date, proj)) {
                indices.push(i);
            }
        }
        let mut restored = 0;
        for i in indices {
            let e = &self.entries[i];
            let entry_date = e.started_at().date_naive();
            let should_restore = if let Some(sh) = &self.sheet {
                if let (Some(row), Some(day_idx)) = (
                    sh.find_task(&e.project, &e.description, &e.tags),
                    sh.days.iter().position(|d| d.date == entry_date),
                ) {
                    sh.fact(row, day_idx) == 0.0
                } else {
                    false
                }
            } else {
                false
            };
            if should_restore && self.sync_entry_to_sheet(i, false) {
                restored += 1;
            }
        }
        restored
    }

    fn start_new(&mut self, p: Parsed) {
        let (stopped, stopped_idx) = self.stop();
        if stopped {
            if let Some(i) = stopped_idx {
                self.sync_entry_to_sheet(i, false);
            }
        }
        let now = Local::now();
        let dur = p.dur.unwrap_or(Duration::zero());
        let dur_sec = dur.num_seconds().max(0);
        let has_dur = dur_sec > 0;
        let start_time = if has_dur { now - Duration::seconds(dur_sec) } else { now };
        let end_time = if has_dur { Some(now) } else { None };

        self.entries.push(Entry {
            project: p.project.clone(),
            description: p.description,
            tags: p.tags,
            start: start_time,
            end: end_time,
            done: false,
            accumulated_sec: dur_sec,
            first_start: Some(start_time),
        });
        self.sel = 0;

        let new_idx = self.entries.len() - 1;
        if has_dur {
            let synced = self.sync_entry_to_sheet(new_idx, true);
            if !synced {
                self.say(format!("added {} to {}", fmt_dur(dur, self.cur_fmt()), p.project));
            }
        } else {
            self.say(format!("started {}", p.project));
        }
    }

    fn open_input(&mut self, mode: Mode, prefill: String) {
        self.cursor = prefill.chars().count();
        self.input = prefill;
        self.mode = mode;
        self.status.clear();
    }

    fn submit(&mut self) {
        let p = parse_input(&self.input);
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Start => {
                self.start_new(p);
            }
            Mode::Manual => match p.dur {
                Some(d) if d > Duration::zero() => {
                    self.start_new(p);
                }
                _ => {
                    self.status = "duration required, e.g. 30m, 1.5h, 1ч".into();
                    self.error = true;
                    self.mode = Mode::Manual;
                    return;
                }
            },
            Mode::Edit(i) => {
                if let Some(e) = self.entries.get_mut(i) {
                    e.project = p.project;
                    e.description = p.description;
                    e.tags = p.tags;
                    if let Some(d) = p.dur {
                        e.accumulated_sec = d.num_seconds();
                        if e.running() {
                            e.start = Local::now();
                        } else {
                            e.start = e.end.unwrap_or_else(Local::now) - d;
                            e.first_start = Some(e.start);
                        }
                    }
                    self.say("saved");
                }
                self.sync_entry_to_sheet(i, false);
            }
            Mode::SheetEdit => {
                let (h, c) = parse_sheet_input(&self.input);
                let comment = if c.is_empty() { None } else { Some(c.clone()) };
                if h.is_none() && comment.is_none() {
                    self.status = "nothing to write".into();
                    self.error = true;
                } else {
                    let clip = clip_text(h, &c);
                    self.sheet_apply(h, false, comment);
                    let via = copy_clipboard(&clip);
                    self.status.push_str(&format!(" · copied \"{clip}\" ({via})"));
                }
            }
            _ => {}
        }
        self.input.clear();
        self.cursor = 0;
        self.save();
        self.clamp();
    }

    fn export_csv(&mut self) {
        let mut out = String::from("start,end,project,description,tags,minutes,hours,status\n");
        for e in &self.entries {
            let mins = e.duration().num_seconds() / 60;
            let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
            out.push_str(&format!(
                "{},{},{},{},{},{},{:.2},{}\n",
                e.started_at().format("%Y-%m-%d %H:%M"),
                e.end.map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default(),
                q(&e.project),
                q(&e.description),
                q(&e.tags.join(" ")),
                mins,
                mins as f64 / 60.0,
                if e.done { "done" } else { "open" }
            ));
        }
        let path = export_path("timepipe_export.csv");
        match fs::write(&path, out) {
            Ok(_) => self.say(format!("exported to {}", path.display())),
            Err(e) => {
                self.status = format!("export failed: {e}");
                self.error = true;
            }
        }
    }

    fn export_xlsx(&mut self) {
        match self.write_xlsx() {
            Ok(_) => self.say(format!("exported to {}", export_path("timepipe_export.xlsx").display())),
            Err(e) => {
                self.status = format!("xlsx failed: {e}");
                self.error = true;
            }
        }
    }

    fn write_xlsx(&self) -> Result<(), rust_xlsxwriter::XlsxError> {
        use rust_xlsxwriter::{Color as XColor, Format, FormatBorder, Note, Workbook};

        let mut wb = Workbook::new();
        let head = Format::new()
            .set_bold()
            .set_font_color(XColor::White)
            .set_background_color(XColor::RGB(0x1F3A5F))
            .set_border(FormatBorder::Thin);
        let cell = Format::new().set_border(FormatBorder::Thin);
        let hours = Format::new().set_border(FormatBorder::Thin).set_num_format("0.00");
        let hmm = Format::new().set_border(FormatBorder::Thin).set_num_format("[h]:mm");
        let total_fmt = Format::new().set_bold().set_num_format("0.00");

        let mut sorted: Vec<&Entry> = self.entries.iter().collect();
        sorted.sort_by_key(|e| e.started_at());

        let ws = wb.add_worksheet();
        ws.set_name("Entries")?;
        let cols = ["Date", "Start", "End", "Project", "Description", "Tags", "Hours", "h:mm", "Status"];
        for (c, name) in cols.iter().enumerate() {
            ws.write_with_format(0, c as u16, *name, &head)?;
        }
        for (i, e) in sorted.iter().enumerate() {
            let r = (i + 1) as u32;
            let secs = e.duration().num_seconds().max(0) as f64;
            ws.write_with_format(r, 0, e.started_at().format("%Y-%m-%d").to_string(), &cell)?;
            ws.write_with_format(r, 1, e.started_at().format("%H:%M").to_string(), &cell)?;
            ws.write_with_format(
                r,
                2,
                e.end.map(|t| t.format("%H:%M").to_string()).unwrap_or_else(|| "running".into()),
                &cell,
            )?;
            ws.write_with_format(r, 3, e.project.as_str(), &cell)?;
            ws.write_with_format(r, 4, e.description.as_str(), &cell)?;
            ws.write_with_format(r, 5, e.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "), &cell)?;
            ws.write_with_format(r, 6, secs / 3600.0, &hours)?;
            ws.write_with_format(r, 7, secs / 86400.0, &hmm)?;
            let status_fmt = Format::new()
                .set_border(FormatBorder::Thin)
                .set_bold()
                .set_font_color(if e.done { XColor::RGB(0x1E7B34) } else { XColor::RGB(0xB26A00) });
            ws.write_with_format(r, 8, if e.done { "Done" } else { "Open" }, &status_fmt)?;

            let note_text = format!(
                "{}\nproject: {}\ntags: {}\n{} → {}",
                if e.description.is_empty() { "(no description)" } else { &e.description },
                e.project,
                if e.tags.is_empty() { "-".to_string() } else { e.tags.join(", ") },
                e.started_at().format("%d.%m %H:%M"),
                e.end.map(|t| t.format("%H:%M").to_string()).unwrap_or_else(|| "running".into()),
            );
            ws.insert_note(r, 6, &Note::new(note_text))?;
        }
        let last = sorted.len() as u32;
        ws.set_freeze_panes(1, 0)?;
        if last > 0 {
            ws.autofilter(0, 0, last, 8)?;
        }
        for (c, w) in [12.0, 8.0, 9.0, 18.0, 40.0, 20.0, 9.0, 9.0, 9.0].iter().enumerate() {
            ws.set_column_width(c as u16, *w)?;
        }
        ws.write_with_format(last + 2, 5, "Total", &total_fmt)?;
        if last > 0 {
            ws.write_formula_with_format(last + 2, 6, format!("=SUM(G2:G{})", last + 1).as_str(), &total_fmt)?;
        }

        // summary sheet
        let mut projects: Vec<String> = sorted.iter().map(|e| e.project.clone()).collect();
        projects.sort();
        projects.dedup();
        let sum = wb.add_worksheet();
        sum.set_name("Summary")?;
        sum.write_with_format(0, 0, "Project", &head)?;
        sum.write_with_format(0, 1, "Hours", &head)?;
        sum.write_with_format(0, 2, "Done", &head)?;
        sum.write_with_format(0, 3, "Open", &head)?;
        for (i, p) in projects.iter().enumerate() {
            let r = (i + 1) as u32;
            sum.write_with_format(r, 0, p.as_str(), &cell)?;
            sum.write_formula_with_format(
                r,
                2,
                format!("=COUNTIFS(Entries!D:D,A{},Entries!I:I,\"Done\")", r + 1).as_str(),
                &cell,
            )?;
            sum.write_formula_with_format(
                r,
                3,
                format!("=COUNTIFS(Entries!D:D,A{},Entries!I:I,\"Open\")", r + 1).as_str(),
                &cell,
            )?;
            sum.write_formula_with_format(
                r,
                1,
                format!("=SUMIF(Entries!D:D,A{},Entries!G:G)", r + 1).as_str(),
                &hours,
            )?;
        }
        sum.set_column_width(0, 24.0)?;
        sum.set_column_width(1, 10.0)?;

        wb.save(export_path("timepipe_export.xlsx"))
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match self.mode {
            Mode::Normal => self.on_key_normal(code),
            Mode::ConfirmDelete(i) => {
                if matches!(code, KeyCode::Char('y') | KeyCode::Enter) && i < self.entries.len() {
                    self.entries.remove(i);
                    self.save();
                    self.say("deleted");
                }
                self.mode = Mode::Normal;
                self.clamp();
            }
            _ => match code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.input.clear();
                    self.cursor = 0;
                    self.status.clear();
                }
                KeyCode::Enter => self.submit(),
                KeyCode::Left => {
                    self.cursor = self.cursor.saturating_sub(1);
                }
                KeyCode::Right => {
                    let len = self.input.chars().count();
                    if self.cursor < len {
                        self.cursor += 1;
                    }
                }
                KeyCode::Home => {
                    self.cursor = 0;
                }
                KeyCode::End => {
                    self.cursor = self.input.chars().count();
                }
                KeyCode::Backspace => {
                    if self.cursor > 0 {
                        let start = byte_index(&self.input, self.cursor - 1);
                        let end = byte_index(&self.input, self.cursor);
                        self.input.replace_range(start..end, "");
                        self.cursor -= 1;
                    }
                }
                KeyCode::Delete => {
                    let len = self.input.chars().count();
                    if self.cursor < len {
                        let start = byte_index(&self.input, self.cursor);
                        let end = byte_index(&self.input, self.cursor + 1);
                        self.input.replace_range(start..end, "");
                    }
                }
                KeyCode::Up => {
                    if let Some(e) = self.entries.last() {
                        self.input = e.as_input();
                        self.cursor = self.input.chars().count();
                    }
                }
                KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => {
                    self.cursor = 0;
                }
                KeyCode::Char('e') if mods.contains(KeyModifiers::CONTROL) => {
                    self.cursor = self.input.chars().count();
                }
                KeyCode::Char(c) => {
                    let idx = byte_index(&self.input, self.cursor);
                    self.input.insert(idx, c);
                    self.cursor += 1;
                }
                _ => {}
            },
        }
    }

    fn sheet_apply(&mut self, hours: Option<f64>, clear: bool, comment: Option<String>) {
        let res = match self.sheet.as_mut() {
            Some(sh) => sh.set_fact(hours, clear, comment),
            None => return,
        };
        match res {
            Ok(m) => self.say(m),
            Err(e) => {
                self.status = e;
                self.error = true;
            }
        }
    }

    fn sheet_step(&mut self, delta: f64) {
        let cur = self.sheet.as_ref().map(|s| s.cur_fact()).unwrap_or(0.0);
        let v = (cur + delta).max(0.0);
        if v <= 0.0 {
            self.sheet_apply(None, true, None);
        } else {
            self.sheet_apply(Some(v), false, None);
        }
    }

    fn sync_sheet_from_drive(&mut self) {
        self.say("syncing from Google Drive...");
        let target = sheet::sheet_path();
        match sync::sync_spreadsheet(sync::DEFAULT_SPREADSHEET_ID, &target) {
            Ok(msg) => {
                let (sh, err) = open_sheet();
                self.sheet = sh;
                self.sheet_err = err;
                let restored = self.restore_entries_to_sheet();
                if restored > 0 {
                    self.say(format!("✔ {msg} · restored {restored} tracked"));
                } else {
                    self.say(format!("✔ {msg}"));
                }
            }
            Err(e) => {
                self.status = format!("Sync error: {e}");
                self.error = true;
            }
        }
    }

    /// keys of the Sheet tab; returns true if handled
    fn on_key_sheet(&mut self, code: KeyCode) -> bool {
        if code == KeyCode::Char('U') {
            self.sync_sheet_from_drive();
            return true;
        }
        if self.sheet.is_none() {
            return false;
        }
        match code {
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(sh) = self.sheet.as_mut() {
                    sh.day = sh.day.saturating_sub(1);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(sh) = self.sheet.as_mut() {
                    if sh.day + 1 < sh.days.len() {
                        sh.day += 1;
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(sh) = self.sheet.as_mut() {
                    sh.row = sh.row.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(sh) = self.sheet.as_mut() {
                    if sh.row + 1 < sh.tasks.len() {
                        sh.row += 1;
                    }
                }
            }
            KeyCode::Char('g') => {
                if let Some(sh) = self.sheet.as_mut() {
                    sh.row = 0;
                }
            }
            KeyCode::Char('G') => {
                if let Some(sh) = self.sheet.as_mut() {
                    sh.row = sh.tasks.len().saturating_sub(1);
                }
            }
            KeyCode::Char('[') | KeyCode::Char(']') => {
                let delta = if code == KeyCode::Char('[') { -1 } else { 1 };
                let res = self.sheet.as_mut().map(|sh| sh.switch_month(delta));
                if let Some(Err(e)) = res {
                    self.status = e;
                    self.error = true;
                }
            }
            KeyCode::Char('y') => {
                if let Some(sh) = &self.sheet {
                    if let Some(&r) = sh.tasks.get(sh.row) {
                        if let Some(day) = sh.days.get(sh.day) {
                            let fcol = day.col + 2;
                            let fact = sh.num(r, fcol);
                            let note = sh.comment_at(r, fcol).map(|c| c.1.as_str()).unwrap_or("");
                            let text = clip_text(if fact > 0.0 { Some(fact) } else { None }, note);
                            if !text.is_empty() {
                                let via = copy_clipboard(&text);
                                self.say(format!("copied \"{text}\" ({via})"));
                            } else {
                                self.say("nothing to copy for this cell");
                            }
                        }
                    }
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.sheet_step(0.5),
            KeyCode::Char('-') | KeyCode::Char('_') => self.sheet_step(-0.5),
            KeyCode::Char('d') => self.sheet_apply(None, true, None),
            KeyCode::Char('f') => self.cycle_fmt(),
            KeyCode::Enter => self.open_input(Mode::SheetEdit, String::new()),
            _ => return false,
        }
        true
    }

    fn on_key_normal(&mut self, code: KeyCode) {
        if self.view == 3 && self.on_key_sheet(code) {
            return;
        }
        let n = self.visible().len();
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.view = (self.view + 1) % 4,
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.view = (self.view + 3) % 4,
            KeyCode::Char('1') => self.view = 0,
            KeyCode::Char('2') => self.view = 1,
            KeyCode::Char('3') => self.view = 2,
            KeyCode::Char('4') => self.view = 3,
            KeyCode::Down | KeyCode::Char('j') => {
                if self.sel + 1 < n {
                    self.sel += 1
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Char('g') => self.sel = 0,
            KeyCode::Char('G') => self.sel = n.saturating_sub(1),
            KeyCode::Char('s') | KeyCode::Char('n') => self.open_input(Mode::Start, String::new()),
            KeyCode::Char('a') => self.open_input(Mode::Manual, String::new()),
            KeyCode::Char('e') => {
                if let Some(i) = self.selected_index() {
                    let s = self.entries[i].as_edit_input();
                    self.open_input(Mode::Edit(i), s);
                }
            }
            KeyCode::Char('d') => {
                if let Some(i) = self.selected_index() {
                    self.mode = Mode::ConfirmDelete(i);
                }
            }
            KeyCode::Char(' ') => {
                let (stopped, stopped_idx) = self.stop();
                if stopped {
                    self.say("paused");
                    self.save();
                    if let Some(i) = stopped_idx {
                        self.sync_entry_to_sheet(i, false);
                    }
                } else if let Some(i) = self.selected_index() {
                    let desc = {
                        let e = &mut self.entries[i];
                        e.start = Local::now();
                        e.end = None;
                        e.done = false;
                        if e.description.is_empty() { e.project.clone() } else { e.description.clone() }
                    };
                    self.say(format!("resumed {}", desc));
                    self.save();
                } else if let Some(desc) = self.entries.last_mut().map(|e| {
                    e.start = Local::now();
                    e.end = None;
                    e.done = false;
                    if e.description.is_empty() { e.project.clone() } else { e.description.clone() }
                }) {
                    self.say(format!("resumed {}", desc));
                    self.save();
                }
            }
            KeyCode::Char('c') => {
                if let Some(i) = self.selected_index() {
                    let now_done = {
                        let e = &mut self.entries[i];
                        if !e.done {
                            if e.end.is_none() {
                                let now = Local::now();
                                e.accumulated_sec += (now - e.start).num_seconds().max(0);
                                e.end = Some(now);
                            }
                            e.done = true;
                        } else {
                            e.done = false;
                        }
                        e.done
                    };
                    self.say(if now_done { "✔ marked done" } else { "reopened" });
                    if let Some(p) = self.visible().iter().position(|&x| x == i) {
                        self.sel = p;
                    }
                    self.save();
                    if now_done {
                        self.sync_entry_to_sheet(i, false);
                    }
                }
            }
            KeyCode::Char('p') => {
                if let Some(i) = self.selected_index() {
                    self.sync_entry_to_sheet(i, true);
                }
            }
            KeyCode::Char('P') => {
                let today = Local::now().date_naive();
                let indices: Vec<usize> = (0..self.entries.len())
                    .filter(|&i| self.entries[i].started_at().date_naive() == today)
                    .collect();
                let mut synced = 0;
                for i in indices {
                    if self.sync_entry_to_sheet(i, false) {
                        synced += 1;
                    }
                }
                if synced > 0 {
                    self.say(format!("✔ synced {synced} tasks to sheet"));
                } else {
                    self.status = "no matching tasks found in sheet to sync".into();
                    self.error = true;
                }
            }
            KeyCode::Char('r') => {
                if let Some(i) = self.selected_index() {
                    let p = parse_input(&self.entries[i].as_input());
                    self.start_new(p);
                    self.say("restarted");
                    self.save();
                }
            }
            KeyCode::Char('f') => self.cycle_fmt(),
            KeyCode::Char('x') => self.export_csv(),
            KeyCode::Char('X') => self.export_xlsx(),
            _ => {}
        }
        self.clamp();
    }
}

// ───────────────────────── ui helpers ─────────────────────────

fn panel<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(SURFACE))
        .title(title)
}

fn key_hint<'a>(key: &'a str, label: &'a str) -> Vec<Span<'a>> {
    vec![
        Span::styled(format!(" {} ", key), Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {}  ", label), Style::default().fg(DIM)),
    ]
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 3, w, h)
}

fn spark(values: &[i64]) -> String {
    const B: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().copied().max().unwrap_or(0);
    values
        .iter()
        .map(|&v| {
            if max == 0 || v == 0 {
                '·'
            } else {
                B[(((v as f64 / max as f64) * 7.0).round() as usize).min(7)]
            }
        })
        .collect()
}

fn bar(v: i64, max: i64, width: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let eighths = ((v as f64 / max as f64) * width as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let part = ['\0', '▏', '▎', '▍', '▌', '▋', '▊', '▉'][eighths % 8];
    let mut s = "█".repeat(full);
    if part != '\0' {
        s.push(part);
    }
    s
}

fn day_totals(app: &App, days: i64) -> Vec<(NaiveDate, i64)> {
    let today = Local::now().date_naive();
    (0..days)
        .rev()
        .map(|i| {
            let d = today - Duration::days(i);
            let s = app
                .entries
                .iter()
                .filter(|e| e.started_at().date_naive() == d || e.start.date_naive() == d)
                .map(|e| e.duration().num_seconds())
                .sum();
            (d, s)
        })
        .collect()
}

// ───────────────────────── ui ─────────────────────────

fn ui(f: &mut Frame, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(f.area());

    draw_header(f, app, rows[0]);

    let top = if app.status.is_empty() {
        Layout::horizontal([Constraint::Min(0), Constraint::Length(0)]).split(rows[1])
    } else {
        Layout::horizontal([Constraint::Length(35), Constraint::Min(10)]).split(rows[1])
    };
    f.render_widget(
        Tabs::new(vec![" 1 Today ", " 2 Week ", " 3 Stats ", " 4 Sheet "])
            .select(app.view)
            .padding("", "")
            .style(Style::default().fg(DIM))
            .highlight_style(Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD))
            .divider(""),
        top[0],
    );
    if !app.status.is_empty() {
        let c = if app.error { RED } else { GREEN };
        f.render_widget(
            Paragraph::new(Span::styled(format!("● {} ", app.status), Style::default().fg(c)))
                .alignment(Alignment::Right),
            top[1],
        );
    }

    if app.view == 3 {
        draw_sheet(f, app, rows[2]);
    } else if app.view == 2 {
        draw_stats(f, app, rows[2]);
    } else {
        let w = rows[2].width;
        if w >= 132 {
            let body = Layout::horizontal([Constraint::Min(50), Constraint::Length(30)]).split(rows[2]);
            let halves = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(body[0]);
            draw_table(f, app, halves[0], false);
            draw_table(f, app, halves[1], true);
            draw_details(f, app, body[1]);
        } else if w >= 96 {
            let body = Layout::horizontal([Constraint::Min(50), Constraint::Length(30)]).split(rows[2]);
            let halves = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(body[0]);
            draw_table(f, app, halves[0], false);
            draw_table(f, app, halves[1], true);
            draw_details(f, app, body[1]);
        } else {
            let halves = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[2]);
            draw_table(f, app, halves[0], false);
            draw_table(f, app, halves[1], true);
        }
    }

    draw_footer(f, app, rows[3]);

    match &app.mode {
        Mode::Start | Mode::Manual | Mode::Edit(_) | Mode::SheetEdit => draw_input(f, app),
        Mode::ConfirmDelete(_) => {
            let r = centered(f.area(), 44, 5);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(""),
                    Line::from("Delete this entry?"),
                    Line::from(vec![
                        Span::styled("y", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
                        Span::styled(" yes   ", Style::default().fg(DIM)),
                        Span::styled("any key", Style::default().fg(ACCENT)),
                        Span::styled(" cancel", Style::default().fg(DIM)),
                    ]),
                ])
                .alignment(Alignment::Center)
                .block(panel(" confirm ").border_style(Style::default().fg(RED))),
                r,
            );
        }
        Mode::Normal => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(area);

    let line = match app.entries.iter().find(|e| e.running()) {
        Some(e) => {
            Line::from(vec![
                Span::styled(format!(" {} ", spinner_frame()), Style::default().fg(GREEN)),
                Span::styled(
                    fmt_clock(e.duration()),
                    Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(e.project.clone(), Style::default().fg(project_color(&e.project)).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {}", e.description), Style::default().fg(Color::White)),
            ])
        }
        None => Line::from(vec![
            Span::styled(" ■ idle", Style::default().fg(DIM)),
            Span::styled("   press s to start tracking", Style::default().fg(DIM)),
        ]),
    };
    f.render_widget(panel(" timepipe ").border_style(Style::default().fg(ACCENT)), cols[0]);
    f.render_widget(Paragraph::new(line), Rect::new(cols[0].x + 1, cols[0].y + 1, cols[0].width.saturating_sub(2), 1));

    let today = Local::now().date_naive();
    let total = app
        .entries
        .iter()
        .filter(|e| e.started_at().date_naive() == today || e.start.date_naive() == today)
        .fold(Duration::zero(), |a, e| a + e.duration());
    let ratio = (total.num_minutes() as f64 / DAILY_GOAL_MIN as f64).clamp(0.0, 1.0);
    let color = if ratio >= 1.0 { GREEN } else if ratio > 0.5 { YELLOW } else { ACCENT };
    f.render_widget(
        Gauge::default()
            .block(panel(format!(" today · goal {}h ", DAILY_GOAL_MIN / 60)))
            .gauge_style(Style::default().fg(color).bg(SURFACE))
            .use_unicode(true)
            .ratio(ratio)
            .label(format!("{} · {:.0}%", fmt_dur(total, app.cur_fmt()), ratio * 100.0)),
        cols[1],
    );
}

fn draw_table(f: &mut Frame, app: &App, area: Rect, done: bool) {
    let all = app.visible();
    let offset = if done { all.iter().filter(|&&i| !app.entries[i].done).count() } else { 0 };
    let vis: Vec<usize> = all.iter().copied().filter(|&i| app.entries[i].done == done).collect();
    let week = app.view == 1;
    let start_w: u16 = if week { 9 } else { 5 };
    let time_w: u16 = match app.cur_fmt() {
        Fmt::Both => 14,
        Fmt::HoursMin => 7,
        Fmt::Decimal => 6,
    };
    let show_tags = area.width >= 54;
    let proj_w: u16 = if area.width < 54 { 10 } else { 12 };
    let tags_w: u16 = if area.width >= 66 { 10 } else { 8 };

    let rows: Vec<Row> = vis
        .iter()
        .map(|&i| {
            let e = &app.entries[i];
            let when = if week { e.started_at().format("%a %H:%M") } else { e.started_at().format("%H:%M") }.to_string();
            let marker = if e.running() {
                "● "
            } else if e.done {
                "✔ "
            } else {
                "◔ "
            };
            let dur_style = if e.running() { Style::default().fg(GREEN) } else { Style::default().fg(Color::White) };
            let mut cells = vec![
                Cell::from(Span::styled(when, Style::default().fg(DIM))),
                Cell::from(Line::from(vec![
                    Span::styled(marker, Style::default().fg(GREEN)),
                    Span::styled(e.project.clone(), Style::default().fg(project_color(&e.project))),
                ])),
                Cell::from(e.description.clone()),
            ];
            if show_tags {
                cells.push(Cell::from(Span::styled(
                    e.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "),
                    Style::default().fg(YELLOW),
                )));
            }
            cells.push(Cell::from(Span::styled(fmt_dur(e.duration(), app.cur_fmt()), dur_style)));
            Row::new(cells)
        })
        .collect();
    let total = vis.iter().fold(Duration::zero(), |a, &i| a + app.entries[i].duration());
    let title = format!(
        " {} · {} · {} · {} ",
        if done { "✔ DONE" } else { "◔ OPEN" },
        if week { "7 days" } else { "today" },
        vis.len(),
        fmt_dur(total, app.cur_fmt())
    );
    let mut widths = vec![
        Constraint::Length(start_w),
        Constraint::Length(proj_w),
        Constraint::Min(8),
    ];
    let mut header_cols = vec!["START", "PROJECT", "DESCRIPTION"];
    if show_tags {
        widths.push(Constraint::Length(tags_w));
        header_cols.push("TAGS");
    }
    widths.push(Constraint::Length(time_w));
    header_cols.push("TIME");

    let table = Table::new(rows, widths)
        .header(Row::new(header_cols).style(Style::default().fg(DIM).add_modifier(Modifier::BOLD)))
        .row_highlight_style(Style::default().bg(SURFACE).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌")
        .block(panel(title));
    let selected = if app.sel >= offset && app.sel < offset + vis.len() { Some(app.sel - offset) } else { None };
    let mut st = TableState::default().with_selected(selected);
    f.render_stateful_widget(table, area, &mut st);

    if vis.is_empty() && area.height >= 4 {
        let r = Rect::new(area.x + 2, area.y + 3, area.width.saturating_sub(4), area.height.saturating_sub(4));
        f.render_widget(
            Paragraph::new(if done { "no finished tasks · press c to mark done" } else { "nothing open · s start · a add" })
                .wrap(ratatui::widgets::Wrap { trim: true })
                .style(Style::default().fg(DIM)),
            r,
        );
    }
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let label = |s: &'static str| Span::styled(format!("{:<9}", s), Style::default().fg(DIM));
    if let Some(i) = app.selected_index() {
        let e = &app.entries[i];
        let d = e.duration();
        lines.push(Line::from(vec![
            label("project"),
            Span::styled(e.project.clone(), Style::default().fg(project_color(&e.project)).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(vec![label("task"), Span::raw(e.description.clone())]));
        lines.push(Line::from(vec![
            label("tags"),
            Span::styled(
                e.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "),
                Style::default().fg(YELLOW),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            label("status"),
            if e.done {
                Span::styled("✔ done", Style::default().fg(GREEN))
            } else if e.running() {
                Span::styled("● running", Style::default().fg(GREEN))
            } else if e.accumulated_sec > 0 {
                Span::styled("◔ paused", Style::default().fg(YELLOW))
            } else {
                Span::styled("◔ open", Style::default().fg(YELLOW))
            },
        ]));
        lines.push(Line::from(vec![label("started"), Span::raw(e.started_at().format("%d.%m %H:%M").to_string())]));
        lines.push(Line::from(vec![
            label("ended"),
            Span::raw(e.end.map(|t| t.format("%d.%m %H:%M").to_string()).unwrap_or_else(|| "running".into())),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            label("minutes"),
            Span::styled(format!("{}", d.num_minutes()), Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(vec![
            label("hours"),
            Span::styled(format!("{:.2}", d.num_seconds() as f64 / 3600.0), Style::default().fg(ACCENT)),
        ]));
        lines.push(Line::from(vec![label("h:mm"), Span::raw(fmt_dur(d, Fmt::HoursMin))]));
        lines.push(Line::from(""));
    }
    let days = day_totals(app, 7);
    let vals: Vec<i64> = days.iter().map(|d| d.1).collect();
    lines.push(Line::from(vec![
        label("7 days"),
        Span::styled(spark(&vals), Style::default().fg(ACCENT)),
    ]));
    f.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: true }).block(panel(" details ")),
        area,
    );
}

fn draw_stats(f: &mut Frame, app: &App, area: Rect) {
    let today = Local::now().date_naive();
    let from = today - Duration::days(6);
    let week: Vec<&Entry> = app
        .entries
        .iter()
        .filter(|e| e.started_at().date_naive() >= from || e.start.date_naive() >= from)
        .collect();

    let mut by_project: BTreeMap<String, i64> = BTreeMap::new();
    for e in &week {
        *by_project.entry(e.project.clone()).or_default() += e.duration().num_seconds();
    }
    let days = day_totals(app, 7);
    let total: i64 = days.iter().map(|d| d.1).sum();
    let active_days = days.iter().filter(|d| d.1 > 0).count() as i64;

    let split = Layout::vertical([Constraint::Length(5), Constraint::Min(5)]).split(area);

    // summary cards
    let cards = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
    ])
    .split(split[0]);
    let avg = if active_days > 0 { total / active_days } else { 0 };
    let best = days.iter().max_by_key(|d| d.1).filter(|d| d.1 > 0);
    let card = |title: &'static str, big: String, small: String, c: Color| {
        Paragraph::new(vec![
            Line::from(Span::styled(big, Style::default().fg(c).add_modifier(Modifier::BOLD))),
            Line::from(Span::styled(small, Style::default().fg(DIM))),
        ])
        .alignment(Alignment::Center)
        .block(panel(title))
    };
    f.render_widget(
        card("total · 7d", fmt_dur(Duration::seconds(total), Fmt::HoursMin), format!("{:.2}h", total as f64 / 3600.0), ACCENT),
        cards[0],
    );
    f.render_widget(
        card("avg / active day", fmt_dur(Duration::seconds(avg), Fmt::HoursMin), format!("{} active days", active_days), GREEN),
        cards[1],
    );
    f.render_widget(
        card(
            "best day",
            best.map(|d| fmt_dur(Duration::seconds(d.1), Fmt::HoursMin)).unwrap_or_else(|| "-".into()),
            best.map(|d| d.0.format("%A").to_string()).unwrap_or_default(),
            YELLOW,
        ),
        cards[2],
    );
    f.render_widget(
        card("entries", format!("{}", week.len()), format!("{} projects", by_project.len()), Color::Rgb(203, 166, 247)),
        cards[3],
    );

    let halves = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(split[1]);

    let mut projects: Vec<_> = by_project.into_iter().collect();
    projects.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
    let pmax = projects.first().map(|p| p.1).unwrap_or(0);
    let plines: Vec<Line> = projects
        .iter()
        .map(|(p, s)| {
            let c = project_color(p);
            Line::from(vec![
                Span::styled(format!("{:<12} ", p.chars().take(12).collect::<String>()), Style::default().fg(c)),
                Span::styled(bar(*s, pmax, 18), Style::default().fg(c)),
                Span::styled(format!(" {}", fmt_dur(Duration::seconds(*s), app.cur_fmt())), Style::default().fg(DIM)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(plines).block(panel(" by project ")), halves[0]);

    let dmax = days.iter().map(|d| d.1).max().unwrap_or(0);
    let dlines: Vec<Line> = days
        .iter()
        .map(|(d, s)| {
            let c = if *d == today { YELLOW } else { ACCENT };
            Line::from(vec![
                Span::styled(format!("{} ", d.format("%a %d.%m")), Style::default().fg(if *d == today { YELLOW } else { DIM })),
                Span::styled(bar(*s, dmax, 18), Style::default().fg(c)),
                Span::styled(format!(" {}", fmt_dur(Duration::seconds(*s), app.cur_fmt())), Style::default().fg(DIM)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(dlines).block(panel(" by day ")), halves[1]);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hints: Vec<(&str, &str)> = match app.mode {
        Mode::Normal if app.view == 3 => vec![
            ("←→", "day"),
            ("↑↓", "task"),
            ("+/-", "±0.5h"),
            ("enter", "fill+copy"),
            ("y", "yank/copy"),
            ("U", "sync drive"),
            ("d", "clear"),
            ("f", "format"),
            ("[ ]", "month"),
            ("tab", "view"),
            ("q", "quit"),
        ],
        Mode::Normal => vec![
            ("s", "start"),
            ("space", "pause/resume"),
            ("p/P", "push sheet"),
            ("a", "add"),
            ("e", "edit"),
            ("c", "done/open"),
            ("r", "repeat"),
            ("d", "delete"),
            ("f", "format"),
            ("x", "csv"),
            ("X", "xlsx"),
            ("q", "quit"),
        ],
        Mode::ConfirmDelete(_) => vec![("y", "delete"), ("any", "cancel")],
        _ => vec![("enter", "confirm"), ("esc", "cancel"), ("↑", "last entry")],
    };
    let mut spans = Vec::new();
    for (k, l) in hints {
        spans.extend(key_hint(k, l));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(s.len())
}

fn input_line<'a>(input: &str, cursor: usize) -> Line<'a> {
    let idx = byte_index(input, cursor);
    let (before, after) = input.split_at(idx);
    Line::from(vec![
        Span::styled("❯ ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(before.to_string()),
        Span::styled("▏", Style::default().fg(ACCENT)),
        Span::raw(after.to_string()),
    ])
}

fn draw_input(f: &mut Frame, app: &App) {
    if matches!(app.mode, Mode::SheetEdit) {
        draw_sheet_input(f, app);
        return;
    }
    let title = match app.mode {
        Mode::Start => " start tracking ",
        Mode::Manual => " add past time ",
        _ => " edit entry ",
    };
    let r = centered(f.area(), 70, 9);
    f.render_widget(Clear, r);
    let p = parse_input(&app.input);
    let mut lines = vec![
        input_line(&app.input, app.cursor),
        Line::from(""),
        Line::from(vec![
            Span::styled("project  ", Style::default().fg(DIM)),
            Span::styled(p.project.clone(), Style::default().fg(project_color(&p.project)).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![Span::styled("task     ", Style::default().fg(DIM)), Span::raw(p.description.clone())]),
        Line::from(vec![
            Span::styled("tags     ", Style::default().fg(DIM)),
            Span::styled(
                p.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "),
                Style::default().fg(YELLOW),
            ),
        ]),
        Line::from(vec![
            Span::styled("time     ", Style::default().fg(DIM)),
            Span::styled(
                p.dur.map(|d| fmt_dur(d, app.cur_fmt())).unwrap_or_else(|| "-".into()),
                Style::default().fg(GREEN),
            ),
        ]),
    ];
    if app.error && !app.status.is_empty() {
        lines.push(Line::from(Span::styled(app.status.clone(), Style::default().fg(RED))));
    }
    f.render_widget(
        Paragraph::new(lines).block(
            panel(title)
                .border_style(Style::default().fg(ACCENT))
                .title_bottom(Line::from(Span::styled(
                    " project: task #tag 1h30m ",
                    Style::default().fg(DIM),
                ))),
        ),
        r,
    );
}

// ───────────────────────── sheet tab ─────────────────────────

/// One-time import: an `.xlsx` lying in the current directory is copied into the data dir.
fn find_local_xlsx() -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(".")
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let n = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            n.ends_with(".xlsx") && !n.starts_with("timepipe_export") && !n.starts_with("~$")
        })
        .collect();
    // prefer a former working copy
    found.sort_by_key(|p| !p.to_string_lossy().ends_with(".timepipe.xlsx"));
    found.into_iter().next()
}

fn open_sheet() -> (Option<sheet::Sheet>, String) {
    let path = sheet::sheet_path();
    if !path.exists() {
        if let Some(src) = find_local_xlsx() {
            let _ = fs::copy(src, &path);
        }
    }
    if !path.exists() {
        return (None, "no sheet yet — open the Sheet tab (4) and press U to sync from Google Drive".into());
    }
    match sheet::Sheet::open(&path) {
        Ok(s) => (Some(s), String::new()),
        Err(e) => (None, format!("{}: {}", path.display(), e)),
    }
}

/// "0.5 fixed login", "1.5h review", "0.5 #баги", "1ч #баги", "fixed login 30m" -> (hours, comment)
fn parse_sheet_input(s: &str) -> (Option<f64>, String) {
    let s = s.trim();
    if s.is_empty() {
        return (None, String::new());
    }

    let toks: Vec<&str> = s.split_whitespace().collect();
    if toks.is_empty() {
        return (None, String::new());
    }

    let mut total_mins = 0.0f64;
    let mut any_hours = false;
    let mut comment_toks = Vec::new();

    for tok in toks {
        if let Some(d) = parse_dur(tok) {
            total_mins += d.num_seconds() as f64 / 60.0;
            any_hours = true;
        } else if let Ok(v) = tok.replace(',', ".").parse::<f64>() {
            if v > 0.0 && v <= 24.0 {
                total_mins += v * 60.0;
                any_hours = true;
            } else {
                comment_toks.push(tok);
            }
        } else {
            comment_toks.push(tok);
        }
    }

    let hours = if any_hours {
        Some(((total_mins / 60.0) * 100.0).round() / 100.0)
    } else {
        None
    };

    (hours, comment_toks.join(" "))
}

fn h1(v: f64) -> String {
    if v == 0.0 {
        "·".into()
    } else {
        format!("{:.2}", v).trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Hours from the sheet in the current display format (`f` toggles it).
fn fmt_h(v: f64, f: Fmt) -> String {
    if v <= 0.0 {
        return "·".into();
    }
    let hm = fmt_dur_compact(Duration::seconds((v * 3600.0).round() as i64)).unwrap_or_else(|| "0m".into());
    match f {
        Fmt::Decimal => h1(v),
        Fmt::HoursMin => hm,
        Fmt::Both => format!("{} ({})", h1(v), hm),
    }
}

fn draw_sheet(f: &mut Frame, app: &App, area: Rect) {
    let Some(sh) = &app.sheet else {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(app.sheet_err.clone(), Style::default().fg(RED))),
            ])
            .block(panel(" sheet ")),
            area,
        );
        return;
    };
    let w: u16 = match app.cur_fmt() {
        Fmt::Both => 14,
        Fmt::HoursMin => 7,
        Fmt::Decimal => 5,
    };
    let days_w = 2 * w + 16;
    let details_min_total = days_w + 67 + 30;
    let (days_area, tasks_area, details_area) = if area.width >= details_min_total {
        let cols = Layout::horizontal([Constraint::Length(days_w), Constraint::Min(67), Constraint::Length(30)]).split(area);
        (cols[0], cols[1], Some(cols[2]))
    } else if area.width >= days_w + 52 {
        let cols = Layout::horizontal([Constraint::Length(days_w), Constraint::Min(52)]).split(area);
        (cols[0], cols[1], None)
    } else {
        let rows = Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)]).split(area);
        (rows[0], rows[1], None)
    };
    let today = Local::now().date_naive();

    // days
    let day_rows: Vec<Row> = sh
        .days
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let wd = d.date.format("%a").to_string();
            let weekend = wd == "Sat" || wd == "Sun";
            let base = if d.date == today {
                Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)
            } else if weekend {
                Style::default().fg(DIM)
            } else {
                Style::default()
            };
            let fact = sh.day_fact(i);
            Row::new(vec![
                Cell::from(format!("{} {}", wd, d.date.format("%d.%m"))),
                Cell::from(fmt_h(sh.day_plan(i), app.cur_fmt())),
                Cell::from(Span::styled(fmt_h(fact, app.cur_fmt()), Style::default().fg(if fact > 0.0 { GREEN } else { DIM }))),
            ])
            .style(base)
        })
        .collect();
    let days_table = Table::new(day_rows, [Constraint::Length(10), Constraint::Length(w), Constraint::Length(w)])
        .header(Row::new(vec!["DAY", "PLAN", "FACT"]).style(Style::default().fg(DIM).add_modifier(Modifier::BOLD)))
        .row_highlight_style(Style::default().bg(SURFACE).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌")
        .block(panel(format!(" ‹ {} › ", sh.month_name())));
    let mut dst = TableState::default().with_selected(Some(sh.day));
    f.render_stateful_widget(days_table, days_area, &mut dst);

    // tasks for the selected day
    let Some(day) = sh.days.get(sh.day) else {
        f.render_widget(Paragraph::new("no days in this sheet").block(panel(" tasks ")), tasks_area);
        return;
    };
    let (pcol, fcol) = (day.col + 1, day.col + 2);
    let show_type = tasks_area.width >= 2 * w + 38;
    let proj_w: u16 = if tasks_area.width >= 2 * w + 50 { 14 } else { 11 };
    let task_rows: Vec<Row> = sh
        .tasks
        .iter()
        .map(|&r| {
            let has_note = sh.comment_at(r, fcol).is_some() || sh.comment_at(r, pcol).is_some();
            let proj = sh.text(r, 2).to_string();
            let fact = sh.num(r, fcol);
            let plan = sh.num(r, pcol);
            let mut cells = vec![
                Cell::from(Span::styled(if has_note { "✎" } else { " " }, Style::default().fg(YELLOW))),
                Cell::from(Span::styled(proj.clone(), Style::default().fg(project_color(&proj)))),
                Cell::from(sh.text(r, 4).to_string()),
            ];
            if show_type {
                cells.push(Cell::from(Span::styled(sh.text(r, 5).to_string(), Style::default().fg(DIM))));
            }
            cells.push(Cell::from(Span::styled(fmt_h(plan, app.cur_fmt()), Style::default().fg(DIM))));
            cells.push(Cell::from(Span::styled(
                fmt_h(fact, app.cur_fmt()),
                Style::default().fg(if fact > 0.0 { GREEN } else { DIM }).add_modifier(Modifier::BOLD),
            )));
            Row::new(cells)
        })
        .collect();
    let title = format!(
        " {} · plan {}h · fact {}h ",
        day.date.format("%a %d.%m"),
        h1(sh.day_plan(sh.day)),
        h1(sh.day_fact(sh.day))
    );
    let mut widths = vec![Constraint::Length(1), Constraint::Length(proj_w), Constraint::Min(10)];
    let mut hdr = vec!["", "PROJECT", "TASK"];
    if show_type {
        widths.push(Constraint::Length(4));
        hdr.push("TYPE");
    }
    widths.push(Constraint::Length(w));
    widths.push(Constraint::Length(w));
    hdr.push("PLAN");
    hdr.push("FACT");

    let tasks_table = Table::new(task_rows, widths)
        .header(Row::new(hdr).style(Style::default().fg(DIM).add_modifier(Modifier::BOLD)))
        .row_highlight_style(Style::default().bg(SURFACE).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌")
        .block(panel(title));
    let mut tst = TableState::default().with_selected(if sh.tasks.is_empty() { None } else { Some(sh.row) });
    f.render_stateful_widget(tasks_table, tasks_area, &mut tst);

    // details
    if let Some(d_area) = details_area {
        let mut lines: Vec<Line> = Vec::new();
        let label = |s: &'static str| Span::styled(format!("{:<9}", s), Style::default().fg(DIM));
        if let Some(&r) = sh.tasks.get(sh.row) {
            lines.push(Line::from(vec![label("owner"), Span::raw(sh.text(r, 1).to_string())]));
            let proj = sh.text(r, 2).to_string();
            lines.push(Line::from(vec![
                label("project"),
                Span::styled(proj.clone(), Style::default().fg(project_color(&proj)).add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::from(vec![label("task"), Span::raw(sh.text(r, 4).to_string())]));
            lines.push(Line::from(vec![label("type"), Span::raw(sh.text(r, 5).to_string())]));
            lines.push(Line::from(vec![
                label("total"),
                Span::raw(format!("plan {} / fact {}", fmt_h(sh.num(r, 7), app.cur_fmt()), fmt_h(sh.num(r, 8), app.cur_fmt()))),
            ]));
            lines.push(Line::from(""));
            for (name, col) in [("fact", fcol), ("plan", pcol)] {
                if let Some((threaded, text)) = sh.comment_at(r, col) {
                    lines.push(Line::from(Span::styled(
                        format!("✎ {} note{}", name, if *threaded { " (thread)" } else { "" }),
                        Style::default().fg(YELLOW),
                    )));
                    for l in text.lines().filter(|l| !l.starts_with("[Threaded") && !l.starts_with("Comment:")).take(6) {
                        lines.push(Line::from(Span::styled(format!(" {}", l), Style::default().fg(Color::White))));
                    }
                }
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("file: {}", sh.path.display()),
            Style::default().fg(DIM),
        )));
        f.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: true }).block(panel(" details ")),
            d_area,
        );
    }
}

fn draw_sheet_input(f: &mut Frame, app: &App) {
    let r = centered(f.area(), 70, 9);
    f.render_widget(Clear, r);
    let (h, c) = parse_sheet_input(&app.input);
    let target = app
        .sheet
        .as_ref()
        .and_then(|sh| {
            let row = *sh.tasks.get(sh.row)?;
            let day = sh.days.get(sh.day)?;
            Some(format!(
                "{} · {} · {}",
                day.date.format("%a %d.%m"),
                sh.text(row, 2),
                sh.text(row, 4)
            ))
        })
        .unwrap_or_default();
    let lines = vec![
        Line::from(Span::styled(target, Style::default().fg(DIM))),
        input_line(&app.input, app.cursor),
        Line::from(""),
        Line::from(vec![
            Span::styled("fact     ", Style::default().fg(DIM)),
            Span::styled(
                h.map(|v| format!("{} h", h1(v))).unwrap_or_else(|| "(unchanged)".into()),
                Style::default().fg(GREEN),
            ),
        ]),
        Line::from(vec![
            Span::styled("note     ", Style::default().fg(DIM)),
            Span::styled(if c.is_empty() { "-".to_string() } else { c }, Style::default().fg(YELLOW)),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines).block(
            panel(" fill cell ")
                .border_style(Style::default().fg(ACCENT))
                .title_bottom(Line::from(Span::styled(" 0.5 comment text  ·  1h30m review ", Style::default().fg(DIM)))),
        ),
        r,
    );
}

// ───────────────────────── clipboard ─────────────────────────

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for ch in data.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if ch.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if ch.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Copies text to the system clipboard. Tries wl-copy / xclip / xsel,
/// and always also sends an OSC 52 sequence (works over ssh in most modern terminals).
fn copy_clipboard(text: &str) -> &'static str {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut used = "osc52";
    let tools: [(&str, &[&str]); 3] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (cmd, args) in tools {
        if let Ok(mut ch) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut si) = ch.stdin.take() {
                let _ = si.write_all(text.as_bytes());
            }
            let _ = ch.wait();
            used = cmd;
            break;
        }
    }
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", b64(text.as_bytes()));
    let _ = out.flush();
    used
}

fn clean_comment_text(raw: &str) -> String {
    let mut lines = Vec::new();
    for l in raw.lines() {
        let trimmed = l.trim();
        if trimmed.starts_with("[Threaded")
            || trimmed.starts_with("Your version of Excel")
            || trimmed.starts_with("Comment:")
            || trimmed.starts_with("Learn more:")
            || trimmed.is_empty()
        {
            continue;
        }
        lines.push(trimmed);
    }
    lines.join(" ")
}

fn fmt_hours(v: f64) -> String {
    format!("{:.2}", v).trim_end_matches('0').trim_end_matches('.').to_string()
}

/// What lands in the clipboard for a sheet entry.
/// Copies specifically the fact hours (e.g. "1.72").
fn clip_text(hours: Option<f64>, comment: &str) -> String {
    if let Some(h) = hours {
        if h > 0.0 {
            return fmt_hours(h);
        }
    }
    clean_comment_text(comment)
}

// ───────────────────────── main ─────────────────────────

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_frame() -> &'static str {
    let ms = Local::now().timestamp_millis();
    SPINNER[((ms / 80) as usize) % SPINNER.len()]
}

fn main() -> io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("sync") {
        println!("Syncing spreadsheet from Google Drive...");
        match sync::sync_spreadsheet(sync::DEFAULT_SPREADSHEET_ID, &sheet::sheet_path()) {
            Ok(msg) => {
                println!("✔ {}", msg);
                return Ok(());
            }
            Err(e) => {
                eprintln!("❌ Sync failed: {}", e);
                std::process::exit(1);
            }
        }
    }

    let mut terminal = ratatui::init();
    let mut app = App::load();
    let res = (|| -> io::Result<()> {
        while !app.quit {
            terminal.draw(|f| ui(f, &app))?;
            if event::poll(StdDuration::from_millis(80))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        app.on_key(k.code, k.modifiers);
                    }
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    let (stopped, stopped_idx) = app.stop();
    if stopped {
        if let Some(i) = stopped_idx {
            app.sync_entry_to_sheet(i, false);
        }
    }
    app.save();
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_legacy_json_backward_compatibility() {
        let legacy_json = r#"{
            "project": "general",
            "description": "legacy task",
            "tags": [],
            "start": "2026-11-23T10:00:00+05:00",
            "end": "2026-11-23T10:30:00+05:00",
            "done": true
        }"#;

        let entry: Entry = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(entry.duration().num_minutes(), 30);
        assert!(!entry.running());
        assert_eq!(entry.accumulated_sec, 0);
    }

    #[test]
    fn test_pause_and_resume_duration() {
        let now = Local::now();
        let mut entry = Entry {
            project: "crm".into(),
            description: "testing pause".into(),
            tags: vec![],
            start: now - Duration::minutes(15),
            end: None,
            done: false,
            accumulated_sec: 0,
            first_start: Some(now - Duration::minutes(15)),
        };

        // running: 15 minutes
        assert_eq!(entry.duration().num_minutes(), 15);
        assert!(entry.running());

        // pause entry
        entry.accumulated_sec += (now - entry.start).num_seconds();
        entry.end = Some(now);
        assert!(!entry.running());
        assert_eq!(entry.duration().num_minutes(), 15);

        // simulate 30 minute pause by advancing current run start
        let resume_time = now + Duration::minutes(30);
        entry.start = resume_time;
        entry.end = None; // resume
        assert!(entry.running());

        // now simulated time is 10 minutes into the new run
        let check_time = resume_time + Duration::minutes(10);
        let simulated_dur = Duration::seconds(entry.accumulated_sec) + (check_time - entry.start);
        assert_eq!(simulated_dur.num_minutes(), 25); // 15m + 10m, pause ignored!
        assert_eq!(entry.started_at(), now - Duration::minutes(15)); // original start preserved
    }

    #[test]
    fn test_russian_durations_and_numbers() {
        assert_eq!(parse_dur("1h").unwrap().num_minutes(), 60);
        assert_eq!(parse_dur("1ч").unwrap().num_minutes(), 60);
        assert_eq!(parse_dur("0.5ч").unwrap().num_minutes(), 30);
        assert_eq!(parse_dur("0,5ч").unwrap().num_minutes(), 30);
        assert_eq!(parse_dur("0.5").unwrap().num_minutes(), 30);
        assert_eq!(parse_dur("0,5").unwrap().num_minutes(), 30);
        assert_eq!(parse_dur("1.5").unwrap().num_minutes(), 90);
        assert_eq!(parse_dur("1,5ч").unwrap().num_minutes(), 90);
        assert_eq!(parse_dur("30м").unwrap().num_minutes(), 30);
        assert_eq!(parse_dur("30мин").unwrap().num_minutes(), 30);
        assert_eq!(parse_dur("1ч30м").unwrap().num_minutes(), 90);
        assert_eq!(parse_dur("2часа").unwrap().num_minutes(), 120);
        assert_eq!(parse_dur("5часов").unwrap().num_minutes(), 300);
    }

    #[test]
    fn test_parse_input_with_time_first_and_tags() {
        let p1 = parse_input("Полигон: 0.5 #баги");
        assert_eq!(p1.project, "Полигон");
        assert_eq!(p1.tags, vec!["баги"]);
        assert_eq!(p1.dur.unwrap().num_minutes(), 30);

        let p2 = parse_input("Полигон: 1ч #баги");
        assert_eq!(p2.project, "Полигон");
        assert_eq!(p2.tags, vec!["баги"]);
        assert_eq!(p2.dur.unwrap().num_minutes(), 60);

        let p3 = parse_input("Полигон: 0.5ч #баги");
        assert_eq!(p3.project, "Полигон");
        assert_eq!(p3.tags, vec!["баги"]);
        assert_eq!(p3.dur.unwrap().num_minutes(), 30);

        let p4 = parse_input("Полигон: 1 #баги");
        assert_eq!(p4.project, "Полигон");
        assert_eq!(p4.tags, vec!["баги"]);
        assert_eq!(p4.dur.unwrap().num_minutes(), 60);
    }

    #[test]
    fn test_parse_sheet_input_variations() {
        assert_eq!(parse_sheet_input("0.5 #баги"), (Some(0.5), "#баги".into()));
        assert_eq!(parse_sheet_input("1ч #баги"), (Some(1.0), "#баги".into()));
        assert_eq!(parse_sheet_input("0.5ч #баги"), (Some(0.5), "#баги".into()));
        assert_eq!(parse_sheet_input("1.5ч #баги"), (Some(1.5), "#баги".into()));
        assert_eq!(parse_sheet_input("1 #баги"), (Some(1.0), "#баги".into()));
        assert_eq!(parse_sheet_input("#баги 0.5"), (Some(0.5), "#баги".into()));
        assert_eq!(parse_sheet_input("#баги 1ч"), (Some(1.0), "#баги".into()));
        assert_eq!(parse_sheet_input("1ч 30м #баги"), (Some(1.5), "#баги".into()));
    }

    #[test]
    fn test_start_new_with_duration() {
        let mut app = App {
            entries: Vec::new(),
            view: 0,
            sel: 0,
            mode: Mode::Normal,
            input: String::new(),
            cursor: 0,
            fmt: [Fmt::Both; 4],
            status: String::new(),
            error: false,
            quit: false,
            sheet: None,
            sheet_err: String::new(),
        };

        let p = parse_input("ГК ВИРА: 1 #баги");
        app.start_new(p);
        assert_eq!(app.entries.len(), 1);
        let e = &app.entries[0];
        assert_eq!(e.project, "ГК ВИРА");
        assert_eq!(e.accumulated_sec, 3600);
        assert_eq!(e.duration().num_minutes(), 60);
        assert!(e.end.is_some()); // recorded and stopped
    }

    #[test]
    fn test_clip_text_only_fact_hours() {
        assert_eq!(clip_text(Some(1.72), "#баги"), "1.72");
        assert_eq!(clip_text(Some(1.0), "#баги"), "1");
        assert_eq!(clip_text(Some(0.5), "комментарий"), "0.5");
        assert_eq!(clip_text(Some(0.42), ""), "0.42");
        assert_eq!(clip_text(None, "только заметка"), "только заметка");
    }

    #[test]
    fn test_entry_as_edit_input_includes_duration() {
        let now = Local::now();
        let mk = |secs: i64, desc: &str, tags: Vec<&str>| Entry {
            project: "crm".into(),
            description: desc.into(),
            tags: tags.into_iter().map(String::from).collect(),
            start: now,
            end: Some(now),
            done: false,
            accumulated_sec: secs,
            first_start: Some(now),
        };

        assert_eq!(mk(80 * 60, "fix bug", vec!["api"]).as_edit_input(), "crm: fix bug #api 1h20m");
        assert_eq!(mk(60 * 60, "review", vec![]).as_edit_input(), "crm: review 1h");
        assert_eq!(mk(25 * 60, "", vec![]).as_edit_input(), "crm: 25m");
        assert_eq!(mk(0, "just started", vec![]).as_edit_input(), "crm: just started");
        assert_eq!(mk(80 * 60, "fix bug", vec!["api"]).as_input(), "crm: fix bug #api");
    }

    #[test]
    fn test_per_tab_format() {
        let mut app = App {
            entries: Vec::new(),
            view: 0,
            sel: 0,
            mode: Mode::Normal,
            input: String::new(),
            cursor: 0,
            fmt: [Fmt::Both; 4],
            status: String::new(),
            error: false,
            quit: false,
            sheet: None,
            sheet_err: String::new(),
        };

        app.cycle_fmt();
        assert!(matches!(app.cur_fmt(), Fmt::HoursMin));

        app.view = 3;
        assert!(matches!(app.cur_fmt(), Fmt::Both));
        app.cycle_fmt();
        app.cycle_fmt();
        assert!(matches!(app.cur_fmt(), Fmt::Decimal));

        app.view = 0;
        assert!(matches!(app.cur_fmt(), Fmt::HoursMin));
    }
}

