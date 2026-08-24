//! Descriptor-confined, append-only transaction evidence for remote process-family upgrades.
//!
//! A committed marker is the sole authority for its matching record. Record
//! files are written and synchronized first; the zero-length marker is then
//! created with no-replace semantics and the transaction directory is
//! synchronized again. Uncommitted record artifacts are retained but never
//! interpreted as authority.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::path::Path;

use anyhow::Context as _;
use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{
    REMOTE_GENERATION_MANIFEST_MODE, REMOTE_GENERATION_MUTABLE_DIRECTORY_MODE,
    create_or_open_remote_generation_directory, sync_capability_directory,
    validate_remote_generation_directory, validate_remote_generation_file_metadata,
};

pub(crate) const MAX_RECORD_BYTES: u64 = 64 * 1024;
pub(crate) const MAX_COMMITTED_RECORDS: u32 = 32;
pub(crate) const MAX_ATTEMPTS: u8 = 16;
pub(crate) const MAX_TRANSACTION_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const MAX_TRANSACTION_DIRECTORIES: usize = 4_096;

const MAX_RECORD_ARTIFACTS: usize = 32 * 16;
const MAX_TRANSACTION_ENTRIES: usize = 1 + MAX_RECORD_ARTIFACTS + 32;
const MAX_RECEIPT_BYTES: usize = 4 * 1024;
const MAX_COMPONENT_BYTES: u64 = 128 * 1024 * 1024;
const CLAIM_SCHEMA: &str = "frankenterm.remote-upgrade-claim.v1";
const RECORD_SCHEMA: &str = "frankenterm.remote-upgrade-record.v1";
const CLAIM_HASH_DOMAIN: &[u8] = b"frankenterm.remote-upgrade.claim.v1";
const RECORD_HASH_DOMAIN: &[u8] = b"frankenterm.remote-upgrade.record.v1";
const EFFECT_ID_HASH_DOMAIN: &[u8] = b"frankenterm.remote-upgrade.effect-id.v1";
const TRANSACTIONS_DIRECTORY: &str = "upgrade-transactions";

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteUpgradeClaim {
    schema: String,
    transaction_id: String,
    operation: String,
    generation_id: String,
    ft_sha256: String,
    ft_bytes: u64,
    mux_server_sha256: String,
    mux_server_bytes: u64,
}

impl RemoteUpgradeClaim {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn process_family_publication(
        transaction_id: &str,
        generation_id: &str,
        ft_sha256: &str,
        ft_bytes: u64,
        mux_server_sha256: &str,
        mux_server_bytes: u64,
    ) -> anyhow::Result<Self> {
        let claim = Self {
            schema: CLAIM_SCHEMA.to_string(),
            transaction_id: transaction_id.to_string(),
            operation: "publish_process_family_generation".to_string(),
            generation_id: generation_id.to_string(),
            ft_sha256: ft_sha256.to_string(),
            ft_bytes,
            mux_server_sha256: mux_server_sha256.to_string(),
            mux_server_bytes,
        };
        claim.validate()?;
        Ok(claim)
    }

    pub(crate) fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub(crate) fn generation_id(&self) -> &str {
        &self.generation_id
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.schema == CLAIM_SCHEMA, "upgrade claim schema is unsupported");
        validate_transaction_id(&self.transaction_id)?;
        anyhow::ensure!(
            self.operation == "publish_process_family_generation",
            "upgrade claim operation is unsupported"
        );
        anyhow::ensure!(
            is_lowercase_sha256(&self.generation_id)
                && is_lowercase_sha256(&self.ft_sha256)
                && is_lowercase_sha256(&self.mux_server_sha256),
            "upgrade claim contains a non-canonical digest"
        );
        anyhow::ensure!(
            (1..=MAX_COMPONENT_BYTES).contains(&self.ft_bytes)
                && (1..=MAX_COMPONENT_BYTES).contains(&self.mux_server_bytes),
            "upgrade claim contains a process-family component outside its byte bound"
        );
        Ok(())
    }

    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = serde_json::to_vec(self)
            .context("cannot serialize canonical remote upgrade claim")?;
        anyhow::ensure!(
            bytes.len() < usize::try_from(MAX_RECORD_BYTES).unwrap_or(usize::MAX),
            "canonical remote upgrade claim exceeds its bound"
        );
        bytes.push(b'\n');
        Ok(bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum SelectorAuthority {
    Missing,
    Unresolved {
        reason: String,
    },
    Selected {
        generation_id: String,
        target: String,
        device: u64,
        inode: u64,
    },
}

impl SelectorAuthority {
    pub(crate) fn selected(
        generation_id: &str,
        target: &Path,
        device: u64,
        inode: u64,
    ) -> anyhow::Result<Self> {
        let target = target
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("selector authority target is not UTF-8"))?;
        anyhow::ensure!(
            is_lowercase_sha256(generation_id)
                && target == format!("generations/{generation_id}"),
            "selector authority is not one canonical process-family generation"
        );
        Ok(Self::Selected {
            generation_id: generation_id.to_string(),
            target: target.to_string(),
            device,
            inode,
        })
    }

    pub(crate) fn generation_id(&self) -> Option<&str> {
        match self {
            Self::Missing | Self::Unresolved { .. } => None,
            Self::Selected { generation_id, .. } => Some(generation_id),
        }
    }

    pub(crate) fn unresolved_post_effect() -> Self {
        Self::Unresolved {
            reason: "post_effect_selector_authority_unreadable".to_string(),
        }
    }

    fn is_resolved(&self) -> bool {
        !matches!(self, Self::Unresolved { .. })
    }

    fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Missing => {}
            Self::Unresolved { reason } => {
                anyhow::ensure!(
                    reason == "post_effect_selector_authority_unreadable",
                    "upgrade record contains an unknown unresolved selector reason"
                );
            }
            Self::Selected {
                generation_id,
                target,
                ..
            } => {
                anyhow::ensure!(
                    is_lowercase_sha256(generation_id)
                        && target == &format!("generations/{generation_id}"),
                    "upgrade record contains a non-canonical selector authority"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) enum RemoteUpgradeState {
    Prepared,
    PendingLiveOwner,
    Activating,
    Committed,
    RolledBack,
    Indeterminate,
}

impl RemoteUpgradeState {
    fn permits_successor(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Prepared, Self::Prepared | Self::PendingLiveOwner | Self::Activating | Self::Indeterminate)
                | (Self::PendingLiveOwner, Self::Activating | Self::Indeterminate)
                | (Self::Activating, Self::Activating | Self::Committed | Self::RolledBack | Self::Indeterminate)
        )
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::RolledBack | Self::Indeterminate)
    }
}

fn record_transition_is_valid(
    previous_state: RemoteUpgradeState,
    previous_attempt: u8,
    next_state: RemoteUpgradeState,
    next_attempt: u8,
) -> bool {
    if !previous_state.permits_successor(next_state) {
        return false;
    }
    if matches!(
        (previous_state, next_state),
        (RemoteUpgradeState::Prepared, RemoteUpgradeState::Prepared)
            | (RemoteUpgradeState::PendingLiveOwner, RemoteUpgradeState::Activating)
            | (RemoteUpgradeState::Activating, RemoteUpgradeState::Activating)
    ) {
        next_attempt > previous_attempt
    } else {
        next_attempt == previous_attempt
    }
}

