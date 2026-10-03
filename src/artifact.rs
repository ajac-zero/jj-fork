//! Versioned, typed artifacts. Only engine-created plans can be signed. The authority is
//! repository-local, never a tracked file; it is not a sandbox against processes that can read
//! it. Truly untrusted repair workers must run isolated from the source repository.
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::workflow::{Plan, Report};

pub const SCHEMA_VERSION: u32 = 1;
// Must match the exact jj-lib dependency: executable graph semantics are version-bound.
const JJ_VERSION: &str = "0.43.0";
type Authentication = Hmac<Sha256>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    #[serde(rename = "jj-fork-plan")]
    Plan,
    #[serde(rename = "jj-fork-report")]
    Report,
    #[serde(rename = "jj-fork-task")]
    Task,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    kind: Kind,
    schema_version: u32,
    engine_version: String,
    jj_version: String,
    payload: T,
    authentication: Option<String>,
}

impl<T: Serialize> Envelope<T> {
    fn new(kind: Kind, payload: T) -> Self {
        Self {
            kind,
            schema_version: SCHEMA_VERSION,
            engine_version: env!("CARGO_PKG_VERSION").into(),
            jj_version: JJ_VERSION.into(),
            payload,
            authentication: None,
        }
    }

    // Struct field order and BTreeMap/ordered vectors make typed serialization deterministic.
    // Include the version and kind in the authenticated domain, not just the payload.
    fn message(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            "jj-fork artifact authority v1",
            &self.kind,
            self.schema_version,
            &self.engine_version,
            &self.jj_version,
            &self.payload,
        ))?)
    }

    fn validate_version(&self, kind: Kind) -> Result<()> {
        ensure!(
            self.kind == kind,
            "artifact is not an executable jj-fork plan"
        );
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported artifact schema version"
        );
        ensure!(
            self.engine_version == env!("CARGO_PKG_VERSION") && self.jj_version == JJ_VERSION,
            "artifact engine/jj version mismatch"
        );
        Ok(())
    }
}

pub fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(hex(&Sha256::digest(serde_json::to_vec(value)?)))
}

pub fn save_plan(path: &Path, repository: &Path, plan: &Plan) -> Result<()> {
    save_authenticated(path, repository, Kind::Plan, plan)
}

pub fn save_task(path: &Path, repository: &Path, task: &crate::repair::Task) -> Result<()> {
    save_authenticated(path, repository, Kind::Task, task)
}

pub fn load_task(path: &Path, repository: &Path) -> Result<crate::repair::Task> {
    let task: crate::repair::Task = load_authenticated(path, repository, Kind::Task)?;
    task.validate_ids()?;
    Ok(task)
}

fn save_authenticated(
    path: &Path,
    repository: &Path,
    kind: Kind,
    payload: &impl Serialize,
) -> Result<()> {
    let mut envelope = Envelope::new(kind, payload);
    let key = authority(repository, true)?;
    let mut mac = Authentication::new_from_slice(&key)?;
    mac.update(&envelope.message()?);
    envelope.authentication = Some(hex(&mac.finalize().into_bytes()));
    atomic_write(path, &serde_json::to_vec_pretty(&envelope)?)
}

/// Deserialize strict typed data first, but do not use any artifact IDs or paths until verified.
pub fn load_plan(path: &Path, repository: &Path) -> Result<Plan> {
    let plan: Plan = load_authenticated(path, repository, Kind::Plan)?;
    plan.validate_ids()?;
    Ok(plan)
}

fn load_authenticated<T: serde::de::DeserializeOwned + Serialize>(
    path: &Path,
    repository: &Path,
    kind: Kind,
) -> Result<T> {
    let mut data = Vec::new();
    File::open(path)?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= 64 * 1024 * 1024, "artifact is too large");
    reject_duplicate_keys(&data)?;
    let envelope: Envelope<T> = serde_json::from_slice(&data).context("invalid plan artifact")?;
    envelope.validate_version(kind)?;
    let signature = unhex(
        envelope
            .authentication
            .as_deref()
            .context("unsigned plan")?,
    )?;
    let key = authority(repository, false)?;
    let mut mac = Authentication::new_from_slice(&key)?;
    mac.update(&envelope.message()?);
    mac.verify_slice(&signature)
        .context("plan authentication failed (edited artifact or wrong repository)")?;
    Ok(envelope.payload)
}

