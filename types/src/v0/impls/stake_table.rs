use std::{
    cmp::{max, min},
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    future::Future,
    sync::Arc,
};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{utils::format_ether, Address, U256},
    providers::Provider,
    rpc::types::Log,
};
use anyhow::{bail, ensure, Context};
use async_lock::{Mutex, RwLock};
use committable::Committable;
use futures::stream::{self, StreamExt};
use hotshot::types::{BLSPubKey, SchnorrPubKey, SignatureKey as _};
use hotshot_contract_adapter::sol_types::{
    EspToken::{self, EspTokenInstance},
    StakeTableV2::{
        self, ConsensusKeysUpdated, ConsensusKeysUpdatedV2, Delegated, Undelegated, ValidatorExit,
        ValidatorRegistered, ValidatorRegisteredV2,
    },
};
use hotshot_types::{
    data::{vid_disperse::VID_TARGET_TOTAL_STAKE, EpochNumber},
    drb::{
        election::{generate_stake_cdf, select_randomized_leader, RandomizedCommittee},
        DrbResult,
    },
    stake_table::{HSStakeTable, StakeTableEntry},
    traits::{
        election::Membership,
        node_implementation::{ConsensusTime, NodeType},
        signature_key::StakeTableEntryType,
    },
    PeerConfig,
};
use indexmap::IndexMap;
use thiserror::Error;
use tokio::{spawn, time::sleep};
use tracing::Instrument;

#[cfg(any(test, feature = "testing"))]
use super::v0_3::DAMembers;
use super::{
    traits::{MembershipPersistence, StateCatchup},
    v0_3::{ChainConfig, EventKey, Fetcher, StakeTableEvent, StakeTableUpdateTask, Validator},
    Header, L1Client, Leaf2, PubKey, SeqTypes,
};
use crate::{
    traits::EventsPersistenceRead,
    v0_1::{L1Provider, RewardAmount, BLOCKS_PER_YEAR, COMMISSION_BASIS_POINTS, INFLATION_RATE},
    v0_3::{EventSortingError, ExpectedStakeTableError, FetchRewardError, StakeTableError},
};

type Epoch = <SeqTypes as NodeType>::Epoch;
pub type ValidatorMap = IndexMap<Address, Validator<BLSPubKey>>;
/// The result of applying a stake table event:
/// - `Ok(Ok(()))`: success
/// - `Ok(Err(...))`: expected error
/// - `Err(...)`: serious error
type ApplyEventResult<T> = Result<Result<T, ExpectedStakeTableError>, StakeTableError>;

/// Format the alloy Log RPC type in a way to make it easy to find the event in an explorer.
trait DisplayLog {
    fn display(&self) -> String;
}

impl DisplayLog for Log {
    fn display(&self) -> String {
        // These values are all unlikely to be missing because we only create Log variables by
        // fetching them from the RPC, so for simplicity we use defaults if the any of the values
        // are missing.
        let block = self.block_number.unwrap_or_default();
        let index = self.log_index.unwrap_or_default();
        let hash = self.transaction_hash.unwrap_or_default();
        format!("Log(block={block},index={index},transaction_hash={hash})")
    }
}

#[derive(Clone, PartialEq)]
pub struct StakeTableEvents {
    registrations: Vec<(ValidatorRegistered, Log)>,
    registrations_v2: Vec<(ValidatorRegisteredV2, Log)>,
    deregistrations: Vec<(ValidatorExit, Log)>,
    delegated: Vec<(Delegated, Log)>,
    undelegated: Vec<(Undelegated, Log)>,
    keys: Vec<(ConsensusKeysUpdated, Log)>,
    keys_v2: Vec<(ConsensusKeysUpdatedV2, Log)>,
}

impl StakeTableEvents {
    /// Creates a new instance of `StakeTableEvents` with the provided events.
    ///
    /// Remove unauthenticated registration and key update events
    fn from_l1_logs(
        registrations: Vec<(ValidatorRegistered, Log)>,
        registrations_v2: Vec<(ValidatorRegisteredV2, Log)>,
        deregistrations: Vec<(ValidatorExit, Log)>,
        delegated: Vec<(Delegated, Log)>,
        undelegated: Vec<(Undelegated, Log)>,
        keys: Vec<(ConsensusKeysUpdated, Log)>,
        keys_v2: Vec<(ConsensusKeysUpdatedV2, Log)>,
    ) -> Self {
        let registrations_v2 = registrations_v2
            .into_iter()
            .filter(|(event, log)| {
                event
                    .authenticate()
                    .map_err(|_| {
                        tracing::warn!(
                            "Failed to authenticate ValidatorRegisteredV2 event {}",
                            log.display()
                        );
                    })
                    .is_ok()
            })
            .collect();
        let keys_v2 = keys_v2
            .into_iter()
            .filter(|(event, log)| {
                event
                    .authenticate()
                    .map_err(|_| {
                        tracing::warn!(
                            "Failed to authenticate ConsensusKeysUpdatedV2 event {}",
                            log.display()
                        );
                    })
                    .is_ok()
            })
            .collect();
        Self {
            registrations,
            registrations_v2,
            deregistrations,
            delegated,
            undelegated,
            keys,
            keys_v2,
        }
    }

    pub fn sort_events(self) -> Result<Vec<(EventKey, StakeTableEvent)>, EventSortingError> {
        let mut events: Vec<(EventKey, StakeTableEvent)> = Vec::new();
        let Self {
            registrations,
            registrations_v2,
            deregistrations,
            delegated,
            undelegated,
            keys,
            keys_v2,
        } = self;

        let key = |log: &Log| -> Result<EventKey, EventSortingError> {
            let block_number = log
                .block_number
                .ok_or(EventSortingError::MissingBlockNumber)?;
            let log_index = log.log_index.ok_or(EventSortingError::MissingLogIndex)?;
            Ok((block_number, log_index))
        };

        for (registration, log) in registrations {
            events.push((key(&log)?, registration.into()));
        }
        for (registration, log) in registrations_v2 {
            events.push((key(&log)?, registration.into()));
        }
        for (dereg, log) in deregistrations {
            events.push((key(&log)?, dereg.into()));
        }
        for (delegation, log) in delegated {
            events.push((key(&log)?, delegation.into()));
        }
        for (undelegated, log) in undelegated {
            events.push((key(&log)?, undelegated.into()));
        }
        for (update, log) in keys {
            events.push((key(&log)?, update.into()));
        }
        for (update, log) in keys_v2 {
            events.push((key(&log)?, update.into()));
        }

        events.sort_by_key(|(key, _)| *key);
        Ok(events)
    }
}

#[derive(Debug)]
pub struct StakeTableState {
    validators: ValidatorMap,
    used_bls_keys: HashSet<BLSPubKey>,
    used_schnorr_keys: HashSet<SchnorrPubKey>,
}

impl StakeTableState {
    pub fn new() -> Self {
        Self {
            validators: IndexMap::new(),
            used_bls_keys: HashSet::new(),
            used_schnorr_keys: HashSet::new(),
        }
    }

    pub fn get_validators(self) -> ValidatorMap {
        self.validators
    }

    pub fn apply_event(&mut self, event: StakeTableEvent) -> ApplyEventResult<()> {
        match event {
            StakeTableEvent::Register(ValidatorRegistered {
                account,
                blsVk,
                schnorrVk,
                commission,
            }) => {
                let stake_table_key: BLSPubKey = blsVk.into();
                let state_ver_key: SchnorrPubKey = schnorrVk.into();

                let entry = self.validators.entry(account);
                if let indexmap::map::Entry::Occupied(_) = entry {
                    return Err(StakeTableError::AlreadyRegistered(account));
                }

                // The stake table contract enforces that each bls key is only used once.
                if !self.used_bls_keys.insert(stake_table_key) {
                    return Err(StakeTableError::BlsKeyAlreadyUsed(
                        stake_table_key.to_string(),
                    ));
                }

                // The contract does *not* enforce that each schnorr key is only used once.
                if !self.used_schnorr_keys.insert(state_ver_key.clone()) {
                    return Ok(Err(ExpectedStakeTableError::SchnorrKeyAlreadyUsed(
                        state_ver_key.to_string(),
                    )));
                }

                entry.or_insert(Validator {
                    account,
                    stake_table_key,
                    state_ver_key,
                    stake: U256::ZERO,
                    commission,
                    delegators: HashMap::new(),
                });
            },

            StakeTableEvent::RegisterV2(reg) => {
                // Signature authentication is performed right after fetching, if we get an
                // unauthenticated event here, something went wrong, we abort early.
                reg.authenticate()
                    .map_err(|e| StakeTableError::AuthenticationFailed(e.to_string()))?;

                let ValidatorRegisteredV2 {
                    account,
                    blsVK,
                    schnorrVK,
                    commission,
                    ..
                } = reg;

                let stake_table_key: BLSPubKey = blsVK.into();
                let state_ver_key: SchnorrPubKey = schnorrVK.into();

                let entry = self.validators.entry(account);
                if let indexmap::map::Entry::Occupied(_) = entry {
                    return Err(StakeTableError::AlreadyRegistered(account));
                }

                // The stake table contract enforces that each bls key is only used once.
                if !self.used_bls_keys.insert(stake_table_key) {
                    return Err(StakeTableError::BlsKeyAlreadyUsed(
                        stake_table_key.to_string(),
                    ));
                }

                // The contract does *not* enforce that each schnorr key is only used once.
                if !self.used_schnorr_keys.insert(state_ver_key.clone()) {
                    return Ok(Err(ExpectedStakeTableError::SchnorrKeyAlreadyUsed(
                        state_ver_key.to_string(),
                    )));
                }

                entry.or_insert(Validator {
                    account,
                    stake_table_key,
                    state_ver_key,
                    stake: U256::ZERO,
                    commission,
                    delegators: HashMap::new(),
                });
            },

            StakeTableEvent::Deregister(exit) => {
                self.validators
                    .shift_remove(&exit.validator)
                    .ok_or(StakeTableError::ValidatorNotFound(exit.validator))?;
            },

            StakeTableEvent::Delegate(delegated) => {
                let Delegated {
                    delegator,
                    validator,
                    amount,
                } = delegated;

                let val = self
                    .validators
                    .get_mut(&validator)
                    .ok_or(StakeTableError::ValidatorNotFound(validator))?;

                if amount.is_zero() {
                    return Err(StakeTableError::ZeroDelegatorStake(delegator));
                }

                val.stake += amount;
                // Insert the delegator with the given stake
                // or increase the stake if already present
                val.delegators
                    .entry(delegator)
                    .and_modify(|stake| *stake += amount)
                    .or_insert(amount);
            },

            StakeTableEvent::Undelegate(undelegated) => {
                let Undelegated {
                    delegator,
                    validator,
                    amount,
                } = undelegated;

                let val = self
                    .validators
                    .get_mut(&validator)
                    .ok_or(StakeTableError::ValidatorNotFound(validator))?;

                val.stake = val
                    .stake
                    .checked_sub(amount)
                    .ok_or(StakeTableError::InsufficientStake)?;

                let delegator_stake = val
                    .delegators
                    .get_mut(&delegator)
                    .ok_or(StakeTableError::DelegatorNotFound(delegator))?;

                *delegator_stake = delegator_stake
                    .checked_sub(amount)
                    .ok_or(StakeTableError::InsufficientStake)?;

                if delegator_stake.is_zero() {
                    val.delegators.remove(&delegator);
                }
            },

            StakeTableEvent::KeyUpdate(update) => {
                let ConsensusKeysUpdated {
                    account,
                    blsVK,
                    schnorrVK,
                } = update;

                let validator = self
                    .validators
                    .get_mut(&account)
                    .ok_or(StakeTableError::ValidatorNotFound(account))?;

                let stake_table_key: BLSPubKey = blsVK.into();
                let state_ver_key: SchnorrPubKey = schnorrVK.into();

                if !self.used_bls_keys.insert(stake_table_key) {
                    return Err(StakeTableError::BlsKeyAlreadyUsed(
                        stake_table_key.to_string(),
                    ));
                }

                // The contract does *not* enforce that each schnorr key is only used once,
                // therefore it's possible to have multiple validators with the same schnorr key.
                if !self.used_schnorr_keys.insert(state_ver_key.clone()) {
                    return Ok(Err(ExpectedStakeTableError::SchnorrKeyAlreadyUsed(
                        state_ver_key.to_string(),
                    )));
                }

                validator.stake_table_key = stake_table_key;
                validator.state_ver_key = state_ver_key;
            },

            StakeTableEvent::KeyUpdateV2(update) => {
                // Signature authentication is performed right after fetching, if we get an
                // unauthenticated event here, something went wrong, we abort early.
                update
                    .authenticate()
                    .map_err(|e| StakeTableError::AuthenticationFailed(e.to_string()))?;

                let ConsensusKeysUpdatedV2 {
                    account,
                    blsVK,
                    schnorrVK,
                    ..
                } = update;

                let validator = self
                    .validators
                    .get_mut(&account)
                    .ok_or(StakeTableError::ValidatorNotFound(account))?;

                let stake_table_key: BLSPubKey = blsVK.into();
                let state_ver_key: SchnorrPubKey = schnorrVK.into();

                // The stake table contract enforces that each bls key is only used once.
                if !self.used_bls_keys.insert(stake_table_key) {
                    return Err(StakeTableError::BlsKeyAlreadyUsed(
                        stake_table_key.to_string(),
                    ));
                }

                // The contract does *not* enforce that each schnorr key is only used once,
                // therefore it's possible to have multiple validators with the same schnorr key.
                if !self.used_schnorr_keys.insert(state_ver_key.clone()) {
                    return Ok(Err(ExpectedStakeTableError::SchnorrKeyAlreadyUsed(
                        state_ver_key.to_string(),
                    )));
                }

                validator.stake_table_key = stake_table_key;
                validator.state_ver_key = state_ver_key;
            },
        }

        Ok(Ok(()))
    }
}

