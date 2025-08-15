use crate::{config::Config, scan::AudioMetadata};

use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct MetadataKey {
    artist: String,
    album: String,
    title: String,
    track: Option<u16>,
}

#[derive(Debug)]
pub struct OrganizeResult {
    pub moved: usize,
    pub skipped: usize,
    pub failed: usize,
    pub duplicates: usize,
}

pub fn organize_music_files(
    music_files: &[(PathBuf, AudioMetadata)],
    output_dir: &PathBuf,
    config: &Config,
) -> Result<OrganizeResult, Box<dyn std::error::Error>> {
    if music_files.is_empty() {
        return Ok(OrganizeResult {
            moved: 0,
            skipped: 0,
            failed: 0,
            duplicates: 0,
        });
    }

    let start_time = Instant::now();

    let pb = Arc::new(ProgressBar::new(music_files.len() as u64));
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  [{bar:40.cyan/blue}] {pos}/{len} [{elapsed_precise}] {msg}")?
            .progress_chars("█▉▊▋▌▍▎▏  "),
    );

    let moved = Arc::new(Mutex::new(0));
    let skipped = Arc::new(Mutex::new(0));
    let failed = Arc::new(Mutex::new(0));
    let duplicates = Arc::new(Mutex::new(0));

    let used_metadata: Arc<Mutex<HashMap<MetadataKey, PathBuf>>> =
        Arc::new(Mutex::new(HashMap::new()));

    if let Ok(existing_files) = crate::scan::scan_for_music(output_dir) {
        let mut metadata_map = used_metadata.lock().unwrap();
        for (path, metadata) in existing_files {
            if let Some(metadata_key) = create_metadata_key(&metadata) {
                metadata_map.insert(metadata_key, path);
            }
        }
    }

    music_files.par_iter().for_each(|(source_path, metadata)| {
        if let Some(filename) = source_path.file_name() {
            pb.set_message(filename.to_string_lossy().to_string());
        }

        match organize_single_file(source_path, metadata, output_dir, config, &used_metadata) {
            Ok(result) => match result {
                FileResult::Moved => {
                    *moved.lock().unwrap() += 1;
                }
                FileResult::Skipped => {
                    *skipped.lock().unwrap() += 1;
                }
                FileResult::Duplicate => {
                    *duplicates.lock().unwrap() += 1;
                }
            },
            Err(_) => {
                *failed.lock().unwrap() += 1;
            }
        }

        pb.inc(1);
    });

    pb.finish_and_clear();

    let duration = start_time.elapsed();
    let result = OrganizeResult {
        moved: *moved.lock().unwrap(),
        skipped: *skipped.lock().unwrap(),
        failed: *failed.lock().unwrap(),
        duplicates: *duplicates.lock().unwrap(),
    };

    println!(
        "  {} files copied in {:.2}s",
        result.moved,
        duration.as_secs_f64()
    );
    if result.skipped > 0 {
        println!("  {} files skipped", result.skipped);
    }
    if result.duplicates > 0 {
        println!("  {} duplicates handled", result.duplicates);
    }
    if result.failed > 0 {
        println!("  {} files failed", result.failed);
    }

    Ok(result)
}

#[derive(Debug)]
enum FileResult {
    Moved,
    Skipped,
    Duplicate,
}

