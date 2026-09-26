use std::io::{self, Cursor, Read, Write};

use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

use super::jars;
use crate::publication::{Files, insert};

/// Publishes a thin app JAR and its runtime classpath under `libs/` and returns a launcher JAR that runs `main` with
/// them. The launcher manifest names every JAR by digest, so its own digest changes whenever any of them does.
pub(super) fn assemble(main: &str, jars: impl IntoIterator<Item = Vec<u8>>, files: &mut Files) -> io::Result<Vec<u8>> {
    let mut classpath = Vec::new();
    for bytes in jars {
        let name = format!("libs/{}.jar", super::content_digest(&bytes));
        insert(files, name.clone(), bytes)?;
        let entry = format!("../../{name}");
        if !classpath.contains(&entry) {
            classpath.push(entry);
        }
    }
    write(main, &classpath)
}

/// The launcher JAR that runs `main` over `classpath`, byte for byte as `assemble` publishes it.
pub(super) fn write(main: &str, classpath: &[String]) -> io::Result<Vec<u8>> {
    let manifest: String = [
        "Manifest-Version: 1.0".to_owned(),
        format!("Main-Class: {main}"),
        format!("Class-Path: {}", classpath.join(" ")),
        "Created-By: chunk dev".into(),
    ]
    .iter()
    .map(|attribute| wrap(attribute))
    .chain(["\r\n".into()])
    .collect();
    let mut jar = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(DateTime::default());
    jar.start_file("META-INF/MANIFEST.MF", options).map_err(io::Error::other)?;
    jar.write_all(manifest.as_bytes())?;
    Ok(jar.finish().map_err(io::Error::other)?.into_inner())
}

/// The `Class-Path` of a launcher JAR, which holds only its manifest; `None` for any other JAR.
pub(super) fn classpath(jar: &[u8]) -> io::Result<Option<Vec<String>>> {
    let mut archive = ZipArchive::new(Cursor::new(jar)).map_err(io::Error::other)?;
    if archive.len() != 1 {
        return Ok(None);
    }
    let mut manifest = Vec::new();
    match archive.by_name("META-INF/MANIFEST.MF") {
        Ok(entry) => entry.take(65_537).read_to_end(&mut manifest)?,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(io::Error::other(error)),
    };
    Ok(jars::attributes(&manifest)?.iter().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case("Class-Path").then(|| value.split_whitespace().map(Into::into).collect())
    }))
}

/// Splits a manifest attribute into 72-byte lines without breaking UTF-8 characters.
fn wrap(attribute: &str) -> String {
    let mut wrapped = String::new();
    let mut width = 0;
    for character in attribute.chars() {
        if width + character.len_utf8() > 72 {
            wrapped.push_str("\r\n ");
            width = 1;
        }
        wrapped.push(character);
        width += character.len_utf8();
    }
    wrapped + "\r\n"
}
