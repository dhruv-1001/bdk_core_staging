use core::{
    fmt::{Debug, Display},
    ops::{Bound, RangeBounds},
};

use crate::{collections::*, tx_graph::TxGraph, BlockId, FullTxOut, TxHeight};
use bitcoin::{hashes::Hash, BlockHash, OutPoint, Txid};

/// This is a non-monotone structure that tracks relevant [`Txid`]s that are ordered by index `I`.
///
/// To "merge" two [`SparseChain`]s, one can calculate the [`ChangeSet`] by calling
/// [`Self::determine_changeset(update)`], and applying the [`ChangeSet`] via
/// [`Self::apply_changeset(changeset)`]. For convenience, one can do the above two steps as one via
/// [`Self::apply_update(update)`].
#[derive(Clone, Debug, PartialEq)]
pub struct SparseChain<I = TxHeight> {
    /// Block height to checkpoint data.
    checkpoints: BTreeMap<u32, BlockHash>,
    /// Txids ordered by the index `I`.
    ordered_txids: BTreeSet<(I, Txid)>,
    /// Confirmation heights of txids.
    txid_to_index: HashMap<Txid, I>,
    /// Limit number of checkpoints.
    checkpoint_limit: Option<usize>,
}

impl<I> Default for SparseChain<I> {
    fn default() -> Self {
        Self {
            checkpoints: Default::default(),
            ordered_txids: Default::default(),
            txid_to_index: Default::default(),
            checkpoint_limit: Default::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum InsertTxErr {
    TxTooHigh,
    TxMoved,
}

impl Display for InsertTxErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for InsertTxErr {}

#[derive(Clone, Debug, PartialEq)]
pub enum InsertCheckpointErr {
    HashNotMatching,
}

impl Display for InsertCheckpointErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for InsertCheckpointErr {}

/// Represents an update failure of [`SparseChain`].
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateFailure<I = TxHeight> {
    /// The update cannot be applied to the chain because the chain suffix it represents did not
    /// connect to the existing chain. This error case contains the checkpoint height to include so
    /// that the chains can connect.
    NotConnected(u32),
    /// The update contains inconsistent tx states (e.g. it changed the transaction's hieght).
    /// This error is usually the inconsistency found.
    InconsistentTx {
        inconsistent_txid: Txid,
        original_index: I,
        update_index: I,
    },
}

impl<I: core::fmt::Debug> core::fmt::Display for UpdateFailure<I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotConnected(h) =>
                write!(f, "the checkpoints in the update could not be connected to the checkpoints in the chain, try include checkpoint of height {} to connect",
                    h),
            Self::InconsistentTx { inconsistent_txid, original_index, update_index } =>
                write!(f, "inconsistent update: first inconsistent tx is ({}) which had index ({:?}), but is ({:?}) in the update", 
                    inconsistent_txid, original_index, update_index),
        }
    }
}

#[cfg(feature = "std")]
impl<I: core::fmt::Debug> std::error::Error for UpdateFailure<I> {}

impl<I: ChainIndex> SparseChain<I> {
    /// Creates a new chain from a list of block hashes and heights. The caller must guarantee they are in the same
    /// chain.
    pub fn from_checkpoints<C>(checkpoints: C) -> Self
    where
        C: IntoIterator<Item = BlockId>,
    {
        let mut chain = Self::default();
        chain.checkpoints = checkpoints
            .into_iter()
            .map(|block_id| block_id.into())
            .collect();
        chain
    }

    /// Get the `BlockId` for the last known tip.
    pub fn latest_checkpoint(&self) -> Option<BlockId> {
        self.checkpoints
            .iter()
            .last()
            .map(|(&height, &hash)| BlockId { height, hash })
    }

    /// Get the checkpoint id at the given height if it exists
    pub fn checkpoint_at(&self, height: u32) -> Option<BlockId> {
        self.checkpoints
            .get(&height)
            .map(|&hash| BlockId { height, hash })
    }

    /// Return the associated index of a tx of txid (if any).
    pub fn tx_index(&self, txid: Txid) -> Option<&I> {
        self.txid_to_index.get(&txid)
    }

    /// Return an iterator over all checkpoints, in descending order.
    pub fn checkpoints(&self) -> &BTreeMap<u32, BlockHash> {
        &self.checkpoints
    }

