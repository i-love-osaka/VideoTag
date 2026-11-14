//! VideoTag desktop app (egui).
//!
//! Add library roots, scan and parse names, probe each file's
//! media streams in the background (the new names' tag segment
//! comes from the file itself), match works against one of three
//! catalogues — TMDB, TheTVDB or Bangumi, picked per run with
//! the toolbar's platform buttons — in a background thread, and
//! preview the new library layout.
//! Execution (phase 2) moves the checked, conflict-free files
//! to their previewed targets — in place, or under a configured
//! output folder — preferring pure renames so files living on a
//! network share never travel through this machine.

// On Windows, run as a GUI app: no console window behind the UI.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use videotag_core::{IdPlatform, NameLang, ParsedName, WorkTitle};
use videotag_sources::tvdb::TvdbCredentials;
use videotag_sources::{Candidate, Episode, MediaType, TmdbLanguage};

const VIDEO_EXTS: [&str; 12] = [
    "mkv", "mp4", "avi", "mov", "wmv", "flv", "ts", "m2ts", "rmvb", "webm", "mpg", "mpeg",
];
const SUB_EXTS: [&str; 6] = ["srt", "ass", "ssa", "vtt", "sub", "sup"];
/// Videos smaller than this are marked as extras (trailers,
/// samples) in the list. They still start out checked, like every
/// other row.
const SMALL_LIMIT: u64 = 50 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Language (follows the system, like MusicTag)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    Zh,
    En,
}

fn t(lang: Lang, zh: &'static str, en: &'static str) -> &'static str {
    match lang {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// The TMDB payload language follows the UI language: naming is
/// done from the same payloads, so a Chinese system names files
/// in Chinese and an English system names them in English.
fn tmdb_language(lang: Lang) -> TmdbLanguage {
    match lang {
        Lang::Zh => TmdbLanguage::ZhCn,
        Lang::En => TmdbLanguage::EnUs,
    }
}

/// The core naming language follows the UI language too (it picks
/// the episode-placeholder wording).
fn name_language(lang: Lang) -> NameLang {
    match lang {
        Lang::Zh => NameLang::Zh,
        Lang::En => NameLang::En,
    }
}

fn detect_lang() -> Lang {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLanguages"])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(first) = text.split('"').nth(1) {
                return if first.trim().to_lowercase().starts_with("zh") {
                    Lang::Zh
                } else {
                    Lang::En
                };
            }
        }
        if let Ok(out) = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLocale"])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
            if !text.is_empty() {
                return if text.starts_with("zh") { Lang::Zh } else { Lang::En };
            }
        }
    }
    for key in ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_lowercase();
            if !v.is_empty() {
                return if v.starts_with("zh") { Lang::Zh } else { Lang::En };
            }
        }
    }
    Lang::En
}

// ---------------------------------------------------------------------------
// Settings (JSON in the app data dir)
// ---------------------------------------------------------------------------

struct Settings {
    api_key: String,
    /// TheTVDB API v4 key; empty disables the TVDB match leg.
    tvdb_api_key: String,
    /// TheTVDB subscriber PIN (only some key types need one).
    tvdb_pin: String,
    /// Phase 2: remove directories left empty by a rename run.
    delete_empty_dirs: bool,
    /// Keep the parsed technical tags in the new file names.
    keep_tech_tags: bool,
    /// Where executed files go: the previewed library layout is
    /// rebuilt under this root. None = rename in place, under
    /// each root the files were scanned from.
    output_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            tvdb_api_key: String::new(),
            tvdb_pin: String::new(),
            delete_empty_dirs: true,
            keep_tech_tags: true,
            output_dir: None,
        }
    }
}

fn settings_file() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else if cfg!(target_os = "windows") {
        std::env::var("APPDATA").ok().map(PathBuf::from)
    } else {
        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/share"))
    }?;
    Some(base.join("VideoTag").join("settings.json"))
}

fn load_settings() -> Settings {
    let mut out = Settings::default();
    let Some(file) = settings_file() else { return out };
    let Ok(text) = std::fs::read_to_string(file) else { return out };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return out;
    };
    if let Some(k) = value.get("api_key").and_then(|v| v.as_str()) {
        out.api_key = k.to_string();
    }
    if let Some(k) = value.get("tvdb_api_key").and_then(|v| v.as_str()) {
        out.tvdb_api_key = k.to_string();
    }
    if let Some(k) = value.get("tvdb_pin").and_then(|v| v.as_str()) {
        out.tvdb_pin = k.to_string();
    }
    let flag = |name: &str, slot: &mut bool| {
        if let Some(b) = value.get(name).and_then(|v| v.as_bool()) {
            *slot = b;
        }
    };
    // Note: older settings files may still carry an
    // "episode_title" key (episode titles are a fixed part of the
    // file-name format now, no longer a setting); it is ignored.
    flag("delete_empty_dirs", &mut out.delete_empty_dirs);
    flag("keep_tech_tags", &mut out.keep_tech_tags);
    if let Some(p) = value.get("output_dir").and_then(|v| v.as_str()) {
        if !p.trim().is_empty() {
            out.output_dir = Some(PathBuf::from(p));
        }
    }
    // Credentials pasted from a chat or a web page often carry a
    // trailing newline or stray spaces — TheTVDB rejects such a
    // key outright ("apikey invalid"), after which EVERY TVDB
    // match fails (user's bug, 2026-11-14). Normalize on load and
    // on store so a dirty saved key heals itself.
    out.api_key = out.api_key.trim().to_string();
    let tvdb_trimmed = out.tvdb_api_key.trim().to_string();
    let _ = std::mem::replace(&mut out.tvdb_api_key, tvdb_trimmed);
    out.tvdb_pin = out.tvdb_pin.trim().to_string();
    out
}

fn store_settings(settings: &Settings) {
    let Some(file) = settings_file() else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let value = serde_json::json!({
        "api_key": settings.api_key.trim(),
        "tvdb_api_key": settings.tvdb_api_key.trim(),
        "tvdb_pin": settings.tvdb_pin.trim(),
        "delete_empty_dirs": settings.delete_empty_dirs,
        "keep_tech_tags": settings.keep_tech_tags,
        "output_dir": settings.output_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
    });
    let _ = std::fs::write(file, value.to_string());
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// How a work's title was extracted from its directory name.
#[derive(Clone, Copy, PartialEq)]
enum Method {
    Id,
    Book,
    Dash,
    Fallback,
}

impl Method {
    fn label(self, lang: Lang) -> &'static str {
        match self {
            Method::Id => "ID",
            Method::Book => "\u{300a}\u{300b}",
            Method::Dash => t(lang, "破折号", "Dash"),
            Method::Fallback => t(lang, "兜底", "Fallback"),
        }
    }
}

fn method_of(name: &str, parsed: &ParsedName) -> Method {
    if parsed.platform_id().is_some() {
        return Method::Id;
    }
    if let (Some(a), Some(b)) = (name.find('\u{300a}'), name.find('\u{300b}')) {
        let inner = &name[a + '\u{300a}'.len_utf8()..b];
        if b > a && !inner.trim().is_empty() {
            return Method::Book;
        }
    }
    if name.contains('\u{2014}') || name.contains('\u{ff0d}') || name.contains(" - ") {
        return Method::Dash;
    }
    Method::Fallback
}

/// One movie or show: the unit TMDB matching works on.
struct Work {
    /// Directory-name parse (title / year / TMDB id live here).
    parsed: ParsedName,
    method: Method,
    /// Structure says TV (any season/episode evidence in its files).
    is_tv: bool,
    /// The title cannot be searched (empty, purely numeric or one
    /// character) and there is no platform id to look up instead.
    /// Such works are never searched: searching "01" would find
    /// some unrelated movie and report it as a confident match.
    unrecognized: bool,
    searching: bool,
    failed: bool,
    /// Cached candidates from the last search (picker material).
    candidates: Vec<Candidate>,
    chosen: Option<usize>,
    /// Detail record of the chosen candidate (votes + en-US title).
    detail: Option<Candidate>,
    /// Which candidate id `episodes` belongs to.
    episodes_for: Option<u64>,
    episodes: HashMap<u32, Vec<Episode>>,
}

impl Work {
    fn new(parsed: ParsedName, method: Method, is_tv: bool) -> Self {
        let unrecognized = parsed.platform_id().is_none()
            && videotag_core::is_unsearchable_title(&parsed.title);
        Self {
            parsed,
            method,
            is_tv,
            unrecognized,
            searching: false,
            failed: false,
            candidates: Vec::new(),
            chosen: None,
            detail: None,
            episodes_for: None,
            episodes: HashMap::new(),
        }
    }

    /// The candidate previews are built from: the enriched detail
    /// record when available, else the chosen search result.
    fn matched(&self) -> Option<&Candidate> {
        self.detail
            .as_ref()
            .or_else(|| self.chosen.and_then(|i| self.candidates.get(i)))
    }
}

struct Row {
    path: PathBuf,
    root: PathBuf,
    work: usize,
    name: String,
    parsed: ParsedName,
    /// Same-stem subtitle files that will follow this video's rename.
    subtitles: Vec<PathBuf>,
    /// Under the small-file limit: treated as an extra.
    small: bool,
    /// Tag values probed from the file's own streams; when
    /// present they take precedence over the tags parsed from
    /// the original file name.
    media_tags: Option<Vec<String>>,
    checked: bool,
    preview: Option<String>,
    conflict: bool,
    /// Which conflict, when `conflict` is set — drives the
    /// status-column reason and the hover explanation.
    conflict_kind: Option<ConflictKind>,
    /// The episode number is absent from TMDB's season listing.
    ep_missing: bool,
    /// Execution has landed this file at its preview target.
    /// Done rows keep no preview and are never re-executed.
    done: bool,
    /// Why this row's execution failed (or only partially
    /// succeeded); shown red in the status column.
    exec_error: Option<String>,
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().starts_with('.'))
        .unwrap_or(false)
}

/// Directories that never hold renameable content: Blu-ray trees,
/// NAS system folders (Synology's @eaDir holds transcoded copies
/// of the real videos — pure duplicates), recycle/trash and OS
/// metadata folders, and trailer/sample extras folders, which are
/// not episodes of the work (user: filter unrelated files when
/// opening a directory, 2026-11-14). Season directories such as
/// Specials are NOT junk and stay.
fn is_junk_dir(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    matches!(
        n.as_str(),
        "bdmv"
            | "@eadir"
            | ".@__thumb"
            | "#recycle"
            | "$recycle.bin"
            | "system volume information"
            | ".trashes"
            | ".spotlight-v100"
            | ".fseventsd"
            | ".appledouble"
            | ".appledb"
            | "sample"
            | "samples"
            | "trailer"
            | "trailers"
            | "extras"
            | "featurettes"
            | "预告片"
            | "花絮"
    )
}

/// Promotional sample clips: the file name carries "sample" as a
/// whole token (`Show.S01E01.sample.mkv`, `movie-sample.mkv`) —
/// never the episode itself.
fn is_sample_file(path: &Path) -> bool {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    stem.split(|c: char| !c.is_alphanumeric())
        .any(|tok| tok == "sample")
}

/// Recursively collect video and subtitle files under a work
/// directory. Hidden entries and BDMV (Blu-ray) trees are skipped.
fn collect_work_files(dir: &Path, videos: &mut Vec<PathBuf>, subs: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if is_hidden(&path) {
            continue;
        }
        if path.is_dir() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if is_junk_dir(&name) {
                continue;
            }
            collect_work_files(&path, videos, subs);
        } else {
            match ext_of(&path).as_str() {
                e if VIDEO_EXTS.contains(&e) => {
                    if !is_sample_file(&path) {
                        videos.push(path);
                    }
                }
                e if SUB_EXTS.contains(&e) => subs.push(path),
                _ => {}
            }
        }
    }
}

