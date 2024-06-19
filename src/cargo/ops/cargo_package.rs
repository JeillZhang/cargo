use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::prelude::*;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::Poll;

use crate::core::compiler::{BuildConfig, CompileMode, DefaultExecutor, Executor};
use crate::core::manifest::Target;
use crate::core::resolver::CliFeatures;
use crate::core::resolver::HasDevUnits;
use crate::core::{Feature, PackageIdSpecQuery, Shell, Verbosity, Workspace};
use crate::core::{Package, PackageId, PackageSet, Resolve, SourceId};
use crate::sources::PathSource;
use crate::util::cache_lock::CacheLockMode;
use crate::util::context::JobsConfig;
use crate::util::errors::CargoResult;
use crate::util::toml::prepare_for_publish;
use crate::util::{self, human_readable_bytes, restricted_names, FileLock, GlobalContext};
use crate::{drop_println, ops};
use anyhow::Context as _;
use cargo_util::paths;
use flate2::read::GzDecoder;
use flate2::{Compression, GzBuilder};
use serde::Serialize;
use tar::{Archive, Builder, EntryType, Header, HeaderMode};
use tracing::debug;
use unicase::Ascii as UncasedAscii;

#[derive(Clone)]
pub struct PackageOpts<'gctx> {
    pub gctx: &'gctx GlobalContext,
    pub list: bool,
    pub check_metadata: bool,
    pub allow_dirty: bool,
    pub verify: bool,
    pub jobs: Option<JobsConfig>,
    pub keep_going: bool,
    pub to_package: ops::Packages,
    pub targets: Vec<String>,
    pub cli_features: CliFeatures,
}

const ORIGINAL_MANIFEST_FILE: &str = "Cargo.toml.orig";
const VCS_INFO_FILE: &str = ".cargo_vcs_info.json";

struct ArchiveFile {
    /// The relative path in the archive (not including the top-level package
    /// name directory).
    rel_path: PathBuf,
    /// String variant of `rel_path`, for convenience.
    rel_str: String,
    /// The contents to add to the archive.
    contents: FileContents,
}

enum FileContents {
    /// Absolute path to the file on disk to add to the archive.
    OnDisk(PathBuf),
    /// Generates a file.
    Generated(GeneratedFile),
}

enum GeneratedFile {
    /// Generates `Cargo.toml` by rewriting the original.
    Manifest,
    /// Generates `Cargo.lock` in some cases (like if there is a binary).
    Lockfile,
    /// Adds a `.cargo_vcs_info.json` file if in a (clean) git repo.
    VcsInfo(VcsInfo),
}

#[derive(Serialize)]
struct VcsInfo {
    git: GitVcsInfo,
    /// Path to the package within repo (empty string if root). / not \
    path_in_vcs: String,
}

#[derive(Serialize)]
struct GitVcsInfo {
    sha1: String,
}

/// Packages a single package in a workspace, returning the resulting tar file.
///
/// # Panics
/// Panics if `opts.list` is true. In that case you probably don't want to
/// actually build the package tarball; you should just make and print the list
/// of files. (We don't currently provide a public API for that, but see how
/// [`package`] does it.)
pub fn package_one(
    ws: &Workspace<'_>,
    pkg: &Package,
    opts: &PackageOpts<'_>,
) -> CargoResult<FileLock> {
    assert!(!opts.list);

    let ar_files = prepare_archive(ws, pkg, opts)?;
    let tarball = create_package(ws, pkg, ar_files)?;

    if opts.verify {
        run_verify(ws, pkg, &tarball, opts)?;
    }

    Ok(tarball)
}

// Builds a tarball and places it in the output directory.
fn create_package(
    ws: &Workspace<'_>,
    pkg: &Package,
    ar_files: Vec<ArchiveFile>,
) -> CargoResult<FileLock> {
    let gctx = ws.gctx();
    let filecount = ar_files.len();

    // Check that the package dependencies are safe to deploy.
    for dep in pkg.dependencies() {
        super::check_dep_has_version(dep, false)?;
    }

    let filename = pkg.package_id().tarball_name();
    let dir = ws.target_dir().join("package");
    let mut dst = {
        let tmp = format!(".{}", filename);
        dir.open_rw_exclusive_create(&tmp, gctx, "package scratch space")?
    };

    // Package up and test a temporary tarball and only move it to the final
    // location if it actually passes all our tests. Any previously existing
    // tarball can be assumed as corrupt or invalid, so we just blow it away if
    // it exists.
    gctx.shell()
        .status("Packaging", pkg.package_id().to_string())?;
    dst.file().set_len(0)?;
    let uncompressed_size = tar(ws, pkg, ar_files, dst.file(), &filename)
        .with_context(|| "failed to prepare local package for uploading")?;

    dst.seek(SeekFrom::Start(0))?;
    let src_path = dst.path();
    let dst_path = dst.parent().join(&filename);
    fs::rename(&src_path, &dst_path)
        .with_context(|| "failed to move temporary tarball into final location")?;

    let dst_metadata = dst
        .file()
        .metadata()
        .with_context(|| format!("could not learn metadata for: `{}`", dst_path.display()))?;
    let compressed_size = dst_metadata.len();

    let uncompressed = human_readable_bytes(uncompressed_size);
    let compressed = human_readable_bytes(compressed_size);

    let message = format!(
        "{} files, {:.1}{} ({:.1}{} compressed)",
        filecount, uncompressed.0, uncompressed.1, compressed.0, compressed.1,
    );
    // It doesn't really matter if this fails.
    drop(gctx.shell().status("Packaged", message));

    return Ok(dst);
}

