//! The SSH terminal's remote-only file sidebar: one connection per host,
//! retained browsing and transfers, and the existing editor/preview flows.

use crate::support::*;
use gpui_kit::component::{
    Placement,
    dock::{DockPlacement, InsertTarget, PanelId},
};
use shellrs::app::{ExplorerAction, ExplorerCommand, ToggleTool, ToolKind};
use shellrs::editor::EditorId;
use shellrs::sftp::{TransferPhase, TransferProgress};
use std::path::PathBuf;

/// Separate fake servers make a transfer sent to the wrong host observable.
/// Record creation synchronously too: an unused sidebar must open no SFTP.
struct FileServers {
    web_address: String,
    web: Arc<FakeSftpProvider>,
    other: Arc<FakeSftpProvider>,
    connections: Mutex<Vec<String>>,
}

impl FileServers {
    fn connections_for(&self, address: &str) -> usize {
        self.connections
            .lock()
            .unwrap()
            .iter()
            .filter(|host| host.as_str() == address)
            .count()
    }
}

impl SftpTransportProvider for FileServers {
    fn create(&self, login: &HostLogin) -> Box<dyn SftpTransport> {
        self.connections.lock().unwrap().push(login.host.clone());
        if login.host == self.web_address {
            self.web.create(login)
        } else {
            self.other.create(login)
        }
    }
}

/// Output channels indexed by connection creation, so reports pass through
/// the production byte parser without sending a command to a real server.
#[derive(Default)]
struct DirectoryTerminals {
    next: AtomicUsize,
    outputs:
        Arc<Mutex<std::collections::HashMap<usize, async_channel::Sender<TerminalTransportEvent>>>>,
}
impl TerminalTransportFactory for DirectoryTerminals {
    fn create(&self) -> Box<dyn TerminalTransport> {
        Box::new(DirectoryTerminal {
            index: self.next.fetch_add(1, Ordering::SeqCst) + 1,
            outputs: self.outputs.clone(),
        })
    }
}
struct DirectoryTerminal {
    index: usize,
    outputs:
        Arc<Mutex<std::collections::HashMap<usize, async_channel::Sender<TerminalTransportEvent>>>>,
}
impl TerminalTransport for DirectoryTerminal {
    fn run(
        self: Box<Self>,
        _: TerminalSize,
        commands: mpsc::Receiver<TerminalTransportCommand>,
        events: async_channel::Sender<TerminalTransportEvent>,
    ) -> anyhow::Result<()> {
        self.outputs
            .lock()
            .unwrap()
            .insert(self.index, events.clone());
        events.send_blocking(TerminalTransportEvent::Started)?;
        while let Ok(command) = commands.recv() {
            if matches!(command, TerminalTransportCommand::Shutdown) {
                break;
            }
        }
        self.outputs.lock().unwrap().remove(&self.index);
        let _ = events.send_blocking(TerminalTransportEvent::Exited {
            code: 0,
            signal: None,
        });
        Ok(())
    }
}
impl DirectoryTerminals {
    fn report(&self, terminal: u64, address: &str, encoded_path: &str) {
        let output = self.outputs.lock().unwrap()[&(terminal as usize)].clone();
        output
            .send_blocking(TerminalTransportEvent::Output(
                format!("\x1b]7;file://{address}{encoded_path}\x07").into_bytes(),
            ))
            .unwrap();
    }
}

fn open_files_workspace(
    cx: &mut TestAppContext,
    web: Arc<FakeSftpProvider>,
) -> (WindowHandle<Root>, Entity<Workspace>, Arc<FileServers>) {
    open_files_workspace_with_terminals(cx, web, Arc::new(FakeTerminalFactory::default()))
}

fn open_files_workspace_with_terminals(
    cx: &mut TestAppContext,
    web: Arc<FakeSftpProvider>,
    factory: SharedTerminalTransportFactory,
) -> (WindowHandle<Root>, Entity<Workspace>, Arc<FileServers>) {
    init_app(cx);
    cx.update(|cx| cx.set_reduce_motion(true));
    let store = HostStore::seed();
    let servers = Arc::new(FileServers {
        web_address: store.host(HostId(WEB_01)).unwrap().address.to_string(),
        web,
        other: Arc::new(FakeSftpProvider::default()),
        connections: Mutex::default(),
    });
    let mut workspace = None;
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let store = cx.new(|_| store);
        let settings = cx.new(|_| settings_store());
        let remote = Arc::new(FixedRemoteTerminalTransportProvider::new(factory.clone()));
        let view = cx.new(|cx| {
            Workspace::new_with_services(
                store,
                settings,
                remote,
                fake_local_terminals(),
                servers.clone(),
                Arc::new(FakeLocalDirectory::default()),
                Arc::new(FakeConnectionTester::default()),
                Arc::new(FakeForwardProvider::default()),
                window,
                cx,
            )
        });
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    (handle, workspace.unwrap(), servers)
}

async fn show_temporary_files(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    workspace: &Entity<Workspace>,
    address: &str,
) -> (HostId, RemoteTerminalId, ExplorerId) {
    let store = cx.update(|cx| workspace.read(cx).store().clone());
    let host = cx.update(|cx| {
        store.update(cx, |store, cx| {
            store.insert_temporary(
                HostDraft::new(
                    "temporary",
                    address.to_string(),
                    22,
                    "tester",
                    AuthKind::NoPassword,
                    None,
                ),
                None,
                cx,
            )
        })
    });
    in_frame(cx, handle, |window, cx| {
        window.click(("terminal-tab", INITIAL_WEB_TERMINAL), cx);
        window.dispatch_action(Box::new(ConnectHost(host)), cx);
    });
    let terminal = cx.update(|cx| match workspace.read(cx).active_tab() {
        Some(CenterTab::Terminal(id)) => id,
        other => panic!("temporary terminal expected, got {other:?}"),
    });
    let id = show_files(cx, handle, workspace, terminal.0, host.0).await;
    (host, terminal, id)
}

async fn wait_for_queue_status(cx: &mut TestAppContext, handle: WindowHandle<Root>, status: &str) {
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        window
            .try_find(("queue-entry", 1u64))
            .is_some_and(|row| row.label() == Some(status))
    })
    .await;
}

fn sidebar_id(workspace: &Entity<Workspace>, host: u64, cx: &App) -> ExplorerId {
    workspace
        .read(cx)
        .file_sidebar(HostId(host))
        .expect("the host's file sidebar")
        .read(cx)
        .id()
}

async fn wait_for_path(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    id: ExplorerId,
    path: &str,
) {
    let path = path.to_string();
    cx.wait_for(handle.into(), Duration::from_secs(2), move |window, cx| {
        window.render_frame(cx);
        window.try_find(("remote-pane", id.0)).is_some()
            && window
                .within(("remote-pane", id.0))
                .try_find("remote-path")
                .is_some_and(|field| field.value() == Some(path.as_str()))
            && window
                .within(("remote-pane", id.0))
                .try_find("file:文件 甲.txt")
                .is_some()
    })
    .await;
}

async fn show_files(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    workspace: &Entity<Workspace>,
    terminal: u64,
    host: u64,
) -> ExplorerId {
    in_frame(cx, handle, |window, cx| {
        window.activate_window();
        window.click(("terminal-tab", terminal), cx);
    });
    in_frame(cx, handle, |window, cx| {
        if window.find("tool-files").checked() != Some(true) {
            window.click("tool-files", cx);
        }
    });
    let id = cx.update(|cx| sidebar_id(workspace, host, cx));
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        workspace
            .read(cx)
            .explorer(id)
            .unwrap()
            .read(cx)
            .connection_state()
            == ConnectionState::Connected
            && window.try_find(("remote-pane", id.0)).is_some()
            && window
                .within(("remote-pane", id.0))
                .try_find("file:文件 甲.txt")
                .is_some()
    })
    .await;
    id
}

fn command(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    id: ExplorerId,
    command: ExplorerCommand,
) {
    in_frame(cx, handle, |window, cx| {
        window.dispatch_action(Box::new(ExplorerAction::new(id, command)), cx)
    });
}

