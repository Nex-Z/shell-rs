use std::sync::{Arc, Mutex, mpsc};

use anyhow::Result;
use async_channel::Sender;

use super::{LocalShell, TerminalSize};
use crate::connection::Latency;
use crate::connection::{ConnectionPrompt, ConnectionPromptReply};
use crate::host::{HostLogin, HostOs};

/// Commands accepted by every terminal byte-stream transport.
pub enum TerminalTransportCommand {
    Write(Vec<u8>),
    Resize(TerminalSize),
    PromptReply {
        request_id: u64,
        reply: ConnectionPromptReply,
    },
    /// Run a command beside the shell. Only a transport with a connection of
    /// its own runs it; the others drop the request.
    Exec(ExecRequest),
    Shutdown,
}

/// What a command run beside the shell printed, or why it did not run.
pub type ExecResult = Result<String, String>;

/// A command to run beside the shell, on the terminal's own SSH connection:
/// how the right sidebar's tools read the host, with no login of their own.
/// The command gets no terminal and no input.
///
/// Answered once on `reply` with the command's standard output, or with
/// why it could not run. A transport without a connection (a local PTY)
/// drops the request, which the asker sees as the reply channel closing.
#[non_exhaustive]
pub struct ExecRequest {
    pub command: String,
    pub reply: mpsc::Sender<ExecResult>,
    /// Tighter bounds for optional telemetry; ordinary commands use the transport defaults.
    pub(crate) limits: Option<(std::time::Duration, usize)>,
    /// Cancelled telemetry must not run after its panel was hidden.
    pub(crate) cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl std::fmt::Debug for TerminalTransportCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Write(bytes) => formatter.debug_tuple("Write").field(&bytes.len()).finish(),
            Self::Resize(size) => formatter.debug_tuple("Resize").field(size).finish(),
            Self::PromptReply { request_id, reply } => formatter
                .debug_struct("PromptReply")
                .field("request_id", request_id)
                .field("reply", reply)
                .finish(),
            Self::Exec(request) => formatter
                .debug_tuple("Exec")
                .field(&request.command.len())
                .finish(),
            Self::Shutdown => formatter.write_str("Shutdown"),
        }
    }
}

/// Events produced by a terminal byte-stream transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalTransportEvent {
    Started,
    Output(Vec<u8>),
    Prompt(ConnectionPrompt),
    /// What the transport found running on the host. Sent once per successful
    /// connection, after the shell is up.
    HostOsDetected(HostOs),
    /// A round trip measured on the live connection. Sent periodically while
    /// the shell runs; transports without a network hop never send it.
    Latency(Latency),
    Exited {
        code: u32,
        signal: Option<String>,
    },
    Failed(String),
}

/// One running transport. Implementations may block because ShellRS always
/// invokes this method on a dedicated worker thread.
pub trait TerminalTransport: Send + 'static {
    fn run(
        self: Box<Self>,
        initial_size: TerminalSize,
        commands: mpsc::Receiver<TerminalTransportCommand>,
        events: Sender<TerminalTransportEvent>,
    ) -> Result<()>;
}

/// Creates a fresh one-shot transport for initial launch and restart.
pub trait TerminalTransportFactory: Send + Sync + 'static {
    fn create(&self) -> Box<dyn TerminalTransport>;
}

/// Creates a fresh remote factory from the latest saved login of a host.
/// Keeping this boundary at the workspace makes reconnects pick up edits
/// immediately and lets UI tests inject a deterministic fake.
pub trait RemoteTerminalTransportProvider: Send + Sync + 'static {
    fn factory_for(&self, login: &HostLogin) -> SharedTerminalTransportFactory;
}

pub type SharedRemoteTerminalTransportProvider = Arc<dyn RemoteTerminalTransportProvider>;

/// Creates the factory of a local terminal for the shell the settings name,
/// asked when it opens and when it restarts, so a changed setting reaches
/// the next one. Like the remote provider, the workspace owns this boundary
/// and UI tests inject a fake.
pub trait LocalTerminalTransportProvider: Send + Sync + 'static {
    fn factory_for(&self, shell: &LocalShell) -> SharedTerminalTransportFactory;
}

pub type SharedLocalTerminalTransportProvider = Arc<dyn LocalTerminalTransportProvider>;

/// For tests: every local terminal gets the same factory, and the shells
/// asked for are kept.
pub struct FixedLocalTerminalTransportProvider {
    factory: SharedTerminalTransportFactory,
    shells: Mutex<Vec<LocalShell>>,
}

impl FixedLocalTerminalTransportProvider {
    pub fn new(factory: SharedTerminalTransportFactory) -> Self {
        Self {
            factory,
            shells: Default::default(),
        }
    }

    /// The shell of each local terminal started, in order.
    pub fn shells(&self) -> Vec<LocalShell> {
        self.shells
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

impl LocalTerminalTransportProvider for FixedLocalTerminalTransportProvider {
    fn factory_for(&self, shell: &LocalShell) -> SharedTerminalTransportFactory {
        self.shells
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(shell.clone());
        self.factory.clone()
    }
}

/// Provider useful for tests and embedders that want every host to use the
/// same factory.
pub struct FixedRemoteTerminalTransportProvider {
    factory: SharedTerminalTransportFactory,
}

impl FixedRemoteTerminalTransportProvider {
    pub fn new(factory: SharedTerminalTransportFactory) -> Self {
        Self { factory }
    }
}

impl RemoteTerminalTransportProvider for FixedRemoteTerminalTransportProvider {
    fn factory_for(&self, _: &HostLogin) -> SharedTerminalTransportFactory {
        self.factory.clone()
    }
}

pub type SharedTerminalTransportFactory = Arc<dyn TerminalTransportFactory>;

pub(crate) fn send_event(
    events: &Sender<TerminalTransportEvent>,
    event: TerminalTransportEvent,
) -> bool {
    events.send_blocking(event).is_ok()
}
