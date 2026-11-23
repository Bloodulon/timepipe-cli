//! Reads and surgically patches the monthly "Загрузка" workbook.
//!
//! We never re-serialize the workbook: only the touched XML parts (sheet,
//! comments, vml, workbook calc flag) are rewritten, every other zip entry is
//! copied byte for byte, so charts, styles, threaded comments, links, etc. stay.

use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use chrono::{Datelike, Duration, Local, NaiveDate};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

pub type Res<T> = Result<T, String>;

fn e<E: std::fmt::Display>(x: E) -> String {
    x.to_string()
}

const MONTHS: [&str; 12] = [
    "Январь", "Февраль", "Март", "Апрель", "Май", "Июнь", "Июль", "Август", "Сентябрь", "Октябрь",
    "Ноябрь", "Декабрь",
];

pub const FIRST_TASK_ROW: u32 = 6;
const LAST_TASK_ROW: u32 = 400;

pub struct Day {
    pub date: NaiveDate,
    /// 1-based column of the "№" cell; plan = col+1, fact = col+2
    pub col: u32,
}

pub struct Sheet {
    pub path: PathBuf,
    pub months: Vec<(String, String)>, // (sheet name, zip path)
    pub cur: usize,
    shared: Vec<String>,
    cells: HashMap<(u32, u32), String>,
    pub days: Vec<Day>,
    pub tasks: Vec<u32>,
    /// cell ref -> (is_threaded, text)
    pub comments: HashMap<String, (bool, String)>,
    pub day: usize,
    pub row: usize,
}

// ───────────────────────── small xml helpers ─────────────────────────

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!(" {}=\"", name);
    let i = tag.find(&pat)? + pat.len();
    let j = tag[i..].find('"')?;
    Some(unescape(&tag[i..i + j]))
}

/// every start tag beginning with `open` (e.g. "<sheet "), up to and including '>'
fn tags(xml: &str, open: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find(open) {
        let s = pos + rel;
        match xml[s..].find('>') {
            Some(g) => {
                out.push(xml[s..=s + g].to_string());
                pos = s + g + 1;
            }
            None => break,
        }
    }
    out
}

fn between<'a>(s: &'a str, a: &str, b: &str) -> Option<&'a str> {
    let i = s.find(a)? + a.len();
    let j = s[i..].find(b)?;
    Some(&s[i..i + j])
}

/// concatenated text of all <t> elements (ignores phonetic runs)
fn texts(body: &str) -> String {
    let mut body = body.to_string();
    while let (Some(i), Some(j)) = (body.find("<rPh"), body.find("</rPh>")) {
        if j < i {
            break;
        }
        body.replace_range(i..j + 6, "");
    }
    let mut out = String::new();
    let mut pos = 0;
    while let Some(rel) = body[pos..].find("<t") {
        let s = pos + rel;
        let next = body[s + 2..].chars().next().unwrap_or(' ');
        if next != '>' && next != ' ' && next != '/' {
            pos = s + 2;
            continue;
        }
        let gt = match body[s..].find('>') {
            Some(g) => s + g,
            None => break,
        };
        if body[..gt].ends_with('/') {
            pos = gt + 1;
            continue;
        }
        let end = match body[gt..].find("</t>") {
            Some(x) => gt + x,
            None => break,
        };
        out.push_str(&unescape(&body[gt + 1..end]));
        pos = end + 4;
    }
    out
}

pub fn col_name(mut c: u32) -> String {
    let mut s = String::new();
    while c > 0 {
        let r = (c - 1) % 26;
        s.insert(0, (b'A' + r as u8) as char);
        c = (c - 1) / 26;
    }
    s
}

fn col_idx(s: &str) -> u32 {
    s.bytes().fold(0, |a, b| a * 26 + (b - b'A' + 1) as u32)
}

pub fn cell_ref(row: u32, col: u32) -> String {
    format!("{}{}", col_name(col), row)
}

/// "AL8" -> (col 38, row 8)
fn split_ref(r: &str) -> (u32, u32) {
    let letters: String = r.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let digits: String = r.chars().skip(letters.len()).collect();
    (col_idx(&letters.to_uppercase()), digits.parse().unwrap_or(0))
}

