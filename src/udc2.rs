use crate::error::Result;
use crate::{clue::Message, error::Udc2Error};
use bytes::{BufMut, Bytes, BytesMut};
use std::future::Future;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::{Receiver, Sender},
    task::JoinHandle,
};
use tracing::{debug, error, info};

/// Connect to the TeamServer (UDC2 listener) and spawn a proxy task.
///
/// The returned `JoinHandle` runs the proxy future that forwards frames
/// between the provided `c2_in_rx` (incoming messages from C2) and the
/// `ts_in_tx` sender used to forward responses back to the C2 side.
///
/// Parameters:
/// - `ts_in_tx`: sender used to forward responses back to the C2 loop.
/// - `c2_in_rx`: receiver from which the proxy reads frames to send to TS.
/// - `host`, `port`: network address of the TeamServer to connect to.
///
/// Returns a `JoinHandle<()>` for the spawned task on success.
pub async fn spawn_udc2_proxy<C>(
    ts_in_tx: Sender<Message<C>>,
    c2_in_rx: Receiver<Message<C>>,
    host: &str,
    port: u16,
) -> Result<JoinHandle<()>>
where
    C: Send + Sync + 'static,
{
    let ts_socket = TcpStream::connect((host, port)).await?;
    ts_socket.set_nodelay(true)?;

    let proxy = new_udc2_proxy(ts_socket, ts_in_tx, c2_in_rx);
    let handle = tokio::spawn(proxy);
    Ok(handle)
}

/// Create a proxy future that forwards frames between TS and C2.
///
/// The returned future:
/// - Logs a new connection,
/// - Sends an initial "go" frame to the TeamServer,
/// - Repeatedly performs one round-trip (C2 -> TS -> C2) via `proxy_once`
///   until the input channel closes or an error occurs,
/// - Logs shutdown when finished.
///
/// The future is `Send` and `'static` so it can be spawned onto the Tokio runtime.
pub fn new_udc2_proxy<S, C>(
    mut ts: S,
    ts_in_tx: Sender<Message<C>>,
    mut c2_in_rx: Receiver<Message<C>>,
) -> impl Future<Output = ()> + Send + 'static
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Send + Sync + 'static,
{
    async move {
        info!("new udc2 listener connection established");
        write_frame(&mut ts, b"go").await;

        loop {
            match proxy_once(&mut ts, &ts_in_tx, &mut c2_in_rx).await {
                Ok(true) => continue,
                Ok(false) => {
                    debug!("input channel closed; stopping proxy task");
                    break;
                }
                Err(err) => {
                    error!(%err, "proxy round-trip failed");
                    break;
                }
            }
        }
        info!("closing udc2 listener connection");
    }
}

/// Perform a single synchronous Beacon -> TeamServer -> Beacon round-trip.
///
/// Behavior:
/// 1. Await a `Message` from `c2_in_rx`. If the channel is closed, return Ok(false).
///    If the TeamServer signals readability (unexpected read), return an error.
/// 2. Forward the received frame bytes as-is to the TeamServer.
/// 3. Read a response frame from the TeamServer using `read_frame`.
/// 4. Forward the response back to the C2 side via `ts_in_tx`.
///
/// Return values:
/// - `Ok(true)`  : round-trip succeeded; caller should continue looping.
/// - `Ok(false)` : input channel closed or the proxy should stop; caller should stop.
/// - `Err(e)`    : I/O or framing error occurred; caller should stop.
async fn proxy_once<S, C>(
    ts: &mut S,
    ts_in_tx: &Sender<Message<C>>,
    c2_in_rx: &mut Receiver<Message<C>>,
) -> Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: Send + Sync + 'static,
{
    // 1) Get a frame from the C2 channel
    let msg = tokio::select! {
        msg = c2_in_rx.recv() => match msg {
            Some(m) => m,
            None => return Ok(false),
        },
        // Dummy read from the TS to detect connection close, etc
        rd = ts.read_u8() => match rd {
            Err(err) => return Err(Udc2Error::TsConnection(err)),
            Ok(_) => return Err(Udc2Error::UnexpectedTsRead),
        },
        _ = ts_in_tx.closed() => return Ok(false),
    };

    let (id, frame) = match msg {
        Message::Metadata { ctx, frame } => (ctx, frame),
        Message::Frame { ctx, frame } => (ctx, frame),
        Message::Exit { ctx: _ } => return Ok(false),
    };

    // 2) Forward the frame as-is to TS
    ts.write_all(frame.as_ref())
        .await
        .map_err(Udc2Error::WriteFrame)?;
    ts.flush().await.ok();

    // 3) Read a response frame from TS
    let response = tokio::select! {
        frame = read_frame(ts) => frame?,
        _ = ts_in_tx.closed() => return Ok(false),
    };

    // 4) Forward to the C2 channel
    ts_in_tx
        .send(Message::Frame {
            ctx: id,
            frame: response,
        })
        .await
        .map_err(|_| Udc2Error::ForwardResponse)?;

    Ok(true)
}

/// Write a framed message to `w`.
///
/// Frame format:
/// - 4-byte little-endian length prefix (u32)
/// - payload bytes
async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) {
    let len = data.len() as u32;
    w.write_all(&len.to_le_bytes()).await.unwrap();
    w.write_all(data).await.unwrap();
    w.flush().await.unwrap();
}

/// Read a framed message from `r` and return it as `Bytes`.
///
/// Expects a 4-byte little-endian length followed by that many payload bytes.
/// The returned `Bytes` contains the length prefix followed by payload (matching
/// the framing used by `write_frame`).
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Bytes> {
    let resp_len = r.read_u32_le().await.map_err(Udc2Error::ReadFrameLen)?;
    let mut resp = BytesMut::with_capacity(4 + resp_len as usize);
    resp.put_u32_le(resp_len);
    resp.resize(4 + resp_len as usize, 0);
    r.read_exact(&mut resp[4..])
        .await
        .map_err(Udc2Error::ReadFrameData)?;
    Ok(resp.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{BufMut, BytesMut};
    use tokio::io::duplex;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn proxy_round_trip_simple() {
        tracing_subscriber::fmt::init();
        let (client_end, mut server_end) = duplex(64 * 1024);

        let (c2_tx, mut ts_rx) = mpsc::channel::<Message>(4);
        let (ts_tx, c2_rx) = mpsc::channel::<Message>(4);
        let proxy = new_udc2_proxy(client_end, ts_tx, c2_rx);
        tokio::spawn(proxy);

        // "echo" TeamServer
        tokio::spawn(async move {
            let mut first_message = true;
            loop {
                let frame = read_frame(&mut server_end).await.expect("expected a frame");
                let data = &frame[4..];
                if first_message {
                    assert_eq!(data, b"go");
                    first_message = false;
                } else {
                    write_frame(&mut server_end, data).await;
                }
            }
        });

        let payload = b"ping";
        let mut frame = BytesMut::with_capacity(4 + payload.len());
        frame.put_u32_le(payload.len() as u32);
        frame.extend_from_slice(payload);

        c2_tx
            .send(Message::Frame {
                ctx: "req-1".into(),
                frame: frame.freeze(),
            })
            .await
            .unwrap();

        let (ctx, frame) = match ts_rx.recv().await.expect("response expected") {
            Message::Frame { ctx, frame } => (ctx, frame),
            _ => unreachable!(),
        };
        assert_eq!(ctx, "req-1");
        assert_eq!(&frame[4..], b"ping");
    }
}
