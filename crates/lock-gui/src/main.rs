//! `lock-gui`: a live window onto the lock queue, laid out like the macOS app.

mod theme;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Application, AssetSource, Bounds, ClickEvent, ClipboardItem, Context,
    FocusHandle, FontFeatures, FontWeight, KeyBinding, Menu, MenuItem, MouseButton, Pixels, Point,
    Rgba, ScrollHandle, SharedString, TitlebarOptions, Window, WindowBounds, WindowDecorations,
    WindowOptions, actions, anchored, deferred, div, prelude::*, px, relative, rgba, size, svg,
};
use lock::{
    Finished, Kind, Outcome, State, Task, TaskId, TaskState, fmt_ago, fmt_duration_ms, now_ms,
    tildify,
};
use theme::{Fonts, OmarchyColors, Theme};

/// Polling also keeps the elapsed times ticking. There's deliberately no animation (no
/// spinner, no indeterminate bar): it would redraw the window every frame, which is
/// exactly the kind of background load lock is meant to keep off benchmarks.
const REFRESH: Duration = Duration::from_millis(500);
const TEXT: f32 = 13.;
const SMALL: f32 = 11.;
/// Leading space before the text column: row padding + icon + gap. Placeholders line up with titles.
const TEXT_INSET: f32 = 6. + 20. + 10.;
/// What the default timeout's stepper steps through.
const TIMEOUTS_S: [u64; 14] = [1, 2, 3, 5, 10, 15, 20, 30, 45, 60, 120, 300, 600, 1800];

#[cfg(target_os = "macos")]
const REVEAL: &str = "Show in Finder";
#[cfg(not(target_os = "macos"))]
const REVEAL: &str = "Open Folder";

actions!(
    lock_gui,
    [
        SelectNext,
        SelectPrevious,
        Stop,
        Reveal,
        CopyCommand,
        CopyPid,
        OpenSettings,
        Dismiss,
        Confirm,
        CloseWindow,
        Quit,
    ]
);

/// Small line icons, drawn as masks in the current text color.
struct Icons;

