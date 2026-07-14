//! TRAINING view (feature inventory §7.1): Loss / Memory / Kernel-time charts,
//! timing breakdown bars, and tensor heatmap previews. Charts are GPUI
//! `canvas` elements painted with `PathBuilder` polylines; the ghost previous
//! run underlays at 25% alpha; the heatmap keeps the working diverging colormap
//! from the spike.
//!
//! Rendering is a pure projection of `JadeApp` state — data prep happens here,
//! owned snapshots move into the paint closures (canvas closures are `'static`).

use gpui::{
    canvas, div, fill, point, prelude::*, px, rgb, size, Bounds, Context, PathBuilder, Pixels,
    Rgba, Window,
};

use forge_telemetry::Kind;

use crate::app::JadeApp;
use crate::format::{fmt_val, format_avg_ms, format_bytes, is_memory_name};
use crate::theme::Theme;

const CHART_H: f32 = 120.0;

/// One plotted polyline: absolute Y range is `[min, max]`, X is index-based.
struct Series {
    color: Rgba,
    width: f32,
    fill: bool,
    values: Vec<f32>,
    min: f32,
    max: f32,
}

struct Label {
    text: String,
    color: u32,
}

pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = &app.theme;
    let grid = rgb(theme.grid_line).alpha(theme.grid_alpha);

    let mut col = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(training_header(app, theme, cx));

    // ── Loss ──
    let (loss_series, loss_labels) = loss_data(app);
    col = col.child(chart_section(
        "Loss",
        theme,
        loss_series,
        loss_labels,
        grid,
    ));

    // ── Memory ──
    let (mem_series, mem_labels) = memory_data(app);
    col = col.child(chart_section(
        "Memory",
        theme,
        mem_series,
        mem_labels,
        grid,
    ));

    // ── Kernel time (auto-hidden until plottable) ──
    if let Some((k_series, k_labels)) = kernel_data(app) {
        col = col.child(chart_section(
            "Kernel time (ms)",
            theme,
            k_series,
            k_labels,
            grid,
        ));
    }

    // ── Timing breakdown ──
    col = col.child(timing_breakdown(app, theme));

    // ── Tensors (auto-hidden until a buffer is enabled with frames) ──
    if let Some(tensors) = tensor_previews(app, theme) {
        col = col.child(tensors);
    }

    col
}

// ── Section scaffolding ──────────────────────────────────────────────────────

fn section_header(text: &str, theme: &Theme) -> impl IntoElement {
    div()
        .text_color(rgb(theme.accent))
        .text_xs()
        .child(text.to_string())
}

/// TRAINING header with the "3D" button (§7.1) that opens the weight-grid
/// overlay (§7.2). The grid's ring already filled while hidden, so it shows the
/// selected buffer's replay immediately.
fn training_header(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let has_buffers = !app.wg3d.buffer_names().is_empty();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(section_header("TRAINING", theme))
        .child(
            div()
                .id("wg3d-open")
                .px_2()
                .py_1()
                .rounded_md()
                .text_xs()
                .bg(rgb(theme.bg))
                .text_color(rgb(if has_buffers { theme.accent } else { theme.muted }))
                .cursor_pointer()
                .on_click(cx.listener(|app, _e, _w, cx| {
                    app.wg3d.open(None);
                    cx.notify();
                }))
                .child("3D"),
        )
}

fn section_label(text: &str, theme: &Theme) -> impl IntoElement {
    div()
        .text_color(rgb(theme.muted))
        .text_xs()
        .child(text.to_string())
}

fn chart_section(
    title: &str,
    theme: &Theme,
    series: Vec<Series>,
    labels: Vec<Label>,
    grid: Rgba,
) -> impl IntoElement {
    // Labels overlaid at top-left of the chart, one line per series.
    let mut overlay = div()
        .absolute()
        .top(px(2.))
        .left(px(4.))
        .flex()
        .flex_col();
    for l in labels {
        overlay = overlay.child(
            div()
                .text_color(rgb(l.color))
                .text_size(px(10.))
                .child(l.text),
        );
    }

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label(title, theme))
        .child(
            div()
                .relative()
                .w_full()
                .h(px(CHART_H))
                .rounded_md()
                .bg(rgb(theme.bg))
                .child(chart_canvas(series, grid))
                .child(overlay),
        )
}

