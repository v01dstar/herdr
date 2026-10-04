use super::*;
use crate::client::shell::locations::{
    CopyChoice, CopyRequest, LocationDialog, LocationDialogKind,
};

pub(in crate::client::shell) fn render_locations(
    buffer: &mut Buffer,
    dialog: &LocationDialog,
    palette: &Palette,
) -> Option<OverlayRender> {
    match &dialog.kind {
        LocationDialogKind::Add(form) => return render_add_remote(buffer, dialog, form, palette),
        LocationDialogKind::Manage => {
            return super::remotes_overlay::render_remotes(buffer, dialog, palette)
        }
        LocationDialogKind::Copy(request) if !request.chosen => {
            return render_copy_chooser(buffer, request, palette)
        }
        _ => {}
    }
    // The sign-up form has one field and a few lines of text.
    let (width, height) = match dialog.kind {
        LocationDialogKind::SignUp => (72, 16),
        _ => (78, 24),
    };
    let area = popup(buffer.area, width, height)?;
    let inner = panel(buffer, area, palette.accent, palette.panel_bg)?;
    if inner.width < 20 || inner.height < 7 {
        return None;
    }
    let normal = Style::default().fg(palette.text).bg(palette.panel_bg);
    let highlight = Style::default().fg(contrast(palette)).bg(palette.accent);
    put_text(
        buffer,
        inner.x,
        inner.y,
        inner.width,
        &format!(" {}", dialog.title()),
        normal.add_modifier(Modifier::BOLD),
    );
    let rows_top = inner.y + 2;
    let visible = usize::from(inner.height.saturating_sub(8));
    let scroll = dialog
        .selected
        .min(dialog.labels().len().saturating_sub(1))
        .saturating_sub(visible.saturating_sub(1));
    let mut hits = Vec::new();
    let mut cursor = None;
    for (position, (index, label)) in dialog
        .labels()
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .enumerate()
    {
        let rect = Rect::new(inner.x, rows_top + position as u16, inner.width, 1);
        let style = if dialog.selected == index && !dialog.busy {
            highlight
        } else {
            normal
        };
        buffer.set_style(rect, style);
        if let Some(editor) = dialog.fields.get(index) {
            let prefix = format!(" {label}: ");
            let width = display_width(&prefix).min(rect.width);
            put_text(buffer, rect.x, rect.y, width, &prefix, style);
            let input = Rect::new(rect.x + width, rect.y, rect.width.saturating_sub(width), 1);
            let input_cursor =
                crate::client::shell::text_editor::render(buffer, input, editor, style);
            if dialog.selected == index && !dialog.busy {
                cursor = input_cursor;
            }
        }
        hits.push((rect, index));
    }
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    // Confirmations have no rows, so their text gets the whole body; the Copy machine…
    // and sign-up forms have few rows and a long explanation below them.
    let message_area = if matches!(
        dialog.kind,
        LocationDialogKind::Copy(_) | LocationDialogKind::SignUp
    ) {
        let top = inner.y + 2 + dialog.labels().len() as u16 + 1;
        Rect::new(
            inner.x + 1,
            top,
            inner.width.saturating_sub(2),
            inner.bottom().saturating_sub(2).saturating_sub(top),
        )
    } else if dialog.labels().is_empty() {
        Rect::new(
            inner.x + 1,
            inner.y + 2,
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(4),
        )
    } else {
        Rect::new(
            inner.x + 1,
            inner.bottom().saturating_sub(5),
            inner.width.saturating_sub(2),
            4,
        )
    };
    Paragraph::new(dialog.message.as_str())
        .style(normal)
        .wrap(Wrap { trim: true })
        .render(message_area, buffer);
    let label = if dialog.busy {
        " working… "
    } else {
        match dialog.kind {
            LocationDialogKind::Add(_) => " ↵ connect ",
            LocationDialogKind::Manage => " ↵ select ",
            LocationDialogKind::SignOut => " ↵ sign out ",
            LocationDialogKind::SignUp => " ↵ sign up ",
            LocationDialogKind::Edit(_) => " ↵ save ",
            LocationDialogKind::Stop => " ↵ stop ",
            LocationDialogKind::Suspend => " ↵ suspend ",
            LocationDialogKind::Delete(_) | LocationDialogKind::DeleteImage(_) => " ↵ delete ",
            LocationDialogKind::Copy(ref request) => request.primary_label(),
        }
    };
    // A Copy machine… form cannot proceed while checking or when the machine cannot be
    // used.
    let enabled = !dialog.busy
        && match &dialog.kind {
            LocationDialogKind::Copy(request) => request.plan().is_some(),
            _ => true,
        };
    let width = display_width(label).max(14);
    let buttons = row(inner, &[width, 12], 2, inner.height.saturating_sub(1));
    let primary = buttons[0];
    let cancel = buttons[1];
    button(
        buffer,
        primary,
        label,
        if enabled { highlight } else { normal },
    );
    // Forms and confirmations of Settings → Remotes go back to it (a Copy machine… form
    // to its chooser).
    button(buffer, cancel, " esc back ", normal.bg(palette.surface0));
    Some(OverlayRender {
        area,
        primary,
        cancel,
        settings_choices: hits,
        cursor,
        ..Default::default()
    })
}