const STROKE: &str = r#"fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round""#;
const CIRCLE: &str = r#"<circle cx="8" cy="8" r="6.3"/>"#;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let body = match path {
            "running" => format!(r#"{CIRCLE}<path d="M6.7 5.6v4.8l3.8-2.4z" fill="black"/>"#),
            "waiting" => format!(r#"{CIRCLE}<path d="M8 4.7V8l2.2 1.4"/>"#),
            "ok" => format!(r#"{CIRCLE}<path d="M5.3 8.2l1.8 1.8 3.6-3.8"/>"#),
            "failed" | "stop" => format!(r#"{CIRCLE}<path d="M5.9 5.9l4.2 4.2M10.1 5.9l-4.2 4.2"/>"#),
            "cancelled" => format!(r#"{CIRCLE}<path d="M5.2 8h5.6"/>"#),
            "timed-out" => r#"<path d="M8 1.9l6.4 11.3H1.6z"/><path d="M8 6.3v3M8 11.3v.1"/>"#.into(),
            "folder" => r#"<path d="M1.8 4.2c0-.6.4-1 1-1h3.1l1.5 1.6h5.8c.6 0 1 .4 1 1v6.9c0 .6-.4 1-1 1H2.8c-.6 0-1-.4-1-1z"/>"#.into(),
            "check" => r#"<path d="M3.8 8.3l2.7 2.7 5.7-6"/>"#.into(),
            "minus" => r#"<path d="M4 8h8"/>"#.into(),
            "plus" => r#"<path d="M4 8h8M8 4v8"/>"#.into(),
            "settings" => r#"<path d="M2.5 4.5h11M2.5 8h11M2.5 11.5h11"/><circle cx="5.5" cy="4.5" r="1.4" fill="white"/><circle cx="10.5" cy="8" r="1.4" fill="white"/><circle cx="6.5" cy="11.5" r="1.4" fill="white"/>"#.into(),
            "cpu" => r#"<rect x="3.5" y="3.5" width="9" height="9" rx="1"/><rect x="6" y="6" width="4" height="4"/><path d="M6 1.5v2M10 1.5v2M6 12.5v2M10 12.5v2M1.5 6h2M1.5 10h2M12.5 6h2M12.5 10h2"/>"#.into(),
            _ => return Ok(None),
        };
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" {STROKE}>{body}</svg>"#
        );
        Ok(Some(Cow::Owned(svg.into_bytes())))
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// One row of the list, as in the macOS app's table.
enum Row {
    Header(&'static str, Option<String>),
    Placeholder(&'static str),
    Task(TaskId),
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Running,
    Waiting,
    Finished,
}

enum Modal {
    ConfirmStop {
        id: TaskId,
        title: String,
        body: String,
        waiting: bool,
    },
    Error {
        title: &'static str,
        message: String,
    },
    Settings,
}

struct LockView {
    state: State,
    error: Option<String>,
    omarchy: OmarchyColors,
    fonts: Fonts,
    focus: FocusHandle,
    scroll: ScrollHandle,
    selected: Option<TaskId>,
    /// A right-clicked row's menu, and where it was opened.
    menu: Option<(TaskId, Point<Pixels>)>,
    modal: Option<Modal>,
    /// Bumped when a settings change starts and when it lands. A read of the state file
    /// that overlapped either is dropped, so it can't put back the old value for a moment.
    writes: u64,
    /// The jobserver size to turn back on with, while it's off.
    jobs_when_on: u32,
}

impl LockView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_window_appearance(window, |_, _, cx| cx.notify())
            .detach();
        cx.spawn(async move |this, cx| {
            let mut omarchy = OmarchyColors::default();
            loop {
                let Ok(writes) = this.read_with(cx, |view, _| view.writes) else {
                    break;
                };
                let (next, colors) = cx
                    .background_executor()
                    .spawn(async move { (lock::read_state(), omarchy.reload()) })
                    .await;
                omarchy = colors.clone();
                let alive = this.update(cx, |view, cx| {
                    match next {
                        Ok(state) if view.writes == writes => {
                            if state.jobserver_tokens > 0 {
                                view.jobs_when_on = state.jobserver_tokens;
                            }
                            view.state = state;
                            view.error = None;
                        }
                        Ok(_) => {}
                        Err(e) => view.error = Some(e.to_string()),
                    }
                    view.omarchy = colors;
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(REFRESH).await;
            }
        })
        .detach();
        let focus = cx.focus_handle();
        window.focus(&focus);
        LockView {
            state: State::default(),
            error: None,
            omarchy: OmarchyColors::default().reload(),
            fonts: Fonts::detect(),
            focus,
            scroll: ScrollHandle::new(),
            selected: None,
            menu: None,
            modal: None,
            writes: 0,
            jobs_when_on: lock::default_jobserver_tokens(),
        }
    }

    fn rows(&self) -> Vec<Row> {
        let running: Vec<&Task> = self.state.running().collect();
        let waiting: Vec<&Task> = self.state.waiting().collect();
        let exclusive = running
            .iter()
            .any(|t| t.spec.kind == Kind::Exclusive && !t.spec.light);
        let capacity = if exclusive {
            "Exclusive".to_string()
        } else {
            // Light tasks and tasks nested in another task's lease take no slot.
            format!(
                "{} of {} slots",
                self.state.slot_holders().count(),
                self.state.shared_slots
            )
        };
        let mut rows = vec![Row::Header("Running", Some(capacity))];
        if running.is_empty() {
            rows.push(Row::Placeholder("Nothing running"));
        }
        rows.extend(running.iter().map(|t| Row::Task(t.id)));
        rows.push(Row::Header(
            "Queue",
            (!waiting.is_empty()).then(|| waiting.len().to_string()),
        ));
        if waiting.is_empty() {
            rows.push(Row::Placeholder("No one is waiting"));
        }
        rows.extend(waiting.iter().map(|t| Row::Task(t.id)));
        if !self.state.history.is_empty() {
            rows.push(Row::Header("Recent", None));
            rows.extend(self.state.history.iter().map(|f| Row::Task(f.task.id)));
        }
        rows
    }

    /// The live task if there is one, otherwise its history entry.
    fn find(&self, id: TaskId) -> Option<(&Task, Phase)> {
        if let Some(task) = self.state.get(id) {
            let phase = match task.state {
                TaskState::Running => Phase::Running,
                TaskState::Waiting => Phase::Waiting,
            };
            return Some((task, phase));
        }
        self.state
            .history
            .iter()
            .find(|f| f.task.id == id)
            .map(|f| (&f.task, Phase::Finished))
    }

    /// For actions: the right-clicked task if its menu is open, otherwise the selection.
    fn target(&self) -> Option<(&Task, Phase)> {
        self.menu
            .map(|(id, _)| id)
            .or(self.selected)
            .and_then(|id| self.find(id))
    }

    fn select(&mut self, id: TaskId, cx: &mut Context<Self>) {
        self.selected = Some(id);
        if let Some(ix) = self
            .rows()
            .iter()
            .position(|r| matches!(r, Row::Task(t) if *t == id))
        {
            self.scroll.scroll_to_item(ix);
        }
        cx.notify();
    }

    fn move_selection(&mut self, forward: bool, cx: &mut Context<Self>) {
        let ids: Vec<TaskId> = self
            .rows()
            .iter()
            .filter_map(|r| {
                if let Row::Task(id) = r {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();
        let current = self
            .selected
            .and_then(|id| ids.iter().position(|&i| i == id));
        let next = match (current, forward) {
            (None, true) => ids.first(),
            (None, false) => ids.last(),
            (Some(i), true) => ids.get(i + 1).or(ids.last()),
            (Some(i), false) => ids.get(i.saturating_sub(1)),
        };
        if let Some(&id) = next {
            self.select(id, cx);
        }
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        // Actions from the menu run first; it closes once they have.
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    fn ask_to_stop(&mut self, id: TaskId, cx: &mut Context<Self>) {
        let Some((task, phase)) = self.find(id) else {
            return;
        };
        if phase == Phase::Finished {
            return;
        }
        let waiting = phase == Phase::Waiting;
        let title = task.spec.title();
        let who = task
            .spec
            .agent
            .as_ref()
            .map(|a| format!(" started by {a}"))
            .unwrap_or_default();
        self.modal = Some(Modal::ConfirmStop {
            id,
            title: if waiting {
                format!("Remove “{title}” from the queue?")
            } else {
                format!("Stop “{title}”?")
            },
            body: if waiting {
                format!("The command{who} won’t run.")
            } else {
                format!(
                    "The command{who} in {} will be terminated and its lock released.",
                    task.spec.location()
                )
            },
            waiting,
        });
        cx.notify();
    }

    fn stop(&mut self, id: TaskId, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { lock::cancel(id) })
                .await;
            if let Err(e) = result {
                this.update(cx, |view, cx| {
                    view.modal = Some(Modal::Error {
                        title: "Couldn’t stop the task",
                        message: e.to_string(),
                    });
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn reveal(&self, task: &Task, cx: &mut App) {
        let path = PathBuf::from(task.spec.repo.as_deref().unwrap_or(&task.spec.cwd));
        if cfg!(target_os = "macos") {
            cx.reveal_path(&path);
        } else {
            cx.open_with_system(&path);
        }
    }

    /// Applies a settings change here straight away and to the state file in the background.
    fn change_settings(
        &mut self,
        change: impl Fn(&mut State) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        change(&mut self.state);
        self.writes += 1;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { lock::with_state(change) })
                .await;
            this.update(cx, |view, cx| {
                view.writes += 1;
                if let Err(e) = result {
                    view.modal = Some(Modal::Error {
                        title: "Couldn’t change the setting",
                        message: e.to_string(),
                    });
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_slots(&mut self, slots: u32, cx: &mut Context<Self>) {
        self.change_settings(move |s| s.shared_slots = slots, cx);
    }

    /// Like `lock jobs`: tasks already running keep the pool they have; the next one starts a new pool.
    fn set_jobs(&mut self, tokens: u32, cx: &mut Context<Self>) {
        if tokens > 0 {
            self.jobs_when_on = tokens;
        }
        self.change_settings(
            move |s| {
                s.jobserver_tokens = tokens;
                if let Some(pool) = s.pool.as_mut() {
                    pool.retired = true;
                }
            },
            cx,
        );
    }

    fn step_timeout(&mut self, forward: bool, cx: &mut Context<Self>) {
        let current = self.state.default_timeout_ms / 1000;
        let next = if forward {
            TIMEOUTS_S.iter().find(|&&s| s > current)
        } else {
            TIMEOUTS_S.iter().rev().find(|&&s| s < current)
        };
        if let Some(&s) = next {
            self.change_settings(move |state| state.default_timeout_ms = s * 1000, cx);
        }
    }

    // Actions

    fn on_select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_none() {
            self.move_selection(true, cx);
        }
    }

    fn on_select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_none() {
            self.move_selection(false, cx);
        }
    }

    fn on_stop(&mut self, _: &Stop, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.target().map(|(t, _)| t.id) {
            self.ask_to_stop(id, cx);
        }
        self.close_menu(cx);
    }

    fn on_reveal(&mut self, _: &Reveal, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((task, _)) = self.target() {
            self.reveal(task, cx);
        }
        self.close_menu(cx);
    }

    fn on_copy_command(&mut self, _: &CopyCommand, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((task, _)) = self.target() {
            cx.write_to_clipboard(ClipboardItem::new_string(task.spec.command.join(" ")));
        }
        self.close_menu(cx);
    }

    fn on_copy_pid(&mut self, _: &CopyPid, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((task, _)) = self.target() {
            cx.write_to_clipboard(ClipboardItem::new_string(display_pid(task).to_string()));
        }
        self.close_menu(cx);
    }

    fn on_open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.modal = Some(Modal::Settings);
        cx.notify();
    }

    fn on_dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            self.menu = None;
        } else if self.modal.is_some() {
            self.modal = None;
        } else {
            self.selected = None;
        }
        cx.notify();
    }

    fn on_confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        match self.modal.take() {
            Some(Modal::ConfirmStop { id, .. }) => self.stop(id, cx),
            Some(_) => {}
            None => self.on_reveal(&Reveal, window, cx),
        }
        cx.notify();
    }

    fn on_close_window(&mut self, _: &CloseWindow, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }
}

fn display_pid(task: &Task) -> u32 {
    task.child.map_or(task.owner.pid, |c| c.pid)
}

impl Render for LockView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::new(window.appearance(), &self.omarchy, &self.fonts);
        let now = now_ms();
        // The selection follows its task between sections, and goes when the task leaves history.
        if self.selected.is_some_and(|id| self.find(id).is_none()) {
            self.selected = None;
        }
        if self.menu.is_some_and(|(id, _)| self.find(id).is_none()) {
            self.menu = None;
        }

        let mut root = div()
            .track_focus(&self.focus)
            .key_context("LockView")
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_select_previous))
            .on_action(cx.listener(Self::on_stop))
            .on_action(cx.listener(Self::on_reveal))
            .on_action(cx.listener(Self::on_copy_command))
            .on_action(cx.listener(Self::on_copy_pid))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_dismiss))
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_close_window))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.p.bg)
            .text_color(t.p.text)
            .text_size(px(TEXT));
        if let Some(font) = &t.font {
            root = root.font_family(font.clone());
        }
        // Tabular figures, so times ticking over don't shift the text after them, like the
        // macOS app's monospacedDigitSystemFont. Monospace fonts have them anyway.
        root.text_style()
            .get_or_insert_with(Default::default)
            .font_features = Some(FontFeatures(Arc::new(vec![("tnum".into(), 1)])));
        // The macOS app puts this in the window's subtitle.
        if let Some(error) = &self.error {
            root = root.child(
                div()
                    .px_4()
                    .py_2()
                    .text_color(t.p.bad)
                    .child(format!("Couldn't read the queue: {error}")),
            );
        }

        let state = &self.state;
        if state.tasks.is_empty() && state.history.is_empty() {
            root = root.child(empty_state(&t, cx));
        } else {
            let rows: Vec<AnyElement> = self
                .rows()
                .into_iter()
                .enumerate()
                .map(|(i, row)| match row {
                    Row::Header(title, detail) => {
                        // Settings are in the macOS app's menus; here they're by the first header.
                        let trailing = (i == 0).then(|| {
                            icon_button(&t, "settings", "settings", "Settings")
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.on_open_settings(&OpenSettings, window, cx)
                                }))
                                .into_any_element()
                        });
                        header(&t, title, detail, trailing)
                    }
                    Row::Placeholder(text) => placeholder(&t, text),
                    Row::Task(id) => {
                        let (task, phase) = self.find(id).expect("rows come from the state");
                        let finished = state.history.iter().find(|f| f.task.id == id);
                        let position = state
                            .waiting()
                            .position(|w| w.id == id)
                            .map_or(0, |i| i + 1);
                        let selected =
                            self.selected == Some(id) || self.menu.is_some_and(|(m, _)| m == id);
                        task_row(&t, task, phase, finished, position, selected, now, cx)
                    }
                })
                .collect();
            root = root.child(
                div()
                    .id("list")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px(px(10.))
                    .pb(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(rows),
            );
        }

        if let Some((id, position)) = self.menu
            && let Some((_, phase)) = self.find(id)
        {
            root = root.child(context_menu(&t, phase, position, cx));
        }
        if let Some(modal) = &self.modal {
            root = root.child(modal_overlay(&t, modal, &self.state, self.jobs_when_on, cx));
        }
        root
    }
}

