use std::path::Path;

/// A loaded media file ready to send.
pub enum Media {
    Image {
        data: Vec<u8>,
        mime: &'static str,
    },
    Audio {
        /// Raw i16 little-endian PCM bytes (mono).
        pcm: Vec<u8>,
        sample_rate: u32,
    },
}

/// Load a media file from disk and prepare it for sending.
pub fn load(path: &str) -> Result<Media, String> {
    let path = Path::new(path);
    if !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        // Images
        "jpg" | "jpeg" => load_image(path, "image/jpeg"),
        "png" => load_image(path, "image/png"),
        "gif" => load_image(path, "image/gif"),
        "webp" => load_image(path, "image/webp"),
        "bmp" => load_image(path, "image/bmp"),

        // Audio
        "wav" | "wave" => load_wav(path),
        "pcm" | "raw" => load_raw_pcm(path),

        _ => Err(format!("unsupported file type: .{ext}")),
    }
}

fn load_image(path: &Path, mime: &'static str) -> Result<Media, String> {
    let data = std::fs::read(path).map_err(|e| format!("read error: {e}"))?;
    Ok(Media::Image { data, mime })
}

fn load_wav(path: &Path) -> Result<Media, String> {
    let reader = hound::WavReader::open(path).map_err(|e| format!("wav error: {e}"))?;
    let spec = reader.spec();

    // Convert to mono i16 PCM bytes
    let pcm = match spec.sample_format {
        hound::SampleFormat::Int => {
            let samples: Vec<i16> = reader
                .into_samples::<i16>()
                .collect::<Result<_, _>>()
                .map_err(|e| format!("wav decode error: {e}"))?;
            to_mono_bytes(&samples, spec.channels)
        }
        hound::SampleFormat::Float => {
            let samples: Vec<i16> = reader
                .into_samples::<f32>()
                .map(|s| s.map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16))
                .collect::<Result<_, _>>()
                .map_err(|e| format!("wav decode error: {e}"))?;
            to_mono_bytes(&samples, spec.channels)
        }
    };

    Ok(Media::Audio {
        pcm,
        sample_rate: spec.sample_rate,
    })
}

fn load_raw_pcm(path: &Path) -> Result<Media, String> {
    let pcm = std::fs::read(path).map_err(|e| format!("read error: {e}"))?;
    // Assume 16kHz mono i16 LE
    Ok(Media::Audio {
        pcm,
        sample_rate: 16_000,
    })
}

/// Mix interleaved multi-channel i16 samples down to mono and return as LE bytes.
fn to_mono_bytes(samples: &[i16], channels: u16) -> Vec<u8> {
    let ch = channels as usize;
    let mono: Vec<i16> = if ch == 1 {
        samples.to_vec()
    } else {
        samples
            .chunks(ch)
            .map(|frame| {
                let sum: i32 = frame.iter().map(|&s| s as i32).sum();
                (sum / ch as i32) as i16
            })
            .collect()
    };

    mono.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Describe a loaded media for user display.
pub fn describe(path: &str, media: &Media) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);

    match media {
        Media::Image { data, mime } => {
            format!("[image] {name} ({}, {mime})", format_size(data.len()))
        }
        Media::Audio { pcm, sample_rate } => {
            let samples = pcm.len() / 2; // i16 = 2 bytes
            let duration = samples as f64 / *sample_rate as f64;
            format!(
                "[audio] {name} ({:.1}s, {} Hz, {})",
                duration,
                sample_rate,
                format_size(pcm.len()),
            )
        }
    }
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Parse a line of input into text portions and `@file` references.
///
/// Returns `(text, file_paths)` where `text` is the non-file content
/// joined with spaces, and `file_paths` are the paths (without `@`).
pub fn parse_input(line: &str) -> (String, Vec<String>) {
    let mut text_parts = Vec::new();
    let mut file_paths = Vec::new();

    for token in line.split_whitespace() {
        if let Some(path) = token.strip_prefix('@') {
            if !path.is_empty() {
                file_paths.push(path.to_string());
            } else {
                text_parts.push(token.to_string());
            }
        } else {
            text_parts.push(token.to_string());
        }
    }

    (text_parts.join(" "), file_paths)
}
