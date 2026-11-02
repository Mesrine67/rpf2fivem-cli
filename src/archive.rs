use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

/// Extract a zip/rar/7z archive, returning a list of extracted file paths.
pub fn extract(archive_path: &Path, output_dir: &Path) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(output_dir)?;

    let ext = archive_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "zip" => extract_zip(archive_path, output_dir),
        "7z" => extract_7z(archive_path, output_dir),
        "rar" => extract_rar(archive_path, output_dir),
        _ => bail!("Unsupported archive format: .{}", ext),
    }
}

// Limits are conservative until FXMP has real-world archive fixtures.
const MAX_ZIP_ENTRIES: usize = 10_000;
const MAX_ZIP_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ZIP_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn extract_zip(archive_path: &Path, output_dir: &Path) -> Result<Vec<PathBuf>> {
    let file = fs::File::open(archive_path)
        .with_context(|| format!("Cannot open zip: {}", archive_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)?;
    if archive.len() > MAX_ZIP_ENTRIES {
        bail!("ZIP contains too many entries: {}", archive.len());
    }
    let root = fs::canonicalize(output_dir)?;
    let mut extracted = Vec::new();
    let mut total_bytes = 0_u64;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_owned();
        // A ZIP member is untrusted, even when the archive was downloaded
        // from a well-known site. Do not allow absolute or traversing paths.
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| anyhow::anyhow!("Unsafe ZIP entry path: {name:?}"))?
            .to_path_buf();
        if relative.as_os_str().is_empty() {
            bail!("Empty ZIP entry path");
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("ZIP symbolic links are not supported: {name:?}");
        }
        if entry.is_dir() {
            continue;
        }
        if entry.size() > MAX_ZIP_ENTRY_BYTES
            || entry.size() > MAX_ZIP_TOTAL_BYTES.saturating_sub(total_bytes)
        {
            bail!("ZIP entry exceeds extraction budget: {name:?}");
        }

        let out_path = output_dir.join(&relative);
        let parent = out_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Invalid ZIP destination"))?;
        fs::create_dir_all(parent)?;
        if !fs::canonicalize(parent)?.starts_with(&root) {
            bail!("ZIP entry escapes the extraction directory: {name:?}");
        }

        // Stream into an isolated temporary file; never load an unbounded
        // decompressed entry into memory or overwrite an existing destination.
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        let copied = std::io::copy(&mut (&mut entry).take(MAX_ZIP_ENTRY_BYTES + 1), &mut temp)?;
        total_bytes = total_bytes
            .checked_add(copied)
            .ok_or_else(|| anyhow::anyhow!("ZIP extraction byte counter overflow"))?;
        if copied > MAX_ZIP_ENTRY_BYTES || total_bytes > MAX_ZIP_TOTAL_BYTES {
            bail!("ZIP decompressed data exceeds extraction budget: {name:?}");
        }
        temp.persist_noclobber(&out_path).with_context(|| {
            format!(
                "Cannot safely create ZIP destination: {}",
                out_path.display()
            )
        })?;
        extracted.push(out_path);
    }

    Ok(extracted)
}

fn extract_7z(archive_path: &Path, output_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut extracted = Vec::new();
    sevenz_rust::decompress_file(archive_path, output_dir)
        .with_context(|| format!("Failed to extract 7z: {}", archive_path.display()))?;

    // Collect all extracted files
    collect_files(output_dir, &mut extracted);
    Ok(extracted)
}

fn extract_rar(archive_path: &Path, output_dir: &Path) -> Result<Vec<PathBuf>> {
    use unrar::Archive;

    let archive = Archive::new(archive_path)
        .open_for_processing()
        .with_context(|| format!("Failed to open RAR: {}", archive_path.display()))?;

    let mut extracted = Vec::new();

    let mut current = archive;
    loop {
        let header = current.read_header();
        match header {
            Ok(Some(entry)) => {
                let entry_path = output_dir.join(entry.entry().filename.as_os_str());
                if entry.entry().is_file() {
                    if let Some(parent) = entry_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let (data, next) = entry.read()?;
                    fs::write(&entry_path, &data)?;
                    extracted.push(entry_path);
                    current = next;
                } else {
                    let next = entry.skip()?;
                    current = next;
                }
            }
            Ok(None) => break,
            Err(e) => bail!("RAR read error: {}", e),
        }
    }

    Ok(extracted)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, out);
            } else {
                out.push(path);
            }
        }
    }
}

/// Find all .rpf files in a directory tree.
pub fn find_rpf_files(dir: &Path) -> Vec<PathBuf> {
    let mut rpfs = Vec::new();
    collect_by_ext(dir, "rpf", &mut rpfs);
    rpfs
}

fn collect_by_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_by_ext(&path, ext, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
                out.push(path);
            }
        }
    }
}