pub fn package(ws: &Workspace<'_>, opts: &PackageOpts<'_>) -> CargoResult<Option<Vec<FileLock>>> {
    let specs = &opts.to_package.to_package_id_specs(ws)?;
    // If -p is used, we should check spec is matched with the members (See #13719)
    if let ops::Packages::Packages(_) = opts.to_package {
        for spec in specs.iter() {
            let member_ids = ws.members().map(|p| p.package_id());
            spec.query(member_ids)?;
        }
    }
    let pkgs = ws.members_with_features(specs, &opts.cli_features)?;
    let mut dsts = Vec::with_capacity(pkgs.len());

    if ws.root().join("Cargo.lock").exists() {
        // Make sure the Cargo.lock is up-to-date and valid.
        let dry_run = false;
        let _ = ops::resolve_ws(ws, dry_run)?;
        // If Cargo.lock does not exist, it will be generated by `build_lock`
        // below, and will be validated during the verification step.
    }

    for (pkg, cli_features) in pkgs {
        let opts = PackageOpts {
            to_package: ops::Packages::Default,
            cli_features,
            ..opts.clone()
        };
        let ar_files = prepare_archive(ws, pkg, &opts)?;

        if opts.list {
            for ar_file in ar_files {
                drop_println!(ws.gctx(), "{}", ar_file.rel_str);
            }
        } else {
            let tarball = create_package(ws, pkg, ar_files)?;
            if opts.verify {
                run_verify(ws, pkg, &tarball, &opts)
                    .with_context(|| "failed to verify package tarball")?;
            }
            dsts.push(tarball);
        }
    }

    if opts.list {
        // We're just listing, so there's no file output
        Ok(None)
    } else {
        Ok(Some(dsts))
    }
}

/// Performs pre-archiving checks and builds a list of files to archive.
fn prepare_archive(
    ws: &Workspace<'_>,
    pkg: &Package,
    opts: &PackageOpts<'_>,
) -> CargoResult<Vec<ArchiveFile>> {
    let gctx = ws.gctx();
    let mut src = PathSource::new(pkg.root(), pkg.package_id().source_id(), gctx);
    src.update()?;

    if opts.check_metadata {
        check_metadata(pkg, gctx)?;
    }

    if !pkg.manifest().exclude().is_empty() && !pkg.manifest().include().is_empty() {
        gctx.shell().warn(
            "both package.include and package.exclude are specified; \
             the exclude list will be ignored",
        )?;
    }
    let src_files = src.list_files(pkg)?;

    // Check (git) repository state, getting the current commit hash if not
    // dirty.
    let vcs_info = if !opts.allow_dirty {
        // This will error if a dirty repo is found.
        check_repo_state(pkg, &src_files, gctx)?
    } else {
        None
    };

    build_ar_list(ws, pkg, src_files, vcs_info)
}

