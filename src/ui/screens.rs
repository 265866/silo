use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, Wrap};

use super::{LABEL_TEXT_W, LABEL_W, format, panel};
use crate::app::{App, SetupStage};
use crate::types::{IntentStatus, Role};

pub(super) fn profile_select(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let h = (app.profiles.len() as u16 + 7).clamp(10, area.height.max(10));
    let rect = super::centered_rect(60, h, area);
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Choose a wallet profile:",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
    ];
    if app.profiles.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none yet — press n to create one)",
            Style::default().fg(theme.text_muted),
        )));
    } else {
        for (i, p) in app.profiles.iter().enumerate() {
            let selected = i == app.profile_sel;
            let marker = if selected { "▌ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            lines.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(theme.accent)),
                Span::styled(format::truncate_end(&p.name, 56), style),
            ]));
        }
    }
    f.render_widget(
        Paragraph::new(lines).block(panel("silo — wallets", true, theme)),
        rect,
    );
}

pub(super) fn unlock(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let title = match app.current_profile_name() {
        Some(name) => format!("Unlock — {}", format::truncate_end(name, 48)),
        None => "Unlock silo".to_string(),
    };
    let block = panel(title, true, theme);
    let masked = format::input_tail(&"•".repeat(app.input.passphrase.chars().count()), 47);
    let error_line = if app.unlock_failed {
        Line::from(Span::styled(
            "  Incorrect passphrase — try again",
            Style::default().fg(theme.danger),
        ))
    } else {
        Line::from("")
    };
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Enter your passphrase to unlock your wallet.",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "passphrase", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled(masked, Style::default().fg(theme.accent)),
            Span::styled("▏", Style::default().fg(theme.accent)),
        ]),
        error_line,
        Line::from(""),
        Line::from(Span::styled(
            "  A forgotten passphrase can't be recovered — only the recovery phrase can.",
            Style::default().fg(theme.text_muted),
        )),
    ];
    let height = lines.len() as u16 + 2;
    let rect = super::centered_rect(78, height, area);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

