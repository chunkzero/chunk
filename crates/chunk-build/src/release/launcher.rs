use std::io::{self, Cursor, Read, Write};

use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

use super::jars;

/// The thin app JAR and at most the 1024 classpath JARs a JVM descriptor allows.
const MAX_CLASSPATH: usize = 1025;
/// Holds the largest manifest `write` produces: `MAX_CLASSPATH` entries of 80 bytes, a 1024-byte `Main-Class` and
/// line wrapping come to under 90 KiB.
const MANIFEST_LIMIT: u64 = 128 * 1024;

/// The launcher JAR that runs `main` over `classpath`, byte for byte as release publication assembles it.
pub(super) fn write(main: &str, classpath: &[String]) -> io::Result<Vec<u8>> {
    if classpath.len() > MAX_CLASSPATH {
        return Err(io::Error::other(format!("launcher classpath exceeds {MAX_CLASSPATH} JARs")));
    }
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
        Ok(entry) => entry.take(MANIFEST_LIMIT + 1).read_to_end(&mut manifest)?,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(io::Error::other(error)),
    };
    if manifest.len() as u64 > MANIFEST_LIMIT {
        return Err(io::Error::other(format!("launcher manifest exceeds {MANIFEST_LIMIT} bytes")));
    }
    let classpath: Option<Vec<String>> = jars::attributes(&manifest)?.iter().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case("Class-Path").then(|| value.split_whitespace().map(Into::into).collect())
    });
    if classpath.as_ref().is_some_and(|entries| entries.len() > MAX_CLASSPATH) {
        return Err(io::Error::other(format!("launcher classpath exceeds {MAX_CLASSPATH} JARs")));
    }
    Ok(classpath)
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