fn organize_single_file(
    source_path: &PathBuf,
    metadata: &AudioMetadata,
    output_dir: &PathBuf,
    config: &Config,
    used_metadata: &Arc<Mutex<HashMap<MetadataKey, PathBuf>>>,
) -> Result<FileResult, Box<dyn std::error::Error>> {
    let relative_path = match generate_target_path(source_path, metadata, config) {
        Some(path) => path,
        None => {
            if config.rules.handle_missing_metadata == "skip" {
                return Ok(FileResult::Skipped);
            } else {
                generate_fallback_path(source_path, config)
            }
        }
    };

    let target_path = output_dir.join(&relative_path);

    let final_target_path = {
        let metadata_key = create_metadata_key(metadata);
        let mut metadata_map = used_metadata.lock().unwrap();

        if let Some(metadata_key) = metadata_key {
            if metadata_map.contains_key(&metadata_key) {
                match config.rules.handle_duplicates.as_str() {
                    "skip" => {
                        return Ok(FileResult::Duplicate);
                    }
                    "rename" => {
                        handle_duplicate_rename(&target_path, &metadata_key, &mut metadata_map)
                    }
                    "overwrite" => {
                        if let Some(old_path) = metadata_map.get(&metadata_key) {
                            let _ = fs::remove_file(old_path);
                        }
                        metadata_map.insert(metadata_key, source_path.clone());
                        target_path
                    }
                    _ => target_path,
                }
            } else {
                metadata_map.insert(metadata_key, source_path.clone());
                target_path
            }
        } else {
            let fallback_key = MetadataKey {
                artist: "Unknown".to_string(),
                album: "Unknown".to_string(),
                title: source_path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                track: None,
            };

            if metadata_map.contains_key(&fallback_key) {
                match config.rules.handle_duplicates.as_str() {
                    "skip" => return Ok(FileResult::Duplicate),
                    "rename" => {
                        handle_duplicate_rename(&target_path, &fallback_key, &mut metadata_map)
                    }
                    _ => target_path,
                }
            } else {
                metadata_map.insert(fallback_key, source_path.clone());
                target_path
            }
        }
    };

    if let Some(parent) = final_target_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::copy(source_path, &final_target_path)?;

    Ok(FileResult::Moved)
}

fn generate_target_path(
    source_path: &PathBuf,
    metadata: &AudioMetadata,
    config: &Config,
) -> Option<PathBuf> {
    let structure = if is_compilation(metadata) {
        config
            .organization
            .compilation_structure
            .as_ref()
            .unwrap_or(&config.organization.structure)
    } else {
        &config.organization.structure
    };

    let path_str = replace_placeholders(structure, source_path, metadata, config)?;
    let sanitized_path = sanitize_path(&path_str, config);
    Some(PathBuf::from(sanitized_path))
}

fn generate_fallback_path(source_path: &PathBuf, config: &Config) -> PathBuf {
    let filename = source_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();

    let fallback_str = config
        .organization
        .fallback_structure
        .replace("{filename}", &filename);

    let sanitized_path = sanitize_path(&fallback_str, config);
    PathBuf::from(sanitized_path)
}

fn replace_placeholders(
    template: &str,
    source_path: &PathBuf,
    metadata: &AudioMetadata,
    config: &Config,
) -> Option<String> {
    let mut result = template.to_string();

    if template.contains("{artist}") {
        if is_compilation(metadata) {
            if let Some(artist) = &metadata.artist {
                result = result.replace("{artist}", &sanitize_metadata_value(artist, config));
            } else {
                return None;
            }
        } else if let Some(album_artist) = &metadata.album_artist {
            result = result.replace("{artist}", &sanitize_metadata_value(album_artist, config));
        } else if let Some(artist) = &metadata.artist {
            result = result.replace("{artist}", &sanitize_metadata_value(artist, config));
        } else {
            return None;
        }
    }

    if let Some(title) = &metadata.title {
        result = result.replace("{title}", &sanitize_metadata_value(title, config));
    } else if template.contains("{title}") {
        return None;
    }

    if let Some(album) = &metadata.album {
        result = result.replace("{album}", &sanitize_metadata_value(album, config));
    } else if template.contains("{album}") {
        return None;
    }

    if let Some(year) = metadata.year {
        result = result.replace("{year}", &year.to_string());
    } else if template.contains("{year}") {
        return None;
    }

    if template.contains("{track") {
        if let Some(track) = metadata.track {
            if let Some(start) = template.find("{track") {
                if let Some(end) = template[start..].find('}') {
                    let full_placeholder = &template[start..start + end + 1];
                    if full_placeholder.contains(':') {
                        let format_part = &full_placeholder[7..full_placeholder.len() - 1];
                        if format_part == "02" {
                            result = result.replace(full_placeholder, &format!("{:02}", track));
                        } else {
                            result = result.replace(full_placeholder, &track.to_string());
                        }
                    } else {
                        result = result.replace("{track}", &track.to_string());
                    }
                }
            }
        } else if template.contains("{track") {
            return None;
        }
    }

    if let Some(genre) = &metadata.genre {
        result = result.replace("{genre}", &sanitize_metadata_value(genre, config));
    } else if template.contains("{genre}") {
        return None;
    }

    if template.contains("{filename}") {
        let filename = source_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        result = result.replace("{filename}", &filename);
    }

    if let Some(extension) = source_path.extension() {
        if !result.ends_with(&format!(".{}", extension.to_string_lossy())) {
            result = format!("{}.{}", result, extension.to_string_lossy());
        }
    }

    Some(result)
}

