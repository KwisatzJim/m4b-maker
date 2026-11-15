use eframe::{App, Frame, egui};
use rfd::FileDialog;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

// Compare digit groups as whole numbers without parsing them into a fixed-size integer.
fn natural_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left = left.to_lowercase();
    let right = right.to_lowercase();
    let (mut a, mut b) = (left.as_bytes(), right.as_bytes());
    while !a.is_empty() && !b.is_empty() {
        let order = if a[0].is_ascii_digit() && b[0].is_ascii_digit() {
            let a_end = a
                .iter()
                .position(|c| !c.is_ascii_digit())
                .unwrap_or(a.len());
            let b_end = b
                .iter()
                .position(|c| !c.is_ascii_digit())
                .unwrap_or(b.len());
            let a_number = &a[a[..a_end].iter().position(|c| *c != b'0').unwrap_or(a_end)..a_end];
            let b_number = &b[b[..b_end].iter().position(|c| *c != b'0').unwrap_or(b_end)..b_end];
            let order = a_number
                .len()
                .cmp(&b_number.len())
                .then_with(|| a_number.cmp(b_number));
            a = &a[a_end..];
            b = &b[b_end..];
            order
        } else {
            let order = a[0].cmp(&b[0]);
            a = &a[1..];
            b = &b[1..];
            order
        };
        if !order.is_eq() {
            return order;
        }
    }
    a.len().cmp(&b.len())
}

fn sort_chapters(files: &mut [String]) {
    files.sort_by(|a, b| {
        let name = |path: &str| {
            Path::new(path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        natural_cmp(&name(a), &name(b))
    });
}

fn is_audio_file(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
        ["mp3", "m4a", "wav"]
            .iter()
            .any(|ext| s.eq_ignore_ascii_case(ext))
    })
}

fn dropped_audio_files(paths: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        let metadata = std::fs::metadata(path)?;
        let candidates = if metadata.is_dir() {
            folder_audio_files(path)?
        } else if metadata.is_file() && is_audio_file(path) {
            vec![path.clone()]
        } else {
            Vec::new()
        };
        for file in candidates {
            if seen.insert(file.canonicalize()?) {
                files.push(file);
            }
        }
    }
    Ok(files)
}

fn folder_audio_files(folder: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file() && is_audio_file(&path) {
            files.push(path);
        }
    }
    Ok(files)
}

fn escape_chapter_title(title: &str) -> String {
    title
        .replace('\\', "\\\\")
        .replace('=', "\\=")
        .replace(';', "\\;")
        .replace('#', "\\#")
        .replace('\r', "")
        .replace('\n', "\\\n")
}

fn cancellable_output(
    command: &mut Command,
    cancel: &AtomicBool,
) -> Result<std::process::Output, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let status = loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => thread::sleep(std::time::Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error.to_string());
            }
        }
    };
    let stdout = out
        .join()
        .map_err(|_| "Could not read probe output")?
        .map_err(|e| e.to_string())?;
    let stderr = err
        .join()
        .map_err(|_| "Could not read probe errors")?
        .map_err(|e| e.to_string())?;
    if cancel.load(Ordering::SeqCst) {
        return Err("Cancelled".into());
    }
    Ok(std::process::Output {
        status: status?,
        stdout,
        stderr,
    })
}

