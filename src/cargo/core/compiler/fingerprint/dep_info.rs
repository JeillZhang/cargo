//! Types and functions managing dep-info files.
//! For more, see [the documentation] in the `fingerprint` module.
//!
//! [the documentation]: crate::core::compiler::fingerprint#dep-info-files

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::str;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::bail;
use cargo_util::paths;
use cargo_util::ProcessBuilder;
use cargo_util::Sha256;

use crate::CargoResult;
use crate::CARGO_ENV;

/// The representation of the `.d` dep-info file generated by rustc
#[derive(Default)]
pub struct RustcDepInfo {
    /// The list of files that the main target in the dep-info file depends on.
    ///
    /// The optional checksums are parsed from the special `# checksum:...` comments.
    pub files: HashMap<PathBuf, Option<(u64, Checksum)>>,
    /// The list of environment variables we found that the rustc compilation
    /// depends on.
    ///
    /// The first element of the pair is the name of the env var and the second
    /// item is the value. `Some` means that the env var was set, and `None`
    /// means that the env var wasn't actually set and the compilation depends
    /// on it not being set.
    ///
    /// These are from the special `# env-var:...` comments.
    pub env: Vec<(String, Option<String>)>,
}

/// Tells the associated path in [`EncodedDepInfo::files`] is relative to package root,
/// target root, or absolute.
#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone)]
pub enum DepInfoPathType {
    /// src/, e.g. src/lib.rs
    PackageRootRelative,
    /// target/debug/deps/lib...
    /// or an absolute path /.../sysroot/...
    TargetRootRelative,
}

/// Same as [`RustcDepInfo`] except avoids absolute paths as much as possible to
/// allow moving around the target directory.
///
/// This is also stored in an optimized format to make parsing it fast because
/// Cargo will read it for crates on all future compilations.
///
/// Currently the format looks like:
///
/// ```text
/// +------------+------------+---------------+---------------+
/// | # of files | file paths | # of env vars | env var pairs |
/// +------------+------------+---------------+---------------+
/// ```
///
/// Each field represents
///
/// * _Number of files/envs_ --- A `u32` representing the number of things.
/// * _File paths_ --- Zero or more paths of files the dep-info file depends on.
///   Each path is encoded as the following:
///
///   ```text
///   +-----------+-------------+------------+---------------+-----------+-------+
///   | Path type | len of path | path bytes | cksum exists? | file size | cksum |
///   +-----------+-------------+------------+---------------+-----------+-------+
///   ```
/// * _Env var pairs_ --- Zero or more env vars the dep-info file depends on.
///   Each env key-value pair is encoded as the following:
///   ```text
///   +------------+-----------+---------------+--------------+-------------+
///   | len of key | key bytes | value exists? | len of value | value bytes |
///   +------------+-----------+---------------+--------------+-------------+
///   ```
#[derive(Default)]
pub struct EncodedDepInfo {
    pub files: Vec<(DepInfoPathType, PathBuf, Option<(u64, String)>)>,
    pub env: Vec<(String, Option<String>)>,
}