fn record_authority_transition_is_valid(
    previous: &RemoteUpgradeRecord,
    next: &RemoteUpgradeRecord,
) -> bool {
    if matches!(
        (previous.state, next.state),
        (RemoteUpgradeState::Prepared, RemoteUpgradeState::Prepared)
    ) {
        return true;
    }
    let expected_before = match (previous.state, next.state) {
        (RemoteUpgradeState::PendingLiveOwner, RemoteUpgradeState::Activating)
        | (RemoteUpgradeState::PendingLiveOwner, RemoteUpgradeState::Indeterminate) => {
            &previous.selector_after
        }
        _ => &previous.selector_before,
    };
    &next.selector_before == expected_before
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteUpgradeRecord {
    schema: String,
    transaction_id: String,
    claim_sha256: String,
    claim: RemoteUpgradeClaim,
    sequence: u32,
    attempt: u8,
    state: RemoteUpgradeState,
    generation_id: String,
    selector_before: SelectorAuthority,
    selector_after: SelectorAuthority,
    receipt: String,
}

impl RemoteUpgradeRecord {
    pub(crate) fn state(&self) -> RemoteUpgradeState {
        self.state
    }

    pub(crate) fn selector_before(&self) -> &SelectorAuthority {
        &self.selector_before
    }

    pub(crate) fn selector_after(&self) -> &SelectorAuthority {
        &self.selector_after
    }

    pub(crate) fn receipt(&self) -> &str {
        &self.receipt
    }

    fn validate(&self, claim: &RemoteUpgradeClaim, claim_sha256: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.schema == RECORD_SCHEMA, "upgrade record schema is unsupported");
        anyhow::ensure!(
            self.transaction_id == claim.transaction_id
                && self.claim_sha256 == claim_sha256
                && &self.claim == claim
                && self.generation_id == claim.generation_id,
            "upgrade record is not bound to the immutable transaction claim"
        );
        anyhow::ensure!(
            (1..=MAX_COMMITTED_RECORDS).contains(&self.sequence),
            "upgrade record sequence is outside the supported bound"
        );
        anyhow::ensure!(
            (1..=MAX_ATTEMPTS).contains(&self.attempt),
            "upgrade record attempt is outside the supported bound"
        );
        self.selector_before.validate()?;
        self.selector_after.validate()?;
        anyhow::ensure!(
            self.selector_before.is_resolved(),
            "upgrade record has unresolved pre-effect selector authority"
        );
        if matches!(
            self.state,
            RemoteUpgradeState::Prepared
                | RemoteUpgradeState::Activating
                | RemoteUpgradeState::RolledBack
        ) {
            anyhow::ensure!(
                self.selector_before == self.selector_after,
                "effect-authorization or rolled-back record does not preserve one exact pre-effect authority"
            );
        }
        if self.state == RemoteUpgradeState::Committed {
            anyhow::ensure!(
                self.selector_after.generation_id() == Some(claim.generation_id()),
                "committed upgrade record does not select the claimed generation"
            );
        }
        validate_receipt(&self.receipt)?;
        let expected_receipt = match self.state {
            RemoteUpgradeState::Prepared => format!(
                "FT_REMOTE_UPGRADE_TRANSACTION_V1={}:prepared:{}\n",
                self.transaction_id, self.generation_id
            ),
            RemoteUpgradeState::PendingLiveOwner => format!(
                "FT_REMOTE_GENERATION_PUBLICATION_V1={}:pending_activation_lease\n",
                self.generation_id
            ),
            RemoteUpgradeState::Activating => format!(
                "FT_REMOTE_UPGRADE_TRANSACTION_V1={}:activating:{}\n",
                self.transaction_id, self.generation_id
            ),
            RemoteUpgradeState::Committed => format!(
                "FT_REMOTE_GENERATION_PUBLICATION_V1={}:current:generations/{}\n",
                self.generation_id, self.generation_id
            ),
            RemoteUpgradeState::RolledBack => format!(
                "FT_REMOTE_UPGRADE_TRANSACTION_V1={}:rolled_back:{}\n",
                self.transaction_id, self.generation_id
            ),
            RemoteUpgradeState::Indeterminate => format!(
                "FT_REMOTE_UPGRADE_TRANSACTION_V1={}:indeterminate:{}\n",
                self.transaction_id, self.generation_id
            ),
        };
        anyhow::ensure!(
            self.receipt == expected_receipt,
            "upgrade record receipt does not match its state and immutable claim"
        );
        Ok(())
    }

    fn canonical_bytes(
        &self,
        claim: &RemoteUpgradeClaim,
        claim_sha256: &str,
    ) -> anyhow::Result<Vec<u8>> {
        self.validate(claim, claim_sha256)?;
        let mut bytes = serde_json::to_vec(self)
            .context("cannot serialize canonical remote upgrade record")?;
        anyhow::ensure!(
            bytes.len() < usize::try_from(MAX_RECORD_BYTES).unwrap_or(usize::MAX),
            "canonical remote upgrade record exceeds its bound"
        );
        bytes.push(b'\n');
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DurableEffectKind {
    PublishImmutableGeneration,
    SwitchCurrentSelector,
}

/// One committed authorization for one external effect.
///
/// Deliberately not `Clone`: entering the external effect consumes the permit,
/// preventing a caller from using one committed authorization more than once.
pub(crate) struct DurableEffectPermit {
    transaction_id: String,
    claim_sha256: String,
    authorization_sequence: u32,
    attempt: u8,
    authorization: RemoteUpgradeRecord,
    transaction_device: u64,
    transaction_inode: u64,
    artifact_transaction_id: String,
    kind: DurableEffectKind,
    allow_existing_artifact: bool,
}

impl DurableEffectPermit {
    pub(crate) fn consume_publication(
        self,
        ledger: &RemoteUpgradeLedger<'_>,
    ) -> anyhow::Result<String> {
        self.validate_latest_authorization(
            ledger,
            DurableEffectKind::PublishImmutableGeneration,
        )?;
        Ok(self.artifact_transaction_id)
    }

    pub(crate) fn consume_selector(
        self,
        ledger: &RemoteUpgradeLedger<'_>,
    ) -> anyhow::Result<(String, bool)> {
        self.validate_latest_authorization(ledger, DurableEffectKind::SwitchCurrentSelector)?;
        Ok((self.transaction_id, self.allow_existing_artifact))
    }

    fn validate_latest_authorization(
        &self,
        ledger: &RemoteUpgradeLedger<'_>,
        expected_kind: DurableEffectKind,
    ) -> anyhow::Result<()> {
        self.validate(expected_kind)?;
        anyhow::ensure!(
            self.transaction_id == ledger.claim.transaction_id
                && self.claim_sha256 == ledger.claim_sha256
                && self.authorization.claim == ledger.claim,
            "durable effect permit belongs to a different remote upgrade ledger claim"
        );
        anyhow::ensure!(
            self.transaction_device == ledger.transaction_device
                && self.transaction_inode == ledger.transaction_inode,
            "durable effect permit belongs to a different pinned upgrade transaction"
        );
        let scan = ledger.scan_revalidated_authority()?;
        anyhow::ensure!(
            scan.latest.as_ref() == Some(&self.authorization),
            "durable effect permit authorization is no longer the latest committed authority"
        );
        let expected_artifact_transaction_id = durable_effect_artifact_transaction_id(
            &self.transaction_id,
            &self.claim_sha256,
            self.authorization_sequence,
            self.attempt,
            expected_kind,
        );
        anyhow::ensure!(
            self.artifact_transaction_id == expected_artifact_transaction_id,
            "durable effect permit artifact identity is not bound to its authorization"
        );
        Ok(())
    }

    fn validate(&self, expected_kind: DurableEffectKind) -> anyhow::Result<()> {
        validate_transaction_id(&self.transaction_id)?;
        let expected_state = match expected_kind {
            DurableEffectKind::PublishImmutableGeneration => RemoteUpgradeState::Prepared,
            DurableEffectKind::SwitchCurrentSelector => RemoteUpgradeState::Activating,
        };
        anyhow::ensure!(
            self.kind == expected_kind
                && is_lowercase_sha256(&self.claim_sha256)
                && (1..=MAX_COMMITTED_RECORDS).contains(&self.authorization_sequence)
                && (1..=MAX_ATTEMPTS).contains(&self.attempt)
                && self.authorization.transaction_id == self.transaction_id
                && self.authorization.claim_sha256 == self.claim_sha256
                && self.authorization.sequence == self.authorization_sequence
                && self.authorization.attempt == self.attempt
                && self.authorization.state == expected_state
                && self.artifact_transaction_id.len() == 32
                && self
                    .artifact_transaction_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
            "durable effect permit is not a valid one-shot authorization"
        );
        Ok(())
    }
}

#[derive(Debug)]
struct RecordArtifact {
    name: String,
    sequence: u32,
    digest: String,
    attempt: u8,
}

pub(crate) struct RemoteUpgradeLedger<'root> {
    root: &'root cap_std::fs::Dir,
    transactions: cap_std::fs::Dir,
    transaction: cap_std::fs::Dir,
    effective_uid: u32,
    device: u64,
    transaction_device: u64,
    transaction_inode: u64,
    claim: RemoteUpgradeClaim,
    claim_sha256: String,
    latest: Option<RemoteUpgradeRecord>,
    next_attempt: u8,
}

impl<'root> RemoteUpgradeLedger<'root> {
    pub(crate) fn open(
        root: &'root cap_std::fs::Dir,
        effective_uid: u32,
        claim: RemoteUpgradeClaim,
    ) -> anyhow::Result<Self> {
        claim.validate()?;
        let claim_bytes = claim.canonical_bytes()?;
        let claim_sha256 = domain_hash(CLAIM_HASH_DOMAIN, &claim_bytes);
        let root_device = root.dir_metadata()?.dev();
        let transactions = create_or_open_remote_generation_directory(
            root,
            Path::new(TRANSACTIONS_DIRECTORY),
            effective_uid,
            Some(root_device),
            "remote upgrade transaction ledger",
        )?;
        validate_transaction_census(&transactions, effective_uid, root_device, claim.transaction_id())?;
        let transaction = create_or_open_remote_generation_directory(
            &transactions,
            Path::new(claim.transaction_id()),
            effective_uid,
            Some(root_device),
            "remote upgrade transaction",
        )?;
        validate_remote_generation_directory(
            &transactions,
            Path::new(claim.transaction_id()),
            &transaction,
            REMOTE_GENERATION_MUTABLE_DIRECTORY_MODE,
            effective_uid,
            Some(root_device),
            "remote upgrade transaction",
        )?;
        create_or_validate_claim(
            &transaction,
            effective_uid,
            root_device,
            &claim_sha256,
        )?;
        let scan = scan_transaction(
            &transaction,
            effective_uid,
            root_device,
            &claim,
            &claim_sha256,
        )?;
        let transaction_metadata = transaction.dir_metadata()?;
        let ledger = Self {
            root,
            transactions,
            transaction,
            effective_uid,
            device: root_device,
            transaction_device: transaction_metadata.dev(),
            transaction_inode: transaction_metadata.ino(),
            claim,
            claim_sha256,
            latest: scan.latest,
            next_attempt: scan.next_attempt,
        };
        ledger.revalidate_authority()?;
        Ok(ledger)
    }

    pub(crate) fn latest(&self) -> Option<&RemoteUpgradeRecord> {
        self.latest.as_ref()
    }

    pub(crate) fn revalidate_authority(&self) -> anyhow::Result<()> {
        self.scan_revalidated_authority()?;
        Ok(())
    }

    fn scan_revalidated_authority(&self) -> anyhow::Result<TransactionScan> {
        validate_remote_generation_directory(
            self.root,
            Path::new(TRANSACTIONS_DIRECTORY),
            &self.transactions,
            REMOTE_GENERATION_MUTABLE_DIRECTORY_MODE,
            self.effective_uid,
            Some(self.device),
            "remote upgrade transaction ledger",
        )?;
        validate_remote_generation_directory(
            &self.transactions,
            Path::new(self.claim.transaction_id()),
            &self.transaction,
            REMOTE_GENERATION_MUTABLE_DIRECTORY_MODE,
            self.effective_uid,
            Some(self.device),
            "remote upgrade transaction",
        )?;
        sync_capability_directory(&self.transaction)
            .context("cannot re-durabilize the remote upgrade transaction directory")?;
        sync_capability_directory(&self.transactions)
            .context("cannot re-durabilize the remote upgrade ledger directory")?;
        sync_capability_directory(self.root)
            .context("cannot re-durabilize the process-family root containing the ledger")?;
        let scan = scan_transaction(
            &self.transaction,
            self.effective_uid,
            self.device,
            &self.claim,
            &self.claim_sha256,
        )?;
        anyhow::ensure!(
            scan.latest == self.latest,
            "remote upgrade committed authority changed after it was pinned"
        );
        sync_capability_directory(&self.transaction)
            .context("cannot re-durabilize scanned remote upgrade artifacts")?;
        sync_capability_directory(&self.transactions)
            .context("cannot re-durabilize the transaction directory entry")?;
        sync_capability_directory(self.root)
            .context("cannot re-durabilize the ledger directory entry")?;
        validate_remote_generation_directory(
            self.root,
            Path::new(TRANSACTIONS_DIRECTORY),
            &self.transactions,
            REMOTE_GENERATION_MUTABLE_DIRECTORY_MODE,
            self.effective_uid,
            Some(self.device),
            "remote upgrade transaction ledger",
        )?;
        validate_remote_generation_directory(
            &self.transactions,
            Path::new(self.claim.transaction_id()),
            &self.transaction,
            REMOTE_GENERATION_MUTABLE_DIRECTORY_MODE,
            self.effective_uid,
            Some(self.device),
            "remote upgrade transaction",
        )?;
        Ok(scan)
    }

    pub(crate) fn authorize_publication(
        &mut self,
        selector_before: SelectorAuthority,
    ) -> anyhow::Result<DurableEffectPermit> {
        anyhow::ensure!(
            self.latest
                .as_ref()
                .is_none_or(|record| record.state == RemoteUpgradeState::Prepared),
            "immutable generation publication cannot start from the committed upgrade state"
        );
        let attempt = self.take_next_attempt()?;
        let receipt = format!(
            "FT_REMOTE_UPGRADE_TRANSACTION_V1={}:prepared:{}\n",
            self.claim.transaction_id, self.claim.generation_id
        );
        let record = self.append_committed_record(
            attempt,
            RemoteUpgradeState::Prepared,
            selector_before.clone(),
            selector_before,
            receipt,
        )?;
        Ok(self.permit_for(
            &record,
            DurableEffectKind::PublishImmutableGeneration,
            false,
        ))
    }

    pub(crate) fn commit_pending_live_owner(
        &mut self,
        selector_after: SelectorAuthority,
        receipt: String,
    ) -> anyhow::Result<RemoteUpgradeRecord> {
        let authorization = self
            .latest
            .as_ref()
            .context("upgrade publication has no committed authorization record")?;
        anyhow::ensure!(
            authorization.state == RemoteUpgradeState::Prepared,
            "upgrade publication completion is not preceded by Prepared authority"
        );
        let before = authorization.selector_before.clone();
        let attempt = authorization.attempt;
        self.append_committed_record(
            attempt,
            RemoteUpgradeState::PendingLiveOwner,
            before,
            selector_after,
            receipt,
        )
    }

    pub(crate) fn authorize_selector_after_publication(
        &mut self,
        selector_before: SelectorAuthority,
    ) -> anyhow::Result<DurableEffectPermit> {
        let authorization = self
            .latest
            .as_ref()
            .context("selector activation has no committed publication authorization")?;
        anyhow::ensure!(
            authorization.state == RemoteUpgradeState::Prepared,
            "selector activation is not preceded by Prepared publication authority"
        );
        let attempt = authorization.attempt;
        let receipt = format!(
            "FT_REMOTE_UPGRADE_TRANSACTION_V1={}:activating:{}\n",
            self.claim.transaction_id, self.claim.generation_id
        );
        let record = self.append_committed_record(
            attempt,
            RemoteUpgradeState::Activating,
            selector_before.clone(),
            selector_before,
            receipt,
        )?;
        Ok(self.permit_for(
            &record,
            DurableEffectKind::SwitchCurrentSelector,
            false,
        ))
    }

    pub(crate) fn authorize_selector_from_pending(
        &mut self,
        selector_before: SelectorAuthority,
    ) -> anyhow::Result<DurableEffectPermit> {
        let latest = self
            .latest
            .as_ref()
            .context("selector activation has no committed pending generation")?;
        anyhow::ensure!(
            latest.state == RemoteUpgradeState::PendingLiveOwner
                && &selector_before == latest.selector_after(),
            "selector authority differs from the committed pending-generation observation"
        );
        let attempt = self.take_next_attempt()?;
        let receipt = format!(
            "FT_REMOTE_UPGRADE_TRANSACTION_V1={}:activating:{}\n",
            self.claim.transaction_id, self.claim.generation_id
        );
        let record = self.append_committed_record(
            attempt,
            RemoteUpgradeState::Activating,
            selector_before.clone(),
            selector_before,
            receipt,
        )?;
        Ok(self.permit_for(
            &record,
            DurableEffectKind::SwitchCurrentSelector,
            false,
        ))
    }

    pub(crate) fn reauthorize_selector(
        &mut self,
        selector_before: SelectorAuthority,
    ) -> anyhow::Result<DurableEffectPermit> {
        let latest = self
            .latest
            .as_ref()
            .context("selector reconciliation has no committed activation record")?;
        anyhow::ensure!(
            latest.state == RemoteUpgradeState::Activating
                && &selector_before == latest.selector_before(),
            "selector authority differs from the committed pre-effect authority"
        );
        let attempt = self.take_next_attempt()?;
        let receipt = format!(
            "FT_REMOTE_UPGRADE_TRANSACTION_V1={}:activating:{}\n",
            self.claim.transaction_id, self.claim.generation_id
        );
        let record = self.append_committed_record(
            attempt,
            RemoteUpgradeState::Activating,
            selector_before.clone(),
            selector_before,
            receipt,
        )?;
        Ok(self.permit_for(
            &record,
            DurableEffectKind::SwitchCurrentSelector,
            true,
        ))
    }

    pub(crate) fn commit_current(
        &mut self,
        selector_after: SelectorAuthority,
        receipt: String,
    ) -> anyhow::Result<RemoteUpgradeRecord> {
        anyhow::ensure!(
            selector_after.generation_id() == Some(self.claim.generation_id()),
            "post-effect selector does not name the claimed generation"
        );
        let authorization = self
            .latest
            .as_ref()
            .context("selector effect has no committed authorization record")?;
        anyhow::ensure!(
            authorization.state == RemoteUpgradeState::Activating,
            "selector completion is not preceded by Activating authority"
        );
        let before = authorization.selector_before.clone();
        let attempt = authorization.attempt;
        self.append_committed_record(
            attempt,
            RemoteUpgradeState::Committed,
            before,
            selector_after,
            receipt,
        )
    }

    /// Commit terminal evidence that the authorized selector effect restored
    /// the exact selector authority observed before that effect began.
    ///
    /// The preceding `Activating` record is the durable effect authorization;
    /// rollback completion therefore retains its exact attempt identity rather
    /// than minting another authorization. A different or unresolved observed
    /// authority is not a rollback and must be recorded as `Indeterminate` by
    /// the caller instead.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn record_rolled_back(
        &mut self,
        selector_after: SelectorAuthority,
    ) -> anyhow::Result<RemoteUpgradeRecord> {
        let authorization = self
            .latest
            .as_ref()
            .context("selector rollback has no committed activation authorization")?;
        anyhow::ensure!(
            authorization.state == RemoteUpgradeState::Activating,
            "selector rollback is not preceded by Activating authority"
        );
        anyhow::ensure!(
            selector_after == authorization.selector_before,
            "selector rollback did not restore the exact committed pre-effect authority"
        );
        let before = authorization.selector_before.clone();
        let attempt = authorization.attempt;
        let receipt = format!(
            "FT_REMOTE_UPGRADE_TRANSACTION_V1={}:rolled_back:{}\n",
            self.claim.transaction_id, self.claim.generation_id
        );
        self.append_committed_record(
            attempt,
            RemoteUpgradeState::RolledBack,
            before,
            selector_after,
            receipt,
        )
    }

    pub(crate) fn record_indeterminate(
        &mut self,
        selector_before: SelectorAuthority,
        selector_after: SelectorAuthority,
        receipt: String,
    ) -> anyhow::Result<RemoteUpgradeRecord> {
        let attempt = if let Some(record) = self.latest.as_ref() {
            record.attempt
        } else {
            self.take_next_attempt()?
        };
        self.append_committed_record(
            attempt,
            RemoteUpgradeState::Indeterminate,
            selector_before,
            selector_after,
            receipt,
        )
    }

    fn take_next_attempt(&mut self) -> anyhow::Result<u8> {
        anyhow::ensure!(
            (1..=MAX_ATTEMPTS).contains(&self.next_attempt),
            "remote upgrade transaction exhausted its bounded attempt budget"
        );
        let attempt = self.next_attempt;
        self.next_attempt = self.next_attempt.saturating_add(1);
        Ok(attempt)
    }

    fn permit_for(
        &self,
        record: &RemoteUpgradeRecord,
        kind: DurableEffectKind,
        allow_existing_artifact: bool,
    ) -> DurableEffectPermit {
        let artifact_transaction_id = durable_effect_artifact_transaction_id(
            &self.claim.transaction_id,
            &self.claim_sha256,
            record.sequence,
            record.attempt,
            kind,
        );
        DurableEffectPermit {
            transaction_id: self.claim.transaction_id.clone(),
            claim_sha256: self.claim_sha256.clone(),
            authorization_sequence: record.sequence,
            attempt: record.attempt,
            authorization: record.clone(),
            transaction_device: self.transaction_device,
            transaction_inode: self.transaction_inode,
            artifact_transaction_id,
            kind,
            allow_existing_artifact,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append_committed_record(
        &mut self,
        attempt: u8,
        state: RemoteUpgradeState,
        selector_before: SelectorAuthority,
        selector_after: SelectorAuthority,
        receipt: String,
    ) -> anyhow::Result<RemoteUpgradeRecord> {
        let preflight = self.scan_revalidated_authority()?;
        if let Some(latest) = self.latest.as_ref() {
            anyhow::ensure!(
                !latest.state.is_terminal()
                    && record_transition_is_valid(
                        latest.state,
                        latest.attempt,
                        state,
                        attempt,
                    ),
                "illegal remote upgrade transaction state transition"
            );
        } else {
            anyhow::ensure!(
                state == RemoteUpgradeState::Prepared,
                "remote upgrade transaction must begin in Prepared"
            );
        }
        let sequence = self
            .latest
            .as_ref()
            .map_or(1, |record| record.sequence.saturating_add(1));
        anyhow::ensure!(
            sequence <= MAX_COMMITTED_RECORDS,
            "remote upgrade transaction exhausted its committed-record bound"
        );
        let record = RemoteUpgradeRecord {
            schema: RECORD_SCHEMA.to_string(),
            transaction_id: self.claim.transaction_id.clone(),
            claim_sha256: self.claim_sha256.clone(),
            claim: self.claim.clone(),
            sequence,
            attempt,
            state,
            generation_id: self.claim.generation_id.clone(),
            selector_before,
            selector_after,
            receipt,
        };
        if let Some(previous) = self.latest.as_ref() {
            anyhow::ensure!(
                record_authority_transition_is_valid(previous, &record),
                "illegal remote upgrade selector-authority transition"
            );
        }
        let bytes = record.canonical_bytes(&self.claim, &self.claim_sha256)?;
        let digest = domain_hash(RECORD_HASH_DOMAIN, &bytes);
        let record_name = format!("record-{sequence:04}-{digest}-{attempt:02}.json");
        let matching_identity = preflight
            .records
            .iter()
            .find(|artifact| artifact.sequence == sequence && artifact.attempt == attempt);
        let adopt_orphan = if let Some(artifact) = matching_identity {
            anyhow::ensure!(
                artifact.name == record_name && artifact.digest == digest,
                "remote upgrade transaction contains a conflicting orphan record for the intended sequence/attempt"
            );
            let orphan = read_record(
                &self.transaction,
                artifact,
                self.effective_uid,
                self.device,
                &self.claim,
                &self.claim_sha256,
            )?;
            anyhow::ensure!(
                orphan == record,
                "remote upgrade orphan record is not the exact canonical intended outcome"
            );
            true
        } else {
            false
        };
        preflight_append_capacity(&preflight, &bytes, adopt_orphan)?;
        if !adopt_orphan {
            create_synchronized_file(
                &self.transaction,
                Path::new(&record_name),
                &bytes,
                self.effective_uid,
                self.device,
            )?;
        }
        sync_capability_directory(&self.transaction)?;
        let commit_name = format!("commit-{sequence:04}-{digest}.v1");
        create_synchronized_file(
            &self.transaction,
            Path::new(&commit_name),
            &[],
            self.effective_uid,
            self.device,
        )?;
        sync_capability_directory(&self.transaction)?;
        let scan = scan_transaction(
            &self.transaction,
            self.effective_uid,
            self.device,
            &self.claim,
            &self.claim_sha256,
        )?;
        anyhow::ensure!(
            scan.latest.as_ref() == Some(&record),
            "committed remote upgrade record did not survive exact readback"
        );
        self.latest = Some(record.clone());
        self.next_attempt = self.next_attempt.max(scan.next_attempt);
        self.revalidate_authority()?;
        Ok(record)
    }
}

fn durable_effect_artifact_transaction_id(
    transaction_id: &str,
    claim_sha256: &str,
    authorization_sequence: u32,
    attempt: u8,
    kind: DurableEffectKind,
) -> String {
    let kind_label = match kind {
        DurableEffectKind::PublishImmutableGeneration => "publish",
        DurableEffectKind::SwitchCurrentSelector => "selector",
    };
    let material = format!(
        "{transaction_id}\0{claim_sha256}\0{authorization_sequence}\0{attempt}\0{kind_label}"
    );
    let digest = domain_hash(EFFECT_ID_HASH_DOMAIN, material.as_bytes());
    digest[..32].to_string()
}

fn preflight_append_capacity(
    scan: &TransactionScan,
    record_bytes: &[u8],
    adopting_orphan: bool,
) -> anyhow::Result<()> {
    let additional_entries = if adopting_orphan { 1 } else { 2 };
    let resulting_entries = scan
        .entry_count
        .checked_add(additional_entries)
        .ok_or_else(|| anyhow::anyhow!("remote upgrade artifact count overflow"))?;
    anyhow::ensure!(
        resulting_entries <= MAX_TRANSACTION_ENTRIES,
        "remote upgrade append would exceed its bounded artifact count"
    );
    let resulting_commits = scan
        .committed_records
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("remote upgrade committed-record count overflow"))?;
    anyhow::ensure!(
        resulting_commits <= usize::try_from(MAX_COMMITTED_RECORDS).unwrap_or(usize::MAX),
        "remote upgrade append would exceed its committed-record bound"
    );
    if !adopting_orphan {
        let resulting_records = scan
            .records
            .len()
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("remote upgrade record-artifact count overflow"))?;
        anyhow::ensure!(
            resulting_records <= MAX_RECORD_ARTIFACTS,
            "remote upgrade append would exceed its record-artifact bound"
        );
        let additional_bytes = u64::try_from(record_bytes.len())
            .map_err(|_| anyhow::anyhow!("remote upgrade record length does not fit u64"))?;
        let resulting_bytes = scan
            .total_bytes
            .checked_add(additional_bytes)
            .ok_or_else(|| anyhow::anyhow!("remote upgrade transaction byte count overflow"))?;
        anyhow::ensure!(
            resulting_bytes <= MAX_TRANSACTION_BYTES,
            "remote upgrade append would exceed its transaction byte bound"
        );
    }
    Ok(())
}

