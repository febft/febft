use std::collections::{BTreeMap, BTreeSet};
use std::iter;
use std::sync::{Arc, Mutex};
use futures::StreamExt;
use crate::bft::benchmarks::BatchMeta;
use crate::bft::communication::message::{ConsensusMessage, StoredMessage};
use crate::bft::communication::NodeId;
use crate::bft::crypto::hash::{Context, Digest};
use crate::bft::executable::{Request, Service};
use crate::bft::globals::ReadOnly;
use crate::bft::error::*;
use crate::bft::msg_log::decisions::StoredConsensusMessage;
use crate::bft::msg_log::persistent::PersistentLog;
use crate::bft::sync::view::ViewInfo;

/// The log for the current consensus decision
/// Stores the pre prepare that is being decided along with
/// Digests of all of the requests, digest of the entire batch and
/// the messages that should be persisted in order to consider this execution unit
/// persisted.
/// Basically some utility information about the current batch.
/// The actual consensus messages are handled by the decided log
pub struct DecidingLog<S> where S: Service {
    // The set of leaders that is currently in vigour for this consensus decision
    leader_set: Vec<NodeId>,

    //The digest of the entire batch that is currently being processed
    // This will only be calculated when we receive all of the requests necessary
    // As this digest requires the knowledge of all of them
    current_digest: Option<Digest>,
    // How many pre prepares have we received
    current_received_pre_prepares: usize,
    //Stores the pre prepare requests that we have received and the
    //ones that are still missing
    pre_prepare_ordering: Vec<Option<Digest>>,
    //Pre prepare messages that will then compose the entire pre prepare
    pre_prepare_messages: Vec<Option<StoredConsensusMessage<S>>>,

    // Received messages from these leaders
    received_leader_messages: BTreeSet<NodeId>,
    request_space_slices: BTreeMap<NodeId, (Vec<u8>, Vec<u8>)>,

    //A vector that contains the digest of all requests contained in the batch that is currently being processed
    current_requests: Vec<Digest>,
    //The size of batch that is currently being processed. Increases as we receive more pre prepares
    current_batch_size: usize,

    //A list of digests of all consensus related messages pertaining to this
    //Consensus instance. Used to keep track of if the persistent log has saved the messages already
    //So the requests can be executed
    current_messages_to_persist: Vec<Digest>,

    // Some logging information about metadata
    batch_meta: Arc<Mutex<BatchMeta>>,
}

///The type that composes a processed batch
/// Contains the pre-prepare message and the Vec of messages that contains all messages
/// to be persisted pertaining to this consensus instance
pub type ProcessedBatch<S> = CompletedBatch<S>;

/// A batch that has been decided by the consensus instance and is now ready to be delivered to the
/// Executor for execution.
/// Contains all of the necessary information for when we are using the strict persistency mode
pub struct CompletedBatch<S> where S: Service {
    //The digest of the batch
    batch_digest: Digest,

    // The ordering of the pre prepares
    pre_prepare_ordering: Vec<Digest>,
    // The prepare message of the batch
    pre_prepare_messages: Vec<StoredConsensusMessage<S>>,

    //The digests of all the requests in the batch
    request_digests: Vec<Digest>,

    //The messages that must be persisted for this consensus decision to be executable
    //This should contain the pre prepare, quorum of prepares and quorum of commits
    messages_to_persist: Vec<Digest>,

    // The metadata for this batch (mostly statistics)
    batch_meta: BatchMeta,
}

pub type CompletedBatchInto<S> = (Digest, Vec<Digest>, Vec<StoredConsensusMessage<S>>,
                                  Vec<Digest>, Vec<Digest>, BatchMeta);

impl<S> Into<CompletedBatchInto<S>> for CompletedBatch<S> where S: Service {
    fn into(self) -> CompletedBatchInto<S> {
        (self.batch_digest, self.pre_prepare_ordering, self.pre_prepare_messages,
         self.request_digests, self.messages_to_persist, self.batch_meta)
    }
}

impl<S> DecidingLog<S> where S: Service {
    pub fn new() -> Self {
        Self {
            leader_set: vec![],
            current_digest: None,
            current_received_pre_prepares: 0,
            pre_prepare_ordering: vec![],
            pre_prepare_messages: vec![],
            received_leader_messages: Default::default(),
            request_space_slices: Default::default(),
            current_requests: Vec::with_capacity(1000),
            current_batch_size: 1000,
            current_messages_to_persist: Vec::with_capacity(1000),
            batch_meta: Arc::new(Mutex::new(BatchMeta::new())),
        }
    }

    pub fn batch_meta(&self) -> &Arc<Mutex<BatchMeta>> {
        &self.batch_meta
    }

    pub fn processing_new_round(&mut self,
                                view: &ViewInfo, ) {
        self.leader_set = view.leader_set().clone();
        self.received_leader_messages.clear();


        // We need to have a number of pre prepares == to the # of leaders
        self.pre_prepare_ordering = iter::repeat(None)
            .take(view.leader_set().len()).collect();

        self.pre_prepare_messages = iter::repeat(None)
            .take(view.leader_set().len()).collect();
    }

