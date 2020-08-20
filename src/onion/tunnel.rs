use crate::onion::circuit::Circuit;
use crate::onion::crypto::{self, EphemeralPrivateKey, SessionKey};
use crate::onion::protocol::{
    CircuitOpaque, CircuitOpaqueBytes, TryFromBytesExt, TunnelRequest, VerifyKey,
};
use crate::onion::socket::{OnionSocket, OnionSocketError, SocketResult};
use crate::Result;
use crate::{Event, Peer};
use anyhow::{anyhow, Context};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use log::trace;
use log::warn;
use ring::rand;
use ring::rand::SecureRandom;
use std::convert::TryInto;
use std::mem;
use std::ops::Deref;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};

const MAX_PEER_FAILURES: usize = 10;

pub type TunnelId = u32;

#[derive(Error, Debug)]
pub(crate) enum TunnelError {
    /// The requested operation could not be run to completion, but the tunnel has a consistent
    /// state that can be expanded on
    #[error("Tunnel operation could not be completed")]
    Incomplete,
    /// The requested operation could not be completed and the tunnel is left in a broken state
    /// that needs to be cleaned up. This may be triggered by an undecryptable `OPAQUE` message,
    /// or a `TEARDOWN` message from the first hop.
    #[error("Tunnel operation caused the tunnel to break")]
    Broken(Option<OnionSocketError>),
}

impl From<OnionSocketError> for TunnelError {
    fn from(e: OnionSocketError) -> Self {
        match e {
            OnionSocketError::Peer => TunnelError::Incomplete,
            e => TunnelError::Broken(Some(e)),
        }
    }
}

pub(crate) type TunnelResult<T> = std::result::Result<T, TunnelError>;

pub(crate) enum Request {
    Data { data: Bytes },
    Switchover,
    Destroy,
}

/// Represents the tunnel controller view of a tunnel.
/// Manages the first circuit and stores all session keys in encryption order.
pub(crate) struct Tunnel {
    pub(crate) id: TunnelId,
    out_circuit: Circuit,
    session_keys: Vec<SessionKey>,
}

impl Tunnel {
    /// Performs a circuit handshake with the first hop (peer).
    pub(crate) async fn init(id: TunnelId, peer: &Peer, rng: &rand::SystemRandom) -> Result<Self> {
        trace!("Creating tunnel {} to peer {}", id, &peer.addr);
        let (private_key, key) = crypto::generate_ephemeral_keypair(rng);

        let circuit_id = Circuit::random_id(rng);
        let stream = TcpStream::connect(peer.addr)
            .await
            .context("Could not connect to peer")?;
        let mut socket = OnionSocket::new(stream);
        let peer_key = socket.initiate_handshake(circuit_id, key, rng).await?;

        let secret = Tunnel::derive_secret(&peer, private_key, peer_key)?;
        Ok(Self {
            id,
            out_circuit: Circuit::new(circuit_id, socket),
            session_keys: vec![secret],
        })
    }

    fn derive_secret(
        peer: &&Peer,
        private_key: EphemeralPrivateKey,
        peer_key: VerifyKey,
    ) -> Result<SessionKey> {
        let peer_key = peer_key
            .verify(&peer.hostkey)
            .context("Could not verify peer public key")?;
        let secret = SessionKey::from_key_exchange(private_key, &peer_key)?;
        Ok(secret)
    }

    /// Returns the length of a tunnel. The result of this function may be used with caution if the
    /// tunnel is in a broken state.
    pub(crate) fn len(&self) -> usize {
        self.session_keys.len()
    }

