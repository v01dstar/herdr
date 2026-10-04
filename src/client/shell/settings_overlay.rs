use super::*;

/// Height of the settings popup on every tab (the Integrations tab grows with its list).
pub(in crate::client::shell) const SETTINGS_HEIGHT: u16 = 24;

fn choice_style(selected: bool, palette: &Palette) -> Style {
    if selected {
        Style::default()
            .fg(contrast(palette))
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text).bg(palette.panel_bg)
    }
}

fn draw_choice(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    selected: bool,
    current: bool,
    palette: &Palette,
) {
    let style = choice_style(selected, palette);
    buffer.set_style(rect, style);
    let marker = if selected { "▸" } else { " " };
    let current = if current { " ✓" } else { "" };
    put_text(
        buffer,
        rect.x,
        rect.y,
        rect.width,
        &format!(" {marker} {label}{current}"),
        style,
    );
}

pub(in crate::client::shell) fn render_settings_overlay(
    buffer: &mut Buffer,
    settings: &ClientSettingsOverlay,
    integration_updates_available: bool,
    palette: &Palette,
) -> Option<OverlayRender> {
    let integration_height = 14u16
        .saturating_add(settings.integrations.len().max(1) as u16)
        .saturating_add(settings.integration_messages.len().min(6) as u16);
    let height = if settings.section == ClientSettingsSection::Integrations {
        integration_height.max(SETTINGS_HEIGHT)
    } else {
        SETTINGS_HEIGHT
    };
    let popup = popup(buffer.area, 76, height)?;
    let inner = panel(buffer, popup, palette.accent, palette.panel_bg)?;
    if inner.width < 20 || inner.height < 8 {
        return None;
    }

    put_text(
        buffer,
        inner.x,
        inner.y,
        inner.width,
        " settings",
        Style::default()
            .fg(palette.text)
            .bg(palette.panel_bg)
            .add_modifier(Modifier::BOLD),
    );

    let integration_badge = integration_updates_available
        || settings
            .integrations
            .iter()
            .any(|integration| integration.state == crate::api::schema::IntegrationState::Outdated);
    let tab_hits =
        render_settings_tabs(buffer, inner, settings.section, integration_badge, palette);

    let content = Rect::new(
        inner.x,
        inner.y + 4,
        inner.width,
        inner.height.saturating_sub(7),
    );
    let mut choice_hits = Vec::new();
    match settings.section {
        // These tabs are the remotes view; selecting one opens it instead.
        ClientSettingsSection::Remotes
        | ClientSettingsSection::Images
        | ClientSettingsSection::Account => {}
        ClientSettingsSection::Theme => {
            let visible = usize::from(content.height);
            let scroll = settings.selected.saturating_sub(visible.saturating_sub(1));
            for (visible_index, (index, name)) in crate::config::THEME_NAMES
                .iter()
                .enumerate()
                .skip(scroll)
                .take(visible)
                .enumerate()
            {
                let rect = Rect::new(
                    content.x,
                    content.y + visible_index as u16,
                    content.width,
                    1,
                );
                draw_choice(
                    buffer,
                    rect,
                    name,
                    index == settings.selected,
                    super::super::settings::normalized_theme_name(name)
                        == super::super::settings::normalized_theme_name(
                            &settings.original_theme_name,
                        ),
                    palette,
                );
                choice_hits.push((rect, index));
            }
        }
        ClientSettingsSection::Indicators => {
            render_choice_section(
                buffer,
                content,
                "agent status indicators",
                "choose color dots or distinct symbols for each state",
                &["color dots  ● ● ● ○ ·", "distinct symbols  × ◐ ✓ ○ ·"],
                settings.selected,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Sound => {
            render_choice_section(
                buffer,
                content,
                "sound alerts",
                "play sounds when agents change state in background",
                &["on", "off"],
                settings.selected,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Toast => {
            render_choice_section(
                buffer,
                content,
                "notification popups",
                "choose where background popup notifications should appear",
                &["off", "inside herdr", "via terminal", "via system"],
                settings.selected,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Integrations => {
            render_integrations(buffer, content, settings, palette);
        }
    }

    let installable = settings
        .integrations
        .iter()
        .any(super::super::settings::integration_needs_install);
    let show_primary = settings.section != ClientSettingsSection::Integrations || installable;
    let labels = if show_primary { vec![10, 12] } else { vec![12] };
    let buttons = row(inner, &labels, 2, inner.height.saturating_sub(1));
    let (primary, close) = if show_primary {
        let primary = buttons[0];
        button(
            buffer,
            primary,
            if settings.section == ClientSettingsSection::Integrations {
                " ↵ install "
            } else {
                " ↵ apply "
            },
            Style::default()
                .fg(contrast(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD),
        );
        (primary, buttons[1])
    } else {
        (Rect::default(), buttons[0])
    };
    button(
        buffer,
        close,
        " esc close ",
        Style::default()
            .fg(palette.text)
            .bg(palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        buffer,
        inner.x,
        inner.bottom().saturating_sub(2),
        inner.width,
        " ↑↓ select  tab section",
        Style::default().fg(palette.overlay1).bg(palette.panel_bg),
    );

    Some(OverlayRender {
        area: popup,
        primary,
        cancel: close,
        settings_popup: popup,
        settings_tabs: tab_hits,
        settings_choices: choice_hits,
        ..OverlayRender::default()
    })
}

fn render_choice_section(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    description: &str,
    choices: &[&str],
    selected: usize,
    palette: &Palette,
    hits: &mut Vec<(Rect, usize)>,
) {
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        title,
        Style::default()
            .fg(palette.text)
            .bg(palette.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        description,
        Style::default().fg(palette.overlay1).bg(palette.panel_bg),
    );
    let row_gap = u16::from(choices.len() > 2);
    for (index, choice) in choices.iter().enumerate() {
        let y = area.y + 3 + index as u16 * (1 + row_gap);
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(area.x, y, area.width, 1);
        draw_choice(buffer, rect, choice, index == selected, false, palette);
        hits.push((rect, index));
    }
}

fn render_integrations(
    buffer: &mut Buffer,
    area: Rect,
    settings: &ClientSettingsOverlay,
    palette: &Palette,
) {
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        "agent integrations",
        Style::default()
            .fg(palette.text)
            .bg(palette.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        "enable session restore and, where supported, direct status updates",
        Style::default().fg(palette.overlay1).bg(palette.panel_bg),
    );
    if settings.loading_integrations {
        put_text(
            buffer,
            area.x,
            area.y + 3,
            area.width,
            " loading integrations…",
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
        return;
    }
    if settings.integrations.is_empty() {
        put_text(
            buffer,
            area.x,
            area.y + 3,
            area.width,
            " no integration targets available",
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
        return;
    }
    for (index, integration) in settings.integrations.iter().enumerate() {
        let y = area.y + 3 + index as u16;
        if y >= area.bottom() {
            break;
        }
        let (marker, color, status) = match integration.state {
            crate::api::schema::IntegrationState::Current => ("✓", palette.green, "installed"),
            crate::api::schema::IntegrationState::Outdated => {
                ("↻", palette.yellow, "update available")
            }
            crate::api::schema::IntegrationState::NotInstalled if integration.available => {
                ("+", palette.accent, "available")
            }
            crate::api::schema::IntegrationState::NotInstalled => {
                ("–", palette.overlay0, "not found")
            }
        };
        put_text(
            buffer,
            area.x,
            y,
            3,
            &format!(" {marker}"),
            Style::default().fg(color).bg(palette.panel_bg),
        );
        put_text(
            buffer,
            area.x + 3,
            y,
            11.min(area.width.saturating_sub(3)),
            &format!("{:<9}", integration.label),
            Style::default().fg(palette.subtext0).bg(palette.panel_bg),
        );
        put_text(
            buffer,
            area.x + 14,
            y,
            area.width.saturating_sub(14),
            status,
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }
    let message_y = area
        .y
        .saturating_add(4)
        .saturating_add(settings.integrations.len() as u16);
    for (offset, message) in settings.integration_messages.iter().take(6).enumerate() {
        let y = message_y.saturating_add(offset as u16);
        if y >= area.bottom() {
            break;
        }
        put_text(
            buffer,
            area.x,
            y,
            area.width,
            &format!(" {message}"),
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }
    if settings.installing_integrations && message_y < area.bottom() {
        put_text(
            buffer,
            area.x,
            message_y,
            area.width,
            " installing…",
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }
}

/// One visible tab of the strip: its section, where it starts (relative to the strip)
/// and the text drawn there (highlighted when active).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client::shell) struct TabSlot {
    pub section: ClientSettingsSection,
    pub x: u16,
    pub text: String,
}

/// The settings tab strip laid out in `width` columns. Tabs are padded (` theme `) when
/// all fit, else packed two then one space apart, and otherwise scrolled so the active
/// tab is always shown, with `‹`/`›` where tabs are hidden. Pure.
pub(in crate::client::shell) fn layout_settings_tabs(
    width: u16,
    active: ClientSettingsSection,
    integration_badge: bool,
) -> (Vec<TabSlot>, bool, bool) {
    let all = ClientSettingsSection::ALL;
    let texts: Vec<String> = all
        .iter()
        .map(|section| {
            if *section == ClientSettingsSection::Integrations && integration_badge {
                format!("● {}", section.label())
            } else {
                section.label().to_owned()
            }
        })
        .collect();
    let widths: Vec<u16> = texts.iter().map(|text| display_width(text)).collect();
    // Tabs `range` from `start`: padded (one space each side, so two between labels)
    // or bare and `gap` apart.
    let place = |range: std::ops::Range<usize>, start: u16, pad: bool, gap: u16| {
        let mut x = start;
        range
            .map(|index| {
                let text = if pad {
                    format!(" {} ", texts[index])
                } else {
                    texts[index].clone()
                };
                let slot = TabSlot {
                    section: all[index],
                    x,
                    text,
                };
                x = x.saturating_add(widths[index] + if pad { 2 } else { gap });
                slot
            })
            .collect::<Vec<_>>()
    };
    let packed = |range: std::ops::Range<usize>, gap: u16| -> u16 {
        let count = range.len() as u16;
        widths[range].iter().sum::<u16>() + count.saturating_sub(1) * gap
    };
    if packed(0..all.len(), 2) + 2 <= width {
        return (place(0..all.len(), 0, true, 0), false, false);
    }
    for gap in [2, 1] {
        if packed(0..all.len(), gap) < width {
            return (place(0..all.len(), 1, false, gap), false, false);
        }
    }
    let packed = |range: std::ops::Range<usize>| packed(range, 1);
    // Scroll: grow a window around the active tab, room kept for both markers.
    let room = width.saturating_sub(4);
    let active = all
        .iter()
        .position(|section| *section == active)
        .unwrap_or(0);
    let (mut start, mut end) = (active, active + 1);
    loop {
        let mut grew = false;
        if end < all.len() && packed(start..end + 1) <= room {
            end += 1;
            grew = true;
        }
        if start > 0 && packed(start - 1..end) <= room {
            start -= 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    (place(start..end, 2, false, 1), start > 0, end < all.len())
}

/// Draws the settings tab strip on the second row of `inner` and the rule below it.
/// Shared with the remotes view, which draws the remotes, images and account tabs.
pub(in crate::client::shell) fn render_settings_tabs(
    buffer: &mut Buffer,
    inner: Rect,
    active_section: ClientSettingsSection,
    integration_badge: bool,
    palette: &Palette,
) -> Vec<(Rect, ClientSettingsSection)> {
    let (slots, more_left, more_right) =
        layout_settings_tabs(inner.width, active_section, integration_badge);
    let y = inner.y + 1;
    let muted = Style::default().fg(palette.overlay1).bg(palette.panel_bg);
    if more_left {
        put_text(buffer, inner.x, y, 1, "‹", muted);
    }
    if more_right {
        put_text(buffer, inner.right().saturating_sub(1), y, 1, "›", muted);
    }
    let right = inner.right().saturating_sub(if more_right { 2 } else { 0 });
    let mut tab_hits = Vec::new();
    for slot in slots {
        let x = inner.x.saturating_add(slot.x);
        let width = display_width(&slot.text).min(right.saturating_sub(x));
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, y, width, 1);
        let active = slot.section == active_section;
        let style = if active {
            Style::default()
                .fg(contrast(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            muted
        };
        buffer.set_style(rect, style);
        put_text(buffer, rect.x, rect.y, rect.width, &slot.text, style);
        if !active {
            if let Some(offset) = slot.text.find('●') {
                put_text(
                    buffer,
                    rect.x.saturating_add(offset as u16),
                    rect.y,
                    1,
                    "●",
                    Style::default()
                        .fg(palette.accent)
                        .bg(palette.panel_bg)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }
        tab_hits.push((rect, slot.section));
    }
    put_text(
        buffer,
        inner.x,
        inner.y + 2,
        inner.width,
        &"─".repeat(inner.width as usize),
        Style::default().fg(palette.surface0).bg(palette.panel_bg),
    );
    tab_hits
}
