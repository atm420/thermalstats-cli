//! Rendering. Reads `App` and registers click targets.
//!
//! Only glyphs from the WGL4 set (box drawing, ▀▄█▌░, ●○►◄▲, •…♥) are used,
//! so the interface looks the same in the classic Windows console (Consolas,
//! no font fallback) as in Windows Terminal. Colours are the 16 named ANSI
//! colours for the same reason.

use crate::app::*;
use crate::engine::{IdleLevel, Part, Phase, TestKind, Warning};
use crate::lang::{fill, LANGUAGES};
use crate::sensors::{Channel, Probe};
use crate::setup::DriverState;
use crate::stress::GpuStress;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Clear, Dataset, GraphType, Paragraph, Wrap};
use ratatui::Frame;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const MIN_W: u16 = 80;
const MIN_H: u16 = 24;
/// Content is capped at this width so lines stay readable on wide windows.
const MAX_W: u16 = 124;

const ACCENT: Color = Color::Cyan;
const GPU_COLOR: Color = Color::Magenta;
const MUTED: Color = Color::Gray;
const DIM: Color = Color::DarkGray;
const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;
/// Light panel for keys, idle fields and chips. Black text on it reads well
/// in every Windows console scheme; white on dark grey did not.
const PANEL: Color = Color::Gray;

type Hits = Vec<(Rect, Action)>;

/// Per-frame drawing context.
struct Ctx<'a> {
    app: &'a App,
    hits: Hits,
    cursor: Option<Position>,
}

impl Ctx<'_> {
    fn hit(&mut self, rect: Rect, action: Action) {
        self.hits.push((rect, action));
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let hits = std::mem::take(&mut app.hits);
    let mut ctx = Ctx { app, hits, cursor: None };
    ctx.hits.clear();
    draw_all(f, &mut ctx);
    let (hits, cursor) = (ctx.hits, ctx.cursor);
    if let Some(pos) = cursor {
        f.set_cursor_position(pos);
    }
    app.hits = hits;
}

fn draw_all(f: &mut Frame, ctx: &mut Ctx) {
    let full = f.area();
    if full.width < MIN_W || full.height < MIN_H {
        let msg = fill(ctx.app.t.too_small, &[("w", &MIN_W.to_string()), ("h", &MIN_H.to_string())]);
        let y = full.y + full.height / 2;
        f.render_widget(
            Paragraph::new(msg).alignment(Alignment::Center).wrap(Wrap { trim: true }),
            Rect::new(full.x, y.saturating_sub(1), full.width, 3.min(full.height)),
        );
        return;
    }

    let width = full.width.min(MAX_W);
    let area = Rect { x: full.x + (full.width - width) / 2, width, ..full };
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)]).areas(area);

    draw_header(f, ctx, header);
    let body = body.inner(Margin { horizontal: 2, vertical: 0 });
    match ctx.app.screen {
        Screen::Boot => draw_boot(f, ctx, body),
        Screen::Home => draw_home(f, ctx, body),
        Screen::Options => draw_options(f, ctx, body),
        Screen::Confirm => draw_confirm(f, ctx, body),
        Screen::Running => draw_running(f, ctx, body),
        Screen::Results => draw_results(f, ctx, body),
        Screen::Diagnostics => draw_diagnostics(f, ctx, body),
    }
    draw_footer(f, ctx, footer);

    if ctx.app.modal.is_some() {
        // Dialog fields own the cursor.
        ctx.cursor = None;
        draw_modal(f, ctx, area);
    }
    draw_toast(f, ctx, area);
}

// ─── Small helpers ─────────────────────────────────────────────────

fn width_of(s: &str) -> u16 {
    UnicodeWidthStr::width(s) as u16
}

/// Left-align `s` in `width` terminal columns (CJK characters take two).
fn pad(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    format!("{}{}", s, " ".repeat(width.saturating_sub(w)))
}

/// Cut `s` to `max` columns, ending with "…" if shortened.
fn truncate(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

fn pulse(frame: u64) -> &'static str {
    ["·", "•", "●", "•"][(frame / 3 % 4) as usize]
}

fn temp_color(t: f64) -> Color {
    if t > 85.0 {
        BAD
    } else if t > 70.0 {
        WARN
    } else {
        OK
    }
}

fn fmt_temp(t: f64) -> String {
    format!("{:.1}\u{00b0}C", t)
}

fn fmt_clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(title.to_string(), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))
}

fn row(f: &mut Frame, area: Rect, y: &mut u16, line: Line) {
    if *y < area.bottom() {
        f.render_widget(line, Rect::new(area.x, *y, area.width, 1));
        *y += 1;
    }
}

/// Wrapped paragraph at `y`; returns the rows it took.
fn para(f: &mut Frame, area: Rect, y: &mut u16, text: Line, indent: u16) -> u16 {
    if *y >= area.bottom() {
        return 0;
    }
    let width = area.width.saturating_sub(indent).max(1);
    let lines = wrapped_height(&text, width);
    let h = lines.min(area.bottom() - *y);
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: true }),
        Rect::new(area.x + indent, *y, width, h),
    );
    *y += h;
    h
}

fn wrapped_height(text: &Line, width: u16) -> u16 {
    let mut rows = 1u16;
    let mut col = 0usize;
    let width = width.max(1) as usize;
    let content: String = text.spans.iter().map(|s| s.content.as_ref()).collect();
    for word in content.split(' ') {
        let w = UnicodeWidthStr::width(word);
        if col > 0 && col + 1 + w > width {
            rows += 1;
            col = 0;
        } else if col > 0 {
            col += 1;
        }
        // Words longer than a line (paths, URLs) are broken across lines.
        col += w;
        while col > width {
            rows += 1;
            col -= width;
        }
    }
    rows
}

struct Btn {
    key: &'static str,
    label: String,
    action: Action,
    primary: bool,
}

fn btn(key: &'static str, label: &str, action: Action, primary: bool) -> Btn {
    Btn { key, label: label.to_string(), action, primary }
}

/// Lay buttons out left to right, wrapping when needed. Returns rows used.
fn draw_buttons(f: &mut Frame, ctx: &mut Ctx, area: Rect, y: u16, buttons: &[Btn]) -> u16 {
    let mut x = area.x;
    let mut row_y = y;
    for b in buttons {
        let w = width_of(b.key) + width_of(&b.label) + 4;
        if x > area.x && x + w > area.right() {
            x = area.x;
            row_y += 1;
        }
        if row_y >= area.bottom() {
            break;
        }
        // Primary: one solid accent block. Secondary: key chip plus plain
        // label, the same style as the footer.
        let (key_style, label_style) = if b.primary {
            (
                Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
                Style::new().fg(Color::Black).bg(ACCENT),
            )
        } else {
            (
                Style::new().fg(Color::Black).bg(PANEL).add_modifier(Modifier::BOLD),
                Style::new().add_modifier(Modifier::BOLD),
            )
        };
        let rect = Rect::new(x, row_y, w.min(area.right() - x), 1);
        f.render_widget(
            Line::from(vec![
                Span::styled(format!(" {} ", b.key), key_style),
                Span::styled(format!(" {} ", b.label), label_style),
            ]),
            rect,
        );
        ctx.hit(rect, b.action.clone());
        x += w + 2;
    }
    row_y - y + 1
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

// ─── Header & footer ───────────────────────────────────────────────

fn draw_header(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let t = &ctx.app.t;
    let top = Rect::new(area.x, area.y, area.width, 1);

    let mut left = vec![
        Span::styled(" THERMALSTATS ", Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(format!("v{}", VERSION), Style::new().fg(MUTED)),
    ];
    if VERSION.contains('-') {
        left.push(Span::raw(" "));
        left.push(Span::styled(format!(" {} ", t.beta), Style::new().fg(Color::Black).bg(WARN).add_modifier(Modifier::BOLD)));
    }
    if ctx.app.opts.demo {
        left.push(Span::styled("  DEMO", Style::new().fg(WARN).add_modifier(Modifier::BOLD)));
    }
    let left_w: u16 = left.iter().map(|s| width_of(&s.content)).sum();
    f.render_widget(Line::from(left), top);

    // Language badge (clickable), far right
    let lang = format!(" {} ", ctx.app.locale.to_uppercase());
    let lang_w = width_of(&lang);
    let lang_rect = Rect::new(top.right().saturating_sub(lang_w), top.y, lang_w, 1);
    f.render_widget(Span::styled(lang, Style::new().fg(Color::Black).bg(PANEL)), lang_rect);
    ctx.hit(lang_rect, Action::Language);

    // Step indicator, right-aligned before the language badge
    let current = match ctx.app.screen {
        Screen::Boot | Screen::Home | Screen::Diagnostics => 0,
        Screen::Options | Screen::Confirm => 1,
        Screen::Running => 2,
        Screen::Results => 3,
    };
    let steps = [t.step_check, t.step_options, t.step_test, t.step_results];
    let mut spans = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" > ", Style::new().fg(DIM)));
        }
        let style = if i == current {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else if i < current {
            Style::new().fg(MUTED)
        } else {
            Style::new().fg(DIM)
        };
        spans.push(Span::styled(format!("{} {}", i + 1, step), style));
    }
    let steps_w: u16 = spans.iter().map(|s| width_of(&s.content)).sum();
    if left_w + steps_w + lang_w + 4 <= top.width {
        let x = lang_rect.x.saturating_sub(steps_w + 2);
        f.render_widget(Line::from(spans), Rect::new(x, top.y, steps_w, 1));
    }

    f.render_widget(
        Span::styled("─".repeat(area.width as usize), Style::new().fg(DIM)),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
}