/// Subtitles belonging to one video: same directory, file stem equal
/// to the video's stem or the stem plus a language suffix
/// (`ep01.chs.srt` belongs to `ep01.mkv`).
fn subtitles_for(video: &Path, subs: &[PathBuf]) -> Vec<PathBuf> {
    let stem = video
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.is_empty() {
        return Vec::new();
    }
    let dir = video.parent();
    subs.iter()
        .filter(|s| {
            if s.parent() != dir {
                return false;
            }
            let s_stem = s
                .file_stem()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_default();
            s_stem == stem
                || s_stem
                    .strip_prefix(&stem)
                    .map(|rest| rest.starts_with('.'))
                    .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// Roots that still own at least one row. When every row of a
/// root is removed, the root itself must be forgotten too — a
/// registered-but-empty root makes re-adding that directory hit
/// the duplicate check with nothing in the list.
fn roots_in_use(roots: &[PathBuf], rows: &[Row]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter(|root| rows.iter().any(|r| &r.root == *root))
        .cloned()
        .collect()
}

fn make_row(path: PathBuf, root: &Path, work: usize, subs: &[PathBuf]) -> Row {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Parse from the path relative to the root so the work and
    // season directory names are the ancestors the parser expects.
    let rel = path.strip_prefix(root).unwrap_or(&path);
    let parsed = videotag_core::parse_file_path(&rel.to_string_lossy());
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let small = size > 0 && size < SMALL_LIMIT;
    let subtitles = subtitles_for(&path, subs);
    Row {
        // Every row starts checked (user's rule, 2026-11-14: a
        // freshly opened directory is fully selected). Small files
        // and extras keep their 附属 marking in the list; they are
        // just no longer excluded from the default selection.
        checked: true,
        path,
        root: root.to_path_buf(),
        work,
        name,
        parsed,
        subtitles,
        small,
        media_tags: None,
        preview: None,
        conflict: false,
        conflict_kind: None,
        ep_missing: false,
        done: false,
        exec_error: None,
    }
}

/// Give a row its work's identity: the season falls back to the
/// work directory's own season, then to 1, and the row title
/// becomes the work's title.
fn adopt_work_identity(row: &mut Row, work: &ParsedName) {
    row.parsed.season = row.parsed.season.or(work.season).or(Some(1));
    if !work.title.is_empty() {
        row.parsed.title = work.title.clone();
    }
}

/// Stamp one row as episode `episode` of its work.
fn stamp_numbered_episode(row: &mut Row, episode: u32, work: &ParsedName) {
    row.parsed.episode = Some(episode);
    adopt_work_identity(row, work);
}

/// Numbered-episode grouping inside one work: rows that share a
/// parent directory and have purely numeric file names (two or
/// more of them, per [`videotag_core::detect_numbered_episodes`])
/// are consecutive episodes of the work's show, not separate
/// movies named "01", "02", ... Each gets stamped via
/// [`stamp_numbered_episode`]. Non-numeric rows in the same
/// directory keep their own handling. Returns true when a group
/// was found (the caller then treats the work as TV).
fn apply_numbered_groups(rows: &mut [Row], work: &ParsedName) -> bool {
    let mut by_dir: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        if let Some(parent) = row.path.parent() {
            by_dir.entry(parent.to_path_buf()).or_default().push(i);
        }
    }
    let mut grouped = false;
    for idxs in by_dir.values() {
        let names: Vec<&str> = idxs.iter().map(|&i| rows[i].name.as_str()).collect();
        let episodes = videotag_core::detect_numbered_episodes(&names);
        for (k, ep) in episodes.iter().enumerate() {
            let Some(n) = ep else { continue };
            let row = &mut rows[idxs[k]];
            if row.parsed.episode.is_some() && row.parsed.episode != Some(*n) {
                continue; // a richer pattern already claimed this file
            }
            // The core path parse may already have set this same
            // episode number; stamping again is harmless and still
            // supplies the season and work title.
            stamp_numbered_episode(row, *n, work);
            grouped = true;
        }
    }
    grouped
}

/// Scan one library root: each first-level subdirectory is a work,
/// each loose video file in the root is a work of its own. Loose
/// files whose names are purely numeric (two or more) are one
/// show instead: the root itself is the show's folder then, and
/// its name provides the title.
fn scan_root(root: &Path) -> (Vec<Work>, Vec<Row>) {
    let mut works: Vec<Work> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return (works, rows);
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    // Root-level subtitles, for loose video files.
    let root_subs: Vec<PathBuf> = entries
        .iter()
        .filter(|p| !p.is_dir() && SUB_EXTS.contains(&ext_of(p).as_str()))
        .cloned()
        .collect();
    // Numbered-episode detection over the loose video files, in
    // entry order (the loop below consumes it with the same filter,
    // hidden entries excluded on both sides so indices line up).
    // When the user adds a show's folder directly, these files are
    // its episodes and the root's own name is the show's name.
    let loose_names: Vec<String> = entries
        .iter()
        .filter(|p| {
            !p.is_dir()
                && !is_hidden(p)
                && !is_sample_file(p)
                && VIDEO_EXTS.contains(&ext_of(p).as_str())
        })
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    let loose_refs: Vec<&str> = loose_names.iter().map(|n| n.as_str()).collect();
    let loose_eps = videotag_core::detect_numbered_episodes(&loose_refs);
    let loose_has_group = loose_eps.iter().any(|e| e.is_some());
    // File-name parses of the same loose files, same order. Files
    // that carry an episode number but no usable title of their
    // own ("E01.mkv", "第1集.mkv") are episode candidates: two or
    // more of them in one root are episodes of the show the root
    // folder is, just like the numbered files.
    let loose_parses: Vec<ParsedName> = loose_names
        .iter()
        .map(|n| videotag_core::parse_file_name(n))
        .collect();
    let is_ep_candidate = |p: &ParsedName| {
        p.episode.is_some()
            && p.platform_id().is_none()
            && videotag_core::is_unsearchable_title(&p.title)
    };
    let loose_ep_group =
        loose_parses.iter().filter(|p| is_ep_candidate(p)).count() >= 2;
    let mut loose_i = 0usize;
    // The shared work for a root-level episode group (numbered
    // and/or episode-named files), created lazily at its first
    // member file.
    let mut loose_group_work: Option<usize> = None;
    // The shared work for files found under season directories at
    // the top level (`红楼梦/Season 1/01.mkv` with 红楼梦 itself
    // added as the root): a season directory is not a work, its
    // files are episodes of the show the root folder is. Created
    // lazily at the first season directory that holds videos.
    let mut root_season_work: Option<usize> = None;
    for entry in &entries {
        if is_hidden(entry) {
            continue;
        }
        if entry.is_dir() {
            let dir_name = entry
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if is_junk_dir(&dir_name) {
                continue;
            }
            if let Some(season_no) =
                videotag_core::parse_season_directory_name(&dir_name)
            {
                // A season directory at the top level is not a
                // work of its own: the user added the show's
                // folder itself, so its files merge into the root
                // work (identity from the root folder's name, the
                // same rule as root-level episode groups), each
                // carrying this directory's season number. Several
                // season directories all merge into that one work.
                let (mut videos, mut subs) = (Vec::new(), Vec::new());
                collect_work_files(entry, &mut videos, &mut subs);
                if videos.is_empty() {
                    continue;
                }
                videos.sort();
                let idx = match root_season_work {
                    Some(idx) => idx,
                    None => {
                        let root_name = root
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let parsed =
                            videotag_core::parse_directory_name(&root_name);
                        let method = method_of(&root_name, &parsed);
                        let idx = works.len();
                        works.push(Work::new(parsed, method, true));
                        root_season_work = Some(idx);
                        idx
                    }
                };
                let work_parsed = works[idx].parsed.clone();
                let mut season_rows: Vec<Row> = videos
                    .into_iter()
                    .map(|video| make_row(video, root, idx, &subs))
                    .collect();
                // Stamp the directory's season before grouping,
                // so numbered-episode stamping and identity
                // adoption keep it instead of defaulting to 1.
                // Rows without episode evidence keep their own
                // parse (a stray movie file in a season folder
                // stays a movie row).
                for row in &mut season_rows {
                    if row.parsed.season.is_none()
                        && (row.parsed.episode.is_some()
                            || videotag_core::numeric_episode(&row.name).is_some())
                    {
                        row.parsed.season = Some(season_no);
                    }
                }
                apply_numbered_groups(&mut season_rows, &work_parsed);
                for row in &mut season_rows {
                    if row.parsed.episode.is_none() {
                        // The ≥2 grouping rule may not fire for a
                        // season that holds a single numeric file,
                        // but inside a season directory a numeric
                        // name is unambiguous: that season's
                        // episode n.
                        if let Some(n) =
                            videotag_core::numeric_episode(&row.name)
                        {
                            stamp_numbered_episode(row, n, &work_parsed);
                        }
                    }
                    if row.parsed.episode.is_some() {
                        adopt_work_identity(row, &work_parsed);
                    }
                }
                rows.extend(season_rows);
                continue;
            }
            let (mut videos, mut subs) = (Vec::new(), Vec::new());
            collect_work_files(entry, &mut videos, &mut subs);
            if videos.is_empty() {
                continue;
            }
            videos.sort();
            let parsed = videotag_core::parse_directory_name(&dir_name);
            let method = method_of(&dir_name, &parsed);
            let idx = works.len();
            let mut is_tv = parsed.season.is_some();
            let mut work_rows: Vec<Row> = Vec::new();
            for video in videos {
                let row = make_row(video, root, idx, &subs);
                is_tv |= row.parsed.season.is_some() || row.parsed.episode.is_some();
                work_rows.push(row);
            }
            // 01.mkv, 02.mkv, ... inside this folder are episodes
            // of the show, not movies named "01", "02", ...
            if apply_numbered_groups(&mut work_rows, &parsed) {
                is_tv = true;
            }
            rows.extend(work_rows);
            works.push(Work::new(parsed, method, is_tv));
        } else if VIDEO_EXTS.contains(&ext_of(entry).as_str()) && !is_sample_file(entry) {
            // The is_sample_file term must mirror the loose_names
            // filter above, so loose_i stays aligned.
            let file_parsed = loose_parses[loose_i].clone();
            let numeric_ep = loose_eps[loose_i];
            let ep_candidate = loose_ep_group && is_ep_candidate(&file_parsed);
            loose_i += 1;
            if (loose_has_group && numeric_ep.is_some()) || ep_candidate {
                // One episode of the show the root folder is.
                let idx = match loose_group_work {
                    Some(idx) => idx,
                    None => {
                        let root_name = root
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let parsed = videotag_core::parse_directory_name(&root_name);
                        let method = method_of(&root_name, &parsed);
                        let idx = works.len();
                        works.push(Work::new(parsed, method, true));
                        loose_group_work = Some(idx);
                        idx
                    }
                };
                let mut row = make_row(entry.clone(), root, idx, &root_subs);
                let work_parsed = works[idx].parsed.clone();
                match numeric_ep {
                    Some(n) => stamp_numbered_episode(&mut row, n, &work_parsed),
                    None => adopt_work_identity(&mut row, &work_parsed),
                }
                rows.push(row);
                continue;
            }
            // A work of its own. When its own name says nothing
            // (just "E01.mkv") and the root folder names a specific
            // work — a platform id in the name, or a title set off
            // by 《》 or a dash — the file inherits the root's
            // identity instead of staying unrecognized.
            let file_name = entry
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut parsed = file_parsed;
            let mut method = method_of(&file_name, &parsed);
            if parsed.platform_id().is_none()
                && videotag_core::is_unsearchable_title(&parsed.title)
            {
                let root_name = root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let dir = videotag_core::parse_directory_name(&root_name);
                let root_method = method_of(&root_name, &dir);
                if dir.platform_id().is_some()
                    || matches!(root_method, Method::Book | Method::Dash)
                {
                    if parsed.year.is_none() {
                        parsed.year = dir.year;
                    }
                    if parsed.season.is_none() {
                        parsed.season = dir.season;
                    }
                    parsed.tmdb_id = dir.tmdb_id;
                    parsed.tvdb_id = dir.tvdb_id;
                    parsed.bgm_id = dir.bgm_id;
                    if !videotag_core::is_unsearchable_title(&dir.title) {
                        parsed.title = dir.title;
                    }
                    method = root_method;
                }
            }
            let idx = works.len();
            let is_tv = parsed.season.is_some() || parsed.episode.is_some();
            let mut row = make_row(entry.clone(), root, idx, &root_subs);
            // The row was parsed from its root-relative path, which
            // cannot see the root folder's name: adopt the season
            // the work parse found (default 1) for episode files,
            // so their previews take the TV layout.
            if row.parsed.episode.is_some() && row.parsed.season.is_none() && is_tv {
                row.parsed.season = parsed.season.or(Some(1));
            }
            rows.push(row);
            works.push(Work::new(parsed, method, is_tv));
        }
    }
    (works, rows)
}

// ---------------------------------------------------------------------------
// Online matching (background thread, MusicTag pattern)
// ---------------------------------------------------------------------------

/// Everything the background matcher needs from the settings:
/// the configured platform credentials (TVDB is `None` until a
/// key is set — its toolbar button stays greyed meanwhile) and
/// the payload language.
#[derive(Clone)]
struct SourceConfig {
    tmdb_key: String,
    tvdb: Option<TvdbCredentials>,
    lang: TmdbLanguage,
}

impl SourceConfig {
    fn from_settings(settings: &Settings, lang: Lang) -> Self {
        let tvdb_key = settings.tvdb_api_key.trim();
        Self {
            tmdb_key: settings.api_key.trim().to_string(),
            tvdb: if tvdb_key.is_empty() {
                None
            } else {
                Some(TvdbCredentials {
                    api_key: tvdb_key.to_string(),
                    pin: settings.tvdb_pin.trim().to_string(),
                })
            },
            lang: tmdb_language(lang),
        }
    }

    /// Whether `platform` can be queried at all right now:
    /// Bangumi is keyless, the other two need their key.
    fn platform_ready(&self, platform: IdPlatform) -> bool {
        match platform {
            IdPlatform::Tmdb => !self.tmdb_key.is_empty(),
            IdPlatform::Tvdb => self.tvdb.is_some(),
            IdPlatform::Bangumi => true,
        }
    }
}

/// Short display name of a catalogue, for picker tags and
/// status lines.
fn platform_label(platform: IdPlatform) -> &'static str {
    match platform {
        IdPlatform::Tmdb => "TMDB",
        IdPlatform::Tvdb => "TVDB",
        IdPlatform::Bangumi => "Bangumi",
    }
}

struct WorkJob {
    work: usize,
    title: String,
    year: Option<i32>,
    /// The work's id on the platform this run matches with
    /// (pinned by the directory name's tag or by the manual-ID
    /// dialog): look it up directly instead of searching.
    pinned_id: Option<u64>,
    is_tv: bool,
    seasons: Vec<u32>,
}

enum MatchEvent {
    WorkSearching { work: usize },
    WorkDone {
        work: usize,
        candidates: Vec<Candidate>,
        chosen: usize,
        detail: Option<Candidate>,
        episodes: Vec<(u32, Vec<Episode>)>,
    },
    WorkFailed { work: usize },
    Finished { matched: usize, failed: usize },
}

/// Result of fetching one picked candidate's detail + episodes
/// after the user changes a selection in the picker.
struct EnrichDone {
    work: usize,
    cand_id: u64,
    detail: Option<Candidate>,
    episodes: Vec<(u32, Vec<Episode>)>,
}

fn throttle() {
    std::thread::sleep(Duration::from_millis(100));
}

fn fetch_episodes(
    key: &str,
    cand: &Candidate,
    seasons: &[u32],
    lang: TmdbLanguage,
) -> Vec<(u32, Vec<Episode>)> {
    let mut out = Vec::new();
    if cand.media_type != MediaType::Tv {
        return out;
    }
    for season in seasons {
        if let Ok(eps) = videotag_sources::get_episodes(key, cand.id, *season, lang) {
            out.push((*season, eps));
        }
        throttle();
    }
    out
}

/// Episode lists for a matched candidate, from its own platform:
/// TMDB is fetched per season; TVDB returns the whole listing in
/// one go and keeps the wanted seasons; Bangumi's list belongs
/// to a single season of the show — the work's only season when
/// it has exactly one, else season 1 (a Bangumi subject usually
/// IS one season).
fn fetch_episodes_for(
    cfg: &SourceConfig,
    cand: &Candidate,
    seasons: &[u32],
) -> Vec<(u32, Vec<Episode>)> {
    match cand.source {
        IdPlatform::Tmdb => fetch_episodes(&cfg.tmdb_key, cand, seasons, cfg.lang),
        IdPlatform::Tvdb => {
            let Some(creds) = &cfg.tvdb else {
                return Vec::new();
            };
            if cand.media_type != MediaType::Tv {
                return Vec::new();
            }
            let all = videotag_sources::tvdb::get_episodes(creds, cand.id, cfg.lang)
                .unwrap_or_default();
            throttle();
            all.into_iter()
                .filter(|(season, _)| seasons.contains(season))
                .collect()
        }
        IdPlatform::Bangumi => {
            let eps = videotag_sources::bangumi::get_episodes(cand.id).unwrap_or_default();
            throttle();
            if eps.is_empty() {
                return Vec::new();
            }
            let target = if seasons.len() == 1 { seasons[0] } else { 1 };
            if seasons.is_empty() || seasons.contains(&target) {
                vec![(target, eps)]
            } else {
                Vec::new()
            }
        }
    }
}

/// The detail record for one candidate id on its own platform
/// (best effort — `None` when the fetch fails).
fn fetch_detail(cfg: &SourceConfig, cand: &Candidate) -> Option<Candidate> {
    match cand.source {
        IdPlatform::Tmdb => match cand.media_type {
            MediaType::Tv => videotag_sources::get_tv(&cfg.tmdb_key, cand.id, cfg.lang)
                .ok()
                .flatten(),
            MediaType::Movie => {
                videotag_sources::get_movie(&cfg.tmdb_key, cand.id, cfg.lang)
                    .ok()
                    .flatten()
            }
        },
        IdPlatform::Tvdb => {
            let creds = cfg.tvdb.as_ref()?;
            match cand.media_type {
                MediaType::Tv => videotag_sources::tvdb::get_tv(creds, cand.id, cfg.lang)
                    .ok()
                    .flatten(),
                MediaType::Movie => {
                    videotag_sources::tvdb::get_movie(creds, cand.id, cfg.lang)
                        .ok()
                        .flatten()
                }
            }
        }
        IdPlatform::Bangumi => videotag_sources::bangumi::get_subject(cand.id, cfg.lang)
            .ok()
            .flatten(),
    }
}

/// Direct id lookup on ONE platform: the structure's type first,
/// the other type on a miss (an id alone does not say whether
/// its work is a series or a movie). Bangumi subjects are looked
/// up as-is.
fn lookup_id(
    cfg: &SourceConfig,
    platform: IdPlatform,
    id: u64,
    is_tv: bool,
) -> Option<Candidate> {
    match platform {
        IdPlatform::Bangumi => {
            let found = videotag_sources::bangumi::get_subject(id, cfg.lang)
                .ok()
                .flatten();
            throttle();
            found
        }
        IdPlatform::Tmdb | IdPlatform::Tvdb => {
            let get = |mt: MediaType| -> Option<Candidate> {
                let found = match (platform, mt) {
                    (IdPlatform::Tmdb, MediaType::Tv) => {
                        videotag_sources::get_tv(&cfg.tmdb_key, id, cfg.lang)
                    }
                    (IdPlatform::Tmdb, MediaType::Movie) => {
                        videotag_sources::get_movie(&cfg.tmdb_key, id, cfg.lang)
                    }
                    (IdPlatform::Tvdb, MediaType::Tv) => match &cfg.tvdb {
                        Some(c) => videotag_sources::tvdb::get_tv(c, id, cfg.lang),
                        None => Ok(None),
                    },
                    (IdPlatform::Tvdb, MediaType::Movie) => match &cfg.tvdb {
                        Some(c) => videotag_sources::tvdb::get_movie(c, id, cfg.lang),
                        None => Ok(None),
                    },
                    (IdPlatform::Bangumi, _) => unreachable!(),
                };
                throttle();
                found.ok().flatten()
            };
            let (first, second) = if is_tv {
                (MediaType::Tv, MediaType::Movie)
            } else {
                (MediaType::Movie, MediaType::Tv)
            };
            get(first).or_else(|| get(second))
        }
    }
}

type MatchOutcome = (
    Vec<Candidate>,
    usize,
    Option<Candidate>,
    Vec<(u32, Vec<Episode>)>,
);

/// Title search on TMDB only: the structure's type first, the
/// other type when that finds nothing (existing behavior).
fn tmdb_search_match(cfg: &SourceConfig, job: &WorkJob) -> Option<MatchOutcome> {
    let key = &cfg.tmdb_key;
    let lang = cfg.lang;
    let primary = if job.is_tv {
        videotag_sources::search_tv(key, &job.title, job.year, lang)
    } else {
        videotag_sources::search_movie(key, &job.title, job.year, lang)
    };
    throttle();
    let mut cands = primary.unwrap_or_default();
    if cands.is_empty() {
        let other = if job.is_tv {
            videotag_sources::search_movie(key, &job.title, job.year, lang)
        } else {
            videotag_sources::search_tv(key, &job.title, job.year, lang)
        };
        throttle();
        cands = other.unwrap_or_default();
    }
    finish_search(cfg, cands, job)
}

/// Title search on TheTVDB only, same type-fallback shape.
fn tvdb_search_match(cfg: &SourceConfig, job: &WorkJob) -> Option<MatchOutcome> {
    let creds = cfg.tvdb.as_ref()?;
    let primary = if job.is_tv {
        videotag_sources::tvdb::search_tv(creds, &job.title, job.year, cfg.lang)
    } else {
        videotag_sources::tvdb::search_movie(creds, &job.title, job.year, cfg.lang)
    };
    throttle();
    let mut cands = primary.unwrap_or_default();
    if cands.is_empty() {
        let other = if job.is_tv {
            videotag_sources::tvdb::search_movie(creds, &job.title, job.year, cfg.lang)
        } else {
            videotag_sources::tvdb::search_tv(creds, &job.title, job.year, cfg.lang)
        };
        throttle();
        cands = other.unwrap_or_default();
    }
    finish_search(cfg, cands, job)
}

/// Title search on Bangumi only. Subjects are series-shaped, so
/// movies search here too — the user picked the platform, the
/// pick counts (the [2, 6] subject filter includes animated and
/// live-action films).
fn bangumi_search_match(cfg: &SourceConfig, job: &WorkJob) -> Option<MatchOutcome> {
    let cands =
        videotag_sources::bangumi::search_subjects(&job.title, cfg.lang).unwrap_or_default();
    throttle();
    finish_search(cfg, cands, job)
}

/// Shared tail of a title search on any platform: pick the best
/// candidate, enrich it with its detail record (the folder
/// format needs votes and the English/original title, which only
/// the detail carries on TMDB/TVDB), and fetch its episodes.
fn finish_search(
    cfg: &SourceConfig,
    cands: Vec<Candidate>,
    job: &WorkJob,
) -> Option<MatchOutcome> {
    if cands.is_empty() {
        return None;
    }
    let chosen = videotag_sources::pick_best(&cands, &job.title, job.year)?;
    let pick = cands[chosen].clone();
    let detail = fetch_detail(cfg, &pick);
    throttle();
    let canonical = detail.clone().unwrap_or_else(|| pick.clone());
    let episodes = fetch_episodes_for(cfg, &canonical, &job.seasons);
    Some((cands, chosen, detail, episodes))
}

/// Match one work on ONE user-chosen platform (there is no
/// cross-platform fallback anywhere): by the work's id on that
/// platform when it carries one, else by title search there.
fn match_one(
    cfg: &SourceConfig,
    platform: IdPlatform,
    job: &WorkJob,
) -> Option<MatchOutcome> {
    if let Some(id) = job.pinned_id {
        let cand = lookup_id(cfg, platform, id, job.is_tv)?;
        let episodes = fetch_episodes_for(cfg, &cand, &job.seasons);
        return Some((vec![cand.clone()], 0, Some(cand), episodes));
    }
    if job.title.trim().is_empty() {
        return None;
    }
    match platform {
        IdPlatform::Tmdb => tmdb_search_match(cfg, job),
        IdPlatform::Tvdb => tvdb_search_match(cfg, job),
        IdPlatform::Bangumi => bangumi_search_match(cfg, job),
    }
}

// ---------------------------------------------------------------------------
// Media probing (background, `ffmpeg -i`)
// ---------------------------------------------------------------------------

enum ProbeEvent {
    Done { path: PathBuf, tags: Vec<String> },
}

/// The ffmpeg used for probing: the copy bundled in the app's
/// Resources first (packaged builds ship one), then plain
/// `ffmpeg` from PATH. When neither exists, spawns simply fail
/// and every row keeps its name-parsed tags.
fn ffmpeg_path() -> PathBuf {
    let name = if cfg!(target_os = "windows") {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // .app layout: Contents/MacOS/<exe> with
            // Contents/Resources/ffmpeg next door.
            let bundled = dir.join("..").join("Resources").join(name);
            if bundled.is_file() {
                return bundled;
            }
            let sibling = dir.join("Resources").join(name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from(name)
}

/// Probe one file: run `ffmpeg -i` and parse the stream facts
/// out of its stderr. Any failure yields no tags (the caller
/// falls back to the name-parsed ones).
fn probe_one(ffmpeg: &Path, file: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new(ffmpeg)
        .arg("-i")
        .arg(file)
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stderr);
    videotag_core::parse_ffmpeg_info(&text).tags()
}

// ---------------------------------------------------------------------------
// Execution (background thread; moves files to their previews)
// ---------------------------------------------------------------------------

/// The absolute target of one preview path: under the configured
/// output folder when one is set, else under the root the row
/// was scanned from (rename in place).
fn target_for_preview(root: &Path, preview: &str, settings: &Settings) -> PathBuf {
    match &settings.output_dir {
        Some(base) => base.join(preview),
        None => root.join(preview),
    }
}

/// The absolute path a row's video will occupy after execution.
fn row_target(row: &Row, settings: &Settings) -> Option<PathBuf> {
    row.preview
        .as_deref()
        .map(|p| target_for_preview(&row.root, p, settings))
}

/// Why a row's target conflicts. The two kinds paint the file
/// name the same red but need opposite remedies, so the status
/// column names the kind and a hover tooltip explains it
/// (user's question, 2026-11-14).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConflictKind {
    /// Another row aims at the same target path.
    Duplicate,
    /// A file already sits at the target that is not the
    /// row's own source.
    TargetExists,
}

/// Conflict kinds for parallel (source, target) pairs: a target
/// conflicts when another row aims at the same path (compared
/// case-insensitively, like the filesystems this runs on), or
/// when a file already sits there that is not the row's own
/// source. Execution never overwrites anything. A duplicate
/// target outranks an existing file when both apply.
fn conflicting_kinds(
    sources: &[PathBuf],
    targets: &[PathBuf],
) -> Vec<Option<ConflictKind>> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for target in targets {
        *counts
            .entry(target.to_string_lossy().to_lowercase())
            .or_default() += 1;
    }
    sources
        .iter()
        .zip(targets.iter())
        .map(|(src, target)| {
            let key = target.to_string_lossy().to_lowercase();
            if counts.get(&key).copied().unwrap_or(0) > 1 {
                Some(ConflictKind::Duplicate)
            } else if target != src && target.exists() {
                Some(ConflictKind::TargetExists)
            } else {
                None
            }
        })
        .collect()
}

/// Where one of a row's subtitles lands: next to the renamed
/// video, carrying the video's new stem plus whatever language
/// suffix the subtitle had (`ep01.chs.srt` → `<new>.chs.srt`).
fn subtitle_target(sub: &Path, video_src: &Path, video_dst: &Path) -> PathBuf {
    let dst_dir = video_dst.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let stem_of = |p: &Path| {
        p.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let src_stem = stem_of(video_src);
    let sub_stem = stem_of(sub);
    let suffix = if sub_stem == src_stem {
        ""
    } else {
        sub_stem.strip_prefix(&src_stem).unwrap_or("")
    };
    let mut name = format!("{}{suffix}", stem_of(video_dst));
    if let Some(ext) = sub.extension() {
        name.push('.');
        name.push_str(&ext.to_string_lossy());
    }
    dst_dir.join(name)
}

/// Directories a moved file leaves behind, deepest first,
/// stopping below `root` (the root the user added is never a
/// candidate). Only truly empty ones can actually be removed —
/// `remove_dir` enforces that at cleanup time.
fn emptied_dir_candidates(file: &Path, root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = file.parent();
    while let Some(dir) = cur {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        out.push(dir.to_path_buf());
        cur = dir.parent();
    }
    out
}

/// Did a copy actually land? Both lengths known and equal.
/// (Pure, so the rule is unit-testable without a NAS.)
fn copy_landed(src_len: Option<u64>, dst_len: Option<u64>) -> bool {
    matches!((src_len, dst_len), (Some(a), Some(b)) if a == b)
}

#[cfg(target_os = "macos")]
fn file_len(p: &Path) -> Option<u64> {
    std::fs::metadata(p).map(|m| m.len()).ok()
}

/// Byte-only streamed copy: no metadata, no attributes —
/// nothing for a network filesystem to refuse at the end.
/// The last-resort fallback when the system copy path fails.
fn streamed_copy(src: &Path, dst: &Path) -> Result<(), String> {
    use std::io::{Read, Write};
    let mut from = std::fs::File::open(src).map_err(|e| e.to_string())?;
    let mut to = std::fs::File::create(dst).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    loop {
        let n = from.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n]).map_err(|e| e.to_string())?;
    }
    to.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// Copy one file across volumes. On macOS this first goes
/// through the system `cp`: its copyfile engine can use a NAS
/// server's copy offload, so the bytes may never cross this
/// machine either. But `cp -p` exits 1 on network volumes when
/// its final step — restoring timestamps/attributes the share
/// won't accept — fails, even though every byte already landed
/// (user's Mandalorian run, 2026-11-14: all 24 rows “cp exit
/// status: 1” after minutes of copying each). So a failed cp
/// whose target matches the source length counts as copied;
/// anything else falls back to the streamed copy, and the
/// error keeps cp's own words for the next diagnosis.
#[cfg(target_os = "macos")]
fn copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    match std::process::Command::new("/bin/cp")
        .arg("-p")
        .arg(src)
        .arg(dst)
        .output()
    {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            if copy_landed(file_len(src), file_len(dst)) {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&o.stderr);
            let stderr = stderr.trim();
            let _ = std::fs::remove_file(dst);
            streamed_copy(src, dst).map_err(|e| {
                if stderr.is_empty() {
                    e
                } else {
                    format!("{stderr} / {e}")
                }
            })
        }
        Err(e) => streamed_copy(src, dst).map_err(|f| format!("cp: {e} / {f}")),
    }
}

#[cfg(not(target_os = "macos"))]
fn copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    streamed_copy(src, dst)
}

/// Move a file, preferring a pure rename: on one volume — and a
/// network share counts as one volume when source and target
/// live on the same share — that is a server-side rename and no
/// bytes travel at all. Only when the rename fails (another
/// volume, EXDEV, ...) does the copy ladder run; the copy is
/// size-checked against the source before the source is
/// removed, and a failed copy removes the partial target and
/// leaves the source untouched.
fn move_file(src: &Path, dst: &Path, lang: Lang) -> Result<(), String> {
    if src == dst {
        return Ok(());
    }
    // Right before moving, look again: a rename would silently
    // replace an existing target on Unix, and one may have
    // appeared since the preview's conflict check ran.
    if dst.exists() {
        return Err(t(lang, "目标已存在", "Target already exists").to_string());
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    let colon = t(lang, "：", ": ");
    if let Err(e) = copy_file(src, dst) {
        let _ = std::fs::remove_file(dst);
        return Err(format!("{}{colon}{e}", t(lang, "拷贝失败", "Copy failed")));
    }
    let src_len = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    let dst_len = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(u64::MAX);
    if src_len != dst_len {
        let _ = std::fs::remove_file(dst);
        return Err(
            t(lang, "拷贝不完整（大小不一致）", "Copy incomplete (size mismatch)")
                .to_string(),
        );
    }
    if let Err(e) = std::fs::remove_file(src) {
        return Err(format!(
            "{}{colon}{e}",
            t(lang, "删源文件失败", "Could not remove the source")
        ));
    }
    Ok(())
}

/// Open a URL in the user's default browser (Help menu's
/// homepage link). Only the macOS native menu calls this today.
#[cfg(target_os = "macos")]
fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Human-readable byte count for the progress modal
/// (“7.9 GB / 15.2 GB”).
fn fmt_bytes(n: u64) -> String {
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.0} MB", n as f64 / MB as f64)
    } else {
        format!("{n} B")
    }
}