impl EncodedDepInfo {
    pub fn parse(mut bytes: &[u8]) -> Option<EncodedDepInfo> {
        let bytes = &mut bytes;
        let nfiles = read_usize(bytes)?;
        let mut files = Vec::with_capacity(nfiles);
        for _ in 0..nfiles {
            let ty = match read_u8(bytes)? {
                0 => DepInfoPathType::PackageRootRelative,
                1 => DepInfoPathType::TargetRootRelative,
                _ => return None,
            };
            let path_bytes = read_bytes(bytes)?;
            let path = paths::bytes2path(path_bytes).ok()?;
            let has_checksum = read_bool(bytes)?;
            let checksum_info = has_checksum
                .then(|| {
                    let file_len = read_u64(bytes);
                    let checksum_string = read_bytes(bytes)
                        .map(Vec::from)
                        .and_then(|v| String::from_utf8(v).ok());
                    file_len.zip(checksum_string)
                })
                .flatten();
            files.push((ty, path, checksum_info));
        }

        let nenv = read_usize(bytes)?;
        let mut env = Vec::with_capacity(nenv);
        for _ in 0..nenv {
            let key = str::from_utf8(read_bytes(bytes)?).ok()?.to_string();
            let val = match read_u8(bytes)? {
                0 => None,
                1 => Some(str::from_utf8(read_bytes(bytes)?).ok()?.to_string()),
                _ => return None,
            };
            env.push((key, val));
        }
        return Some(EncodedDepInfo { files, env });

        fn read_usize(bytes: &mut &[u8]) -> Option<usize> {
            let ret = bytes.get(..4)?;
            *bytes = &bytes[4..];
            Some(u32::from_le_bytes(ret.try_into().unwrap()) as usize)
        }

        fn read_u64(bytes: &mut &[u8]) -> Option<u64> {
            let ret = bytes.get(..8)?;
            *bytes = &bytes[8..];
            Some(u64::from_le_bytes(ret.try_into().unwrap()))
        }

        fn read_bool(bytes: &mut &[u8]) -> Option<bool> {
            read_u8(bytes).map(|b| b != 0)
        }

        fn read_u8(bytes: &mut &[u8]) -> Option<u8> {
            let ret = *bytes.get(0)?;
            *bytes = &bytes[1..];
            Some(ret)
        }

        fn read_bytes<'a>(bytes: &mut &'a [u8]) -> Option<&'a [u8]> {
            let n = read_usize(bytes)? as usize;
            let ret = bytes.get(..n)?;
            *bytes = &bytes[n..];
            Some(ret)
        }
    }

    pub fn serialize(&self) -> CargoResult<Vec<u8>> {
        let mut ret = Vec::new();
        let dst = &mut ret;
        write_usize(dst, self.files.len());
        for (ty, file, checksum_info) in self.files.iter() {
            match ty {
                DepInfoPathType::PackageRootRelative => dst.push(0),
                DepInfoPathType::TargetRootRelative => dst.push(1),
            }
            write_bytes(dst, paths::path2bytes(file)?);
            write_bool(dst, checksum_info.is_some());
            if let Some((len, checksum)) = checksum_info {
                write_u64(dst, *len);
                write_bytes(dst, checksum);
            }
        }

        write_usize(dst, self.env.len());
        for (key, val) in self.env.iter() {
            write_bytes(dst, key);
            match val {
                None => dst.push(0),
                Some(val) => {
                    dst.push(1);
                    write_bytes(dst, val);
                }
            }
        }
        return Ok(ret);

        fn write_bytes(dst: &mut Vec<u8>, val: impl AsRef<[u8]>) {
            let val = val.as_ref();
            write_usize(dst, val.len());
            dst.extend_from_slice(val);
        }

        fn write_usize(dst: &mut Vec<u8>, val: usize) {
            dst.extend(&u32::to_le_bytes(val as u32));
        }

        fn write_u64(dst: &mut Vec<u8>, val: u64) {
            dst.extend(&u64::to_le_bytes(val));
        }

        fn write_bool(dst: &mut Vec<u8>, val: bool) {
            dst.push(u8::from(val));
        }
    }
}