/// The polyline canvas. Grid lines are 1px filled quads; series are stroked
/// paths; area-filled series add a closed polygon down to the baseline.
fn chart_canvas(series: Vec<Series>, grid: Rgba) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window: &mut Window, _| {
            let ox = f32::from(bounds.origin.x);
            let oy = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            if w <= 0.0 || h <= 0.0 {
                return;
            }

            // Horizontal grid (5 lines).
            for i in 0..5 {
                let y = oy + (h / 5.0) * i as f32;
                window.paint_quad(fill(
                    Bounds {
                        origin: point(px(ox), px(y)),
                        size: size(px(w), px(1.)),
                    },
                    grid,
                ));
            }

            for s in &series {
                if s.values.len() < 2 {
                    continue;
                }
                let range = (s.max - s.min).abs().max(f32::EPSILON);
                let map = |i: usize, v: f32| -> gpui::Point<Pixels> {
                    let x = ox + (i as f32 / (s.values.len() as f32 - 1.0)) * w;
                    let y = oy + h - ((v - s.min) / range) * (h - 16.0) - 8.0;
                    point(px(x), px(y))
                };

                // Optional area fill under the curve.
                if s.fill {
                    let mut fb = PathBuilder::fill();
                    fb.move_to(map(0, s.values[0]));
                    for (i, v) in s.values.iter().enumerate().skip(1) {
                        fb.line_to(map(i, *v));
                    }
                    fb.line_to(point(px(ox + w), px(oy + h)));
                    fb.line_to(point(px(ox), px(oy + h)));
                    fb.close();
                    if let Ok(path) = fb.build() {
                        window.paint_path(path, s.color.alpha(0.10));
                    }
                }

                // Stroked polyline.
                let mut b = PathBuilder::stroke(px(s.width));
                b.move_to(map(0, s.values[0]));
                for (i, v) in s.values.iter().enumerate().skip(1) {
                    b.line_to(map(i, *v));
                }
                if let Ok(path) = b.build() {
                    window.paint_path(path, s.color);
                }
            }
        },
    )
    .size_full()
}

// ── Data preparation ─────────────────────────────────────────────────────────

fn series_colors(theme: &Theme) -> [u32; 5] {
    theme.series
}

