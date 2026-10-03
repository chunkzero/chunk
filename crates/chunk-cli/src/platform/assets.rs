//! `chunk platform assets`: a project's worlds, resource packs and files as the platform holds them. Push publishes
//! the local ones as the project's head, pull brings the head's single-file entries back into the sources, and deploy
//! runs an environment's release against a revision.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write as _},
    path::{Component, Path, PathBuf},
};

use chunk_build::assets::Store;
use chunk_contract::AssetRevision;
use chunk_management::{
    Client, Code,
    v1::{
        CompleteAssetUploadRequest, GetAssetRevisionRequest, GetDeploymentRequest, ListAssetRevisionsRequest,
        SetAssetHeadRequest, UploadAssetsRequest,
    },
};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::{
    EnvironmentArgs, ProjectArg, Session, all, api_error,
    deploy::{self, mebibytes},
    resources::{table, time},
};
use crate::building::{self, BuildMode, progress::Progress};
use plan::{Entry, Pull, Push};

mod plan;

#[derive(Args)]
pub(crate) struct Assets {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Build the project's assets, upload them and make them the project's head.
    Push {
        /// The project directory.
        #[arg(default_value = ".")]
        path: PathBuf,
        #[command(flatten)]
        project: ProjectArg,
        /// Replace a head that moved since your last push or pull.
        #[arg(long)]
        force: bool,
    },
    /// Update local single-file sources from the head, never overwriting a file you changed.
    Pull {
        /// The project directory.
        #[arg(default_value = ".")]
        path: PathBuf,
        #[command(flatten)]
        project: ProjectArg,
    },
    /// Deploy the environment's active release with the head, or another revision.
    Deploy {
        #[command(flatten)]
        environment: EnvironmentArgs,
        /// The revision to deploy, by ID; defaults to the head.
        #[arg(long)]
        revision: Option<String>,
        /// Stop the deployments this one replaces at once, disconnecting their players, instead of draining them.
        #[arg(long)]
        stop_previous: bool,
    },
    /// List the project's revisions, newest first.
    List {
        #[command(flatten)]
        project: ProjectArg,
    },
}

pub(super) async fn run(options: Assets) -> io::Result<()> {
    let session = Session::open()?;
    match options.action {
        Action::Push { path, project, force } => push(&session, path, &project, force).await,
        Action::Pull { path, project } => pull(&session, &path, &project).await,
        Action::Deploy { environment, revision, stop_previous } => {
            deploy_revision(&session, &environment, revision, stop_previous).await
        }
        Action::List { project } => list(&session, &project).await,
    }
}

/// What a checkout last published or pulled, in `.chunk/assets.json`. The revision's manifest is in the local store
/// or on the platform.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Base {
    project_id: String,
    revision_id: String,
}

fn base_path(root: &Path) -> PathBuf {
    root.join(".chunk/assets.json")
}

fn dev_store(root: &Path) -> Store {
    Store::new(root.join(".chunk/local/assets"))
}

