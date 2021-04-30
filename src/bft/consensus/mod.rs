//! The consensus algorithm used for `febft` and other logic.

use std::marker::PhantomData;
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};

#[cfg(feature = "serialize_serde")]
use serde::{Serialize, Deserialize};

use crate::bft::history::{self, Log};
use crate::bft::crypto::hash::Digest;
use crate::bft::core::server::ViewInfo;
use crate::bft::communication::message::{
    Header,
    SystemMessage,
    ConsensusMessage,
    ConsensusMessageKind,
};
//use crate::bft::collections::{
//    self,
//    HashSet,
//};
use crate::bft::communication::{
    Node,
    NodeId,
};
use crate::bft::executable::{
    Service,
    Request,
    Reply,
    State,
};

/// Represents a sequence number attributed to a client request
/// during a `Consensus` instance.
#[cfg_attr(feature = "serialize_serde", derive(Serialize, Deserialize))]
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct SeqNo(i32);

impl From<u32> for SeqNo {
    #[inline]
    fn from(sequence_number: u32) -> SeqNo {
        // FIXME: is this correct?
        SeqNo(sequence_number as i32)
    }
}

impl From<SeqNo> for u32 {
    #[inline]
    fn from(sequence_number: SeqNo) -> u32 {
        sequence_number.0 as u32
    }
}

impl SeqNo {
    /// Returns the following sequence number.
    #[inline]
    fn next(self) -> SeqNo {
        let (next, overflow) = (self.0).overflowing_add(1);
        SeqNo(if overflow { 0 } else { next })
    }

    /// Return an appropriate value to index the `TBOQueue`.
    #[inline]
    fn index(self, other: SeqNo) -> Option<usize> {
        // FIXME: probably swap this logic out for
        // low and high water marks, like in PBFT
        const OVERFLOW_THRES_POS: i32 = 10000;
        const OVERFLOW_THRES_NEG: i32 = -OVERFLOW_THRES_POS;
        const DROP_SEQNO_THRES: i32 = (history::PERIOD + (history::PERIOD >> 1)) as i32;

        let index = {
            let index = (self.0).wrapping_sub(other.0);
            if index < OVERFLOW_THRES_NEG || index > OVERFLOW_THRES_POS {
                // guard against overflows
                i32::MAX
                    .wrapping_add(index)
                    .wrapping_add(1)
            } else {
                index
            }
        };

        if index < 0 || index > DROP_SEQNO_THRES {
            // drop old messages or messages whose seq no. is too
            // large, which may be due to a DoS attack of
            // a malicious node
            None
        } else {
            Some(index as usize)
        }
    }
}

/// Represents the status of calling `poll()` on a `TBOQueue`.
pub enum PollStatus {
    /// The `Replica` associated with this `TBOQueue` should
    /// poll its main channel for more messages.
    Recv,
    /// The `Replica` associated with this `TBOQueue` should
    /// propose a new client request to be ordered, if it is
    /// the leader, and then it should poll its main channel
    /// for more messages. Alternatively, if the request has
    /// already been decided, it should be queued for
    /// execution.
    TryProposeAndRecv,
    /// A new consensus message is available to be processed.
    NextMessage(Header, ConsensusMessage),
}

/// Represents a queue of messages to be ordered in a consensus instance.
///
/// Because of the asynchrony of the Internet, messages may arrive out of
/// context, e.g. for the same consensus instance, a `PRE-PREPARE` reaches
/// a node after a `PREPARE`. A `TBOQueue` arranges these messages to be
/// processed in the correct order.
pub struct TBOQueue {
    curr_seq: SeqNo,
    get_queue: bool,
    pre_prepares: VecDeque<VecDeque<(Header, ConsensusMessage)>>,
    prepares: VecDeque<VecDeque<(Header, ConsensusMessage)>>,
    commits: VecDeque<VecDeque<(Header, ConsensusMessage)>>,
}

// XXX: details
impl TBOQueue {
    fn new_impl(curr_seq: SeqNo) -> Self {
        Self {
            curr_seq,
            get_queue: false,
            pre_prepares: VecDeque::new(),
            prepares: VecDeque::new(),
            commits: VecDeque::new(),
        }
    }

    fn pop_message(
        tbo: &mut VecDeque<VecDeque<(Header, ConsensusMessage)>>,
    ) -> Option<(Header, ConsensusMessage)> {
        if tbo.is_empty() {
            None
        } else {
            tbo[0].pop_front()
        }
    }