async fn navigate(cx: &mut TestAppContext, handle: WindowHandle<Root>, id: ExplorerId, path: &str) {
    command(
        cx,
        handle,
        id,
        ExplorerCommand::Navigate {
            remote: true,
            path: path.into(),
        },
    );
    wait_for_path(cx, handle, id, path).await;
}

fn drop_file(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    id: ExplorerId,
    directory: Option<&str>,
    path: &str,
) {
    in_frame(cx, handle, |window, cx| {
        let position = if let Some(directory) = directory {
            let row = ElementId::Name(format!("file:{directory}").into());
            window.within(("remote-pane", id.0)).hover(row.clone(), cx);
            window
                .within(("remote-pane", id.0))
                .find(row)
                .bounds()
                .center()
        } else {
            let table = window.within(("remote-pane", id.0)).find("table").bounds();
            point(table.center().x, table.bottom() - px(8.))
        };
        let paths = gpui_kit::ExternalPaths(vec![PathBuf::from(path)].into());
        window.dispatch_event(
            gpui_kit::FileDropEvent::Entered { position, paths }.to_platform_input(),
            cx,
        );
        window.dispatch_event(
            gpui_kit::FileDropEvent::Submit { position }.to_platform_input(),
            cx,
        );
    });
}

async fn wait_for_uploads(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    provider: &FakeSftpProvider,
    count: usize,
) {
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        provider.requests.lock().unwrap().len() == count
    })
    .await;
}

#[gpui_kit::test]
async fn files_open_lazily_as_one_remote_sidebar_and_leave_dual_pane_tabs_alone(
    cx: &mut TestAppContext,
) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    in_frame(cx, handle, |window, cx| {
        window.click(("terminal-tab", INITIAL_WEB_TERMINAL), cx);
        assert_eq!(window.find("tool-files").checked(), Some(false));
        assert!(workspace.read(cx).file_sidebar(HostId(WEB_01)).is_none());
    });
    assert!(servers.connections.lock().unwrap().is_empty());
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    in_frame(cx, handle, |window, cx| {
        assert!(window.find(("terminal", INITIAL_WEB_TERMINAL)).visible());
        assert!(window.find(("remote-pane", id.0)).visible());
        assert!(window.try_find(("local-pane", id.0)).is_none());
        assert!(window.try_find(("explorer-tab", id.0)).is_none());
        assert_eq!(
            workspace.read(cx).active_tab(),
            Some(CenterTab::Terminal(RemoteTerminalId(INITIAL_WEB_TERMINAL)))
        );
        assert_eq!(
            workspace
                .read(cx)
                .file_sidebar(HostId(WEB_01))
                .unwrap()
                .entity_id(),
            workspace.read(cx).explorer(id).unwrap().entity_id()
        );
        window.click("tool-files", cx);
    });
    in_frame(cx, handle, |window, cx| {
        assert!(window.try_find(("remote-pane", id.0)).is_none());
        assert_eq!(sidebar_id(&workspace, WEB_01, cx), id);
        window.dispatch_action(Box::new(ConnectHost(HostId(WEB_01))), cx);
    });
    assert_eq!(
        show_files(cx, handle, &workspace, FIRST_NEW_TERMINAL, WEB_01).await,
        id
    );
    assert_eq!(servers.connections.lock().unwrap().len(), 1);

    // The explicit SFTP command still opens its own two-pane center tab.
    in_frame(cx, handle, |window, cx| {
        window.dispatch_action(Box::new(OpenExplorer(HostId(WEB_01))), cx)
    });
    let tab = cx.update(|cx| match workspace.read(cx).active_tab() {
        Some(CenterTab::Explorer(id)) => id,
        other => panic!("expected an independent SFTP tab, got {other:?}"),
    });
    assert_ne!(tab, id);
    wait_for_path(cx, handle, tab, "/home/tester").await;
    in_frame(cx, handle, |window, _| {
        assert!(window.find(("explorer-tab", tab.0)).visible());
        assert!(window.find(("local-pane", tab.0)).visible());
        assert!(window.find(("remote-pane", tab.0)).visible());
        assert!(window.try_find("tool-files").is_none());
    });
    assert_eq!(servers.connections.lock().unwrap().len(), 2);
}

#[gpui_kit::test]
async fn files_keep_each_hosts_directory_when_split_terminal_focus_changes(
    cx: &mut TestAppContext,
) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let web = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    navigate(cx, handle, web, "/srv/web").await;
    let staging = show_files(
        cx,
        handle,
        &workspace,
        INITIAL_STAGING_TERMINAL,
        STAGING_API,
    )
    .await;
    assert_ne!(web, staging);
    navigate(cx, handle, staging, "/srv/staging").await;
    cx.update_window(handle.into(), |_, window, cx| {
        let panel = PanelId::from(
            workspace
                .read(cx)
                .remote_terminal(RemoteTerminalId(INITIAL_STAGING_TERMINAL))
                .unwrap()
                .entity_id(),
        );
        let area = workspace.read(cx).dock_area().clone();
        area.update(cx, |area, cx| {
            let node = area
                .layout(DockPlacement::Center)
                .and_then(|tree| tree.find_panel_node(panel))
                .unwrap();
            area.move_panel(
                panel,
                InsertTarget::Split {
                    node,
                    placement: Placement::Right,
                    size: None,
                },
                window,
                cx,
            );
        });
    })
    .unwrap();
    cx.run_until_parked();
    for (terminal, host, name, id, path) in [
        (INITIAL_WEB_TERMINAL, WEB_01, "web-01", web, "/srv/web"),
        (
            INITIAL_STAGING_TERMINAL,
            STAGING_API,
            "staging-api",
            staging,
            "/srv/staging",
        ),
        (INITIAL_WEB_TERMINAL, WEB_01, "web-01", web, "/srv/web"),
    ] {
        in_frame(cx, handle, |window, cx| {
            window.click(("terminal", terminal), cx)
        });
        wait_for_path(cx, handle, id, path).await;
        in_frame(cx, handle, |window, cx| {
            assert!(window.find(("terminal", INITIAL_WEB_TERMINAL)).visible());
            assert!(
                window
                    .find(("terminal", INITIAL_STAGING_TERMINAL))
                    .visible()
            );
            assert_eq!(window.find("tool-sidebar-host").label(), Some(name));
            assert_eq!(sidebar_id(&workspace, host, cx), id);
        });
    }
    assert_eq!(servers.connections.lock().unwrap().len(), 2);
}

