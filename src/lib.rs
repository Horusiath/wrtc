//! `wrtc` is a crate which wraps a [webrtc](https://webrtc.rs) library into simpler, more friendly
//! API:
//! - It offers semi-automatic connection negotiation via signal exchange.
//! - It's build around async Rust.
//! - Its primitives implement Stream trait, making them composable and reducing the amount of
//!   closures and capturing that webrtc crate requires.
//!
//! Its design is partially inspired by NodeJS [SimplePeer](https://www.npmjs.com/package/simple-peer)
//! library and it offers the same message negotiation format: therefore it can be used to
//! communicate with WebRTC peers written in that library as well.
//!
//! Atm. this library is focused on Data Channels - other WebRTC primitives may be implemented in
//! the future.
//!
//! # Examples
//!
//! ```rust
//! use wrtc::{Options, Error, PeerConnection};
//! use std::sync::Arc;
//! use bytes::Bytes;
//! use futures_util::{SinkExt, StreamExt};
//! use tokio::task::JoinHandle;
//!
//! fn exchange(from: Arc<PeerConnection>, to: Arc<PeerConnection>) -> JoinHandle<Result<(), Error>> {
//!     // Create a task that will exchange signals between two peers
//!     // necessary to negotiate and establish connection. This part
//!     // is not covered by WebRTC and may require setting up a 3rd
//!     // party connection ie. via WebSockets.
//!     tokio::spawn(async move {
//!         while let Some(signal) = from.signal().await {
//!             to.apply_signal(signal).await?;
//!         }
//!         Ok(())
//!     })
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!     let options = Options::with_data_channels(&["test-dc"]);
//!     let initiator = Arc::new(PeerConnection::start(true, options.clone()).await?);
//!     let acceptor = Arc::new(PeerConnection::start(false, options).await?);
//!
//!     // setup tasks to negotiate connection between two peers
//!     let _ = exchange(initiator.clone(), acceptor.clone());
//!     let _ = exchange(acceptor.clone(), initiator.clone());
//!
//!     // wait for connection become established
//!     initiator.connected().await?;
//!     acceptor.connected().await?;
//!
//!     {
//!         // both connections have defined only one data channel ("test-dc")
//!         // so we know that the next incoming data channels belong to the same pair
//!         let mut dc1 = initiator.data_channels().next().await.unwrap();
//!         let mut dc2 = acceptor.data_channels().next().await.unwrap();
//!
//!         // wait for the data channels to be ready
//!         dc1.ready().await?;
//!         dc2.ready().await?;
//!
//!         assert_eq!(dc1.label(), "test-dc");
//!         assert_eq!(dc2.label(), "test-dc");
//!
//!         // send message from dc1 and receive it at dc2
//!         let data: Bytes = "hello".into();
//!         dc1.send(data.clone()).await?;
//!         let msg = dc2.next().await.unwrap()?;
//!         assert_eq!(msg.data, data);
//!     }
//!
//!     // gracefully close both peers
//!     initiator.close().await?;
//!     acceptor.close().await?;
//!     Ok(())
//! }
//! ```

pub mod data_channel;
pub mod error;
pub mod peer_connection;
pub use data_channel::DataChannel;
pub use error::Error;
pub use peer_connection::{Options, PeerConnection};