/// Download a file from a URL into cache_dir, returning the saved path.
/// The filename is taken from the Content-Disposition header if available,
/// otherwise inferred from the URL's last path segment.
// A network request must never read an unbounded response into process memory.
const MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn safe_download_filename(raw: &str) -> Option<String> {
    // The HTTP header is untrusted. Accept a basename only (both slash styles
    // matter on Windows), with a conservative portable character set.
    let basename = raw.rsplit(&['/', '\\'][..]).next()?;
    let basename = basename.split(['?', '#']).next()?.trim();
    if basename.is_empty() || basename == "." || basename == ".." {
        return None;
    }
    let clean: String = basename
        .chars()
        .take(128)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.trim_matches(['.', '_']).is_empty() {
        return None;
    }
    Some(clean)
}

pub fn download(url: &str, cache_dir: &Path) -> Result<PathBuf> {
    eprintln!("[Download] Fetching {}...", url);
    fs::create_dir_all(cache_dir)?;

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .build()?;

    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("HTTP request failed: {}", url))?;

    if !response.status().is_success() {
        bail!("HTTP {} for {}", response.status(), url);
    }
    if response
        .content_length()
        .is_some_and(|bytes| bytes > MAX_DOWNLOAD_BYTES)
    {
        bail!("Download exceeds the permitted size");
    }

    let filename = response
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .and_then(filename_from_content_disposition)
        .as_deref()
        .and_then(safe_download_filename)
        .or_else(|| {
            response
                .url()
                .path_segments()
                .and_then(|segments| segments.last())
                .and_then(safe_download_filename)
        })
        .unwrap_or_else(|| "download.zip".to_string());

    // Check the first bytes before creating a visible output. Stream to a
    // temporary file and commit only if the entire bounded download succeeds.
    let mut initial = [0u8; 64];
    let first_len = response.read(&mut initial)?;
    let prefix = &initial[..first_len];
    if prefix.starts_with(b"<!DOCTYPE")
        || prefix.starts_with(b"<!doctype")
        || prefix.starts_with(b"<html")
    {
        bail!("Received HTML instead of a file; download it manually.");
    }
    let mut temp = tempfile::NamedTempFile::new_in(cache_dir)?;
    use std::io::Write;
    temp.write_all(prefix)?;
    let copied = std::io::copy(
        &mut response.take(MAX_DOWNLOAD_BYTES + 1 - first_len as u64),
        &mut temp,
    )?;
    let total = copied + first_len as u64;
    if total > MAX_DOWNLOAD_BYTES {
        bail!("Downloaded content exceeds the permitted size");
    }

    let dest = cache_dir.join(filename);
    temp.persist_noclobber(&dest).with_context(|| {
        format!(
            "Cannot create download without overwriting: {}",
            dest.display()
        )
    })?;
    eprintln!("[Download] Saved {} bytes to {}", total, dest.display());
    Ok(dest)
}

fn filename_from_content_disposition(header: &str) -> Option<String> {
    for part in header.split(';') {
        let part = part.trim();
        // RFC 5987 extended value: filename*=UTF-8''name.zip
        if let Some(rest) = part.strip_prefix("filename*=") {
            let name = rest.trim();
            let name = name.split_once("''").map(|(_, n)| n).unwrap_or(name);
            let name = name.trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
        // Plain value: filename="name.zip"
        if let Some(rest) = part.strip_prefix("filename=") {
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture_zip(zip_path: &Path, members: &[(&str, &[u8])]) {
        let file = fs::File::create(zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for &(name, content) in members {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(content).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn zip_extracts_nested_files_under_the_selected_root() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("test.zip");
        let dest = dir.path().join("out");
        fixture_zip(
            &archive,
            &[
                ("stream/car.yft", b"model"),
                ("data/handling.meta", b"meta"),
            ],
        );
        let paths = extract(&archive, &dest).unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(fs::read(dest.join("stream/car.yft")).unwrap(), b"model");
        assert_eq!(fs::read(dest.join("data/handling.meta")).unwrap(), b"meta");
    }

    #[test]
    fn zip_rejects_parent_traversal_without_writing_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bad.zip");
        let dest = dir.path().join("out");
        fixture_zip(&archive, &[("../outside.txt", b"escape")]);
        assert!(extract(&archive, &dest).is_err());
        assert!(!dir.path().join("outside.txt").exists());
    }

    #[test]
    fn download_names_cannot_escape_cache_directory() {
        assert_eq!(
            safe_download_filename("../../etc/passwd"),
            Some("passwd".into())
        );
        assert_eq!(
            safe_download_filename(r"..\\..\\secret.zip"),
            Some("secret.zip".into())
        );
        assert_eq!(
            safe_download_filename("archive.zip?token=sensitive"),
            Some("archive.zip".into())
        );
        assert_eq!(
            safe_download_filename("name:bad?.zip"),
            Some("name_bad".into())
        );
        assert_eq!(safe_download_filename(".."), None);
        assert_eq!(safe_download_filename("."), None);
        assert_eq!(safe_download_filename(""), None);
    }

    #[test]
    fn zip_never_overwrites_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("test.zip");
        let dest = dir.path().join("out");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("file.txt"), b"original").unwrap();
        fixture_zip(&archive, &[("file.txt", b"overwrite")]);
        assert!(extract(&archive, &dest).is_err());
        assert_eq!(fs::read(dest.join("file.txt")).unwrap(), b"original");
    }
}
