//! Components needed to verify TUF metadata and targets.

use chrono::offset::Utc;
use log::info;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;

use crate::crypto::PublicKey;
use crate::error::Error;
use crate::interchange::DataInterchange;
use crate::metadata::{
    Delegation, Delegations, Metadata, MetadataPath, Role, RootMetadata, SignedMetadata,
    SnapshotMetadata, TargetDescription, TargetsMetadata, TimestampMetadata, VirtualTargetPath,
};
use crate::Result;

/// Contains trusted TUF metadata and can be used to verify other metadata and targets.
#[derive(Debug)]
pub struct Tuf<D: DataInterchange> {
    root: RootMetadata,
    snapshot: Option<SnapshotMetadata>,
    targets: Option<TargetsMetadata>,
    timestamp: Option<TimestampMetadata>,
    delegations: HashMap<MetadataPath, TargetsMetadata>,
    interchange: PhantomData<D>,
}

impl<D: DataInterchange> Tuf<D> {
    /// Create a new [`Tuf`] struct from a set of trusted root keys that are used to verify the
    /// signed metadata. The signed root metadata must be signed with at least a `root_threshold`
    /// of the provided root_keys. It is not necessary for the root metadata to contain these keys.
    pub fn from_root_with_trusted_keys<'a, I>(
        signed_root: SignedMetadata<D, RootMetadata>,
        root_threshold: u32,
        root_keys: I,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = &'a PublicKey>,
    {
        signed_root.verify(root_threshold, root_keys)?;
        Self::from_trusted_root(signed_root)
    }

    /// Create a new [`Tuf`] struct from a piece of metadata that is assumed to be trusted.
    ///
    /// **WARNING**: This is trust-on-first-use (TOFU) and offers weaker security guarantees than
    /// the related method [`Tuf::from_root_with_trusted_keys`].
    pub fn from_trusted_root(signed_root: SignedMetadata<D, RootMetadata>) -> Result<Self> {
        let verified = {
            let root = signed_root.assume_valid()?;

            signed_root.verify(
                root.root().threshold(),
                root.keys().iter().filter_map(|(k, v)| {
                    if root.root().key_ids().contains(k) {
                        Some(v)
                    } else {
                        None
                    }
                }),
            )?
        };

        Ok(Tuf {
            root: verified,
            snapshot: None,
            targets: None,
            timestamp: None,
            delegations: HashMap::new(),
            interchange: PhantomData,
        })
    }

    /// An immutable reference to the root metadata.
    pub fn root(&self) -> &RootMetadata {
        &self.root
    }

    /// An immutable reference to the optional snapshot metadata.
    pub fn snapshot(&self) -> Option<&SnapshotMetadata> {
        self.snapshot.as_ref()
    }

    /// An immutable reference to the optional targets metadata.
    pub fn targets(&self) -> Option<&TargetsMetadata> {
        self.targets.as_ref()
    }

    /// An immutable reference to the optional timestamp metadata.
    pub fn timestamp(&self) -> Option<&TimestampMetadata> {
        self.timestamp.as_ref()
    }

    /// An immutable reference to the delegated metadata.
    pub fn delegations(&self) -> &HashMap<MetadataPath, TargetsMetadata> {
        &self.delegations
    }

    fn current_timestamp_version(&self) -> u32 {
        self.timestamp.as_ref().map(|t| t.version()).unwrap_or(0)
    }

    fn current_snapshot_version(&self) -> u32 {
        self.snapshot.as_ref().map(|t| t.version()).unwrap_or(0)
    }

    fn current_targets_version(&self) -> u32 {
        self.targets.as_ref().map(|t| t.version()).unwrap_or(0)
    }

    fn current_delegation_version(&self, role: &MetadataPath) -> u32 {
        self.delegations.get(role).map(|t| t.version()).unwrap_or(0)
    }