pub(super) fn setup(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    match app.setup.stage {
        SetupStage::Choose => {
            let block = panel("Welcome to silo", true, theme);
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  A self-custody SOL wallet — your keys stay on this computer.",
                    Style::default().fg(theme.text_muted),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("   c ", Style::default().bg(theme.accent).fg(theme.bg)),
                    Span::styled(
                        "  Create a new wallet (generate a recovery phrase)",
                        Style::default().fg(theme.text),
                    ),
                    Span::styled("  (recommended)", Style::default().fg(theme.text_muted)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("   i ", Style::default().bg(theme.accent).fg(theme.bg)),
                    Span::styled(
                        "  Import an existing recovery phrase",
                        Style::default().fg(theme.text),
                    ),
                ]),
            ];
            let rect = super::setup_panel(area, lines.len() as u16 + 2);
            f.render_widget(Paragraph::new(lines).block(block), rect);
        }
        SetupStage::ShowMnemonic => {
            let inner_w = super::SETUP_WIDTH.saturating_sub(2) as usize;
            let grid_cols = if inner_w >= 59 { 4 } else { 2 };
            let words = &app.setup.mnemonic_words;
            let mut lines = vec![
                Line::from(Span::styled(
                    format!(
                        "  Write these {} words down and keep them safe.",
                        words.len()
                    ),
                    Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "  Anyone with this recovery phrase controls all funds. silo never",
                    Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "  shows it again.",
                    Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "  Write it on paper. Never screenshot or save it to a file.",
                    Style::default().fg(theme.text_muted),
                )),
                Line::from(""),
            ];
            let row_count = words.len().div_ceil(grid_cols);
            for row in 0..row_count {
                let mut spans = vec![Span::raw("   ")];
                for col in 0..grid_cols {
                    let idx = row * grid_cols + col;
                    if let Some(w) = words.get(idx) {
                        spans.push(Span::styled(
                            format!("{:>2}. {:<10}", idx + 1, w),
                            Style::default().fg(theme.accent),
                        ));
                    }
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Enter continues · Esc goes back (a new recovery phrase will be generated).",
                Style::default().fg(theme.text_muted),
            )));
            let rect = super::setup_panel(area, lines.len() as u16 + 2);
            let block = panel("Your recovery phrase", true, theme)
                .border_style(Style::default().fg(theme.warn));
            f.render_widget(Paragraph::new(lines).block(block), rect);
        }
        SetupStage::ConfirmMnemonic => {
            let inner_w = super::SETUP_WIDTH.saturating_sub(2) as usize;
            let block = panel("Confirm recovery phrase", true, theme);
            let words = &app.setup.confirm_words;
            let total = words.len();
            let focus = app.setup.confirm_focus;
            let mismatch = app.setup.confirm_mismatch;
            let filled = words.iter().filter(|w| !w.is_empty()).count();

            let mut lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Re-enter your recovery phrase. Each word auto-completes and jumps",
                    Style::default().fg(theme.text_muted),
                )),
                Line::from(Span::styled(
                    "  to the next box; backspace on an empty box goes back.",
                    Style::default().fg(theme.text_muted),
                )),
                Line::from(""),
            ];

            let cols = if inner_w >= 54 { 4 } else { 2 };
            let rows = total.div_ceil(cols);
            for row in 0..rows {
                let mut spans = vec![Span::raw("  ")];
                for col in 0..cols {
                    let idx = row * cols + col;
                    if idx >= total {
                        break;
                    }
                    let w = &words[idx];
                    let focused = idx == focus;
                    let mismatched = mismatch == Some(idx);
                    spans.push(Span::styled(
                        format!("{:>2} ", idx + 1),
                        Style::default().fg(theme.text_muted),
                    ));
                    let field_style = if mismatched {
                        let base = Style::default()
                            .fg(theme.danger)
                            .add_modifier(Modifier::BOLD);
                        if focused {
                            base.bg(theme.selection_bg)
                        } else {
                            base
                        }
                    } else if focused {
                        Style::default()
                            .fg(theme.accent)
                            .bg(theme.selection_bg)
                            .add_modifier(Modifier::BOLD)
                    } else if w.is_empty() {
                        Style::default().fg(theme.text_muted)
                    } else if crate::crypto::word_is_valid(w) {
                        Style::default().fg(theme.usd)
                    } else {
                        Style::default().fg(theme.danger)
                    };
                    let shown = if focused {
                        format!("{w:<8}▏")
                    } else {
                        format!("{w:<9}")
                    };
                    spans.push(Span::styled(shown, field_style));
                    spans.push(Span::raw(" "));
                }
                lines.push(Line::from(spans));
            }

            lines.push(Line::from(""));
            if let Some(i) = mismatch {
                lines.push(Line::from(Span::styled(
                    format!("  word {} doesn't match", i + 1),
                    Style::default().fg(theme.danger),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("  {filled}/{total} entered"),
                    Style::default().fg(theme.text_muted),
                )));
            }
            lines.push(Line::from(Span::styled(
                "  space/tab next · ←/→ move · enter confirm · esc back",
                Style::default().fg(theme.text_muted),
            )));
            let rect = super::setup_panel(area, lines.len() as u16 + 2);
            f.render_widget(Paragraph::new(lines).block(block), rect);
        }
        SetupStage::ImportEntry => {
            let block = panel("Import recovery phrase", true, theme);
            let count = app.input.import_phrase.split_whitespace().count();
            let target = if count > 12 { 24 } else { 12 };
            let counter_style = if count == 12 || count == 24 {
                Style::default().fg(theme.usd)
            } else {
                Style::default().fg(theme.text_muted)
            };
            let inner_w = super::SETUP_WIDTH.saturating_sub(2) as usize;
            let phrase_lines = format::wrap_lines(app.input.import_phrase.as_str(), inner_w)
                .len()
                .max(1);
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Type or paste your 12/24-word recovery phrase, then Enter.",
                    Style::default().fg(theme.text_muted),
                )),
                Line::from(Span::styled(
                    "  Paste with ^V.",
                    Style::default().fg(theme.text_muted),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        app.input.import_phrase.as_str().to_string(),
                        Style::default().fg(theme.text),
                    ),
                    Span::styled("▏", Style::default().fg(theme.accent)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    format!("  words: {count}/{target}"),
                    counter_style,
                )),
            ];
            let height = lines.len() as u16 + phrase_lines.saturating_sub(1) as u16 + 2;
            let rect = super::setup_panel(area, height);
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
                rect,
            );
        }
        SetupStage::SetPassphrase => {
            let block = panel("Set a passphrase", true, theme);
            let p1 = format::input_tail(&"•".repeat(app.input.passphrase.chars().count()), 45);
            let p2 = format::input_tail(&"•".repeat(app.input.passphrase2.chars().count()), 45);
            let cur = |i: usize| {
                if app.input.focus == i {
                    Span::styled("▏", Style::default().fg(theme.accent))
                } else {
                    Span::raw("")
                }
            };
            let pass = app.input.passphrase.as_str();
            let conf = app.input.passphrase2.as_str();
            let match_line = if pass.is_empty() && conf.is_empty() {
                Line::from(Span::raw(""))
            } else if pass == conf {
                Line::from(Span::styled("  ✓ match", Style::default().fg(theme.usd)))
            } else {
                Line::from(Span::styled(
                    "  ✗ no match",
                    Style::default().fg(theme.danger),
                ))
            };
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Encrypts your recovery phrase on disk.",
                    Style::default().fg(theme.text_muted),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        format!("  {:<w$}", "passphrase", w = LABEL_TEXT_W),
                        Style::default().fg(theme.text_muted),
                    ),
                    Span::styled(p1, Style::default().fg(theme.accent)),
                    cur(0),
                ]),
                Line::from(vec![
                    Span::styled(
                        format!("  {:<w$}", "confirm", w = LABEL_TEXT_W),
                        Style::default().fg(theme.text_muted),
                    ),
                    Span::styled(p2, Style::default().fg(theme.accent)),
                    cur(1),
                ]),
                match_line,
                Line::from(Span::styled(
                    "  8+ characters recommended",
                    Style::default().fg(theme.text_muted),
                )),
                Line::from(Span::styled(
                    "  tab switch · enter create",
                    Style::default().fg(theme.text_muted),
                )),
            ];
            let rect = super::setup_panel(area, lines.len() as u16 + 2);
            f.render_widget(Paragraph::new(lines).block(block), rect);
        }
    }
}

