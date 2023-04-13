use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Error(Arc<webrtc::Error>);

impl Error {
    pub fn channel_closed() -> Self {
        Error(Arc::new(webrtc::Error::ErrDataChannelNotOpen))
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl From<webrtc::Error> for Error {
    fn from(value: webrtc::Error) -> Self {
        Error(Arc::new(value))
    }
}