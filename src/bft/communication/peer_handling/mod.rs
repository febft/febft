use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvError, SendError};
use std::thread::JoinHandle;
use std::time::Duration;
use dsrust::channels::async_ch::ReceiverMultFut;
use dsrust::channels::queue_channel::{make_mult_recv_from, make_mult_recv_partial_from, Receiver, ReceiverMult, ReceiverPartialMult, Sender};
use dsrust::queues::lf_array_queue::LFBQueue;

use dsrust::queues::mqueue::MQueue;
use dsrust::queues::queues::{BQueue, PartiallyDumpable, Queue, SizableQueue};
use dsrust::utils::backoff::BackoffN;
use futures::select;
use futures_timer::Delay;
use intmap::IntMap;
use parking_lot::{Mutex, RwLock};

use crate::bft::communication::{NODE_CHAN_BOUND, NodeConfig, NodeId};
use crate::bft::communication::channel::{ChannelRx, ChannelTx, new_bounded};
use crate::bft::communication::message::Message;
use crate::bft::error::*;
use crate::bft::threadpool;

///Handles the communication between two peers (replica - replica, replica - client)
/// Only handles reception of requests, not transmission

type QueueType<T> = LFBQueue<Vec<T>>;

type ClientQueueType<T> = MQueue<T>;

fn channel_init<T>(capacity: usize) -> (Sender<Vec<T>, QueueType<T>>, Receiver<Vec<T>, QueueType<T>>) {
    dsrust::channels::queue_channel::bounded_lf_queue(capacity)
}

fn client_channel_init<T>(capacity: usize) -> (Sender<T, ClientQueueType<T>>, ReceiverPartialMult<T, ClientQueueType<T>>) {
    let (tx, rx) = dsrust::channels::queue_channel::bounded_mutex_backoff_queue(capacity);

    (tx, make_mult_recv_partial_from(rx))
}

pub struct NodePeers<T: 'static> {
    first_cli: NodeId,
    own_id: NodeId,
    peer_loopback: Arc<ConnectedPeer<T>>,
    replica_handling: Arc<ConnectedPeersGroup<T>>,
    client_handling: Option<Arc<ConnectedPeersGroup<T>>>,
    replica_tx: Sender<Vec<T>, QueueType<T>>,
    client_tx: Option<Sender<Vec<T>, QueueType<T>>>,
    client_rx: Option<Receiver<Vec<T>, QueueType<T>>>,
    replica_rx: Receiver<Vec<T>, QueueType<T>>,
}

///We make this class Sync and send since the clients are going to be handled by a single class
///And the replicas are going to be handled by another class.
/// There is no possibility of 2 threads accessing the client_rx or replica_rx concurrently
unsafe impl<T> Sync for NodePeers<T> {}

unsafe impl<T> Send for NodePeers<T> {}

impl<T> NodePeers<T> {
    pub fn new(id: NodeId, first_cli: NodeId, batch_size: usize) -> NodePeers<T> {
        //We only want to setup client handling if we are a replica
        let client_handling;

        let client_channel;

        if id < first_cli {
            let (client_tx, client_rx) = channel_init(NODE_CHAN_BOUND);

            client_handling = Some(ConnectedPeersGroup::new(32,
                                                            batch_size,
                                                            client_tx.clone()));
            client_channel = Some((client_tx, client_rx));
        } else {
            client_handling = None;
            client_channel = None;
        };

        //TODO: maybe change the channel bound?
        let (replica_tx, replica_rx) = channel_init(NODE_CHAN_BOUND);

        //TODO: Batch size is not correct, should be the value found in env
        //Both replicas and clients have to interact with replicas, so we always need this pool
        //We have a much larger queue because we don't want small slowdowns slowing down the connections
        //And also because there are few replicas, while there can be a very large amount of clients
        let replica_handling = ConnectedPeersGroup::new(1024, NODE_CHAN_BOUND,
                                                        replica_tx.clone());

        let loopback_address = replica_handling.init_client(id);

        let (cl_tx, cl_rx) = if let Some((cl_tx, cl_rx)) = client_channel {
            (Some(cl_tx), Some(cl_rx))
        } else {
            (None, None)
        };

        let peers = NodePeers {
            first_cli,
            own_id: id,
            peer_loopback: loopback_address,
            replica_handling,
            client_handling,
            replica_tx,
            client_tx: cl_tx,
            client_rx: cl_rx,
            replica_rx,
        };

        peers
    }

