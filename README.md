# wrtc

This is a Rust library wrapper around popular [webrtc](https://webrtc.rs/), that's focused on a developer experience.
Message exchange protocol is compatible with NPM [simple-peer](https://github.com/feross/simple-peer) library and tries its best to conform to idiomatic Rust structures eg. futures `Sink`/`Stream` instead of callbacks. 


## API coverage

Current state only covers data channels.

- [x] WebRTC `PeerConnection`
- [x] data channels
- [ ] streams
- [ ] transceivers

## Example

```rust
#[tokio::main]
async fn main() -> Result<(), Error> {
    // we use default options with "test" data channel to be initialized
    let options = Options::with_channels(&["test"]);
    
    // setup first peer as a connection initiator
    let p1 = Arc::new(PeerConnection::start(true, options.clone()).await?);
    // setup second peer as a connection acceptor
    let p2 = Arc::new(PeerConnection::start(false, options).await?);
    
    // WebRTC requires offer/answer roundtrip between peers to be send
    // outside of the WebRTC protocol itself. It's usually done via another
    // medium (HTTP, Web Sockets etc.). Here we just send them straight 
    // over the memory.
    let _ = exchange(p1.clone(), p2.clone());
    let _ = exchange(p2.clone(), p1.clone());
    
    // wait for connection negotiation to complete
    p1.connected().await?;
    p2.connected().await?;
    
    {
        // Get data channel references on both ends. In `wrtc`,
        // `DataChannel` implements both futures Sink and Stream.
        let mut dc1 = p1.data_channels().next().await.unwrap();
        let mut dc2 = p2.data_channels().next().await.unwrap();
        
        // make sure that channels have been established
        dc1.ready().await?;
        dc2.ready().await?;
        
        // send message from peer 1 to peer 2
        let data: Bytes = "hello".into();
        dc1.send(data.clone()).await?;
        let msg = dc2.next().await.unwrap()?;
        
        assert_eq!(msg.data, data);
    }
    
    // gracefully close peers
    p1.close().await?;
    p2.close().await?;
    Ok(())
}

fn exchange(
    from: Arc<PeerConnection>,
    to: Arc<PeerConnection>,
) -> JoinHandle<Result<(), Error>> {
    tokio::spawn(async move {
        while let Some(signal) = from.listen().await {
            to.signal(signal).await?;
        }
        Ok(())
    })
}
```