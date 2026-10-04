//! Settings → Remotes: the settings frame and tabs, a sub-view switcher, then two panes
//! (a list and the selected item's details and actions). Pure: reads the dialog only.
use super::*;
use crate::client::shell::locations::account::{badge, SHARED_NOTE};
use crate::client::shell::locations::view::{
    bytes, date, ActionGroup, Focus, ImageAction, ListRow, RemotesHit, RemotesTab, Tone,
};
use crate::client::shell::locations::LocationDialog;
use ratatui::style::Color;

struct Styles {
    normal: Style,
    dim: Style,
    faint: Style,
    bold: Style,
    highlight: Style,
    inactive: Style,
}

impl Styles {
    fn new(palette: &Palette) -> Self {
        let normal = Style::default().fg(palette.text).bg(palette.panel_bg);
        Self {
            normal,
            dim: normal.fg(palette.subtext0),
            faint: normal.fg(palette.overlay0),
            bold: normal.add_modifier(Modifier::BOLD),
            highlight: Style::default()
                .fg(contrast(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD),
            inactive: normal.bg(palette.surface0),
        }
    }
}

fn tone(palette: &Palette, tone: Tone) -> Color {
    match tone {
        Tone::Good => palette.green,
        Tone::Warn => palette.yellow,
        Tone::Bad => palette.red,
        Tone::Muted => palette.overlay1,
    }
}

/// Greedy word wrap by display width.
fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if UnicodeWidthStr::width(candidate.as_str()) <= width || line.is_empty() {
                line = candidate;
            } else {
                lines.push(std::mem::replace(&mut line, word.to_owned()));
            }
        }
        lines.push(line);
    }
    lines
}

/// Puts `text` from `x` up to `right`; returns where it ended.
fn put_until(buffer: &mut Buffer, x: u16, y: u16, right: u16, text: &str, style: Style) -> u16 {
    let width = display_width(text).min(right.saturating_sub(x));
    put_text(buffer, x, y, width, text, style);
    x.saturating_add(width)
}

/// A row with `left` and, right-aligned, `right` (which wins when space is short).
fn put_split(
    buffer: &mut Buffer,
    rect: Rect,
    left: &str,
    left_style: Style,
    right: &str,
    right_style: Style,
) {
    let right_width = display_width(right).min(rect.width);
    let left_width = rect
        .width
        .saturating_sub(right_width + u16::from(right_width > 0));
    put_text(buffer, rect.x, rect.y, left_width, left, left_style);
    put_text(
        buffer,
        rect.right().saturating_sub(right_width),
        rect.y,
        right_width,
        right,
        right_style,
    );
}