    /// Verify and update the root metadata.
    pub fn update_root(&mut self, signed_root: SignedMetadata<D, RootMetadata>) -> Result<bool> {
        let verified = {
            let old_root = &self.root;

            // First, check that the new root was signed by the old root.
            let new_root = signed_root.verify(
                old_root.root().threshold(),
                old_root.keys().iter().filter_map(|(k, v)| {
                    if old_root.root().key_ids().contains(k) {
                        Some(v)
                    } else {
                        None
                    }
                }),
            )?;

            // Next, make sure the new root has a higher version than the old root.
            if new_root.version() == old_root.version() {
                info!(
                    "Attempted to update root to new metadata with the same version. \
                     Refusing to update."
                );
                return Ok(false);
            } else if new_root.version() < old_root.version() {
                return Err(Error::VerificationFailure(format!(
                    "Attempted to roll back root metadata at version {} to {}.",
                    old_root.version(),
                    new_root.version()
                )));
            }

            // Finally, make sure the new root was signed by the keys in the new root.
            signed_root.verify(
                new_root.root().threshold(),
                new_root.keys().iter().filter_map(|(k, v)| {
                    if new_root.root().key_ids().contains(k) {
                        Some(v)
                    } else {
                        None
                    }
                }),
            )?
        };

        self.purge_metadata();

        self.root = verified;
        Ok(true)
    }

    /// Verify and update the timestamp metadata.
    ///
    /// Returns a reference to the parsed metadata if the metadata was newer.
    pub fn update_timestamp(
        &mut self,
        signed_timestamp: SignedMetadata<D, TimestampMetadata>,
    ) -> Result<Option<&TimestampMetadata>> {
        let verified = {
            let root = &self.root;

            // First, make sure the root signed the metadata.
            let timestamp = signed_timestamp.verify(
                root.timestamp().threshold(),
                root.keys().iter().filter_map(|(k, v)| {
                    if root.timestamp().key_ids().contains(k) {
                        Some(v)
                    } else {
                        None
                    }
                }),
            )?;

            // Next, make sure the timestamp hasn't expired.
            if timestamp.expires() <= &Utc::now() {
                return Err(Error::ExpiredMetadata(Role::Timestamp));
            }

            // Next, make sure the new metadata has a higher version than the old metadata.
            let current_version = self.current_timestamp_version();

            if timestamp.version() < current_version {
                return Err(Error::VerificationFailure(format!(
                    "Attempted to roll back timestamp metadata at version {} to {}.",
                    current_version,
                    timestamp.version()
                )));
            } else if timestamp.version() == current_version {
                return Ok(None);
            }

            if self.current_snapshot_version() != timestamp.snapshot().version() {
                self.snapshot = None;
            }

            timestamp
        };

        self.timestamp = Some(verified);
        Ok(self.timestamp.as_ref())
    }

    /// Verify and update the snapshot metadata.
    pub fn update_snapshot(
        &mut self,
        signed_snapshot: SignedMetadata<D, SnapshotMetadata>,
    ) -> Result<bool> {
        let verified = {
            let root = self.safe_root_ref()?;
            let timestamp = self.safe_timestamp_ref()?;
            let current_version = self.current_snapshot_version();

            if timestamp.snapshot().version() < current_version {
                return Err(Error::VerificationFailure(format!(
                    "Attempted to roll back snapshot metadata at version {} to {}.",
                    current_version,
                    timestamp.snapshot().version()
                )));
            } else if timestamp.snapshot().version() == current_version {
                return Ok(false);
            }

            let snapshot = signed_snapshot.verify(
                root.snapshot().threshold(),
                self.root.keys().iter().filter_map(|(k, v)| {
                    if root.snapshot().key_ids().contains(k) {
                        Some(v)
                    } else {
                        None
                    }
                }),
            )?;

            if snapshot.version() != timestamp.snapshot().version() {
                return Err(Error::VerificationFailure(format!(
                    "The timestamp metadata reported that the snapshot metadata should be at \
                     version {} but version {} was found instead.",
                    timestamp.snapshot().version(),
                    snapshot.version()
                )));
            }

            // Note: this doesn't check the expiration because we need to be able to update it
            // regardless so we can prevent rollback attacks againsts targets/delegations.
            snapshot
        };

        if self.targets.as_ref().map(|s| s.version()).unwrap_or(0)
            != verified
                .meta()
                .get(&MetadataPath::from_role(&Role::Targets))
                .map(|m| m.version())
                .unwrap_or(0)
        {
            self.targets = None;
        }

        self.snapshot = Some(verified);
        self.purge_delegations();
        Ok(true)
    }

