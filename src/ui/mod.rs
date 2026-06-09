pub mod format;
#[cfg(test)]
mod preview;
mod screens;
pub mod theme;

pub(super) const LABEL_W: usize = 12;
pub(super) const LABEL_TEXT_W: usize = LABEL_W - 2;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::app::{App, Modal, Route, ToastKind};
use crate::types::NetStatus;
use theme::Theme;

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 16;

pub fn render(f: &mut Frame, app: &mut App) {
    let theme = app.theme;
    let area = f.area();
    app.last_area = area;
    f.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let p = Paragraph::new(Line::from(Span::styled(
            "silo needs at least 60x16 — please resize",
            Style::default().fg(theme.text),
        )))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });
        let rect = centered_rect(area.width, 1, area);
        f.render_widget(p, rect);
        return;
    }

    let footer_text = footer_hints(app);
    let footer_h = footer_height(&footer_text, area.width);

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(footer_h),
    ])
    .split(area);

    status_bar(f, app, chunks[0]);

    match app.route {
        Route::ProfileSelect => screens::profile_select(f, app, chunks[1]),
        Route::Unlock => screens::unlock(f, app, chunks[1]),
        Route::Setup => screens::setup(f, app, chunks[1]),
        Route::WalletList => screens::wallet_list(f, app, chunks[1]),
        Route::WalletDetail => screens::wallet_detail(f, app, chunks[1]),
        Route::Send => screens::send(f, app, chunks[1]),
        Route::History => screens::history(f, app, chunks[1]),
        Route::AuditLog => screens::audit_log(f, app, chunks[1]),
        Route::Settings => screens::settings(f, app, chunks[1]),
    }

    footer(f, app, &footer_text, chunks[2]);
    render_toasts(f, app, chunks[2]);

    if app.modal.is_some() {
        render_modal(f, app, area);
    }

    render_confetti(f, app);
}

fn render_confetti(f: &mut Frame, app: &App) {
    if app.confetti.is_empty() {
        return;
    }
    let area = f.area();
    let bg = app.theme.bg;
    let buf = f.buffer_mut();
    let mut utf8 = [0u8; 4];
    for p in &app.confetti {
        if p.x < area.x as f32 || p.y < area.y as f32 {
            continue;
        }
        let x = p.x.round() as i64;
        let y = p.y.round() as i64;
        if x >= (area.x + area.width) as i64 || y >= (area.y + area.height) as i64 {
            continue;
        }
        let color = blend(bg, p.color, p.life.clamp(0.0, 1.0).powf(0.6));
        if let Some(cell) = buf.cell_mut((x as u16, y as u16)) {
            cell.set_symbol(p.glyph.encode_utf8(&mut utf8));
            cell.set_fg(color);
        }
    }
}

pub fn panel(title: impl Into<String>, focused: bool, theme: &Theme) -> Block<'static> {
    let border_color = if focused {
        theme.border_focus
    } else {
        theme.border_idle
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            format!(" {} ", title.into()),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg))
}

pub fn blend(a: ratatui::style::Color, b: ratatui::style::Color, t: f32) -> ratatui::style::Color {
    use ratatui::style::Color;
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
        }
        _ => a,
    }
}

fn pulse(frame: usize) -> f32 {
    let p = frame as f32 / (super::app::SPINNER.len() - 1) as f32;
    1.0 - (p * 2.0 - 1.0).abs()
}