fn read_entry(path: &Path, name: &str) -> Res<String> {
    let f = fs::File::open(path).map_err(e)?;
    let mut z = ZipArchive::new(f).map_err(e)?;
    let mut file = z.by_name(name).map_err(|x| format!("{name}: {x}"))?;
    let mut s = String::new();
    file.read_to_string(&mut s).map_err(e)?;
    Ok(s)
}

fn read_entry_opt(path: &Path, name: &str) -> Option<String> {
    read_entry(path, name).ok()
}

fn resolve_rel(target: &str) -> String {
    if let Some(t) = target.strip_prefix('/') {
        t.to_string()
    } else {
        format!("xl/{}", target.trim_start_matches("../"))
    }
}

fn fmt_num(v: f64) -> String {
    format!("{}", (v * 100.0).round() / 100.0)
}

// ───────────────────────── reading ─────────────────────────

fn parse_shared(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find("<si>") {
        let s = pos + rel + 4;
        let end = match xml[s..].find("</si>") {
            Some(x) => s + x,
            None => break,
        };
        out.push(texts(&xml[s..end]));
        pos = end + 5;
    }
    out
}

fn parse_cells(xml: &str, shared: &[String]) -> HashMap<(u32, u32), String> {
    let mut out = HashMap::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find("<c ") {
        let start = pos + rel;
        let gt = match xml[start..].find('>') {
            Some(g) => start + g,
            None => break,
        };
        let tag = &xml[start..=gt];
        let self_close = tag.ends_with("/>");
        let end = if self_close {
            gt + 1
        } else {
            match xml[gt..].find("</c>") {
                Some(x) => gt + x + 4,
                None => break,
            }
        };
        if !self_close {
            let body = &xml[gt + 1..end - 4];
            let t = attr(tag, "t").unwrap_or_default();
            let val = match t.as_str() {
                "s" => between(body, "<v>", "</v>")
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .and_then(|i| shared.get(i).cloned()),
                "inlineStr" => Some(texts(body)),
                _ => between(body, "<v>", "</v>").map(unescape),
            };
            if let (Some(v), Some(r)) = (val, attr(tag, "r")) {
                if !v.is_empty() {
                    let (c, rw) = split_ref(&r);
                    out.insert((rw, c), v);
                }
            }
        }
        pos = end;
    }
    out
}

impl Sheet {
    /// `orig` is never modified: we work on `<name>.timepipe.xlsx` next to it.
    pub fn open(orig: &Path) -> Res<Sheet> {
        let stem = orig.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let work = orig.with_file_name(format!("{stem}.timepipe.xlsx"));
        let path = if orig.to_string_lossy().ends_with(".timepipe.xlsx") {
            orig.to_path_buf()
        } else {
            if !work.exists() {
                fs::copy(orig, &work).map_err(e)?;
            }
            work
        };

        let wb = read_entry(&path, "xl/workbook.xml")?;
        let rels_xml = read_entry(&path, "xl/_rels/workbook.xml.rels")?;
        let mut rels: HashMap<String, String> = HashMap::new();
        for t in tags(&rels_xml, "<Relationship ") {
            if let (Some(id), Some(target)) = (attr(&t, "Id"), attr(&t, "Target")) {
                rels.insert(id, resolve_rel(&target));
            }
        }
        let mut months = Vec::new();
        for t in tags(&wb, "<sheet ") {
            let name = attr(&t, "name").unwrap_or_default();
            if MONTHS.contains(&name.as_str()) {
                if let Some(file) = attr(&t, "r:id").and_then(|id| rels.get(&id).cloned()) {
                    months.push((name, file));
                }
            }
        }
        if months.is_empty() {
            return Err("no month sheets (Май, Июнь, ...) found in workbook".into());
        }
        let shared = read_entry_opt(&path, "xl/sharedStrings.xml").map(|x| parse_shared(&x)).unwrap_or_default();

        let this_month = MONTHS[Local::now().month0() as usize];
        let cur = months.iter().position(|m| m.0 == this_month).unwrap_or(months.len() - 1);
        let mut s = Sheet {
            path,
            months,
            cur,
            shared,
            cells: HashMap::new(),
            days: Vec::new(),
            tasks: Vec::new(),
            comments: HashMap::new(),
            day: 0,
            row: 0,
        };
        s.load_month()?;
        Ok(s)
    }

    pub fn file_name(&self) -> String {
        self.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    }

    pub fn month_name(&self) -> &str {
        &self.months[self.cur].0
    }