pub fn validators_from_l1_events<I: Iterator<Item = StakeTableEvent>>(
    events: I,
) -> Result<ValidatorMap, StakeTableError> {
    let mut state = StakeTableState::new();
    for event in events {
        match state.apply_event(event.clone()) {
            Ok(Ok(())) => (), // Event successfully applied
            Ok(Err(expected_err)) => {
                // expected error, continue
                tracing::warn!("Expected error while applying event {event:?}: {expected_err}");
            },
            Err(err) => {
                // stop processing due to fatal error
                tracing::error!("Fatal error in applying event {event:?}: {err}");
                return Err(err);
            },
        }
    }
    Ok(state.get_validators())
}

/// Select active validators
///
/// Removes the validators without stake and selects the top 100 staked validators.
pub(crate) fn select_active_validator_set(
    validators: &mut ValidatorMap,
) -> Result<(), StakeTableError> {
    let total_validators = validators.len();

    // Remove invalid validators first
    validators.retain(|address, validator| {
        if validator.delegators.is_empty() {
            tracing::info!("Validator {address:?} does not have any delegator");
            return false;
        }

        if validator.stake.is_zero() {
            tracing::info!("Validator {address:?} does not have any stake");
            return false;
        }

        true
    });

    tracing::debug!(
        total_validators,
        filtered = validators.len(),
        "Filtered out invalid validators"
    );

    if validators.is_empty() {
        tracing::warn!("Validator selection failed: no validators passed minimum criteria");
        return Err(StakeTableError::NoValidValidators);
    }

    let maximum_stake = validators.values().map(|v| v.stake).max().ok_or_else(|| {
        tracing::error!("Could not compute maximum stake from filtered validators");
        StakeTableError::MissingMaximumStake
    })?;

    let minimum_stake = maximum_stake
        .checked_div(U256::from(VID_TARGET_TOTAL_STAKE))
        .ok_or_else(|| {
            tracing::error!("Overflow while calculating minimum stake threshold");
            StakeTableError::MinimumStakeOverflow
        })?;

    let mut valid_stakers: Vec<_> = validators
        .iter()
        .filter(|(_, v)| v.stake >= minimum_stake)
        .map(|(addr, v)| (*addr, v.stake))
        .collect();

    tracing::info!(
        count = valid_stakers.len(),
        "Number of validators above minimum stake threshold"
    );

    // Sort by stake (descending order)
    valid_stakers.sort_by_key(|(_, stake)| std::cmp::Reverse(*stake));

    if valid_stakers.len() > 100 {
        valid_stakers.truncate(100);
    }

    // Retain only the selected validators
    let selected_addresses: HashSet<_> = valid_stakers.iter().map(|(addr, _)| *addr).collect();
    validators.retain(|address, _| selected_addresses.contains(address));

    tracing::info!(
        final_count = validators.len(),
        "Selected active validator set"
    );

    Ok(())
}

/// Extract the active validator set from the L1 stake table events.
pub(crate) fn active_validator_set_from_l1_events<I: Iterator<Item = StakeTableEvent>>(
    events: I,
) -> Result<ValidatorMap, StakeTableError> {
    let mut validators = validators_from_l1_events(events)?;
    select_active_validator_set(&mut validators)?;
    Ok(validators)
}

impl std::fmt::Debug for StakeTableEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StakeTableEvent::Register(event) => write!(f, "Register({:?})", event.account),
            StakeTableEvent::RegisterV2(event) => write!(f, "RegisterV2({:?})", event.account),
            StakeTableEvent::Deregister(event) => write!(f, "Deregister({:?})", event.validator),
            StakeTableEvent::Delegate(event) => write!(f, "Delegate({:?})", event.delegator),
            StakeTableEvent::Undelegate(event) => write!(f, "Undelegate({:?})", event.delegator),
            StakeTableEvent::KeyUpdate(event) => write!(f, "KeyUpdate({:?})", event.account),
            StakeTableEvent::KeyUpdateV2(event) => write!(f, "KeyUpdateV2({:?})", event.account),
        }
    }
}

#[derive(Clone, derive_more::derive::Debug)]
/// Type to describe DA and Stake memberships
pub struct EpochCommittees {
    /// Committee used when we're in pre-epoch state
    non_epoch_committee: NonEpochCommittee,
    /// Holds Stake table and da stake
    state: HashMap<Epoch, EpochCommittee>,
    /// Randomized committees, filled when we receive the DrbResult
    randomized_committees: BTreeMap<Epoch, RandomizedCommittee<StakeTableEntry<PubKey>>>,
    first_epoch: Option<Epoch>,
    block_reward: RewardAmount,
    fetcher: Arc<Fetcher>,
}

impl Fetcher {
    pub fn new(
        peers: Arc<dyn StateCatchup>,
        persistence: Arc<Mutex<dyn MembershipPersistence>>,
        l1_client: L1Client,
        chain_config: ChainConfig,
    ) -> Self {
        Self {
            peers,
            persistence,
            l1_client,
            chain_config: Arc::new(Mutex::new(chain_config)),
            update_task: StakeTableUpdateTask(Mutex::new(None)).into(),
        }
    }

    pub async fn spawn_update_loop(&self) {
        let mut update_task = self.update_task.0.lock().await;
        if update_task.is_none() {
            *update_task = Some(spawn(self.update_loop()));
        }
    }

    /// Periodically updates the stake table from the L1 contract.
    /// This function polls the finalized block number from the L1 client at an interval
    /// and fetches stake table from contract
    /// and updates the persistence
    fn update_loop(&self) -> impl Future<Output = ()> {
        let span = tracing::warn_span!("Stake table update loop");
        let self_clone = self.clone();
        let state = self.l1_client.state.clone();
        let l1_retry = self.l1_client.options().l1_retry_delay;
        let update_delay = self.l1_client.options().stake_table_update_interval;
        let chain_config = self.chain_config.clone();

        async move {
            // Get the stake table contract address from the chain config.
            // This may not contain a stake table address if we are on a pre-epoch version.
            // It keeps retrying until the chain config is upgraded
            // after a successful upgrade to an epoch version.
            let stake_contract_address = loop {
                match chain_config.lock().await.stake_table_contract {
                    Some(addr) => break addr,
                    None => {
                        tracing::debug!(
                            "Stake table contract address not found. Retrying in {l1_retry:?}...",
                        );
                    },
                }
                sleep(l1_retry).await;
            };

            // Begin the main polling loop
            loop {
                let finalized_block = loop {
                    if let Some(block) = state.lock().await.last_finalized {
                        break block;
                    }
                    tracing::debug!(
                        "Finalized block not yet available. Retrying in {l1_retry:?}",
                    );
                    sleep(l1_retry).await;
                };

                tracing::debug!(
                    "Attempting to fetch stake table at L1 block {finalized_block:?}",
                );

                loop {
                    match self_clone
                        .fetch_and_store_stake_table_events(stake_contract_address, finalized_block)
                        .await
                        {
                            Ok(events) => {
                                tracing::info!("Successfully fetched and stored stake table events at block={finalized_block:?}");
                                tracing::debug!("events={events:?}");
                                break;
                            },
                            Err(e) => {
                                tracing::error!(
                                    "Error fetching stake table at block {finalized_block:?}. err= {e:#}",
                                );
                                sleep(l1_retry).await;
                            },
                        }
                    }

                tracing::debug!(
                    "Waiting {update_delay:?} before next stake table update...",
                );
                sleep(update_delay).await;
            }
        }
        .instrument(span)
    }

