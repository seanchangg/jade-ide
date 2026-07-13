//! The Jade window shell (feature inventory §2): action-bar strip, floating-card
//! main area (left panel / center content / right runtime sidebar), and a bottom
//! status strip. The runtime sidebar hosts the TRAINING view + telemetry
//! sidebar. `JadeApp` owns all telemetry state and the event pump; the panel
//! modules render pure projections of it.

use std::sync::Arc;

use forge_telemetry::{Event, Kind, TelemetryServer};
use gpui::{div, prelude::*, px, rgb, Context, Window};
use serde_json::{Map, Value};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::panels::{telemetry_sidebar, training_view};
use crate::prefs::TelemetryPrefs;
use crate::registry::{key_of, TelemetryRegistry, DEFAULT_MAX_DIM};
use crate::theme::Theme;
use crate::training::{TensorFrame, TrainingData};

pub struct JadeApp {
    pub server: Arc<TelemetryServer>,
    pub registry: TelemetryRegistry,
    pub training: TrainingData,
    pub prefs: TelemetryPrefs,
    pub theme: Theme,

    // Demo/telemetry counters (also drive the stdout log the spike printed).
    pub scalars_seen: u64,
    pub timings_seen: u64,
    pub tensors_seen: u64,
    demo: bool,
    /// Headless smoke-test hatch (`JADE_DEMO_ENABLE_BUFFERS=1`): auto-enables
    /// discovered buffers so `--train` streams tensor frames without a click.
    /// Off by default — the shipped rule is that checkboxes gate buffers.
    demo_enable_buffers: bool,
}

impl JadeApp {
    pub fn new(
        cx: &mut Context<Self>,
        server: Arc<TelemetryServer>,
        mut events: UnboundedReceiver<Event>,
        demo: bool,
    ) -> Self {
        // Event pump: coalesce bursts — drain everything queued, apply, then one
        // notify per batch (the spike's rule; the probe emits thousands/sec).
        cx.spawn(async move |this, cx| {
            while let Some(first) = events.recv().await {
                let mut batch = vec![first];
                while let Ok(more) = events.try_recv() {
                    batch.push(more);
                }
                if this
                    .update(cx, |app, cx| {
                        let before = app.scalars_seen;
                        for event in batch {
                            app.apply(event);
                        }
                        // Demo heartbeat: log each time the scalar count crosses a
                        // 200-mark (event-gated, so coalesced bursts still print).
                        if app.demo && before / 200 != app.scalars_seen / 200 {
                            println!(
                                "[jade] scalars {} timings {} tensors {} (buffers stream only when a checkbox enables them)",
                                app.scalars_seen, app.timings_seen, app.tensors_seen
                            );
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break; // window closed
                }
            }
        })
        .detach();

        Self {
            server,
            registry: TelemetryRegistry::new(),
            training: TrainingData::new(),
            prefs: TelemetryPrefs::load(),
            theme: Theme::forge_dark(),
            scalars_seen: 0,
            timings_seen: 0,
            tensors_seen: 0,
            demo,
            demo_enable_buffers: std::env::var_os("JADE_DEMO_ENABLE_BUFFERS").is_some(),
        }
    }

    /// Apply one telemetry event to the registry + training buffers. Auto-check
    /// side effects (sending `track` to the probe) happen here; buffers are
    /// never auto-enabled (checkboxes rule now).
    fn apply(&mut self, event: Event) {
        match event {
            Event::Decl {
                kind,
                name,
                meta,
                renamed_from,
            } => {
                let (mr, mc) = meta_dims(meta.as_ref());
                if let Some(from) = renamed_from {
                    if from != name {
                        self.registry.rename(kind, &from, &name, &self.prefs);
                        self.prefs
                            .migrate(&key_of(kind, &from), &key_of(kind, &name));
                    }
                }
                let out = self.registry.declare(kind, &name, mr, mc, &self.prefs);
                if out.auto_enabled {
                    // Auto-check is implicit (not persisted) — matches the TS
                    // behavior where a stored pref only appears on user toggle.
                    self.server.set_track(kind, &name, true, None, None);
                }
                if out.pref_enabled {
                    // Stored pref re-enabled it: the server's registry starts
                    // empty each session, so the track must be pushed now.
                    self.push_track(kind, &name, true);
                }
                if self.demo_enable_buffers
                    && kind == Kind::Buffer
                    && !self.registry.is_enabled(kind, &name)
                {
                    self.toggle_enabled(kind, &name); // smoke-test hatch only
                }
            }
            Event::Scalar(s) => {
                let out = self.registry.note_scalar(&s.name, s.step, s.value, &self.prefs);
                if out.auto_enabled {
                    self.server.set_track(Kind::Scalar, &s.name, true, None, None);
                }
                if out.pref_enabled {
                    self.push_track(Kind::Scalar, &s.name, true);
                }
                self.training.push_scalar(&s.name, s.step, s.value);
                self.scalars_seen += 1;
            }
            Event::Timing(t) => {
                self.registry.note_timing(&t.name, t.ms, t.step, &self.prefs);
                self.training.push_timing(&t.name, t.ms, t.step);
                self.timings_seen += 1;
            }
            Event::Tensor {
                name,
                step,
                rows,
                cols,
                src_rows,
                src_cols,
                data,
                ..
            } => {
                let out = self.registry.note_tensor(
                    &name,
                    src_rows.unwrap_or(rows),
                    src_cols.unwrap_or(cols),
                    step,
                    &self.prefs,
                );
                if out.pref_enabled {
                    self.push_track(Kind::Buffer, &name, true);
                }
                self.training.push_tensor(
                    &name,
                    TensorFrame {
                        step,
                        rows,
                        cols,
                        src_rows,
                        src_cols,
                        data,
                    },
                );
                self.tensors_seen += 1;
                if self.demo && (self.tensors_seen == 1 || self.tensors_seen % 25 == 0) {
                    println!(
                        "[jade] tensor #{}: {} {}x{} step {}",
                        self.tensors_seen, name, rows, cols, step
                    );
                }
            }
        }
    }

    /// User checkbox toggle: flip registry state, persist, send `track`.
    pub fn toggle_enabled(&mut self, kind: Kind, name: &str) {
        let now = !self.registry.is_enabled(kind, name);
        if !self.registry.set_enabled(kind, name, now) {
            return;
        }
        let key = key_of(kind, name);
        self.prefs.set_enabled(&key, now);
        self.prefs.save();
        self.push_track(kind, name, now);
    }

    /// Send `track` to the server with the item's persisted maxDim/shape —
    /// used by checkbox toggles and by pref-restored enables at declare time.
    fn push_track(&self, kind: Kind, name: &str, enabled: bool) {
        let (max_dim, shape) = if kind == Kind::Buffer {
            let item = self.registry.get(kind, name);
            (
                item.and_then(|i| i.max_dim).or(Some(DEFAULT_MAX_DIM)),
                item.and_then(|i| i.effective_shape()),
            )
        } else {
            (None, None)
        };
        self.server.set_track(kind, name, enabled, max_dim, shape);
    }
}

fn meta_dims(meta: Option<&Map<String, Value>>) -> (Option<u32>, Option<u32>) {
    let get = |m: &Map<String, Value>, k: &str| m.get(k).and_then(|v| v.as_u64()).map(|n| n as u32);
    match meta {
        Some(m) => (get(m, "rows"), get(m, "cols")),
        None => (None, None),
    }
}

// ── Layout ───────────────────────────────────────────────────────────────────

impl Render for JadeApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(theme.bg))
            .text_color(rgb(theme.text))
            .font_family("Menlo") // JetBrains Mono isn't installed on this machine
            .text_sm()
            .child(action_bar(&theme))
            .child(
                // Main area: left panel | center content | right runtime sidebar.
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .gap(px(6.))
                    .p(px(6.))
                    .child(left_panel(&theme))
                    .child(center_content(&theme))
                    .child(runtime_sidebar(self, cx, &theme)),
            )
            .child(status_strip(self, &theme))
    }
}