#[gpui_kit::test]
async fn terminal_directory_reports_follow_only_the_active_connection_and_new_paths(
    cx: &mut TestAppContext,
) {
    let terminals = Arc::new(DirectoryTerminals::default());
    let (handle, workspace, _) = open_files_workspace_with_terminals(
        cx,
        Arc::new(FakeSftpProvider::default()),
        terminals.clone(),
    );
    let address = cx.update(|cx| {
        workspace
            .read(cx)
            .store()
            .read(cx)
            .host(HostId(WEB_01))
            .unwrap()
            .address
            .to_string()
    });
    let web = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, _| {
        terminals.outputs.lock().unwrap().len() == 2
    })
    .await;
    terminals.report(
        INITIAL_WEB_TERMINAL,
        &address,
        "/srv/%E4%B8%AD%E6%96%87%20%25",
    );
    wait_for_path(cx, handle, web, "/srv/中文 %").await;
    navigate(cx, handle, web, "/manual").await;
    in_frame(cx, handle, |window, _| {
        assert_eq!(
            window
                .within(("remote-pane", web.0))
                .find("remote-path")
                .value(),
            Some("/manual")
        );
    });
    terminals.report(INITIAL_WEB_TERMINAL, &address, "/srv/new-report");
    wait_for_path(cx, handle, web, "/srv/new-report").await;

    let staging = show_files(
        cx,
        handle,
        &workspace,
        INITIAL_STAGING_TERMINAL,
        STAGING_API,
    )
    .await;
    terminals.report(INITIAL_WEB_TERMINAL, &address, "/background-host");
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, cx| {
        workspace
            .read(cx)
            .remote_terminal(RemoteTerminalId(INITIAL_WEB_TERMINAL))
            .unwrap()
            .read(cx)
            .reported_directory(cx)
            .is_some_and(|report| report.path() == "/background-host")
    })
    .await;
    wait_for_path(cx, handle, staging, "/home/tester").await;
    show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    wait_for_path(cx, handle, web, "/background-host").await;

    in_frame(cx, handle, |window, cx| {
        window.dispatch_action(Box::new(ConnectHost(HostId(WEB_01))), cx);
    });
    show_files(cx, handle, &workspace, FIRST_NEW_TERMINAL, WEB_01).await;
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, _| {
        terminals.outputs.lock().unwrap().len() == 3
    })
    .await;
    terminals.report(INITIAL_WEB_TERMINAL, &address, "/background-same-host");
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, cx| {
        workspace
            .read(cx)
            .remote_terminal(RemoteTerminalId(INITIAL_WEB_TERMINAL))
            .unwrap()
            .read(cx)
            .reported_directory(cx)
            .is_some_and(|report| report.path() == "/background-same-host")
    })
    .await;
    in_frame(cx, handle, |window, _| {
        assert_eq!(
            window
                .within(("remote-pane", web.0))
                .find("remote-path")
                .value(),
            Some("/background-host")
        );
    });
    terminals.report(FIRST_NEW_TERMINAL, &address, "/foreground-tab");
    wait_for_path(cx, handle, web, "/foreground-tab").await;
    terminals.report(
        FIRST_NEW_TERMINAL,
        "nested-ssh.example",
        "/must-not-follow-nested-host",
    );
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, cx| {
        workspace
            .read(cx)
            .remote_terminal(RemoteTerminalId(FIRST_NEW_TERMINAL))
            .unwrap()
            .read(cx)
            .reported_directory(cx)
            .is_some_and(|report| report.path() == "/must-not-follow-nested-host")
    })
    .await;
    in_frame(cx, handle, |window, _| {
        assert_eq!(
            window
                .within(("remote-pane", web.0))
                .find("remote-path")
                .value(),
            Some("/foreground-tab")
        );
    });

    // A report that arrives while the user edits the address must not
    // replace the input or spring back after Escape dismisses the editor.
    in_frame(cx, handle, |window, cx| {
        window
            .within(("remote-pane", web.0))
            .click("remote-path", cx);
    });
    terminals.report(FIRST_NEW_TERMINAL, &address, "/while-editing");
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, cx| {
        workspace
            .read(cx)
            .remote_terminal(RemoteTerminalId(FIRST_NEW_TERMINAL))
            .unwrap()
            .read(cx)
            .reported_directory(cx)
            .is_some_and(|report| report.path() == "/while-editing")
    })
    .await;
    in_frame(cx, handle, |window, cx| {
        assert_eq!(
            window
                .within(("remote-pane", web.0))
                .find("sidebar-path-input")
                .value(),
            Some("/foreground-tab")
        );
        window.press("escape", cx);
    });
    wait_for_path(cx, handle, web, "/foreground-tab").await;
    terminals.report(FIRST_NEW_TERMINAL, &address, "/after-editing");
    wait_for_path(cx, handle, web, "/after-editing").await;
}

#[gpui_kit::test]
async fn sidebar_path_can_be_edited_by_click_and_enter_or_canceled_with_escape(
    cx: &mut TestAppContext,
) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    in_frame(cx, handle, |window, cx| {
        window
            .within(("remote-pane", id.0))
            .click("remote-path", cx);
    });
    in_frame(cx, handle, |window, cx| {
        let mut pane = window.within(("remote-pane", id.0));
        assert!(pane.find("sidebar-path-input").visible());
        pane.click("sidebar-path-input", cx);
        #[cfg(target_os = "macos")]
        pane.press("cmd-a", cx);
        #[cfg(not(target_os = "macos"))]
        pane.press("ctrl-a", cx);
        pane.input("/srv/中文 目录", cx);
        pane.press("enter", cx);
    });
    wait_for_path(cx, handle, id, "/srv/中文 目录").await;
    in_frame(cx, handle, |window, cx| {
        assert!(
            window
                .within(("remote-pane", id.0))
                .try_find("sidebar-path-input")
                .is_none()
        );
        window
            .within(("remote-pane", id.0))
            .click("remote-path", cx);
    });
    in_frame(cx, handle, |window, cx| {
        let mut pane = window.within(("remote-pane", id.0));
        pane.click("sidebar-path-input", cx);
        #[cfg(target_os = "macos")]
        pane.press("cmd-a", cx);
        #[cfg(not(target_os = "macos"))]
        pane.press("ctrl-a", cx);
        pane.input("/must-not-open", cx);
        pane.press("escape", cx);
    });
    in_frame(cx, handle, |window, _| {
        let pane = window.within(("remote-pane", id.0));
        assert!(pane.try_find("sidebar-path-input").is_none());
        assert_eq!(pane.find("remote-path").value(), Some("/srv/中文 目录"));
    });
    assert!(servers.web.requests.lock().unwrap().is_empty());
    assert!(servers.other.requests.lock().unwrap().is_empty());
}

#[gpui_kit::test]
async fn external_files_drop_into_the_current_directory_or_the_directory_row(
    cx: &mut TestAppContext,
) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    for (index, (row, target)) in [(None, "/home/tester"), (Some("目录"), "/home/tester/目录")]
        .into_iter()
        .enumerate()
    {
        drop_file(cx, handle, id, row, "/tmp/Finder 文件.txt");
        in_frame(cx, handle, |window, _| {
            assert!(window.try_find("upload-confirm").is_none());
        });
        wait_for_uploads(cx, handle, &servers.web, index + 1).await;
        {
            let requests = servers.web.requests.lock().unwrap();
            assert_eq!(
                requests[index].sources(),
                [PathBuf::from("/tmp/Finder 文件.txt")]
            );
            assert_eq!(requests[index].destination().as_str(), target);
        }
        // The fake transport does not complete by itself. Complete the first
        // upload so the next real drop can start and expose its own target.
        let events = servers.web.events.lock().unwrap()[0].clone();
        events
            .send_blocking(SftpEvent::Progress(TransferProgress::new(
                TransferPhase::Completed,
            )))
            .unwrap();
        events.send_blocking(SftpEvent::Idle).unwrap();
        cx.run_until_parked();
    }
    assert!(servers.other.requests.lock().unwrap().is_empty());
}

#[gpui_kit::test]
async fn sidebar_drop_skips_initial_confirmation_but_keeps_overwrite_questions(
    cx: &mut TestAppContext,
) {
    use shellrs::sftp::{TransferQuestion, TransferQuestionKind};
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    drop_file(cx, handle, id, None, "/tmp/existing.txt");
    in_frame(cx, handle, |window, _| {
        assert!(window.try_find("upload-confirm").is_none());
    });
    wait_for_uploads(cx, handle, &servers.web, 1).await;
    servers.web.events.lock().unwrap()[0]
        .send_blocking(SftpEvent::Question(TransferQuestion::new(
            900,
            TransferQuestionKind::Conflict,
            "/home/tester/existing.txt",
            "目标已存在",
        )))
        .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        window.try_find("transfer-question-confirm").is_some()
    })
    .await;
    in_frame(cx, handle, |window, cx| {
        assert!(window.try_find("upload-confirm").is_none());
        window.click("transfer-question-cancel", cx);
    });
    wait_for_queue_status(cx, handle, "已停止").await;
    assert_eq!(servers.web.requests.lock().unwrap().len(), 1);
    assert!(servers.other.requests.lock().unwrap().is_empty());
}