    fn load_month(&mut self) -> Res<()> {
        let sfile = self.months[self.cur].1.clone();
        let xml = read_entry(&self.path, &sfile)?;
        self.cells = parse_cells(&xml, &self.shared);

        // day headers: numeric date serials in row 3, from column I on
        let epoch = NaiveDate::from_ymd_opt(1899, 12, 30).unwrap();
        let mut days: Vec<Day> = self
            .cells
            .iter()
            .filter(|((r, c), _)| *r == 3 && *c >= 9)
            .filter_map(|((_, c), v)| {
                let n: f64 = v.parse().ok()?;
                if n < 40000.0 {
                    return None;
                }
                Some(Day { date: epoch + Duration::days(n as i64), col: *c })
            })
            .collect();
        days.sort_by_key(|d| d.col);
        self.days = days;

        let mut rows: Vec<u32> = (FIRST_TASK_ROW..=LAST_TASK_ROW)
            .filter(|r| self.cells.contains_key(&(*r, 4)) || self.cells.contains_key(&(*r, 2)))
            .collect();
        rows.sort();
        self.tasks = rows;

        // comments
        self.comments.clear();
        if let Ok(Some(cp)) = self.sheet_rel(&sfile, "/comments") {
            if let Some(cx) = read_entry_opt(&self.path, &cp) {
                let mut pos = 0;
                while let Some(rel) = cx[pos..].find("<comment ") {
                    let s = pos + rel;
                    let gt = match cx[s..].find('>') {
                        Some(g) => s + g,
                        None => break,
                    };
                    let tag = cx[s..=gt].to_string();
                    let end = cx[gt..].find("</comment>").map(|x| gt + x).unwrap_or(cx.len());
                    let body = &cx[gt + 1..end];
                    if let Some(r) = attr(&tag, "ref") {
                        let threaded = attr(&tag, "authorId").map(|a| a != "0").unwrap_or(false);
                        self.comments.insert(r, (threaded, texts(body)));
                    }
                    pos = end;
                }
            }
        }
        if let Ok(Some(tp)) = self.sheet_rel(&sfile, "/threadedComment") {
            if let Some(tx) = read_entry_opt(&self.path, &tp) {
                let mut by_ref: HashMap<String, Vec<String>> = HashMap::new();
                let mut pos = 0;
                while let Some(rel) = tx[pos..].find("<threadedComment ") {
                    let s = pos + rel;
                    let gt = match tx[s..].find('>') {
                        Some(g) => s + g,
                        None => break,
                    };
                    let tag = tx[s..=gt].to_string();
                    let end = tx[gt..].find("</threadedComment>").map(|x| gt + x).unwrap_or(tx.len());
                    if let (Some(r), Some(t)) = (attr(&tag, "ref"), between(&tx[gt..end], "<text>", "</text>")) {
                        by_ref.entry(r).or_default().push(unescape(t));
                    }
                    pos = end;
                }
                for (r, v) in by_ref {
                    self.comments.insert(r, (true, v.join("\n↳ ")));
                }
            }
        }

        let today = Local::now().date_naive();
        self.day = self.days.iter().position(|d| d.date == today).unwrap_or(0);
        self.row = 0;
        Ok(())
    }

    pub fn switch_month(&mut self, delta: i32) -> Res<()> {
        let n = self.months.len() as i32;
        self.cur = (((self.cur as i32 + delta) % n + n) % n) as usize;
        self.load_month()
    }

    fn sheet_rel(&self, sfile: &str, kind: &str) -> Res<Option<String>> {
        let (dir, file) = sfile.rsplit_once('/').ok_or("bad sheet path".to_string())?;
        let rp = format!("{dir}/_rels/{file}.rels");
        let Some(x) = read_entry_opt(&self.path, &rp) else { return Ok(None) };
        for t in tags(&x, "<Relationship ") {
            if attr(&t, "Type").unwrap_or_default().ends_with(kind) {
                if let Some(target) = attr(&t, "Target") {
                    return Ok(Some(resolve_rel(&target)));
                }
            }
        }
        Ok(None)
    }

    // ───────────── accessors ─────────────

    pub fn text(&self, row: u32, col: u32) -> &str {
        self.cells.get(&(row, col)).map(|s| s.as_str()).unwrap_or("")
    }

