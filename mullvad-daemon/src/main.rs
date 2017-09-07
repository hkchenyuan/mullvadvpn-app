//! # License
//!
//! Copyright (C) 2017  Amagicom AB
//!
//! This program is free software: you can redistribute it and/or modify it under the terms of the
//! GNU General Public License as published by the Free Software Foundation, either version 3 of
//! the License, or (at your option) any later version.

#[macro_use]
extern crate clap;
extern crate chrono;
#[macro_use]
extern crate log;
#[macro_use]
extern crate error_chain;
extern crate fern;
extern crate futures;

extern crate serde;
#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate jsonrpc_client_core;
extern crate jsonrpc_client_http;
extern crate jsonrpc_core;
extern crate jsonrpc_pubsub;
#[macro_use]
extern crate jsonrpc_macros;
extern crate jsonrpc_ws_server;
extern crate uuid;
#[macro_use]
extern crate lazy_static;

extern crate mullvad_types;
extern crate talpid_core;
extern crate talpid_ipc;

mod cli;
mod management_interface;
mod master;
mod rpc_info;
mod settings;
mod shutdown;


use error_chain::ChainedError;
use futures::Future;
use jsonrpc_client_http::HttpHandle;
use jsonrpc_core::futures::sync::oneshot::Sender as OneshotSender;
use management_interface::{BoxFuture, ManagementInterfaceServer, TunnelCommand};
use master::AccountsProxy;
use mullvad_types::account::{AccountData, AccountToken};
use mullvad_types::relay_endpoint::RelayEndpoint;
use mullvad_types::states::{DaemonState, SecurityState, TargetState};

use std::io;
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use talpid_core::firewall::{Firewall, FirewallProxy, SecurityPolicy};
use talpid_core::mpsc::IntoSender;
use talpid_core::net::{Endpoint, TransportProtocol};
use talpid_core::tunnel::{self, TunnelEvent, TunnelMonitor};

error_chain!{
    errors {
        /// The client is in the wrong state for the requested operation. Optimally the code should
        /// be written in such a way so such states can't exist.
        InvalidState {
            description("Client is in an invalid state for the requested operation")
        }
        TunnelError(msg: &'static str) {
            description("Error in the tunnel monitor")
            display("Tunnel monitor error: {}", msg)
        }
        ManagementInterfaceError(msg: &'static str) {
            description("Error in the management interface")
            display("Management interface error: {}", msg)
        }
        FirewallError {
            description("Firewall error")
        }
        InvalidSettings(msg: &'static str) {
            description("Invalid settings")
            display("Invalid Settings: {}", msg)
        }
        NoRelay {
            description("Found no valid relays to connect to")
        }
    }
}

lazy_static! {
    // Temporary store of hardcoded relays.
    static ref RELAYS: [Endpoint; 3] = [
        // se5.mullvad.net
        Endpoint::new(Ipv4Addr::new(193, 138, 219, 240), 1300, TransportProtocol::Udp),
        // se6.mullvad.net
        Endpoint::new(Ipv4Addr::new(193, 138, 219, 241), 1300, TransportProtocol::Udp),
        // se7.mullvad.net
        Endpoint::new(Ipv4Addr::new(185, 65, 132, 104), 1300, TransportProtocol::Udp),
    ];
}

const CRATE_NAME: &str = "mullvadd";


/// All events that can happen in the daemon. Sent from various threads and exposed interfaces.
pub enum DaemonEvent {
    /// An event coming from the tunnel software to indicate a change in state.
    TunnelEvent(TunnelEvent),
    /// Triggered by the thread waiting for the tunnel process. Means the tunnel process exited.
    TunnelExited(tunnel::Result<()>),
    /// Triggered by the thread waiting for a tunnel close operation to complete. Contains the
    /// result of trying to kill the tunnel.
    TunnelKillResult(io::Result<()>),
    /// An event coming from the JSONRPC-2.0 management interface.
    ManagementInterfaceEvent(TunnelCommand),
    /// Triggered if the server hosting the JSONRPC-2.0 management interface dies unexpectedly.
    ManagementInterfaceExited(talpid_ipc::Result<()>),
    /// Daemon shutdown triggered by a signal, ctrl-c or similar.
    TriggerShutdown,
}