struct TransactionScan {
    latest: Option<RemoteUpgradeRecord>,
    next_attempt: u8,
    records: Vec<RecordArtifact>,
    committed_records: usize,
    entry_count: usize,
    total_bytes: u64,
}

fn validate_transaction_census(
    transactions: &cap_std::fs::Dir,
    effective_uid: u32,
    expected_device: u64,
    requested_transaction_id: &str,
) -> anyhow::Result<()> {
    validate_transaction_id(requested_transaction_id)?;
    let mut count = 0usize;
    let mut requested_exists = false;
    for entry in transactions.entries()? {
        let entry = entry?;
        count = count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("upgrade transaction directory count overflow"))?;
        anyhow::ensure!(
            count <= MAX_TRANSACTION_DIRECTORIES,
            "remote upgrade ledger exceeds its {} transaction-directory bound",
            MAX_TRANSACTION_DIRECTORIES
        );
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("upgrade ledger contains a non-UTF-8 entry"))?;
        validate_transaction_id(&name)
            .context("upgrade ledger contains a non-canonical transaction directory")?;
        let directory = transactions
            .open_dir_nofollow(Path::new(&name))
            .with_context(|| format!("cannot pin upgrade transaction {name}"))?;
        validate_remote_generation_directory(
            transactions,
            Path::new(&name),
            &directory,
            REMOTE_GENERATION_MUTABLE_DIRECTORY_MODE,
            effective_uid,
            Some(expected_device),
            "remote upgrade transaction",
        )?;
        requested_exists |= name == requested_transaction_id;
    }
    anyhow::ensure!(
        requested_exists || count < MAX_TRANSACTION_DIRECTORIES,
        "remote upgrade ledger has no capacity for a new transaction"
    );
    Ok(())
}

