//! Compact manual workspace: the graph and the selected instruction stay visible.

use super::{App, Screen, clip_to_columns, manual::Field, node_status_label, status_symbol};
use gloop_core::{EdgeKind, Node, NodeKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}

fn label(node: &Node) -> String {
    clean(node.label.as_deref().unwrap_or(&node.id))
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn number(app: &App, id: &str) -> String {
    app.graph
        .spec
        .nodes
        .iter()
        .position(|node| node.id == id)
        .map_or_else(|| "?".to_owned(), |index| (index + 1).to_string())
}

fn kind(app: &App, node: &Node) -> &'static str {
    match node.kind {
        NodeKind::Agent { .. } | NodeKind::Reduce { .. } | NodeKind::Synthesize { .. } => "AI",
        NodeKind::Command { .. } => ">_",
        NodeKind::Verify { .. } => app.manual_text("Test", "テスト"),
        NodeKind::Gate { .. } => app.manual_text("Pause", "確認待ち"),
        NodeKind::Loop { .. } => "↻",
        NodeKind::Subgraph { .. } => "▦",
    }
}

pub(super) fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let name = if app.create_only {
        app.manual_text("New graph", "新しいグラフ")
    } else {
        &app.graph.metadata.name
    };
    let dirty = if app.dirty { " ●" } else { "" };
    let budget = format!(
        "{} {} · {}s · {} {}",
        app.graph
            .spec
            .budgets
            .model_calls
            .map_or_else(|| "∞".to_owned(), |v| v.to_string()),
        app.manual_text("AI calls max", "AI呼び出しまで"),
        app.graph
            .spec
            .budgets
            .wall_time_seconds
            .map_or_else(|| "∞".to_owned(), |v| v.to_string()),
        app.manual_text("parallel", "並列"),
        app.graph.spec.policies.max_parallel
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    " gloop  ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{}{dirty}", clean(name))),
            ]),
            Line::from(Span::styled(
                format!(" {budget}"),
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        area,
    );
}

fn node_item(app: &App, index: usize, node: &Node, width: usize) -> ListItem<'static> {
    let clip = |text: &str| clip_to_columns(text, width.saturating_sub(3));
    let state = app
        .node_status
        .get(&node.id)
        .copied()
        .filter(|_| app.screen == Screen::Run || app.active_run.is_some());
    let marker = state.map_or("╭─", |state| status_symbol(state).0);
    let title = clip(&format!("{marker} {}  {}", index + 1, label(node)));
    let binding = if super::binds_provider(node) {
        format!(
            "│  {} · {}",
            node.profile().unwrap_or("AI"),
            node.model().unwrap_or(app.manual_text("default", "既定"))
        )
    } else {
        format!("│  {}", kind(app, node))
    };
    let edges = app
        .graph
        .spec
        .edges
        .iter()
        .filter(|edge| edge.from == node.id)
        .map(|edge| {
            let arrow = match edge.kind {
                EdgeKind::Data => "→",
                EdgeKind::Control => "⇢",
                EdgeKind::Failure => "!→",
                EdgeKind::Resource => "◇→",
                EdgeKind::Conditional => "?→",
            };
            format!(
                "{arrow} {}{}",
                number(app, &edge.to),
                if edge.when.is_some() { "?" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("  ");
    let end = if edges.is_empty() {
        "╰─ ■".to_owned()
    } else {
        format!("╰─ {edges}")
    };
    ListItem::new(vec![
        Line::from(Span::styled(
            title,
            Style::default().fg(state.map_or(Color::White, |state| status_symbol(state).1)),
        )),
        Line::from(Span::styled(
            clip(&binding),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(clip(&end), Style::default().fg(Color::Cyan))),
    ])
}

pub(super) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).split(area);
    let toolbar = if app.active_run.is_some() {
        app.manual_text(
            " ● Running   ↑↓ select   o output   q ■ stop",
            " ● 実行中   ↑↓ 選択   o 出力   q ■ 停止",
        )
        .to_owned()
    } else if app.screen == Screen::Run {
        app.manual_text(
            " ↑↓ select   o output   Esc graph   r ▶ run again",
            " ↑↓ 選択   o 出力   Esc グラフ   r ▶ 再実行",
        )
        .to_owned()
    } else if let Some(from) = &app.connect_from {
        format!(
            " {} → ?    {}",
            number(app, from),
            app.manual_text(
                "↑↓ target · Enter connect · Esc cancel",
                "↑↓ 接続先 · Enter 接続 · Esc 取消"
            )
        )
    } else {
        app.manual_text(
            " a + AI   Enter edit   c → connect   A branch   r ▶ run",
            " a + AI   Enter 編集   c → 接続   A 分岐   r ▶ 実行",
        )
        .to_owned()
    };
    frame.render_widget(
        Paragraph::new(toolbar).style(Style::default().fg(Color::Cyan)),
        rows[0],
    );
    if app.graph.spec.nodes.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(""),
                Line::from(Span::styled(
                    "          [ a  + AI ]",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(
                    app.manual_text("          Type your instruction", "          依頼を入力"),
                ),
            ])
            .block(Block::default().borders(Borders::ALL)),
            rows[1],
        );
        return;
    }
    let columns =
        Layout::horizontal([Constraint::Percentage(53), Constraint::Percentage(47)]).split(rows[1]);
    let items = app
        .graph
        .spec
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| node_item(app, index, node, usize::from(columns[0].width)))
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("▸ ").highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        columns[0],
        &mut ListState::default().with_selected(Some(app.selected_node)),
    );
    render_detail(frame, app, columns[1]);
}