    fn queue_message(
        curr_seq: SeqNo,
        tbo: &mut VecDeque<VecDeque<(Header, ConsensusMessage)>>,
        h: Header,
        m: ConsensusMessage,
    ) {
        let index = match m.sequence_number().index(curr_seq) {
            Some(i) => i,
            None => {
                // FIXME: maybe notify peers if we detect a message
                // with an invalid (too large) seq no? return the
                // `NodeId` of the offending node.
                return;
            },
        };
        if index >= tbo.len() {
            let len = index - tbo.len() + 1;
            tbo.extend(std::iter::repeat_with(VecDeque::new).take(len));
        }
        tbo[index].push_back((h, m));
    }

    fn advance_message_queue(
        tbo: &mut VecDeque<VecDeque<(Header, ConsensusMessage)>>,
    ) {
        match tbo.pop_front() {
            Some(mut vec) => {
                // recycle memory
                vec.clear();
                tbo.push_back(vec);
            },
            None => (),
        }
    }
}

// XXX: api
impl TBOQueue {
    fn new(curr_seq: SeqNo) -> Self {
        Self::new_impl(curr_seq)
    }

    /// Signal this `TBOQueue` that it may be able to extract new
    /// consensus messages from its internal storage.
    pub fn signal(&mut self) {
        self.get_queue = true;
    }

    /// Reports the id of the consensus this `TBOQueue` is tracking.
    pub fn sequence_number(&self) -> SeqNo {
        self.curr_seq
    }

    /// Advances the message queue, and updates the consensus instance id.
    fn next_instance_queue(&mut self) {
        self.curr_seq = self.curr_seq.next();
        Self::advance_message_queue(&mut self.pre_prepares);
        Self::advance_message_queue(&mut self.prepares);
        Self::advance_message_queue(&mut self.commits);
    }

    /// Queues a consensus message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    pub fn queue(&mut self, h: Header, m: ConsensusMessage) {
        match m.kind() {
            ConsensusMessageKind::PrePrepare(_) => self.queue_pre_prepare(h, m),
            ConsensusMessageKind::Prepare => self.queue_prepare(h, m),
            ConsensusMessageKind::Commit => self.queue_commit(h, m),
        }
    }

    /// Queues a `PRE-PREPARE` message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    fn queue_pre_prepare(&mut self, h: Header, m: ConsensusMessage) {
        Self::queue_message(self.curr_seq, &mut self.pre_prepares, h, m)
    }

    /// Queues a `PREPARE` message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    fn queue_prepare(&mut self, h: Header, m: ConsensusMessage) {
        Self::queue_message(self.curr_seq, &mut self.prepares, h, m)
    }

    /// Queues a `COMMIT` message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    fn queue_commit(&mut self, h: Header, m: ConsensusMessage) {
        Self::queue_message(self.curr_seq, &mut self.commits, h, m)
    }
}

/// Repreents the current phase of the consensus protocol.
#[derive(Debug, Copy, Clone)]
pub enum ProtoPhase {
    /// Start of a new consensus instance.
    Init,
    /// Running the `PRE-PREPARE` phase.
    PrePreparing,
    /// Running the `PREPARE` phase. The integer represents
    /// the number of votes received.
    Preparing(usize),
    /// Running the `COMMIT` phase. The integer represents
    /// the number of votes received.
    Committing(usize),
}

/// Contains the state of an active consensus instance, as well
/// as future instances.
pub struct Consensus<S: Service> {
    phase: ProtoPhase,
    tbo: TBOQueue,
    current: Digest,
    //voted: HashSet<NodeId>,
    _phantom: PhantomData<S>,
}

/// Status returned from processing a consensus message.
pub enum ConsensusStatus {
    /// A particular node tried voting twice.
    VotedTwice(NodeId),
    /// A `febft` quorum still hasn't made a decision
    /// on a client request to be executed.
    Deciding,
    /// A `febft` quorum decided on the execution of
    /// the request with the given `Digest`.
    Decided(Digest),
}

macro_rules! extract_msg {
    ($g:expr, $q:expr) => {
        extract_msg!({}, $g, $q)
    };

    ($opt:block, $g:expr, $q:expr) => {
        if let Some((header, message)) = TBOQueue::pop_message($q) {
            $opt
            PollStatus::NextMessage(header, message)
        } else {
            *$g = false;
            PollStatus::Recv
        }
    };
}