    fn purge_delegations(&mut self) {
        let purge = {
            let snapshot = match self.snapshot() {
                Some(s) => s,
                None => return,
            };
            let mut purge = HashSet::new();
            for (role, definition) in snapshot.meta().iter() {
                let delegation = match self.delegations.get(role) {
                    Some(d) => d,
                    None => continue,
                };

                if delegation.version() > definition.version() {
                    let _ = purge.insert(role.clone());
                    continue;
                }
            }

            purge
        };

        for role in &purge {
            let _ = self.delegations.remove(role);
        }
    }

    /// Verify and update the targets metadata.
    pub fn update_targets(
        &mut self,
        signed_targets: SignedMetadata<D, TargetsMetadata>,
    ) -> Result<bool> {
        let verified = {
            let root = self.safe_root_ref()?;
            let snapshot = self.safe_snapshot_ref()?;
            let targets_description = snapshot
                .meta()
                .get(&MetadataPath::from_role(&Role::Targets))
                .ok_or_else(|| {
                    Error::VerificationFailure(
                        "Snapshot metadata had no description of the targets metadata".into(),
                    )
                })?;

            let current_version = self.current_targets_version();

            if targets_description.version() < current_version {
                return Err(Error::VerificationFailure(format!(
                    "Attempted to roll back targets metadata at version {} to {}.",
                    current_version,
                    targets_description.version()
                )));
            } else if targets_description.version() == current_version {
                return Ok(false);
            }

            let targets = signed_targets.verify(
                root.targets().threshold(),
                root.keys().iter().filter_map(|(k, v)| {
                    if root.targets().key_ids().contains(k) {
                        Some(v)
                    } else {
                        None
                    }
                }),
            )?;

            if targets.version() != targets_description.version() {
                return Err(Error::VerificationFailure(format!(
                    "The timestamp metadata reported that the targets metadata should be at \
                     version {} but version {} was found instead.",
                    targets_description.version(),
                    targets.version()
                )));
            }

            if targets.expires() <= &Utc::now() {
                return Err(Error::ExpiredMetadata(Role::Snapshot));
            }

            targets
        };

        self.targets = Some(verified);
        Ok(true)
    }

    /// Find the signing keys and metadata for the delegation given by `role`, as seen from the
    /// point of view of `parent_role`.
    fn find_delegation(
        &self,
        parent_role: &MetadataPath,
        role: &MetadataPath,
    ) -> Option<(Vec<&PublicKey>, &Delegation)> {
        // Find the parent TargetsMetadata that is expected to refer to `role`.
        let parent = {
            if parent_role == &MetadataPath::from_role(&Role::Targets) {
                if let Some(targets) = self.targets() {
                    targets
                } else {
                    return None;
                }
            } else {
                if let Some(targets) = self.delegations.get(parent_role) {
                    targets
                } else {
                    return None;
                }
            }
        };

        // Only consider targets metadata that define delegations.
        let delegations = match parent.delegations() {
            Some(d) => d,
            None => return None,
        };

        for delegation in delegations.roles() {
            if delegation.role() != role {
                continue;
            }

            // Filter the delegations keys to just the ones for this delegation.
            let authorized_keys = delegations
                .keys()
                .iter()
                .filter_map(|(k, v)| {
                    if delegation.key_ids().contains(k) {
                        Some(v)
                    } else {
                        None
                    }
                })
                .collect();

            return Some((authorized_keys, delegation));
        }

        None
    }