    pub async fn fetch_events(
        &self,
        contract: Address,
        to_block: u64,
    ) -> anyhow::Result<Vec<(EventKey, StakeTableEvent)>> {
        let persistence_lock = self.persistence.lock().await;
        let (read_l1_offset, persistence_events) = persistence_lock.load_events(to_block).await?;
        drop(persistence_lock);

        tracing::info!("loaded events from storage to_block={to_block:?}");

        // No need to fetch from contract
        // if persistence returns all the events that we need
        if let Some(EventsPersistenceRead::Complete) = read_l1_offset {
            return Ok(persistence_events);
        }

        let from_block = read_l1_offset
            .map(|read| match read {
                EventsPersistenceRead::UntilL1Block(block) => Ok(block + 1),
                EventsPersistenceRead::Complete => Err(anyhow::anyhow!(
                    "Unexpected state. offset is complete after returning early"
                )),
            })
            .transpose()?;

        ensure!(
            Some(to_block) >= from_block,
            "to_block {to_block:?} is less than from_block {from_block:?}"
        );

        tracing::info!(%to_block, from_block = ?from_block, "Fetching events from contract");

        let contract_events = Self::fetch_events_from_contract(
            self.l1_client.clone(),
            contract,
            from_block,
            to_block,
        )
        .await;

        let contract_events = contract_events.sort_events()?;
        let mut events = match from_block {
            Some(_) => persistence_events
                .into_iter()
                .chain(contract_events)
                .collect(),
            None => contract_events,
        };

        // There are no duplicates because the RPC returns all events,
        // which are stored directly in persistence as is.
        // However, this step is taken as a precaution.
        // The vector is already sorted above, so this should be fast.
        let len_before_dedup = events.len();
        events.dedup();
        let len_after_dedup = events.len();
        if len_before_dedup != len_after_dedup {
            tracing::warn!("Duplicate events found and removed. This should not normally happen.")
        }

        Ok(events)
    }

    /// Fetch all stake table events from L1
    pub async fn fetch_events_from_contract(
        l1_client: L1Client,
        contract: Address,
        from_block: Option<u64>,
        to_block: u64,
    ) -> StakeTableEvents {
        let stake_table_contract = StakeTableV2::new(contract, l1_client.provider.clone());

        // get the block number when the contract was initialized
        // to avoid fetching events from block number 0
        let from_block = match from_block {
            Some(block) => block,
            None => {
                loop {
                    match stake_table_contract.initializedAtBlock().call().await {
                        Ok(init_block) => {
                            break init_block._0.to::<u64>();
                        },
                        Err(err) => {
                            // Retry fetching incase of an error
                            tracing::warn!(%err, "Failed to retrieve initial block, retrying..");
                            sleep(l1_client.options().l1_retry_delay).await;
                        },
                    }
                }
            },
        };

        // To avoid making large RPC calls, divide the range into smaller chunks.
        // chunk size is from env "ESPRESSO_SEQUENCER_L1_EVENTS_MAX_BLOCK_RANGE
        // default value  is `10000` if env variable is not set
        let mut start = from_block;
        let end = to_block;
        let chunk_size = l1_client.options().l1_events_max_block_range;
        let chunks = std::iter::from_fn(move || {
            let chunk_end = min(start + chunk_size - 1, end);
            if chunk_end < start {
                return None;
            }

            let chunk = (start, chunk_end);
            start = chunk_end + 1;
            Some(chunk)
        });

        // fetch registered events
        // retry if the call to the provider to fetch the events fails
        let registered_events = stream::iter(chunks.clone()).then(|(from, to)| {
            let retry_delay = l1_client.options().l1_retry_delay;
            let stake_table_contract = stake_table_contract.clone();
            async move {
                tracing::debug!(from, to, "fetch ValidatorRegistered events in range");
                loop {
                    match stake_table_contract
                        .clone()
                        .ValidatorRegistered_filter()
                        .from_block(from)
                        .to_block(to)
                        .query()
                        .await
                    {
                        Ok(events) => break stream::iter(events),
                        Err(err) => {
                            tracing::warn!(from, to, %err, "ValidatorRegistered Error");
                            sleep(retry_delay).await;
                        },
                    }
                }
            }
        });

        // fetch registered events v2
        // retry if the call to the provider to fetch the events fails
        let registered_events_v2 = stream::iter(chunks.clone()).then(|(from, to)| {
            let retry_delay = l1_client.options().l1_retry_delay;
            let stake_table_contract = stake_table_contract.clone();
            async move {
                tracing::debug!(from, to, "fetch ValidatorRegisteredV2 events in range");
                loop {
                    match stake_table_contract
                        .clone()
                        .ValidatorRegisteredV2_filter()
                        .from_block(from)
                        .to_block(to)
                        .query()
                        .await
                    {
                        Ok(events) => {
                            break stream::iter(events.into_iter().filter(|(event, log)| {
                                if let Err(e) = event.authenticate() {
                                    tracing::warn!(%e, "Failed to authenticate ValidatorRegisteredV2 event: {}", log.display());
                                    return false;
                                }
                                true
                            }));
                        },
                        Err(err) => {
                            tracing::warn!(from, to, %err, "ValidatorRegisteredV2 Error");
                            sleep(retry_delay).await;
                        },
                    }
                }
            }
        });

        // fetch validator de registration events
        let deregistered_events = stream::iter(chunks.clone()).then(|(from, to)| {
            let retry_delay = l1_client.options().l1_retry_delay;
            let stake_table_contract = stake_table_contract.clone();
            async move {
                tracing::debug!(from, to, "fetch ValidatorExit events in range");
                loop {
                    match stake_table_contract
                        .ValidatorExit_filter()
                        .from_block(from)
                        .to_block(to)
                        .query()
                        .await
                    {
                        Ok(events) => break stream::iter(events),
                        Err(err) => {
                            tracing::warn!(from, to, %err, "ValidatorExit Error");
                            sleep(retry_delay).await;
                        },
                    }
                }
            }
        });

        // fetch delegated events
        let delegated_events = stream::iter(chunks.clone()).then(|(from, to)| {
            let retry_delay = l1_client.options().l1_retry_delay;
            let stake_table_contract = stake_table_contract.clone();
            async move {
                tracing::debug!(from, to, "fetch Delegated events in range");
                loop {
                    match stake_table_contract
                        .Delegated_filter()
                        .from_block(from)
                        .to_block(to)
                        .query()
                        .await
                    {
                        Ok(events) => break stream::iter(events),
                        Err(err) => {
                            tracing::warn!(from, to, %err, "Delegated Error");
                            sleep(retry_delay).await;
                        },
                    }
                }
            }
        });
        // fetch undelegated events
        let undelegated_events = stream::iter(chunks.clone()).then(|(from, to)| {
            let retry_delay = l1_client.options().l1_retry_delay;
            let stake_table_contract = stake_table_contract.clone();
            async move {
                tracing::debug!(from, to, "fetch Undelegated events in range");
                loop {
                    match stake_table_contract
                        .Undelegated_filter()
                        .from_block(from)
                        .to_block(to)
                        .query()
                        .await
                    {
                        Ok(events) => break stream::iter(events),
                        Err(err) => {
                            tracing::warn!(from, to, %err, "Undelegated Error");
                            sleep(retry_delay).await;
                        },
                    }
                }
            }
        });

        // fetch consensus keys updated events
        let keys_update_events = stream::iter(chunks.clone()).then(|(from, to)| {
            let retry_delay = l1_client.options().l1_retry_delay;
            let stake_table_contract = stake_table_contract.clone();
            async move {
                tracing::debug!(from, to, "fetch ConsensusKeysUpdated events in range");
                loop {
                    match stake_table_contract
                        .ConsensusKeysUpdated_filter()
                        .from_block(from)
                        .to_block(to)
                        .query()
                        .await
                    {
                        Ok(events) => break stream::iter(events),
                        Err(err) => {
                            tracing::warn!(from, to, %err, "ConsensusKeysUpdated Error");
                            sleep(retry_delay).await;
                        },
                    }
                }
            }
        });

        // fetch consensus keys updated v2 events
        let keys_update_events_v2 = stream::iter(chunks).then(|(from, to)| {
            let retry_delay = l1_client.options().l1_retry_delay;
            let stake_table_contract = stake_table_contract.clone();
            async move {
                tracing::debug!(from, to, "fetch ConsensusKeysUpdatedV2 events in range");
                loop {
                    match stake_table_contract
                        .ConsensusKeysUpdatedV2_filter()
                        .from_block(from)
                        .to_block(to)
                        .query()
                        .await
                    {
                        Ok(events) => {
                            break stream::iter(events.into_iter().filter(|(event, log)| {
                                if let Err(e) = event.authenticate() {
                                    tracing::warn!(%e, "Failed to authenticate ConsensusKeysUpdatedV2 event {}", log.display());
                                    return false;
                                }
                                true
                            }));
                        },
                        Err(err) => {
                            tracing::warn!(from, to, %err, "ConsensusKeysUpdatedV2 Error");
                            sleep(retry_delay).await;
                        },
                    }
                }
            }
        });

        let registrations = registered_events.flatten().collect().await;
        let registrations_v2 = registered_events_v2.flatten().collect().await;
        let deregistrations = deregistered_events.flatten().collect().await;
        let delegated = delegated_events.flatten().collect().await;
        let undelegated = undelegated_events.flatten().collect().await;
        let keys = keys_update_events.flatten().collect().await;
        let keys_v2 = keys_update_events_v2.flatten().collect().await;

        StakeTableEvents::from_l1_logs(
            registrations,
            registrations_v2,
            deregistrations,
            delegated,
            undelegated,
            keys,
            keys_v2,
        )
    }

    /// Get `StakeTable` at specific l1 block height.
    /// This function fetches and processes various events (ValidatorRegistered, ValidatorExit,
    /// Delegated, Undelegated, and ConsensusKeysUpdated) within the block range from the
    /// contract's initialization block to the provided `to_block` value.
    /// Events are fetched in chunks to and retries are implemented for failed requests.
    pub async fn fetch_and_store_stake_table_events(
        &self,
        contract: Address,
        to_block: u64,
    ) -> anyhow::Result<Vec<(EventKey, StakeTableEvent)>> {
        let events = self.fetch_events(contract, to_block).await?;

        tracing::info!("storing events in storage to_block={to_block:?}");

        {
            let persistence_lock = self.persistence.lock().await;
            persistence_lock
                .store_events(to_block, events.clone())
                .await
                .inspect_err(|e| tracing::error!("failed to store events. err={e}"))?;
        }

        Ok(events)
    }

