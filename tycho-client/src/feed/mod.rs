use std::{
    collections::{HashMap, HashSet},
    fmt::{Display, Formatter},
    time::Duration,
};

use chrono::{Duration as ChronoDuration, Local, NaiveDateTime};
use futures03::{
    future::{join_all, try_join_all},
    stream::FuturesUnordered,
    StreamExt,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{
        mpsc::{self, Receiver},
        oneshot,
    },
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, error, info, trace, warn};
use tycho_common::{
    display::opt,
    dto::{Block, ExtractorIdentity},
    Bytes,
};

use crate::feed::{
    block_history::{BlockHistory, BlockHistoryError, BlockPosition},
    synchronizer::{StateSyncMessage, StateSynchronizer, SyncResult, SynchronizerError},
};

mod block_history;
pub mod component_tracker;
pub mod synchronizer;

/// A trait representing a minimal interface for types that behave like a block header.
///
/// This abstraction allows working with either full block headers (`BlockHeader`)
/// or simplified structures that only provide a timestamp (e.g., for RFQ logic).
pub trait HeaderLike {
    fn block(self) -> Option<BlockHeader>;
    fn block_number_or_timestamp(self) -> u64;
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, Eq, Hash)]
pub struct BlockHeader {
    pub hash: Bytes,
    pub number: u64,
    pub parent_hash: Bytes,
    pub revert: bool,
    pub timestamp: u64,
}

impl Display for BlockHeader {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // Take first 6 hex chars of the hash for readability
        let short_hash = if self.hash.len() >= 4 {
            hex::encode(&self.hash[..4]) // 4 bytes → 8 hex chars
        } else {
            hex::encode(&self.hash)
        };

        write!(f, "Block #{} [0x{}..]", self.number, short_hash)
    }
}

impl BlockHeader {
    fn from_block(block: &Block, revert: bool) -> Self {
        Self {
            hash: block.hash.clone(),
            number: block.number,
            parent_hash: block.parent_hash.clone(),
            revert,
            timestamp: block.ts.and_utc().timestamp() as u64,
        }
    }
}

impl HeaderLike for BlockHeader {
    fn block(self) -> Option<BlockHeader> {
        Some(self)
    }

    fn block_number_or_timestamp(self) -> u64 {
        self.number
    }
}

#[derive(Error, Debug)]
pub enum BlockSynchronizerError {
    #[error("Failed to initialize synchronizer: {0}")]
    InitializationError(#[from] SynchronizerError),

    #[error("Failed to process new block: {0}")]
    BlockHistoryError(#[from] BlockHistoryError),

    #[error("Not a single synchronizer was ready: {0}")]
    NoReadySynchronizers(String),

    #[error("No synchronizers were set")]
    NoSynchronizers,

    #[error("Failed to convert duration: {0}")]
    DurationConversionError(String),
}

type BlockSyncResult<T> = Result<T, BlockSynchronizerError>;

/// Aligns multiple StateSynchronizers on the block dimension.
///
/// ## Purpose
/// The purpose of this component is to handle streams from multiple state synchronizers and
/// align/merge them according to their blocks. Ideally this should be done in a fault-tolerant way,
/// meaning we can recover from a state synchronizer suffering from timing issues. E.g. a delayed or
/// unresponsive state synchronizer might recover again, or an advanced state synchronizer can be
/// included again once we reach the block it is at.
///
/// ## Limitations
/// - Supports only chains with fixed blocks time for now due to the lock step mechanism.
///
/// ## Initialisation
/// Queries all registered synchronizers for their first message and evaluates the state of each
/// synchronizer. If a synchronizer's first message is an older block, it is marked as delayed.
// TODO: what is the startup timeout
/// If no message is received within the startup timeout, the synchronizer is marked as stale and is
/// closed.
///
/// ## Main loop
/// Once started, the synchronizers are queried concurrently for messages in lock step:
/// the main loop queries all synchronizers in ready for the last emitted data, builds the
/// `FeedMessage` and emits it, then it schedules the wait procedure for the next block.
///
/// ## Synchronization Logic
///
/// To classify a synchronizer as delayed, we need to first define the current block. The highest
/// block number of all ready synchronizers is considered the current block.
///
/// Once we have the current block we can easily determine which block we expect next. And if a
/// synchronizer delivers an older block we can classify it as delayed.
///
/// If any synchronizer is not in the ready state we will try to bring it back to the ready state.
/// This is done by trying to empty any buffers of a delayed synchronizer or waiting to reach
/// the height of an advanced synchronizer (and flagging it as such in the meantime).
///
/// Of course, we can't wait forever for a synchronizer to reply/recover. All of this must happen
/// within the block production step of the blockchain:
/// The wait procedure consists of waiting for any of the individual ProtocolStateSynchronizers
/// to emit a new message (within a max timeout - several multiples of the block time). Once a
/// message is received a very short timeout starts for the remaining synchronizers, to deliver a
/// message. Any synchronizer failing to do so is transitioned to delayed.
///
/// ### Note
/// The described process above is the goal. It is currently not implemented like that. Instead we
/// simply wait `block_time` + `wait_time`. Synchronizers are expected to respond within that
/// timeout. This is simpler but only works well on chains with fixed block times.
pub struct BlockSynchronizer<S> {
    synchronizers: Option<HashMap<ExtractorIdentity, S>>,
    /// Time to wait for a block usually
    block_time: std::time::Duration,
    /// Added on top of block time to account for latency
    latency_buffer: std::time::Duration,
    /// Time to wait for the full first message, including snapshot retrieval
    startup_timeout: std::time::Duration,
    /// Optionally, end the stream after emitting max messages
    max_messages: Option<usize>,
    /// Amount of blocks a protocol can be delayed for, before it is considered stale
    max_missed_blocks: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum SynchronizerState {
    /// Initial state, assigned before trying to receive any message
    Started,
    /// The synchronizer emitted a message for the block as expected
    Ready(BlockHeader),
    /// The synchronizer is on a previous block compared to others and is expected to
    /// catch up soon.
    Delayed(BlockHeader),
    /// The synchronizer hasn't emitted messages for > `max_missed_blocks` or has
    /// fallen far behind. At this point we do not wait for it anymore but it can
    /// still eventually recover.
    Stale(BlockHeader),
    /// The synchronizer is on future not connected block.
    // For this to happen we must have a gap, and a gap usually means a new snapshot from the
    // StateSynchronizer. This can only happen if we are processing too slow and one or all of the
    // synchronizers restarts e.g. due to websocket connection drops.
    Advanced(BlockHeader),
    /// The synchronizer ended with an error.
    Ended(String),
}

impl Display for SynchronizerState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SynchronizerState::Started => write!(f, "Started"),

            SynchronizerState::Ready(b) => write!(f, "Started({})", b.number),
            SynchronizerState::Delayed(b) => write!(f, "Delayed({})", b.number),
            SynchronizerState::Stale(b) => write!(f, "Stale({})", b.number),
            SynchronizerState::Advanced(b) => write!(f, "Advanced({})", b.number),
            SynchronizerState::Ended(reason) => write!(f, "Ended({})", reason),
        }
    }
}

pub struct SynchronizerStream {
    extractor_id: ExtractorIdentity,
    state: SynchronizerState,
    error: Option<SynchronizerError>,
    modify_ts: NaiveDateTime,
    rx: Receiver<SyncResult<StateSyncMessage<BlockHeader>>>,
}