pub(in crate::client::shell) fn render_remotes(
    buffer: &mut Buffer,
    dialog: &LocationDialog,
    palette: &Palette,
) -> Option<OverlayRender> {
    let area = popup(buffer.area, 76, super::settings_overlay::SETTINGS_HEIGHT)?;
    let inner = panel(buffer, area, palette.accent, palette.panel_bg)?;
    if inner.width < 24 || inner.height < 8 {
        return None;
    }
    let styles = Styles::new(palette);
    put_text(
        buffer,
        inner.x,
        inner.y,
        inner.width,
        " settings",
        styles.bold,
    );
    let settings_tabs = super::settings_overlay::render_settings_tabs(
        buffer,
        inner,
        ClientSettingsSection::Remotes,
        false,
        palette,
    );
    let mut hits = Vec::new();

    // The sub-view switcher, and who is signed in at its right.
    let switcher_y = inner.y + 3;
    let login = badge(dialog.account.as_ref());
    let login_width = display_width(&login).min(inner.width / 3);
    put_text(
        buffer,
        inner.right().saturating_sub(login_width + 1),
        switcher_y,
        login_width,
        &login,
        styles.dim,
    );
    let mut x = inner.x + 1;
    for (index, tab) in RemotesTab::ALL.iter().enumerate() {
        if index > 0 {
            x = put_until(buffer, x, switcher_y, inner.right(), " · ", styles.faint);
        }
        let style = if *tab == dialog.view.tab {
            styles
                .bold
                .fg(palette.accent)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            styles.normal.fg(palette.overlay1)
        };
        let start = x;
        x = put_until(buffer, x, switcher_y, inner.right(), tab.label(), style);
        hits.push((
            Rect::new(start, switcher_y, x - start, 1),
            RemotesHit::Tab(*tab),
        ));
    }

    // Bottom: the close button, a key hint and a message strip.
    let bottom = inner.bottom();
    let buttons = row(inner, &[12], 2, inner.height.saturating_sub(1));
    let cancel = buttons[0];
    button(
        buffer,
        cancel,
        " esc close ",
        styles.bold.bg(palette.surface0),
    );
    let hint = match (dialog.view.tab, dialog.view.focus) {
        (RemotesTab::Account, _) => " ↑↓ select  ↵ run  1-3 view  ←→ settings tab  esc close",
        (_, Focus::List) => " ↑↓ select  ↵/tab actions  1-3 view  ←→ settings tab  esc close",
        (_, Focus::Actions) => " ↑↓ select  ↵ run  tab/esc list  1-3 view  ←→ settings tab",
    };
    put_text(
        buffer,
        inner.x,
        bottom.saturating_sub(2),
        inner.width,
        hint,
        styles.faint,
    );
    let message_rows: u16 = match inner.height {
        16.. => 2,
        12.. => 1,
        _ => 0,
    };
    let content_top = inner.y + 4;
    let content_bottom = bottom.saturating_sub(2 + message_rows);
    let content = Rect::new(
        inner.x,
        content_top,
        inner.width,
        content_bottom.saturating_sub(content_top),
    );
    let message = Rect::new(
        inner.x + 1,
        content_bottom,
        inner.width.saturating_sub(2),
        message_rows,
    );
    match dialog.view.tab {
        RemotesTab::Remotes => {
            render_remotes_tab(buffer, content, dialog, palette, &styles, &mut hits);
            render_message(buffer, message, &dialog.message, &styles);
        }
        RemotesTab::Images => {
            render_images_tab(buffer, content, dialog, palette, &styles, &mut hits);
            render_message(buffer, message, &dialog.message, &styles);
        }
        RemotesTab::Account => {
            // The account's text (sign-in steps, the shared note) uses the strip too.
            let content = Rect::new(
                content.x,
                content.y,
                content.width,
                content.height + message_rows,
            );
            render_account_tab(buffer, content, dialog, &styles, &mut hits);
        }
    }
    Some(OverlayRender {
        area,
        cancel,
        settings_popup: area,
        settings_tabs,
        remotes: hits,
        ..Default::default()
    })
}

fn render_message(buffer: &mut Buffer, area: Rect, message: &str, styles: &Styles) {
    let lines = wrap(message, area.width);
    for (offset, line) in lines.iter().take(usize::from(area.height)).enumerate() {
        put_text(
            buffer,
            area.x,
            area.y + offset as u16,
            area.width,
            line,
            styles.dim,
        );
    }
}

/// Splits `content` into the list pane, a separator and the details pane.
fn panes(buffer: &mut Buffer, content: Rect, styles: &Styles, palette: &Palette) -> (Rect, Rect) {
    let left_width = (content.width / 3).clamp(14, 26).min(content.width / 2);
    let separator = content.x + left_width;
    for y in content.y..content.bottom() {
        put_text(
            buffer,
            separator,
            y,
            1,
            "│",
            styles.normal.fg(palette.surface0),
        );
    }
    let right_x = separator + 2;
    (
        Rect::new(content.x, content.y, left_width, content.height),
        Rect::new(
            right_x,
            content.y,
            content.right().saturating_sub(right_x + 1),
            content.height,
        ),
    )
}

/// The first row to show so that `selected` is visible.
fn scroll_for(selected: usize, count: usize, height: usize) -> usize {
    if count <= height || height == 0 {
        0
    } else {
        selected.saturating_sub(height - 1).min(count - height)
    }
}

