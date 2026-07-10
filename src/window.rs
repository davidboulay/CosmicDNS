// SPDX-License-Identifier: GPL-3.0-only

use std::sync::LazyLock;
use std::time::Duration;

use cosmic::{
    Element, Task, app,
    applet::{cosmic_panel_config::PanelAnchor, menu_button, padded_control},
    cosmic_theme::Spacing,
    iced::{
        Alignment, Length, Subscription, stream,
        futures::{SinkExt, channel::mpsc},
        platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup},
        widget::{column, row},
        window,
    },
    theme,
    widget::{
        Id, autosize, button, container, divider, icon, mouse_area, scrollable, settings, text,
        text_input, toggler,
    },
};

use cosmic::cosmic_config::{self, ConfigGet, ConfigSet};

use crate::backend::{self, DnsState};
use crate::updater;

static AUTOSIZE_MAIN_ID: LazyLock<Id> = LazyLock::new(|| Id::new("dns-autosize-main"));

/// Bump if the persisted config layout ever changes incompatibly.
const CONFIG_VERSION: u64 = 1;
/// Config key for the saved DNS server list.
const SERVERS_KEY: &str = "servers";
/// Config key for the name of the entry last applied, so the on/off toggle
/// knows what "on" should mean.
const LAST_CUSTOM_KEY: &str = "last-custom";
/// Config key marking that we already imported the DNS found on the device at
/// first run, so deleting that entry doesn't resurrect it forever.
const IMPORTED_KEY: &str = "imported-initial";
/// Config key for the "automatically update the applet" toggle.
const AUTO_UPDATE_KEY: &str = "auto-update";
/// The release tag we last auto-installed. Prevents an endless update loop when
/// a release is mis-versioned (its binary reports an older version than its tag,
/// so it always looks "newer"): we only auto-apply a given tag once.
const LAST_AUTO_UPDATE_KEY: &str = "last-auto-update";

/// A named set of DNS servers the user can switch to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DnsEntry {
    pub name: String,
    pub addresses: Vec<String>,
}

/// Sensible starting points; the user can delete any of them in Settings.
fn preset_entries() -> Vec<DnsEntry> {
    let entry = |name: &str, addrs: &[&str]| DnsEntry {
        name: name.to_string(),
        addresses: addrs.iter().map(|s| s.to_string()).collect(),
    };
    vec![
        entry("Cloudflare", &["1.1.1.1", "1.0.0.1"]),
        entry("Google", &["8.8.8.8", "8.8.4.4"]),
        entry("Quad9", &["9.9.9.9", "149.112.112.112"]),
    ]
}

/// Where the applet's own version sits relative to the latest GitHub release.
#[derive(Debug, Clone)]
enum ReleaseStatus {
    /// No check has completed yet.
    Unknown,
    Checking,
    UpToDate,
    /// A newer release exists; holds its tag (e.g. "v0.2.0").
    Available(String),
    Error(String),
}

// Panel badges with the state baked into the colour: orange server-stack when a
// custom DNS is forced, grey one when the router/DHCP default is in use.
const ICON_CUSTOM_SVG: &[u8] = include_bytes!("../icons/dns-custom.svg");
const ICON_ROUTER_SVG: &[u8] = include_bytes!("../icons/dns-router.svg");