impl SynchronizerStream {
    fn new(
        extractor_id: &ExtractorIdentity,
        rx: Receiver<SyncResult<StateSyncMessage<BlockHeader>>>,
    ) -> Self {
        Self {
            extractor_id: extractor_id.clone(),
            state: SynchronizerState::Started,
            error: None,
            modify_ts: Local::now().naive_utc(),
            rx,
        }
    }
    async fn try_advance(
        &mut self,
        block_history: &BlockHistory,
        block_time: std::time::Duration,
        latency_buffer: std::time::Duration,
        stale_threshold: std::time::Duration,
    ) -> BlockSyncResult<Option<StateSyncMessage<BlockHeader>>> {
        let extractor_id = self.extractor_id.clone();
        let latest_block = block_history.latest();

        match &self.state {
            SynchronizerState::Started | SynchronizerState::Ended(_) => {
                warn!(state=?&self.state, "Advancing Synchronizer in this state not supported!");
                Ok(None)
            }
            SynchronizerState::Advanced(b) => {
                let future_block = b.clone();
                // Transition to ready once we arrived at the expected height
                self.transition(future_block, block_history, stale_threshold)?;
                Ok(None)
            }
            SynchronizerState::Ready(previous_block) => {
                // Try to recv the next expected block, update state accordingly.
                self.try_recv_next_expected(
                    block_time + latency_buffer,
                    block_history,
                    previous_block.clone(),
                    stale_threshold,
                )
                .await
            }
            SynchronizerState::Delayed(old_block) => {
                // try to catch up all currently queued blocks until the expected block
                debug!(
                    %old_block,
                    latest_block=opt(&latest_block),
                    %extractor_id,
                    "Trying to catch up to latest block"
                );
                self.try_catch_up(block_history, block_time + latency_buffer, stale_threshold)
                    .await
            }
            SynchronizerState::Stale(old_block) => {
                // try to catch up all currently queued blocks until the expected block
                debug!(
                    %old_block,
                    latest_block=opt(&latest_block),
                    %extractor_id,
                    "Trying to catch up stale synchronizer to latest block"
                );
                self.try_catch_up(block_history, block_time, stale_threshold)
                    .await
            }
        }
    }

    /// Standard way to advance a well-behaved state synchronizer.
    ///
    /// Will wait for a new block on the synchronizer within a timeout. And modify its state based
    /// on the outcome.
    async fn try_recv_next_expected(
        &mut self,
        max_wait: std::time::Duration,
        block_history: &BlockHistory,
        previous_block: BlockHeader,
        stale_threshold: std::time::Duration,
    ) -> BlockSyncResult<Option<StateSyncMessage<BlockHeader>>> {
        let extractor_id = self.extractor_id.clone();
        match timeout(max_wait, self.rx.recv()).await {
            Ok(Some(Ok(msg))) => {
                self.transition(msg.header.clone(), block_history, stale_threshold)?;
                Ok(Some(msg))
            }
            Ok(Some(Err(e))) => {
                // The underlying synchronizer exhausted its retries
                self.mark_errored(e);
                Ok(None)
            }
            Ok(None) => {
                // This case should not happen, as we shouldn't poll the synchronizer after we
                // closed it or after it errored.
                warn!(
                    %extractor_id,
                    "Tried to poll from closed synchronizer.",
                );
                self.mark_closed();
                Ok(None)
            }
            Err(_) => {
                // trying to advance a block timed out
                debug!(%extractor_id, %previous_block, "No block received within time limit.");

                // TODO: as per prev condition check, the state will always be always ready here -
                // simplify
                match &self.state {
                    SynchronizerState::Ready(_) => {
                        // First timeout: always transition to Delayed
                        self.state = SynchronizerState::Delayed(previous_block.clone());
                        self.modify_ts = Local::now().naive_utc();
                    }
                    SynchronizerState::Delayed(_) => {
                        // Already delayed, check if we should go stale
                        // DON'T update modify_ts here - we want to track time since first delay
                        self.check_and_transition_to_stale_if_needed(
                            stale_threshold,
                            Some(previous_block.clone()),
                        )?;
                    }
                    _ => {
                        // For other states, use the stale check
                        if !self.check_and_transition_to_stale_if_needed(
                            stale_threshold,
                            Some(previous_block.clone()),
                        )? {
                            self.state = SynchronizerState::Delayed(previous_block.clone());
                            self.modify_ts = Local::now().naive_utc();
                        }
                    }
                }
                Ok(None)
            }
        }
    }

    /// Tries to catch up a delayed state synchronizer.
    ///
    /// If a synchronizer is delayed, this method will try to catch up to the next expected block
    /// by consuming all waiting messages in its queue and waiting for any new block messages
    /// within a timeout. Finally, all update messages are merged into one and returned.
    async fn try_catch_up(
        &mut self,
        block_history: &BlockHistory,
        max_wait: std::time::Duration,
        stale_threshold: std::time::Duration,
    ) -> BlockSyncResult<Option<StateSyncMessage<BlockHeader>>> {
        let mut results = Vec::new();
        let extractor_id = self.extractor_id.clone();

        // Set a deadline for the overall catch-up operation
        let deadline = std::time::Instant::now() + max_wait;

        while std::time::Instant::now() < deadline {
            match timeout(
                deadline.saturating_duration_since(std::time::Instant::now()),
                self.rx.recv(),
            )
            .await
            {
                Ok(Some(Ok(msg))) => {
                    debug!(%extractor_id, block=%msg.header, "Received new message during catch-up");
                    let block_pos = block_history.determine_block_position(&msg.header)?;
                    results.push(msg);
                    if matches!(block_pos, BlockPosition::NextExpected) {
                        break;
                    }
                }
                Ok(Some(Err(e))) => {
                    // Synchronizer errored during catch up
                    self.mark_errored(e);
                    return Ok(None);
                }
                Ok(None) => {
                    // This case should not happen, as we shouldn't poll the synchronizer after we
                    // closed it or after it errored.
                    warn!(
                        %extractor_id,
                        "Tried to poll from closed synchronizer during catch up.",
                    );
                    self.mark_closed();
                    return Ok(None)
                }
                Err(_) => {
                    debug!(%extractor_id, "Timed out waiting for catch-up");
                    break;
                }
            }
        }

        let merged = results
            .into_iter()
            .reduce(|l, r| l.merge(r));

        if let Some(msg) = merged {
            // we were able to get at least one block out
            debug!(%extractor_id, "Delayed extractor made progress!");
            self.transition(msg.header.clone(), block_history, stale_threshold)?;
            Ok(Some(msg))
        } else {
            // No progress made during catch-up, check if we should go stale
            self.check_and_transition_to_stale_if_needed(stale_threshold, None)?;
            Ok(None)
        }
    }