fn create_or_validate_claim(
    transaction: &cap_std::fs::Dir,
    effective_uid: u32,
    expected_device: u64,
    claim_sha256: &str,
) -> anyhow::Result<()> {
    let expected_name = format!("claim-{claim_sha256}.v1");
    let mut claims = Vec::new();
    let mut non_claim_entries = 0usize;
    let mut entry_count = 0usize;
    for entry in transaction.entries()? {
        let entry = entry?;
        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("upgrade transaction entry count overflow"))?;
        anyhow::ensure!(
            entry_count <= MAX_TRANSACTION_ENTRIES,
            "remote upgrade transaction exceeds its bounded artifact count"
        );
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("upgrade transaction contains a non-UTF-8 entry"))?;
        if name.starts_with("claim-") && name.ends_with(".v1") {
            claims.push(name);
        } else {
            non_claim_entries += 1;
        }
    }
    anyhow::ensure!(
        claims.is_empty() || (claims.len() == 1 && claims[0] == expected_name),
        "remote upgrade transaction ID is already claimed by a different payload"
    );
    if claims.is_empty() {
        anyhow::ensure!(
            non_claim_entries == 0,
            "unclaimed remote upgrade transaction contains artifacts; refusing to add authority"
        );
        create_synchronized_file(
            transaction,
            Path::new(&expected_name),
            &[],
            effective_uid,
            expected_device,
        )?;
        sync_capability_directory(transaction)?;
    } else {
        validate_exact_file(
            transaction,
            Path::new(&expected_name),
            0,
            effective_uid,
            expected_device,
        )?;
    }
    sync_capability_directory(transaction)
        .context("cannot re-durabilize the transaction containing its immutable claim")?;
    validate_exact_file(
        transaction,
        Path::new(&expected_name),
        0,
        effective_uid,
        expected_device,
    )?;
    Ok(())
}

fn scan_transaction(
    transaction: &cap_std::fs::Dir,
    effective_uid: u32,
    expected_device: u64,
    claim: &RemoteUpgradeClaim,
    claim_sha256: &str,
) -> anyhow::Result<TransactionScan> {
    let expected_claim = format!("claim-{claim_sha256}.v1");
    let mut saw_claim = false;
    let mut records = Vec::new();
    let mut commits = BTreeMap::<u32, String>::new();
    let mut attempts = BTreeSet::new();
    let mut record_identities = BTreeSet::new();
    let mut total_bytes = 0u64;
    let mut entry_count = 0usize;

    for entry in transaction.entries()? {
        let entry = entry?;
        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("upgrade transaction entry count overflow"))?;
        anyhow::ensure!(
            entry_count <= MAX_TRANSACTION_ENTRIES,
            "remote upgrade transaction exceeds its bounded artifact count"
        );
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("upgrade transaction contains a non-UTF-8 entry"))?;
        let metadata = transaction.symlink_metadata(Path::new(&name))?;
        anyhow::ensure!(
            metadata.is_file(),
            "upgrade transaction contains a non-regular artifact"
        );
        total_bytes = total_bytes
            .checked_add(metadata.len())
            .ok_or_else(|| anyhow::anyhow!("upgrade transaction byte census overflow"))?;
        anyhow::ensure!(
            total_bytes <= MAX_TRANSACTION_BYTES,
            "remote upgrade transaction exceeds its {} byte bound",
            MAX_TRANSACTION_BYTES
        );

        if name == expected_claim {
            anyhow::ensure!(!saw_claim, "upgrade transaction contains duplicate claims");
            validate_exact_file(
                transaction,
                Path::new(&name),
                0,
                effective_uid,
                expected_device,
            )?;
            saw_claim = true;
        } else if name.starts_with("claim-") {
            anyhow::bail!("remote upgrade transaction contains a conflicting claim");
        } else if name.starts_with("record-") {
            let artifact = parse_record_name(&name)?;
            anyhow::ensure!(
                record_identities.insert((artifact.sequence, artifact.attempt)),
                "remote upgrade transaction contains conflicting records for one bounded sequence/attempt identity"
            );
            anyhow::ensure!(
                records.len() < MAX_RECORD_ARTIFACTS,
                "remote upgrade transaction exceeds its bounded record-artifact count"
            );
            anyhow::ensure!(
                metadata.len() <= MAX_RECORD_BYTES,
                "remote upgrade record exceeds its {} byte bound",
                MAX_RECORD_BYTES
            );
            validate_exact_file(
                transaction,
                Path::new(&name),
                metadata.len(),
                effective_uid,
                expected_device,
            )?;
            attempts.insert(artifact.attempt);
            records.push(artifact);
        } else if name.starts_with("commit-") {
            let (sequence, digest) = parse_commit_name(&name)?;
            validate_exact_file(
                transaction,
                Path::new(&name),
                0,
                effective_uid,
                expected_device,
            )?;
            anyhow::ensure!(
                commits.insert(sequence, digest).is_none(),
                "upgrade transaction contains multiple commits for one sequence"
            );
        } else {
            anyhow::bail!("remote upgrade transaction contains an unknown artifact");
        }
    }
    anyhow::ensure!(saw_claim, "remote upgrade transaction has no immutable claim");
    anyhow::ensure!(
        commits.len() <= usize::try_from(MAX_COMMITTED_RECORDS).unwrap_or(usize::MAX),
        "remote upgrade transaction exceeds its committed-record bound"
    );
    anyhow::ensure!(
        attempts.len() <= usize::from(MAX_ATTEMPTS),
        "remote upgrade transaction exceeds its attempt bound"
    );

    let mut latest: Option<RemoteUpgradeRecord> = None;
    for expected_sequence in 1..=u32::try_from(commits.len()).unwrap_or(u32::MAX) {
        let digest = commits.get(&expected_sequence).ok_or_else(|| {
            anyhow::anyhow!(
                "upgrade transaction commit sequence has a gap; refusing older-record fallback"
            )
        })?;
        let mut matching_records = records
            .iter()
            .filter(|record| record.sequence == expected_sequence && &record.digest == digest);
        let matching_record = matching_records.next().ok_or_else(|| {
            anyhow::anyhow!(
                "committed upgrade marker has no matching record; refusing older-record fallback"
            )
        })?;
        anyhow::ensure!(
            matching_records.next().is_none(),
            "committed upgrade marker has multiple matching records; refusing older-record fallback"
        );
        let record = read_record(
            transaction,
            matching_record,
            effective_uid,
            expected_device,
            claim,
            claim_sha256,
        )?;
        if let Some(previous) = latest.as_ref() {
            anyhow::ensure!(
                record_transition_is_valid(
                    previous.state,
                    previous.attempt,
                    record.state,
                    record.attempt,
                ) && record_authority_transition_is_valid(previous, &record),
                "committed upgrade records contain an illegal state or attempt transition"
            );
        } else {
            anyhow::ensure!(
                record.state == RemoteUpgradeState::Prepared,
                "first committed upgrade record is not Prepared"
            );
        }
        latest = Some(record);
    }
    let maximum_attempt = attempts.into_iter().next_back().unwrap_or(0);
    sync_capability_directory(transaction)
        .context("cannot re-durabilize the scanned remote upgrade artifact namespace")?;
    Ok(TransactionScan {
        latest,
        next_attempt: maximum_attempt.saturating_add(1),
        records,
        committed_records: commits.len(),
        entry_count,
        total_bytes,
    })
}

