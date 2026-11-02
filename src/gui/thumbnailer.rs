//! Previews of the files that are not images, rendered by the freedesktop thumbnailers of the
//! system (`*/thumbnailers/*.thumbnailer`, the same files Nautilus uses: papers for PDF and
//! DjVu, ffmpegthumbnailer for video, gdk-pixbuf for SVG, …). The thumbnailer is asked for a
//! big image, not a thumbnail, and is killed when it takes too long or when the user has
//! already moved on to another file.

use {
    gtk4::{
        gdk::Texture,
        gio::{self, prelude::FileExt, File},
        glib,
    },
    std::{
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::{
            atomic::{AtomicU64, Ordering},
            OnceLock,
        },
        time::{Duration, Instant},
    },
};

const TIMEOUT: Duration = Duration::from_secs(15);

struct Thumbnailer {
    exec: Vec<String>,
    mime_types: Vec<String>,
}

impl Thumbnailer {
    fn handles(&self, content_type: &str) -> bool {
        self.mime_types
            .iter()
            .any(|m| m == content_type || gio::content_type_is_a(content_type, m))
    }
}

/// The installed thumbnailers, user ones first, then those of each data dir, in file name
/// order within a dir. Those whose `TryExec` is missing are left out.
fn thumbnailers() -> &'static [Thumbnailer] {
    static ALL: OnceLock<Vec<Thumbnailer>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut dirs = vec![glib::user_data_dir()];
        dirs.extend(glib::system_data_dirs());
        let mut all = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir.join("thumbnailers")) else {
                continue;
            };
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "thumbnailer"))
                .collect();
            files.sort();
            all.extend(files.iter().filter_map(|p| parse(p)));
        }
        log::debug!("{} thumbnailers found", all.len());
        all
    })
}

fn parse(path: &Path) -> Option<Thumbnailer> {
    let key_file = glib::KeyFile::new();
    key_file
        .load_from_file(path, glib::KeyFileFlags::NONE)
        .ok()?;
    let group = "Thumbnailer Entry";
    if let Ok(try_exec) = key_file.string(group, "TryExec") {
        let program = Path::new(try_exec.as_str());
        let found = if program.is_absolute() {
            program.is_file()
        } else {
            glib::find_program_in_path(program).is_some()
        };
        if !found {
            return None;
        }
    }
    let exec = key_file.string(group, "Exec").ok()?;
    let exec: Vec<String> = glib::shell_parse_argv(exec.as_str())
        .ok()?
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if exec.is_empty() {
        return None;
    }
    let mime_types = key_file
        .string_list(group, "MimeType")
        .ok()?
        .iter()
        .map(|m| m.to_string())
        .filter(|m| !m.is_empty())
        .collect();
    Some(Thumbnailer { exec, mime_types })
}

/// Renders `file` with the first thumbnailer that handles `content_type`, `size` pixels on the
/// longer side. None when there is no such thumbnailer, when it fails or takes longer than
/// `TIMEOUT`, or when `generation` moved past `id` meanwhile (another file was asked for).
pub fn render(
    file: &File,
    content_type: &str,
    size: i32,
    generation: &AtomicU64,
    id: u64,
) -> Option<Texture> {
    let thumbnailer = thumbnailers().iter().find(|t| t.handles(content_type))?;
    let dir = temp_dir()?;
    let output = dir.join("preview.png");
    let args = thumbnailer.exec.iter().try_fold(Vec::new(), |mut args, arg| {
        args.push(expand(arg, file, size, &output)?);
        Some(args)
    });
    let texture = args.and_then(|args| run(&args, &output, generation, id));
    let _ = std::fs::remove_file(&output);
    let _ = std::fs::remove_dir(&dir);
    texture
}

/// The `%` fields of the Exec line: `%s` size, `%u` URI, `%i` path, `%o` output file.
fn expand(arg: &str, file: &File, size: i32, output: &Path) -> Option<String> {
    let mut out = String::with_capacity(arg.len());
    let mut chars = arg.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push_str(&size.to_string()),
            Some('u') => out.push_str(file.uri().as_str()),
            // A thumbnailer that wants a path cannot render a remote file.
            Some('i') => out.push_str(&file.path()?.to_string_lossy()),
            Some('o') => out.push_str(&output.to_string_lossy()),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    Some(out)
}

fn run(args: &[String], output: &Path, generation: &AtomicU64, id: u64) -> Option<Texture> {
    let (program, rest) = args.split_first()?;
    if generation.load(Ordering::SeqCst) != id {
        return None;
    }
    let mut child = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .inspect_err(|e| log::debug!("{program}: {e}"))
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => {
                log::debug!("{program} failed: {status}");
                return None;
            }
            Ok(None) => {
                if started.elapsed() > TIMEOUT || generation.load(Ordering::SeqCst) != id {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
    Texture::from_filename(output)
        .inspect_err(|e| log::debug!("{}: {e}", output.display()))
        .ok()
}

/// A fresh private directory for the output file; `create_dir` fails on an existing one, so a
/// directory someone else prepared is never reused.
fn temp_dir() -> Option<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    for _ in 0..16 {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = base.join(format!(
            "xdg-desktop-portal-gtk4-quick-look-{}-{n}-{:x}",
            std::process::id(),
            rand::random::<u32>()
        ));
        if std::fs::create_dir(&dir).is_ok() {
            return Some(dir);
        }
    }
    None
}
