use crate::error::TransactionError;
use ckb_core::transaction::{Capacity, CellOutput, Transaction, TX_VERSION};
use ckb_core::{
    cell::{CellMeta, ResolvedOutPoint, ResolvedTransaction},
    BlockNumber, Cycle, EpochNumber,
};
use ckb_script::{ScriptConfig, TransactionScriptsVerifier};
use ckb_store::ChainStore;
use ckb_traits::BlockMedianTimeContext;
use lru_cache::LruCache;
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;

pub struct ContextualTransactionVerifier<'a, M> {
    pub maturity: MaturityVerifier<'a>,
    pub since: SinceVerifier<'a, M>,
}
impl<'a, M> ContextualTransactionVerifier<'a, M>
where
    M: BlockMedianTimeContext,
{
    pub fn new(
        rtx: &'a ResolvedTransaction,
        median_time_context: &'a M,
        tip_number: BlockNumber,
        tip_epoch_number: BlockNumber,
        cellbase_maturity: BlockNumber,
    ) -> Self {
        ContextualTransactionVerifier {
            maturity: MaturityVerifier::new(&rtx, tip_number, cellbase_maturity),
            since: SinceVerifier::new(rtx, median_time_context, tip_number, tip_epoch_number),
        }
    }

    pub fn verify(&self) -> Result<(), TransactionError> {
        self.maturity.verify()?;
        self.since.verify()?;
        Ok(())
    }
}

pub struct TransactionVerifier<'a, M, CS> {
    pub version: VersionVerifier<'a>,
    pub empty: EmptyVerifier<'a>,
    pub maturity: MaturityVerifier<'a>,
    pub capacity: CapacityVerifier<'a>,
    pub duplicate_deps: DuplicateDepsVerifier<'a>,
    pub script: ScriptVerifier<'a, CS>,
    pub since: SinceVerifier<'a, M>,
}

impl<'a, M, CS: ChainStore> TransactionVerifier<'a, M, CS>
where
    M: BlockMedianTimeContext,
{
    pub fn new(
        rtx: &'a ResolvedTransaction,
        store: Arc<CS>,
        median_time_context: &'a M,
        tip_number: BlockNumber,
        tip_epoch_number: BlockNumber,
        cellbase_maturity: BlockNumber,
        script_config: &'a ScriptConfig,
    ) -> Self {
        TransactionVerifier {
            version: VersionVerifier::new(&rtx.transaction),
            empty: EmptyVerifier::new(&rtx.transaction),
            maturity: MaturityVerifier::new(&rtx, tip_number, cellbase_maturity),
            duplicate_deps: DuplicateDepsVerifier::new(&rtx.transaction),
            script: ScriptVerifier::new(rtx, Arc::clone(&store), script_config),
            capacity: CapacityVerifier::new(rtx),
            since: SinceVerifier::new(rtx, median_time_context, tip_number, tip_epoch_number),
        }
    }

    pub fn verify(&self, max_cycles: Cycle) -> Result<Cycle, TransactionError> {
        self.version.verify()?;
        self.empty.verify()?;
        self.maturity.verify()?;
        self.capacity.verify()?;
        self.duplicate_deps.verify()?;
        self.since.verify()?;
        let cycles = self.script.verify(max_cycles)?;
        Ok(cycles)
    }
}

pub struct VersionVerifier<'a> {
    transaction: &'a Transaction,
}

impl<'a> VersionVerifier<'a> {
    pub fn new(transaction: &'a Transaction) -> Self {
        VersionVerifier { transaction }
    }

    pub fn verify(&self) -> Result<(), TransactionError> {
        if self.transaction.version() != TX_VERSION {
            return Err(TransactionError::Version);
        }
        Ok(())
    }
}

pub struct ScriptVerifier<'a, CS> {
    store: Arc<CS>,
    resolved_transaction: &'a ResolvedTransaction<'a>,
    script_config: &'a ScriptConfig,
}

impl<'a, CS: ChainStore> ScriptVerifier<'a, CS> {
    pub fn new(
        resolved_transaction: &'a ResolvedTransaction,
        store: Arc<CS>,
        script_config: &'a ScriptConfig,
    ) -> Self {
        ScriptVerifier {
            store,
            resolved_transaction,
            script_config,
        }
    }

    pub fn verify(&self, max_cycles: Cycle) -> Result<Cycle, TransactionError> {
        TransactionScriptsVerifier::new(
            &self.resolved_transaction,
            Arc::clone(&self.store),
            &self.script_config,
        )
        .verify(max_cycles)
        .map_err(TransactionError::ScriptFailure)
    }
}

pub struct EmptyVerifier<'a> {
    transaction: &'a Transaction,
}

impl<'a> EmptyVerifier<'a> {
    pub fn new(transaction: &'a Transaction) -> Self {
        EmptyVerifier { transaction }
    }

    pub fn verify(&self) -> Result<(), TransactionError> {
        if self.transaction.is_empty() {
            Err(TransactionError::Empty)
        } else {
            Ok(())
        }
    }
}