/// The revision ID the checkout is based on, if it was synced with this project.
fn read_base(root: &Path, project_id: &str) -> io::Result<Option<String>> {
    let base: Base = match fs::read(base_path(root)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok((base.project_id == project_id).then_some(base.revision_id))
}

fn write_base(root: &Path, project_id: &str, revision_id: &str) -> io::Result<()> {
    let base = Base { project_id: project_id.into(), revision_id: revision_id.into() };
    let mut json = serde_json::to_vec_pretty(&base).map_err(io::Error::other)?;
    json.push(b'\n');
    let directory = root.join(".chunk");
    fs::create_dir_all(&directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&json)?;
    file.persist(base_path(root)).map_err(|error| error.error)?;
    Ok(())
}

/// The project's head revision ID; empty when nothing was pushed.
async fn head_id(client: &Client, project_id: &str) -> io::Result<String> {
    let request = ListAssetRevisionsRequest { project_id: project_id.into(), page_size: 1, page_token: String::new() };
    Ok(client.list_asset_revisions(&request).await.map_err(api_error)?.head_id)
}

/// The manifest of revision `id`, from the store or else the platform, which the store then keeps.
async fn manifest(client: &Client, store: &Store, project_id: &str, id: &str) -> io::Result<AssetRevision> {
    if let Ok(revision) = store.read_revision(id) {
        return Ok(revision);
    }
    let request = GetAssetRevisionRequest { project_id: project_id.into(), revision_id: id.into(), downloads: false };
    let response = client.get_asset_revision(&request).await.map_err(api_error)?;
    let revision = response.revision.ok_or_else(|| io::Error::other(format!("The platform has no revision {id}")))?;
    let revision = decode(&revision.manifest, id)?;
    store.write_revision(&revision)?;
    Ok(revision)
}

fn decode(manifest: &[u8], id: &str) -> io::Result<AssetRevision> {
    let revision = AssetRevision::decode(manifest).map_err(io::Error::other)?;
    if revision.id() != id {
        return Err(io::Error::other(format!("The platform returned a manifest that is not revision {id}")));
    }
    Ok(revision)
}

/// Declares `revision` and uploads the blobs from `store` the project lacks, so it becomes deployable.
pub(super) async fn upload_revision(
    client: &Client,
    project_id: &str,
    revision: &AssetRevision,
    store: &Path,
) -> io::Result<()> {
    let id = revision.id();
    let declared = UploadAssetsRequest { project_id: project_id.into(), manifest: revision.encode() };
    let response = client.upload_assets(&declared).await.map_err(api_error)?;
    let blobs = revision.blobs();
    if response.uploads.is_empty() {
        cliclack::log::info(format!("The platform already holds the blobs of assets revision {}", deploy::short(&id)))?;
    } else {
        let size = response.uploads.iter().filter_map(|upload| blobs.get(upload.sha256.as_str())).sum();
        cliclack::log::info(format!(
            "Uploading {} asset blobs ({}) of revision {}…",
            response.uploads.len(),
            mebibytes(size),
            deploy::short(&id)
        ))?;
    }
    for upload in response.uploads {
        let (Some(target), true) = (upload.upload, blobs.contains_key(upload.sha256.as_str())) else {
            return Err(io::Error::other("The platform asked for a blob that is not in the revision"));
        };
        let bytes = tokio::fs::read(store.join("blobs").join(&upload.sha256)).await?;
        client.upload_archive(&target, bytes).await.map_err(api_error)?;
    }
    let complete = CompleteAssetUploadRequest { project_id: project_id.into(), revision_id: id };
    client.complete_asset_upload(&complete).await.map_err(api_error)?;
    Ok(())
}

async fn push(session: &Session, path: PathBuf, selector: &ProjectArg, force: bool) -> io::Result<()> {
    let project = session.project(selector).await?;
    chunk_service::run(|stop| async move {
        let local = building::prepare(&building::Options { project: path, output: None, frozen: false })?;
        cliclack::log::info("Building assets…")?;
        let built = building::execute(&local, BuildMode::Dev, stop.clone(), Progress::default()).await?;
        let client = &session.client;
        let (revision, id) = (&built.assets, built.assets.id());
        let head = head_id(client, &project.id).await?;
        let base = read_base(&local.root, &project.id)?;
        match plan::check_push(&head, base.as_deref(), &id, force) {
            Push::UpToDate => {
                write_base(&local.root, &project.id, &id)?;
                return cliclack::log::info(format!("Nothing to push; the head is already revision {id}"));
            }
            Push::Moved => {
                return Err(io::Error::other(format!(
                    "The head moved to {head} since your base {}. Run `chunk platform assets pull` first, or \
                     push with --force to replace it.",
                    base.as_deref().unwrap_or("(none)")
                )));
            }
            Push::Send => {}
        }
        let previous = if head.is_empty() {
            AssetRevision::default()
        } else {
            manifest(client, &Store::new(&built.asset_store), &project.id, &head).await?
        };
        let moved = tokio::select! {
            moved = async {
                upload_revision(client, &project.id, revision, &built.asset_store).await?;
                let request = SetAssetHeadRequest {
                    project_id: project.id.clone(),
                    revision_id: id.clone(),
                    expected_head_id: head,
                };
                client.set_asset_head(&request).await.map_err(|error| {
                    if error.code() == Code::Aborted {
                        io::Error::other(
                            "The head moved while pushing. Run `chunk platform assets pull`, then push again.",
                        )
                    } else {
                        api_error(error)
                    }
                })
            } => moved,
            () = stop.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "push stopped")),
        };
        moved?;
        write_base(&local.root, &project.id, &id)?;
        let mut message = format!("Pushed assets revision {id}");
        for line in plan::summary(&plan::entries(&previous), &plan::entries(revision)) {
            message.push_str("\n  ");
            message.push_str(&line);
        }
        cliclack::log::success(message)
    })
    .await
}

