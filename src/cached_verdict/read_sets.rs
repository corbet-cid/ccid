//! The read-sets the verdict store keeps for narrowed checks, and the marker that makes a stale
//! one measure itself again (`docs/verdicts.md#re-measuring-a-stale-read-set`).
//!
//! A narrowed key is built from a measured read-set. The measurement normally lives on the check
//! in the manifest (`cache_inputs`, `cache_inputs_measured`), made by a person with
//! `ccid measure-inputs`, and a source change can make it stale. The run that finds it stale
//! cannot fix the manifest: CI never commits. It leaves its findings in the verdict store, in
//! three small files per check definition (the *slot*), below `verdicts/measured/`:
//!
//! | File | Meaning |
//! |---|---|
//! | `<slot>.stale.json` | *The read-set with these `inputs` is stale for this check definition since this commit.* `state` is `stale` (measure again) or `refused` (the automatic measurement did not prove either: measure by hand) |
//! | `<slot>.candidate.json` | The read-set one traced whole-tree run measured after the marker was set. Not trusted: it keys nothing until a run proves it |
//! | `<slot>.proven.json` | A read-set a proving run passed with: the check's current measured set. Lookups take it before the manifest's |
//!
//! The slot is the digest of the repository, the check's name, the digest of the check's
//! declaration and the digest of its *definition* (everything its verdict key says except the
//! tree: declaration and declared environment, the git dependency commits of its locks, the
//! toolchain, the build profile, the verdict epoch). A check whose definition changes starts a
//! slot of its own: a refusal, a candidate or a stored set made for the old definition never
//! applies to the new one, which starts over from the manifest's measurement and is proven or
//! found stale again on its own. That is how a check refused into whole-tree mode comes back
//! without anybody touching it; a person running `ccid measure-inputs` is the other way out.
//! A stored set descends from the manifest's measurement it replaced
//! (`base`, that measurement's `inputs` digest): when a person measures again and puts new lines
//! on the check, the manifest's digest changes, the stored lineage no longer applies, and the
//! manifest wins.
//!
//! Every file is written whole, atomically, under the slot's lock, and read back with the same
//! fail-closed discipline as a verdict record: a symlink, an oversized or malformed file, a file
//! written by a binary the release channel does not name, or one that does not describe this slot
//! is ignored, never trusted. Ignoring one costs a whole-tree key or a new measurement; nothing
//! here can make a verdict answer for a tree it did not prove.
use super::{
    channel_accepts, ensure_store, event, failure, is_hex, lock_key, now, own_tool, Producer, Tool,
    MAX_RECORD_BYTES,
};
use crate::narrowing::{self, Measured};
use crate::{Check, Environment, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Directory below the verdict store that holds the slots.
pub(crate) const DIR: &str = "measured";
/// Schema of the three file kinds. Unknown schemas are ignored.
const SCHEMA: u32 = 1;

/// A measured read-set: the `cache_inputs` globs and the provenance a key narrows on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetRecord {
    pub schema: u32,
    pub slot: String,
    pub check: String,
    pub repository: String,
    /// The `cache_inputs` list; empty when the check lists the repository root (nothing narrows).
    pub cache_inputs: Vec<String>,
    /// What `ccid measure-inputs` would print: the same provenance a manifest carries.
    pub measured: Measured,
    /// The `inputs` digest of the manifest's measurement this lineage descends from.
    pub base: String,
    /// The commit the tracing run judged.
    pub commit: String,
    /// How many paths the traced run touched.
    pub observations: u64,
    pub producer: Producer,
    /// The ccid binary that wrote the record (the release channel gates it like a verdict).
    pub tool: Tool,
    /// Unix seconds, UTC.
    pub at: u64,
    /// Present on a proven set: the run that proved it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<SetProof>,
}

/// The run that proved a read-set: the check passed in a copy holding `files` of the tree's `of`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetProof {
    pub method: String,
    pub files: u64,
    pub of: u64,
    pub commit: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MarkerState {
    /// The read-set is stale: the next executing whole-tree run measures it again.
    Stale,
    /// The measurement that replaced it did not prove either: nothing measures or proves again
    /// until the check is measured by hand (or edited).
    Refused,
}

/// *The read-set with these `inputs` is stale for this check definition since this commit.*
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Marker {
    pub schema: u32,
    pub slot: String,
    pub check: String,
    pub repository: String,
    /// The digest of the check definition the read-set was measured for.
    pub commands: String,
    /// The `inputs` digest of the read-set that failed its proof.
    pub stale_inputs: String,
    /// The `inputs` digest of the manifest's measurement the stale lineage descends from.
    pub base: String,
    pub state: MarkerState,
    /// The commit whose run found the read-set stale.
    pub since: String,
    pub at: u64,
    pub producer: Producer,
    pub tool: Tool,
}