fn icon(name: &'static str, size: f32, color: Rgba) -> gpui::Svg {
    svg()
        .path(name)
        .flex_none()
        .size(px(size))
        .text_color(color)
}

/// Shown when there's nothing running, queued or in recent history.
fn empty_state(t: &Theme, cx: &mut Context<LockView>) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .p_5()
        .child(icon("cpu", 44., t.p.tertiary).mb(px(6.)))
        .child(
            div()
                .text_size(px(17.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.p.secondary)
                .child("No Tasks"),
        )
        .child(
            div()
                .text_color(t.p.tertiary)
                .text_center()
                .child("Commands started with lock will appear here."),
        )
        .child(
            text_button(t, "empty-settings", "Settings…", ButtonStyle::Normal)
                .mt(px(12.))
                .on_click(cx.listener(|view, _, window, cx| {
                    view.on_open_settings(&OpenSettings, window, cx)
                })),
        )
}

/// "Running  2 of 3 slots". Sits a little low in its row, closer to the rows it introduces.
fn header(
    t: &Theme,
    title: &'static str,
    detail: Option<String>,
    trailing: Option<AnyElement>,
) -> AnyElement {
    div()
        .flex_none()
        .h(px(28.))
        .mt(px(4.))
        .px(px(6.))
        .flex()
        .items_end()
        .pb(px(3.))
        .gap(px(6.))
        .child(div().font_weight(FontWeight::BOLD).child(title))
        .children(detail.map(|d| div().text_color(t.p.tertiary).child(d)))
        .child(div().flex_1())
        .children(trailing)
        .into_any_element()
}

