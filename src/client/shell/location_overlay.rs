use super::*;
use crate::client::shell::locations::{LocationDialog, LocationDialogKind};

pub(in crate::client::shell) fn render_locations(
    buffer: &mut Buffer,
    dialog: &LocationDialog,
    palette: &Palette,
) -> Option<OverlayRender> {
    if let LocationDialogKind::Add(form) = &dialog.kind {
        return render_add_remote(buffer, dialog, form, palette);
    }
    // The remotes list is the Settings → Remotes tab, so it keeps the settings frame and tabs.
    let manage = matches!(dialog.kind, LocationDialogKind::Manage);
    let area = if manage {
        popup(buffer.area, 76, 22)?
    } else {
        popup(buffer.area, 78, 24)?
    };
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
        &format!(" {}", if manage { "settings" } else { dialog.title() }),
        normal.add_modifier(Modifier::BOLD),
    );
    let settings_tabs = if manage {
        super::settings_overlay::render_settings_tabs(
            buffer,
            inner,
            crate::client::shell::state::ClientSettingsSection::Remotes,
            false,
            palette,
        )
    } else {
        Vec::new()
    };
    let rows_top = if manage { inner.y + 4 } else { inner.y + 2 };
    let visible = usize::from(
        inner
            .height
            .saturating_sub(8)
            .saturating_sub(rows_top - inner.y - 2),
    );
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
        if dialog.choice_field(index) {
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width,
                &format!(" {label}: ‹ {} ›", dialog.location_label()),
                style,
            );
        } else if matches!(dialog.kind, LocationDialogKind::Manage) {
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width,
                &format!(" {}", dialog.row_label(index)),
                style,
            );
        } else if let Some(editor) = dialog.fields.get(index) {
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
    // Confirmations have no rows, so their text gets the whole body; Save as image has
    // two rows and a long explanation below them.
    let message_area = if let LocationDialogKind::SaveImage(_) = dialog.kind {
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
            LocationDialogKind::Edit(_) => " ↵ save ",
            LocationDialogKind::New => " ↵ create ",
            LocationDialogKind::Stop => " ↵ stop ",
            LocationDialogKind::Suspend => " ↵ suspend ",
            LocationDialogKind::Delete(_) | LocationDialogKind::DeleteImage(_) => " ↵ delete ",
            LocationDialogKind::SaveImage(ref request) => request.primary_label(),
        }
    };
    // Save as image cannot proceed while checking or when the machine cannot be saved.
    let enabled = !dialog.busy
        && match &dialog.kind {
            LocationDialogKind::SaveImage(request) => request.plan().is_some(),
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
    button(buffer, cancel, " esc close ", normal.bg(palette.surface0));
    Some(OverlayRender {
        area,
        primary,
        cancel,
        settings_choices: hits,
        settings_tabs,
        cursor,
        ..Default::default()
    })
}

use crate::client::shell::locations::add::{
    AddRemoteForm, IMAGE_ACTION_FIELD, NAME_FIELD, SOURCE_FIELD,
};

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
        if i == NAME_FIELD && form.deletable().is_some() {
            // A machine no remote uses can be deleted instead of added.
            buffer.set_style(rect, style);
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width,
                &format!("{:<10} [ Delete machine… ]", ""),
                style,
            );
            hits.push((rect, i));
            continue;
        }
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
        if i == SOURCE_FIELD && !form.creating() {
            continue;
        }
        if i == NAME_FIELD {
            // Only creating a machine needs a name.
            if !form.creating() {
                continue;
            }
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
    button(buffer, cancel, " esc close ", normal.bg(palette.surface0));
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
