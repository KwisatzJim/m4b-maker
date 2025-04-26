use eframe::{egui, App, Frame};
use rfd::FileDialog;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct M4bApp {
    pub files: Vec<String>,
    pub dark_mode: bool,
    pub title: String,
    pub author: String,
    pub ffmpeg_output: Arc<Mutex<String>>,
}

impl Default for M4bApp {
    fn default() -> Self {
        Self {
            files: vec![],
            dark_mode: true,
            title: String::new(),
            author: String::new(),
            ffmpeg_output: Arc::new(Mutex::new(String::new())),
        }
    }
}

impl M4bApp {
    fn generate_m4b(&mut self, output_path: &str, ctx: &egui::Context) {
        // Clear previous output
        if let Ok(mut log) = self.ffmpeg_output.lock() {
            *log = "🔄 Generating m4b...\n".to_string();
        }
        ctx.request_repaint();

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

        let mut input_list = String::from("🔍 FFmpeg input list:\n");
        for file in &self.files {
            if let Err(e) = writeln!(tmp_file, "file '{}'", file) {
                if let Ok(mut log) = self.ffmpeg_output.lock() {
                    *log = format!("❌ Failed to write to temp file: {e}");
                }
                return;
            }
            input_list += &format!("file '{}'\n", file);
        }

        // Persist file and path
        let concat_path: PathBuf = match tmp_file.keep() {
            Ok((_, path)) => path,
            Err(e) => {
                if let Ok(mut log) = self.ffmpeg_output.lock() {
                    *log = format!("❌ Failed to keep temp file: {e}");
                }
                return;
            }
        };

        let ffmpeg_output_clone = Arc::clone(&self.ffmpeg_output);
        let ctx_clone = ctx.clone();
        let title = self.title.clone();
        let author = self.author.clone();
        let output_path = output_path.to_string();

        thread::spawn(move || {
            let mut args = vec![
                "-y".to_string(),
                "-f".to_string(),
                "concat".to_string(),
                "-safe".to_string(),
                "0".to_string(),
                "-i".to_string(),
                concat_path.to_string_lossy().to_string(),
                "-c:a".to_string(),
                "aac".to_string(),
                "-b:a".to_string(),
                "128k".to_string(),
                "-movflags".to_string(),
                "+faststart".to_string(),
                "-progress".to_string(), 
                "-".to_string(), // output progress to stdout
            ];

            if !title.trim().is_empty() {
                args.push("-metadata".to_string());
                args.push(format!("title={}", title));
            }

            if !author.trim().is_empty() {
                args.push("-metadata".to_string());
                args.push(format!("artist={}", author));
            }

            args.push(output_path);

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
                    return;
                }
            };

            let stdout = BufReader::new(child.stdout.take().unwrap());
            let stderr = BufReader::new(child.stderr.take().unwrap());

            let output_log = Arc::clone(&ffmpeg_output_clone);
            let repaint_ctx = ctx_clone.clone();

            // Handle stderr in a separate thread
            thread::spawn(move || {
                for line in stderr.lines() {
                    if let Ok(line) = line {
                        if let Ok(mut log) = output_log.lock() {
                            log.push_str(&line);
                            log.push('\n');
                        }
                        repaint_ctx.request_repaint();
                    }
                }
            });

            // Read stdout here (usually nothing from ffmpeg, but included just in case)
            for line in stdout.lines() {
                if let Ok(line) = line {
                    if let Ok(mut log) = ffmpeg_output_clone.lock() {
                        log.push_str(&line);
                        log.push('\n');
                    }
                    ctx_clone.request_repaint();
                }
            }

            let exit_code = match child.wait() {
                Ok(status) => status.code().unwrap_or(-1),
                Err(_) => -1,
            };

            if let Ok(mut log) = ffmpeg_output_clone.lock() {
                if exit_code == 0 {
                    log.push_str("✅ FFmpeg finished successfully.\n");
                } else {
                    log.push_str(&format!("❌ FFmpeg failed with code {exit_code}\n"));
                }
            }

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
        ctx.set_visuals(if self.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("🎧 M4B Maker");

                if ui.button("🎚 Toggle Theme").clicked() {
                    self.dark_mode = !self.dark_mode;
                }

                if ui.button("📁 Select Files").clicked() {
                    if let Some(files) = FileDialog::new()
                        .add_filter("Audio", &["mp3", "m4a", "wav"])
                        .pick_files()
                    {
                        self.files = files.iter().map(|p| p.display().to_string()).collect();
                    }
                }

                if ui.button("💾 Export to .m4b").clicked() {
                    if let Some(output) = FileDialog::new()
                        .set_file_name("audiobook.m4b")
                        .save_file()
                    {
                        self.generate_m4b(&output.display().to_string(), ctx);
                    }
                }
            });

            ui.horizontal(|ui| {
                ui.label("📖 Title:");
                ui.text_edit_singleline(&mut self.title);
                ui.label("👤 Author:");
                ui.text_edit_singleline(&mut self.author);
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label("📜 FFmpeg Output:");

            egui::ScrollArea::vertical()
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
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions::default();
    eframe::run_native("M4B Maker", options, Box::new(|_cc| Box::new(M4bApp::default())))
}
