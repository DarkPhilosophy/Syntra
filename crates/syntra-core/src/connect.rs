use crate::{client::ClientManager, config::local_commit, crypto};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use syntra_api::{ClientHandle, DEFAULT_PORT};
use syntra_proto::{MAX_DATAGRAM_SIZE, ProtoEvent};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_local},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum SyntraConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Protocol(#[from] syntra_proto::ProtocolError),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
    #[error("peer did not present a certificate")]
    MissingPeerCertificate,
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
async fn connect(
    addr: SocketAddr,
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Sync + Send>, SocketAddr), (SocketAddr, SyntraConnectionError)> {
    log::info!("connecting to {addr} ...");
    let conn = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, SyntraConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), SyntraConnectionError> {
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(SyntraConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`")
                }
            },
        };
    }
}

pub(crate) struct SyntraConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    peer_fingerprints: Rc<Mutex<HashMap<SocketAddr, String>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    recv_rx: Receiver<(ClientHandle, String, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, String, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
}

impl SyntraConnection {
    pub(crate) fn new(cert: Certificate, client_manager: ClientManager) -> Self {
        let (recv_tx, recv_rx) = channel();
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            peer_fingerprints: Default::default(),
            connecting: Default::default(),
            recv_rx,
            recv_tx,
            ping_response: Default::default(),
        }
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, String, ProtoEvent) {
        loop {
            let event = self.recv_rx.recv().await.expect("channel closed");
            if self.client_manager.contains(event.0) {
                return event;
            }
        }
    }

    /// Start establishing a connection before the first input event.
    ///
    /// This makes pairing and peer-version discovery independent of reaching
    /// an input-capture barrier.
    pub(crate) async fn connect(&self, handle: ClientHandle) {
        if !self.client_manager.contains(handle)
            || self.client_manager.active_addr(handle).is_some()
        {
            return;
        }

        let mut connecting = self.connecting.lock().await;
        if !connecting.insert(handle) {
            return;
        }
        spawn_local(connect_to_handle(
            self.client_manager.clone(),
            self.cert.clone(),
            handle,
            self.conns.clone(),
            self.peer_fingerprints.clone(),
            self.connecting.clone(),
            self.recv_tx.clone(),
            self.ping_response.clone(),
        ));
    }

    /// The peer has an authenticated transport, independently of whether
    /// either portal currently allows remote input.
    pub(crate) fn peer_connected(&self, handle: ClientHandle) -> bool {
        self.client_manager.active_addr(handle).is_some()
    }

    /// The destination explicitly confirmed that both receiving input and
    /// returning control are currently available.
    pub(crate) fn remote_ready(&self, handle: ClientHandle) -> bool {
        self.peer_connected(handle) && self.client_manager.remote_ready(handle)
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), SyntraConnectionError> {
        log::trace!("{event} >->->->->-");
        let requires_remote_ready = matches!(&event, ProtoEvent::Input(_) | ProtoEvent::Enter(_));
        let buf = event.encode()?;
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let conn = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(conn) = conn {
                if requires_remote_ready && !self.remote_ready(handle) {
                    return Err(SyntraConnectionError::TargetEmulationDisabled);
                }
                if let Err(e) = conn.send(&buf).await {
                    log::warn!("client {handle} failed to send: {e}");
                    if let Some(fingerprint) = disconnect(
                        &self.client_manager,
                        handle,
                        addr,
                        &conn,
                        &self.conns,
                        &self.peer_fingerprints,
                    )
                    .await
                    {
                        // Announce the dead transport exactly once so bound
                        // transfers are dropped instead of stalling.
                        self.recv_tx
                            .send((handle, fingerprint, ProtoEvent::Pong(false)))
                            .expect("channel closed");
                    }
                    self.connect(handle).await;
                    return Err(SyntraConnectionError::NotConnected);
                }
                log::trace!("sent event to {addr}");
                return Ok(());
            }
        }

        self.connect(handle).await;
        Err(SyntraConnectionError::NotConnected)
    }
}

async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    peer_fingerprints: Rc<Mutex<HashMap<SocketAddr, String>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, String, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) -> Result<(), SyntraConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(addrs) = client_manager.get_ips(handle) {
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = addrs
            .into_iter()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let res = connect_any(&addrs, cert).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                connecting.lock().await.remove(&handle);
                return Err(e);
            }
        };
        log::info!("client ({handle}) connected @ {addr}");
        let Some(fingerprint) = peer_certificate_fingerprint(&conn).await else {
            log::warn!("client ({handle}) @ {addr} did not present a peer certificate");
            let _ = conn.close().await;
            connecting.lock().await.remove(&handle);
            return Err(SyntraConnectionError::MissingPeerCertificate);
        };
        let retained = {
            let mut connections = conns.lock().await;
            let mut identities = peer_fingerprints.lock().await;
            if client_manager.contains(handle) {
                client_manager.set_active_addr(handle, Some(addr));
                connections.insert(addr, conn.clone());
                identities.insert(addr, fingerprint.clone());
                true
            } else {
                false
            }
        };
        if !retained {
            let _ = conn.close().await;
            connecting.lock().await.remove(&handle);
            return Err(SyntraConnectionError::NotConnected);
        }
        connecting.lock().await.remove(&handle);

        // Best-effort version handshake. Send our commit hash once
        // immediately after the DTLS handshake; the listen side
        // mirrors a Hello back so the receive loop can populate
        // `peer_commit`. Old peers will silently skip this event
        // per the forward-compat handler in [`receive_loop`].
        let buf = ProtoEvent::Hello {
            commit: local_commit(),
        }
        .encode()?;
        if let Err(e) = conn.send(&buf).await {
            log::debug!("hello send to {addr} failed: {e}");
        }

        // poll connection for active
        spawn_local(ping_pong(addr, conn.clone(), ping_response.clone()));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            handle,
            addr,
            conn,
            conns,
            peer_fingerprints,
            fingerprint,
            tx,
            ping_response.clone(),
        ));
        return Ok(());
    }
    connecting.lock().await.remove(&handle);
    Err(SyntraConnectionError::NotConnected)
}

async fn ping_pong(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    loop {
        let buf = ProtoEvent::Ping.encode().expect("Ping always encodes");

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if let Err(e) = conn.send(&buf).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                let _ = conn.close().await;
                break;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&addr) {
            log::warn!("{addr} did not respond, closing connection");
            let _ = conn.close().await;
            return;
        }
    }
}

fn pong_is_edge(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    remote_ready: bool,
) -> bool {
    !client_manager.alive(handle)
        || client_manager.remote_ready(handle) != remote_ready
        || client_manager.active_addr(handle) != Some(addr)
}
async fn receive_loop(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    peer_fingerprints: Rc<Mutex<HashMap<SocketAddr, String>>>,
    fingerprint: String,
    tx: Sender<(ClientHandle, String, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    let mut buf = [0u8; MAX_DATAGRAM_SIZE];
    while let Ok(len) = conn.recv(&mut buf).await {
        let current = conns
            .lock()
            .await
            .get(&addr)
            .is_some_and(|current| Arc::ptr_eq(current, &conn));
        if !client_manager.contains(handle) || !current {
            let _ = conn.close().await;
            break;
        }
        match ProtoEvent::decode(&buf[..len]) {
            Ok(event) => {
                log::trace!("{addr} <==<==<== {event}");
                match event {
                    ProtoEvent::Pong(remote_ready) => {
                        let changed = pong_is_edge(&client_manager, handle, addr, remote_ready);
                        client_manager.set_active_addr(handle, Some(addr));
                        client_manager.set_alive(handle, true);
                        client_manager.set_remote_ready(handle, remote_ready);
                        ping_response.borrow_mut().insert(addr);
                        if changed {
                            log::info!(
                                "peer state handle={handle} connected=true remote_ready={remote_ready} version_known={}",
                                client_manager.peer_commit(handle).is_some()
                            );
                            tx.send((handle, fingerprint.clone(), ProtoEvent::Pong(remote_ready)))
                                .expect("channel closed");
                        }
                    }
                    ProtoEvent::Hello { commit } => {
                        client_manager.set_peer_commit(handle, Some(commit));
                        log::info!(
                            "peer state handle={handle} connected={} remote_ready={} version_known=true",
                            client_manager.alive(handle),
                            client_manager.remote_ready(handle)
                        );
                        tx.send((handle, fingerprint.clone(), ProtoEvent::Hello { commit }))
                            .expect("channel closed");
                    }
                    event => tx
                        .send((handle, fingerprint.clone(), event))
                        .expect("channel closed"),
                }
            }
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            Err(e) => log::debug!("ignoring undecodable event from {addr}: {e}"),
        }
    }
    log::warn!("recv error");
    // Only the connection that is still current may tear down peer state:
    // a newer transport to the same address must not be invalidated by the
    // late exit of the one it replaced.
    if disconnect(
        &client_manager,
        handle,
        addr,
        &conn,
        &conns,
        &peer_fingerprints,
    )
    .await
    .is_some()
    {
        // Wake the capture state machine immediately when the transport
        // dies. Otherwise its last `remote_ready=true` could survive until
        // another unrelated event and leave a stale barrier active.
        tx.send((handle, fingerprint, ProtoEvent::Pong(false)))
            .expect("channel closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pong_edge_detects_state_and_endpoint_changes() {
        let manager = ClientManager::default();
        let handle = manager.add_client();
        let first: SocketAddr = "192.0.2.10:5353".parse().unwrap();
        let second: SocketAddr = "192.0.2.11:5353".parse().unwrap();

        assert!(pong_is_edge(&manager, handle, first, true));
        manager.set_active_addr(handle, Some(first));
        manager.set_alive(handle, true);
        manager.set_remote_ready(handle, true);
        assert!(!pong_is_edge(&manager, handle, first, true));
        assert!(pong_is_edge(&manager, handle, second, true));
        assert!(pong_is_edge(&manager, handle, first, false));
        manager.set_alive(handle, false);
        assert!(pong_is_edge(&manager, handle, first, true));
    }

    #[tokio::test]
    async fn queued_deleted_peer_events_never_reach_replacement_client() {
        let manager = ClientManager::default();
        let deleted = manager.add_client();
        let certificate = Certificate::generate_self_signed(["test".to_owned()]).unwrap();
        let mut connection = SyntraConnection::new(certificate, manager.clone());
        connection
            .recv_tx
            .send((deleted, "old-peer".into(), ProtoEvent::Pong(true)))
            .unwrap();
        manager.remove_client(deleted);
        let replacement = manager.add_client();
        connection
            .recv_tx
            .send((replacement, "new-peer".into(), ProtoEvent::Pong(false)))
            .unwrap();
        let (handle, fingerprint, event) = connection.recv().await;
        assert_eq!(handle, replacement);
        assert_eq!(fingerprint, "new-peer");
        assert!(matches!(event, ProtoEvent::Pong(false)));
    }
}

async fn peer_certificate_fingerprint(conn: &Arc<dyn Conn + Send + Sync>) -> Option<String> {
    let dtls_conn: &DTLSConn = conn.as_any().downcast_ref()?;
    let state = dtls_conn.connection_state().await;
    state
        .peer_certificates
        .first()
        .map(|cert| crypto::generate_fingerprint(cert))
}

async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: &Arc<dyn Conn + Send + Sync>,
    conns: &Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>,
    peer_fingerprints: &Mutex<HashMap<SocketAddr, String>>,
) -> Option<String> {
    let mut conns = conns.lock().await;
    // A newer connection may already have replaced this one on the same
    // address. Cleaning up then would tear down the live transport.
    match conns.get(&addr) {
        Some(current) if Arc::ptr_eq(current, conn) => {}
        _ => {
            log::debug!("stale cleanup for {addr} ignored");
            return None;
        }
    }
    log::warn!("client ({handle}) @ {addr} connection closed");
    conns.remove(&addr);
    client_manager.set_active_addr(handle, None);
    client_manager.set_alive(handle, false);
    client_manager.set_remote_ready(handle, false);
    client_manager.set_peer_commit(handle, None);
    let active: Vec<SocketAddr> = conns.keys().copied().collect();
    log::info!("active connections: {active:?}");
    peer_fingerprints.lock().await.remove(&addr)
}