fn shimmer_line(
    text: &str,
    frame: u64,
    base: ratatui::style::Color,
    hi: ratatui::style::Color,
) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n == 0 {
        return Line::from("");
    }
    let period = (n + 6) as u64;
    let pos = ((frame / 2) % period) as i64;
    let mut spans = Vec::with_capacity(n + 2);
    spans.push(Span::raw(" "));
    for (i, c) in chars.into_iter().enumerate() {
        let t = match (i as i64 - pos).abs() {
            0 => 1.0,
            1 => 0.45,
            2 => 0.15,
            _ => 0.0,
        };
        spans.push(Span::styled(
            c.to_string(),
            Style::default()
                .fg(blend(base, hi, t))
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn status_bar(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let mut dot_color = match app.net_status {
        NetStatus::Online => theme.usd,
        NetStatus::Syncing => theme.warn,
        NetStatus::Offline => theme.danger,
    };
    if app.net_status == NetStatus::Syncing || !app.reconcile_done {
        dot_color = blend(dot_color, theme.bg, 0.55 * (1.0 - pulse(app.spinner_frame)));
    }

    let mut left = match app.net_status {
        NetStatus::Online => vec![Span::styled("● ", Style::default().fg(dot_color))],
        NetStatus::Syncing => vec![Span::styled("● syncing ", Style::default().fg(dot_color))],
        NetStatus::Offline => vec![Span::styled("● offline ", Style::default().fg(dot_color))],
    };
    if app.update_available().is_some() {
        left.push(Span::styled(
            "update available ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        left.push(Span::styled(
            "· press U for upgrade command",
            Style::default().fg(theme.text_muted),
        ));
    }
    if !app.reconcile_done {
        left.push(Span::styled(
            format!("  {} reconciling", app.spinner()),
            Style::default().fg(theme.warn),
        ));
    }
    let loading_balances = app.wallets.iter().any(|w| w.balance_lamports.is_none());
    if app.inflight > 0 || loading_balances {
        let label = if loading_balances {
            "loading balances"
        } else {
            ""
        };
        left.push(Span::styled(
            format!("  {} {label}", app.spinner()),
            Style::default().fg(theme.accent),
        ));
    }

    let price = format::fmt_price(app.price_now());
    let arrow = if app.price_flash > 0.05 {
        if app.price_up { " ▲" } else { " ▼" }
    } else {
        ""
    };
    let flash_to = if app.price_up { theme.usd } else { theme.warn };
    let price_color = blend(theme.text_muted, flash_to, app.price_flash);
    let right = Span::styled(format!("{price}{arrow}"), Style::default().fg(price_color));

    let mut title = shimmer_line("silo", app.anim_frame(), theme.text, theme.accent);
    title.spans.push(Span::styled(
        format!("v{}  ", crate::update::CURRENT_VERSION),
        Style::default().fg(theme.text_muted),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border_idle))
        .title(title)
        .style(Style::default().bg(theme.bg));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cols = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(right.width() as u16 + 1),
    ])
    .split(inner);
    let left = clamp_spans(left, cols[0].width as usize);
    f.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    f.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        cols[1],
    );
}

fn footer_hints(app: &App) -> String {
    let hints = match app.route {
        Route::ProfileSelect => {
            "↑↓ move · enter open · n new wallet · r rename · d delete · q quit"
        }
        Route::Unlock => "type passphrase · enter unlock · ^C quit",
        Route::Setup => "c create · i import · enter continue · esc back",
        Route::WalletList => "enter open · s send · n new · c copy · ^L lock · q quit",
        Route::WalletDetail => "s send · M →master · F fund · c copy · h history · esc back",
        Route::Send => {
            "tab field · c SOL/fiat · m max · a all · enter review · ^V paste · esc cancel"
        }
        Route::History => "↑↓ scroll · c copy txid · t note · esc back",
        Route::AuditLog => "↑↓ scroll · esc back",
        Route::Settings => {
            "e edit RPC · u currency · p priority · +/- auto-lock · L lock now · esc back"
        }
    };
    let mut hints = hints.to_string();
    if app.update_available().is_some()
        && matches!(app.route, Route::WalletList | Route::WalletDetail)
    {
        hints.push_str(" · U changelog");
    }
    hints
}

fn footer_height(hints: &str, width: u16) -> u16 {
    let avail = (width as usize).saturating_sub(1).max(1);
    let lines = format::wrap_lines(hints, avail).len() as u16;
    lines.clamp(1, 3)
}

