use super::*;
use std::io::Seek;

pub(super) fn pointer(data: &[u8]) -> Result<Option<(String, u64)>> {
    if !data.starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
        return Ok(None);
    }
    let re = regex::Regex::new(
        r"^version https://git-lfs.github.com/spec/v1\noid sha256:([0-9a-f]{64})\nsize ([0-9]+)\n?$",
    )?;
    let raw = std::str::from_utf8(data)?;
    let captures = re
        .captures(raw)
        .ok_or("Unsupported or malformed Git LFS pointer")?;
    Ok(Some((captures[1].into(), captures[2].parse()?)))
}
fn modules(repo: &Path, commit: &str) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for record in git_bytes(repo, &["ls-tree", "-r", "-z", commit])?
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
    {
        let raw = std::str::from_utf8(record)?;
        let (meta, path) = raw.split_once('\t').ok_or("Invalid Git tree")?;
        let fields: Vec<_> = meta.split_whitespace().collect();
        if fields.first() == Some(&"160000") {
            if Path::new(path).is_absolute()
                || Path::new(path)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err("Unsafe submodule path".into());
            }
            result.insert(
                path.into(),
                fields.get(2).ok_or("Missing gitlink")?.to_string(),
            );
        }
    }
    Ok(result)
}
fn checkout(repo: &Path, path: &str, commit: &str) -> Result<PathBuf> {
    let mut candidates = vec![repo.to_path_buf()];
    let list = git(repo, &["worktree", "list", "--porcelain", "-z"])?;
    candidates.extend(
        list.split('\0')
            .filter_map(|r| r.strip_prefix("worktree "))
            .map(PathBuf::from),
    );
    for parent in candidates {
        let Ok(child) = parent.join(path).canonicalize() else {
            continue;
        };
        if git(&child, &["rev-parse", "--show-toplevel"])
            .ok()
            .and_then(|s| PathBuf::from(s).canonicalize().ok())
            != Some(child.clone())
        {
            continue;
        }
        if git(&child, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_ok() {
            return Ok(child);
        }
    }
    Err(format!("Required exact local submodule checkout missing: {path}").into())
}

// Match tarfile.PAX_FORMAT byte-for-byte: Python's rewritten archive is part of
// existing request identities. Native git still selects the committed export.
fn field(header: &mut [u8; 512], start: usize, width: usize, value: &str) {
    for (target, c) in header[start..start + width].iter_mut().zip(value.chars()) {
        *target = if c.is_ascii() { c as u8 } else { b'?' };
    }
}
fn octal(header: &mut [u8; 512], start: usize, width: usize, value: u64) -> Result<()> {
    let value = if value >= 1u64 << (3 * (width - 1)) {
        0 // The original value is retained in a PAX numeric extension.
    } else {
        value
    };
    let value = format!("{value:0width$o}", width = width - 1);
    if value.len() >= width {
        return Err("Archive numeric field requires unsupported extended encoding".into());
    }
    field(header, start, width, &value);
    Ok(())
}
#[derive(Default)]
struct Header {
    name: String,
    link: String,
    size: u64,
    mode: u64,
    uid: u64,
    gid: u64,
    mtime: u64,
    kind: u8,
    uname: String,
    gname: String,
    pax: Vec<(String, String)>,
}
impl Header {
    fn bytes(&self) -> Result<[u8; 512]> {
        let mut b = [0; 512];
        field(&mut b, 0, 100, &self.name);
        octal(&mut b, 100, 8, self.mode & 0o7777)?;
        octal(&mut b, 108, 8, self.uid)?;
        octal(&mut b, 116, 8, self.gid)?;
        octal(&mut b, 124, 12, self.size)?;
        octal(&mut b, 136, 12, self.mtime)?;
        b[148..156].fill(b' ');
        b[156] = self.kind;
        field(&mut b, 157, 100, &self.link);
        b[257..263].copy_from_slice(b"ustar\0");
        b[263..265].copy_from_slice(b"00");
        field(&mut b, 265, 32, &self.uname);
        field(&mut b, 297, 32, &self.gname);
        let checksum: u64 = b.iter().map(|v| *v as u64).sum();
        field(&mut b, 148, 8, &format!("{checksum:06o}\0 "));
        Ok(b)
    }
}
fn padded(output: &mut File, data: &[u8]) -> Result<()> {
    output.write_all(data)?;
    let pad = (512 - data.len() % 512) % 512;
    output.write_all(&vec![0; pad])?;
    Ok(())
}
fn pax(output: &mut File, fields: &[(String, String)], kind: u8) -> Result<()> {
    if fields.is_empty() {
        return Ok(());
    }
    let mut content = String::new();
    for (key, value) in fields {
        let base = key.len() + value.len() + 3;
        let mut size = base + 1;
        loop {
            let next = base + size.to_string().len();
            if next == size {
                break;
            }
            size = next;
        }
        content.push_str(&format!("{size} {key}={value}\n"));
    }
    let header = Header {
        name: "././@PaxHeader".into(),
        size: content.len() as u64,
        kind,
        ..Default::default()
    };
    output.write_all(&header.bytes()?)?;
    padded(output, content.as_bytes())
}
fn append_header(output: &mut File, header: &mut Header) -> Result<()> {
    for (key, value, width) in [
        ("path", &header.name, 100),
        ("linkpath", &header.link, 100),
        ("uname", &header.uname, 32),
        ("gname", &header.gname, 32),
    ] {
        if (!value.is_ascii() || value.len() > width) && !header.pax.iter().any(|(k, _)| k == key) {
            header.pax.push((key.into(), value.clone()));
        }
    }
    for (key, value, width) in [
        ("uid", header.uid, 8),
        ("gid", header.gid, 8),
        ("size", header.size, 12),
        ("mtime", header.mtime, 12),
    ] {
        if value >= 1u64 << (3 * (width - 1)) && !header.pax.iter().any(|(k, _)| k == key) {
            header.pax.push((key.into(), value.to_string()));
        }
    }
    pax(output, &header.pax, b'x')?;
    output.write_all(&header.bytes()?)?;
    Ok(())
}

#[test]
fn large_lfs_sizes_use_python_pax_numeric_encoding() {
    let mut output = tempfile::tempfile().unwrap();
    let mut header = Header {
        name: "large".into(),
        size: 1 << 33,
        kind: b'0',
        ..Default::default()
    };
    append_header(&mut output, &mut header).unwrap();
    output.rewind().unwrap();
    let mut bytes = Vec::new();
    output.read_to_end(&mut bytes).unwrap();
    assert_eq!(&bytes[512..531], b"19 size=8589934592\n");
    assert_eq!(&bytes[1024 + 124..1024 + 136], b"00000000000\0");
}

struct Closure<'a> {
    output: &'a mut File,
    temporary: &'a Path,
    stats: Value,
    sequence: u64,
    media: BTreeMap<PathBuf, PathBuf>,
}
impl Closure<'_> {
    fn append(&mut self, repo: &Path, commit: &str, prefix: &str, depth: u8) -> Result<()> {
        if depth > 8 {
            return Err("Nested submodule depth exceeds supported source closure".into());
        }
        let modules = modules(repo, commit)?;
        self.sequence += 1;
        let source = self.temporary.join(format!("git-{}.tar", self.sequence));
        let keys = git(repo, &["config", "--null", "--name-only", "--list"])?;
        let mut args = Vec::<String>::new();
        let mut filters = BTreeSet::new();
        for key in keys.split('\0') {
            if matches(r"^filter\..+\.(smudge|process|required)$", key) {
                filters.insert(key.rsplit_once('.').ok_or("Invalid filter")?.0.to_string());
            }
        }
        for filter in filters {
            for suffix in ["smudge=", "process=", "required=false"] {
                args.extend(["-c".into(), format!("{filter}.{suffix}")]);
            }
        }
        args.extend([
            "archive".into(),
            "--format=tar".into(),
            format!("--output={}", source.display()),
            commit.into(),
        ]);
        git(repo, &args.iter().map(String::as_str).collect::<Vec<_>>())?;
        let mut archive = tar::Archive::new(File::open(&source)?);
        let mut exported = BTreeSet::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.header().entry_type().is_pax_global_extensions() {
                continue;
            }
            let path = entry.path()?.to_string_lossy().to_string();
            let relative = path.trim_end_matches('/');
            if modules.contains_key(relative) {
                exported.insert(relative.to_string());
                continue;
            }
            let h = entry.header();
            let kind = h.entry_type();
            let mut header = Header {
                name: format!("{prefix}{path}"),
                link: entry
                    .link_name()?
                    .map(|l| l.to_string_lossy().to_string())
                    .unwrap_or_default(),
                size: entry.size(),
                mode: h.mode()? as u64,
                uid: h.uid()?,
                gid: h.gid()?,
                mtime: h.mtime()?,
                kind: kind.as_byte(),
                uname: h.username()?.unwrap_or("").into(),
                gname: h.groupname()?.unwrap_or("").into(),
                pax: Vec::new(),
            };
            if kind.is_hard_link() {
                header.link = format!("{prefix}{}", header.link);
            }
            if kind.is_dir() && !header.name.ends_with('/') {
                header.name.push('/');
            }
            if let Some(fields) = entry.pax_extensions()? {
                for field in fields {
                    let field = field?;
                    let key = field.key()?;
                    if !["comment", "path", "linkpath"].contains(&key) {
                        header.pax.push((key.into(), field.value()?.into()));
                    }
                }
            }
            if !kind.is_file() {
                header.size = 0;
                append_header(self.output, &mut header)?;
                continue;
            }
            if header.size <= 4096 {
                let mut small = Vec::new();
                entry.read_to_end(&mut small)?;
                if let Some((oid, size)) = pointer(&small)? {
                    if !self.media.contains_key(repo) {
                        let env = git(repo, &["lfs", "env"])?;
                        let path = env
                            .lines()
                            .find_map(|l| l.strip_prefix("LocalMediaDir="))
                            .ok_or("Cannot locate existing Git LFS cache")?;
                        self.media.insert(repo.into(), PathBuf::from(path));
                    }
                    let cached = self.media[repo].join(&oid[..2]).join(&oid[2..4]).join(&oid);
                    if !fs::symlink_metadata(&cached)?.file_type().is_file() {
                        return Err("Local Git LFS object is not a regular file".into());
                    }
                    let mut snapshot = tempfile::tempfile_in(self.temporary)?;
                    let mut source = File::open(cached)?;
                    let count = std::io::copy(
                        &mut (&mut source).take(size.saturating_add(1)),
                        &mut snapshot,
                    )?;
                    if count != size {
                        return Err("Git LFS size mismatch".into());
                    }
                    snapshot.rewind()?;
                    let mut hasher = Sha256::new();
                    let mut buffer = [0; 65536];
                    loop {
                        let n = snapshot.read(&mut buffer)?;
                        if n == 0 {
                            break;
                        }
                        hasher.update(&buffer[..n]);
                    }
                    if format!("{:x}", hasher.finalize()) != oid {
                        return Err("Git LFS content mismatch".into());
                    }
                    snapshot.rewind()?;
                    header.size = size;
                    append_header(self.output, &mut header)?;
                    std::io::copy(&mut snapshot, self.output)?;
                    self.output
                        .write_all(&vec![0; ((512 - size % 512) % 512) as usize])?;
                    self.stats["lfs_objects"] = json!(number(&self.stats, "lfs_objects") + 1);
                    self.stats["lfs_bytes"] = json!(number(&self.stats, "lfs_bytes") + size);
                } else {
                    append_header(self.output, &mut header)?;
                    padded(self.output, &small)?;
                }
            } else {
                append_header(self.output, &mut header)?;
                std::io::copy(&mut entry, self.output)?;
                self.output
                    .write_all(&vec![0; ((512 - header.size % 512) % 512) as usize])?;
            }
        }
        for path in exported {
            let revision = &modules[&path];
            let child = checkout(repo, &path, revision)?;
            let full = format!("{prefix}{path}");
            self.stats["submodules"]
                .as_array_mut()
                .ok_or("Invalid submodule stats")?
                .push(json!({"path":full,"commit":revision}));
            self.append(&child, revision, &format!("{full}/"), depth + 1)?;
        }
        Ok(())
    }
}
pub(super) fn create(repo: &Path, commit: &str, destination: &Path) -> Result<Value> {
    if !exact_sha(commit) {
        return Err("Source archive requires an exact commit".into());
    }
    let temporary = tempfile::Builder::new()
        .prefix("source-closure-")
        .tempdir_in(destination.parent().ok_or("Archive parent missing")?)?;
    let path = temporary.path().join("complete.tar");
    let mut output = File::create(&path)?;
    pax(&mut output, &[("comment".into(), commit.into())], b'g')?;
    let mut closure = Closure {
        output: &mut output,
        temporary: temporary.path(),
        stats: json!({"submodules":[],"lfs_objects":0,"lfs_bytes":0}),
        sequence: 0,
        media: BTreeMap::new(),
    };
    closure.append(&repo.canonicalize()?, commit, "", 0)?;
    let stats = closure.stats;
    output.write_all(&[0; 1024])?;
    let length = output.stream_position()?;
    let pad = (10240 - length % 10240) % 10240;
    output.write_all(&vec![0; pad as usize])?;
    output.sync_all()?;
    fs::rename(path, destination)?;
    Ok(stats)
}
pub(super) fn bundle(repo: &Path, commit: &str, destination: &Path) -> Result<()> {
    if !exact_sha(commit) || git(repo, &["rev-parse", "--is-shallow-repository"])? != "false" {
        return Err("Git bundle requires exact commit and complete local ancestry".into());
    }
    git(repo, &["cat-file", "-e", &format!("{commit}^{{commit}}")])?;
    let objects = git(
        repo,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
    )?;
    if objects.contains(['\r', '\n']) {
        return Err("Unsupported local Git object path".into());
    }
    let isolated = tempfile::tempdir_in(destination.parent().ok_or("Bundle parent missing")?)?;
    let root = isolated.path();
    git(root, &["init", "--bare", "--template=", "--quiet"])?;
    let alternates = root.join("objects/info/alternates");
    fs::write(&alternates, format!("{objects}\n"))?;
    git(root, &["update-ref", "refs/heads/source", commit])?;
    let destination = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        std::env::current_dir()?.join(destination)
    };
    let path = destination.to_string_lossy();
    git(
        root,
        &[
            "-c",
            "pack.threads=1",
            "bundle",
            "create",
            "--version=2",
            &path,
            "refs/heads/source",
        ],
    )?;
    if git(root, &["bundle", "list-heads", &path])? != format!("{commit} refs/heads/source") {
        return Err("Bundle advertises unexpected source identity".into());
    }
    fs::remove_file(alternates)?;
    git(root, &["bundle", "verify", &path])?;
    Ok(())
}