    pub fn init_peer_conn(&self, peer: NodeId) -> Arc<ConnectedPeer<T>> {
        println!("Initializing peer connection for peer {:?} on peer {:?}", peer, self.own_id);

        return if peer >= self.first_cli {
            self.client_handling.as_ref().expect("Tried to init client request from client itself?")
                .init_client(peer)
        } else {
            self.replica_handling.init_client(peer)
        };
    }

    pub fn resolve_peer_conn(&self, peer: NodeId) -> Option<Arc<ConnectedPeer<T>>> {
        if peer == self.own_id {
            return Some(self.peer_loopback.clone());
        }

        return if peer < self.first_cli {
            self.replica_handling.get_client_conn(peer)
        } else {
            self.client_handling.as_ref().expect("Tried to resolve client conn in the client")
                .get_client_conn(peer)
        };
    }

    pub fn peer_loopback(&self) -> &Arc<ConnectedPeer<T>> {
        &self.peer_loopback
    }

    pub fn receive_from_clients(&self) -> Result<Vec<T>> {
        return match &self.client_rx {
            None => {
                Err(Error::simple(ErrorKind::Communication))
            }
            Some(rx) => {
                match rx.recv_blk() {
                    Ok(vec) => {
                        Ok(vec)
                    }
                    Err(_) => {
                        Err(Error::simple(ErrorKind::Communication))
                    }
                }
            }
        };
    }

    pub async fn receive_from_client_async(&self) -> Result<Vec<T>> {
        return match &self.client_rx {
            None => {
                Err(Error::simple(ErrorKind::Communication))
            }
            Some(rx) => {
                match rx.recv_fut().await {
                    Ok(vec) => {
                        Ok(vec)
                    }
                    Err(_) => {
                        Err(Error::simple(ErrorKind::Communication))
                    }
                }
            }
        };
    }

    pub fn receive_from_replicas(&self) -> Result<Vec<T>> {
        match self.replica_rx.recv_blk() {
            Ok(vec) => {
                Ok(vec)
            }
            Err(_) => {
                Err(Error::simple(ErrorKind::Communication))
            }
        }
    }

    pub async fn receive_from_replicas_async(&self) -> Result<Vec<T>> {
        match self.replica_rx.recv_fut().await {
            Ok(vec) => {
                Ok(vec)
            }
            Err(_) => {
                Err(Error::simple(ErrorKind::Communication))
            }
        }
    }

    pub fn client_count(&self) -> Option<usize> {
        return match &self.client_handling {
            None => { None }
            Some(client) => {
                Some(client.connected_clients.load(Ordering::Relaxed))
            }
        };
    }

    pub fn replica_count(&self) -> usize {
        return self.replica_handling.connected_clients.load(Ordering::Relaxed);
    }
}

///
///Client pool design, where each pool contains a number of clients (Maximum of BATCH_SIZE clients
/// per pool). This is to prevent starvation for each client, as when we are performing
/// the fair collection of requests from the clients, if there are more clients than batch size
/// then we will get very unfair distribution of requests
///
/// This will push Vecs of T types into the ChannelTx provided
/// The type T is not wrapped in any other way
/// no socket handling is done here
/// This is just built on top of the actual per client connection socket stuff and each socket
/// should push items into its own ConnectedPeer instance
pub struct ConnectedPeersGroup<T: 'static> {
    //We can use mutexes here since there will only be concurrency on client connections and dcs
    //And since each client has his own reference to push data to, this only needs to be accessed by the thread
    //That's producing the batches and the threads of clients connecting and disconnecting
    client_pools: Mutex<Vec<Arc<ConnectedPeersPool<T>>>>,
    client_connections_cache: RwLock<IntMap<Arc<ConnectedPeer<T>>>>,
    connected_clients: AtomicUsize,
    per_client_cache: usize,
    batch_size: usize,
    batch_transmission: Sender<Vec<T>, QueueType<T>>,
}