    /// Verify and update a delegation metadata.
    pub fn update_delegation(
        &mut self,
        parent_role: &MetadataPath,
        role: &MetadataPath,
        signed_delegation: SignedMetadata<D, TargetsMetadata>,
    ) -> Result<bool> {
        let verified = {
            let _ = self.safe_root_ref()?;
            let snapshot = self.safe_snapshot_ref()?;
            let targets = self.safe_targets_ref()?;
            if targets.delegations().is_none() {
                return Err(Error::VerificationFailure(
                    "Delegations not authorized".into(),
                ));
            };

            let delegation_description = match snapshot.meta().get(role) {
                Some(d) => d,
                None => {
                    return Err(Error::VerificationFailure(format!(
                        "The degated role {:?} was not present in the snapshot metadata.",
                        role
                    )));
                }
            };

            let current_version = self.current_delegation_version(role);

            if delegation_description.version() < current_version {
                return Err(Error::VerificationFailure(format!(
                    "Snapshot metadata did listed delegation {:?} version as {} but current\
                     version is {}",
                    role,
                    delegation_description.version(),
                    current_version
                )));
            }

            // FIXME(#279) update_delegation trusts tuf::Client to provide too much information,
            // making this difficult to verify as correct.

            let (keys, delegation) =
                self.find_delegation(parent_role, role)
                    .ok_or(Error::VerificationFailure(format!(
                        "The delegated role {:?} is not known to the base \
                        targets metadata or any known delegated targets metadata",
                        role
                    )))?;
            let delegation = signed_delegation.verify(delegation.threshold(), keys)?;

            if current_version == delegation_description.version() {
                return Ok(false);
            }

            if delegation.version() != delegation_description.version() {
                return Err(Error::VerificationFailure(format!(
                    "The snapshot metadata reported that the delegation {:?} should be at \
                     version {} but version {} was found instead.",
                    role,
                    delegation_description.version(),
                    delegation.version(),
                )));
            }

            if delegation.expires() <= &Utc::now() {
                // TODO this needs to be chagned to accept a MetadataPath and not Role
                return Err(Error::ExpiredMetadata(Role::Targets));
            }

            delegation
        };

        let _ = self.delegations.insert(role.clone(), verified);

        Ok(true)
    }

    /// Get a reference to the description needed to verify the target defined by the given
    /// `VirtualTargetPath`. Returns an `Error` if the target is not defined in the trusted
    /// metadata. This may mean the target exists somewhere in the metadata, but the chain of trust
    /// to that target may be invalid or incomplete.
    pub fn target_description(&self, target_path: &VirtualTargetPath) -> Result<TargetDescription> {
        let _ = self.safe_root_ref()?;
        let _ = self.safe_snapshot_ref()?;
        let targets = self.safe_targets_ref()?;

        if let Some(d) = targets.targets().get(target_path) {
            return Ok(d.clone());
        }

        fn lookup<D: DataInterchange>(
            tuf: &Tuf<D>,
            default_terminate: bool,
            current_depth: u32,
            target_path: &VirtualTargetPath,
            delegations: &Delegations,
            parents: &[HashSet<VirtualTargetPath>],
            visited: &mut HashSet<MetadataPath>,
        ) -> (bool, Option<TargetDescription>) {
            for delegation in delegations.roles() {
                if visited.contains(delegation.role()) {
                    return (delegation.terminating(), None);
                }
                let _ = visited.insert(delegation.role().clone());

                let mut new_parents = parents.to_owned();
                new_parents.push(delegation.paths().clone());

                if current_depth > 0 && !target_path.matches_chain(&parents) {
                    return (delegation.terminating(), None);
                }

                let targets = match tuf.delegations.get(delegation.role()) {
                    Some(t) => t,
                    None => return (delegation.terminating(), None),
                };

                if targets.expires() <= &Utc::now() {
                    return (delegation.terminating(), None);
                }

                if let Some(d) = targets.targets().get(target_path) {
                    return (delegation.terminating(), Some(d.clone()));
                }

                if let Some(d) = targets.delegations() {
                    let mut new_parents = parents.to_vec();
                    new_parents.push(delegation.paths().clone());
                    let (term, res) = lookup(
                        tuf,
                        delegation.terminating(),
                        current_depth + 1,
                        target_path,
                        d,
                        &new_parents,
                        visited,
                    );
                    if term {
                        return (true, res);
                    } else if res.is_some() {
                        return (term, res);
                    }
                }
            }
            (default_terminate, None)
        }

        match targets.delegations() {
            Some(d) => {
                let mut visited = HashSet::new();
                lookup(self, false, 0, target_path, d, &[], &mut visited)
                    .1
                    .ok_or_else(|| Error::TargetUnavailable)
            }
            None => Err(Error::TargetUnavailable),
        }
    }