fn action_bar(theme: &Theme) -> impl IntoElement {
    // Placeholder toggle buttons (§2). No build wiring yet — Phase-3 does that.
    let toggles = ["Files", "Terminal", "Flow", "Runtime"];
    let actions = ["ASM", "Build", "Run", "Debug", "Stop"];

    let mut left_group = div().flex().items_center().gap_2();
    for t in toggles {
        left_group = left_group.child(chip(t, theme, false));
    }
    let mut right_group = div().flex().items_center().gap_2();
    for a in actions {
        right_group = right_group.child(chip(a, theme, a == "Build"));
    }
    right_group = right_group
        .child(chip("AI", theme, false))
        .child(chip("Theme", theme, false));

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(38.))
        .pl(px(80.)) // clear the traffic lights (hiddenInset title bar)
        .pr(px(12.))
        .bg(rgb(theme.panel))
        .border_b_1()
        .border_color(rgb(theme.border))
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .text_color(rgb(theme.accent))
                        .child("Jade"),
                )
                .child(left_group),
        )
        .child(right_group)
}

fn chip(label: &str, theme: &Theme, accent: bool) -> impl IntoElement {
    let mut el = div()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .bg(rgb(theme.bg))
        .text_color(rgb(theme.muted));
    if accent {
        el = el.text_color(rgb(theme.accent));
    }
    el.child(label.to_string())
}

fn left_panel(theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .w(px(260.))
        .p(px(10.))
        .rounded_lg()
        .bg(rgb(theme.panel))
        .border_1()
        .border_color(rgb(theme.border))
        .child(div().text_color(rgb(theme.muted)).text_xs().child("FILES"))
        .child(
            div()
                .text_color(rgb(theme.muted))
                .text_xs()
                .child("Open a folder to get started"),
        )
}

fn center_content(theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .rounded_lg()
        .bg(rgb(theme.bg))
        .border_1()
        .border_color(rgb(theme.border))
        .child(
            div()
                .text_color(rgb(theme.muted))
                .child("editor — center content placeholder"),
        )
}

fn runtime_sidebar(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    div()
        .id("runtime-sidebar")
        .flex()
        .flex_col()
        .gap_3()
        .w(px(280.))
        .p(px(10.))
        .rounded_lg()
        .bg(rgb(theme.panel))
        .border_1()
        .border_color(rgb(theme.border))
        .overflow_y_scroll()
        .child(training_view::render(app, cx))
        .child(telemetry_sidebar::render(app, cx))
}

fn status_strip(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let text = format!(
        "socket {}   ·   scalars {}   timings {}   tensors {}",
        app.server.socket_path().display(),
        app.scalars_seen,
        app.timings_seen,
        app.tensors_seen
    );
    div()
        .flex()
        .items_center()
        .h(px(22.))
        .px(px(10.))
        .bg(rgb(theme.panel))
        .border_t_1()
        .border_color(rgb(theme.border))
        .child(
            div()
                .text_color(rgb(theme.muted))
                .text_xs()
                .child(text),
        )
}
