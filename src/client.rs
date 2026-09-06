use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    rc::Rc,
};

use lan_mouse_ipc::{ClientConfig, ClientHandle, ClientState, Position};

use crate::config::ConfigClient;

#[derive(Clone)]
pub struct ClientManager {
    clients: Rc<RefCell<BTreeMap<ClientHandle, (ClientConfig, ClientState)>>>,
    next_handle: Rc<Cell<ClientHandle>>,
    peer_fingerprints: Rc<RefCell<HashMap<ClientHandle, String>>>,
}

impl Default for ClientManager {
    fn default() -> Self {
        Self {
            clients: Rc::default(),
            next_handle: Rc::new(Cell::new(0)),
            peer_fingerprints: Rc::default(),
        }
    }
}

pub(crate) const ENTER_HANDLE_BEGIN: ClientHandle = u64::MAX / 2 + 1;

impl ClientManager {
    /// get all clients
    pub fn clients(&self) -> Vec<(ClientConfig, ClientState)> {
        self.clients.borrow().values().cloned().collect()
    }

    pub fn add_with_config(&self, config_client: ConfigClient) -> ClientHandle {
        let config = ClientConfig {
            hostname: config_client.hostname,
            fix_ips: config_client.ips.into_iter().collect(),
            port: config_client.port,
            pos: config_client.pos,
            cmd: config_client.enter_hook,
        };
        let state = ClientState {
            active: config_client.active,
            ips: HashSet::from_iter(config.fix_ips.iter().cloned()),
            ..Default::default()
        };
        let handle = self.add_client();
        self.set_config(handle, config);
        self.set_state(handle, state);
        if let Some(fingerprint) = config_client.peer_fingerprint {
            self.set_peer_fingerprint(handle, fingerprint);
        }
        handle
    }

    pub fn add_client(&self) -> ClientHandle {
        let handle = self.next_handle.get();
        if handle >= ENTER_HANDLE_BEGIN {
            panic!("client handle space exhausted");
        }
        self.next_handle.set(
            handle
                .checked_add(1)
                .expect("client handle space exhausted"),
        );
        self.clients.borrow_mut().insert(handle, Default::default());
        handle
    }

    pub fn contains(&self, handle: ClientHandle) -> bool {
        self.clients.borrow().contains_key(&handle)
    }