    fn purge_metadata(&mut self) {
        self.snapshot = None;
        self.targets = None;
        self.timestamp = None;
        self.delegations.clear();
    }

    fn safe_root_ref(&self) -> Result<&RootMetadata> {
        let root = &self.root;
        if root.expires() <= &Utc::now() {
            return Err(Error::ExpiredMetadata(Role::Root));
        }
        Ok(&root)
    }

    fn safe_snapshot_ref(&self) -> Result<&SnapshotMetadata> {
        match self.snapshot {
            Some(ref snapshot) => {
                if snapshot.expires() <= &Utc::now() {
                    return Err(Error::ExpiredMetadata(Role::Snapshot));
                }
                Ok(snapshot)
            }
            None => Err(Error::MissingMetadata(Role::Snapshot)),
        }
    }

    fn safe_targets_ref(&self) -> Result<&TargetsMetadata> {
        match self.targets {
            Some(ref targets) => {
                if targets.expires() <= &Utc::now() {
                    return Err(Error::ExpiredMetadata(Role::Targets));
                }
                Ok(targets)
            }
            None => Err(Error::MissingMetadata(Role::Targets)),
        }
    }
    fn safe_timestamp_ref(&self) -> Result<&TimestampMetadata> {
        match self.timestamp {
            Some(ref timestamp) => {
                if timestamp.expires() <= &Utc::now() {
                    return Err(Error::ExpiredMetadata(Role::Timestamp));
                }
                Ok(timestamp)
            }
            None => Err(Error::MissingMetadata(Role::Timestamp)),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::crypto::{HashAlgorithm, PrivateKey, SignatureScheme};
    use crate::interchange::Json;
    use crate::metadata::{
        RootMetadataBuilder, SnapshotMetadataBuilder, TargetsMetadataBuilder,
        TimestampMetadataBuilder,
    };
    use lazy_static::lazy_static;
    use matches::assert_matches;
    use std::iter::once;

    lazy_static! {
        static ref KEYS: Vec<PrivateKey> = {
            let keys: &[&[u8]] = &[
                include_bytes!("../tests/ed25519/ed25519-1.pk8.der"),
                include_bytes!("../tests/ed25519/ed25519-2.pk8.der"),
                include_bytes!("../tests/ed25519/ed25519-3.pk8.der"),
                include_bytes!("../tests/ed25519/ed25519-4.pk8.der"),
                include_bytes!("../tests/ed25519/ed25519-5.pk8.der"),
                include_bytes!("../tests/ed25519/ed25519-6.pk8.der"),
            ];
            keys.iter()
                .map(|b| PrivateKey::from_pkcs8(b, SignatureScheme::Ed25519).unwrap())
                .collect()
        };
    }

    #[test]
    fn root_trusted_keys_success() {
        let root_key = &KEYS[0];
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[0].public().clone())
            .targets_key(KEYS[0].public().clone())
            .timestamp_key(KEYS[0].public().clone())
            .signed::<Json>(&root_key)
            .unwrap();

        assert_matches!(
            Tuf::from_root_with_trusted_keys(root, 1, once(root_key.public())),
            Ok(_)
        );
    }

    #[test]
    fn root_trusted_keys_failure() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[0].public().clone())
            .targets_key(KEYS[0].public().clone())
            .timestamp_key(KEYS[0].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        assert_matches!(
            Tuf::from_root_with_trusted_keys(root, 1, once(KEYS[1].public())),
            Err(Error::VerificationFailure(s)) if s == "Signature threshold not met: 0/1"
        );
    }