fn draw_footer(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let t = &ctx.app.t;
    if ctx.app.screen == Screen::Boot {
        return;
    }
    let mut items: Vec<(&str, String, Action)> = Vec::new();
    if ctx.app.screen == Screen::Running {
        items.push(("Esc", t.btn_stop.to_string(), Action::StopTest));
    }
    items.push(("F", t.key_feedback.to_string(), Action::Feedback));
    items.push(("S", format!("{} \u{2665}", t.key_support), Action::Support));
    items.push(("L", t.key_language.to_string(), Action::Language));
    items.push(("?", t.key_help.to_string(), Action::Help));
    if ctx.app.screen != Screen::Running {
        items.push(("Q", t.key_quit.to_string(), Action::Quit));
    }

    // While typing in a field, letter shortcuts type instead (clicks still work).
    let typing = ctx.app.is_typing();
    let mut x = area.x + 1;
    for (key, label, action) in items {
        let w = width_of(key) + width_of(&label) + 3;
        if x + w > area.right() {
            break;
        }
        let rect = Rect::new(x, area.y, w, 1);
        let label_style = if typing && key.len() == 1 {
            Style::new().fg(DIM)
        } else if action == Action::Support {
            Style::new().fg(Color::LightRed)
        } else {
            Style::new().fg(MUTED)
        };
        f.render_widget(
            Line::from(vec![
                Span::styled(format!(" {} ", key), Style::new().fg(Color::Black).bg(PANEL)),
                Span::styled(format!(" {}", label), label_style),
            ]),
            rect,
        );
        ctx.hit(rect, action);
        x += w + 2;
    }
}

// ─── Boot ──────────────────────────────────────────────────────────