fn chapter_metadata(
    files: &[String],
    cancel: &AtomicBool,
) -> Result<(tempfile::NamedTempFile, u64), String> {
    let mut metadata = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    writeln!(metadata, ";FFMETADATA1").map_err(|e| e.to_string())?;
    let mut start = 0_u64;
    for file in files {
        if cancel.load(Ordering::SeqCst) {
            return Err("Cancelled".into());
        }
        let probe = cancellable_output(
            Command::new("ffprobe").args([
                "-v",
                "error",
                "-show_format",
                "-show_streams",
                "-of",
                "json",
                "-i",
                file,
            ]),
            cancel,
        )
        .map_err(|e| format!("Cannot read chapter information: {e}"))?;
        if !probe.status.success() {
            return Err(format!("Cannot read chapter information for {file}"));
        }
        let value: serde_json::Value =
            serde_json::from_slice(&probe.stdout).map_err(|e| e.to_string())?;
        let duration = value["format"]["duration"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|d| d.is_finite() && *d > 0.0 && *d < (u64::MAX / 1_000_000) as f64)
            .ok_or_else(|| format!("Cannot determine chapter duration for {file}"))?;
        let end = start
            .checked_add((duration * 1_000_000.0).round() as u64)
            .filter(|end| *end > start)
            .ok_or("Chapter duration is out of range")?;
        let tag_title = |tags: &serde_json::Value| -> Option<String> {
            tags.as_object()?.iter().find_map(|(key, value)| {
                if key.eq_ignore_ascii_case("title") {
                    value
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| s.trim().to_owned())
                } else {
                    None
                }
            })
        };
        let title = tag_title(&value["format"]["tags"])
            .or_else(|| {
                value["streams"]
                    .as_array()?
                    .iter()
                    .filter(|s| s["codec_type"] == "audio")
                    .find_map(|s| tag_title(&s["tags"]))
            })
            .unwrap_or_else(|| {
                Path::new(file)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        writeln!(
            metadata,
            "[CHAPTER]\nTIMEBASE=1/1000000\nSTART={start}\nEND={end}\ntitle={}",
            escape_chapter_title(&title)
        )
        .map_err(|e| e.to_string())?;
        start = end;
    }
    metadata.flush().map_err(|e| e.to_string())?;
    Ok((metadata, start))
}

fn remaining_time(total_us: u64, processed_us: Option<u64>, speed: &str) -> String {
    let Some(processed_us) = processed_us else {
        return "Estimating…".into();
    };
    let Some(speed) = speed
        .trim()
        .trim_end_matches('x')
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s > 0.0)
    else {
        return "Estimating…".into();
    };
    let seconds = (total_us.saturating_sub(processed_us) as f64 / 1_000_000.0 / speed).ceil();
    if !seconds.is_finite() || seconds >= u64::MAX as f64 {
        return "Estimating…".into();
    }
    let seconds = seconds as u64;
    format!(
        "~{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[derive(Clone)]
struct Artwork {
    path: String,
    stream: usize,
}

fn cover_preview(cover: &Artwork) -> Result<egui::ColorImage, String> {
    const SIDE: usize = 180;
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i", &cover.path, "-map", &format!("0:{}", cover.stream),
            "-frames:v", "1", "-vf",
            "format=rgba,scale=180:180:force_original_aspect_ratio=decrease,pad=180:180:(ow-iw)/2:(oh-ih)/2:color=black@0",
            "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
        .output().map_err(|e| format!("Could not start cover preview: {e}"))?;
    if !output.status.success() || output.stdout.len() != SIDE * SIDE * 4 {
        return Err("Could not preview this cover. Choose another image or check Conversion details after exporting.".into());
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [SIDE, SIDE],
        &output.stdout,
    ))
}

#[derive(Default)]
struct BookInfo {
    title: String,
    author: String,
    artwork: Option<Artwork>,
}

fn filename_info(path: &str) -> BookInfo {
    let stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    // Interpret supported filename pairs as Author - Title or Author,Title.
    // Prefer spaced dashes so commas inside a title do not override that convention.
    for separator in [" - ", " – ", " — ", ","] {
        if let Some((author, title)) = stem.split_once(separator)
            && !author.trim().is_empty()
            && !title.trim().is_empty()
            && !author.trim().chars().all(|c| c.is_ascii_digit())
        {
            return BookInfo {
                title: title.trim().to_owned(),
                author: author.trim().to_owned(),
                artwork: None,
            };
        }
    }
    BookInfo::default()
}

fn detect_book_info(files: &[String]) -> BookInfo {
    let mut info = BookInfo::default();
    for file in files {
        if let Ok(output) = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_format",
                "-show_streams",
                "-of",
                "json",
                "-i",
                file,
            ])
            .output()
            && output.status.success()
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        {
            let mut tags = Vec::new();
            if let Some(t) = value.get("format").and_then(|v| v.get("tags")) {
                tags.push(t);
            }
            if let Some(streams) = value.get("streams").and_then(|v| v.as_array()) {
                for stream in streams {
                    if info.artwork.is_none()
                        && stream["disposition"]["attached_pic"].as_u64() == Some(1)
                        && let Some(index) = stream["index"].as_u64()
                    {
                        info.artwork = Some(Artwork {
                            path: file.clone(),
                            stream: index as usize,
                        });
                    }
                    if let Some(t) = stream.get("tags") {
                        tags.push(t);
                    }
                }
            }
            let find = |keys: &[&str]| -> String {
                for key in keys {
                    for tag in &tags {
                        if let Some(map) = tag.as_object() {
                            for (name, value) in map {
                                if name.eq_ignore_ascii_case(key)
                                    && let Some(text) =
                                        value.as_str().filter(|s| !s.trim().is_empty())
                                {
                                    return text.trim().to_owned();
                                }
                            }
                        }
                    }
                }
                String::new()
            };
            if info.title.is_empty() {
                info.title = find(&["album"]);
            }
            if info.author.is_empty() {
                info.author = find(&["author", "album_artist", "albumartist", "artist"]);
            }
            // A single file can be a complete book; multi-file titles are often chapter names.
            if files.len() == 1 && info.title.is_empty() {
                info.title = find(&["title"]);
            }
        }
        if !info.title.is_empty() && !info.author.is_empty() && info.artwork.is_some() {
            break;
        }
    }
    for file in files {
        let fallback = filename_info(file);
        if info.title.is_empty() {
            info.title = fallback.title;
        }
        if info.author.is_empty() {
            info.author = fallback.author;
        }
        if !info.title.is_empty() && !info.author.is_empty() {
            break;
        }
    }
    info
}

// Store only dialog locations, independently of the selected book.
#[derive(Default)]
struct FolderMemory {
    input: Option<PathBuf>,
    output: Option<PathBuf>,
}

impl FolderMemory {
    fn config_path() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        let base =
            std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
        #[cfg(target_os = "windows")]
        let base = std::env::var_os("APPDATA").map(PathBuf::from);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
        base.map(|p| p.join("m4b-maker/folders.json"))
    }

    fn load() -> Self {
        Self::config_path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    fn load_from(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .map(|v| Self {
                input: v["input"].as_str().map(PathBuf::from),
                output: v["output"].as_str().map(PathBuf::from),
            })
            .unwrap_or_default()
    }

    fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path()
            .ok_or_else(|| std::io::Error::other("Settings location unavailable"))?;
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let data = serde_json::json!({ "input": self.input, "output": self.output });
        // Write and rename together so an interrupted save cannot truncate settings.
        let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        temp.write_all(data.to_string().as_bytes())?;
        temp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
}

fn output_filename(title: &str) -> String {
    let name: String = title
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim().trim_end_matches(['.', ' ']);
    if name.is_empty() {
        "audiobook.m4b".into()
    } else {
        format!("{name}.m4b")
    }
}

pub struct M4bApp {
    pub files: Vec<String>,
    pub dark_mode: bool,
    pub title: String,
    pub author: String,
    pub ffmpeg_output: Arc<Mutex<String>>,
    export_running: Arc<AtomicBool>,
    cancel_export: Arc<AtomicBool>,
    detected_info: Option<Receiver<BookInfo>>,
    progress: Arc<Mutex<String>>,
    artwork: Option<Artwork>,
    folders: FolderMemory,
    cover_texture: Option<egui::TextureHandle>,
    pending_preview: Option<Receiver<Result<egui::ColorImage, String>>>,
    preview_error: Option<String>,
    selection_error: Option<String>,
}

impl Default for M4bApp {
    fn default() -> Self {
        Self {
            artwork: None,
            cover_texture: None,
            pending_preview: None,
            preview_error: None,
            selection_error: None,
            folders: FolderMemory::load(),
            files: vec![],
            dark_mode: true,
            title: String::new(),
            author: String::new(),
            ffmpeg_output: Arc::new(Mutex::new(String::new())),
            export_running: Arc::new(AtomicBool::new(false)),
            cancel_export: Arc::new(AtomicBool::new(false)),
            detected_info: None,
            progress: Arc::new(Mutex::new(String::from("Ready"))),
        }
    }
}