impl From<TunnelEvent> for DaemonEvent {
    fn from(tunnel_event: TunnelEvent) -> Self {
        DaemonEvent::TunnelEvent(tunnel_event)
    }
}

impl From<TunnelCommand> for DaemonEvent {
    fn from(tunnel_command: TunnelCommand) -> Self {
        DaemonEvent::ManagementInterfaceEvent(tunnel_command)
    }
}

/// Represents the internal state of the actual tunnel.
// TODO(linus): Put the tunnel::CloseHandle into this state, so it can't exist when not running.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TunnelState {
    /// No tunnel is running.
    NotRunning,
    /// The tunnel has been started, but it is not established/functional.
    Connecting,
    /// The tunnel is up and working.
    Connected,
    /// This state is active from when we manually trigger a tunnel kill until the tunnel wait
    /// operation (TunnelExit) returned.
    Exiting,
}

impl TunnelState {
    pub fn as_security_state(&self) -> SecurityState {
        match *self {
            TunnelState::Connected => SecurityState::Secured,
            _ => SecurityState::Unsecured,
        }
    }
}


struct Daemon {
    state: TunnelState,
    // The tunnel_close_handle must only exist in the Connecting and Connected states!
    tunnel_close_handle: Option<tunnel::CloseHandle>,
    last_broadcasted_state: DaemonState,
    target_state: TargetState,
    shutdown: bool,
    rx: mpsc::Receiver<DaemonEvent>,
    tx: mpsc::Sender<DaemonEvent>,
    management_interface_broadcaster: management_interface::EventBroadcaster,
    settings: settings::Settings,
    accounts_proxy: AccountsProxy<HttpHandle>,
    firewall: FirewallProxy,
    relay_endpoint: Option<Endpoint>,
    tunnel_interface: Option<String>,

    // Just for testing. A cyclic iterator iterating over the hardcoded relays,
    // picking a new one for each retry.
    relay_iter: std::iter::Cycle<std::iter::Cloned<std::slice::Iter<'static, Endpoint>>>,
}

