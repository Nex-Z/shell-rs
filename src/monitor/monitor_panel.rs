use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    group_box::{GroupBox, GroupBoxVariants as _},
    h_flex,
    progress::{Progress, ProgressCircle},
    scroll::ScrollableElement as _,
    spinner::Spinner,
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MonitorDetail;
use super::gpu::{self, Device, Problem as GpuProblem};
use super::linux::{self, Parsed, Parts};
use super::model::{
    DiskUsage, InterfaceRate, Load, Memory, Snapshot, SystemInfo, Tracker, bars_per_row,
    format_rate,
};
use crate::app::{CatalogIcon, ToggleMonitorDetail};
use crate::i18n::{t, tn};
use crate::shared::{format_bytes, format_duration, format_percent};
use crate::terminal::{ExecResult, ExecTarget, RemoteTerminalId, exec_answer};

/// Time between readings of the load. The second comes sooner: the CPU's
/// load and the network's rates need two, and the panel should not sit on
/// dashes.
const INTERVAL: Duration = Duration::from_secs(2);
const SECOND_READING: Duration = Duration::from_secs(1);
/// Disks are read this seldom: they fill slowly, and `df` is the slowest
/// part of a reading.
const DISK_INTERVAL: Duration = Duration::from_secs(30);
const GPU_INTERVAL: Duration = Duration::from_secs(6);
const GPU_IDLE_INTERVAL: Duration = Duration::from_secs(30);
const GPU_TIMEOUT: Duration = Duration::from_secs(3);
const GPU_OUTPUT_LIMIT: usize = 64 * 1024;
/// 系统监控: the CPU, memory, network and disks of the host of the SSH
/// terminal in front, read every two seconds while the panel shows.
///
/// Each reading runs `linux::command` beside the shell, on the terminal's
/// own connection: no second login, nothing asked again. What does not
/// change while connected is asked once per connection, the disks every
/// half minute, so most readings start no process on the host but the `sh`
/// running them. Nothing runs on the host while the panel is hidden. What
/// each terminal's host showed last is kept, so coming back to a terminal
/// shows its numbers at once.
pub struct MonitorPanel {
    target: Option<ExecTarget>,
    active: bool,
    hosts: HashMap<RemoteTerminalId, HostState>,
    /// Every core's load, not only the first row of it.
    cores_unfolded: bool,
    /// How wide the row of per-core bars was last frame, which decides how
    /// many make the first row.
    cores_width: Rc<Cell<Option<Pixels>>>,
    /// Every interface, not only the main one.
    interfaces_unfolded: bool,
    /// The workspace's focus handle: the fold buttons dispatch on it, so
    /// they work whatever is focused.
    dispatch: FocusHandle,
    sampling: Option<Task<()>>,
    gpu_sampling: Option<Task<()>>,
}

#[derive(Default)]
struct HostState {
    tracker: Tracker,
    snapshot: Option<Snapshot>,
    /// With the terminal's connection it was read on: a reconnect may land
    /// on another machine.
    system: Option<(u64, SystemInfo)>,
    /// With when they were read.
    disks: Option<(Instant, Vec<DiskUsage>)>,
    problem: Option<Problem>,
    gpu: GpuState,
}

/// Lives inside the sampling task, so dropping it also cancels queued and
/// in-flight telemetry on the transport thread.
#[derive(Default)]
struct GpuCancellation(Arc<AtomicBool>);

impl GpuCancellation {
    fn flag(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}

impl Drop for GpuCancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// GPU failure is separate from the CPU/network reading. Transport failure
/// stops queries until reconnect: an unresponsive driver must not leave a
/// growing collection of remote processes or monopolize the exec queue.
#[derive(Default)]
struct GpuState {
    connection: Option<u64>,
    devices: Vec<Device>,
    problem: Option<String>,
    sampled_at: Option<Instant>,
    stopped: bool,
}

impl GpuState {
    fn interval(&self) -> Duration {
        if self.devices.is_empty()
            || self.problem.is_some()
            || self.devices.iter().all(|device| device.problem.is_some())
        {
            GPU_IDLE_INTERVAL
        } else {
            GPU_INTERVAL
        }
    }

    fn due(&self, connection: u64, now: Instant) -> bool {
        if self.connection != Some(connection) {
            return true;
        }
        !self.stopped
            && self
                .sampled_at
                .is_none_or(|at| now.saturating_duration_since(at) >= self.interval())
    }