    pub fn num(&self, row: u32, col: u32) -> f64 {
        self.cells.get(&(row, col)).and_then(|v| v.replace(',', ".").parse().ok()).unwrap_or(0.0)
    }

    pub fn plan(&self, row: u32, day: usize) -> f64 {
        self.days.get(day).map(|d| self.num(row, d.col + 1)).unwrap_or(0.0)
    }

    pub fn fact(&self, row: u32, day: usize) -> f64 {
        self.days.get(day).map(|d| self.num(row, d.col + 2)).unwrap_or(0.0)
    }

    pub fn day_plan(&self, day: usize) -> f64 {
        self.tasks.iter().map(|&r| self.plan(r, day)).sum()
    }

    pub fn day_fact(&self, day: usize) -> f64 {
        self.tasks.iter().map(|&r| self.fact(r, day)).sum()
    }

    pub fn cur_fact(&self) -> f64 {
        self.tasks.get(self.row).map(|&r| self.fact(r, self.day)).unwrap_or(0.0)
    }

    pub fn comment_at(&self, row: u32, col: u32) -> Option<&(bool, String)> {
        self.comments.get(&cell_ref(row, col))
    }

    // ───────────── writing ─────────────

    pub fn find_task(&self, project: &str, description: &str, tags: &[String]) -> Option<u32> {
        let norm = |s: &str| -> String {
            s.replace('\u{a0}', " ").trim().to_lowercase()
        };

        let p_clean = norm(project);
        if p_clean.is_empty() || p_clean == "general" {
            return None;
        }
        let d_clean = norm(description);

        // 1. Best match: project matches AND task description/role matches
        for &r in &self.tasks {
            let sp = norm(self.text(r, 2));
            let st = norm(self.text(r, 4));
            let sr = norm(self.text(r, 5));

            if (sp == p_clean || sp.contains(&p_clean) || p_clean.contains(&sp)) && !st.is_empty() {
                if !d_clean.is_empty() && (st.contains(&d_clean) || d_clean.contains(&st)) {
                    return Some(r);
                }
                for tag in tags {
                    let t_lower = norm(tag);
                    if sr.contains(&t_lower) || st.contains(&t_lower) {
                        return Some(r);
                    }
                }
            }
        }

        // 2. Exact project match
        for &r in &self.tasks {
            let sp = norm(self.text(r, 2));
            if sp == p_clean {
                return Some(r);
            }
        }

        // 3. Substring project match
        for &r in &self.tasks {
            let sp = norm(self.text(r, 2));
            if !sp.is_empty() && (sp.contains(&p_clean) || p_clean.contains(&sp)) {
                return Some(r);
            }
        }

        None
    }

    /// Sets "Факт" of a specific row and day_idx.
    pub fn set_fact_at(
        &mut self,
        row: u32,
        day_idx: usize,
        hours: Option<f64>,
        clear: bool,
        comment: Option<String>,
    ) -> Res<String> {
        let dcol = self.days.get(day_idx).ok_or("no day selected".to_string())?.col;
        let fcol = dcol + 2;
        let sfile = self.months[self.cur].1.clone();
        let mut patches: HashMap<String, String> = HashMap::new();

        let mut upd: Vec<(u32, Option<String>)> = Vec::new();
        if clear {
            upd.push((fcol, None));
        } else if let Some(h) = hours {
            let v = fmt_num(h);
            upd.push((fcol, Some(v)));
        }
        if !upd.is_empty() {
            let mut sxml = read_entry(&self.path, &sfile)?;
            for (c, v) in &upd {
                set_cell(&mut sxml, row, *c, v.as_deref())?;
            }
            patches.insert(sfile.clone(), sxml);
        }

        let rf = cell_ref(row, fcol);
        let mut note = String::new();
        let mut new_comment_text: Option<String> = None;
        if let Some(text) = &comment {
            let cp = self.sheet_rel(&sfile, "/comments")?;
            let vp = self.sheet_rel(&sfile, "/vmlDrawing")?;
            match (cp, vp) {
                (Some(cp), Some(vp)) => {
                    let mut cx = read_entry(&self.path, &cp)?;
                    match add_comment(&mut cx, &rf, text)? {
                        CommentOutcome::New => {
                            let mut vx = read_entry(&self.path, &vp)?;
                            add_vml_shape(&mut vx, row, fcol);
                            patches.insert(vp, vx);
                            patches.insert(cp, cx);
                            new_comment_text = Some(text.clone());
                            note = " + note".into();
                        }
                        CommentOutcome::Appended => {
                            patches.insert(cp, cx);
                            new_comment_text = Some(text.clone());
                            note = " + note appended".into();
                        }
                        CommentOutcome::Skipped => {
                            note = " (cell already has a threaded comment: note skipped)".into();
                        }
                    }
                }
                _ => note = " (sheet has no comments part: note skipped)".into(),
            }
        }
        if patches.is_empty() {
            return Err(format!("nothing written{note}"));
        }

        let mut wb = read_entry(&self.path, "xl/workbook.xml")?;
        if patch_calc(&mut wb) {
            patches.insert("xl/workbook.xml".into(), wb);
        }
        rewrite_zip(&self.path, &patches)?;

        for (c, v) in upd {
            match v {
                Some(s) => {
                    self.cells.insert((row, c), s);
                }
                None => {
                    self.cells.remove(&(row, c));
                }
            }
        }
        if let Some(t) = new_comment_text {
            match self.comments.get_mut(&rf) {
                Some(entry) => {
                    entry.1.push('\n');
                    entry.1.push_str(&t);
                }
                None => {
                    self.comments.insert(rf.clone(), (false, t));
                }
            }
        }
        let date = self.days[day_idx].date.format("%d.%m");
        Ok(if clear {
            format!("cleared {rf} ({date}){note}")
        } else {
            format!("{rf} ({date}) = {}h{note}", fmt_num(hours.unwrap_or(0.0)))
        })
    }