    /// Performs a key exchange with the given peer and extends the tunnel with a new hop
    pub(crate) async fn extend(
        &mut self,
        peer: &Peer,
        rng: &rand::SystemRandom,
    ) -> TunnelResult<()> {
        trace!("Extending tunnel {} to peer {}", self.id, &peer.addr);
        let (private_key, key) = crypto::generate_ephemeral_keypair(rng);

        let peer_key = self
            .out_circuit
            .socket()
            .await
            .initiate_tunnel_handshake(self.out_circuit.id, peer.addr, key, &self.session_keys, rng)
            .await?;

        // Any failure because of any incorrect secret answer should not cause our tunnel to become corrupted
        if let Ok(secret) = Tunnel::derive_secret(&peer, private_key, peer_key) {
            self.session_keys.insert(0, secret);
            Ok(())
        } else {
            // key derivation failed, the final hop needs to be truncated
            // if the truncate fails too, the tunnel is broken
            self.truncate(0, rng)
                .await
                .map_err(|_| TunnelError::Broken(None))?;
            Err(TunnelError::Incomplete)
        }
    }

    /// Truncates the tunnel by `n` hops with one `TUNNEL TRUNCATE` message. If message returns with
    /// an error code, `Incomplete` will be returned.
    ///
    /// Returns `Incomplete` if the resulting hop count would be less than one.
    pub(crate) async fn truncate(
        &mut self,
        n: usize,
        rng: &rand::SystemRandom,
    ) -> TunnelResult<()> {
        if n >= self.session_keys.len() {
            return Err(TunnelError::Incomplete);
        }

        self.out_circuit
            .socket()
            .await
            .truncate_tunnel(self.out_circuit.id, &self.session_keys[n..], rng)
            .await?;

        for _ in 0..n {
            &self.session_keys.remove(0);
        }
        Ok(())
    }

    /// Begins a data connection with the last hop in the tunnel
    ///
    /// If there is already a tunnel connected to the same peer with the same `TunnelId`, the
    /// targeted peer should no longer use the old tunnel for communication and is expected to send a
    /// `TUNNEL END` message through the old tunnel. This works like an implicit `TUNNEL END` to the
    /// remote tunnel.
    ///
    /// After sending a `TUNNEL BEGIN` message to the other endpoint of a tunnel, the tunnel should
    /// be monitored for `TUNNEL DATA` messages, as long as no explicit `TUNNEL END` message is
    /// received, or any explicit or implicit `TUNNEL END` message is sent. After sending an
    /// implicit `TUNNEL END` message by calling this function (as mentioned above), the old tunnel
    /// should be monitored for any incoming `TUNNEL DATA` packets and a final `TUNNEL END` packet
    /// before tearing down the old tunnel. Be aware that the other endpoint peer should not be
    /// allowed to use the old tunnel indefinitely despite receiving a `TUNNEL END` packet. Any old
    /// tunnel that has been replaced should only have finite lifetime.
    pub(crate) async fn begin(&mut self, rng: &rand::SystemRandom) -> TunnelResult<()> {
        self.out_circuit
            .socket()
            .await
            .begin(self.out_circuit.id, self.id, &self.session_keys, rng)
            .await?;
        Ok(())
    }

    pub(crate) async fn truncate_to_length(
        &mut self,
        n_hops: usize,
        rng: &rand::SystemRandom,
    ) -> TunnelResult<()> {
        let mut num_fails = 0;

        while self.session_keys.len() > n_hops + 1 {
            match self.truncate(1, rng).await {
                Err(TunnelError::Broken(e)) => {
                    // do not try to fix this error to prevent endless looping
                    return Err(TunnelError::Broken(e));
                }
                Err(TunnelError::Incomplete) => {
                    num_fails += 1;
                    if num_fails >= MAX_PEER_FAILURES {
                        return Err(TunnelError::Incomplete);
                    }
                }
                Ok(_) => {}
            }
        }

        Ok(())
    }

    pub(crate) fn random_id(rng: &rand::SystemRandom) -> TunnelId {
        // FIXME an attacker may fill up all ids
        let mut id_buf = [0u8; 4];
        rng.fill(&mut id_buf).unwrap();
        u32::from_le_bytes(id_buf)
    }

    async fn unbuild(&self, rng: &rand::SystemRandom) {
        // TODO graceful deconstruction
        self.teardown(rng).await;
    }

    async fn teardown(&self, rng: &rand::SystemRandom) {
        self.out_circuit.teardown_with_timeout(rng).await;
    }
}