    pub fn set_config(&self, handle: ClientHandle, config: ClientConfig) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(&handle) {
            *c = config;
        }
    }

    pub fn set_state(&self, handle: ClientHandle, state: ClientState) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(&handle) {
            *s = state;
        }
    }

    pub fn activate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(&handle) {
            Some((_, s)) if !s.active => {
                s.active = true;
                true
            }
            _ => false,
        }
    }

    pub fn deactivate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(&handle) {
            Some((_, s)) if s.active => {
                s.active = false;
                true
            }
            _ => false,
        }
    }

    pub fn get_client(&self, addr: SocketAddr) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (_, s))| (s.active && s.ips.contains(&addr.ip())).then_some(*k))
    }

    pub fn client_at(&self, pos: Position) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (c, s))| (s.active && c.pos == pos).then_some(*k))
    }

    pub(crate) fn get_hostname(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow()
            .get(&handle)
            .and_then(|(c, _)| c.hostname.clone())
    }

    pub(crate) fn get_pos(&self, handle: ClientHandle) -> Option<Position> {
        self.clients.borrow().get(&handle).map(|(c, _)| c.pos)
    }

    pub fn remove_client(&self, client: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        self.peer_fingerprints.borrow_mut().remove(&client);
        self.clients.borrow_mut().remove(&client)
    }

    pub fn get_state(&self, handle: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        self.clients.borrow().get(&handle).cloned()
    }

    /// Last certificate identity authenticated for this configured route.
    ///
    /// This is presentation/persistence metadata only. Transport authorization
    /// must continue to use the live authenticated connection.
    pub(crate) fn peer_fingerprint(&self, handle: ClientHandle) -> Option<String> {
        self.peer_fingerprints.borrow().get(&handle).cloned()
    }

    pub(crate) fn set_peer_fingerprint(&self, handle: ClientHandle, fingerprint: String) {
        if self.clients.borrow().contains_key(&handle) {
            self.peer_fingerprints
                .borrow_mut()
                .insert(handle, fingerprint);
        }
    }

    /// get the current config & state of all clients
    pub fn get_client_states(&self) -> Vec<(ClientHandle, ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(k, v)| (*k, v.0.clone(), v.1.clone()))
            .collect()
    }

    /// update the fix ips of the client
    pub fn set_fix_ips(&self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(&handle) {
            c.fix_ips = fix_ips
        }
        self.update_ips(handle);
    }

    /// update the dns-ips of the client
    pub fn set_dns_ips(&self, handle: ClientHandle, dns_ips: Vec<IpAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(&handle) {
            s.dns_ips = dns_ips
        }
        self.update_ips(handle);
    }

    fn update_ips(&self, handle: ClientHandle) {
        if let Some((c, s)) = self.clients.borrow_mut().get_mut(&handle) {
            s.ips = c
                .fix_ips
                .iter()
                .cloned()
                .chain(s.dns_ips.iter().cloned())
                .collect::<HashSet<_>>();
        }
    }

    /// update the hostname of the given client
    /// this automatically clears the active ip address and ips from dns
    pub fn set_hostname(&self, handle: ClientHandle, hostname: Option<String>) -> bool {
        let mut clients = self.clients.borrow_mut();
        let Some((c, s)) = clients.get_mut(&handle) else {
            return false;
        };

        // hostname changed
        if c.hostname != hostname {
            c.hostname = hostname;
            s.active_addr = None;
            s.dns_ips.clear();
            drop(clients);
            self.update_ips(handle);
            true
        } else {
            false
        }
    }

    /// update the port of the client
    pub(crate) fn set_port(&self, handle: ClientHandle, port: u16) {
        match self.clients.borrow_mut().get_mut(&handle) {
            Some((c, s)) if c.port != port => {
                c.port = port;
                s.active_addr = s.active_addr.map(|a| SocketAddr::new(a.ip(), port));
            }
            _ => {}
        };
    }

    /// update the position of the client
    /// returns true, if a change in capture position is required (pos changed & client is active)
    pub(crate) fn set_pos(&self, handle: ClientHandle, pos: Position) -> bool {
        match self.clients.borrow_mut().get_mut(&handle) {
            Some((c, s)) if c.pos != pos => {
                log::info!("update pos {handle} {} -> {}", c.pos, pos);
                c.pos = pos;
                s.active
            }
            _ => false,
        }
    }

    /// update the enter hook command of the client
    pub(crate) fn set_enter_hook(&self, handle: ClientHandle, enter_hook: Option<String>) {
        if let Some((c, _s)) = self.clients.borrow_mut().get_mut(&handle) {
            c.cmd = enter_hook;
        }
    }

    /// set resolving status of the client
    pub(crate) fn set_resolving(&self, handle: ClientHandle, status: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(&handle) {
            s.resolving = status;
        }
    }

    /// get the enter hook command
    pub(crate) fn get_enter_cmd(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow()
            .get(&handle)
            .and_then(|(c, _)| c.cmd.clone())
    }

    /// returns all clients that are currently registered
    pub(crate) fn registered_clients(&self) -> Vec<ClientHandle> {
        self.clients.borrow().iter().map(|(h, _)| *h).collect()
    }

    /// returns all clients that are currently active
    pub(crate) fn active_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .filter(|(_, (_, s))| s.active)
            .map(|(h, _)| *h)
            .collect()
    }

    /// Returns every connected peer that can currently receive clipboard data.
    ///
    /// Clipboard synchronization is independent of which peer currently owns
    /// the input-capture route.
    pub(crate) fn clipboard_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .filter(|(_, (_, state))| state.alive && state.remote_ready)
            .map(|(handle, _)| *handle)
            .collect()
    }

    pub(crate) fn set_active_addr(&self, handle: ClientHandle, addr: Option<SocketAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(&handle) {
            s.active_addr = addr;
        }
    }

    pub(crate) fn set_alive(&self, handle: ClientHandle, alive: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(&handle) {
            s.alive = alive;
        }
    }

    pub(crate) fn set_remote_ready(&self, handle: ClientHandle, ready: bool) {
        if let Some((_, state)) = self.clients.borrow_mut().get_mut(&handle) {
            state.remote_ready = ready;
        }
    }

    pub(crate) fn set_peer_commit(&self, handle: ClientHandle, commit: Option<[u8; 8]>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(&handle) {
            s.peer_commit = commit;
        }
    }

    pub(crate) fn active_addr(&self, handle: ClientHandle) -> Option<SocketAddr> {
        self.clients
            .borrow()
            .get(&handle)
            .and_then(|(_, s)| s.active_addr)
    }

    pub(crate) fn alive(&self, handle: ClientHandle) -> bool {
        self.clients
            .borrow()
            .get(&handle)
            .map(|(_, s)| s.alive)
            .unwrap_or(false)
    }

    pub(crate) fn remote_ready(&self, handle: ClientHandle) -> bool {
        self.clients
            .borrow()
            .get(&handle)
            .map(|(_, state)| state.remote_ready)
            .unwrap_or(false)
    }
    pub(crate) fn peer_commit(&self, handle: ClientHandle) -> Option<[u8; 8]> {
        self.clients
            .borrow()
            .get(&handle)
            .and_then(|(_, state)| state.peer_commit)
    }

    pub(crate) fn get_port(&self, handle: ClientHandle) -> Option<u16> {
        self.clients.borrow().get(&handle).map(|(c, _)| c.port)
    }

    pub(crate) fn get_ips(&self, handle: ClientHandle) -> Option<HashSet<IpAddr>> {
        self.clients
            .borrow()
            .get(&handle)
            .map(|(_, s)| s.ips.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_handle_cannot_alias_new_client() {
        let manager = ClientManager::default();
        let deleted = manager.add_client();
        assert!(manager.remove_client(deleted).is_some());

        let current = manager.add_client();
        assert_ne!(deleted, current);
        assert!(!manager.contains(deleted));
        assert!(manager.contains(current));

        manager.set_config(deleted, ClientConfig::default());
        manager.set_state(
            deleted,
            ClientState {
                active: true,
                alive: true,
                ..ClientState::default()
            },
        );
        manager.set_alive(deleted, true);

        assert!(manager.get_state(deleted).is_none());
        let (_, state) = manager.get_state(current).unwrap();
        assert!(!state.active);
        assert!(!state.alive);
    }
}