fn footer(f: &mut Frame, app: &App, hints: &str, area: Rect) {
    let p = Paragraph::new(Line::from(Span::styled(
        format!(" {hints}"),
        Style::default().fg(app.theme.text_muted),
    )))
    .wrap(Wrap { trim: true })
    .style(Style::default().bg(app.theme.surface));
    f.render_widget(p, area);
}

fn render_toasts(f: &mut Frame, app: &App, footer_area: Rect) {
    if app.toasts.is_empty() {
        return;
    }
    let theme = &app.theme;
    let frame = f.area();
    let n = app.toasts.len() as u16;
    let width = 52.min(frame.width.saturating_sub(2));
    let height = n + 2;
    if frame.height < height {
        return;
    }
    let bottom = footer_area.y + footer_area.height;
    let y = bottom.saturating_sub(height);
    let rect = Rect {
        x: frame.x + frame.width.saturating_sub(width + 1),
        y,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let now = std::time::Instant::now();
    let lines: Vec<Line> = app
        .toasts
        .iter()
        .map(|t| {
            let base = match t.kind {
                ToastKind::Info => theme.accent,
                ToastKind::Success => theme.usd,
                ToastKind::Error => theme.danger,
            };
            let mark = match t.kind {
                ToastKind::Info => "•",
                ToastKind::Success => "✓",
                ToastKind::Error => "✗",
            };
            let age = now.duration_since(t.created).as_millis() as f32;
            let life = crate::app::TOAST_TTL.as_millis() as f32;
            let alpha = if age < 150.0 {
                age / 150.0
            } else if age > life - 500.0 {
                ((life - age) / 500.0).max(0.0)
            } else {
                1.0
            };
            let mark_c = blend(theme.surface, base, alpha);
            let text_c = blend(theme.surface, theme.text, alpha);
            Line::from(vec![
                Span::styled(format!(" {mark} "), Style::default().fg(mark_c)),
                Span::styled(t.text.clone(), Style::default().fg(text_c)),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border_idle))
        .style(Style::default().bg(theme.surface));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn clamp_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if total <= max {
        return spans;
    }
    if max == 0 {
        return Vec::new();
    }
    let budget = max.saturating_sub(1);
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    for s in spans {
        let w = s.content.chars().count();
        if used + w <= budget {
            used += w;
            out.push(s);
        } else {
            let take = budget - used;
            let kept: String = s.content.chars().take(take).collect();
            out.push(Span::styled(kept, s.style));
            break;
        }
    }
    out.push(Span::raw("…"));
    out
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_message_modal(
    f: &mut Frame,
    theme: &Theme,
    area: Rect,
    width: u16,
    title: &str,
    body: &str,
    border: ratatui::style::Color,
    hint: &str,
) {
    let inner_w = width.min(area.width).saturating_sub(2).max(1) as usize;
    let body_lines = format::wrap_lines(body, inner_w);
    let want = 2 + 1 + body_lines.len() + 1 + 1;
    let height = (want as u16).min(area.height);
    let rect = centered_rect(width, height, area);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.surface));

    let avail = rect.height.saturating_sub(2) as usize;
    let mut lines = vec![Line::from("")];
    for l in &body_lines {
        lines.push(Line::from(Span::styled(
            l.clone(),
            Style::default().fg(theme.text),
        )));
    }
    lines.push(Line::from(""));
    let hint_line = Line::from(Span::styled(
        hint.to_string(),
        Style::default().fg(theme.text_muted),
    ));
    if lines.len() + 1 > avail {
        lines.truncate(avail.saturating_sub(1));
    }
    while lines.len() + 1 < avail {
        lines.push(Line::from(""));
    }
    lines.push(hint_line);

    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn render_modal(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    match app.modal.as_ref().unwrap() {
        Modal::ConfirmSend => render_confirm_send(f, app, area),
        Modal::Confirm { title, body, .. } => {
            render_message_modal(
                f,
                theme,
                area,
                62,
                title,
                body,
                theme.warn,
                "  Enter confirm · Esc cancel",
            );
        }
        Modal::Error { title, body } => {
            render_message_modal(
                f,
                theme,
                area,
                60,
                title,
                body,
                theme.danger,
                "press Enter to dismiss",
            );
        }
        Modal::Prompt { title, kind } => {
            let make_block = || {
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.border_focus))
                    .title(Span::styled(
                        format!(" {title} "),
                        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                    ))
                    .style(Style::default().bg(theme.surface))
            };
            if kind.multiline() {
                const TEXT_W: usize = 59;
                const MAX_LINES: usize = 8;
                let wrapped = format::wrap_lines(&app.input.prompt_text, TEXT_W);
                let shown = wrapped.len().clamp(1, MAX_LINES);
                let start = wrapped.len().saturating_sub(shown);
                let rect = centered_rect(64, shown as u16 + 5, area);
                f.render_widget(Clear, rect);
                let mut lines = vec![Line::from("")];
                for (i, text) in wrapped[start..].iter().enumerate() {
                    let last = i == shown - 1;
                    let mut spans = vec![
                        Span::raw("  "),
                        Span::styled(text.clone(), Style::default().fg(theme.accent)),
                    ];
                    if last {
                        spans.push(Span::styled("▏", Style::default().fg(theme.accent)));
                    }
                    lines.push(Line::from(spans));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  ctrl+s save · enter newline · esc cancel",
                    Style::default().fg(theme.text_muted),
                )));
                f.render_widget(Paragraph::new(lines).block(make_block()), rect);
            } else {
                let rect = centered_rect(60, 6, area);
                f.render_widget(Clear, rect);
                let p = Paragraph::new(vec![
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("  ", Style::default()),
                        Span::styled(
                            format::input_tail(&app.input.prompt_text, 55),
                            Style::default().fg(theme.accent),
                        ),
                        Span::styled("▏", Style::default().fg(theme.accent)),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  enter save · esc cancel",
                        Style::default().fg(theme.text_muted),
                    )),
                ])
                .block(make_block());
                f.render_widget(p, rect);
            }
        }
    }
}