    // Only used by staking CLI which doesn't have persistence
    pub async fn fetch_all_validators_from_contract(
        l1_client: L1Client,
        contract: Address,
        to_block: u64,
    ) -> anyhow::Result<ValidatorMap> {
        let events = Self::fetch_events_from_contract(l1_client, contract, None, to_block).await;
        let sorted = events.sort_events()?;
        // Process the sorted events and return the resulting stake table.
        validators_from_l1_events(sorted.into_iter().map(|(_, e)| e))
            .context("failed to construct validators set from l1 events")
    }
    /// This function is used to calculate the reward for a block.
    /// It fetches the initial supply from the token contract.
    ///
    /// - We now rely on the `Initialized` event of the token contract (which should only occur once).
    /// - After locating this event, we fetch its transaction receipt and look for a decoded `Transfer` log
    /// - If either step fails, the function aborts to prevent incorrect reward calculations.
    ///
    /// Relying on mint events directly e.g., searching for mints from the zero address is prone to errors
    /// because in future when reward withdrawals are supported, there might be more than one mint transfer logs from
    /// zero address
    ///
    /// The ESP token contract itself does not expose the initialization block
    /// but the stake table contract does
    /// The stake table contract is deployed after the token contract as it holds the token
    /// contract address. We use the stake table contract initialization block as a safe upper bound when scanning
    ///  backwards for the token contract initialization event
    pub async fn fetch_block_reward(&self) -> Result<RewardAmount, FetchRewardError> {
        let chain_config = *self.chain_config.lock().await;

        let stake_table_contract = chain_config
            .stake_table_contract
            .ok_or(FetchRewardError::MissingStakeTableContract)?;

        let provider = self.l1_client.provider.clone();
        let stake_table = StakeTableV2::new(stake_table_contract, provider.clone());

        // Get the block number where the stake table was initialized
        // Stake table contract has the token contract address
        // so the token contract is deployed before the stake table contract
        let stake_table_init_block = stake_table
            .initializedAtBlock()
            .block(BlockId::finalized())
            .call()
            .await
            .map_err(FetchRewardError::ContractCall)?
            ._0
            .to::<u64>();

        tracing::info!("stake table init block ={stake_table_init_block}");

        let token_address = stake_table
            .token()
            .block(BlockId::finalized())
            .call()
            .await
            .map_err(FetchRewardError::TokenAddressFetch)?
            ._0;

        let token = EspToken::new(token_address, provider.clone());

        // Try to fetch the `Initialized` event directly. This event is emitted only once,
        // during the token contract initialization. The initialization transaction also transfers initial supply minted
        // from the zero address. Since the result set is small (a single event),
        // most RPC providers like Infura and Alchemy allow querying across the full block range
        // If this fails because provider does not allow the query due to rate limiting (or some other error), we fall back to scanning over
        // a fixed block range.
        let init_logs = token
            .Initialized_filter()
            .from_block(0u64)
            .to_block(BlockNumberOrTag::Finalized)
            .query()
            .await;

        let init_log = match init_logs {
            Ok(init_logs) => {
                if init_logs.is_empty() {
                    tracing::error!(
                        "Token Initialized event logs are empty. This should never happen"
                    );
                    return Err(FetchRewardError::MissingInitializedEvent);
                }

                let (_, init_log) = init_logs[0].clone();

                tracing::debug!(tx_hash = ?init_log.transaction_hash, "Found token `Initialized` event");
                init_log
            },
            Err(err) => {
                tracing::warn!(
                    "RPC returned error {err:?}. will fallback to scanning over fixed block range"
                );
                self.scan_token_contract_initialized_event_log(stake_table_init_block, token)
                    .await?
            },
        };

        // Get the transaction that emitted the Initialized event
        let tx_hash =
            init_log
                .transaction_hash
                .ok_or_else(|| FetchRewardError::MissingTransactionHash {
                    init_log: init_log.clone().into(),
                })?;

        // Get the transaction that emitted the Initialized event
        let init_tx = provider
            .get_transaction_receipt(tx_hash)
            .await
            .map_err(FetchRewardError::Rpc)?
            .ok_or_else(|| FetchRewardError::MissingTransactionReceipt {
                tx_hash: tx_hash.to_string(),
            })?;

        let mint_transfer = init_tx.decoded_log::<EspToken::Transfer>().ok_or(
            FetchRewardError::DecodeTransferLog {
                tx_hash: tx_hash.to_string(),
            },
        )?;

        tracing::debug!("mint transfer event ={mint_transfer:?}");
        if mint_transfer.from != Address::ZERO {
            return Err(FetchRewardError::InvalidMintFromAddress);
        }

        let initial_supply = mint_transfer.value;

        tracing::info!("Initial token amount: {} ESP", format_ether(initial_supply));

        let reward = ((initial_supply * U256::from(INFLATION_RATE)) / U256::from(BLOCKS_PER_YEAR))
            .checked_div(U256::from(COMMISSION_BASIS_POINTS))
            .ok_or(FetchRewardError::DivisionByZero)?;

        Ok(RewardAmount(reward))
    }

    pub async fn scan_token_contract_initialized_event_log(
        &self,
        stake_table_init_block: u64,
        token: EspTokenInstance<(), L1Provider>,
    ) -> Result<Log, FetchRewardError> {
        let max_events_range = self.l1_client.options().l1_events_max_block_range;
        const MAX_BLOCKS_SCANNED: u64 = 200_000;
        let mut total_scanned = 0;

        let mut from_block = stake_table_init_block.saturating_sub(max_events_range);
        let mut to_block = stake_table_init_block;

        loop {
            if total_scanned >= MAX_BLOCKS_SCANNED {
                tracing::error!(
                    total_scanned,
                    "Exceeded maximum scan range while searching for token Initialized event"
                );
                return Err(FetchRewardError::ExceededMaxScanRange(MAX_BLOCKS_SCANNED));
            }

            let init_logs = token
                .Initialized_filter()
                .from_block(from_block)
                .to_block(to_block)
                .query()
                .await
                .map_err(FetchRewardError::ScanQueryFailed)?;

            if !init_logs.is_empty() {
                let (_, init_log) = init_logs[0].clone();
                tracing::info!(
                    from_block,
                    tx_hash = ?init_log.transaction_hash,
                    "Found token Initialized event during scan"
                );
                return Ok(init_log);
            }

            total_scanned += max_events_range;
            from_block = from_block.saturating_sub(max_events_range);
            to_block = to_block.saturating_sub(max_events_range);
        }
    }

    pub async fn fetch(&self, epoch: Epoch, header: Header) -> anyhow::Result<ValidatorMap> {
        let chain_config = self.get_chain_config(&header).await?;
        // update chain config
        *self.chain_config.lock().await = chain_config;

        let Some(address) = chain_config.stake_table_contract else {
            bail!("No stake table contract address found in Chain config");
        };

        let Some(l1_finalized_block_info) = header.l1_finalized() else {
            bail!("The epoch root for epoch {epoch} is missing the L1 finalized block info. This is a fatal error. Consensus is blocked and will not recover.");
        };

        let events = match self
            .fetch_and_store_stake_table_events(address, l1_finalized_block_info.number())
            .await
            .map_err(GetStakeTablesError::L1ClientFetchError)
        {
            Ok(events) => events,
            Err(e) => {
                bail!("failed to fetch stake table events {e:?}");
            },
        };

        match active_validator_set_from_l1_events(events.into_iter().map(|(_, e)| e)) {
            Ok(validators) => Ok(validators),
            Err(e) => {
                bail!("failed to construct stake table {e:?}");
            },
        }
    }

    /// Retrieve and verify `ChainConfig`
    // TODO move to appropriate object (Header?)
    pub(crate) async fn get_chain_config(&self, header: &Header) -> anyhow::Result<ChainConfig> {
        let chain_config = self.chain_config.lock().await;
        let peers = self.peers.clone();
        let header_cf = header.chain_config();
        if chain_config.commit() == header_cf.commit() {
            return Ok(*chain_config);
        }

        let cf = match header_cf.resolve() {
            Some(cf) => cf,
            None => peers
                .fetch_chain_config(header_cf.commit())
                .await
                .map_err(|err| {
                    tracing::error!("failed to get chain_config from peers. err: {err:?}");
                    err
                })?,
        };

        Ok(cf)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn mock() -> Self {
        use crate::{mock, v0_1::NoStorage};
        let chain_config = ChainConfig::default();
        let l1 = L1Client::new(vec!["http://localhost:3331".parse().unwrap()])
            .expect("Failed to create L1 client");

        let peers = Arc::new(mock::MockStateCatchup::default());
        let persistence = NoStorage;

        Self::new(peers, Arc::new(Mutex::new(persistence)), l1, chain_config)
    }
}

/// Holds Stake table and da stake
#[derive(Clone, Debug)]
struct NonEpochCommittee {
    /// The nodes eligible for leadership.
    /// NOTE: This is currently a hack because the DA leader needs to be the quorum
    /// leader but without voting rights.
    eligible_leaders: Vec<PeerConfig<SeqTypes>>,

    /// Keys for nodes participating in the network
    stake_table: Vec<PeerConfig<SeqTypes>>,

    /// Keys for DA members
    da_members: Vec<PeerConfig<SeqTypes>>,

    /// Stake entries indexed by public key, for efficient lookup.
    indexed_stake_table: HashMap<PubKey, PeerConfig<SeqTypes>>,

    /// DA entries indexed by public key, for efficient lookup.
    indexed_da_members: HashMap<PubKey, PeerConfig<SeqTypes>>,
}

/// Holds Stake table and da stake
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct EpochCommittee {
    /// The nodes eligible for leadership.
    /// NOTE: This is currently a hack because the DA leader needs to be the quorum
    /// leader but without voting rights.
    eligible_leaders: Vec<PeerConfig<SeqTypes>>,
    /// Keys for nodes participating in the network
    stake_table: IndexMap<PubKey, PeerConfig<SeqTypes>>,
    validators: ValidatorMap,
    address_mapping: HashMap<BLSPubKey, Address>,
}

impl EpochCommittees {
    pub fn first_epoch(&self) -> Option<Epoch> {
        self.first_epoch
    }

    pub fn fetcher(&self) -> &Fetcher {
        &self.fetcher
    }