#[gpui_kit::test]
async fn choosing_uploads_keeps_the_original_host_and_directory_after_a_tab_switch(
    cx: &mut TestAppContext,
) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let web = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    navigate(cx, handle, web, "/original").await;
    command(cx, handle, web, ExplorerCommand::ChooseFiles);
    assert!(cx.did_prompt_for_paths());
    // The native picker has not answered. Another terminal and directory
    // must not retarget the operation already offered by web-01.
    let staging = show_files(
        cx,
        handle,
        &workspace,
        INITIAL_STAGING_TERMINAL,
        STAGING_API,
    )
    .await;
    navigate(cx, handle, staging, "/other-host").await;
    command(
        cx,
        handle,
        web,
        ExplorerCommand::Navigate {
            remote: true,
            path: "/later".into(),
        },
    );
    cx.simulate_path_prompt_response(|options| {
        assert!(options.files && options.directories && options.multiple);
        Some(vec!["/tmp/original.txt".into()])
    });
    cx.run_until_parked();
    in_frame(cx, handle, |window, _| {
        assert!(window.try_find("upload-confirm").is_none());
    });
    wait_for_uploads(cx, handle, &servers.web, 1).await;
    assert!(servers.other.requests.lock().unwrap().is_empty());
    let requests = servers.web.requests.lock().unwrap();
    assert_eq!(requests[0].sources(), [PathBuf::from("/tmp/original.txt")]);
    assert_eq!(requests[0].destination().as_str(), "/original");
}

#[gpui_kit::test]
async fn an_upload_picker_from_an_old_connection_cannot_enqueue_after_reconnect(
    cx: &mut TestAppContext,
) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    let generation = cx.update(|cx| {
        workspace
            .read(cx)
            .explorer(id)
            .unwrap()
            .read(cx)
            .generation()
    });
    command(cx, handle, id, ExplorerCommand::ChooseFiles);
    assert!(cx.did_prompt_for_paths());
    // The native chooser is still open when the original connection is replaced.
    let events = servers.web.events.lock().unwrap()[0].clone();
    events.send_blocking(SftpEvent::Connecting).unwrap();
    events
        .send_blocking(SftpEvent::Connected {
            home: RemotePath::new("/home/tester").unwrap(),
        })
        .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        let panel = workspace.read(cx).explorer(id).unwrap().read(cx);
        panel.generation() != generation && panel.connection_state() == ConnectionState::Connected
    })
    .await;
    cx.simulate_path_prompt_response(|_| Some(vec!["/tmp/stale-picker.txt".into()]));
    cx.run_until_parked();
    in_frame(cx, handle, |window, cx| {
        assert!(window.try_find("upload-confirm").is_none());
        assert!(window.try_find(("queue-entry", 1u64)).is_none());
        assert!(
            !workspace
                .read(cx)
                .explorer(id)
                .unwrap()
                .read(cx)
                .is_transferring()
        );
    });
    assert!(servers.web.requests.lock().unwrap().is_empty());
    assert!(servers.other.requests.lock().unwrap().is_empty());
}

#[gpui_kit::test]
async fn the_remote_download_command_confirms_the_selection_and_chosen_local_folder(
    cx: &mut TestAppContext,
) {
    // A root-relative Unix path is not absolute on Windows. Model the
    // native directory chooser with an actual platform-absolute folder.
    let download_dir = tempfile::tempdir().unwrap();
    let download_path = download_dir.path().to_path_buf();
    assert!(download_path.is_absolute());
    let download_target = download_path.to_string_lossy().into_owned();
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    in_frame(cx, handle, |window, cx| {
        window
            .within(("remote-pane", id.0))
            .click("name:文件 甲.txt", cx)
    });
    // Dispatch exactly the context-menu item; real PopupMenus leak handles
    // in these headless tests.
    command(cx, handle, id, ExplorerCommand::Transfer { remote: true });
    in_frame(cx, handle, |window, cx| {
        assert!(window.find("download-confirm").visible());
        assert!(servers.web.downloads.lock().unwrap().is_empty());
        window.click("download-browse", cx);
    });
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|options| {
        assert!(options.directories && !options.files && !options.multiple);
        Some(vec![download_path.clone()])
    });
    cx.run_until_parked();
    in_frame(cx, handle, |window, cx| {
        assert_eq!(
            window.find("download-target").value(),
            Some(download_target.as_str())
        );
        window.click("download-confirm", cx);
    });
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        servers.web.downloads.lock().unwrap().len() == 1
    })
    .await;
    let downloads = servers.web.downloads.lock().unwrap();
    assert_eq!(
        downloads[0].sources(),
        [RemotePath::new("/home/tester/文件 甲.txt").unwrap()]
    );
    assert_eq!(downloads[0].destination(), download_path.as_path());
    assert!(servers.other.downloads.lock().unwrap().is_empty());
}

#[gpui_kit::test]
async fn hiding_files_and_closing_its_terminal_keep_the_connection_and_queue_running(
    cx: &mut TestAppContext,
) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    for (index, path) in ["/tmp/first.txt", "/tmp/second.txt"]
        .into_iter()
        .enumerate()
    {
        drop_file(cx, handle, id, None, path);
        in_frame(cx, handle, |window, _| {
            assert!(window.try_find("upload-confirm").is_none());
            assert!(window.find(("queue-entry", (index + 1) as u64)).visible());
        });
        wait_for_uploads(cx, handle, &servers.web, 1).await;
    }
    in_frame(cx, handle, |window, cx| window.click("tool-files", cx));
    in_frame(cx, handle, |window, cx| {
        window.click(("close-terminal", INITIAL_WEB_TERMINAL), cx)
    });
    in_frame(cx, handle, |window, cx| {
        assert!(window.try_find("ok").is_none());
        assert!(
            workspace
                .read(cx)
                .remote_terminal(RemoteTerminalId(INITIAL_WEB_TERMINAL))
                .is_none()
        );
        let sidebar = workspace.read(cx).explorer(id).unwrap().read(cx);
        assert_eq!(sidebar.connection_state(), ConnectionState::Connected);
        assert!(sidebar.is_transferring());
        assert!(
            workspace
                .read(cx)
                .file_sidebar(HostId(STAGING_API))
                .is_none()
        );
    });
    let events = servers.web.events.lock().unwrap()[0].clone();
    events
        .send_blocking(SftpEvent::Progress(TransferProgress::new(
            TransferPhase::Completed,
        )))
        .unwrap();
    events.send_blocking(SftpEvent::Idle).unwrap();
    wait_for_uploads(cx, handle, &servers.web, 2).await;
    assert_eq!(
        servers.web.requests.lock().unwrap()[1].sources(),
        [PathBuf::from("/tmp/second.txt")]
    );
    assert_eq!(servers.connections.lock().unwrap().len(), 1);
    in_frame(cx, handle, |window, cx| {
        window.click(("terminal", INITIAL_STAGING_TERMINAL), cx);
        window.dispatch_action(Box::new(ConnectHost(HostId(WEB_01))), cx);
    });
    assert_eq!(
        show_files(cx, handle, &workspace, FIRST_NEW_TERMINAL, WEB_01).await,
        id
    );
    in_frame(cx, handle, |window, _| {
        assert_eq!(window.find(("queue-entry", 1u64)).label(), Some("已完成"));
        assert!(window.find(("queue-entry", 2u64)).visible());
    });
    assert_eq!(servers.connections.lock().unwrap().len(), 1);
}

