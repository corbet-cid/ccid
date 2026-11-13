//! The draft of one commit (`build-<commit>`): the one place every file of a release is collected.
//!
//! Rules carried over from the first release flow (ccvl), now in one place:
//!
//! * one draft per commit, named after the commit; drafts of different commits never meet;
//! * every upload is checked by size on the release itself: the forge keeps the part of an interrupted
//!   upload, so a wrong-size asset of a DRAFT is removed and uploaded again, and an asset of the right
//!   size is never touched;
//! * a file whose bytes differ from the one the draft holds is replaced only when the caller says so
//!   (`put`) and only while the release is a draft; a public release is final.

use super::forge::{Asset, Forge, Release};
use super::{plain_asset_name, DRAFT_PREFIX, SUMS};
use crate::{failure, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

/// The result of putting one file into the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    /// The draft held exactly these bytes already.
    Unchanged,
    /// The draft did not hold the file.
    Added,
    /// The draft held other bytes under that name and now holds these.
    Replaced,
}

pub struct Draft<'a> {
    forge: &'a Forge,
    sha: String,
    release: Release,
}

pub fn tag_of(sha: &str) -> String {
    format!("{DRAFT_PREFIX}{sha}")
}

pub fn valid_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// SHA-256 of a file, streamed.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .map_err(|error| failure(format!("cannot read {}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The files of a directory (flat, regular) by name with their SHA-256; anything else is refused.
pub fn snapshot(dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(dir)
        .map_err(|error| failure(format!("cannot read {}: {error}", dir.display())))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry.file_type()?;
        if !kind.is_file() {
            return Err(failure(format!(
                "{name} in {} is not a regular file: a release is a flat set of files",
                dir.display()
            )));
        }
        if !plain_asset_name(&name) {
            return Err(failure(format!("{name:?} is not a name an asset can have")));
        }
        files.insert(name.clone(), sha256_file(&entry.path())?);
    }
    Ok(files)
}

/// `SHA256SUMS` text for `files` (name -> sha256): coreutils format, byte order, the sums file itself left out.
pub fn sums_text(files: &BTreeMap<String, String>) -> String {
    files
        .iter()
        .filter(|(name, _)| name.as_str() != SUMS)
        .map(|(name, sha)| format!("{sha}  {name}\n"))
        .collect()
}