/// Builds list of files to archive.
fn build_ar_list(
    ws: &Workspace<'_>,
    pkg: &Package,
    src_files: Vec<PathBuf>,
    vcs_info: Option<VcsInfo>,
) -> CargoResult<Vec<ArchiveFile>> {
    let mut result = HashMap::new();
    let root = pkg.root();
    for src_file in &src_files {
        let rel_path = src_file.strip_prefix(&root)?;
        check_filename(rel_path, &mut ws.gctx().shell())?;
        let rel_str = rel_path.to_str().ok_or_else(|| {
            anyhow::format_err!("non-utf8 path in source directory: {}", rel_path.display())
        })?;
        match rel_str {
            "Cargo.lock" => continue,
            VCS_INFO_FILE | ORIGINAL_MANIFEST_FILE => anyhow::bail!(
                "invalid inclusion of reserved file name {} in package source",
                rel_str
            ),
            _ => {
                result
                    .entry(UncasedAscii::new(rel_str))
                    .or_insert_with(Vec::new)
                    .push(ArchiveFile {
                        rel_path: rel_path.to_owned(),
                        rel_str: rel_str.to_owned(),
                        contents: FileContents::OnDisk(src_file.clone()),
                    });
            }
        }
    }

    // Ensure we normalize for case insensitive filesystems (like on Windows) by removing the
    // existing entry, regardless of case, and adding in with the correct case
    if result.remove(&UncasedAscii::new("Cargo.toml")).is_some() {
        result
            .entry(UncasedAscii::new(ORIGINAL_MANIFEST_FILE))
            .or_insert_with(Vec::new)
            .push(ArchiveFile {
                rel_path: PathBuf::from(ORIGINAL_MANIFEST_FILE),
                rel_str: ORIGINAL_MANIFEST_FILE.to_string(),
                contents: FileContents::OnDisk(pkg.manifest_path().to_owned()),
            });
        result
            .entry(UncasedAscii::new("Cargo.toml"))
            .or_insert_with(Vec::new)
            .push(ArchiveFile {
                rel_path: PathBuf::from("Cargo.toml"),
                rel_str: "Cargo.toml".to_string(),
                contents: FileContents::Generated(GeneratedFile::Manifest),
            });
    } else {
        ws.gctx().shell().warn(&format!(
            "no `Cargo.toml` file found when packaging `{}` (note the case of the file name).",
            pkg.name()
        ))?;
    }

    if pkg.include_lockfile() {
        let rel_str = "Cargo.lock";
        result
            .entry(UncasedAscii::new(rel_str))
            .or_insert_with(Vec::new)
            .push(ArchiveFile {
                rel_path: PathBuf::from(rel_str),
                rel_str: rel_str.to_string(),
                contents: FileContents::Generated(GeneratedFile::Lockfile),
            });
    }
    if let Some(vcs_info) = vcs_info {
        let rel_str = VCS_INFO_FILE;
        result
            .entry(UncasedAscii::new(rel_str))
            .or_insert_with(Vec::new)
            .push(ArchiveFile {
                rel_path: PathBuf::from(rel_str),
                rel_str: rel_str.to_string(),
                contents: FileContents::Generated(GeneratedFile::VcsInfo(vcs_info)),
            });
    }

    let mut invalid_manifest_field: Vec<String> = vec![];

    let mut result = result.into_values().flatten().collect();
    if let Some(license_file) = &pkg.manifest().metadata().license_file {
        let license_path = Path::new(license_file);
        let abs_file_path = paths::normalize_path(&pkg.root().join(license_path));
        if abs_file_path.is_file() {
            check_for_file_and_add(
                "license-file",
                license_path,
                abs_file_path,
                pkg,
                &mut result,
                ws,
            )?;
        } else {
            error_on_nonexistent_file(
                &pkg,
                &license_path,
                "license-file",
                &mut invalid_manifest_field,
            );
        }
    }
    if let Some(readme) = &pkg.manifest().metadata().readme {
        let readme_path = Path::new(readme);
        let abs_file_path = paths::normalize_path(&pkg.root().join(readme_path));
        if abs_file_path.is_file() {
            check_for_file_and_add("readme", readme_path, abs_file_path, pkg, &mut result, ws)?;
        } else {
            error_on_nonexistent_file(&pkg, &readme_path, "readme", &mut invalid_manifest_field);
        }
    }

    if !invalid_manifest_field.is_empty() {
        return Err(anyhow::anyhow!(invalid_manifest_field.join("\n")));
    }

    for t in pkg
        .manifest()
        .targets()
        .iter()
        .filter(|t| t.is_custom_build())
    {
        if let Some(custome_build_path) = t.src_path().path() {
            let abs_custome_build_path =
                paths::normalize_path(&pkg.root().join(custome_build_path));
            if !abs_custome_build_path.is_file() || !abs_custome_build_path.starts_with(pkg.root())
            {
                error_custom_build_file_not_in_package(pkg, &abs_custome_build_path, t)?;
            }
        }
    }

    result.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));

    Ok(result)
}

