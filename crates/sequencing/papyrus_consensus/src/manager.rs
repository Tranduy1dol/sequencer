//! Consensus manager, see Manager struct.

#[cfg(test)]
#[path = "manager_test.rs"]
mod manager_test;

use std::collections::BTreeMap;
use std::time::Duration;

use futures::channel::{mpsc, oneshot};
use futures::stream::FuturesUnordered;
use futures::{Stream, StreamExt};
use papyrus_common::metrics::{PAPYRUS_CONSENSUS_HEIGHT, PAPYRUS_CONSENSUS_SYNC_COUNT};
use papyrus_network::network_manager::ReportSender;
use papyrus_protobuf::consensus::{ConsensusMessage, Proposal};
use papyrus_protobuf::converters::ProtobufConversionError;
use starknet_api::block::{BlockHash, BlockNumber};
use tracing::{debug, info, instrument};

use crate::config::TimeoutsConfig;
use crate::single_height_consensus::{ShcReturn, ShcTask, SingleHeightConsensus};
use crate::types::{
    ConsensusBlock,
    ConsensusContext,
    ConsensusError,
    Decision,
    ProposalInit,
    ValidatorId,
};

// TODO(dvir): add test for this.
#[instrument(skip_all, level = "info")]
#[allow(missing_docs)]
pub async fn run_consensus<BlockT, ContextT, NetworkReceiverT, SyncReceiverT>(
    mut context: ContextT,
    start_height: BlockNumber,
    validator_id: ValidatorId,
    consensus_delay: Duration,
    timeouts: TimeoutsConfig,
    mut network_receiver: NetworkReceiverT,
    mut sync_receiver: SyncReceiverT,
) -> Result<(), ConsensusError>
where
    BlockT: ConsensusBlock,
    ContextT: ConsensusContext<Block = BlockT>,
    NetworkReceiverT:
        Stream<Item = (Result<ConsensusMessage, ProtobufConversionError>, ReportSender)> + Unpin,
    SyncReceiverT: Stream<Item = BlockNumber> + Unpin,
    ProposalWrapper:
        Into<(ProposalInit, mpsc::Receiver<BlockT::ProposalChunk>, oneshot::Receiver<BlockHash>)>,
{
    info!(
        "Running consensus, start_height={}, validator_id={}, consensus_delay={}, timeouts={:?}",
        start_height,
        validator_id,
        consensus_delay.as_secs(),
        timeouts
    );

    // Add a short delay to allow peers to connect and avoid "InsufficientPeers" error
    tokio::time::sleep(consensus_delay).await;
    let mut current_height = start_height;
    let mut manager = MultiHeightManager::new(validator_id, timeouts);
    loop {
        metrics::gauge!(PAPYRUS_CONSENSUS_HEIGHT, current_height.0 as f64);

        let run_height = manager.run_height(&mut context, current_height, &mut network_receiver);

        // `run_height` is not cancel safe. Our implementation doesn't enable us to start and stop
        // it. We also cannot restart the height; when we dropped the future we dropped the state it
        // built and risk equivocating. Therefore, we must only enter the other select branches if
        // we are certain to leave this height.
        tokio::select! {
            decision = run_height => {
                let decision = decision?;
                context.decision_reached(decision.block, decision.precommits).await?;
                current_height = current_height.unchecked_next();
            },
            sync_height = sync_height(current_height, &mut sync_receiver) => {
                metrics::increment_counter!(PAPYRUS_CONSENSUS_SYNC_COUNT);
                current_height = sync_height?.unchecked_next();
            }
        }
    }
}

// `Proposal` is defined in the protobuf crate so we can't implement `Into` for it because of the
// orphan rule. This wrapper enables us to implement `Into` for the inner `Proposal`.
#[allow(missing_docs)]
pub struct ProposalWrapper(pub Proposal);

/// Runs Tendermint repeatedly across different heights. Handles issues which are not explicitly
/// part of the single height consensus algorithm (e.g. messages from future heights).
#[derive(Debug, Default)]
struct MultiHeightManager {
    validator_id: ValidatorId,
    cached_messages: BTreeMap<u64, Vec<ConsensusMessage>>,
    timeouts: TimeoutsConfig,
}

impl MultiHeightManager {
    /// Create a new consensus manager.
    pub fn new(validator_id: ValidatorId, timeouts: TimeoutsConfig) -> Self {
        Self { validator_id, cached_messages: BTreeMap::new(), timeouts }
    }