impl<S> Consensus<S>
where
    S: Service + Send + 'static,
    State<S>: Send + 'static,
    Request<S>: Send + 'static,
    Reply<S>: Send + 'static,
{
    /// Starts a new consensus protocol tracker.
    pub fn new(initial_seq_no: SeqNo) -> Self {
        Self {
            _phantom: PhantomData,
            phase: ProtoPhase::Init,
            tbo: TBOQueue::new(initial_seq_no),
            //voted: collections::hash_set(),
            current: Digest::from_bytes(&[0; Digest::LENGTH][..]).unwrap(),
        }
    }

    /// Proposes a new request with digest `dig`.
    ///
    /// This function will only succeed if the `node` is
    /// the leader of the current `view` and the `node` is
    /// in the phase `ProtoPhase::Init`.
    pub fn propose(&mut self, dig: Digest, view: ViewInfo, node: &mut Node<S::Data>) {
        match self.phase {
            ProtoPhase::Init => self.phase = ProtoPhase::PrePreparing,
            _ => return,
        }
        if node.id() != view.leader() {
            return;
        }
        let message = SystemMessage::Consensus(ConsensusMessage::new(
            self.sequence_number(),
            ConsensusMessageKind::PrePrepare(dig),
        ));
        let targets = NodeId::targets(0..view.params().n());
        node.broadcast(message, targets);
    }

    /// Returns the current protocol phase.
    pub fn phase(&self) -> ProtoPhase {
        self.phase
    }

    /// Check if we can process new consensus messages.
    pub fn poll(&mut self, log: &Log<Request<S>, Reply<S>>) -> PollStatus {
        match self.phase {
            ProtoPhase::Init if self.tbo.get_queue => {
                extract_msg!(
                    { self.phase = ProtoPhase::PrePreparing; },
                    &mut self.tbo.get_queue,
                    &mut self.tbo.pre_prepares
                )
            },
            ProtoPhase::Init => {
                PollStatus::TryProposeAndRecv
            },
            ProtoPhase::PrePreparing if self.tbo.get_queue => {
                extract_msg!(&mut self.tbo.get_queue, &mut self.tbo.pre_prepares)
            },
            // TODO: maybe optimize this for leaders,
            // e.g. `node.id() != view.leader() && ...`
            ProtoPhase::Preparing(_) if !log.has_request(&self.current) => {
                self.tbo.get_queue = false;
                PollStatus::Recv
            },
            ProtoPhase::Preparing(_) if self.tbo.get_queue => {
                extract_msg!(&mut self.tbo.get_queue, &mut self.tbo.prepares)
            },
            ProtoPhase::Committing(_) if self.tbo.get_queue => {
                extract_msg!(&mut self.tbo.get_queue, &mut self.tbo.commits)
            },
            _ => PollStatus::Recv,
        }
    }

    /// Starts a new consensus instance.
    pub fn next_instance(&mut self) {
        self.tbo.next_instance_queue();
        //self.voted.clear();
    }

    /// Process a message for a particular consensus instance.
    pub fn process_message(
        &mut self,
        header: Header,
        message: ConsensusMessage,
        view: ViewInfo,
        log: &mut Log<Request<S>, Reply<S>>,
        node: &mut Node<S::Data>,
    ) -> ConsensusStatus {
        // FIXME: use order imposed by leader
        // FIXME: check if the pre-prepare is from the leader
        // FIXME: make sure a replica doesn't vote twice
        // by keeping track of who voted, and not just
        // the amount of votes received
        // FIXME: log the message
        match self.phase {
            ProtoPhase::Init => {
                // in the init phase, we can't do anything,
                // queue the message for later
                match message.kind() {
                    ConsensusMessageKind::PrePrepare(_) => {
                        self.queue_pre_prepare(header, message);
                        return ConsensusStatus::Deciding;
                    },
                    ConsensusMessageKind::Prepare => {
                        self.queue_prepare(header, message);
                        return ConsensusStatus::Deciding;
                    },
                    ConsensusMessageKind::Commit => {
                        self.queue_commit(header, message);
                        return ConsensusStatus::Deciding;
                    },
                }
            },
            ProtoPhase::PrePreparing => {
                // queue message if we're not pre-preparing
                // or in the same seq as the message
                self.current = match message.kind() {
                    ConsensusMessageKind::PrePrepare(_) if message.sequence_number() != self.sequence_number() => {
                        self.queue_pre_prepare(header, message);
                        return ConsensusStatus::Deciding;
                    },
                    ConsensusMessageKind::PrePrepare(dig) => {
                        dig.clone()
                    },
                    ConsensusMessageKind::Prepare => {
                        self.queue_prepare(header, message);
                        return ConsensusStatus::Deciding;
                    },
                    ConsensusMessageKind::Commit => {
                        self.queue_commit(header, message);
                        return ConsensusStatus::Deciding;
                    },
                };
                // leader can't vote for a prepare
                if node.id() != view.leader() {
                    let message = SystemMessage::Consensus(ConsensusMessage::new(
                        self.sequence_number(),
                        ConsensusMessageKind::Prepare,
                    ));
                    let targets = NodeId::targets(0..view.params().n());
                    node.broadcast(message, targets);
                }
                // enter preparing phase
                self.phase = ProtoPhase::Preparing(0);
                ConsensusStatus::Deciding
            },
            ProtoPhase::Preparing(i) => {
                // queue message if we're not preparing
                // or in the same seq as the message
                let i = match message.kind() {
                    ConsensusMessageKind::PrePrepare(_) => {
                        self.queue_pre_prepare(header, message);
                        return ConsensusStatus::Deciding;
                    },
                    ConsensusMessageKind::Prepare if message.sequence_number() != self.sequence_number() => {
                        self.queue_prepare(header, message);
                        return ConsensusStatus::Deciding;
                    },
                    ConsensusMessageKind::Prepare => i + 1,
                    ConsensusMessageKind::Commit => {
                        self.queue_commit(header, message);
                        return ConsensusStatus::Deciding;
                    },
                };
                // wait for the request to arrive before
                // proceeding with the protocol
                if node.id() != view.leader() && !log.has_request(&self.current) {
                    self.queue(header, message);
                    return ConsensusStatus::Deciding;
                }
                // check if we have gathered enough votes,
                // and transition to a new phase
                self.phase = if i == view.params().quorum() {
                    let message = SystemMessage::Consensus(ConsensusMessage::new(
                        self.sequence_number(),
                        ConsensusMessageKind::Commit,
                    ));
                    let targets = NodeId::targets(0..view.params().n());
                    node.broadcast(message, targets);
                    ProtoPhase::Committing(0)
                } else {
                    ProtoPhase::Preparing(i)
                };
                ConsensusStatus::Deciding
            },
            ProtoPhase::Committing(i) => {
                // queue message if we're not committing
                // or in the same seq as the message
                let i = match message.kind() {
                    ConsensusMessageKind::PrePrepare(_) => {
                        self.queue_pre_prepare(header, message);
                        return ConsensusStatus::Deciding;
                    },
                    ConsensusMessageKind::Prepare => {
                        self.queue_prepare(header, message);
                        return ConsensusStatus::Deciding;
                    },
                    ConsensusMessageKind::Commit if message.sequence_number() != self.sequence_number() => {
                        self.queue_commit(header, message);
                        return ConsensusStatus::Deciding;
                    },
                    ConsensusMessageKind::Commit => i + 1,
                };
                // check if we have gathered enough votes,
                // and transition to a new phase
                if i == view.params().quorum() {
                    // we have reached a decision,
                    // notify core protocol
                    self.phase = ProtoPhase::Init;
                    ConsensusStatus::Decided(self.current)
                } else {
                    self.phase = ProtoPhase::Committing(i);
                    ConsensusStatus::Deciding
                }
            },
        }
    }
}

impl<S> Deref for Consensus<S>
where
    S: Service + Send + 'static,
    State<S>: Send + 'static,
    Request<S>: Send + 'static,
    Reply<S>: Send + 'static,
{
    type Target = TBOQueue;

    #[inline]
    fn deref(&self) -> &TBOQueue {
        &self.tbo
    }
}

impl<S> DerefMut for Consensus<S>
where
    S: Service + Send + 'static,
    State<S>: Send + 'static,
    Request<S>: Send + 'static,
    Reply<S>: Send + 'static,
{
    #[inline]
    fn deref_mut(&mut self) -> &mut TBOQueue {
        &mut self.tbo
    }
}