/// Parses the dep-info file coming out of rustc into a Cargo-specific format.
///
/// This function will parse `rustc_dep_info` as a makefile-style dep info to
/// learn about the all files which a crate depends on. This is then
/// re-serialized into the `cargo_dep_info` path in a Cargo-specific format.
///
/// The `pkg_root` argument here is the absolute path to the directory
/// containing `Cargo.toml` for this crate that was compiled. The paths listed
/// in the rustc dep-info file may or may not be absolute but we'll want to
/// consider all of them relative to the `root` specified.
///
/// The `rustc_cwd` argument is the absolute path to the cwd of the compiler
/// when it was invoked.
///
/// If the `allow_package` argument is true, then package-relative paths are
/// included. If it is false, then package-relative paths are skipped and
/// ignored (typically used for registry or git dependencies where we assume
/// the source never changes, and we don't want the cost of running `stat` on
/// all those files). See the module-level docs for the note about
/// `-Zbinary-dep-depinfo` for more details on why this is done.
///
/// The serialized Cargo format will contain a list of files, all of which are
/// relative if they're under `root`. or absolute if they're elsewhere.
///
/// The `env_config` argument is a set of environment variables that are
/// defined in `[env]` table of the `config.toml`.
pub fn translate_dep_info(
    rustc_dep_info: &Path,
    cargo_dep_info: &Path,
    rustc_cwd: &Path,
    pkg_root: &Path,
    target_root: &Path,
    rustc_cmd: &ProcessBuilder,
    allow_package: bool,
    env_config: &Arc<HashMap<String, OsString>>,
) -> CargoResult<()> {
    let depinfo = parse_rustc_dep_info(rustc_dep_info)?;

    let target_root = crate::util::try_canonicalize(target_root)?;
    let pkg_root = crate::util::try_canonicalize(pkg_root)?;
    let mut on_disk_info = EncodedDepInfo::default();
    on_disk_info.env = depinfo.env;

    // This is a bit of a tricky statement, but here we're *removing* the
    // dependency on environment variables that were defined specifically for
    // the command itself. Environment variables returned by `get_envs` includes
    // environment variables like:
    //
    // * `OUT_DIR` if applicable
    // * env vars added by a build script, if any
    //
    // The general idea here is that the dep info file tells us what, when
    // changed, should cause us to rebuild the crate. These environment
    // variables are synthesized by Cargo and/or the build script, and the
    // intention is that their values are tracked elsewhere for whether the
    // crate needs to be rebuilt.
    //
    // For example a build script says when it needs to be rerun and otherwise
    // it's assumed to produce the same output, so we're guaranteed that env
    // vars defined by the build script will always be the same unless the build
    // script itself reruns, in which case the crate will rerun anyway.
    //
    // For things like `OUT_DIR` it's a bit sketchy for now. Most of the time
    // that's used for code generation but this is technically buggy where if
    // you write a binary that does `println!("{}", env!("OUT_DIR"))` we won't
    // recompile that if you move the target directory. Hopefully that's not too
    // bad of an issue for now...
    //
    // This also includes `CARGO` since if the code is explicitly wanting to
    // know that path, it should be rebuilt if it changes. The CARGO path is
    // not tracked elsewhere in the fingerprint.
    //
    // For cargo#13280, We trace env vars that are defined in the `[env]` config table.
    on_disk_info.env.retain(|(key, _)| {
        env_config.contains_key(key) || !rustc_cmd.get_envs().contains_key(key) || key == CARGO_ENV
    });

    let serialize_path = |file| {
        // The path may be absolute or relative, canonical or not. Make sure
        // it is canonicalized so we are comparing the same kinds of paths.
        let abs_file = rustc_cwd.join(file);
        // If canonicalization fails, just use the abs path. There is currently
        // a bug where --remap-path-prefix is affecting .d files, causing them
        // to point to non-existent paths.
        let canon_file =
            crate::util::try_canonicalize(&abs_file).unwrap_or_else(|_| abs_file.clone());

        let (ty, path) = if let Ok(stripped) = canon_file.strip_prefix(&target_root) {
            (DepInfoPathType::TargetRootRelative, stripped)
        } else if let Ok(stripped) = canon_file.strip_prefix(&pkg_root) {
            if !allow_package {
                return None;
            }
            (DepInfoPathType::PackageRootRelative, stripped)
        } else {
            // It's definitely not target root relative, but this is an absolute path (since it was
            // joined to rustc_cwd) and as such re-joining it later to the target root will have no
            // effect.
            (DepInfoPathType::TargetRootRelative, &*abs_file)
        };
        Some((ty, path.to_owned()))
    };

    for (file, checksum_info) in depinfo.files {
        let Some((path_type, path)) = serialize_path(file) else {
            continue;
        };
        on_disk_info.files.push((
            path_type,
            path,
            checksum_info.map(|(len, checksum)| (len, checksum.to_string())),
        ));
    }
    paths::write(cargo_dep_info, on_disk_info.serialize()?)?;
    Ok(())
}