pub fn save_report(path: &Path, report: &Report) -> Result<()> {
    atomic_write(
        path,
        &serde_json::to_vec_pretty(&Envelope::new(Kind::Report, report))?,
    )
}

fn authority(repository: &Path, create: bool) -> Result<Vec<u8>> {
    let dir = repository.join("jj-fork");
    let path = dir.join("authority");
    if create {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        ensure!(
            fs::symlink_metadata(&dir)?.is_dir(),
            "invalid artifact authority directory"
        );
        // Publish a complete key with an exclusive hard link: readers never see a partially
        // written key, and concurrent creators all use the winner's authority.
        if !path.exists() {
            let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
            temp.write_all(&rand::random::<[u8; 32]>())?;
            temp.as_file().sync_all()?;
            match fs::hard_link(temp.path(), &path) {
                Ok(()) => sync_directory(&dir)?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    let metadata = fs::symlink_metadata(&path).context("missing repository artifact authority")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "invalid artifact authority"
    );
    let key = fs::read(&path)?;
    ensure!(key.len() == 32, "invalid artifact authority length");
    Ok(key)
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "refusing to overwrite a symlink or non-file artifact"
        );
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(contents)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    // rename replaces the directory entry, never follows a raced-in symlink.
    temp.persist(path).map_err(|e| e.error)?;
    sync_directory(parent)
}

// Directory fsync is how Unix makes a rename or link durable; Windows cannot open directories.
fn sync_directory(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    ensure!(
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "malformed authentication"
    );
    (0..s.len())
        .step_by(2)
        .map(|i| Ok(u8::from_str_radix(&s[i..i + 2], 16)?))
        .collect()
}

// serde's derived structs reject duplicate fields, but ordinary map deserialization would
// silently keep the last duplicate key. Check every nested object before typed deserialization.
fn reject_duplicate_keys(data: &[u8]) -> Result<()> {
    use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
    struct Strict;
    impl<'de> DeserializeSeed<'de> for Strict {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> std::result::Result<(), D::Error> {
            deserializer.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for Strict {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("JSON without duplicate object keys")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> std::result::Result<(), M::Error> {
            let mut keys = std::collections::BTreeSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key) {
                    return Err(de::Error::custom("duplicate artifact field"));
                }
                map.next_value_seed(Strict)?;
            }
            Ok(())
        }
        fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> std::result::Result<(), S::Error> {
            while seq.next_element_seed(Strict)?.is_some() {}
            Ok(())
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<(), E> {
            Ok(())
        }
        fn visit_unit<E: de::Error>(self) -> std::result::Result<(), E> {
            Ok(())
        }
    }
    let mut deserializer = serde_json::Deserializer::from_slice(data);
    Strict.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_is_stable_and_domain_separated() {
        let dir = tempfile::tempdir().unwrap();
        let first = authority(dir.path(), true).unwrap();
        assert_eq!(authority(dir.path(), true).unwrap(), first);
        let plan = Envelope::new(Kind::Plan, vec![1, 2]);
        let report = Envelope::new(Kind::Report, vec![1, 2]);
        assert_ne!(plan.message().unwrap(), report.message().unwrap());
    }

    #[test]
    fn duplicate_map_keys_are_not_canonicalized_away() {
        assert!(
            reject_duplicate_keys(br#"{"view":{"heads":[],"bookmarks":{"x":1,"x":1}}}"#).is_err()
        );
        reject_duplicate_keys(br#"{"view":{"heads":[null,true,3,"x"],"bookmarks":{"x":1}}}"#)
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn artifact_write_is_private_and_does_not_follow_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact");
        atomic_write(&path, b"first").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(atomic_write(&link, b"bad").is_err());
        assert_eq!(fs::read(path).unwrap(), b"first\n");
    }
}