fn read_record(
    transaction: &cap_std::fs::Dir,
    artifact: &RecordArtifact,
    effective_uid: u32,
    expected_device: u64,
    claim: &RemoteUpgradeClaim,
    claim_sha256: &str,
) -> anyhow::Result<RemoteUpgradeRecord> {
    let bytes = read_exact_file(
        transaction,
        Path::new(&artifact.name),
        MAX_RECORD_BYTES,
        effective_uid,
        expected_device,
    )?;
    anyhow::ensure!(
        domain_hash(RECORD_HASH_DOMAIN, &bytes) == artifact.digest,
        "committed upgrade record digest is corrupt; refusing older-record fallback"
    );
    let record: RemoteUpgradeRecord = serde_json::from_slice(&bytes)
        .context("committed upgrade record is not valid canonical JSON")?;
    anyhow::ensure!(
        record.sequence == artifact.sequence && record.attempt == artifact.attempt,
        "committed upgrade record filename does not match its payload"
    );
    let canonical = record.canonical_bytes(claim, claim_sha256)?;
    anyhow::ensure!(
        canonical == bytes,
        "committed upgrade record does not use the canonical v1 encoding"
    );
    Ok(record)
}

fn create_synchronized_file(
    directory: &cap_std::fs::Dir,
    name: &Path,
    bytes: &[u8],
    effective_uid: u32,
    expected_device: u64,
) -> anyhow::Result<()> {
    let expected_len = u64::try_from(bytes.len())
        .map_err(|_| anyhow::anyhow!("upgrade artifact length does not fit this platform"))?;
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No)
        .mode(REMOTE_GENERATION_MANIFEST_MODE);
    let mut file = directory
        .open_with(name, &options)
        .with_context(|| format!("cannot create upgrade artifact {}", name.display()))?;
    file.write_all(bytes)?;
    file.set_permissions(cap_std::fs::Permissions::from_mode(
        REMOTE_GENERATION_MANIFEST_MODE,
    ))?;
    file.sync_all()?;
    validate_remote_generation_file_metadata(
        directory,
        name,
        &file,
        REMOTE_GENERATION_MANIFEST_MODE,
        Some(expected_len),
        effective_uid,
        expected_device,
    )?;
    Ok(())
}

fn validate_exact_file(
    directory: &cap_std::fs::Dir,
    name: &Path,
    expected_len: u64,
    effective_uid: u32,
    expected_device: u64,
) -> anyhow::Result<()> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options)?;
    validate_remote_generation_file_metadata(
        directory,
        name,
        &file,
        REMOTE_GENERATION_MANIFEST_MODE,
        Some(expected_len),
        effective_uid,
        expected_device,
    )?;
    file.sync_all()
        .with_context(|| format!("cannot re-durabilize upgrade artifact {}", name.display()))?;
    validate_remote_generation_file_metadata(
        directory,
        name,
        &file,
        REMOTE_GENERATION_MANIFEST_MODE,
        Some(expected_len),
        effective_uid,
        expected_device,
    )?;
    Ok(())
}

fn read_exact_file(
    directory: &cap_std::fs::Dir,
    name: &Path,
    maximum_len: u64,
    effective_uid: u32,
    expected_device: u64,
) -> anyhow::Result<Vec<u8>> {
    let metadata = directory.symlink_metadata(name)?;
    anyhow::ensure!(
        metadata.len() <= maximum_len,
        "upgrade artifact exceeds its bounded read limit"
    );
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = directory.open_with(name, &options)?;
    let before = validate_remote_generation_file_metadata(
        directory,
        name,
        &file,
        REMOTE_GENERATION_MANIFEST_MODE,
        Some(metadata.len()),
        effective_uid,
        expected_device,
    )?;
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| anyhow::anyhow!("upgrade artifact length does not fit this platform"))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity)?;
    (&mut file)
        .take(maximum_len.saturating_add(1))
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        u64::try_from(bytes.len()).unwrap_or(u64::MAX) == metadata.len(),
        "upgrade artifact changed length while it was read"
    );
    file.sync_all()
        .with_context(|| format!("cannot re-durabilize upgrade artifact {}", name.display()))?;
    let after = validate_remote_generation_file_metadata(
        directory,
        name,
        &file,
        REMOTE_GENERATION_MANIFEST_MODE,
        Some(metadata.len()),
        effective_uid,
        expected_device,
    )?;
    anyhow::ensure!(before == after, "upgrade artifact changed while it was read");
    Ok(bytes)
}

fn parse_record_name(name: &str) -> anyhow::Result<RecordArtifact> {
    let body = name
        .strip_prefix("record-")
        .and_then(|value| value.strip_suffix(".json"))
        .ok_or_else(|| anyhow::anyhow!("upgrade record filename is malformed"))?;
    let mut fields = body.split('-');
    let sequence = parse_fixed_decimal(fields.next(), 4, 1, MAX_COMMITTED_RECORDS, "sequence")?;
    let digest = fields
        .next()
        .filter(|value| is_lowercase_sha256(value))
        .ok_or_else(|| anyhow::anyhow!("upgrade record filename has an invalid digest"))?;
    let attempt = parse_fixed_decimal(
        fields.next(),
        2,
        1,
        u32::from(MAX_ATTEMPTS),
        "attempt",
    )?;
    anyhow::ensure!(
        fields.next().is_none(),
        "upgrade record filename has unexpected fields"
    );
    Ok(RecordArtifact {
        name: name.to_string(),
        sequence,
        digest: digest.to_string(),
        attempt: u8::try_from(attempt)
            .map_err(|_| anyhow::anyhow!("upgrade record attempt does not fit u8"))?,
    })
}

fn parse_commit_name(name: &str) -> anyhow::Result<(u32, String)> {
    let body = name
        .strip_prefix("commit-")
        .and_then(|value| value.strip_suffix(".v1"))
        .ok_or_else(|| anyhow::anyhow!("upgrade commit filename is malformed"))?;
    let mut fields = body.split('-');
    let sequence = parse_fixed_decimal(fields.next(), 4, 1, MAX_COMMITTED_RECORDS, "sequence")?;
    let digest = fields
        .next()
        .filter(|value| is_lowercase_sha256(value))
        .ok_or_else(|| anyhow::anyhow!("upgrade commit filename has an invalid digest"))?;
    anyhow::ensure!(
        fields.next().is_none(),
        "upgrade commit filename has unexpected fields"
    );
    Ok((sequence, digest.to_string()))
}

fn parse_fixed_decimal(
    value: Option<&str>,
    width: usize,
    minimum: u32,
    maximum: u32,
    label: &str,
) -> anyhow::Result<u32> {
    let value = value.ok_or_else(|| anyhow::anyhow!("upgrade artifact has no {label}"))?;
    anyhow::ensure!(
        value.len() == width && value.bytes().all(|byte| byte.is_ascii_digit()),
        "upgrade artifact {label} is not fixed-width decimal"
    );
    let parsed = value
        .parse::<u32>()
        .with_context(|| format!("upgrade artifact {label} is invalid"))?;
    anyhow::ensure!(
        (minimum..=maximum).contains(&parsed),
        "upgrade artifact {label} is outside its supported bound"
    );
    Ok(parsed)
}

fn validate_transaction_id(transaction_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        transaction_id.len() == 32
            && transaction_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "remote upgrade transaction ID must be 32 lowercase hex characters"
    );
    Ok(())
}