    /// Updates `Self.stake_table` with stake_table for
    /// `Self.contract_address` at `l1_block_height`. This is intended
    /// to be called before calling `self.stake()` so that
    /// `Self.stake_table` only needs to be updated once in a given
    /// life-cycle but may be read from many times.
    fn update(
        &mut self,
        epoch: EpochNumber,
        validators: ValidatorMap,
        block_reward: Option<RewardAmount>,
    ) {
        let mut address_mapping = HashMap::new();
        let stake_table: IndexMap<PubKey, PeerConfig<SeqTypes>> = validators
            .values()
            .map(|v| {
                address_mapping.insert(v.stake_table_key, v.account);
                (
                    v.stake_table_key,
                    PeerConfig {
                        stake_table_entry: BLSPubKey::stake_table_entry(
                            &v.stake_table_key,
                            v.stake,
                        ),
                        state_ver_key: v.state_ver_key.clone(),
                    },
                )
            })
            .collect();

        let eligible_leaders: Vec<PeerConfig<SeqTypes>> =
            stake_table.iter().map(|(_, l)| l.clone()).collect();

        self.state.insert(
            epoch,
            EpochCommittee {
                eligible_leaders,
                stake_table,
                validators,
                address_mapping,
            },
        );

        if let Some(block_reward) = block_reward {
            self.block_reward = block_reward;
        }
    }

    pub fn validators(&self, epoch: &Epoch) -> anyhow::Result<ValidatorMap> {
        Ok(self
            .state
            .get(epoch)
            .context("state for found")?
            .validators
            .clone())
    }

    pub fn address(&self, epoch: &Epoch, bls_key: BLSPubKey) -> anyhow::Result<Address> {
        let mapping = self
            .state
            .get(epoch)
            .context("state for found")?
            .address_mapping
            .clone();

        Ok(*mapping.get(&bls_key).context(format!(
            "failed to get ethereum address for bls key {bls_key}. epoch={epoch}"
        ))?)
    }

    pub fn get_validator_config(
        &self,
        epoch: &Epoch,
        key: BLSPubKey,
    ) -> anyhow::Result<Validator<BLSPubKey>> {
        let address = self.address(epoch, key)?;
        let validators = self.validators(epoch)?;
        validators
            .get(&address)
            .context("validator not found")
            .cloned()
    }

    pub fn block_reward(&self) -> RewardAmount {
        self.block_reward
    }

    // We need a constructor to match our concrete type.
    pub fn new_stake(
        // TODO remove `new` from trait and rename this to `new`.
        // https://github.com/EspressoSystems/HotShot/commit/fcb7d54a4443e29d643b3bbc53761856aef4de8b
        committee_members: Vec<PeerConfig<SeqTypes>>,
        da_members: Vec<PeerConfig<SeqTypes>>,
        block_reward: RewardAmount,
        fetcher: Fetcher,
    ) -> Self {
        // For each member, get the stake table entry
        let stake_table: Vec<_> = committee_members
            .iter()
            .filter(|&peer_config| peer_config.stake_table_entry.stake() > U256::ZERO)
            .cloned()
            .collect();

        let eligible_leaders = stake_table.clone();
        // For each member, get the stake table entry
        let da_members: Vec<_> = da_members
            .iter()
            .filter(|&peer_config| peer_config.stake_table_entry.stake() > U256::ZERO)
            .cloned()
            .collect();

        // Index the stake table by public key
        let indexed_stake_table: HashMap<PubKey, _> = stake_table
            .iter()
            .map(|peer_config| {
                (
                    PubKey::public_key(&peer_config.stake_table_entry),
                    peer_config.clone(),
                )
            })
            .collect();

        // Index the stake table by public key
        let indexed_da_members: HashMap<PubKey, _> = da_members
            .iter()
            .map(|peer_config| {
                (
                    PubKey::public_key(&peer_config.stake_table_entry),
                    peer_config.clone(),
                )
            })
            .collect();

        let members = NonEpochCommittee {
            eligible_leaders,
            stake_table,
            da_members,
            indexed_stake_table,
            indexed_da_members,
        };

        let mut map = HashMap::new();
        let epoch_committee = EpochCommittee {
            eligible_leaders: members.eligible_leaders.clone(),
            stake_table: members
                .stake_table
                .iter()
                .map(|x| (PubKey::public_key(&x.stake_table_entry), x.clone()))
                .collect(),
            validators: Default::default(),
            address_mapping: HashMap::new(),
        };
        map.insert(Epoch::genesis(), epoch_committee.clone());
        // TODO: remove this, workaround for hotshot asking for stake tables from epoch 1
        map.insert(Epoch::genesis() + 1u64, epoch_committee.clone());

        Self {
            non_epoch_committee: members,
            state: map,
            randomized_committees: BTreeMap::new(),
            first_epoch: None,
            block_reward,
            fetcher: Arc::new(fetcher),
        }
    }

    pub async fn reload_stake(&mut self, limit: u64) {
        // Load the 50 latest stored stake tables

        let loaded_stake = match self
            .fetcher
            .persistence
            .lock()
            .await
            .load_latest_stake(limit)
            .await
        {
            Ok(Some(loaded)) => loaded,
            Ok(None) => {
                tracing::warn!("No stake table history found in persistence!");
                return;
            },
            Err(e) => {
                tracing::error!("Failed to load stake table history from persistence: {e}");
                return;
            },
        };

        for (epoch, stake_table) in loaded_stake {
            self.update(epoch, stake_table, None);
        }
    }

    fn get_stake_table(&self, epoch: &Option<Epoch>) -> Option<Vec<PeerConfig<SeqTypes>>> {
        if let Some(epoch) = epoch {
            self.state
                .get(epoch)
                .map(|committee| committee.stake_table.clone().into_values().collect())
        } else {
            Some(self.non_epoch_committee.stake_table.clone())
        }
    }
}

#[derive(Error, Debug)]
/// Error representing fail cases for retrieving the stake table.
enum GetStakeTablesError {
    #[error("Error fetching from L1: {0}")]
    L1ClientFetchError(anyhow::Error),
}

#[derive(Error, Debug)]
#[error("Could not lookup leader")] // TODO error variants? message?
pub struct LeaderLookupError;

// #[async_trait]
impl Membership<SeqTypes> for EpochCommittees {
    type Error = LeaderLookupError;
    // DO NOT USE. Dummy constructor to comply w/ trait.
    fn new(
        // TODO remove `new` from trait and remove this fn as well.
        // https://github.com/EspressoSystems/HotShot/commit/fcb7d54a4443e29d643b3bbc53761856aef4de8b
        _committee_members: Vec<PeerConfig<SeqTypes>>,
        _da_members: Vec<PeerConfig<SeqTypes>>,
    ) -> Self {
        panic!("This function has been replaced with new_stake()");
    }

    /// Get the stake table for the current view
    fn stake_table(&self, epoch: Option<Epoch>) -> HSStakeTable<SeqTypes> {
        self.get_stake_table(&epoch).unwrap_or_default().into()
    }
    /// Get the stake table for the current view
    fn da_stake_table(&self, _epoch: Option<Epoch>) -> HSStakeTable<SeqTypes> {
        self.non_epoch_committee.da_members.clone().into()
    }

    /// Get all members of the committee for the current view
    fn committee_members(
        &self,
        _view_number: <SeqTypes as NodeType>::View,
        epoch: Option<Epoch>,
    ) -> BTreeSet<PubKey> {
        let stake_table = self.stake_table(epoch);
        stake_table
            .iter()
            .map(|x| PubKey::public_key(&x.stake_table_entry))
            .collect()
    }

    /// Get all members of the committee for the current view
    fn da_committee_members(
        &self,
        _view_number: <SeqTypes as NodeType>::View,
        _epoch: Option<Epoch>,
    ) -> BTreeSet<PubKey> {
        self.non_epoch_committee
            .indexed_da_members
            .clone()
            .into_keys()
            .collect()
    }

    /// Get the stake table entry for a public key
    fn stake(&self, pub_key: &PubKey, epoch: Option<Epoch>) -> Option<PeerConfig<SeqTypes>> {
        // Only return the stake if it is above zero
        if let Some(epoch) = epoch {
            self.state
                .get(&epoch)
                .and_then(|h| h.stake_table.get(pub_key))
                .cloned()
        } else {
            self.non_epoch_committee
                .indexed_stake_table
                .get(pub_key)
                .cloned()
        }
    }

    /// Get the DA stake table entry for a public key
    fn da_stake(&self, pub_key: &PubKey, _epoch: Option<Epoch>) -> Option<PeerConfig<SeqTypes>> {
        // Only return the stake if it is above zero
        self.non_epoch_committee
            .indexed_da_members
            .get(pub_key)
            .cloned()
    }

    /// Check if a node has stake in the committee
    fn has_stake(&self, pub_key: &PubKey, epoch: Option<Epoch>) -> bool {
        self.stake(pub_key, epoch)
            .map(|x| x.stake_table_entry.stake() > U256::ZERO)
            .unwrap_or_default()
    }

    /// Check if a node has stake in the committee
    fn has_da_stake(&self, pub_key: &PubKey, epoch: Option<Epoch>) -> bool {
        self.da_stake(pub_key, epoch)
            .map(|x| x.stake_table_entry.stake() > U256::ZERO)
            .unwrap_or_default()
    }

    /// Returns the leader's public key for a given view number and epoch.
    ///
    /// If an epoch is provided and a randomized committee exists for that epoch,
    /// the leader is selected from the randomized committee. Otherwise, the leader
    /// is selected from the non-epoch committee.
    ///
    /// # Arguments
    /// * `view_number` - The view number to index into the committee.
    /// * `epoch` - The epoch for which to determine the leader. If `None`, uses the non-epoch committee.
    ///
    /// # Errors
    /// Returns `LeaderLookupError` if the epoch is before the first epoch or if the committee is missing.
    fn lookup_leader(
        &self,
        view_number: <SeqTypes as NodeType>::View,
        epoch: Option<Epoch>,
    ) -> Result<PubKey, Self::Error> {
        match (self.first_epoch(), epoch) {
            (Some(first_epoch), Some(epoch)) => {
                if epoch < first_epoch {
                    tracing::error!(
                        "lookup_leader called with epoch {} before first epoch {}",
                        epoch,
                        first_epoch,
                    );
                    return Err(LeaderLookupError);
                }
                let Some(randomized_committee) = self.randomized_committees.get(&epoch) else {
                    tracing::error!(
                        "We are missing the randomized committee for epoch {}",
                        epoch
                    );
                    return Err(LeaderLookupError);
                };

                Ok(PubKey::public_key(&select_randomized_leader(
                    randomized_committee,
                    *view_number,
                )))
            },
            (_, None) => {
                let leaders = &self.non_epoch_committee.eligible_leaders;

                let index = *view_number as usize % leaders.len();
                let res = leaders[index].clone();
                Ok(PubKey::public_key(&res.stake_table_entry))
            },
            (None, Some(epoch)) => {
                tracing::error!(
                    "lookup_leader called with epoch {} but we don't have a first epoch",
                    epoch,
                );
                Err(LeaderLookupError)
            },
        }
    }