fn placeholder(t: &Theme, text: &'static str) -> AnyElement {
    div()
        .flex_none()
        .h(px(30.))
        .pl(px(TEXT_INSET))
        .flex()
        .items_center()
        .text_color(t.p.tertiary)
        .child(text)
        .into_any_element()
}

fn icon_button(
    t: &Theme,
    id: impl Into<gpui::ElementId>,
    icon_name: &'static str,
    tooltip: &'static str,
) -> gpui::Stateful<gpui::Div> {
    let p = t.p;
    div()
        .id(id)
        .group("icon-button")
        .flex_none()
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(t.radius.min(px(4.)))
        .cursor_pointer()
        .hover(move |s| s.bg(p.fill))
        .child(
            icon(icon_name, 15., p.tertiary)
                .group_hover("icon-button", move |s| s.text_color(p.text)),
        )
        .tooltip(Tooltip::builder(t, tooltip.into()))
}

#[derive(Clone, Copy, PartialEq)]
enum ButtonStyle {
    Normal,
    Default,
    Destructive,
}

fn text_button(
    t: &Theme,
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    style: ButtonStyle,
) -> gpui::Stateful<gpui::Div> {
    let p = t.p;
    let (bg, fg) = match style {
        ButtonStyle::Normal => (p.fill, p.text),
        ButtonStyle::Default => (p.accent, p.bg),
        ButtonStyle::Destructive => (p.bad, p.bg),
    };
    div()
        .id(id)
        .flex_none()
        .min_w(px(72.))
        .px(px(12.))
        .py(px(4.))
        .flex()
        .justify_center()
        .rounded(t.radius.min(px(6.)))
        .bg(bg)
        .text_color(fg)
        .font_weight(if style == ButtonStyle::Normal {
            FontWeight::NORMAL
        } else {
            FontWeight::MEDIUM
        })
        .cursor_pointer()
        .hover(|s| s.opacity(0.85))
        .child(label)
}