#[gpui_kit::test]
async fn deleting_the_host_releases_its_hidden_file_sidebar(cx: &mut TestAppContext) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    let weak = cx.update(|cx| workspace.read(cx).explorer(id).unwrap().downgrade());
    drop_file(cx, handle, id, None, "/tmp/unfinished.txt");
    in_frame(cx, handle, |window, _| {
        assert!(window.try_find("upload-confirm").is_none());
    });
    wait_for_uploads(cx, handle, &servers.web, 1).await;
    in_frame(cx, handle, |window, cx| window.click("tool-files", cx));
    in_frame(cx, handle, |window, cx| {
        window.click(("terminal", INITIAL_WEB_TERMINAL), cx);
        window.dispatch_action(Box::new(DeleteHost(HostId(WEB_01))), cx);
    });
    in_frame(cx, handle, |window, cx| {
        assert!(window.find("ok").visible());
        assert!(workspace.read(cx).file_sidebar(HostId(WEB_01)).is_some());
        window.click("ok", cx);
    });
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        weak.upgrade().is_none()
    })
    .await;
    cx.update(|cx| {
        let workspace = workspace.read(cx);
        assert!(workspace.file_sidebar(HostId(WEB_01)).is_none());
        assert!(workspace.explorer(id).is_none());
        assert!(workspace.store().read(cx).host(HostId(WEB_01)).is_none());
    });
    assert!(servers.web.events.lock().unwrap()[0].is_closed());
}

#[gpui_kit::test]
async fn sidebar_files_reuse_the_preview_and_editor_and_save_to_the_original_host(
    cx: &mut TestAppContext,
) {
    let path = "/pictures/README.md";
    let web = Arc::new(FakeSftpProvider::with_files(&[(path, b"# Original\n")]));
    let (handle, workspace, servers) = open_files_workspace(cx, web);
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    // Preview is a menu command; it reads through this sidebar's SFTP.
    command(
        cx,
        handle,
        id,
        ExplorerCommand::Preview {
            remote: true,
            path: Some(path.into()),
        },
    );
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        window.try_find("preview-markdown").is_some()
    })
    .await;
    in_frame(cx, handle, |window, cx| {
        assert!(window.find("preview-download").visible());
        window.click("preview-edit", cx);
    });
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        window.try_find(("editor", 1u64)).is_some()
    })
    .await;
    in_frame(cx, handle, |window, cx| {
        assert_eq!(
            window.find(("editor-path", 1u64)).label(),
            Some("web-01:/pictures/README.md")
        );
        window.input("saved: ", cx);
    });
    // Show another host, then save the editor that web-01 opened.
    show_files(
        cx,
        handle,
        &workspace,
        INITIAL_STAGING_TERMINAL,
        STAGING_API,
    )
    .await;
    in_frame(cx, handle, |window, cx| {
        window.click(("editor-tab", 1u64), cx)
    });
    in_frame(cx, handle, |window, cx| window.press("secondary-s", cx));
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        !servers.web.writes.lock().unwrap().is_empty()
            && window.try_find(("editor-state", 1u64)).is_none()
    })
    .await;
    assert_eq!(servers.web.file(path), b"saved: # Original\n");
    assert!(servers.other.writes.lock().unwrap().is_empty());
    show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    command(
        cx,
        handle,
        id,
        ExplorerCommand::Navigate {
            remote: true,
            path: "/pictures".into(),
        },
    );
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        window
            .within(("remote-pane", id.0))
            .try_find("name:README.md")
            .is_some()
    })
    .await;
    let reads = servers.web.reads.lock().unwrap().len();
    in_frame(cx, handle, |window, cx| {
        window
            .within(("remote-pane", id.0))
            .double_click("name:README.md", cx)
    });
    in_frame(cx, handle, |window, cx| {
        assert!(window.find(("editor", 1u64)).visible());
        assert!(workspace.read(cx).editor(EditorId(2)).is_none());
    });
    assert_eq!(servers.web.reads.lock().unwrap().len(), reads);

    // The editor is in front, so the sidebar's render tree holds no strong
    // reference. Its Closed event must still reach the workspace and close
    // this clean editor when the sidebar connection is removed.
    let weak = cx.update(|cx| workspace.read(cx).explorer(id).unwrap().downgrade());
    in_frame(cx, handle, |window, cx| {
        assert_eq!(
            workspace.read(cx).active_tab(),
            Some(CenterTab::Editor(EditorId(1)))
        );
        assert!(window.try_find(("file-sidebar", id.0)).is_none());
        window.dispatch_action(Box::new(shellrs::app::CloseExplorer(id)), cx);
    });
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        workspace.read(cx).editor(EditorId(1)).is_none()
            && workspace.read(cx).explorer(id).is_none()
            && weak.upgrade().is_none()
    })
    .await;
    in_frame(cx, handle, |window, _| {
        assert!(window.try_find(("editor-tab", 1u64)).is_none());
        assert!(window.try_find("ok").is_none());
        assert!(window.try_find("editor-close-save").is_none());
    });
}

#[gpui_kit::test]
async fn files_are_offered_only_for_eligible_ssh_terminals(cx: &mut TestAppContext) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let store = cx.update(|cx| workspace.read(cx).store().clone());
    cx.update(|cx| {
        store.update(cx, |store, cx| {
            store.set_host_os(HostId(WEB_01), Some(HostOs::Windows), cx)
        });
    });
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    // Windows SSH still has SFTP; local terminals and settings do not.
    for action in [
        Box::new(NewLocalTerminal) as Box<dyn gpui_kit::Action>,
        Box::new(OpenSettings),
    ] {
        in_frame(cx, handle, |window, cx| window.dispatch_action(action, cx));
        in_frame(cx, handle, |window, cx| {
            assert!(window.try_find("tool-files").is_none());
            assert!(window.try_find(("remote-pane", id.0)).is_none());
            window.dispatch_action(Box::new(ToggleTool(ToolKind::Files)), cx);
        });
        assert_eq!(servers.connections.lock().unwrap().len(), 1);
    }
    cx.update_window(handle.into(), |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_link(
                shellrs::cli::OpenLink {
                    url: "ssh://token@bastion.test:2222".into(),
                    tab: Some("bastion".into()),
                },
                window,
                cx,
            );
        });
    })
    .unwrap();
    cx.run_until_parked();
    in_frame(cx, handle, |window, cx| {
        assert!(window.find("tool-snippets").visible());
        assert!(window.try_find("tool-files").is_none());
        window.dispatch_action(Box::new(ToggleTool(ToolKind::Files)), cx);
    });
    assert_eq!(servers.connections.lock().unwrap().len(), 1);
    let external = cx.update(|cx| {
        let workspace = workspace.read(cx);
        let Some(CenterTab::Terminal(id)) = workspace.active_tab() else {
            panic!("external SSH terminal")
        };
        workspace.remote_terminal(id).unwrap().read(cx).host_id()
    });
    cx.update(|cx| assert!(workspace.read(cx).file_sidebar(external).is_none()));
    // Returning to the saved SSH host restores the existing sidebar.
    assert_eq!(
        show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await,
        id
    );
}

/// Tab still means "the other pane" in the shared shortcut map, but this
/// sidebar has no local pane to take focus. Both directions must stay remote.
#[gpui_kit::test]
async fn tab_keeps_the_remote_sidebar_focused_and_f5_can_still_download(cx: &mut TestAppContext) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    for key in ["tab", "shift-tab"] {
        // Cancelling a dialog can leave focus empty, so click the row again.
        in_frame(cx, handle, |window, cx| {
            window
                .within(("remote-pane", id.0))
                .click("name:文件 甲.txt", cx);
            window.press(key, cx);
        });
        in_frame(cx, handle, |window, cx| {
            let explorer = workspace.read(cx).explorer(id).unwrap().read(cx);
            assert!(
                explorer
                    .remote()
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            );
            assert!(
                !explorer
                    .local()
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            );
            window.press("f5", cx);
        });
        in_frame(cx, handle, |window, cx| {
            assert!(window.find("download-confirm").visible());
            assert!(window.try_find("upload-confirm").is_none());
            window.press("escape", cx);
        });
    }
    assert!(servers.web.downloads.lock().unwrap().is_empty());
}