/// One row's execution plan, captured before the run starts.
struct ExecJob {
    src: PathBuf,
    video_dst: PathBuf,
    subs: Vec<(PathBuf, PathBuf)>,
    /// Directories to try removing once every move is done
    /// (in-place mode with the setting on; empty otherwise).
    cleanup_dirs: Vec<PathBuf>,
}

enum ExecOutcome {
    /// Video and subtitles all landed.
    Done,
    /// The video landed but a subtitle did not; the row still
    /// advances to its new path, with the message attached.
    Partial(String),
    /// The video did not move; nothing of this row was touched.
    Failed(String),
}

struct ExecResult {
    src: PathBuf,
    video_dst: PathBuf,
    subs_dst: Vec<PathBuf>,
    outcome: ExecOutcome,
}

enum ExecEvent {
    /// A row's move is starting — carries its new file name so
    /// the progress modal can say what is being saved right now
    /// (cross-volume moves can take a while per file), plus the
    /// video's byte size for the per-file progress bar.
    RowStarted { name: String, bytes: u64 },
    /// Bytes landed so far for the file currently being copied
    /// (a watcher thread stats the growing target; rename-fast
    /// rows never produce these).
    FileProgress { done: u64 },
    RowFinished(ExecResult),
    Finished { done: usize, failed: usize },
}

/// Move one row's video and its subtitles to their targets.
fn execute_one(job: &ExecJob, lang: Lang) -> ExecResult {
    let result = |outcome: ExecOutcome| ExecResult {
        src: job.src.clone(),
        video_dst: job.video_dst.clone(),
        subs_dst: job.subs.iter().map(|(_, d)| d.clone()).collect(),
        outcome,
    };
    if let Some(parent) = job.video_dst.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return result(ExecOutcome::Failed(format!(
                "{}{}{e}",
                t(lang, "建目录失败", "Cannot create folder"),
                t(lang, "：", ": ")
            )));
        }
    }
    if let Err(e) = move_file(&job.src, &job.video_dst, lang) {
        return result(ExecOutcome::Failed(e));
    }
    for (s, d) in &job.subs {
        if let Err(e) = move_file(s, d, lang) {
            return result(ExecOutcome::Partial(e));
        }
    }
    result(ExecOutcome::Done)
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct PickerUi {
    work: usize,
    /// Buffer for the manual platform-ID row at the picker's foot.
    id_input: String,
    /// Platform the picker's ID row will query (the small button
    /// next to the input cycles it).
    id_platform: IdPlatform,
}

struct VideoTagApp {
    lang: Lang,
    show_about: bool,
    show_settings: bool,
    wants_quit: bool,
    #[cfg(target_os = "macos")]
    native_menu: Option<NativeMenu>,
    settings: Settings,
    roots: Vec<PathBuf>,
    works: Vec<Work>,
    rows: Vec<Row>,
    selected: Option<usize>,
    status: String,
    conflict_count: usize,
    picker: Option<PickerUi>,
    /// Manual platform-ID dialog (toolbar): visibility + input buffer.
    show_id_dialog: bool,
    id_input: String,
    /// Platform the ID dialog will query; re-synced from
    /// `last_platform` every time the dialog opens.
    id_platform: IdPlatform,
    /// The platform of the most recent match action (a toolbar
    /// platform button or an applied manual ID). The ID dialogs
    /// default to it. Session state only, not persisted.
    last_platform: IdPlatform,
    /// Save-destination dialog (opened by the 保存修改 toolbar
    /// button): pick an export folder, reuse the last one, or
    /// rename in place — then execution starts (user's flow,
    /// 2026-11-14; the destination no longer lives in Settings,
    /// `settings.output_dir` just remembers the last export dir).
    show_save_dialog: bool,
    batch_rx: Option<Receiver<MatchEvent>>,
    batch_running: bool,
    batch_cancel: Arc<AtomicBool>,
    batch_progress: (usize, usize),
    /// Works skipped by the current/last batch because their
    /// titles cannot be searched (reported separately, never as
    /// failures).
    batch_unrecognized: usize,
    enrich_rx: Option<Receiver<EnrichDone>>,
    probe_rx: Option<Receiver<ProbeEvent>>,
    /// Files waiting for a media-probe slot (one batch runs at a
    /// time; see `maybe_start_probe`).
    probe_pending: Vec<PathBuf>,
    exec_rx: Option<Receiver<ExecEvent>>,
    exec_running: bool,
    exec_progress: (usize, usize),
    /// New file name of the row currently being moved (for the
    /// progress modal), plus running success/failure counts.
    exec_current: String,
    exec_ok: usize,
    exec_failed: usize,
    /// Byte progress of the file currently being copied
    /// (landed bytes / total bytes), for the modal's second bar.
    exec_file_done: u64,
    exec_file_total: u64,
    /// Extra top margin for text fields, calibrated once from the
    /// loaded fonts (same fix as MusicTag: some fonts place their
    /// ink above the line-box center).
    field_margin_top: i8,
    skew_measured: bool,
}

impl Default for VideoTagApp {
    fn default() -> Self {
        let lang = detect_lang();
        Self {
            lang,
            show_about: false,
            show_settings: false,
            wants_quit: false,
            #[cfg(target_os = "macos")]
            native_menu: None,
            settings: load_settings(),
            roots: Vec::new(),
            works: Vec::new(),
            rows: Vec::new(),
            selected: None,
            status: t(lang, "添加装有电影或电视剧的目录开始。", "Add a folder of movies or shows to start.")
                .to_string(),
            conflict_count: 0,
            picker: None,
            show_id_dialog: false,
            id_input: String::new(),
            id_platform: IdPlatform::Tmdb,
            last_platform: IdPlatform::Tmdb,
            show_save_dialog: false,
            batch_rx: None,
            batch_running: false,
            batch_cancel: Arc::new(AtomicBool::new(false)),
            batch_progress: (0, 0),
            batch_unrecognized: 0,
            enrich_rx: None,
            probe_rx: None,
            probe_pending: Vec::new(),
            exec_rx: None,
            exec_running: false,
            exec_progress: (0, 0),
            exec_current: String::new(),
            exec_ok: 0,
            exec_failed: 0,
            exec_file_done: 0,
            exec_file_total: 0,
            field_margin_top: 2,
            skew_measured: false,
        }
    }
}

impl VideoTagApp {
    fn add_root(&mut self, root: PathBuf) {
        if self.roots.contains(&root) {
            self.status = t(self.lang, "这个目录已经添加过了。", "That folder is already added.").to_string();
            return;
        }
        let (new_works, mut new_rows) = scan_root(&root);
        if new_rows.is_empty() {
            self.status = t(
                self.lang,
                "这个目录里没找到视频文件。",
                "No video files found in that folder.",
            )
            .to_string();
            return;
        }
        let count = new_rows.len();
        let base = self.works.len();
        for row in new_rows.iter_mut() {
            row.work += base;
        }
        // Probe the new files' streams in the background so their
        // previews can carry real format tags even when the file
        // name says nothing (`01.mkv`). EVERY row is probed —
        // probing only reads headers, and small files are exactly
        // the ones whose names carry no tech tokens; skipping
        // them left whole packs of small episodes with no audio/
        // video info at all (user's bug, 2026-11-14).
        let probe_paths: Vec<PathBuf> =
            new_rows.iter().map(|r| r.path.clone()).collect();
        self.roots.push(root);
        self.works.extend(new_works);
        self.rows.extend(new_rows);
        self.status = format!(
            "{} {count} {}",
            t(self.lang, "已添加", "Added"),
            t(self.lang, "个文件。", "files.")
        );
        self.refresh_previews();
        self.enqueue_probe(probe_paths);
    }

    /// Queue files for media probing and kick off a batch when
    /// none is running.
    fn enqueue_probe(&mut self, paths: Vec<PathBuf>) {
        self.probe_pending.extend(paths);
        self.maybe_start_probe();
    }

    /// Start one probe batch over the pending files: at most
    /// three `ffmpeg -i` runs at a time, results arriving over a
    /// channel like the matcher's.
    fn maybe_start_probe(&mut self) {
        if self.probe_rx.is_some() || self.probe_pending.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.probe_pending);
        let (tx, rx) = channel();
        self.probe_rx = Some(rx);
        let ffmpeg = ffmpeg_path();
        std::thread::spawn(move || {
            let queue = Arc::new(Mutex::new(VecDeque::from(paths)));
            let mut workers = Vec::new();
            for _ in 0..3 {
                let queue = queue.clone();
                let tx = tx.clone();
                let ffmpeg = ffmpeg.clone();
                workers.push(std::thread::spawn(move || loop {
                    let next = queue.lock().ok().and_then(|mut q| q.pop_front());
                    let Some(path) = next else { break };
                    let tags = probe_one(&ffmpeg, &path);
                    if tx.send(ProbeEvent::Done { path, tags }).is_err() {
                        break;
                    }
                }));
            }
            for w in workers {
                let _ = w.join();
            }
        });
    }

    fn poll_probe(&mut self, ctx: &egui::Context) {
        let mut events: Vec<ProbeEvent> = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.probe_rx {
            loop {
                match rx.try_recv() {
                    Ok(ev) => events.push(ev),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            self.probe_rx = None;
        }
        let mut dirty = false;
        for ev in events {
            let ProbeEvent::Done { path, tags } = ev;
            if tags.is_empty() {
                continue; // probe failed / no ffmpeg: keep the fallback
            }
            if let Some(row) = self.rows.iter_mut().find(|r| r.path == path) {
                if row.media_tags.as_deref() != Some(tags.as_slice()) {
                    row.media_tags = Some(tags);
                    dirty = true;
                }
            }
        }
        if dirty {
            self.refresh_previews();
        }
        if self.probe_rx.is_none() {
            self.maybe_start_probe();
        }
        if self.probe_rx.is_some() {
            ctx.request_repaint();
        }
    }

    /// Adding a directory starts from a clean list (user's call,
    /// 2026-11-14: one batch at a time — old rows must not linger
    /// into the next folder's work). View-only: no file is touched.
    /// In-flight probe results for vanished paths are ignored by
    /// `poll_probe`, so clearing mid-probe is safe.
    fn clear_list(&mut self) {
        self.rows.clear();
        self.works.clear();
        self.roots.clear();
        self.selected = None;
        self.picker = None;
        self.probe_pending.clear();
        self.conflict_count = 0;
        self.batch_unrecognized = 0;
    }

    fn pick_root_dialog(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                self.clear_list();
                self.add_root(dir);
            }
        }
    }

    fn any_checked(&self) -> bool {
        self.rows.iter().any(|r| r.checked)
    }

    /// A background batch (matching or executing) is running:
    /// actions that would move, remove or re-match rows wait.
    fn busy(&self) -> bool {
        self.batch_running || self.exec_running
    }

    /// Execute the checked, previewed, conflict-free rows: move
    /// each video (and its subtitles) to its absolute target —
    /// under the output folder when one is set, else in place.
    /// Jobs are captured up front and run on a background thread;
    /// see `move_file` for how a move stays server-side whenever
    /// the files allow it.
    fn start_execute(&mut self) {
        if self.busy() {
            return;
        }
        let cleanup = self.settings.output_dir.is_none() && self.settings.delete_empty_dirs;
        let mut jobs: Vec<ExecJob> = Vec::new();
        for row in &self.rows {
            if !(row.checked && row.preview.is_some() && !row.conflict) {
                continue;
            }
            let Some(video_dst) = row_target(row, &self.settings) else {
                continue;
            };
            let subs = row
                .subtitles
                .iter()
                .map(|s| (s.clone(), subtitle_target(s, &row.path, &video_dst)))
                .collect();
            let cleanup_dirs = if cleanup {
                emptied_dir_candidates(&row.path, &row.root)
            } else {
                Vec::new()
            };
            jobs.push(ExecJob {
                src: row.path.clone(),
                video_dst,
                subs,
                cleanup_dirs,
            });
        }
        if jobs.is_empty() {
            self.status = t(
                self.lang,
                "没有可执行的文件：先匹配出无冲突的预览并勾选。",
                "Nothing to execute: rows need a conflict-free preview and a check mark.",
            )
            .to_string();
            return;
        }
        let total = jobs.len();
        let (tx, rx) = channel();
        self.exec_rx = Some(rx);
        self.exec_running = true;
        self.exec_progress = (0, total);
        self.exec_current = String::new();
        self.exec_ok = 0;
        self.exec_failed = 0;
        self.exec_file_done = 0;
        self.exec_file_total = 0;
        let lang = self.lang;
        // Byte-progress watcher: while a row's copy runs, this
        // thread stats the growing target file a few times a
        // second and reports landed bytes. Counting rows alone
        // made a multi-GB file look frozen for minutes (user:
        // “还是不动”, 2026-11-14) — the bytes are the truth.
        let watch: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let watch_active = Arc::new(AtomicBool::new(true));
        let watch2 = Arc::clone(&watch);
        let watch_active2 = Arc::clone(&watch_active);
        let tx_watch = tx.clone();
        std::thread::spawn(move || {
            let mut last = u64::MAX;
            while watch_active2.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let dst = watch2.lock().unwrap().clone();
                if let Some(dst) = dst {
                    let done = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
                    if done != last {
                        last = done;
                        if tx_watch.send(ExecEvent::FileProgress { done }).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        std::thread::spawn(move || {
            let (mut done, mut failed) = (0usize, 0usize);
            let mut cleanup_all: Vec<PathBuf> = Vec::new();
            for job in &jobs {
                let name = job
                    .video_dst
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let bytes = std::fs::metadata(&job.src).map(|m| m.len()).unwrap_or(0);
                *watch.lock().unwrap() = Some(job.video_dst.clone());
                if tx.send(ExecEvent::RowStarted { name, bytes }).is_err() {
                    watch_active.store(false, Ordering::Relaxed);
                    return;
                }
                let result = execute_one(job, lang);
                *watch.lock().unwrap() = None;
                match result.outcome {
                    ExecOutcome::Done => done += 1,
                    _ => failed += 1,
                }
                cleanup_all.extend(job.cleanup_dirs.iter().cloned());
                if tx.send(ExecEvent::RowFinished(result)).is_err() {
                    watch_active.store(false, Ordering::Relaxed);
                    return;
                }
            }
            watch_active.store(false, Ordering::Relaxed);
            // Directories the moves left empty (in-place mode
            // only — output-mode jobs carry none): deepest first.
            // `remove_dir` only succeeds on truly empty folders,
            // so anything still holding files is left alone.
            cleanup_all.sort();
            cleanup_all.dedup();
            cleanup_all
                .sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            for dir in cleanup_all {
                let _ = std::fs::remove_dir(dir);
            }
            let _ = tx.send(ExecEvent::Finished { done, failed });
        });
        self.status = t(self.lang, "开始执行…", "Executing…").to_string();
    }

    fn poll_execute(&mut self, ctx: &egui::Context) {
        let mut events: Vec<ExecEvent> = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.exec_rx {
            loop {
                match rx.try_recv() {
                    Ok(ev) => events.push(ev),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            self.exec_rx = None;
            self.exec_running = false;
            self.exec_current = String::new();
        }
        let output_dir = self.settings.output_dir.clone();
        for ev in events {
            match ev {
                ExecEvent::RowStarted { name, bytes } => {
                    self.exec_current = name;
                    self.exec_file_total = bytes;
                    self.exec_file_done = 0;
                }
                ExecEvent::FileProgress { done } => {
                    // Monotonic: a stat racing a rename can
                    // briefly read a stale smaller size.
                    if done > self.exec_file_done {
                        self.exec_file_done = if self.exec_file_total > 0 {
                            done.min(self.exec_file_total)
                        } else {
                            done
                        };
                    }
                }
                ExecEvent::RowFinished(result) => {
                    self.exec_progress.0 += 1;
                    self.exec_file_done = self.exec_file_total;
                    match &result.outcome {
                        ExecOutcome::Done => self.exec_ok += 1,
                        _ => self.exec_failed += 1,
                    }
                    if let Some(row) =
                        self.rows.iter_mut().find(|r| r.path == result.src)
                    {
                        match &result.outcome {
                            ExecOutcome::Failed(msg) => {
                                row.exec_error = Some(msg.clone());
                            }
                            outcome => {
                                row.path = result.video_dst.clone();
                                row.name = result
                                    .video_dst
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                row.subtitles = result.subs_dst.clone();
                                row.preview = None;
                                row.conflict = false;
                                row.conflict_kind = None;
                                row.done = true;
                                row.checked = false;
                                if let Some(base) = &output_dir {
                                    row.root = base.clone();
                                }
                                row.exec_error = match outcome {
                                    ExecOutcome::Partial(msg) => Some(msg.clone()),
                                    _ => None,
                                };
                            }
                        }
                    }
                    self.status = format!(
                        "{} {}/{}",
                        t(self.lang, "执行中…", "Executing…"),
                        self.exec_progress.0,
                        self.exec_progress.1
                    );
                }
                ExecEvent::Finished { done, failed } => {
                    self.exec_running = false;
                    self.exec_rx = None;
                    self.exec_current = String::new();
                    self.status = format!(
                        "{} {}，{} {}",
                        t(self.lang, "执行完成：成功", "Done: succeeded"),
                        done,
                        t(self.lang, "失败", "failed"),
                        failed
                    );
                    // A successful save empties the list (user's
                    // call, 2026-11-14): rows that fully landed
                    // leave. Failed rows stay untouched, and so
                    // do partial rows (video landed, a subtitle
                    // did not) — their error still needs eyes.
                    let before = self.rows.len();
                    self.rows
                        .retain(|r| !(r.done && r.exec_error.is_none()));
                    if self.rows.len() != before {
                        self.prune_after_row_removal();
                    }
                    self.refresh_previews();
                }
            }
        }
        if self.exec_running {
            ctx.request_repaint();
        }
    }

    /// Modal progress window shown while a save executes
    /// (user's request, 2026-11-14): cross-volume saves can be
    /// slow, and the status-bar counter alone left the app
    /// looking hung. The Modal's backdrop blocks input to the
    /// rest of the UI until the run finishes; the window shows
    /// the progress bar, the counts so far, and the file being
    /// moved right now. It closes itself when execution ends.
    fn exec_progress_modal(&self, ctx: &egui::Context) {
        let lang = self.lang;
        let (done, total) = self.exec_progress;
        let frac = if total > 0 {
            done as f32 / total as f32
        } else {
            0.0
        };
        egui::Modal::new(egui::Id::new("exec_progress")).show(ctx, |ui| {
            ui.set_width(520.0);
            // Generous inner margin: with the bare default frame
            // the content sat right against the modal's edges
            // and felt cramped (user screenshot, 2026-11-14) —
            // same breathing-room fix as the settings window.
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(20, 18))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t(lang, "正在保存…", "Saving…"));
                    });
                    ui.add_space(14.0);
                    progress_bar_ink(ui, frac, &format!("{done} / {total}"));
                    ui.add_space(12.0);
                    if self.exec_current.is_empty() {
                        ui.label(" ");
                    } else {
                        ui.add(
                            egui::Label::new(self.exec_current.as_str()).truncate(),
                        );
                    }
                    if self.exec_file_total > 0 {
                        ui.add_space(10.0);
                        let file_frac =
                            self.exec_file_done as f32 / self.exec_file_total as f32;
                        progress_bar_ink(
                            ui,
                            file_frac,
                            &format!(
                                "{} / {}",
                                fmt_bytes(self.exec_file_done),
                                fmt_bytes(self.exec_file_total)
                            ),
                        );
                    }
                    ui.add_space(10.0);
                    ui.label(format!(
                        "{} {} · {} {}",
                        t(lang, "成功", "Succeeded"),
                        self.exec_ok,
                        t(lang, "失败", "failed"),
                        self.exec_failed
                    ));
                });
        });
    }

    /// Shared tidy-up after rows leave the list (manual removal,
    /// the post-save clear): drop the picker and selection, remap
    /// work indices to the surviving works, and forget roots
    /// whose rows are all gone — a root that stays registered
    /// with an empty list makes re-adding that directory bounce
    /// off the duplicate check (user's bug report, 2026-11-14).
    fn prune_after_row_removal(&mut self) {
        self.picker = None;
        self.selected = None;
        // Remap work indices to the surviving works, in row order.
        let mut remap: HashMap<usize, usize> = HashMap::new();
        for row in self.rows.iter() {
            let next = remap.len();
            remap.entry(row.work).or_insert(next);
        }
        let mut order: Vec<(usize, usize)> =
            remap.iter().map(|(old, new)| (*new, *old)).collect();
        order.sort();
        let mut old: Vec<Option<Work>> = self.works.drain(..).map(Some).collect();
        self.works = order
            .into_iter()
            .map(|(_, old_idx)| old[old_idx].take().expect("work present"))
            .collect();
        for row in self.rows.iter_mut() {
            row.work = remap[&row.work];
        }
        self.roots = roots_in_use(&self.roots, &self.rows);
    }

    /// Remove all checked rows; works left without rows go away too.
    fn remove_checked(&mut self) {
        let before = self.rows.len();
        self.rows.retain(|r| !r.checked);
        let removed = before - self.rows.len();
        if removed == 0 {
            return;
        }
        self.prune_after_row_removal();
        self.status = format!("{} {removed}", t(self.lang, "已移除", "Removed"));
        self.refresh_previews();
    }

    fn seasons_of(&self, work: usize) -> Vec<u32> {
        let mut seasons: Vec<u32> = self
            .rows
            .iter()
            .filter(|r| r.work == work)
            .filter_map(|r| r.parsed.season)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        seasons.sort_unstable();
        seasons
    }

    /// Batch-match every checked work on ONE platform — the one
    /// whose toolbar button was clicked. A work carrying that
    /// platform's id is looked up directly; anything else is
    /// title-searched on that platform only. There is no
    /// cross-platform fallback (user's call, 2026-11-14).
    fn start_match(&mut self, platform: IdPlatform) {
        if self.busy() {
            return;
        }
        self.last_platform = platform;
        self.batch_unrecognized = 0;
        // TMDB without a key keeps the old guidance behavior;
        // the TVDB button is greyed out without a key, so this
        // is only a belt-and-braces guard for it.
        if platform == IdPlatform::Tmdb && self.settings.api_key.trim().is_empty() {
            self.show_settings = true;
            self.status = t(
                self.lang,
                "先在设置里填 TMDB API Key 才能在线匹配。",
                "Add your TMDB API key in Settings to match online.",
            )
            .to_string();
            return;
        }
        if platform == IdPlatform::Tvdb && self.settings.tvdb_api_key.trim().is_empty() {
            return;
        }
        let mut jobs: Vec<WorkJob> = Vec::new();
        let mut unrecognized = 0usize;
        for (idx, work) in self.works.iter().enumerate() {
            if !self.rows.iter().any(|r| r.work == idx && r.checked) {
                continue;
            }
            let pinned_id = work.parsed.id_for(platform);
            // Never search a meaningless title: the catalogue
            // would happily return some unrelated movie for "01"
            // and the batch would report it as a success. A work
            // pinned with THIS platform's id needs no title.
            if pinned_id.is_none()
                && videotag_core::is_unsearchable_title(&work.parsed.title)
            {
                unrecognized += 1;
                continue;
            }
            jobs.push(WorkJob {
                work: idx,
                title: work.parsed.title.clone(),
                year: work.parsed.year.map(|y| y as i32),
                pinned_id,
                is_tv: work.is_tv,
                seasons: self.seasons_of(idx),
            });
        }
        self.batch_unrecognized = unrecognized;
        if jobs.is_empty() {
            self.status = if unrecognized > 0 {
                match self.lang {
                    Lang::Zh => {
                        format!("无法识别 {unrecognized} 个作品（标题无意义），已跳过。")
                    }
                    Lang::En => {
                        format!("{unrecognized} unrecognized work(s): meaningless titles, skipped.")
                    }
                }
            } else {
                t(self.lang, "没有勾选任何文件。", "Nothing is checked.").to_string()
            };
            return;
        }
        self.run_match_jobs(jobs, platform);
    }

    /// Send a prepared set of jobs to the background matcher
    /// (shared by the batch matcher and manual-ID matching).
    fn run_match_jobs(&mut self, jobs: Vec<WorkJob>, platform: IdPlatform) {
        let total = jobs.len();
        for job in &jobs {
            if let Some(w) = self.works.get_mut(job.work) {
                w.searching = true;
                w.failed = false;
            }
        }
        let (tx, rx) = channel();
        self.batch_rx = Some(rx);
        self.batch_running = true;
        self.batch_progress = (0, total);
        self.batch_cancel.store(false, Ordering::Relaxed);
        let cancel = self.batch_cancel.clone();
        let cfg = SourceConfig::from_settings(&self.settings, self.lang);
        std::thread::spawn(move || {
            let (mut matched, mut failed) = (0, 0);
            for job in &jobs {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let _ = tx.send(MatchEvent::WorkSearching { work: job.work });
                match match_one(&cfg, platform, job) {
                    Some((candidates, chosen, detail, episodes)) => {
                        matched += 1;
                        let _ = tx.send(MatchEvent::WorkDone {
                            work: job.work,
                            candidates,
                            chosen,
                            detail,
                            episodes,
                        });
                    }
                    None => {
                        failed += 1;
                        let _ = tx.send(MatchEvent::WorkFailed { work: job.work });
                    }
                }
            }
            let _ = tx.send(MatchEvent::Finished { matched, failed });
        });
        self.status = t(self.lang, "开始在线匹配…", "Matching online…").to_string();
    }

    /// Manual platform-ID match: pin `id` for `platform` on each
    /// target work and run it through that platform's ID-lookup
    /// path. The ID is written into the work's parse (the other
    /// platforms' ids are cleared — the work now belongs to this
    /// catalogue), so later matches on the same platform look it
    /// up directly too; an unsearchable title stops being
    /// unrecognized. Any previous match is replaced wholesale
    /// (like a picker re-pick): stale candidates, detail and
    /// episode lists go.
    fn start_id_match(&mut self, id: u64, platform: IdPlatform, target_works: Vec<usize>) {
        if self.busy() {
            return;
        }
        self.last_platform = platform;
        let cfg = SourceConfig::from_settings(&self.settings, self.lang);
        if !cfg.platform_ready(platform) {
            self.show_settings = true;
            self.status = match platform {
                IdPlatform::Tmdb => t(
                    self.lang,
                    "先在设置里填 TMDB API Key 才能在线匹配。",
                    "Add your TMDB API key in Settings to match online.",
                ),
                IdPlatform::Tvdb => t(
                    self.lang,
                    "先在设置里填 TheTVDB API Key 才能用 TVDB 匹配。",
                    "Add your TheTVDB API key in Settings to match with TVDB.",
                ),
                IdPlatform::Bangumi => unreachable!("Bangumi needs no key"),
            }
            .to_string();
            return;
        }
        let mut targets: Vec<usize> = Vec::new();
        for idx in target_works {
            if idx < self.works.len() && !targets.contains(&idx) {
                targets.push(idx);
            }
        }
        if targets.is_empty() {
            return;
        }
        let mut jobs: Vec<WorkJob> = Vec::new();
        for idx in targets {
            let seasons = self.seasons_of(idx);
            let Some(work) = self.works.get_mut(idx) else { continue };
            work.parsed.tmdb_id = None;
            work.parsed.tvdb_id = None;
            work.parsed.bgm_id = None;
            match platform {
                IdPlatform::Tmdb => work.parsed.tmdb_id = Some(id),
                IdPlatform::Tvdb => work.parsed.tvdb_id = Some(id),
                IdPlatform::Bangumi => work.parsed.bgm_id = Some(id),
            }
            work.method = Method::Id;
            work.unrecognized = false;
            work.candidates.clear();
            work.chosen = None;
            work.detail = None;
            work.episodes.clear();
            work.episodes_for = None;
            work.failed = false;
            jobs.push(WorkJob {
                work: idx,
                title: work.parsed.title.clone(),
                year: work.parsed.year.map(|y| y as i32),
                pinned_id: Some(id),
                is_tv: work.is_tv,
                seasons,
            });
        }
        self.batch_unrecognized = 0;
        self.refresh_previews();
        self.run_match_jobs(jobs, platform);
        self.status = match self.lang {
            Lang::Zh => format!("按 {} ID {id} 匹配中…", platform_label(platform)),
            Lang::En => format!("Matching {} ID {id}…", platform_label(platform)),
        };
    }

    fn poll_match(&mut self, ctx: &egui::Context) {
        let mut events: Vec<MatchEvent> = Vec::new();
        if let Some(rx) = &self.batch_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        let mut dirty = false;
        for ev in events {
            match ev {
                MatchEvent::WorkSearching { work } => {
                    if let Some(w) = self.works.get_mut(work) {
                        w.searching = true;
                    }
                }
                MatchEvent::WorkDone {
                    work,
                    candidates,
                    chosen,
                    detail,
                    episodes,
                } => {
                    self.batch_progress.0 += 1;
                    if let Some(w) = self.works.get_mut(work) {
                        let prev_source = w.matched().map(|c| c.source);
                        w.searching = false;
                        w.failed = false;
                        w.candidates = candidates;
                        w.chosen = Some(chosen);
                        w.detail = detail;
                        // A re-match on a different platform must
                        // not inherit the old platform's episode
                        // lists (ids collide across catalogues).
                        if prev_source.is_some()
                            && prev_source != w.matched().map(|c| c.source)
                        {
                            w.episodes.clear();
                        }
                        let canon = w.matched().map(|c| c.id);
                        w.episodes_for = canon;
                        for (season, eps) in episodes {
                            w.episodes.insert(season, eps);
                        }
                    }
                    dirty = true;
                    self.status = format!(
                        "{} {}/{}",
                        t(self.lang, "在线匹配中…", "Matching…"),
                        self.batch_progress.0,
                        self.batch_progress.1
                    );
                }
                MatchEvent::WorkFailed { work } => {
                    self.batch_progress.0 += 1;
                    if let Some(w) = self.works.get_mut(work) {
                        w.searching = false;
                        w.failed = true;
                    }
                    dirty = true;
                }
                MatchEvent::Finished { matched, failed } => {
                    self.batch_running = false;
                    self.batch_rx = None;
                    for w in self.works.iter_mut() {
                        w.searching = false;
                    }
                    let mut status = format!(
                        "{} {}，{} {}",
                        t(self.lang, "匹配完成：成功", "Done: matched"),
                        matched,
                        t(self.lang, "失败", "failed"),
                        failed
                    );
                    if self.batch_unrecognized > 0 {
                        status.push_str(&match self.lang {
                            Lang::Zh => format!("，无法识别 {}", self.batch_unrecognized),
                            Lang::En => {
                                format!(", {} unrecognized", self.batch_unrecognized)
                            }
                        });
                    }
                    self.status = status;
                }
            }
        }
        if dirty {
            self.refresh_previews();
        }
        if self.batch_running {
            ctx.request_repaint();
        }
    }

    /// After a picker change: fetch the newly chosen candidate's
    /// detail record and episode lists in the background.
    fn start_enrich(&mut self, work_idx: usize) {
        let Some(work) = self.works.get(work_idx) else { return };
        let Some(cand) = work.chosen.and_then(|i| work.candidates.get(i)).cloned() else {
            return;
        };
        let detail_fresh = work.detail.as_ref().map(|d| d.id) == Some(cand.id);
        let episodes_fresh = work.episodes_for == Some(cand.id);
        if detail_fresh && (cand.media_type != MediaType::Tv || episodes_fresh) {
            return;
        }
        let seasons = self.seasons_of(work_idx);
        let cfg = SourceConfig::from_settings(&self.settings, self.lang);
        if !cfg.platform_ready(cand.source) {
            return;
        }
        let (tx, rx) = channel();
        self.enrich_rx = Some(rx);
        std::thread::spawn(move || {
            let detail = fetch_detail(&cfg, &cand);
            throttle();
            let canonical = detail.clone().unwrap_or_else(|| cand.clone());
            let episodes = fetch_episodes_for(&cfg, &canonical, &seasons);
            let _ = tx.send(EnrichDone {
                work: work_idx,
                cand_id: cand.id,
                detail,
                episodes,
            });
        });
    }

    fn poll_enrich(&mut self, ctx: &egui::Context) {
        let mut done: Vec<EnrichDone> = Vec::new();
        if let Some(rx) = &self.enrich_rx {
            while let Ok(ev) = rx.try_recv() {
                done.push(ev);
            }
        }
        if done.is_empty() {
            if self.enrich_rx.is_some() {
                ctx.request_repaint();
            }
            return;
        }
        self.enrich_rx = None;
        let mut dirty = false;
        for ev in done {
            let still_chosen = self
                .works
                .get(ev.work)
                .and_then(|w| w.chosen)
                .and_then(|i| self.works[ev.work].candidates.get(i))
                .map(|c| c.id)
                == Some(ev.cand_id);
            if !still_chosen {
                continue;
            }
            if let Some(w) = self.works.get_mut(ev.work) {
                if ev.detail.is_some() {
                    w.detail = ev.detail;
                }
                w.episodes_for = Some(ev.cand_id);
                for (season, eps) in ev.episodes {
                    w.episodes.insert(season, eps);
                }
            }
            dirty = true;
        }
        if dirty {
            self.refresh_previews();
        }
    }

    fn choose_candidate(&mut self, work_idx: usize, cand_idx: usize) {
        let Some(w) = self.works.get_mut(work_idx) else { return };
        if w.chosen == Some(cand_idx) {
            return;
        }
        w.chosen = Some(cand_idx);
        w.failed = false;
        let new_id = w.candidates.get(cand_idx).map(|c| c.id);
        // Drop detail/episodes that belong to the previous pick.
        if w.detail.as_ref().map(|d| d.id) != new_id {
            w.detail = None;
        }
        if w.episodes_for != new_id {
            w.episodes.clear();
            w.episodes_for = None;
        }
        self.start_enrich(work_idx);
        self.refresh_previews();
    }
}