    /// Get the total number of nodes in the committee
    fn total_nodes(&self, epoch: Option<Epoch>) -> usize {
        self.stake_table(epoch).len()
    }

    /// Get the total number of DA nodes in the committee
    fn da_total_nodes(&self, epoch: Option<Epoch>) -> usize {
        self.da_stake_table(epoch).len()
    }

    /// Get the voting success threshold for the committee
    fn success_threshold(&self, epoch: Option<Epoch>) -> U256 {
        let total_stake = self.total_stake(epoch);
        let one = U256::ONE;
        let two = U256::from(2);
        let three = U256::from(3);
        if total_stake < U256::MAX / two {
            ((total_stake * two) / three) + one
        } else {
            ((total_stake / three) * two) + two
        }
    }

    /// Get the voting success threshold for the committee
    fn da_success_threshold(&self, epoch: Option<Epoch>) -> U256 {
        let total_stake = self.total_da_stake(epoch);
        let one = U256::ONE;
        let two = U256::from(2);
        let three = U256::from(3);

        if total_stake < U256::MAX / two {
            ((total_stake * two) / three) + one
        } else {
            ((total_stake / three) * two) + two
        }
    }

    /// Get the voting failure threshold for the committee
    fn failure_threshold(&self, epoch: Option<Epoch>) -> U256 {
        let total_stake = self.total_stake(epoch);
        let one = U256::ONE;
        let three = U256::from(3);

        (total_stake / three) + one
    }

    /// Get the voting upgrade threshold for the committee
    fn upgrade_threshold(&self, epoch: Option<Epoch>) -> U256 {
        let total_stake = self.total_stake(epoch);
        let nine = U256::from(9);
        let ten = U256::from(10);

        let normal_threshold = self.success_threshold(epoch);
        let higher_threshold = if total_stake < U256::MAX / nine {
            (total_stake * nine) / ten
        } else {
            (total_stake / ten) * nine
        };

        max(higher_threshold, normal_threshold)
    }

    async fn add_epoch_root(
        membership: Arc<RwLock<Self>>,
        epoch: Epoch,
        block_header: Header,
    ) -> anyhow::Result<()> {
        let membership_reader = membership.read().await;
        if membership_reader.state.contains_key(&epoch) {
            tracing::info!(
                "We already have the stake table for epoch {}. Skipping L1 fetching.",
                epoch
            );
            return Ok(());
        }
        let fetcher = Arc::clone(&membership_reader.fetcher);
        drop(membership_reader);

        let stake_tables = fetcher.fetch(epoch, block_header).await?;

        let mut block_reward = None;

        {
            let membership_reader = membership.read().await;
            // Assumes the stake table contract proxy address does not change
            // In the future, if we want to support updates to the stake table contract address via chain config,
            // or allow the contract to handle additional block reward calculation parameters (e.g., inflation, block time),
            // the `fetch_block_reward` logic can be updated to support per-epoch rewards.
            // Initially, the block reward is zero if the node starts on pre-epoch version
            // but it is updated on the first call to `add_epoch_root()`
            if membership_reader.block_reward == RewardAmount(U256::ZERO) {
                block_reward = Some(fetcher.fetch_block_reward().await?);
            }
        }

        // Store stake table in persistence
        {
            let persistence_lock = fetcher.persistence.lock().await;
            if let Err(e) = persistence_lock
                .store_stake(epoch, stake_tables.clone())
                .await
            {
                tracing::error!(?e, "`add_epoch_root`, error storing stake table");
            }
        }

        let mut membership_writer = membership.write().await;
        membership_writer.update(epoch, stake_tables, block_reward);
        Ok(())
    }

    fn has_stake_table(&self, epoch: Epoch) -> bool {
        self.state.contains_key(&epoch)
    }

    /// Checks if the randomized stake table is available for the given epoch.
    ///
    /// Returns `Ok(true)` if a randomized committee exists for the specified epoch and
    /// the epoch is not before the first epoch. Returns an error if `first_epoch` is `None`
    /// or if the provided epoch is before the first epoch.
    ///
    /// # Arguments
    /// * `epoch` - The epoch for which to check the presence of a randomized stake table.
    ///
    /// # Errors
    /// Returns an error if `first_epoch` is `None` or if `epoch` is before `first_epoch`.
    fn has_randomized_stake_table(&self, epoch: Epoch) -> anyhow::Result<bool> {
        let Some(first_epoch) = self.first_epoch else {
            bail!(
                "Called has_randomized_stake_table with epoch {} but first_epoch is None",
                epoch
            );
        };
        ensure!(
            epoch >= first_epoch,
            "Called has_randomized_stake_table with epoch {} but first_epoch is {}",
            epoch,
            first_epoch
        );
        Ok(self.randomized_committees.contains_key(&epoch))
    }

    async fn get_epoch_root(
        membership: Arc<RwLock<Self>>,
        block_height: u64,
        epoch: Epoch,
    ) -> anyhow::Result<Leaf2> {
        let membership_reader = membership.read().await;
        let peers = membership_reader.fetcher.peers.clone();
        let stake_table = membership_reader.stake_table(Some(epoch)).clone();
        let success_threshold = membership_reader.success_threshold(Some(epoch));
        drop(membership_reader);

        // Fetch leaves from peers
        let leaf: Leaf2 = peers
            .fetch_leaf(block_height, stake_table.clone(), success_threshold)
            .await?;

        Ok(leaf)
    }

    async fn get_epoch_drb(
        membership: Arc<RwLock<Self>>,
        block_height: u64,
        epoch: Epoch,
    ) -> anyhow::Result<DrbResult> {
        let membership_reader = membership.read().await;
        let peers = membership_reader.fetcher.peers.clone();
        let stake_table = membership_reader.stake_table(Some(epoch)).clone();
        let success_threshold = membership_reader.success_threshold(Some(epoch));
        drop(membership_reader);

        tracing::debug!(
            "Getting DRB for epoch {}, block height {}",
            epoch,
            block_height
        );
        let drb_leaf = peers
            .try_fetch_leaf(1, block_height, stake_table, success_threshold)
            .await?;

        let Some(drb) = drb_leaf.next_drb_result else {
            tracing::error!(
          "We received a leaf that should contain a DRB result, but the DRB result is missing: {:?}",
          drb_leaf
        );

            bail!("DRB leaf is missing the DRB result.");
        };

        Ok(drb)
    }

    fn add_drb_result(&mut self, epoch: Epoch, drb: DrbResult) {
        let Some(raw_stake_table) = self.state.get(&epoch) else {
            tracing::error!("add_drb_result({epoch}, {drb:?}) was called, but we do not yet have the stake table for epoch {epoch}");
            return;
        };

        let leaders = raw_stake_table
            .eligible_leaders
            .clone()
            .into_iter()
            .map(|peer_config| peer_config.stake_table_entry)
            .collect::<Vec<_>>();
        let randomized_committee = generate_stake_cdf(leaders, drb);

        self.randomized_committees
            .insert(epoch, randomized_committee);
    }

    fn set_first_epoch(&mut self, epoch: Epoch, initial_drb_result: DrbResult) {
        self.first_epoch = Some(epoch);

        let epoch_committee = self.state.get(&Epoch::genesis()).unwrap().clone();
        self.state.insert(epoch, epoch_committee.clone());
        self.state.insert(epoch + 1, epoch_committee);
        self.add_drb_result(epoch, initial_drb_result);
        self.add_drb_result(epoch + 1, initial_drb_result);
    }