fn check_for_file_and_add(
    label: &str,
    file_path: &Path,
    abs_file_path: PathBuf,
    pkg: &Package,
    result: &mut Vec<ArchiveFile>,
    ws: &Workspace<'_>,
) -> CargoResult<()> {
    match abs_file_path.strip_prefix(&pkg.root()) {
        Ok(rel_file_path) => {
            if !result.iter().any(|ar| ar.rel_path == rel_file_path) {
                result.push(ArchiveFile {
                    rel_path: rel_file_path.to_path_buf(),
                    rel_str: rel_file_path
                        .to_str()
                        .expect("everything was utf8")
                        .to_string(),
                    contents: FileContents::OnDisk(abs_file_path),
                })
            }
        }
        Err(_) => {
            // The file exists somewhere outside of the package.
            let file_name = file_path.file_name().unwrap();
            if result.iter().any(|ar| ar.rel_path == file_name) {
                ws.gctx().shell().warn(&format!(
                    "{} `{}` appears to be a path outside of the package, \
                            but there is already a file named `{}` in the root of the package. \
                            The archived crate will contain the copy in the root of the package. \
                            Update the {} to point to the path relative \
                            to the root of the package to remove this warning.",
                    label,
                    file_path.display(),
                    file_name.to_str().unwrap(),
                    label,
                ))?;
            } else {
                result.push(ArchiveFile {
                    rel_path: PathBuf::from(file_name),
                    rel_str: file_name.to_str().unwrap().to_string(),
                    contents: FileContents::OnDisk(abs_file_path),
                })
            }
        }
    }
    Ok(())
}

fn error_on_nonexistent_file(
    pkg: &Package,
    path: &Path,
    manifest_key_name: &'static str,
    invalid: &mut Vec<String>,
) {
    let rel_msg = if path.is_absolute() {
        "".to_string()
    } else {
        format!(" (relative to `{}`)", pkg.root().display())
    };

    let msg = format!(
        "{manifest_key_name} `{}` does not appear to exist{}.\n\
                Please update the {manifest_key_name} setting in the manifest at `{}`.",
        path.display(),
        rel_msg,
        pkg.manifest_path().display()
    );

    invalid.push(msg);
}

fn error_custom_build_file_not_in_package(
    pkg: &Package,
    path: &Path,
    target: &Target,
) -> CargoResult<Vec<ArchiveFile>> {
    let tip = {
        let description_name = target.description_named();
        if path.is_file() {
            format!("the source file of {description_name} doesn't appear to be a path inside of the package.\n\
            It is at `{}`, whereas the root the package is `{}`.\n",
            path.display(), pkg.root().display()
            )
        } else {
            format!("the source file of {description_name} doesn't appear to exist.\n",)
        }
    };
    let msg = format!(
        "{}\
        This may cause issue during packaging, as modules resolution and resources included via macros are often relative to the path of source files.\n\
        Please update the `build` setting in the manifest at `{}` and point to a path inside the root of the package.",
        tip,  pkg.manifest_path().display()
    );
    anyhow::bail!(msg)
}

/// Construct `Cargo.lock` for the package to be published.
fn build_lock(ws: &Workspace<'_>, publish_pkg: &Package) -> CargoResult<String> {
    let gctx = ws.gctx();
    let orig_resolve = ops::load_pkg_lockfile(ws)?;

    let tmp_ws = Workspace::ephemeral(publish_pkg.clone(), ws.gctx(), None, true)?;
    let mut tmp_reg = ws.package_registry()?;
    let mut new_resolve = ops::resolve_with_previous(
        &mut tmp_reg,
        &tmp_ws,
        &CliFeatures::new_all(true),
        HasDevUnits::Yes,
        orig_resolve.as_ref(),
        None,
        &[],
        true,
    )?;
    let pkg_set = ops::get_resolved_packages(&new_resolve, tmp_reg)?;

    if let Some(orig_resolve) = orig_resolve {
        compare_resolve(gctx, tmp_ws.current()?, &orig_resolve, &new_resolve)?;
    }
    check_yanked(
        gctx,
        &pkg_set,
        &new_resolve,
        "consider updating to a version that is not yanked",
    )?;

    ops::resolve_to_string(&tmp_ws, &mut new_resolve)
}

// Checks that the package has some piece of metadata that a human can
// use to tell what the package is about.
fn check_metadata(pkg: &Package, gctx: &GlobalContext) -> CargoResult<()> {
    let md = pkg.manifest().metadata();

    let mut missing = vec![];

    macro_rules! lacking {
        ($( $($field: ident)||* ),*) => {{
            $(
                if $(md.$field.as_ref().map_or(true, |s| s.is_empty()))&&* {
                    $(missing.push(stringify!($field).replace("_", "-"));)*
                }
            )*
        }}
    }
    lacking!(
        description,
        license || license_file,
        documentation || homepage || repository
    );

    if !missing.is_empty() {
        let mut things = missing[..missing.len() - 1].join(", ");
        // `things` will be empty if and only if its length is 1 (i.e., the only case
        // to have no `or`).
        if !things.is_empty() {
            things.push_str(" or ");
        }
        things.push_str(missing.last().unwrap());

        gctx.shell().warn(&format!(
            "manifest has no {things}.\n\
             See https://doc.rust-lang.org/cargo/reference/manifest.html#package-metadata for more info.",
            things = things
        ))?
    }

    Ok(())
}