    /// Run the consensus algorithm for a single height.
    ///
    /// Assumes that `height` is monotonically increasing across calls for the sake of filtering
    /// `cached_messaged`.
    #[instrument(skip(self, context, network_receiver), level = "info")]
    pub async fn run_height<BlockT, ContextT, NetworkReceiverT>(
        &mut self,
        context: &mut ContextT,
        height: BlockNumber,
        network_receiver: &mut NetworkReceiverT,
    ) -> Result<Decision<BlockT>, ConsensusError>
    where
        BlockT: ConsensusBlock,
        ContextT: ConsensusContext<Block = BlockT>,
        NetworkReceiverT: Stream<Item = (Result<ConsensusMessage, ProtobufConversionError>, ReportSender)>
            + Unpin,
        ProposalWrapper: Into<(
            ProposalInit,
            mpsc::Receiver<BlockT::ProposalChunk>,
            oneshot::Receiver<BlockHash>,
        )>,
    {
        let validators = context.validators(height).await;
        let mut shc = SingleHeightConsensus::new(
            height,
            self.validator_id,
            validators,
            self.timeouts.clone(),
        );
        let mut shc_tasks = FuturesUnordered::new();

        match shc.start(context).await? {
            ShcReturn::Decision(decision) => return Ok(decision),
            ShcReturn::Tasks(tasks) => {
                for task in tasks {
                    shc_tasks.push(create_task_handler(task));
                }
            }
        }

        let mut current_height_messages = self.get_current_height_messages(height);
        loop {
            let shc_return = tokio::select! {
                message = next_message(&mut current_height_messages, network_receiver) => {
                    self.handle_message(context, height, &mut shc, message?).await?
                },
                Some(shc_task) = shc_tasks.next() => {
                    shc.handle_task(context, shc_task).await?
                },
            };

            match shc_return {
                ShcReturn::Decision(decision) => return Ok(decision),
                ShcReturn::Tasks(tasks) => {
                    for task in tasks {
                        shc_tasks.push(create_task_handler(task));
                    }
                }
            }
        }
    }

    // Handle a single consensus message.
    async fn handle_message<BlockT, ContextT>(
        &mut self,
        context: &mut ContextT,
        height: BlockNumber,
        shc: &mut SingleHeightConsensus<BlockT>,
        message: ConsensusMessage,
    ) -> Result<ShcReturn<BlockT>, ConsensusError>
    where
        BlockT: ConsensusBlock,
        ContextT: ConsensusContext<Block = BlockT>,
        ProposalWrapper: Into<(
            ProposalInit,
            mpsc::Receiver<BlockT::ProposalChunk>,
            oneshot::Receiver<BlockHash>,
        )>,
    {
        // TODO(matan): We need to figure out an actual cacheing strategy under 2 constraints:
        // 1. Malicious - must be capped so a malicious peer can't DoS us.
        // 2. Parallel proposals - we may send/receive a proposal for (H+1, 0).
        // In general I think we will want to only cache (H+1, 0) messages.
        if message.height() != height.0 {
            debug!("Received a message for a different height. {:?}", message);
            if message.height() > height.0 {
                self.cached_messages.entry(message.height()).or_default().push(message);
            }
            return Ok(ShcReturn::Tasks(vec![]));
        }
        match message {
            ConsensusMessage::Proposal(proposal) => {
                // Special case due to fake streaming.
                let (proposal_init, content_receiver, fin_receiver) =
                    ProposalWrapper(proposal).into();
                let res = shc
                    .handle_proposal(context, proposal_init, content_receiver, fin_receiver)
                    .await?;
                Ok(res)
            }
            _ => {
                let res = shc.handle_message(context, message).await?;
                Ok(res)
            }
        }
    }

    // Filters the cached messages:
    // - returns all of the current height messages.
    // - drops messages from earlier heights.
    // - retains future messages in the cache.
    fn get_current_height_messages(&mut self, height: BlockNumber) -> Vec<ConsensusMessage> {
        // Depends on `cached_messages` being sorted by height.
        loop {
            let Some(entry) = self.cached_messages.first_entry() else {
                return Vec::new();
            };
            match entry.key().cmp(&height.0) {
                std::cmp::Ordering::Greater => return Vec::new(),
                std::cmp::Ordering::Equal => return entry.remove(),
                std::cmp::Ordering::Less => {
                    entry.remove();
                }
            }
        }
    }
}

async fn next_message<NetworkReceiverT>(
    cached_messages: &mut Vec<ConsensusMessage>,
    network_receiver: &mut NetworkReceiverT,
) -> Result<ConsensusMessage, ConsensusError>
where
    NetworkReceiverT:
        Stream<Item = (Result<ConsensusMessage, ProtobufConversionError>, ReportSender)> + Unpin,
{
    if let Some(msg) = cached_messages.pop() {
        return Ok(msg);
    }

    let (msg, report_sender) = network_receiver.next().await.ok_or_else(|| {
        ConsensusError::InternalNetworkError("NetworkReceiver should never be closed".to_string())
    })?;
    match msg {
        // TODO(matan): Return report_sender for use in later errors by SHC.
        Ok(msg) => Ok(msg),
        Err(e) => {
            // Failed to parse consensus message
            report_sender.send(()).or(Err(ConsensusError::InternalNetworkError(
                "Failed to send report".to_string(),
            )))?;
            Err(e.into())
        }
    }
}

// Return only when a height is reached that is greater than or equal to the current height.
async fn sync_height<SyncReceiverT>(
    height: BlockNumber,
    mut sync_receiver: SyncReceiverT,
) -> Result<BlockNumber, ConsensusError>
where
    SyncReceiverT: Stream<Item = BlockNumber> + Unpin,
{
    loop {
        match sync_receiver.next().await {
            Some(sync_height) if sync_height >= height => {
                info!("Sync to height: {}. current_height={}", sync_height, height);
                return Ok(sync_height);
            }
            Some(sync_height) => {
                debug!("Ignoring sync to height: {}. current_height={}", sync_height, height);
            }
            None => {
                return Err(ConsensusError::SyncError("Sync receiver closed".to_string()));
            }
        }
    }
}

async fn create_task_handler(task: ShcTask) -> ShcTask {
    tokio::time::sleep(task.duration).await;
    task
}