fn sanitize_path(path: &str, config: &Config) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let sanitized_parts: Vec<String> = parts
        .iter()
        .map(|part| {
            let mut sanitized_part = part.to_string();

            for (from, to) in &config.formatting.replace_chars {
                if from != "/" {
                    sanitized_part = sanitized_part.replace(from, to);
                }
            }

            if sanitized_part.len() > (config.formatting.max_filename_length as usize) {
                let max_len = config.formatting.max_filename_length as usize;
                if let Some(dot_pos) = sanitized_part.rfind('.') {
                    let name_part = &sanitized_part[..dot_pos];
                    let ext_part = &sanitized_part[dot_pos..];
                    if name_part.len() + ext_part.len() > max_len {
                        let available_for_name = max_len.saturating_sub(ext_part.len());
                        format!(
                            "{}{}",
                            &name_part[..available_for_name.min(name_part.len())],
                            ext_part
                        )
                    } else {
                        sanitized_part
                    }
                } else {
                    sanitized_part[..max_len.min(sanitized_part.len())].to_string()
                }
            } else {
                sanitized_part
            }
        })
        .collect();

    sanitized_parts.join("/")
}

fn handle_duplicate_rename(
    target_path: &PathBuf,
    metadata_key: &MetadataKey,
    used_metadata: &mut HashMap<MetadataKey, PathBuf>,
) -> PathBuf {
    let mut counter = 1;
    let stem = target_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let extension = target_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = target_path.parent().unwrap_or(Path::new(""));

    loop {
        let new_filename = format!("{} ({}){}", stem, counter, extension);
        let new_path = parent.join(new_filename);

        let mut new_metadata_key = metadata_key.clone();
        new_metadata_key.title = format!("{} ({})", metadata_key.title, counter);

        if !used_metadata.contains_key(&new_metadata_key) {
            used_metadata.insert(new_metadata_key, new_path.clone());
            return new_path;
        }
        counter += 1;
    }
}

fn create_metadata_key(metadata: &AudioMetadata) -> Option<MetadataKey> {
    let artist = if is_compilation(metadata) {
        metadata.artist.as_ref()
    } else {
        metadata.album_artist.as_ref().or(metadata.artist.as_ref())
    };

    let artist = artist?;
    let album = metadata.album.as_ref()?;
    let title = metadata.title.as_ref()?;

    Some(MetadataKey {
        artist: artist.clone(),
        album: album.clone(),
        title: title.clone(),
        track: metadata.track,
    })
}

fn sanitize_metadata_value(value: &str, config: &Config) -> String {
    let mut sanitized = value.to_string();
    for (from, to) in &config.formatting.replace_chars {
        sanitized = sanitized.replace(from, to);
    }
    sanitized
}

fn is_compilation(metadata: &AudioMetadata) -> bool {
    if let Some(album_artist) = &metadata.album_artist {
        album_artist.to_lowercase() == "various artists"
    } else {
        false
    }
}