    /// Return an iterator over the checkpoint locations in a height range, in ascending height order.
    pub fn range_checkpoints(
        &self,
        range: impl RangeBounds<u32>,
    ) -> impl DoubleEndedIterator<Item = BlockId> + '_ {
        self.checkpoints
            .range(range)
            .map(|(&height, &hash)| BlockId { height, hash })
    }

    /// Derives a [`ChangeSet`] that could be applied to an empty index.
    pub fn initial_change_set(&self) -> ChangeSet<I> {
        ChangeSet {
            checkpoints: self
                .checkpoints
                .iter()
                .map(|(height, hash)| (*height, Some(*hash)))
                .collect(),
            txids: self
                .ordered_txids
                .iter()
                .map(|(index, txid)| (*txid, Some(index.clone())))
                .collect(),
        }
    }

    /// Determine the changeset when `update` is applied to self. Invalidated checkpoints result in
    /// invalidated transactions becoming "unconfirmed".
    pub fn determine_changeset(&self, update: &Self) -> Result<ChangeSet<I>, UpdateFailure<I>> {
        let agreement_point = update
            .checkpoints
            .iter()
            .rev()
            .find(|&(height, hash)| self.checkpoints.get(height) == Some(hash))
            .map(|(&h, _)| h);

        let last_update_cp = update.checkpoints.iter().last().map(|(&h, _)| h);

        // the lower bound of the invalidation range
        let invalid_lb = if last_update_cp.is_none() || last_update_cp == agreement_point {
            // if agreement point is the last update checkpoint, or there is no update checkpoints,
            // no invalidation is required
            u32::MAX
        } else {
            agreement_point.map(|h| h + 1).unwrap_or(0)
        };

        // the first checkpoint of the sparsechain to invalidate (if any)
        let invalid_from = self.checkpoints.range(invalid_lb..).next().map(|(&h, _)| h);

        // the first checkpoint to invalidate (if any) should be represented in the update
        if let Some(first_invalid) = invalid_from {
            if !update.checkpoints.contains_key(&first_invalid) {
                return Err(UpdateFailure::NotConnected(first_invalid));
            }
        }

        for (&txid, update_index) in &update.txid_to_index {
            // ensure all currently confirmed txs are still at the same height (unless they are
            // within invalidation range, or to be confirmed)
            if let Some(original_index) = &self.txid_to_index.get(&txid) {
                if original_index.height() < TxHeight::Confirmed(invalid_lb)
                    && original_index != &update_index
                {
                    return Err(UpdateFailure::InconsistentTx {
                        inconsistent_txid: txid,
                        original_index: I::clone(original_index),
                        update_index: update_index.clone(),
                    });
                }
            }
        }

        // create initial change-set, based on checkpoints and txids that are to be "invalidated"
        let mut changeset = invalid_from
            .map(|invalid_from| ChangeSet::<I> {
                checkpoints: self
                    .checkpoints
                    .range(invalid_from..)
                    .map(|(height, _)| (*height, None))
                    .collect(),
                // invalidated transactions become unconfirmed
                txids: self
                    .range_txids_by_height(TxHeight::Confirmed(invalid_from)..)
                    .map(|(_, txid)| (*txid, Some(I::max_ord_of_height(TxHeight::Unconfirmed))))
                    .collect(),
            })
            .unwrap_or_default();

        for (&height, &new_hash) in &update.checkpoints {
            let original_hash = self.checkpoints.get(&height).cloned();

            let update_hash = *changeset
                .checkpoints
                .entry(height)
                .and_modify(|change| *change = Some(new_hash))
                .or_insert_with(|| Some(new_hash));

            if original_hash == update_hash {
                changeset.checkpoints.remove(&height);
            }
        }

        for (txid, new_index) in &update.txid_to_index {
            let original_index = self.txid_to_index.get(txid).cloned();

            let update_index = changeset
                .txids
                .entry(*txid)
                .and_modify(|change| *change = Some(new_index.clone()))
                .or_insert_with(|| Some(new_index.clone()));

            if original_index == *update_index {
                changeset.txids.remove(txid);
            }
        }

        Ok(changeset)
    }

    /// Tries to update `self` with another chain that connects to it.
    pub fn apply_update(&mut self, update: Self) -> Result<(), UpdateFailure<I>> {
        let changeset = self.determine_changeset(&update)?;
        self.apply_changeset(changeset);
        Ok(())
    }

    pub fn apply_changeset(&mut self, changeset: ChangeSet<I>) {
        for (height, update_hash) in &changeset.checkpoints {
            let _original_hash = match update_hash {
                Some(update_hash) => self.checkpoints.insert(*height, *update_hash),
                None => self.checkpoints.remove(&height),
            };
        }

        for (txid, update_index) in &changeset.txids {
            let original_index = self.txid_to_index.remove(txid);

            if let Some(index) = original_index {
                self.ordered_txids.remove(&(index, *txid));
            }

            if let Some(index) = update_index {
                self.txid_to_index.insert(*txid, index.clone());
                self.ordered_txids.insert((index.clone(), *txid));
            }
        }

        self.prune_checkpoints();
    }

    pub fn clear_mempool_changeset(&self) -> ChangeSet<I> {
        let txids = self
            .ordered_txids
            .range(
                &(
                    I::min_ord_of_height(TxHeight::Unconfirmed),
                    Txid::all_zeros(),
                )..,
            )
            .map(|(_, txid)| (*txid, None))
            .collect();

        ChangeSet::<I> {
            txids,
            ..Default::default()
        }
    }

    /// Clear the mempool list. Use with caution.
    pub fn clear_mempool(&mut self) {
        let changeset = self.clear_mempool_changeset();
        self.apply_changeset(changeset);
    }

    /// Insert an arbitrary txid. This assumes that we have at least one checkpoint and the tx does
    /// not already exist in [`SparseChain`]. Returns a [`ChangeSet`] on success.
    pub fn insert_tx(&mut self, txid: Txid, index: I) -> Result<bool, InsertTxErr> {
        let new_height = index.height();

        let latest = self
            .checkpoints
            .keys()
            .last()
            .cloned()
            .map(TxHeight::Confirmed);

        if new_height.is_confirmed() && (latest.is_none() || new_height > latest.unwrap()) {
            return Err(InsertTxErr::TxTooHigh);
        }

        if let Some(original_index) = self.txid_to_index.get(&txid) {
            if original_index.height().is_confirmed() && *original_index != index {
                return Err(InsertTxErr::TxMoved);
            }

            return Ok(false);
        }

        self.txid_to_index.insert(txid, index.clone());
        self.ordered_txids.insert((index, txid));

        Ok(true)
    }

    pub fn insert_checkpoint(&mut self, block_id: BlockId) -> Result<bool, InsertCheckpointErr> {
        if let Some(&old_hash) = self.checkpoints.get(&block_id.height) {
            if old_hash != block_id.hash {
                return Err(InsertCheckpointErr::HashNotMatching);
            }

            return Ok(false);
        }

        self.checkpoints.insert(block_id.height, block_id.hash);
        self.prune_checkpoints();
        Ok(true)
    }

    pub fn iter_txids(
        &self,
    ) -> impl DoubleEndedIterator<Item = &(I, Txid)> + ExactSizeIterator + '_ {
        self.ordered_txids.iter()
    }

    pub fn range_txids<R>(&self, range: R) -> impl DoubleEndedIterator<Item = &(I, Txid)> + '_
    where
        R: RangeBounds<(I, Txid)>,
    {
        let map_bound = |b: Bound<&(I, Txid)>| match b {
            Bound::Included((index, txid)) => Bound::Included((index.clone(), *txid)),
            Bound::Excluded((index, txid)) => Bound::Excluded((index.clone(), *txid)),
            Bound::Unbounded => Bound::Unbounded,
        };

        self.ordered_txids
            .range((map_bound(range.start_bound()), map_bound(range.end_bound())))
    }

    pub fn range_txids_by_index<R>(
        &self,
        range: R,
    ) -> impl DoubleEndedIterator<Item = &(I, Txid)> + '_
    where
        R: RangeBounds<I>,
    {
        let map_bound = |b: Bound<&I>, inc: Txid, exc: Txid| match b {
            Bound::Included(index) => Bound::Included((index.clone(), inc)),
            Bound::Excluded(index) => Bound::Excluded((index.clone(), exc)),
            Bound::Unbounded => Bound::Unbounded,
        };

        self.ordered_txids.range((
            map_bound(range.start_bound(), min_txid(), max_txid()),
            map_bound(range.end_bound(), max_txid(), min_txid()),
        ))
    }

    pub fn range_txids_by_height<R>(
        &self,
        range: R,
    ) -> impl DoubleEndedIterator<Item = &(I, Txid)> + '_
    where
        R: RangeBounds<TxHeight>,
    {
        let ord_it = |height, is_max| match is_max {
            true => I::max_ord_of_height(height),
            false => I::min_ord_of_height(height),
        };

        let map_bound = |b: Bound<&TxHeight>, inc: (bool, Txid), exc: (bool, Txid)| match b {
            Bound::Included(&h) => Bound::Included((ord_it(h, inc.0), inc.1)),
            Bound::Excluded(&h) => Bound::Excluded((ord_it(h, exc.0), exc.1)),
            Bound::Unbounded => Bound::Unbounded,
        };

        self.ordered_txids.range((
            map_bound(range.start_bound(), (false, min_txid()), (true, max_txid())),
            map_bound(range.end_bound(), (true, max_txid()), (false, min_txid())),
        ))
    }

    /// Given a transaction graph and a particular outpoint attempts to retrieve a `FullTxOut`. This
    /// function will return `Some(full_txout)` only if the output's transaction is in `self` and
    /// the graph.
    pub fn full_txout(&self, graph: &TxGraph, outpoint: OutPoint) -> Option<FullTxOut<I>> {
        let chain_index = self.tx_index(outpoint.txid)?;

        let txout = graph.txout(outpoint).cloned()?;

        let spent_by = self
            .spent_by(graph, outpoint)
            .map(|(index, txid)| (index.clone(), txid));

        Some(FullTxOut {
            outpoint,
            txout,
            chain_index: chain_index.clone(),
            spent_by,
        })
    }

    pub fn set_checkpoint_limit(&mut self, limit: Option<usize>) {
        self.checkpoint_limit = limit;
        self.prune_checkpoints();
    }

    fn prune_checkpoints(&mut self) -> Option<BTreeMap<u32, BlockHash>> {
        let limit = self.checkpoint_limit?;

        // find last height to be pruned
        let last_height = *self.checkpoints.keys().rev().nth(limit)?;
        // first height to be kept
        let keep_height = last_height + 1;

        let mut split = self.checkpoints.split_off(&keep_height);
        core::mem::swap(&mut self.checkpoints, &mut split);

        Some(split)
    }

    /// Finds the transaction in the chain that spends `outpoint` given the input/output
    /// relationships in `graph`. Note that the transaction including `outpoint` does not need to be
    /// in the `graph` or the `chain` for this to return `Some(_)`.
    pub fn spent_by(&self, graph: &TxGraph, outpoint: OutPoint) -> Option<(&I, Txid)> {
        graph
            .outspends(outpoint)
            .iter()
            .find_map(|&txid| Some((self.tx_index(txid)?, txid)))
    }
}

