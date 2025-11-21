use crate::error::{Result, Udc2Error};
use crate::udc2::spawn_udc2_proxy;
use bytes::Bytes;
use std::hash::Hash;
use std::{collections::HashMap, fmt::Display};
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    task::JoinHandle,
};
use tracing::{error, info, warn};

/// Configuration for the UDC2 listener used by the relay
///
/// This specifies the network address (host and port) of the teamserver
/// (UDC2 listener) that per-beacon proxies will connect to.
#[derive(Debug)]
pub struct Udc2ListenerConfig {
    pub host: String,
    pub port: u16,
}

/// Messages exchanged between the C2 frontend, the relay, and the teamserver
///
/// Generic parameter `Ctx` is the context/identifier type used to correlate
/// frames and control messages with a logical "beacon".
/// Defaults to `String`.
///
/// Variants:
/// - `Metadata` — initial metadata for a beacon; used to create a new proxy.
///                This is the first received frame from the beacon.
/// - `Frame` — a regular data frame that should be forwarded.
/// - `Exit` — indicates the beacon terminated.
#[derive(Debug)]
pub enum Message<Ctx = String> {
    Metadata { ctx: Ctx, frame: Bytes },
    Frame { ctx: Ctx, frame: Bytes },
    Exit { ctx: Ctx },
}

/// Local channels used by the C2 side to communicate with the relay
///
/// - `tx` is used to send the messages from the teamserver to the C2 channel
/// - `rx` is used to receive messages from the C2 channel
///
/// `Ctx` is the context/identifier type used to correlate messages with
/// specific beacons.
pub struct C2<Ctx = String> {
    pub tx: Sender<Message<Ctx>>,
    pub rx: Receiver<Message<Ctx>>,
}

/// Main loop that orchestrates per-beacon UDC2 proxy connections.
///
/// The function listens for `Message<Ctx>` values on `c2.rx`. On receiving:
/// - `Message::Metadata`, it spawns a UDC2 proxy that connects
///    to the configured `listener`.
/// - `Message::Frame` is forwarded to the corresponding proxy
/// - `Message::Exit` is forwarded to the proxy to trigger graceful shutdown.
///
/// Returns `Ok(())` when the `c2.rx` channel is closed and the loop terminates.
/// Errors propagate as `Udc2Error` when sending to spawning/forwarding channels
/// fails (e.g. `Udc2Error::SendToTs`).
pub async fn clue<Ctx>(mut c2: C2<Ctx>, listener: Udc2ListenerConfig) -> Result<()>
where
    Ctx: Send + Sync + Clone + Eq + Hash + Display + 'static,
{
    let mut conns: HashMap<Ctx, (JoinHandle<()>, Sender<Message<Ctx>>)> = HashMap::new();
    while let Some(message) = c2.rx.recv().await {
        match message {
            Message::Metadata { ctx, frame } => {
                info!(%ctx, "received metadata from C2");
                match conns.get(&ctx) {
                    Some(_) => {
                        warn!(%ctx, "beacon already exists")
                    }
                    None => {
                        info!(%ctx, "new beacon");
                        let (ts_in_tx, ts_in_rx) = mpsc::channel(16);
                        match spawn_udc2_proxy(
                            c2.tx.clone(),
                            ts_in_rx,
                            &listener.host,
                            listener.port,
                        )
                        .await
                        {
                            Err(err) => error!(%err, "could not connnect to teamserver"),
                            Ok(handle) => {
                                ts_in_tx
                                    .send(Message::Frame {
                                        ctx: ctx.clone(),
                                        frame,
                                    })
                                    .await
                                    .map_err(|_| Udc2Error::SendToTs)?;
                                conns.entry(ctx).insert_entry((handle, ts_in_tx));
                            }
                        }
                    }
                }
            }
            Message::Frame { ctx, frame } => {
                info!(%ctx, "received frame from C2");
                match conns.get(&ctx) {
                    Some((h, ts_in_tx)) => {
                        if !(h.is_finished() || ts_in_tx.is_closed()) {
                            info!(%ctx, "forwarding frame to ts");
                            ts_in_tx
                                .send(Message::Frame { ctx, frame })
                                .await
                                .map_err(|_| Udc2Error::SendToTs)?;
                        } else {
                            warn!(%ctx, "ts connection is closed")
                        }
                    }
                    None => {
                        error!(%ctx, "beacon doesn't exist")
                    }
                }
            }
            Message::Exit { ctx } => match conns.get(&ctx) {
                Some((h, ts_in_tx)) => {
                    if !(h.is_finished() || ts_in_tx.is_closed()) {
                        info!(%ctx, "forwarding exit frame to ts");
                        ts_in_tx
                            .send(Message::Exit { ctx })
                            .await
                            .map_err(|_| Udc2Error::SendToTs)?;
                    } else {
                        warn!(%ctx, "ts connection is closed")
                    }
                }
                None => warn!(%ctx, "could not find the ts connection"),
            },
        };
        // Prune closed TS connections
        conns.retain(|_, (h, c)| !(h.is_finished() || c.is_closed()));
    }
    Ok(())
}