    ///Inform the log that we are now processing a new batch of operations
    pub fn processing_batch_request(&mut self,
                                    request_batch: Arc<ReadOnly<StoredMessage<ConsensusMessage<Request<S>>>>>,
                                    digest: Digest,
                                    mut batch_rq_digests: Vec<Digest>) -> Result<Option<Digest>> {

        let sending_leader = request_batch.header().from();

        let slice = self.request_space_slices.get(&sending_leader).unwrap();

        for request in batch_rq_digests {

            if !crate::bft::sync::view::is_request_in_hash_space(&request, slice) {
                return Err(Error::simple_with_msg(ErrorKind::MsgLogDecidingLog,
                                                  "This batch contains requests that are not in the hash space of the leader."))
            }
        }

        // Check if we have already received messages from this leader
        if !self.received_leader_messages.insert(sending_leader.clone()) {
            return Err(Error::simple_with_msg(ErrorKind::MsgLogDecidingLog,
                                              "We have already received a message from that leader."))
        }

        // Get the correct index for this batch
        let leader_index = self.get_index_for_leader(sending_leader.clone())
            .expect("Leader not in leader set ?");

        self.pre_prepare_ordering[leader_index] = Some(digest.clone());

        self.pre_prepare_messages[leader_index] = Some(request_batch);

        self.current_received_pre_prepares += 1;

        self.current_batch_size += batch_rq_digests.len();

        // Register this new batch as one that must be persisted for this batch to be executed
        self.register_consensus_message(request_batch.header().digest().clone());

        // if we have received all of the messages in the set, calculate the digest.
        Ok(if self.current_received_pre_prepares == self.leader_set.len() {
            // We have received all of the required batches
            self.current_digest = self.calculate_instance_digest();

            self.current_digest.clone()
        } else {
            None
        })
    }

    /// Get the index for the batch producer leader
    fn get_index_for_leader(&self, leader: NodeId) -> Option<usize> {
        self.leader_set.iter().position(|id| *id == leader)
    }

    /// Calculate the instance of a completed consensus pre prepare phase with
    /// all the batches received
    fn calculate_instance_digest(&self) -> Option<Digest> {
        let mut ctx = Context::new();

        for order_digest in &self.pre_prepare_ordering {
            ctx.update(order_digest?[..]);
        }

        Some(ctx.finish())
    }

    /// Register a message that is important to this consensus instance
    pub fn register_consensus_message(&mut self, message_digest: Digest) {
        self.current_messages_to_persist.push(message_digest)
    }

    /// Indicate that the batch is finished processing and
    /// return the relevant information for it
    pub fn finish_processing_batch(&mut self) -> Option<ProcessedBatch<S>> {
        let pre_prepare_ordering: Vec<Digest> = self.pre_prepare_ordering.into_iter()
            .map(|order| order?)
            .collect();

        let pre_prepare_messages: Vec<StoredConsensusMessage<S>> = self.pre_prepare_messages.into_iter()
            .map(|msg| msg?)
            .collect();

        let current_digest = self.current_digest?;

        let msg_to_persist_size = self.current_messages_to_persist.len();

        let messages_to_persist = std::mem::replace(
            &mut self.current_messages_to_persist,
            Vec::with_capacity(msg_to_persist_size),
        );

        let new_meta = BatchMeta::new();
        let batch_meta = std::mem::replace(&mut *self.batch_meta().lock().unwrap(), new_meta);

        self.received_leader_messages.clear();
        self.request_space_slices.clear();

        Some(CompletedBatch {
            batch_digest: current_digest,
            pre_prepare_ordering,
            pre_prepare_messages,
            request_digests: vec![],
            messages_to_persist,
            batch_meta,
        })
    }

    /// Reset the batch that is currently being processed
    pub fn reset(&mut self) {
        self.leader_set.clear();
        self.pre_prepare_messages.clear();
        self.pre_prepare_ordering.clear();
        self.received_leader_messages.clear();
        self.request_space_slices.clear();
        self.current_digest = None;
        self.current_received_pre_prepares = 0;
        self.current_messages_to_persist.clear();
    }

    /// Are we currently processing a batch
    pub fn is_currently_processing(&self) -> bool {
        !self.pre_prepare_ordering.is_empty()
    }

    /// Get a reference to the pre prepare message of the batch that we are currently processing
    pub fn pre_prepare_message(&self) -> &Option<Arc<ReadOnly<StoredMessage<ConsensusMessage<Request<S>>>>>> {
        &self.pre_prepare_message
    }

    /// The digest of the batch that is currently being processed
    pub fn current_digest(&self) -> Option<Digest> {
        self.current_digest
    }

    /// The current request list for the batch that is being processed
    pub fn current_requests(&self) -> &Vec<Digest> {
        &self.current_requests
    }

    /// The size of the batch that is currently being processed
    pub fn current_batch_size(&self) -> Option<usize> {
        if self.is_currently_processing() {
            Some(self.current_batch_size)
        } else {
            None
        }
    }

    /// The current messages that should be persisted for the current consensus instance to be
    /// considered executable
    pub fn current_messages(&self) -> &Vec<Digest> {
        &self.current_messages_to_persist
    }
}