#[gpui_kit::test]
async fn changing_a_hosts_address_blocks_old_sidebar_commands_until_reopened(
    cx: &mut TestAppContext,
) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let old = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    let store = cx.update(|cx| workspace.read(cx).store().clone());
    let endpoint = cx.update(|cx| {
        workspace
            .read(cx)
            .explorer(old)
            .unwrap()
            .read(cx)
            .endpoint()
            .to_string()
    });
    for path in ["/tmp/approved-first.txt", "/tmp/approved-second.txt"] {
        drop_file(cx, handle, old, None, path);
        in_frame(cx, handle, |window, _| {
            assert!(window.try_find("upload-confirm").is_none());
        });
        wait_for_uploads(cx, handle, &servers.web, 1).await;
    }
    let change_address = |cx: &mut TestAppContext, address: &str| {
        cx.update(|cx| {
            store.update(cx, |store, cx| {
                let mut draft = store.host(HostId(WEB_01)).unwrap().draft();
                draft.address = address.to_string().into();
                assert!(store.update(HostId(WEB_01), draft, cx));
            });
        });
        cx.run_until_parked();
    };
    change_address(cx, "moved-once.test");
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        window.try_find("file-sidebar-config-changed").is_some()
    })
    .await;
    in_frame(cx, handle, |window, cx| {
        assert!(
            window
                .find("file-sidebar-config-changed")
                .label()
                .is_some_and(|label| label.contains(&endpoint))
        );
        assert_eq!(sidebar_id(&workspace, WEB_01, cx), old);
        window.click(("terminal", INITIAL_WEB_TERMINAL), cx);
    });
    for blocked in [
        ExplorerCommand::Navigate {
            remote: true,
            path: "/must-not-open".into(),
        },
        ExplorerCommand::ChooseFiles,
        ExplorerCommand::UploadPaths {
            paths: vec!["/tmp/not-approved.txt".into()],
            target: "/must-not-upload".into(),
        },
        ExplorerCommand::Edit {
            remote: true,
            path: Some("/home/tester/文件 甲.txt".into()),
        },
    ] {
        command(cx, handle, old, blocked);
    }
    in_frame(cx, handle, |window, cx| {
        assert!(window.try_find(("remote-pane", old.0)).is_none());
        assert_eq!(
            workspace
                .read(cx)
                .explorer(old)
                .unwrap()
                .read(cx)
                .remote()
                .read(cx)
                .path(),
            "/home/tester"
        );
        assert!(window.try_find("upload-confirm").is_none());
        assert!(workspace.read(cx).editor(EditorId(1)).is_none());
    });
    assert!(!cx.did_prompt_for_paths());
    assert!(servers.web.reads.lock().unwrap().is_empty());
    assert_eq!(servers.connections.lock().unwrap().len(), 1);

    // Work already confirmed for the original endpoint keeps that endpoint:
    // the second queued upload still starts there after the address changes.
    let events = servers.web.events.lock().unwrap()[0].clone();
    events
        .send_blocking(SftpEvent::Progress(TransferProgress::new(
            TransferPhase::Completed,
        )))
        .unwrap();
    events.send_blocking(SftpEvent::Idle).unwrap();
    wait_for_uploads(cx, handle, &servers.web, 2).await;
    assert_eq!(
        servers.web.requests.lock().unwrap()[1].sources(),
        [PathBuf::from("/tmp/approved-second.txt")]
    );
    assert!(servers.other.requests.lock().unwrap().is_empty());

    in_frame(cx, handle, |window, cx| {
        window.click("file-sidebar-reopen", cx)
    });
    in_frame(cx, handle, |window, cx| {
        assert!(window.find("ok").visible());
        window.click("cancel", cx);
    });
    cx.update(|cx| assert_eq!(sidebar_id(&workspace, WEB_01, cx), old));
    assert_eq!(servers.connections.lock().unwrap().len(), 1);
    in_frame(cx, handle, |window, cx| {
        window.click("file-sidebar-reopen", cx)
    });
    in_frame(cx, handle, |window, cx| window.click("ok", cx));
    let reopened = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    assert_ne!(old, reopened);
    cx.update(|cx| assert!(workspace.read(cx).explorer(old).is_none()));
    assert_eq!(
        servers
            .connections
            .lock()
            .unwrap()
            .last()
            .map(String::as_str),
        Some("moved-once.test")
    );

    // With no unfinished work the next change can reopen directly.
    change_address(cx, "moved-twice.test");
    in_frame(cx, handle, |window, cx| {
        window.click("file-sidebar-reopen", cx)
    });
    let newest = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    assert_ne!(newest, reopened);
    in_frame(cx, handle, |window, _| {
        assert!(window.try_find("ok").is_none());
        assert!(window.try_find("file-sidebar-config-changed").is_none());
    });
    assert_eq!(
        servers
            .connections
            .lock()
            .unwrap()
            .last()
            .map(String::as_str),
        Some("moved-twice.test")
    );
}

#[gpui_kit::test]
async fn temporary_hosts_release_idle_sidebars_but_keep_promoted_transfers_until_closed(
    cx: &mut TestAppContext,
) {
    use shellrs::app::CloseEditor;

    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let store = cx.update(|cx| workspace.read(cx).store().clone());
    servers
        .other
        .change_file("/home/tester/文件 甲.txt", b"temporary file\n");
    for work in ["idle", "upload", "editor"] {
        let address = format!("{work}.test");
        let (host, terminal, id) = show_temporary_files(cx, handle, &workspace, &address).await;
        let entity = cx.update(|cx| workspace.read(cx).explorer(id).unwrap().entity_id());
        let weak = cx.update(|cx| workspace.read(cx).explorer(id).unwrap().downgrade());
        let events = servers.other.events.lock().unwrap().last().unwrap().clone();
        if work == "upload" {
            drop_file(cx, handle, id, None, "/tmp/temporary.txt");
            in_frame(cx, handle, |window, _| {
                assert!(window.try_find("upload-confirm").is_none());
            });
            wait_for_uploads(cx, handle, &servers.other, 1).await;
        }
        if work == "editor" {
            in_frame(cx, handle, |window, cx| {
                window
                    .within(("remote-pane", id.0))
                    .double_click("name:文件 甲.txt", cx)
            });
            cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
                window.render_frame(cx);
                window.try_find(("editor", 1u64)).is_some()
            })
            .await;
        } else {
            // Hiding also must not leave a temporary host retained forever.
            in_frame(cx, handle, |window, cx| window.click("tool-files", cx));
        }
        in_frame(cx, handle, |window, cx| {
            window.click(("close-terminal", terminal.0), cx)
        });
        if work == "upload" {
            wait_for_path(cx, handle, id, "/home/tester").await;
            in_frame(cx, handle, |window, cx| {
                assert_eq!(
                    workspace.read(cx).active_tab(),
                    Some(CenterTab::Explorer(id))
                );
                assert!(workspace.read(cx).file_sidebar(host).is_none());
                assert_eq!(workspace.read(cx).explorer(id).unwrap().entity_id(), entity);
                assert!(window.find(("explorer-tab", id.0)).visible());
                assert!(window.try_find(("local-pane", id.0)).is_none());
                assert!(store.read(cx).host(host).is_some());
                // A running transfer can stop after its terminal has gone;
                // the promoted tab must still expose its resume control.
                window.click("cancel-transfer", cx);
            });
            wait_for_queue_status(cx, handle, "已停止").await;
            in_frame(cx, handle, |window, cx| window.click("resume-transfer", cx));
            wait_for_queue_status(cx, handle, "0%").await;
            assert_eq!(servers.other.requests.lock().unwrap().len(), 1);
            assert_eq!(*servers.other.reconnects.lock().unwrap(), 0);
            assert_eq!(servers.connections_for(&address), 1);
            events
                .send_blocking(SftpEvent::Progress(TransferProgress::new(
                    TransferPhase::Completed,
                )))
                .unwrap();
            events.send_blocking(SftpEvent::Idle).unwrap();
            wait_for_queue_status(cx, handle, "已完成").await;
            in_frame(cx, handle, |window, cx| {
                assert!(store.read(cx).host(host).is_some());
                assert_eq!(workspace.read(cx).explorer(id).unwrap().entity_id(), entity);
                assert!(workspace.read(cx).file_sidebar(host).is_none());
                window.click(("close-explorer", id.0), cx);
            });
        } else if work == "editor" {
            cx.update(|cx| {
                assert!(store.read(cx).host(host).is_some());
                assert!(workspace.read(cx).file_sidebar(host).is_some());
            });
            in_frame(cx, handle, |window, cx| {
                window.click(("editor", 1u64), cx);
                window.dispatch_action(Box::new(CloseEditor(EditorId(1))), cx);
            });
        }
        cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
            window.render_frame(cx);
            store.read(cx).host(host).is_none() && weak.upgrade().is_none()
        })
        .await;
        cx.update(|cx| {
            assert!(workspace.read(cx).file_sidebar(host).is_none());
            assert!(workspace.read(cx).explorer(id).is_none());
        });
    }
}