impl M4bApp {
    fn accept_drop(&mut self, paths: &[PathBuf]) {
        if self.export_running.load(Ordering::SeqCst) {
            self.selection_error = Some("Wait for the current conversion to finish or cancel it before dropping another book.".into());
            return;
        }
        match dropped_audio_files(paths) {
            Ok(files) if !files.is_empty() => {
                let folder = paths.first().and_then(|p| if p.is_dir() { Some(p.clone()) } else { p.parent().map(Path::to_path_buf) });
                self.select_audio(files, folder);
            }
            Ok(_) => self.selection_error = Some("Drop MP3, M4A, or WAV files, or a folder containing them. Subfolders are not included.".into()),
            Err(error) => self.selection_error = Some(format!("Could not read dropped files: {error}")),
        }
    }

    fn select_audio(&mut self, files: Vec<PathBuf>, input_folder: Option<PathBuf>) {
        self.selection_error = None;
        self.folders.input = input_folder;
        self.save_folders();
        self.files = files.iter().map(|p| p.display().to_string()).collect();
        sort_chapters(&mut self.files);
        self.set_artwork(None);
        let files = self.files.clone();
        let (sender, receiver) = mpsc::channel();
        self.detected_info = Some(receiver);
        thread::spawn(move || {
            let _ = sender.send(detect_book_info(&files));
        });
    }

    fn new_book(&mut self) {
        if self.export_running.load(Ordering::SeqCst) {
            return;
        }
        self.files.clear();
        self.title.clear();
        self.author.clear();
        self.selection_error = None;
        // Drop receivers so late detection results cannot refill the cleared fields.
        self.detected_info = None;
        self.set_artwork(None);
        self.cancel_export.store(false, Ordering::SeqCst);
        if let Ok(mut log) = self.ffmpeg_output.lock() {
            log.clear();
        }
        if let Ok(mut status) = self.progress.lock() {
            *status = "Ready".into();
        }
    }

