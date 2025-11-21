use thiserror::Error;

#[derive(Debug, Error)]
pub enum Udc2Error {
    /// Catch-all I/O error
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// TS connection error detected
    #[error("ts connection error: {0}")]
    TsConnection(#[source] std::io::Error),

    /// Error writing a frame to TS
    #[error("failed to write frame: {0}")]
    WriteFrame(#[source] std::io::Error),

    /// Error reading the frame length from TS
    #[error("failed to read frame length: {0}")]
    ReadFrameLen(#[source] std::io::Error),

    /// Error reading the frame payload from TS
    #[error("failed to read frame data: {0}")]
    ReadFrameData(#[source] std::io::Error),

    /// TS produced unexpected data while we were waiting for C2
    #[error("unexpected ts read while waiting for C2 frame")]
    UnexpectedTsRead,

    /// C2 channel closed while forwarding the response
    #[error("failed to forward response to C2 channel (receiver dropped)")]
    ForwardResponse,

    #[error("could not send frame to ts")]
    SendToTs,
}

pub type Result<T> = std::result::Result<T, Udc2Error>;
