use crate::{
    sequence::FlashBlockPendingSequence,
    worker::{BuildArgs, FlashBlockBuilder},
    ExecutionPayloadBaseV1, FlashBlock, FlashBlockCompleteSequence, FlashBlockCompleteSequenceRx,
    InProgressFlashBlockRx, PendingFlashBlock,
};
use alloy_eips::eip2718::WithEncoded;
use alloy_primitives::B256;
use futures_util::{FutureExt, Stream, StreamExt};
use metrics::Histogram;
use reth_chain_state::{CanonStateNotification, CanonStateNotifications, CanonStateSubscriptions};
use reth_evm::ConfigureEvm;
use reth_metrics::Metrics;
use reth_primitives_traits::{
    AlloyBlockHeader, BlockTy, HeaderTy, NodePrimitives, ReceiptTy, Recovered,
};
use reth_revm::cached::CachedReads;
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};
use reth_tasks::TaskExecutor;
use std::{
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::Instant,
};
use tokio::{
    pin,
    sync::{oneshot, watch},
};
use tracing::{debug, trace, warn};

pub(crate) const FB_STATE_ROOT_FROM_INDEX: usize = 9;

/// The `FlashBlockService` maintains an in-memory [`PendingFlashBlock`] built out of a sequence of
/// [`FlashBlock`]s.
#[derive(Debug)]
pub struct FlashBlockService<
    P: PayloadTypes,
    N: NodePrimitives,
    S,
    EvmConfig: ConfigureEvm<Primitives = N, NextBlockEnvCtx: Unpin>,
    Provider,
> {
    rx: S,
    current: Option<PendingFlashBlock<N>>,
    blocks: FlashBlockPendingSequence<N::SignedTx>,
    /// Broadcast channel to forward received flashblocks from the subscription.
    received_flashblocks_tx: tokio::sync::broadcast::Sender<Arc<FlashBlock>>,
    rebuild: bool,
    builder: FlashBlockBuilder<EvmConfig, Provider>,
    canon_receiver: CanonStateNotifications<N>,
    spawner: TaskExecutor,
    job: Option<BuildJob<N>>,
    /// Manages flashblock sequences with caching and intelligent build selection.
    sequences: Arc<SequenceManager<P, N::SignedTx>>,

    /// `FlashBlock` service's metrics
    metrics: FlashBlockServiceMetrics,
    /// Enable state root calculation from flashblock with index [`FB_STATE_ROOT_FROM_INDEX`]
    compute_state_root: bool,
}