/// Parse the `.d` dep-info file generated by rustc.
pub fn parse_rustc_dep_info(rustc_dep_info: &Path) -> CargoResult<RustcDepInfo> {
    let contents = paths::read(rustc_dep_info)?;
    let mut ret = RustcDepInfo::default();
    let mut found_deps = false;

    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("# env-dep:") {
            let mut parts = rest.splitn(2, '=');
            let Some(env_var) = parts.next() else {
                continue;
            };
            let env_val = match parts.next() {
                Some(s) => Some(unescape_env(s)?),
                None => None,
            };
            ret.env.push((unescape_env(env_var)?, env_val));
        } else if let Some(pos) = line.find(": ") {
            if found_deps {
                continue;
            }
            found_deps = true;
            let mut deps = line[pos + 2..].split_whitespace();

            while let Some(s) = deps.next() {
                let mut file = s.to_string();
                while file.ends_with('\\') {
                    file.pop();
                    file.push(' ');
                    file.push_str(deps.next().ok_or_else(|| {
                        crate::util::internal("malformed dep-info format, trailing \\")
                    })?);
                }
                ret.files.entry(file.into()).or_default();
            }
        } else if let Some(rest) = line.strip_prefix("# checksum:") {
            let mut parts = rest.splitn(3, ' ');
            let Some(checksum) = parts.next().map(Checksum::from_str).transpose()? else {
                continue;
            };
            let Some(Ok(file_len)) = parts
                .next()
                .and_then(|s| s.strip_prefix("file_len:").map(|s| s.parse::<u64>()))
            else {
                continue;
            };
            let Some(path) = parts.next().map(PathBuf::from) else {
                continue;
            };

            ret.files.insert(path, Some((file_len, checksum)));
        }
    }
    return Ok(ret);

    // rustc tries to fit env var names and values all on a single line, which
    // means it needs to escape `\r` and `\n`. The escape syntax used is "\n"
    // which means that `\` also needs to be escaped.
    fn unescape_env(s: &str) -> CargoResult<String> {
        let mut ret = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                ret.push(c);
                continue;
            }
            match chars.next() {
                Some('\\') => ret.push('\\'),
                Some('n') => ret.push('\n'),
                Some('r') => ret.push('\r'),
                Some(c) => bail!("unknown escape character `{}`", c),
                None => bail!("unterminated escape character"),
            }
        }
        Ok(ret)
    }
}

/// Parses Cargo's internal [`EncodedDepInfo`] structure that was previously
/// serialized to disk.
///
/// Note that this is not rustc's `*.d` files.
///
/// Also note that rustc's `*.d` files are translated to Cargo-specific
/// `EncodedDepInfo` files after compilations have finished in
/// [`translate_dep_info`].
///
/// Returns `None` if the file is corrupt or couldn't be read from disk. This
/// indicates that the crate should likely be rebuilt.
pub fn parse_dep_info(
    pkg_root: &Path,
    target_root: &Path,
    dep_info: &Path,
) -> CargoResult<Option<RustcDepInfo>> {
    let Ok(data) = paths::read_bytes(dep_info) else {
        return Ok(None);
    };
    let Some(info) = EncodedDepInfo::parse(&data) else {
        tracing::warn!("failed to parse cargo's dep-info at {:?}", dep_info);
        return Ok(None);
    };
    let mut ret = RustcDepInfo::default();
    ret.env = info.env;
    ret.files
        .extend(info.files.into_iter().map(|(ty, path, checksum_info)| {
            (
                make_absolute_path(ty, pkg_root, target_root, path),
                checksum_info.and_then(|(file_len, checksum)| {
                    Checksum::from_str(&checksum).ok().map(|c| (file_len, c))
                }),
            )
        }));
    Ok(Some(ret))
}

fn make_absolute_path(
    ty: DepInfoPathType,
    pkg_root: &Path,
    target_root: &Path,
    path: PathBuf,
) -> PathBuf {
    match ty {
        DepInfoPathType::PackageRootRelative => pkg_root.join(path),
        // N.B. path might be absolute here in which case the join will have no effect
        DepInfoPathType::TargetRootRelative => target_root.join(path),
    }
}

/// Some algorithms are here to ensure compatibility with possible rustc outputs.
/// The presence of an algorithm here is not a suggestion that it's fit for use.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ChecksumAlgo {
    Sha256,
    Blake3,
}