    fn set_artwork(&mut self, artwork: Option<Artwork>) {
        self.artwork = artwork;
        self.cover_texture = None;
        self.preview_error = None;
        // Replacing the receiver discards results belonging to an older cover.
        self.pending_preview = self.artwork.as_ref().map(|cover| {
            let cover = cover.clone();
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let _ = sender.send(cover_preview(&cover));
            });
            receiver
        });
    }

    fn save_folders(&self) {
        if let Err(error) = self.folders.save()
            && let Ok(mut log) = self.ffmpeg_output.lock()
        {
            log.push_str(&format!("Could not remember folders: {error}\n"));
        }
    }

    fn generate_m4b(&mut self, output_path: &str, ctx: &egui::Context) {
        if self.export_running.load(Ordering::SeqCst) {
            return;
        }

        // Clear previous output
        if let Ok(mut log) = self.ffmpeg_output.lock() {
            *log = "🔄 Generating m4b...\n".to_string();
        }
        ctx.request_repaint();

        if let Ok(mut progress) = self.progress.lock() {
            *progress = "Starting conversion…".into();
        }

        if self.files.is_empty() {
            if let Ok(mut log) = self.ffmpeg_output.lock() {
                *log = "⚠ No input files selected.".to_string();
            }
            return;
        }

        // Create the temp file for ffmpeg concat list
        let mut tmp_file = match tempfile::NamedTempFile::new() {
            Ok(f) => f,
            Err(e) => {
                if let Ok(mut log) = self.ffmpeg_output.lock() {
                    *log = format!("❌ Failed to create temp file: {e}");
                }
                return;
            }
        };

        let input_list = format!("Input files: {}\n", self.files.len());
        for file in &self.files {
            // FFmpeg requires closing the quote, escaping the apostrophe,
            // then reopening the quote around the rest of the path.
            let escaped_file = file.replace('\'', "'\\''");
            if let Err(e) = writeln!(tmp_file, "file '{}'", escaped_file) {
                if let Ok(mut log) = self.ffmpeg_output.lock() {
                    *log = format!("❌ Failed to write to temp file: {e}");
                }
                return;
            }
        }

        // Close the file and move its cleanup owner into the export worker.
        // The list stays available to FFmpeg and is deleted when the worker exits.
        let concat_path = tmp_file.into_temp_path();

        let ffmpeg_output_clone = Arc::clone(&self.ffmpeg_output);
        let ctx_clone = ctx.clone();
        let artwork = self.artwork.clone();
        let files = self.files.clone();
        let title = self.title.clone();
        let author = self.author.clone();
        let output_path = output_path.to_string();
        let export_running = Arc::clone(&self.export_running);
        let progress = Arc::clone(&self.progress);
        let cancel = Arc::clone(&self.cancel_export);
        cancel.store(false, Ordering::SeqCst);
        export_running.store(true, Ordering::SeqCst);

        thread::spawn(move || {
            if let Ok(mut status) = progress.lock() {
                *status = "Preparing chapters…".into();
            }
            ctx_clone.request_repaint();
            // Keep the temporary chapter file alive until FFmpeg has finished.
            let (chapters, total_us) = match chapter_metadata(&files, &cancel) {
                Ok(chapters) => chapters,
                Err(error) => {
                    if let Ok(mut log) = ffmpeg_output_clone.lock() {
                        log.push_str(&format!("Chapter preparation failed: {error}\n"));
                    }
                    if let Ok(mut status) = progress.lock() {
                        *status = if cancel.load(Ordering::SeqCst) {
                            "Conversion cancelled".into()
                        } else {
                            "Could not prepare chapters — see details below".into()
                        };
                    }
                    export_running.store(false, Ordering::SeqCst);
                    ctx_clone.request_repaint();
                    return;
                }
            };
            // Write beside the destination, then publish only after successful completion.
            let destination = Path::new(&output_path);
            let parent = destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let staged = match tempfile::Builder::new()
                .prefix(".m4b-maker-")
                .suffix(".m4b")
                .tempfile_in(parent)
            {
                Ok(file) => file.into_temp_path(),
                Err(error) => {
                    if let Ok(mut log) = ffmpeg_output_clone.lock() {
                        log.push_str(&format!("Cannot create output: {error}\n"));
                    }
                    if let Ok(mut status) = progress.lock() {
                        *status = "Could not create output — see details below".into();
                    }
                    export_running.store(false, Ordering::SeqCst);
                    ctx_clone.request_repaint();
                    return;
                }
            };
            let mut args = vec![
                "-y".to_string(),
                "-nostdin".into(),
                "-hide_banner".to_string(),
                "-nostats".to_string(),
                "-loglevel".to_string(),
                "warning".to_string(),
                "-f".to_string(),
                "concat".to_string(),
                "-safe".to_string(),
                "0".to_string(),
                "-i".to_string(),
                concat_path.to_string_lossy().to_string(),
                "-f".into(),
                "ffmetadata".into(),
                "-i".into(),
                chapters.path().to_string_lossy().into_owned(),
            ];

            // Explicit mapping keeps only audio and the selected cover.
            if let Some(cover) = artwork {
                args.extend([
                    "-i".into(),
                    cover.path,
                    "-map".into(),
                    "0:a:0".into(),
                    "-map".into(),
                    format!("2:{}", cover.stream),
                    "-c:v".into(),
                    "mjpeg".into(),
                    "-disposition:v:0".into(),
                    "attached_pic".into(),
                ]);
            } else {
                args.extend(["-map".into(), "0:a:0".into()]);
            }

            args.extend([
                "-map_chapters".into(),
                "1".into(),
                // Avoid chapter timestamp overflow on long audiobooks.
                "-movie_timescale".into(),
                "1000".into(),
                "-c:a".to_string(),
                "aac".to_string(),
                "-b:a".to_string(),
                "128k".to_string(),
                "-movflags".to_string(),
                "+faststart".to_string(),
                "-progress".to_string(),
                "-".to_string(), // output progress to stdout
            ]);

            if !title.trim().is_empty() {
                args.push("-metadata".to_string());
                args.push(format!("title={}", title));
            }

            if !author.trim().is_empty() {
                args.push("-metadata".to_string());
                args.push(format!("artist={}", author));
            }

            args.push(staged.to_string_lossy().into_owned());

            let mut child = match Command::new("ffmpeg")
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    if let Ok(mut log) = ffmpeg_output_clone.lock() {
                        *log = format!("❌ Failed to launch ffmpeg: {e}");
                    }
                    export_running.store(false, Ordering::SeqCst);
                    if let Ok(mut status) = progress.lock() {
                        *status = "Conversion could not start".into();
                    }
                    ctx_clone.request_repaint();
                    return;
                }
            };

            if let Ok(mut status) = progress.lock() {
                *status = "Converting • Time remaining: Estimating…".into();
            }
            ctx_clone.request_repaint();
            let stdout = BufReader::new(child.stdout.take().unwrap());
            let stderr = BufReader::new(child.stderr.take().unwrap());

            let output_log = Arc::clone(&ffmpeg_output_clone);
            let repaint_ctx = ctx_clone.clone();

            // Handle stderr in a separate thread
            let stderr_thread = thread::spawn(move || {
                for line in stderr.lines().map_while(Result::ok) {
                    if let Ok(mut log) = output_log.lock() {
                        log.push_str(&line);
                        log.push('\n');
                        // Retain only recent diagnostics, even during a noisy conversion.
                        while log.len() > 16_384 {
                            let end = log.find('\n').map(|i| i + 1).unwrap_or(log.len());
                            log.drain(..end);
                        }
                    }
                    repaint_ctx.request_repaint();
                }
            });

            // FFmpeg emits key=value progress records. Update a single status line.
            let progress_for_reader = Arc::clone(&progress);
            let repaint = ctx_clone.clone();
            let stdout_thread = thread::spawn(move || {
                let mut elapsed = String::from("00:00:00");
                let mut speed = String::new();
                let mut processed_us = None;
                for line in stdout.lines().map_while(Result::ok) {
                    if let Some((key, value)) = line.split_once('=') {
                        match key {
                            "out_time_us" => {
                                processed_us = value.parse::<i64>().ok().map(|us| us.max(0) as u64)
                            }
                            "out_time" => {
                                elapsed = value.split('.').next().unwrap_or(value).to_owned()
                            }
                            "speed" => speed = value.trim().to_owned(),
                            "progress" => {
                                if let Ok(mut status) = progress_for_reader.lock() {
                                    *status = if value == "end"
                                        || processed_us.is_some_and(|us| us >= total_us)
                                    {
                                        "Finalizing audiobook…".into()
                                    } else {
                                        format!(
                                            "Converting • Audio processed: {elapsed} • Speed: {speed} • Time remaining: {}",
                                            remaining_time(total_us, processed_us, &speed)
                                        )
                                    };
                                }
                                repaint.request_repaint();
                            }
                            _ => {}
                        }
                    }
                }
            });
            let exit_code = loop {
                if cancel.load(Ordering::SeqCst) {
                    let _ = child.kill();
                }
                match child.try_wait() {
                    Ok(Some(status)) => break status.code().unwrap_or(-1),
                    Ok(None) => thread::sleep(std::time::Duration::from_millis(50)),
                    Err(_) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break -1;
                    }
                }
            };
            let _ = stdout_thread.join();
            // Finish logging before allowing another export to clear the log.
            let _ = stderr_thread.join();

            let message = if cancel.load(Ordering::SeqCst) {
                "Conversion cancelled".to_owned()
            } else if exit_code != 0 {
                format!("Conversion failed (code {exit_code}) — see details below")
            } else {
                match staged.persist(destination) {
                    Ok(()) => "Conversion complete".to_owned(),
                    Err(error) => format!("Could not save completed audiobook: {}", error.error),
                }
            };
            if let Ok(mut log) = ffmpeg_output_clone.lock() {
                if message == "Conversion complete" {
                    log.push_str("✅ FFmpeg finished successfully.\n");
                } else {
                    log.push_str(&format!("{message}\n"));
                }
            }
            if let Ok(mut status) = progress.lock() {
                *status = message;
            }
            export_running.store(false, Ordering::SeqCst);
            ctx_clone.request_repaint();
        });

        if let Ok(mut log) = self.ffmpeg_output.lock() {
            log.push_str(&input_list);
        }
        ctx.request_repaint();
    }
}