fn render_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(node) = app.graph.spec.nodes.get(app.selected_node) else {
        return;
    };
    let incoming = app
        .graph
        .spec
        .edges
        .iter()
        .filter(|edge| edge.to == node.id)
        .map(|edge| number(app, &edge.from))
        .collect::<Vec<_>>()
        .join(" + ");
    let route = if incoming.is_empty() {
        format!("● → {}", app.selected_node + 1)
    } else {
        format!("{incoming} → {}", app.selected_node + 1)
    };
    let binding = if super::binds_provider(node) {
        format!(
            "{}\n{}",
            node.profile().unwrap_or("AI"),
            node.model().unwrap_or(app.manual_text("default", "既定"))
        )
    } else {
        kind(app, node).to_owned()
    };
    let body = if app.screen == Screen::Run {
        run_output(app, node)
    } else {
        app.manual_value(Field::Body).unwrap_or_else(|_| {
            app.manual_text("Tab → node settings", "Tab → ノード設定")
                .to_owned()
        })
    };
    let text = format!("{route}\n{}\n\n{}", clean(&binding), clean(&body));
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default().borders(Borders::LEFT).title(format!(
                " {} ",
                if app.screen == Screen::Run {
                    app.manual_text("o output", "o 出力")
                } else {
                    app.manual_text("Enter edit", "Enter 編集")
                }
            )),
        ),
        area,
    );
}

fn run_output(app: &App, node: &Node) -> String {
    if let Some(outcome) = app
        .last_summary
        .as_ref()
        .and_then(|summary| summary.nodes.get(&node.id))
    {
        let output = outcome.output.as_ref().map_or_else(String::new, |value| {
            value.as_str().map_or_else(
                || serde_json::to_string_pretty(value).unwrap_or_default(),
                str::to_owned,
            )
        });
        return format!(
            "{}\n{}\n{}",
            node_status_label(outcome.status, app.lang),
            output,
            outcome.error.as_deref().unwrap_or_default()
        );
    }
    let status = app
        .node_status
        .get(&node.id)
        .copied()
        .unwrap_or(gloop_core::NodeStatus::Pending);
    let events =
        super::selected_node_recent_events(&app.events, Some(&node.id), 2, app.lang).join("\n");
    format!("{}\n{events}", node_status_label(status, app.lang))
}

pub(super) fn render_picker(
    frame: &mut Frame,
    app: &App,
    title: &str,
    choices: Vec<String>,
    selected: usize,
    area: Rect,
    custom: bool,
) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
    frame.render_stateful_widget(
        List::new(choices.into_iter().map(ListItem::new).collect::<Vec<_>>())
            .highlight_symbol("▸ ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        rows[0],
        &mut ListState::default().with_selected(Some(selected)),
    );
    let hint = if custom {
        app.manual_text(
            "↑↓  Enter  Esc · e custom · Backspace default",
            "↑↓  Enter  Esc · e 入力 · Backspace 既定",
        )
    } else {
        app.manual_text(
            "↑↓  Enter  Esc · Backspace default",
            "↑↓  Enter  Esc · Backspace 既定",
        )
    };
    frame.render_widget(
        Paragraph::new(hint)
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: false }),
        rows[1],
    );
}

pub(super) fn render_profiles(frame: &mut Frame, app: &App, selected: usize, area: Rect) {
    let choices = app
        .profiles
        .iter()
        .map(|profile| {
            if profile.enabled {
                profile.name.clone()
            } else {
                format!("{} ×", profile.name)
            }
        })
        .collect();
    render_picker(frame, app, "AI", choices, selected, area, false);
}