    #[test]
    fn good_root_rotation() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[0].public().clone())
            .targets_key(KEYS[0].public().clone())
            .timestamp_key(KEYS[0].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut tuf = Tuf::from_trusted_root(root).unwrap();

        let mut root = RootMetadataBuilder::new()
            .version(2)
            .root_key(KEYS[1].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[1].public().clone())
            .timestamp_key(KEYS[1].public().clone())
            .signed::<Json>(&KEYS[1])
            .unwrap();

        // add the original key's signature to make it cross signed
        root.add_signature(&KEYS[0]).unwrap();

        assert_eq!(tuf.update_root(root.clone()), Ok(true));

        // second update should do nothing
        assert_eq!(tuf.update_root(root), Ok(false));
    }

    #[test]
    fn no_cross_sign_root_rotation() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[0].public().clone())
            .targets_key(KEYS[0].public().clone())
            .timestamp_key(KEYS[0].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut tuf = Tuf::from_trusted_root(root).unwrap();

        let root = RootMetadataBuilder::new()
            .root_key(KEYS[1].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[1].public().clone())
            .timestamp_key(KEYS[1].public().clone())
            .signed::<Json>(&KEYS[1])
            .unwrap();

        assert!(tuf.update_root(root).is_err());
    }

    #[test]
    fn good_timestamp_update() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[1].public().clone())
            .timestamp_key(KEYS[1].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut tuf = Tuf::from_trusted_root(root).unwrap();

        let snapshot = SnapshotMetadataBuilder::new()
            .signed::<Json>(&KEYS[1])
            .unwrap();

        let timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                .signed::<Json>(&KEYS[1])
                .unwrap();
        let parsed_timestamp = timestamp.assume_valid().unwrap();

        assert_eq!(
            tuf.update_timestamp(timestamp.clone()),
            Ok(Some(&parsed_timestamp))
        );

        // second update should do nothing
        assert_eq!(tuf.update_timestamp(timestamp), Ok(None))
    }

    #[test]
    fn bad_timestamp_update_wrong_key() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[1].public().clone())
            .timestamp_key(KEYS[1].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut tuf = Tuf::from_trusted_root(root).unwrap();

        let snapshot = SnapshotMetadataBuilder::new()
            .signed::<Json>(&KEYS[1])
            .unwrap();

        let timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                // sign it with the root key
                .signed::<Json>(&KEYS[0])
                .unwrap();

        assert!(tuf.update_timestamp(timestamp).is_err())
    }

    #[test]
    fn good_snapshot_update() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[2].public().clone())
            .timestamp_key(KEYS[2].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut tuf = Tuf::from_trusted_root(root).unwrap();

        let snapshot = SnapshotMetadataBuilder::new().signed(&KEYS[1]).unwrap();

        let timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                .signed::<Json>(&KEYS[2])
                .unwrap();

        tuf.update_timestamp(timestamp).unwrap();

        assert_eq!(tuf.update_snapshot(snapshot.clone()), Ok(true));

        // second update should do nothing
        assert_eq!(tuf.update_snapshot(snapshot), Ok(false));
    }

    #[test]
    fn bad_snapshot_update_wrong_key() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[2].public().clone())
            .timestamp_key(KEYS[2].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut tuf = Tuf::from_trusted_root(root).unwrap();

        let snapshot = SnapshotMetadataBuilder::new()
            .signed::<Json>(&KEYS[2])
            .unwrap();

        let timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                // sign it with the targets key
                .signed::<Json>(&KEYS[2])
                .unwrap();

        tuf.update_timestamp(timestamp).unwrap();

        assert!(tuf.update_snapshot(snapshot).is_err());
    }

    #[test]
    fn bad_snapshot_update_wrong_version() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[2].public().clone())
            .timestamp_key(KEYS[2].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut tuf = Tuf::from_trusted_root(root).unwrap();

        let snapshot = SnapshotMetadataBuilder::new()
            .version(2)
            .signed::<Json>(&KEYS[2])
            .unwrap();

        let timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                .signed::<Json>(&KEYS[2])
                .unwrap();

        tuf.update_timestamp(timestamp).unwrap();

        let snapshot = SnapshotMetadataBuilder::new()
            .version(1)
            .signed::<Json>(&KEYS[1])
            .unwrap();

        assert!(tuf.update_snapshot(snapshot).is_err());
    }

    #[test]
    fn good_targets_update() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[2].public().clone())
            .timestamp_key(KEYS[3].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let signed_targets = TargetsMetadataBuilder::new()
            .signed::<Json>(&KEYS[2])
            .unwrap();

        let snapshot = SnapshotMetadataBuilder::new()
            .insert_metadata(&signed_targets, &[HashAlgorithm::Sha256])
            .unwrap()
            .signed::<Json>(&KEYS[1])
            .unwrap();

        let timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                .signed::<Json>(&KEYS[3])
                .unwrap();

        let mut tuf = Tuf::from_trusted_root(root).unwrap();

        tuf.update_timestamp(timestamp).unwrap();
        tuf.update_snapshot(snapshot).unwrap();

        assert_eq!(tuf.update_targets(signed_targets.clone()), Ok(true));

        // second update should do nothing
        assert_eq!(tuf.update_targets(signed_targets), Ok(false));
    }

    #[test]
    fn bad_targets_update_wrong_key() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[2].public().clone())
            .timestamp_key(KEYS[3].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut tuf = Tuf::from_trusted_root(root).unwrap();

        let signed_targets = TargetsMetadataBuilder::new()
            // sign it with the timestamp key
            .signed::<Json>(&KEYS[3])
            .unwrap();

        let snapshot = SnapshotMetadataBuilder::new()
            .insert_metadata(&signed_targets, &[HashAlgorithm::Sha256])
            .unwrap()
            .signed::<Json>(&KEYS[1])
            .unwrap();

        let timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                .signed::<Json>(&KEYS[3])
                .unwrap();

        tuf.update_timestamp(timestamp).unwrap();
        tuf.update_snapshot(snapshot).unwrap();

        assert!(tuf.update_targets(signed_targets).is_err());
    }

    #[test]
    fn bad_targets_update_wrong_version() {
        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[2].public().clone())
            .timestamp_key(KEYS[3].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut tuf = Tuf::from_trusted_root(root).unwrap();

        let signed_targets = TargetsMetadataBuilder::new()
            .version(2)
            .signed::<Json>(&KEYS[2])
            .unwrap();

        let snapshot = SnapshotMetadataBuilder::new()
            .insert_metadata(&signed_targets, &[HashAlgorithm::Sha256])
            .unwrap()
            .signed::<Json>(&KEYS[1])
            .unwrap();

        let timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                .signed::<Json>(&KEYS[3])
                .unwrap();

        tuf.update_timestamp(timestamp).unwrap();
        tuf.update_snapshot(snapshot).unwrap();

        let signed_targets = TargetsMetadataBuilder::new()
            .version(1)
            .signed::<Json>(&KEYS[2])
            .unwrap();

        assert!(tuf.update_targets(signed_targets).is_err());
    }
}
