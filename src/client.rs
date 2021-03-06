//! Clients for high level interactions with TUF repositories.
//!
//! # Example
//!
//! ```no_run
//! # use futures_executor::block_on;
//! # use hyper::client::Client as HttpClient;
//! # use std::path::PathBuf;
//! # use std::str::FromStr;
//! # use tuf::{Result, Tuf};
//! # use tuf::crypto::KeyId;
//! # use tuf::client::{Client, Config};
//! # use tuf::metadata::{RootMetadata, Role, MetadataPath, MetadataVersion};
//! # use tuf::interchange::Json;
//! # use tuf::repository::{FileSystemRepository, HttpRepositoryBuilder};
//!
//! static TRUSTED_ROOT_KEY_IDS: &'static [&str] = &[
//!     "4750eaf6878740780d6f97b12dbad079fb012bec88c78de2c380add56d3f51db",
//!     "01892c662c8cd79fab20edec21de1dcb8b75d9353103face7fe086ff5c0098e4",
//!     "0823260c29956d0b64baa19a64c49c87922ba2bc532d09a6965aef044da490bd",
//! ];
//!
//! # fn main() -> Result<()> {
//! # block_on(async {
//! let key_ids: Vec<KeyId> = TRUSTED_ROOT_KEY_IDS.iter()
//!     .map(|k| KeyId::from_str(k).unwrap())
//!     .collect();
//!
//! let local = FileSystemRepository::<Json>::new(PathBuf::from("~/.rustup"))?;
//!
//! let remote = HttpRepositoryBuilder::new_with_uri(
//!     "https://static.rust-lang.org/".parse::<http::Uri>().unwrap(),
//!     HttpClient::new(),
//! )
//! .user_agent("rustup/1.4.0")
//! .build();
//!
//! let mut client = Client::with_trusted_root_keyids(
//!     Config::default(),
//!     &MetadataVersion::Number(1),
//!     1,
//!     &key_ids,
//!     local,
//!     remote,
//! ).await?;
//!
//! let _ = client.update().await?;
//! # Ok(())
//! # })
//! # }
//! ```

use chrono::offset::Utc;
use futures_io::{AsyncRead, AsyncWrite};
use futures_util::io::copy;
use log::{error, warn};
use std::future::Future;
use std::pin::Pin;

use crate::crypto::{self, HashAlgorithm, HashValue, KeyId, PublicKey};
use crate::error::Error;
use crate::interchange::DataInterchange;
use crate::metadata::{
    Metadata, MetadataPath, MetadataVersion, RawSignedMetadata, Role, RootMetadata, SignedMetadata,
    SnapshotMetadata, TargetDescription, TargetPath, TargetsMetadata, VirtualTargetPath,
};
use crate::repository::{Repository, RepositoryProvider, RepositoryStorage};
use crate::tuf::Tuf;
use crate::Result;

/// Translates real paths (where a file is stored) into virtual paths (how it is addressed in TUF)
/// and back.
///
/// Implementations must obey the following identities for all possible inputs.
///
/// ```
/// # use tuf::client::{PathTranslator, DefaultTranslator};
/// # use tuf::metadata::{VirtualTargetPath, TargetPath};
/// let path = TargetPath::new("foo".into()).unwrap();
/// let virt = VirtualTargetPath::new("foo".into()).unwrap();
/// let translator = DefaultTranslator::new();
/// assert_eq!(path,
///            translator.virtual_to_real(&translator.real_to_virtual(&path).unwrap()).unwrap());
/// assert_eq!(virt,
///            translator.real_to_virtual(&translator.virtual_to_real(&virt).unwrap()).unwrap());
/// ```
pub trait PathTranslator {
    /// Convert a real path into a virtual path.
    fn real_to_virtual(&self, path: &TargetPath) -> Result<VirtualTargetPath>;

    /// Convert a virtual path into a real path.
    fn virtual_to_real(&self, path: &VirtualTargetPath) -> Result<TargetPath>;
}

/// A `PathTranslator` that does nothing.
#[derive(Clone, Debug, Default)]
pub struct DefaultTranslator;

impl DefaultTranslator {
    /// Create a new `DefaultTranslator`.
    pub fn new() -> Self {
        DefaultTranslator
    }
}

impl PathTranslator for DefaultTranslator {
    fn real_to_virtual(&self, path: &TargetPath) -> Result<VirtualTargetPath> {
        VirtualTargetPath::new(path.value().into())
    }

    fn virtual_to_real(&self, path: &VirtualTargetPath) -> Result<TargetPath> {
        TargetPath::new(path.value().into())
    }
}

/// A client that interacts with TUF repositories.
#[derive(Debug)]
pub struct Client<D, L, R, T>
where
    D: DataInterchange + Sync,
    L: RepositoryProvider<D> + RepositoryStorage<D>,
    R: RepositoryProvider<D>,
    T: PathTranslator,
{
    tuf: Tuf<D>,
    config: Config<T>,
    local: Repository<L, D>,
    remote: Repository<R, D>,
}