pub(super) fn wallet_list(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let price = app.price_now();

    if app.wallets.is_empty() {
        let p = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No wallets yet — press n to add a subwallet",
                Style::default().fg(theme.text_muted),
            )),
        ])
        .block(panel("Wallets", true, &theme));
        f.render_widget(p, area);
        return;
    }

    let show_addr = area.width >= 62;
    let show_usd = area.width >= 76;

    let mut header_cells = vec![Cell::from("#"), Cell::from("NAME")];
    if show_addr {
        header_cells.push(Cell::from("ADDRESS"));
    }
    header_cells.push(Cell::from("BALANCE"));
    if show_usd {
        header_cells.push(Cell::from(app.currency.label()));
    }
    let header = Row::new(header_cells).style(
        Style::default()
            .fg(theme.text_muted)
            .add_modifier(Modifier::BOLD),
    );

    let archived_count = app.wallets.iter().filter(|w| w.archived).count();
    let rows: Vec<Row> = app
        .wallet_list_rows()
        .into_iter()
        .map(|r| match r {
            crate::app::WalletListRow::ArchivedHeader => {
                let caret = if app.archived_expanded { "▾" } else { "▸" };
                let mut cells = vec![
                    Cell::from(""),
                    Cell::from(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            format!("{caret} archived ({archived_count})"),
                            Style::default()
                                .fg(theme.text_muted)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])),
                ];
                if show_addr {
                    cells.push(Cell::from(""));
                }
                cells.push(Cell::from(""));
                if show_usd {
                    cells.push(Cell::from(""));
                }
                Row::new(cells)
            }
            crate::app::WalletListRow::Wallet(i) => {
                let w = &app.wallets[i];
                let is_master = w.role == Role::Master;
                let archived = w.archived;
                let name_color = if archived {
                    theme.text_muted
                } else if is_master {
                    theme.master
                } else {
                    theme.text
                };
                let star_span = if is_master {
                    let phase = w.account_index as f32 * 1.7;
                    let tw = ((app.anim_frame() as f32 * 0.20 + phase).sin() * 0.5 + 0.5).powf(1.5);
                    Span::styled(
                        "★ ",
                        Style::default().fg(super::blend(
                            theme.master,
                            ratatui::style::Color::Rgb(255, 255, 255),
                            tw,
                        )),
                    )
                } else {
                    Span::raw("  ")
                };
                let pending = if w.has_open_intent { " ⏳" } else { "" };
                let name_text = w.display_name();
                let bal = match app.shown_balance(w) {
                    Some(l) => format!("{} SOL", format::fmt_sol(l)),
                    None => "…".to_string(),
                };
                let usd = match app.shown_balance(w) {
                    Some(l) => format::fmt_usd(price, l),
                    None => "…".to_string(),
                };
                let bal_color = if archived {
                    theme.text_muted
                } else {
                    theme.text
                };
                let usd_color = if archived {
                    theme.text_muted
                } else {
                    theme.usd
                };
                let mut cells = vec![
                    Cell::from(Span::styled(
                        w.account_index.to_string(),
                        Style::default().fg(theme.text_muted),
                    )),
                    Cell::from(Line::from(vec![
                        star_span,
                        Span::styled(name_text, Style::default().fg(name_color)),
                        Span::styled(pending, Style::default().fg(theme.warn)),
                    ])),
                ];
                if show_addr {
                    cells.push(Cell::from(Span::styled(
                        format::elide_addr(&w.pubkey),
                        Style::default().fg(theme.text_muted),
                    )));
                }
                cells.push(Cell::from(Span::styled(
                    bal,
                    Style::default().fg(bal_color).add_modifier(Modifier::BOLD),
                )));
                if show_usd {
                    cells.push(Cell::from(Span::styled(
                        usd,
                        Style::default().fg(usd_color),
                    )));
                }
                Row::new(cells)
            }
        })
        .collect();

    let master_count = app
        .wallets
        .iter()
        .filter(|w| w.role == Role::Master)
        .count();
    let sub_count = app
        .wallets
        .iter()
        .filter(|w| w.role == Role::Sub && !w.archived)
        .count();
    let title = format!("Wallets ({master_count} master · {sub_count} subwallet)");

    let mut widths = vec![Constraint::Length(4), Constraint::Min(18)];
    if show_addr {
        widths.push(Constraint::Length(14));
    }
    widths.push(Constraint::Length(18));
    if show_usd {
        widths.push(Constraint::Length(14));
    }
    const MARKERS: [&str; 6] = ["▏ ", "▎ ", "▍ ", "▌ ", "▍ ", "▎ "];
    let marker = MARKERS[(app.anim_frame() as usize / 2) % MARKERS.len()];
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(
            Style::default()
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(marker)
        .block(panel(title, true, &theme));

    f.render_stateful_widget(table, area, &mut app.list_state);
}