impl<'a> Draft<'a> {
    /// The draft of `sha`, if there is one. A PUBLIC release under that tag is an error: the commit's
    /// draft was promoted or never existed.
    pub fn find(forge: &'a Forge, sha: &str) -> Result<Option<Draft<'a>>> {
        let tag = tag_of(sha);
        match forge.release(&tag)? {
            None => Ok(None),
            Some(release) => {
                if release.target_commitish != sha && !release.target_commitish.is_empty() {
                    return Err(failure(format!(
                        "release {tag} targets {} and not the commit it is named after",
                        release.target_commitish
                    )));
                }
                Ok(Some(Draft {
                    forge,
                    sha: sha.to_owned(),
                    release,
                }))
            }
        }
    }

    /// The draft of `sha`, created when missing.
    pub fn ensure(forge: &'a Forge, sha: &str, title: &str, notes: &str) -> Result<Draft<'a>> {
        if let Some(draft) = Draft::find(forge, sha)? {
            draft.require_draft()?;
            return Ok(draft);
        }
        let tag = tag_of(sha);
        // The tag of a draft does not exist yet; one that does must be this commit's.
        match forge.tag_commit(&tag)? {
            None => {}
            Some(commit) if commit == sha => {}
            Some(_) => return Err(failure(format!("existing tag {tag} targets another commit"))),
        }
        forge.create_draft(&tag, sha, title, notes)?;
        Draft::find(forge, sha)?
            .ok_or_else(|| failure(format!("the forge did not keep the new draft {tag}")))
    }

    pub fn release(&self) -> &Release {
        &self.release
    }

    pub fn is_draft(&self) -> bool {
        self.release.draft
    }

    pub fn require_draft(&self) -> Result<()> {
        if self.release.draft {
            Ok(())
        } else {
            Err(failure(format!(
                "release {} is public: its assets are final",
                self.release.tag_name
            )))
        }
    }

    pub fn reload(&mut self) -> Result<()> {
        let tag = tag_of(&self.sha);
        self.release = self
            .forge
            .release(&tag)?
            .ok_or_else(|| failure(format!("the draft {tag} is gone")))?;
        Ok(())
    }

    /// Remove a wrong-size asset of `name` (a part of an interrupted upload). Returns whether one went.
    fn drop_wrong_size(&mut self, name: &str, size: u64) -> Result<bool> {
        let Some(held) = self.release.asset(name).cloned() else {
            return Ok(false);
        };
        if held.size == size {
            return Ok(false);
        }
        self.require_draft()?;
        eprintln!(
            "ccid release: removing the truncated draft asset {name} ({} of {size} bytes)",
            held.size
        );
        self.forge.delete_asset(self.release.id, held.id)?;
        self.reload()?;
        Ok(true)
    }

    /// Upload one file that the draft does not hold yet: up to four attempts, each followed by a look
    /// at the draft (the forge may finish storing what the client gave up on).
    fn upload(&mut self, name: &str, path: &Path) -> Result<()> {
        let size = fs::metadata(path)?.len();
        for attempt in 1..=4u32 {
            self.require_draft()?;
            match self.forge.upload(self.release.id, name, path, size) {
                Ok(()) => {}
                Err(error) => eprintln!("ccid release: upload of {name} failed (attempt {attempt} of 4): {error}"),
            }
            if attempt < 4 {
                std::thread::sleep(self.forge.retry_delay * attempt);
            }
            self.reload()?;
            self.drop_wrong_size(name, size)?;
            if self.release.asset(name).is_some_and(|held| held.size == size) {
                return Ok(());
            }
        }
        Err(failure(format!("could not upload {name} to {}", self.release.tag_name)))
    }

    /// Download one asset of the draft.
    pub fn fetch(&self, name: &str, destination: &Path) -> Result<()> {
        let asset: &Asset = self
            .release
            .asset(name)
            .ok_or_else(|| failure(format!("the release {} has no asset {name}", self.release.tag_name)))?;
        self.forge.download(asset, destination)
    }

    /// Every asset of the draft into `dir` (which must exist).
    pub fn fetch_all(&self, dir: &Path) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for asset in &self.release.assets {
            self.forge.download(asset, &dir.join(&asset.name))?;
            names.push(asset.name.clone());
        }
        Ok(names)
    }

    /// Every asset already held must be one of `files` with exactly its bytes; a wrong-size one (an interrupted
    /// upload) is removed first. Anything else is refused: a draft never mixes two runs' bytes.
    pub fn settle_existing(&mut self, files: &BTreeMap<String, PathBuf>) -> Result<()> {
        for (name, path) in files {
            self.drop_wrong_size(name, fs::metadata(path)?.len())?;
        }
        let held: Vec<Asset> = self.release.assets.clone();
        for asset in held {
            let Some(path) = files.get(&asset.name) else {
                return Err(failure(format!(
                    "the draft {} holds {}, which this run does not produce: delete the draft (ccid release discard) and release again",
                    self.release.tag_name, asset.name
                )));
            };
            let scratch = tempfile::tempdir()?;
            let copy = scratch.path().join("held");
            self.forge.download(&asset, &copy).map_err(|error| {
                failure(format!("cannot compare the held asset {}: {error}", asset.name))
            })?;
            if sha256_file(&copy)? != sha256_file(path)? {
                return Err(failure(format!(
                    "the draft's {} differs from this run's: delete the draft (ccid release discard) and release again",
                    asset.name
                )));
            }
        }
        Ok(())
    }

    /// Put every file of `files` into the draft: add what is missing (the held ones were settled before).
    pub fn upload_missing(&mut self, files: &BTreeMap<String, PathBuf>) -> Result<()> {
        for (name, path) in files {
            if self.release.asset(name).is_none() {
                self.upload(name, path)?;
                eprintln!("ccid release: {name} uploaded");
            }
        }
        self.reload()
    }

    /// The draft ends up holding `path` under its own name. Only a draft changes.
    pub fn put(&mut self, path: &Path) -> Result<Put> {
        self.require_draft()?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| plain_asset_name(n))
            .ok_or_else(|| failure(format!("{} is not a file an asset can come from", path.display())))?
            .to_owned();
        let size = fs::metadata(path)
            .map_err(|error| failure(format!("cannot read {}: {error}", path.display())))?
            .len();
        let mut outcome = Put::Added;
        if let Some(held) = self.release.asset(&name).cloned() {
            outcome = Put::Replaced;
            if held.size == size {
                let scratch = tempfile::tempdir()?;
                let copy = scratch.path().join("held");
                if self.forge.download(&held, &copy).is_ok() && sha256_file(&copy)? == sha256_file(path)? {
                    return Ok(Put::Unchanged);
                }
            }
            self.forge.delete_asset(self.release.id, held.id)?;
            self.reload()?;
        }
        self.upload(&name, path)?;
        Ok(outcome)
    }

    /// Delete the draft (never a public release).
    pub fn discard(self) -> Result<()> {
        self.require_draft()?;
        self.forge.delete_release(self.release.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_are_coreutils_lines_in_byte_order_without_the_sums_file() {
        let mut files = BTreeMap::new();
        files.insert("b.txt".to_owned(), "22".repeat(32));
        files.insert("A.txt".to_owned(), "11".repeat(32));
        files.insert(SUMS.to_owned(), "00".repeat(32));
        let text = sums_text(&files);
        assert_eq!(
            text,
            format!("{}  A.txt\n{}  b.txt\n", "11".repeat(32), "22".repeat(32))
        );
    }

    #[test]
    fn a_directory_snapshot_refuses_anything_but_flat_plain_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.bin"), b"x").unwrap();
        assert_eq!(snapshot(dir.path()).unwrap().len(), 1);
        fs::create_dir(dir.path().join("sub")).unwrap();
        assert!(snapshot(dir.path()).unwrap_err().to_string().contains("flat set"));
        fs::remove_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join(".hidden"), b"x").unwrap();
        assert!(snapshot(dir.path()).is_err());
    }

    #[test]
    fn commit_ids_are_forty_lowercase_hex_digits() {
        assert!(valid_sha(&"a".repeat(40)));
        assert!(!valid_sha(&"A".repeat(40)));
        assert!(!valid_sha(&"a".repeat(39)));
        assert_eq!(tag_of(&"b".repeat(40)), format!("build-{}", "b".repeat(40)));
    }
}
