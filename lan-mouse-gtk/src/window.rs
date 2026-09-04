mod imp;

use std::collections::HashMap;
#[cfg(unix)]
use std::{fs, os::unix::net::UnixDatagram, path::PathBuf};

use adw::ActionRow;
use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::{Object, clone};
use gtk::{
    Button, NoSelection, gio,
    glib::{self, closure_local},
    prelude::*,
};

use lan_mouse_ipc::{
    ClientConfig, ClientHandle, ClientState, ClipboardSettings, ClipboardTransferId,
    ClipboardTransferState, ClipboardTransferStatus, DEFAULT_PORT, FrontendRequest,
    FrontendRequestWriter, Position,
};

use crate::{
    authorization_window::AuthorizationWindow, fingerprint_window::FingerprintWindow,
    key_object::KeyObject, key_row::KeyRow,
};

use super::{client_object::ClientObject, client_row::ClientRow};

#[cfg(target_os = "macos")]
fn set_button_content_label(button: &gtk::Button, label: &str) {
    // The Reenable/Grant/Relaunch button wraps its icon+label in an
    // AdwButtonContent (see window.ui). Walk into it and swap the label
    // rather than GtkButton::set_label, which would replace the content
    // widget and drop the icon.
    if let Some(content) = button.child().and_downcast::<adw::ButtonContent>() {
        content.set_label(label);
    }
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub(super) fn new(app: &adw::Application, conn: FrontendRequestWriter) -> Self {
        let window: Self = Object::builder().property("application", app).build();
        window
            .imp()
            .frontend_request_writer
            .borrow_mut()
            .replace(conn);
        window
    }
    #[cfg(unix)]
    fn clipboard_console_path() -> PathBuf {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("lan-mouse-clipboard-console.sock")
    }

    pub(super) fn setup_clipboard_console(&self) {
        #[cfg(unix)]
        {
            let path = Self::clipboard_console_path();
            let _ = fs::remove_file(&path);
            match UnixDatagram::bind(&path) {
                Ok(socket) => {
                    if socket.set_nonblocking(true).is_ok() {
                        self.imp().clipboard_console_socket.replace(Some(socket));
                    }
                }
                Err(error) => self.append_console_line(format!(
                    "[ERROR][console] cannot open live diagnostic stream: {error}"
                )),
            }
        }
        let refresh = clone!(
            #[weak(rename_to = window)]
            self,
            move || window.render_clipboard_console()
        );
        self.imp().clipboard_console_search.connect_search_changed(clone!(
            #[strong]
            refresh,
            move |_| refresh()
        ));
        for dropdown in [
            self.imp().clipboard_console_level.get(),
            self.imp().clipboard_console_stage.get(),
            self.imp().clipboard_console_direction.get(),
        ] {
            dropdown.connect_selected_notify(clone!(
                #[strong]
                refresh,
                move |_| refresh()
            ));
        }
        self.imp().clipboard_console_pause.connect_toggled(clone!(
            #[weak(rename_to = window)]
            self,
            move |button| {
                button.set_label(if button.is_active() { "Resume" } else { "Pause" });
                if !button.is_active() {
                    window.render_clipboard_console();
                }
            }
        ));
        self.imp().clipboard_console_clear.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| {
                window.imp().clipboard_console_lines.borrow_mut().clear();
                window.render_clipboard_console();
            }
        ));
        glib::timeout_add_local(
            std::time::Duration::from_millis(50),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    window.receive_clipboard_console();
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    fn append_console_line(&self, line: String) {
        let mut lines = self.imp().clipboard_console_lines.borrow_mut();
        lines.push_back(line);
        while lines.len() > 2_000 {
            lines.pop_front();
        }
    }

    fn receive_clipboard_console(&self) {
        #[cfg(unix)]
        {
            let mut received = false;
            if let Some(socket) = self.imp().clipboard_console_socket.borrow().as_ref() {
                let mut buffer = [0_u8; 65_536];
                loop {
                    match socket.recv(&mut buffer) {
                        Ok(length) => {
                            for line in String::from_utf8_lossy(&buffer[..length]).lines() {
                                self.append_console_line(line.to_owned());
                            }
                            received = true;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => {
                            self.append_console_line(format!(
                                "[ERROR][console] live stream failed: {error}"
                            ));
                            received = true;
                            break;
                        }
                    }
                }
            }
            if received && !self.imp().clipboard_console_pause.is_active() {
                self.render_clipboard_console();
            }
        }
    }

    fn render_clipboard_console(&self) {
        let query = self.imp().clipboard_console_search.text().to_string().to_lowercase();
        let level = self.imp().clipboard_console_level.selected();
        let stage = self.imp().clipboard_console_stage.selected();
        let direction = self.imp().clipboard_console_direction.selected();
        let stage_terms: &[&str] = match stage {
            1 => &["copy", "owner-changed", "local-selection", "manifest"],
            2 => &["peer", "protocol", "range", "manifest", "transport"],
            3 => &["set-selection", "publication", "portal", "selection-transfer"],
            4 => &["paste", "selection-transfer-request", "lookup", "open", "read"],
            5 => &["fuse", "lookup", "getattr", "open", "read", "release"],
            6 => &["progress", "completed", "cancel", "unmounted", "released"],
            7 => &["[error]", "[warn]", "failed", "rejected"],
            _ => &[],
        };
        let direction_term = match direction {
            1 => "direction=local",
            2 => "direction=outbound",
            3 => "direction=inbound",
            _ => "",
        };
        let level_term = match level {
            1 => "[ERROR]",
            2 => "[WARN]",
            3 => "[INFO]",
            4 => "[DEBUG]",
            5 => "[TRACE]",
            _ => "",
        };
        let lines = self.imp().clipboard_console_lines.borrow();
        let text = lines
            .iter()
            .filter(|line| level_term.is_empty() || line.contains(level_term))
            .filter(|line| {
                let lower = line.to_lowercase();
                (stage_terms.is_empty() || stage_terms.iter().any(|term| lower.contains(term)))
                    && (direction_term.is_empty() || lower.contains(direction_term))
                    && (query.is_empty() || lower.contains(&query))
            })
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let adjustment = self.imp().clipboard_console_scroll.vadjustment();
        let follow = adjustment.value() + adjustment.page_size() >= adjustment.upper() - 4.0;
        let buffer = self.imp().clipboard_console.buffer();
        buffer.set_text(&text);
        if follow {
            glib::idle_add_local_once(move || {
                adjustment.set_value(adjustment.upper() - adjustment.page_size());
            });
        }
    }

    fn clients(&self) -> gio::ListStore {
        self.imp()
            .clients
            .borrow()
            .clone()
            .expect("Could not get clients")
    }

    fn authorized(&self) -> gio::ListStore {
        self.imp()
            .authorized
            .borrow()
            .clone()
            .expect("Could not get authorized")
    }

    fn client_by_idx(&self, idx: u32) -> Option<ClientObject> {
        self.clients().item(idx).map(|o| o.downcast().unwrap())
    }

    fn authorized_by_idx(&self, idx: u32) -> Option<KeyObject> {
        self.authorized().item(idx).map(|o| o.downcast().unwrap())
    }

    fn row_by_idx(&self, idx: i32) -> Option<ClientRow> {
        self.imp()
            .client_list
            .get()
            .row_at_index(idx)
            .map(|o| o.downcast().expect("expected ClientRow"))
    }

    fn setup_authorized(&self) {
        let store = gio::ListStore::new::<KeyObject>();
        self.imp().authorized.replace(Some(store));
        let selection_model = NoSelection::new(Some(self.authorized()));
        self.imp().authorized_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let key_obj = obj.downcast_ref().expect("object of type `KeyObject`");
                    let row = window.create_key_row(key_obj);
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: KeyRow| {
                                if let Some(key_obj) = window.authorized_by_idx(row.index() as u32)
                                {
                                    window.request_fingerprint_remove(key_obj.get_fingerprint());
                                }
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        )
    }

    fn setup_clients(&self) {
        let model = gio::ListStore::new::<ClientObject>();
        self.imp().clients.replace(Some(model));

        let selection_model = NoSelection::new(Some(self.clients()));
        self.imp().client_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let client_object = obj
                        .downcast_ref()
                        .expect("Expected object of type `ClientObject`.");
                    let row = window.create_client_row(client_object);
                    row.connect_closure(
                        "request-hostname-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, hostname: String| {
                                log::debug!("request-hostname-change");
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let hostname = Some(hostname).filter(|s| !s.is_empty());
                                    /* changed in response to FrontendEvent
                                     * -> do not request additional update */
                                    window.request(FrontendRequest::UpdateHostname(
                                        client.handle(),
                                        hostname,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-port-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, port: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::UpdatePort(
                                        client.handle(),
                                        port as u16,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-activate",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, active: bool| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    log::debug!(
                                        "request: {} client",
                                        if active { "activating" } else { "deactivating" }
                                    );
                                    window.request(FrontendRequest::Activate(
                                        client.handle(),
                                        active,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::Delete(client.handle()));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-dns",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::ResolveDns(
                                        client.get_data().handle,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-position-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, pos_idx: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let position = match pos_idx {
                                        0 => Position::Left,
                                        1 => Position::Right,
                                        2 => Position::Top,
                                        _ => Position::Bottom,
                                    };
                                    window.request(FrontendRequest::UpdatePosition(
                                        client.handle(),
                                        position,
                                    ));
                                }
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        );
    }

    fn setup_icon(&self) {
        self.set_icon_name(Some("de.feschber.LanMouse"));
    }

    /// workaround for a bug in libadwaita that shows an ugly line beneath
    /// the last element if a placeholder is set.
    /// https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/6308
    fn update_placeholder_visibility(&self) {
        let visible = self.clients().n_items() == 0;
        let placeholder = self.imp().client_placeholder.get();
        self.imp().client_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn update_auth_placeholder_visibility(&self) {
        let visible = self.authorized().n_items() == 0;
        let placeholder = self.imp().authorized_placeholder.get();
        self.imp().authorized_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn create_client_row(&self, client_object: &ClientObject) -> ClientRow {
        let row = ClientRow::new(client_object);
        row.bind(client_object);
        row
    }

    fn create_key_row(&self, key_object: &KeyObject) -> KeyRow {
        let row = KeyRow::new();
        row.bind(key_object);
        row
    }

    pub(super) fn new_client(
        &self,
        handle: ClientHandle,
        client: ClientConfig,
        state: ClientState,
    ) {
        let client = ClientObject::new(handle, client, state.clone());
        self.clients().append(&client);
        self.update_placeholder_visibility();
        self.update_dns_state(handle, !state.ips.is_empty());
    }

    pub(super) fn update_client_list(
        &self,
        clients: Vec<(ClientHandle, ClientConfig, ClientState)>,
    ) {
        for (handle, client, state) in clients {
            if self.client_idx(handle).is_some() {
                self.update_client_config(handle, client);
                self.update_client_state(handle, state);
            } else {
                self.new_client(handle, client, state);
            }
        }
    }

    pub(super) fn update_port(&self, port: u16, msg: Option<String>) {
        if let Some(msg) = msg {
            self.show_toast(msg.as_str());
        }
        self.imp().set_port(port);
    }

    fn client_idx(&self, handle: ClientHandle) -> Option<usize> {
        self.clients()
            .iter::<ClientObject>()
            .position(|c| c.ok().map(|c| c.handle() == handle).unwrap_or_default())
    }

    pub(super) fn delete_client(&self, handle: ClientHandle) {
        let Some(idx) = self.client_idx(handle) else {
            log::warn!("could not find client with handle {handle}");
            return;
        };

        self.clients().remove(idx as u32);
        if self.clients().n_items() == 0 {
            self.update_placeholder_visibility();
        }
    }

    pub(super) fn update_client_config(&self, handle: ClientHandle, client: ClientConfig) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        row.set_hostname(client.hostname);
        row.set_port(client.port);
        row.set_position(client.pos);
    }

    pub(super) fn update_client_state(&self, handle: ClientHandle, state: ClientState) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        let Some(client_object) = self.client_object_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };

        /* activation state */
        row.set_active(state.active);

        /* dns state */
        client_object.set_resolving(state.resolving);

        self.update_dns_state(handle, !state.ips.is_empty());
        let ips = state
            .ips
            .into_iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>();
        client_object.set_ips(ips);

        /* peer build version (drives the version-match indicator) */
        client_object.set_property(
            "peer-commit",
            crate::client_object::peer_commit_to_string(state.peer_commit),
        );
        row.refresh_version_status();
    }

    fn client_object_for_handle(&self, handle: ClientHandle) -> Option<ClientObject> {
        self.client_idx(handle)
            .and_then(|i| self.client_by_idx(i as u32))
    }

    fn row_for_handle(&self, handle: ClientHandle) -> Option<ClientRow> {
        self.client_idx(handle)
            .and_then(|i| self.row_by_idx(i as i32))
    }

    fn update_dns_state(&self, handle: ClientHandle, resolved: bool) {
        if let Some(client_row) = self.row_for_handle(handle) {
            client_row.set_dns_state(resolved);
        }
    }

    fn request_port_change(&self) {
        let port = self
            .imp()
            .port_entry
            .get()
            .text()
            .as_str()
            .parse::<u16>()
            .unwrap_or(DEFAULT_PORT);
        self.request(FrontendRequest::ChangePort(port));
    }

    fn request_capture(&self) {
        self.request(FrontendRequest::EnableCapture);
    }

    fn request_emulation(&self) {
        self.request(FrontendRequest::EnableEmulation);
    }

    fn request_client_create(&self) {
        self.request(FrontendRequest::Create);
    }

    fn open_fingerprint_dialog(&self, fp: Option<String>) {
        let window = FingerprintWindow::new(fp);
        window.set_transient_for(Some(self));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: FingerprintWindow, desc: String, fp: String| {
                    parent.request_fingerprint_add(desc, fp);
                    w.close();
                }
            ),
        );
        window.present();
    }

    fn request_fingerprint_add(&self, desc: String, fp: String) {
        self.request(FrontendRequest::AuthorizeKey(desc, fp));
    }

    fn request_fingerprint_remove(&self, fp: String) {
        self.request(FrontendRequest::RemoveAuthorizedKey(fp));
    }

    fn request(&self, request: FrontendRequest) {
        let mut requester = self.imp().frontend_request_writer.borrow_mut();
        let requester = requester.as_mut().unwrap();
        if let Err(e) = requester.request(request) {
            log::error!("error sending message: {e}");
        };
    }

    pub(super) fn show_toast(&self, msg: &str) {
        let toast = adw::Toast::new(msg);
        self.add_toast(toast);
    }

    pub(super) fn add_toast(&self, toast: adw::Toast) {
        let toast_overlay = &self.imp().toast_overlay;
        toast_overlay.add_toast(toast);
    }

    pub(super) fn set_clipboard_settings(&self, settings: ClipboardSettings) {
        self.imp().clipboard_text_switch.set_state(settings.text);
        self.imp().clipboard_text_switch.set_active(settings.text);
        self.imp().clipboard_image_switch.set_state(settings.image);
        self.imp().clipboard_image_switch.set_active(settings.image);
        self.imp().clipboard_files_switch.set_state(settings.files);
        self.imp().clipboard_files_switch.set_active(settings.files);
    }

    pub(super) fn update_clipboard_transfer(&self, status: ClipboardTransferStatus) {
        let mut rows = self.imp().transfer_rows.borrow_mut();
        let entry = rows
            .entry((status.transfer_id, status.file_id))
            .or_insert_with(|| {
                let row = ActionRow::new();
                let cancel = Button::with_label("Cancel");
                cancel.update_property(&[gtk::accessible::Property::Label(&format!(
                    "Cancel clipboard transfer: {}",
                    status.name
                ))]);
                cancel.connect_clicked(clone!(
                    #[strong(rename_to = window)]
                    self,
                    move |_| window
                        .request(FrontendRequest::CancelClipboardTransfer(status.transfer_id))
                ));
                row.add_suffix(&cancel);
                self.imp().transfers_list.append(&row);
                (row, cancel)
            });
        let (row, cancel) = entry;
        let terminal = matches!(
            &status.state,
            ClipboardTransferState::Completed
                | ClipboardTransferState::Cancelled
                | ClipboardTransferState::Failed(_)
        );
        cancel.set_sensitive(!terminal);
        cancel.set_visible(!terminal);
        let direction = match status.direction {
            lan_mouse_ipc::ClipboardTransferDirection::Sending => "Sending",
            lan_mouse_ipc::ClipboardTransferDirection::Receiving => "Receiving",
        };
        let state = match &status.state {
            ClipboardTransferState::Failed(reason) => format!("Failed: {reason}"),
            other => format!("{other:?}"),
        };
        let subtitle = format!(
            "{direction} · {} / {} bytes · {state} · {} bytes/s",
            status.transferred_bytes, status.total_bytes, status.bytes_per_second
        );
        row.set_title(&status.name);
        row.set_subtitle(&subtitle);
        row.update_property(&[gtk::accessible::Property::Label(&format!(
            "{}: {}",
            status.name, subtitle
        ))]);
    }

    pub(super) fn set_capture(&self, active: bool) {
        self.imp().capture_active.replace(active);
        self.update_capture_emulation_status();
    }

    pub(super) fn set_emulation(&self, active: bool) {
        self.imp().emulation_active.replace(active);
        self.update_capture_emulation_status();
    }

    #[cfg(target_os = "macos")]
    pub(super) fn refresh_capture_emulation_status(&self) {
        self.update_capture_emulation_status();
    }

    fn update_capture_emulation_status(&self) {
        let capture = self.imp().capture_active.get();
        let emulation = self.imp().emulation_active.get();

        #[cfg(target_os = "macos")]
        {
            // On macOS, capture and emulation share the same TCC gate
            // (Accessibility). Collapse to a single warning row —
            // emulation_status_row stays hidden and capture_status_row
            // doubles as the shared status indicator. Its text and
            // button mutate based on whether we're waiting for AX or
            // waiting for the user to relaunch the app.
            let anything_off = !capture || !emulation;
            self.imp().emulation_status_row.set_visible(false);
            self.imp().capture_status_row.set_visible(anything_off);
            self.imp().capture_emulation_group.set_visible(anything_off);

            if anything_off {
                self.update_macos_warning_row_text();
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            self.imp().capture_status_row.set_visible(!capture);
            self.imp().emulation_status_row.set_visible(!emulation);
            self.imp()
                .capture_emulation_group
                .set_visible(!capture || !emulation);
        }
    }

    #[cfg(target_os = "macos")]
    fn update_macos_warning_row_text(&self) {
        let row = &self.imp().capture_status_row;
        let button = &self.imp().input_capture_button;

        if crate::macos_privacy::accessibility_granted() {
            // AX granted but capture/emulation still off → the daemon
            // subprocess bailed at startup and needs a fresh process to
            // re-initialize with the new grant in place.
            row.set_title("relaunch required");
            row.set_subtitle("Accessibility granted — restart to activate capture and emulation");
            set_button_content_label(button, "Relaunch");
        } else {
            // AX missing → send the user to System Settings.
            row.set_title("input capture is disabled");
            row.set_subtitle("grant Accessibility permission to enable");
            set_button_content_label(button, "Grant");
        }
    }

    pub(super) fn set_authorized_keys(&self, fingerprints: HashMap<String, String>) {
        let authorized = self.authorized();
        // clear list
        authorized.remove_all();
        // insert fingerprints
        for (fingerprint, description) in fingerprints {
            let key_obj = KeyObject::new(description, fingerprint);
            authorized.append(&key_obj);
        }
        self.update_auth_placeholder_visibility();
    }

    pub(super) fn set_pk_fp(&self, fingerprint: &str) {
        self.imp().fingerprint_row.set_subtitle(fingerprint);
    }

    pub(super) fn request_authorization(&self, fingerprint: &str) {
        if let Some(w) = self.imp().authorization_window.borrow_mut().take() {
            w.close();
        }
        let window = AuthorizationWindow::new(fingerprint);
        window.set_transient_for(Some(self));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: AuthorizationWindow, fp: String| {
                    w.close();
                    parent.open_fingerprint_dialog(Some(fp));
                }
            ),
        );
        window.connect_closure(
            "cancel-clicked",
            false,
            closure_local!(move |w: AuthorizationWindow| {
                w.close();
            }),
        );
        window.present();
        self.imp().authorization_window.replace(Some(window));
    }
}
