use crate::data_channel::DataChannel;
use crate::error::Error;
use arc_swap::{ArcSwap, Guard};
use std::collections::HashMap;
use std::fmt::Formatter;
use std::sync::{Arc, Weak};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, Notify};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::{APIBuilder, API};
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::signaling_state::RTCSignalingState;
use webrtc::peer_connection::RTCPeerConnection;

/// WebRTC peer connection with simplified access patterns.
pub struct PeerConnection {
    api: API,
    pc: Arc<RTCPeerConnection>,
    status: PeerConnectionState,
    initiator: bool,
    data_channels: PeerConnectionDataChannels,
    signal_sender: UnboundedSender<Signal>,
    signal_receiver: Mutex<UnboundedReceiver<Signal>>,
}

impl PeerConnection {
    /// Starts a new instance of [PeerConnection].
    ///
    /// In order to exchange necessary data between initiator/acceptor peers, you need to follow
    /// messages coming from [PeerConnection::listen] on one peer and apply them on another via
    /// [PeerConnection::signal].
    ///
    /// Use [PeerConnection::connected] in order to await for connection to be established.
    /// Use [PeerConnection::close] in order to gracefully close the connection.
    pub async fn start(initiator: bool, mut options: Options) -> Result<Self, Error> {
        // Create a MediaEngine object to configure the supported codec
        let mut media_engine = MediaEngine::default();

        // Register default codecs
        media_engine.register_default_codecs()?;

        // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
        // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
        // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
        // for each PeerConnection.
        let mut registry = Registry::new();

        // Use the default set of Interceptors
        registry = register_default_interceptors(registry, &mut media_engine)?;

        // Create the API object with the MediaEngine
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let status = PeerConnectionState::default();

        // Create a new RTCPeerConnection
        let peer_connection = Arc::new(api.new_peer_connection(options.rtc_config).await?);
        let (data_channels_tx, data_channels) = unbounded_channel();
        let data_channels = PeerConnectionDataChannels::new(data_channels);
        let (signal_sender, signal_receiver) = unbounded_channel();

        {
            let status = status.weak_ref();
            peer_connection.on_peer_connection_state_change(Box::new(move |s| {
                if let Some(status) = PeerConnectionState::upgrade(&status) {
                    match s {
                        RTCPeerConnectionState::Connected => {
                            let _ = status.set_ready();
                        }
                        RTCPeerConnectionState::Failed => {
                            let _ = status.set_failed(webrtc::Error::ErrConnectionClosed.into());
                        }
                        RTCPeerConnectionState::Closed => {
                            let _ = status.set_closed();
                        }
                        RTCPeerConnectionState::Disconnected => {}
                        RTCPeerConnectionState::Unspecified => {}
                        RTCPeerConnectionState::New => {}
                        RTCPeerConnectionState::Connecting => {}
                    }
                }
                Box::pin(async move {})
            }));
        }
        {
            let status = status.weak_ref();
            peer_connection.on_ice_connection_state_change(Box::new(move |s| {
                if let Some(status) = PeerConnectionState::upgrade(&status) {
                    match s {
                        RTCIceConnectionState::Failed => {
                            let _ =
                                status.set_failed(webrtc::Error::ErrICEConnectionNotStarted.into());
                        }
                        RTCIceConnectionState::Closed => {
                            let _ = status.set_closed();
                        }
                        RTCIceConnectionState::Completed | RTCIceConnectionState::Connected => {
                            // negotiation completed
                            let _ = status.set_ready();
                        }
                        RTCIceConnectionState::Unspecified => {}
                        RTCIceConnectionState::New => {}
                        RTCIceConnectionState::Checking => {}
                        RTCIceConnectionState::Disconnected => {}
                    }
                }
                Box::pin(async move {})
            }));
        }
        {
            let signals = signal_sender.clone();
            peer_connection.on_ice_candidate(Box::new(move |candidate| {
                if let Some(candidate) = candidate {
                    let _ = signals.send(Signal::Candidate(candidate));
                } else {
                    // ICE complete
                }
                Box::pin(async move {})
            }));
        }
        {
            let status = status.weak_ref();
            peer_connection.on_ice_gathering_state_change(Box::new(move |s| {
                match s {
                    RTCIceGathererState::Unspecified => {}
                    RTCIceGathererState::New => {}
                    RTCIceGathererState::Gathering => {}
                    RTCIceGathererState::Complete => {}
                    RTCIceGathererState::Closed => {}
                }
                Box::pin(async move {})
            }));
        }
        {
            let status = status.weak_ref();
            let pc = Arc::downgrade(&peer_connection);
            peer_connection.on_signaling_state_change(Box::new(move |s| {

                match s {
                    RTCSignalingState::Unspecified => {}
                    RTCSignalingState::Stable => {}
                    RTCSignalingState::HaveLocalOffer => {}
                    RTCSignalingState::HaveRemoteOffer => {}
                    RTCSignalingState::HaveLocalPranswer => {}
                    RTCSignalingState::HaveRemotePranswer => {}
                    RTCSignalingState::Closed => {}
                }
                Box::pin(async move {})
            }));
        }
        {
            let status = status.weak_ref();
            let pc = Arc::downgrade(&peer_connection);
            let signals = signal_sender.clone();
            peer_connection.on_negotiation_needed(Box::new(move || {
                // reset the negotiation state
                let status = status.clone();
                let pc = pc.clone();
                let signals = signals.clone();
                Box::pin(async move {
                    if let Some(status) = PeerConnectionState::upgrade(&status) {
                        let negotiation = Negotiation::new(pc, initiator);
                        if status.set_negotiating(negotiation).is_ok() {
                            if let InnerState::Negotiating(n) = &**status.get() {
                                match n.initiate().await {
                                    Ok(Some(offer)) => {
                                        let _ = signals.send(Signal::Sdp(offer));
                                    }
                                    Ok(None) => {
                                        // do nothing
                                    }
                                    Err(cause) => {
                                        let _ = status.set_failed(cause);
                                    }
                                }
                            }
                        }
                    }
                })
            }));
        }
        {
            let data_channels_tx = data_channels_tx.clone();
            peer_connection.on_data_channel(Box::new(move |dc| {
                data_channels_tx.send(DataChannel::new(dc)).unwrap();
                Box::pin(async move {})
            }));
        }

        if initiator {
            // Create a datachannel with label 'data'
            for (label, config) in options.labels.drain() {
                let dc = peer_connection.create_data_channel(&label, config).await?;
                let _ = data_channels_tx.send(DataChannel::new(dc));
            }
        }

        let pc = PeerConnection {
            initiator: true,
            api,
            status,
            pc: peer_connection,
            data_channels,
            signal_sender,
            signal_receiver: Mutex::new(signal_receiver),
        };

        Ok(pc)
    }