/// Checks if the package source is in a *git* DVCS repository. If *git*, and
/// the source is *dirty* (e.g., has uncommitted changes) then `bail!` with an
/// informative message. Otherwise return the sha1 hash of the current *HEAD*
/// commit, or `None` if no repo is found.
fn check_repo_state(
    p: &Package,
    src_files: &[PathBuf],
    gctx: &GlobalContext,
) -> CargoResult<Option<VcsInfo>> {
    if let Ok(repo) = git2::Repository::discover(p.root()) {
        if let Some(workdir) = repo.workdir() {
            debug!("found a git repo at {:?}", workdir);
            let path = p.manifest_path();
            let path =
                paths::strip_prefix_canonical(path, workdir).unwrap_or_else(|_| path.to_path_buf());
            if let Ok(status) = repo.status_file(&path) {
                if (status & git2::Status::IGNORED).is_empty() {
                    debug!(
                        "found (git) Cargo.toml at {:?} in workdir {:?}",
                        path, workdir
                    );
                    let path_in_vcs = path
                        .parent()
                        .and_then(|p| p.to_str())
                        .unwrap_or("")
                        .replace("\\", "/");
                    return Ok(Some(VcsInfo {
                        git: git(p, src_files, &repo)?,
                        path_in_vcs,
                    }));
                }
            }
            gctx.shell().verbose(|shell| {
                shell.warn(format!(
                    "no (git) Cargo.toml found at `{}` in workdir `{}`",
                    path.display(),
                    workdir.display()
                ))
            })?;
        }
    } else {
        gctx.shell().verbose(|shell| {
            shell.warn(format!("no (git) VCS found for `{}`", p.root().display()))
        })?;
    }

    // No VCS with a checked in `Cargo.toml` found, so we don't know if the
    // directory is dirty or not, thus we have to assume that it's clean.
    return Ok(None);

    fn git(p: &Package, src_files: &[PathBuf], repo: &git2::Repository) -> CargoResult<GitVcsInfo> {
        // This is a collection of any dirty or untracked files. This covers:
        // - new/modified/deleted/renamed/type change (index or worktree)
        // - untracked files (which are "new" worktree files)
        // - ignored (in case the user has an `include` directive that
        //   conflicts with .gitignore).
        let mut dirty_files = Vec::new();
        collect_statuses(repo, &mut dirty_files)?;
        // Include each submodule so that the error message can provide
        // specifically *which* files in a submodule are modified.
        status_submodules(repo, &mut dirty_files)?;

        // Find the intersection of dirty in git, and the src_files that would
        // be packaged. This is a lazy n^2 check, but seems fine with
        // thousands of files.
        let dirty_src_files: Vec<String> = src_files
            .iter()
            .filter(|src_file| dirty_files.iter().any(|path| src_file.starts_with(path)))
            .map(|path| {
                path.strip_prefix(p.root())
                    .unwrap_or(path)
                    .display()
                    .to_string()
            })
            .collect();
        if dirty_src_files.is_empty() {
            let rev_obj = repo.revparse_single("HEAD")?;
            Ok(GitVcsInfo {
                sha1: rev_obj.id().to_string(),
            })
        } else {
            anyhow::bail!(
                "{} files in the working directory contain changes that were \
                 not yet committed into git:\n\n{}\n\n\
                 to proceed despite this and include the uncommitted changes, pass the `--allow-dirty` flag",
                dirty_src_files.len(),
                dirty_src_files.join("\n")
            )
        }
    }

    // Helper to collect dirty statuses for a single repo.
    fn collect_statuses(
        repo: &git2::Repository,
        dirty_files: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        let mut status_opts = git2::StatusOptions::new();
        // Exclude submodules, as they are being handled manually by recursing
        // into each one so that details about specific files can be
        // retrieved.
        status_opts
            .exclude_submodules(true)
            .include_ignored(true)
            .include_untracked(true);
        let repo_statuses = repo.statuses(Some(&mut status_opts)).with_context(|| {
            format!(
                "failed to retrieve git status from repo {}",
                repo.path().display()
            )
        })?;
        let workdir = repo.workdir().unwrap();
        let this_dirty = repo_statuses.iter().filter_map(|entry| {
            let path = entry.path().expect("valid utf-8 path");
            if path.ends_with("Cargo.lock") && entry.status() == git2::Status::IGNORED {
                // It is OK to include Cargo.lock even if it is ignored.
                return None;
            }
            // Use an absolute path, so that comparing paths is easier
            // (particularly with submodules).
            Some(workdir.join(path))
        });
        dirty_files.extend(this_dirty);
        Ok(())
    }

    // Helper to collect dirty statuses while recursing into submodules.
    fn status_submodules(
        repo: &git2::Repository,
        dirty_files: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        for submodule in repo.submodules()? {
            // Ignore submodules that don't open, they are probably not initialized.
            // If its files are required, then the verification step should fail.
            if let Ok(sub_repo) = submodule.open() {
                status_submodules(&sub_repo, dirty_files)?;
                collect_statuses(&sub_repo, dirty_files)?;
            }
        }
        Ok(())
    }
}