/// Where a check's measured set stands for the key being looked up or stored.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// No marker applies to the key's read-set: a miss proves it.
    Open,
    /// The key's read-set is stale. `candidate` is the traced measurement waiting for its proof,
    /// `None` while no run has measured yet.
    Stale {
        marker: Box<Marker>,
        candidate: Option<Box<SetRecord>>,
    },
    /// The automatic measurement failed its proof: runs neither prove nor measure.
    Refused,
}

/// The files of one (repository, check, check definition).
#[derive(Clone, Debug)]
pub(crate) struct Slot {
    directory: PathBuf,
    pub id: String,
    check: String,
    repository: String,
    commands: String,
    definition: String,
}

impl Slot {
    /// The slot of `check` in `repository` for one definition of it: `commands` is the digest of
    /// its declaration (what a measurement names), `definition` the digest of everything its
    /// verdict key says except the tree (declaration and environment, locks, toolchain, profile).
    pub(crate) fn new(
        store: &Path,
        repository: &str,
        check: &str,
        commands: &str,
        definition: &str,
    ) -> Slot {
        let id = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&json!([repository, check, commands, definition]))
                    .unwrap_or_default()
            )
        );
        Slot {
            directory: store.join(DIR),
            id,
            check: check.to_owned(),
            repository: repository.to_owned(),
            commands: commands.to_owned(),
            definition: definition.to_owned(),
        }
    }

    /// The check this slot belongs to.
    pub(crate) fn check(&self) -> &str {
        &self.check
    }

    fn path(&self, kind: &str) -> PathBuf {
        self.directory.join(format!("{}.{kind}.json", self.id))
    }

    /// Run `action` with the slot's lock held and its directory created.
    fn locked<T>(&self, action: impl FnOnce() -> Result<T>) -> Result<T> {
        if let Some(parent) = self.directory.parent() {
            ensure_store(parent)?;
        }
        ensure_store(&self.directory)?;
        let _lock = lock_key(&self.directory, &self.id)?;
        action()
    }

    fn publish<T: Serialize>(&self, kind: &str, value: &T) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if fs::metadata(&self.directory).is_ok_and(|meta| meta.mode() & 0o020 != 0) {
                file.as_file()
                    .set_permissions(fs::Permissions::from_mode(0o660))?;
            }
        }
        serde_json::to_writer(&mut file, value)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(self.path(kind)).map_err(|error| {
            failure(format!("Cannot publish a measured read-set file: {error}"))
        })?;
        Ok(())
    }

    fn remove(&self, kind: &str) -> Result<()> {
        match fs::remove_file(self.path(kind)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn read<T: DeserializeOwned>(&self, kind: &str) -> std::result::Result<Option<T>, String> {
        let path = self.path(kind);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Err("it is a symlink".into()),
            Ok(meta) if !meta.file_type().is_file() => return Err("it is not a file".into()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        }
        let mut bytes = Vec::new();
        File::open(&path)
            .and_then(|file| file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes))
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err("it exceeds its size bound".into());
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "it is not valid".into())
    }

    fn ignored(&self, kind: &str, reason: &str) {
        event(json!({
            "event": "read-set-ignored",
            "check": self.check,
            "file": kind,
            "reason": reason,
        }));
    }

    /// A stored set that describes this slot and that this binary may trust.
    fn load_set(&self, kind: &str, environment: &Environment) -> Option<SetRecord> {
        let record = match self.read::<SetRecord>(kind) {
            Ok(Some(record)) => record,
            Ok(None) => return None,
            Err(reason) => {
                self.ignored(kind, &reason);
                return None;
            }
        };
        match self.accept_set(&record, kind, environment) {
            Ok(()) => Some(record),
            Err(reason) => {
                self.ignored(kind, &reason);
                None
            }
        }
    }

    fn accept_set(
        &self,
        record: &SetRecord,
        kind: &str,
        environment: &Environment,
    ) -> std::result::Result<(), String> {
        if record.schema != SCHEMA {
            return Err("unknown schema".into());
        }
        if record.slot != self.id
            || record.check != self.check
            || record.repository != self.repository
            || record.measured.commands != self.commands
        {
            return Err("it describes another check definition".into());
        }
        record
            .measured
            .validate()
            .map_err(|error| error.to_string())?;
        let inputs =
            narrowing::inputs_digest(&record.cache_inputs).map_err(|error| error.to_string())?;
        if inputs != record.measured.inputs {
            return Err("its inputs differ from the list that was measured".into());
        }
        if !is_hex(&record.commit, 40) || !is_hex(&record.base, 64) {
            return Err("malformed provenance".into());
        }
        match (kind, &record.proof, record.cache_inputs.is_empty()) {
            // A candidate has proved nothing.
            ("candidate", None, _) => {}
            // A proven set carries its proof, except the whole tree (nothing to prove).
            ("proven", Some(proof), false)
                if proof.method == narrowing::PROOF
                    && proof.files > 0
                    && proof.files <= proof.of
                    && is_hex(&proof.commit, 40) => {}
            ("proven", None, true) => {}
            _ => return Err("its proof does not match its kind".into()),
        }
        channel_accepts(&record.tool, environment)
    }

    /// The check's proven measured set, if the store has a trustworthy one.
    pub(crate) fn proven(&self, environment: &Environment) -> Option<SetRecord> {
        self.load_set("proven", environment)
    }

    /// The traced measurement waiting for a proof.
    pub(crate) fn candidate(&self, environment: &Environment) -> Option<SetRecord> {
        self.load_set("candidate", environment)
    }

    /// The staleness marker, if any.
    pub(crate) fn marker(&self) -> Option<Marker> {
        let marker = match self.read::<Marker>("stale") {
            Ok(Some(marker)) => marker,
            Ok(None) => return None,
            Err(reason) => {
                self.ignored("stale", &reason);
                return None;
            }
        };
        if marker.schema != SCHEMA
            || marker.slot != self.id
            || marker.commands != self.commands
            || !is_hex(&marker.stale_inputs, 64)
            || !is_hex(&marker.since, 40)
        {
            self.ignored("stale", "it does not describe this check definition");
            return None;
        }
        Some(marker)
    }

    /// Where the read-set with `inputs` stands: the marker applies to exactly the read-set it
    /// declared stale, so a person's new measurement (other digest) is never held back by it.
    pub(crate) fn state(&self, inputs: &str, environment: &Environment) -> State {
        let Some(marker) = self.marker().filter(|marker| marker.stale_inputs == inputs) else {
            return State::Open;
        };
        match marker.state {
            MarkerState::Refused => State::Refused,
            MarkerState::Stale => State::Stale {
                candidate: self.candidate(environment).map(Box::new),
                marker: Box::new(marker),
            },
        }
    }

    fn marker_record(
        &self,
        stale_inputs: &str,
        base: &str,
        since: &str,
        state: MarkerState,
        producer: &Producer,
    ) -> Result<Marker> {
        Ok(Marker {
            schema: SCHEMA,
            slot: self.id.clone(),
            check: self.check.clone(),
            repository: self.repository.clone(),
            commands: self.commands.clone(),
            stale_inputs: stale_inputs.to_owned(),
            base: base.to_owned(),
            state,
            since: since.to_owned(),
            at: now(),
            producer: producer.clone(),
            tool: own_tool()?,
        })
    }

    /// A proof failed and the whole-tree run passed: the read-set with `stale_inputs` is stale
    /// since `since`. Any earlier candidate was measured for another staleness and goes.
    pub(crate) fn mark_stale(
        &self,
        stale_inputs: &str,
        base: &str,
        since: &str,
        producer: &Producer,
    ) -> Result<()> {
        let marker = self.marker_record(stale_inputs, base, since, MarkerState::Stale, producer)?;
        self.locked(|| {
            self.remove("candidate")?;
            self.publish("stale", &marker)
        })?;
        event(json!({
            "event": "read-set-stale",
            "check": self.check,
            "commands": self.commands,
            "definition": self.definition,
            "inputs": stale_inputs,
            "since": since,
        }));
        Ok(())
    }

    /// The measurement did not prove either: stop measuring and proving for this read-set.
    pub(crate) fn refuse(&self, marker: &Marker, producer: &Producer) -> Result<()> {
        let refused = self.marker_record(
            &marker.stale_inputs,
            &marker.base,
            &marker.since,
            MarkerState::Refused,
            producer,
        )?;
        self.locked(|| {
            self.remove("candidate")?;
            self.publish("stale", &refused)
        })?;
        event(json!({
            "event": "read-set-refused",
            "check": self.check,
            "definition": self.definition,
            "inputs": marker.stale_inputs,
            "reason": "the traced read-set failed its proof as well; measure the check by hand with `ccid measure-inputs`",
        }));
        Ok(())
    }

    /// A traced whole-tree run measured the check: keep the set for the next miss to prove. The
    /// marker stays; only a proof clears it.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn store_candidate(&self, record: &SetRecord) -> Result<()> {
        self.locked(|| self.publish("candidate", record))?;
        event(json!({
            "event": "read-set-measure",
            "check": self.check,
            "status": "stored",
            "inputs": record.measured.inputs,
            "globs": record.cache_inputs.len(),
            "observations": record.observations,
            "commit": record.commit,
        }));
        Ok(())
    }

    /// A proving run passed with `candidate`: it becomes the check's measured set and the marker
    /// is cleared. The proof event is the clearing rule.
    pub(crate) fn promote(&self, candidate: &SetRecord, proof: SetProof) -> Result<()> {
        let mut proven = candidate.clone();
        proven.proof = Some(proof.clone());
        self.locked(|| {
            self.publish("proven", &proven)?;
            self.remove("candidate")?;
            self.remove("stale")
        })?;
        event(json!({
            "event": "read-set-proven",
            "check": self.check,
            "inputs": proven.measured.inputs,
            "files": proof.files,
            "of": proof.of,
            "cleared": true,
        }));
        Ok(())
    }

    /// The traced check lists the repository root: no read-set narrows it, and nothing is left to
    /// prove. The whole tree is the measured set, which ends the staleness.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn store_whole_tree(&self, record: &SetRecord) -> Result<()> {
        self.locked(|| {
            self.publish("proven", record)?;
            self.remove("candidate")?;
            self.remove("stale")
        })?;
        event(json!({
            "event": "read-set-measure",
            "check": self.check,
            "status": "whole-tree",
            "reason": "the check lists the repository root: nothing narrows",
            "observations": record.observations,
            "commit": record.commit,
            "cleared": true,
        }));
        Ok(())
    }

    /// Drop a candidate that can no longer be proven (the tree's layout moved on).
    pub(crate) fn drop_candidate(&self) -> Result<()> {
        self.locked(|| self.remove("candidate"))
    }

    /// A measured set for this slot, as the traced run found it.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn record(
        &self,
        globs: Vec<String>,
        layout: String,
        base: &str,
        commit: &str,
        observations: usize,
        producer: &Producer,
    ) -> Result<SetRecord> {
        let inputs = narrowing::inputs_digest(&globs)?;
        Ok(SetRecord {
            schema: SCHEMA,
            slot: self.id.clone(),
            check: self.check.clone(),
            repository: self.repository.clone(),
            cache_inputs: globs,
            measured: Measured {
                tool: narrowing::measuring_revision(),
                commands: self.commands.clone(),
                inputs,
                layout,
                date: narrowing::today(),
            },
            base: base.to_owned(),
            commit: commit.to_owned(),
            observations: observations as u64,
            producer: producer.clone(),
            tool: own_tool()?,
            at: now(),
            proof: None,
        })
    }
}