pub struct Window {
    core: app::Core,
    popup: Option<window::Id>,
    /// Saved DNS server entries (persisted).
    servers: Vec<DnsEntry>,
    /// Last state read from NetworkManager (None until the first check lands).
    current: Option<DnsState>,
    /// Set while a switch is being applied to the device.
    applying: bool,
    /// Set while a manual cache flush runs.
    flushing: bool,
    /// Outcome caption of the last action ("DNS cache flushed", errors, …).
    notice: Option<(bool, String)>, // (is_error, text)
    /// Name of the entry last applied — what the on/off toggle turns back on.
    last_custom: Option<String>,
    /// Whether we already imported the device's pre-existing custom DNS.
    imported_initial: bool,
    /// Persisted settings handle (None if the config backend is unavailable).
    config: Option<cosmic_config::Config>,
    /// True while the settings panel is showing instead of the server list.
    show_settings: bool,
    /// Settings form fields for adding a new entry — or, when `editing` is
    /// set, for modifying that existing one.
    edit_name: String,
    edit_addresses: String,
    /// Index of the entry loaded into the form for editing.
    editing: Option<usize>,
    /// Index of the entry currently being drag-reordered (follows the row as
    /// it moves — the list is reordered live while the mouse is held down).
    dragging: Option<usize>,
    /// Whether to auto-install newer releases of the applet itself.
    auto_update: bool,
    /// Latest-release status for the applet's own version.
    release: ReleaseStatus,
    /// True while a self-update download is in progress.
    self_updating: bool,
    /// The release tag we've already auto-installed (persisted), so a
    /// mis-versioned release can't loop us into re-applying it forever.
    last_auto_update: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    CloseRequested(window::Id),
    /// Re-read the DNS state from NetworkManager.
    RefreshStatus,
    StatusChecked(Result<DnsState, String>),
    /// Switch to the saved entry at this index.
    SelectEntry(usize),
    /// The on/off switch: on re-applies the last used entry, off returns to
    /// the router/DHCP default.
    SetCustom(bool),
    /// A switch (either direction) finished.
    Applied(Result<DnsState, String>),
    /// Flush the resolver cache without changing servers.
    FlushCache,
    Flushed(Result<(), String>),
    /// Show/hide the settings panel.
    ToggleSettings,
    EditName(String),
    EditAddresses(String),
    /// Create a new entry from the form — or save it over the entry being
    /// edited when one is loaded.
    AddEntry,
    RemoveEntry(usize),
    /// Load an existing entry into the form for editing.
    EditEntry(usize),
    CancelEdit,
    /// Drag-reorder: press on a row, live-move while hovering others, commit
    /// on release (or when the pointer leaves the list).
    DragStart(usize),
    DragOver(usize),
    DragEnd,
    /// Check GitHub for a newer release of the applet.
    CheckRelease,
    ReleaseChecked(Result<String, String>),
    SetAutoUpdate(bool),
    /// Download and install the given release tag of the applet, then relaunch.
    SelfUpdate(String),
    /// Carries the tag that was installed and the path of the replaced binary.
    SelfUpdated(String, Result<std::path::PathBuf, String>),
}