fn draw_boot(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let t = &ctx.app.t;
    let step = *ctx.app.boot_step.lock().unwrap_or_else(|e| e.into_inner());
    let mut steps = vec![
        (crate::setup::BootStep::Hardware, t.boot_hardware),
        (crate::setup::BootStep::SensorTools, t.boot_tools),
    ];
    if cfg!(windows) || step == crate::setup::BootStep::Driver {
        steps.push((crate::setup::BootStep::Driver, t.boot_driver));
    }

    let box_area = centered(area, 60, steps.len() as u16 + 6);
    let block = Block::bordered().border_style(Style::new().fg(DIM)).title(Span::styled(
        format!(" {} ", t.boot_title),
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(box_area).inner(Margin { horizontal: 2, vertical: 1 });
    f.render_widget(block, box_area);

    let mut y = inner.y;
    for (s, label) in steps {
        let (mark, style) = if s < step {
            ("●", Style::new().fg(OK))
        } else if s == step {
            (pulse(ctx.app.frame), Style::new().fg(ACCENT))
        } else {
            ("○", Style::new().fg(DIM))
        };
        let label_style = if s <= step { Style::new() } else { Style::new().fg(DIM) };
        row(f, inner, &mut y, Line::from(vec![Span::styled(format!("{} ", mark), style), Span::styled(label, label_style)]));
    }
    y += 1;
    row(f, inner, &mut y, Line::from(Span::styled(t.boot_note, Style::new().fg(MUTED))));
}

// ─── Home ──────────────────────────────────────────────────────────

fn draw_home(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let app = ctx.app;
    let t = &app.t;
    let mut y = area.y + 1;
    let label_w = [t.label_cpu, t.label_gpu, t.label_os, t.label_type].iter().map(|s| width_of(s)).max().unwrap_or(6) + 3;
    let value_x = area.x + 2 + label_w;
    let value_w = area.right().saturating_sub(value_x);

    row(f, area, &mut y, section(t.your_pc));
    let label = |s: &str| Span::styled(format!("  {}", pad(s, label_w as usize)), Style::new().fg(MUTED));

    if let Some(hw) = &app.hw {
        let cpu = hw.cpu_model.clone().unwrap_or_else(|| t.unknown.to_string());
        let mut spans = vec![label(t.label_cpu), Span::styled(cpu, Style::new().add_modifier(Modifier::BOLD))];
        if let (Some(c), Some(th)) = (hw.cpu_cores, hw.cpu_threads) {
            spans.push(Span::styled(
                format!("   {}", fill(t.cores_threads, &[("cores", &c.to_string()), ("threads", &th.to_string())])),
                Style::new().fg(MUTED),
            ));
        }
        row(f, area, &mut y, Line::from(spans));

        // GPU with switcher
        match app.selected_gpu() {
            Some(gpu) => {
                let multi = hw.gpus.len() > 1;
                let mut desc = gpu.name.clone();
                if let Some(v) = gpu.vram() {
                    desc.push_str(&format!(", {}", v));
                }
                match gpu.kind {
                    crate::gpus::GpuKind::Discrete => desc.push_str(&format!(", {}", t.discrete)),
                    crate::gpus::GpuKind::Integrated => desc.push_str(&format!(", {}", t.integrated)),
                    _ => {}
                }
                let desc = truncate(&desc, value_w.saturating_sub(6) as usize);
                f.render_widget(label(t.label_gpu), Rect::new(area.x, y, label_w + 2, 1));
                if multi {
                    let left = Rect::new(value_x, y, 2, 1);
                    let right = Rect::new(value_x + 3 + width_of(&desc), y, 2, 1);
                    f.render_widget(Span::styled("◄", Style::new().fg(ACCENT)), left);
                    f.render_widget(
                        Span::styled(desc.clone(), Style::new().add_modifier(Modifier::BOLD)),
                        Rect::new(value_x + 2, y, width_of(&desc), 1),
                    );
                    f.render_widget(Span::styled("►", Style::new().fg(ACCENT)), right);
                    ctx.hit(left, Action::PrevGpu);
                    ctx.hit(right, Action::NextGpu);
                    y += 1;
                    let hint = fill(t.gpu_switch_hint, &[("n", &hw.gpus.len().to_string())]);
                    f.render_widget(Span::styled(hint, Style::new().fg(DIM)), Rect::new(value_x, y, value_w, 1));
                    y += 1;
                } else {
                    f.render_widget(
                        Span::styled(desc, Style::new().add_modifier(Modifier::BOLD)),
                        Rect::new(value_x, y, value_w, 1),
                    );
                    y += 1;
                }
            }
            None => row(f, area, &mut y, Line::from(vec![label(t.label_gpu), Span::styled(t.no_gpu, Style::new().fg(MUTED))])),
        }

        row(f, area, &mut y, Line::from(vec![label(t.label_os), Span::raw(hw.os.clone().unwrap_or_default())]));
        let mut kind = (if hw.is_laptop { t.laptop } else { t.desktop }).to_string();
        match app.on_battery {
            Some(true) => kind.push_str(&format!(", {}", t.on_battery)),
            Some(false) => kind.push_str(&format!(", {}", t.on_ac)),
            None => {}
        }
        let kind_style = if app.on_battery == Some(true) { Style::new().fg(WARN) } else { Style::new() };
        row(f, area, &mut y, Line::from(vec![label(t.label_type), Span::styled(kind, kind_style)]));
    }

    y += 1;
    row(f, area, &mut y, section(t.sensor_check));

    let Some(hub) = &app.hub else { return };
    let (cpu, gpu) = hub.with(|r| (r.cpu_temp.clone_light(), r.gpu_temp.clone_light()));
    let has_gpu = app.selected_gpu().is_some();
    sensor_row(f, ctx, area, &mut y, t.label_cpu, &cpu, label_w);
    if has_gpu {
        sensor_row(f, ctx, area, &mut y, t.label_gpu, &gpu, label_w);
    }
    y += 1;

    // Verdict and tips
    let searching = cpu.probe == Probe::Searching || (has_gpu && gpu.probe == Probe::Searching);
    let cpu_board = cpu.source.as_deref().is_some_and(|s| s.contains("motherboard"));
    let all_ok = cpu.probe == Probe::Found && !cpu_board && (!has_gpu || gpu.probe == Probe::Found);
    let (mark, style, text) = if searching {
        (pulse(app.frame), Style::new().fg(ACCENT), t.check_waiting)
    } else if all_ok {
        ("●", Style::new().fg(OK), t.check_all_ok)
    } else {
        ("▲", Style::new().fg(WARN), t.check_partial)
    };
    para(f, area, &mut y, Line::from(vec![Span::styled(format!("{} ", mark), style), Span::styled(text, style)]), 2);

    if !searching {
        // Starting warm skews the idle reading: say so before they start.
        let current = [(t.label_cpu, cpu.latest()), (t.label_gpu, if has_gpu { gpu.latest() } else { None })];
        for (part, sample) in current {
            if let Some((note, color)) = sample.and_then(|s| idle_note(app, part, s.value)) {
                para(f, area, &mut y, Line::from(vec![Span::styled("▲ ", Style::new().fg(color)), Span::styled(note, Style::new().fg(color))]), 2);
            }
        }
        for tip in sensor_tips(app, &cpu, &gpu, has_gpu, cpu_board) {
            para(f, area, &mut y, Line::from(vec![Span::styled("• ", Style::new().fg(WARN)), Span::raw(tip)]), 4);
        }
    }
    if let Some(setup) = &app.setup {
        if let Some(name) = setup.monitoring_app {
            para(f, area, &mut y, Line::from(Span::styled(fill(t.monitoring_app, &[("app", name)]), Style::new().fg(DIM))), 2);
        }
    }

    let by = (y + 1).min(area.bottom().saturating_sub(1));
    draw_buttons(
        f,
        ctx,
        area,
        by,
        &[btn("Enter", t.btn_continue, Action::Continue, true), btn("D", t.btn_diagnostics, Action::Diagnostics, false)],
    );
}

fn sensor_tips(app: &App, cpu: &Channel, gpu: &Channel, has_gpu: bool, cpu_board: bool) -> Vec<String> {
    let t = &app.t;
    let mut tips = Vec::new();
    let Some(setup) = &app.setup else { return tips };
    if setup.hwinfo_shared_memory_off {
        tips.push(t.tip_hwinfo_sm.to_string());
    }
    if cpu.probe == Probe::Missing || cpu_board {
        if cfg!(windows) {
            match &setup.driver {
                DriverState::NoAdmin => tips.push(t.tip_run_as_admin.to_string()),
                DriverState::Failed(reason) => tips.push(fill(t.tip_driver_failed, &[("reason", reason)])),
                _ => {}
            }
        } else if cfg!(target_os = "macos") {
            tips.push(t.tip_macos_sudo.to_string());
        } else {
            tips.push(t.tip_linux_sensors.to_string());
        }
    }
    if has_gpu && gpu.probe == Probe::Missing {
        tips.push(t.tip_gpu_driver.to_string());
    }
    tips
}

fn sensor_row(f: &mut Frame, ctx: &mut Ctx, area: Rect, y: &mut u16, name: &str, ch: &Channel, label_w: u16) {
    let t = &ctx.app.t;
    let label = Span::styled(format!("  {}", pad(name, label_w as usize)), Style::new().fg(MUTED));
    let spans = match (ch.probe, ch.latest()) {
        (_, Some(sample)) => {
            let mut spans = vec![
                label,
                Span::styled("● ", Style::new().fg(OK)),
                Span::styled(pad(&fmt_temp(sample.value), 9), Style::new().fg(temp_color(sample.value)).add_modifier(Modifier::BOLD)),
            ];
            if let Some(src) = &ch.source {
                spans.push(Span::styled(fill(t.via, &[("source", src)]), Style::new().fg(MUTED)));
            }
            let age = sample.at.elapsed().as_secs();
            if age >= 4 {
                spans.push(Span::styled(
                    format!("  ({})", fill(t.updated_ago, &[("secs", &age.to_string())])),
                    Style::new().fg(WARN),
                ));
            }
            spans
        }
        (Probe::Searching, None) => vec![
            label,
            Span::styled(format!("{} ", pulse(ctx.app.frame)), Style::new().fg(ACCENT)),
            Span::styled(t.sensor_searching, Style::new().fg(MUTED)),
        ],
        _ => vec![
            label,
            Span::styled("× ", Style::new().fg(BAD)),
            Span::styled(t.sensor_missing, Style::new().fg(BAD)),
        ],
    };
    let line = Line::from(spans);
    let w = area.width;
    f.render_widget(line, Rect::new(area.x, *y, w, 1));
    *y += 1;
}

// ─── Options ───────────────────────────────────────────────────────

fn field_label(t: &crate::lang::Lang, field: Field) -> &'static str {
    match field {
        Field::Test => t.opt_what,
        Field::Gpu => t.opt_gpu,
        Field::Duration => t.opt_duration,
        Field::CustomSecs => t.opt_custom_secs,
        Field::Cooling => t.opt_cooling,
        Field::CoolerModel => t.opt_cooler_model,
        Field::LaptopModel => t.opt_laptop_model,
        Field::Ambient => t.opt_ambient,
    }
}

/// Draw a form's fields; returns the y after the last row.
fn draw_form_fields(f: &mut Frame, ctx: &mut Ctx, area: Rect, mut y: u16, form: &Form, fields: &[Field]) -> u16 {
    let app = ctx.app;
    let t = &app.t;
    let label_w = fields.iter().map(|fl| width_of(field_label(t, *fl))).max().unwrap_or(10) + 3;
    let x = area.x + 2 + label_w;
    let w = area.right().saturating_sub(x);

    for &field in fields {
        if y >= area.bottom() {
            break;
        }
        let focused = form.focus == field;
        let marker = if focused { Span::styled("► ", Style::new().fg(ACCENT)) } else { Span::raw("  ") };
        let label_style = if focused { Style::new().fg(ACCENT).add_modifier(Modifier::BOLD) } else { Style::new().fg(MUTED) };
        let label_rect = Rect::new(area.x, y, label_w + 2, 1);
        f.render_widget(
            Line::from(vec![marker, Span::styled(field_label(t, field), label_style)]),
            label_rect,
        );
        ctx.hit(label_rect, Action::Focus(field));

        let rows = match field {
            Field::Test => {
                let chips: Vec<String> = TEST_KINDS
                    .iter()
                    .map(|k| {
                        if *k == TestKind::Both {
                            format!("{} ({})", app.kind_label(*k), t.recommended)
                        } else {
                            app.kind_label(*k).to_string()
                        }
                    })
                    .collect();
                chips_row(f, ctx, Rect::new(x, y, w, area.bottom() - y), &chips, form.test, focused, field)
            }
            Field::Gpu => {
                let chips: Vec<String> = app
                    .gpus()
                    .iter()
                    .map(|g| match g.vram() {
                        Some(v) => format!("{} ({})", g.name, v),
                        None => g.name.clone(),
                    })
                    .collect();
                chips_row(f, ctx, Rect::new(x, y, w, area.bottom() - y), &chips, app.gpu_index, focused, field)
            }
            Field::Duration => {
                let mut chips: Vec<String> = DURATIONS
                    .iter()
                    .map(|d| {
                        if *d == 120 {
                            format!("{} ({})", app.format_duration(*d), t.recommended)
                        } else {
                            app.format_duration(*d)
                        }
                    })
                    .collect();
                chips.push(t.dur_custom.to_string());
                let rows = chips_row(f, ctx, Rect::new(x, y, w, area.bottom() - y), &chips, form.duration, focused, field);
                if focused && y + rows < area.bottom() {
                    let hint_rows = wrapped_height(&Line::from(t.duration_hint), w);
                    f.render_widget(
                        Paragraph::new(Span::styled(t.duration_hint, Style::new().fg(DIM))).wrap(Wrap { trim: true }),
                        Rect::new(x, y + rows, w, hint_rows.min(area.bottom() - y - rows)),
                    );
                    rows + hint_rows
                } else {
                    rows
                }
            }
            Field::Cooling => {
                let chips: Vec<String> = COOLING.iter().map(|c| app.cooling_label(c).to_string()).collect();
                chips_row(f, ctx, Rect::new(x, y, w, area.bottom() - y), &chips, form.cooling, focused, field)
            }
            Field::CustomSecs => text_field(f, ctx, Rect::new(x, y, 12.min(w), 1), &form.custom_secs, t.ph_custom_secs, focused, field),
            Field::CoolerModel => text_field(f, ctx, Rect::new(x, y, 44.min(w), 1), &form.cooler_model, t.ph_cooler, focused, field),
            Field::LaptopModel => text_field(f, ctx, Rect::new(x, y, 44.min(w), 1), &form.laptop_model, t.ph_laptop, focused, field),
            Field::Ambient => text_field(f, ctx, Rect::new(x, y, 34.min(w), 1), &form.ambient, t.ph_ambient, focused, field),
        };
        y += rows.max(1);
    }
    y
}

/// Radio choices, wrapping as needed. Returns rows used.
fn chips_row(f: &mut Frame, ctx: &mut Ctx, area: Rect, chips: &[String], selected: usize, focused: bool, field: Field) -> u16 {
    let mut x = area.x;
    let mut y = area.y;
    for (i, chip) in chips.iter().enumerate() {
        let is_sel = i == selected;
        let text = format!("{} {}", if is_sel { "●" } else { "○" }, chip);
        let w = width_of(&text).min(area.width);
        if x > area.x && x + w > area.right() {
            x = area.x;
            y += 1;
        }
        if y >= area.bottom() {
            break;
        }
        let style = match (is_sel, focused) {
            (true, true) => Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            (true, false) => Style::new().add_modifier(Modifier::BOLD),
            (false, _) => Style::new().fg(MUTED),
        };
        let rect = Rect::new(x, y, w, 1);
        f.render_widget(Span::styled(truncate(&text, w as usize), style), rect);
        ctx.hit(rect, Action::Choose(field, i));
        x += w + 3;
    }
    y - area.y + 1
}

/// One-line text box. Places the terminal cursor when focused.
fn text_field(f: &mut Frame, ctx: &mut Ctx, rect: Rect, input: &TextInput, placeholder: &str, focused: bool, field: Field) -> u16 {
    let inner_w = rect.width.saturating_sub(2) as usize;
    let chars: Vec<char> = input.value.chars().collect();
    // Scroll so the cursor stays visible.
    let mut start = 0;
    while UnicodeWidthStr::width(chars[start..input.cursor.max(start)].iter().collect::<String>().as_str()) > inner_w.saturating_sub(1) {
        start += 1;
    }
    let visible: String = chars[start..].iter().collect();
    let visible = truncate(&visible, inner_w);
    let style = if focused {
        Style::new().fg(Color::White).bg(Color::Blue)
    } else {
        Style::new().fg(Color::Black).bg(PANEL)
    };
    let text = if input.value.is_empty() { truncate(placeholder, inner_w) } else { visible };
    let padded = format!(" {} ", pad(&text, inner_w));
    f.render_widget(Span::styled(padded, style), rect);
    ctx.hit(rect, Action::Focus(field));
    if focused {
        let before: String = chars[start..input.cursor.min(chars.len())].iter().collect();
        let cx = rect.x + 1 + width_of(&before);
        ctx.cursor = Some(Position::new(cx.min(rect.right().saturating_sub(1)), rect.y));
    }
    1
}

fn draw_options(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let app = ctx.app;
    let t = &app.t;
    let mut y = area.y + 1;
    row(f, area, &mut y, section(t.options_title));
    y += 1;
    let fields = app.form.fields(app.is_laptop(), app.gpus().len(), false);
    y = draw_form_fields(f, ctx, area, y, &app.form, &fields);
    y += 1;
    if let Some(err) = &app.form.error {
        para(f, area, &mut y, Line::from(Span::styled(format!("▲ {}", err), Style::new().fg(BAD))), 2);
    }
    para(f, area, &mut y, Line::from(Span::styled(t.options_hint, Style::new().fg(DIM))), 2);
    let by = (y + 1).min(area.bottom().saturating_sub(1));
    draw_buttons(f, ctx, area, by, &[btn("Enter", t.btn_continue, Action::Continue, true), btn("Esc", t.btn_back, Action::Back, false)]);
}

// ─── Before you start ──────────────────────────────────────────────

fn draw_confirm(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let app = ctx.app;
    let t = &app.t;
    let kind = app.form.kind();
    let secs = app.form.duration_secs().unwrap_or(120);
    let duration = app.format_duration(secs);
    let mut y = area.y + 1;
    row(f, area, &mut y, section(t.confirm_title));

    let mut plan = format!("{}  ·  {}", app.kind_label(kind), duration);
    if kind.gpu() {
        if let Some(g) = app.selected_gpu() {
            plan.push_str(&format!("  ·  {}", g.name));
        }
    }
    row(f, area, &mut y, Line::from(Span::styled(plan, Style::new().add_modifier(Modifier::BOLD))));
    y += 1;

    let load = match kind {
        TestKind::Both => t.hu_load_both,
        TestKind::Cpu => t.hu_load_cpu,
        TestKind::Gpu => t.hu_load_gpu,
    };
    let mut bullets: Vec<(String, Color)> = vec![
        (fill(load, &[("duration", &duration)]), Color::Reset),
        (t.hu_slow.to_string(), Color::Reset),
        (t.hu_close.to_string(), Color::Reset),
        (t.hu_stop.to_string(), Color::Reset),
    ];
    if app.submits() {
        bullets.push((format!("{} {}", t.hu_auto_submit, t.submit_what), Color::Reset));
    }
    if app.on_battery == Some(true) {
        bullets.push((t.hu_battery.to_string(), WARN));
    }
    if let Some(hub) = &app.hub {
        let (cpu_now, gpu_now) = hub.with(|r| (r.cpu_temp.latest(), r.gpu_temp.latest()));
        if let Some(note) = cpu_now.filter(|_| kind.cpu()).and_then(|s| idle_note(app, t.label_cpu, s.value)) {
            bullets.push(note);
        }
        if let Some(note) = gpu_now.filter(|_| kind.gpu()).and_then(|s| idle_note(app, t.label_gpu, s.value)) {
            bullets.push(note);
        }
    }
    if let Some(hub) = &app.hub {
        let (cpu_missing, gpu_missing) = hub.with(|r| (r.cpu_temp.probe == Probe::Missing, r.gpu_temp.probe == Probe::Missing));
        if kind.cpu() && cpu_missing {
            bullets.push((t.hu_cpu_missing.to_string(), WARN));
        }
        if kind.gpu() && gpu_missing {
            bullets.push((t.hu_gpu_missing.to_string(), WARN));
        }
    }

    let inner_w = area.width.saturating_sub(6);
    let body_h: u16 = bullets.iter().map(|(b, _)| wrapped_height(&Line::from(b.as_str()), inner_w.saturating_sub(2))).sum();
    let box_h = (body_h + 2).min(area.bottom().saturating_sub(y + 2));
    let box_rect = Rect::new(area.x, y, area.width, box_h);
    let block = Block::bordered().border_style(Style::new().fg(WARN)).title(Span::styled(
        format!(" ▲ {} ", t.heads_up),
        Style::new().fg(WARN).add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(box_rect).inner(Margin { horizontal: 1, vertical: 0 });
    f.render_widget(block, box_rect);
    let mut by = inner.y;
    for (text, color) in bullets {
        para(f, inner, &mut by, Line::from(vec![Span::styled("• ", Style::new().fg(WARN)), Span::styled(text, Style::new().fg(color))]), 0);
    }
    y += box_h + 1;
    draw_buttons(f, ctx, area, y.min(area.bottom().saturating_sub(1)), &[
        btn("Enter", t.btn_start, Action::StartTest, true),
        btn("Esc", t.btn_back, Action::Back, false),
    ]);
}

// ─── Running ───────────────────────────────────────────────────────

fn draw_running(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let app = ctx.app;
    let t = &app.t;
    let Some(session) = &app.session else { return };
    let p = session.progress();
    let plan = &session.plan;
    let diagnostic = app.diag.as_ref().is_some_and(|d| matches!(d.stage, DiagStage::Stress));

    // Progress from the clock alone: it moves smoothly whatever the sensors do.
    let now = Instant::now();
    let elapsed = match (p.started, p.phase) {
        (Some(s), Phase::Running) => now.saturating_duration_since(s),
        (Some(s), _) => p.stopped.unwrap_or(now).saturating_duration_since(s),
        (None, _) => Duration::ZERO,
    }
    .min(plan.duration);
    let ratio = elapsed.as_secs_f64() / plan.duration.as_secs_f64().max(1.0);

    let mut y = area.y + 1;
    // Title line
    let title = match p.phase {
        Phase::Starting => t.starting_title,
        Phase::Running => if diagnostic { t.diag_running } else { t.running_title },
        Phase::Stopping | Phase::Finished => t.stopping_title,
    };
    let mut left = vec![
        Span::styled(format!("{} ", pulse(app.frame)), Style::new().fg(ACCENT)),
        Span::styled(title, Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!("  ·  {}", app.kind_label(plan.kind)), Style::new().fg(MUTED)),
    ];
    if diagnostic {
        left.push(Span::styled(format!("  ·  {}", t.diag_title), Style::new().fg(WARN)));
    }
    f.render_widget(Line::from(left), Rect::new(area.x, y, area.width, 1));
    if let Some(end) = p.ends_wall {
        let text = fill(t.ends_at, &[("time", &end.format("%H:%M:%S").to_string())]);
        let w = width_of(&text);
        f.render_widget(Span::styled(text, Style::new().fg(MUTED)), Rect::new(area.right().saturating_sub(w), y, w, 1));
    }
    y += 1;

    // Progress bar
    let label = format!("  {:>3}%   {} / {}", (ratio * 100.0).floor() as u32, fmt_clock(elapsed), fmt_clock(plan.duration));
    let bar_w = area.width.saturating_sub(width_of(&label));
    progress_bar(f, Rect::new(area.x, y, bar_w, 1), ratio, p.phase);
    f.render_widget(Span::styled(label, Style::new().add_modifier(Modifier::BOLD)), Rect::new(area.x + bar_w, y, area.right() - area.x - bar_w, 1));
    y += 1;
    let status = match p.phase {
        Phase::Starting => t.measuring_idle,
        Phase::Stopping | Phase::Finished => t.stopping_workers,
        Phase::Running => "",
    };
    if !status.is_empty() {
        f.render_widget(Span::styled(status, Style::new().fg(ACCENT)), Rect::new(area.x, y, area.width, 1));
    }
    y += 1;

    // Warnings and the notice go at the bottom; cards take the rest.
    let warnings: Vec<(String, Color)> = p.warnings.iter().map(|w| (warning_text(app, w), warning_color(w))).collect();
    let notice_w = area.width.saturating_sub(4);
    let notice_h = wrapped_height(&Line::from(t.running_notice), notice_w);
    let warn_h: u16 = warnings.iter().map(|(w, _)| wrapped_height(&Line::from(w.as_str()), notice_w)).sum::<u16>().min(4);
    let bottom_h = notice_h + warn_h + 1;
    let cards_h = area.bottom().saturating_sub(y + bottom_h);
    let cards = Rect::new(area.x, y, area.width, cards_h);

    let started = p.started.unwrap_or(now);
    let lagging = |ch: &Channel| {
        p.phase == Phase::Running && ch.latest().is_some_and(|s| s.at.elapsed() > Duration::from_secs(5))
    };
    let (cpu_series, gpu_series, cpu_src, gpu_src, cpu_lag, gpu_lag) = match &app.hub {
        Some(hub) => hub.with(|r| {
            let series = |ch: &Channel| -> Vec<(f64, f64)> {
                ch.since(started).map(|s| ((s.at - started).as_secs_f64(), s.value)).collect()
            };
            (
                series(&r.cpu_temp),
                series(&r.gpu_temp),
                r.cpu_temp.source.clone(),
                r.gpu_temp.source.clone(),
                lagging(&r.cpu_temp),
                lagging(&r.gpu_temp),
            )
        }),
        None => (vec![], vec![], None, None, false, false),
    };

    let mut parts: Vec<(String, Color, &Part, Vec<(f64, f64)>, Line, bool)> = Vec::new();
    if plan.kind.cpu() {
        let model = app.hw.as_ref().and_then(|h| h.cpu_model.clone()).unwrap_or_default();
        let status = Line::from(Span::styled(
            match &cpu_src {
                Some(src) => format!(
                    "{}   {}",
                    fill(t.cpu_stress_running, &[("n", &p.stress.cpu_threads.to_string())]),
                    fill(t.via, &[("source", src)])
                ),
                None => fill(t.cpu_stress_running, &[("n", &p.stress.cpu_threads.to_string())]),
            },
            Style::new().fg(DIM),
        ));
        parts.push((format!("{}  {}", t.label_cpu, model), ACCENT, &p.cpu, cpu_series, status, cpu_lag));
    }
    if plan.kind.gpu() {
        let model = plan.gpu.as_ref().map(|g| g.name.clone()).unwrap_or_default();
        let status = match &p.stress.gpu {
            GpuStress::Running { adapter, backend } => Line::from(Span::styled(
                format!(
                    "{}{}",
                    fill(t.gpu_stress_running, &[("adapter", adapter), ("backend", backend)]),
                    gpu_src.as_ref().map(|s| format!("   {}", fill(t.via, &[("source", s)]))).unwrap_or_default()
                ),
                Style::new().fg(DIM),
            )),
            GpuStress::Failed(reason) => Line::from(Span::styled(
                fill(t.warn_gpu_failed, &[("reason", reason)]),
                Style::new().fg(BAD),
            )),
            _ => Line::from(Span::styled(t.gpu_stress_starting, Style::new().fg(MUTED))),
        };
        parts.push((format!("{}  {}", t.label_gpu, model), GPU_COLOR, &p.gpu, gpu_series, status, gpu_lag));
    }

    // Side by side keeps each chart tall; only very narrow windows stack.
    let side_by_side = parts.len() == 2 && cards.width >= 72;
    let rects: Vec<Rect> = if parts.len() == 1 {
        vec![cards]
    } else if side_by_side {
        Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(cards).to_vec()
    } else {
        Layout::vertical([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(cards).to_vec()
    };
    for ((title, color, part, series, status, lag), rect) in parts.into_iter().zip(rects) {
        part_card(f, app, rect, &title, color, part, &series, plan.duration, status, lag);
    }

    // Bottom: warnings, then the reassurance notice.
    let mut by = area.bottom().saturating_sub(bottom_h) + 1;
    let bottom = Rect::new(area.x, by, area.width, bottom_h);
    for (w, color) in warnings.iter().take(3) {
        para(f, bottom, &mut by, Line::from(vec![Span::styled("▲ ", Style::new().fg(*color)), Span::styled(w.clone(), Style::new().fg(*color))]), 0);
    }
    para(
        f,
        bottom,
        &mut by,
        Line::from(vec![
            Span::styled(" i ", Style::new().fg(Color::Black).bg(ACCENT)),
            Span::styled(format!(" {}", t.running_notice), Style::new().fg(MUTED)),
        ]),
        0,
    );
}

/// Full-width bar with half-cell precision.
fn progress_bar(f: &mut Frame, rect: Rect, ratio: f64, phase: Phase) {
    let cells = rect.width as f64;
    let filled = (ratio.clamp(0.0, 1.0) * cells * 2.0).round() as u16; // half-cells
    let full = filled / 2;
    let half = filled % 2 == 1;
    let color = if phase == Phase::Stopping || phase == Phase::Finished { OK } else { ACCENT };
    let mut s = "█".repeat(full as usize);
    if half {
        s.push('▌');
    }
    let used = full + half as u16;
    f.render_widget(
        Line::from(vec![
            Span::styled(s, Style::new().fg(color)),
            Span::styled("░".repeat(rect.width.saturating_sub(used) as usize), Style::new().fg(DIM)),
        ]),
        rect,
    );
}

#[allow(clippy::too_many_arguments)]
fn part_card(
    f: &mut Frame,
    app: &App,
    rect: Rect,
    title: &str,
    color: Color,
    part: &Part,
    series: &[(f64, f64)],
    duration: Duration,
    status: Line,
    lagging: bool,
) {
    let t = &app.t;
    if rect.height < 3 {
        return;
    }
    let block = Block::bordered()
        .border_style(Style::new().fg(DIM))
        .title(Span::styled(format!(" {} ", truncate(title, rect.width.saturating_sub(4) as usize)), Style::new().fg(color).add_modifier(Modifier::BOLD)));
    let inner = block.inner(rect).inner(Margin { horizontal: 1, vertical: 0 });
    f.render_widget(block, rect);
    if inner.height == 0 {
        return;
    }

    let mut y = inner.y;
    let stat = |label: &str, value: String, style: Style| {
        vec![Span::styled(format!("{} ", label), Style::new().fg(MUTED)), Span::styled(value, style), Span::raw("   ")]
    };
    let temp_span = |v: Option<f64>| match v {
        Some(v) => (fmt_temp(v), Style::new().fg(temp_color(v)).add_modifier(Modifier::BOLD)),
        None => ("--".to_string(), Style::new().fg(DIM)),
    };

    let big = inner.height >= 10 && inner.width >= 32;
    if big {
        // Large "now" reading, stats beside it.
        let now_text = part.current.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "--".into());
        let now_color = part.current.map(temp_color).unwrap_or(DIM);
        let glyph_w = big_digits(f, Rect::new(inner.x, y, inner.width, 3), &now_text, now_color);
        f.render_widget(Span::styled("\u{00b0}C", Style::new().fg(now_color)), Rect::new(inner.x + glyph_w + 1, y, 2, 1));
        let sx = inner.x + glyph_w + 5;
        let sw = inner.right().saturating_sub(sx);
        let (pk, pk_style) = temp_span(part.peak);
        let (idle, idle_style) = temp_span(part.idle);
        f.render_widget(Line::from(stat(t.peak, pk, pk_style)), Rect::new(sx, y, sw, 1));
        f.render_widget(Line::from(stat(t.idle, idle, idle_style)), Rect::new(sx, y + 1, sw, 1));
        let load = part.usage_now.map(|u| format!("{:.0}%", u)).unwrap_or_else(|| "--".into());
        f.render_widget(Line::from(stat(t.load, load, Style::new().add_modifier(Modifier::BOLD))), Rect::new(sx, y + 2, sw, 1));
        y += 4;
    } else {
        let (now, now_style) = temp_span(part.current);
        let (pk, pk_style) = temp_span(part.peak);
        let (idle, idle_style) = temp_span(part.idle);
        let load = part.usage_now.map(|u| format!("{:.0}%", u)).unwrap_or_else(|| "--".into());
        let mut first = stat(t.now, now, now_style);
        first.extend(stat(t.peak, pk, pk_style));
        let mut second = stat(t.idle, idle, idle_style);
        second.extend(stat(t.load, load, Style::new().add_modifier(Modifier::BOLD)));
        let one_line: u16 = first.iter().chain(second.iter()).map(|s| width_of(&s.content)).sum();
        if one_line <= inner.width {
            first.extend(second);
            f.render_widget(Line::from(first), Rect::new(inner.x, y, inner.width, 1));
            y += 1;
        } else {
            f.render_widget(Line::from(first), Rect::new(inner.x, y, inner.width, 1));
            f.render_widget(Line::from(second), Rect::new(inner.x, y + 1, inner.width, 1));
            y += 2;
        }
    }

    // Status line at the bottom; chart in between.
    let status_y = inner.bottom().saturating_sub(1);
    let status = if lagging {
        Line::from(Span::styled(format!("{} {}", pulse(app.frame), t.waiting_sensor), Style::new().fg(WARN)))
    } else {
        let style = status.spans.first().map(|s| s.style).unwrap_or_default();
        let text: String = status.spans.iter().map(|s| s.content.as_ref()).collect();
        Line::from(Span::styled(truncate(&text, inner.width as usize), style))
    };
    f.render_widget(status, Rect::new(inner.x, status_y, inner.width, 1));

    // A chart needs a few rows to say anything; below that, numbers only.
    let chart_h = status_y.saturating_sub(y);
    if chart_h >= 4 {
        temp_chart(f, Rect::new(inner.x, y, inner.width, chart_h), series, part.idle, part.peak, duration, color);
    }
}

fn temp_chart(f: &mut Frame, rect: Rect, series: &[(f64, f64)], idle: Option<f64>, peak: Option<f64>, duration: Duration, color: Color) {
    let values = series.iter().map(|(_, v)| *v).chain(idle).chain(peak);
    let (lo, hi) = values.fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let (mut lo, mut hi) = if lo > hi { (30.0, 90.0) } else { ((lo / 5.0).floor() * 5.0 - 5.0, (hi / 5.0).ceil() * 5.0 + 5.0) };
    if hi - lo < 20.0 {
        let mid = (hi + lo) / 2.0;
        lo = mid - 10.0;
        hi = mid + 10.0;
    }
    let x_max = duration.as_secs_f64().max(1.0);
    // Dotted idle baseline, only where there's room for it to read as a line.
    let idle_line: Vec<(f64, f64)> = idle
        .filter(|_| rect.height >= 6)
        .map(|i| vec![(0.0, i), (x_max, i)])
        .unwrap_or_default();

    let mut datasets = vec![Dataset::default()
        .marker(Marker::HalfBlock)
        .graph_type(GraphType::Line)
        .style(Style::new().fg(color))
        .data(series)];
    if !idle_line.is_empty() {
        datasets.insert(
            0,
            Dataset::default().marker(Marker::Dot).graph_type(GraphType::Line).style(Style::new().fg(DIM)).data(&idle_line),
        );
    }
    let chart = Chart::new(datasets)
        .x_axis(
            Axis::default()
                .bounds([0.0, x_max])
                .style(Style::new().fg(DIM))
                .labels(vec![Span::raw("0:00"), Span::raw(fmt_clock(duration))]),
        )
        .y_axis(
            Axis::default()
                .bounds([lo, hi])
                .style(Style::new().fg(DIM))
                .labels(vec![Span::raw(format!("{:.0}\u{00b0}", lo)), Span::raw(format!("{:.0}\u{00b0}", hi))]),
        );
    f.render_widget(chart, rect);
}

/// 3-row digits made of half blocks. Returns the width drawn.
fn big_digits(f: &mut Frame, rect: Rect, text: &str, color: Color) -> u16 {
    fn glyph(c: char) -> [&'static str; 3] {
        match c {
            '0' => ["█▀█", "█ █", "▀▀▀"],
            '1' => ["▀█ ", " █ ", "▀▀▀"],
            '2' => ["▀▀█", "█▀▀", "▀▀▀"],
            '3' => ["▀▀█", " ▀█", "▀▀▀"],
            '4' => ["█ █", "▀▀█", "  ▀"],
            '5' => ["█▀▀", "▀▀█", "▀▀▀"],
            '6' => ["█▀▀", "█▀█", "▀▀▀"],
            '7' => ["▀▀█", "  █", "  ▀"],
            '8' => ["█▀█", "█▀█", "▀▀▀"],
            '9' => ["█▀█", "▀▀█", "▀▀▀"],
            '.' => [" ", " ", "▀"],
            '-' => ["   ", "▀▀▀", "   "],
            _ => ["   ", "   ", "   "],
        }
    }
    let mut rows = [String::new(), String::new(), String::new()];
    for (i, c) in text.chars().enumerate() {
        let g = glyph(c);
        for r in 0..3 {
            if i > 0 {
                rows[r].push(' ');
            }
            rows[r].push_str(g[r]);
        }
    }
    let w = width_of(&rows[0]);
    for (r, line) in rows.iter().enumerate() {
        if rect.y + (r as u16) < rect.bottom() {
            f.render_widget(Span::styled(line.clone(), Style::new().fg(color).add_modifier(Modifier::BOLD)), Rect::new(rect.x, rect.y + r as u16, w.min(rect.width), 1));
        }
    }
    w
}

fn warning_text(app: &App, w: &Warning) -> String {
    let t = &app.t;
    match w {
        Warning::CpuTempFlat => t.warn_cpu_flat.to_string(),
        Warning::GpuTempFlat => t.warn_gpu_flat.to_string(),
        Warning::GpuLoadLow(pct) => fill(t.warn_gpu_low, &[("pct", &format!("{:.0}", pct))]),
        Warning::CpuVeryHot(v) => fill(t.warn_cpu_hot, &[("temp", &fmt_temp(*v))]),
        Warning::GpuVeryHot(v) => fill(t.warn_gpu_hot, &[("temp", &fmt_temp(*v))]),
        Warning::GpuStressFailed(reason) => fill(t.warn_gpu_failed, &[("reason", reason)]),
        Warning::CpuSensorMissing => t.warn_cpu_missing.to_string(),
        Warning::GpuSensorMissing => t.warn_gpu_missing.to_string(),
        Warning::CpuIdleWarm(v) => fill(t.warn_idle_warm, &[("part", t.label_cpu), ("temp", &fmt_temp(*v))]),
        Warning::CpuIdleHot(v) => fill(t.warn_idle_hot, &[("part", t.label_cpu), ("temp", &fmt_temp(*v))]),
        Warning::GpuIdleWarm(v) => fill(t.warn_idle_warm, &[("part", t.label_gpu), ("temp", &fmt_temp(*v))]),
        Warning::GpuIdleHot(v) => fill(t.warn_idle_hot, &[("part", t.label_gpu), ("temp", &fmt_temp(*v))]),
    }
}

fn warning_color(w: &Warning) -> Color {
    match w {
        Warning::CpuIdleHot(_) | Warning::GpuIdleHot(_) => BAD,
        _ => WARN,
    }
}

/// Warm/hot note for a temperature read before the test starts.
fn idle_note(app: &App, part: &str, celsius: f64) -> Option<(String, Color)> {
    let t = &app.t;
    let values = [("part", part), ("temp", &fmt_temp(celsius) as &str)];
    match crate::engine::idle_level(celsius)? {
        IdleLevel::Warm => Some((fill(t.warn_idle_warm, &values), WARN)),
        IdleLevel::Hot => Some((fill(t.warn_idle_hot, &values), BAD)),
    }
}

// ─── Results ───────────────────────────────────────────────────────

fn draw_results(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let app = ctx.app;
    let t = &app.t;
    let Some(r) = &app.results else { return };
    let p = &r.progress;
    let mut y = area.y + 1;

    let (mark, color, title) = if p.stopped_early { ("▲", WARN, t.results_stopped) } else { ("●", OK, t.results_title) };
    row(f, area, &mut y, Line::from(vec![
        Span::styled(format!("{} ", mark), Style::new().fg(color)),
        Span::styled(title, Style::new().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("  ·  {}  ·  {}", app.kind_label(r.plan.kind), app.format_duration(r.plan.duration.as_secs())),
            Style::new().fg(MUTED),
        ),
    ]));
    y += 1;

    // Table
    let name_w = width_of(t.label_cpu).max(width_of(t.label_gpu)) + 4;
    let cols = [t.col_idle, t.col_peak, t.col_rise, t.col_load];
    let col_w = cols.iter().map(|c| width_of(c)).max().unwrap_or(8).max(9) + 3;
    let mut header = vec![Span::raw(" ".repeat(name_w as usize + 2))];
    for c in cols {
        header.push(Span::styled(pad(c, col_w as usize), Style::new().fg(MUTED)));
    }
    row(f, area, &mut y, Line::from(header));

    let table_row = |name: &str, part: &Part, c: Color| -> Line<'static> {
        let cell = |s: String, style: Style| Span::styled(pad(&s, col_w as usize), style);
        let temp = |v: Option<f64>| match v {
            Some(v) => cell(fmt_temp(v), Style::new().fg(temp_color(v)).add_modifier(Modifier::BOLD)),
            None => cell("--".into(), Style::new().fg(DIM)),
        };
        Line::from(vec![
            Span::styled(format!("  {}", pad(name, name_w as usize)), Style::new().fg(c).add_modifier(Modifier::BOLD)),
            temp(part.idle),
            temp(part.peak),
            cell(part.rise().map(|d| format!("{:+.1}", d)).unwrap_or("--".into()), Style::new()),
            cell(part.usage_max.map(|u| format!("{:.0}%", u)).unwrap_or("--".into()), Style::new()),
        ])
    };
    if r.plan.kind.cpu() {
        row(f, area, &mut y, table_row(t.label_cpu, &p.cpu, ACCENT));
    }
    if r.plan.kind.gpu() {
        row(f, area, &mut y, table_row(t.label_gpu, &p.gpu, GPU_COLOR));
    }

    // Cooling details
    let laptop = app.is_laptop();
    let mut details = Vec::new();
    if laptop {
        if let Some(m) = app.form.laptop_model.trimmed() {
            details.push(format!("{}: {}", t.opt_laptop_model, m));
        }
    } else if let Some(c) = app.form.cooling_type(false, &app.opts) {
        let mut s = app.cooling_label(&c).to_string();
        if let Some(m) = app.form.cooler_model.trimmed() {
            s.push_str(&format!(", {}", m));
        }
        details.push(format!("{}: {}", t.cooling_label, s));
    }
    if let Ok(Some(a)) = app.form.ambient_celsius() {
        details.push(format!("{}: {}", t.room_label, fmt_temp(a)));
    }
    if !details.is_empty() {
        row(f, area, &mut y, Line::from(Span::styled(format!("  {}", details.join("   ·   ")), Style::new().fg(MUTED))));
    }
    y += 1;

    // Checks
    row(f, area, &mut y, section(t.checks_title));
    if r.plan.kind.cpu() && r.verdict.cpu_ok && !p.warnings.contains(&Warning::CpuTempFlat) {
        row(f, area, &mut y, Line::from(vec![Span::styled("  ● ", Style::new().fg(OK)), Span::raw(t.check_cpu_ok)]));
    }
    if r.plan.kind.gpu() && r.verdict.gpu_ok && !p.warnings.contains(&Warning::GpuTempFlat) {
        row(f, area, &mut y, Line::from(vec![Span::styled("  ● ", Style::new().fg(OK)), Span::raw(t.check_gpu_ok)]));
    }
    for w in &p.warnings {
        para(f, area, &mut y, Line::from(vec![Span::styled("▲ ", Style::new().fg(warning_color(w))), Span::raw(warning_text(app, w))]), 2);
    }
    y += 1;

    // Submission
    let buttons: Vec<Btn> = match &r.submit {
        SubmitState::Ready | SubmitState::Sending(_) => {
            row(f, area, &mut y, Line::from(vec![
                Span::styled(format!("{} ", pulse(app.frame)), Style::new().fg(ACCENT)),
                Span::raw(t.submitting),
            ]));
            vec![]
        }
        SubmitState::Done { url } => {
            row(f, area, &mut y, Line::from(Span::styled(format!("● {}", t.submitted), Style::new().fg(OK).add_modifier(Modifier::BOLD))));
            para(f, area, &mut y, Line::from(Span::raw(fill(t.view_results, &[("url", url)]))), 2);
            y += 1;
            draw_compare(f, app, area, &mut y, r);
            y += 1;
            para(f, area, &mut y, Line::from(vec![
                Span::styled("\u{2665} ", Style::new().fg(Color::LightRed)),
                Span::styled(t.support_nudge, Style::new().fg(MUTED)),
            ]), 0);
            vec![
                btn("Enter", t.btn_open, Action::OpenResults, true),
                btn("C", t.btn_copy, Action::CopyLink, false),
                btn("R", t.btn_again, Action::RunAgain, false),
                btn("Q", t.btn_quit, Action::Quit, false),
            ]
        }
        SubmitState::Failed { reason, retry } => {
            para(f, area, &mut y, Line::from(Span::styled(format!("× {}", fill(t.submit_failed, &[("reason", reason)])), Style::new().fg(BAD))), 0);
            let mut b = Vec::new();
            if *retry {
                b.push(btn("Enter", t.btn_retry, Action::Submit, true));
                b.push(btn("E", t.btn_edit, Action::EditDetails, false));
            }
            b.push(btn("R", t.btn_again, Action::RunAgain, !*retry));
            b.push(btn("Q", t.btn_quit, Action::Quit, false));
            b
        }
        SubmitState::Blocked(reason) => {
            para(f, area, &mut y, Line::from(Span::styled(reason.clone(), Style::new().fg(MUTED))), 0);
            vec![btn("Enter", t.btn_again, Action::RunAgain, true), btn("Q", t.btn_quit, Action::Quit, false)]
        }
    };
    if !buttons.is_empty() {
        let by = (y + 1).min(area.bottom().saturating_sub(1));
        draw_buttons(f, ctx, area, by, &buttons);
    }
}

fn draw_compare(f: &mut Frame, app: &App, area: Rect, y: &mut u16, r: &ResultsState) {
    let t = &app.t;
    match &r.compare {
        CompareState::Idle | CompareState::Failed => {}
        CompareState::Loading(_) => {
            row(f, area, y, Line::from(Span::styled(format!("{} {}", pulse(app.frame), t.compare_loading), Style::new().fg(MUTED))));
        }
        CompareState::Ready(c) => {
            row(f, area, y, section(t.compare_title));
            let entries = [
                (t.label_cpu, c.cpu.as_ref(), r.progress.cpu.peak, r.verdict.cpu_ok),
                (t.label_gpu, c.gpu.as_ref(), r.progress.gpu.peak, r.verdict.gpu_ok),
            ];
            for (part, entry, yours, ok) in entries {
                let (Some(entry), Some(yours), true) = (entry, yours, ok) else { continue };
                let count = entry.count.unwrap_or(0);
                let model = if entry.scope.as_deref() == Some("exact") {
                    entry.model.clone().unwrap_or_default()
                } else {
                    t.similar_models.to_string()
                };
                let line = match entry.avg_load {
                    Some(avg) if count > 1 => {
                        let diff = yours - avg;
                        let verdict = if diff.abs() < 0.5 {
                            (t.compare_same.to_string(), MUTED)
                        } else if diff < 0.0 {
                            (fill(t.compare_cooler, &[("diff", &format!("{:.1}\u{00b0}C", -diff))]), OK)
                        } else {
                            (fill(t.compare_warmer, &[("diff", &format!("{:.1}\u{00b0}C", diff))]), WARN)
                        };
                        Line::from(vec![
                            Span::raw(format!(
                                "  {}",
                                fill(t.compare_line, &[
                                    ("part", part),
                                    ("yours", &fmt_temp(yours)),
                                    ("avg", &fmt_temp(avg)),
                                    ("model", &model),
                                    ("count", &count.to_string()),
                                ])
                            )),
                            Span::styled(format!("  ·  {}", verdict.0), Style::new().fg(verdict.1).add_modifier(Modifier::BOLD)),
                        ])
                    }
                    _ => Line::from(Span::raw(format!(
                        "  {}",
                        fill(t.compare_first, &[("part", part), ("model", &entry.model.clone().unwrap_or_default())])
                    ))),
                };
                para(f, area, y, line, 0);
            }
        }
    }
}

// ─── Diagnostics ───────────────────────────────────────────────────

fn draw_diagnostics(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let app = ctx.app;
    let t = &app.t;
    let Some(diag) = &app.diag else { return };
    let mut y = area.y + 1;
    row(f, area, &mut y, section(t.diag_title));
    para(f, area, &mut y, Line::from(Span::styled(t.diag_intro, Style::new().fg(MUTED))), 0);
    y += 1;

    let (status, buttons): (Line, Vec<Btn>) = match &diag.stage {
        DiagStage::Intro => (Line::default(), vec![btn("Enter", t.btn_start_diag, Action::StartDiagnostics, true), btn("Esc", t.btn_back, Action::Back, false)]),
        DiagStage::Collecting(_) | DiagStage::Stress => (
            Line::from(vec![Span::styled(format!("{} ", pulse(app.frame)), Style::new().fg(ACCENT)), Span::raw(t.diag_collecting)]),
            vec![],
        ),
        DiagStage::Uploading(_) => (
            Line::from(vec![Span::styled(format!("{} ", pulse(app.frame)), Style::new().fg(ACCENT)), Span::raw(t.diag_uploading)]),
            vec![],
        ),
        DiagStage::Done { url } => (
            Line::from(Span::styled(format!("● {}", fill(t.diag_done, &[("url", url)])), Style::new().fg(OK))),
            vec![
                btn("Enter", t.btn_open_log, Action::OpenResults, true),
                btn("C", t.btn_copy, Action::CopyLink, false),
                btn("Esc", t.btn_back, Action::Back, false),
            ],
        ),
        DiagStage::Failed(reason) => (
            Line::from(Span::styled(format!("× {}", fill(t.diag_failed, &[("reason", reason)])), Style::new().fg(BAD))),
            vec![btn("Esc", t.btn_back, Action::Back, true)],
        ),
    };

    let mut extra: Vec<Line> = Vec::new();
    if matches!(diag.stage, DiagStage::Done { .. }) {
        extra.push(Line::from(Span::styled(t.diag_hint, Style::new().fg(MUTED))));
    }
    if let (Some(path), DiagStage::Done { .. } | DiagStage::Failed(_)) = (&diag.saved, &diag.stage) {
        extra.push(Line::from(Span::styled(fill(t.diag_saved, &[("path", &path.display().to_string())]), Style::new().fg(DIM))));
    }

    let footer_h = 2
        + extra.iter().map(|l| wrapped_height(l, area.width)).sum::<u16>()
        + if buttons.is_empty() { 0 } else { 2 };
    let log_h = area.bottom().saturating_sub(y + footer_h);
    if !matches!(diag.stage, DiagStage::Intro) && log_h >= 3 {
        let lines = diag.lines.lock().unwrap_or_else(|e| e.into_inner());
        let block = Block::default().borders(Borders::ALL).border_style(Style::new().fg(DIM));
        let rect = Rect::new(area.x, y, area.width, log_h);
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let visible = inner.height as usize;
        let end = lines.len().saturating_sub(diag.scroll.min(lines.len().saturating_sub(visible)));
        let start = end.saturating_sub(visible);
        for (i, line) in lines[start..end].iter().enumerate() {
            let style = if line.starts_with("===") { Style::new().fg(ACCENT) } else if line.contains("WARNING") || line.contains("NOT ") || line.contains("ISSUE") { Style::new().fg(WARN) } else { Style::new().fg(MUTED) };
            f.render_widget(
                Span::styled(truncate(line, inner.width as usize), style),
                Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
            );
        }
        y += log_h;
    }
    y += 1;
    para(f, area, &mut y, status, 0);
    for line in extra {
        para(f, area, &mut y, line, 0);
    }
    if !buttons.is_empty() {
        let by = (y + 1).min(area.bottom().saturating_sub(1));
        draw_buttons(f, ctx, area, by, &buttons);
    }
}

// ─── Dialogs ───────────────────────────────────────────────────────

fn modal_frame(f: &mut Frame, area: Rect, width: u16, height: u16, title: &str, color: Color) -> Rect {
    let rect = centered(area, width, height);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::new().fg(color))
        .title(Span::styled(format!(" {} ", title), Style::new().fg(color).add_modifier(Modifier::BOLD)));
    let inner = block.inner(rect).inner(Margin { horizontal: 2, vertical: 1 });
    f.render_widget(block, rect);
    inner
}

fn draw_modal(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let app = ctx.app;
    let t = &app.t;
    let Some(modal) = &app.modal else { return };
    let width = area.width.saturating_sub(8).min(76);
    // While a dialog is open, only its own controls react to clicks.
    ctx.hits.clear();

    match modal {
        Modal::Help => {
            let text_w = width.saturating_sub(6);
            let body_h = wrapped_height(&Line::from(t.help_what), text_w)
                + 11
                + wrapped_height(&Line::from(t.help_mouse), text_w)
                + wrapped_height(&Line::from(t.help_privacy), text_w);
            let inner = modal_frame(f, area, width, body_h + 6, t.help_title, ACCENT);
            let content = Rect { height: inner.height.saturating_sub(2), ..inner };
            let mut y = content.y;
            para(f, content, &mut y, Line::from(t.help_what), 0);
            y += 1;
            row(f, content, &mut y, section(t.help_keys));
            let keys: [(&str, String); 8] = [
                ("Enter", t.help_key_enter.to_string()),
                ("Esc", t.help_key_esc.to_string()),
                ("Tab / arrows", t.help_key_move.to_string()),
                ("F", t.key_feedback.to_string()),
                ("S", t.key_support.to_string()),
                ("L", t.key_language.to_string()),
                ("?", t.key_help.to_string()),
                ("Q", t.key_quit.to_string()),
            ];
            for (k, v) in keys {
                row(f, content, &mut y, Line::from(vec![Span::styled(format!("  {:<14}", k), Style::new().fg(ACCENT)), Span::raw(v)]));
            }
            para(f, content, &mut y, Line::from(Span::styled(t.help_mouse, Style::new().fg(MUTED))), 0);
            y += 1;
            para(f, content, &mut y, Line::from(Span::styled(t.help_privacy, Style::new().fg(MUTED))), 0);
            let by = inner.bottom().saturating_sub(1);
            draw_buttons(f, ctx, inner, by, &[btn("Esc", t.btn_close, Action::CloseModal, true)]);
        }
        Modal::Language { cursor } => {
            let inner = modal_frame(f, area, 40, LANGUAGES.len() as u16 + 6, t.lang_title, ACCENT);
            let mut y = inner.y;
            for (i, (code, name)) in LANGUAGES.iter().enumerate() {
                let selected = i == *cursor;
                let current = *code == app.locale;
                let style = if selected { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new() };
                let rect = Rect::new(inner.x, y, inner.width, 1);
                let pad = " ".repeat(18usize.saturating_sub(UnicodeWidthStr::width(*name)));
                f.render_widget(
                    Span::styled(format!(" {} {}{} {}", if current { "●" } else { " " }, name, pad, code), style),
                    rect,
                );
                ctx.hit(rect, Action::SetLanguage(i));
                y += 1;
            }
            let by = inner.bottom().saturating_sub(1);
            draw_buttons(f, ctx, inner, by, &[btn("Esc", t.btn_close, Action::CloseModal, false)]);
        }
        Modal::StopTest => {
            let w = width.min(60);
            let body_h = wrapped_height(&Line::from(t.stop_body), w.saturating_sub(6));
            let inner = modal_frame(f, area, w, body_h + 6, t.stop_title, WARN);
            let content = Rect { height: inner.height.saturating_sub(2), ..inner };
            let mut y = content.y;
            para(f, content, &mut y, Line::from(t.stop_body), 0);
            let by = inner.bottom().saturating_sub(1);
            draw_buttons(f, ctx, inner, by, &[
                btn("Enter", t.btn_keep_running, Action::KeepRunning, true),
                btn("S", t.btn_stop_now, Action::ConfirmStop, false),
            ]);
        }
        Modal::EditDetails(form) => {
            let fields = form.fields(app.is_laptop(), 0, true);
            let inner = modal_frame(f, area, width, fields.len() as u16 * 2 + 7, t.edit_title, ACCENT);
            let mut y = draw_form_fields(f, ctx, inner, inner.y, form, &fields);
            if let Some(err) = &form.error {
                para(f, inner, &mut y, Line::from(Span::styled(format!("▲ {}", err), Style::new().fg(BAD))), 0);
            }
            let by = inner.bottom().saturating_sub(1);
            draw_buttons(f, ctx, inner, by, &[btn("Enter", t.btn_save, Action::SaveDetails, true), btn("Esc", t.btn_cancel, Action::CloseModal, false)]);
        }
        Modal::Feedback(fb) => draw_feedback(f, ctx, area, width, fb),
        Modal::Pawnio { removing, error } => {
            let w = width.min(66);
            let body_h = wrapped_height(&Line::from(t.pawn_body), w.saturating_sub(6));
            let inner = modal_frame(f, area, w, body_h + 6, t.pawn_title, ACCENT);
            let mut y = inner.y;
            if removing.is_some() {
                row(f, inner, &mut y, Line::from(vec![Span::styled(format!("{} ", pulse(app.frame)), Style::new().fg(ACCENT)), Span::raw(t.pawn_removing)]));
            } else if let Some(e) = error {
                para(f, inner, &mut y, Line::from(Span::styled(fill(t.pawn_remove_failed, &[("reason", e)]), Style::new().fg(BAD))), 0);
                let by = inner.bottom().saturating_sub(1);
                draw_buttons(f, ctx, inner, by, &[btn("Enter", t.btn_close, Action::KeepPawnio, true)]);
            } else {
                para(f, inner, &mut y, Line::from(t.pawn_body), 0);
                let by = inner.bottom().saturating_sub(1);
                draw_buttons(f, ctx, inner, by, &[
                    btn("K", t.btn_keep, Action::KeepPawnio, true),
                    btn("U", t.btn_uninstall, Action::UninstallPawnio, false),
                ]);
            }
        }
    }
}

fn draw_feedback(f: &mut Frame, ctx: &mut Ctx, area: Rect, width: u16, fb: &FeedbackForm) {
    let t = &ctx.app.t;
    let height = 22.min(area.height.saturating_sub(2));
    let inner = modal_frame(f, area, width, height, t.fb_title, ACCENT);
    let mut y = inner.y;

    match &fb.state {
        FbState::Sent => {
            row(f, inner, &mut y, Line::from(Span::styled(format!("● {}", t.fb_sent), Style::new().fg(OK).add_modifier(Modifier::BOLD))));
            let by = inner.bottom().saturating_sub(1);
            draw_buttons(f, ctx, inner, by, &[btn("Enter", t.btn_close, Action::CloseModal, true)]);
            return;
        }
        FbState::Failed(reason) => {
            para(f, inner, &mut y, Line::from(Span::styled(fill(t.fb_failed, &[("reason", reason)]), Style::new().fg(BAD))), 0);
            para(f, inner, &mut y, Line::from(t.fb_fallback), 0);
            let by = inner.bottom().saturating_sub(1);
            draw_buttons(f, ctx, inner, by, &[btn("Esc", t.btn_close, Action::CloseModal, true)]);
            return;
        }
        _ => {}
    }

    para(f, inner, &mut y, Line::from(Span::styled(t.fb_intro, Style::new().fg(MUTED))), 0);
    y += 1;
    let focus_style = |field: FbField| {
        if fb.focus == field { Style::new().fg(ACCENT).add_modifier(Modifier::BOLD) } else { Style::new().fg(MUTED) }
    };
    let marker = |field: FbField| if fb.focus == field { "► " } else { "  " };

    // Rating
    let label_rect = Rect::new(inner.x, y, inner.width, 1);
    f.render_widget(Line::from(vec![Span::styled(marker(FbField::Rating), Style::new().fg(ACCENT)), Span::styled(t.fb_rating, focus_style(FbField::Rating))]), label_rect);
    ctx.hit(label_rect, Action::FbFocus(FbField::Rating));
    y += 1;
    let mut x = inner.x + 2;
    for n in 1..=5u8 {
        let on = fb.rating.is_some_and(|r| n <= r);
        let style = if on { Style::new().fg(Color::Black).bg(WARN).add_modifier(Modifier::BOLD) } else { Style::new().fg(Color::Black).bg(PANEL) };
        let rect = Rect::new(x, y, 3, 1);
        f.render_widget(Span::styled(format!(" {} ", n), style), rect);
        ctx.hit(rect, Action::FbRating(n));
        x += 4;
    }
    f.render_widget(Span::styled(format!("  {}", t.fb_rating_hint), Style::new().fg(DIM)), Rect::new(x, y, inner.right().saturating_sub(x), 1));
    y += 2;

    // Category
    let cat_label = format!("{}{}   ", marker(FbField::Category), t.fb_type);
    let cat_w = width_of(&cat_label);
    f.render_widget(Span::styled(cat_label, focus_style(FbField::Category)), Rect::new(inner.x, y, cat_w, 1));
    let cats: Vec<String> = [t.fb_bug, t.fb_idea, t.fb_praise, t.fb_other].iter().map(|s| s.to_string()).collect();
    let mut x = inner.x + cat_w;
    for (i, c) in cats.iter().enumerate() {
        let text = format!("{} {}", if i == fb.category { "●" } else { "○" }, c);
        let w = width_of(&text);
        let style = if i == fb.category { Style::new().add_modifier(Modifier::BOLD).fg(if fb.focus == FbField::Category { ACCENT } else { Color::Reset }) } else { Style::new().fg(MUTED) };
        let rect = Rect::new(x, y, w.min(inner.right().saturating_sub(x)), 1);
        f.render_widget(Span::styled(text, style), rect);
        ctx.hit(rect, Action::FbCategory(i));
        x += w + 3;
    }
    y += 2;

    // Message (wraps; typing appends)
    let msg_label = Rect::new(inner.x, y, inner.width, 1);
    f.render_widget(Line::from(vec![Span::styled(marker(FbField::Message), Style::new().fg(ACCENT)), Span::styled(t.fb_message, focus_style(FbField::Message))]), msg_label);
    ctx.hit(msg_label, Action::FbFocus(FbField::Message));
    y += 1;
    let msg_h = 4u16;
    let msg_rect = Rect::new(inner.x + 2, y, inner.width.saturating_sub(2), msg_h);
    let style = if fb.focus == FbField::Message {
        Style::new().fg(Color::White).bg(Color::Blue)
    } else {
        Style::new().fg(Color::Black).bg(PANEL)
    };
    let text = if fb.message.value.is_empty() {
        t.fb_message_ph.to_string()
    } else {
        let mut v = fb.message.value.clone();
        if fb.focus == FbField::Message {
            v.push('▌');
        }
        v
    };
    f.render_widget(Paragraph::new(text).style(style).wrap(Wrap { trim: false }), msg_rect);
    ctx.hit(msg_rect, Action::FbFocus(FbField::Message));
    y += msg_h + 1;

    // Email
    let email_label = format!("{}{}   ", marker(FbField::Email), t.fb_email);
    let ew = width_of(&email_label);
    f.render_widget(Span::styled(email_label, focus_style(FbField::Email)), Rect::new(inner.x, y, ew, 1));
    let field_w = inner.width.saturating_sub(ew).min(48);
    text_field(f, ctx, Rect::new(inner.x + ew, y, field_w, 1), &fb.email, t.fb_email_ph, fb.focus == FbField::Email, Field::Ambient);
    // The Field::Ambient hit above is a placeholder; point it at the email field.
    if let Some(last) = ctx.hits.last_mut() {
        last.1 = Action::FbFocus(FbField::Email);
    }
    if fb.focus != FbField::Email {
        ctx.cursor = None;
    }
    y += 2;

    // Include system info
    let include = format!("{}[{}] {}", marker(FbField::Include), if fb.include_info { "x" } else { " " }, t.fb_include);
    let inc_rect = Rect::new(inner.x, y, inner.width, wrapped_height(&Line::from(include.as_str()), inner.width).min(2));
    f.render_widget(Paragraph::new(Span::styled(include, focus_style(FbField::Include))).wrap(Wrap { trim: true }), inc_rect);
    ctx.hit(inc_rect, Action::FbToggleInclude);
    y += inc_rect.height + 1;

    if let Some(err) = &fb.error {
        row(f, inner, &mut y, Line::from(Span::styled(format!("▲ {}", err), Style::new().fg(BAD))));
    }

    let by = inner.bottom().saturating_sub(1);
    if let FbState::Sending(_) = fb.state {
        f.render_widget(Line::from(vec![Span::styled(format!("{} ", pulse(ctx.app.frame)), Style::new().fg(ACCENT)), Span::raw(t.fb_sending)]), Rect::new(inner.x, by, inner.width, 1));
    } else {
        let send_focused = fb.focus == FbField::Send;
        let send_label = if send_focused { format!("{} ◄", t.btn_send) } else { t.btn_send.to_string() };
        draw_buttons(f, ctx, inner, by, &[
            Btn { key: "Enter", label: send_label, action: Action::FbSend, primary: true },
            btn("Esc", t.btn_cancel, Action::CloseModal, false),
        ]);
    }
}

fn draw_toast(f: &mut Frame, ctx: &mut Ctx, area: Rect) {
    let Some((msg, _)) = &ctx.app.toast else { return };
    let text = truncate(msg, area.width.saturating_sub(6) as usize);
    let w = width_of(&text) + 2;
    let rect = Rect::new(area.right().saturating_sub(w + 1), area.bottom().saturating_sub(2), w, 1);
    f.render_widget(Clear, rect);
    f.render_widget(Span::styled(format!(" {} ", text), Style::new().fg(Color::Black).bg(Color::White)), rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Session, TestPlan};
    use crate::gpus::{GpuDevice, GpuKind};
    use crate::hardware::HardwareInfo;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn app(locale: &str) -> App {
        let opts = LaunchOptions {
            api_url: crate::api::DEFAULT_API_URL.into(),
            no_submit: false,
            demo: true,
            test: None,
            duration: Some(60),
            cooling_type: None,
            cooling_model: None,
            ambient_temp: None,
            diagnostics: false,
        };
        let mut app = App::new(locale.to_string(), opts);
        let gpus = vec![
            GpuDevice {
                name: "NVIDIA GeForce RTX 4080 SUPER".into(),
                vram_bytes: Some(16 << 30),
                kind: GpuKind::Discrete,
                vendor_id: Some(0x10DE),
                device_id: Some(0x2702),
                nvml_index: None,
                sysfs_device: None,
            },
            GpuDevice {
                name: "Intel(R) UHD Graphics 770".into(),
                vram_bytes: None,
                kind: GpuKind::Integrated,
                vendor_id: Some(0x8086),
                device_id: Some(0x4680),
                nvml_index: None,
                sysfs_device: None,
            },
        ];
        app.hub = Some(Arc::new(crate::sensors::SensorHub::start(None, gpus.first().cloned(), gpus.len(), true)));
        app.hw = Some(HardwareInfo {
            cpu_model: Some("AMD Ryzen 7 7800X3D".into()),
            cpu_cores: Some(8),
            cpu_threads: Some(16),
            os: Some("Windows 11 (26100)".into()),
            is_laptop: false,
            gpus,
        });
        app.screen = Screen::Home;
        app
    }

    fn render(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
            .collect()
    }

    /// Every screen and dialog renders in every language at the minimum and
    /// the default console size without panicking or overflowing.
    #[test]
    fn all_screens_render_in_all_languages() {
        for (locale, _) in crate::lang::LANGUAGES {
            for (w, h) in [(80, 24), (120, 30)] {
                let mut a = app(locale);
                for screen in [Screen::Home, Screen::Options, Screen::Confirm] {
                    a.screen = screen;
                    render(&mut a, w, h);
                }
                a.act(Action::Feedback);
                render(&mut a, w, h);
                a.modal = Some(Modal::Help);
                render(&mut a, w, h);
                a.modal = Some(Modal::StopTest);
                render(&mut a, w, h);
                a.modal = Some(Modal::Pawnio { removing: None, error: None });
                render(&mut a, w, h);
                a.modal = None;

                let plan = TestPlan { kind: crate::engine::TestKind::Both, duration: Duration::from_secs(60), gpu: a.selected_gpu().cloned() };
                a.session = Some(Session::start(plan, a.hub.clone().unwrap()));
                a.screen = Screen::Running;
                render(&mut a, w, h);
                if let Some(s) = &a.session {
                    s.request_stop();
                }
            }
        }
    }

    /// Secondary buttons and fields must never be light text on dark grey
    /// (unreadable in the Windows console).
    #[test]
    fn no_light_text_on_dark_grey() {
        let mut a = app("en");
        for screen in [Screen::Home, Screen::Options, Screen::Confirm] {
            a.screen = screen;
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal.draw(|f| draw(f, &mut a)).unwrap();
            let buffer = terminal.backend().buffer();
            for cell in buffer.content() {
                assert_ne!(cell.bg, DIM, "dark grey background on {:?} ({:?})", screen, cell.symbol());
            }
            if screen == Screen::Home {
                // The "D" key chip of the Diagnostics button: black on the light panel.
                let d = buffer.content().iter().find(|c| c.symbol() == "D" && c.bg == PANEL);
                assert!(d.is_some_and(|c| c.fg == Color::Black), "Diagnostics key chip not black on light grey");
            }
        }
    }

    /// Print a screen for eyeballing: cargo test preview -- --nocapture --ignored
    #[test]
    #[ignore]
    fn preview() {
        let locale = std::env::var("PREVIEW_LANG").unwrap_or_else(|_| "ko".into());
        let mut a = app(&locale);
        for screen in [Screen::Home, Screen::Options, Screen::Confirm] {
            a.screen = screen;
            std::thread::sleep(Duration::from_millis(1200));
            println!("{}", render(&mut a, 80, 24).join("\n"));
        }
    }
}