fn render_remotes_tab(
    buffer: &mut Buffer,
    content: Rect,
    dialog: &LocationDialog,
    palette: &Palette,
    styles: &Styles,
    hits: &mut Vec<(Rect, RemotesHit)>,
) {
    let (list, details) = panes(buffer, content, styles, palette);
    let rows = dialog.list_rows();
    let selected_row = dialog.selected_row();
    let selected = rows
        .iter()
        .position(|row| *row == selected_row)
        .unwrap_or(0);
    let list_focus = dialog.view.focus == Focus::List && !dialog.busy;
    let skip = scroll_for(selected, rows.len(), usize::from(list.height));
    for (offset, row) in rows
        .iter()
        .skip(skip)
        .take(usize::from(list.height))
        .enumerate()
    {
        let rect = Rect::new(list.x, list.y + offset as u16, list.width, 1);
        let style = match (*row == selected_row, list_focus) {
            (true, true) => styles.highlight,
            (true, false) => styles.inactive,
            _ => styles.normal,
        };
        buffer.set_style(rect, style);
        let name = match row {
            ListRow::Local => "Local".to_owned(),
            ListRow::Remote(index) => dialog.profiles[*index].label.clone(),
            ListRow::Add => "Add remote".to_owned(),
        };
        let (glyph, glyph_tone) = dialog.glyph(*row);
        let glyph_style = if style == styles.highlight {
            style
        } else if *row == ListRow::Add {
            style.fg(palette.accent)
        } else {
            style.fg(tone(palette, glyph_tone))
        };
        if *row == selected_row {
            put_text(buffer, rect.x, rect.y, 1, "▸", style);
        }
        put_text(buffer, rect.x + 2, rect.y, 1, glyph, glyph_style);
        let text = Rect::new(rect.x + 4, rect.y, rect.width.saturating_sub(5), 1);
        let name_style = if *row == ListRow::Add && style != styles.highlight {
            style.fg(palette.accent)
        } else {
            style
        };
        let suffix_style = if style == styles.highlight {
            style
        } else {
            style.fg(palette.overlay1)
        };
        put_split(
            buffer,
            text,
            &name,
            name_style,
            &dialog.suffix(*row),
            suffix_style,
        );
        hits.push((rect, RemotesHit::Row(*row)));
    }
    render_remote_details(buffer, details, dialog, palette, styles, hits);
}

/// A line of the details pane.
enum Line {
    Text(String, Style),
    Blank,
    /// Two columns of group headings or actions (indices into the actions).
    Grid([Option<Cell>; 2]),
    Reason(String),
}

#[derive(Clone, Copy)]
enum Cell {
    Heading(ActionGroup),
    Action(usize),
}

fn remote_info(dialog: &LocationDialog, row: ListRow) -> (String, Vec<String>) {
    match row {
        ListRow::Local => (
            "Local · this computer".into(),
            if dialog.is_default(row) {
                vec!["New workspaces open here by default.".into()]
            } else {
                Vec::new()
            },
        ),
        ListRow::Add => (
            "Add remote".into(),
            vec!["Create a hangar machine, from the herdr template or one of your images, or add an SSH remote. Press ↵ or click to start.".into()],
        ),
        ListRow::Remote(index) => {
            let profile = &dialog.profiles[index];
            let state = dialog.state_text(index);
            let mut info = Vec::new();
            let Some(binding) = dialog.binding_of(index) else {
                info.push(format!("{} · session {}", profile.target, profile.session));
                if let Some(cwd) = dialog
                    .prefs
                    .remotes
                    .get(&profile.id)
                    .map(|options| options.cwd.as_str())
                    .filter(|cwd| !cwd.is_empty())
                {
                    info.push(format!("directory {cwd}"));
                }
                return (format!("{} · ssh · {state}", profile.label), info);
            };
            let host = binding
                .server
                .split_once("://")
                .map_or(binding.server.as_str(), |(_, host)| host)
                .trim_end_matches('/');
            let details = dialog.details_of(index);
            match details.and_then(|details| details.template.as_deref()) {
                Some(template) => info.push(format!("{host} · {template}")),
                None => info.push(host.to_owned()),
            }
            if let Some(spec) = details.and_then(|details| details.spec.as_ref()) {
                let memory = if spec.mem_mib % 1024 == 0 {
                    format!("{} GiB", spec.mem_mib / 1024)
                } else {
                    format!("{} MiB", spec.mem_mib)
                };
                let mut parts = vec![format!("{} vCPU", spec.vcpus), format!("{memory} RAM")];
                if let Some(root) = spec.root_disk_gib {
                    parts.push(format!("{root} GiB root"));
                }
                parts.push(format!("{} GiB /data", spec.persistent_disk_gib));
                info.push(parts.join(" · "));
            }
            if let Some(source) = details.and_then(|details| details.forked_from.as_deref()) {
                info.push(format!(
                    "forked from {}",
                    dialog.machine_name(source).unwrap_or(source)
                ));
            }
            if let Some(image) = details.and_then(|details| details.image_id.as_deref()) {
                info.push(format!(
                    "from image {}",
                    dialog.image_name(image).unwrap_or(image)
                ));
            }
            if let Some(note) = dialog.sync_notes.get(&profile.id) {
                info.push(format!("List {note}: showing the last synced state."));
            }
            if dialog.hidden.contains(&profile.id) {
                info.push("Hidden from the sidebar and not connected.".into());
            }
            (format!("{} · hangar · {state}", profile.label), info)
        }
    }
}

