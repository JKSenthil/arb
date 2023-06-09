use std::{
    cell::RefCell,
    convert::Infallible,
    hash::BuildHasherDefault,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
};

use async_trait::async_trait;
use bytes::{Buf as _, BytesMut};
use ethers::{
    providers::{IpcError, JsonRpcClient, PubsubClient},
    types::U256,
};
use futures_channel::mpsc;
use futures_util::stream::StreamExt as _;
use hashers::fx_hash::FxHasher64;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{value::RawValue, Deserializer};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{
        unix::{ReadHalf, WriteHalf},
        UnixStream,
    },
    runtime,
    sync::oneshot::{self},
};

use super::common::{BatchRequest, BatchResponse, JsonRpcError, Params, Request, Response};

type FxHashMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher64>>;

type Pending = oneshot::Sender<Result<Box<RawValue>, JsonRpcError>>;
type BatchPending = oneshot::Sender<BatchResponse>;
type Subscription = mpsc::UnboundedSender<Box<RawValue>>;

/// Unix Domain Sockets (IPC) transport.
#[derive(Debug, Clone)]
pub struct Ipc {
    id: Arc<AtomicU64>,
    request_tx: mpsc::UnboundedSender<TransportMessage>,
}

#[derive(Debug)]
enum TransportMessage {
    Request {
        id: u64,
        request: Box<[u8]>,
        sender: Pending,
    },
    Subscribe {
        id: U256,
        sink: Subscription,
    },
    Unsubscribe {
        id: U256,
    },
    Batch {
        id: u64,
        requests: Box<[u8]>,
        sender: BatchPending,
    },
}

impl Ipc {
    /// Creates a new IPC transport from a given path using Unix sockets.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        let id = Arc::new(AtomicU64::new(1));
        let (request_tx, request_rx) = mpsc::unbounded();

        let stream = UnixStream::connect(path).await?;
        spawn_ipc_server(stream, request_rx);

        Ok(Self { id, request_tx })
    }

    /// Executes the batch of JSON-RPC requests.
    ///
    /// # Arguments
    ///
    /// `batch` - batch of JSON-RPC requests.
    pub async fn execute_batch(&self, batch: &mut BatchRequest) -> Result<BatchResponse, IpcError> {
        // The request id of the client is incremented by the batch size.
        let next_id = self.id.fetch_add(batch.len() as u64, Ordering::SeqCst);

        // Ids in the batch will start from next_id.
        batch.set_ids(next_id).unwrap();
        // Send the message.
        let (sender, receiver) = oneshot::channel();
        // The id of the first request in the batch matches the id of the channel in the pending
        // map.
        let payload = TransportMessage::Batch {
            id: next_id,
            requests: serde_json::to_vec(batch.requests().unwrap())
                .unwrap()
                .into_boxed_slice(),
            sender,
        };

        // Send the data.
        self.send(payload)?;

        // Wait for the response (the request itself may have errors as well).
        let res = receiver.await?;

        // Returns the batch of JSON-RPC responses.
        Ok(res)
    }

    fn send(&self, msg: TransportMessage) -> Result<(), IpcError> {
        self.request_tx
            .unbounded_send(msg)
            .map_err(|_| IpcError::ChannelError("IPC server receiver dropped".to_string()))?;

        Ok(())
    }
}

#[async_trait]
impl JsonRpcClient for Ipc {
    type Error = IpcError;

    async fn request<T: Serialize + Send + Sync, R: DeserializeOwned>(
        &self,
        method: &str,
        params: T,
    ) -> Result<R, IpcError> {
        let next_id = self.id.fetch_add(1, Ordering::SeqCst);

        // Create the request and initialize the response channel
        let (sender, receiver) = oneshot::channel();
        let payload = TransportMessage::Request {
            id: next_id,
            request: serde_json::to_vec(&Request::new(next_id, method, params))?.into_boxed_slice(),
            sender,
        };

        // Send the request to the IPC server to be handled.
        self.send(payload)?;

        // Wait for the response from the IPC server.
        let res = receiver.await.unwrap().unwrap();

        // Parse JSON response.
        Ok(serde_json::from_str(res.get())?)
    }
}