    /// Sets "Факт" of the selected task/day.
    /// `clear` empties the fact cell. `comment` is attached as a cell note.
    pub fn set_fact(&mut self, hours: Option<f64>, clear: bool, comment: Option<String>) -> Res<String> {
        let row = *self.tasks.get(self.row).ok_or("no task selected".to_string())?;
        self.set_fact_at(row, self.day, hours, clear, comment)
    }
}

// ───────────────────────── xml patching ─────────────────────────

fn mk_cell(r: &str, style: &str, val: Option<&str>) -> String {
    match val {
        Some(v) => format!("<c r=\"{r}\"{style}><v>{v}</v></c>"),
        None => format!("<c r=\"{r}\"{style}/>"),
    }
}

fn set_cell(xml: &mut String, row: u32, col: u32, val: Option<&str>) -> Res<()> {
    let rpat = format!("<row r=\"{row}\"");
    let rs = xml.find(&rpat).ok_or(format!("row {row} not found in sheet xml"))?;
    let open_end = rs + xml[rs..].find('>').ok_or("bad row tag".to_string())?;
    let r_ref = cell_ref(row, col);

    if xml[..open_end].ends_with('/') {
        // empty self-closed row
        let open = xml[rs..open_end - 1].to_string();
        let new = format!("{open}>{}</row>", mk_cell(&r_ref, "", val));
        xml.replace_range(rs..=open_end, &new);
        return Ok(());
    }
    let row_end = open_end + xml[open_end..].find("</row>").ok_or("bad row end".to_string())?;

    let mut pos = open_end + 1;
    let mut insert_at = row_end;
    while let Some(rel) = xml[pos..row_end].find("<c ") {
        let s = pos + rel;
        let gt = s + xml[s..].find('>').ok_or("bad cell".to_string())?;
        let tag = xml[s..=gt].to_string();
        let end = if tag.ends_with("/>") {
            gt + 1
        } else {
            gt + xml[gt..].find("</c>").ok_or("bad cell end".to_string())? + 4
        };
        let (c, _) = split_ref(&attr(&tag, "r").unwrap_or_default());
        if c == col {
            let style = attr(&tag, "s").map(|s| format!(" s=\"{s}\"")).unwrap_or_default();
            xml.replace_range(s..end, &mk_cell(&r_ref, &style, val));
            return Ok(());
        }
        if c > col {
            insert_at = s;
            break;
        }
        pos = end;
    }
    xml.insert_str(insert_at, &mk_cell(&r_ref, "", val));
    Ok(())
}

enum CommentOutcome {
    New,
    Appended,
    Skipped,
}