fn render_remote_details(
    buffer: &mut Buffer,
    area: Rect,
    dialog: &LocationDialog,
    palette: &Palette,
    styles: &Styles,
    hits: &mut Vec<(Rect, RemotesHit)>,
) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    let row = dialog.selected_row();
    let (header, info) = remote_info(dialog, row);
    let mut lines = vec![Line::Text(header, styles.bold)];
    for text in info {
        for line in wrap(&text, area.width) {
            lines.push(Line::Text(line, styles.dim));
        }
    }
    let actions = dialog.actions();
    let selected = dialog.selected_action();
    let actions_focus = dialog.view.focus == Focus::Actions && !dialog.busy;
    let mut selected_line = None;
    if !actions.is_empty() {
        let mut groups: Vec<(ActionGroup, Vec<usize>)> = Vec::new();
        for (index, entry) in actions.iter().enumerate() {
            match groups.last_mut() {
                Some((group, members)) if *group == entry.group => members.push(index),
                _ => groups.push((entry.group, vec![index])),
            }
        }
        let columns = if area.width >= 40 { 2 } else { 1 };
        for chunk in groups.chunks(columns) {
            lines.push(Line::Blank);
            let mut headings = [None, None];
            for (column, (group, _)) in chunk.iter().enumerate() {
                headings[column] = Some(Cell::Heading(*group));
            }
            lines.push(Line::Grid(headings));
            let depth = chunk
                .iter()
                .map(|(_, members)| members.len())
                .max()
                .unwrap_or(0);
            for depth_index in 0..depth {
                let mut cells = [None, None];
                let mut reason = None;
                for (column, (_, members)) in chunk.iter().enumerate() {
                    if let Some(index) = members.get(depth_index) {
                        let entry = &actions[*index];
                        if selected.as_ref() == Some(entry) {
                            selected_line = Some(lines.len());
                            reason = entry.reason.filter(|_| actions_focus);
                        }
                        cells[column] = Some(Cell::Action(*index));
                    }
                }
                lines.push(Line::Grid(cells));
                // Why the selected action is dimmed, right below it.
                if let Some(reason) = reason {
                    lines.push(Line::Reason(format!("↳ {reason}")));
                }
            }
        }
    }
    let height = usize::from(area.height);
    // The header stays; the rest scrolls to keep the selected action and its reason in
    // view.
    let visible: Vec<usize> = if lines.len() <= height || height < 2 {
        (0..lines.len().min(height)).collect()
    } else {
        let body = height - 1;
        let skip = match selected_line {
            Some(line) if actions_focus => {
                (line + 1).saturating_sub(body).min(lines.len() - 1 - body)
            }
            _ => 0,
        };
        std::iter::once(0)
            .chain(1 + skip..1 + skip + body)
            .collect()
    };
    let column_width = if area.width >= 40 {
        area.width / 2
    } else {
        area.width
    };
    for (offset, line) in visible.iter().map(|index| &lines[*index]).enumerate() {
        let y = area.y + offset as u16;
        match line {
            Line::Text(text, style) => put_text(buffer, area.x, y, area.width, text, *style),
            Line::Blank => {}
            Line::Reason(text) => put_text(
                buffer,
                area.x,
                y,
                area.width,
                text,
                styles.normal.fg(palette.yellow),
            ),
            Line::Grid(cells) => {
                for (column, cell) in cells.iter().enumerate() {
                    let rect = Rect::new(
                        area.x + column as u16 * column_width,
                        y,
                        column_width.saturating_sub(1),
                        1,
                    );
                    match cell {
                        Some(Cell::Heading(group)) => put_text(
                            buffer,
                            rect.x,
                            y,
                            rect.width,
                            group.title(),
                            styles.dim.add_modifier(Modifier::BOLD),
                        ),
                        Some(Cell::Action(index)) => {
                            let entry = &actions[*index];
                            let is_selected = selected.as_ref() == Some(entry);
                            let style = match (is_selected && actions_focus, entry.reason.is_some())
                            {
                                (true, false) => styles.highlight,
                                (true, true) => styles.inactive.fg(palette.overlay1),
                                (false, true) => styles.faint,
                                (false, false) => styles.normal,
                            };
                            buffer.set_style(rect, style);
                            let marker = if is_selected && actions_focus {
                                "▸"
                            } else {
                                " "
                            };
                            put_text(
                                buffer,
                                rect.x,
                                y,
                                rect.width,
                                &format!("{marker} {}", entry.label),
                                style,
                            );
                            hits.push((rect, RemotesHit::Action(entry.action)));
                        }
                        None => {}
                    }
                }
            }
        }
    }
}