    fn record(&mut self, connection: u64, result: ExecResult, now: Instant) {
        if self.connection != Some(connection) {
            *self = Self::default();
        }
        self.connection = Some(connection);
        self.sampled_at = Some(now);
        match result {
            Ok(output) => match gpu::parse(&output) {
                Ok(devices) => {
                    self.devices = devices;
                    self.problem = None;
                }
                Err(_) => {
                    self.devices.clear();
                    self.problem = Some(t!("monitor.gpu.invalid").to_string());
                }
            },
            Err(_) => {
                self.devices.clear();
                self.problem = Some(t!("monitor.gpu.failed").to_string());
                self.stopped = true;
            }
        }
    }
}

impl HostState {
    /// What the next reading on `connection` asks for besides the load.
    fn parts_due(&self, connection: u64, now: Instant) -> Parts {
        let system = self
            .system
            .as_ref()
            .is_none_or(|(read_on, _)| *read_on != connection);
        let disks = system
            || self
                .disks
                .as_ref()
                .is_none_or(|(at, _)| now.saturating_duration_since(*at) >= DISK_INTERVAL);
        Parts { system, disks }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Problem {
    /// The terminal is not connected, so there is no connection to read on.
    NotConnected,
    /// Not Linux; `uname -s` when the host said it.
    Unsupported(Option<String>),
    /// The command did not run, or did not finish.
    Failed(String),
}

/// What one try at a reading came back with.
enum Outcome {
    Output(ExecResult),
    NotConnected,
}

impl MonitorPanel {
    pub fn new(dispatch: FocusHandle) -> Self {
        Self {
            target: None,
            active: false,
            hosts: HashMap::new(),
            cores_unfolded: false,
            cores_width: Rc::new(Cell::new(None)),
            interfaces_unfolded: false,
            dispatch,
            sampling: None,
            gpu_sampling: None,
        }
    }

    /// Unfold a folded part, or fold it away again. Stays so for every
    /// terminal's host.
    pub fn toggle(&mut self, detail: MonitorDetail, cx: &mut Context<Self>) {
        let unfolded = match detail {
            MonitorDetail::Cores => &mut self.cores_unfolded,
            MonitorDetail::Interfaces => &mut self.interfaces_unfolded,
        };
        *unfolded = !*unfolded;
        cx.notify();
    }

    /// Read `target`'s host while `active`, the panel showing it.
    pub fn set_target(&mut self, target: Option<ExecTarget>, active: bool, cx: &mut Context<Self>) {
        if self.target == target && self.active == active {
            return;
        }
        self.target = target;
        self.active = active;
        self.restart(cx);
        cx.notify();
    }

    /// Start reading the target's host over, or stop when there is nothing
    /// to show. Dropping the task stops it; a reading still on its way is
    /// answered to no one.
    fn restart(&mut self, cx: &mut Context<Self>) {
        self.sampling = None;
        self.gpu_sampling = None;
        let Some(target) = self.target.clone().filter(|_| self.active) else {
            return;
        };
        self.start_gpu(target.clone(), cx);
        self.sampling = Some(cx.spawn(async move |this, cx| {
            let mut first = true;
            loop {
                let request = this.update(cx, |this, cx| {
                    let view = target.view.upgrade()?;
                    let view = view.read(cx);
                    let connection = view.generation(cx);
                    let now = cx.background_executor().now();
                    let parts = this.hosts.get(&target.terminal).map_or(
                        Parts {
                            system: true,
                            disks: true,
                        },
                        |host| host.parts_due(connection, now),
                    );
                    let reply = view.exec(linux::command(parts), cx)?;
                    Some((reply, connection))
                });
                let Ok(request) = request else { break };
                let (outcome, connection) = match request {
                    None => (Outcome::NotConnected, 0),
                    Some((reply, connection)) => (
                        exec_answer(reply, cx)
                            .await
                            .map_or(Outcome::NotConnected, Outcome::Output),
                        connection,
                    ),
                };
                if this
                    .update(cx, |this, cx| {
                        if target
                            .view
                            .upgrade()
                            .is_some_and(|view| view.read(cx).generation(cx) == connection)
                            || matches!(outcome, Outcome::NotConnected)
                        {
                            this.record(target.terminal, connection, outcome, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
                let pause = if first { SECOND_READING } else { INTERVAL };
                first = false;
                cx.background_executor().timer(pause).await;
            }
        }));
    }

    fn start_gpu(&mut self, target: ExecTarget, cx: &mut Context<Self>) {
        let cancellation = GpuCancellation::default();
        self.gpu_sampling = Some(cx.spawn(async move |this, cx| {
            loop {
                let request = this.update(cx, |this, cx| {
                    let view = target.view.upgrade()?;
                    let view = view.read(cx);
                    let connection = view.generation(cx);
                    let now = cx.background_executor().now();
                    let host = this.hosts.get(&target.terminal)?;
                    // Wait for a successful Linux reading on this connection.
                    if host.problem.is_some()
                        || !host
                            .system
                            .as_ref()
                            .is_some_and(|(id, _)| *id == connection)
                        || !host.gpu.due(connection, now)
                    {
                        return None;
                    }
                    let reply = view.exec_bounded(
                        gpu::command(),
                        GPU_TIMEOUT,
                        GPU_OUTPUT_LIMIT,
                        cancellation.flag(),
                        cx,
                    )?;
                    Some((reply, connection))
                });
                let Ok(request) = request else { break };
                if let Some((reply, connection)) = request {
                    let result = exec_answer(reply, cx).await;
                    if this
                        .update(cx, |this, cx| {
                            let current = target
                                .view
                                .upgrade()
                                .is_some_and(|view| view.read(cx).generation(cx) == connection);
                            if current && let Some(result) = result {
                                let now = cx.background_executor().now();
                                this.hosts
                                    .entry(target.terminal)
                                    .or_default()
                                    .gpu
                                    .record(connection, result, now);
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                // A local timer only. No remote request before the reading is due.
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        }));
    }

    fn record(
        &mut self,
        terminal: RemoteTerminalId,
        connection: u64,
        outcome: Outcome,
        cx: &mut Context<Self>,
    ) {
        let now = cx.background_executor().now();
        let host = self.hosts.entry(terminal).or_default();
        host.problem = match outcome {
            Outcome::NotConnected => Some(Problem::NotConnected),
            Outcome::Output(Err(error)) => Some(Problem::Failed(error)),
            Outcome::Output(Ok(output)) => match linux::parse(&output) {
                Parsed::Reading(mut reading) => {
                    if let Some(system) = reading.system.take() {
                        host.system = Some((connection, system));
                    }
                    if let Some(disks) = reading.disks.take() {
                        host.disks = Some((now, disks));
                    }
                    host.snapshot = Some(host.tracker.update(*reading, now));
                    None
                }
                Parsed::Unsupported(system) => Some(Problem::Unsupported(system)),
            },
        };
        cx.notify();
    }
}

impl Render for MonitorPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let host = self
            .target
            .as_ref()
            .and_then(|target| self.hosts.get(&target.terminal));
        let snapshot = host.and_then(|host| host.snapshot.as_ref());
        let system = host.and_then(|host| host.system.as_ref().map(|(_, system)| system));
        let disks = host.and_then(|host| host.disks.as_ref().map(|(_, disks)| disks.as_slice()));
        let problem = host.and_then(|host| host.problem.clone());
        let connection = self
            .target
            .as_ref()
            .and_then(|target| target.view.upgrade())
            .map(|view| view.read(cx).generation(cx));
        let gpu = host
            .filter(|host| host.gpu.connection == connection)
            .map(|host| &host.gpu);
        let interfaces_unfolded = self.interfaces_unfolded;
        let cores = CoresLayout {
            unfolded: self.cores_unfolded,
            per_row: self
                .cores_width
                .get()
                .map(|width| bars_per_row(f32::from(width), f32::from(window.rem_size()))),
            width: self.cores_width.clone(),
            panel: cx.entity_id(),
        };
        let dispatch = &self.dispatch;
        // The scrollbar goes on a layer beside the scrolled content, which
        // `overflow_y_scrollbar` lays out: on the content itself it would
        // scroll away with it, and stop matching where the content is.
        let content = v_flex()
            .size_full()
            .p_3()
            .gap_3()
            .map(|panel| match (snapshot, problem) {
                // Numbers from before a failed reading still say something;
                // the failure goes above them.
                (Some(snapshot), problem @ (None | Some(Problem::Failed(_)))) => panel
                    .when_some(problem, |panel, problem| {
                        panel.child(render_problem(&problem, cx))
                    })
                    .when_some(system, |panel, system| {
                        panel.child(render_system(system, snapshot.uptime, cx))
                    })
                    .child(render_cpu(system, snapshot, &cores, dispatch, cx))
                    .child(render_memory(&snapshot.memory, cx))
                    .when_some(gpu, |panel, gpu| {
                        let stale = gpu.sampled_at.is_some_and(|at| {
                            cx.background_executor().now().saturating_duration_since(at)
                                > GPU_INTERVAL + GPU_TIMEOUT
                        });
                        panel
                            .when(stale && !gpu.devices.is_empty(), |panel| {
                                panel.child(
                                    div()
                                        .id("monitor-gpu-stale")
                                        .test_support()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(t!("monitor.gpu.stale")),
                                )
                            })
                            .children(gpu.devices.iter().map(|device| render_gpu(device, cx)))
                            .when_some(gpu.problem.as_ref(), |panel, problem| {
                                panel.child(
                                    div()
                                        .id("monitor-gpu-error")
                                        .test_support()
                                        .aria_label(problem.clone())
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(problem.clone()),
                                )
                            })
                    })
                    .child(render_network(snapshot, interfaces_unfolded, dispatch, cx))
                    .when_some(disks, |panel, disks| panel.child(render_disks(disks, cx))),
                (_, Some(problem)) => panel.child(render_problem(&problem, cx)),
                (None, None) => panel.child(
                    h_flex()
                        .id("monitor-loading")
                        .test_support()
                        .gap_2()
                        .justify_center()
                        .py_8()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(Spinner::new().small())
                        .child(t!("tools.reading")),
                ),
            })
            .overflow_y_scrollbar();
        div()
            .id("monitor")
            .test_support()
            .size_full()
            .child(content)
    }
}

fn render_problem(problem: &Problem, cx: &App) -> impl IntoElement {
    let (text, color) = match problem {
        Problem::NotConnected => (
            t!("monitor.panel.not_connected"),
            cx.theme().muted_foreground,
        ),
        Problem::Unsupported(Some(system)) => (
            t!("monitor.panel.unsupported_system", system = system),
            cx.theme().muted_foreground,
        ),
        Problem::Unsupported(None) => {
            (t!("monitor.panel.unsupported"), cx.theme().muted_foreground)
        }
        Problem::Failed(error) => (t!("tools.read_failed", error = error), cx.theme().danger),
    };
    div()
        .id("monitor-message")
        .test_support()
        .aria_label(text.clone())
        .text_sm()
        .text_color(color)
        .child(text)
}

/// A card: its icon and title, with `meta` at the end of the title row.
fn card(
    id: &'static str,
    icon: impl Into<Icon>,
    title: impl Into<SharedString>,
    meta: Option<AnyElement>,
    cx: &App,
) -> GroupBox {
    GroupBox::new().id(id).fill().child(
        h_flex()
            .gap_2()
            .child(icon.into().small().text_color(cx.theme().muted_foreground))
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(title.into()),
            )
            .children(meta),
    )
}

/// A card's quiet note at the end of its title row: 「1 核」.
fn note(text: SharedString, cx: &App) -> AnyElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

/// In place of the note, when the card folds something away: 「8 核 ⌄」.
fn unfold_button(
    id: &'static str,
    label: SharedString,
    detail: MonitorDetail,
    unfolded: bool,
    tooltip: SharedString,
    dispatch: &FocusHandle,
) -> AnyElement {
    let dispatch = dispatch.clone();
    Button::new(id)
        .ghost()
        .xsmall()
        .label(label)
        .child(Icon::new(if unfolded {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        }))
        .toggled(unfolded)
        .tooltip(tooltip)
        .on_click(move |_, window, cx| {
            dispatch.dispatch_action(&ToggleMonitorDetail(detail), window, cx)
        })
        .into_any_element()
}

fn load_color(percent: f32, cx: &App) -> Hsla {
    match Load::of(percent) {
        Load::Normal => cx.theme().success,
        Load::High => cx.theme().warning,
        Load::Critical => cx.theme().danger,
    }
}

/// Numbers that change every reading: monospaced, so they hold still.
fn numeric(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .font_family(cx.theme().mono_font_family.clone())
        .child(text.into())
}

fn render_system(system: &SystemInfo, uptime: Option<Duration>, cx: &App) -> impl IntoElement {
    let field = |id: &'static str, label: SharedString, value: String| {
        v_flex()
            .min_w_0()
            .gap_0p5()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(
                div()
                    .id(id)
                    .test_support()
                    .aria_label(value.clone())
                    .text_sm()
                    .truncate()
                    .child(value),
            )
    };
    card(
        "monitor-system",
        CatalogIcon::Monitor,
        t!("monitor.system.title"),
        None,
        cx,
    )
    .child(
        div()
            .grid()
            .grid_cols(2)
            .gap_x_4()
            .gap_y_3()
            .child(field(
                "monitor-host",
                t!("monitor.system.host"),
                system.host_name.clone(),
            ))
            .child(field(
                "monitor-arch",
                t!("monitor.system.arch"),
                system.arch.clone(),
            ))
            .child(field(
                "monitor-os",
                t!("monitor.system.os"),
                system.os.clone(),
            ))
            .child(field(
                "monitor-uptime",
                t!("monitor.system.uptime"),
                uptime.map(format_duration).unwrap_or_default(),
            )),
    )
}

/// A ring with the share in its middle, as big as the eye needs to catch a
/// change across the room.
fn gauge(id: &'static str, percent: Option<f32>, cx: &App) -> impl IntoElement {
    let color = load_color(percent.unwrap_or(0.), cx);
    ProgressCircle::new(id)
        .value(percent.unwrap_or(0.))
        .color(color)
        .size_16()
        .child(
            v_flex()
                .items_center()
                .text_color(color)
                .child(
                    numeric(
                        percent.map_or("—".into(), |p| format!("{}", p.round() as u32)),
                        cx,
                    )
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD),
                )
                .child(div().text_xs().child("%")),
        )
}

/// A labelled bar: the label and the share above it.
fn meter(
    id: impl Into<SharedString>,
    label: SharedString,
    percent: Option<f32>,
    cx: &App,
) -> impl IntoElement {
    let id = id.into();
    let color = percent.map_or(cx.theme().muted_foreground, |value| load_color(value, cx));
    let text = percent.map_or("—".into(), format_percent);
    v_flex()
        .gap_1p5()
        .child(
            h_flex().justify_between().text_sm().child(label).child(
                numeric(text.clone(), cx)
                    .id(id.clone())
                    .test_support()
                    .aria_label(text)
                    .text_color(color)
                    .font_weight(FontWeight::SEMIBOLD),
            ),
        )
        .when_some(percent, |meter, percent| {
            meter.child(
                Progress::new(SharedString::from(format!("{id}-bar")))
                    .small()
                    .value(percent)
                    .color(color),
            )
        })
}

fn render_gpu(device: &Device, cx: &App) -> impl IntoElement {
    let id = format!("monitor-gpu:{}", device.id);
    let memory_percent = device
        .memory_used
        .zip(device.memory_total)
        .filter(|(used, total)| *total > 0 && used <= total)
        .map(|(used, total)| (used as f64 * 100. / total as f64) as f32);
    let memory = match (device.memory_used, device.memory_total) {
        (Some(used), Some(total)) => format!("{} / {}", format_bytes(used), format_bytes(total)),
        (Some(used), None) => format!("{} / —", format_bytes(used)),
        (None, Some(total)) => format!("— / {}", format_bytes(total)),
        _ => "—".to_string(),
    };
    let temperature = device
        .temperature
        .map_or("—".to_string(), |value| format!("{value:.0} °C"));
    let power = device
        .power
        .map_or("—".to_string(), |value| format!("{value:.1} W"));
    GroupBox::new()
        .id(SharedString::from(id.clone()))
        .fill()
        .child(
            h_flex()
                .gap_2()
                .min_w_0()
                .child(
                    Icon::new(IconName::Cpu)
                        .small()
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .truncate()
                        .child(device.name.clone()),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{} · {}", device.vendor, device.id)),
        )
        .child(meter(
            format!("{id}:usage"),
            t!("monitor.gpu.utilization"),
            device.utilization,
            cx,
        ))
        .child(
            v_flex()
                .gap_1p5()
                .child(meter(
                    format!("{id}:memory"),
                    t!("monitor.gpu.memory"),
                    memory_percent,
                    cx,
                ))
                .child(
                    numeric(memory.clone(), cx)
                        .id(SharedString::from(format!("{id}:amount")))
                        .test_support()
                        .aria_label(memory)
                        .text_xs(),
                ),
        )
        .child(
            h_flex()
                .justify_between()
                .gap_2()
                .text_xs()
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("monitor.gpu.temperature")),
                        )
                        .child(numeric(temperature, cx)),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .items_end()
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("monitor.gpu.power")),
                        )
                        .child(numeric(power, cx)),
                ),
        )
        .when_some(device.problem.as_ref(), |card, problem| {
            let text = match problem {
                GpuProblem::MissingTool => t!("monitor.gpu.missing_tool"),
                GpuProblem::PermissionDenied => t!("monitor.gpu.denied"),
                GpuProblem::ToolFailed => t!("monitor.gpu.tool_failed"),
                GpuProblem::MetricsUnavailable => t!("monitor.gpu.unavailable"),
            };
            card.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(text),
            )
        })
}

/// How the per-core bars lay out this frame.
struct CoresLayout {
    /// Every row, not only the first.
    unfolded: bool,
    /// How many bars make a row, from last frame's width; `None` until the
    /// row has been laid out once.
    per_row: Option<usize>,
    /// Where the row records its width for the next frame.
    width: Rc<Cell<Option<Pixels>>>,
    /// The panel to redraw when that width changes.
    panel: EntityId,
}

fn render_cpu(
    system: Option<&SystemInfo>,
    snapshot: &Snapshot,
    layout: &CoresLayout,
    dispatch: &FocusHandle,
    cx: &App,
) -> impl IntoElement {
    let model = system
        .map(|system| system.cpu_model.clone())
        .filter(|model| !model.is_empty());
    let total = snapshot.cpu.as_ref().map(|load| load.total);
    let cores = snapshot
        .cpu
        .as_ref()
        .map(|load| load.cores.as_slice())
        .filter(|cores| cores.len() > 1);
    // A host whose cores fill more than the one row shown can unfold the
    // rest; most hosts fit, and have no button.
    let more = layout
        .per_row
        .filter(|per_row| cores.is_some_and(|cores| cores.len() > *per_row));
    let count = tn!("monitor.cpu.cores", snapshot.cores);
    let meta = if more.is_some() {
        unfold_button(
            "monitor-cores-toggle",
            count,
            MonitorDetail::Cores,
            layout.unfolded,
            if layout.unfolded {
                t!("monitor.cpu.fold_cores")
            } else {
                t!("monitor.cpu.unfold_cores")
            },
            dispatch,
        )
    } else {
        note(count, cx)
    };
    let shown = cores.map(|cores| match more {
        Some(per_row) if !layout.unfolded => &cores[..per_row],
        _ => cores,
    });
    card("monitor-cpu", IconName::Cpu, "CPU", Some(meta), cx)
        .when_some(model, |card, model| {
            card.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(model),
            )
        })
        .child(
            h_flex()
                .gap_4()
                .child(gauge("monitor-cpu-gauge", total, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_3()
                        .child(meter(
                            "monitor-cpu-usage",
                            t!("monitor.cpu.average"),
                            total,
                            cx,
                        ))
                        .when_some(shown, |column, shown| {
                            column.child(render_cores(shown, layout, cx))
                        }),
                ),
        )
}

/// The cores' loads as a column each, wrapping onto more rows the more
/// there are: 64 take three rows at the sidebar's usual width. Folded, only
/// the first row; it measures its width for the next frame to know how many
/// make one. Pointing at a bar tells its core's number and share.
fn render_cores(cores: &[f32], layout: &CoresLayout, cx: &App) -> impl IntoElement {
    let core = |index: usize, load: f32| {
        t!(
            "monitor.cpu.core",
            index = index,
            load = format_percent(load)
        )
    };
    let label = cores
        .iter()
        .enumerate()
        .map(|(index, load)| core(index, *load))
        .reduce(|list, next| t!("monitor.cpu.core_list", list = list, core = next))
        .unwrap_or_default();
    let (width, panel) = (layout.width.clone(), layout.panel);
    h_flex()
        .id("monitor-cpu-cores")
        .test_support()
        .aria_label(label)
        .relative()
        .w_full()
        .flex_wrap()
        .gap_0p5()
        // Before the first measurement, a row's height keeps the rest out.
        .when(!layout.unfolded, |row| {
            row.max_h(rems(1.5)).overflow_hidden()
        })
        .children(cores.iter().enumerate().map(|(index, load)| {
            let color = load_color(*load, cx);
            let tooltip = core(index, *load);
            div()
                .id(("monitor-core", index as u64))
                .test_support()
                .relative()
                .w(rems(0.375))
                .h(rems(1.5))
                .rounded_sm()
                .bg(color.opacity(0.2))
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .w_full()
                        .h(relative(load.clamp(0., 100.) / 100.))
                        .rounded_sm()
                        .bg(color),
                )
        }))
        .child(
            canvas(
                move |bounds, window, _| {
                    if width.get() != Some(bounds.size.width) {
                        width.set(Some(bounds.size.width));
                        window.on_next_frame(move |_, cx| cx.notify(panel));
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
}

/// 「421.02 MB / 973.25 MB」, the total quieter.
fn amount(id: &'static str, used: u64, total: u64, cx: &App) -> impl IntoElement {
    let (used, total) = (format_bytes(used), format_bytes(total));
    h_flex()
        .id(id)
        .test_support()
        .aria_label(format!("{used} / {total}"))
        .gap_1()
        .text_xs()
        .child(numeric(used, cx))
        .child(numeric(format!("/ {total}"), cx).text_color(cx.theme().muted_foreground))
}

fn render_memory(memory: &Memory, cx: &App) -> impl IntoElement {
    card(
        "monitor-memory",
        IconName::MemoryStick,
        t!("monitor.memory.title"),
        None,
        cx,
    )
    .child(
        h_flex()
            .gap_4()
            .child(gauge("monitor-memory-gauge", Some(memory.percent()), cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_1p5()
                            .child(meter(
                                "monitor-memory-usage",
                                t!("monitor.memory.physical"),
                                Some(memory.percent()),
                                cx,
                            ))
                            .child(amount(
                                "monitor-memory-amount",
                                memory.used,
                                memory.total,
                                cx,
                            )),
                    )
                    .when(memory.swap_total > 0, |column| {
                        column.child(
                            v_flex()
                                .gap_1p5()
                                .child(meter(
                                    "monitor-swap-usage",
                                    t!("monitor.memory.swap"),
                                    Some(memory.swap_percent()),
                                    cx,
                                ))
                                .child(amount(
                                    "monitor-swap-amount",
                                    memory.swap_used,
                                    memory.swap_total,
                                    cx,
                                )),
                        )
                    }),
            ),
    )
}

fn render_network(
    snapshot: &Snapshot,
    unfolded: bool,
    dispatch: &FocusHandle,
    cx: &App,
) -> impl IntoElement {
    let busy = snapshot.busy_interfaces();
    let meta = (busy.len() > 1).then(|| {
        unfold_button(
            "monitor-interfaces-toggle",
            tn!("monitor.network.interfaces", busy.len()),
            MonitorDetail::Interfaces,
            unfolded,
            if unfolded {
                t!("monitor.network.fold")
            } else {
                t!("monitor.network.unfold")
            },
            dispatch,
        )
    });
    let shown: Vec<&InterfaceRate> = if unfolded {
        busy
    } else {
        snapshot.main_interface().into_iter().collect()
    };
    let rate = |rate: Option<f64>| rate.map_or("—".into(), format_rate);
    card(
        "monitor-network",
        IconName::Network,
        t!("monitor.network.title"),
        meta,
        cx,
    )
    .when(shown.is_empty(), |card| {
        card.child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(t!("monitor.network.none")),
        )
    })
    .children(shown.into_iter().map(|interface| {
        let (up, down) = (rate(interface.transmitted), rate(interface.received));
        h_flex()
            .id(SharedString::from(format!(
                "monitor-interface:{}",
                interface.name
            )))
            .test_support()
            .aria_label(t!("monitor.network.label", up = up, down = down))
            .gap_3()
            .text_sm()
            .child(
                numeric(interface.name.clone(), cx)
                    .flex_1()
                    .min_w_0()
                    .truncate(),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_3()
                    .text_xs()
                    .child(direction(IconName::ArrowUp, up, cx))
                    .child(direction(IconName::ArrowDown, down, cx)),
            )
    }))
}

fn direction(icon: IconName, rate: String, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_0p5()
        .child(
            Icon::new(icon)
                .xsmall()
                .text_color(cx.theme().muted_foreground),
        )
        .child(numeric(rate, cx))
}

fn render_disks(disks: &[DiskUsage], cx: &App) -> impl IntoElement {
    card(
        "monitor-disks",
        IconName::HardDrive,
        t!("monitor.disks.title"),
        None,
        cx,
    )
    .when(disks.is_empty(), |card| {
        card.child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(t!("monitor.disks.none")),
        )
    })
    .children(disks.iter().map(|disk| {
        let percent = disk.percent();
        let color = load_color(percent, cx);
        let (used, total) = (format_bytes(disk.used), format_bytes(disk.total));
        v_flex()
            .id(SharedString::from(format!("monitor-disk:{}", disk.mount)))
            .test_support()
            .aria_label(t!(
                "monitor.disks.label",
                percent = format_percent(percent),
                used = used,
                total = total
            ))
            .gap_1p5()
            .child(
                h_flex()
                    .gap_3()
                    .text_sm()
                    .child(
                        numeric(disk.mount.clone(), cx)
                            .flex_1()
                            .min_w_0()
                            .truncate(),
                    )
                    .child(
                        numeric(format_percent(percent), cx)
                            .flex_shrink_0()
                            .text_color(color)
                            .font_weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(
                Progress::new(SharedString::from(format!(
                    "monitor-disk-bar:{}",
                    disk.mount
                )))
                .small()
                .value(percent)
                .color(color),
            )
            .child(
                numeric(format!("{used} / {total}"), cx)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground),
            )
    }))
}

#[cfg(test)]
mod gpu_tests {
    use super::{GPU_IDLE_INTERVAL, GPU_INTERVAL, GpuCancellation, GpuState};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    const EMPTY: &str = "@@shellrs-gpu-v1\n@@nvidia-begin\n@@nvidia-status=missing\n@@sysfs-begin\n@@sysfs-status=ok\n@@shellrs-gpu-end\n";
    const ONE: &str = "@@shellrs-gpu-v1\n@@nvidia-begin\n0000:01:00.0, Test GPU, 50, 1024, 2048, 60, 100\n@@nvidia-status=0\n@@sysfs-begin\n@@sysfs-status=ok\n@@shellrs-gpu-end\n";

    #[test]
    fn dropping_a_gpu_task_cancels_its_outstanding_requests() {
        let cancellation = GpuCancellation::default();
        let request_flag = cancellation.flag();
        assert!(!request_flag.load(Ordering::Acquire));
        drop(cancellation);
        assert!(request_flag.load(Ordering::Acquire));
    }

    #[test]
    fn gpu_sampling_has_a_slower_cadence_and_empty_backoff() {
        let at = Instant::now();
        let mut state = GpuState::default();
        assert!(state.due(1, at));
        state.record(1, Ok(ONE.into()), at);
        assert!(!state.due(1, at + Duration::from_secs(5)));
        assert!(state.due(1, at + GPU_INTERVAL));
        state.record(1, Ok(EMPTY.into()), at);
        assert!(state.devices.is_empty());
        assert!(state.problem.is_none());
        assert!(!state.due(1, at + GPU_INTERVAL));
        assert!(state.due(1, at + GPU_IDLE_INTERVAL));
    }

    #[test]
    fn gpu_transport_failure_stops_until_a_new_connection() {
        let at = Instant::now();
        let mut state = GpuState::default();
        state.record(1, Ok(ONE.into()), at);
        state.record(1, Err("timed out".into()), at);
        assert!(
            state.devices.is_empty(),
            "old numbers are not shown as current"
        );
        assert!(state.problem.is_some());
        assert!(!state.due(1, at + Duration::from_secs(3600)));
        assert!(state.due(2, at));
        state.record(2, Ok(EMPTY.into()), at);
        assert!(!state.stopped);
        assert!(state.problem.is_none());
    }

    #[test]
    fn malformed_gpu_output_is_not_an_empty_device_list() {
        let at = Instant::now();
        let mut state = GpuState::default();
        state.record(1, Ok("truncated".into()), at);
        assert!(state.problem.is_some());
        assert!(!state.stopped);
        assert!(!state.due(1, at + GPU_INTERVAL));
        assert!(state.due(1, at + GPU_IDLE_INTERVAL));
    }
}