/// Loss: every ENABLED, non-memory scalar, ghost prev + current, per-series
/// min/max combining both runs.
fn loss_data(app: &JadeApp) -> (Vec<Series>, Vec<Label>) {
    let colors = series_colors(&app.theme);
    let mut series = Vec::new();
    let mut labels = Vec::new();

    // Stable name set from current then previous.
    let mut names: Vec<String> = Vec::new();
    for name in app.training.current.scalars.keys() {
        if is_memory_name(name) || !app.registry.is_enabled(Kind::Scalar, name) {
            continue;
        }
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    for name in app.training.previous.scalars.keys() {
        if is_memory_name(name) || !app.registry.is_enabled(Kind::Scalar, name) {
            continue;
        }
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names.sort();

    let mut ci = 0usize;
    for name in names {
        let color = colors[ci % colors.len()];
        ci += 1;

        let cur = app.training.current.scalars.get(&name);
        let prev = app.training.previous.scalars.get(&name);

        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        if let (Some(c), Some(st)) = (cur, app.training.current.scalar_stats.get(&name)) {
            if c.len() >= 2 {
                min = min.min(st.min);
                max = max.max(st.max);
            }
        }
        if let (Some(p), Some(st)) = (prev, app.training.previous.scalar_stats.get(&name)) {
            if p.len() >= 2 {
                min = min.min(st.min);
                max = max.max(st.max);
            }
        }
        if !min.is_finite() || !max.is_finite() {
            continue;
        }

        if let Some(p) = prev {
            if p.len() >= 2 {
                series.push(Series {
                    color: rgb(color).alpha(0.25),
                    width: 1.0,
                    fill: false,
                    values: p.iter().map(|x| x.value as f32).collect(),
                    min: min as f32,
                    max: max as f32,
                });
            }
        }
        if let Some(c) = cur {
            if c.len() >= 2 {
                series.push(Series {
                    color: rgb(color),
                    width: 1.5,
                    fill: false,
                    values: c.iter().map(|x| x.value as f32).collect(),
                    min: min as f32,
                    max: max as f32,
                });
            }
        }

        let last = cur
            .and_then(|c| c.last())
            .or_else(|| prev.and_then(|p| p.last()));
        if let Some(pt) = last {
            labels.push(Label {
                text: format!("{}: {:.4}", name, pt.value),
                color,
            });
        }
    }
    (series, labels)
}

/// Memory: a `memory`/`heap`-named scalar if present, else live heap history;
/// baseline 0, area-filled current run, ghost prev.
fn memory_data(app: &JadeApp) -> (Vec<Series>, Vec<Label>) {
    let accent = app.theme.mem_accent;
    let mut series = Vec::new();
    let mut labels = Vec::new();

    let mem_name = app
        .training
        .current
        .scalars
        .keys()
        .find(|n| is_memory_name(n))
        .cloned();

    let (cur_vals, prev_vals, max_val, label_text): (Vec<f32>, Vec<f32>, f64, String) =
        if let Some(name) = &mem_name {
            let cur: Vec<f32> = app
                .training
                .current
                .scalars
                .get(name)
                .map(|a| a.iter().map(|p| p.value as f32).collect())
                .unwrap_or_default();
            let prev: Vec<f32> = app
                .training
                .previous
                .scalars
                .get(name)
                .map(|a| a.iter().map(|p| p.value as f32).collect())
                .unwrap_or_default();
            let cs = app
                .training
                .current
                .scalar_stats
                .get(name)
                .map(|s| s.max)
                .unwrap_or(0.0);
            let ps = app
                .training
                .previous
                .scalar_stats
                .get(name)
                .map(|s| s.max)
                .unwrap_or(0.0);
            let last = app
                .training
                .current
                .scalars
                .get(name)
                .and_then(|a| a.last())
                .map(|p| p.value)
                .unwrap_or(0.0);
            (
                cur,
                prev,
                cs.max(ps).max(1.0),
                format!("{}: {}", name, format_bytes(last)),
            )
        } else {
            let cur: Vec<f32> = app
                .training
                .current
                .memory
                .iter()
                .map(|m| m.heap_used as f32)
                .collect();
            let prev: Vec<f32> = app
                .training
                .previous
                .memory
                .iter()
                .map(|m| m.heap_used as f32)
                .collect();
            let max = app
                .training
                .current
                .memory_max
                .max(app.training.previous.memory_max)
                .max(1.0);
            let last = app
                .training
                .current
                .memory
                .last()
                .map(|m| m.heap_used)
                .unwrap_or(0.0);
            (cur, prev, max, format!("Heap: {}", format_bytes(last)))
        };

    if cur_vals.len() < 2 && prev_vals.len() < 2 {
        return (series, labels);
    }

    if prev_vals.len() >= 2 {
        series.push(Series {
            color: rgb(accent).alpha(0.25),
            width: 1.0,
            fill: false,
            values: prev_vals,
            min: 0.0,
            max: max_val as f32,
        });
    }
    if cur_vals.len() >= 2 {
        series.push(Series {
            color: rgb(accent),
            width: 1.5,
            fill: true,
            values: cur_vals,
            min: 0.0,
            max: max_val as f32,
        });
    }
    labels.push(Label {
        text: label_text,
        color: accent,
    });
    (series, labels)
}

/// Kernel time: per-step curves for ENABLED timers, shared ms scale across all
/// series. Returns `None` when nothing is plottable (section stays hidden).
fn kernel_data(app: &JadeApp) -> Option<(Vec<Series>, Vec<Label>)> {
    let colors = series_colors(&app.theme);

    let group = |timings: &[crate::training::TimingPoint]| {
        let mut order: Vec<String> = Vec::new();
        let mut map: std::collections::HashMap<String, Vec<f32>> = std::collections::HashMap::new();
        for t in timings {
            if !app.registry.is_enabled(Kind::Timer, &t.name) {
                continue;
            }
            if !map.contains_key(&t.name) {
                order.push(t.name.clone());
            }
            map.entry(t.name.clone()).or_default().push(t.ms as f32);
        }
        (order, map)
    };

    let (cur_order, cur) = group(&app.training.current.timings);
    let (_prev_order, prev) = group(&app.training.previous.timings);

    let plottable = cur.values().chain(prev.values()).any(|v| v.len() >= 2);
    if !plottable {
        return None;
    }

    // Shared max across all series (timers share a unit).
    let mut max_ms = 0.0f32;
    for v in cur.values().chain(prev.values()) {
        for &x in v {
            if x > max_ms {
                max_ms = x;
            }
        }
    }
    if max_ms <= 0.0 {
        max_ms = 1.0;
    }

    // Stable name order: current first, then any prev-only.
    let mut names = cur_order.clone();
    for n in prev.keys() {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }

    let mut series = Vec::new();
    let mut labels = Vec::new();
    let mut ci = 0usize;
    for name in names {
        let color = colors[ci % colors.len()];
        ci += 1;
        if let Some(p) = prev.get(&name) {
            if p.len() >= 2 {
                series.push(Series {
                    color: rgb(color).alpha(0.25),
                    width: 1.0,
                    fill: false,
                    values: p.clone(),
                    min: 0.0,
                    max: max_ms,
                });
            }
        }
        if let Some(c) = cur.get(&name) {
            if c.len() >= 2 {
                series.push(Series {
                    color: rgb(color),
                    width: 1.5,
                    fill: false,
                    values: c.clone(),
                    min: 0.0,
                    max: max_ms,
                });
            }
        }
        let last = cur
            .get(&name)
            .and_then(|c| c.last())
            .or_else(|| prev.get(&name).and_then(|p| p.last()));
        if let Some(v) = last {
            labels.push(Label {
                text: format!("{}: {:.3}ms", name, v),
                color,
            });
        }
    }
    Some((series, labels))
}

/// Timing breakdown: top 8 enabled timers by total ms, horizontal bars with an
/// average label (seconds at ≥1000ms).
fn timing_breakdown(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let mut order: Vec<String> = Vec::new();
    let mut agg: std::collections::HashMap<String, (f64, u32)> = std::collections::HashMap::new();
    for t in &app.training.current.timings {
        if !app.registry.is_enabled(Kind::Timer, &t.name) {
            continue;
        }
        if !agg.contains_key(&t.name) {
            order.push(t.name.clone());
        }
        let e = agg.entry(t.name.clone()).or_insert((0.0, 0));
        e.0 += t.ms;
        e.1 += 1;
    }
    let mut rows: Vec<(String, f64, u32)> =
        order.into_iter().map(|n| { let (t, c) = agg[&n]; (n, t, c) }).collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));
    rows.truncate(8);
    let max_total = rows.first().map(|r| r.1).unwrap_or(1.0).max(1.0);

    const TRACK_W: f32 = 110.0;
    let mut list = div().flex().flex_col().gap_1();
    for (name, total, count) in rows {
        let frac = (total / max_total) as f32;
        let avg = total / count.max(1) as f64;
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(10.))
                .child(
                    div()
                        .w(px(96.))
                        .text_color(rgb(theme.text))
                        .overflow_hidden()
                        .child(name),
                )
                .child(
                    div()
                        .w(px(TRACK_W))
                        .h(px(6.))
                        .rounded_sm()
                        .bg(rgb(theme.panel))
                        .child(
                            div()
                                .h_full()
                                .w(px(TRACK_W * frac))
                                .rounded_sm()
                                .bg(rgb(theme.accent)),
                        ),
                )
                .child(
                    div()
                        .text_color(rgb(theme.muted))
                        .child(format_avg_ms(avg)),
                ),
        );
    }

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label("Timing", theme))
        .child(list)
}

