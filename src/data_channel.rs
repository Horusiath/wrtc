use crate::error::Error;
use arc_swap::ArcSwap;
use bytes::Bytes;
use futures_util::Future;
use futures_util::{ready, Sink, Stream};
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;
use tokio_util::sync::ReusableBoxFuture;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;

pub struct DataChannel {
    dc: Arc<RTCDataChannel>,
    status: Arc<ArcSwap<DataChannelStreamState>>,
    /// Buffer used for data channel internally to stash received messages waiting to be read.
    sender: UnboundedSender<Result<Option<DataChannelMessage>, Error>>,
    /// Buffer reader part, used for asynchronously iterating over incoming messages.
    receiver: UnboundedReceiver<Result<Option<DataChannelMessage>, Error>>,
    /// Async sink-send state. It's dependent upon `status`, which describes the general state of
    /// the channel (sink_state only describes status of send operation).
    sink_state: DataChannelSinkState,
    /// Awaiter used by the send operation. When status is `Waiting`, nested future awaits for the
    /// underlying channel to open. When status is `Open` and sink state is `Awaiting`, nested
    /// future awaits for send to complete.
    send_waiter: ReusableBoxFuture<'static, Result<(), Error>>,
}

impl DataChannel {
    pub fn new(dc: Arc<RTCDataChannel>) -> Self {
        let (sender, receiver) = unbounded_channel();
        let status = Arc::new(ArcSwap::new(DataChannelStreamState::waiting()));
        let s = Arc::downgrade(&status);
        dc.on_open(Box::new(move || {
            let s = s.clone();
            Box::pin(async move {
                if let Some(status) = s.upgrade() {
                    status.rcu(|old| match &**old {
                        DataChannelStreamState::Waiting { ready } => {
                            ready.notify_waiters();
                            DataChannelStreamState::open()
                        }
                        _ => old.clone(),
                    });
                }
            })
        }));
        let s = Arc::downgrade(&status);
        let tx = sender.clone();
        dc.on_close(Box::new(move || {
            let s = s.clone();
            let tx = tx.clone();
            Box::pin(async move {
                if let Some(status) = s.upgrade() {
                    let old = status.swap(DataChannelStreamState::closed_gracefully());
                    match &*old {
                        DataChannelStreamState::Waiting { ready } => ready.notify_waiters(),
                        DataChannelStreamState::Open => {
                            let _ = tx.send(Ok(None));
                        }
                        DataChannelStreamState::Closed { .. } => {}
                    }
                }
            })
        }));
        let s = Arc::downgrade(&status);
        let tx = sender.clone();
        dc.on_error(Box::new(move |e| {
            let s = s.clone();
            let tx = tx.clone();
            Box::pin(async move {
                if let Some(status) = s.upgrade() {
                    let error: Error = e.into();
                    let old = status.swap(DataChannelStreamState::failed(error.clone()));
                    match &*old {
                        DataChannelStreamState::Waiting { ready } => ready.notify_waiters(),
                        DataChannelStreamState::Open => {
                            let _ = tx.send(Err(error.clone()));
                        }
                        DataChannelStreamState::Closed { .. } => {}
                    }
                }
            })
        }));
        let s = Arc::downgrade(&status);
        let tx = sender.clone();
        dc.on_message(Box::new(move |msg| {
            let s = s.clone();
            let tx = tx.clone();
            Box::pin(async move {
                if let Some(status) = s.upgrade() {
                    let status = Self::ready_internal(&status).await;
                    if status.is_open() {
                        let _ = tx.send(Ok(Some(msg)));
                    }
                }
            })
        }));
        let notify = status.clone();
        let send_waiter = ReusableBoxFuture::new(async move {
            match &*Self::ready_internal(&notify).await {
                DataChannelStreamState::Open { .. } => Ok(()),
                DataChannelStreamState::Closed { reason } => {
                    if let Some(reason) = reason {
                        Err(reason.clone())
                    } else {
                        Ok(())
                    }
                }
                DataChannelStreamState::Waiting { .. } => {
                    panic!("Defect: ready_internal returned non-ready state")
                }
            }
        });
        DataChannel {
            dc,
            sender,
            receiver,
            status,
            sink_state: DataChannelSinkState::Idle,
            send_waiter,
        }
    }