// ---------------------------------------------------------------------------
// Preview building + conflict detection
// ---------------------------------------------------------------------------

/// The TMDB title and air date of one episode, from the episode
/// listing fetched for the work's matched candidate. Either part
/// may be missing (episode not in the listing, title not
/// translated, no air date); the file-name builder substitutes
/// the `第 N 集` placeholder for a missing title.
fn episode_info_of(work: &Work, season: u32, episode: u32) -> (Option<String>, Option<String>) {
    let Some(ep) = work
        .episodes
        .get(&season)
        .and_then(|eps| eps.iter().find(|e| e.number == episode))
    else {
        return (None, None);
    };
    let title = Some(ep.title.clone()).filter(|s| !s.trim().is_empty());
    let date = ep.air_date.clone().filter(|s| !s.trim().is_empty());
    (title, date)
}

/// Whether a string contains any CJK ideograph — the test for
/// "this zh-CN title really is Chinese" (TMDB falls back to the
/// original language when a work has no Chinese translation, and
/// such a title must not head a file name on a Chinese system).
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{3400}'..='\u{9fff}').contains(&c))
}

/// The show title heading an episode file name, in the naming
/// language:
/// - Chinese: the zh-CN title when it actually contains Chinese;
///   when TMDB had no Chinese name and `title` fell back to a
///   foreign one, the chain continues original → English → title.
/// - English: the (en-US) title first, then the original title.
/// The folder name is unaffected — it always pairs the localized
/// title with the English one in 《》.
fn tv_file_show_title(cand: &Candidate, lang: Lang) -> &str {
    match lang {
        Lang::Zh => {
            if contains_cjk(&cand.title) {
                return &cand.title;
            }
            for s in [&cand.original_title, &cand.english_title, &cand.title] {
                if !s.trim().is_empty() {
                    return s;
                }
            }
            &cand.title
        }
        Lang::En => {
            if !cand.title.trim().is_empty() {
                return &cand.title;
            }
            if !cand.original_title.trim().is_empty() {
                return &cand.original_title;
            }
            &cand.title
        }
    }
}

/// The row's target path relative to its root, in the library
/// layout. Only matched works get a preview; executed rows are
/// already at their target and keep none.
fn build_preview(row: &Row, work: &Work, settings: &Settings, lang: Lang) -> Option<String> {
    if row.done {
        return None;
    }
    let cand = work.matched()?;
    let wt = WorkTitle {
        title: cand.title.clone(),
        english_title: cand.english_title.clone(),
        year: cand.year.and_then(|y| u16::try_from(y).ok()),
        tmdb_id: Some(cand.id),
        id_platform: cand.source,
        vote_average: cand.vote_average,
        vote_count: cand.vote_count,
    };
    // The tag segment comes from the file itself when probing
    // produced values; until then (or when probing failed) the
    // tags parsed from the original file name are the fallback.
    let tags: &[String] = if settings.keep_tech_tags {
        row.media_tags.as_deref().unwrap_or(&row.parsed.tech_tags)
    } else {
        &[]
    };
    let ext = ext_of(&row.path);
    match (row.parsed.season, row.parsed.episode) {
        (Some(season), Some(episode)) => {
            // The episode title (and its air date) is a fixed
            // part of the file-name format: whatever TMDB's
            // episode listing holds for this episode goes in,
            // the builder substitutes the placeholder when the
            // listing has nothing.
            let (ep_title, ep_air_date) = episode_info_of(work, season, episode);
            // The folder keeps the localized/English pairing,
            // while the file name's show title follows the
            // naming-language chain above — so the path is
            // assembled here instead of via build_tv_library_path.
            let mut file_wt = wt.clone();
            file_wt.title = tv_file_show_title(cand, lang).to_string();
            Some(format!(
                "{}/Season {season:02}/{}",
                videotag_core::build_work_folder_name(&wt),
                videotag_core::build_tv_file_name(
                    &file_wt,
                    season,
                    episode,
                    row.parsed.episode_end,
                    ep_title.as_deref(),
                    ep_air_date.as_deref(),
                    tags,
                    &ext,
                    name_language(lang),
                )
            ))
        }
        (Some(_), None) => None,
        _ => Some(videotag_core::build_movie_library_path(
            &wt,
            row.parsed.part.as_deref(),
            tags,
            &ext,
        )),
    }
}

impl VideoTagApp {
    /// Recompute every row's preview, episode-missing flag and
    /// conflict flag. Called whenever matches, selections, rows or
    /// settings change — not per frame (it stats the filesystem).
    fn refresh_previews(&mut self) {
        let mut previews: Vec<Option<String>> = Vec::with_capacity(self.rows.len());
        let mut targets: Vec<Option<PathBuf>> = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            let work = &self.works[row.work];
            let preview = build_preview(row, work, &self.settings, self.lang);
            let target = preview
                .as_deref()
                .map(|p| target_for_preview(&row.root, p, &self.settings));
            previews.push(preview);
            targets.push(target);
        }
        // Conflicts are judged on the absolute targets: with an
        // output folder set, rows from different roots can aim
        // at the very same file.
        let mut pair_rows: Vec<usize> = Vec::new();
        let mut pair_src: Vec<PathBuf> = Vec::new();
        let mut pair_tgt: Vec<PathBuf> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            if let Some(target) = &targets[i] {
                pair_rows.push(i);
                pair_src.push(row.path.clone());
                pair_tgt.push(target.clone());
            }
        }
        let kinds = conflicting_kinds(&pair_src, &pair_tgt);
        let mut conflict_of: HashMap<usize, Option<ConflictKind>> = HashMap::new();
        for (k, i) in pair_rows.iter().enumerate() {
            conflict_of.insert(*i, kinds[k]);
        }
        let mut conflict_count = 0;
        for (i, row) in self.rows.iter_mut().enumerate() {
            let preview = previews[i].take();
            let kind = conflict_of.get(&i).copied().unwrap_or(None);
            let conflict = kind.is_some();
            if conflict {
                conflict_count += 1;
            }
            row.preview = preview;
            row.conflict = conflict;
            row.conflict_kind = kind;
            // Episode missing from TMDB's listing for this season.
            row.ep_missing = false;
            if row.done {
                continue;
            }
            let work = &self.works[row.work];
            if let (Some(cand), Some(season), Some(episode)) = (
                work.matched(),
                row.parsed.season,
                row.parsed.episode,
            ) {
                if cand.media_type == MediaType::Tv && work.episodes_for == Some(cand.id) {
                    if let Some(eps) = work.episodes.get(&season) {
                        let last = row.parsed.episode_end.unwrap_or(episode);
                        if !eps.iter().any(|e| e.number >= episode && e.number <= last) {
                            row.ep_missing = true;
                        }
                    }
                }
            }
        }
        self.conflict_count = conflict_count;
    }
}

// ---------------------------------------------------------------------------
// Native menu (macOS)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
struct NativeMenu {
    _menu: muda::Menu,
    about: muda::MenuItem,
    settings_m: muda::MenuItem,
    quit_app: muda::MenuItem,
    add_dir: muda::MenuItem,
    save_m: muda::MenuItem,
    select_all_m: muda::MenuItem,
    invert_m: muda::MenuItem,
    remove_m: muda::MenuItem,
    set_id_m: muda::MenuItem,
    match_tmdb_m: muda::MenuItem,
    match_tvdb_m: muda::MenuItem,
    match_bangumi_m: muda::MenuItem,
    homepage: muda::MenuItem,
    github: muda::MenuItem,
}

#[cfg(target_os = "macos")]
fn build_native_menu(lang: Lang) -> Option<NativeMenu> {
    use muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};
    let accel = |code: muda::accelerator::Code| {
        Some(muda::accelerator::Accelerator::new(
            Some(muda::accelerator::Modifiers::META),
            code,
        ))
    };
    let menu = Menu::new();
    let app_menu = Submenu::new("VideoTag", true);
    let about = MenuItem::new(t(lang, "关于 VideoTag", "About VideoTag"), true, None);
    let settings_m = MenuItem::new(t(lang, "系统偏好", "Preferences"), true, None);
    let quit_app = MenuItem::new(
        t(lang, "退出 VideoTag", "Quit VideoTag"),
        true,
        accel(muda::accelerator::Code::KeyQ),
    );
    app_menu.append(&settings_m).ok()?;
    app_menu.append(&PredefinedMenuItem::separator()).ok()?;
    app_menu.append(&quit_app).ok()?;
    menu.append(&app_menu).ok()?;

    let file_menu = Submenu::new(t(lang, "文件", "File"), true);
    let add_dir = MenuItem::new(
        t(lang, "添加目录…", "Add Folder…"),
        true,
        accel(muda::accelerator::Code::KeyO),
    );
    let save_m = MenuItem::new(
        t(lang, "保存修改", "Save Changes"),
        true,
        accel(muda::accelerator::Code::KeyS),
    );
    file_menu.append(&add_dir).ok()?;
    file_menu.append(&PredefinedMenuItem::separator()).ok()?;
    file_menu.append(&save_m).ok()?;
    menu.append(&file_menu).ok()?;

    // The Edit menu mirrors the toolbar one-to-one (menu/toolbar
    // unification, 2026-11-14): selection group, Set ID, then one
    // match item per platform — the toolbar has three distinct
    // platform commands, so the menu names all three instead of
    // hiding the remembered platform behind a generic label.
    let edit_menu = Submenu::new(t(lang, "编辑", "Edit"), true);
    // No ⌘A accelerator on Select All: a native menu key equivalent
    // would win over the text fields in the settings / ID dialogs,
    // where ⌘A must keep meaning "select the text".
    let select_all_m = MenuItem::new(t(lang, "全选", "Select All"), true, None);
    let invert_m = MenuItem::new(t(lang, "反选", "Invert Selection"), true, None);
    let remove_m = MenuItem::new(t(lang, "移除", "Remove"), true, None);
    let set_id_m = MenuItem::new(t(lang, "指定 ID…", "Set ID…"), true, None);
    let match_tmdb_m = MenuItem::new(t(lang, "用 TMDB 匹配", "Match with TMDB"), true, None);
    let match_tvdb_m = MenuItem::new(t(lang, "用 TVDB 匹配", "Match with TVDB"), true, None);
    let match_bangumi_m = MenuItem::new(t(lang, "用 Bangumi 匹配", "Match with Bangumi"), true, None);
    edit_menu.append(&select_all_m).ok()?;
    edit_menu.append(&invert_m).ok()?;
    edit_menu.append(&remove_m).ok()?;
    edit_menu.append(&PredefinedMenuItem::separator()).ok()?;
    edit_menu.append(&set_id_m).ok()?;
    edit_menu.append(&PredefinedMenuItem::separator()).ok()?;
    edit_menu.append(&match_tmdb_m).ok()?;
    edit_menu.append(&match_tvdb_m).ok()?;
    edit_menu.append(&match_bangumi_m).ok()?;
    menu.append(&edit_menu).ok()?;

    // Help menu (user's call, 2026-11-14): About moved here
    // from the app menu, plus a link to his homepage. The
    // GitHub entry is a placeholder — VideoTag has no repo
    // yet; it gets a real URL (and handler) at open-source time.
    let help_menu = Submenu::new(t(lang, "帮助", "Help"), true);
    let homepage = MenuItem::new(t(lang, "作者主页", "Author's Homepage"), true, None);
    let github = MenuItem::new(t(lang, "在GitHub上查看", "View on GitHub"), true, None);
    help_menu.append(&about).ok()?;
    help_menu.append(&PredefinedMenuItem::separator()).ok()?;
    help_menu.append(&homepage).ok()?;
    help_menu.append(&github).ok()?;
    menu.append(&help_menu).ok()?;
    menu.init_for_nsapp();
    Some(NativeMenu {
        _menu: menu,
        about,
        settings_m,
        quit_app,
        add_dir,
        save_m,
        select_all_m,
        invert_m,
        remove_m,
        set_id_m,
        match_tmdb_m,
        match_tvdb_m,
        match_bangumi_m,
        homepage,
        github,
    })
}