/// The return value of [`determine_changeset`].
///
/// [`determine_changeset`]: SparseChain::determine_changeset.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Deserialize, serde::Serialize),
    serde(crate = "serde_crate")
)]
pub struct ChangeSet<I = TxHeight> {
    pub checkpoints: BTreeMap<u32, Option<BlockHash>>,
    pub txids: BTreeMap<Txid, Option<I>>,
}

impl<I> Default for ChangeSet<I> {
    fn default() -> Self {
        Self {
            checkpoints: Default::default(),
            txids: Default::default(),
        }
    }
}

impl<I: ChainIndex> ChangeSet<I> {
    pub fn merge(mut self, new_set: Self) -> Self {
        for (height, new_change) in new_set.checkpoints {
            self.checkpoints
                .entry(height)
                .and_modify(|change| *change = new_change)
                .or_insert_with(|| new_change.clone());
        }

        for (txid, new_change) in new_set.txids {
            self.txids
                .entry(txid)
                .and_modify(|change| *change = new_change.clone())
                .or_insert_with(|| new_change);
        }

        self
    }
}

impl<I> ChangeSet<I> {
    pub fn tx_additions(&self) -> impl Iterator<Item = Txid> + '_ {
        self.txids
            .iter()
            .filter_map(|(txid, new_value)| new_value.as_ref().map(|_| *txid))
    }

    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty() && self.txids.is_empty()
    }
}