impl ChecksumAlgo {
    fn hash_len(&self) -> usize {
        match self {
            ChecksumAlgo::Sha256 | ChecksumAlgo::Blake3 => 32,
        }
    }
}

impl FromStr for ChecksumAlgo {
    type Err = InvalidChecksum;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sha256" => Ok(Self::Sha256),
            "blake3" => Ok(Self::Blake3),
            _ => Err(InvalidChecksum::InvalidChecksumAlgo),
        }
    }
}

impl fmt::Display for ChecksumAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ChecksumAlgo::Sha256 => "sha256",
            ChecksumAlgo::Blake3 => "blake3",
        })
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Checksum {
    algo: ChecksumAlgo,
    /// If the algorithm uses fewer than 32 bytes, then the remaining bytes will be zero.
    value: [u8; 32],
}

impl Checksum {
    pub fn new(algo: ChecksumAlgo, value: [u8; 32]) -> Self {
        Self { algo, value }
    }

    pub fn compute(algo: ChecksumAlgo, contents: impl Read) -> Result<Self, io::Error> {
        // Buffer size is the recommended amount to fully leverage SIMD instructions on AVX-512 as per
        // blake3 documentation.
        let mut buf = vec![0; 16 * 1024];
        let mut ret = Self {
            algo,
            value: [0; 32],
        };
        let len = algo.hash_len();
        let value = &mut ret.value[..len];

        fn digest<T>(
            mut hasher: T,
            mut update: impl FnMut(&mut T, &[u8]),
            finish: impl FnOnce(T, &mut [u8]),
            mut contents: impl Read,
            buf: &mut [u8],
            value: &mut [u8],
        ) -> Result<(), io::Error> {
            loop {
                let bytes_read = contents.read(buf)?;
                if bytes_read == 0 {
                    break;
                }
                update(&mut hasher, &buf[0..bytes_read]);
            }
            finish(hasher, value);
            Ok(())
        }

        match algo {
            ChecksumAlgo::Sha256 => {
                digest(
                    Sha256::new(),
                    |h, b| {
                        h.update(b);
                    },
                    |mut h, out| out.copy_from_slice(&h.finish()),
                    contents,
                    &mut buf,
                    value,
                )?;
            }
            ChecksumAlgo::Blake3 => {
                digest(
                    blake3::Hasher::new(),
                    |h, b| {
                        h.update(b);
                    },
                    |h, out| out.copy_from_slice(h.finalize().as_bytes()),
                    contents,
                    &mut buf,
                    value,
                )?;
            }
        }
        Ok(ret)
    }

    pub fn algo(&self) -> ChecksumAlgo {
        self.algo
    }

    pub fn value(&self) -> &[u8; 32] {
        &self.value
    }
}

impl FromStr for Checksum {
    type Err = InvalidChecksum;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('=');
        let Some(algo) = parts.next().map(ChecksumAlgo::from_str).transpose()? else {
            return Err(InvalidChecksum::InvalidFormat);
        };
        let Some(checksum) = parts.next() else {
            return Err(InvalidChecksum::InvalidFormat);
        };
        let mut value = [0; 32];
        if hex::decode_to_slice(checksum, &mut value[0..algo.hash_len()]).is_err() {
            return Err(InvalidChecksum::InvalidChecksum(algo));
        }
        Ok(Self { algo, value })
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut checksum = [0; 64];
        let hash_len = self.algo.hash_len();
        hex::encode_to_slice(&self.value[0..hash_len], &mut checksum[0..(hash_len * 2)])
            .map_err(|_| fmt::Error)?;
        write!(
            f,
            "{}={}",
            self.algo,
            str::from_utf8(&checksum[0..(hash_len * 2)]).unwrap_or_default()
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidChecksum {
    #[error("algorithm portion incorrect, expected `sha256`, or `blake3`")]
    InvalidChecksumAlgo,
    #[error("expected {} hexadecimal digits in checksum portion", .0.hash_len() * 2)]
    InvalidChecksum(ChecksumAlgo),
    #[error("expected a string with format \"algorithm=hex_checksum\"")]
    InvalidFormat,
}