    fn first_epoch(&self) -> Option<<SeqTypes as NodeType>::Epoch> {
        self.first_epoch
    }
}

#[cfg(any(test, feature = "testing"))]
impl super::v0_3::StakeTable {
    /// Generate a `StakeTable` with `n` members.
    pub fn mock(n: u64) -> Self {
        [..n]
            .iter()
            .map(|_| PeerConfig::default())
            .collect::<Vec<PeerConfig<SeqTypes>>>()
            .into()
    }
}

#[cfg(any(test, feature = "testing"))]
impl DAMembers {
    /// Generate a `DaMembers` (alias committee) with `n` members.
    pub fn mock(n: u64) -> Self {
        [..n]
            .iter()
            .map(|_| PeerConfig::default())
            .collect::<Vec<PeerConfig<SeqTypes>>>()
            .into()
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use alloy::primitives::Bytes;
    use hotshot_contract_adapter::{
        sol_types::{EdOnBN254PointSol, G1PointSol, G2PointSol},
        stake_table::{sign_address_bls, sign_address_schnorr},
    };
    use hotshot_types::{light_client::StateKeyPair, signature_key::BLSKeyPair};
    use rand::{Rng as _, RngCore as _};

    use super::*;

    // TODO: current tests are just sanity checks, we need more.

    #[derive(Debug, Clone)]
    pub struct TestValidator {
        pub account: Address,
        pub bls_vk: G2PointSol,
        pub schnorr_vk: EdOnBN254PointSol,
        pub commission: u16,
        pub bls_sig: G1PointSol,
        pub schnorr_sig: Bytes,
    }

    impl TestValidator {
        pub fn random() -> Self {
            let account = Address::random();
            let commission = rand::thread_rng().gen_range(0..10000);
            Self::random_update_keys(account, commission)
        }

        pub fn randomize_keys(&self) -> Self {
            Self::random_update_keys(self.account, self.commission)
        }

        fn random_update_keys(account: Address, commission: u16) -> Self {
            let mut rng = &mut rand::thread_rng();
            let mut seed = [0u8; 32];
            rng.fill_bytes(&mut seed);
            let bls_key_pair = BLSKeyPair::generate(&mut rng);
            let bls_sig = sign_address_bls(&bls_key_pair, account);
            let schnorr_key_pair = StateKeyPair::generate_from_seed_indexed(seed, 0);
            let schnorr_sig = sign_address_schnorr(&schnorr_key_pair, account);
            Self {
                account,
                bls_vk: bls_key_pair.ver_key().to_affine().into(),
                schnorr_vk: schnorr_key_pair.ver_key().to_affine().into(),
                commission,
                bls_sig,
                schnorr_sig,
            }
        }
    }

    impl From<&TestValidator> for ValidatorRegistered {
        fn from(value: &TestValidator) -> Self {
            Self {
                account: value.account,
                blsVk: value.bls_vk,
                schnorrVk: value.schnorr_vk,
                commission: value.commission,
            }
        }
    }

    impl From<&TestValidator> for ValidatorRegisteredV2 {
        fn from(value: &TestValidator) -> Self {
            Self {
                account: value.account,
                blsVK: value.bls_vk,
                schnorrVK: value.schnorr_vk,
                commission: value.commission,
                blsSig: value.bls_sig.into(),
                schnorrSig: value.schnorr_sig.clone(),
            }
        }
    }

    impl From<&TestValidator> for ConsensusKeysUpdated {
        fn from(value: &TestValidator) -> Self {
            Self {
                account: value.account,
                blsVK: value.bls_vk,
                schnorrVK: value.schnorr_vk,
            }
        }
    }

    impl From<&TestValidator> for ConsensusKeysUpdatedV2 {
        fn from(value: &TestValidator) -> Self {
            Self {
                account: value.account,
                blsVK: value.bls_vk,
                schnorrVK: value.schnorr_vk,
                blsSig: value.bls_sig.into(),
                schnorrSig: value.schnorr_sig.clone(),
            }
        }
    }

    impl From<&TestValidator> for ValidatorExit {
        fn from(value: &TestValidator) -> Self {
            Self {
                validator: value.account,
            }
        }
    }

    impl Validator<BLSPubKey> {
        pub fn mock() -> Validator<BLSPubKey> {
            let val = TestValidator::random();
            let rng = &mut rand::thread_rng();
            let mut seed = [1u8; 32];
            rng.fill_bytes(&mut seed);
            let mut validator_stake = alloy::primitives::U256::from(0);
            let mut delegators = HashMap::new();
            for _i in 0..=5000 {
                let stake: u64 = rng.gen_range(0..10000);
                delegators.insert(Address::random(), alloy::primitives::U256::from(stake));
                validator_stake += alloy::primitives::U256::from(stake);
            }

            let stake_table_key = val.bls_vk.into();
            let state_ver_key = val.schnorr_vk.into();

            Validator {
                account: val.account,
                stake_table_key,
                state_ver_key,
                stake: validator_stake,
                commission: val.commission,
                delegators,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::{primitives::Address, rpc::types::Log};
    use hotshot_contract_adapter::stake_table::StakeTableContractVersion;
    use pretty_assertions::assert_matches;
    use rstest::rstest;
    use sequencer_utils::test_utils::setup_test;

    use super::*;
    use crate::v0::impls::testing::*;

    #[test]
    fn test_from_l1_events() -> anyhow::Result<()> {
        setup_test();
        // Build a stake table with one DA node and one consensus node.
        let val_1 = TestValidator::random();
        let val_1_new_keys = val_1.randomize_keys();
        let val_2 = TestValidator::random();
        let val_2_new_keys = val_2.randomize_keys();
        let delegator = Address::random();
        let mut events: Vec<StakeTableEvent> = [
            ValidatorRegistered::from(&val_1).into(),
            ValidatorRegisteredV2::from(&val_2).into(),
            Delegated {
                delegator,
                validator: val_1.account,
                amount: U256::from(10),
            }
            .into(),
            ConsensusKeysUpdated::from(&val_1_new_keys).into(),
            ConsensusKeysUpdatedV2::from(&val_2_new_keys).into(),
            Undelegated {
                delegator,
                validator: val_1.account,
                amount: U256::from(7),
            }
            .into(),
            // delegate to the same validator again
            Delegated {
                delegator,
                validator: val_1.account,
                amount: U256::from(5),
            }
            .into(),
            // delegate to the second validator
            Delegated {
                delegator: Address::random(),
                validator: val_2.account,
                amount: U256::from(3),
            }
            .into(),
        ]
        .to_vec();

        let st = active_validator_set_from_l1_events(events.iter().cloned())?;
        let st_val_1 = st.get(&val_1.account).unwrap();
        // final staked amount should be 10 (delegated) - 7 (undelegated) + 5 (Delegated)
        assert_eq!(st_val_1.stake, U256::from(8));
        assert_eq!(st_val_1.commission, val_1.commission);
        assert_eq!(st_val_1.delegators.len(), 1);
        // final delegated amount should be 10 (delegated) - 7 (undelegated) + 5 (Delegated)
        assert_eq!(*st_val_1.delegators.get(&delegator).unwrap(), U256::from(8));

        let st_val_2 = st.get(&val_2.account).unwrap();
        assert_eq!(st_val_2.stake, U256::from(3));
        assert_eq!(st_val_2.commission, val_2.commission);
        assert_eq!(st_val_2.delegators.len(), 1);

        events.push(ValidatorExit::from(&val_1).into());

        let st = active_validator_set_from_l1_events(events.iter().cloned())?;
        // The first validator should have been removed
        assert_eq!(st.get(&val_1.account), None);

        // The second validator should be unchanged
        let st_val_2 = st.get(&val_2.account).unwrap();
        assert_eq!(st_val_2.stake, U256::from(3));
        assert_eq!(st_val_2.commission, val_2.commission);
        assert_eq!(st_val_2.delegators.len(), 1);

        // remove the 2nd validator
        events.push(ValidatorExit::from(&val_2).into());

        // This should fail because the validator has exited and no longer exists in the stake table.
        assert!(active_validator_set_from_l1_events(events.iter().cloned()).is_err());

        Ok(())
    }

    #[test]
    fn test_from_l1_events_failures() -> anyhow::Result<()> {
        let val = TestValidator::random();
        let delegator = Address::random();

        let register: StakeTableEvent = ValidatorRegistered::from(&val).into();
        let register_v2: StakeTableEvent = ValidatorRegisteredV2::from(&val).into();
        let delegate: StakeTableEvent = Delegated {
            delegator,
            validator: val.account,
            amount: U256::from(10),
        }
        .into();
        let key_update: StakeTableEvent = ConsensusKeysUpdated::from(&val).into();
        let key_update_v2: StakeTableEvent = ConsensusKeysUpdatedV2::from(&val).into();
        let undelegate: StakeTableEvent = Undelegated {
            delegator,
            validator: val.account,
            amount: U256::from(7),
        }
        .into();

        let exit: StakeTableEvent = ValidatorExit::from(&val).into();

        let cases = [
            vec![exit],
            vec![undelegate.clone()],
            vec![delegate.clone()],
            vec![key_update],
            vec![key_update_v2],
            vec![register.clone(), register.clone()],
            vec![register_v2.clone(), register_v2.clone()],
            vec![register.clone(), register_v2.clone()],
            vec![register_v2.clone(), register.clone()],
            vec![
                register,
                delegate.clone(),
                undelegate.clone(),
                undelegate.clone(),
            ],
            vec![register_v2, delegate, undelegate.clone(), undelegate],
        ];

        for events in cases.iter() {
            // NOTE: not selecting the active validator set because we care about wrong sequences of
            // events being detected. If we compute the active set we will also get an error if the
            // set is empty but that's not what we want to test here.
            let res = validators_from_l1_events(events.iter().cloned());
            assert!(
                res.is_err(),
                "events {res:?}, not a valid sequence of events"
            );
        }
        Ok(())
    }

    #[test]
    fn test_validators_selection() {
        let mut validators = IndexMap::new();
        let mut highest_stake = alloy::primitives::U256::ZERO;

        for _i in 0..3000 {
            let validator = Validator::mock();
            validators.insert(validator.account, validator.clone());

            if validator.stake > highest_stake {
                highest_stake = validator.stake;
            }
        }

        let minimum_stake = highest_stake / U256::from(VID_TARGET_TOTAL_STAKE);

        select_active_validator_set(&mut validators).expect("Failed to select validators");
        assert!(
            validators.len() <= 100,
            "validators len is {}, expected at most 100",
            validators.len()
        );

        let mut selected_validators_highest_stake = alloy::primitives::U256::ZERO;
        // Ensure every validator in the final selection is above or equal to minimum stake
        for (address, validator) in &validators {
            assert!(
                validator.stake >= minimum_stake,
                "Validator {:?} has stake below minimum: {}",
                address,
                validator.stake
            );

            if validator.stake > selected_validators_highest_stake {
                selected_validators_highest_stake = validator.stake;
            }
        }
    }

    // For a bug where the GCL did not match the stake table contract implementation and allowed
    // duplicated BLS keys via the update keys events.
    #[rstest::rstest]
    fn test_regression_non_unique_bls_keys_not_discarded(
        #[values(StakeTableContractVersion::V1, StakeTableContractVersion::V2)]
        version: StakeTableContractVersion,
    ) {
        let val = TestValidator::random();
        let register: StakeTableEvent = match version {
            StakeTableContractVersion::V1 => ValidatorRegistered::from(&val).into(),
            StakeTableContractVersion::V2 => ValidatorRegisteredV2::from(&val).into(),
        };
        let delegate: StakeTableEvent = Delegated {
            delegator: Address::random(),
            validator: val.account,
            amount: U256::from(10),
        }
        .into();

        // first ensure that wan build a valid stake table
        assert!(active_validator_set_from_l1_events(
            vec![register.clone(), delegate.clone()].into_iter()
        )
        .is_ok());

        // add the invalid key update (re-using the same consensus keys)
        let key_update = ConsensusKeysUpdated::from(&val).into();
        let err =
            active_validator_set_from_l1_events(vec![register, delegate, key_update].into_iter())
                .unwrap_err();

        let bls: BLSPubKey = val.bls_vk.into();
        assert!(matches!(err, StakeTableError::BlsKeyAlreadyUsed(addr) if addr == bls.to_string()));
    }

    #[test]
    fn test_display_log() {
        let serialized = r#"{"address":"0x0000000000000000000000000000000000000069","topics":["0x0000000000000000000000000000000000000000000000000000000000000069"],"data":"0x69","blockHash":"0x0000000000000000000000000000000000000000000000000000000000000069","blockNumber":"0x69","blockTimestamp":"0x69","transactionHash":"0x0000000000000000000000000000000000000000000000000000000000000069","transactionIndex":"0x69","logIndex":"0x70","removed":false}"#;
        let log: Log = serde_json::from_str(serialized).unwrap();
        assert_eq!(
            log.display(),
            "Log(block=105,index=112,transaction_hash=0x0000000000000000000000000000000000000000000000000000000000000069)"
        )
    }

    #[rstest]
    #[case::v1(StakeTableContractVersion::V1)]
    #[case::v2(StakeTableContractVersion::V2)]
    fn test_register_validator(#[case] version: StakeTableContractVersion) {
        let mut state = StakeTableState::new();
        let validator = TestValidator::random();

        let event = match version {
            StakeTableContractVersion::V1 => StakeTableEvent::Register((&validator).into()),
            StakeTableContractVersion::V2 => StakeTableEvent::RegisterV2((&validator).into()),
        };

        assert!(state.apply_event(event).unwrap().is_ok());

        let stored = state.validators.get(&validator.account).unwrap();
        assert_eq!(stored.account, validator.account);
    }

    #[rstest]
    #[case::v1(StakeTableContractVersion::V1)]
    #[case::v2(StakeTableContractVersion::V2)]
    fn test_validator_already_registered(#[case] version: StakeTableContractVersion) {
        let mut stake_table_state = StakeTableState::new();

        let test_validator = TestValidator::random();

        // First registration attempt using the specified contract version
        let first_registration_result =
            match version {
                StakeTableContractVersion::V1 => stake_table_state
                    .apply_event(StakeTableEvent::Register((&test_validator).into())),
                StakeTableContractVersion::V2 => stake_table_state
                    .apply_event(StakeTableEvent::RegisterV2((&test_validator).into())),
            };

        // Expect the first registration to succeed
        assert!(first_registration_result.unwrap().is_ok());

        // attempt using V1 registration (should fail with AlreadyRegistered)
        let v1_already_registered_result =
            stake_table_state.apply_event(StakeTableEvent::Register((&test_validator).into()));

        pretty_assertions::assert_matches!(
           v1_already_registered_result,  Err(StakeTableError::AlreadyRegistered(account)) if account == test_validator.account,
           "Expected AlreadyRegistered error. version ={version:?} result={v1_already_registered_result:?}",
        );

        // attempt using V2 registration (should also fail with AlreadyRegistered)
        let v2_already_registered_result =
            stake_table_state.apply_event(StakeTableEvent::RegisterV2((&test_validator).into()));

        pretty_assertions::assert_matches!(
            v2_already_registered_result,
            Err(StakeTableError::AlreadyRegistered(account)) if account == test_validator.account,
            "Expected AlreadyRegistered error. version ={version:?} result={v2_already_registered_result:?}",

        );
    }

    #[test]
    fn test_register_validator_v2_auth_fails() {
        let mut state = StakeTableState::new();
        let mut val = TestValidator::random();
        val.bls_sig = Default::default();
        let event = StakeTableEvent::RegisterV2((&val).into());

        let result = state.apply_event(event);
        assert!(matches!(
            result,
            Err(StakeTableError::AuthenticationFailed(_))
        ));
    }

    #[test]
    fn test_deregister_validator() {
        let mut state = StakeTableState::new();
        let val = TestValidator::random();

        let reg = StakeTableEvent::Register((&val).into());
        state.apply_event(reg).unwrap().unwrap();

        let dereg = StakeTableEvent::Deregister((&val).into());
        assert!(state.apply_event(dereg).unwrap().is_ok());
        assert!(!state.validators.contains_key(&val.account));
    }

    #[test]
    fn test_delegate_and_undelegate() {
        let mut state = StakeTableState::new();
        let val = TestValidator::random();
        state
            .apply_event(StakeTableEvent::Register((&val).into()))
            .unwrap()
            .unwrap();

        let delegator = Address::random();
        let amount = U256::from(1000);
        let delegate_event = StakeTableEvent::Delegate(Delegated {
            delegator,
            validator: val.account,
            amount,
        });
        assert!(state.apply_event(delegate_event).unwrap().is_ok());

        let validator = state.validators.get(&val.account).unwrap();
        assert_eq!(validator.delegators.get(&delegator).cloned(), Some(amount));

        let undelegate_event = StakeTableEvent::Undelegate(Undelegated {
            delegator,
            validator: val.account,
            amount,
        });
        assert!(state.apply_event(undelegate_event).unwrap().is_ok());
        let validator = state.validators.get(&val.account).unwrap();
        assert!(!validator.delegators.contains_key(&delegator));
    }

    #[rstest]
    #[case::v1(StakeTableContractVersion::V1)]
    #[case::v2(StakeTableContractVersion::V2)]
    fn test_key_update_event(#[case] version: StakeTableContractVersion) {
        let mut state = StakeTableState::new();
        let val = TestValidator::random();

        // Always register first using V1 to simulate upgrade scenarios
        state
            .apply_event(StakeTableEvent::Register((&val).into()))
            .unwrap()
            .unwrap();

        let new_keys = val.randomize_keys();

        let event = match version {
            StakeTableContractVersion::V1 => StakeTableEvent::KeyUpdate((&new_keys).into()),
            StakeTableContractVersion::V2 => StakeTableEvent::KeyUpdateV2((&new_keys).into()),
        };

        assert!(state.apply_event(event).unwrap().is_ok());

        let updated = state.validators.get(&val.account).unwrap();
        assert_eq!(updated.stake_table_key, new_keys.bls_vk.into());
        assert_eq!(updated.state_ver_key, new_keys.schnorr_vk.into());
    }

    #[test]
    fn test_duplicate_bls_key() {
        let mut state = StakeTableState::new();
        let val = TestValidator::random();
        let event1 = StakeTableEvent::Register((&val).into());
        let mut val2 = TestValidator::random();
        val2.bls_vk = val.bls_vk;
        val2.account = Address::random();

        let event2 = StakeTableEvent::Register((&val2).into());
        assert!(state.apply_event(event1).unwrap().is_ok());
        let result = state.apply_event(event2);

        let expected_bls_key = BLSPubKey::from(val.bls_vk).to_string();

        assert_matches!(
            result,
            Err(StakeTableError::BlsKeyAlreadyUsed(key))
                if key == expected_bls_key,
            "Expected BlsKeyAlreadyUsed({expected_bls_key}), but got: {result:?}",
        );
    }

    #[test]
    fn test_duplicate_schnorr_key() {
        let mut state = StakeTableState::new();
        let val = TestValidator::random();
        let event1 = StakeTableEvent::Register((&val).into());
        let mut val2 = TestValidator::random();
        val2.schnorr_vk = val.schnorr_vk;
        val2.account = Address::random();
        val2.bls_vk = val2.randomize_keys().bls_vk;

        let event2 = StakeTableEvent::Register((&val2).into());
        assert!(state.apply_event(event1).unwrap().is_ok());
        let result = state.apply_event(event2);

        let schnorr: SchnorrPubKey = val.schnorr_vk.into();
        assert_matches!(
            result,
            Ok(Err(ExpectedStakeTableError::SchnorrKeyAlreadyUsed(key)))
                if key == schnorr.to_string(),
            "Expected SchnorrKeyAlreadyUsed({schnorr}), but got: {result:?}",

        );
    }

    #[test]
    fn test_register_and_deregister_validator() {
        let mut state = StakeTableState::new();
        let validator = TestValidator::random();
        let event = StakeTableEvent::Register((&validator).into());
        assert!(state.apply_event(event).unwrap().is_ok());

        let deregister_event = StakeTableEvent::Deregister((&validator).into());
        assert!(state.apply_event(deregister_event).unwrap().is_ok());
    }

    #[test]
    fn test_delegate_zero_amount_is_rejected() {
        let mut state = StakeTableState::new();
        let validator = TestValidator::random();
        let account = validator.account;
        state
            .apply_event(StakeTableEvent::Register((&validator).into()))
            .unwrap()
            .unwrap();

        let delegator = Address::random();
        let amount = U256::ZERO;
        let event = StakeTableEvent::Delegate(Delegated {
            delegator,
            validator: account,
            amount,
        });
        let result = state.apply_event(event);

        assert_matches!(
            result,
            Err(StakeTableError::ZeroDelegatorStake(addr))
                if addr == delegator,
            "delegator stake is zero"

        );
    }

    #[test]
    fn test_undelegate_more_than_stake_fails() {
        let mut state = StakeTableState::new();
        let validator = TestValidator::random();
        let account = validator.account;
        state
            .apply_event(StakeTableEvent::Register((&validator).into()))
            .unwrap()
            .unwrap();

        let delegator = Address::random();
        let event = StakeTableEvent::Delegate(Delegated {
            delegator,
            validator: account,
            amount: U256::from(10u64),
        });
        state.apply_event(event).unwrap().unwrap();

        let result = state.apply_event(StakeTableEvent::Undelegate(Undelegated {
            delegator,
            validator: account,
            amount: U256::from(20u64),
        }));
        assert_matches!(
            result,
            Err(StakeTableError::InsufficientStake),
            "Expected InsufficientStake error, got: {result:?}",
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_decaf_stake_table() {
        setup_test();

        // The following commented-out block demonstrates how the `decaf_stake_table_events.json`
        // and `decaf_stake_table.json` files were originally generated.

        // It generates decaf stake table data by fetching events from the contract,
        // writes events and the constructed stake table to JSON files.

        /*
        let l1 = L1Client::new(vec!["https://ethereum-sepolia.publicnode.com"
            .parse()
            .unwrap()])
        .unwrap();
        let contract_address = "0x40304fbe94d5e7d1492dd90c53a2d63e8506a037";

        let events = Fetcher::fetch_events_from_contract(
            l1,
            contract_address.parse().unwrap(),
            None,
            8582328,
        )
        .await;

        let sorted_events = events.sort_events().expect("failed to sort");

        // Serialize and write sorted events
        let json_events = serde_json::to_string_pretty(&sorted_events)?;
        let mut events_file = File::create("decaf_stake_table_events.json").await?;
        events_file.write_all(json_events.as_bytes()).await?;

        // Process into stake table
        let stake_table = validators_from_l1_events(sorted_events.into_iter().map(|(_, e)| e))?;

        // Serialize and write stake table
        let json_stake_table = serde_json::to_string_pretty(&stake_table)?;
        let mut stake_file = File::create("decaf_stake_table.json").await?;
        stake_file.write_all(json_stake_table.as_bytes()).await?;
        */

        let events_json =
            std::fs::read_to_string("../data/v3/decaf_stake_table_events.json").unwrap();
        let events: Vec<(EventKey, StakeTableEvent)> = serde_json::from_str(&events_json).unwrap();

        // Reconstruct stake table from events
        let reconstructed_stake_table =
            active_validator_set_from_l1_events(events.into_iter().map(|(_, e)| e)).unwrap();

        let stake_table_json =
            std::fs::read_to_string("../data/v3/decaf_stake_table.json").unwrap();
        let expected: IndexMap<Address, Validator<BLSPubKey>> =
            serde_json::from_str(&stake_table_json).unwrap();

        assert_eq!(
            reconstructed_stake_table, expected,
            "Stake table reconstructed from events does not match the expected stake table "
        );
    }
}