/// The check as the verdict key reads its measured set, and where the set came from.
pub(crate) struct Overlay {
    pub check: Check,
    /// The `inputs` digest of the manifest measurement this set descends from.
    pub base: String,
    pub empty: bool,
}

/// The newest proven set the store holds for `check`, laid over the manifest's declaration.
///
/// Only a check that declares both `cache_inputs` and the provenance of a measurement opted into
/// narrowing, and only a set that descends from the measurement the manifest carries now applies:
/// a person's new measurement, a hand edit of the list or an edited check each start over.
pub(crate) fn overlay(
    store: Option<&Path>,
    repository: &str,
    name: &str,
    check: &Check,
    definition: &str,
    environment: &Environment,
) -> Option<Overlay> {
    let manifest = check.cache_inputs_measured.as_ref()?;
    check
        .cache_inputs
        .as_ref()
        .filter(|globs| !globs.is_empty())?;
    if check.kind == "nix" {
        return None;
    }
    let commands = narrowing::commands_digest(check).ok()?;
    let slot = Slot::new(store?, repository, name, &commands, definition);
    let proven = slot.proven(environment)?;
    if proven.base != manifest.inputs {
        return None;
    }
    let mut overlaid = check.clone();
    overlaid.cache_inputs = Some(proven.cache_inputs.clone());
    overlaid.cache_inputs_measured = Some(proven.measured.clone());
    Some(Overlay {
        check: overlaid,
        base: proven.base.clone(),
        empty: proven.cache_inputs.is_empty(),
    })
}