/// Tensor heatmap previews — one per enabled buffer with frames. Returns `None`
/// when the section should stay hidden.
fn tensor_previews(app: &JadeApp, theme: &Theme) -> Option<impl IntoElement> {
    let mut previews = div().flex().flex_col().gap_2();
    let mut any = false;

    for (name, ring) in &app.training.tensors {
        if ring.is_empty() || !app.registry.is_enabled(Kind::Buffer, name) {
            continue;
        }
        let frame = ring.back().unwrap();
        any = true;

        let n = (frame.rows as usize) * (frame.cols as usize);
        let mut mn = f32::INFINITY;
        let mut mx = f32::NEG_INFINITY;
        let mut max_abs = 0.0f32;
        for &v in frame.data.iter().take(n) {
            if v < mn {
                mn = v;
            }
            if v > mx {
                mx = v;
            }
            let a = v.abs();
            if a > max_abs {
                max_abs = a;
            }
        }
        let range = if mn.is_finite() {
            format!("  [{}…{}]", fmt_val(mn as f64), fmt_val(mx as f64))
        } else {
            String::new()
        };
        let dim_r = frame.src_rows.unwrap_or(frame.rows);
        let dim_c = frame.src_cols.unwrap_or(frame.cols);
        let label = format!("{}  {}×{} @{}{}", name, dim_r, dim_c, frame.step, range);

        // Preserve source aspect ratio inside a fixed-width preview box.
        let aspect_r = frame.src_rows.unwrap_or(frame.rows) as f32;
        let aspect_c = frame.src_cols.unwrap_or(frame.cols).max(1) as f32;
        let preview_w = 236.0f32;
        let box_h = ((preview_w * aspect_r) / aspect_c).clamp(40.0, 180.0);

        let rows = frame.rows.max(1);
        let cols = frame.cols.max(1);
        let data = frame.data.clone();

        previews = previews.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_color(rgb(theme.muted))
                        .text_size(px(9.))
                        .child(label),
                )
                .child(
                    div().w_full().h(px(box_h)).rounded_md().bg(rgb(theme.bg)).child(
                        heatmap_canvas(rows, cols, data, max_abs),
                    ),
                ),
        );
    }

    if any {
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(section_label("Tensors", theme))
                .child(previews),
        )
    } else {
        None
    }
}