fn validate_receipt(receipt: &str) -> anyhow::Result<()> {
    let line = receipt
        .strip_suffix('\n')
        .ok_or_else(|| anyhow::anyhow!("upgrade record receipt has no final LF"))?;
    anyhow::ensure!(
        !line.is_empty()
            && receipt.len() <= MAX_RECEIPT_BYTES
            && !receipt.contains('\r')
            && receipt.lines().count() == 1
            && line.bytes().all(|byte| byte.is_ascii_graphic()),
        "upgrade record receipt is not one bounded canonical ASCII line"
    );
    Ok(())
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn domain_hash(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update([0]);
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const TRANSACTION_ID: &str = "0123456789abcdef0123456789abcdef";

    fn claim(generation: char) -> RemoteUpgradeClaim {
        claim_for(TRANSACTION_ID, generation)
    }

    fn claim_for(transaction_id: &str, generation: char) -> RemoteUpgradeClaim {
        RemoteUpgradeClaim::process_family_publication(
            transaction_id,
            &generation.to_string().repeat(64),
            &"b".repeat(64),
            101,
            &"c".repeat(64),
            202,
        )
        .expect("create canonical upgrade claim")
    }

    fn root_fixture() -> (
        tempfile::TempDir,
        cap_std::fs::Dir,
        u32,
    ) {
        let fixture = tempfile::tempdir().expect("create upgrade ledger fixture");
        let root_path = fixture.path().join("process-family");
        let effective_uid = super::super::remote_generation_effective_uid();
        let (root, _generations) =
            super::super::open_remote_generation_root(&root_path, effective_uid)
                .expect("open descriptor-confined process-family root");
        (fixture, root, effective_uid)
    }

    fn pending_outcome_record(
        ledger: &RemoteUpgradeLedger<'_>,
    ) -> (RemoteUpgradeRecord, Vec<u8>, String) {
        let authorization = ledger.latest().expect("Prepared authorization");
        let receipt = format!(
            "FT_REMOTE_GENERATION_PUBLICATION_V1={}:pending_activation_lease\n",
            ledger.claim.generation_id
        );
        let record = RemoteUpgradeRecord {
            schema: RECORD_SCHEMA.to_string(),
            transaction_id: ledger.claim.transaction_id.clone(),
            claim_sha256: ledger.claim_sha256.clone(),
            claim: ledger.claim.clone(),
            sequence: authorization.sequence + 1,
            attempt: authorization.attempt,
            state: RemoteUpgradeState::PendingLiveOwner,
            generation_id: ledger.claim.generation_id.clone(),
            selector_before: authorization.selector_before.clone(),
            selector_after: authorization.selector_before.clone(),
            receipt,
        };
        let bytes = record
            .canonical_bytes(&ledger.claim, &ledger.claim_sha256)
            .expect("serialize intended pending outcome");
        let digest = domain_hash(RECORD_HASH_DOMAIN, &bytes);
        (record, bytes, digest)
    }

    fn transaction_names(ledger: &RemoteUpgradeLedger<'_>) -> BTreeSet<String> {
        ledger
            .transaction
            .entries()
            .expect("read transaction artifacts")
            .map(|entry| {
                entry
                    .expect("read transaction artifact")
                    .file_name()
                    .into_string()
                    .expect("transaction artifact name is UTF-8")
            })
            .collect()
    }

    fn authorize_activation_from_pending(ledger: &mut RemoteUpgradeLedger<'_>) {
        ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared publication authorization")
            .consume_publication(ledger)
            .expect("consume publication permit");
        ledger
            .commit_pending_live_owner(
                SelectorAuthority::Missing,
                format!(
                    "FT_REMOTE_GENERATION_PUBLICATION_V1={}:pending_activation_lease\n",
                    ledger.claim.generation_id
                ),
            )
            .expect("commit PendingLiveOwner publication outcome");
        let permit = ledger
            .authorize_selector_from_pending(SelectorAuthority::Missing)
            .expect("commit Activating selector authorization");
        let (transaction_id, allow_existing_artifact) = permit
            .consume_selector(ledger)
            .expect("consume selector effect permit");
        assert_eq!(transaction_id, TRANSACTION_ID);
        assert!(!allow_existing_artifact);
    }

    #[test]
    fn committed_marker_replays_exact_pending_receipt_and_claims_transaction_id() {
        let (fixture, root, effective_uid) = root_fixture();
        let upgrade_claim = claim('a');
        let expected_receipt = format!(
            "FT_REMOTE_GENERATION_PUBLICATION_V1={}:pending_activation_lease\n",
            "a".repeat(64)
        );
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim.clone())
            .expect("create transaction claim");
        let permit = ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared authorization");
        assert_eq!(
            permit
                .consume_publication(&ledger)
                .expect("consume permit")
                .len(),
            32
        );
        ledger
            .commit_pending_live_owner(
                SelectorAuthority::Missing,
                expected_receipt.clone(),
            )
            .expect("commit pending terminal progress");

        let replay = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim)
            .expect("reopen exact transaction");
        let latest = replay.latest().expect("committed pending record");
        assert_eq!(latest.state(), RemoteUpgradeState::PendingLiveOwner);
        assert_eq!(latest.receipt(), expected_receipt);
        let conflict = RemoteUpgradeLedger::open(&root, effective_uid, claim('d'))
            .err()
            .expect("same transaction ID with a different payload must fail");
        assert!(conflict
            .to_string()
            .contains("claimed by a different payload"));

        let transaction_path = fixture
            .path()
            .join("process-family/upgrade-transactions")
            .join(TRANSACTION_ID);
        let names = std::fs::read_dir(transaction_path)
            .expect("read transaction artifacts")
            .map(|entry| {
                entry
                    .expect("read transaction artifact")
                    .file_name()
                    .into_string()
                    .expect("artifact name is UTF-8")
            })
            .collect::<Vec<_>>();
        assert_eq!(names.iter().filter(|name| name.starts_with("claim-")).count(), 1);
        assert_eq!(names.iter().filter(|name| name.starts_with("record-")).count(), 2);
        assert_eq!(names.iter().filter(|name| name.starts_with("commit-")).count(), 2);
    }

    #[test]
    fn ordinary_effect_permits_consume_only_against_exact_latest_authority() {
        let (_fixture, root, effective_uid) = root_fixture();
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
            .expect("create valid permit transaction");
        let publication_permit = ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared publication authorization");
        assert_eq!(
            publication_permit
                .consume_publication(&ledger)
                .expect("consume current publication permit")
                .len(),
            32
        );

        let selector_permit = ledger
            .authorize_selector_after_publication(SelectorAuthority::Missing)
            .expect("commit Activating selector authorization");
        let (transaction_id, allow_existing_artifact) = selector_permit
            .consume_selector(&ledger)
            .expect("consume current selector permit");
        assert_eq!(transaction_id, TRANSACTION_ID);
        assert!(!allow_existing_artifact);

        let retry_permit = ledger
            .reauthorize_selector(SelectorAuthority::Missing)
            .expect("commit a fresh Activating retry authorization");
        let (retry_transaction_id, retry_allows_existing_artifact) = retry_permit
            .consume_selector(&ledger)
            .expect("consume current selector retry permit");
        assert_eq!(retry_transaction_id, TRANSACTION_ID);
        assert!(retry_allows_existing_artifact);
    }

    #[test]
    fn effect_permit_rejects_wrong_kind_ledger_and_claim() {
        const OTHER_TRANSACTION_ID: &str = "fedcba9876543210fedcba9876543210";

        let (_fixture, root, effective_uid) = root_fixture();
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
            .expect("create wrong-kind permit transaction");
        let wrong_kind = ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared publication authorization");
        let wrong_kind_error = wrong_kind
            .consume_selector(&ledger)
            .expect_err("publication permit must not authorize a selector effect");
        assert!(wrong_kind_error
            .to_string()
            .contains("not a valid one-shot authorization"));

        let wrong_ledger = ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit a fresh Prepared publication authorization");
        let other_ledger = RemoteUpgradeLedger::open(
            &root,
            effective_uid,
            claim_for(OTHER_TRANSACTION_ID, 'd'),
        )
        .expect("create different durable ledger claim");
        let wrong_ledger_error = wrong_ledger
            .consume_publication(&other_ledger)
            .expect_err("permit must not cross durable ledger claims");
        assert!(wrong_ledger_error
            .to_string()
            .contains("belongs to a different remote upgrade ledger claim"));

        let mut wrong_claim = ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit another Prepared publication authorization");
        wrong_claim.claim_sha256 = "f".repeat(64);
        let wrong_claim_error = wrong_claim
            .consume_publication(&ledger)
            .expect_err("permit with a substituted claim digest must fail");
        assert!(wrong_claim_error
            .to_string()
            .contains("not a valid one-shot authorization"));

        let mut wrong_artifact = ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared authorization for artifact-binding mutation");
        wrong_artifact.artifact_transaction_id = "0".repeat(32);
        let wrong_artifact_error = wrong_artifact
            .consume_publication(&ledger)
            .expect_err("permit with a substituted effect identity must fail");
        assert!(wrong_artifact_error
            .to_string()
            .contains("artifact identity is not bound to its authorization"));
    }

    #[test]
    fn effect_permit_rejects_a_different_root_with_an_identical_claim_and_record() {
        let (_first_fixture, first_root, first_uid) = root_fixture();
        let (_second_fixture, second_root, second_uid) = root_fixture();
        let mut first = RemoteUpgradeLedger::open(&first_root, first_uid, claim('a'))
            .expect("create first identical durable ledger");
        let permit = first
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit first identical Prepared authorization");
        let mut second = RemoteUpgradeLedger::open(&second_root, second_uid, claim('a'))
            .expect("create second identical durable ledger");
        let _second_permit = second
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit second identical Prepared authorization");
        assert_eq!(
            first.latest(),
            second.latest(),
            "the attack fixture requires byte-identical logical authority"
        );
        assert_ne!(
            (first.transaction_device, first.transaction_inode),
            (second.transaction_device, second.transaction_inode),
            "independently pinned transaction directories need distinct identities"
        );

        let error = permit
            .consume_publication(&second)
            .expect_err("permit must not cross identical claims in different roots");
        assert!(error
            .to_string()
            .contains("different pinned upgrade transaction"));
    }

    #[test]
    fn same_process_terminal_advancement_invalidates_retained_permits() {
        {
            let (_fixture, root, effective_uid) = root_fixture();
            let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
                .expect("create Indeterminate stale-permit transaction");
            let stale_publication = ledger
                .authorize_publication(SelectorAuthority::Missing)
                .expect("commit Prepared publication authorization");
            ledger
                .record_indeterminate(
                    SelectorAuthority::Missing,
                    SelectorAuthority::Missing,
                    format!(
                        "FT_REMOTE_UPGRADE_TRANSACTION_V1={TRANSACTION_ID}:indeterminate:{}\n",
                        "a".repeat(64)
                    ),
                )
                .expect("advance transaction to Indeterminate");
            let names_before = transaction_names(&ledger);
            let error = stale_publication
                .consume_publication(&ledger)
                .expect_err("terminal Indeterminate must invalidate publication permit");
            assert!(error
                .to_string()
                .contains("authorization is no longer the latest committed authority"));
            assert_eq!(transaction_names(&ledger), names_before);
        }

        {
            let (_fixture, root, effective_uid) = root_fixture();
            let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
                .expect("create RolledBack stale-permit transaction");
            ledger
                .authorize_publication(SelectorAuthority::Missing)
                .expect("commit Prepared publication authorization")
                .consume_publication(&ledger)
                .expect("consume current publication permit");
            let stale_selector = ledger
                .authorize_selector_after_publication(SelectorAuthority::Missing)
                .expect("commit Activating selector authorization");
            ledger
                .record_rolled_back(SelectorAuthority::Missing)
                .expect("advance transaction to RolledBack");
            let names_before = transaction_names(&ledger);
            let error = stale_selector
                .consume_selector(&ledger)
                .expect_err("terminal RolledBack must invalidate selector permit");
            assert!(error
                .to_string()
                .contains("authorization is no longer the latest committed authority"));
            assert_eq!(transaction_names(&ledger), names_before);
        }

        {
            let (_fixture, root, effective_uid) = root_fixture();
            let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
                .expect("create Committed stale-permit transaction");
            ledger
                .authorize_publication(SelectorAuthority::Missing)
                .expect("commit Prepared publication authorization")
                .consume_publication(&ledger)
                .expect("consume current publication permit");
            let stale_selector = ledger
                .authorize_selector_after_publication(SelectorAuthority::Missing)
                .expect("commit Activating selector authorization");
            let generation_id = "a".repeat(64);
            let target = format!("generations/{generation_id}");
            let selector_after = SelectorAuthority::selected(
                &generation_id,
                Path::new(&target),
                67,
                71,
            )
            .expect("create committed selector authority");
            ledger
                .commit_current(
                    selector_after,
                    format!(
                        "FT_REMOTE_GENERATION_PUBLICATION_V1={generation_id}:current:generations/{generation_id}\n"
                    ),
                )
                .expect("advance transaction to Committed");
            let names_before = transaction_names(&ledger);
            let error = stale_selector
                .consume_selector(&ledger)
                .expect_err("terminal Committed must invalidate selector permit");
            assert!(error
                .to_string()
                .contains("authorization is no longer the latest committed authority"));
            assert_eq!(transaction_names(&ledger), names_before);
        }
    }

    #[test]
    fn cross_open_durable_advancement_invalidates_retained_permit() {
        let (_fixture, root, effective_uid) = root_fixture();
        let upgrade_claim = claim('a');
        let mut original =
            RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim.clone())
                .expect("create original stale-permit ledger");
        let stale_permit = original
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared publication authorization");
        let mut advancing = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim)
            .expect("open the same durable transaction independently");
        advancing
            .record_indeterminate(
                SelectorAuthority::Missing,
                SelectorAuthority::Missing,
                format!(
                    "FT_REMOTE_UPGRADE_TRANSACTION_V1={TRANSACTION_ID}:indeterminate:{}\n",
                    "a".repeat(64)
                ),
            )
            .expect("advance independently opened transaction to Indeterminate");
        let names_before = transaction_names(&advancing);

        let error = stale_permit
            .consume_publication(&original)
            .expect_err("cross-open durable advancement must invalidate retained permit");
        assert!(error
            .to_string()
            .contains("committed authority changed after it was pinned"));
        assert_eq!(transaction_names(&advancing), names_before);
    }

    #[test]
    fn rolled_back_outcome_is_durable_terminal_and_preserves_authorized_attempt() {
        let (_fixture, root, effective_uid) = root_fixture();
        let upgrade_claim = claim('a');
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim.clone())
            .expect("create rollback transaction claim");
        authorize_activation_from_pending(&mut ledger);
        let activating = ledger.latest().expect("Activating authorization").clone();
        assert_eq!(activating.state(), RemoteUpgradeState::Activating);
        assert_eq!(activating.sequence, 3);
        assert_eq!(activating.attempt, 2);
        assert_eq!(activating.selector_before(), &SelectorAuthority::Missing);
        let names_before = transaction_names(&ledger);

        let rolled_back = ledger
            .record_rolled_back(SelectorAuthority::Missing)
            .expect("commit exact rolled-back selector outcome");
        assert_eq!(rolled_back.state(), RemoteUpgradeState::RolledBack);
        assert_eq!(rolled_back.sequence, activating.sequence + 1);
        assert_eq!(rolled_back.attempt, activating.attempt);
        assert_eq!(rolled_back.selector_before(), activating.selector_before());
        assert_eq!(rolled_back.selector_after(), activating.selector_before());
        assert_eq!(
            rolled_back.receipt(),
            format!(
                "FT_REMOTE_UPGRADE_TRANSACTION_V1={TRANSACTION_ID}:rolled_back:{}\n",
                "a".repeat(64)
            )
        );
        assert_eq!(
            transaction_names(&ledger).len(),
            names_before.len() + 2,
            "rollback completion must append exactly one record and one commit marker"
        );

        let mut replay = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim)
            .expect("reopen exact rolled-back transaction");
        assert_eq!(replay.latest(), Some(&rolled_back));
        let terminal_names = transaction_names(&replay);
        let error = replay
            .record_indeterminate(
                SelectorAuthority::Missing,
                SelectorAuthority::Missing,
                format!(
                    "FT_REMOTE_UPGRADE_TRANSACTION_V1={TRANSACTION_ID}:indeterminate:{}\n",
                    "a".repeat(64)
                ),
            )
            .expect_err("RolledBack must remain terminal");
        assert!(error
            .to_string()
            .contains("illegal remote upgrade transaction state transition"));
        assert_eq!(transaction_names(&replay), terminal_names);
    }

    #[test]
    fn rollback_requires_activating_authority_without_namespace_mutation() {
        let (_fixture, root, effective_uid) = root_fixture();
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
            .expect("create rollback precondition transaction");
        let unstarted_names = transaction_names(&ledger);
        let unstarted_error = ledger
            .record_rolled_back(SelectorAuthority::Missing)
            .expect_err("rollback without any authorization must fail");
        assert!(unstarted_error
            .to_string()
            .contains("no committed activation authorization"));
        assert_eq!(transaction_names(&ledger), unstarted_names);

        ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared publication authorization")
            .consume_publication(&ledger)
            .expect("consume publication permit");
        let prepared_names = transaction_names(&ledger);
        let prepared_error = ledger
            .record_rolled_back(SelectorAuthority::Missing)
            .expect_err("Prepared publication authority cannot authorize rollback");
        assert!(prepared_error
            .to_string()
            .contains("not preceded by Activating authority"));
        assert_eq!(
            ledger.latest().expect("Prepared remains").state(),
            RemoteUpgradeState::Prepared
        );
        assert_eq!(transaction_names(&ledger), prepared_names);
    }

    #[test]
    fn rollback_refuses_changed_or_unresolved_authority_without_mutation() {
        let (_fixture, root, effective_uid) = root_fixture();
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
            .expect("create rollback authority transaction");
        ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared publication authorization")
            .consume_publication(&ledger)
            .expect("consume publication permit");
        ledger
            .authorize_selector_after_publication(SelectorAuthority::Missing)
            .expect("commit Activating selector authorization")
            .consume_selector(&ledger)
            .expect("consume selector effect permit");
        let names_before = transaction_names(&ledger);
        let changed_generation = "d".repeat(64);
        let changed_target = format!("generations/{changed_generation}");
        let changed = SelectorAuthority::selected(
            &changed_generation,
            Path::new(&changed_target),
            41,
            43,
        )
        .expect("create different resolved selector authority");

        for observed in [changed, SelectorAuthority::unresolved_post_effect()] {
            let error = ledger
                .record_rolled_back(observed)
                .expect_err("non-restored authority must not become RolledBack");
            assert!(error
                .to_string()
                .contains("did not restore the exact committed pre-effect authority"));
            assert_eq!(transaction_names(&ledger), names_before);
            assert_eq!(
                ledger.latest().expect("Activating remains").state(),
                RemoteUpgradeState::Activating
            );
        }
    }

    #[test]
    fn rolled_back_record_validation_rejects_forged_changed_authority() {
        let (_fixture, root, effective_uid) = root_fixture();
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
            .expect("create forged rollback transaction");
        ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared publication authorization")
            .consume_publication(&ledger)
            .expect("consume publication permit");
        ledger
            .authorize_selector_after_publication(SelectorAuthority::Missing)
            .expect("commit Activating selector authorization")
            .consume_selector(&ledger)
            .expect("consume selector effect permit");
        let authorization = ledger.latest().expect("Activating authorization");
        let changed_generation = "d".repeat(64);
        let changed_target = format!("generations/{changed_generation}");
        let changed = SelectorAuthority::selected(
            &changed_generation,
            Path::new(&changed_target),
            47,
            53,
        )
        .expect("create forged post-rollback authority");
        let forged = RemoteUpgradeRecord {
            schema: RECORD_SCHEMA.to_string(),
            transaction_id: ledger.claim.transaction_id.clone(),
            claim_sha256: ledger.claim_sha256.clone(),
            claim: ledger.claim.clone(),
            sequence: authorization.sequence + 1,
            attempt: authorization.attempt,
            state: RemoteUpgradeState::RolledBack,
            generation_id: ledger.claim.generation_id.clone(),
            selector_before: authorization.selector_before.clone(),
            selector_after: changed,
            receipt: format!(
                "FT_REMOTE_UPGRADE_TRANSACTION_V1={TRANSACTION_ID}:rolled_back:{}\n",
                ledger.claim.generation_id
            ),
        };

        let error = forged
            .canonical_bytes(&ledger.claim, &ledger.claim_sha256)
            .expect_err("forged changed authority must fail canonical record validation");
        assert!(error
            .to_string()
            .contains("rolled-back record does not preserve one exact pre-effect authority"));
    }

    #[test]
    fn replay_rejects_rolled_back_record_bound_to_wrong_pre_effect_authority() {
        let (_fixture, root, effective_uid) = root_fixture();
        let upgrade_claim = claim('a');
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim.clone())
            .expect("create wrong-authority rollback transaction");
        ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared publication authorization")
            .consume_publication(&ledger)
            .expect("consume publication permit");
        ledger
            .authorize_selector_after_publication(SelectorAuthority::Missing)
            .expect("commit Activating selector authorization")
            .consume_selector(&ledger)
            .expect("consume selector effect permit");
        let authorization = ledger.latest().expect("Activating authorization");
        let wrong_generation = "d".repeat(64);
        let wrong_target = format!("generations/{wrong_generation}");
        let wrong_authority = SelectorAuthority::selected(
            &wrong_generation,
            Path::new(&wrong_target),
            59,
            61,
        )
        .expect("create wrong restored authority");
        let forged = RemoteUpgradeRecord {
            schema: RECORD_SCHEMA.to_string(),
            transaction_id: ledger.claim.transaction_id.clone(),
            claim_sha256: ledger.claim_sha256.clone(),
            claim: ledger.claim.clone(),
            sequence: authorization.sequence + 1,
            attempt: authorization.attempt,
            state: RemoteUpgradeState::RolledBack,
            generation_id: ledger.claim.generation_id.clone(),
            selector_before: wrong_authority.clone(),
            selector_after: wrong_authority,
            receipt: format!(
                "FT_REMOTE_UPGRADE_TRANSACTION_V1={TRANSACTION_ID}:rolled_back:{}\n",
                ledger.claim.generation_id
            ),
        };
        let bytes = forged
            .canonical_bytes(&ledger.claim, &ledger.claim_sha256)
            .expect("wrong-authority rollback is internally canonical");
        assert!(!record_authority_transition_is_valid(
            authorization,
            &forged
        ));
        let digest = domain_hash(RECORD_HASH_DOMAIN, &bytes);
        let record_name = format!(
            "record-{:04}-{digest}-{:02}.json",
            forged.sequence, forged.attempt
        );
        create_synchronized_file(
            &ledger.transaction,
            Path::new(&record_name),
            &bytes,
            effective_uid,
            ledger.device,
        )
        .expect("plant internally canonical wrong-authority rollback record");
        let commit_name = format!("commit-{:04}-{digest}.v1", forged.sequence);
        create_synchronized_file(
            &ledger.transaction,
            Path::new(&commit_name),
            &[],
            effective_uid,
            ledger.device,
        )
        .expect("commit internally canonical wrong-authority rollback record");
        sync_capability_directory(&ledger.transaction)
            .expect("sync forged rollback record and marker");

        let error = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim)
            .err()
            .expect("wrong-authority rollback must poison durable replay");
        assert!(error
            .to_string()
            .contains("illegal state or attempt transition"));
    }

    #[test]
    fn exact_canonical_outcome_orphan_is_adopted_without_a_second_record() {
        let (_fixture, root, effective_uid) = root_fixture();
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
            .expect("create transaction claim");
        ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared authorization")
            .consume_publication(&ledger)
            .expect("consume publication permit");
        let (orphan, bytes, digest) = pending_outcome_record(&ledger);
        let orphan_name = format!(
            "record-{:04}-{digest}-{:02}.json",
            orphan.sequence, orphan.attempt
        );
        create_synchronized_file(
            &ledger.transaction,
            Path::new(&orphan_name),
            &bytes,
            effective_uid,
            ledger.device,
        )
        .expect("plant exact canonical outcome orphan");
        sync_capability_directory(&ledger.transaction).expect("sync exact outcome orphan");
        let records_before = transaction_names(&ledger)
            .into_iter()
            .filter(|name| name.starts_with("record-"))
            .count();

        let committed = ledger
            .commit_pending_live_owner(SelectorAuthority::Missing, orphan.receipt.clone())
            .expect("adopt exact canonical outcome orphan");
        assert_eq!(committed, orphan);
        assert_eq!(
            transaction_names(&ledger)
                .into_iter()
                .filter(|name| name.starts_with("record-"))
                .count(),
            records_before,
            "orphan adoption must add only its commit marker"
        );
    }

    #[test]
    fn conflicting_canonical_outcome_orphan_poisons_without_mutation() {
        let (_fixture, root, effective_uid) = root_fixture();
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
            .expect("create transaction claim");
        ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared authorization")
            .consume_publication(&ledger)
            .expect("consume publication permit");
        let authorization = ledger.latest().expect("Prepared authorization");
        let conflicting = RemoteUpgradeRecord {
            schema: RECORD_SCHEMA.to_string(),
            transaction_id: ledger.claim.transaction_id.clone(),
            claim_sha256: ledger.claim_sha256.clone(),
            claim: ledger.claim.clone(),
            sequence: authorization.sequence + 1,
            attempt: authorization.attempt,
            state: RemoteUpgradeState::Indeterminate,
            generation_id: ledger.claim.generation_id.clone(),
            selector_before: authorization.selector_before.clone(),
            selector_after: authorization.selector_before.clone(),
            receipt: format!(
                "FT_REMOTE_UPGRADE_TRANSACTION_V1={}:indeterminate:{}\n",
                ledger.claim.transaction_id, ledger.claim.generation_id
            ),
        };
        let bytes = conflicting
            .canonical_bytes(&ledger.claim, &ledger.claim_sha256)
            .expect("serialize conflicting canonical outcome");
        let digest = domain_hash(RECORD_HASH_DOMAIN, &bytes);
        let name = format!(
            "record-{:04}-{digest}-{:02}.json",
            conflicting.sequence, conflicting.attempt
        );
        create_synchronized_file(
            &ledger.transaction,
            Path::new(&name),
            &bytes,
            effective_uid,
            ledger.device,
        )
        .expect("plant conflicting canonical orphan");
        sync_capability_directory(&ledger.transaction).expect("sync conflicting orphan");
        let names_before = transaction_names(&ledger);
        let (_, _, intended_digest) = pending_outcome_record(&ledger);
        let intended_receipt = format!(
            "FT_REMOTE_GENERATION_PUBLICATION_V1={}:pending_activation_lease\n",
            ledger.claim.generation_id
        );

        let error = ledger
            .commit_pending_live_owner(
                SelectorAuthority::Missing,
                intended_receipt,
            )
            .expect_err("conflicting orphan identity must poison the append");
        assert!(error.to_string().contains("conflicting orphan record"));
        assert_ne!(digest, intended_digest);
        assert_eq!(transaction_names(&ledger), names_before);
    }

    #[test]
    fn near_byte_cap_append_preflight_fails_without_namespace_mutation() {
        let (_fixture, root, effective_uid) = root_fixture();
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
            .expect("create transaction claim");
        ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared authorization")
            .consume_publication(&ledger)
            .expect("consume publication permit");
        let (pending, pending_bytes, _) = pending_outcome_record(&ledger);
        let scan = ledger
            .scan_revalidated_authority()
            .expect("scan transaction before near-cap fixture");
        let pending_len = u64::try_from(pending_bytes.len()).expect("pending record length fits");
        let mut filler_bytes = MAX_TRANSACTION_BYTES
            .checked_sub(scan.total_bytes)
            .and_then(|remaining| remaining.checked_sub(pending_len))
            .and_then(|remaining| remaining.checked_add(1))
            .expect("near-cap filler has positive bounded size");
        for sequence in 1..=MAX_COMMITTED_RECORDS {
            if filler_bytes == 0 {
                break;
            }
            let chunk = filler_bytes.min(MAX_RECORD_BYTES);
            let name = format!("record-{sequence:04}-{}-02.json", "e".repeat(64));
            let bytes = vec![b'x'; usize::try_from(chunk).expect("filler chunk fits usize")];
            create_synchronized_file(
                &ledger.transaction,
                Path::new(&name),
                &bytes,
                effective_uid,
                ledger.device,
            )
            .expect("plant bounded near-cap orphan");
            filler_bytes -= chunk;
        }
        assert_eq!(filler_bytes, 0, "bounded orphan identities must cover the fixture");
        sync_capability_directory(&ledger.transaction).expect("sync near-cap fixture");
        let names_before = transaction_names(&ledger);

        let error = ledger
            .commit_pending_live_owner(SelectorAuthority::Missing, pending.receipt)
            .expect_err("near-cap append must fail in preflight");
        assert!(error.to_string().contains("transaction byte bound"));
        assert_eq!(transaction_names(&ledger), names_before);
    }

    #[test]
    fn corrupt_newest_commit_poisons_transaction_without_older_record_fallback() {
        let (fixture, root, effective_uid) = root_fixture();
        let upgrade_claim = claim('a');
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim.clone())
            .expect("create transaction claim");
        let permit = ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared authorization");
        permit
            .consume_publication(&ledger)
            .expect("consume permit");
        ledger
            .commit_pending_live_owner(
                SelectorAuthority::Missing,
                format!(
                    "FT_REMOTE_GENERATION_PUBLICATION_V1={}:pending_activation_lease\n",
                    "a".repeat(64)
                ),
            )
            .expect("commit pending terminal progress");

        let transaction_path = fixture
            .path()
            .join("process-family/upgrade-transactions")
            .join(TRANSACTION_ID);
        let newest_commit = std::fs::read_dir(&transaction_path)
            .expect("read transaction artifacts")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("commit-0002-"))
            })
            .next()
            .expect("newest commit marker");
        std::fs::set_permissions(&newest_commit, std::fs::Permissions::from_mode(0o600))
            .expect("plant corrupt newest commit mode");
        let error = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim)
            .err()
            .expect("corrupt newest commit must poison the transaction");
        assert!(
            error.to_string().contains("mode 0o400"),
            "unexpected corruption rejection: {error:#}"
        );
    }

    #[test]
    fn detached_transaction_namespace_is_rejected_before_effect() {
        let (fixture, root, effective_uid) = root_fixture();
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, claim('a'))
            .expect("create transaction claim");
        let _permit = ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit Prepared authorization");

        let transactions_path = fixture
            .path()
            .join("process-family/upgrade-transactions");
        let retained_path = fixture
            .path()
            .join("process-family/upgrade-transactions-retained");
        std::fs::rename(&transactions_path, retained_path)
            .expect("detach the pinned transaction namespace");
        std::fs::create_dir(&transactions_path)
            .expect("plant a replacement transaction namespace");
        std::fs::set_permissions(
            &transactions_path,
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("set replacement transaction namespace mode");

        let error = ledger
            .revalidate_authority()
            .expect_err("a detached transaction namespace must not authorize an effect");
        assert!(
            error.to_string().contains("not one stable nofollow owner directory"),
            "unexpected detached-namespace rejection: {error:#}"
        );
    }

    #[test]
    fn uncommitted_partial_record_has_no_authority_and_advances_attempt_identity() {
        let (fixture, root, effective_uid) = root_fixture();
        let upgrade_claim = claim('a');
        let mut ledger = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim.clone())
            .expect("create transaction claim");
        let permit = ledger
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit first Prepared authorization");
        permit
            .consume_publication(&ledger)
            .expect("consume first permit");

        let transaction_path = fixture
            .path()
            .join("process-family/upgrade-transactions")
            .join(TRANSACTION_ID);
        let partial = transaction_path.join(format!(
            "record-0002-{}-02.json",
            "d".repeat(64)
        ));
        std::fs::write(&partial, b"partial-after-crash")
            .expect("plant uncommitted partial record artifact");
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o400))
            .expect("set canonical uncommitted record mode");

        let mut replay = RemoteUpgradeLedger::open(&root, effective_uid, upgrade_claim)
            .expect("ignore uncommitted record authority while retaining its attempt");
        let retry_permit = replay
            .authorize_publication(SelectorAuthority::Missing)
            .expect("commit a fresh Prepared retry");
        assert_eq!(replay.latest().expect("retry record").attempt, 3);
        assert_eq!(
            retry_permit
                .consume_publication(&replay)
                .expect("consume retry permit")
                .len(),
            32
        );
    }

    #[test]
    fn existing_authority_replay_redurabilizes_files_and_parent_chain() {
        let source = include_str!("remote_upgrade_ledger.rs");
        let authority = source
            .split("fn scan_revalidated_authority")
            .nth(1)
            .and_then(|tail| tail.split("pub(crate) fn authorize_publication").next())
            .expect("scan-revalidation source boundary");
        for required in [
            "sync_capability_directory(&self.transaction)",
            "sync_capability_directory(&self.transactions)",
            "sync_capability_directory(self.root)",
            "scan_transaction(",
        ] {
            assert!(
                authority.contains(required),
                "existing authority lost required re-durability step: {required}"
            );
        }
        let exact_file = source
            .split("fn validate_exact_file")
            .nth(1)
            .and_then(|tail| tail.split("fn read_exact_file").next())
            .expect("exact-file source boundary");
        assert!(
            exact_file.contains("file.sync_all()"),
            "claim/marker/uncommitted-record authority must be re-durabilized"
        );
        let exact_read = source
            .split("fn read_exact_file")
            .nth(1)
            .and_then(|tail| tail.split("fn parse_record_name").next())
            .expect("exact-read source boundary");
        assert!(
            exact_read.contains("file.sync_all()"),
            "committed record bytes must be re-durabilized before replay"
        );
    }

    #[test]
    fn state_machine_and_bounds_are_frozen() {
        assert!(RemoteUpgradeState::Prepared.permits_successor(RemoteUpgradeState::Prepared));
        assert!(
            RemoteUpgradeState::Prepared
                .permits_successor(RemoteUpgradeState::PendingLiveOwner)
        );
        assert!(
            RemoteUpgradeState::PendingLiveOwner
                .permits_successor(RemoteUpgradeState::Activating)
        );
        assert!(
            RemoteUpgradeState::Activating.permits_successor(RemoteUpgradeState::Committed)
        );
        assert!(
            RemoteUpgradeState::Activating.permits_successor(RemoteUpgradeState::RolledBack)
        );
        assert!(
            RemoteUpgradeState::Activating
                .permits_successor(RemoteUpgradeState::Indeterminate)
        );
        assert!(!RemoteUpgradeState::Committed.permits_successor(RemoteUpgradeState::Prepared));
        assert!(record_transition_is_valid(
            RemoteUpgradeState::Prepared,
            1,
            RemoteUpgradeState::Prepared,
            2,
        ));
        assert!(!record_transition_is_valid(
            RemoteUpgradeState::Prepared,
            1,
            RemoteUpgradeState::PendingLiveOwner,
            2,
        ));
        assert!(record_transition_is_valid(
            RemoteUpgradeState::Activating,
            2,
            RemoteUpgradeState::Committed,
            2,
        ));
        assert!(record_transition_is_valid(
            RemoteUpgradeState::Activating,
            2,
            RemoteUpgradeState::RolledBack,
            2,
        ));
        assert!(!record_transition_is_valid(
            RemoteUpgradeState::Activating,
            2,
            RemoteUpgradeState::RolledBack,
            3,
        ));
        assert!(RemoteUpgradeState::RolledBack.is_terminal());
        assert!(!RemoteUpgradeState::RolledBack
            .permits_successor(RemoteUpgradeState::Activating));
        assert_eq!(MAX_RECORD_BYTES, 64 * 1024);
        assert_eq!(MAX_COMMITTED_RECORDS, 32);
        assert_eq!(MAX_ATTEMPTS, 16);
        assert_eq!(MAX_TRANSACTION_BYTES, 2 * 1024 * 1024);
        assert_eq!(MAX_TRANSACTION_DIRECTORIES, 4_096);
    }
}