/// Two-line row: status icon, title (+ tag), optional progress bar, detail line, and an action button.
#[allow(clippy::too_many_arguments)]
fn task_row(
    t: &Theme,
    task: &Task,
    phase: Phase,
    finished: Option<&Finished>,
    position: usize,
    selected: bool,
    now: u64,
    cx: &mut Context<LockView>,
) -> AnyElement {
    let p = t.p;
    let id = task.id;
    let mut parts: Vec<String> = Vec::new();
    let mut bar = None;
    let (status, status_color) = match (phase, finished) {
        (Phase::Running, _) => {
            let elapsed = task.elapsed_ms(now);
            // Progress is judged against how long this task usually takes, not the timeout,
            // which is only an upper bound. No history means no bar.
            match task.expected_ms.filter(|&ms| ms > 0) {
                Some(expected) if elapsed < expected => {
                    bar = Some((elapsed as f32 / expected as f32, p.accent));
                    parts.push(format!(
                        "{} of ~{}",
                        fmt_duration_ms(elapsed),
                        fmt_duration_ms(expected)
                    ));
                }
                Some(expected) => {
                    bar = Some((1., p.warn));
                    parts.push(format!(
                        "{}, usually ~{}",
                        fmt_duration_ms(elapsed),
                        fmt_duration_ms(expected)
                    ));
                }
                None => parts.push(fmt_duration_ms(elapsed)),
            }
            let started = task.started_at_ms.unwrap_or(now);
            let waited = started.saturating_sub(task.enqueued_at_ms);
            if waited >= 1000 {
                parts.push(format!("waited {}", fmt_duration_ms(waited)));
            }
            parts.extend(task.timeout_warning(now));
            ("running", p.accent)
        }
        (Phase::Waiting, _) => {
            parts.push(format!("#{position} in queue"));
            parts.push(format!(
                "waiting {}",
                fmt_duration_ms(now.saturating_sub(task.enqueued_at_ms))
            ));
            if let Some(expected) = task.expected_ms {
                parts.push(format!("usually ~{}", fmt_duration_ms(expected)));
            }
            if let Some(timeout) = task.spec.timeout_ms {
                parts.push(format!("limit {}", fmt_duration_ms(timeout)));
            }
            ("waiting", p.secondary)
        }
        (Phase::Finished, f) => {
            let f = f.expect("finished rows come from history");
            parts.push(finished_summary(f));
            parts.push(fmt_ago(now.saturating_sub(f.ended_at_ms)));
            match f.outcome {
                _ if f.outcome.is_success() => ("ok", p.ok),
                Outcome::TimedOut => ("timed-out", p.warn),
                Outcome::Cancelled | Outcome::Abandoned => ("cancelled", p.secondary),
                _ => ("failed", p.bad),
            }
        }
    };
    parts.push(task.spec.location());
    parts.extend(task.spec.agent.clone());

    let tag = if task.spec.kind == Kind::Exclusive {
        Some(("Exclusive", p.exclusive))
    } else if task.spec.light {
        Some((
            if task.paused_at_ms.is_some() {
                "Light · paused"
            } else {
                "Light"
            },
            p.secondary,
        ))
    } else {
        None
    };
    let title = div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .child(task.spec.title()),
        )
        .children(tag.map(|(label, color)| {
            div()
                .flex_none()
                .px(px(6.))
                .rounded(t.pill())
                .bg(p.fill)
                .text_size(px(10.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color)
                .child(label)
        }));

    let mut text = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(title);
    if let Some((fraction, color)) = bar {
        text = text.child(
            div().h(px(5.)).rounded(t.pill()).bg(p.fill).child(
                div()
                    .h_full()
                    .rounded(t.pill())
                    .bg(color)
                    .w(relative(fraction)),
            ),
        );
    }
    text = text.child(
        div()
            .text_size(px(SMALL))
            .text_color(p.secondary)
            .truncate()
            // Monospace fonts space it out enough already.
            .child(parts.join(if t.font.is_some() { " · " } else { "  ·  " })),
    );

    let action = match phase {
        Phase::Running | Phase::Waiting => {
            let tooltip = if phase == Phase::Waiting {
                "Remove from Queue"
            } else {
                "Stop"
            };
            icon_button(t, ("action", id as usize), "stop", tooltip)
                .on_click(cx.listener(move |view, _, _, cx| view.ask_to_stop(id, cx)))
        }
        Phase::Finished => icon_button(t, ("action", id as usize), "folder", REVEAL).on_click(
            cx.listener(move |view, _, _, cx| {
                if let Some((task, _)) = view.find(id) {
                    view.reveal(task, cx);
                }
            }),
        ),
    };
    let tooltip = format!(
        "{}\n{}\nPID {}",
        task.spec.command.join(" "),
        tildify(&task.spec.cwd),
        display_pid(task)
    );

    let mut row = div()
        .id(("task", id as usize))
        .flex_none()
        .h(px(if bar.is_some() { 58. } else { 46. }))
        .px(px(6.))
        .flex()
        .items_center()
        .gap(px(10.))
        .rounded(t.radius)
        .on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
            view.select(id, cx);
            if event.click_count() == 2 {
                view.on_reveal(&Reveal, window, cx);
            }
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                view.menu = Some((id, event.position));
                cx.notify();
            }),
        )
        .child(
            div()
                .flex_none()
                .w(px(20.))
                .flex()
                .justify_center()
                .child(icon(status, 16., status_color)),
        )
        .child(text)
        .child(div().pl(px(2.)).child(action))
        .tooltip(Tooltip::builder(t, tooltip.into()));
    if t.is_square() {
        // Hyprland marks the focused window with an accent border; Omarchy's selection gets one too.
        row = row.border_l_2().border_color(if selected {
            p.accent
        } else {
            Rgba { a: 0., ..p.accent }
        });
    }
    row = if selected {
        row.bg(p.selected)
    } else {
        row.hover(move |s| s.bg(p.hover))
    };
    row.into_any_element()
}