/// What each Copy machine… choice copies, as the chooser's table shows it:
/// (row, clone, image).
pub(in crate::client::shell) const COPY_TABLE: [(&str, bool, bool); 4] = [
    ("Installed software", true, true),
    ("System settings", true, true),
    ("Repos & home files", true, false),
    ("Logins (gh, claude)", true, false),
];

pub(in crate::client::shell) const CHECK: &str = "✓";
pub(in crate::client::shell) const CROSS: &str = "✗";

/// A choice's title, description lines, and result (long and short).
fn copy_choice_text(
    choice: CopyChoice,
) -> (&'static str, [&'static str; 2], &'static str, &'static str) {
    match choice {
        CopyChoice::Clone => (
            "Clone now",
            ["a second machine", "exactly like this one"],
            "new machine",
            "machine",
        ),
        CopyChoice::Image => (
            "Save as image",
            ["a starting point for", "new machines"],
            "reusable image",
            "image",
        ),
    }
}

/// The Copy machine… chooser: the two choices side by side (stacked when narrow) over a
/// table of what each copies. ←/→ or a click switch; ↵ or a click on the chosen one
/// continues.
fn render_copy_chooser(
    buffer: &mut Buffer,
    request: &CopyRequest,
    palette: &Palette,
) -> Option<OverlayRender> {
    // Just tall enough for the choices, the table and the buttons.
    let area = popup(buffer.area, 62, 17)?;
    let inner = panel(buffer, area, palette.accent, palette.panel_bg)?;
    if inner.width < 20 || inner.height < 7 {
        return None;
    }
    let normal = Style::default().fg(palette.text).bg(palette.panel_bg);
    let dim = normal.fg(palette.subtext0);
    let faint = normal.fg(palette.overlay0);
    let highlight = Style::default()
        .fg(contrast(palette))
        .bg(palette.accent)
        .add_modifier(Modifier::BOLD);
    put_text(
        buffer,
        inner.x,
        inner.y,
        inner.width,
        &format!(" copy {}", request.machine_name),
        normal.add_modifier(Modifier::BOLD),
    );
    // The last two rows hold a spacer and the buttons.
    let limit = inner.bottom().saturating_sub(2);
    let mut hits = Vec::new();
    let mut y = inner.y + 2;
    let wide = inner.width >= 50;
    let x = inner.x + 2;
    let width = inner.width.saturating_sub(3);
    if wide {
        // Side by side: each column a title and two description lines.
        let column = (width / 2).min(23);
        for choice in CopyChoice::ALL {
            let (title, description, _, _) = copy_choice_text(choice);
            let left = x + column * choice.index() as u16;
            let selected = choice == request.choice;
            let marker = if selected { "▸ " } else { "  " };
            if y < limit {
                put_text(buffer, left, y, 2, marker, normal);
                let title_style = if selected { highlight } else { normal };
                let text = format!(" {title} ");
                put_text(
                    buffer,
                    left + 1,
                    y,
                    display_width(&text).min(column.saturating_sub(1)),
                    &text,
                    title_style,
                );
            }
            for (offset, line) in description.iter().enumerate() {
                let row = y + 1 + offset as u16;
                if row < limit {
                    put_text(buffer, left + 2, row, column.saturating_sub(2), line, dim);
                }
            }
            let rows = 3.min(limit.saturating_sub(y));
            hits.push((Rect::new(left, y, column, rows), choice.index()));
        }
        y += 4;
    } else {
        // Stacked: each choice's title, then its description on one line.
        for choice in CopyChoice::ALL {
            if y >= limit {
                break;
            }
            let (title, description, _, _) = copy_choice_text(choice);
            let selected = choice == request.choice;
            put_text(buffer, x, y, 2, if selected { "▸ " } else { "  " }, normal);
            let text = format!(" {title} ");
            put_text(
                buffer,
                x + 1,
                y,
                display_width(&text).min(width.saturating_sub(1)),
                &text,
                if selected { highlight } else { normal },
            );
            let rows = if y + 1 < limit {
                let text = crate::ui::truncate_end(
                    &description.join(" "),
                    usize::from(width.saturating_sub(2)),
                );
                put_text(buffer, x + 2, y + 1, width.saturating_sub(2), &text, dim);
                2
            } else {
                1
            };
            hits.push((Rect::new(x, y, width, rows), choice.index()));
            y += rows;
        }
        y += 1;
    }
    // The table: a label column, then one column per choice; the chosen one's header
    // is accented.
    let cell = if wide { [14u16, 16] } else { [7, 7] };
    let label_width = width.saturating_sub(cell[0] + cell[1]).min(20);
    let column_x = [x + label_width, x + label_width + cell[0]];
    let centered = |buffer: &mut Buffer, column: usize, y: u16, text: &str, style: Style| {
        let text_width = display_width(text).min(cell[column]);
        put_text(
            buffer,
            column_x[column] + (cell[column] - text_width) / 2,
            y,
            text_width,
            text,
            style,
        );
    };
    let table_top = y;
    if y < limit {
        for choice in CopyChoice::ALL {
            let style = if choice == request.choice {
                normal.fg(palette.accent).add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            let header = match choice {
                CopyChoice::Clone => "Clone",
                CopyChoice::Image => "Image",
            };
            centered(buffer, choice.index(), y, header, style);
        }
        y += 1;
    }
    for (label, clone, image) in COPY_TABLE {
        if y >= limit {
            break;
        }
        put_text(buffer, x, y, label_width.saturating_sub(1), label, normal);
        for (column, included) in [clone, image].into_iter().enumerate() {
            let (glyph, style) = if included {
                (CHECK, normal.fg(palette.green))
            } else {
                (CROSS, faint)
            };
            centered(buffer, column, y, glyph, style);
        }
        y += 1;
    }
    if y < limit {
        put_text(
            buffer,
            x,
            y,
            label_width.saturating_sub(1),
            "Result",
            normal,
        );
        for choice in CopyChoice::ALL {
            let (_, _, long, short) = copy_choice_text(choice);
            let text = if display_width(long) <= cell[choice.index()] {
                long
            } else {
                short
            };
            centered(buffer, choice.index(), y, text, normal);
        }
        y += 1;
    }
    // A click on a table column picks that choice too.
    for choice in CopyChoice::ALL {
        let column = choice.index();
        if y > table_top {
            hits.push((
                Rect::new(column_x[column], table_top, cell[column], y - table_top),
                column,
            ));
        }
    }
    // ↵ continue, the ←→ hint when there is room, esc cancel.
    let bottom = inner.height.saturating_sub(1);
    let (primary, cancel) = if inner.width >= 46 {
        let buttons = row(inner, &[14, 11, 14], 2, bottom);
        put_text(
            buffer,
            buttons[1].x + 1,
            buttons[1].y,
            buttons[1].width.saturating_sub(1),
            "←→ switch",
            faint,
        );
        (buttons[0], buttons[2])
    } else {
        let buttons = row(inner, &[14, 14], 2, bottom);
        (buttons[0], buttons[1])
    };
    button(buffer, primary, " ↵ continue ", highlight);
    button(buffer, cancel, " esc cancel ", normal.bg(palette.surface0));
    Some(OverlayRender {
        area,
        primary,
        cancel,
        settings_choices: hits,
        ..Default::default()
    })
}

use crate::client::shell::locations::add::{AddRemoteForm, IMAGE_ACTION_FIELD, NAME_FIELD};

pub(in crate::client::shell) fn render_add_remote(
    buffer: &mut Buffer,
    dialog: &LocationDialog,
    form: &AddRemoteForm,
    palette: &Palette,
) -> Option<OverlayRender> {
    let area = popup(buffer.area, 78, 24)?;
    let inner = panel(buffer, area, palette.accent, palette.panel_bg)?;
    if inner.width < 20 || inner.height < 10 {
        return None;
    }
    let normal = Style::default().fg(palette.text).bg(palette.panel_bg);
    let selected = normal.fg(contrast(palette)).bg(palette.accent);
    put_text(
        buffer,
        inner.x,
        inner.y,
        inner.width,
        " add remote",
        normal.add_modifier(Modifier::BOLD),
    );
    let mut hits = Vec::new();
    let mut cursor = None;
    for (i, label) in dialog.labels().iter().enumerate() {
        let rect = Rect::new(
            inner.x + 1,
            inner.y + 2 + i as u16 * 2,
            inner.width.saturating_sub(2),
            1,
        );
        let style = if dialog.selected == i && !dialog.busy {
            selected
        } else {
            normal
        };
        if i == IMAGE_ACTION_FIELD {
            // The image chosen as source can be deleted here.
            if form.deletable_image().is_some() {
                buffer.set_style(rect, style);
                put_text(
                    buffer,
                    rect.x,
                    rect.y,
                    rect.width,
                    &format!("{:<10} [ Delete image… ]", ""),
                    style,
                );
                hits.push((rect, i));
            }
            continue;
        }
        if i == NAME_FIELD {
            buffer.set_style(rect, style);
            let prefix = format!("{label:<10} ");
            let width = display_width(&prefix).min(rect.width);
            put_text(buffer, rect.x, rect.y, width, &prefix, style);
            if let Some(editor) = dialog.fields.first() {
                let input = Rect::new(rect.x + width, rect.y, rect.width.saturating_sub(width), 1);
                let input_cursor =
                    crate::client::shell::text_editor::render(buffer, input, editor, style);
                if dialog.selected == i && !dialog.busy && form.edits_name() {
                    cursor = input_cursor;
                }
            }
            hits.push((rect, i));
            continue;
        }
        buffer.set_style(rect, style);
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width,
            &format!("{label:<10} [ {} ▾ ]", form.label(i)),
            style,
        );
        hits.push((rect, i));
    }
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    Paragraph::new(dialog.message.as_str())
        .style(normal)
        .wrap(Wrap { trim: true })
        .render(
            Rect::new(
                inner.x + 1,
                inner.bottom().saturating_sub(6),
                inner.width.saturating_sub(2),
                4,
            ),
            buffer,
        );
    let buttons = row(inner, &[24, 12], 2, inner.height.saturating_sub(1));
    let primary = buttons[0];
    let cancel = buttons[1];
    let text = if dialog.busy {
        " working… "
    } else {
        form.primary_label()
    };
    button(
        buffer,
        primary,
        text,
        if dialog.busy || !form.can_submit() {
            normal
        } else {
            selected
        },
    );
    button(buffer, cancel, " esc back ", normal.bg(palette.surface0));
    if let Some((field, selection)) = form.dropdown {
        let items = form.options(field);
        let y = inner.y + 3 + field as u16 * 2;
        let height = usize::from(inner.bottom().saturating_sub(y + 1)).min(8);
        let skip = selection.saturating_sub(height.saturating_sub(1));
        for (visible, (index, item)) in items.iter().enumerate().skip(skip).take(height).enumerate()
        {
            let rect = Rect::new(
                inner.x + 10,
                y + visible as u16,
                inner.width.saturating_sub(11),
                1,
            );
            let style = if index == selection {
                selected
            } else {
                normal.bg(palette.surface0)
            };
            for x in rect.x..rect.right() {
                buffer[(x, rect.y)].set_symbol(" ").set_style(style);
            }
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width,
                &format!(" {item}"),
                style,
            );
            hits.push((rect, 1000 + index));
        }
    }
    Some(OverlayRender {
        area,
        primary,
        cancel,
        settings_choices: hits,
        cursor: if form.dropdown.is_some() {
            None
        } else {
            cursor
        },
        ..Default::default()
    })
}