pub struct ConnectedPeersPool<T: 'static> {
    //We can use mutexes here since there will only be concurrency on client connections and dcs
    //And since each client has his own reference to push data to, this only needs to be accessed by the thread
    //That's producing the batches and the threads of clients connecting and disconnecting
    connected_clients: Mutex<Vec<Arc<ConnectedPeer<T>>>>,
    batch_size: usize,
    batch_transmission: Sender<Vec<T>, QueueType<T>>,
    finish_execution: AtomicBool,
    owner: Arc<ConnectedPeersGroup<T>>,
}

pub struct ConnectedPeer<T> {
    client_id: NodeId,
    sender: Mutex<Option<Sender<T, ClientQueueType<T>>>>,
    receiver: ReceiverPartialMult<T, ClientQueueType<T>>,
}

impl<T: 'static> ConnectedPeersGroup<T> {
    pub fn new(per_client_bound: usize, batch_size: usize, batch_transmission: Sender<Vec<T>, QueueType<T>>) -> Arc<Self> {
        Arc::new(Self {
            client_pools: parking_lot::Mutex::new(Vec::new()),
            client_connections_cache: RwLock::new(IntMap::new()),
            per_client_cache: per_client_bound,
            connected_clients: AtomicUsize::new(0),
            batch_size,
            batch_transmission,
        })
    }

    pub fn init_client(self: &Arc<Self>, peer_id: NodeId) -> Arc<ConnectedPeer<T>> {
        let connected_client = Arc::new(ConnectedPeer::new(peer_id, self.per_client_cache));

        let mut cached_clients = self.client_connections_cache.write();

        cached_clients.insert(peer_id.0 as u64, connected_client.clone());

        drop(cached_clients);

        self.connected_clients.fetch_add(1, Ordering::SeqCst);

        let mut clone_queue = connected_client.clone();

        let mut guard = self.client_pools.lock();

        for pool in &*guard {
            match pool.attempt_to_add(clone_queue) {
                Ok(_) => {
                    return connected_client;
                }
                Err(queue) => {
                    clone_queue = queue;
                }
            }
        }

        //In the case all the pools are already full, allocate a new pool
        let pool = ConnectedPeersPool::new(self.batch_size,
                                           self.batch_transmission.clone(),
                                           Arc::clone(self));

        match pool.attempt_to_add(clone_queue) {
            Ok(_) => {}
            Err(e) => {
                panic!("Failed to add pool to pool list.")
            }
        };

        let pool_clone = pool.clone();

        guard.push(pool);

        let id = guard.len();

        drop (guard);

        pool_clone.start(id as u32);

        connected_client
    }

    pub fn get_client_conn(&self, client_id: NodeId) -> Option<Arc<ConnectedPeer<T>>> {
        let cache_guard = self.client_connections_cache.read();

        return match cache_guard.get(client_id.0 as u64) {
            None => {
                None
            }
            Some(peer) => {
                Some(Arc::clone(peer))
            }
        };
    }

    fn del_cached_clients(&self, clients: Vec<NodeId>) {
        let mut cache_guard = self.client_connections_cache.write();

        for client_id in &clients {
            cache_guard.remove(client_id.0 as u64);
        }

        drop(cache_guard);

        self.connected_clients.fetch_sub(clients.len(), Ordering::Relaxed);
    }

    pub fn del_client(&self, client_id: &NodeId) -> bool {
        let mut cache_guard = self.client_connections_cache.write();

        cache_guard.remove(client_id.0 as u64);

        drop(cache_guard);

        let mut guard = self.client_pools.lock();

        for i in 0..guard.len() {
            match guard[i].attempt_to_remove(client_id) {
                Ok(empty) => {
                    if empty {
                        //Since order of the pools is not really important
                        //Use the O(1) remove instead of the O(n) normal remove
                        guard.swap_remove(i).shutdown();
                    }

                    return true;
                }
                Err(_) => {}
            }
        }

        self.connected_clients.fetch_sub(1, Ordering::SeqCst);

        false
    }
}

impl<T> ConnectedPeersPool<T> {
    //We mark the owner as static since if the pool is active then
    //The owner also has to be active
    pub fn new(client_count: usize, batch_transmission: Sender<Vec<T>, QueueType<T>>,
               owner: Arc<ConnectedPeersGroup<T>>) -> Arc<Self> {
        let result = Self {
            connected_clients: parking_lot::Mutex::new(Vec::new()),
            batch_size: client_count,
            batch_transmission,
            finish_execution: AtomicBool::new(false),
            owner,
        };

        let pool = Arc::new(result);

        pool
    }