/// "Completed in 22s", "Exit 3 after 5s", "Gave up after waiting 1m02s", ...
fn finished_summary(f: &Finished) -> String {
    let t = &f.task;
    let Some(started) = t.started_at_ms else {
        let waited = fmt_duration_ms(f.ended_at_ms.saturating_sub(t.enqueued_at_ms));
        let verb = if f.outcome == Outcome::Cancelled {
            "Removed"
        } else {
            "Gave up"
        };
        return format!("{verb} after waiting {waited}");
    };
    let ran = fmt_duration_ms(f.ended_at_ms.saturating_sub(started));
    let label = match f.outcome {
        Outcome::Completed { exit_code: Some(0) } => return format!("Completed in {ran}"),
        Outcome::Completed {
            exit_code: Some(code),
        } => format!("Exit {code}"),
        Outcome::Completed { exit_code: None } => "Killed".into(),
        Outcome::TimedOut => "Timed out".into(),
        Outcome::Cancelled => "Stopped".into(),
        Outcome::Vanished => "Vanished".into(),
        Outcome::Abandoned => "Gave up".into(),
    };
    format!("{label} after {ran}")
}

/// A popover: a card with the theme's border, square and accent-bordered on Omarchy like
/// Hyprland's floating windows.
fn card(t: &Theme) -> gpui::Div {
    let card = div().bg(t.p.bg).shadow_lg();
    if t.is_square() {
        card.border_2().border_color(t.p.accent)
    } else {
        card.rounded(px(8.)).border_1().border_color(t.p.fill)
    }
}

fn context_menu(
    t: &Theme,
    phase: Phase,
    position: Point<Pixels>,
    cx: &mut Context<LockView>,
) -> impl IntoElement {
    let p = t.p;
    let item = |label: &'static str, id: &'static str| {
        div()
            .id(id)
            .px(px(10.))
            .py(px(3.))
            .rounded(t.radius.min(px(4.)))
            .cursor_pointer()
            .hover(move |s| s.bg(p.accent).text_color(p.bg))
            .child(label)
    };
    let mut menu = card(t)
        .id("context-menu")
        .occlude()
        .min_w(px(180.))
        .p(px(4.))
        .flex()
        .flex_col()
        .on_mouse_down_out(cx.listener(|view, _, _, cx| view.close_menu(cx)));
    if phase != Phase::Finished {
        let label = if phase == Phase::Waiting {
            "Remove from Queue"
        } else {
            "Stop"
        };
        menu = menu
            .child(
                item(label, "menu-stop")
                    .on_click(cx.listener(|view, _, window, cx| view.on_stop(&Stop, window, cx))),
            )
            .child(div().my(px(4.)).mx(px(10.)).h(px(1.)).bg(p.fill));
    }
    menu = menu
        .child(
            item(REVEAL, "menu-reveal")
                .on_click(cx.listener(|view, _, window, cx| view.on_reveal(&Reveal, window, cx))),
        )
        .child(item("Copy Command", "menu-copy").on_click(
            cx.listener(|view, _, window, cx| view.on_copy_command(&CopyCommand, window, cx)),
        ))
        .child(
            item("Copy PID", "menu-pid").on_click(
                cx.listener(|view, _, window, cx| view.on_copy_pid(&CopyPid, window, cx)),
            ),
        );
    deferred(
        anchored()
            .position(position)
            .snap_to_window_with_margin(px(8.))
            .child(menu),
    )
    .with_priority(1)
}