pub struct MaturityVerifier<'a> {
    transaction: &'a ResolvedTransaction<'a>,
    tip_number: BlockNumber,
    cellbase_maturity: BlockNumber,
}

impl<'a> MaturityVerifier<'a> {
    pub fn new(
        transaction: &'a ResolvedTransaction,
        tip_number: BlockNumber,
        cellbase_maturity: BlockNumber,
    ) -> Self {
        MaturityVerifier {
            transaction,
            tip_number,
            cellbase_maturity,
        }
    }

    pub fn verify(&self) -> Result<(), TransactionError> {
        let cellbase_immature = |meta: &CellMeta| -> bool {
            meta.is_cellbase()
                && self.tip_number
                    < meta
                        .block_info
                        .as_ref()
                        .expect("cell meta should have block number when transaction verify")
                        .number
                        + self.cellbase_maturity
        };

        let input_immature_spend = || {
            self.transaction
                .resolved_inputs
                .iter()
                .filter_map(ResolvedOutPoint::cell)
                .any(cellbase_immature)
        };
        let dep_immature_spend = || {
            self.transaction
                .resolved_deps
                .iter()
                .filter_map(ResolvedOutPoint::cell)
                .any(cellbase_immature)
        };

        if input_immature_spend() || dep_immature_spend() {
            Err(TransactionError::CellbaseImmaturity)
        } else {
            Ok(())
        }
    }
}

pub struct DuplicateDepsVerifier<'a> {
    transaction: &'a Transaction,
}

impl<'a> DuplicateDepsVerifier<'a> {
    pub fn new(transaction: &'a Transaction) -> Self {
        DuplicateDepsVerifier { transaction }
    }

    pub fn verify(&self) -> Result<(), TransactionError> {
        let transaction = self.transaction;
        let mut seen = HashSet::with_capacity(self.transaction.deps().len());

        if transaction.deps().iter().all(|id| seen.insert(id)) {
            Ok(())
        } else {
            Err(TransactionError::DuplicateDeps)
        }
    }
}

pub struct CapacityVerifier<'a> {
    resolved_transaction: &'a ResolvedTransaction<'a>,
}

impl<'a> CapacityVerifier<'a> {
    pub fn new(resolved_transaction: &'a ResolvedTransaction) -> Self {
        CapacityVerifier {
            resolved_transaction,
        }
    }

    pub fn verify(&self) -> Result<(), TransactionError> {
        // skip OutputsSumOverflow verification for resolved cellbase and DAO
        // withdraw transactions.
        // cellbase's outputs are verified by TransactionsVerifier#InvalidReward
        // DAO withdraw transaction is verified in TransactionScriptsVerifier
        if !(self.resolved_transaction.is_cellbase()
            || self
                .resolved_transaction
                .transaction
                .is_withdrawing_from_dao())
        {
            let inputs_total = self.resolved_transaction.resolved_inputs.iter().try_fold(
                Capacity::zero(),
                |acc, resolved_out_point| {
                    let capacity = resolved_out_point
                        .cell()
                        .map(|cell_meta| cell_meta.capacity)
                        .unwrap_or_else(Capacity::zero);
                    acc.safe_add(capacity)
                },
            )?;

            let outputs_total = self
                .resolved_transaction
                .transaction
                .outputs()
                .iter()
                .try_fold(Capacity::zero(), |acc, output| {
                    acc.safe_add(output.capacity)
                })?;

            if inputs_total < outputs_total {
                return Err(TransactionError::OutputsSumOverflow);
            }
        }

        if self
            .resolved_transaction
            .transaction
            .outputs()
            .iter()
            .any(CellOutput::is_occupied_capacity_overflow)
        {
            return Err(TransactionError::CapacityOverflow);
        }

        Ok(())
    }
}

const LOCK_TYPE_FLAG: u64 = 1 << 63;
const METRIC_TYPE_FLAG_MASK: u64 = 0x6000_0000_0000_0000;
const VALUE_MASK: u64 = 0x00ff_ffff_ffff_ffff;
const REMAIN_FLAGS_BITS: u64 = 0x1f00_0000_0000_0000;

enum SinceMetric {
    BlockNumber(u64),
    EpochNumber(u64),
    Timestamp(u64),
}

/// RFC 0017
#[derive(Copy, Clone, Debug)]
struct Since(u64);

impl Since {
    pub fn is_absolute(self) -> bool {
        self.0 & LOCK_TYPE_FLAG == 0
    }

    #[inline]
    pub fn is_relative(self) -> bool {
        !self.is_absolute()
    }

    pub fn flags_is_valid(self) -> bool {
        (self.0 & REMAIN_FLAGS_BITS == 0)
            && ((self.0 & METRIC_TYPE_FLAG_MASK) != (0b0110_0000 << 56))
    }