#[gpui_kit::test]
async fn narrow_english_file_sidebar_keeps_long_queue_errors_inside_at_larger_text_sizes(
    cx: &mut TestAppContext,
) {
    use gpui_kit::component::{Theme, ThemeMode};
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let id = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    let settings = cx.update(|cx| workspace.read(cx).settings().clone());
    cx.update(|cx| {
        settings.update(cx, |settings, cx| {
            settings.update(
                |settings| settings.language = InterfaceLanguage::English,
                cx,
            )
        });
    });
    cx.run_until_parked();
    // A real queued upload can report a long reconnect error. Invalid
    // requests are rejected before enqueue, so they cannot test this layout.
    drop_file(
        cx,
        handle,
        id,
        None,
        "/tmp/a-very-long-source-file-name-that-exceeds-a-narrow-sidebar.txt",
    );
    in_frame(cx, handle, |window, _| {
        assert!(window.try_find("upload-confirm").is_none());
    });
    wait_for_uploads(cx, handle, &servers.web, 1).await;
    wait_for_queue_status(cx, handle, "Scanning").await;
    let note = "Connection lost while uploading the selected files; waiting to reconnect to the original server before continuing this transfer";
    servers.web.events.lock().unwrap()[0]
        .send_blocking(SftpEvent::Progress(
            TransferProgress::new(TransferPhase::Reconnecting).with_note(note),
        ))
        .unwrap();
    wait_for_queue_status(cx, handle, note).await;
    in_frame(cx, handle, |window, cx| {
        window.click(("queue-expand", 1u64), cx)
    });
    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        for zoom in [12., 20.] {
            in_frame(cx, handle, |window, cx| {
                Theme::change(mode, Some(window), cx);
                Theme::global_mut(cx).font_size = px(zoom);
                workspace
                    .read(cx)
                    .dock_area()
                    .clone()
                    .update(cx, |area, cx| {
                        area.set_dock_size(DockPlacement::Right, px(320.), window, cx);
                    });
                window.simulate_next_frame(cx);
            });
            in_frame(cx, handle, |window, _| {
                let sidebar = window.find(("file-sidebar", id.0)).bounds();
                assert!(
                    (sidebar.size.width - px(320.)).abs() < px(2.),
                    "{sidebar:?}"
                );
                for item in [("queue-entry", 1u64), ("transfer-detail-list", 1u64)] {
                    let bounds = window.find(item).bounds();
                    assert!(
                        bounds.left() >= sidebar.left() && bounds.right() <= sidebar.right(),
                        "{item:?} at {zoom}: {bounds:?} outside {sidebar:?}"
                    );
                }
                let status = window.find("transfer-status");
                assert!(status.visible());
                assert!(
                    status.bounds().left() >= sidebar.left()
                        && status.bounds().right() <= sidebar.right()
                );
                if zoom == 20. {
                    assert!(
                        status.bounds().size.height > px(20.),
                        "the long error must wrap"
                    );
                }
                let pane = window.find(("remote-pane", id.0)).bounds();
                for control in ["up", "home", "refresh", "bookmarks", "hidden-files"] {
                    let control = window.within(("remote-pane", id.0)).find(control);
                    if control.visible() {
                        assert!(
                            control.bounds().left() >= pane.left()
                                && control.bounds().right() <= pane.right()
                        );
                    }
                }
            });
        }
    }
    assert_eq!(servers.web.requests.lock().unwrap().len(), 1);
}

#[gpui_kit::test]
async fn a_temporary_hosts_stopped_sidebar_becomes_a_resumable_closable_file_tab(
    cx: &mut TestAppContext,
) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let address = "stopped-transfer.test";
    let (host, terminal, id) = show_temporary_files(cx, handle, &workspace, address).await;
    let entity = cx.update(|cx| workspace.read(cx).explorer(id).unwrap().entity_id());
    drop_file(cx, handle, id, None, "/tmp/stopped.txt");
    in_frame(cx, handle, |window, _| {
        assert!(window.try_find("upload-confirm").is_none());
    });
    wait_for_uploads(cx, handle, &servers.other, 1).await;
    in_frame(cx, handle, |window, cx| window.click("cancel-transfer", cx));
    wait_for_queue_status(cx, handle, "已停止").await;
    // This time the sidebar is still visible when its last terminal closes.
    in_frame(cx, handle, |window, cx| {
        // Dialog animations can leave no focused path. Closing must still
        // reach the workspace, without clicking a terminal or file first.
        window.blur(cx);
        assert!(window.focused(cx).is_none());
        // The sidebar and compose toolbar narrow the scrollable tab strip.
        // Bring the close button out of its clipped edge before clicking it.
        window.scroll(
            ("terminal-tab", terminal.0),
            gpui_kit::ScrollDelta::Pixels(point(px(-400.), px(0.))),
            cx,
        );
        window.render_frame(cx);
        let close = window.find(("close-terminal", terminal.0)).bounds();
        let toolbar = window
            .find(("toggle-terminal-compose", terminal.0))
            .bounds();
        assert!(
            close.right() < toolbar.left(),
            "close must be outside the toolbar"
        );
        assert!(window.focused(cx).is_none());
        window.click(("close-terminal", terminal.0), cx)
    });
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        workspace.read(cx).active_tab() == Some(CenterTab::Explorer(id))
            && workspace.read(cx).file_sidebar(host).is_none()
            && window.try_find(("terminal-tab", terminal.0)).is_none()
            && window.try_find(("explorer-tab", id.0)).is_some()
    })
    .await;
    in_frame(cx, handle, |window, cx| {
        assert_eq!(
            workspace.read(cx).active_tab(),
            Some(CenterTab::Explorer(id))
        );
        assert_eq!(workspace.read(cx).explorer(id).unwrap().entity_id(), entity);
        assert!(workspace.read(cx).file_sidebar(host).is_none());
        assert!(window.find(("explorer-tab", id.0)).visible());
        assert!(window.try_find(("local-pane", id.0)).is_none());
        assert_eq!(window.find(("queue-entry", 1u64)).label(), Some("已停止"));
        window.click(("close-explorer", id.0), cx);
    });
    in_frame(cx, handle, |window, cx| {
        assert!(window.find("ok").visible());
        window.click("cancel", cx);
    });
    in_frame(cx, handle, |window, cx| {
        window.click("resume-transfer", cx);
    });
    wait_for_queue_status(cx, handle, "0%").await;
    assert_eq!(servers.connections_for(address), 1);
    assert_eq!(servers.other.requests.lock().unwrap().len(), 1);
    assert_eq!(*servers.other.reconnects.lock().unwrap(), 0);
    // Stop once more, then explicitly discard the reachable tab.
    in_frame(cx, handle, |window, cx| window.click("cancel-transfer", cx));
    wait_for_queue_status(cx, handle, "已停止").await;
    in_frame(cx, handle, |window, cx| {
        window.blur(cx);
        assert!(window.focused(cx).is_none());
        window.click(("close-explorer", id.0), cx)
    });
    in_frame(cx, handle, |window, cx| window.click("ok", cx));
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        workspace.read(cx).explorer(id).is_none()
            && workspace.read(cx).store().read(cx).host(host).is_none()
    })
    .await;
}

