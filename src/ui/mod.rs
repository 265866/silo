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

pub fn render(f: &mut Frame, app: &mut App) {
    let theme = app.theme;
    let area = f.area();
    app.last_area = area;
    f.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
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

    footer(f, app, chunks[2]);
    render_toasts(f, app, chunks[1]);

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
            "Update available! ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        left.push(Span::styled(
            app.install_method.upgrade_hint().to_string(),
            Style::default().fg(theme.text),
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
    f.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    f.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        cols[1],
    );
}

fn footer(f: &mut Frame, app: &App, area: Rect) {
    let hints = match app.route {
        Route::ProfileSelect => {
            "↑↓ move · enter open · n new wallet · r rename · d delete · q quit"
        }
        Route::Unlock => "type passphrase · enter unlock · ^C quit",
        Route::Setup => "c create · i import · enter continue · esc back",
        Route::WalletList => {
            "↑↓ move · enter open · s send · M →master · F fund · n new sub · c copy · l label · t note · x archive · h history · a audit · g settings · r refresh · ^L lock · q quit"
        }
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
    let p = Paragraph::new(Line::from(Span::styled(
        format!(" {hints}"),
        Style::default().fg(app.theme.text_muted),
    )))
    .wrap(Wrap { trim: true })
    .style(Style::default().bg(app.theme.surface));
    f.render_widget(p, area);
}

fn render_toasts(f: &mut Frame, app: &App, area: Rect) {
    if app.toasts.is_empty() {
        return;
    }
    let theme = &app.theme;
    let n = app.toasts.len() as u16;
    let width = 52.min(area.width.saturating_sub(2));
    let height = n + 2;
    if area.height < height + 1 {
        return;
    }
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width + 1),
        y: area.y + area.height.saturating_sub(height + 1),
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

fn render_modal(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    match app.modal.as_ref().unwrap() {
        Modal::ConfirmSend => render_confirm_send(f, app, area),
        Modal::Confirm { title, body, .. } => {
            let rect = centered_rect(62, 11, area);
            f.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.warn))
                .title(Span::styled(
                    format!(" {title} "),
                    Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(theme.surface));
            let p = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(body.clone(), Style::default().fg(theme.text))),
                Line::from(""),
                Line::from(Span::styled(
                    "  Enter confirm · Esc cancel",
                    Style::default().fg(theme.text_muted),
                )),
            ])
            .wrap(Wrap { trim: true })
            .block(block);
            f.render_widget(p, rect);
        }
        Modal::Error { title, body } => {
            let rect = centered_rect(60, 9, area);
            f.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.danger))
                .title(Span::styled(
                    format!(" {title} "),
                    Style::default()
                        .fg(theme.danger)
                        .add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(theme.surface));
            let p = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(body.clone(), Style::default().fg(theme.text))),
                Line::from(""),
                Line::from(Span::styled(
                    "press Enter to dismiss",
                    Style::default().fg(theme.text_muted),
                )),
            ])
            .wrap(Wrap { trim: true })
            .block(block);
            f.render_widget(p, rect);
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
    let from_label = from.map(|w| w.display_name()).unwrap_or_default();
    let from_addr = from.map(|w| w.pubkey.clone()).unwrap_or_default();
    let dest_label = app
        .wallets
        .iter()
        .find(|w| w.pubkey == ps.to)
        .map(|w| format!(" ({})", w.display_name()))
        .unwrap_or_else(|| " (external)".to_string());

    let price = app.price_now();
    let usd = format::fmt_usd(price, ps.lamports);
    let price_note = match price {
        Some(p) => format!("{} {}s", p.source.as_str(), p.age_secs()),
        None => "price unavailable".to_string(),
    };
    let total = ps.lamports.saturating_add(ps.fee);
    let bh_age = ps.prepared_at.elapsed().as_secs();

    let rect = centered_rect(70, 16, area);
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

    let label = |s: &str| {
        Span::styled(
            format!("  {s:<w$}", w = LABEL_TEXT_W),
            Style::default().fg(theme.text_muted),
        )
    };
    let val = |s: String| Span::styled(s, Style::default().fg(theme.text));
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            label("send"),
            Span::styled(
                format!("{} SOL", format::fmt_sol_exact(ps.lamports)),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ≈ {usd}  ({price_note})"),
                Style::default().fg(theme.text_muted),
            ),
        ]),
        Line::from(vec![label("from"), val(from_label)]),
        Line::from(vec![
            Span::styled(format!("{:w$}", "", w = LABEL_W), Style::default()),
            Span::styled(from_addr, Style::default().fg(theme.text)),
        ]),
        Line::from(vec![label("to"), val(dest_label)]),
        Line::from(vec![
            Span::styled(format!("{:w$}", "", w = LABEL_W), Style::default()),
            Span::styled(ps.to.clone(), Style::default().fg(theme.text)),
        ]),
        Line::from(vec![
            label("fee"),
            val(format!("{} SOL", format::fmt_sol_exact(ps.fee))),
            Span::styled(
                format!("   total {} SOL", format::fmt_sol_exact(total)),
                Style::default().fg(theme.text_muted),
            ),
        ]),
        Line::from(vec![
            label("hash"),
            Span::styled(
                format!("blockhash age {bh_age}s (re-fetched if stale)"),
                Style::default().fg(theme.text_muted),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     ", Style::default()),
            Span::styled(
                " Enter ",
                Style::default()
                    .bg(theme.accent)
                    .fg(theme.bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" sign & broadcast      ", Style::default().fg(theme.text)),
            Span::styled(
                " Esc ",
                Style::default().bg(theme.border_idle).fg(theme.text),
            ),
            Span::styled(" cancel", Style::default().fg(theme.text)),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        rect,
    );
}