    /// Helper method to check if synchronizer should transition to stale based on time elapsed
    fn check_and_transition_to_stale_if_needed(
        &mut self,
        stale_threshold: std::time::Duration,
        fallback_header: Option<BlockHeader>,
    ) -> Result<bool, BlockSynchronizerError> {
        let now = Local::now().naive_utc();
        let wait_duration = now.signed_duration_since(self.modify_ts);
        let stale_threshold_chrono = ChronoDuration::from_std(stale_threshold)
            .map_err(|e| BlockSynchronizerError::DurationConversionError(e.to_string()))?;

        if wait_duration > stale_threshold_chrono {
            let header_to_use = match (&self.state, fallback_header) {
                (SynchronizerState::Ready(h), _) |
                (SynchronizerState::Delayed(h), _) |
                (SynchronizerState::Stale(h), _) => h.clone(),
                (_, Some(h)) => h,
                _ => BlockHeader::default(),
            };

            warn!(
                extractor_id=%self.extractor_id,
                last_message_at=?self.modify_ts,
                "SynchronizerStream transition to stale due to timeout."
            );
            self.state = SynchronizerState::Stale(header_to_use);
            self.modify_ts = now;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Logic to transition a state synchronizer based on newly received block
    ///
    /// Updates the synchronizer's state according to the position of the received block:
    /// - Next expected block -> Ready state
    /// - Latest/Delayed block -> Either Delayed or Stale (if >60s since last update)
    /// - Advanced block -> Advanced state (block ahead of expected position)
    fn transition(
        &mut self,
        latest_retrieved: BlockHeader,
        block_history: &BlockHistory,
        stale_threshold: std::time::Duration,
    ) -> Result<(), BlockSynchronizerError> {
        let extractor_id = self.extractor_id.clone();
        let last_message_at = self.modify_ts;
        let block = &latest_retrieved;

        match block_history.determine_block_position(&latest_retrieved)? {
            BlockPosition::NextExpected => {
                self.state = SynchronizerState::Ready(latest_retrieved.clone());
                trace!(
                    next = %latest_retrieved,
                    extractor = %extractor_id,
                    "SynchronizerStream transition to next expected"
                )
            }
            BlockPosition::Latest | BlockPosition::Delayed => {
                if !self.check_and_transition_to_stale_if_needed(
                    stale_threshold,
                    Some(latest_retrieved.clone()),
                )? {
                    warn!(
                        %extractor_id,
                        ?last_message_at,
                        %block,
                        "SynchronizerStream transition transition to delayed."
                    );
                    self.state = SynchronizerState::Delayed(latest_retrieved.clone());
                }
            }
            BlockPosition::Advanced => {
                info!(
                    %extractor_id,
                    ?last_message_at,
                    latest = opt(&block_history.latest()),
                    %block,
                    "SynchronizerStream transition to advanced."
                );
                self.state = SynchronizerState::Advanced(latest_retrieved.clone());
            }
        }
        self.modify_ts = Local::now().naive_utc();
        Ok(())
    }

    /// Marks this stream as errored
    ///
    /// Sets an error and transitions the stream to Ended, correctly recording the
    /// time at which this happened.
    fn mark_errored(&mut self, error: SynchronizerError) {
        self.state = SynchronizerState::Ended(error.to_string());
        self.modify_ts = Local::now().naive_utc();
        self.error = Some(error);
    }

    /// Marks a stream as closed.
    ///
    /// If the stream has not been ended previously, e.g. by an error it will be marked
    /// as Ended without error. This should not happen since we should stop consuming
    /// from the stream if an error occured.
    fn mark_closed(&mut self) {
        if !matches!(self.state, SynchronizerState::Ended(_)) {
            self.state = SynchronizerState::Ended("Closed".to_string());
            self.modify_ts = Local::now().naive_utc();
        }
    }

    /// Marks a stream as stale.
    fn mark_stale(&mut self, header: &BlockHeader) {
        self.state = SynchronizerState::Stale(header.clone());
        self.modify_ts = Local::now().naive_utc();
    }

    /// Marks this stream as ready.
    fn mark_ready(&mut self, header: &BlockHeader) {
        self.state = SynchronizerState::Ready(header.clone());
        self.modify_ts = Local::now().naive_utc();
    }

    fn has_ended(&self) -> bool {
        matches!(self.state, SynchronizerState::Ended(_))
    }

    fn is_stale(&self) -> bool {
        matches!(self.state, SynchronizerState::Stale(_))
    }

    fn is_advanced(&self) -> bool {
        matches!(self.state, SynchronizerState::Advanced(_))
    }

    /// Gets the streams current header from active streams.
    ///
    /// A stream is considered as active unless it has ended or is stale.
    fn get_current_header(&self) -> Option<&BlockHeader> {
        match &self.state {
            SynchronizerState::Ready(b) |
            SynchronizerState::Delayed(b) |
            SynchronizerState::Advanced(b) => Some(b),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct FeedMessage<H = BlockHeader>
where
    H: HeaderLike,
{
    pub state_msgs: HashMap<String, StateSyncMessage<H>>,
    pub sync_states: HashMap<String, SynchronizerState>,
}

impl<H> FeedMessage<H>
where
    H: HeaderLike,
{
    fn new(
        state_msgs: HashMap<String, StateSyncMessage<H>>,
        sync_states: HashMap<String, SynchronizerState>,
    ) -> Self {
        Self { state_msgs, sync_states }
    }
}

impl<S> BlockSynchronizer<S>
where
    S: StateSynchronizer,
{
    pub fn new(
        block_time: std::time::Duration,
        latency_buffer: std::time::Duration,
        max_missed_blocks: u64,
    ) -> Self {
        Self {
            synchronizers: None,
            max_messages: None,
            block_time,
            latency_buffer,
            startup_timeout: block_time.mul_f64(max_missed_blocks as f64),
            max_missed_blocks,
        }
    }

    /// Limits the stream to emit a maximum number of messages.
    ///
    /// After the stream emitted max messages it will end. This is only useful for
    /// testing purposes or if you only want to process a fixed amount of messages
    /// and then terminate cleanly.
    pub fn max_messages(&mut self, val: usize) {
        self.max_messages = Some(val);
    }

    /// Sets timeout for the first message of a protocol.
    ///
    /// Time to wait for the full first message, including snapshot retrieval.
    pub fn startup_timeout(mut self, val: Duration) {
        self.startup_timeout = val;
    }

    pub fn register_synchronizer(mut self, id: ExtractorIdentity, synchronizer: S) -> Self {
        let mut registered = self.synchronizers.unwrap_or_default();
        registered.insert(id, synchronizer);
        self.synchronizers = Some(registered);
        self
    }

    #[cfg(test)]
    pub fn with_short_timeouts() -> Self {
        Self::new(Duration::from_millis(10), Duration::from_millis(10), 3)
    }

    /// Cleanup function for shutting down remaining synchronizers when the nanny detects an error.
    /// Sends close signals to all remaining synchronizers and waits for them to complete.
    async fn cleanup_synchronizers(
        mut state_sync_tasks: FuturesUnordered<JoinHandle<()>>,
        sync_close_senders: Vec<oneshot::Sender<()>>,
    ) {
        // Send close signals to all remaining synchronizers
        for close_sender in sync_close_senders {
            let _ = close_sender.send(());
        }

        // Await remaining tasks with timeout
        let mut completed_tasks = 0;
        while let Ok(Some(_)) = timeout(Duration::from_secs(5), state_sync_tasks.next()).await {
            completed_tasks += 1;
        }

        // Warn if any synchronizers timed out during cleanup
        let remaining_tasks = state_sync_tasks.len();
        if remaining_tasks > 0 {
            warn!(
                completed = completed_tasks,
                timed_out = remaining_tasks,
                "Some synchronizers timed out during cleanup and may not have shut down cleanly"
            );
        }
    }

    /// Starts the synchronization of streams.
    ///
    /// Will error directly if the startup fails. Once the startup is complete, it will
    /// communicate any fatal errors through the stream before closing it.
    pub async fn run(
        mut self,
    ) -> BlockSyncResult<(JoinHandle<()>, Receiver<BlockSyncResult<FeedMessage<BlockHeader>>>)>
    {
        trace!("Starting BlockSynchronizer...");
        let state_sync_tasks = FuturesUnordered::new();
        let mut synchronizers = self
            .synchronizers
            .take()
            .ok_or(BlockSynchronizerError::NoSynchronizers)?;
        // init synchronizers
        let init_tasks = synchronizers
            .values_mut()
            .map(|s| s.initialize())
            .collect::<Vec<_>>();
        try_join_all(init_tasks).await?;

        let mut sync_streams = HashMap::with_capacity(synchronizers.len());
        let mut sync_close_senders = Vec::new();
        for (extractor_id, synchronizer) in synchronizers.drain() {
            let (handle, rx) = synchronizer.start().await;
            let (join_handle, close_sender) = handle.split();
            state_sync_tasks.push(join_handle);
            sync_close_senders.push(close_sender);

            sync_streams.insert(extractor_id.clone(), SynchronizerStream::new(&extractor_id, rx));
        }

        // startup, schedule first set of futures and wait for them to return to initialise
        // synchronizers.
        debug!("Waiting for initial synchronizer messages...");
        let mut startup_futures = Vec::new();
        for (id, sh) in sync_streams.iter_mut() {
            let fut = async {
                let res = timeout(self.startup_timeout, sh.rx.recv()).await;
                (id.clone(), res)
            };
            startup_futures.push(fut);
        }
        let mut ready_sync_msgs = HashMap::new();
        let initial_headers = join_all(startup_futures)
            .await
            .into_iter()
            .filter_map(|(extractor_id, res)| {
                let synchronizer = sync_streams
                .get_mut(&extractor_id)
                .unwrap();
            match res {
                Ok(Some(Ok(msg))) => {
                    debug!(%extractor_id, height=?&msg.header.number, "Synchronizer started successfully!");
                    // initially default all synchronizers to Ready
                    synchronizer.mark_ready(&msg.header);
                    ready_sync_msgs.insert(extractor_id.name.clone(), msg.clone());
                    Some(msg.header)
                }
                Ok(Some(Err(e))) => {
                    synchronizer.mark_errored(e);
                    None
                }
                Ok(None) => {
                    // Synchronizer closed channel. This can only happen if the run
                    // task ended, before this, the synchronizer should have sent
                    // an error, so this case we likely don't have to handle that
                    // explicitly
                    warn!(%extractor_id, "Synchronizer closed during startup");
                    synchronizer.mark_closed();
                    None
                }
                Err(_) => {
                    // We got an error because the synchronizer timed out during startup
                    warn!(%extractor_id, "Timed out waiting for first message");
                    synchronizer.mark_stale(&BlockHeader::default());
                    None
                }
            }
        })
        .collect::<HashSet<_>>() // remove duplicates
        .into_iter()
        .collect::<Vec<_>>();

        // Ensures we have at least one ready stream
        Self::check_streams(&sync_streams)?;
        let mut block_history = BlockHistory::new(initial_headers, 15)?;
        // Determine the starting header for synchronization
        let start_header = block_history
            .latest()
            .expect("Safe since we checked streams before");
        info!(
            start_block=%start_header,
            n_healthy=ready_sync_msgs.len(),
            n_total=sync_streams.len(),
            "Block synchronization started successfully!"
        );

        // Determine correct state for each remaining synchronizer, based on their header vs the
        // latest one
        for (_, stream) in sync_streams.iter_mut() {
            if let SynchronizerState::Ready(header) = stream.state.clone() {
                if header.number < start_header.number {
                    debug!(
                        extractor_id=%stream.extractor_id,
                        synchronizer_block=header.number,
                        current_block=start_header.number,
                        "Marking synchronizer as delayed during initialization"
                    );
                    stream.state = SynchronizerState::Delayed(header);
                }
            }
        }

        let (sync_tx, sync_rx) = mpsc::channel(30);
        let main_loop_jh = tokio::spawn(async move {
            let mut n_iter = 1;
            loop {
                // Send retrieved data to receivers.
                let msg = FeedMessage::new(
                    std::mem::take(&mut ready_sync_msgs),
                    sync_streams
                        .iter()
                        .map(|(a, b)| (a.name.to_string(), b.state.clone()))
                        .collect(),
                );
                if sync_tx.send(Ok(msg)).await.is_err() {
                    info!("Receiver closed, block synchronizer terminating..");
                    return;
                };

                // Check if we have reached the max messages
                if let Some(max_messages) = self.max_messages {
                    if n_iter >= max_messages {
                        info!(max_messages, "StreamEnd");
                        return;
                    }
                }
                n_iter += 1;

                let res = self
                    .handle_next_message(
                        &mut sync_streams,
                        &mut ready_sync_msgs,
                        &mut block_history,
                    )
                    .await;

                if let Err(e) = res {
                    // Communicate error to clients, then end the loop
                    let _ = sync_tx.send(Err(e)).await;
                    return;
                }
            }
        });

        // We await the main loop and log any panics (should be impossible). If the
        // main loop exits, all synchronizers should be ended or stale. So we kill any
        // remaining stale ones just in case. A final error is propagated through the
        // channel to the user.
        let nanny_jh = tokio::spawn(async move {
            // report any panics
            let _ = main_loop_jh.await.map_err(|e| {
                if e.is_panic() {
                    error!("BlockSynchornizer main loop panicked: {e}")
                }
            });
            debug!("Main loop exited. Closing synchronizers");
            Self::cleanup_synchronizers(state_sync_tasks, sync_close_senders).await;
            debug!("Shutdown complete");
        });
        Ok((nanny_jh, sync_rx))
    }

    /// Retrieves next message from synchronizers
    ///
    /// The result is written into `ready_sync_messages`. Errors only if there is a
    /// non-recoverable error or all synchronizers have ended.
    async fn handle_next_message(
        &self,
        sync_streams: &mut HashMap<ExtractorIdentity, SynchronizerStream>,
        ready_sync_msgs: &mut HashMap<String, StateSyncMessage<BlockHeader>>,
        block_history: &mut BlockHistory,
    ) -> BlockSyncResult<()> {
        let mut recv_futures = Vec::new();
        for (extractor_id, stream) in sync_streams.iter_mut() {
            // If stream is in ended state, do not check for any messages (it's receiver
            // is closed), but do check stale streams.
            if stream.has_ended() {
                continue
            }
            // Here we simply wait block_time + max_wait. This will not work for chains with
            // unknown block times but is simple enough for now.
            // If we would like to support unknown block times we could: Instruct all handles to
            // await the max block time, if a header arrives within that time transition as
            // usual, but via a select statement get notified (using e.g. Notify) if any other
            // handle finishes before the timeout. Then await again but this time only for
            // max_wait and then proceed as usual. So basically each try_advance task would have
            // a select statement that allows it to exit the first timeout preemptively if any
            // other try_advance task finished earlier.
            recv_futures.push(async {
                let res = stream
                    .try_advance(
                        block_history,
                        self.block_time,
                        self.latency_buffer,
                        self.block_time
                            .mul_f64(self.max_missed_blocks as f64),
                    )
                    .await?;
                Ok::<_, BlockSynchronizerError>(res.map(|msg| (extractor_id.name.clone(), msg)))
            });
        }
        ready_sync_msgs.extend(
            join_all(recv_futures)
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten(),
        );

        // Check if we have any active synchronizers (Ready, Delayed, or Advanced)
        // If all synchronizers have been purged (Stale/Ended), exit the main loop
        Self::check_streams(sync_streams)?;

        // if we have any advanced header, we reinit the block history,
        // else we simply advance the existing history
        if sync_streams
            .values()
            .any(SynchronizerStream::is_advanced)
        {
            *block_history = Self::reinit_block_history(sync_streams, block_history)?;
        } else {
            let header = sync_streams
                .values()
                .filter_map(SynchronizerStream::get_current_header)
                .max_by_key(|b| b.number)
                .expect("Active streams are present, since we checked above");
            block_history.push(header.clone())?;
        }
        Ok(())
    }

    /// Reinitialise block history and reclassifies active synchronizers states.
    ///
    /// We call this if we detect a future detached block. This usually only happens if
    /// a synchronizer has a restart.
    fn reinit_block_history(
        sync_streams: &mut HashMap<ExtractorIdentity, SynchronizerStream>,
        block_history: &mut BlockHistory,
    ) -> Result<BlockHistory, BlockSynchronizerError> {
        let previous = block_history
            .latest()
            .expect("Old block history is not empty, startup finished at this point");
        let blocks = sync_streams
            .values()
            .filter_map(SynchronizerStream::get_current_header)
            .cloned()
            .collect();
        let new_block_history = BlockHistory::new(blocks, 10)?;
        let latest = block_history
            .latest()
            .expect("block history is not empty");
        info!(
             %previous,
            %latest,
            "Advanced synchronizer detected. Reinitialized block history."
        );
        sync_streams
            .values_mut()
            .for_each(|stream| {
                // we only get headers from, advanced, ready and delayed so stale
                // or ended streams are not considered here
                if let Some(header) = stream.get_current_header() {
                    if header.number < latest.number {
                        stream.state = SynchronizerState::Delayed(header.clone());
                    } else if header.number == latest.number {
                        stream.state = SynchronizerState::Ready(header.clone());
                    }
                }
            });
        Ok(new_block_history)
    }

    /// Checks if we still have at least one active stream else errors.
    ///
    /// If there are not active streams meaning all  areended or stale, it returns a
    /// summary error message for the state of all synchronizers.
    fn check_streams(
        sync_streams: &HashMap<ExtractorIdentity, SynchronizerStream>,
    ) -> BlockSyncResult<()> {
        if sync_streams
            .values()
            .all(|stream| stream.has_ended() | stream.is_stale())
        {
            let mut reason = Vec::new();
            if let Some((last_errored_id, last_errored_stream)) = sync_streams
                .iter()
                .filter(|(_, stream)| stream.has_ended() | stream.is_stale())
                .max_by_key(|(_, stream)| stream.modify_ts)
            {
                if let Some(err) = &last_errored_stream.error {
                    // All synchronizers were errored/stale and the last one errored
                    reason.push(format!("Synchronizer for {last_errored_id} errored with: {err}"))
                } else {
                    // All synchronizer were errored/stale and the last one also becomae stale
                    reason.push(format!(
                        "Synchronizer for {last_errored_id} became: {}",
                        last_errored_stream.state
                    ))
                }
            } else {
                reason.push(
                    "Can't identify protocol that caused the stream to end! \
                    This condition should be unreachable!"
                        .to_string(),
                )
            }

            sync_streams
                .iter()
                .for_each(|(id, stream)| {
                    reason
                        .push(format!("{id} reported as {} at {}", stream.state, stream.modify_ts))
                });

            return Err(BlockSynchronizerError::NoReadySynchronizers(reason.join(", ")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use test_log::test;
    use tokio::sync::{oneshot, Mutex};
    use tycho_common::dto::Chain;

    use super::*;
    use crate::feed::synchronizer::{SyncResult, SynchronizerTaskHandle};

    #[derive(Clone, Debug)]
    enum MockBehavior {
        Normal,          // Exit successfully when receiving close signal
        IgnoreClose,     // Ignore close signals and hang (for timeout testing)
        ExitImmediately, // Exit immediately after first message (for quick failure testing)
    }

    type HeaderReceiver = Receiver<SyncResult<StateSyncMessage<BlockHeader>>>;

    #[derive(Clone)]
    struct MockStateSync {
        header_tx: mpsc::Sender<SyncResult<StateSyncMessage<BlockHeader>>>,
        header_rx: Arc<Mutex<Option<HeaderReceiver>>>,
        close_received: Arc<Mutex<bool>>,
        behavior: MockBehavior,
        // For testing: store the close sender so tests can trigger close signals
        close_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    }

    impl MockStateSync {
        fn new() -> Self {
            Self::with_behavior(MockBehavior::Normal)
        }

        fn with_behavior(behavior: MockBehavior) -> Self {
            let (tx, rx) = mpsc::channel(1);
            Self {
                header_tx: tx,
                header_rx: Arc::new(Mutex::new(Some(rx))),
                close_received: Arc::new(Mutex::new(false)),
                behavior,
                close_tx: Arc::new(Mutex::new(None)),
            }
        }

        async fn was_close_received(&self) -> bool {
            *self.close_received.lock().await
        }

        async fn send_header(&self, header: StateSyncMessage<BlockHeader>) -> Result<(), String> {
            self.header_tx
                .send(Ok(header))
                .await
                .map_err(|e| format!("sending header failed: {e}"))
        }

        // For testing: trigger a close signal to make the synchronizer exit
        async fn trigger_close(&self) {
            if let Some(close_tx) = self.close_tx.lock().await.take() {
                let _ = close_tx.send(());
            }
        }
    }

    #[async_trait]
    impl StateSynchronizer for MockStateSync {
        async fn initialize(&mut self) -> SyncResult<()> {
            Ok(())
        }

        async fn start(
            mut self,
        ) -> (SynchronizerTaskHandle, Receiver<SyncResult<StateSyncMessage<BlockHeader>>>) {
            let block_rx = {
                let mut guard = self.header_rx.lock().await;
                guard
                    .take()
                    .expect("Block receiver was not set!")
            };

            // Create close channel - we need to store one sender for testing and give one to the
            // handle
            let (close_tx_for_handle, close_rx) = oneshot::channel();
            let (close_tx_for_test, close_rx_for_test) = oneshot::channel();

            // Store the test close sender
            {
                let mut guard = self.close_tx.lock().await;
                *guard = Some(close_tx_for_test);
            }

            let behavior = self.behavior.clone();
            let close_received_clone = self.close_received.clone();
            let tx = self.header_tx.clone();

            let jh = tokio::spawn(async move {
                match behavior {
                    MockBehavior::IgnoreClose => {
                        // Infinite loop to simulate a hung synchronizer that doesn't respond to
                        // close signals
                        loop {
                            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        }
                    }
                    MockBehavior::ExitImmediately => {
                        // Exit immediately with error to simulate immediate task failure
                        tx.send(SyncResult::Err(SynchronizerError::ConnectionError(
                            "Simulated immediate task failure".to_string(),
                        )))
                        .await
                        .unwrap();
                    }
                    MockBehavior::Normal => {
                        // Wait for close signal from either handle or test, then respond based on
                        // behavior
                        let _ = tokio::select! {
                            result = close_rx => result,
                            result = close_rx_for_test => result,
                        };
                        let mut guard = close_received_clone.lock().await;
                        *guard = true;
                    }
                }
            });

            let handle = SynchronizerTaskHandle::new(jh, close_tx_for_handle);
            (handle, block_rx)
        }
    }

    fn header_message(block: u8) -> StateSyncMessage<BlockHeader> {
        StateSyncMessage {
            header: BlockHeader {
                number: block as u64,
                hash: Bytes::from(vec![block]),
                parent_hash: Bytes::from(vec![block - 1]),
                revert: false,
                timestamp: 1000,
            },
            ..Default::default()
        }
    }

    async fn receive_message(rx: &mut Receiver<BlockSyncResult<FeedMessage>>) -> FeedMessage {
        timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("Responds in time")
            .expect("Should receive first message")
            .expect("No error")
    }

    async fn setup_block_sync(
    ) -> (MockStateSync, MockStateSync, JoinHandle<()>, Receiver<BlockSyncResult<FeedMessage>>)
    {
        setup_block_sync_with_behaviour(MockBehavior::Normal, MockBehavior::Normal).await
    }

    // Starts up a synchronizer and consumes the first message on block 1.
    async fn setup_block_sync_with_behaviour(
        v2_behavior: MockBehavior,
        v3_behavior: MockBehavior,
    ) -> (MockStateSync, MockStateSync, JoinHandle<()>, Receiver<BlockSyncResult<FeedMessage>>)
    {
        let v2_sync = MockStateSync::with_behavior(v2_behavior);
        let v3_sync = MockStateSync::with_behavior(v3_behavior);

        // Use reasonable timeouts to observe proper state transitions
        let mut block_sync = BlockSynchronizer::new(
            Duration::from_millis(20), // block_time
            Duration::from_millis(10), // max_wait
            3,                         // max_missed_blocks (stale threshold = 20ms * 3 = 60ms)
        );
        block_sync.max_messages(10); // Allow enough messages to see the progression

        let block_sync = block_sync
            .register_synchronizer(
                ExtractorIdentity { chain: Chain::Ethereum, name: "uniswap-v2".to_string() },
                v2_sync.clone(),
            )
            .register_synchronizer(
                ExtractorIdentity { chain: Chain::Ethereum, name: "uniswap-v3".to_string() },
                v3_sync.clone(),
            );

        // Send initial messages to both synchronizers
        let block1_msg = header_message(1);
        let _ = v2_sync
            .send_header(block1_msg.clone())
            .await;
        let _ = v3_sync
            .send_header(block1_msg.clone())
            .await;

        // Start the block synchronizer
        let (nanny_handle, mut rx) = block_sync
            .run()
            .await
            .expect("BlockSynchronizer failed to start");

        let first_feed_msg = receive_message(&mut rx).await;
        assert_eq!(first_feed_msg.state_msgs.len(), 2);
        assert!(matches!(
            first_feed_msg
                .sync_states
                .get("uniswap-v2")
                .unwrap(),
            SynchronizerState::Ready(_)
        ));
        assert!(matches!(
            first_feed_msg
                .sync_states
                .get("uniswap-v3")
                .unwrap(),
            SynchronizerState::Ready(_)
        ));

        (v2_sync, v3_sync, nanny_handle, rx)
    }

    async fn shutdown_block_synchronizer(
        v2_sync: &MockStateSync,
        v3_sync: &MockStateSync,
        nanny_handle: JoinHandle<()>,
    ) {
        v3_sync.trigger_close().await;
        v2_sync.trigger_close().await;

        timeout(Duration::from_millis(100), nanny_handle)
            .await
            .expect("Nanny failed to exit within time")
            .expect("Nanny panicked");
    }

    #[test(tokio::test)]
    async fn test_two_ready_synchronizers() {
        let (v2_sync, v3_sync, nanny_handle, mut rx) = setup_block_sync().await;

        let second_msg = header_message(2);
        v2_sync
            .send_header(second_msg.clone())
            .await
            .expect("send_header failed");
        v3_sync
            .send_header(second_msg.clone())
            .await
            .expect("send_header failed");
        let second_feed_msg = receive_message(&mut rx).await;

        let exp2 = FeedMessage {
            state_msgs: [
                ("uniswap-v2".to_string(), second_msg.clone()),
                ("uniswap-v3".to_string(), second_msg.clone()),
            ]
            .into_iter()
            .collect(),
            sync_states: [
                ("uniswap-v3".to_string(), SynchronizerState::Ready(second_msg.header.clone())),
                ("uniswap-v2".to_string(), SynchronizerState::Ready(second_msg.header.clone())),
            ]
            .into_iter()
            .collect(),
        };
        assert_eq!(second_feed_msg, exp2);

        shutdown_block_synchronizer(&v2_sync, &v3_sync, nanny_handle).await;
    }

    #[test(tokio::test)]
    async fn test_delayed_synchronizer_catches_up() {
        let (v2_sync, v3_sync, nanny_handle, mut rx) = setup_block_sync().await;

        // Send block 2 to v2 synchronizer only
        let block2_msg = header_message(2);
        v2_sync
            .send_header(block2_msg.clone())
            .await
            .expect("send_header failed");

        // Consume second message - v3 should be delayed
        let second_feed_msg = receive_message(&mut rx).await;
        debug!("Consumed second message for v2");

        assert!(second_feed_msg
            .state_msgs
            .contains_key("uniswap-v2"));
        assert!(matches!(
            second_feed_msg.sync_states.get("uniswap-v2").unwrap(),
            SynchronizerState::Ready(header) if header.number == 2
        ));
        assert!(!second_feed_msg
            .state_msgs
            .contains_key("uniswap-v3"));
        assert!(matches!(
            second_feed_msg.sync_states.get("uniswap-v3").unwrap(),
            SynchronizerState::Delayed(header) if header.number == 1
        ));

        // Now v3 catches up to block 2
        v3_sync
            .send_header(block2_msg.clone())
            .await
            .expect("send_header failed");

        // Both advance to block 3
        let block3_msg = header_message(3);
        v2_sync
            .send_header(block3_msg.clone())
            .await
            .expect("send_header failed");
        v3_sync
            .send_header(block3_msg)
            .await
            .expect("send_header failed");

        // Consume messages until we get both synchronizers on block 3
        // We may get an intermediate message for v3's catch-up or a combined message
        let mut third_feed_msg = receive_message(&mut rx).await;

        // If this message doesn't have both univ2, it's an intermediate message, so we get the next
        // one
        if !third_feed_msg
            .state_msgs
            .contains_key("uniswap-v2")
        {
            third_feed_msg = rx
                .recv()
                .await
                .expect("header channel was closed")
                .expect("no error");
        }
        assert!(third_feed_msg
            .state_msgs
            .contains_key("uniswap-v2"));
        assert!(third_feed_msg
            .state_msgs
            .contains_key("uniswap-v3"));
        assert!(matches!(
            third_feed_msg.sync_states.get("uniswap-v2").unwrap(),
            SynchronizerState::Ready(header) if header.number == 3
        ));
        assert!(matches!(
            third_feed_msg.sync_states.get("uniswap-v3").unwrap(),
            SynchronizerState::Ready(header) if header.number == 3
        ));

        shutdown_block_synchronizer(&v2_sync, &v3_sync, nanny_handle).await;
    }

    #[test(tokio::test)]
    async fn test_different_start_blocks() {
        let v2_sync = MockStateSync::new();
        let v3_sync = MockStateSync::new();
        let block_sync = BlockSynchronizer::with_short_timeouts()
            .register_synchronizer(
                ExtractorIdentity { chain: Chain::Ethereum, name: "uniswap-v2".to_string() },
                v2_sync.clone(),
            )
            .register_synchronizer(
                ExtractorIdentity { chain: Chain::Ethereum, name: "uniswap-v3".to_string() },
                v3_sync.clone(),
            );

        // Initial messages - synchronizers at different blocks
        let block1_msg = header_message(1);
        let block2_msg = header_message(2);

        let _ = v2_sync
            .send_header(block1_msg.clone())
            .await;
        v3_sync
            .send_header(block2_msg.clone())
            .await
            .expect("send_header failed");

        // Start the block synchronizer - it should use block 2 as the starting block
        let (jh, mut rx) = block_sync
            .run()
            .await
            .expect("BlockSynchronizer failed to start.");

        // Consume first message
        let first_feed_msg = receive_message(&mut rx).await;
        assert!(matches!(
            first_feed_msg.sync_states.get("uniswap-v2").unwrap(),
            SynchronizerState::Delayed(header) if header.number == 1
        ));
        assert!(matches!(
            first_feed_msg.sync_states.get("uniswap-v3").unwrap(),
            SynchronizerState::Ready(header) if header.number == 2
        ));

        // Now v2 catches up to block 2
        v2_sync
            .send_header(block2_msg.clone())
            .await
            .expect("send_header failed");

        // Both advance to block 3
        let block3_msg = header_message(3);
        let _ = v2_sync
            .send_header(block3_msg.clone())
            .await;
        v3_sync
            .send_header(block3_msg.clone())
            .await
            .expect("send_header failed");

        // Consume third message - both should be on block 3
        let second_feed_msg = receive_message(&mut rx).await;
        assert_eq!(second_feed_msg.state_msgs.len(), 2);
        assert!(matches!(
            second_feed_msg.sync_states.get("uniswap-v2").unwrap(),
            SynchronizerState::Ready(header) if header.number == 3
        ));
        assert!(matches!(
            second_feed_msg.sync_states.get("uniswap-v3").unwrap(),
            SynchronizerState::Ready(header) if header.number == 3
        ));

        shutdown_block_synchronizer(&v2_sync, &v3_sync, jh).await;
    }

    #[test(tokio::test)]
    async fn test_synchronizer_fails_other_goes_stale() {
        let (_v2_sync, v3_sync, nanny_handle, mut sync_rx) =
            setup_block_sync_with_behaviour(MockBehavior::ExitImmediately, MockBehavior::Normal)
                .await;

        let mut error_reported = false;
        for _ in 0..3 {
            if let Some(msg) = sync_rx.recv().await {
                match msg {
                    Err(_) => error_reported = true,
                    Ok(msg) => {
                        assert!(matches!(
                            msg.sync_states
                                .get("uniswap-v3")
                                .unwrap(),
                            SynchronizerState::Delayed(_)
                        ));
                        assert!(matches!(
                            msg.sync_states
                                .get("uniswap-v2")
                                .unwrap(),
                            SynchronizerState::Ended(_)
                        ));
                    }
                }
            }
        }
        assert!(error_reported, "BlockSynchronizer did not report final error");

        // Wait for nanny to detect task failure and execute cleanup
        let result = timeout(Duration::from_secs(2), nanny_handle).await;
        assert!(result.is_ok(), "Nanny should complete when synchronizer task exits");

        // Verify that the remaining synchronizer received close signal during cleanup
        assert!(
            v3_sync.was_close_received().await,
            "v3_sync should have received close signal during cleanup"
        );
    }

    #[test(tokio::test)]
    async fn test_cleanup_timeout_warning() {
        // Verify that cleanup_synchronizers emits a warning when synchronizers timeout during
        // cleanup
        let (_v2_sync, _v3_sync, nanny_handle, _rx) = setup_block_sync_with_behaviour(
            MockBehavior::ExitImmediately,
            MockBehavior::IgnoreClose,
        )
        .await;

        // Wait for nanny to complete - cleanup should timeout on v3_sync but still complete
        let result = timeout(Duration::from_secs(10), nanny_handle).await;
        assert!(
            result.is_ok(),
            "Nanny should complete even when some synchronizers timeout during cleanup"
        );

        // Note: In a real test environment, we would capture log output to verify the warning was
        // emitted. Since this is a unit test without log capture setup, we just verify that
        // cleanup completes even when some synchronizers timeout.
    }

    #[test(tokio::test)]
    async fn test_one_synchronizer_goes_stale_while_other_works() {
        // Test Case 1: One protocol goes stale and is removed while another protocol works normally
        let (v2_sync, v3_sync, nanny_handle, mut rx) = setup_block_sync().await;

        // Send block 2 only to v3, v2 will timeout and become delayed
        let block2_msg = header_message(2);
        let _ = v3_sync
            .send_header(block2_msg.clone())
            .await;
        // Don't send to v2_sync - it will timeout

        // Consume second message - v2 should be delayed, v3 ready
        let second_feed_msg = receive_message(&mut rx).await;
        assert!(second_feed_msg
            .state_msgs
            .contains_key("uniswap-v3"));
        assert!(!second_feed_msg
            .state_msgs
            .contains_key("uniswap-v2"));
        assert!(matches!(
            second_feed_msg
                .sync_states
                .get("uniswap-v3")
                .unwrap(),
            SynchronizerState::Ready(_)
        ));
        // v2 should be delayed (if still present) - check nanny is still running
        if let Some(v2_state) = second_feed_msg
            .sync_states
            .get("uniswap-v2")
        {
            if matches!(v2_state, SynchronizerState::Delayed(_)) {
                // Verify nanny is still running when synchronizer is just delayed
                assert!(
                    !nanny_handle.is_finished(),
                    "Nanny should still be running when synchronizer is delayed (not stale yet)"
                );
            }
        }

        // Wait a bit, then continue sending blocks to v3 but not v2
        tokio::time::sleep(Duration::from_millis(15)).await;

        // Continue sending blocks only to v3 to keep it healthy while v2 goes stale
        let block3_msg = header_message(3);
        let _ = v3_sync
            .send_header(block3_msg.clone())
            .await;

        tokio::time::sleep(Duration::from_millis(40)).await;

        let mut stale_found = false;
        for _ in 0..2 {
            if let Some(Ok(msg)) = rx.recv().await {
                if let Some(SynchronizerState::Stale(_)) = msg.sync_states.get("uniswap-v2") {
                    stale_found = true;
                }
            }
        }
        assert!(stale_found, "v2 synchronizer should be stale");

        shutdown_block_synchronizer(&v2_sync, &v3_sync, nanny_handle).await;
    }

    #[test(tokio::test)]
    async fn test_all_synchronizers_go_stale_main_loop_exits() {
        // Test Case 2: All protocols go stale and main loop exits gracefully
        let (v2_sync, v3_sync, nanny_handle, mut rx) = setup_block_sync().await;

        // Stop sending messages to both synchronizers - they should both timeout and go stale
        // Don't send any more messages, let them timeout and become delayed, then stale

        // Monitor the state transitions to ensure proper delayed -> stale progression
        let mut seen_delayed = false;

        // Consume messages and track state transitions
        // Give enough time for the synchronizers to transition through states
        let timeout_duration = Duration::from_millis(500); // Generous timeout
        let start_time = tokio::time::Instant::now();

        while let Ok(Some(Ok(msg))) =
            tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
        {
            // Track when synchronizers transition to delayed
            if !seen_delayed {
                let v2_state = msg.sync_states.get("uniswap-v2");
                let v3_state = msg.sync_states.get("uniswap-v3");

                if matches!(v2_state, Some(SynchronizerState::Delayed(_))) ||
                    matches!(v3_state, Some(SynchronizerState::Delayed(_)))
                {
                    seen_delayed = true;
                    // Verify nanny is still running when synchronizers are just delayed
                    assert!(!nanny_handle.is_finished(),
                        "Nanny should still be running when synchronizers are delayed (not stale yet)");
                    // Once we've seen delayed and verified nanny is running, we can break
                    break;
                }
            }

            // Safety timeout to avoid infinite loop
            if start_time.elapsed() > timeout_duration {
                break;
            }
        }
        // Verify that synchronizers went through proper state transitions
        assert!(seen_delayed, "Synchronizers should transition to Delayed state first");

        let mut error_reported = false;
        // Consume any remaining messages until channel closes
        while let Some(msg) = rx.recv().await {
            if let Err(e) = msg {
                assert!(e
                    .to_string()
                    .contains("became: Stale(1)"));
                assert!(e
                    .to_string()
                    .contains("reported as Stale(1)"));
                error_reported = true;
            }
        }
        assert!(error_reported, "Expected the channel to report an error before closing");

        // The nanny should complete when the main loop exits due to no ready synchronizers
        let nanny_result = timeout(Duration::from_secs(2), nanny_handle).await;
        assert!(nanny_result.is_ok(), "Nanny should complete when main loop exits");

        // Verify cleanup was triggered for both synchronizers
        assert!(
            v2_sync.was_close_received().await,
            "v2_sync should have received close signal during cleanup"
        );
        assert!(
            v3_sync.was_close_received().await,
            "v3_sync should have received close signal during cleanup"
        );
    }

    #[test(tokio::test)]
    async fn test_stale_synchronizer_recovers() {
        // Test Case 2: All protocols go stale and main loop exits gracefully
        let (v2_sync, v3_sync, nanny_handle, mut rx) = setup_block_sync().await;

        // Send second messages to v2 only, shortly before both would go stale
        tokio::time::sleep(Duration::from_millis(50)).await;
        let block2_msg = header_message(2);
        let _ = v2_sync
            .send_header(block2_msg.clone())
            .await;

        // we should get two messages here
        for _ in 0..2 {
            if let Some(msg) = rx.recv().await {
                if let Ok(msg) = msg {
                    if matches!(
                        msg.sync_states
                            .get("uniswap-v2")
                            .unwrap(),
                        SynchronizerState::Ready(_)
                    ) {
                        assert!(matches!(
                            msg.sync_states
                                .get("uniswap-v3")
                                .unwrap(),
                            SynchronizerState::Delayed(_)
                        ));
                        break;
                    };
                }
            } else {
                panic!("Channel closed unexpectedly")
            }
        }

        // Now v3 should be stale
        tokio::time::sleep(Duration::from_millis(15)).await;
        let block3_msg = header_message(3);
        let _ = v2_sync
            .send_header(block3_msg.clone())
            .await;
        let third_msg = receive_message(&mut rx).await;
        dbg!(&third_msg);
        assert!(matches!(
            third_msg
                .sync_states
                .get("uniswap-v2")
                .unwrap(),
            SynchronizerState::Ready(_)
        ));
        assert!(matches!(
            third_msg
                .sync_states
                .get("uniswap-v3")
                .unwrap(),
            SynchronizerState::Stale(_)
        ));

        let block4_msg = header_message(4);
        let _ = v3_sync
            .send_header(block2_msg.clone())
            .await;
        let _ = v3_sync
            .send_header(block3_msg.clone())
            .await;
        let _ = v3_sync
            .send_header(block4_msg.clone())
            .await;
        let _ = v2_sync
            .send_header(block4_msg.clone())
            .await;
        let fourth_msg = receive_message(&mut rx).await;
        assert!(matches!(
            fourth_msg
                .sync_states
                .get("uniswap-v2")
                .unwrap(),
            SynchronizerState::Ready(_)
        ));
        assert!(matches!(
            fourth_msg
                .sync_states
                .get("uniswap-v3")
                .unwrap(),
            SynchronizerState::Ready(_)
        ));

        shutdown_block_synchronizer(&v2_sync, &v3_sync, nanny_handle).await;

        // Verify cleanup was triggered for both synchronizers
        assert!(
            v2_sync.was_close_received().await,
            "v2_sync should have received close signal during cleanup"
        );
        assert!(
            v3_sync.was_close_received().await,
            "v3_sync should have received close signal during cleanup"
        );
    }

    #[test(tokio::test)]
    async fn test_all_synchronizer_advanced() {
        // Test the case were all synchronizers successfully recover but stream
        // from a disconnected future block.

        let (v2_sync, v3_sync, nanny_handle, mut rx) = setup_block_sync().await;

        let block3 = header_message(3);
        v2_sync
            .send_header(block3.clone())
            .await
            .unwrap();
        v3_sync
            .send_header(block3)
            .await
            .unwrap();

        let msg = receive_message(&mut rx).await;
        matches!(
            msg.sync_states
                .get("uniswap-v2")
                .unwrap(),
            SynchronizerState::Ready(_)
        );
        matches!(
            msg.sync_states
                .get("uniswap-v3")
                .unwrap(),
            SynchronizerState::Ready(_)
        );

        shutdown_block_synchronizer(&v2_sync, &v3_sync, nanny_handle).await;
    }

    #[test(tokio::test)]
    async fn test_one_synchronizer_advanced() {
        let (v2_sync, v3_sync, nanny_handle, mut rx) = setup_block_sync().await;

        let block2 = header_message(2);
        let block4 = header_message(4);
        v2_sync
            .send_header(block4.clone())
            .await
            .unwrap();
        v3_sync
            .send_header(block2.clone())
            .await
            .unwrap();

        let msg = receive_message(&mut rx).await;
        matches!(
            msg.sync_states
                .get("uniswap-v2")
                .unwrap(),
            SynchronizerState::Ready(_)
        );
        matches!(
            msg.sync_states
                .get("uniswap-v3")
                .unwrap(),
            SynchronizerState::Delayed(_)
        );

        shutdown_block_synchronizer(&v2_sync, &v3_sync, nanny_handle).await;
    }
}