    pub fn label(&self) -> &str {
        self.dc.label()
    }

    pub fn id(&self) -> u16 {
        self.dc.id()
    }

    pub fn is_open(&self) -> bool {
        self.status.load().is_open()
    }

    pub fn is_closed(&self) -> bool {
        self.status.load().is_closed()
    }

    /// Wait's until the data channel is ready to operate (it becomes open).
    ///
    /// # Returns
    ///
    /// If channel has been opened successfully, this method returns `Ok(true)`.
    /// If channel has been closed gracefully it returns `Ok(false)`.
    /// If channel has been closed due to failure it returns `Err`.
    pub async fn ready(&self) -> Result<bool, Error> {
        let status = Self::ready_internal(&self.status).await;
        match &*status {
            DataChannelStreamState::Open { .. } => Ok(true),
            DataChannelStreamState::Closed { reason } => {
                if let Some(reason) = reason {
                    Err(reason.clone())
                } else {
                    Ok(false)
                }
            }
            DataChannelStreamState::Waiting { .. } => {
                panic!("Defect: ready_internal returned non-ready state")
            }
        }
    }

    /// Asynchronously closes current stream.
    async fn close_internal(&mut self) -> Result<(), Error> {
        self.status.rcu(|old| match &**old {
            DataChannelStreamState::Waiting { ready } => {
                ready.notify_waiters();
                DataChannelStreamState::closed_gracefully()
            }
            DataChannelStreamState::Open => DataChannelStreamState::closed_gracefully(),
            DataChannelStreamState::Closed { .. } => old.clone(),
        });
        let _ = self.sender.send(Ok(None));
        self.receiver.close();
        if self.sink_state == DataChannelSinkState::Awaiting {
            poll_fn(|cx| self.send_waiter.poll(cx)).await?;
        }
        self.dc.close().await?;
        self.sender.closed().await;

        Ok(())
    }

    async fn ready_internal(
        status: &Arc<ArcSwap<DataChannelStreamState>>,
    ) -> Arc<DataChannelStreamState> {
        loop {
            let status = status.load_full();
            match &*status {
                DataChannelStreamState::Open { .. } => return status,
                DataChannelStreamState::Closed { .. } => {
                    return status;
                }
                DataChannelStreamState::Waiting { ready } => {
                    ready.notified().await;
                }
            }
        }
    }

    async fn recv_internal(&mut self) -> Option<Result<DataChannelMessage, Error>> {
        let status = Self::ready_internal(&self.status).await;
        match &*status {
            DataChannelStreamState::Open => {
                let msg = self.receiver.recv().await?;
                match msg {
                    Ok(None) => {
                        self.receiver.close();
                        None
                    }
                    Ok(Some(msg)) => Some(Ok(msg)),
                    Err(err) => Some(Err(err)),
                }
            }
            DataChannelStreamState::Closed { reason } => {
                if let Some(reason) = reason {
                    Some(Err(reason.clone()))
                } else {
                    None
                }
            }
            DataChannelStreamState::Waiting { .. } => {
                panic!("Defect: should not happen")
            }
        }
    }
}

impl Stream for DataChannel {
    type Item = Result<DataChannelMessage, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut fut = Box::pin(self.recv_internal());
        unsafe { Pin::new_unchecked(&mut fut) }.poll(cx)
    }
}

impl std::fmt::Debug for DataChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WRTCDataStream")
            .field("label", &self.dc.label())
            .field("state", &self.status)
            .finish()
    }
}

impl AsRef<RTCDataChannel> for DataChannel {
    fn as_ref(&self) -> &RTCDataChannel {
        &self.dc
    }
}

#[derive(Debug)]
enum DataChannelStreamState {
    /// Underlying data channel is waiting to become open.
    Waiting {
        /// Notifier used when the underlying data channel changes it's state from ready to open.
        ready: Notify,
    },
    /// Underlying data channel is open and ready to send/receive messages.
    Open,
    /// Underlying data channel has already been closed.
    Closed {
        /// Optional error reason, why the channel has been closed.
        reason: Option<Error>,
    },
}