async fn pull(session: &Session, path: &Path, selector: &ProjectArg) -> io::Result<()> {
    let project = session.project(selector).await?;
    let root = path.canonicalize()?;
    let metadata = chunk_build::project::inspect(&root)?;
    chunk_service::run(|stop| async move {
        tokio::select! {
            pulled = pull_head(&session.client, &project.id, &root, &metadata) => pulled,
            () = stop.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "pull stopped")),
        }
    })
    .await
}

/// A file a pull changes: the entry, its local path, the SHA-256 it had when planned (none when absent), and the
/// head's SHA-256 to write, or none to delete it.
struct Change<'a> {
    entry: Entry,
    path: PathBuf,
    seen: Option<String>,
    write: Option<&'a str>,
}

async fn pull_head(
    client: &Client,
    project_id: &str,
    root: &Path,
    metadata: &chunk_build::project::ProjectMetadata,
) -> io::Result<()> {
    let request =
        GetAssetRevisionRequest { project_id: project_id.into(), revision_id: String::new(), downloads: true };
    let response = client.get_asset_revision(&request).await.map_err(api_error)?;
    let Some(revision) = response.revision else {
        return cliclack::log::info("The project has no assets yet; `chunk platform assets push` publishes them.");
    };
    let head = decode(&revision.manifest, &revision.id)?;
    let store = dev_store(root);
    let base = match read_base(root, project_id)? {
        Some(id) if id == revision.id => return cliclack::log::info(format!("Already at revision {id}")),
        Some(id) => manifest(client, &store, project_id, &id).await?,
        None => AssetRevision::default(),
    };
    let (base, heads) = (plan::entries(&base), plan::entries(&head));
    let mut changes = Vec::new();
    let mut conflicts = Vec::new();
    let mut skipped = Vec::new();
    for entry in base.keys().chain(heads.keys()).collect::<std::collections::BTreeSet<_>>() {
        let (before, after) = (base.get(entry).copied(), heads.get(entry).copied());
        if before == after {
            continue;
        }
        let path = match plan::locate(entry, root, metadata) {
            Ok(path) => path,
            Err(reason) => {
                skipped.push(format!("{entry}: {reason}"));
                continue;
            }
        };
        ensure_inside(root, &path)?;
        let seen = digest(&path)?;
        match plan::decide(before, after, seen.as_deref()) {
            Pull::Keep => {}
            Pull::Write => changes.push(Change { entry: entry.clone(), path, seen, write: after }),
            Pull::Delete => changes.push(Change { entry: entry.clone(), path, seen, write: None }),
            Pull::Conflict => {
                conflicts.push(format!("{entry}: {}", path.strip_prefix(root).unwrap_or(&path).display()));
            }
        }
    }
    let blobs: BTreeMap<_, _> =
        response.downloads.iter().map(|download| (download.sha256.as_str(), download)).collect();
    for sha256 in changes.iter().filter_map(|change| change.write) {
        if store.contains(sha256)? {
            continue;
        }
        let url =
            blobs.get(sha256).ok_or_else(|| io::Error::other(format!("The platform gave no download for {sha256}")))?;
        download(client, &store, sha256, head.blobs()[sha256], &url.url).await?;
    }
    let (mut updated, mut deleted) = (Vec::new(), Vec::new());
    for change in changes {
        let shown = format!("{}: {}", change.entry, change.path.strip_prefix(root).unwrap_or(&change.path).display());
        let writing = change.write.is_some();
        let blob = change.write.map(|sha256| store.root().join("blobs").join(sha256));
        let root = root.to_path_buf();
        let (path, seen) = (change.path, change.seen);
        let applied = tokio::task::spawn_blocking(move || apply(&root, &path, seen.as_deref(), blob.as_deref()));
        if !applied.await.map_err(io::Error::other)?? {
            conflicts.push(shown);
        } else if writing {
            updated.push(shown);
        } else {
            deleted.push(shown);
        }
    }
    store.write_revision(&head)?;
    write_base(root, project_id, &revision.id)?;
    cliclack::log::success(format!(
        "Pulled assets revision {}: {} updated, {} deleted, {} conflicted, {} not pulled",
        revision.id,
        updated.len(),
        deleted.len(),
        conflicts.len(),
        skipped.len()
    ))?;
    for (title, lines) in [("Updated", &updated), ("Deleted", &deleted)] {
        if !lines.is_empty() {
            cliclack::log::info(format!("{title}:\n  {}", lines.join("\n  ")))?;
        }
    }
    if !conflicts.is_empty() {
        cliclack::log::warning(format!(
            "Changed here and on the platform, so left as they are; the next push publishes your version:\n  {}",
            conflicts.join("\n  ")
        ))?;
    }
    if !skipped.is_empty() {
        cliclack::log::warning(format!("Not pulled:\n  {}", skipped.join("\n  ")))?;
    }
    Ok(())
}

