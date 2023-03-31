use bytes::{Bytes, BytesMut};
use log::error;
use febft_common::channel::{new_oneshot_channel, OneShotRx};
use febft_common::crypto::hash::Digest;
use febft_common::error::*;
use febft_common::threadpool;
use crate::message::{Header, NetworkMessageKind};
use crate::serialize;
use crate::serialize::Serializable;

//TODO: Statistics

/// Serialize and digest a given message.
/// Returns a OneShotRx that can be recv() or awaited depending on whether it's being used
/// in synchronous or asynchronous workloads.
pub(crate) fn serialize_digest_message<M: Serializable>(message: NetworkMessageKind<M>) -> OneShotRx<Result<(Bytes, Digest)>> {
    let (tx, rx) = new_oneshot_channel();

    threadpool::execute(|| {
        // serialize
        // TODO: Use a memory pool here
        let mut buf = Vec::with_capacity(512);

        let digest = match serialize::serialize_digest::<Vec<u8>, M>(&message, &mut buf) {
            Ok(dig) => dig,
            Err(err) => {
                error!("Failed to serialize message {:?}. Message is {:?}", err, message);

                panic!("Failed to serialize message {:?}", err);
            }
        };

        let buf = Bytes::from(buf);

        tx.send(Ok((buf, digest))).unwrap();
    });

    rx
}

/// Deserialize the message that is contained in the given payload.
/// Returns a OneShotRx that can be recv() or awaited depending on whether it's being used
/// in synchronous or asynchronous workloads.
/// Also returns the bytes so we can re utilize them for our next operation.
pub(crate) fn deserialize_message<M: Serializable>(header: Header, payload: BytesMut) -> OneShotRx<Result<(NetworkMessageKind<M>, BytesMut)>> {
    let (tx, rx) = new_oneshot_channel();

    threadpool::execute(|| {
        //TODO: Verify signatures

        // deserialize payload
        let message = match serialize::deserialize_message::<&[u8], M>(&payload[..header.payload_length()]) {
            Ok(m) => m,
            Err(err) => {
                // errors deserializing -> faulty connection;
                // drop this socket
                error!("{:?} // Failed to deserialize message {:?}", header.to(), err);

                tx.send(Err(Error::wrapped(ErrorKind::CommunicationSerialize, err))).unwrap();

                return;
            }
        };

        tx.send(Ok((message, payload))).unwrap();
    });

    rx
}