    pub fn start(self: Arc<Self>, pool_id: u32) {

        //Spawn the thread that will collect client requests
        //and then send the batches to the channel.
        std::thread::spawn(move || {
            let backoff = BackoffN::new();

            loop {
                if self.finish_execution.load(Ordering::Relaxed) {
                    break;
                }

                let vec = self.collect_requests(self.batch_size, &self.owner);

                //println!("Collected {} requests...pool {} with {} clients", vec.len(), pool_id,
                //         self.connected_clients.lock().len());

                if !vec.is_empty() {
                    self.batch_transmission.send(vec);

                    backoff.reset();
                } else {

                    //std::thread::sleep(Duration::from_secs(1));

                    backoff.snooze();
                }
            }
        });
    }

    pub fn attempt_to_add(&self, client: Arc<ConnectedPeer<T>>) -> std::result::Result<(), Arc<ConnectedPeer<T>>> {
        let mut guard = self.connected_clients.lock();

        if guard.len() < self.batch_size {
            guard.push(client);

            return Ok(());
        }

        Err(client)
    }

    pub fn attempt_to_remove(&self, client_id: &NodeId) -> std::result::Result<bool, ()> {
        let mut guard = self.connected_clients.lock();

        return match guard.iter().position(|client| client.client_id().eq(client_id)) {
            None => {
                Err(())
            }
            Some(position) => {
                guard.swap_remove(position);

                Ok(guard.is_empty())
            }
        };
    }

    pub fn collect_requests(&self, batch_size: usize, owner: &Arc<ConnectedPeersGroup<T>>) -> Vec<T> {
        let mut batch = Vec::with_capacity(batch_size);

        let mut guard = self.connected_clients.lock();

        let mut dced = Vec::new();

        //We can do this because our pooling system prevents the number of clients
        //In each pool to be larger than the batch size, so the requests_per_client is always
        //> 1, leading to no starvation
        let requests_per_client = batch_size / guard.len();
        let requests_remainder = batch_size % guard.len();

        let start_point = fastrand::usize(0..guard.len());

        //We don't want to leave any slot in the batch unfilled...
        let mut next_client_requests = requests_per_client + requests_remainder;

        for index in 0..guard.len() {
            let client = &guard[(start_point + index) % guard.len()];

            if client.is_dc() {
                //FIXME: When multiple clients dc this gets weird.
                dced.push(guard.swap_remove(index).client_id);

                //Assign the remaining slots to the next client
                next_client_requests += requests_per_client;

                continue;
            }

            let rqs_dumped = client.dump_n_requests(next_client_requests, &mut batch);

            //Leave the requests that were not used open for the following clients, in a greedy fashion
            next_client_requests -= rqs_dumped;
            //Add the requests for the upcoming requests
            next_client_requests += requests_per_client;
        }

        drop(guard);

        //This might cause some lag since it has to access the intmap, but
        //Should be fine as it will only happen on client dcs
        if !dced.is_empty() {
            owner.del_cached_clients(dced);
        }

        batch
    }

    pub fn shutdown(&self) {
        self.finish_execution.store(true, Ordering::Relaxed);
    }
}

impl<T> ConnectedPeer<T> {
    pub fn new(client_id: NodeId, per_client_bound: usize) -> Self {
        let (tx, rx) = client_channel_init(per_client_bound);

        Self {
            client_id,
            sender: Mutex::new(Some(tx)),
            receiver: rx
        }
    }

    pub fn client_id(&self) -> &NodeId {
        &self.client_id
    }

    pub fn is_dc(&self) -> bool {
        self.receiver.is_dc()
    }

    pub fn disconnect(&self) {
        //By destroying the sender, we close the channel when
        self.sender.lock().take();
    }

    ///Dump n requests into the provided vector
    ///Returns the amount of requests that were dumped into the array
    pub fn dump_n_requests(&self, rq_bound: usize, dump_vec: &mut Vec<T>) -> usize {
        self.receiver.try_recv_mult(dump_vec, rq_bound).unwrap()
    }

    pub async fn push_request(&self, msg: T) {
        let sender = self.sender.lock().as_ref().unwrap().clone();

        match sender.send_async(msg).await {
            Ok(_) => {}
            Err(err) => {
                panic!("Failed to send because {:?}", err);
            }
        };
    }
}