#[gpui_kit::test]
async fn an_existing_sftp_tab_does_not_hide_a_temporary_hosts_promoted_transfer(
    cx: &mut TestAppContext,
) {
    let (handle, workspace, servers) =
        open_files_workspace(cx, Arc::new(FakeSftpProvider::default()));
    let address = "two-file-tabs.test";
    let (host, terminal, id) = show_temporary_files(cx, handle, &workspace, address).await;
    let entity = cx.update(|cx| workspace.read(cx).explorer(id).unwrap().entity_id());
    let events = servers.other.events.lock().unwrap().last().unwrap().clone();
    drop_file(cx, handle, id, None, "/tmp/still-running.txt");
    in_frame(cx, handle, |window, _| {
        assert!(window.try_find("upload-confirm").is_none());
    });
    wait_for_uploads(cx, handle, &servers.other, 1).await;
    in_frame(cx, handle, |window, cx| {
        window.dispatch_action(Box::new(OpenExplorer(host)), cx)
    });
    let standalone = cx.update(|cx| match workspace.read(cx).active_tab() {
        Some(CenterTab::Explorer(id)) => id,
        other => panic!("independent SFTP tab expected, got {other:?}"),
    });
    assert_ne!(standalone, id);
    wait_for_path(cx, handle, standalone, "/home/tester").await;
    assert_eq!(servers.connections_for(address), 2);
    in_frame(cx, handle, |window, cx| {
        assert!(window.find(("local-pane", standalone.0)).visible());
        window.click(("close-terminal", terminal.0), cx);
    });
    wait_for_path(cx, handle, id, "/home/tester").await;
    in_frame(cx, handle, |window, cx| {
        assert_eq!(
            workspace.read(cx).active_tab(),
            Some(CenterTab::Explorer(id))
        );
        assert_eq!(workspace.read(cx).explorer(id).unwrap().entity_id(), entity);
        assert!(workspace.read(cx).explorer(standalone).is_some());
        assert!(workspace.read(cx).file_sidebar(host).is_none());
        assert!(window.find(("explorer-tab", id.0)).visible());
        assert!(window.find(("explorer-tab", standalone.0)).visible());
        assert!(window.try_find(("local-pane", id.0)).is_none());
    });
    assert_eq!(
        servers.connections_for(address),
        2,
        "promotion reuses the original connection"
    );
    assert_eq!(servers.other.requests.lock().unwrap().len(), 1);
    events
        .send_blocking(SftpEvent::Progress(TransferProgress::new(
            TransferPhase::Completed,
        )))
        .unwrap();
    events.send_blocking(SftpEvent::Idle).unwrap();
    wait_for_queue_status(cx, handle, "已完成").await;
    in_frame(cx, handle, |window, cx| {
        window.click(("close-explorer", id.0), cx)
    });
    in_frame(cx, handle, |window, cx| {
        assert!(workspace.read(cx).explorer(id).is_none());
        assert!(workspace.read(cx).store().read(cx).host(host).is_some());
        window.click(("explorer-tab", standalone.0), cx);
    });
    in_frame(cx, handle, |window, cx| {
        assert!(window.find(("local-pane", standalone.0)).visible());
        window.click(("close-explorer", standalone.0), cx);
    });
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, cx| {
        window.render_frame(cx);
        workspace.read(cx).store().read(cx).host(host).is_none()
    })
    .await;
}

#[gpui_kit::test]
async fn directory_follow_diagnostic_reconnect_toolbar_without_focus(cx: &mut TestAppContext) {
    let terminals = Arc::new(DirectoryTerminals::default());
    let (handle, workspace, _) =
        open_files_workspace_with_terminals(cx, Arc::new(FakeSftpProvider::default()), terminals);
    show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    let generation = cx.update(|cx| {
        workspace
            .read(cx)
            .remote_terminal(RemoteTerminalId(INITIAL_WEB_TERMINAL))
            .unwrap()
            .read(cx)
            .connection_generation(cx)
    });
    in_frame(cx, handle, |window, cx| window.blur(cx));
    in_frame(cx, handle, |window, cx| {
        window.click(("reconnect", INITIAL_WEB_TERMINAL), cx)
    });
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, cx| {
        workspace
            .read(cx)
            .remote_terminal(RemoteTerminalId(INITIAL_WEB_TERMINAL))
            .unwrap()
            .read(cx)
            .connection_generation(cx)
            > generation
    })
    .await;
}

#[gpui_kit::test]
async fn directory_follow_diagnostic_path_blur_allows_new_reports(cx: &mut TestAppContext) {
    let terminals = Arc::new(DirectoryTerminals::default());
    let (handle, workspace, _) = open_files_workspace_with_terminals(
        cx,
        Arc::new(FakeSftpProvider::default()),
        terminals.clone(),
    );
    let web = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    let address = cx.update(|cx| {
        workspace
            .read(cx)
            .store()
            .read(cx)
            .host(HostId(WEB_01))
            .unwrap()
            .address
            .to_string()
    });
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, _| {
        terminals.outputs.lock().unwrap().len() == 2
    })
    .await;
    in_frame(cx, handle, |window, cx| {
        window
            .within(("remote-pane", web.0))
            .click("remote-path", cx);
    });
    in_frame(cx, handle, |window, cx| {
        window.input("/uncommitted", cx);
        window.click(("terminal", INITIAL_WEB_TERMINAL), cx);
    });
    terminals.report(INITIAL_WEB_TERMINAL, &address, "/after-path-blur");
    wait_for_path(cx, handle, web, "/after-path-blur").await;
}

#[gpui_kit::test]
async fn directory_follow_diagnostic_disconnected_sftp_blocks_reports(cx: &mut TestAppContext) {
    let terminals = Arc::new(DirectoryTerminals::default());
    let (handle, workspace, _) = open_files_workspace_with_terminals(
        cx,
        Arc::new(FakeSftpProvider::default()),
        terminals.clone(),
    );
    let web = show_files(cx, handle, &workspace, INITIAL_WEB_TERMINAL, WEB_01).await;
    let address = cx.update(|cx| {
        workspace
            .read(cx)
            .store()
            .read(cx)
            .host(HostId(WEB_01))
            .unwrap()
            .address
            .to_string()
    });
    in_frame(cx, handle, |window, cx| {
        window.click(("terminal", INITIAL_WEB_TERMINAL), cx);
        window.dispatch_action(Box::new(DisconnectHost(HostId(WEB_01))), cx);
    });
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, cx| {
        workspace
            .read(cx)
            .explorer(web)
            .unwrap()
            .read(cx)
            .connection_state()
            == ConnectionState::Disconnected
            && terminals.outputs.lock().unwrap().len() == 1
    })
    .await;
    in_frame(cx, handle, |window, cx| {
        window.click(("terminal", INITIAL_WEB_TERMINAL), cx);
        window.dispatch_action(
            Box::new(shellrs::app::ReconnectTerminal(RemoteTerminalId(
                INITIAL_WEB_TERMINAL,
            ))),
            cx,
        );
    });
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, _| {
        terminals.outputs.lock().unwrap().contains_key(&3)
    })
    .await;
    terminals.report(3, &address, "/reported-while-sftp-disconnected");
    cx.wait_for(handle.into(), Duration::from_secs(2), |_, cx| {
        workspace
            .read(cx)
            .remote_terminal(RemoteTerminalId(INITIAL_WEB_TERMINAL))
            .unwrap()
            .read(cx)
            .reported_directory(cx)
            .is_some_and(|report| report.path() == "/reported-while-sftp-disconnected")
    })
    .await;
    in_frame(cx, handle, |window, cx| {
        let explorer = workspace.read(cx).explorer(web).unwrap().read(cx);
        assert_eq!(explorer.connection_state(), ConnectionState::Disconnected);
        assert_ne!(
            explorer.remote().read(cx).path(),
            "/reported-while-sftp-disconnected"
        );
        assert!(window.find("file-sidebar-reconnect").visible());
        window.click("file-sidebar-reconnect", cx);
    });
    wait_for_path(cx, handle, web, "/reported-while-sftp-disconnected").await;
}