const NO_IMAGES: &str = "No images yet. An image saves a hangar machine's root disk (installed packages and system configuration) so new machines can start from it. To save one, select a hangar machine under Remotes and choose Save as image….";

fn render_images_tab(
    buffer: &mut Buffer,
    content: Rect,
    dialog: &LocationDialog,
    palette: &Palette,
    styles: &Styles,
    hits: &mut Vec<(Rect, RemotesHit)>,
) {
    let (list, details) = panes(buffer, content, styles, palette);
    let images = dialog.images();
    let explain = |buffer: &mut Buffer, text: &str| {
        for (offset, line) in wrap(text, details.width)
            .iter()
            .take(usize::from(details.height))
            .enumerate()
        {
            put_text(
                buffer,
                details.x,
                details.y + offset as u16,
                details.width,
                line,
                styles.dim,
            );
        }
    };
    match &dialog.view.images {
        None => {
            put_text(
                buffer,
                list.x + 1,
                list.y,
                list.width.saturating_sub(1),
                "Loading…",
                styles.faint,
            );
            explain(buffer, "Loading your images…");
            return;
        }
        Some(Err(error)) => {
            explain(buffer, error);
            return;
        }
        Some(Ok(_)) if images.is_empty() => {
            put_text(
                buffer,
                list.x + 1,
                list.y,
                list.width.saturating_sub(1),
                "No images",
                styles.faint,
            );
            explain(buffer, NO_IMAGES);
            return;
        }
        Some(Ok(_)) => {}
    }
    let selected = dialog.selected_image_index().unwrap_or(0);
    let list_focus = dialog.view.focus == Focus::List && !dialog.busy;
    let skip = scroll_for(selected, images.len(), usize::from(list.height));
    for (offset, (index, image)) in images
        .iter()
        .enumerate()
        .skip(skip)
        .take(usize::from(list.height))
        .enumerate()
    {
        let rect = Rect::new(list.x, list.y + offset as u16, list.width, 1);
        let style = match (index == selected, list_focus) {
            (true, true) => styles.highlight,
            (true, false) => styles.inactive,
            _ => styles.normal,
        };
        buffer.set_style(rect, style);
        if index == selected {
            put_text(buffer, rect.x, rect.y, 1, "▸", style);
        }
        let date_style = if style == styles.highlight {
            style
        } else {
            style.fg(palette.overlay1)
        };
        put_split(
            buffer,
            Rect::new(rect.x + 2, rect.y, rect.width.saturating_sub(3), 1),
            &image.name,
            style,
            date(&image.created_at),
            date_style,
        );
        hits.push((rect, RemotesHit::Image(index)));
    }
    let Some(image) = images.get(selected) else {
        return;
    };
    let mut lines: Vec<(String, Style)> = vec![(image.name.clone(), styles.bold)];
    if !image.description.is_empty() {
        for line in wrap(&image.description, details.width).into_iter().take(3) {
            lines.push((line, styles.normal));
        }
    }
    lines.push((format!("created {}", date(&image.created_at)), styles.dim));
    let source = match dialog.machine_name(&image.source_machine_id) {
        Some(name) => format!("saved from {name}"),
        None => "saved from a machine that no longer exists".into(),
    };
    lines.push((source, styles.dim));
    lines.push((format!("template {}", image.template.label()), styles.dim));
    if image.root_size_bytes > 0 {
        lines.push((
            format!("root disk {}", bytes(image.root_size_bytes)),
            styles.dim,
        ));
    }
    if let Some(exclusive) = image.exclusive_bytes {
        lines.push((
            format!("{} stored only for this image", bytes(exclusive)),
            styles.dim,
        ));
    }
    lines.push((String::new(), styles.normal));
    lines.push(("Image".into(), styles.dim.add_modifier(Modifier::BOLD)));
    let actions_focus = dialog.view.focus == Focus::Actions && !dialog.busy;
    let action_top = lines.len();
    let height = usize::from(details.height);
    let skip = if actions_focus {
        (action_top + ImageAction::ALL.len()).saturating_sub(height)
    } else {
        0
    };
    for (offset, (text, style)) in lines.iter().skip(skip).take(height).enumerate() {
        put_text(
            buffer,
            details.x,
            details.y + offset as u16,
            details.width,
            text,
            *style,
        );
    }
    for (index, action) in ImageAction::ALL.iter().enumerate() {
        let Some(line) = (action_top + index).checked_sub(skip) else {
            continue;
        };
        if line >= height {
            break;
        }
        let rect = Rect::new(details.x, details.y + line as u16, details.width.min(30), 1);
        let selected = actions_focus && *action == dialog.view.image_action;
        let style = if selected {
            styles.highlight
        } else {
            styles.normal
        };
        buffer.set_style(rect, style);
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width,
            &format!("{} {}", if selected { "▸" } else { " " }, action.label()),
            style,
        );
        hits.push((rect, RemotesHit::ImageAction(*action)));
    }
}