fn render_confirm_send(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let Some(ps) = app.pending_send.as_ref() else {
        return;
    };
    let from = app.wallets.iter().find(|w| w.id == ps.from_id);
    let from_label = from
        .map(|w| format!("{} (#{})", w.display_name(), w.account_index))
        .unwrap_or_default();
    let from_addr = from.map(|w| w.pubkey.clone()).unwrap_or_default();
    let dest = app.wallets.iter().find(|w| w.pubkey == ps.to);
    let dest_internal = dest.map(|w| w.display_name());

    let price = app.price_now();
    let total = ps.lamports.saturating_add(ps.fee);

    let armed = app.send_confirm_armed && app.pending_send_is_large();
    let large_pct = from
        .and_then(|w| w.balance_lamports)
        .filter(|bal| *bal > 0)
        .map(|bal| (total.saturating_mul(100) / bal).min(100));
    let after = from
        .and_then(|w| w.balance_lamports)
        .map(|bal| bal.saturating_sub(total));

    let width = 70u16;
    let inner_w = width.min(area.width).saturating_sub(2).max(1) as usize;
    let addr_w = inner_w.saturating_sub(LABEL_W).max(1);
    let from_addr = format::elide_middle(&from_addr, addr_w);
    let to_addr = format::elide_middle(&ps.to, addr_w);

    let label = |s: &str| {
        Span::styled(
            format!("  {s:<w$}", w = LABEL_TEXT_W),
            Style::default().fg(theme.text_muted),
        )
    };
    let val = |s: String| Span::styled(s, Style::default().fg(theme.text));
    let indent = || Span::styled(format!("{:w$}", "", w = LABEL_W), Style::default());

    let mut send_spans = vec![
        label("send"),
        Span::styled(
            format!("{} SOL", format::fmt_sol_exact(ps.lamports)),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(p) = price {
        let usd = format::fmt_usd(Some(p), ps.lamports);
        send_spans.push(Span::styled(
            format!("  ≈ {usd}"),
            Style::default().fg(theme.text_muted),
        ));
        let age = p.age_secs();
        if age > 60 {
            let stale = age > crate::price::STALE_AFTER_SECS;
            let color = if stale { theme.warn } else { theme.text_muted };
            send_spans.push(Span::styled(
                format!("  (price {}m old)", age / 60),
                Style::default().fg(color),
            ));
        }
    } else {
        send_spans.push(Span::styled(
            "  ≈ usd unavailable",
            Style::default().fg(theme.text_muted),
        ));
    }

    let mut body = vec![Line::from(""), Line::from(send_spans)];

    if armed {
        let pct = large_pct.unwrap_or(100);
        body.push(Line::from(Span::styled(
            format!("  ⚠ this sends ~{pct}% of this wallet's balance"),
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        )));
    }

    body.push(Line::from(vec![label("from"), val(from_label)]));
    body.push(Line::from(vec![
        indent(),
        Span::styled(from_addr, Style::default().fg(theme.text)),
    ]));

    let to_span = match &dest_internal {
        Some(name) => Span::styled(format!("({name})"), Style::default().fg(theme.text_muted)),
        None => Span::styled(
            "(external)".to_string(),
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ),
    };
    body.push(Line::from(vec![label("to"), to_span]));
    body.push(Line::from(vec![
        indent(),
        Span::styled(to_addr, Style::default().fg(theme.text)),
    ]));
    if dest_internal.is_none() {
        body.push(Line::from(vec![
            indent(),
            Span::styled(
                "⚠ leaving your wallets",
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    body.push(Line::from(vec![
        label("fee"),
        val(format!("{} SOL", format::fmt_sol_exact(ps.fee))),
    ]));
    body.push(Line::from(vec![
        label("total"),
        Span::styled(
            format!("{} SOL", format::fmt_sol_exact(total)),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if let Some(after) = after {
        body.push(Line::from(vec![
            label("after"),
            Span::styled(
                format!("≈ {} SOL", format::fmt_sol_exact(after)),
                Style::default().fg(theme.text_muted),
            ),
        ]));
    }

    let refreshing = ps.prepared_at.elapsed() > crate::input::BLOCKHASH_REFRESH_AFTER;
    if refreshing {
        body.push(Line::from(Span::styled(
            "  Refreshing network details…",
            Style::default().fg(theme.text_muted),
        )));
    }

    let send_label = if armed {
        "confirm large send"
    } else {
        "Send now"
    };
    let action = Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            " Enter ",
            Style::default()
                .bg(theme.warn)
                .fg(theme.bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {send_label}"),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled("   ", Style::default()),
        Span::styled(
            " Esc ",
            Style::default().bg(theme.border_idle).fg(theme.text),
        ),
        Span::styled(" cancel", Style::default().fg(theme.text)),
    ]);
    let helper = Line::from(Span::styled(
        "  signs and broadcasts the transaction",
        Style::default().fg(theme.text_muted),
    ));

    let want = (2 + body.len() + 2 + 1) as u16;
    let height = want.min(area.height).max(3);
    let rect = centered_rect(width, height, area);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.warn))
        .title(Span::styled(
            " Confirm send ",
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.surface));

    let avail = rect.height.saturating_sub(2) as usize;
    if body.len() + 3 > avail {
        body.truncate(avail.saturating_sub(3));
    }
    while body.len() + 3 < avail {
        body.push(Line::from(""));
    }
    body.push(Line::from(""));
    body.push(helper);
    body.push(action);
    f.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: false }),
        rect,
    );
}