fn modal_overlay(
    t: &Theme,
    modal: &Modal,
    state: &State,
    jobs_when_on: u32,
    cx: &mut Context<LockView>,
) -> impl IntoElement {
    let dismiss = cx.listener(|view, _, window, cx| view.on_dismiss(&Dismiss, window, cx));
    let content = match modal {
        Modal::ConfirmStop {
            title,
            body,
            waiting,
            ..
        } => {
            let confirm = if *waiting { "Remove" } else { "Stop" };
            alert(t, title.clone(), body.clone()).child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        text_button(t, "cancel", "Cancel", ButtonStyle::Normal).on_click(dismiss),
                    )
                    .child(
                        text_button(t, "confirm", confirm, ButtonStyle::Destructive).on_click(
                            cx.listener(|view, _, window, cx| {
                                view.on_confirm(&Confirm, window, cx)
                            }),
                        ),
                    ),
            )
        }
        Modal::Error { title, message } => alert(t, title.to_string(), message.clone()).child(
            div()
                .flex()
                .justify_end()
                .child(text_button(t, "ok", "OK", ButtonStyle::Default).on_click(dismiss)),
        ),
        Modal::Settings => settings(t, state, jobs_when_on, cx),
    };
    div()
        .id("modal")
        .absolute()
        .inset_0()
        .occlude()
        .bg(rgba(0x00000059))
        .flex()
        .items_center()
        .justify_center()
        .p_5()
        .child(content)
}

