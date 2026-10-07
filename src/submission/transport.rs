use super::*;

#[derive(Debug)]
pub(super) struct Response {
    pub status: u16,
    pub data: Vec<u8>,
}

// curl performs one bounded native HTTP transfer. Credentials are private file
// contents, never command-line arguments. Redirects and retries are disabled.
pub(super) fn http(
    url: &str,
    token: Option<&str>,
    body: Option<&Value>,
    limit: u64,
) -> Result<Response> {
    url::Url::parse(url)?;
    let mut headers = tempfile::NamedTempFile::new()?;
    writeln!(
        headers,
        "User-Agent: ccid\nAccept: application/json\nContent-Type: application/json"
    )?;
    if let Some(token) = token {
        if token.contains(['\r', '\n']) {
            return Err("Invalid HTTP credential".into());
        }
        writeln!(headers, "Authorization: Bearer {token}")?;
    }
    let mut config = tempfile::NamedTempFile::new()?;
    if url.contains(['\r', '\n']) {
        return Err("Invalid request URL".into());
    }
    writeln!(
        config,
        "url = \"{}\"",
        url.replace('\\', "\\\\").replace('"', "\\\"")
    )?;
    let response = tempfile::NamedTempFile::new()?;
    let mut argv = strings(&[
        "curl",
        "--disable",
        "--silent",
        "--show-error",
        "--retry",
        "0",
        "--max-time",
        "60",
        "--connect-timeout",
        "10",
        "--proto",
        "=https",
        "--max-filesize",
        &limit.to_string(),
        "--config",
    ]);
    argv.push(config.path().to_string_lossy().into_owned());
    argv.extend(strings(&[
        "--header",
        &format!("@{}", headers.path().display()),
        "--output",
        &response.path().to_string_lossy(),
        "--write-out",
        "%{http_code}",
    ]));
    let encoded = body.map(encode).transpose()?;
    if body.is_some() {
        argv.extend(strings(&["--request", "POST", "--data-binary", "@-"]));
    }
    let status: u16 = String::from_utf8(
        output(&argv, None, encoded.as_deref().map(str::as_bytes))
            .map_err(|_| "Response unavailable; inspect existing runs before retrying")?,
    )?
    .parse()?;
    if response.as_file().metadata()?.len() > limit {
        return Err("HTTP response exceeds size limit".into());
    }
    Ok(Response {
        status,
        data: fs::read(response.path())?,
    })
}

pub(super) fn receive(target: &Path, expected: &str, mut input: impl Read) -> Result<()> {
    if !exact_digest(expected) {
        return Err("Invalid source digest".into());
    }
    let parent = target
        .parent()
        .ok_or("Upload requires a target directory")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".upload-")
        .tempfile_in(parent)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        temporary.write_all(&buffer[..n])?;
    }
    temporary.as_file().sync_all()?;
    if format!("{:x}", hash.finalize()) != expected {
        return Err("Source upload checksum mismatch".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    match fs::hard_link(temporary.path(), target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(target)?.file_type().is_symlink() || digest(target)? != expected
            {
                return Err("Existing source object differs; refusing overwrite".into());
            }
        }
        Err(e) => return Err(e.into()),
    }
    File::open(parent)?.sync_all()?;
    Ok(())
}
pub(super) fn stage(
    config: &Config,
    archive: &Path,
    namespace: &str,
    expected: &str,
    extension: &str,
) -> Result<String> {
    if !["tar", "bundle", "json"].contains(&extension)
        || !name(namespace)
        || !exact_digest(expected)
    {
        return Err("Invalid source transport format or identity".into());
    }
    let suffix = format!("/{namespace}/{expected}.{extension}");
    let argv = [
        config.remote_binary.clone(),
        "crow-ci".into(),
        "receive".into(),
        "--target".into(),
        format!("{}{suffix}", config.host_sources),
        "--sha256".into(),
        expected.into(),
    ];
    let mut command = if let Some((program, args)) = config.ssh.split_first() {
        let mut ssh = Command::new(program);
        ssh.args(args)
            .arg(argv.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" "));
        ssh
    } else {
        // Pod-local mode: receive straight into the mounted source store.
        let mut local = Command::new(&argv[0]);
        local.args(&argv[1..]);
        local
    };
    let result = command
        .stdin(File::open(archive)?)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !result.success() {
        return Err("Source transfer failed; no pipeline submitted".into());
    }
    Ok(format!("{}{suffix}", config.worker_sources))
}
pub(super) fn binary_receipt(root: &Path, revision: &str, target: &str) -> Result<Value> {
    if !exact_sha(revision) || !name(target) {
        return Err("Invalid pinned binary identity".into());
    }
    let receipt: Value = serde_json::from_slice(&fs::read(root.join("receipt.json"))?)?;
    let hash = digest(&root.join("ccid"))?;
    if receipt["source_revision"] != revision
        || receipt["target"] != target
        || receipt["binary_sha256"] != hash
    {
        return Err("Shared executable differs from its source/platform build receipt".into());
    }
    Ok(json!({"source_revision":revision,"target":target,"binary_sha256":hash}))
}