impl<D, L, R, T> Client<D, L, R, T>
where
    D: DataInterchange + Sync,
    L: RepositoryProvider<D> + RepositoryStorage<D>,
    R: RepositoryProvider<D>,
    T: PathTranslator,
{
    /// Create a new TUF client. It will attempt to load the latest root metadata from the local
    /// repo and use it as the initial trusted root metadata, or it will return an error if it
    /// cannot do so.
    ///
    /// **WARNING**: This is trust-on-first-use (TOFU) and offers weaker security guarantees than
    /// the related methods [`Client::with_trusted_root`], [`Client::with_trusted_root_keys`] and
    /// [`Client::with_trusted_root_keyids`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use chrono::offset::{Utc, TimeZone};
    /// # use futures_executor::block_on;
    /// # use tuf::{
    /// #     Error,
    /// #     interchange::Json,
    /// #     client::{Client, Config},
    /// #     crypto::{KeyType, PrivateKey, SignatureScheme},
    /// #     metadata::{MetadataPath, MetadataVersion, Role, RootMetadataBuilder},
    /// #     repository::{EphemeralRepository, RepositoryStorage},
    /// # };
    /// # fn main() -> Result<(), Error> {
    /// # block_on(async {
    /// # let private_key = PrivateKey::from_pkcs8(
    /// #     &PrivateKey::new(KeyType::Ed25519)?,
    /// #     SignatureScheme::Ed25519,
    /// # )?;
    /// # let public_key = private_key.public().clone();
    /// let local = EphemeralRepository::<Json>::new();
    /// let remote = EphemeralRepository::<Json>::new();
    ///
    /// let root_version = 1;
    /// let root = RootMetadataBuilder::new()
    ///     .version(root_version)
    ///     .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
    ///     .root_key(public_key.clone())
    ///     .snapshot_key(public_key.clone())
    ///     .targets_key(public_key.clone())
    ///     .timestamp_key(public_key.clone())
    ///     .signed::<Json>(&private_key)?;
    ///
    /// let root_path = MetadataPath::from_role(&Role::Root);
    /// let root_version = MetadataVersion::Number(root_version);
    ///
    /// local.store_metadata(
    ///     &root_path,
    ///     &root_version,
    ///     root.to_raw().unwrap().as_bytes()
    /// ).await?;
    ///
    /// let client = Client::with_trusted_local(
    ///     Config::default(),
    ///     local,
    ///     remote,
    /// ).await?;
    /// # Ok(())
    /// # })
    /// # }
    /// ```
    pub async fn with_trusted_local(config: Config<T>, local: L, remote: R) -> Result<Self> {
        let (local, remote) = (Repository::new(local), Repository::new(remote));
        let root_path = MetadataPath::from_role(&Role::Root);

        // FIXME should this be MetadataVersion::None so we bootstrap with the latest version?
        let root_version = MetadataVersion::Number(1);

        let (_, root) = local
            .fetch_metadata(&root_path, &root_version, config.max_root_length, None)
            .await?;

        let tuf = Tuf::from_trusted_root(root)?;

        Ok(Client {
            tuf,
            config,
            local,
            remote,
        })
    }

    /// Create a new TUF client. It will trust this initial root metadata.
    ///
    /// # Examples
    ///
    /// ```
    /// # use chrono::offset::{Utc, TimeZone};
    /// # use futures_executor::block_on;
    /// # use tuf::{
    /// #     Error,
    /// #     interchange::Json,
    /// #     client::{Client, Config},
    /// #     crypto::{KeyType, PrivateKey, SignatureScheme},
    /// #     metadata::{MetadataPath, MetadataVersion, Role, RootMetadataBuilder},
    /// #     repository::{EphemeralRepository},
    /// # };
    /// # fn main() -> Result<(), Error> {
    /// # block_on(async {
    /// # let private_key = PrivateKey::from_pkcs8(
    /// #     &PrivateKey::new(KeyType::Ed25519)?,
    /// #     SignatureScheme::Ed25519,
    /// # )?;
    /// # let public_key = private_key.public().clone();
    /// let local = EphemeralRepository::<Json>::new();
    /// let remote = EphemeralRepository::<Json>::new();
    ///
    /// let root_version = 1;
    /// let root_threshold = 1;
    /// let root = RootMetadataBuilder::new()
    ///     .version(root_version)
    ///     .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
    ///     .root_key(public_key.clone())
    ///     .root_threshold(root_threshold)
    ///     .snapshot_key(public_key.clone())
    ///     .targets_key(public_key.clone())
    ///     .timestamp_key(public_key.clone())
    ///     .signed::<Json>(&private_key)
    ///     .unwrap();
    ///
    /// let client = Client::with_trusted_root(
    ///     Config::default(),
    ///     root,
    ///     local,
    ///     remote,
    /// ).await?;
    /// # Ok(())
    /// # })
    /// # }
    /// ```
    pub async fn with_trusted_root(
        config: Config<T>,
        trusted_root: SignedMetadata<D, RootMetadata>,
        local: L,
        remote: R,
    ) -> Result<Self> {
        let (local, remote) = (Repository::new(local), Repository::new(remote));
        let tuf = Tuf::from_trusted_root(trusted_root)?;

        Ok(Client {
            tuf,
            config,
            local,
            remote,
        })
    }

    /// Create a new TUF client. It will attempt to load initial root metadata the local and remote
    /// repositories, then extract out the public keys from this metadata that correspond to the
    /// provided key ids, and finally establish trust by verifying the root metadata was signed
    /// with these keys.
    ///
    /// # Examples
    ///
    /// ```
    /// # use chrono::offset::{Utc, TimeZone};
    /// # use futures_executor::block_on;
    /// # use std::iter::once;
    /// # use tuf::{
    /// #     Error,
    /// #     interchange::Json,
    /// #     client::{Client, Config},
    /// #     crypto::{KeyType, PrivateKey, SignatureScheme},
    /// #     metadata::{MetadataPath, MetadataVersion, Role, RootMetadataBuilder},
    /// #     repository::{EphemeralRepository, RepositoryStorage},
    /// # };
    /// # fn main() -> Result<(), Error> {
    /// # block_on(async {
    /// # let private_key = PrivateKey::from_pkcs8(
    /// #     &PrivateKey::new(KeyType::Ed25519)?,
    /// #     SignatureScheme::Ed25519,
    /// # )?;
    /// # let public_key = private_key.public().clone();
    /// let local = EphemeralRepository::<Json>::new();
    /// let remote = EphemeralRepository::<Json>::new();
    ///
    /// let root_version = 1;
    /// let root_threshold = 1;
    /// let root = RootMetadataBuilder::new()
    ///     .version(root_version)
    ///     .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
    ///     .root_key(public_key.clone())
    ///     .root_threshold(root_threshold)
    ///     .snapshot_key(public_key.clone())
    ///     .targets_key(public_key.clone())
    ///     .timestamp_key(public_key.clone())
    ///     .signed::<Json>(&private_key)?;
    ///
    /// let root_path = MetadataPath::from_role(&Role::Root);
    /// let root_version = MetadataVersion::Number(root_version);
    ///
    /// remote.store_metadata(
    ///     &root_path,
    ///     &root_version,
    ///     root.to_raw().unwrap().as_bytes()
    /// ).await?;
    ///
    /// let client = Client::with_trusted_root_keyids(
    ///     Config::default(),
    ///     &root_version,
    ///     root_threshold,
    ///     once(public_key.key_id()),
    ///     local,
    ///     remote,
    /// ).await?;
    /// # Ok(())
    /// # })
    /// # }
    /// ```
    pub async fn with_trusted_root_keyids<'a, I>(
        config: Config<T>,
        root_version: &MetadataVersion,
        root_threshold: u32,
        trusted_root_key_ids: I,
        local: L,
        remote: R,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = &'a KeyId>,
    {
        let (local, remote) = (Repository::new(local), Repository::new(remote));

        let root_path = MetadataPath::from_role(&Role::Root);
        let (fetched, raw_trusted_root, trusted_root) = fetch_metadata_from_local_or_else_remote(
            &root_path,
            &root_version,
            config.max_root_length,
            None,
            &local,
            &remote,
        )
        .await?;

        let tuf = {
            // Extract the necessary information from the not-yet-verified root metadata to verify
            // it is signed by the trusted root key_ids.
            let root: RootMetadata = trusted_root.assume_valid()?;
            let trusted_root_keys = trusted_root_key_ids
                .into_iter()
                .filter_map(|key_id| root.keys().get(key_id));

            Tuf::from_root_with_trusted_keys(trusted_root, root_threshold, trusted_root_keys)?
        };

        // FIXME(#253) verify the trusted root version matches the provided version.
        let root_version = MetadataVersion::Number(tuf.root().version());

        let mut client = Client {
            tuf,
            config,
            local,
            remote,
        };

        // Only store the metadata after we have validated it.
        if fetched {
            client
                .store_metadata(&root_path, &root_version, &raw_trusted_root)
                .await;

            // FIXME: should we also store the root as `MetadataVersion::None`?
        }

        Ok(client)
    }

    /// Create a new TUF client. It will attempt to load initial root metadata from the local and remote
    /// repositories using the provided keys to pin the verification.
    ///
    /// # Examples
    ///
    /// ```
    /// # use chrono::offset::{Utc, TimeZone};
    /// # use futures_executor::block_on;
    /// # use std::iter::once;
    /// # use tuf::{
    /// #     Error,
    /// #     interchange::Json,
    /// #     client::{Client, Config},
    /// #     crypto::{KeyType, PrivateKey, SignatureScheme},
    /// #     metadata::{MetadataPath, MetadataVersion, Role, RootMetadataBuilder},
    /// #     repository::{EphemeralRepository, RepositoryStorage},
    /// # };
    /// # fn main() -> Result<(), Error> {
    /// # block_on(async {
    /// # let private_key = PrivateKey::from_pkcs8(
    /// #     &PrivateKey::new(KeyType::Ed25519)?,
    /// #     SignatureScheme::Ed25519,
    /// # )?;
    /// # let public_key = private_key.public().clone();
    /// let local = EphemeralRepository::<Json>::new();
    /// let remote = EphemeralRepository::<Json>::new();
    ///
    /// let root_version = 1;
    /// let root_threshold = 1;
    /// let root = RootMetadataBuilder::new()
    ///     .version(root_version)
    ///     .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
    ///     .root_key(public_key.clone())
    ///     .root_threshold(root_threshold)
    ///     .snapshot_key(public_key.clone())
    ///     .targets_key(public_key.clone())
    ///     .timestamp_key(public_key.clone())
    ///     .signed::<Json>(&private_key)?;
    ///
    /// let root_path = MetadataPath::from_role(&Role::Root);
    /// let root_version = MetadataVersion::Number(root_version);
    ///
    /// remote.store_metadata(
    ///     &root_path,
    ///     &root_version,
    ///     root.to_raw().unwrap().as_bytes()
    /// ).await?;
    ///
    /// let client = Client::with_trusted_root_keys(
    ///     Config::default(),
    ///     &root_version,
    ///     root_threshold,
    ///     once(&public_key),
    ///     local,
    ///     remote,
    /// ).await?;
    /// # Ok(())
    /// # })
    /// # }
    /// ```
    pub async fn with_trusted_root_keys<'a, I>(
        config: Config<T>,
        root_version: &MetadataVersion,
        root_threshold: u32,
        trusted_root_keys: I,
        local: L,
        remote: R,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = &'a PublicKey>,
    {
        let (local, remote) = (Repository::new(local), Repository::new(remote));

        let root_path = MetadataPath::from_role(&Role::Root);
        let (fetched, raw_root, root) = fetch_metadata_from_local_or_else_remote(
            &root_path,
            root_version,
            config.max_root_length,
            None,
            &local,
            &remote,
        )
        .await?;

        let tuf = Tuf::from_root_with_trusted_keys(root, root_threshold, trusted_root_keys)?;

        // FIXME(#253) verify the trusted root version matches the provided version.
        let root_version = MetadataVersion::Number(tuf.root().version());

        let mut client = Client {
            tuf,
            config,
            local,
            remote,
        };

        // Only store the metadata after we have validated it.
        if fetched {
            client
                .store_metadata(&root_path, &root_version, &raw_root)
                .await;

            // FIXME: should we also store the root as `MetadataVersion::None`?
        }

        Ok(client)
    }

    /// Update TUF metadata from the remote repository.
    ///
    /// Returns `true` if an update occurred and `false` otherwise.
    pub async fn update(&mut self) -> Result<bool> {
        let r = self.update_root().await?;
        let ts = self.update_timestamp().await?;
        let sn = self.update_snapshot().await?;
        let ta = self.update_targets().await?;

        Ok(r || ts || sn || ta)
    }

    /// Store the metadata in the local repository. This is just a local cache, so we ignore if it
    /// experiences any errors.
    async fn store_metadata<'a, M>(
        &'a mut self,
        path: &'a MetadataPath,
        version: &'a MetadataVersion,
        metadata: &'a RawSignedMetadata<D, M>,
    ) where
        M: Metadata + Sync,
    {
        match self.local.store_metadata(path, version, metadata).await {
            Ok(()) => {}
            Err(err) => {
                warn!(
                    "failed to store metadata version {:?} to {}: {}",
                    version,
                    path.to_string(),
                    err,
                );
            }
        }
    }

    /// Returns `true` if an update occurred and `false` otherwise.
    async fn update_root(&mut self) -> Result<bool> {
        let root_path = MetadataPath::from_role(&Role::Root);

        let (raw_latest_root, latest_root) = self
            .remote
            .fetch_metadata(
                &root_path,
                &MetadataVersion::None,
                self.config.max_root_length,
                None,
            )
            .await?;

        // Root metadata is signed by its own keys, but we should only trust it if it is also
        // signed by the previous root metadata, which we can't check without knowing what version
        // this root metadata claims to be.
        let latest_version = latest_root.parse_version_untrusted()?;

        if latest_version < self.tuf.root().version() {
            return Err(Error::VerificationFailure(format!(
                "Latest root version is lower than current root version: {} < {}",
                latest_version,
                self.tuf.root().version()
            )));
        } else if latest_version == self.tuf.root().version() {
            return Ok(false);
        }

        let err_msg = "TUF claimed no update occurred when one should have. \
                       This is a programming error. Please report this as a bug.";

        for i in (self.tuf.root().version() + 1)..latest_version {
            let version = MetadataVersion::Number(i);

            let (raw_signed_root, signed_root) = self
                .remote
                .fetch_metadata(&root_path, &version, self.config.max_root_length, None)
                .await?;

            if !self.tuf.update_root(signed_root)? {
                error!("{}", err_msg);
                return Err(Error::Programming(err_msg.into()));
            }

            self.store_metadata(&root_path, &version, &raw_signed_root)
                .await;
        }

        if !self.tuf.update_root(latest_root)? {
            error!("{}", err_msg);
            return Err(Error::Programming(err_msg.into()));
        }

        let latest_version = MetadataVersion::Number(latest_version);

        self.store_metadata(&root_path, &latest_version, &raw_latest_root)
            .await;
        self.store_metadata(&root_path, &MetadataVersion::None, &raw_latest_root)
            .await;

        if self.tuf.root().expires() <= &Utc::now() {
            error!("Root metadata expired, potential freeze attack");
            return Err(Error::ExpiredMetadata(Role::Root));
        }

        Ok(true)
    }

    /// Returns `true` if an update occurred and `false` otherwise.
    async fn update_timestamp(&mut self) -> Result<bool> {
        let timestamp_path = MetadataPath::from_role(&Role::Timestamp);

        let (raw_signed_timestamp, signed_timestamp) = self
            .remote
            .fetch_metadata(
                &timestamp_path,
                &MetadataVersion::None,
                self.config.max_timestamp_length,
                None,
            )
            .await?;

        if let Some(updated_timestamp) = self.tuf.update_timestamp(signed_timestamp)? {
            let latest_version = MetadataVersion::Number(updated_timestamp.version());
            self.store_metadata(&timestamp_path, &latest_version, &raw_signed_timestamp)
                .await;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Returns `true` if an update occurred and `false` otherwise.
    async fn update_snapshot(&mut self) -> Result<bool> {
        // 5.3.1 Check against timestamp metadata. The hashes and version number listed in the
        // timestamp metadata. If hashes and version do not match, discard the new snapshot
        // metadata, abort the update cycle, and report the failure.
        let snapshot_description = match self.tuf.timestamp() {
            Some(ts) => Ok(ts.snapshot()),
            None => Err(Error::MissingMetadata(Role::Timestamp)),
        }?
        .clone();

        if snapshot_description.version() <= self.tuf.snapshot().map(|s| s.version()).unwrap_or(0) {
            return Ok(false);
        }

        let (alg, value) = crypto::hash_preference(snapshot_description.hashes())?;

        let version = if self.tuf.root().consistent_snapshot() {
            MetadataVersion::Number(snapshot_description.version())
        } else {
            MetadataVersion::None
        };

        let snapshot_path = MetadataPath::from_role(&Role::Snapshot);
        let snapshot_length = Some(snapshot_description.length());

        let (raw_signed_snapshot, signed_snapshot) = self
            .remote
            .fetch_metadata(
                &snapshot_path,
                &version,
                snapshot_length,
                Some((alg, value.clone())),
            )
            .await?;

        if self.tuf.update_snapshot(signed_snapshot)? {
            self.store_metadata(&snapshot_path, &version, &raw_signed_snapshot)
                .await;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Returns `true` if an update occurred and `false` otherwise.
    async fn update_targets(&mut self) -> Result<bool> {
        let targets_description = match self.tuf.snapshot() {
            Some(sn) => match sn.meta().get(&MetadataPath::from_role(&Role::Targets)) {
                Some(d) => Ok(d),
                None => Err(Error::VerificationFailure(
                    "Snapshot metadata did not contain a description of the \
                     current targets metadata."
                        .into(),
                )),
            },
            None => Err(Error::MissingMetadata(Role::Snapshot)),
        }?
        .clone();

        if targets_description.version() <= self.tuf.targets().map(|t| t.version()).unwrap_or(0) {
            return Ok(false);
        }

        let (alg, value) = crypto::hash_preference(targets_description.hashes())?;

        let version = if self.tuf.root().consistent_snapshot() {
            MetadataVersion::Number(targets_description.version())
        } else {
            MetadataVersion::None
        };

        let targets_path = MetadataPath::from_role(&Role::Targets);
        let targets_length = Some(targets_description.length());

        let (raw_signed_targets, signed_targets) = self
            .remote
            .fetch_metadata(
                &targets_path,
                &version,
                targets_length,
                Some((alg, value.clone())),
            )
            .await?;

        if self.tuf.update_targets(signed_targets)? {
            self.store_metadata(&targets_path, &version, &raw_signed_targets)
                .await;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Fetch a target from the remote repo and write it to the local repo.
    pub async fn fetch_target<'a>(&'a mut self, target: &'a TargetPath) -> Result<()> {
        let read = self._fetch_target(target).await?;
        self.local.store_target(read, target).await
    }

    /// Fetch a target from the remote repo and write it to the provided writer.
    ///
    /// It is **critical** that none of the bytes written to the `write` are used until this future
    /// returns `Ok`, as the hash of the target is not verified until all bytes are read from the
    /// repository.
    pub async fn fetch_target_to_writer<'a, W>(
        &'a mut self,
        target: &'a TargetPath,
        mut write: W,
    ) -> Result<()>
    where
        W: AsyncWrite + Send + Unpin,
    {
        let read = self._fetch_target(&target).await?;
        copy(read, &mut write).await?;
        Ok(())
    }

    /// Fetch a target description from the remote repo and return it.
    pub async fn fetch_target_description<'a>(
        &'a mut self,
        target: &'a TargetPath,
    ) -> Result<TargetDescription> {
        let virt = self.config.path_translator.real_to_virtual(target)?;

        let snapshot = self
            .tuf
            .snapshot()
            .ok_or_else(|| Error::MissingMetadata(Role::Snapshot))?
            .clone();
        let (_, target_description) = self
            .lookup_target_description(false, 0, &virt, &snapshot, None)
            .await;
        target_description
    }

    // TODO this should check the local repo first
    async fn _fetch_target<'a>(
        &'a mut self,
        target: &'a TargetPath,
    ) -> Result<impl AsyncRead + Send + Unpin> {
        let target_description = self.fetch_target_description(target).await?;

        // According to TUF section 5.5.2, when consistent snapshot is enabled, target files should
        // be found at `$HASH.FILENAME.EXT`. Otherwise it is stored at `FILENAME.EXT`.
        if self.tuf.root().consistent_snapshot() {
            let (_, value) = crypto::hash_preference(target_description.hashes())?;
            let target = target.with_hash_prefix(value)?;
            self.remote.fetch_target(&target, &target_description).await
        } else {
            self.remote.fetch_target(target, &target_description).await
        }
    }

    async fn lookup_target_description<'a>(
        &'a mut self,
        default_terminate: bool,
        current_depth: u32,
        target: &'a VirtualTargetPath,
        snapshot: &'a SnapshotMetadata,
        targets: Option<(&'a TargetsMetadata, MetadataPath)>,
    ) -> (bool, Result<TargetDescription>) {
        if current_depth > self.config.max_delegation_depth {
            warn!(
                "Walking the delegation graph would have exceeded the configured max depth: {}",
                self.config.max_delegation_depth
            );
            return (default_terminate, Err(Error::NotFound));
        }

        // these clones are dumb, but we need immutable values and not references for update
        // tuf in the loop below
        let (targets, targets_role) = match targets {
            Some((t, role)) => (t.clone(), role),
            None => match self.tuf.targets() {
                Some(t) => (t.clone(), MetadataPath::from_role(&Role::Targets)),
                None => {
                    return (
                        default_terminate,
                        Err(Error::MissingMetadata(Role::Targets)),
                    );
                }
            },
        };

        if let Some(t) = targets.targets().get(target) {
            return (default_terminate, Ok(t.clone()));
        }

        let delegations = match targets.delegations() {
            Some(d) => d,
            None => return (default_terminate, Err(Error::NotFound)),
        };

        for delegation in delegations.roles().iter() {
            if !delegation.paths().iter().any(|p| target.is_child(p)) {
                if delegation.terminating() {
                    return (true, Err(Error::NotFound));
                } else {
                    continue;
                }
            }

            let role_meta = match snapshot.meta().get(delegation.role()) {
                Some(m) => m,
                None if !delegation.terminating() => continue,
                None => return (true, Err(Error::NotFound)),
            };

            let (alg, value) = match crypto::hash_preference(role_meta.hashes()) {
                Ok(h) => h,
                Err(e) => return (delegation.terminating(), Err(e)),
            };

            let version = if self.tuf.root().consistent_snapshot() {
                MetadataVersion::Hash(value.clone())
            } else {
                MetadataVersion::None
            };

            let role_length = Some(role_meta.length());
            let raw_signed_meta = self
                .local
                .fetch_metadata(
                    delegation.role(),
                    &MetadataVersion::None,
                    role_length,
                    Some((alg, value.clone())),
                )
                .await;

            let (raw_signed_meta, signed_meta) = match raw_signed_meta {
                Ok(m) => m,
                Err(_) => {
                    match self
                        .remote
                        .fetch_metadata(
                            delegation.role(),
                            &version,
                            role_length,
                            Some((alg, value.clone())),
                        )
                        .await
                    {
                        Ok(m) => m,
                        Err(ref e) if !delegation.terminating() => {
                            warn!("Failed to fetch metadata {:?}: {:?}", delegation.role(), e);
                            continue;
                        }
                        Err(e) => {
                            warn!("Failed to fetch metadata {:?}: {:?}", delegation.role(), e);
                            return (true, Err(e));
                        }
                    }
                }
            };

            match self
                .tuf
                .update_delegation(&targets_role, delegation.role(), signed_meta)
            {
                Ok(_) => {
                    match self
                        .local
                        .store_metadata(delegation.role(), &MetadataVersion::None, &raw_signed_meta)
                        .await
                    {
                        Ok(_) => (),
                        Err(e) => warn!(
                            "Error storing metadata {:?} locally: {:?}",
                            delegation.role(),
                            e
                        ),
                    }

                    let meta = self
                        .tuf
                        .delegations()
                        .get(delegation.role())
                        .unwrap()
                        .clone();
                    let f: Pin<Box<dyn Future<Output = _>>> =
                        Box::pin(self.lookup_target_description(
                            delegation.terminating(),
                            current_depth + 1,
                            target,
                            snapshot,
                            Some((&meta, delegation.role().clone())),
                        ));
                    let (term, res) = f.await;

                    if term && res.is_err() {
                        return (true, res);
                    }

                    // TODO end recursion early
                }
                Err(_) if !delegation.terminating() => continue,
                Err(e) => return (true, Err(e)),
            };
        }

        (default_terminate, Err(Error::NotFound))
    }
}

/// Helper function that first tries to fetch the metadata from the local store, and if it doesn't
/// exist or does and fails to parse, try fetching it from the remote store.
async fn fetch_metadata_from_local_or_else_remote<'a, D, L, R, M>(
    path: &'a MetadataPath,
    version: &'a MetadataVersion,
    max_length: Option<usize>,
    hash_data: Option<(&'static HashAlgorithm, HashValue)>,
    local: &'a Repository<L, D>,
    remote: &'a Repository<R, D>,
) -> Result<(bool, RawSignedMetadata<D, M>, SignedMetadata<D, M>)>
where
    D: DataInterchange + Sync,
    L: RepositoryProvider<D> + RepositoryStorage<D>,
    R: RepositoryProvider<D>,
    M: Metadata + 'static,
{
    match local
        .fetch_metadata(path, version, max_length, hash_data.clone())
        .await
    {
        Ok((raw_meta, meta)) => Ok((false, raw_meta, meta)),
        Err(Error::NotFound) => {
            let (raw_meta, meta) = remote
                .fetch_metadata(path, version, max_length, hash_data)
                .await?;
            Ok((true, raw_meta, meta))
        }
        Err(err) => Err(err),
    }
}

/// Configuration for a TUF `Client`.
///
/// # Defaults
///
/// The following values are considered reasonably safe defaults, however these values may change
/// as this crate moves out of beta. If you are concered about them changing, you should use the
/// `ConfigBuilder` and set your own values.
///
/// ```
/// # use tuf::client::{Config, DefaultTranslator};
/// let config = Config::default();
/// assert_eq!(config.max_root_length(), &Some(1024 * 1024));
/// assert_eq!(config.max_timestamp_length(), &Some(32 * 1024));
/// assert_eq!(config.max_delegation_depth(), 8);
/// let _: &DefaultTranslator = config.path_translator();
/// ```
#[derive(Clone, Debug)]
pub struct Config<T>
where
    T: PathTranslator,
{
    max_root_length: Option<usize>,
    max_timestamp_length: Option<usize>,
    max_delegation_depth: u32,
    path_translator: T,
}

impl Config<DefaultTranslator> {
    /// Initialize a `ConfigBuilder` with the default values.
    pub fn build() -> ConfigBuilder<DefaultTranslator> {
        ConfigBuilder::default()
    }
}

impl<T> Config<T>
where
    T: PathTranslator,
{
    /// Return the optional maximum root metadata length.
    pub fn max_root_length(&self) -> &Option<usize> {
        &self.max_root_length
    }

    /// Return the optional maximum timestamp metadata size.
    pub fn max_timestamp_length(&self) -> &Option<usize> {
        &self.max_timestamp_length
    }

    /// The maximum number of steps used when walking the delegation graph.
    pub fn max_delegation_depth(&self) -> u32 {
        self.max_delegation_depth
    }

    /// The `PathTranslator`.
    pub fn path_translator(&self) -> &T {
        &self.path_translator
    }
}

impl Default for Config<DefaultTranslator> {
    fn default() -> Self {
        Config {
            max_root_length: Some(1024 * 1024),
            max_timestamp_length: Some(32 * 1024),
            max_delegation_depth: 8,
            path_translator: DefaultTranslator::new(),
        }
    }
}

/// Helper for building and validating a TUF client `Config`.
#[derive(Debug, PartialEq)]
pub struct ConfigBuilder<T>
where
    T: PathTranslator,
{
    max_root_length: Option<usize>,
    max_timestamp_length: Option<usize>,
    max_delegation_depth: u32,
    path_translator: T,
}

impl<T> ConfigBuilder<T>
where
    T: PathTranslator,
{
    /// Validate this builder return a `Config` if validation succeeds.
    pub fn finish(self) -> Result<Config<T>> {
        Ok(Config {
            max_root_length: self.max_root_length,
            max_timestamp_length: self.max_timestamp_length,
            max_delegation_depth: self.max_delegation_depth,
            path_translator: self.path_translator,
        })
    }

    /// Set the optional maximum download length for root metadata.
    pub fn max_root_length(mut self, max: Option<usize>) -> Self {
        self.max_root_length = max;
        self
    }

    /// Set the optional maximum download length for timestamp metadata.
    pub fn max_timestamp_length(mut self, max: Option<usize>) -> Self {
        self.max_timestamp_length = max;
        self
    }

    /// Set the maximum number of steps used when walking the delegation graph.
    pub fn max_delegation_depth(mut self, max: u32) -> Self {
        self.max_delegation_depth = max;
        self
    }

    /// Set the `PathTranslator`.
    pub fn path_translator<TT>(self, path_translator: TT) -> ConfigBuilder<TT>
    where
        TT: PathTranslator,
    {
        ConfigBuilder {
            max_root_length: self.max_root_length,
            max_timestamp_length: self.max_timestamp_length,
            max_delegation_depth: self.max_delegation_depth,
            path_translator,
        }
    }
}

impl Default for ConfigBuilder<DefaultTranslator> {
    fn default() -> ConfigBuilder<DefaultTranslator> {
        let cfg = Config::default();
        ConfigBuilder {
            max_root_length: cfg.max_root_length,
            max_timestamp_length: cfg.max_timestamp_length,
            max_delegation_depth: cfg.max_delegation_depth,
            path_translator: cfg.path_translator,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::crypto::{HashAlgorithm, KeyType, PrivateKey, SignatureScheme};
    use crate::interchange::Json;
    use crate::metadata::{
        MetadataPath, MetadataVersion, RootMetadata, RootMetadataBuilder, SnapshotMetadataBuilder,
        TargetsMetadataBuilder, TimestampMetadataBuilder,
    };
    use crate::repository::EphemeralRepository;
    use chrono::prelude::*;
    use futures_executor::block_on;
    use lazy_static::lazy_static;
    use maplit::hashmap;
    use matches::assert_matches;
    use serde_json::json;
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
    fn client_constructors_err_with_not_found() {
        block_on(async {
            let local = EphemeralRepository::<Json>::new();
            let remote = EphemeralRepository::<Json>::new();

            let private_key = PrivateKey::from_pkcs8(
                &PrivateKey::new(KeyType::Ed25519).unwrap(),
                SignatureScheme::Ed25519,
            )
            .unwrap();
            let public_key = private_key.public().clone();

            assert_matches!(
                Client::with_trusted_local(Config::default(), &local, &remote).await,
                Err(Error::NotFound)
            );

            assert_matches!(
                Client::with_trusted_root_keys(
                    Config::default(),
                    &MetadataVersion::Number(1),
                    1,
                    once(&public_key),
                    &local,
                    &remote,
                )
                .await,
                Err(Error::NotFound)
            );

            assert_matches!(
                Client::with_trusted_root_keyids(
                    Config::default(),
                    &MetadataVersion::Number(1),
                    1,
                    once(public_key.key_id()),
                    &local,
                    &remote,
                )
                .await,
                Err(Error::NotFound)
            );
        })
    }

    #[test]
    fn client_constructors_err_with_invalid_keys() {
        block_on(async {
            let local = EphemeralRepository::new();
            let remote = EphemeralRepository::new();
            let mut repo = Repository::<_, Json>::new(&remote);

            let good_private_key = PrivateKey::from_pkcs8(
                &PrivateKey::new(KeyType::Ed25519).unwrap(),
                SignatureScheme::Ed25519,
            )
            .unwrap();
            let good_public_key = good_private_key.public().clone();

            let root_version = 1;
            let root = RootMetadataBuilder::new()
                .version(root_version)
                .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
                .root_key(good_public_key.clone())
                .snapshot_key(good_public_key.clone())
                .targets_key(good_public_key.clone())
                .timestamp_key(good_public_key.clone())
                .signed::<Json>(&good_private_key)
                .unwrap();

            let root_path = MetadataPath::from_role(&Role::Root);
            let root_version = MetadataVersion::Number(root_version);

            repo.store_metadata(&root_path, &root_version, &root.to_raw().unwrap())
                .await
                .unwrap();

            let bad_private_key = PrivateKey::from_pkcs8(
                &PrivateKey::new(KeyType::Ed25519).unwrap(),
                SignatureScheme::Ed25519,
            )
            .unwrap();
            let bad_public_key = bad_private_key.public().clone();

            assert_matches!(
                Client::with_trusted_root_keys(
                    Config::default(),
                    &root_version,
                    1,
                    once(&bad_public_key),
                    &local,
                    &remote,
                )
                .await,
                Err(Error::VerificationFailure(_))
            );

            assert_matches!(
                Client::with_trusted_root_keyids(
                    Config::default(),
                    &root_version,
                    1,
                    once(bad_public_key.key_id()),
                    &local,
                    &remote,
                )
                .await,
                Err(Error::VerificationFailure(_))
            );
        })
    }

    #[test]
    fn root_chain_update() {
        block_on(async {
            let repo = EphemeralRepository::<Json>::new();
            let mut remote = Repository::new(&repo);

            //// First, create the root metadata.
            let root1 = RootMetadataBuilder::new()
                .version(1)
                .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
                .root_key(KEYS[0].public().clone())
                .snapshot_key(KEYS[0].public().clone())
                .targets_key(KEYS[0].public().clone())
                .timestamp_key(KEYS[0].public().clone())
                .signed::<Json>(&KEYS[0])
                .unwrap();

            let mut root2 = RootMetadataBuilder::new()
                .version(2)
                .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
                .root_key(KEYS[1].public().clone())
                .snapshot_key(KEYS[1].public().clone())
                .targets_key(KEYS[1].public().clone())
                .timestamp_key(KEYS[1].public().clone())
                .signed::<Json>(&KEYS[1])
                .unwrap();

            root2.add_signature(&KEYS[0]).unwrap();

            // Make sure the version 2 is signed by version 1's keys.
            root2.add_signature(&KEYS[0]).unwrap();

            let mut root3 = RootMetadataBuilder::new()
                .version(3)
                .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
                .root_key(KEYS[2].public().clone())
                .snapshot_key(KEYS[2].public().clone())
                .targets_key(KEYS[2].public().clone())
                .timestamp_key(KEYS[2].public().clone())
                .signed::<Json>(&KEYS[2])
                .unwrap();

            // Make sure the version 3 is signed by version 2's keys.
            root3.add_signature(&KEYS[1]).unwrap();

            let mut targets = TargetsMetadataBuilder::new()
                .signed::<Json>(&KEYS[0])
                .unwrap();

            targets.add_signature(&KEYS[1]).unwrap();
            targets.add_signature(&KEYS[2]).unwrap();

            let mut snapshot = SnapshotMetadataBuilder::new()
                .insert_metadata(&targets, &[HashAlgorithm::Sha256])
                .unwrap()
                .signed::<Json>(&KEYS[0])
                .unwrap();

            snapshot.add_signature(&KEYS[1]).unwrap();
            snapshot.add_signature(&KEYS[2]).unwrap();

            let mut timestamp =
                TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                    .unwrap()
                    .signed::<Json>(&KEYS[0])
                    .unwrap();

            timestamp.add_signature(&KEYS[1]).unwrap();
            timestamp.add_signature(&KEYS[2]).unwrap();

            ////
            // Now register the metadata.

            let root_path = MetadataPath::from_role(&Role::Root);
            let targets_path = MetadataPath::from_role(&Role::Targets);
            let snapshot_path = MetadataPath::from_role(&Role::Snapshot);
            let timestamp_path = MetadataPath::from_role(&Role::Timestamp);

            remote
                .store_metadata(
                    &root_path,
                    &MetadataVersion::Number(1),
                    &root1.to_raw().unwrap(),
                )
                .await
                .unwrap();

            remote
                .store_metadata(&root_path, &MetadataVersion::None, &root1.to_raw().unwrap())
                .await
                .unwrap();

            remote
                .store_metadata(
                    &targets_path,
                    &MetadataVersion::Number(1),
                    &targets.to_raw().unwrap(),
                )
                .await
                .unwrap();

            remote
                .store_metadata(
                    &targets_path,
                    &MetadataVersion::None,
                    &targets.to_raw().unwrap(),
                )
                .await
                .unwrap();

            remote
                .store_metadata(
                    &snapshot_path,
                    &MetadataVersion::Number(1),
                    &snapshot.to_raw().unwrap(),
                )
                .await
                .unwrap();

            remote
                .store_metadata(
                    &snapshot_path,
                    &MetadataVersion::None,
                    &snapshot.to_raw().unwrap(),
                )
                .await
                .unwrap();

            remote
                .store_metadata(
                    &timestamp_path,
                    &MetadataVersion::Number(1),
                    &timestamp.to_raw().unwrap(),
                )
                .await
                .unwrap();

            remote
                .store_metadata(
                    &timestamp_path,
                    &MetadataVersion::None,
                    &timestamp.to_raw().unwrap(),
                )
                .await
                .unwrap();

            ////
            // Now, make sure that the local metadata got version 1.
            let key_ids = [KEYS[0].public().key_id().clone()];
            let mut client = Client::with_trusted_root_keyids(
                Config::default(),
                &MetadataVersion::Number(1),
                1,
                &key_ids,
                EphemeralRepository::new(),
                repo,
            )
            .await
            .unwrap();

            assert_eq!(client.update().await, Ok(true));
            assert_eq!(client.tuf.root().version(), 1);

            assert_eq!(
                root1.to_raw().unwrap(),
                client
                    .local
                    .fetch_metadata::<RootMetadata>(
                        &root_path,
                        &MetadataVersion::Number(1),
                        None,
                        None
                    )
                    .await
                    .unwrap()
                    .0
            );

            ////
            // Now bump the root to version 3

            client
                .remote
                .store_metadata(
                    &root_path,
                    &MetadataVersion::Number(2),
                    &root2.to_raw().unwrap(),
                )
                .await
                .unwrap();

            client
                .remote
                .store_metadata(&root_path, &MetadataVersion::None, &root2.to_raw().unwrap())
                .await
                .unwrap();

            client
                .remote
                .store_metadata(
                    &root_path,
                    &MetadataVersion::Number(3),
                    &root3.to_raw().unwrap(),
                )
                .await
                .unwrap();

            client
                .remote
                .store_metadata(&root_path, &MetadataVersion::None, &root3.to_raw().unwrap())
                .await
                .unwrap();

            ////
            // Finally, check that the update brings us to version 3.

            assert_eq!(client.update().await, Ok(true));
            assert_eq!(client.tuf.root().version(), 3);

            assert_eq!(
                root3.to_raw().unwrap(),
                client
                    .local
                    .fetch_metadata::<RootMetadata>(
                        &root_path,
                        &MetadataVersion::Number(3),
                        None,
                        None
                    )
                    .await
                    .unwrap()
                    .0
            );
        });
    }

    #[test]
    fn versioned_init_consistent_snapshot_false() {
        block_on(test_versioned_init(false))
    }

    #[test]
    fn versioned_init_consistent_snapshot_true() {
        block_on(test_versioned_init(true))
    }

    async fn test_versioned_init(consistent_snapshot: bool) {
        let repo = EphemeralRepository::<Json>::new();
        let mut remote = Repository::new(&repo);

        //// First, create the root metadata.
        let root1 = RootMetadataBuilder::new()
            .consistent_snapshot(consistent_snapshot)
            .version(1)
            .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[0].public().clone())
            .targets_key(KEYS[0].public().clone())
            .timestamp_key(KEYS[0].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut root2 = RootMetadataBuilder::new()
            .consistent_snapshot(consistent_snapshot)
            .version(2)
            .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
            .root_key(KEYS[1].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[1].public().clone())
            .timestamp_key(KEYS[1].public().clone())
            .signed::<Json>(&KEYS[1])
            .unwrap();

        // Make sure the version 2 is signed by version 1's keys.
        root2.add_signature(&KEYS[0]).unwrap();

        let mut targets = TargetsMetadataBuilder::new()
            .signed::<Json>(&KEYS[0])
            .unwrap();

        targets.add_signature(&KEYS[1]).unwrap();

        let mut snapshot = SnapshotMetadataBuilder::new()
            .insert_metadata(&targets, &[HashAlgorithm::Sha256])
            .unwrap()
            .signed::<Json>(&KEYS[0])
            .unwrap();

        snapshot.add_signature(&KEYS[1]).unwrap();

        let mut timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                .signed::<Json>(&KEYS[0])
                .unwrap();

        timestamp.add_signature(&KEYS[1]).unwrap();

        ////
        // Now register the metadata (root version 1 and 2).
        let root_path = MetadataPath::from_role(&Role::Root);
        let targets_path = MetadataPath::from_role(&Role::Targets);
        let snapshot_path = MetadataPath::from_role(&Role::Snapshot);
        let timestamp_path = MetadataPath::from_role(&Role::Timestamp);

        remote
            .store_metadata(
                &root_path,
                &MetadataVersion::Number(1),
                &root1.to_raw().unwrap(),
            )
            .await
            .unwrap();

        remote
            .store_metadata(&root_path, &MetadataVersion::None, &root1.to_raw().unwrap())
            .await
            .unwrap();

        let metadata_version = if consistent_snapshot {
            MetadataVersion::Number(1)
        } else {
            MetadataVersion::None
        };

        remote
            .store_metadata(&targets_path, &metadata_version, &targets.to_raw().unwrap())
            .await
            .unwrap();

        remote
            .store_metadata(
                &snapshot_path,
                &metadata_version,
                &snapshot.to_raw().unwrap(),
            )
            .await
            .unwrap();

        remote
            .store_metadata(
                &timestamp_path,
                &MetadataVersion::None,
                &timestamp.to_raw().unwrap(),
            )
            .await
            .unwrap();

        remote
            .store_metadata(
                &root_path,
                &MetadataVersion::Number(2),
                &root2.to_raw().unwrap(),
            )
            .await
            .unwrap();

        remote
            .store_metadata(&root_path, &MetadataVersion::None, &root2.to_raw().unwrap())
            .await
            .unwrap();

        ////
        // Initialize with root metadata version 2.
        let key_ids = [
            KEYS[0].public().key_id().clone(),
            KEYS[1].public().key_id().clone(),
        ];
        let mut client = Client::with_trusted_root_keyids(
            Config::build().finish().unwrap(),
            &MetadataVersion::Number(2),
            1,
            &key_ids,
            EphemeralRepository::new(),
            repo,
        )
        .await
        .unwrap();

        ////
        // Ensure client doesn't fetch previous version (1).
        assert_eq!(client.update().await, Ok(true));
        assert_eq!(client.tuf.root().version(), 2);

        assert_eq!(
            root2.to_raw().unwrap(),
            client
                .local
                .fetch_metadata::<RootMetadata>(&root_path, &MetadataVersion::Number(2), None, None)
                .await
                .unwrap()
                .0
        );
    }

    #[test]
    fn test_fetch_target_description_standard() {
        block_on(test_fetch_target_description(
            "standard/metadata".to_string(),
            TargetDescription::from_reader(
                "target with no custom metadata".as_bytes(),
                &[HashAlgorithm::Sha256],
            )
            .unwrap(),
        ));
    }

    #[test]
    fn test_fetch_target_description_custom_empty() {
        block_on(test_fetch_target_description(
            "custom-empty".to_string(),
            TargetDescription::from_reader_with_custom(
                "target with empty custom metadata".as_bytes(),
                &[HashAlgorithm::Sha256],
                hashmap!(),
            )
            .unwrap(),
        ));
    }

    #[test]
    fn test_fetch_target_description_custom() {
        block_on(test_fetch_target_description(
            "custom/metadata".to_string(),
            TargetDescription::from_reader_with_custom(
                "target with lots of custom metadata".as_bytes(),
                &[HashAlgorithm::Sha256],
                hashmap!(
                    "string".to_string() => json!("string"),
                    "bool".to_string() => json!(true),
                    "int".to_string() => json!(42),
                    "object".to_string() => json!({
                        "string": json!("string"),
                        "bool": json!(true),
                        "int": json!(42),
                    }),
                    "array".to_string() => json!([1, 2, 3]),
                ),
            )
            .unwrap(),
        ));
    }

    async fn test_fetch_target_description(path: String, expected_description: TargetDescription) {
        // Generate an ephemeral repository with a single target.
        let repo = EphemeralRepository::<Json>::new();
        let mut remote = Repository::new(&repo);

        let root = RootMetadataBuilder::new()
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[0].public().clone())
            .targets_key(KEYS[0].public().clone())
            .timestamp_key(KEYS[0].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let targets = TargetsMetadataBuilder::new()
            .insert_target_description(
                VirtualTargetPath::new(path.clone()).unwrap(),
                expected_description.clone(),
            )
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let snapshot = SnapshotMetadataBuilder::new()
            .insert_metadata(&targets, &[HashAlgorithm::Sha256])
            .unwrap()
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                .signed::<Json>(&KEYS[0])
                .unwrap();

        // Register the metadata in the remote repository.
        let root_path = MetadataPath::from_role(&Role::Root);
        let targets_path = MetadataPath::from_role(&Role::Targets);
        let snapshot_path = MetadataPath::from_role(&Role::Snapshot);
        let timestamp_path = MetadataPath::from_role(&Role::Timestamp);

        remote
            .store_metadata(
                &root_path,
                &MetadataVersion::Number(1),
                &root.to_raw().unwrap(),
            )
            .await
            .unwrap();

        remote
            .store_metadata(&root_path, &MetadataVersion::None, &root.to_raw().unwrap())
            .await
            .unwrap();

        remote
            .store_metadata(
                &targets_path,
                &MetadataVersion::None,
                &targets.to_raw().unwrap(),
            )
            .await
            .unwrap();

        remote
            .store_metadata(
                &snapshot_path,
                &MetadataVersion::None,
                &snapshot.to_raw().unwrap(),
            )
            .await
            .unwrap();

        remote
            .store_metadata(
                &timestamp_path,
                &MetadataVersion::None,
                &timestamp.to_raw().unwrap(),
            )
            .await
            .unwrap();

        // Initialize and update client.
        let mut client =
            Client::with_trusted_root(Config::default(), root, EphemeralRepository::new(), repo)
                .await
                .unwrap();

        assert_eq!(client.update().await, Ok(true));

        // Verify fetch_target_description returns expected target metadata
        let description = client
            .fetch_target_description(&TargetPath::new(path).unwrap())
            .await
            .unwrap();

        assert_eq!(description, expected_description);
    }
}