impl VideoTagApp {
    #[cfg(target_os = "macos")]
    fn native_menu_update(&mut self) {
        if self.native_menu.is_none() {
            self.native_menu = build_native_menu(self.lang);
        }
        let ids = {
            let Some(menu) = &self.native_menu else { return };
            (
                menu.about.id().clone(),
                menu.settings_m.id().clone(),
                menu.quit_app.id().clone(),
                menu.add_dir.id().clone(),
                menu.save_m.id().clone(),
                menu.select_all_m.id().clone(),
                menu.invert_m.id().clone(),
                menu.remove_m.id().clone(),
                menu.set_id_m.id().clone(),
                menu.match_tmdb_m.id().clone(),
                menu.match_tvdb_m.id().clone(),
                menu.match_bangumi_m.id().clone(),
                menu.homepage.id().clone(),
                menu.github.id().clone(),
            )
        };
        let (about_id, settings_id, quit_id, add_id, save_id, select_all_id, invert_id,
            remove_id, set_id_id, tmdb_id, tvdb_id, bangumi_id, homepage_id, github_id) =
            ids;
        while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            let id = event.id();
            if id == &about_id {
                self.show_about = true;
            } else if id == &settings_id {
                self.show_settings = true;
            } else if id == &quit_id {
                self.wants_quit = true;
            } else if id == &add_id {
                if !self.busy() {
                    self.pick_root_dialog();
                }
            } else if id == &save_id {
                // Same gate as the toolbar's 保存修改 button.
                if !self.busy()
                    && self.rows.iter().any(|r| r.checked && r.preview.is_some())
                {
                    self.show_save_dialog = true;
                }
            } else if id == &select_all_id {
                if !self.rows.is_empty() && !self.busy() {
                    for row in &mut self.rows {
                        row.checked = true;
                    }
                }
            } else if id == &invert_id {
                if !self.rows.is_empty() && !self.busy() {
                    for row in &mut self.rows {
                        row.checked = !row.checked;
                    }
                }
            } else if id == &remove_id {
                if self.any_checked() && !self.busy() {
                    self.remove_checked();
                }
            } else if id == &set_id_id {
                if !self.rows.is_empty() && self.any_checked() && !self.busy() {
                    self.id_input.clear();
                    self.id_platform = self.last_platform;
                    self.show_id_dialog = true;
                }
            } else if id == &tmdb_id {
                if !self.busy() {
                    self.start_match(IdPlatform::Tmdb);
                }
            } else if id == &tvdb_id {
                if !self.busy() {
                    self.start_match(IdPlatform::Tvdb);
                }
            } else if id == &bangumi_id {
                if !self.busy() {
                    self.start_match(IdPlatform::Bangumi);
                }
            } else if id == &homepage_id {
                open_url("https://mrben.net");
            } else if id == &github_id {
                // Placeholder until the VideoTag repo exists
                // (user's call, 2026-11-14): no destination yet.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UI helpers (house style, shared with MusicTag/SmartReName)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum TbIcon {
    AddFolder,
    SelectAll,
    Invert,
    Remove,
    Match,
    SetId,
    Execute,
    Stop,
    Settings,
}

fn draw_tb_icon(painter: &egui::Painter, c: egui::Pos2, icon: TbIcon, stroke: egui::Stroke) {
    // Same hand as SmartReName/PureMap: every mark is the same
    // 1.8px stroke in the same single color — no fills, no accents.
    let line = |a: egui::Pos2, b: egui::Pos2| painter.line_segment([a, b], stroke);
    let p = |dx: f32, dy: f32| egui::pos2(c.x + dx, c.y + dy);
    match icon {
        TbIcon::AddFolder => {
            painter.rect_stroke(
                egui::Rect::from_min_max(p(-9.0, -8.0), p(-2.0, -3.5)),
                1.5,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.rect_stroke(
                egui::Rect::from_center_size(p(0.0, 2.5), egui::vec2(19.0, 13.0)),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            line(p(1.5, 2.5), p(7.5, 2.5));
            line(p(4.5, -0.5), p(4.5, 5.5));
        }
        TbIcon::SelectAll | TbIcon::Invert => {
            painter.rect_stroke(
                egui::Rect::from_center_size(c, egui::vec2(17.0, 17.0)),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            if matches!(icon, TbIcon::SelectAll) {
                painter.add(egui::Shape::line(
                    vec![p(-5.0, 0.5), p(-1.5, 4.5), p(5.5, -4.5)],
                    stroke,
                ));
            } else {
                painter.rect_filled(
                    egui::Rect::from_min_max(p(0.0, -6.5), p(6.5, 6.5)),
                    0.0,
                    stroke.color,
                );
            }
        }
        TbIcon::Remove => {
            painter.rect_stroke(
                egui::Rect::from_center_size(p(0.0, 2.0), egui::vec2(12.0, 14.0)),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            line(p(-8.0, -7.0), p(8.0, -7.0));
            line(p(-3.0, -10.0), p(3.0, -10.0));
            line(p(-3.0, -2.0), p(-3.0, 7.0));
            line(p(3.0, -2.0), p(3.0, 7.0));
        }
        TbIcon::Match => {
            // Scope reticle: a ring with the cross running through it.
            painter.circle_stroke(c, 6.5, stroke);
            line(p(0.0, -10.5), p(0.0, 10.5));
            line(p(-10.5, 0.0), p(10.5, 0.0));
        }
        TbIcon::SetId => {
            // Hash sign: two slightly slanted uprights crossed
            // by two level bars.
            line(p(-3.0, -9.5), p(-5.5, 9.5));
            line(p(5.5, -9.5), p(3.0, 9.5));
            line(p(-9.5, -3.2), p(9.5, -3.2));
            line(p(-9.5, 3.2), p(9.5, 3.2));
        }
        TbIcon::Execute => {
            // A plain check mark: the "do it" sign.
            painter.add(egui::Shape::line(
                vec![p(-7.5, 0.5), p(-2.5, 5.5), p(7.5, -5.5)],
                stroke,
            ));
        }
        TbIcon::Stop => {
            painter.rect_stroke(
                egui::Rect::from_center_size(c, egui::vec2(12.0, 12.0)),
                1.5,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.rect_filled(
                egui::Rect::from_center_size(c, egui::vec2(6.0, 6.0)),
                0.0,
                stroke.color,
            );
        }
        TbIcon::Settings => {
            // Gear: a toothed outline (8 teeth) with a center hole.
            let teeth = 8;
            let r_out = 10.0_f32;
            let r_root = 7.0_f32;
            let mut pts: Vec<egui::Pos2> = Vec::new();
            for k in 0..teeth {
                let a = (k as f32) * std::f32::consts::TAU / teeth as f32;
                let deg = std::f32::consts::PI / 180.0;
                let at = |ang: f32, r: f32| {
                    let (sn, cs) = (a + ang * deg).sin_cos();
                    p(cs * r, sn * r)
                };
                pts.push(at(-17.0, r_root));
                pts.push(at(-10.0, r_out));
                pts.push(at(10.0, r_out));
                pts.push(at(17.0, r_root));
            }
            pts.push(pts[0]);
            painter.add(egui::Shape::line(pts, stroke));
            painter.circle_stroke(c, 2.6, stroke);
        }
    }
}

fn toolbar_button(ui: &mut egui::Ui, icon: TbIcon, label: &str, enabled: bool) -> bool {
    // 58x62, hover fill over the whole cell, one 1.8px stroke,
    // colors only from the text-color family.
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(58.0, 62.0), sense);
    let painter = ui.painter();
    if enabled && response.hovered() {
        painter.rect_filled(rect, 8.0, ui.visuals().widgets.hovered.bg_fill);
    }
    let color = if !enabled {
        ui.visuals().weak_text_color()
    } else if response.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    draw_tb_icon(
        &painter,
        egui::pos2(rect.center().x, rect.top() + 21.0),
        icon,
        egui::Stroke::new(1.8_f32, color),
    );
    painter.text(
        egui::pos2(rect.center().x, rect.bottom() - 9.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        if enabled {
            ui.visuals().text_color()
        } else {
            ui.visuals().weak_text_color()
        },
    );
    enabled && response.clicked()
}

/// Vertical hairline between toolbar groups.
fn toolbar_separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 44.0), egui::Sense::hover());
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    ui.painter().line_segment(
        [
            egui::pos2(rect.center().x, rect.top()),
            egui::pos2(rect.center().x, rect.bottom()),
        ],
        egui::Stroke::new(1.0_f32, color),
    );
}

fn paint_checkbox(
    painter: &egui::Painter,
    rect: egui::Rect,
    checked: bool,
    hovered: bool,
    visuals: &egui::Visuals,
) {
    let fill = if checked {
        egui::Color32::from_rgb(64, 120, 235)
    } else if hovered {
        visuals.widgets.hovered.bg_fill
    } else {
        visuals.widgets.inactive.bg_fill
    };
    painter.rect(
        rect,
        4.0,
        fill,
        egui::Stroke::new(1.0_f32, visuals.widgets.inactive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
    if checked {
        let c = rect.center();
        let st = egui::Stroke::new(2.0_f32, egui::Color32::WHITE);
        painter.line_segment(
            [egui::pos2(c.x - 5.0, c.y + 0.5), egui::pos2(c.x - 1.5, c.y + 4.0)],
            st,
        );
        painter.line_segment(
            [egui::pos2(c.x - 1.5, c.y + 4.0), egui::pos2(c.x + 5.5, c.y - 3.5)],
            st,
        );
    }
}

/// Paint text so its *ink* (the galley's mesh bounds), not its line
/// box, is centered at `center`.
fn paint_text_ink_centered(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    center: egui::Pos2,
) {
    let galley = painter.layout_no_wrap(text.to_string(), font, color);
    let pos = center - galley.mesh_bounds.center().to_vec2();
    painter.galley(pos, galley, color);
}

/// Same, with the ink's right edge at `right`, ink vertically
/// centered at `center_y`.
fn paint_text_ink_right(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    right: f32,
    center_y: f32,
) {
    let galley = painter.layout_no_wrap(text.to_string(), font, color);
    let mb = galley.mesh_bounds;
    painter.galley(egui::pos2(right - mb.max.x, center_y - mb.center().y), galley, color);
}

/// Same, with the ink's left edge at `left`.
fn paint_text_ink_left(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    left: f32,
    center_y: f32,
) {
    let galley = painter.layout_no_wrap(text.to_string(), font, color);
    let mb = galley.mesh_bounds;
    painter.galley(egui::pos2(left - mb.min.x, center_y - mb.center().y), galley, color);
}

/// A progress bar with its text painted ink-centered: the
/// stock egui bar centers its label by line box, so with
/// PingFang the text rides a few pixels high inside the bar
/// (user screenshot, 2026-11-14). The label is painted twice —
/// once in the plain text color over the track, once in white
/// clipped to the filled part — like the stock bar's two-tone
/// label, but ink-centered both times.
fn progress_bar_ink(ui: &mut egui::Ui, frac: f32, text: &str) {
    let height = 20.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    let visuals = ui.visuals();
    let radius = height / 2.0;
    painter.rect_filled(rect, radius, visuals.extreme_bg_color);
    let frac = frac.clamp(0.0, 1.0);
    let mut fill_rect = rect;
    fill_rect.set_width(rect.width() * frac);
    if frac > 0.0 {
        painter.rect_filled(fill_rect, radius, visuals.selection.bg_fill);
    }
    let font = egui::FontId::proportional(12.0);
    let center = rect.center();
    let galley = painter.layout_no_wrap(
        text.to_string(),
        font.clone(),
        visuals.text_color(),
    );
    let pos = center - galley.mesh_bounds.center().to_vec2();
    painter.galley(pos, galley, visuals.text_color());
    if fill_rect.width() > 1.0 {
        let clipped = painter.with_clip_rect(fill_rect);
        let galley =
            clipped.layout_no_wrap(text.to_string(), font, egui::Color32::WHITE);
        let pos = center - galley.mesh_bounds.center().to_vec2();
        clipped.galley(pos, galley, egui::Color32::WHITE);
    }
}

/// A settings checkbox row, hand-painted like the list rows:
/// the stock egui checkbox centers its label by line box, and
/// PingFang's ink sits above the line-box center, so the label
/// always looked a few pixels high next to the box (user
/// screenshot, 2026-11-14). Here the box is centered on the
/// row and the label is painted ink-centered on the same line.
/// Clicking anywhere on the row toggles. Returns true when the
/// value changed.
fn settings_checkbox(ui: &mut egui::Ui, value: &mut bool, label: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 22.0),
        egui::Sense::click(),
    );
    let mut changed = false;
    if response.clicked() {
        *value = !*value;
        changed = true;
    }
    let painter = ui.painter();
    let cb_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 9.0, rect.center().y),
        egui::vec2(18.0, 18.0),
    );
    paint_checkbox(painter, cb_rect, *value, response.hovered(), ui.visuals());
    let font = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .cloned()
        .unwrap_or_else(|| egui::FontId::proportional(14.0));
    paint_text_ink_left(
        painter,
        label,
        font,
        ui.visuals().text_color(),
        rect.left() + 27.0,
        rect.center().y,
    );
    changed
}

/// One segment of a platform choice (TMDB / TVDB / Bangumi) in
/// the ID dialogs: the selected segment wears the selection
/// colors, the others look like plain painted buttons.
fn segment_button(ui: &mut egui::Ui, label: &str, selected: bool, size: egui::Vec2) -> bool {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    let visuals = ui.visuals();
    let (fill, stroke_color, text_color) = if selected {
        (
            visuals.selection.bg_fill,
            visuals.selection.stroke.color,
            egui::Color32::WHITE,
        )
    } else if response.hovered() {
        (
            visuals.widgets.hovered.bg_fill,
            visuals.widgets.inactive.bg_stroke.color,
            if visuals.dark_mode {
                egui::Color32::WHITE
            } else {
                egui::Color32::from_rgb(30, 30, 30)
            },
        )
    } else {
        (
            visuals.widgets.inactive.bg_fill,
            visuals.widgets.inactive.bg_stroke.color,
            if visuals.dark_mode {
                egui::Color32::WHITE
            } else {
                egui::Color32::from_rgb(30, 30, 30)
            },
        )
    };
    painter.rect(
        rect,
        6.0,
        fill,
        egui::Stroke::new(1.0_f32, stroke_color),
        egui::StrokeKind::Inside,
    );
    let font = egui::FontId::proportional(13.0);
    let galley = painter.layout_no_wrap(label.to_string(), font, text_color);
    let pos = rect.center() - galley.mesh_bounds.center().to_vec2();
    painter.galley(pos, galley, text_color);
    response.clicked()
}

fn painted_button(
    ui: &mut egui::Ui,
    label: &str,
    font_size: f32,
    size: egui::Vec2,
    enabled: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = *ui.style().interact(&response);
    let painter = ui.painter();
    let fill = if !enabled {
        ui.visuals().widgets.inactive.bg_fill
    } else if response.is_pointer_button_down_on() {
        visuals.bg_fill
    } else if response.hovered() {
        ui.visuals().widgets.hovered.bg_fill
    } else {
        ui.visuals().widgets.inactive.bg_fill
    };
    painter.rect(
        rect,
        6.0,
        fill,
        egui::Stroke::new(1.0_f32, ui.visuals().widgets.inactive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
    let color = if enabled {
        if ui.visuals().dark_mode {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_rgb(30, 30, 30)
        }
    } else {
        ui.visuals().weak_text_color()
    };
    let font = egui::FontId::proportional(font_size);
    let galley = painter.layout_no_wrap(label.to_string(), font, color);
    let pos = rect.center() - galley.mesh_bounds.center().to_vec2();
    painter.galley(pos, galley, color);
    enabled && response.clicked()
}

// ---------------------------------------------------------------------------
// Main UI
// ---------------------------------------------------------------------------

fn ep_code(parsed: &ParsedName) -> Option<String> {
    let (season, episode) = (parsed.season?, parsed.episode?);
    let mut code = format!("S{season:02}E{episode:02}");
    if let Some(end) = parsed.episode_end {
        code.push_str(&format!("-E{end:02}"));
    }
    Some(code)
}

/// The file-name portion of a preview path (after the last '/')
/// — what a matched row shows as its main title.
fn preview_file_name(preview: &str) -> &str {
    preview.rsplit('/').next().unwrap_or(preview)
}

/// The directory portion of a preview path (before the last
/// '/'): the work folder / season folder the file will live in.
fn preview_dir(preview: &str) -> &str {
    preview.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

/// A matched row's second line: just where the file will go.
/// With an output folder set, that is the absolute target
/// directory; in-place mode shows the target's directory part
/// relative to the row's root, as before.
fn save_to_line(preview: &str, settings: &Settings, lang: Lang) -> String {
    let dir = match &settings.output_dir {
        Some(base) => base.join(preview_dir(preview)).display().to_string(),
        None => preview_dir(preview).to_string(),
    };
    format!("{}{}", t(lang, "保存至：", "Save to: "), dir)
}

/// Forgiving parse of a user-typed platform id. People paste all
/// sorts of shapes — the bare number, "TMDB-48455",
/// "[TVDB-00001234]", a whole library folder name carrying the
/// bracket, a platform page URL. Rules: when the text mentions
/// the platform's anchor ("tmdb" / "tvdb" / "bgm", case aside),
/// the first digit run after it wins; else a lone digit run
/// anywhere is the id. Zero, no digits, or several competing
/// runs with no anchor are rejected.
fn parse_platform_id_input(text: &str, platform: IdPlatform) -> Option<u64> {
    let anchor = match platform {
        IdPlatform::Tmdb => "tmdb",
        IdPlatform::Tvdb => "tvdb",
        IdPlatform::Bangumi => "bgm",
    };
    let lower = text.to_lowercase();
    let digit_runs: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .collect();
    let digits: &str = if let Some(pos) = lower.find(anchor) {
        let after = &lower[pos + anchor.len()..];
        after
            .split(|c: char| !c.is_ascii_digit())
            .find(|s| !s.is_empty())?
    } else if digit_runs.len() == 1 {
        digit_runs[0]
    } else {
        return None;
    };
    let id: u64 = digits.parse().ok()?;
    if id == 0 {
        None
    } else {
        Some(id)
    }
}

/// The TMDB-only spelling, kept for the original input tests.
#[cfg(test)]
fn parse_tmdb_id_input(text: &str) -> Option<u64> {
    parse_platform_id_input(text, IdPlatform::Tmdb)
}

impl eframe::App for VideoTagApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.skew_measured {
            self.skew_measured = true;
            let galley = ctx.fonts(|f| {
                f.layout_no_wrap(
                    "标题".to_string(),
                    egui::FontId::proportional(13.5),
                    egui::Color32::WHITE,
                )
            });
            let skew = galley.size().y / 2.0 - galley.mesh_bounds.center().y;
            self.field_margin_top = 2 + skew.round().clamp(0.0, 3.0) as i8;
        }
        #[cfg(target_os = "macos")]
        self.native_menu_update();
        if self.wants_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.show_about {
            show_about_window(ctx, &mut self.show_about, self.lang);
        }
        if self.show_settings {
            self.settings_window(ctx);
        }
        if self.show_save_dialog {
            self.save_dialog_window(ctx);
        }
        if self.show_id_dialog {
            self.id_dialog_window(ctx);
        }
        self.poll_match(ctx);
        self.poll_enrich(ctx);
        self.poll_probe(ctx);
        self.poll_execute(ctx);
        if self.picker.is_some() {
            self.picker_window(ctx);
        }
        if self.exec_running {
            self.exec_progress_modal(ctx);
        }
        // Folders dropped onto the window become library roots —
        // and, like the Add Folder dialog, start from a clean list
        // (cleared once for the whole drop, not per folder).
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !self.busy() {
            let dirs: Vec<PathBuf> =
                dropped.into_iter().filter(|p| p.is_dir()).collect();
            if !dirs.is_empty() {
                self.clear_list();
                for path in dirs {
                    self.add_root(path);
                }
            }
        }

        egui::TopBottomPanel::top("header")
            .exact_height(76.0)
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin {
                        left: 12,
                        top: 0,
                        right: 12,
                        bottom: 0,
                    }),
            )
            .show(ctx, |ui| {
                let resp = ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), ui.available_height()),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        if toolbar_button(ui, TbIcon::AddFolder, t(self.lang, "添加目录", "Add Folder"), !self.busy()) {
                            self.pick_root_dialog();
                        }
                        toolbar_separator(ui);
                        let has_rows = !self.rows.is_empty();
                        if toolbar_button(ui, TbIcon::SelectAll, t(self.lang, "全选", "Select All"), has_rows && !self.busy()) {
                            for row in self.rows.iter_mut() {
                                row.checked = true;
                            }
                        }
                        if toolbar_button(ui, TbIcon::Invert, t(self.lang, "反选", "Invert"), has_rows && !self.busy()) {
                            for row in self.rows.iter_mut() {
                                row.checked = !row.checked;
                            }
                        }
                        if toolbar_button(ui, TbIcon::Remove, t(self.lang, "移除", "Remove"), self.any_checked() && !self.busy()) {
                            self.remove_checked();
                        }
                        toolbar_separator(ui);
                        // 指定 ID leads the match group (user's
                        // order, 2026-11-14): Set ID, then the three
                        // platform buttons, then 保存修改 — one
                        // group, no separators inside.
                        if toolbar_button(ui, TbIcon::SetId, t(self.lang, "指定 ID", "Set ID"), has_rows && self.any_checked() && !self.busy()) {
                            self.id_input.clear();
                            self.id_platform = self.last_platform;
                            self.show_id_dialog = true;
                        }
                        // Manual platform buttons (user's call,
                        // 2026-11-14): click one and matching runs
                        // on that platform only — no automatic
                        // cross-platform chain. TVDB greys out
                        // without a key; TMDB without a key still
                        // clicks through to the settings guidance
                        // in start_match. While a batch runs, the
                        // layout must NOT change (user flagged the
                        // toolbar reshaping, 2026-11-14): the
                        // running platform's own button turns into
                        // 停止 in place, the other two just grey
                        // out.
                        {
                            let can_match =
                                has_rows && self.any_checked() && !self.exec_running;
                            let has_tvdb =
                                !self.settings.tvdb_api_key.trim().is_empty();
                            for (platform, name) in [
                                (IdPlatform::Tmdb, "TMDB"),
                                (IdPlatform::Tvdb, "TVDB"),
                                (IdPlatform::Bangumi, "Bangumi"),
                            ] {
                                if self.batch_running && platform == self.last_platform {
                                    if toolbar_button(ui, TbIcon::Stop, t(self.lang, "停止", "Stop"), true) {
                                        self.batch_cancel.store(true, Ordering::Relaxed);
                                    }
                                } else {
                                    let enabled = !self.batch_running
                                        && can_match
                                        && (platform != IdPlatform::Tvdb || has_tvdb);
                                    if toolbar_button(ui, TbIcon::Match, name, enabled) {
                                        self.start_match(platform);
                                    }
                                }
                            }
                        }
                        // 指定 ID / 平台匹配 / 保存修改 are ONE group
                        // (user's call, 2026-11-14 — same ruling as
                        // MusicTag: no separator before the apply
                        // button, and the apply button is labelled
                        // 保存修改 in both output modes). Clicking it
                        // opens the save-destination dialog; conflicts
                        // do not disable it — the dialog's choice of
                        // root re-decides them before executing.
                        let can_execute = !self.busy()
                            && self.rows.iter().any(|r| {
                                r.checked && r.preview.is_some()
                            });
                        if toolbar_button(ui, TbIcon::Execute, t(self.lang, "保存修改", "Save Changes"), can_execute) {
                            self.show_save_dialog = true;
                        }
                        toolbar_separator(ui);
                        if toolbar_button(ui, TbIcon::Settings, t(self.lang, "系统偏好", "Preferences"), true) {
                            self.show_settings = true;
                        }
                    },
                );
                // Single flat separator line (no fake shadow).
                let r = resp.response.rect;
                let line_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
                ui.painter().line_segment(
                    [
                        egui::pos2(r.left(), r.bottom() - 0.5),
                        egui::pos2(r.right(), r.bottom() - 0.5),
                    ],
                    egui::Stroke::new(1.0_f32, line_color),
                );
            });

        egui::TopBottomPanel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin {
                        left: 12,
                        top: 0,
                        right: 12,
                        bottom: 0,
                    }),
            )
            .show(ctx, |ui| {
                let resp = ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 44.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(&self.status).truncate());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.conflict_count > 0 {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} {} {}",
                                        t(self.lang, "重命名冲突", "Rename conflicts"),
                                        self.conflict_count,
                                        t(self.lang, "个", "")
                                    ))
                                    .color(egui::Color32::from_rgb(205, 95, 95)),
                                );
                            }
                        });
                    },
                );
                let r = resp.response.rect;
                let line_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
                ui.painter().line_segment(
                    [
                        egui::pos2(r.left(), r.top() + 0.5),
                        egui::pos2(r.right(), r.top() + 0.5),
                    ],
                    egui::Stroke::new(1.0_f32, line_color),
                );
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.rows.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(t(
                        self.lang,
                        "把装有电影或电视剧的目录添加进来（也可以直接把文件夹拖进窗口），然后点「在线匹配」。",
                        "Add a folder of movies or shows (or drop one onto the window), then hit Match.",
                    ));
                });
                return;
            }
            // List header: master checkbox + column titles.
            {
                let width = ui.available_width();
                let (hrect, _) =
                    ui.allocate_exact_size(egui::vec2(width, 26.0), egui::Sense::hover());
                let painter = ui.painter();
                let dim = egui::Color32::from_rgba_unmultiplied(150, 150, 150, 255);
                let font12 = egui::FontId::proportional(12.0);
                let all_checked =
                    !self.rows.is_empty() && self.rows.iter().all(|r| r.checked);
                let cb = egui::Rect::from_center_size(
                    egui::pos2(hrect.left() + 17.0, hrect.center().y),
                    egui::vec2(18.0, 18.0),
                );
                let cb_resp = ui.interact(cb, ui.id().with("master_cb"), egui::Sense::click());
                paint_checkbox(&painter, cb, all_checked, cb_resp.hovered(), ui.visuals());
                if cb_resp.clicked() && !self.busy() {
                    let target = !all_checked;
                    for row in self.rows.iter_mut() {
                        row.checked = target;
                    }
                }
                let mid = hrect.center().y;
                paint_text_ink_left(&painter, t(self.lang, "文件", "File"), font12.clone(), dim, hrect.left() + 34.0, mid);
                paint_text_ink_right(&painter, t(self.lang, "状态", "Status"), font12, dim, hrect.right() - 70.0, mid);
                painter.line_segment(
                    [
                        egui::pos2(hrect.left(), hrect.bottom() - 0.5),
                        egui::pos2(hrect.right(), hrect.bottom() - 0.5),
                    ],
                    egui::Stroke::new(
                        1.0_f32,
                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                    ),
                );
            }
            let mut clicked: Option<usize> = None;
            let mut more_clicked: Option<usize> = None;
            let mut toggles: Vec<usize> = Vec::new();
            egui::ScrollArea::vertical().show(ui, |ui| {
                let dark = ui.visuals().dark_mode;
                let zebra = if dark {
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 6)
                } else {
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 6)
                };
                let name_color = ui.visuals().text_color();
                let dim_color = egui::Color32::from_rgba_unmultiplied(150, 150, 150, 255);
                let weak_color = if dark {
                    egui::Color32::from_rgb(168, 172, 180)
                } else {
                    egui::Color32::from_rgb(92, 97, 108)
                };
                let sel_fill = if dark {
                    egui::Color32::from_rgba_unmultiplied(90, 140, 255, 60)
                } else {
                    egui::Color32::from_rgba_unmultiplied(60, 120, 255, 50)
                };
                let hover_fill = if dark {
                    egui::Color32::from_rgba_unmultiplied(120, 170, 255, 30)
                } else {
                    egui::Color32::from_rgba_unmultiplied(60, 120, 255, 28)
                };
                let font = egui::FontId::proportional(14.0);
                let font12 = egui::FontId::proportional(12.0);
                for (i, row) in self.rows.iter().enumerate() {
                    let work = &self.works[row.work];
                    let width = ui.available_width();
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(width, 46.0), egui::Sense::click());
                    let painter = ui.painter();
                    if Some(i) == self.selected {
                        painter.rect_filled(rect, 4.0, sel_fill);
                    } else if i % 2 == 1 {
                        painter.rect_filled(rect, 4.0, zebra);
                    }
                    if response.hovered() && Some(i) != self.selected {
                        painter.rect_filled(rect, 4.0, hover_fill);
                    }
                    if response.clicked() {
                        clicked = Some(i);
                    }
                    if row.conflict {
                        let hint = match row.conflict_kind {
                            Some(ConflictKind::TargetExists) => t(
                                self.lang,
                                "目标文件夹里已有同名文件，保存时会跳过这一行、不会覆盖。删掉目标里的那个文件（或换个保存目录）即可。",
                                "A file with this name already exists in the target folder. Saving skips this row and never overwrites. Delete that file (or pick another destination) to fix it.",
                            ),
                            _ => t(
                                self.lang,
                                "列表中有多行要保存成同一个文件，保存时会全部跳过。让它们的名字区分开（比如分出集数）即可。",
                                "Several rows would be saved as the same file. Saving skips them all. Give them distinct names (e.g. episode numbers) to fix it.",
                            ),
                        };
                        response.on_hover_text(hint);
                    }
                    let cb_rect = egui::Rect::from_center_size(
                        egui::pos2(rect.left() + 17.0, rect.center().y),
                        egui::vec2(18.0, 18.0),
                    );
                    let cb_resp =
                        ui.interact(cb_rect, ui.id().with(("cb", i)), egui::Sense::click());
                    paint_checkbox(&painter, cb_rect, row.checked, cb_resp.hovered(), ui.visuals());
                    if cb_resp.clicked() {
                        toggles.push(i);
                    }
                    let line1_y = rect.center().y - 10.0;
                    let line2_y = rect.center().y + 11.0;
                    // Status (line 1, right side).
                    let green = egui::Color32::from_rgb(80, 170, 90);
                    let red = egui::Color32::from_rgb(205, 95, 95);
                    let amber = egui::Color32::from_rgb(230, 170, 60);
                    let (status_text, status_color) = if let Some(err) = &row.exec_error {
                        (err.clone(), red)
                    } else if row.conflict {
                        // Name the reason, not just the red paint:
                        // the two kinds need opposite remedies
                        // (full explanation on hover, below).
                        match row.conflict_kind {
                            Some(ConflictKind::TargetExists) => (
                                t(self.lang, "目标已存在", "Target exists").to_string(),
                                red,
                            ),
                            _ => (
                                t(
                                    self.lang,
                                    "与列表中其他行同名",
                                    "Same target as another row",
                                )
                                .to_string(),
                                red,
                            ),
                        }
                    } else if row.done {
                        (t(self.lang, "已完成", "Done").to_string(), green)
                    } else if work.searching {
                        (t(self.lang, "匹配中…", "Matching…").to_string(), dim_color)
                    } else if row.ep_missing {
                        (t(self.lang, "TMDB 无此集", "Not on TMDB").to_string(), amber)
                    } else if let Some(cand) = work.matched() {
                        let year = cand
                            .year
                            .map(|y| format!(" ({y})"))
                            .unwrap_or_default();
                        (
                            format!("{} · {}{}", t(self.lang, "已匹配", "Matched"), cand.title, year),
                            green,
                        )
                    } else if work.unrecognized {
                        (t(self.lang, "无法识别", "Unrecognized").to_string(), amber)
                    } else if work.failed {
                        (t(self.lang, "匹配失败", "Failed").to_string(), red)
                    } else if self.batch_running && row.checked {
                        (t(self.lang, "等待中…", "Queued…").to_string(), dim_color)
                    } else {
                        (String::new(), dim_color)
                    };
                    if !status_text.is_empty() {
                        paint_text_ink_right(
                            &painter,
                            &status_text,
                            font12.clone(),
                            status_color,
                            rect.right() - 70.0,
                            rect.center().y,
                        );
                    }
                    // "More" button: opens this work's candidate picker.
                    // The status text and this button form one unit,
                    // centered across the row's two lines (user's
                    // call, 2026-11-14).
                    if !work.candidates.is_empty() {
                        let btn_rect = egui::Rect::from_center_size(
                            egui::pos2(rect.right() - 35.0, rect.center().y),
                            egui::vec2(54.0, 24.0),
                        );
                        let btn_resp =
                            ui.interact(btn_rect, ui.id().with(("more", i)), egui::Sense::click());
                        let btn_visuals = *ui.style().interact(&btn_resp);
                        let btn_fill = if !self.busy() && btn_resp.hovered() {
                            ui.visuals().widgets.hovered.bg_fill
                        } else {
                            ui.visuals().widgets.inactive.bg_fill
                        };
                        painter.rect(
                            btn_rect,
                            5.0,
                            btn_fill,
                            btn_visuals.bg_stroke,
                            egui::StrokeKind::Inside,
                        );
                        let btn_color = if self.busy() {
                            dim_color
                        } else if dark {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::from_rgb(30, 30, 30)
                        };
                        paint_text_ink_centered(
                            &painter,
                            t(self.lang, "更多", "More"),
                            font12.clone(),
                            btn_color,
                            btn_rect.center(),
                        );
                        if btn_resp.clicked() && !self.busy() {
                            more_clicked = Some(i);
                        }
                    }
                    // Line 1: the new file name once the row has
                    // a preview (the original name until then).
                    // A rename conflict paints the new name red —
                    // the row's one conflict flag (the footer
                    // carries the batch count).
                    {
                        let (text, color) = match &row.preview {
                            Some(preview) => (
                                preview_file_name(preview).to_string(),
                                if row.conflict { red } else { name_color },
                            ),
                            None => (row.name.clone(), name_color),
                        };
                        let mut job = egui::text::LayoutJob::simple(
                            text,
                            font.clone(),
                            color,
                            (rect.width() - 34.0 - 290.0).max(80.0),
                        );
                        job.wrap.max_rows = 1;
                        job.wrap.overflow_character = Some('\u{2026}');
                        let galley = painter.layout_job(job);
                        let mb = galley.mesh_bounds;
                        painter.galley(
                            egui::pos2(rect.left() + 34.0 - mb.min.x, line1_y - mb.center().y),
                            galley,
                            color,
                        );
                    }
                    // Line 2: for a matched row, only where the
                    // file will go (its target directory); before
                    // matching, the parsed-info line.
                    {
                        if let Some(preview) = &row.preview {
                            let color = if dark {
                                egui::Color32::from_rgb(140, 180, 235)
                            } else {
                                egui::Color32::from_rgb(40, 90, 170)
                            };
                            let mut job = egui::text::LayoutJob::simple(
                                save_to_line(preview, &self.settings, self.lang),
                                font12.clone(),
                                color,
                                (rect.width() - 34.0 - 10.0).max(80.0),
                            );
                            job.wrap.max_rows = 1;
                            job.wrap.overflow_character = Some('\u{2026}');
                            let galley = painter.layout_job(job);
                            let mb = galley.mesh_bounds;
                            painter.galley(
                                egui::pos2(rect.left() + 34.0 - mb.min.x, line2_y - mb.center().y),
                                galley,
                                color,
                            );
                        } else {
                            let mut bits: Vec<String> = Vec::new();
                            bits.push(if work.parsed.title.is_empty() {
                                "—".to_string()
                            } else {
                                work.parsed.title.clone()
                            });
                            match ep_code(&row.parsed) {
                                Some(code) => bits.push(code),
                                None => bits.push(t(self.lang, "电影", "Movie").to_string()),
                            }
                            bits.push(work.method.label(self.lang).to_string());
                            if row.small || row.parsed.is_extra {
                                bits.push(t(self.lang, "附属", "Extra").to_string());
                            }
                            if !row.subtitles.is_empty() {
                                bits.push(format!(
                                    "{} ×{}",
                                    t(self.lang, "字幕", "Subs"),
                                    row.subtitles.len()
                                ));
                            }
                            let mut job = egui::text::LayoutJob::default();
                            job.append(
                                &bits.join(" · "),
                                0.0,
                                egui::TextFormat {
                                    font_id: font12.clone(),
                                    color: weak_color,
                                    ..Default::default()
                                },
                            );
                            job.wrap.max_width = (rect.width() - 34.0 - 10.0).max(80.0);
                            job.wrap.max_rows = 1;
                            job.wrap.overflow_character = Some('\u{2026}');
                            let galley = painter.layout_job(job);
                            let mb = galley.mesh_bounds;
                            painter.galley(
                                egui::pos2(rect.left() + 34.0 - mb.min.x, line2_y - mb.center().y),
                                galley,
                                weak_color,
                            );
                        }
                    }
                }
            });
            for i in toggles {
                if !self.busy() {
                    if let Some(row) = self.rows.get_mut(i) {
                        row.checked = !row.checked;
                    }
                }
            }
            if let Some(index) = clicked {
                self.selected = Some(index);
            }
            if let Some(index) = more_clicked {
                self.selected = Some(index);
                if let Some(row) = self.rows.get(index) {
                    // The ID row starts on the platform the work
                    // already belongs to, else the last-used one.
                    let id_platform = self
                        .works
                        .get(row.work)
                        .and_then(|w| w.parsed.platform_id())
                        .map(|(p, _)| p)
                        .unwrap_or(self.last_platform);
                    self.picker = Some(PickerUi {
                        work: row.work,
                        id_input: String::new(),
                        id_platform,
                    });
                }
            }
        });
    }
}