#[derive(Clone)]
pub(crate) struct TunnelBuilder {
    tunnel_id: TunnelId,
    dest: Peer,
    n_hops: usize,
    peer_provider: mpsc::Sender<oneshot::Sender<Peer>>,
    rng: rand::SystemRandom,
}

impl TunnelBuilder {
    pub(crate) fn new(
        tunnel_id: TunnelId,
        dest: Peer,
        n_hops: usize,
        peer_provider: mpsc::Sender<oneshot::Sender<Peer>>,
        rng: rand::SystemRandom,
    ) -> Self {
        TunnelBuilder {
            tunnel_id,
            dest,
            n_hops,
            peer_provider,
            rng,
        }
    }

    /// Tries to extend this tunnel to intermediate hop count `n_hops` and final hop `final_peer`.
    ///
    /// The peers provided by `peer_provider` will be used as a source for the intermediate hops,
    /// the final hop at index `n` will be `final_peer`.
    ///
    /// This function does not check whether the peers provided by `peer_provider` are particularity
    /// secure. In order to preserve anonymity, there should never be two consecutive hops to the
    /// same peer. Also, `peer_provider` should produce peers in a way that potentially malicious
    /// peers with shared knowledge of circuits should be returned with a low probability (or with
    /// equal probability to any other peer) to prevent the tunnel from becoming compromised.
    ///
    /// Even if there is a high failure-rate among peers, the `peer_provider` should be able to
    /// generate a secure stream of peers.
    pub(crate) async fn build(&mut self) -> Result<Tunnel> {
        let mut tunnel = None;
        for i in 0..MAX_PEER_FAILURES {
            tunnel = match tunnel.take() {
                None if self.n_hops == 0 => Tunnel::init(self.tunnel_id, &self.dest, &self.rng)
                    .await
                    .ok(),
                None => {
                    let peer = self
                        .random_peer()
                        .await
                        .context(anyhow!("Failed to get random peer"))?;
                    Tunnel::init(self.tunnel_id, &peer, &self.rng).await.ok()
                }
                Some(mut tunnel) if tunnel.len() - 1 < self.n_hops => {
                    let peer = self
                        .random_peer()
                        .await
                        .context(anyhow!("Failed to get random peer"))?;

                    match tunnel.extend(&peer, &self.rng).await {
                        Err(TunnelError::Broken(e)) => {
                            tunnel.teardown(&self.rng).await;
                            None
                        }
                        Err(TunnelError::Incomplete) => Some(tunnel),
                        Ok(_) => Some(tunnel),
                    }
                }
                Some(tunnel) => return Ok(tunnel),
            }
        }
        Err(anyhow!("failed to build tunnel"))
    }

    async fn random_peer(&mut self) -> Result<Peer> {
        let (peer_tx, peer_rx) = oneshot::channel();
        let _ = self.peer_provider.send(peer_tx).await;
        Ok(peer_rx.await?)
    }
}

/// Manages a tunnel after its creation.
/// Associates a requests channel with a concrete tunnel (enabling switch-over??)
pub(crate) struct TunnelHandler {
    tunnel: Tunnel,
    next_tunnel: Arc<Mutex<Option<Tunnel>>>,
    state: State,
    requests: mpsc::UnboundedReceiver<Request>,
    events: mpsc::Sender<Event>,
    builder: TunnelBuilder,
}

#[derive(Copy, Clone)]
pub(crate) enum State {
    Building,
    Ready,
    Destroying,
    Destroyed,
}

impl TunnelHandler {
    pub(crate) fn new(
        first_tunnel: Tunnel,
        tunnel_builder: TunnelBuilder,
        requests: mpsc::UnboundedReceiver<Request>,
        events: mpsc::Sender<Event>,
    ) -> Self {
        TunnelHandler {
            tunnel: first_tunnel,
            next_tunnel: Arc::new(Mutex::new(None)),
            state: State::Building,
            requests,
            events,
            builder: tunnel_builder,
        }
    }