/// Compresses and packages a list of [`ArchiveFile`]s and writes into the given file.
///
/// Returns the uncompressed size of the contents of the new archive file.
fn tar(
    ws: &Workspace<'_>,
    pkg: &Package,
    ar_files: Vec<ArchiveFile>,
    dst: &File,
    filename: &str,
) -> CargoResult<u64> {
    // Prepare the encoder and its header.
    let filename = Path::new(filename);
    let encoder = GzBuilder::new()
        .filename(paths::path2bytes(filename)?)
        .write(dst, Compression::best());

    // Put all package files into a compressed archive.
    let mut ar = Builder::new(encoder);
    let gctx = ws.gctx();

    let base_name = format!("{}-{}", pkg.name(), pkg.version());
    let base_path = Path::new(&base_name);
    let included = ar_files
        .iter()
        .map(|ar_file| ar_file.rel_path.clone())
        .collect::<Vec<_>>();
    let publish_pkg = prepare_for_publish(pkg, ws, &included)?;

    let mut uncompressed_size = 0;
    for ar_file in ar_files {
        let ArchiveFile {
            rel_path,
            rel_str,
            contents,
        } = ar_file;
        let ar_path = base_path.join(&rel_path);
        gctx.shell()
            .verbose(|shell| shell.status("Archiving", &rel_str))?;
        let mut header = Header::new_gnu();
        match contents {
            FileContents::OnDisk(disk_path) => {
                let mut file = File::open(&disk_path).with_context(|| {
                    format!("failed to open for archiving: `{}`", disk_path.display())
                })?;
                let metadata = file.metadata().with_context(|| {
                    format!("could not learn metadata for: `{}`", disk_path.display())
                })?;
                header.set_metadata_in_mode(&metadata, HeaderMode::Deterministic);
                header.set_cksum();
                ar.append_data(&mut header, &ar_path, &mut file)
                    .with_context(|| {
                        format!("could not archive source file `{}`", disk_path.display())
                    })?;
                uncompressed_size += metadata.len() as u64;
            }
            FileContents::Generated(generated_kind) => {
                let contents = match generated_kind {
                    GeneratedFile::Manifest => publish_pkg.manifest().to_resolved_contents()?,
                    GeneratedFile::Lockfile => build_lock(ws, &publish_pkg)?,
                    GeneratedFile::VcsInfo(ref s) => serde_json::to_string_pretty(s)?,
                };
                header.set_entry_type(EntryType::file());
                header.set_mode(0o644);
                header.set_size(contents.len() as u64);
                // use something nonzero to avoid rust-lang/cargo#9512
                header.set_mtime(1);
                header.set_cksum();
                ar.append_data(&mut header, &ar_path, contents.as_bytes())
                    .with_context(|| format!("could not archive source file `{}`", rel_str))?;
                uncompressed_size += contents.len() as u64;
            }
        }
    }

    let encoder = ar.into_inner()?;
    encoder.finish()?;
    Ok(uncompressed_size)
}