impl Daemon {
    pub fn new() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let management_interface_broadcaster = Self::start_management_interface(tx.clone())?;
        let state = TunnelState::NotRunning;
        let target_state = TargetState::Unsecured;
        Ok(
            Daemon {
                state,
                tunnel_close_handle: None,
                target_state,
                last_broadcasted_state: DaemonState {
                    state: state.as_security_state(),
                    target_state,
                },
                shutdown: false,
                rx,
                tx,
                management_interface_broadcaster,
                settings: settings::Settings::load().chain_err(|| "Unable to read settings")?,
                accounts_proxy: master::create_account_proxy()
                    .chain_err(|| "Unable to bootstrap RPC client")?,
                firewall: FirewallProxy::new().chain_err(|| ErrorKind::FirewallError)?,
                relay_endpoint: None,
                tunnel_interface: None,
                relay_iter: RELAYS.iter().cloned().cycle(),
            },
        )
    }

    // Starts the management interface and spawns a thread that will process it.
    // Returns a handle that allows notifying all subscribers on events.
    fn start_management_interface(event_tx: mpsc::Sender<DaemonEvent>)
                                  -> Result<management_interface::EventBroadcaster> {
        let multiplex_event_tx = IntoSender::from(event_tx.clone());
        let server = Self::start_management_interface_server(multiplex_event_tx)?;
        let event_broadcaster = server.event_broadcaster();
        Self::spawn_management_interface_wait_thread(server, event_tx);
        Ok(event_broadcaster)
    }

    fn start_management_interface_server(event_tx: IntoSender<TunnelCommand, DaemonEvent>)
                                         -> Result<ManagementInterfaceServer> {
        let server =
            ManagementInterfaceServer::start(event_tx)
                .chain_err(|| ErrorKind::ManagementInterfaceError("Failed to start server"),)?;
        info!(
            "Mullvad management interface listening on {}",
            server.address()
        );
        rpc_info::write(server.address()).chain_err(|| ErrorKind::ManagementInterfaceError(
                "Failed to write RPC address to file"))?;
        Ok(server)
    }

    fn spawn_management_interface_wait_thread(server: ManagementInterfaceServer,
                                              exit_tx: mpsc::Sender<DaemonEvent>) {
        thread::spawn(
            move || {
                let result = server.wait();
                debug!("Mullvad management interface shut down");
                let _ = exit_tx.send(DaemonEvent::ManagementInterfaceExited(result));
            },
        );
    }

    /// Consume the `Daemon` and run the main event loop. Blocks until an error happens or a
    /// shutdown event is received.
    pub fn run(mut self) -> Result<()> {
        while let Ok(event) = self.rx.recv() {
            self.handle_event(event)?;
            if self.shutdown && self.state == TunnelState::NotRunning {
                break;
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, event: DaemonEvent) -> Result<()> {
        use DaemonEvent::*;
        match event {
            TunnelEvent(event) => self.handle_tunnel_event(event),
            TunnelExited(result) => self.handle_tunnel_exited(result),
            TunnelKillResult(result) => self.handle_tunnel_kill_result(result),
            ManagementInterfaceEvent(event) => self.handle_management_interface_event(event),
            ManagementInterfaceExited(result) => self.handle_management_interface_exited(result),
            TriggerShutdown => self.handle_trigger_shutdown_event(),
        }
    }

    fn handle_tunnel_event(&mut self, tunnel_event: TunnelEvent) -> Result<()> {
        debug!("Tunnel event: {:?}", tunnel_event);
        if self.state == TunnelState::Connecting {
            if let TunnelEvent::Up { tunnel_interface } = tunnel_event {
                self.tunnel_interface = Some(tunnel_interface);
                self.set_security_policy()?;
                self.set_state(TunnelState::Connected)
            } else {
                Ok(())
            }
        } else if self.state == TunnelState::Connected && tunnel_event == TunnelEvent::Down {
            self.kill_tunnel()
        } else {
            Ok(())
        }
    }

    fn handle_tunnel_exited(&mut self, result: tunnel::Result<()>) -> Result<()> {
        if let Err(e) = result.chain_err(|| "Tunnel exited in an unexpected way") {
            error!("{}", e.display_chain());
        }
        self.relay_endpoint = None;
        self.tunnel_interface = None;
        self.reset_security_policy()?;
        self.tunnel_close_handle = None;
        self.set_state(TunnelState::NotRunning)
    }

    fn handle_tunnel_kill_result(&mut self, result: io::Result<()>) -> Result<()> {
        result.chain_err(|| "Error while trying to close tunnel")
    }

    fn handle_management_interface_event(&mut self, event: TunnelCommand) -> Result<()> {
        use TunnelCommand::*;
        match event {
            SetTargetState(state) => self.on_set_target_state(state),
            GetState(tx) => Ok(self.on_get_state(tx)),
            GetAccountData(tx, account_token) => Ok(self.on_get_account_data(tx, account_token)),
            SetAccount(tx, account_token) => self.on_set_account(tx, account_token),
            GetAccount(tx) => Ok(self.on_get_account(tx)),
            SetCustomRelay(tx, relay_endpoint) => self.on_set_custom_relay(tx, relay_endpoint),
        }
    }

    fn on_set_target_state(&mut self, new_target_state: TargetState) -> Result<()> {
        if !self.shutdown {
            self.set_target_state(new_target_state)
        } else {
            warn!("Ignoring target state change request due to shutdown");
            Ok(())
        }
    }

    fn on_get_state(&self, tx: OneshotSender<DaemonState>) {
        Self::oneshot_send(tx, self.last_broadcasted_state, "current state");
    }

    fn on_get_account_data(&mut self,
                           tx: OneshotSender<BoxFuture<AccountData, jsonrpc_client_core::Error>>,
                           account_token: AccountToken) {
        let rpc_call = self.accounts_proxy
            .get_expiry(account_token)
            .map(|expiry| AccountData { expiry });
        Self::oneshot_send(tx, Box::new(rpc_call), "account data")
    }


    fn on_set_account(&mut self,
                      tx: OneshotSender<()>,
                      account_token: Option<String>)
                      -> Result<()> {

        let save_result = self.settings.set_account_token(account_token);

        match save_result.chain_err(|| "Unable to save settings") {
            Ok(account_changed) => {
                Self::oneshot_send(tx, (), "set_account response");
                let tunnel_needs_restart = self.state == TunnelState::Connecting ||
                                           self.state == TunnelState::Connected;
                if account_changed && tunnel_needs_restart {
                    info!("Initiating tunnel restart because the account token changed");
                    self.kill_tunnel()?;
                }
            }
            Err(e) => error!("{}", e.display_chain()),
        }
        Ok(())
    }

    fn on_get_account(&self, tx: OneshotSender<Option<String>>) {
        Self::oneshot_send(tx, self.settings.get_account_token(), "current account")
    }

    fn on_set_custom_relay(&mut self,
                           tx: OneshotSender<()>,
                           relay_endpoint: RelayEndpoint)
                           -> Result<()> {

        let save_result = self.settings.set_custom_relay(Some(relay_endpoint));
        match save_result.chain_err(|| "Unable to save settings") {
            Ok(servers_changed) => {
                Self::oneshot_send(tx, (), "set_custom_relay response");

                let tunnel_needs_restart = self.state == TunnelState::Connecting ||
                                           self.state == TunnelState::Connected;

                if servers_changed && tunnel_needs_restart {
                    info!("Initiating tunnel restart because a custom relay was selected");
                    self.kill_tunnel()?;
                }
            }
            Err(e) => error!("{}", e.display_chain()),
        }

        Ok(())
    }

    fn oneshot_send<T>(tx: OneshotSender<T>, t: T, msg: &'static str) {
        if let Err(_) = tx.send(t) {
            warn!("Unable to send {} to management interface client", msg);
        }
    }

    fn handle_management_interface_exited(&self, result: talpid_ipc::Result<()>) -> Result<()> {
        let error = ErrorKind::ManagementInterfaceError("Server exited unexpectedly");
        match result {
            Ok(()) => Err(error.into()),
            Err(e) => Err(e).chain_err(|| error),
        }
    }

    fn handle_trigger_shutdown_event(&mut self) -> Result<()> {
        self.shutdown = true;
        self.set_target_state(TargetState::Unsecured)
    }

    /// Update the state of the client. If it changed, notify the subscribers and trigger
    /// appropriate actions.
    fn set_state(&mut self, new_state: TunnelState) -> Result<()> {
        if new_state != self.state {
            debug!("State {:?} => {:?}", self.state, new_state);
            self.state = new_state;
            self.broadcast_state();
            self.verify_state_consistency()?;
            self.apply_target_state()
        } else {
            // Calling set_state with the same state we already have is an error. Should try to
            // mitigate this possibility completely with a better state machine later.
            Err(ErrorKind::InvalidState.into())
        }
    }

    fn broadcast_state(&mut self) {
        let new_daemon_state = DaemonState {
            state: self.state.as_security_state(),
            target_state: self.target_state,
        };
        if self.last_broadcasted_state != new_daemon_state {
            self.last_broadcasted_state = new_daemon_state;
            self.management_interface_broadcaster.notify_new_state(new_daemon_state);
        }
    }

    // Check that the current state is valid and consistent.
    fn verify_state_consistency(&self) -> Result<()> {
        use TunnelState::*;
        ensure!(
            match self.state {
                NotRunning => self.tunnel_close_handle.is_none(),
                Connecting => self.tunnel_close_handle.is_some(),
                Connected => self.tunnel_close_handle.is_some(),
                Exiting => self.tunnel_close_handle.is_none(),
            },
            ErrorKind::InvalidState
        );
        Ok(())
    }

    /// Set the target state of the client. If it changed trigger the operations needed to progress
    /// towards that state.
    fn set_target_state(&mut self, new_state: TargetState) -> Result<()> {
        if new_state != self.target_state {
            debug!("Target state {:?} => {:?}", self.target_state, new_state);
            self.target_state = new_state;
            self.broadcast_state();
            self.apply_target_state()
        } else {
            Ok(())
        }
    }

    fn apply_target_state(&mut self) -> Result<()> {
        match (self.target_state, self.state) {
            (TargetState::Secured, TunnelState::NotRunning) => {
                debug!("Triggering tunnel start");
                if let Err(e) = self.start_tunnel().chain_err(|| "Failed to start tunnel") {
                    error!("{}", e.display_chain());
                    self.relay_endpoint = None;
                    self.reset_security_policy()?;
                    self.management_interface_broadcaster.notify_error(&e);
                    self.set_target_state(TargetState::Unsecured)?;
                }
                Ok(())
            }
            (TargetState::Unsecured, TunnelState::Connecting) |
            (TargetState::Unsecured, TunnelState::Connected) => self.kill_tunnel(),
            (..) => Ok(()),
        }
    }

    fn start_tunnel(&mut self) -> Result<()> {
        ensure!(
            self.state == TunnelState::NotRunning,
            ErrorKind::InvalidState
        );

        let relay = self.get_relay()
            .chain_err(|| ErrorKind::NoRelay)?;

        let account_token = self.settings
            .get_account_token()
            .ok_or(ErrorKind::InvalidSettings("No account token"))?;

        self.relay_endpoint = Some(relay);
        self.set_security_policy()?;

        let tunnel_monitor = self.spawn_tunnel_monitor(relay, &account_token)?;
        self.tunnel_close_handle = Some(tunnel_monitor.close_handle());
        self.spawn_tunnel_monitor_wait_thread(tunnel_monitor);

        self.set_state(TunnelState::Connecting)?;
        Ok(())
    }

    fn get_relay(&mut self) -> Result<Endpoint> {
        if let Some(relay_endpoint) = self.settings.get_custom_relay() {
            self.parse_custom_relay(&relay_endpoint)
        } else {
            Ok(self.relay_iter.next().unwrap())
        }
    }

    fn parse_custom_relay(&self, relay_endpoint: &RelayEndpoint) -> Result<Endpoint> {
        let socket_addrs: Vec<SocketAddr> =
            format!("{}:{}", &relay_endpoint.host, relay_endpoint.port)
                .to_socket_addrs()
                .chain_err(
                    || {
                        format!(
                            "Invalid custom server host identifier: {}",
                            relay_endpoint.host
                        )
                    },
                )?
                .collect();

        if socket_addrs.len() == 0 {
            bail!("Unable to resolve {}", relay_endpoint.host)
        } else {

            let socket_addr = socket_addrs[0];

            if socket_addrs.len() > 1 {
                info!(
                    "{} resolved to more than one IP, ignoring all but {}",
                    relay_endpoint.host,
                    socket_addr.ip()
                )
            }

            let protocol = TransportProtocol::from_str(&relay_endpoint.protocol)
                .map_err(
                    |_| {
                        format!(
                            "Invalid custom server protocol: {}",
                            relay_endpoint.protocol
                        )
                    },
                )?;

            Ok(Endpoint::new(socket_addr.ip(), socket_addr.port(), protocol),)
        }
    }

    fn spawn_tunnel_monitor(&self, relay: Endpoint, account_token: &str) -> Result<TunnelMonitor> {
        // Must wrap the channel in a Mutex because TunnelMonitor forces the closure to be Sync
        let event_tx = Arc::new(Mutex::new(self.tx.clone()));
        let on_tunnel_event = move |event| {
            let _ = event_tx.lock().unwrap().send(DaemonEvent::TunnelEvent(event));
        };
        TunnelMonitor::new(relay, account_token, on_tunnel_event)
            .chain_err(|| ErrorKind::TunnelError("Unable to start tunnel monitor"))
    }

    fn spawn_tunnel_monitor_wait_thread(&self, tunnel_monitor: TunnelMonitor) {
        let error_tx = self.tx.clone();
        thread::spawn(
            move || {
                let result = tunnel_monitor.wait();
                let _ = error_tx.send(DaemonEvent::TunnelExited(result));
                trace!("Tunnel monitor thread exit");
            },
        );
    }

    fn kill_tunnel(&mut self) -> Result<()> {
        ensure!(
            self.state == TunnelState::Connecting || self.state == TunnelState::Connected,
            ErrorKind::InvalidState
        );
        let close_handle = self.tunnel_close_handle.take().unwrap();
        self.set_state(TunnelState::Exiting)?;
        let result_tx = self.tx.clone();
        thread::spawn(
            move || {
                let result = close_handle.close();
                let _ = result_tx.send(DaemonEvent::TunnelKillResult(result));
                trace!("Tunnel kill thread exit");
            },
        );
        Ok(())
    }

    pub fn shutdown_handle(&self) -> DaemonShutdownHandle {
        DaemonShutdownHandle { tx: self.tx.clone() }
    }

    fn set_security_policy(&mut self) -> Result<()> {
        let policy = match (self.relay_endpoint, self.tunnel_interface.as_ref()) {
            (Some(relay), None) => SecurityPolicy::Connecting(relay),
            (Some(relay), Some(interface)) => SecurityPolicy::Connected(relay, interface.clone()),
            _ => bail!(ErrorKind::InvalidState),
        };
        debug!("Set security policy: {:?}", policy);
        self.firewall.apply_policy(policy).chain_err(|| ErrorKind::FirewallError)
    }

    fn reset_security_policy(&mut self) -> Result<()> {
        debug!("Reset security policy");
        self.firewall.reset_policy().chain_err(|| ErrorKind::FirewallError)
    }
}

struct DaemonShutdownHandle {
    tx: mpsc::Sender<DaemonEvent>,
}

impl DaemonShutdownHandle {
    pub fn shutdown(&self) {
        let _ = self.tx.send(DaemonEvent::TriggerShutdown);
    }
}

impl Drop for Daemon {
    fn drop(self: &mut Daemon) {
        if let Err(e) = rpc_info::remove().chain_err(|| "Unable to clean up rpc address file") {
            error!("{}", e.display_chain());
        }
    }
}


quick_main!(run);

fn run() -> Result<()> {
    let config = cli::get_config();
    init_logger(config.log_level, config.log_file.as_ref())?;

    let daemon = Daemon::new().chain_err(|| "Unable to initialize daemon")?;

    let shutdown_handle = daemon.shutdown_handle();
    shutdown::set_shutdown_signal_handler(move || shutdown_handle.shutdown())
        .chain_err(|| "Unable to attach shutdown signal handler")?;

    daemon.run()?;

    debug!("Mullvad daemon is quitting");
    Ok(())
}

fn init_logger(log_level: log::LogLevelFilter, log_file: Option<&PathBuf>) -> Result<()> {
    let silenced_crates = [
        "jsonrpc_core",
        "tokio_core",
        "tokio_proto",
        "jsonrpc_ws_server",
        "ws",
        "mio",
        "hyper",
    ];
    let mut config = fern::Dispatch::new()
        .format(
            |out, message, record| {
                out.finish(
                    format_args!(
                        "{}[{}][{}] {}",
                        chrono::Local::now().format("[%Y-%m-%d %H:%M:%S%.3f]"),
                        record.target(),
                        record.level(),
                        message
                    ),
                )
            },
        )
        .level(log_level)
        .chain(std::io::stdout());
    for silenced_crate in &silenced_crates {
        config = config.level_for(*silenced_crate, log::LogLevelFilter::Warn);
    }
    if let Some(ref log_file) = log_file {
        let f = fern::log_file(log_file).chain_err(|| "Failed to open log file for writing")?;
        config = config.chain(f);
    }
    config.apply().chain_err(|| "Failed to bootstrap logging system")
}