    pub(crate) async fn handle(&mut self) -> Result<()> {
        loop {
            match self.state {
                State::Building => {
                    tokio::select! {
                        Some(req) = self.requests.recv() => {
                            self.handle_request(req).await?;
                        }
                    }
                }
                State::Ready | State::Destroying => {
                    tokio::select! {
                        msg = self.tunnel.out_circuit.accept_opaque() => {
                            self.handle_tunnel_message(msg).await?;
                        }
                        Some(req) = self.requests.recv() => {
                            self.handle_request(req).await?;
                        }
                    }
                }
                State::Destroyed => return Ok(()),
            }
        }
        self.tunnel.teardown(&self.builder.rng).await;
        // TODO cleanup next tunnel
        Ok(())
    }

    async fn handle_tunnel_message(
        &mut self,
        msg: SocketResult<CircuitOpaque<CircuitOpaqueBytes>>,
    ) -> Result<()> {
        // TODO apply timeout to handle tunnel rotation
        // TODO send event in case of error
        let mut msg = msg?;
        // TODO send event in case of error
        msg.decrypt(self.tunnel.session_keys.iter().rev())?;
        let tunnel_msg = TunnelRequest::read_with_digest_from(&mut msg.payload.bytes);
        match tunnel_msg {
            Ok(TunnelRequest::Data(tunnel_id, data)) => {
                let event = Event::Data { tunnel_id, data };
                // TODO send event in case of error
                self.events.send(event).await?
            }
            Ok(TunnelRequest::End(tunnel_id)) => {
                // TODO send event and deconstruct tunnel
                todo!()
            }
            _ => {
                // invalid request or broken digest
                // TODO teardown tunnel
                todo!()
            }
        }
        Ok(())
    }

    async fn handle_request(&mut self, req: Request) -> Result<()> {
        match (req, self.state) {
            (Request::Data { data }, State::Ready) => {
                let circuit_id = self.tunnel.out_circuit.id;
                let tunnel_id = self.tunnel.id;
                self.tunnel
                    .out_circuit
                    .socket()
                    .await
                    .send_data(
                        circuit_id,
                        tunnel_id,
                        data,
                        &self.tunnel.session_keys,
                        &self.builder.rng,
                    )
                    .await?;
            }
            (Request::Switchover, State::Building) => {
                self.state = State::Ready;
                self.events
                    .send(Event::Ready {
                        tunnel_id: self.tunnel.id,
                    })
                    .await?;

                self.spawn_next_tunnel_task();
            }
            (Request::Switchover, State::Ready) => {
                let mut new_tunnel = self
                    .next_tunnel
                    .lock()
                    .await
                    .take()
                    .ok_or(anyhow!("Switchover failed: no next tunnel"))?;

                mem::swap(&mut self.tunnel, &mut new_tunnel);
                let old_tunnel = new_tunnel;
                self.tunnel.begin(&self.builder.rng).await?;
                // TODO send end on old_tunnel

                self.spawn_next_tunnel_task();
                tokio::spawn({
                    let rng = self.builder.rng.clone();
                    async move {
                        old_tunnel.unbuild(&rng).await;
                    }
                });
            }
            (Request::Switchover, State::Destroying) => {
                self.state = State::Destroyed;

                self.tunnel.unbuild(&self.builder.rng).await;
                if let Some(next_tunnel) = self.next_tunnel.lock().await.deref() {
                    next_tunnel.unbuild(&self.builder.rng).await;
                }
            }
            (Request::Destroy, State::Ready) => self.state = State::Destroying,
            _ => return Err(anyhow!("Illegal TunnelHandler state")),
        }
        Ok(())
    }

    fn spawn_next_tunnel_task(&self) {
        tokio::spawn({
            let next_tunnel = self.next_tunnel.clone();
            let mut builder = self.builder.clone();
            async move {
                let new_tunnel = builder.build().await.unwrap();
                next_tunnel.lock().await.replace(new_tunnel);
            }
        });
    }
}