impl<P, N, S, EvmConfig, Provider> FlashBlockService<P, N, S, EvmConfig, Provider>
where
    P: PayloadTypes,
    N: NodePrimitives,
    N::BlockHeader: reth_primitives_traits::header::HeaderMut,
    P::BuiltPayload: reth_payload_primitives::BuiltPayload<Primitives = N>,
    S: Stream<Item = eyre::Result<FlashBlock>> + Unpin + 'static,
    EvmConfig: ConfigureEvm<Primitives = N, NextBlockEnvCtx: From<ExecutionPayloadBaseV1> + Unpin>
        + Clone
        + 'static,
    Provider: StateProviderFactory
        + CanonStateSubscriptions<Primitives = N>
        + BlockReaderIdExt<
            Header = HeaderTy<N>,
            Block = BlockTy<N>,
            Transaction = N::SignedTx,
            Receipt = ReceiptTy<N>,
        > + Unpin
        + Clone
        + 'static,
{
    /// Constructs a new `FlashBlockService` that receives [`FlashBlock`]s from `rx` stream.
    pub fn new(rx: S, evm_config: EvmConfig, provider: Provider, spawner: TaskExecutor) -> Self {
        let (in_progress_tx, _) = watch::channel(None);
        let (received_flashblocks_tx, _) = tokio::sync::broadcast::channel(128);
        Self {
            rx,
            current: None,
            blocks: FlashBlockPendingSequence::new(),
            received_flashblocks_tx,
            canon_receiver: provider.subscribe_to_canonical_state(),
            builder: FlashBlockBuilder::new(evm_config, provider),
            rebuild: false,
            spawner,
            job: None,
            sequences: Arc::new(SequenceManager::new(engine_handle)),
            metrics: FlashBlockServiceMetrics::default(),
            compute_state_root: false,
        }
    }

    /// Enable state root calculation from flashblock
    pub const fn compute_state_root(mut self, enable_state_root: bool) -> Self {
        self.compute_state_root = enable_state_root;
        self
    }

    /// Returns the sender half to the received flashblocks.
    pub const fn flashblocks_broadcaster(
        &self,
    ) -> &tokio::sync::broadcast::Sender<Arc<FlashBlock>> {
        &self.received_flashblocks_tx
    }

    /// Returns the sender half to the flashblock sequence.
    pub fn block_sequence_broadcaster(
        &self,
    ) -> &tokio::sync::broadcast::Sender<FlashBlockCompleteSequence> {
        self.blocks.block_sequence_broadcaster()
    }

    /// Returns a subscriber to the flashblock sequence.
    pub fn subscribe_block_sequence(&self) -> FlashBlockCompleteSequenceRx {
        self.blocks.subscribe_block_sequence()
    }

    /// Returns a receiver that signals when a flashblock is being built.
    pub fn subscribe_in_progress(&self) -> InProgressFlashBlockRx {
        self.in_progress_tx.subscribe()
    }

    /// Drives the services and sends new blocks to the receiver
    ///
    /// Note: this should be spawned
    pub async fn run(mut self, tx: watch::Sender<Option<PendingFlashBlock<N>>>) {
        loop {
            tokio::select! {
                // Event 1: job exists, listen to job results
                Some(result) = async {
                    match self.job.as_mut() {
                        Some((_, rx)) => rx.await.ok(),
                        None => std::future::pending().await,
                    }
                } => {
                    let (start_time, _) = self.job.take().unwrap();
                    let _ = self.in_progress_tx.send(None);

                    match result {
                        Ok(Some((pending, cached_reads))) => {
                            // Update the new pending state immediately after execution for eth api handler
                            let parent_hash = pending.parent_hash();
                            self.sequences
                                .on_build_complete(parent_hash, cached_reads).await;
                            let _ = tx.send(Some(pending.clone()));

                            let elapsed = start_time.elapsed();
                            self.metrics.execution_duration.record(elapsed.as_secs_f64());

                            // For sync and flashblocks consensus. Calculate the state root if missing
                            if pending.compute_state_root {
                                let sequences = Arc::clone(&self.sequences);
                                let builder = self.builder.clone();
                                self.spawner.spawn(async move {
                                    match builder.compute_state_root(pending) {
                                        Ok(execution) => {
                                            if let Some(computed_block) = execution {
                                                sequences.on_state_root_complete(parent_hash, computed_block).await;
                                            }
                                        }
                                        Err(err) => {
                                            warn!(target: "flashblocks", %err, "Failed to compute state root");
                                        }
                                    }
                                });
                            }


                        }
                        Ok(None) => {
                            trace!(target: "flashblocks", "Build job returned None");
                        }
                        Err(err) => {
                            warn!(target: "flashblocks", %err, "Build job failed");
                        }
                    }
                }

                // Event 2: New flashblock arrives (batch process all ready flashblocks)
                result = self.incoming_flashblock_rx.next() => {
                    match result {
                        Some(Ok(flashblock)) => {
                            // Process first flashblock
                            self.process_flashblock(flashblock).await;

                            // Batch process all other immediately available flashblocks
                            while let Some(result) = self.incoming_flashblock_rx.next().now_or_never().flatten() {
                                match result {
                                    Ok(fb) => self.process_flashblock(fb).await,
                                    Err(err) => warn!(target: "flashblocks", %err, "Error receiving flashblock"),
                                }
                            }

                            self.try_start_build_job().await;
                        }
                        Some(Err(err)) => {
                            warn!(target: "flashblocks", %err, "Error receiving flashblock");
                        }
                        None => {
                            warn!(target: "flashblocks", "Flashblock stream ended");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Processes a single flashblock: notifies subscribers, records metrics, and inserts into
    /// sequence.
    async fn process_flashblock(&self, flashblock: FlashBlock) {
        self.notify_received_flashblock(&flashblock);

        if flashblock.index == 0 {
            self.metrics.last_flashblock_length.record(self.sequences.pending_count().await as f64);
        }

        if let Err(err) = self.sequences.insert_flashblock(flashblock).await {
            trace!(target: "flashblocks", %err, "Failed to insert flashblock");
        }
    }

    /// Notifies all subscribers about the received flashblock
    fn notify_received_flashblock(&self, flashblock: &FlashBlock) {
        if self.received_flashblocks_tx.receiver_count() > 0 {
            let _ = self.received_flashblocks_tx.send(Arc::new(flashblock.clone()));
        }
    }

    /// Attempts to build a block if no job is currently running and a buildable sequence exists.
    async fn try_start_build_job(&mut self) {
        if self.job.is_some() {
            return; // Already building
        }

        let Some(latest) = self.builder.provider().latest_header().ok().flatten() else {
            return;
        };

        let Some(args) =
            self.sequences.next_buildable_args(latest.hash(), latest.timestamp()).await
        else {
            // Nothing buildable, skipping - state mismatch between local chainstate tip and
            // flashblock sequences cache.
            debug!(target: "flashblocks", local_latest=?latest.num_hash(),
                "No buildable jobs available, chainstate mismatch between flashblock cache and local chainstate tip");
            return;
        };

        // Spawn build job
        let fb_info = FlashBlockBuildInfo {
            parent_hash: args.base.parent_hash,
            index: args.last_flashblock_index,
            block_number: args.base.block_number,
        };
        self.metrics.current_block_height.set(fb_info.block_number as f64);
        self.metrics.current_index.set(fb_info.index as f64);
        let _ = self.in_progress_tx.send(Some(fb_info));

        let (tx, rx) = oneshot::channel();
        let builder = self.builder.clone();
        self.spawner.spawn_blocking(Box::pin(async move {
            let _ = tx.send(builder.execute(args));
        }));
        self.job = Some((Instant::now(), rx));
    }
}

/// Information for a flashblock currently built
#[derive(Debug, Clone, Copy)]
pub struct FlashBlockBuildInfo {
    /// Parent block hash
    pub parent_hash: B256,
    /// Flashblock index within the current block's sequence
    pub index: u64,
    /// Block number of the flashblock being built.
    pub block_number: u64,
}

type BuildJob<N> =
    (Instant, oneshot::Receiver<eyre::Result<Option<(PendingFlashBlock<N>, CachedReads)>>>);

#[derive(Metrics)]
#[metrics(scope = "flashblock_service")]
struct FlashBlockServiceMetrics {
    /// The last complete length of flashblocks per block.
    last_flashblock_length: Histogram,
    /// The duration applying flashblock state changes in seconds.
    execution_duration: Histogram,
}