/// Blocky nearest-neighbor heatmap, diverging colormap normalized by max-abs
/// (identical convention to the spike / training-view.ts:792-804).
fn heatmap_canvas(rows: u32, cols: u32, data: Vec<f32>, max_abs: f32) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window: &mut Window, _| {
            if rows == 0 || cols == 0 {
                return;
            }
            let cell = (f32::from(bounds.size.width) / cols as f32)
                .min(f32::from(bounds.size.height) / rows as f32)
                .max(0.0);
            if cell <= 0.0 {
                return;
            }
            for r in 0..rows {
                for c in 0..cols {
                    let idx = (r * cols + c) as usize;
                    let Some(&v) = data.get(idx) else { continue };
                    let origin =
                        bounds.origin + point(px(cell * c as f32), px(cell * r as f32));
                    window.paint_quad(fill(
                        Bounds {
                            origin,
                            size: size(px(cell), px(cell)),
                        },
                        diverging(v, max_abs),
                    ));
                }
            }
        },
    )
    .size_full()
}

fn diverging(value: f32, max_abs: f32) -> Rgba {
    let t = if max_abs > 0.0 {
        (value / max_abs).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    if t >= 0.0 {
        Rgba {
            r: 1.0,
            g: 1.0 - t,
            b: 1.0 - t,
            a: 1.0,
        }
    } else {
        Rgba {
            r: 1.0 + t,
            g: 1.0 + t,
            b: 1.0,
            a: 1.0,
        }
    }
}