fn min_txid() -> Txid {
    Txid::from_inner([0x00; 32])
}

fn max_txid() -> Txid {
    Txid::from_inner([0xff; 32])
}

/// Represents an index in which transactions are ordered by in [`SparseChain`].
///
/// [`ChainIndex`] implementations must be [`Ord`] by [`TxHeight`] first.
pub trait ChainIndex: core::fmt::Debug + Clone + Eq + PartialOrd + Ord + core::hash::Hash {
    /// Obtain the transaction height of the index.
    fn height(&self) -> TxHeight;

    /// Obtain the index's upper bound of a given height.
    fn max_ord_of_height(height: TxHeight) -> Self;

    /// Obtain the index's lower bound of a given height.
    fn min_ord_of_height(height: TxHeight) -> Self;
}

#[cfg(test)]
pub mod verify_chain_index {
    use crate::{sparse_chain::ChainIndex, ConfirmationTime, TxHeight};
    use alloc::vec::Vec;

    pub fn verify_chain_index<I: ChainIndex>(head_count: u32, tail_count: u32) {
        let values = (0..head_count)
            .chain(u32::MAX - tail_count..u32::MAX)
            .flat_map(|i| {
                [
                    I::min_ord_of_height(TxHeight::Confirmed(i)),
                    I::max_ord_of_height(TxHeight::Confirmed(i)),
                ]
            })
            .chain([
                I::min_ord_of_height(TxHeight::Unconfirmed),
                I::max_ord_of_height(TxHeight::Unconfirmed),
            ])
            .collect::<Vec<_>>();

        for i in 0..values.len() {
            for j in 0..values.len() {
                if i == j {
                    assert_eq!(values[i], values[j]);
                }
                if i < j {
                    assert!(values[i] <= values[j]);
                }
                if i > j {
                    assert!(values[i] >= values[j]);
                }
            }
        }
    }

    #[test]
    fn verify_tx_height() {
        verify_chain_index::<TxHeight>(1000, 1000);
    }

    #[test]
    fn verify_confirmation_time() {
        verify_chain_index::<ConfirmationTime>(1000, 1000);
    }
}