fn alert(t: &Theme, title: String, body: String) -> gpui::Div {
    card(t)
        .w(px(380.))
        .p(px(18.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(div().font_weight(FontWeight::BOLD).child(title))
        .child(
            div()
                .text_size(px(SMALL))
                .text_color(t.p.secondary)
                .mb(px(8.))
                .child(body),
        )
}

fn settings(t: &Theme, state: &State, jobs_when_on: u32, cx: &mut Context<LockView>) -> gpui::Div {
    let p = t.p;
    let cpus = lock::default_jobserver_tokens();
    let slots = state.shared_slots;
    let jobs_on = state.jobserver_tokens > 0;
    let jobs = if jobs_on {
        state.jobserver_tokens
    } else {
        jobs_when_on
    };
    let help = |text: &'static str| {
        div()
            .text_size(px(SMALL))
            .text_color(p.secondary)
            .child(text)
    };
    let label = |text: &'static str| div().w(px(120.)).flex_none().child(text);
    let setting = |title: &'static str, control: gpui::Div, help_text: &'static str| {
        div().flex().gap(px(10.)).child(label(title)).child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(control)
                .child(help(help_text)),
        )
    };

    let slots_control = stepper(
        t,
        "slots",
        slots.to_string(),
        None,
        true,
        cx.listener(move |view, _, _, cx| view.set_slots(slots.saturating_sub(1).max(1), cx)),
        cx.listener(move |view, _, _, cx| view.set_slots((slots + 1).min(cpus * 2), cx)),
    );
    let checkbox = div()
        .id("jobs-enabled")
        .flex()
        .items_center()
        .gap(px(6.))
        .cursor_pointer()
        .on_click(
            cx.listener(move |view, _, _, cx| view.set_jobs(if jobs_on { 0 } else { jobs }, cx)),
        )
        .child(
            div()
                .size(px(15.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(t.radius.min(px(4.)))
                .when(jobs_on, |d| d.bg(p.accent).child(icon("check", 12., p.bg)))
                .when(!jobs_on, |d| d.border_1().border_color(p.tertiary)),
        )
        .child("Share build jobs between tasks");
    let jobs_control = div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(checkbox)
        .child(stepper(
            t,
            "jobs",
            jobs.to_string(),
            Some("jobs"),
            jobs_on,
            cx.listener(move |view, _, _, cx| view.set_jobs(jobs.saturating_sub(1).max(1), cx)),
            cx.listener(move |view, _, _, cx| view.set_jobs((jobs + 1).min(cpus * 4), cx)),
        ));
    let timeout_control = stepper(
        t,
        "timeout",
        fmt_duration_ms(state.default_timeout_ms),
        None,
        true,
        cx.listener(|view, _, _, cx| view.step_timeout(false, cx)),
        cx.listener(|view, _, _, cx| view.step_timeout(true, cx)),
    );

    card(t)
        .w(px(480.))
        .p(px(20.))
        .flex()
        .flex_col()
        .gap(px(18.))
        .child(div().font_weight(FontWeight::BOLD).child("Settings"))
        .child(setting(
            "Shared slots",
            slots_control,
            "How many shared tasks, such as builds and test runs, may run at once.",
        ))
        .child(setting(
            "Jobserver",
            jobs_control,
            "Ninja, make and cargo take one of these for each job they start, so builds running at the \
             same time split the machine between them. Takes effect for the next task that starts.",
        ))
        .child(setting(
            "Default timeout",
            timeout_control,
            "How long a command run without -t may take. Keep it short: it's meant for quick commands, \
             so anything longer states its own estimate.",
        ))
        .child(
            div().flex().justify_end().child(
                text_button(t, "done", "Done", ButtonStyle::Default)
                    .on_click(cx.listener(|view, _, window, cx| view.on_dismiss(&Dismiss, window, cx))),
            ),
        )
}

fn stepper(
    t: &Theme,
    id: &'static str,
    value: String,
    unit: Option<&'static str>,
    enabled: bool,
    down: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    up: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let p = t.p;
    let mut row = div().flex().items_center().gap(px(4.)).child(
        div()
            .min_w(px(40.))
            .px(px(6.))
            .py(px(1.))
            .flex()
            .justify_end()
            .rounded(t.radius.min(px(4.)))
            .bg(p.fill)
            .font_family(t.mono.clone())
            .text_color(if enabled { p.text } else { p.tertiary })
            .child(value),
    );
    if enabled {
        row = row
            .child(
                icon_button(t, SharedString::from(format!("{id}-down")), "minus", "Less")
                    .on_click(down),
            )
            .child(
                icon_button(t, SharedString::from(format!("{id}-up")), "plus", "More").on_click(up),
            );
    }
    row.children(unit.map(|u| {
        div()
            .text_color(if enabled { p.text } else { p.tertiary })
            .child(u)
    }))
}

struct Tooltip {
    text: SharedString,
    theme: Theme,
}

impl Tooltip {
    fn builder(
        t: &Theme,
        text: SharedString,
    ) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
        let theme = t.clone();
        move |_, cx| {
            let tooltip = Tooltip {
                text: text.clone(),
                theme: theme.clone(),
            };
            cx.new(|_| tooltip).into()
        }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = &self.theme;
        let mut tip = div()
            .mt(px(6.))
            .max_w(px(480.))
            .px(px(8.))
            .py(px(5.))
            .rounded(t.radius.min(px(5.)))
            .bg(t.p.bg)
            .border_1()
            .border_color(t.p.fill)
            .shadow_md()
            .text_size(px(SMALL))
            .text_color(t.p.text)
            .child(self.text.clone());
        if let Some(font) = &t.font {
            tip = tip.font_family(font.clone());
        }
        tip
    }
}

fn main() {
    Application::new().with_assets(Icons).run(|cx: &mut App| {
        // "secondary" is Cmd on macOS and Ctrl elsewhere.
        cx.bind_keys([
            KeyBinding::new("down", SelectNext, Some("LockView")),
            KeyBinding::new("up", SelectPrevious, Some("LockView")),
            KeyBinding::new("secondary-.", Stop, Some("LockView")),
            KeyBinding::new("secondary-shift-r", Reveal, Some("LockView")),
            KeyBinding::new("secondary-c", CopyCommand, Some("LockView")),
            KeyBinding::new("secondary-,", OpenSettings, Some("LockView")),
            KeyBinding::new("escape", Dismiss, Some("LockView")),
            KeyBinding::new("enter", Confirm, Some("LockView")),
            KeyBinding::new("secondary-w", CloseWindow, Some("LockView")),
            KeyBinding::new("secondary-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        // Only macOS shows these; the shortcuts work everywhere.
        cx.set_menus(vec![
            Menu {
                name: "Lock".into(),
                items: vec![
                    MenuItem::action("Settings…", OpenSettings),
                    MenuItem::separator(),
                    MenuItem::action("Quit Lock", Quit),
                ],
            },
            Menu {
                name: "File".into(),
                items: vec![MenuItem::action("Close Window", CloseWindow)],
            },
            Menu {
                name: "Task".into(),
                items: vec![
                    MenuItem::action("Stop", Stop),
                    MenuItem::separator(),
                    MenuItem::action(REVEAL, Reveal),
                    MenuItem::action("Copy Command", CopyCommand),
                    MenuItem::action("Copy PID", CopyPid),
                ],
            },
        ]);

        let bounds = Bounds::centered(None, size(px(640.), px(600.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Lock".into()),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                // Tiling compositors like Hyprland draw no titlebar; it's the border that shows focus.
                window_decorations: Some(WindowDecorations::Server),
                app_id: Some("lock".into()),
                window_min_size: Some(size(px(440.), px(300.))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| LockView::new(window, cx)),
        )
        .expect("open window");
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}