impl PubsubClient for Ipc {
    type NotificationStream = mpsc::UnboundedReceiver<Box<RawValue>>;

    fn subscribe<T: Into<U256>>(&self, id: T) -> Result<Self::NotificationStream, IpcError> {
        let (sink, stream) = mpsc::unbounded();
        self.send(TransportMessage::Subscribe {
            id: id.into(),
            sink,
        })?;
        Ok(stream)
    }

    fn unsubscribe<T: Into<U256>>(&self, id: T) -> Result<(), IpcError> {
        self.send(TransportMessage::Unsubscribe { id: id.into() })
    }
}

fn spawn_ipc_server(stream: UnixStream, request_rx: mpsc::UnboundedReceiver<TransportMessage>) {
    // 65 KiB should be more than enough for this thread, as all unbounded data
    // growth occurs on heap-allocated data structures and buffers and the call
    // stack is not going to do anything crazy either
    const STACK_SIZE: usize = 1 << 16;
    // spawn a light-weight thread with a thread-local async runtime just for
    // sending and receiving data over the IPC socket
    let _ = thread::Builder::new()
        .name("ipc-server-thread".to_string())
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let rt = runtime::Builder::new_current_thread()
                .enable_io()
                .build()
                .expect("failed to create ipc-server-thread async runtime");

            rt.block_on(run_ipc_server(stream, request_rx));
        })
        .expect("failed to spawn ipc server thread");
}

async fn run_ipc_server(
    mut stream: UnixStream,
    request_rx: mpsc::UnboundedReceiver<TransportMessage>,
) {
    // the shared state for both reads & writes
    let shared = Shared::default();

    // split the stream and run two independent concurrently (local), thereby
    // allowing reads and writes to occurr concurrently
    let (reader, writer) = stream.split();
    let read = shared.handle_ipc_reads(reader);
    let write = shared.handle_ipc_writes(writer, request_rx);

    // run both loops concurrently, until either encounts an error
    if let Err(e) = futures_util::try_join!(read, write) {
        match e {
            IpcError::ServerExit => {}
            err => tracing::error!(?err, "exiting IPC server due to error"),
        }
    }
}

struct Shared {
    pending: RefCell<FxHashMap<u64, Pending>>,
    batch_pending: RefCell<FxHashMap<u64, BatchPending>>,
    subs: RefCell<FxHashMap<U256, Subscription>>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            pending: FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default()).into(),
            batch_pending: FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default())
                .into(),
            subs: FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default()).into(),
        }
    }
}

impl Shared {
    async fn handle_ipc_reads(&self, reader: ReadHalf<'_>) -> Result<Infallible, IpcError> {
        let mut reader = BufReader::new(reader);
        let mut buf = BytesMut::with_capacity(4096);

        loop {
            // try to read the next batch of bytes into the buffer
            let read = reader.read_buf(&mut buf).await?;
            if read == 0 {
                // eof, socket was closed
                return Err(IpcError::ServerExit);
            }

            // parse the received bytes into 0-n jsonrpc messages
            let read = self.handle_bytes(&buf)?;
            // split off all bytes that were parsed into complete messages
            // any remaining bytes that correspond to incomplete messages remain
            // in the buffer
            buf.advance(read);
        }
    }