pub(super) fn wallet_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = &app.theme;
    let Some(w) = app.focused_wallet() else {
        return;
    };
    let price = app.price_now();

    let is_master = w.role == Role::Master;
    let bal = app.shown_balance(w);
    const BASE_HEADER: u16 = 9;
    const MAX_NOTE_LINES: usize = 6;
    const MIN_TABLE_H: u16 = 7;
    let field_w = (area.width.saturating_sub(2 + super::LABEL_W as u16) as usize).max(1);
    let note_raw = w.note.clone().unwrap_or_else(|| "—".into());
    let note_lines = format::wrap_lines(&note_raw, field_w);
    let want = note_lines.len().clamp(1, MAX_NOTE_LINES);
    let room = area.height.saturating_sub(BASE_HEADER + MIN_TABLE_H);
    let extra = ((want - 1) as u16).min(room);
    let header_h = BASE_HEADER + extra;
    let shown = 1 + extra as usize;
    let chunks = Layout::vertical([Constraint::Length(header_h), Constraint::Min(0)]).split(area);

    let truncated = note_lines.len() > shown;
    let mut display: Vec<String> = note_lines.into_iter().take(shown).collect();
    if truncated && let Some(last) = display.last_mut() {
        let mut chars: Vec<char> = last.chars().collect();
        while chars.len() + 1 > field_w && !chars.is_empty() {
            chars.pop();
        }
        chars.push('…');
        *last = chars.into_iter().collect();
    }

    let addr_str = format::elide_middle(&w.pubkey, field_w);
    let mut info = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "address", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled(addr_str, Style::default().fg(theme.text)),
        ]),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "balance", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled(
                bal.map(|l| format!("{} SOL", format::fmt_sol(l)))
                    .unwrap_or_else(|| "loading…".into()),
                Style::default()
                    .fg(if bal.is_some() {
                        theme.accent
                    } else {
                        theme.text_muted
                    })
                    .add_modifier(if bal.is_some() {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(
                bal.map(|l| format!("   ≈ {}", format::fmt_usd(price, l)))
                    .unwrap_or_default(),
                Style::default().fg(theme.usd),
            ),
        ]),
        {
            let mut spans = vec![Span::styled(
                format!("  {:<w$}", "type", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            )];
            if is_master {
                spans.push(Span::styled(
                    "★ master",
                    Style::default()
                        .fg(theme.master)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    "  funds subwallets · cannot be archived",
                    Style::default().fg(theme.text_muted),
                ));
            } else {
                spans.push(Span::styled("subwallet", Style::default().fg(theme.text)));
                spans.push(Span::styled(
                    format!("   index {}", w.account_index),
                    Style::default().fg(theme.text_muted),
                ));
            }
            Line::from(spans)
        },
    ];
    for (idx, line) in display.into_iter().enumerate() {
        let label = if idx == 0 {
            format!("  {:<w$}", "note", w = LABEL_TEXT_W)
        } else {
            format!("{:<w$}", "", w = LABEL_W)
        };
        info.push(Line::from(vec![
            Span::styled(label, Style::default().fg(theme.text_muted)),
            Span::styled(line, Style::default().fg(theme.text)),
        ]));
    }
    f.render_widget(
        Paragraph::new(info).block(panel(w.display_name(), true, theme)),
        chunks[0],
    );

    render_intent_table(f, app, chunks[1], "Recent transfers");
}

