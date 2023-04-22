use crate::data_channel::DataChannel;
use crate::error::Error;
use arc_swap::{ArcSwap, Guard};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Formatter;
use std::sync::{Arc, Weak};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, Notify};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::{APIBuilder, API};
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
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
    /// messages coming from [PeerConnection::signal] on one peer and apply them on another via
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

    /// Check if current [PeerConnection] is the initiator of the connection.
    pub fn is_initiator(&self) -> bool {
        self.initiator
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

    /// This method allows to await until the peer connection is closed without requesting to close
    /// it.
    pub async fn closed(&self) -> Result<(), Error> {
        loop {
            let s = &**self.status.get();
            match s {
                InnerState::Waiting(ready) => ready.notified().await,
                InnerState::Negotiating(negotiation) => negotiation.ready.notified().await,
                InnerState::Ready(n) => n.notified().await,
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

    /// Listen to the next [Signal] message coming out of the current [PeerConnection]. This signal
    /// should be serialized, passed over to its remote counterpart and applied there using
    /// [PeerConnection::apply_signal].
    pub async fn signal(&self) -> Option<Signal> {
        if self.status.get().is_closed() {
            None
        } else {
            let mut signals = self.signal_receiver.lock().await;
            signals.recv().await
        }
    }

    /// Apply [Signal]s received from the remote [PeerConnection].
    pub async fn apply_signal(&self, signal: Signal) -> Result<(), Error> {
        match signal {
            Signal::Renegotiate(renegotiate) => {
                if renegotiate {
                    if !self.status.get().is_closed() && self.initiator {
                        let negotiation =
                            Negotiation::new(Arc::downgrade(&self.pc), self.initiator);
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
            InnerState::Ready(_) => return Ok(()),
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
    #[serde(rename = "renegotiate")]
    Renegotiate(bool),
    #[serde(rename = "candidate")]
    Candidate(RTCIceCandidate),
    #[serde(rename = "sdp")]
    Sdp(RTCSessionDescription),
}

impl PartialEq for Signal {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Signal::Renegotiate(x), Signal::Renegotiate(y)) => x == y,
            (Signal::Candidate(c1), Signal::Candidate(c2)) => c1 == c2,
            (Signal::Sdp(s1), Signal::Sdp(s2)) => s1.sdp_type == s2.sdp_type && s1.sdp == s2.sdp,
            (_, _) => false,
        }
    }
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
            InnerState::Ready(n) => n.notify_waiters(),
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

impl std::fmt::Display for PeerConnectionState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &**self.0.load() {
            InnerState::Waiting(_) => write!(f, "waiting"),
            InnerState::Negotiating(_) => write!(f, "negotiating"),
            InnerState::Ready(_) => write!(f, "ready"),
            InnerState::Closed(_) => write!(f, "closed"),
        }
    }
}

#[derive(Debug)]
enum InnerState {
    Waiting(Notify),
    Negotiating(Negotiation),
    Ready(Notify),
    Closed(Option<Error>),
}

impl InnerState {
    fn waiting() -> Arc<Self> {
        Arc::new(InnerState::Waiting(Notify::new()))
    }

    fn ready() -> Arc<Self> {
        Arc::new(InnerState::Ready(Notify::new()))
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
    use crate::peer_connection::{Options, PeerConnection, Signal};
    use futures_util::TryFutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::spawn;
    use tokio::task::JoinHandle;
    use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
    use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
    use webrtc::ice_transport::ice_protocol::RTCIceProtocol;
    use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

    fn exchange(
        from: Arc<PeerConnection>,
        to: Arc<PeerConnection>,
    ) -> JoinHandle<Result<(), Error>> {
        spawn(async move {
            while let Some(signal) = from.signal().await {
                to.apply_signal(signal).await?;
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

    #[tokio::test]
    async fn connection_closed() -> Result<(), Error> {
        let options = Options::with_data_channels(&["dc"]);
        let p1 = Arc::new(PeerConnection::start(true, options.clone()).await?);
        let p2 = Arc::new(PeerConnection::start(false, options).await?);

        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let f1 = p1.closed().and_then(move |_| async move {
            c.fetch_add(1, Ordering::AcqRel);
            Ok(())
        });
        let c = counter.clone();
        let f2 = p1.closed().and_then(move |_| async move {
            c.fetch_add(1, Ordering::AcqRel);
            Ok(())
        });
        let c = counter.clone();
        let f3 = p2.closed().and_then(move |_| async move {
            c.fetch_add(1, Ordering::AcqRel);
            Ok(())
        });
        let c = counter.clone();
        let f4 = p2.closed().and_then(move |_| async move {
            c.fetch_add(1, Ordering::AcqRel);
            Ok(())
        });

        let _ = exchange(p1.clone(), p2.clone());
        let _ = exchange(p2.clone(), p1.clone());

        p1.connected().await?;
        p2.connected().await?;

        p1.close().await?;
        p2.close().await?;

        f1.await?;
        f2.await?;
        f3.await?;
        f4.await?;

        assert_eq!(counter.load(Ordering::Relaxed), 4);

        Ok(())
    }

    #[test]
    fn signal_serialization() {
        let candidate = RTCIceCandidate {
            stats_id: "candidate:UcZ4TWueGMPo2CVb89j0JmbGbeEgpDyK".to_string(),
            foundation: "3299231860".to_string(),
            priority: 2130706431,
            address: "192.168.56.1".to_string(),
            protocol: RTCIceProtocol::Udp,
            port: 53545,
            typ: RTCIceCandidateType::Host,
            component: 1,
            related_address: "".to_string(),
            related_port: 0,
            tcp_type: "unspecified".to_string(),
        };
        let sdp = r#"v=0
o=- 2043488436270048026 560464900 IN IP4 0.0.0.0
s=-
t=0 0
a=fingerprint:sha-256 D8:53:60:55:F5:04:35:D0:30:3A:8A:DC:2B:26:D6:EF:F5:09:67:0B:0E:B3:C5:CA:B0:85:2E:9C:FC:25:57:45
a=group:BUNDLE 0
m=application 9 UDP/DTLS/SCTP webrtc-datachannel
c=IN IP4 0.0.0.0
a=setup:actpass
a=mid:0
a=sendrecv
a=sctp-port:5000
a=ice-ufrag:aeqnRWKqXnAJjFbZ
a=ice-pwd:cqzvayDwlWVFBfsBYhDgOiPjsnFnSZMC
"#;
        let signals = vec![
            (
                Signal::Sdp(RTCSessionDescription::offer(sdp.to_string()).unwrap()),
                format!(
                    r#"{{"sdp":{{"type":"offer","sdp":{}}}}}"#,
                    serde_json::to_string(sdp).unwrap()
                ),
            ),
            (
                Signal::Candidate(candidate),
                r#"{"candidate":{"stats_id":"candidate:UcZ4TWueGMPo2CVb89j0JmbGbeEgpDyK","foundation":"3299231860","priority":2130706431,"address":"192.168.56.1","protocol":"udp","port":53545,"typ":"host","component":1,"related_address":"","related_port":0,"tcp_type":"unspecified"}}"#.to_string(),
            ),
            (Signal::Renegotiate(true), r#"{"renegotiate":true}"#.to_string()),
        ];
        for (signal, expected) in signals {
            let json = serde_json::to_string(&signal).unwrap();
            assert_eq!(json, expected);

            let actual: Signal = serde_json::from_str(&json).unwrap();
            assert_eq!(actual, signal);
        }
    }
}