    async fn handle_ipc_writes(
        &self,
        mut writer: WriteHalf<'_>,
        mut request_rx: mpsc::UnboundedReceiver<TransportMessage>,
    ) -> Result<Infallible, IpcError> {
        use TransportMessage::*;

        while let Some(msg) = request_rx.next().await {
            match msg {
                Request {
                    id,
                    request,
                    sender,
                } => {
                    let prev = self.pending.borrow_mut().insert(id, sender);
                    assert!(prev.is_none(), "replaced pending IPC request (id={})", id);

                    if let Err(err) = writer.write_all(&request).await {
                        tracing::error!("IPC connection error: {:?}", err);
                        self.pending.borrow_mut().remove(&id);
                    }
                }
                Batch {
                    id,
                    requests,
                    sender,
                } => {
                    let prev = self.batch_pending.borrow_mut().insert(id, sender);
                    assert!(prev.is_none(), "replaced pending IPC request (id={})", id);

                    if let Err(err) = writer.write_all(&requests).await {
                        tracing::error!("IPC connection error: {:?}", err);
                        self.batch_pending.borrow_mut().remove(&id);
                    }
                }
                Subscribe { id, sink } => {
                    if self.subs.borrow_mut().insert(id, sink).is_some() {
                        tracing::warn!(
                            %id,
                            "replaced already-registered subscription"
                        );
                    }
                }
                Unsubscribe { id } => {
                    if self.subs.borrow_mut().remove(&id).is_none() {
                        tracing::warn!(
                            %id,
                            "attempted to unsubscribe from non-existent subscription"
                        );
                    }
                }
            }
        }

        // the request receiver will only be closed if the sender instance
        // located within the transport handle is dropped, this is not truly an
        // error but leads to the `try_join` in `run_ipc_server` to cancel the
        // read half future
        Err(IpcError::ServerExit)
    }

    /// Tries to  deserialize all complete jsonrpc responses in the buffer.
    fn parse_response(&self, bytes: &BytesMut) -> Result<usize, IpcError> {
        let mut de = Deserializer::from_slice(bytes.as_ref()).into_iter();
        while let Some(Ok(response)) = de.next() {
            match response {
                Response::Success { id, result } => self.send_response(id, Ok(result.to_owned())),
                Response::Error { id, error } => self.send_response(id, Err(error)),
                Response::Notification { params, .. } => self.send_notification(params),
            };
        }

        Ok(de.byte_offset())
    }

    fn parse_batch(&self, bytes: &BytesMut) -> Result<usize, IpcError> {
        let mut de = Deserializer::from_slice(bytes.as_ref()).into_iter();
        while let Some(Ok(responses)) = de.next() {
            // Build the batch with the JSON-RPC responses.
            let batch = BatchResponse::new(responses);
            // Get id.
            let id = batch.id().unwrap();
            // Send the batch.
            self.send_batch(id, batch);
        }

        Ok(de.byte_offset())
    }

    fn handle_bytes(&self, bytes: &BytesMut) -> Result<usize, IpcError> {
        Ok(self.parse_response(bytes)? + self.parse_batch(bytes)?)
    }

    fn send_response(&self, id: u64, result: Result<Box<RawValue>, JsonRpcError>) {
        // retrieve the channel sender for responding to the pending request
        let response_tx = match self.pending.borrow_mut().remove(&id) {
            Some(tx) => tx,
            None => {
                tracing::warn!(%id, "no pending request exists for the response ID");
                return;
            }
        };

        // a failure to send the response indicates that the pending request has
        // been dropped in the mean time
        let _ = response_tx.send(result.map_err(Into::into));
    }

    fn send_batch(&self, id: u64, result: BatchResponse) {
        // retrieve the channel sender for responding to the pending batch
        let response_tx = match self.batch_pending.borrow_mut().remove(&id) {
            Some(tx) => tx,
            None => {
                tracing::warn!(%id, "no pending batch exists for the response ID");
                return;
            }
        };

        // a failure to send the response indicates that the pending request has
        // been dropped in the mean time
        let _ = response_tx.send(result);
    }

    /// Sends notification through the channel based on the ID of the subscription.
    /// This handles streaming responses.
    fn send_notification(&self, params: Params<'_>) {
        // retrieve the channel sender for notifying the subscription stream
        let subs = self.subs.borrow();
        let tx = match subs.get(&params.subscription) {
            Some(tx) => tx,
            None => {
                tracing::warn!(
                    id = ?params.subscription,
                    "no subscription exists for the notification ID"
                );
                return;
            }
        };

        // a failure to send the response indicates that the pending request has
        // been dropped in the mean time (and should have been unsubscribed!)
        let _ = tx.unbounded_send(params.result.to_owned());
    }
}