pub(super) fn send(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let width = 72u16;
    let inner_w = width.min(area.width).saturating_sub(2) as usize;
    let field_w = inner_w.saturating_sub(LABEL_W + 1).max(1);
    let note_w = inner_w.saturating_sub(LABEL_W).max(1);
    let from = app.focused_wallet();
    let from_name = from.map(|w| w.display_name()).unwrap_or_default();
    let from_id = from.map(|w| (w.account_index, w.id)).unwrap_or((0, 0));
    let avail = from.and_then(|w| w.balance_lamports);

    let to_ok = crate::clipboard::validate_solana_pubkey(&app.input.send_to).is_ok();
    let dest = app
        .wallets
        .iter()
        .find(|w| w.pubkey == app.input.send_to.trim());
    let route_note = match (from, to_ok) {
        (Some(fw), true) => {
            match crate::input::classify_route(&app.wallets, fw, &app.input.send_to) {
                Ok(()) => match dest {
                    Some(d) => (format!("✓ valid ({})", d.display_name()), theme.usd),
                    None => ("✓ valid (external)".to_string(), theme.usd),
                },
                Err(e) => (format!("⚠ {e}"), theme.danger),
            }
        }
        (_, false) if !app.input.send_to.is_empty() => {
            ("⚠ not a valid address".to_string(), theme.danger)
        }
        _ => (String::new(), theme.text_muted),
    };

    let cur = |i: usize| {
        if app.input.focus == i {
            Span::styled("▏", Style::default().fg(theme.accent))
        } else {
            Span::raw("")
        }
    };

    let lamports = app.compose_lamports().ok();
    let fiat = app.input.send_in_fiat;
    let denom_label = if fiat { app.currency.label() } else { "SOL" };
    let equiv = match (fiat, lamports) {
        (false, Some(l)) => {
            let u = format::fmt_usd(app.price_now(), l);
            if u == "—" {
                String::new()
            } else {
                format!("   ≈ {u}")
            }
        }
        (true, Some(l)) => format!("   ≈ {} SOL", format::fmt_sol_exact(l)),
        (true, None) if app.price_now().is_none() => "   (no price)".to_string(),
        _ => String::new(),
    };

    let min = app.rent_exempt_min;
    let fee = app.send_fee();
    let floor_note = match lamports {
        Some(amt) if to_ok && amt > 0 => {
            let recipient_under = dest
                .and_then(|d| d.balance_lamports)
                .unwrap_or(0)
                .saturating_add(amt)
                < min;
            let new_recipient = dest.is_none() || dest.and_then(|d| d.balance_lamports) == Some(0);
            let source_under = avail
                .map(|a| {
                    let after = a.saturating_sub(amt.saturating_add(fee));
                    after > 0 && after < min
                })
                .unwrap_or(false);
            if source_under {
                Some(format!(
                    "⚠ would leave this wallet below the minimum balance (keep ≥ {} SOL)",
                    format::fmt_sol_exact(min)
                ))
            } else if recipient_under && new_recipient {
                Some(format!(
                    "⚠ first deposit to a new address must be at least {} SOL",
                    format::fmt_sol_exact(min)
                ))
            } else {
                None
            }
        }
        _ => None,
    };

    let (active_unit, other_unit) = if fiat {
        (denom_label, "sol")
    } else {
        ("SOL", "usd")
    };
    let switch_hint = if app.input.focus == 1 {
        format!(" {other_unit} · c to switch")
    } else {
        format!(" {other_unit}")
    };
    let switch = Line::from(vec![
        Span::styled(format!("{:>w$}", "", w = LABEL_W), Style::default()),
        Span::styled(
            format!("[{active_unit}]"),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(switch_hint, Style::default().fg(theme.text_muted)),
    ]);

    let after = match (avail, lamports) {
        (Some(a), Some(amt)) => {
            let spent = amt.saturating_add(fee);
            Some(format!(
                "after send ≈ {} SOL (fee subtracted)",
                format::fmt_sol(a.saturating_sub(spent))
            ))
        }
        _ => None,
    };

    let avail_fee = match avail {
        Some(a) => format!(
            "available {} SOL · fee ≈ {} SOL",
            format::fmt_sol(a),
            format::fmt_sol(fee)
        ),
        None => format!("available … · fee ≈ {} SOL", format::fmt_sol(fee)),
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "to", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled(
                format::input_tail(&app.input.send_to, field_w),
                Style::default().fg(theme.text),
            ),
            cur(0),
        ]),
        Line::from(Span::styled(
            format!(
                "{:>w$}{}",
                "",
                format::truncate_end(&route_note.0, note_w),
                w = LABEL_W
            ),
            Style::default().fg(route_note.1),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "amount", w = LABEL_TEXT_W),
                Style::default().fg(theme.accent),
            ),
            Span::styled(
                app.input.send_amount.clone(),
                Style::default().fg(theme.text),
            ),
            cur(1),
            Span::styled(equiv, Style::default().fg(theme.usd)),
        ]),
        switch,
    ];
    if let Some(note) = floor_note {
        lines.push(Line::from(Span::styled(
            format!(
                "{:>w$}{}",
                "",
                format::truncate_end(&note, note_w),
                w = LABEL_W
            ),
            Style::default().fg(theme.danger),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("{:>w$}{avail_fee}", "", w = LABEL_W),
        Style::default().fg(theme.text_muted),
    )));
    if let Some(after) = after {
        lines.push(Line::from(Span::styled(
            format!("{:>w$}{after}", "", w = LABEL_W),
            Style::default().fg(theme.text_muted),
        )));
    }

    let want = (lines.len() as u16).saturating_add(2);
    let height = want.min(area.height).max(3);
    let rect = super::centered_rect(width, height, area);
    let title = format!("Send SOL — from {} (#{})", from_name, from_id.0);
    f.render_widget(Paragraph::new(lines).block(panel(title, true, theme)), rect);
}