impl VideoTagApp {
    /// Manual platform-ID dialog (toolbar): pin one ID, on one
    /// chosen platform, on every work the checked rows belong
    /// to and match by ID lookup. The platform choice starts at
    /// the last-used match platform.
    fn id_dialog_window(&mut self, ctx: &egui::Context) {
        let lang = self.lang;
        let mut open = self.show_id_dialog;
        let mut apply = false;
        let mut cancel = false;
        let mut platform = self.id_platform;
        egui::Window::new(t(lang, "指定 ID", "Set ID"))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .movable(false)
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_size(egui::vec2(400.0, 252.0))
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} {} {}",
                    t(lang, "给所有勾选文件所属的作品指定", "Assign a"),
                    platform_label(platform),
                    t(lang, "ID，按 ID 重新匹配。", "ID to every checked file's work and match by ID."),
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    for (p, label) in [
                        (IdPlatform::Tmdb, "TMDB"),
                        (IdPlatform::Tvdb, "TVDB"),
                        (IdPlatform::Bangumi, "Bangumi"),
                    ] {
                        if segment_button(ui, label, platform == p, egui::vec2(84.0, 28.0)) {
                            platform = p;
                        }
                    }
                });
                ui.add_space(8.0);
                let valid = parse_platform_id_input(&self.id_input, platform).is_some();
                let hint = match platform {
                    IdPlatform::Tmdb => "TMDB-48455",
                    IdPlatform::Tvdb => "TVDB-78801",
                    IdPlatform::Bangumi => "BGM-64131",
                };
                let resp = ui.add_sized(
                    [ui.available_width(), 26.0],
                    egui::TextEdit::singleline(&mut self.id_input)
                        .margin(egui::Margin {
                            left: 4,
                            top: self.field_margin_top,
                            right: 4,
                            bottom: 2,
                        })
                        .hint_text(hint),
                );
                if valid
                    && resp.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                {
                    apply = true;
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if painted_button(ui, t(lang, "应用", "Apply"), 14.0, egui::vec2(92.0, 32.0), valid) {
                            apply = true;
                        }
                        if painted_button(ui, t(lang, "取消", "Cancel"), 14.0, egui::vec2(92.0, 32.0), true) {
                            cancel = true;
                        }
                    });
                });
            });
        self.id_platform = platform;
        self.show_id_dialog = open && !apply && !cancel;
        if cancel {
            self.id_input.clear();
        }
        if apply {
            if let Some(id) = parse_platform_id_input(&self.id_input, platform) {
                let mut works: Vec<usize> = Vec::new();
                for row in &self.rows {
                    if row.checked && !works.contains(&row.work) {
                        works.push(row.work);
                    }
                }
                self.id_input.clear();
                self.start_id_match(id, platform, works);
            }
        }
    }

    /// Save-destination dialog: the 保存修改 button opens this.
    /// The user picks where the checked files go — export folder,
    /// the last export folder again, or rename in place — and
    /// execution starts immediately on the choice (user's flow,
    /// 2026-11-14; this replaces the Settings-based output dir).
    fn save_dialog_window(&mut self, ctx: &egui::Context) {
        let lang = self.lang;
        let mut open = self.show_save_dialog;
        let count = self
            .rows
            .iter()
            .filter(|r| r.checked && r.preview.is_some())
            .count();
        let last_dir = self.settings.output_dir.clone();
        // None = still deciding; Some(choice) where choice is
        // Some(dir) to export there, None to rename in place.
        let mut action: Option<Option<PathBuf>> = None;
        let mut cancel = false;
        egui::Window::new(t(lang, "保存修改", "Save Changes"))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .movable(false)
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_size(egui::vec2(420.0, 280.0))
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} {} {}",
                    t(lang, "将保存", "Save"),
                    count,
                    t(lang, "个已勾选文件，保存到哪里？", "checked files — where to?")
                ));
                ui.add_space(10.0);
                let width = ui.available_width();
                if let Some(dir) = &last_dir {
                    ui.label(t(lang, "上次导出目录：", "Last export folder:"));
                    ui.add(egui::Label::new(dir.display().to_string()).truncate());
                    ui.add_space(6.0);
                    if painted_button(
                        ui,
                        t(lang, "保存到上次目录", "Save to the last folder"),
                        13.0,
                        egui::vec2(width, 30.0),
                        count > 0,
                    ) {
                        action = Some(Some(dir.clone()));
                    }
                    ui.add_space(8.0);
                }
                if painted_button(
                    ui,
                    t(lang, "选择导出目录…", "Choose export folder…"),
                    13.0,
                    egui::vec2(width, 30.0),
                    count > 0,
                ) {
                    #[cfg(any(target_os = "macos", target_os = "windows"))]
                    {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            action = Some(Some(dir));
                        }
                    }
                }
                ui.add_space(8.0);
                if painted_button(
                    ui,
                    t(lang, "原地改名", "Rename in place"),
                    13.0,
                    egui::vec2(width, 30.0),
                    count > 0,
                ) {
                    action = Some(None);
                }
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if painted_button(ui, t(lang, "取消", "Cancel"), 13.0, egui::vec2(84.0, 28.0), true) {
                        cancel = true;
                    }
                });
            });
        if cancel {
            open = false;
        }
        if let Some(choice) = action {
            // Root the previews at the chosen destination first so
            // targets and conflicts are re-decided for it, then go.
            if self.settings.output_dir != choice {
                self.settings.output_dir = choice;
                store_settings(&self.settings);
            }
            self.refresh_previews();
            open = false;
            self.start_execute();
        }
        self.show_save_dialog = open;
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let lang = self.lang;
        let mut open = self.show_settings;
        let mut changed = false;
        egui::Window::new(t(lang, "系统偏好", "Preferences"))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .movable(false)
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            // Generous inner margins: the default window margin
            // (8) left the content hugging the edges and the
            // window felt packed (user, 2026-11-14).
            .frame(egui::Frame::window(&ctx.style()).inner_margin(egui::Margin {
                left: 22,
                right: 22,
                top: 16,
                bottom: 20,
            }))
            .fixed_size(egui::vec2(500.0, 545.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(t(lang, "TMDB API Key（v3）", "TMDB API Key (v3)"));
                ui.add_space(6.0);
                let resp = ui.add_sized(
                    [ui.available_width(), 26.0],
                    egui::TextEdit::singleline(&mut self.settings.api_key)
                        .password(true)
                        .margin(egui::Margin {
                            left: 4,
                            top: self.field_margin_top,
                            right: 4,
                            bottom: 2,
                        })
                        .hint_text("6009…"),
                );
                if resp.changed() {
                    changed = true;
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(t(
                        lang,
                        "没有 Key？在 TMDB 网站免费申请：",
                        "No key yet? Get one free on TMDB:",
                    ));
                    ui.hyperlink_to(
                        "themoviedb.org/settings/api",
                        "https://www.themoviedb.org/settings/api",
                    );
                });
                ui.add_space(16.0);
                ui.separator();
                ui.add_space(16.0);
                ui.label(t(lang, "TheTVDB API Key（v4）", "TheTVDB API Key (v4)"));
                ui.add_space(6.0);
                let resp = ui.add_sized(
                    [ui.available_width(), 26.0],
                    egui::TextEdit::singleline(&mut self.settings.tvdb_api_key)
                        .password(true)
                        .margin(egui::Margin {
                            left: 4,
                            top: self.field_margin_top,
                            right: 4,
                            bottom: 2,
                        })
                        .hint_text("…"),
                );
                if resp.changed() {
                    changed = true;
                }
                ui.add_space(16.0);
                ui.separator();
                ui.add_space(16.0);
                ui.label(t(
                    lang,
                    "TheTVDB PIN（订阅型 Key 才需要，可空）",
                    "TheTVDB PIN (subscriber keys only, may be empty)",
                ));
                ui.add_space(6.0);
                let resp = ui.add_sized(
                    [ui.available_width(), 26.0],
                    egui::TextEdit::singleline(&mut self.settings.tvdb_pin)
                        .password(true)
                        .margin(egui::Margin {
                            left: 4,
                            top: self.field_margin_top,
                            right: 4,
                            bottom: 2,
                        })
                        .hint_text("…"),
                );
                if resp.changed() {
                    changed = true;
                }
                ui.add_space(16.0);
                ui.separator();
                ui.add_space(16.0);
                if settings_checkbox(
                    ui,
                    &mut self.settings.keep_tech_tags,
                    t(lang, "保留画质标记（2160p、HEVC 等）", "Keep quality tags (2160p, HEVC, …)"),
                ) {
                    changed = true;
                }
                ui.add_space(8.0);
                if settings_checkbox(
                    ui,
                    &mut self.settings.delete_empty_dirs,
                    t(lang, "执行后删除变空的旧目录", "Remove folders left empty after renaming"),
                ) {
                    changed = true;
                }
                // The save destination used to live here; it moved
                // onto the 保存修改 button's dialog (user's call,
                // 2026-11-14). settings.output_dir now only
                // remembers the last export folder for that dialog.
            });
        self.show_settings = open;
        if changed {
            store_settings(&self.settings);
            self.refresh_previews();
        }
    }

    /// Candidate picker for one matched work: shows the cached
    /// candidates; clicking one re-points the work (and every row's
    /// preview) at it.
    fn picker_window(&mut self, ctx: &egui::Context) {
        let lang = self.lang;
        let Some(picker) = &self.picker else { return };
        let work_idx = picker.work;
        let Some(work) = self.works.get(work_idx) else {
            self.picker = None;
            return;
        };
        let candidates = work.candidates.clone();
        let chosen = work.chosen;
        let searching = work.searching;
        let batch_running = self.batch_running;
        let field_margin_top = self.field_margin_top;
        let mut id_buf = picker.id_input.clone();
        let mut id_platform = picker.id_platform;
        let work_title = if work.parsed.title.is_empty() {
            t(lang, "选择匹配结果", "Choose a match").to_string()
        } else {
            work.parsed.title.clone()
        };
        let mut close = false;
        let mut pick: Option<usize> = None;
        let mut apply_id = false;
        egui::Window::new(format!(
            "{} · {}",
            t(lang, "匹配结果", "Matches"),
            work_title
        ))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .movable(false)
        .default_size(egui::vec2(680.0, 480.0))
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.label(t(
                lang,
                "点一行换用那个结果，这个作品的所有文件预览会跟着变。",
                "Click a row to use it — every file of this work follows.",
            ));
            ui.add_space(6.0);
            if candidates.is_empty() {
                ui.label(if searching {
                    t(lang, "匹配中…", "Matching…")
                } else {
                    t(lang, "暂无候选结果。", "No candidates yet.")
                });
                ui.add_space(6.0);
            }
            let dark = ui.visuals().dark_mode;
            let zebra = if dark {
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 6)
            } else {
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 6)
            };
            let sel_fill = if dark {
                egui::Color32::from_rgba_unmultiplied(90, 140, 255, 60)
            } else {
                egui::Color32::from_rgba_unmultiplied(60, 120, 255, 50)
            };
            let hover_fill = if dark {
                egui::Color32::from_rgba_unmultiplied(120, 170, 255, 30)
            } else {
                egui::Color32::from_rgba_unmultiplied(60, 120, 255, 28)
            };
            let name_color = ui.visuals().text_color();
            let weak_color = if dark {
                egui::Color32::from_rgb(168, 172, 180)
            } else {
                egui::Color32::from_rgb(92, 97, 108)
            };
            let font1 = egui::FontId::proportional(14.0);
            let font2 = egui::FontId::proportional(12.0);
            egui::ScrollArea::vertical()
                .max_height(340.0)
                .id_salt("picker_list")
                .show(ui, |ui| {
                    for (i, cand) in candidates.iter().enumerate() {
                        let width = ui.available_width();
                        let (rect, response) =
                            ui.allocate_exact_size(egui::vec2(width, 46.0), egui::Sense::click());
                        let painter = ui.painter();
                        if chosen == Some(i) {
                            painter.rect_filled(rect, 4.0, sel_fill);
                        } else if i % 2 == 1 {
                            painter.rect_filled(rect, 4.0, zebra);
                        }
                        if response.hovered() && chosen != Some(i) {
                            painter.rect_filled(rect, 4.0, hover_fill);
                        }
                        if response.clicked() {
                            pick = Some(i);
                        }
                        let text_left = rect.left() + 10.0;
                        let year = cand.year.map(|y| format!(" ({y})")).unwrap_or_default();
                        let mut job = egui::text::LayoutJob::simple(
                            format!("{}{year}", cand.title),
                            font1.clone(),
                            name_color,
                            rect.width() - 20.0,
                        );
                        job.wrap.max_rows = 1;
                        job.wrap.overflow_character = Some('\u{2026}');
                        let galley = painter.layout_job(job);
                        let mb1 = galley.mesh_bounds;
                        painter.galley(
                            egui::pos2(text_left - mb1.min.x, rect.center().y - 10.0 - mb1.center().y),
                            galley,
                            name_color,
                        );
                        let mut bits: Vec<String> = Vec::new();
                        // Source tag first: which catalogue this
                        // candidate came from.
                        bits.push(platform_label(cand.source).to_string());
                        if !cand.original_title.is_empty() && cand.original_title != cand.title {
                            bits.push(cand.original_title.clone());
                        }
                        bits.push(match cand.media_type {
                            MediaType::Tv => t(lang, "电视剧", "TV").to_string(),
                            MediaType::Movie => t(lang, "电影", "Movie").to_string(),
                        });
                        if cand.vote_count > 0 {
                            bits.push(format!("★ {:.1}", cand.vote_average));
                        }
                        if chosen == Some(i) {
                            bits.push(t(lang, "当前选择", "Selected").to_string());
                        }
                        paint_text_ink_left(
                            &painter,
                            &bits.join(" · "),
                            font2.clone(),
                            weak_color,
                            text_left,
                            rect.center().y + 11.0,
                        );
                    }
                });
            ui.add_space(8.0);
            // Manual platform-ID row: force this one work onto an
            // ID lookup on the chosen platform, independent of the
            // candidate list above. The small button cycles the
            // platform (TMDB → TVDB → BGM → TMDB).
            ui.horizontal(|ui| {
                ui.label(t(lang, "ID：", "ID:"));
                if painted_button(ui, id_platform.tag(), 13.0, egui::vec2(64.0, 26.0), true) {
                    id_platform = match id_platform {
                        IdPlatform::Tmdb => IdPlatform::Tvdb,
                        IdPlatform::Tvdb => IdPlatform::Bangumi,
                        IdPlatform::Bangumi => IdPlatform::Tmdb,
                    };
                }
                let valid = parse_platform_id_input(&id_buf, id_platform).is_some();
                let hint = match id_platform {
                    IdPlatform::Tmdb => "TMDB-48455",
                    IdPlatform::Tvdb => "TVDB-78801",
                    IdPlatform::Bangumi => "BGM-64131",
                };
                ui.add_sized(
                    [170.0, 26.0],
                    egui::TextEdit::singleline(&mut id_buf)
                        .margin(egui::Margin {
                            left: 4,
                            top: field_margin_top,
                            right: 4,
                            bottom: 2,
                        })
                        .hint_text(hint),
                );
                if painted_button(ui, t(lang, "用此 ID", "Use this ID"), 13.0, egui::vec2(96.0, 26.0), valid && !batch_running) {
                    apply_id = true;
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if painted_button(ui, t(lang, "关闭", "Close"), 14.0, egui::vec2(92.0, 32.0), true) {
                        close = true;
                    }
                });
            });
        });
        let applied_id = if apply_id {
            parse_platform_id_input(&id_buf, id_platform)
        } else {
            None
        };
        if let Some(p) = &mut self.picker {
            p.id_input = if applied_id.is_some() {
                String::new()
            } else {
                id_buf
            };
            p.id_platform = id_platform;
        }
        if close {
            self.picker = None;
        }
        if let Some(i) = pick {
            self.choose_candidate(work_idx, i);
        }
        if let Some(id) = applied_id {
            self.start_id_match(id, id_platform, vec![work_idx]);
        }
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "C:\\Windows\\Fonts\\msyh.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                if let Some(list) = fonts.families.get_mut(&family) {
                    list.push("cjk".to_owned());
                }
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