fn add_comment(cx: &mut String, refc: &str, text: &str) -> Res<CommentOutcome> {
    // existing comment on this cell?
    let mut pos = 0;
    while let Some(rel) = cx[pos..].find("<comment ") {
        let s = pos + rel;
        let gt = s + cx[s..].find('>').ok_or("bad comment".to_string())?;
        let tag = cx[s..=gt].to_string();
        let end = gt + cx[gt..].find("</comment>").ok_or("bad comment end".to_string())?;
        if attr(&tag, "ref").as_deref() == Some(refc) {
            if attr(&tag, "authorId").as_deref() != Some("0") {
                return Ok(CommentOutcome::Skipped);
            }
            let t_end = s + cx[s..end].rfind("</text>").ok_or("bad comment text".to_string())?;
            let run = format!("<r><t xml:space=\"preserve\">\n{}</t></r>", escape(text));
            cx.insert_str(t_end, &run);
            return Ok(CommentOutcome::Appended);
        }
        pos = end;
    }
    let at = cx.rfind("</commentList>").ok_or("no commentList".to_string())?;
    let new = format!(
        "<comment ref=\"{refc}\" authorId=\"0\" shapeId=\"0\"><text><t xml:space=\"preserve\">{}</t></text></comment>",
        escape(text)
    );
    cx.insert_str(at, &new);
    Ok(CommentOutcome::New)
}

fn add_vml_shape(vml: &mut String, row: u32, col: u32) {
    // max existing shape id
    let mut max_id = 1024;
    let mut pos = 0;
    while let Some(rel) = vml[pos..].find("_x0000_s") {
        let s = pos + rel + 8;
        let digits: String = vml[s..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u32>() {
            max_id = max_id.max(n);
        }
        pos = s;
    }
    let (r0, c0) = (row - 1, col - 1);
    let shape = format!(
        "<v:shape id=\"_x0000_s{}\" style=\"visibility:hidden\" fillcolor=\"#ffffe1\" type=\"#_x0000_t202\">\
<v:fill angle=\"0\" color2=\"#ffffe1\"/><v:shadow color=\"black\" obscured=\"t\" on=\"t\"/>\
<v:path o:connecttype=\"none\"/><v:textbox/>\
<x:ClientData ObjectType=\"Note\"><x:MoveWithCells></x:MoveWithCells><x:SizeWithCells></x:SizeWithCells>\
<x:Anchor>{}, 15, {}, 10, {}, 15, {}, 4</x:Anchor><x:AutoFill>False</x:AutoFill>\
<x:Row>{}</x:Row><x:Column>{}</x:Column></x:ClientData></v:shape>",
        max_id + 1,
        c0 + 1,
        r0 + 1,
        c0 + 5,
        r0 + 4,
        r0,
        c0
    );
    if let Some(at) = vml.rfind("</xml>") {
        vml.insert_str(at, &shape);
    }
}

/// make Excel recompute totals on open; returns true if changed
fn patch_calc(wb: &mut String) -> bool {
    if let Some(i) = wb.find("<calcPr") {
        let gt = i + wb[i..].find('>').unwrap_or(0);
        if wb[i..gt].contains("fullCalcOnLoad") {
            return false;
        }
        wb.insert_str(i + 7, " fullCalcOnLoad=\"1\"");
        return true;
    }
    let at = wb.find("<extLst").or_else(|| wb.rfind("</workbook>"));
    match at {
        Some(at) => {
            wb.insert_str(at, "<calcPr fullCalcOnLoad=\"1\"/>");
            true
        }
        None => false,
    }
}

fn rewrite_zip(path: &Path, patches: &HashMap<String, String>) -> Res<()> {
    let src = fs::File::open(path).map_err(e)?;
    let mut z = ZipArchive::new(src).map_err(e)?;
    let tmp = path.with_extension("xlsx.tmp");
    let out = fs::File::create(&tmp).map_err(e)?;
    let mut w = ZipWriter::new(out);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for i in 0..z.len() {
        let name = z.by_index_raw(i).map_err(e)?.name().to_string();
        if let Some(data) = patches.get(&name) {
            w.start_file(name, opts).map_err(e)?;
            w.write_all(data.as_bytes()).map_err(e)?;
        } else {
            let f = z.by_index_raw(i).map_err(e)?;
            w.raw_copy_file(f).map_err(e)?;
        }
    }
    w.finish().map_err(e)?;
    fs::rename(&tmp, path).map_err(e)?;
    Ok(())
}