pub(super) fn history(f: &mut Frame, app: &mut App, area: Rect) {
    let name = app
        .focused_wallet()
        .map(|w| w.display_name())
        .unwrap_or_default();
    render_intent_table(f, app, area, &format!("History — {name}"));
}

fn render_intent_table(f: &mut Frame, app: &mut App, area: Rect, title: &str) {
    let theme = &app.theme;
    if app.detail_intents.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "  no transfers yet",
            Style::default().fg(theme.text_muted),
        )))
        .block(panel(title.to_string(), false, theme));
        f.render_widget(p, area);
        return;
    }
    let header = Row::new(vec![
        Cell::from("WHEN"),
        Cell::from("TO"),
        Cell::from("AMOUNT"),
        Cell::from("STATUS"),
        Cell::from("TX"),
    ])
    .style(
        Style::default()
            .fg(theme.text_muted)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = app
        .detail_intents
        .iter()
        .map(|i| {
            let (status_txt, color) = status_style(i.status, theme);
            let txid = match i.signature.as_deref() {
                Some(sig) => format::elide_addr(sig),
                None => "—".to_string(),
            };
            Row::new(vec![
                Cell::from(Span::styled(
                    format::fmt_relative_time(i.created_at),
                    Style::default().fg(theme.text_muted),
                )),
                Cell::from(Span::styled(
                    format::elide_addr(&i.to_address),
                    Style::default().fg(theme.text),
                )),
                Cell::from(Span::styled(
                    format!("{} SOL", format::fmt_sol(i.lamports)),
                    Style::default().fg(theme.text),
                )),
                Cell::from(Span::styled(status_txt, Style::default().fg(color))),
                Cell::from(Span::styled(txid, Style::default().fg(theme.text_muted))),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(12),
        Constraint::Length(14),
        Constraint::Length(18),
        Constraint::Length(13),
        Constraint::Min(11),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(theme.selection_bg))
        .highlight_symbol("▸ ")
        .block(panel(title.to_string(), false, theme));
    f.render_stateful_widget(table, area, &mut app.history_state);
}

fn status_style(s: IntentStatus, theme: &super::theme::Theme) -> (String, ratatui::style::Color) {
    match s {
        IntentStatus::Confirmed => ("confirmed ✓".into(), theme.usd),
        IntentStatus::Failed => ("failed".into(), theme.danger),
        IntentStatus::Expired => ("expired".into(), theme.warn),
        IntentStatus::Submitted => ("submitted ⏳".into(), theme.warn),
        IntentStatus::Signed => ("signed ⏳".into(), theme.warn),
        IntentStatus::Created => ("pending ⏳".into(), theme.warn),
    }
}

pub(super) fn audit_log(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = &app.theme;

    if app.audit.is_empty() {
        let lines = vec![
            Line::from(Span::styled(
                "  Tamper-evident record of every action",
                Style::default().fg(theme.text_muted),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  no events yet",
                Style::default().fg(theme.text_muted),
            )),
        ];
        let p = Paragraph::new(lines).block(panel("Audit log", true, theme));
        f.render_widget(p, area);
        return;
    }

    let blk = panel("Audit log", true, theme);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  Tamper-evident record of every action",
            Style::default().fg(theme.text_muted),
        ))),
        chunks[0],
    );

    let header = Row::new(vec![
        Cell::from("WHEN"),
        Cell::from("EVENT"),
        Cell::from("DETAIL"),
    ])
    .style(
        Style::default()
            .fg(theme.text_muted)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = app
        .audit
        .iter()
        .map(|e| {
            let detail = compact_json(&e.details);
            Row::new(vec![
                Cell::from(Span::styled(
                    format::fmt_relative_time(e.ts),
                    Style::default().fg(theme.text_muted),
                )),
                Cell::from(Span::styled(
                    e.event_type.clone(),
                    Style::default().fg(theme.text),
                )),
                Cell::from(Span::styled(detail, Style::default().fg(theme.text_muted))),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(12),
        Constraint::Length(22),
        Constraint::Min(10),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(theme.selection_bg))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(table, chunks[1], &mut app.audit_state);
}

fn compact_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) if m.is_empty() => String::new(),
        _ => {
            let s = v.to_string();
            if s.chars().count() > 60 {
                let truncated: String = s.chars().take(59).collect();
                format!("{truncated}…")
            } else {
                s
            }
        }
    }
}