fn main() -> eframe::Result<()> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).ok();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("")
            .with_inner_size([1280.0, 840.0])
            .with_min_inner_size([1024.0, 700.0])
            .with_icon(icon.unwrap_or_default()),
        ..Default::default()
    };
    eframe::run_native(
        "VideoTag",
        options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(VideoTagApp::default()))
        }),
    )
}

/// The About window, in the house style shared with the other apps.
fn show_about_window(ctx: &egui::Context, show_about: &mut bool, lang: Lang) {
    let mut open = *show_about;
    let mut close_requested = false;
    egui::Window::new(t(lang, "关于 VideoTag", "About VideoTag"))
        .id(egui::Id::new("about_window"))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .title_bar(false)
        .fixed_size(egui::Vec2::new(520.0, 520.0))
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.allocate_ui_with_layout(
                egui::Vec2::new(ui.available_width(), 40.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.label(egui::RichText::new(t(lang, "关于 VideoTag", "About VideoTag")).size(14.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (button_rect, response) =
                            ui.allocate_exact_size(egui::Vec2::splat(30.0), egui::Sense::click());
                        if response.hovered() {
                            ui.painter().rect_filled(
                                button_rect,
                                4.0,
                                ui.visuals().widgets.hovered.bg_fill,
                            );
                        }
                        let center = button_rect.center();
                        let half = 5.0;
                        let stroke = egui::Stroke::new(1.5_f32, ui.visuals().text_color());
                        ui.painter().line_segment(
                            [center + egui::Vec2::splat(-half), center + egui::Vec2::splat(half)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [
                                center + egui::Vec2::new(-half, half),
                                center + egui::Vec2::new(half, -half),
                            ],
                            stroke,
                        );
                        if response.clicked() {
                            close_requested = true;
                        }
                    });
                },
            );
            ui.separator();
            let body_size = ui.available_size();
            ui.allocate_ui_with_layout(
                body_size,
                egui::Layout::top_down(egui::Align::Center).with_main_align(egui::Align::Center),
                |ui| {
                    ui.add_space(10.0);
                    about_icon(ui);
                    ui.add_space(20.0);
                    ui.label(egui::RichText::new("VideoTag").size(22.0));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(match lang {
                            Lang::Zh => format!("版本 {}", env!("CARGO_PKG_VERSION")),
                            Lang::En => format!("Version {}", env!("CARGO_PKG_VERSION")),
                        })
                        .size(16.0),
                    );
                    ui.add_space(12.0);
                    // The blurb is broken into two lines by hand:
                    // left to auto-wrap it shed an orphan “件。”
                    // onto a line of its own (user screenshot,
                    // 2026-11-14).
                    ui.label(
                        egui::RichText::new(t(
                            lang,
                            "从 TMDB、TheTVDB、Bangumi 匹配电影与剧集信息，",
                            "Match movies and shows on TMDB, TheTVDB and Bangumi,",
                        ))
                        .size(13.5)
                        .weak(),
                    );
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(t(
                            lang,
                            "按统一结构重命名文件。",
                            "and rename them into a tidy library layout.",
                        ))
                        .size(13.5)
                        .weak(),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("TMDB · TheTVDB · Bangumi · MKV · MP4 · AVI · MOV · TS · RMVB")
                            .size(12.5)
                            .weak(),
                    );
                    ui.add_space(24.0);
                    ui.label(egui::RichText::new("By Mr.Ben").size(16.0));
                    ui.add_space(2.0);
                    ui.hyperlink_to(
                        egui::RichText::new("https://mrben.net").size(16.0),
                        "https://mrben.net",
                    );
                    ui.add_space(10.0);
                },
            );
        });
    if close_requested {
        open = false;
    }
    *show_about = open;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junk_directories_are_filtered_but_seasons_stay() {
        for name in [
            "BDMV", "@eaDir", "#recycle", "$RECYCLE.BIN", "System Volume Information",
            "Sample", "samples", "Trailer", "Extras", "Featurettes", "预告片", "花絮",
        ] {
            assert!(is_junk_dir(name), "{name}");
        }
        for name in ["Season 1", "Specials", "S01", "刘老根", "CD1", "Subs"] {
            assert!(!is_junk_dir(name), "{name}");
        }
    }

    #[test]
    fn sample_files_are_filtered_by_whole_token() {
        assert!(is_sample_file(Path::new("Show.S01E01.sample.mkv")));
        assert!(is_sample_file(Path::new("movie-sample.mp4")));
        assert!(!is_sample_file(Path::new("Show.S01E01.mkv")));
        assert!(!is_sample_file(Path::new("Samplers.2024.mkv")));
    }

    #[test]
    fn tmdb_id_input_accepts_bare_and_padded_numbers() {
        assert_eq!(parse_tmdb_id_input("48455"), Some(48455));
        assert_eq!(parse_tmdb_id_input("  48455  "), Some(48455));
        assert_eq!(parse_tmdb_id_input("00048455"), Some(48455));
        assert_eq!(parse_tmdb_id_input("ID: 48455"), Some(48455));
    }

    #[test]
    fn tmdb_id_input_follows_the_tmdb_anchor() {
        assert_eq!(parse_tmdb_id_input("TMDB-48455"), Some(48455));
        assert_eq!(parse_tmdb_id_input("tmdb-00048455"), Some(48455));
        assert_eq!(parse_tmdb_id_input("{tmdb=48455}"), Some(48455));
        assert_eq!(parse_tmdb_id_input("[TMDB-00115782]"), Some(115782));
        // A whole library folder name: rating and year digits
        // before the anchor must not leak into the id.
        assert_eq!(
            parse_tmdb_id_input("[7.9] [1991] [TMDB-00048455]《101次求婚》(The 101st Proposal)"),
            Some(48455)
        );
    }

    #[test]
    fn tmdb_id_input_reads_a_tmdb_page_url() {
        assert_eq!(
            parse_tmdb_id_input("https://www.themoviedb.org/movie/115782"),
            Some(115782)
        );
        assert_eq!(
            parse_tmdb_id_input("https://www.themoviedb.org/tv/48455"),
            Some(48455)
        );
    }

    #[test]
    fn tmdb_id_input_rejects_junk() {
        assert_eq!(parse_tmdb_id_input(""), None);
        assert_eq!(parse_tmdb_id_input("abc"), None);
        assert_eq!(parse_tmdb_id_input("0"), None);
        assert_eq!(parse_tmdb_id_input("TMDB-"), None);
        // Two competing numbers and no anchor: ambiguous.
        assert_eq!(parse_tmdb_id_input("1991 48455"), None);
        // Overflow is not an id.
        assert_eq!(parse_tmdb_id_input("99999999999999999999999999"), None);
    }

    #[test]
    fn platform_id_input_uses_the_platform_anchor() {
        // Each platform's own tag anchors the parse; the anchor
        // wins over other digit runs (a folder name with a year).
        assert_eq!(
            parse_platform_id_input("[TVDB-00078801]", IdPlatform::Tvdb),
            Some(78801)
        );
        assert_eq!(
            parse_platform_id_input(
                "[7.9] [1991] [TVDB-00078801]《101次求婚》",
                IdPlatform::Tvdb
            ),
            Some(78801)
        );
        assert_eq!(parse_platform_id_input("bgm 64131", IdPlatform::Bangumi), Some(64131));
        assert_eq!(
            parse_platform_id_input("https://bgm.tv/subject/64131", IdPlatform::Bangumi),
            Some(64131)
        );
        // A bare number parses on any platform.
        assert_eq!(parse_platform_id_input("48455", IdPlatform::Tvdb), Some(48455));
        // Several runs and no anchor at all: ambiguous.
        assert_eq!(parse_platform_id_input("2024 115", IdPlatform::Bangumi), None);
    }

    fn temp_base(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "videotag-scan-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"").unwrap();
    }

    fn candidate(title: &str, original_title: &str, english_title: &str) -> Candidate {
        Candidate {
            id: 1,
            source: IdPlatform::Tmdb,
            media_type: MediaType::Tv,
            title: title.to_string(),
            original_title: original_title.to_string(),
            year: Some(2004),
            overview: String::new(),
            popularity: 0.0,
            vote_average: 0.0,
            vote_count: 0,
            english_title: english_title.to_string(),
        }
    }

    #[test]
    fn tv_show_title_prefers_chinese_when_present() {
        // TMDB's zh-CN names head the file name even though the
        // original titles are English.
        let cand = candidate("豪斯医生", "House", "House");
        assert_eq!(tv_file_show_title(&cand, Lang::Zh), "豪斯医生");
        let cand = candidate("绝命毒师", "Breaking Bad", "Breaking Bad");
        assert_eq!(tv_file_show_title(&cand, Lang::Zh), "绝命毒师");
    }

    #[test]
    fn tv_show_title_falls_back_when_no_chinese() {
        // No zh-CN translation: `title` fell back to the foreign
        // name, so the chain continues original → English → title.
        let cand = candidate("Original Name", "Original Name", "English Name");
        assert_eq!(tv_file_show_title(&cand, Lang::Zh), "Original Name");
        let cand = candidate("Foreign Title", "", "English Name");
        assert_eq!(tv_file_show_title(&cand, Lang::Zh), "English Name");
    }

    #[test]
    fn tv_show_title_english_mode() {
        // English naming: the en-US title first, then the original.
        let cand = candidate("House", "House", "House");
        assert_eq!(tv_file_show_title(&cand, Lang::En), "House");
        let cand = candidate("", "Breaking Bad", "");
        assert_eq!(tv_file_show_title(&cand, Lang::En), "Breaking Bad");
    }

    #[test]
    fn house_episode_file_name_uses_chinese_show_title() {
        // The user's House example under the Chinese-first rule,
        // assembled the way build_preview assembles it.
        let cand = candidate("豪斯医生", "House", "House");
        let wt = WorkTitle {
            title: tv_file_show_title(&cand, Lang::Zh).to_string(),
            ..Default::default()
        };
        let tags =
            videotag_core::extract_tech_tags("House.S01E01.1080p.x264.8bit.AAC 2ch 2.0.mkv");
        assert_eq!(tags, vec!["1080p", "x264", "8bit", "AAC", "2ch", "2.0"]);
        assert_eq!(
            videotag_core::build_tv_file_name(
                &wt,
                1,
                1,
                None,
                Some("人人都说谎"),
                Some("2004-11-16"),
                &tags,
                "mkv",
                NameLang::Zh,
            ),
            "豪斯医生 - S01E01 - 人人都说谎 (2004-11-16)-1080p-x264-8bit-AAC 2ch 2.0.mkv"
        );
    }

    #[test]
    fn preview_path_parts_split_name_and_dir() {
        let p = "[7.9] [1991] [TMDB-00048455]《101次求婚》(The 101st Proposal)/Season 01/101次求婚 - S01E01 - 第一乐章：宿命的相亲 (1991-07-01).mkv";
        assert_eq!(
            preview_file_name(p),
            "101次求婚 - S01E01 - 第一乐章：宿命的相亲 (1991-07-01).mkv"
        );
        assert_eq!(
            preview_dir(p),
            "[7.9] [1991] [TMDB-00048455]《101次求婚》(The 101st Proposal)/Season 01"
        );
        assert_eq!(
            save_to_line(p, &Settings::default(), Lang::Zh),
            "保存至：[7.9] [1991] [TMDB-00048455]《101次求婚》(The 101st Proposal)/Season 01"
        );
        assert_eq!(
            save_to_line(p, &Settings::default(), Lang::En),
            "Save to: [7.9] [1991] [TMDB-00048455]《101次求婚》(The 101st Proposal)/Season 01"
        );
        // A bare file name has no directory part.
        assert_eq!(preview_file_name("file.mkv"), "file.mkv");
        assert_eq!(preview_dir("file.mkv"), "");
    }

    fn movie_work() -> Work {
        let mut work = Work::new(
            videotag_core::parse_directory_name("Movie"),
            Method::Fallback,
            false,
        );
        let mut cand = candidate("片名", "Title", "Title");
        cand.media_type = MediaType::Movie;
        work.detail = Some(cand);
        work
    }

    #[test]
    fn preview_prefers_probed_tags_over_name_tags() {
        let base = temp_base("probe-pref");
        let file = base.join("Movie.2160p.WEB-DL.mkv");
        std::fs::write(&file, b"").unwrap();
        let mut row = make_row(file, &base, 0, &[]);
        // As if `ffmpeg -i` reported a 1080p AVC file: the probed
        // values win over the (wrong) ones in the file name.
        row.media_tags = Some(vec![
            "1080p".to_string(),
            "AVC".to_string(),
            "8bit".to_string(),
        ]);
        let preview = build_preview(&row, &movie_work(), &Settings::default(), Lang::Zh)
            .expect("a preview");
        assert!(preview.contains("-1080p-AVC-8bit"), "{preview}");
        assert!(!preview.contains("2160p"), "{preview}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn preview_falls_back_to_name_tags_without_probe() {
        let base = temp_base("probe-fallback");
        let file = base.join("Movie.2160p.WEB-DL.mkv");
        std::fs::write(&file, b"").unwrap();
        let row = make_row(file, &base, 0, &[]);
        assert!(row.media_tags.is_none());
        let preview = build_preview(&row, &movie_work(), &Settings::default(), Lang::Zh)
            .expect("a preview");
        assert!(preview.contains("2160p"), "{preview}");
        assert!(preview.contains("WEB-DL"), "{preview}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The user's case: a show's folder is added directly and its
    /// files are 01.mkv .. 12.mkv. One TV work named after the
    /// folder, episodes 1..12 in season 1 — not twelve "movies"
    /// named "01", "02", ...
    #[test]
    fn root_named_show_folder_groups_numbered_files() {
        let base = temp_base("hlm-root");
        let show = base.join("红楼梦");
        std::fs::create_dir_all(&show).unwrap();
        for ep in 1..=12 {
            touch(&show, &format!("{ep:02}.mkv"));
        }
        let (works, rows) = scan_root(&show);
        assert_eq!(works.len(), 1);
        assert!(works[0].is_tv);
        assert_eq!(works[0].parsed.title, "红楼梦");
        assert!(!works[0].unrecognized);
        assert_eq!(rows.len(), 12);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.parsed.episode, Some(i as u32 + 1));
            assert_eq!(row.parsed.season, Some(1));
            assert_eq!(row.parsed.title, "红楼梦");
        }
        assert_eq!(ep_code(&rows[0].parsed).as_deref(), Some("S01E01"));
        assert_eq!(ep_code(&rows[11].parsed).as_deref(), Some("S01E12"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Same grouping when the show sits in a subdirectory of the
    /// added root.
    #[test]
    fn show_subdirectory_groups_numbered_files() {
        let root = temp_base("hlm-sub");
        let show = root.join("红楼梦");
        std::fs::create_dir_all(&show).unwrap();
        for ep in 1..=3 {
            touch(&show, &format!("{ep:02}.mkv"));
        }
        let (works, rows) = scan_root(&root);
        assert_eq!(works.len(), 1);
        assert!(works[0].is_tv);
        assert_eq!(works[0].parsed.title, "红楼梦");
        assert_eq!(rows.len(), 3);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.parsed.episode, Some(i as u32 + 1));
            assert_eq!(row.parsed.season, Some(1));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Numbered files under a season directory keep that season.
    #[test]
    fn numbered_files_in_season_dir_keep_that_season() {
        let root = temp_base("hlm-season");
        let season = root.join("红楼梦").join("Season 2");
        std::fs::create_dir_all(&season).unwrap();
        touch(&season, "01.mkv");
        touch(&season, "02.mkv");
        let (works, rows) = scan_root(&root);
        assert_eq!(works.len(), 1);
        assert!(works[0].is_tv);
        assert_eq!(rows.len(), 2);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.parsed.season, Some(2));
            assert_eq!(row.parsed.episode, Some(i as u32 + 1));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The show's folder is the root and holds nothing but a
    /// season directory: the season directory is not a work, its
    /// files are episodes of the root show.
    #[test]
    fn season_dir_under_show_root_merges_into_root_work() {
        let base = temp_base("season-merge");
        let show = base.join("红楼梦");
        let season = show.join("Season 1");
        std::fs::create_dir_all(&season).unwrap();
        touch(&season, "01.mkv");
        touch(&season, "02.mkv");
        let (works, rows) = scan_root(&show);
        assert_eq!(works.len(), 1);
        assert!(works[0].is_tv);
        assert_eq!(works[0].parsed.title, "红楼梦");
        assert!(!works[0].unrecognized);
        assert_eq!(rows.len(), 2);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.work, 0);
            assert_eq!(row.parsed.season, Some(1));
            assert_eq!(row.parsed.episode, Some(i as u32 + 1));
            assert_eq!(row.parsed.title, "红楼梦");
        }
        assert_eq!(ep_code(&rows[0].parsed).as_deref(), Some("S01E01"));
        assert_eq!(ep_code(&rows[1].parsed).as_deref(), Some("S01E02"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Several season directories under the show root all merge
    /// into the same root work, each keeping its own season.
    #[test]
    fn multiple_season_dirs_merge_into_one_root_work() {
        let base = temp_base("season-multi");
        let show = base.join("红楼梦");
        let s1 = show.join("Season 1");
        let s2 = show.join("Season 2");
        std::fs::create_dir_all(&s1).unwrap();
        std::fs::create_dir_all(&s2).unwrap();
        touch(&s1, "01.mkv");
        touch(&s1, "02.mkv");
        touch(&s2, "01.mkv");
        let (works, rows) = scan_root(&show);
        assert_eq!(works.len(), 1);
        assert!(works[0].is_tv);
        assert_eq!(rows.len(), 3);
        let seasons: Vec<Option<u32>> =
            rows.iter().map(|r| r.parsed.season).collect();
        assert_eq!(seasons, vec![Some(1), Some(1), Some(2)]);
        let episodes: Vec<Option<u32>> =
            rows.iter().map(|r| r.parsed.episode).collect();
        assert_eq!(episodes, vec![Some(1), Some(2), Some(1)]);
        assert_eq!(ep_code(&rows[2].parsed).as_deref(), Some("S02E01"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Episode-named (non-numeric) files under a root season
    /// directory adopt the root work's identity and the
    /// directory's season too.
    #[test]
    fn episode_named_files_in_root_season_dir_adopt_identity() {
        let base = temp_base("season-epnamed");
        let show = base.join("101次求婚-TMDB48455");
        let season = show.join("Season 2");
        std::fs::create_dir_all(&season).unwrap();
        touch(&season, "E01.mkv");
        touch(&season, "E02.mkv");
        let (works, rows) = scan_root(&show);
        assert_eq!(works.len(), 1);
        assert_eq!(works[0].parsed.tmdb_id, Some(48455));
        assert_eq!(rows.len(), 2);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.parsed.season, Some(2));
            assert_eq!(row.parsed.episode, Some(i as u32 + 1));
            assert_eq!(row.parsed.title, "101次求婚");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// When the root name itself is meaningless and everything
    /// under it is season directories, the merged root work is
    /// still unrecognized — the structure is read, the title
    /// protection is not bypassed.
    #[test]
    fn season_dirs_under_meaningless_root_stay_unrecognized() {
        let base = temp_base("season-meaningless");
        let root = base.join("1");
        let season = root.join("Season 1");
        std::fs::create_dir_all(&season).unwrap();
        touch(&season, "01.mkv");
        touch(&season, "02.mkv");
        let (works, rows) = scan_root(&root);
        assert_eq!(works.len(), 1);
        assert!(works[0].unrecognized);
        assert_eq!(rows.len(), 2);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.parsed.season, Some(1));
            assert_eq!(row.parsed.episode, Some(i as u32 + 1));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// In a mixed loose folder only the numeric files join the
    /// group; a real movie file keeps its own work.
    #[test]
    fn mixed_loose_files_only_group_the_numbered_ones() {
        let root = temp_base("mixed");
        touch(&root, "01.mkv");
        touch(&root, "02.mkv");
        touch(&root, "Some Movie (2020).mkv");
        let (works, rows) = scan_root(&root);
        assert_eq!(works.len(), 2);
        assert_eq!(rows.len(), 3);
        let group = works.iter().position(|w| w.is_tv).expect("a TV group work");
        let episodes: Vec<Option<u32>> = rows
            .iter()
            .filter(|r| r.work == group)
            .map(|r| r.parsed.episode)
            .collect();
        assert_eq!(episodes, vec![Some(1), Some(2)]);
        let movie = rows
            .iter()
            .find(|r| r.work != group)
            .expect("the movie keeps its own row");
        assert_eq!(movie.parsed.title, "Some Movie");
        assert_eq!(movie.parsed.episode, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The user's second case: the show folder itself is the root
    /// and its name carries a TMDB id. The numbered files form one
    /// work whose identity (title + id) comes from the root name,
    /// so matching goes by id lookup.
    #[test]
    fn root_with_tmdb_id_groups_numbered_files() {
        let base = temp_base("id-numeric");
        let show = base.join("101次求婚-TMDB48455");
        std::fs::create_dir_all(&show).unwrap();
        for ep in 1..=3 {
            touch(&show, &format!("{ep:02}.mkv"));
        }
        let (works, rows) = scan_root(&show);
        assert_eq!(works.len(), 1);
        assert!(works[0].is_tv);
        assert_eq!(works[0].parsed.title, "101次求婚");
        assert_eq!(works[0].parsed.tmdb_id, Some(48455));
        assert!(!works[0].unrecognized);
        assert_eq!(rows.len(), 3);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.parsed.episode, Some(i as u32 + 1));
            assert_eq!(row.parsed.season, Some(1));
            assert_eq!(row.parsed.title, "101次求婚");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Same root, but the episodes are named E01/E02: they group
    /// into the same single work with the root's identity.
    #[test]
    fn root_with_tmdb_id_groups_episode_named_files() {
        let base = temp_base("id-epnamed");
        let show = base.join("101次求婚-TMDB48455");
        std::fs::create_dir_all(&show).unwrap();
        touch(&show, "E01.mkv");
        touch(&show, "E02.mkv");
        let (works, rows) = scan_root(&show);
        assert_eq!(works.len(), 1);
        assert!(works[0].is_tv);
        assert_eq!(works[0].parsed.title, "101次求婚");
        assert_eq!(works[0].parsed.tmdb_id, Some(48455));
        assert_eq!(rows.len(), 2);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.parsed.episode, Some(i as u32 + 1));
            assert_eq!(row.parsed.season, Some(1));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Episode-named files group under a plainly named root too.
    #[test]
    fn plain_root_groups_episode_named_files() {
        let base = temp_base("plain-epnamed");
        let show = base.join("红楼梦");
        std::fs::create_dir_all(&show).unwrap();
        touch(&show, "E01.mkv");
        touch(&show, "E02.mkv");
        let (works, rows) = scan_root(&show);
        assert_eq!(works.len(), 1);
        assert!(works[0].is_tv);
        assert_eq!(works[0].parsed.title, "红楼梦");
        assert_eq!(rows.len(), 2);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.parsed.episode, Some(i as u32 + 1));
            assert_eq!(row.parsed.season, Some(1));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A single episode file forms no group, but still inherits
    /// the identity of a root whose name carries a TMDB id.
    #[test]
    fn single_episode_file_inherits_id_root_identity() {
        let base = temp_base("id-single");
        let show = base.join("101次求婚-TMDB48455");
        std::fs::create_dir_all(&show).unwrap();
        touch(&show, "E01.mkv");
        let (works, rows) = scan_root(&show);
        assert_eq!(works.len(), 1);
        assert!(works[0].is_tv);
        assert_eq!(works[0].parsed.title, "101次求婚");
        assert_eq!(works[0].parsed.tmdb_id, Some(48455));
        assert!(!works[0].unrecognized);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].parsed.episode, Some(1));
        assert_eq!(rows[0].parsed.season, Some(1));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A single numeric file forms no group: it stays a standalone
    /// work, and its meaningless title marks it unrecognized so the
    /// matcher skips it instead of searching "01".
    #[test]
    fn lone_numeric_file_is_unrecognized_not_a_group() {
        let root = temp_base("lone");
        touch(&root, "01.mkv");
        let (works, rows) = scan_root(&root);
        assert_eq!(works.len(), 1);
        assert_eq!(rows.len(), 1);
        assert!(!works[0].is_tv);
        assert!(works[0].unrecognized);
        assert_eq!(rows[0].parsed.episode, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn roots_in_use_drops_emptied_roots() {
        let a = PathBuf::from("/lib/a");
        let b = PathBuf::from("/lib/b");
        let roots = vec![a.clone(), b.clone()];
        let rows = vec![bare_row(&a, "01.mkv", None)];
        assert_eq!(roots_in_use(&roots, &rows), vec![a]);
        // No rows left: every root is forgotten, so the same
        // directory can be added again afterwards.
        assert!(roots_in_use(&roots, &[]).is_empty());
    }

    fn bare_row(root: &Path, name: &str, preview: Option<&str>) -> Row {
        Row {
            path: root.join(name),
            root: root.to_path_buf(),
            work: 0,
            name: name.to_string(),
            parsed: videotag_core::parse_file_name(name),
            subtitles: Vec::new(),
            small: false,
            media_tags: None,
            checked: true,
            preview: preview.map(|p| p.to_string()),
            conflict: false,
            conflict_kind: None,
            ep_missing: false,
            done: false,
            exec_error: None,
        }
    }

    #[test]
    fn targets_follow_the_output_dir_setting() {
        let root = PathBuf::from("/lib/root");
        let row = bare_row(&root, "01.mkv", Some("Folder/Season 01/ep.mkv"));
        // No output folder: the target stays under the row's root.
        assert_eq!(
            row_target(&row, &Settings::default()),
            Some(PathBuf::from("/lib/root/Folder/Season 01/ep.mkv"))
        );
        // With one set, the same relative layout hangs under it.
        let settings = Settings {
            output_dir: Some(PathBuf::from("/nas/out")),
            ..Default::default()
        };
        assert_eq!(
            row_target(&row, &settings),
            Some(PathBuf::from("/nas/out/Folder/Season 01/ep.mkv"))
        );
        // No preview, no target.
        let row = bare_row(&root, "01.mkv", None);
        assert_eq!(row_target(&row, &Settings::default()), None);
    }

    #[test]
    fn save_to_line_roots_at_the_output_dir() {
        let p = "Folder/Season 01/ep.mkv";
        let settings = Settings {
            output_dir: Some(PathBuf::from("/nas/out")),
            ..Default::default()
        };
        assert_eq!(
            save_to_line(p, &settings, Lang::Zh),
            "保存至：/nas/out/Folder/Season 01"
        );
        assert_eq!(
            save_to_line(p, &settings, Lang::En),
            "Save to: /nas/out/Folder/Season 01"
        );
    }

    #[test]
    fn conflicts_flag_duplicate_targets() {
        let srcs = vec![PathBuf::from("/a/1.mkv"), PathBuf::from("/b/2.mkv")];
        let tgts = vec![
            PathBuf::from("/out/Folder/ep.mkv"),
            PathBuf::from("/out/Folder/ep.mkv"),
        ];
        assert_eq!(
            conflicting_kinds(&srcs, &tgts),
            vec![
                Some(ConflictKind::Duplicate),
                Some(ConflictKind::Duplicate)
            ]
        );
    }

    #[test]
    fn conflicts_flag_existing_files_but_not_own_source() {
        let base = temp_base("conflict");
        let taken = base.join("taken.mkv");
        std::fs::write(&taken, b"x").unwrap();
        let srcs = vec![base.join("src.mkv"), taken.clone()];
        // Another row aims at the occupied path: conflict. The
        // row whose target is its own current path: no conflict.
        let tgts = vec![taken.clone(), taken.clone()];
        // Both rows share one target, so the duplicate rule fires
        // for both and outranks the exists rule; check the
        // exists rule in isolation below.
        assert_eq!(
            conflicting_kinds(&srcs, &tgts),
            vec![
                Some(ConflictKind::Duplicate),
                Some(ConflictKind::Duplicate)
            ]
        );
        let kinds = conflicting_kinds(&[base.join("src.mkv")], &[taken.clone()]);
        assert_eq!(kinds, vec![Some(ConflictKind::TargetExists)]);
        let kinds = conflicting_kinds(&[taken.clone()], &[taken.clone()]);
        assert_eq!(kinds, vec![None]);
        let kinds = conflicting_kinds(
            &[base.join("src.mkv")],
            &[base.join("free.mkv")],
        );
        assert_eq!(kinds, vec![None]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn move_file_renames_within_a_directory() {
        let base = temp_base("move-rename");
        let src = base.join("a.mkv");
        let dst = base.join("b.mkv");
        std::fs::write(&src, b"video-bytes").unwrap();
        move_file(&src, &dst, Lang::Zh).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"video-bytes");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn move_file_refuses_to_overwrite() {
        let base = temp_base("move-clash");
        let src = base.join("a.mkv");
        let dst = base.join("b.mkv");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        assert!(move_file(&src, &dst, Lang::Zh).is_err());
        assert_eq!(std::fs::read(&src).unwrap(), b"new");
        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn move_file_same_path_is_a_no_op() {
        let base = temp_base("move-same");
        let file = base.join("a.mkv");
        std::fs::write(&file, b"data").unwrap();
        move_file(&file, &file, Lang::Zh).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"data");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn move_file_copies_across_filesystems() {
        // /dev/shm is a tmpfs on Linux while the temp dir sits on
        // the regular filesystem: a rename between them fails
        // with EXDEV, so the copy ladder has to take over. (If
        // they ever share a filesystem the rename path runs and
        // the same end state is asserted.)
        let shm = PathBuf::from("/dev/shm");
        if !shm.is_dir() {
            return;
        }
        let src_dir = shm.join(format!("videotag-xfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src_dir);
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("movie.mkv");
        std::fs::write(&src, b"cross-volume").unwrap();
        let base = temp_base("move-xfs");
        let dst = base.join("movie.mkv");
        move_file(&src, &dst, Lang::Zh).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"cross-volume");
        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn copy_landed_needs_both_lengths_equal() {
        assert!(copy_landed(Some(10), Some(10)));
        assert!(copy_landed(Some(0), Some(0)));
        assert!(!copy_landed(Some(10), Some(9)));
        assert!(!copy_landed(Some(10), None));
        assert!(!copy_landed(None, Some(10)));
        assert!(!copy_landed(None, None));
    }

    #[test]
    fn streamed_copy_copies_bytes_exactly() {
        let base = temp_base("streamed");
        let src = base.join("a.bin");
        let dst = base.join("sub").join("b.bin");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        // Bigger than one read for small buffers, odd tail length.
        let data: Vec<u8> = (0..100_003u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        streamed_copy(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), data);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn subtitle_targets_carry_the_language_suffix() {
        let video_src = PathBuf::from("/lib/Show/ep01.mkv");
        let video_dst = PathBuf::from("/out/Folder/Show - S01E01.mkv");
        assert_eq!(
            subtitle_target(
                &PathBuf::from("/lib/Show/ep01.chs.srt"),
                &video_src,
                &video_dst
            ),
            PathBuf::from("/out/Folder/Show - S01E01.chs.srt")
        );
        assert_eq!(
            subtitle_target(
                &PathBuf::from("/lib/Show/ep01.srt"),
                &video_src,
                &video_dst
            ),
            PathBuf::from("/out/Folder/Show - S01E01.srt")
        );
        assert_eq!(
            subtitle_target(
                &PathBuf::from("/lib/Show/ep01.zh.ass"),
                &video_src,
                &video_dst
            ),
            PathBuf::from("/out/Folder/Show - S01E01.zh.ass")
        );
    }

    #[test]
    fn emptied_dir_candidates_stop_below_the_root() {
        let root = PathBuf::from("/lib");
        let file = PathBuf::from("/lib/Work/Season 01/ep.mkv");
        assert_eq!(
            emptied_dir_candidates(&file, &root),
            vec![
                PathBuf::from("/lib/Work/Season 01"),
                PathBuf::from("/lib/Work")
            ]
        );
        // A loose file in the root itself leaves nothing behind.
        assert!(emptied_dir_candidates(&root.join("ep.mkv"), &root).is_empty());
    }

    #[test]
    fn execute_one_moves_video_and_subtitles() {
        let base = temp_base("exec-one");
        let src_dir = base.join("old");
        std::fs::create_dir_all(&src_dir).unwrap();
        let video = src_dir.join("ep01.mkv");
        let sub = src_dir.join("ep01.chs.srt");
        std::fs::write(&video, b"v").unwrap();
        std::fs::write(&sub, b"s").unwrap();
        let dst = base.join("New/Season 01/Show - S01E01.mkv");
        let job = ExecJob {
            src: video.clone(),
            video_dst: dst.clone(),
            subs: vec![(
                sub.clone(),
                subtitle_target(&sub, &video, &dst),
            )],
            cleanup_dirs: Vec::new(),
        };
        let result = execute_one(&job, Lang::Zh);
        assert!(matches!(result.outcome, ExecOutcome::Done));
        assert!(dst.exists());
        assert_eq!(
            result.subs_dst,
            vec![base.join("New/Season 01/Show - S01E01.chs.srt")]
        );
        assert!(result.subs_dst[0].exists());
        assert!(!video.exists());
        assert!(!sub.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn execute_one_fails_clean_when_the_target_is_taken() {
        let base = temp_base("exec-clash");
        let video = base.join("ep01.mkv");
        std::fs::write(&video, b"v").unwrap();
        let dst = base.join("taken.mkv");
        std::fs::write(&dst, b"old").unwrap();
        let job = ExecJob {
            src: video.clone(),
            video_dst: dst.clone(),
            subs: Vec::new(),
            cleanup_dirs: Vec::new(),
        };
        let result = execute_one(&job, Lang::Zh);
        assert!(matches!(result.outcome, ExecOutcome::Failed(_)));
        // Source and target both untouched.
        assert_eq!(std::fs::read(&video).unwrap(), b"v");
        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// About-window icon miniature: blue tile, film perforations, play mark.
fn about_icon(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(88.0, 88.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, egui::CornerRadius::same(20), egui::Color32::from_rgb(38, 92, 160));
    let w = egui::Color32::WHITE;
    for i in 0..5 {
        let x = rect.left() + 12.0 + i as f32 * 14.0;
        p.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x, rect.top() + 10.0), egui::pos2(x + 8.0, rect.top() + 18.0)),
            2.0,
            w,
        );
    }
    let c = rect.center();
    p.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(c.x - 12.0, c.y - 2.0),
            egui::pos2(c.x - 12.0, c.y + 30.0),
            egui::pos2(c.x + 16.0, c.y + 14.0),
        ],
        w,
        egui::Stroke::NONE,
    ));
}