/// Generate warnings when packaging Cargo.lock, and the resolve have changed.
fn compare_resolve(
    gctx: &GlobalContext,
    current_pkg: &Package,
    orig_resolve: &Resolve,
    new_resolve: &Resolve,
) -> CargoResult<()> {
    if gctx.shell().verbosity() != Verbosity::Verbose {
        return Ok(());
    }
    let new_set: BTreeSet<PackageId> = new_resolve.iter().collect();
    let orig_set: BTreeSet<PackageId> = orig_resolve.iter().collect();
    let added = new_set.difference(&orig_set);
    // Removed entries are ignored, this is used to quickly find hints for why
    // an entry changed.
    let removed: Vec<&PackageId> = orig_set.difference(&new_set).collect();
    for pkg_id in added {
        if pkg_id.name() == current_pkg.name() && pkg_id.version() == current_pkg.version() {
            // Skip the package that is being created, since its SourceId
            // (directory) changes.
            continue;
        }
        // Check for candidates where the source has changed (such as [patch]
        // or a dependency with multiple sources like path/version).
        let removed_candidates: Vec<&PackageId> = removed
            .iter()
            .filter(|orig_pkg_id| {
                orig_pkg_id.name() == pkg_id.name() && orig_pkg_id.version() == pkg_id.version()
            })
            .cloned()
            .collect();
        let extra = match removed_candidates.len() {
            0 => {
                // This can happen if the original was out of date.
                let previous_versions: Vec<&PackageId> = removed
                    .iter()
                    .filter(|orig_pkg_id| orig_pkg_id.name() == pkg_id.name())
                    .cloned()
                    .collect();
                match previous_versions.len() {
                    0 => String::new(),
                    1 => format!(
                        ", previous version was `{}`",
                        previous_versions[0].version()
                    ),
                    _ => format!(
                        ", previous versions were: {}",
                        previous_versions
                            .iter()
                            .map(|pkg_id| format!("`{}`", pkg_id.version()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
            1 => {
                // This can happen for multi-sourced dependencies like
                // `{path="...", version="..."}` or `[patch]` replacement.
                // `[replace]` is not captured in Cargo.lock.
                format!(
                    ", was originally sourced from `{}`",
                    removed_candidates[0].source_id()
                )
            }
            _ => {
                // I don't know if there is a way to actually trigger this,
                // but handle it just in case.
                let comma_list = removed_candidates
                    .iter()
                    .map(|pkg_id| format!("`{}`", pkg_id.source_id()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    ", was originally sourced from one of these sources: {}",
                    comma_list
                )
            }
        };
        let msg = format!(
            "package `{}` added to the packaged Cargo.lock file{}",
            pkg_id, extra
        );
        gctx.shell().note(msg)?;
    }
    Ok(())
}

pub fn check_yanked(
    gctx: &GlobalContext,
    pkg_set: &PackageSet<'_>,
    resolve: &Resolve,
    hint: &str,
) -> CargoResult<()> {
    // Checking the yanked status involves taking a look at the registry and
    // maybe updating files, so be sure to lock it here.
    let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;

    let mut sources = pkg_set.sources_mut();
    let mut pending: Vec<PackageId> = resolve.iter().collect();
    let mut results = Vec::new();
    for (_id, source) in sources.sources_mut() {
        source.invalidate_cache();
    }
    while !pending.is_empty() {
        pending.retain(|pkg_id| {
            if let Some(source) = sources.get_mut(pkg_id.source_id()) {
                match source.is_yanked(*pkg_id) {
                    Poll::Ready(result) => results.push((*pkg_id, result)),
                    Poll::Pending => return true,
                }
            }
            false
        });
        for (_id, source) in sources.sources_mut() {
            source.block_until_ready()?;
        }
    }

    for (pkg_id, is_yanked) in results {
        if is_yanked? {
            gctx.shell().warn(format!(
                "package `{}` in Cargo.lock is yanked in registry `{}`, {}",
                pkg_id,
                pkg_id.source_id().display_registry_name(),
                hint
            ))?;
        }
    }
    Ok(())
}

fn run_verify(
    ws: &Workspace<'_>,
    pkg: &Package,
    tar: &FileLock,
    opts: &PackageOpts<'_>,
) -> CargoResult<()> {
    let gctx = ws.gctx();

    gctx.shell().status("Verifying", pkg)?;

    let f = GzDecoder::new(tar.file());
    let dst = tar
        .parent()
        .join(&format!("{}-{}", pkg.name(), pkg.version()));
    if dst.exists() {
        paths::remove_dir_all(&dst)?;
    }
    let mut archive = Archive::new(f);
    // We don't need to set the Modified Time, as it's not relevant to verification
    // and it errors on filesystems that don't support setting a modified timestamp
    archive.set_preserve_mtime(false);
    archive.unpack(dst.parent().unwrap())?;

    // Manufacture an ephemeral workspace to ensure that even if the top-level
    // package has a workspace we can still build our new crate.
    let id = SourceId::for_path(&dst)?;
    let mut src = PathSource::new(&dst, id, ws.gctx());
    let new_pkg = src.root_package()?;
    let pkg_fingerprint = hash_all(&dst)?;
    let ws = Workspace::ephemeral(new_pkg, gctx, None, true)?;

    let rustc_args = if pkg
        .manifest()
        .unstable_features()
        .require(Feature::public_dependency())
        .is_ok()
        || ws.gctx().cli_unstable().public_dependency
    {
        // FIXME: Turn this on at some point in the future
        //Some(vec!["-D exported_private_dependencies".to_string()])
        Some(vec![])
    } else {
        None
    };

    let exec: Arc<dyn Executor> = Arc::new(DefaultExecutor);
    ops::compile_with_exec(
        &ws,
        &ops::CompileOptions {
            build_config: BuildConfig::new(
                gctx,
                opts.jobs.clone(),
                opts.keep_going,
                &opts.targets,
                CompileMode::Build,
            )?,
            cli_features: opts.cli_features.clone(),
            spec: ops::Packages::Packages(Vec::new()),
            filter: ops::CompileFilter::Default {
                required_features_filterable: true,
            },
            target_rustdoc_args: None,
            target_rustc_args: rustc_args,
            target_rustc_crate_types: None,
            rustdoc_document_private_items: false,
            honor_rust_version: None,
        },
        &exec,
    )?;

    // Check that `build.rs` didn't modify any files in the `src` directory.
    let ws_fingerprint = hash_all(&dst)?;
    if pkg_fingerprint != ws_fingerprint {
        let changes = report_hash_difference(&pkg_fingerprint, &ws_fingerprint);
        anyhow::bail!(
            "Source directory was modified by build.rs during cargo publish. \
             Build scripts should not modify anything outside of OUT_DIR.\n\
             {}\n\n\
             To proceed despite this, pass the `--no-verify` flag.",
            changes
        )
    }

    Ok(())
}

fn hash_all(path: &Path) -> CargoResult<HashMap<PathBuf, u64>> {
    fn wrap(path: &Path) -> CargoResult<HashMap<PathBuf, u64>> {
        let mut result = HashMap::new();
        let walker = walkdir::WalkDir::new(path).into_iter();
        for entry in walker.filter_entry(|e| !(e.depth() == 1 && e.file_name() == "target")) {
            let entry = entry?;
            let file_type = entry.file_type();
            if file_type.is_file() {
                let file = File::open(entry.path())?;
                let hash = util::hex::hash_u64_file(&file)?;
                result.insert(entry.path().to_path_buf(), hash);
            } else if file_type.is_symlink() {
                let hash = util::hex::hash_u64(&fs::read_link(entry.path())?);
                result.insert(entry.path().to_path_buf(), hash);
            } else if file_type.is_dir() {
                let hash = util::hex::hash_u64(&());
                result.insert(entry.path().to_path_buf(), hash);
            }
        }
        Ok(result)
    }
    let result = wrap(path).with_context(|| format!("failed to verify output at {:?}", path))?;
    Ok(result)
}

fn report_hash_difference(orig: &HashMap<PathBuf, u64>, after: &HashMap<PathBuf, u64>) -> String {
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    for (key, value) in orig {
        match after.get(key) {
            Some(after_value) => {
                if value != after_value {
                    changed.push(key.to_string_lossy());
                }
            }
            None => removed.push(key.to_string_lossy()),
        }
    }
    let mut added: Vec<_> = after
        .keys()
        .filter(|key| !orig.contains_key(*key))
        .map(|key| key.to_string_lossy())
        .collect();
    let mut result = Vec::new();
    if !changed.is_empty() {
        changed.sort_unstable();
        result.push(format!("Changed: {}", changed.join("\n\t")));
    }
    if !added.is_empty() {
        added.sort_unstable();
        result.push(format!("Added: {}", added.join("\n\t")));
    }
    if !removed.is_empty() {
        removed.sort_unstable();
        result.push(format!("Removed: {}", removed.join("\n\t")));
    }
    assert!(!result.is_empty(), "unexpected empty change detection");
    result.join("\n")
}

// It can often be the case that files of a particular name on one platform
// can't actually be created on another platform. For example files with colons
// in the name are allowed on Unix but not on Windows.
//
// To help out in situations like this, issue about weird filenames when
// packaging as a "heads up" that something may not work on other platforms.
fn check_filename(file: &Path, shell: &mut Shell) -> CargoResult<()> {
    let Some(name) = file.file_name() else {
        return Ok(());
    };
    let Some(name) = name.to_str() else {
        anyhow::bail!(
            "path does not have a unicode filename which may not unpack \
             on all platforms: {}",
            file.display()
        )
    };
    let bad_chars = ['/', '\\', '<', '>', ':', '"', '|', '?', '*'];
    if let Some(c) = bad_chars.iter().find(|c| name.contains(**c)) {
        anyhow::bail!(
            "cannot package a filename with a special character `{}`: {}",
            c,
            file.display()
        )
    }
    if restricted_names::is_windows_reserved_path(file) {
        shell.warn(format!(
            "file {} is a reserved Windows filename, \
                it will not work on Windows platforms",
            file.display()
        ))?;
    }
    Ok(())
}