pub(super) fn settings(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let rect = super::centered_rect(72, 13, area);
    let lock_min = app.auto_lock_after.as_secs() / 60;
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "network", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled("mainnet-beta", Style::default().fg(theme.text)),
        ]),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "rpc", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled(
                format::elide_middle(&crate::solana::rpc::redact_rpc_url(&app.rpc_url), 56),
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "commitment", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled("confirmed", Style::default().fg(theme.text)),
        ]),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "currency", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled(
                format!(
                    "{} ({})",
                    app.currency.label(),
                    app.currency.symbol().trim()
                ),
                Style::default().fg(theme.accent),
            ),
            Span::styled("   (u to cycle)", Style::default().fg(theme.text_muted)),
        ]),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "priority", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled(
                format!(
                    "{} (≈ {} SOL)",
                    crate::money::priority_label(app.priority_micro),
                    format::fmt_sol_exact(crate::money::priority_fee_lamports(app.priority_micro))
                ),
                Style::default().fg(theme.text),
            ),
            Span::styled("   (p to cycle)", Style::default().fg(theme.text_muted)),
        ]),
        Line::from(vec![
            Span::styled(
                format!("  {:<w$}", "auto-lock", w = LABEL_TEXT_W),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled(format!("{lock_min} min"), Style::default().fg(theme.text)),
            Span::styled("   (+/- to adjust)", Style::default().fg(theme.text_muted)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  e edit RPC · u currency · p priority · L lock now · esc back",
            Style::default().fg(theme.text_muted),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(panel("Settings", true, theme)),
        rect,
    );
}