/// Downloads a blob of `size` bytes into the store, which verifies its SHA-256.
async fn download(client: &Client, store: &Store, sha256: &str, size: u64, url: &str) -> io::Result<()> {
    let mut download = client.download_blob(url).await.map_err(api_error)?;
    fs::create_dir_all(store.root())?;
    let mut file = tempfile::NamedTempFile::new_in(store.root())?;
    let mut received = 0;
    while let Some(chunk) = download.chunk().await.map_err(api_error)? {
        received += chunk.len() as u64;
        if received > size {
            return Err(io::Error::other(format!("Blob {sha256} is larger than its {size} bytes")));
        }
        file.write_all(&chunk)?;
    }
    file.flush()?;
    store.insert_file(sha256, file.path())
}

/// The SHA-256 of the file at `path`; none when it doesn't exist.
fn digest(path: &Path) -> io::Result<Option<String>> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(Some(format!("{:x}", digest.finalize())));
        }
        digest.update(&buffer[..read]);
    }
}

/// Writes the blob over the file at `path`, or deletes it, unless its digest is no longer `seen`.
/// Returns whether it applied the change.
fn apply(root: &Path, path: &Path, seen: Option<&str>, blob: Option<&Path>) -> io::Result<bool> {
    ensure_inside(root, path)?;
    if digest(path)?.as_deref() != seen {
        return Ok(false);
    }
    match blob {
        Some(blob) => replace(blob, path)?,
        None => fs::remove_file(path)?,
    }
    Ok(true)
}

/// Rejects a path that leaves `root` or has a symlink among the components below it.
fn ensure_inside(root: &Path, path: &Path) -> io::Result<()> {
    let relative =
        path.strip_prefix(root).map_err(|_| io::Error::other(format!("{} is outside the project", path.display())))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::other(format!("{} is outside the project", path.display())));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_symlink() => {
                return Err(io::Error::other(format!(
                    "{} is a symlink; pull does not write through symlinks",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Atomically replaces `target` with a copy of the blob.
fn replace(blob: &Path, target: &Path) -> io::Result<()> {
    let directory = target.parent().expect("sources have parents");
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    io::copy(&mut fs::File::open(blob)?, &mut temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary.as_file().set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    temporary.persist(target).map_err(|error| error.error)?;
    Ok(())
}

async fn deploy_revision(
    session: &Session,
    selector: &EnvironmentArgs,
    revision: Option<String>,
    stop_previous: bool,
) -> io::Result<()> {
    let (project, environment) = session.environment(selector).await?;
    let client = &session.client;
    if environment.active_deployment_id.is_empty() {
        return Err(io::Error::other(format!(
            "{} has nothing deployed yet; `chunk platform deploy --env {}` deploys a release.",
            environment.name, environment.name
        )));
    }
    let active = GetDeploymentRequest { deployment_id: environment.active_deployment_id.clone() };
    let release = client.get_deployment(&active).await.map_err(api_error)?.deployment.unwrap_or_default().release_id;
    let revision = match revision {
        Some(revision) => revision,
        None => head_id(client, &project.id).await?,
    };
    if revision.is_empty() {
        return Err(io::Error::other("The project has no assets yet; `chunk platform assets push` publishes them."));
    }
    let deployment = deploy::deploy(client, &environment, &release, &revision, stop_previous).await?;
    chunk_service::run(|stop: CancellationToken| deploy::follow_until_stopped(client, &environment, deployment, stop))
        .await
}

async fn list(session: &Session, selector: &ProjectArg) -> io::Result<()> {
    let project = session.project(selector).await?;
    let client = &session.client;
    let head = head_id(client, &project.id).await?;
    let project_id = &project.id;
    let revisions = all(|page_token| async move {
        let request = ListAssetRevisionsRequest { project_id: project_id.clone(), page_size: 0, page_token };
        client.list_asset_revisions(&request).await.map(|page| (page.revisions, page.next_page_token))
    })
    .await?;
    table(
        ["REVISION", "HEAD", "SIZE", "CREATED"],
        revisions.into_iter().map(|revision| {
            let marker = if revision.id == head { "head" } else { "" };
            [revision.id, marker.into(), mebibytes(revision.size_bytes), time(revision.create_time)]
        }),
    )
}

#[cfg(test)]
mod tests;