impl App for M4bApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        let dropped = ctx.input(|input| input.raw.dropped_files.clone());
        if !dropped.is_empty() {
            let paths: Vec<_> = dropped
                .iter()
                .filter_map(|file| file.path.clone())
                .collect();
            if paths.is_empty() {
                self.selection_error =
                    Some("Drop local audio files or folders from your file manager.".into());
            } else {
                self.accept_drop(&paths);
            }
        }
        if let Some(receiver) = &self.detected_info {
            match receiver.try_recv() {
                Ok(info) => {
                    if self.title.trim().is_empty() {
                        self.title = info.title;
                    }
                    if self.author.trim().is_empty() {
                        self.author = info.author;
                    }
                    if self.artwork.is_none() {
                        self.set_artwork(info.artwork);
                    }
                    self.detected_info = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.detected_info = None,
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
            }
        }
        if let Some(receiver) = &self.pending_preview {
            match receiver.try_recv() {
                Ok(result) => {
                    match result {
                        Ok(image) => {
                            self.cover_texture = Some(ctx.load_texture(
                                "book_cover",
                                image,
                                egui::TextureOptions::LINEAR,
                            ))
                        }
                        Err(error) => self.preview_error = Some(error),
                    }
                    self.pending_preview = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.preview_error = Some("Cover preview could not finish.".into());
                    self.pending_preview = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100))
                }
            }
        }
        ctx.set_visuals(if self.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });

        let running = self.export_running.load(Ordering::SeqCst);
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("M4B Maker");
                if ui
                    .add_enabled(!running, egui::Button::new("New book"))
                    .clicked()
                {
                    self.new_book();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.dark_mode, "Dark mode");
                });
            });
            ui.label("Turn audio files into an audiobook");
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("main_content")
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                    ui.add_enabled_ui(!running, |ui| {
                ui.heading("Audio files");
                if ctx.input(|input| !input.raw.hovered_files.is_empty()) {
                    ui.colored_label(ui.visuals().selection.stroke.color, "Release to load audio files or folders");
                } else {
                    ui.small("Drag audio files or a folder into this window, or select them below. A new selection replaces the file list.");
                }
                ui.horizontal(|ui| {
                    let mut choose_folder = None;
                    ui.menu_button("Select files…", |ui| {
                        if ui.button("Individual audio files…").clicked() { choose_folder = Some(false); ui.close(); }
                        if ui.button("Audio folder…").clicked() { choose_folder = Some(true); ui.close(); }
                    });
                    if let Some(folder_mode) = choose_folder {
                        let mut dialog = FileDialog::new().add_filter("Audio", &["mp3", "m4a", "wav"]);
                        if let Some(folder) = &self.folders.input { dialog = dialog.set_directory(folder); }
                        if folder_mode {
                            if let Some(folder) = dialog.pick_folder() {
                                match folder_audio_files(&folder) {
                                    Ok(files) if !files.is_empty() => self.select_audio(files, Some(folder)),
                                    Ok(_) => self.selection_error = Some("This folder contains no MP3, M4A, or WAV files. Subfolders are not included.".into()),
                                    Err(error) => self.selection_error = Some(format!("Could not read the folder: {error}")),
                                }
                            }
                        } else if let Some(files) = dialog.pick_files() {
                            let folder = files.first().and_then(|p| p.parent()).map(Path::to_path_buf);
                            self.select_audio(files, folder);
                        }
                    }
                    ui.label(format!("{} files selected", self.files.len()));
                });
                if let Some(error) = &self.selection_error { ui.colored_label(ui.visuals().error_fg_color, error); }
                ui.small("Files are sorted by name with chapter numbers in numeric order. Expand below to adjust.");
                ui.collapsing("Selected files (export order)", |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("files")
                        .max_height(100.0)
                        .show(ui, |ui| {
                            let mut movement = None;
                            let count = self.files.len();
                            for (i, file) in self.files.iter().enumerate() {
                                let name = Path::new(file)
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy();
                                ui.horizontal(|ui| {
                                    if ui.add_enabled(i > 0, egui::Button::new("Up")).clicked() {
                                        movement = Some((i, i - 1));
                                    }
                                    if ui.add_enabled(i + 1 < count, egui::Button::new("Down")).clicked() {
                                        movement = Some((i, i + 1));
                                    }
                                    ui.label(format!("{}. {name}", i + 1)).on_hover_text(file);
                                });
                            }
                            if let Some((from, to)) = movement { self.files.swap(from, to); }
                            if ui.add_enabled(count > 1, egui::Button::new("Sort by chapter number")).clicked() {
                                sort_chapters(&mut self.files);
                            }

                        });
                });
                ui.separator();
                ui.heading("Book information");
                egui::Grid::new("book_fields")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("Title");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.title)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                        ui.label("Author");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.author)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                    });
                if self.detected_info.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Checking book information and artwork…");
                    });
                }
                ui.small("Empty fields use audio tags, then Author - Title or Author,Title filenames.");
                ui.separator();
                ui.heading("Cover artwork");
                if let Some(texture) = &self.cover_texture {
                    ui.image((texture.id(), egui::vec2(180.0, 180.0)));
                } else if self.pending_preview.is_some() {
                    ui.horizontal(|ui| { ui.spinner(); ui.label("Loading cover preview…"); });
                } else if let Some(error) = &self.preview_error {
                    ui.label(error);
                }
                if let Some(cover) = &self.artwork {
                    let name = Path::new(&cover.path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    ui.label(format!("Cover from: {name}"));
                } else {
                    ui.label(
                        "No cover selected. Embedded artwork is detected from the audio files.",
                    );
                }
                ui.horizontal(|ui| {
                    if ui.button("Choose cover…").clicked()
                        && let Some(path) = FileDialog::new()
                            .add_filter("Cover image", &["jpg", "jpeg", "png"])
                            .pick_file()
                    {
                        self.set_artwork(Some(Artwork {
                            path: path.display().to_string(),
                            stream: 0,
                        }));
                    }
                    if ui
                        .add_enabled(
                            self.artwork.is_some() && self.detected_info.is_none(),
                            egui::Button::new("Remove cover"),
                        )
                        .clicked()
                    {
                        self.set_artwork(None);
                    }
                });
                ui.separator();
                if ui
                    .add_enabled(
                        !self.files.is_empty() && self.detected_info.is_none(),
                        egui::Button::new("Export audiobook…"),
                    )
                    .clicked()
                {
                    let mut dialog = FileDialog::new()
                        .set_file_name(output_filename(&self.title))
                        .add_filter("Audiobook", &["m4b"]);
                    if let Some(folder) = &self.folders.output {
                        dialog = dialog.set_directory(folder);
                    }
                    if let Some(output) = dialog.save_file() {
                        self.folders.output = output.parent().map(Path::to_path_buf);
                        self.save_folders();
                        self.generate_m4b(&output.display().to_string(), ctx);
                    }
                }
            });
                    ui.separator();
                    ui.heading("Conversion");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if self.export_running.load(Ordering::SeqCst) {
                            ui.spinner();
                        }
                        if let Ok(status) = self.progress.lock() {
                            ui.label(status.as_str());
                        }
                    });
                    if running {
                        let cancelling = self.cancel_export.load(Ordering::SeqCst);
                        if ui.add_enabled(!cancelling, egui::Button::new(if cancelling { "Cancelling…" } else { "Cancel conversion" })).clicked() {
                            self.cancel_export.store(true, Ordering::SeqCst);
                        }
                    }
                    ui.add_space(12.0);

                    ui.collapsing("Conversion details", |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                if let Ok(log) = self.ffmpeg_output.lock() {
                                    ui.add(
                                        egui::TextEdit::multiline(&mut log.clone())
                                            .desired_width(f32::INFINITY)
                                            .lock_focus(true)
                                            .font(egui::TextStyle::Monospace),
                                    );
                                }
                            });
                    });
                });
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_drag_and_drop(true)
            .with_inner_size([720.0, 700.0])
            .with_min_inner_size([560.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "M4B Maker",
        options,
        Box::new(|_cc| Ok(Box::new(M4bApp::default()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_files_and_folders_filter_and_deduplicate_audio() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("Chapter 2.MP3");
        let cover = dir.path().join("cover.jpg");
        std::fs::write(&audio, []).unwrap();
        std::fs::write(&cover, []).unwrap();
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("nested.mp3"), []).unwrap();
        let files = dropped_audio_files(&[
            audio.clone(),
            dir.path().to_path_buf(),
            cover.clone(),
            audio.clone(),
        ])
        .unwrap();
        assert_eq!(files, vec![audio]);
        assert!(dropped_audio_files(&[cover]).unwrap().is_empty());
        assert!(dropped_audio_files(&[dir.path().join("missing.mp3")]).is_err());
    }

    #[test]
    fn dropping_during_export_preserves_current_book() {
        let mut app = M4bApp {
            files: vec!["original.mp3".into()],
            title: "Current book".into(),
            ..Default::default()
        };
        app.export_running.store(true, Ordering::SeqCst);
        app.accept_drop(&[PathBuf::from("another.mp3")]);
        assert_eq!(app.files, ["original.mp3"]);
        assert_eq!(app.title, "Current book");
        assert!(app.selection_error.is_some());
    }

    #[test]
    fn comma_separated_filenames_supply_author_and_title() {
        for filename in [
            "Jane Austen,Pride and Prejudice.mp3",
            "Jane Austen, Pride and Prejudice.mp3",
        ] {
            let info = filename_info(filename);
            assert_eq!(info.author, "Jane Austen");
            assert_eq!(info.title, "Pride and Prejudice");
        }
        let info = filename_info("Jane Austen - Book title, with a comma.mp3");
        assert_eq!(info.author, "Jane Austen");
        assert_eq!(info.title, "Book title, with a comma");
        assert!(filename_info("01,Chapter One.mp3").author.is_empty());
        assert!(filename_info(",Missing Author.mp3").author.is_empty());
        assert!(filename_info("Missing Title,.mp3").title.is_empty());
    }

    #[test]
    fn eta_uses_remaining_audio_and_conversion_speed() {
        assert_eq!(
            remaining_time(7_200_000_000, Some(3_600_000_000), "2.0x"),
            "~00:30:00"
        );
        assert_eq!(
            remaining_time(7_200_000_000, Some(3_600_000_000), "0.5x"),
            "~02:00:00"
        );
        assert_eq!(
            remaining_time(1_000_000, Some(2_000_000), "1x"),
            "~00:00:00"
        );
        assert_eq!(remaining_time(1_000_000, Some(0), "3x"), "~00:00:01");
        for speed in ["N/A", "0x", "-1x", "NaNx", "infx"] {
            assert_eq!(remaining_time(1_000_000, Some(0), speed), "Estimating…");
        }
        assert_eq!(remaining_time(1_000_000, None, "2x"), "Estimating…");
    }

    #[test]
    fn folder_selection_includes_only_supported_files_directly_inside() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "Chapter 10.mp3",
            "Chapter 2.MP3",
            "Chapter 1.m4a",
            "bonus.wav",
            "cover.jpg",
            "notes.txt",
        ] {
            std::fs::write(dir.path().join(name), []).unwrap();
        }
        let nested = dir.path().join("subfolder.mp3");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("nested.mp3"), []).unwrap();
        let mut files = folder_audio_files(dir.path())
            .unwrap()
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>();
        sort_chapters(&mut files);
        let names: Vec<_> = files
            .iter()
            .map(|p| Path::new(p).file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "bonus.wav",
                "Chapter 1.m4a",
                "Chapter 2.MP3",
                "Chapter 10.mp3"
            ]
        );
        assert!(folder_audio_files(&dir.path().join("missing")).is_err());
        assert!(folder_audio_files(&nested).unwrap().len() == 1);
        let empty = tempfile::tempdir().unwrap();
        assert!(folder_audio_files(empty.path()).unwrap().is_empty());
    }

    #[test]
    fn new_book_clears_book_state_and_discards_late_results() {
        let (sender, receiver) = mpsc::channel();
        let mut app = M4bApp {
            files: vec!["chapter.mp3".into()],
            title: "Old title".into(),
            author: "Old author".into(),
            artwork: Some(Artwork {
                path: "cover.png".into(),
                stream: 0,
            }),
            detected_info: Some(receiver),
            dark_mode: false,
            folders: FolderMemory {
                input: Some("/input".into()),
                output: Some("/output".into()),
            },
            ..Default::default()
        };
        *app.progress.lock().unwrap() = "Conversion complete".into();
        app.ffmpeg_output.lock().unwrap().push_str("old log");
        app.cancel_export.store(true, Ordering::SeqCst);
        app.new_book();
        assert!(app.files.is_empty() && app.title.is_empty() && app.author.is_empty());
        assert!(app.artwork.is_none() && app.detected_info.is_none());
        assert!(app.cover_texture.is_none() && app.pending_preview.is_none());
        assert!(!app.cancel_export.load(Ordering::SeqCst));
        assert_eq!(*app.progress.lock().unwrap(), "Ready");
        assert!(app.ffmpeg_output.lock().unwrap().is_empty());
        assert_eq!(app.folders.input, Some(PathBuf::from("/input")));
        assert_eq!(app.folders.output, Some(PathBuf::from("/output")));
        assert!(!app.dark_mode);
        assert!(sender.send(BookInfo::default()).is_err());
    }

    #[test]
    fn new_book_does_not_reset_an_active_export() {
        let mut app = M4bApp {
            title: "In progress".into(),
            ..Default::default()
        };
        app.export_running.store(true, Ordering::SeqCst);
        app.new_book();
        assert_eq!(app.title, "In progress");
        assert!(app.export_running.load(Ordering::SeqCst));
    }

    #[test]
    #[ignore = "requires ffmpeg and ffprobe on PATH"]
    fn cancelling_export_preserves_destination_and_removes_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("long.wav");
        let destination = dir.path().join("book.m4b");
        std::fs::write(&destination, b"existing audiobook").unwrap();
        assert!(
            Command::new("ffmpeg")
                .args(["-v", "error", "-f", "lavfi", "-i", "anullsrc", "-t", "300"])
                .arg(&input)
                .status()
                .unwrap()
                .success()
        );
        let input_size = std::fs::metadata(&input).unwrap().len();
        let mut app = M4bApp {
            files: vec![input.to_string_lossy().into_owned()],
            ..Default::default()
        };
        app.generate_m4b(destination.to_str().unwrap(), &egui::Context::default());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        // Wait for actual encoder progress, so this exercises cancellation of FFmpeg itself.
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "Encoder did not start"
            );
            let status = app.progress.lock().unwrap().clone();
            if status.starts_with("Converting") {
                break;
            }
            assert!(
                app.export_running.load(Ordering::SeqCst),
                "Export ended early: {status}"
            );
            thread::sleep(std::time::Duration::from_millis(1));
        }
        app.cancel_export.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.export_running.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "Cancellation did not finish"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(*app.progress.lock().unwrap(), "Conversion cancelled");
        assert_eq!(std::fs::read(&destination).unwrap(), b"existing audiobook");
        assert_eq!(std::fs::metadata(&input).unwrap().len(), input_size);
        // Cleanup occurs as the worker returns, just after it releases the running flag.
        while std::fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".m4b-maker-")
        }) {
            assert!(
                std::time::Instant::now() < deadline,
                "Partial output was not removed"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn chapter_preparation_honors_cancellation_before_probing() {
        let error = chapter_metadata(&["unused.mp3".into()], &AtomicBool::new(true)).unwrap_err();
        assert_eq!(error, "Cancelled");
    }

    #[test]
    fn removing_cover_discards_pending_preview() {
        let (sender, receiver) = mpsc::channel();
        let mut app = M4bApp {
            pending_preview: Some(receiver),
            preview_error: Some("old error".into()),
            ..Default::default()
        };
        app.set_artwork(None);
        assert!(app.artwork.is_none());
        assert!(app.pending_preview.is_none());
        assert!(app.cover_texture.is_none());
        assert!(app.preview_error.is_none());
        assert!(sender.send(Err("stale preview".into())).is_err());
    }

    #[test]
    #[ignore = "requires ffprobe on PATH"]
    fn chapter_preparation_rejects_unreadable_input() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("invalid.mp3");
        std::fs::write(&bad, "This is not an audio file").unwrap();
        assert!(
            chapter_metadata(
                &[bad.to_string_lossy().into_owned()],
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn chapters_sort_numerically_and_ignore_folder_names() {
        let mut files = vec![
            "/a/Chapter 10.mp3",
            "/z/Chapter 2.mp3",
            "/b/chapter 1.mp3",
            "/a/Chapter 02b.mp3",
            "/a/Chapter 2a.mp3",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
        sort_chapters(&mut files);
        assert_eq!(
            files,
            [
                "/b/chapter 1.mp3",
                "/z/Chapter 2.mp3",
                "/a/Chapter 2a.mp3",
                "/a/Chapter 02b.mp3",
                "/a/Chapter 10.mp3"
            ]
        );
        assert!(natural_cmp("Disc 2 Track 9", "Disc 2 Track 10").is_lt());
        assert!(
            natural_cmp(
                "Chapter 999999999999999999999999",
                "Chapter 1000000000000000000000000"
            )
            .is_lt()
        );
        assert!(natural_cmp("Chapter 01", "chapter 1").is_eq());
    }

    #[test]
    fn title_becomes_a_safe_output_filename() {
        assert_eq!(
            output_filename(" Pride and Prejudice "),
            "Pride and Prejudice.m4b"
        );
        assert_eq!(output_filename("Book: Part/Two"), "Book_ Part_Two.m4b");
        assert_eq!(output_filename("..."), "audiobook.m4b");
    }

    #[test]
    fn folders_survive_reloading_independently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings/folders.json");
        let mut folders = FolderMemory {
            input: Some(dir.path().join("input")),
            output: Some(dir.path().join("output")),
        };
        folders.save_to(&path).unwrap();
        folders.input = Some(dir.path().join("different input"));
        folders.save_to(&path).unwrap();
        let loaded = FolderMemory::load_from(&path);
        assert_eq!(loaded.input, folders.input);
        assert_eq!(loaded.output, folders.output);
    }

    #[test]
    #[ignore = "requires ffmpeg and ffprobe on PATH"]
    fn exports_audio_with_embedded_or_selected_cover() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("cover.png");
        let audio = dir.path().join("book.mp3");
        let run = |args: &[&str], path: &Path| {
            let output = Command::new("ffmpeg")
                .args(["-v", "error", "-y"])
                .args(args)
                .arg(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=64x64",
                "-frames:v",
                "1",
            ],
            &image,
        );
        run(
            &[
                "-f",
                "lavfi",
                "-i",
                "anullsrc",
                "-i",
                image.to_str().unwrap(),
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:v",
                "copy",
                "-disposition:v",
                "attached_pic",
                "-metadata",
                "title=Opening = #1; \\ notes\nSecond line",
                "-t",
                "0.2",
            ],
            &audio,
        );
        let second_audio = dir.path().join("chapter two.mp3");
        run(
            &["-f", "lavfi", "-i", "anullsrc", "-t", "0.2"],
            &second_audio,
        );
        let files = vec![
            audio.to_string_lossy().into_owned(),
            second_audio.to_string_lossy().into_owned(),
        ];
        let duration_probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "json",
            ])
            .arg(&audio)
            .output()
            .unwrap();
        let duration_value: serde_json::Value =
            serde_json::from_slice(&duration_probe.stdout).unwrap();
        let expected_boundary = duration_value["format"]["duration"]
            .as_str()
            .unwrap()
            .parse::<f64>()
            .unwrap();
        let detected = detect_book_info(&files);
        assert!(detected.artwork.is_some());
        for (name, artwork) in [
            ("embedded", detected.artwork),
            (
                "selected",
                Some(Artwork {
                    path: image.to_string_lossy().into_owned(),
                    stream: 0,
                }),
            ),
            ("none", None),
        ] {
            if let Some(cover) = &artwork {
                let preview = cover_preview(cover).unwrap();
                assert_eq!(preview.size, [180, 180]);
                let center = preview.pixels[90 * 180 + 90];
                assert!(center.b() > 200 && center.r() < 30 && center.g() < 30);
            }
            let output = dir.path().join(format!("{name}.m4b"));
            let mut app = M4bApp {
                files: files.clone(),
                artwork,
                title: "Test Book".into(),
                author: "Test Author".into(),
                ..Default::default()
            };
            app.generate_m4b(output.to_str().unwrap(), &egui::Context::default());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while app.export_running.load(Ordering::SeqCst) {
                assert!(std::time::Instant::now() < deadline, "Export timed out");
                thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(
                app.ffmpeg_output.lock().unwrap().contains("successfully"),
                "{}",
                app.ffmpeg_output.lock().unwrap()
            );
            let probe = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_streams",
                    "-show_format",
                    "-show_chapters",
                    "-of",
                    "json",
                ])
                .arg(&output)
                .output()
                .unwrap();
            assert!(probe.status.success());
            let value: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
            assert_eq!(value["format"]["tags"]["title"], "Test Book");
            assert_eq!(value["format"]["tags"]["artist"], "Test Author");
            let chapters = value["chapters"].as_array().unwrap();
            assert_eq!(chapters.len(), 2);
            assert_eq!(chapters[0]["time_base"], "1/1000");
            assert!(
                value["streams"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["codec_name"] == "bin_data"),
                "QuickTime chapter track is missing"
            );
            assert_eq!(
                chapters[0]["tags"]["title"],
                "Opening = #1; \\ notes\nSecond line"
            );
            assert_eq!(chapters[1]["tags"]["title"], "chapter two");
            assert_eq!(chapters[0]["start_time"], "0.000000");
            assert_eq!(chapters[0]["end_time"], chapters[1]["start_time"]);
            let boundary = chapters[1]["start_time"]
                .as_str()
                .unwrap()
                .parse::<f64>()
                .unwrap();
            assert!(
                (boundary - expected_boundary).abs() < 0.001,
                "Unexpected boundary: {boundary}"
            );

            let streams = value["streams"].as_array().unwrap();
            assert!(streams.iter().any(|s| s["codec_name"] == "aac"));
            assert_eq!(
                streams
                    .iter()
                    .any(|s| s["disposition"]["attached_pic"] == 1),
                name != "none"
            );
        }
    }

    #[test]
    fn filename_requires_recognizable_author_title_pair() {
        let info = filename_info("/books/Jane Austen - Pride and Prejudice.mp3");
        assert_eq!(info.author, "Jane Austen");
        assert_eq!(info.title, "Pride and Prejudice");
        assert!(filename_info("01 - Chapter One.mp3").author.is_empty());
        assert!(filename_info("Chapter One.mp3").title.is_empty());
    }

    #[test]
    #[ignore = "requires ffmpeg and ffprobe on PATH"]
    fn audio_tags_take_priority_and_missing_tags_use_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Filename Author - Filename Title.mp3");
        let create = |tags: &[&str]| {
            assert!(
                Command::new("ffmpeg")
                    .args([
                        "-v", "error", "-y", "-f", "lavfi", "-i", "anullsrc", "-t", "0.1"
                    ])
                    .args(tags)
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        let files = vec![path.to_string_lossy().into_owned()];
        create(&[
            "-metadata",
            "album=Tagged Book",
            "-metadata",
            "artist=Tagged Author",
            "-metadata",
            "title=Chapter One",
        ]);
        let info = detect_book_info(&files);
        assert_eq!(info.title, "Tagged Book");
        assert_eq!(info.author, "Tagged Author");
        create(&["-metadata", "album=Tagged Book"]);
        let info = detect_book_info(&files);
        assert_eq!(info.title, "Tagged Book");
        assert_eq!(info.author, "Filename Author");
        create(&[]);
        let info = detect_book_info(&files);
        assert_eq!(info.title, "Filename Title");
        assert_eq!(info.author, "Filename Author");
    }
}
