use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(super) fn validate(directory: &Path) -> io::Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("project directory must not be a symlink: {}", directory.display()),
        )),
        Ok(metadata) if metadata.is_dir() && fs::read_dir(directory)?.next().is_none() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("project directory must be new or empty: {}", directory.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = directory.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
            if parent.try_exists()? {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("project parent directory does not exist: {}", parent.display()),
                ))
            }
        }
        Err(error) => Err(error),
    }
}

pub(super) fn install(staged: &Path, directory: &Path) -> io::Result<()> {
    validate(directory)?;
    let mut created = Vec::new();
    if !directory.exists() {
        fs::create_dir(directory)?;
        created.push(directory.to_owned());
    }
    let result = copy(staged, directory, &mut created);
    if result.is_err() {
        for path in created.into_iter().rev() {
            if path.is_dir() {
                let _ = fs::remove_dir(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
    result
}

fn copy(source: &Path, destination: &Path, created: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir(&target)?;
            created.push(target.clone());
            copy(&entry.path(), &target, created)?;
        } else {
            let mut output = fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
            created.push(target);
            io::copy(&mut fs::File::open(entry.path())?, &mut output)?;
            output.set_permissions(entry.metadata()?.permissions())?;
        }
    }
    Ok(())
}