/// Order-insensitive comparison of two server lists, so "1.1.1.1, 1.0.0.1"
/// still matches its entry if NetworkManager reports the pair reversed.
fn same_servers(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<&str> = a.iter().map(String::as_str).collect();
    let mut b: Vec<&str> = b.iter().map(String::as_str).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

/// Parse the free-form addresses field: comma/space separated IPv4/IPv6
/// literals. Returns None if anything in it isn't an IP address.
fn parse_addresses(input: &str) -> Option<Vec<String>> {
    let addrs: Vec<String> = input
        .split([',', ' ', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if addrs.is_empty() || !addrs.iter().all(|a| a.parse::<std::net::IpAddr>().is_ok()) {
        return None;
    }
    Some(addrs)
}

impl Window {
    fn refresh_status() -> app::Task<Message> {
        cosmic::task::future(async move {
            cosmic::Action::App(Message::StatusChecked(backend::status().await))
        })
    }

    /// Query GitHub for the latest release tag in the background.
    fn check_release() -> app::Task<Message> {
        cosmic::task::future(async move {
            cosmic::Action::App(Message::ReleaseChecked(updater::latest_release().await))
        })
    }

    /// Download and install the given release tag in the background.
    fn do_self_update(tag: String) -> app::Task<Message> {
        cosmic::task::future(async move {
            let result = updater::self_update(&tag).await;
            cosmic::Action::App(Message::SelfUpdated(tag, result))
        })
    }

    /// Switch the device to `entry` (records it as the toggle's "on" state).
    fn apply_entry(&mut self, index: usize) -> app::Task<Message> {
        let Some(entry) = self.servers.get(index) else {
            return Task::none();
        };
        self.applying = true;
        self.notice = None;
        self.last_custom = Some(entry.name.clone());
        if let Some(cfg) = &self.config
            && let Err(e) = cfg.set(LAST_CUSTOM_KEY, entry.name.clone())
        {
            tracing::warn!("could not persist last-custom: {e}");
        }
        let addresses = entry.addresses.clone();
        cosmic::task::future(async move {
            cosmic::Action::App(Message::Applied(backend::apply(&addresses).await))
        })
    }

    /// Return the device to the router/DHCP-provided DNS.
    fn apply_reset(&mut self) -> app::Task<Message> {
        self.applying = true;
        self.notice = None;
        cosmic::task::future(async move {
            cosmic::Action::App(Message::Applied(backend::reset().await))
        })
    }

    /// Persist the current server list.
    fn save_servers(&self) {
        if let Some(cfg) = &self.config
            && let Err(e) = cfg.set(SERVERS_KEY, self.servers.clone())
        {
            tracing::warn!("could not persist servers: {e}");
        }
    }

    /// The saved entry matching the currently forced servers, if any.
    fn active_index(&self) -> Option<usize> {
        let custom = self.current.as_ref()?.custom.as_ref()?;
        self.servers
            .iter()
            .position(|e| same_servers(&e.addresses, custom))
    }

    /// The coloured status badge, sized for the given pixel size.
    fn status_icon(&self, size: u16) -> cosmic::widget::icon::Icon {
        let custom_on = self
            .current
            .as_ref()
            .is_some_and(|s| s.custom.is_some());
        let bytes: &'static [u8] = if custom_on {
            ICON_CUSTOM_SVG
        } else {
            ICON_ROUTER_SVG
        };
        icon::from_svg_bytes(bytes).icon().size(size)
    }

    /// One-line summary of the current state for the popup header.
    fn status_line(&self) -> String {
        if self.applying {
            return "Switching…".to_string();
        }
        match &self.current {
            None => "Reading network state…".to_string(),
            Some(s) => match &s.custom {
                None => "Router default (DHCP)".to_string(),
                Some(servers) => match self.active_index() {
                    Some(i) => format!("Custom: {}", self.servers[i].name),
                    None => format!("Custom: {}", servers.join(", ")),
                },
            },
        }
    }
}

impl cosmic::Application for Window {
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    const APP_ID: &'static str = "com.github.davidboulay.CosmicAppletDNS";

    fn init(core: app::Core, _flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        let config = cosmic_config::Config::new(Self::APP_ID, CONFIG_VERSION).ok();
        let servers = match config.as_ref().and_then(|c| c.get::<Vec<DnsEntry>>(SERVERS_KEY).ok()) {
            Some(list) => list,
            None => preset_entries(), // first run
        };
        let last_custom = config
            .as_ref()
            .and_then(|c| c.get::<String>(LAST_CUSTOM_KEY).ok())
            .filter(|s| !s.is_empty());
        let imported_initial = config
            .as_ref()
            .and_then(|c| c.get::<bool>(IMPORTED_KEY).ok())
            .unwrap_or(false);
        let auto_update = config
            .as_ref()
            .and_then(|c| c.get::<bool>(AUTO_UPDATE_KEY).ok())
            .unwrap_or(false);
        let last_auto_update = config
            .as_ref()
            .and_then(|c| c.get::<String>(LAST_AUTO_UPDATE_KEY).ok())
            .filter(|s| !s.is_empty());

        let window = Self {
            core,
            popup: None,
            servers,
            current: None,
            applying: false,
            flushing: false,
            notice: None,
            last_custom,
            imported_initial,
            config,
            show_settings: false,
            edit_name: String::new(),
            edit_addresses: String::new(),
            editing: None,
            dragging: None,
            auto_update,
            release: ReleaseStatus::Unknown,
            self_updating: false,
            last_auto_update,
        };
        window.save_servers(); // persist the seeded presets on first run
        let task = Task::batch([Self::refresh_status(), Self::check_release()]);
        (window, task)
    }

    fn core(&self) -> &app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut app::Core {
        &mut self.core
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn subscription(&self) -> Subscription<Message> {
        // Re-read the DNS state periodically so the badge stays truthful when
        // the network changes underneath us (reconnects, VPNs, nmtui edits…).
        fn periodic_status() -> Subscription<Message> {
            const INTERVAL: Duration = Duration::from_secs(2 * 60);
            Subscription::run_with("dns-periodic-status", |_| {
                stream::channel(1, |mut output: mpsc::Sender<Message>| async move {
                    let mut timer = tokio::time::interval(INTERVAL);
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    // The first tick is immediate; skip it since init() already
                    // ran a check on startup.
                    timer.tick().await;
                    loop {
                        timer.tick().await;
                        if output.send(Message::RefreshStatus).await.is_err() {
                            break;
                        }
                    }
                })
            })
        }

        // Periodically check whether a newer applet release is out (and auto-
        // update if the user enabled it). Infrequent since releases are rare.
        fn periodic_release_check() -> Subscription<Message> {
            const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60); // 6 hours
            Subscription::run_with("dns-release-check", |_| {
                stream::channel(1, |mut output: mpsc::Sender<Message>| async move {
                    let mut timer = tokio::time::interval(INTERVAL);
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    // First tick is immediate; skip it since init() already checked.
                    timer.tick().await;
                    loop {
                        timer.tick().await;
                        if output.send(Message::CheckRelease).await.is_err() {
                            break;
                        }
                    }
                })
            })
        }

        Subscription::batch([periodic_status(), periodic_release_check()])
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::TogglePopup => {
                if let Some(p) = self.popup.take() {
                    // Reset to the main view so reopening never lands on Settings.
                    self.show_settings = false;
                    destroy_popup(p)
                } else {
                    let new_id = window::Id::unique();
                    self.popup = Some(new_id);
                    let popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    // Re-read state whenever the popup opens so it never shows
                    // a stale toggle.
                    Task::batch([get_popup(popup_settings), Self::refresh_status()])
                }
            }
            Message::CloseRequested(id) => {
                if Some(id) == self.popup {
                    self.popup = None;
                    // Reset to the main view so reopening never lands on Settings.
                    self.show_settings = false;
                }
                Task::none()
            }
            Message::RefreshStatus => Self::refresh_status(),
            Message::StatusChecked(Ok(state)) => {
                // First run on a device that already had a custom DNS forced
                // (e.g. set by hand before this applet existed): import it as
                // an entry so switching back to it stays one click away.
                if !self.imported_initial {
                    self.imported_initial = true;
                    if let Some(cfg) = &self.config
                        && let Err(e) = cfg.set(IMPORTED_KEY, true)
                    {
                        tracing::warn!("could not persist imported-initial: {e}");
                    }
                    if let Some(custom) = &state.custom
                        && !self
                            .servers
                            .iter()
                            .any(|e| same_servers(&e.addresses, custom))
                    {
                        let entry = DnsEntry {
                            name: format!("Imported ({})", custom.join(", ")),
                            addresses: custom.clone(),
                        };
                        self.last_custom = Some(entry.name.clone());
                        self.servers.insert(0, entry);
                        self.save_servers();
                    }
                }
                self.current = Some(state);
                Task::none()
            }
            Message::StatusChecked(Err(e)) => {
                self.current = None;
                self.notice = Some((true, e));
                Task::none()
            }
            Message::SelectEntry(i) => {
                if self.applying {
                    return Task::none();
                }
                self.apply_entry(i)
            }
            Message::SetCustom(on) => {
                if self.applying {
                    return Task::none();
                }
                if !on {
                    return self.apply_reset();
                }
                // "On" means the entry we last used, or the first saved one.
                let index = self
                    .last_custom
                    .as_deref()
                    .and_then(|name| self.servers.iter().position(|e| e.name == name))
                    .or(if self.servers.is_empty() { None } else { Some(0) });
                match index {
                    Some(i) => self.apply_entry(i),
                    None => {
                        self.notice =
                            Some((true, "Add a DNS server in Settings first".to_string()));
                        Task::none()
                    }
                }
            }
            Message::Applied(result) => {
                self.applying = false;
                match result {
                    Ok(state) => {
                        let what = match &state.custom {
                            Some(_) => "Switched — DNS cache flushed",
                            None => "Back to router DNS — cache flushed",
                        };
                        self.notice = Some((false, what.to_string()));
                        self.current = Some(state);
                    }
                    Err(e) => {
                        self.notice = Some((true, e));
                        // Re-sync: the modify may have half-landed.
                        return Self::refresh_status();
                    }
                }
                Task::none()
            }
            Message::FlushCache => {
                if self.flushing {
                    return Task::none();
                }
                self.flushing = true;
                self.notice = None;
                cosmic::task::future(async move {
                    cosmic::Action::App(Message::Flushed(backend::flush_cache().await))
                })
            }
            Message::Flushed(result) => {
                self.flushing = false;
                self.notice = Some(match result {
                    Ok(()) => (false, "DNS cache flushed".to_string()),
                    Err(e) => (true, e),
                });
                Task::none()
            }
            Message::ToggleSettings => {
                self.show_settings = !self.show_settings;
                if !self.show_settings {
                    // Leaving the panel abandons any in-progress edit/drag.
                    self.editing = None;
                    self.dragging = None;
                    self.edit_name.clear();
                    self.edit_addresses.clear();
                }
                // Refresh the release status when opening the panel.
                if self.show_settings && !matches!(self.release, ReleaseStatus::Checking) {
                    self.release = ReleaseStatus::Checking;
                    return Self::check_release();
                }
                Task::none()
            }
            Message::EditName(s) => {
                self.edit_name = s;
                Task::none()
            }
            Message::EditAddresses(s) => {
                self.edit_addresses = s;
                Task::none()
            }
            Message::AddEntry => {
                let name = self.edit_name.trim().to_string();
                let Some(addresses) = parse_addresses(&self.edit_addresses) else {
                    return Task::none();
                };
                let duplicate = self
                    .servers
                    .iter()
                    .enumerate()
                    .any(|(i, e)| e.name == name && self.editing != Some(i));
                if name.is_empty() || duplicate {
                    return Task::none();
                }
                match self.editing.take() {
                    // Save over the entry loaded for editing.
                    Some(i) if i < self.servers.len() => {
                        // A rename must follow through to what the on/off
                        // toggle re-applies.
                        if self.last_custom.as_deref() == Some(self.servers[i].name.as_str()) {
                            self.last_custom = Some(name.clone());
                            if let Some(cfg) = &self.config
                                && let Err(e) = cfg.set(LAST_CUSTOM_KEY, name.clone())
                            {
                                tracing::warn!("could not persist last-custom: {e}");
                            }
                        }
                        self.servers[i] = DnsEntry { name, addresses };
                    }
                    _ => self.servers.push(DnsEntry { name, addresses }),
                }
                self.save_servers();
                self.edit_name.clear();
                self.edit_addresses.clear();
                Task::none()
            }
            Message::RemoveEntry(i) => {
                if i < self.servers.len() {
                    let removed = self.servers.remove(i);
                    if self.last_custom.as_deref() == Some(removed.name.as_str()) {
                        self.last_custom = None;
                    }
                    match self.editing {
                        Some(e) if e == i => {
                            self.editing = None;
                            self.edit_name.clear();
                            self.edit_addresses.clear();
                        }
                        Some(e) if e > i => self.editing = Some(e - 1),
                        _ => {}
                    }
                    self.dragging = None;
                    self.save_servers();
                }
                Task::none()
            }
            Message::EditEntry(i) => {
                if let Some(entry) = self.servers.get(i) {
                    self.editing = Some(i);
                    self.edit_name = entry.name.clone();
                    self.edit_addresses = entry.addresses.join(", ");
                }
                Task::none()
            }
            Message::CancelEdit => {
                self.editing = None;
                self.edit_name.clear();
                self.edit_addresses.clear();
                Task::none()
            }
            Message::DragStart(i) => {
                if i < self.servers.len() {
                    self.dragging = Some(i);
                }
                Task::none()
            }
            Message::DragOver(to) => {
                if let Some(from) = self.dragging
                    && from != to
                    && from < self.servers.len()
                    && to < self.servers.len()
                {
                    let entry = self.servers.remove(from);
                    self.servers.insert(to, entry);
                    // Keep the edit form pointed at the same entry.
                    if let Some(e) = self.editing {
                        self.editing = Some(if e == from {
                            to
                        } else if from < e && e <= to {
                            e - 1
                        } else if to <= e && e < from {
                            e + 1
                        } else {
                            e
                        });
                    }
                    self.dragging = Some(to);
                }
                Task::none()
            }
            Message::DragEnd => {
                if self.dragging.take().is_some() {
                    self.save_servers();
                }
                Task::none()
            }
            Message::CheckRelease => {
                if matches!(self.release, ReleaseStatus::Checking) || self.self_updating {
                    return Task::none();
                }
                self.release = ReleaseStatus::Checking;
                Self::check_release()
            }
            Message::ReleaseChecked(Ok(tag)) => {
                if updater::is_newer(&tag, updater::CURRENT_VERSION) {
                    self.release = ReleaseStatus::Available(tag.clone());
                    // Auto-install the new version if the user opted in — but
                    // only once per tag. If we already auto-applied this exact
                    // tag and it still looks newer, the release is mis-versioned
                    // (its binary reports an older version than its tag); applying
                    // again would loop forever, so we stop and leave it shown as
                    // available for a manual decision.
                    let already_applied =
                        self.last_auto_update.as_deref() == Some(tag.as_str());
                    if self.auto_update && !self.self_updating && !already_applied {
                        self.self_updating = true;
                        return Self::do_self_update(tag);
                    }
                    if already_applied {
                        tracing::warn!(
                            "release {tag} still reports newer than {} after updating \
                             — skipping auto-update to avoid a loop (mis-versioned release?)",
                            updater::CURRENT_VERSION
                        );
                    }
                } else {
                    self.release = ReleaseStatus::UpToDate;
                }
                Task::none()
            }
            Message::ReleaseChecked(Err(e)) => {
                self.release = ReleaseStatus::Error(e);
                Task::none()
            }
            Message::SetAutoUpdate(on) => {
                self.auto_update = on;
                if let Some(cfg) = &self.config
                    && let Err(e) = cfg.set(AUTO_UPDATE_KEY, on)
                {
                    tracing::warn!("could not persist auto-update setting: {e}");
                }
                // If switching on while an update is already pending, apply it
                // now — unless we already auto-applied that tag (loop guard).
                if on
                    && !self.self_updating
                    && let ReleaseStatus::Available(tag) = &self.release
                    && self.last_auto_update.as_deref() != Some(tag.as_str())
                {
                    let tag = tag.clone();
                    self.self_updating = true;
                    return Self::do_self_update(tag);
                }
                Task::none()
            }
            Message::SelfUpdate(tag) => {
                if self.self_updating {
                    return Task::none();
                }
                self.self_updating = true;
                Self::do_self_update(tag)
            }
            Message::SelfUpdated(tag, Ok(exe)) => {
                // Record the tag we just installed so that if the new binary
                // still reports an older version (mis-versioned release), the
                // next check won't re-apply it and loop.
                self.last_auto_update = Some(tag.clone());
                if let Some(cfg) = &self.config
                    && let Err(e) = cfg.set(LAST_AUTO_UPDATE_KEY, tag)
                {
                    tracing::warn!("could not persist last-auto-update: {e}");
                }
                // The binary has been replaced — now run the new version.
                //
                // Under cosmic-panel, exec-ing ourselves can't re-attach to
                // the panel slot: the panel hands each applet a private
                // Wayland session when *it* spawns them, and that session
                // died with the old process image — the exec'd binary falls
                // back to the regular compositor socket and opens as a
                // floating window while the panel keeps a dead slot. Exit
                // instead: cosmic-panel respawns exited applets, and the
                // respawn runs the replaced binary properly in the panel.
                if std::env::var_os("COSMIC_PANEL_NAME").is_some() {
                    tracing::info!(
                        "self-update installed; exiting so the panel respawns the new version"
                    );
                    // Non-zero on purpose: cosmic-panel respawns applets that
                    // die abnormally but treats a clean exit 0 as an
                    // intentional quit and leaves the slot dead.
                    std::process::exit(1);
                }
                // Standalone (e.g. launched from a terminal): exec in place.
                // This only returns if the exec itself fails.
                let err = updater::relaunch(&exe);
                tracing::error!("relaunch after self-update failed: {err}");
                self.self_updating = false;
                self.release =
                    ReleaseStatus::Error(format!("Updated, but relaunch failed: {err}"));
                Task::none()
            }
            Message::SelfUpdated(_tag, Err(e)) => {
                self.self_updating = false;
                self.release = ReleaseStatus::Error(e);
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let horizontal = matches!(
            self.core.applet.anchor,
            PanelAnchor::Top | PanelAnchor::Bottom
        );

        let suggested = self.core.applet.suggested_size(true);
        let content: Element<'_, Message> = self.status_icon(suggested.0).into();

        // Match stock applets: give the button a fixed cross-axis size and
        // centre the content, so the hover highlight covers the full panel
        // height (rather than just the icon).
        let (_pad_shrinkable, pad_regular) = self.core.applet.suggested_padding(true);
        let button = if horizontal {
            button::custom(container(content).center_y(Length::Fill))
                .height(Length::Fixed((suggested.1 + 2 * pad_regular) as f32))
                .padding([0, pad_regular])
        } else {
            button::custom(container(content).center_x(Length::Fill))
                .width(Length::Fixed((suggested.0 + 2 * pad_regular) as f32))
                .padding([pad_regular, 0])
        }
        .on_press_down(Message::TogglePopup)
        .class(cosmic::theme::Button::AppletIcon);

        autosize::autosize(button, AUTOSIZE_MAIN_ID.clone()).into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        let Spacing {
            space_xxs,
            space_s,
            space_m,
            ..
        } = theme::active().cosmic().spacing;

        if self.show_settings {
            return self.settings_view();
        }

        let custom_on = self
            .current
            .as_ref()
            .is_some_and(|s| s.custom.is_some());

        // Header: badge, title + one-line status, settings gear.
        let header = padded_control(
            row![
                self.status_icon(28),
                column![text::title4("DNS"), text::body(self.status_line())]
                    .spacing(2)
                    .width(Length::Fill),
                button::icon(icon::from_name("emblem-system-symbolic").symbolic(true))
                    .on_press(Message::ToggleSettings),
            ]
            .spacing(space_s)
            .align_y(Alignment::Center),
        );

        // The on/off switch: custom DNS vs. router default.
        let toggle_row = settings::item(
            "Custom DNS",
            toggler(custom_on).on_toggle(Message::SetCustom),
        );

        let mut content = column![header, padded_control(toggle_row)].spacing(space_xxs);
        content = content.push(padded_control(divider::horizontal::default()));

        // Saved servers, active one marked.
        if self.servers.is_empty() {
            content = content.push(
                padded_control(text::caption("No DNS servers saved — add one in Settings."))
                    .padding([space_xxs, space_m]),
            );
        } else {
            let active = self.active_index();
            let mut list = column![].spacing(0);
            for (i, entry) in self.servers.iter().enumerate() {
                let mut item = row![
                    column![
                        text::body(entry.name.clone()),
                        text::caption(entry.addresses.join(", ")),
                    ]
                    .spacing(0)
                    .width(Length::Fill),
                ]
                .spacing(space_s)
                .align_y(Alignment::Center);
                if active == Some(i) {
                    item = item.push(
                        icon::from_name("object-select-symbolic").symbolic(true).size(16),
                    );
                }
                list = list.push(
                    menu_button(item)
                        .on_press_maybe((!self.applying).then_some(Message::SelectEntry(i))),
                );
            }
            content = content.push(
                container(scrollable(list).height(Length::Shrink)).max_height(320.0),
            );
        }

        content = content.push(padded_control(divider::horizontal::default()));

        // Manual cache flush.
        let flush_label = if self.flushing {
            "Flushing…"
        } else {
            "Flush DNS cache"
        };
        content = content.push(
            menu_button(
                row![
                    icon::from_name("view-refresh-symbolic").symbolic(true).size(16),
                    text::body(flush_label),
                ]
                .spacing(space_s)
                .align_y(Alignment::Center),
            )
            .on_press_maybe((!self.flushing).then_some(Message::FlushCache)),
        );

        // Outcome / error line for the last action.
        if let Some((is_error, note)) = &self.notice {
            let class = if *is_error {
                cosmic::theme::Text::Color(theme::active().cosmic().destructive_color().into())
            } else {
                cosmic::theme::Text::Default
            };
            content = content.push(
                padded_control(text::caption(note.clone()).class(class))
                    .padding([space_xxs, space_m]),
            );
        }

        // Which connection all of this applies to — and a heads-up when a VPN
        // is intercepting DNS so a switch here wouldn't take effect anyway.
        if let Some(state) = &self.current {
            if let Some(owner) = &state.dns_owner {
                content = content.push(
                    padded_control(text::caption(format!(
                        "Heads up: “{owner}” (VPN/tunnel) is answering DNS right now — \
                         switches apply once it disconnects.",
                    )))
                    .padding([space_xxs, space_m]),
                );
            }
            content = content.push(
                padded_control(text::caption(format!(
                    "Network: {} ({})",
                    state.connection, state.device
                )))
                .padding([space_xxs, space_m]),
            );
        }

        self.core
            .applet
            .popup_container(
                container(content.spacing(space_xxs).padding([space_s, 0]))
                    .width(Length::Fixed(360.0)),
            )
            .into()
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }
}

impl Window {
    /// The settings panel: manage the saved server list, plus the applet's
    /// own version / self-update controls.
    fn settings_view(&self) -> Element<'_, Message> {
        let Spacing {
            space_xxs,
            space_s,
            space_m,
            ..
        } = theme::active().cosmic().spacing;

        // Header with a back button.
        let header = padded_control(
            row![
                button::icon(icon::from_name("go-previous-symbolic").symbolic(true))
                    .on_press(Message::ToggleSettings),
                text::title4("Settings"),
            ]
            .spacing(space_s)
            .align_y(Alignment::Center),
        );

        // --- Saved servers (drag a row by its handle/background to reorder;
        // press on the edit/trash buttons is captured by them, so it doesn't
        // start a drag) ---
        let mut rows = column![].spacing(2);
        for (i, entry) in self.servers.iter().enumerate() {
            let content = row![
                icon::from_name("list-drag-handle-symbolic").symbolic(true).size(16),
                column![
                    text::body(entry.name.clone()),
                    text::caption(entry.addresses.join(", ")),
                ]
                .spacing(0)
                .width(Length::Fill),
                button::icon(icon::from_name("document-edit-symbolic").symbolic(true))
                    .on_press(Message::EditEntry(i)),
                button::icon(icon::from_name("user-trash-symbolic").symbolic(true))
                    .on_press(Message::RemoveEntry(i)),
            ]
            .spacing(space_s)
            .align_y(Alignment::Center);
            // Lift the row visually while it's being dragged.
            let styled = container(content).padding([space_xxs, space_xxs]).class(
                if self.dragging == Some(i) {
                    cosmic::theme::Container::Card
                } else {
                    cosmic::theme::Container::Transparent
                },
            );
            rows = rows.push(
                mouse_area(styled)
                    .on_press(Message::DragStart(i))
                    .on_enter(Message::DragOver(i))
                    .on_release(Message::DragEnd),
            );
        }
        // Commit the new order when the button is released anywhere over the
        // list (including between rows) or the pointer leaves it mid-drag.
        let list = mouse_area(rows)
            .on_release(Message::DragEnd)
            .on_exit(Message::DragEnd);

        // --- Add a server / edit the selected one (same form) ---
        let editing_name = self.editing.and_then(|i| self.servers.get(i)).map(|e| &e.name);
        let name_input = text_input("Name (e.g. Pi-hole)", &self.edit_name)
            .on_input(Message::EditName)
            .width(Length::Fill);
        let addr_input = text_input("Addresses (e.g. 1.1.1.1, 1.0.0.1)", &self.edit_addresses)
            .on_input(Message::EditAddresses)
            .width(Length::Fill);
        let name = self.edit_name.trim();
        let can_add = !name.is_empty()
            && !self
                .servers
                .iter()
                .enumerate()
                .any(|(i, e)| e.name == name && self.editing != Some(i))
            && parse_addresses(&self.edit_addresses).is_some();
        let mut form_buttons = row![
            button::suggested(if self.editing.is_some() { "Save changes" } else { "Add server" })
                .on_press_maybe(can_add.then_some(Message::AddEntry))
                .width(Length::Fill),
        ]
        .spacing(space_s);
        if self.editing.is_some() {
            form_buttons = form_buttons.push(
                button::standard("Cancel")
                    .on_press(Message::CancelEdit)
                    .width(Length::Fill),
            );
        }
        let form_heading = match editing_name {
            Some(n) => format!("Edit “{n}”"),
            None => "Add a server".to_string(),
        };

        // --- Applet self-update ---
        let version_row = settings::item("Version", text::body(updater::CURRENT_VERSION));

        // Manual "check GitHub" button (disabled mid-check / mid-update).
        let busy = matches!(self.release, ReleaseStatus::Checking) || self.self_updating;
        let check_button = button::standard("Check for new version")
            .leading_icon(icon::from_name("software-update-available-symbolic").symbolic(true))
            .on_press_maybe((!busy).then_some(Message::CheckRelease))
            .width(Length::Fill);

        // Status line + (when an update is available) an "Update now" action.
        let (status, update_now): (String, Option<Element<'_, Message>>) = match &self.release {
            ReleaseStatus::Unknown => ("Not checked yet".to_string(), None),
            ReleaseStatus::Checking => ("Checking GitHub…".to_string(), None),
            ReleaseStatus::UpToDate => {
                (format!("Up to date (v{})", updater::CURRENT_VERSION), None)
            }
            ReleaseStatus::Available(tag) => (
                format!("{tag} is available"),
                (!self.self_updating).then(|| {
                    button::suggested("Update now")
                        .on_press(Message::SelfUpdate(tag.clone()))
                        .into()
                }),
            ),
            ReleaseStatus::Error(e) => (format!("Check failed: {e}"), None),
        };

        let status_class = if matches!(self.release, ReleaseStatus::Error(_)) {
            cosmic::theme::Text::Color(theme::active().cosmic().destructive_color().into())
        } else {
            cosmic::theme::Text::Default
        };
        let mut status_col = column![text::caption(status).class(status_class)].spacing(space_xxs);
        if self.self_updating {
            status_col = status_col.push(text::caption("Downloading and installing…"));
        }
        if let Some(action) = update_now {
            status_col = status_col.push(action);
        }

        // Auto-update toggle.
        let auto_row = settings::item(
            "Automatically update the applet",
            toggler(self.auto_update).on_toggle(Message::SetAutoUpdate),
        );

        let content = column![
            header,
            padded_control(text::heading("DNS servers")),
            padded_control(text::caption(
                "The list you can switch between from the panel — drag rows to \
                 reorder. Changes apply to the connection with the default route.",
            ))
            .padding([0, space_m]),
            padded_control(list),
            padded_control(text::heading(form_heading)),
            padded_control(name_input),
            padded_control(addr_input),
            padded_control(form_buttons),
            padded_control(divider::horizontal::default()),
            padded_control(text::heading("Applet")),
            padded_control(text::caption(
                "Updates for the applet itself, fetched from GitHub releases.",
            ))
            .padding([0, space_m]),
            padded_control(version_row),
            padded_control(check_button),
            padded_control(status_col).padding([space_xxs, space_m]),
            padded_control(auto_row),
        ]
        .spacing(space_xxs);

        self.core
            .applet
            .popup_container(
                container(
                    scrollable(content.spacing(space_xxs).padding([space_s, 0]))
                        .height(Length::Shrink),
                )
                .width(Length::Fixed(360.0))
                .max_height(560.0),
            )
            .into()
    }
}