fn render_account_tab(
    buffer: &mut Buffer,
    content: Rect,
    dialog: &LocationDialog,
    styles: &Styles,
    hits: &mut Vec<(Rect, RemotesHit)>,
) {
    let area = Rect::new(
        content.x + 1,
        content.y,
        content.width.saturating_sub(2),
        content.height,
    );
    let mut y = area.y;
    let line = |buffer: &mut Buffer, y: &mut u16, text: &str, style: Style| {
        if *y < area.bottom() {
            put_text(buffer, area.x, *y, area.width, text, style);
        }
        *y += 1;
    };
    let summary = dialog
        .account
        .as_ref()
        .map(|status| status.summary())
        .unwrap_or_else(|| "Checking your hangar sign-in…".into());
    for text in wrap(&summary, area.width).iter().take(2) {
        line(buffer, &mut y, text, styles.bold);
    }
    y += 1;
    let selected_action = dialog.selected_account_action();
    for action in dialog.account_actions() {
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(area.x, y, area.width.min(30), 1);
        let selected = !dialog.busy && action == selected_action;
        let style = if selected {
            styles.highlight
        } else {
            styles.normal
        };
        buffer.set_style(rect, style);
        put_text(
            buffer,
            rect.x,
            y,
            rect.width,
            &format!("{} {}", if selected { "▸" } else { " " }, action.label()),
            style,
        );
        hits.push((rect, RemotesHit::AccountAction(action)));
        y += 1;
    }
    y += 1;
    line(
        buffer,
        &mut y,
        "Usage",
        styles.dim.add_modifier(Modifier::BOLD),
    );
    match &dialog.view.usage {
        None => line(buffer, &mut y, "Loading usage…", styles.faint),
        Some(Err(error)) => {
            for text in wrap(error, area.width).iter().take(2) {
                line(buffer, &mut y, text, styles.dim);
            }
        }
        Some(Ok(usage)) => {
            let limits = &usage.limits;
            let storage = if limits.max_stored_gib > 0 {
                let quota = limits.max_stored_gib << 30;
                format!(
                    "Storage   {} of {} stored ({}%)",
                    bytes(usage.stored_bytes),
                    bytes(quota),
                    usage.stored_bytes.saturating_mul(100) / quota
                )
            } else {
                format!("Storage   {} stored", bytes(usage.stored_bytes))
            };
            line(buffer, &mut y, &storage, styles.normal);
            let count = |label: &str, used: u64, max: u64| {
                if max > 0 {
                    format!("{label}{used} of {max}")
                } else {
                    format!("{label}{used}")
                }
            };
            line(
                buffer,
                &mut y,
                &count("Machines  ", usage.machines, limits.max_machines),
                styles.normal,
            );
            line(
                buffer,
                &mut y,
                &count("Images    ", usage.images, limits.max_images),
                styles.normal,
            );
            let measured = match &usage.computed_at {
                Some(at) => format!(
                    "Measured {} {} UTC (usage is measured hourly)",
                    date(at),
                    at.get(11..16).unwrap_or("")
                ),
                None => "Not measured yet; usage is measured hourly.".into(),
            };
            line(buffer, &mut y, &measured, styles.faint);
        }
    }
    y += 1;
    let text = if dialog.message.is_empty() {
        SHARED_NOTE
    } else {
        dialog.message.as_str()
    };
    for text in wrap(text, area.width) {
        if y >= area.bottom() {
            break;
        }
        line(buffer, &mut y, &text, styles.dim);
    }
}