    fn extract_metric(self) -> Option<SinceMetric> {
        let value = self.0 & VALUE_MASK;
        match self.0 & METRIC_TYPE_FLAG_MASK {
            //0b0000_0000
            0x0000_0000_0000_0000 => Some(SinceMetric::BlockNumber(value)),
            //0b0010_0000
            0x2000_0000_0000_0000 => Some(SinceMetric::EpochNumber(value)),
            //0b0100_0000
            0x4000_0000_0000_0000 => Some(SinceMetric::Timestamp(value * 1000)),
            _ => None,
        }
    }
}

/// https://github.com/nervosnetwork/rfcs/blob/master/rfcs/0017-tx-valid-since/0017-tx-valid-since.md#detailed-specification
pub struct SinceVerifier<'a, M> {
    rtx: &'a ResolvedTransaction<'a>,
    block_median_time_context: &'a M,
    tip_number: BlockNumber,
    tip_epoch_number: EpochNumber,
    median_timestamps_cache: RefCell<LruCache<BlockNumber, Option<u64>>>,
}

impl<'a, M> SinceVerifier<'a, M>
where
    M: BlockMedianTimeContext,
{
    pub fn new(
        rtx: &'a ResolvedTransaction,
        block_median_time_context: &'a M,
        tip_number: BlockNumber,
        tip_epoch_number: BlockNumber,
    ) -> Self {
        let median_timestamps_cache = RefCell::new(LruCache::new(rtx.resolved_inputs.len()));
        SinceVerifier {
            rtx,
            block_median_time_context,
            tip_number,
            tip_epoch_number,
            median_timestamps_cache,
        }
    }

    fn block_median_time(&self, n: BlockNumber) -> Option<u64> {
        let result = self.median_timestamps_cache.borrow().get(&n).cloned();
        match result {
            Some(r) => r,
            None => {
                let timestamp = self.block_median_time_context.block_median_time(n);
                self.median_timestamps_cache
                    .borrow_mut()
                    .insert(n, timestamp);
                timestamp
            }
        }
    }

    fn verify_absolute_lock(&self, since: Since) -> Result<(), TransactionError> {
        if since.is_absolute() {
            match since.extract_metric() {
                Some(SinceMetric::BlockNumber(block_number)) => {
                    if self.tip_number < block_number {
                        return Err(TransactionError::Immature);
                    }
                }
                Some(SinceMetric::EpochNumber(epoch_number)) => {
                    if self.tip_epoch_number < epoch_number {
                        return Err(TransactionError::Immature);
                    }
                }
                Some(SinceMetric::Timestamp(timestamp)) => {
                    let tip_timestamp = self
                        .block_median_time(self.tip_number.saturating_sub(1))
                        .unwrap_or_else(|| 0);
                    if tip_timestamp < timestamp {
                        return Err(TransactionError::Immature);
                    }
                }
                None => {
                    return Err(TransactionError::InvalidSince);
                }
            }
        }
        Ok(())
    }

    fn verify_relative_lock(
        &self,
        since: Since,
        cell_meta: &CellMeta,
    ) -> Result<(), TransactionError> {
        if since.is_relative() {
            // cell still in tx_pool
            let (cell_block_number, cell_epoch_number) = match cell_meta.block_info {
                Some(ref block_info) => (block_info.number, block_info.epoch),
                None => return Err(TransactionError::Immature),
            };
            match since.extract_metric() {
                Some(SinceMetric::BlockNumber(block_number)) => {
                    if self.tip_number < cell_block_number + block_number {
                        return Err(TransactionError::Immature);
                    }
                }
                Some(SinceMetric::EpochNumber(epoch_number)) => {
                    if self.tip_epoch_number < cell_epoch_number + epoch_number {
                        return Err(TransactionError::Immature);
                    }
                }
                Some(SinceMetric::Timestamp(timestamp)) => {
                    let tip_timestamp = self
                        .block_median_time(self.tip_number.saturating_sub(1))
                        .unwrap_or_else(|| 0);
                    let median_timestamp = self
                        .block_median_time(cell_block_number.saturating_sub(1))
                        .unwrap_or_else(|| 0);
                    if tip_timestamp < median_timestamp + timestamp {
                        return Err(TransactionError::Immature);
                    }
                }
                None => {
                    return Err(TransactionError::InvalidSince);
                }
            }
        }
        Ok(())
    }

    pub fn verify(&self) -> Result<(), TransactionError> {
        for (resolved_out_point, input) in self
            .rtx
            .resolved_inputs
            .iter()
            .zip(self.rtx.transaction.inputs())
        {
            if resolved_out_point.cell().is_none() {
                continue;
            }
            let cell_meta = resolved_out_point.cell().unwrap();
            // ignore empty since
            if input.since == 0 {
                continue;
            }
            let since = Since(input.since);
            // check remain flags
            if !since.flags_is_valid() {
                return Err(TransactionError::InvalidSince);
            }

            // verify time lock
            self.verify_absolute_lock(since)?;
            self.verify_relative_lock(since, cell_meta)?;
        }
        Ok(())
    }
}