    /// Check if current peer connection is in process of being negotiated with its remote
    /// counterpart.
    pub fn is_negotiating(&self) -> bool {
        if let InnerState::Negotiating(_) = &**self.status.get() {
            true
        } else {
            false
        }
    }

    /// Returns the reference to stream of [DataChannel]s established by the initiator for this
    /// peer connection pair.
    pub fn data_channels(&self) -> &PeerConnectionDataChannels {
        &self.data_channels
    }

    /// This method allows to await until the peer connection pair negotiation is finished.
    pub async fn connected(&self) -> Result<(), Error> {
        status_connected(&self.status).await
    }

    /// Listen to the next [Signal] message coming out of the current [PeerConnection]. This signal
    /// should be serialized, passed over to its remote counterpart and applied there using
    /// [PeerConnection::signal].
    pub async fn listen(&self) -> Option<Signal> {
        if self.status.get().is_closed() {
            None
        } else {
            let mut signals = self.signal_receiver.lock().await;
            signals.recv().await
        }
    }

    /// Apply [Signal]s received from the remote [PeerConnection].
    pub async fn signal(&self, signal: Signal) -> Result<(), Error> {
        match signal {
            Signal::Renegotiate => {
                if !self.status.get().is_closed() && self.initiator {
                    let negotiation = Negotiation::new(Arc::downgrade(&self.pc), self.initiator);
                    self.status.set_negotiating(negotiation)?;
                    if let InnerState::Negotiating(n) = &**self.status.get() {
                        match n.initiate().await {
                            Ok(Some(offer)) => {
                                let _ = self.signal_sender.send(Signal::Sdp(offer));
                            }
                            Ok(None) => {
                                // do nothing
                            }
                            Err(cause) => {
                                let _ = self.status.set_failed(cause);
                            }
                        }
                    }
                }
            }
            Signal::Candidate(candidate) => {
                if let Some(_) = self.pc.remote_description().await {
                    let candidate = candidate.to_json()?;
                    self.pc.add_ice_candidate(candidate).await?;
                } else if let InnerState::Negotiating(n) = &**self.status.get() {
                    let mut candidates = n.pending_candidates.lock().await;
                    candidates.push(candidate);
                }
            }
            Signal::Sdp(sdp) => {
                self.pc.set_remote_description(sdp).await?;
                if let Some(sdp) = self.pc.remote_description().await {
                    if sdp.sdp_type == RTCSdpType::Offer {
                        self.answer().await?;
                    }
                }
                if let InnerState::Negotiating(n) = &**self.status.get() {
                    let mut candidates = n.pending_candidates.lock().await;
                    for candidate in candidates.drain(..) {
                        let candidate = candidate.to_json()?;
                        self.pc.add_ice_candidate(candidate).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn answer(&self) -> Result<(), Error> {
        if self.status.get().is_closed() {
            return Err(Error::channel_closed());
        } else {
            let answer = self.pc.create_answer(None).await?;
            self.pc.set_local_description(answer.clone()).await?;
            let _ = self.signal_sender.send(Signal::Sdp(answer));
            Ok(())
        }
    }

    /// Gracefully close current [PeerConnection].
    pub async fn close(&self) -> Result<(), Error> {
        self.status.set_closed()?;
        self.pc.close().await?;
        Ok(())
    }
}

impl AsRef<RTCPeerConnection> for PeerConnection {
    fn as_ref(&self) -> &RTCPeerConnection {
        &self.pc
    }
}

impl std::fmt::Debug for PeerConnection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerConnection")
            .field("status", &**self.status.get())
            .finish()
    }
}

#[derive(Debug)]
struct Negotiation {
    initiator: bool,
    ready: Notify,
    pending_candidates: Mutex<Vec<RTCIceCandidate>>,
    pc: Weak<RTCPeerConnection>,
}

impl Negotiation {
    async fn initiate(&self) -> Result<Option<RTCSessionDescription>, Error> {
        if let Some(pc) = self.pc.upgrade() {
            let offer = pc.create_offer(None).await?;
            pc.set_local_description(offer.clone()).await?;
            Ok(Some(offer))
        } else {
            Ok(None) // tbh. this shouldn never happen
        }
    }
}

impl Negotiation {
    fn new(pc: Weak<RTCPeerConnection>, initiator: bool) -> Self {
        Negotiation {
            pc,
            initiator,
            ready: Notify::new(),
            pending_candidates: Mutex::new(Vec::new()),
        }
    }
}

#[derive(Clone)]
pub struct Options {
    pub labels: HashMap<Arc<str>, Option<RTCDataChannelInit>>,
    pub rtc_config: RTCConfiguration,
}

impl Options {
    pub fn with_data_channels(labels: &[&str]) -> Self {
        let rtc_config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };
        Options {
            labels: labels
                .into_iter()
                .map(|&label| (Arc::from(label), None))
                .collect(),
            rtc_config,
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Options::with_data_channels(&[])
    }
}

async fn status_connected(status: &PeerConnectionState) -> Result<(), Error> {
    loop {
        let s = &**status.get();
        match s {
            InnerState::Waiting(ready) => ready.notified().await,
            InnerState::Negotiating(negotiation) => negotiation.ready.notified().await,
            InnerState::Ready => return Ok(()),
            InnerState::Closed(err) => {
                return if let Some(e) = err {
                    Err(e.clone())
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct PeerConnectionDataChannels(Mutex<UnboundedReceiver<DataChannel>>);

impl PeerConnectionDataChannels {
    fn new(receiver: UnboundedReceiver<DataChannel>) -> Self {
        PeerConnectionDataChannels(Mutex::new(receiver))
    }

    pub async fn next(&self) -> Option<DataChannel> {
        let mut guard = self.0.lock().await;
        guard.recv().await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Signal {
    Renegotiate,
    Candidate(RTCIceCandidate),
    Sdp(RTCSessionDescription),
}

#[repr(transparent)]
#[derive(Debug, Clone)]
struct PeerConnectionState(Arc<ArcSwap<InnerState>>);

impl PeerConnectionState {
    fn get(&self) -> Guard<Arc<InnerState>> {
        self.0.load()
    }

    fn weak_ref(&self) -> Weak<ArcSwap<InnerState>> {
        Arc::downgrade(&self.0)
    }

    fn upgrade(w: &Weak<ArcSwap<InnerState>>) -> Option<Self> {
        let arc = w.upgrade()?;
        Some(PeerConnectionState(arc))
    }

    fn set_ready(&self) -> Result<(), Error> {
        self.update(InnerState::ready())
    }

    fn set_closed(&self) -> Result<(), Error> {
        self.update(InnerState::closed_gracefully())
    }

    fn set_failed(&self, cause: Error) -> Result<(), Error> {
        self.update(InnerState::failed(cause))
    }

    fn set_negotiating(&self, negotiation: Negotiation) -> Result<(), Error> {
        self.update(InnerState::negotiating(negotiation))
    }

    fn update(&self, new_state: Arc<InnerState>) -> Result<(), Error> {
        let old = self.0.rcu(move |old| {
            if old.is_closed() {
                old.clone()
            } else {
                new_state.clone()
            }
        });
        match &*old {
            InnerState::Waiting(ready) => ready.notify_waiters(),
            InnerState::Negotiating(n) => n.ready.notify_waiters(),
            InnerState::Ready => {}
            InnerState::Closed(cause) => {
                if let Some(cause) = cause {
                    return Err(cause.clone());
                }
            }
        }
        Ok(())
    }
}

impl Default for PeerConnectionState {
    fn default() -> Self {
        PeerConnectionState(Arc::new(ArcSwap::new(InnerState::waiting())))
    }
}

#[derive(Debug)]
enum InnerState {
    Waiting(Notify),
    Negotiating(Negotiation),
    Ready,
    Closed(Option<Error>),
}

impl InnerState {
    fn waiting() -> Arc<Self> {
        Arc::new(InnerState::Waiting(Notify::new()))
    }

    fn ready() -> Arc<Self> {
        Arc::new(InnerState::Ready)
    }

    fn closed_gracefully() -> Arc<Self> {
        Arc::new(InnerState::Closed(None))
    }

    fn failed(e: Error) -> Arc<Self> {
        Arc::new(InnerState::Closed(Some(e)))
    }

    fn negotiating(negotiation: Negotiation) -> Arc<Self> {
        Arc::new(InnerState::Negotiating(negotiation))
    }

    fn is_closed(&self) -> bool {
        if let InnerState::Closed(_) = self {
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
    use std::sync::Arc;
    use tokio::spawn;
    use tokio::task::JoinHandle;

    fn exchange(
        from: Arc<PeerConnection>,
        to: Arc<PeerConnection>,
    ) -> JoinHandle<Result<(), Error>> {
        spawn(async move {
            while let Some(signal) = from.listen().await {
                to.signal(signal).await?;
            }
            Ok(())
        })
    }

    #[tokio::test]
    async fn connection_negotiation() -> Result<(), Error> {
        let options = Options::with_data_channels(&["dc"]);
        let p1 = Arc::new(PeerConnection::start(true, options.clone()).await?);
        let p2 = Arc::new(PeerConnection::start(false, options).await?);

        let _ = exchange(p1.clone(), p2.clone());
        let _ = exchange(p2.clone(), p1.clone());

        p1.connected().await?;
        p2.connected().await?;

        p1.close().await?;
        p2.close().await?;

        Ok(())
    }
}