#[derive(Debug, Copy, Clone, PartialOrd, PartialEq)]
enum DataChannelSinkState {
    Idle,
    Awaiting,
}

impl Sink<Bytes> for DataChannel {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match &**self.status.load() {
            DataChannelStreamState::Open => match &mut self.sink_state {
                DataChannelSinkState::Idle => Poll::Ready(Ok(())),
                DataChannelSinkState::Awaiting => {
                    let res = ready!(self.send_waiter.poll(cx));
                    self.sink_state = DataChannelSinkState::Idle;
                    Poll::Ready(res)
                }
            },
            DataChannelStreamState::Waiting { .. } => {
                // in a context of waiting state send_waiter represents future waiting for channel
                // to open
                self.send_waiter.poll(cx)
            }
            DataChannelStreamState::Closed { .. } => Poll::Ready(Err(Error::channel_closed())),
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        let dc = self.dc.clone();
        self.send_waiter.set(async move {
            dc.send(&item).await?;
            Ok(())
        });
        self.sink_state = DataChannelSinkState::Awaiting;
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match &self.sink_state {
            DataChannelSinkState::Idle => {
                if self.status.load().is_closed() {
                    Poll::Ready(Err(Error::channel_closed()))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            DataChannelSinkState::Awaiting => {
                let res = ready!(self.send_waiter.poll(cx));
                self.sink_state = DataChannelSinkState::Idle;
                Poll::Ready(res)
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut fut = Box::pin(self.close_internal());
        Pin::new(&mut fut).poll(cx)
    }
}

impl DataChannelStreamState {
    fn waiting() -> Arc<Self> {
        Arc::new(DataChannelStreamState::Waiting {
            ready: Notify::new(),
        })
    }

    fn open() -> Arc<Self> {
        Arc::new(DataChannelStreamState::Open)
    }

    fn closed_gracefully() -> Arc<Self> {
        Arc::new(DataChannelStreamState::Closed { reason: None })
    }

    fn failed(reason: Error) -> Arc<Self> {
        Arc::new(DataChannelStreamState::Closed {
            reason: Some(reason),
        })
    }

    pub fn is_waiting(&self) -> bool {
        if let DataChannelStreamState::Waiting { .. } = self {
            true
        } else {
            false
        }
    }

    pub fn is_open(&self) -> bool {
        match self {
            DataChannelStreamState::Open { .. } => true,
            _ => false,
        }
    }

    pub fn is_closed(&self) -> bool {
        if let DataChannelStreamState::Closed { .. } = self {
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod test {
    use crate::error::Error;
    use crate::peer_connection::{Options, PeerConnection};
    use bytes::Bytes;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::Arc;
    use tokio::task::JoinHandle;

    fn exchange(
        from: Arc<PeerConnection>,
        to: Arc<PeerConnection>,
    ) -> JoinHandle<Result<(), Error>> {
        tokio::spawn(async move {
            while let Some(signal) = from.signal().await {
                to.apply_signal(signal).await?;
            }
            Ok(())
        })
    }

    #[tokio::test]
    async fn basic() -> Result<(), Error> {
        let options = Options::with_data_channels(&["test-dc"]);
        let p1 = Arc::new(PeerConnection::start(true, options.clone()).await?);
        let p2 = Arc::new(PeerConnection::start(false, options).await?);

        let _ = exchange(p1.clone(), p2.clone());
        let _ = exchange(p2.clone(), p1.clone());

        p1.connected().await?;
        p2.connected().await?;

        {
            let mut dc1 = p1.data_channels().next().await.unwrap();
            let mut dc2 = p2.data_channels().next().await.unwrap();

            dc1.ready().await?;
            dc2.ready().await?;

            assert_eq!(dc1.label(), "test-dc");
            assert_eq!(dc2.label(), "test-dc");

            let data: Bytes = "hello".into();
            dc1.send(data.clone()).await?;
            let msg = dc2.next().await.unwrap()?;
            assert_eq!(msg.data, data);
        }

        p1.close().await?;
        p2.close().await?;

        Ok(())
    }
}
