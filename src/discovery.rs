use lan_mouse_ipc::DiscoveredPeer;
use mdns_sd::{Receiver, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use std::{collections::HashMap, net::IpAddr, sync::mpsc as std_mpsc, thread, time::Duration};
use tokio::sync::watch;

pub const SERVICE_TYPE: &str = "_syntra._udp.local.";
const MAX_DISCOVERED_PEERS: usize = 256;
#[derive(Debug)]
pub enum DiscoveryEvent {
    Peers(Vec<DiscoveredPeer>),
    Error(String),
}

enum WorkerCommand {
    Refresh,
    ChangePort(u16),
    Shutdown,
}
#[derive(Clone)]
struct EventQueue {
    errors: watch::Sender<Option<String>>,
    snapshots: watch::Sender<Option<Vec<DiscoveredPeer>>>,
}

impl EventQueue {
    fn push_snapshot(&self, snapshot: Vec<DiscoveredPeer>) {
        self.snapshots.send_replace(Some(snapshot));
    }

    fn push_error(&self, error: String) {
        self.errors.send_replace(Some(error));
    }
}

struct PeerRecord {
    peer: DiscoveredPeer,
    generation: u64,
}

pub struct Discovery {
    errors: watch::Receiver<Option<String>>,
    snapshots: watch::Receiver<Option<Vec<DiscoveredPeer>>>,
    commands: Option<std_mpsc::Sender<WorkerCommand>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Discovery {
    pub fn start(
        port: u16,
        identity_fingerprint: &str,
        display_name: &str,
    ) -> Result<Self, String> {
        let mdns = ServiceDaemon::new().map_err(|error| error.to_string())?;
        let instance = instance_name(display_name, identity_fingerprint);
        let fullname = register(&mdns, &instance, display_name, port)?;
        let browser = mdns
            .browse(SERVICE_TYPE)
            .map_err(|error| error.to_string())?;
        let (error_tx, errors) = watch::channel(None);
        let (snapshot_tx, snapshots) = watch::channel(None);
        let worker_events = EventQueue {
            errors: error_tx,
            snapshots: snapshot_tx,
        };
        let (command_tx, command_rx) = std_mpsc::channel();
        let display_name = display_name.to_owned();

        let worker = thread::spawn(move || {
            run_worker(
                mdns,
                browser,
                command_rx,
                worker_events,
                instance,
                display_name,
                fullname,
            );
        });

        Ok(Self {
            errors,
            snapshots,
            commands: Some(command_tx),
            worker: Some(worker),
        })
    }

    pub fn refresh(&self) -> Result<(), String> {
        self.request(WorkerCommand::Refresh)
    }

    pub fn change_port(&mut self, port: u16) -> Result<(), String> {
        self.request(WorkerCommand::ChangePort(port))
    }
    pub async fn event(&mut self) -> Option<DiscoveryEvent> {
        loop {
            tokio::select! {
                biased;
                changed = self.errors.changed() => {
                    if changed.is_ok() {
                        if let Some(error) = self.errors.borrow_and_update().clone() {
                            return Some(DiscoveryEvent::Error(error));
                        }
                    } else if self.snapshots.has_changed().is_err() {
                        return None;
                    }
                }
                changed = self.snapshots.changed() => {
                    if changed.is_ok() {
                        if let Some(snapshot) = self.snapshots.borrow_and_update().clone() {
                            return Some(DiscoveryEvent::Peers(snapshot));
                        }
                    } else if self.errors.has_changed().is_err() {
                        return None;
                    }
                }
            }
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(WorkerCommand::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    fn request(&self, command: WorkerCommand) -> Result<(), String> {
        let commands = self
            .commands
            .as_ref()
            .ok_or_else(|| "mDNS discovery is shut down".to_owned())?;
        commands
            .send(command)
            .map_err(|_| "mDNS discovery worker stopped".to_owned())
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_worker(
    mdns: ServiceDaemon,
    mut browser: Receiver<ServiceEvent>,
    commands: std_mpsc::Receiver<WorkerCommand>,
    events: EventQueue,
    instance: String,
    display_name: String,
    mut own_fullname: String,
) {
    let mut peers = HashMap::<String, PeerRecord>::new();
    let mut last_snapshot = Vec::new();
    let mut generation = 0u64;
    let mut refresh_error_reported = false;
    let mut refresh_pending = false;

    loop {
        if refresh_pending {
            match mdns.browse(SERVICE_TYPE) {
                Ok(new_browser) => {
                    browser = new_browser;
                    refresh_error_reported = false;
                    refresh_pending = false;
                    peers.clear();
                    last_snapshot.clear();
                    events.push_snapshot(Vec::new());
                }
                Err(error) => {
                    if !refresh_error_reported {
                        events.push_error(format!("mDNS discovery restart failed: {error}"));
                        refresh_error_reported = true;
                    }
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                WorkerCommand::Refresh => match mdns.stop_browse(SERVICE_TYPE) {
                    Ok(()) => {
                        refresh_pending = true;
                        refresh_error_reported = false;
                    }
                    Err(error) => {
                        events.push_error(format!("mDNS discovery refresh failed: {error}"));
                    }
                },
                WorkerCommand::ChangePort(port) => {
                    let result = change_advertisement(
                        &mdns,
                        &instance,
                        &display_name,
                        &mut own_fullname,
                        port,
                    );
                    if let Err(error) = result {
                        events.push_error(error);
                    }
                }
                WorkerCommand::Shutdown => {
                    stop(&mdns, &own_fullname);
                    return;
                }
            }
        }

        match browser.recv_timeout(Duration::from_millis(100)) {
            Ok(ServiceEvent::ServiceResolved(service)) => {
                if service.get_fullname() == own_fullname {
                    continue;
                }
                let mut addresses = service
                    .get_addresses()
                    .iter()
                    .map(ScopedIp::to_ip_addr)
                    .collect::<Vec<IpAddr>>();
                addresses.sort_unstable();
                addresses.dedup();
                if addresses.is_empty() {
                    continue;
                }
                let peer = DiscoveredPeer {
                    id: service.get_fullname().to_owned(),
                    display_name: service
                        .get_property_val_str("name")
                        .unwrap_or_else(|| instance_label(service.get_fullname()))
                        .to_owned(),
                    addresses,
                    port: service.get_port(),
                };
                generation = generation.wrapping_add(1);
                insert_peer(&mut peers, peer, generation);
                emit_snapshot(&events, &peers, &mut last_snapshot);
            }
            Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                if peers.remove(&fullname).is_some() {
                    emit_snapshot(&events, &peers, &mut last_snapshot);
                }
            }
            Ok(_) => {}
            Err(_) if !browser.is_disconnected() => {}
            Err(error) => {
                events.push_error(format!("mDNS discovery receiver failed: {error}"));
                stop(&mdns, &own_fullname);
                return;
            }
        }
    }
}

fn change_advertisement(
    mdns: &ServiceDaemon,
    instance: &str,
    display_name: &str,
    own_fullname: &mut String,
    port: u16,
) -> Result<(), String> {
    let new_fullname = register(mdns, instance, display_name, port)
        .map_err(|error| format!("mDNS advertisement update failed: {error}"))?;
    *own_fullname = new_fullname;
    Ok(())
}

fn register(
    mdns: &ServiceDaemon,
    instance: &str,
    display_name: &str,
    port: u16,
) -> Result<String, String> {
    let hostname = format!("{instance}.local.");
    let properties = [("app", "syntra"), ("name", display_name)];
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        instance,
        &hostname,
        "0.0.0.0",
        port,
        &properties[..],
    )
    .map_err(|error| error.to_string())?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_owned();
    mdns.register(info).map_err(|error| error.to_string())?;
    Ok(fullname)
}

fn stop(mdns: &ServiceDaemon, own_fullname: &str) {
    let _ = mdns.unregister(own_fullname);
    let _ = mdns.stop_browse(SERVICE_TYPE);
    let _ = mdns.shutdown();
}

fn insert_peer(peers: &mut HashMap<String, PeerRecord>, peer: DiscoveredPeer, generation: u64) {
    if !peers.contains_key(&peer.id) && peers.len() == MAX_DISCOVERED_PEERS {
        if let Some(oldest_id) = peers
            .iter()
            .min_by_key(|(_, record)| record.generation)
            .map(|(id, _)| id.clone())
        {
            peers.remove(&oldest_id);
        }
    }
    peers.insert(peer.id.clone(), PeerRecord { peer, generation });
}

fn emit_snapshot(
    events: &EventQueue,
    peers: &HashMap<String, PeerRecord>,
    last_snapshot: &mut Vec<DiscoveredPeer>,
) {
    let mut snapshot = peers
        .values()
        .map(|record| record.peer.clone())
        .collect::<Vec<_>>();
    snapshot.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.display_name.cmp(&right.display_name))
            .then_with(|| left.port.cmp(&right.port))
    });
    if snapshot != *last_snapshot {
        *last_snapshot = snapshot.clone();
        events.push_snapshot(snapshot);
    }
}

fn instance_name(display_name: &str, fingerprint: &str) -> String {
    let suffix = fingerprint
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();
    let suffix = if suffix.is_empty() {
        "unknown"
    } else {
        &suffix
    };
    let label = sanitize_label(display_name, 63usize.saturating_sub(suffix.len() + 1));
    format!("{label}-{suffix}")
}

fn sanitize_label(value: &str, max_len: usize) -> String {
    let label = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(max_len)
        .collect::<String>();
    if label.is_empty() {
        "Syntra".chars().take(max_len).collect()
    } else {
        label
    }
}

fn instance_label(fullname: &str) -> &str {
    fullname.split('.').next().unwrap_or("Syntra")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn peer(id: usize) -> DiscoveredPeer {
        DiscoveredPeer {
            id: format!("peer-{id:03}._syntra._udp.local."),
            display_name: format!("Peer {id}"),
            addresses: vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))],
            port: 4242,
        }
    }

    #[test]
    fn peer_cache_evicts_the_oldest_record_at_its_limit() {
        let mut peers = HashMap::new();
        for generation in 0..MAX_DISCOVERED_PEERS {
            insert_peer(&mut peers, peer(generation), generation as u64);
        }

        let newest = peer(MAX_DISCOVERED_PEERS);
        insert_peer(&mut peers, newest.clone(), MAX_DISCOVERED_PEERS as u64);

        assert_eq!(peers.len(), MAX_DISCOVERED_PEERS);
        assert!(!peers.contains_key(&peer(0).id));
        assert!(peers.contains_key(&newest.id));
    }

    #[test]
    fn pending_snapshots_coalesce_to_the_latest_sorted_result() {
        let (error_tx, _errors) = watch::channel(None);
        let (snapshot_tx, mut snapshots) = watch::channel(None);
        let events = EventQueue {
            errors: error_tx,
            snapshots: snapshot_tx,
        };
        let mut peers = HashMap::new();
        let mut last_snapshot = Vec::new();

        insert_peer(&mut peers, peer(2), 1);
        emit_snapshot(&events, &peers, &mut last_snapshot);
        insert_peer(&mut peers, peer(1), 2);
        emit_snapshot(&events, &peers, &mut last_snapshot);

        assert!(snapshots.has_changed().unwrap());
        let snapshot = snapshots.borrow_and_update().clone().unwrap();
        assert_eq!(
            snapshot
                .iter()
                .map(|peer| peer.id.as_str())
                .collect::<Vec<_>>(),
            [
                "peer-001._syntra._udp.local.",
                "peer-002._syntra._udp.local."
            ]
        );
        assert!(!snapshots.has_changed().unwrap());
    }
}
