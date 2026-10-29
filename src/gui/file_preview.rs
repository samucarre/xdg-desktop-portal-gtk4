//! Previews for the open dialog, like the macOS Finder: a panel on the right that shows the
//! selected file, and Quick Look (space opens a bigger preview, space or escape closes it,
//! the arrows move through the files while it is open).
//!
//! GTK 4 dropped the file chooser preview widget and has no selection signal, so the
//! selection is polled and files are loaded off the main thread.

use {
    gtk4::{
        gdk::{prelude::TextureExt, Key, ModifierType, Texture},
        gio::{self, Cancellable, File, FileQueryInfoFlags, FileType},
        glib::{self, ControlFlow, Propagation},
        pango::WrapMode,
        prelude::{
            BoxExt, Cast, DialogExt, DisplayExt, EventControllerExt, FileChooserExt, FileExt,
            GtkWindowExt, ListModelExt, MonitorExt, NativeExt, ObjectExt, OrientableExt,
            SelectionModelExt, ToVariant, WidgetExt,
        },
        EventControllerKey, FileChooserDialog, GridView, Image, Justification, Label, ListView,
        Orientation, Picture, PolicyType, PropagationPhase, ScrolledWindow, Separator, Widget,
        Window,
    },
    std::{cell::RefCell, rc::Rc, time::Duration},
};

const PANEL_WIDTH: i32 = 320;
// Images bigger than this are not decoded for the preview.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

struct PreviewData {
    name: String,
    content_type: String,
    size: Option<u64>,
    texture: Option<Texture>,
}

#[derive(Clone)]
struct Shown {
    name: String,
    details: String,
    content_type: String,
    texture: Option<Texture>,
}

#[derive(Clone)]
struct PreviewView {
    content: gtk4::Box,
    picture: Picture,
    icon: Image,
    name: Label,
    details: Label,
}

impl PreviewView {
    fn new(icon_size: i32, margin: i32) -> Self {
        let picture = Picture::new();
        picture.set_can_shrink(true);
        picture.set_vexpand(true);
        let icon = Image::new();
        icon.set_pixel_size(icon_size);
        icon.set_vexpand(true);
        let name = Label::new(None);
        name.set_wrap(true);
        name.set_wrap_mode(WrapMode::WordChar);
        name.set_justify(Justification::Center);
        name.add_css_class("heading");
        let details = Label::new(None);
        details.set_wrap(true);
        details.set_justify(Justification::Center);
        details.add_css_class("dim-label");
        let content = gtk4::Box::new(Orientation::Vertical, 12);
        content.set_margin_top(margin);
        content.set_margin_bottom(margin);
        content.set_margin_start(margin);
        content.set_margin_end(margin);
        content.append(&picture);
        content.append(&icon);
        content.append(&name);
        content.append(&details);
        Self {
            content,
            picture,
            icon,
            name,
            details,
        }
    }

    fn show(&self, s: &Shown) {
        match &s.texture {
            Some(t) => self.picture.set_paintable(Some(t)),
            None => self
                .icon
                .set_from_gicon(&gio::content_type_get_icon(&s.content_type)),
        }
        self.picture.set_visible(s.texture.is_some());
        self.icon.set_visible(s.texture.is_none());
        self.name.set_label(&s.name);
        self.details.set_label(&s.details);
        self.content.set_visible(true);
    }
}

type QuickLook = Rc<RefCell<Option<(Window, PreviewView)>>>;

pub fn add_preview(dialog: &FileChooserDialog) {
    let panel_view = PreviewView::new(128, 18);
    panel_view.content.set_visible(false);
    // The scrolled window keeps the natural size of big images from widening the panel.
    let panel = ScrolledWindow::new();
    panel.set_policy(PolicyType::Never, PolicyType::Never);
    panel.set_propagate_natural_width(false);
    panel.set_size_request(PANEL_WIDTH, -1);
    panel.set_child(Some(&panel_view.content));
    let area = dialog.content_area();
    area.set_orientation(Orientation::Horizontal);
    area.append(&Separator::new(Orientation::Vertical));
    area.append(&panel);

    let current = Rc::new(RefCell::new(None::<Shown>));
    let quick_look: QuickLook = Default::default();

    let keys = EventControllerKey::new();
    keys.set_propagation_phase(PropagationPhase::Capture);
    let weak = dialog.downgrade();
    let (c, q) = (current.clone(), quick_look.clone());
    keys.connect_key_pressed(move |_, key, _, mods| {
        let Some(dialog) = weak.upgrade() else {
            return Propagation::Proceed;
        };
        // Only in the file list, so that space still works in the search and location entries.
        if key != Key::space || has_modifiers(mods) || !focus_in_file_list(&dialog) {
            return Propagation::Proceed;
        }
        toggle_quick_look(&dialog, &c, &q);
        Propagation::Stop
    });
    dialog.add_controller(keys);

    let selected = Rc::new(RefCell::new(None::<String>));
    let weak = dialog.downgrade();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        let Some(dialog) = weak.upgrade() else {
            return ControlFlow::Break;
        };
        let file = dialog.file();
        let uri = file.as_ref().map(|f| f.uri().to_string());
        if *selected.borrow() == uri {
            return ControlFlow::Continue;
        }
        *selected.borrow_mut() = uri.clone();
        let Some(uri) = uri else {
            panel_view.content.set_visible(false);
            *current.borrow_mut() = None;
            return ControlFlow::Continue;
        };
        let (panel_view, current, quick_look, selected) = (
            panel_view.clone(),
            current.clone(),
            quick_look.clone(),
            selected.clone(),
        );
        glib::spawn_future_local(async move {
            let u = uri.clone();
            let Ok(data) = gio::spawn_blocking(move || load_preview(&u)).await else {
                return;
            };
            // The selection changed while the file was loading.
            if selected.borrow().as_deref() != Some(uri.as_str()) {
                return;
            }
            let shown = shown(data);
            panel_view.show(&shown);
            if let Some((window, view)) = &*quick_look.borrow() {
                window.set_title(Some(&shown.name));
                view.show(&shown);
            }
            *current.borrow_mut() = Some(shown);
        });
        ControlFlow::Continue
    });
}

fn shown(data: PreviewData) -> Shown {
    let mut lines = vec![gio::content_type_get_description(&data.content_type).to_string()];
    if let Some(t) = &data.texture {
        lines.push(format!("{} × {}", t.width(), t.height()));
    }
    if let Some(s) = data.size {
        lines.push(glib::format_size(s).to_string());
    }
    Shown {
        name: data.name,
        details: lines.join("\n"),
        content_type: data.content_type,
        texture: data.texture,
    }
}

fn has_modifiers(mods: ModifierType) -> bool {
    mods.intersects(
        ModifierType::CONTROL_MASK
            | ModifierType::ALT_MASK
            | ModifierType::SHIFT_MASK
            | ModifierType::SUPER_MASK,
    )
}

fn toggle_quick_look(
    dialog: &FileChooserDialog,
    current: &Rc<RefCell<Option<Shown>>>,
    quick_look: &QuickLook,
) {
    let open = quick_look.borrow_mut().take();
    if let Some((window, _)) = open {
        window.close();
        return;
    }
    let Some(shown) = current.borrow().clone() else {
        return;
    };
    let view = PreviewView::new(256, 24);
    let window = Window::new();
    window.set_transient_for(Some(dialog));
    window.set_destroy_with_parent(true);
    window.set_title(Some(&shown.name));
    window.set_child(Some(&view.content));
    let (width, height) = quick_look_size(dialog, shown.texture.as_ref());
    window.set_default_size(width, height);
    view.show(&shown);

    let keys = EventControllerKey::new();
    let weak = dialog.downgrade();
    keys.connect_key_pressed(move |controller, key, _, mods| {
        if has_modifiers(mods) {
            return Propagation::Proceed;
        }
        let step = match key {
            Key::space | Key::Escape => {
                if let Some(w) = controller.widget() {
                    w.downcast_ref::<Window>().unwrap().close();
                }
                return Propagation::Stop;
            }
            Key::Left | Key::Up => -1,
            Key::Right | Key::Down => 1,
            _ => return Propagation::Proceed,
        };
        if let Some(dialog) = weak.upgrade() {
            move_selection(&dialog, step);
        }
        Propagation::Stop
    });
    window.add_controller(keys);
    let q = quick_look.clone();
    window.connect_close_request(move |_| {
        q.borrow_mut().take();
        Propagation::Proceed
    });
    *quick_look.borrow_mut() = Some((window.clone(), view));
    window.present();
}

/// Fits the image in 80 % of the monitor, leaving room for the labels.
fn quick_look_size(dialog: &FileChooserDialog, texture: Option<&Texture>) -> (i32, i32) {
    let (mw, mh) = dialog
        .surface()
        .and_then(|s| dialog.display().monitor_at_surface(&s))
        .map(|m| (m.geometry().width() as f64, m.geometry().height() as f64))
        .unwrap_or((1280.0, 800.0));
    let Some(t) = texture else {
        return (480, 420);
    };
    let (labels, margins) = (110.0, 48.0);
    let scale = ((mw * 0.8 - margins) / t.width() as f64)
        .min((mh * 0.85 - labels - margins) / t.height() as f64)
        .min(1.0);
    let width = t.width() as f64 * scale + margins;
    let height = t.height() as f64 * scale + labels + margins;
    ((width as i32).max(480), (height as i32).max(360))
}

/// The view that shows the files: a grid view, or the list view inside the column view.
fn file_list(widget: &Widget) -> Option<Widget> {
    if (widget.is::<GridView>() || widget.is::<ListView>()) && widget.is_mapped() {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(list) = file_list(&c) {
            return Some(list);
        }
        child = c.next_sibling();
    }
    None
}

fn focus_in_file_list(dialog: &FileChooserDialog) -> bool {
    let (Some(focus), Some(list)) = (GtkWindowExt::focus(dialog), file_list(dialog.upcast_ref()))
    else {
        return false;
    };
    focus == list || focus.is_ancestor(&list)
}

fn move_selection(dialog: &FileChooserDialog, step: i64) {
    let Some(list) = file_list(dialog.upcast_ref()) else {
        return;
    };
    let model = match list.downcast_ref::<GridView>() {
        Some(grid) => grid.model(),
        None => list.downcast_ref::<ListView>().and_then(|l| l.model()),
    };
    let Some(model) = model else {
        return;
    };
    let n = model.n_items() as i64;
    let selection = model.selection();
    let pos = match selection.is_empty() {
        true => -1,
        false => selection.minimum() as i64,
    };
    let next = (pos + step).clamp(0, n - 1);
    if n == 0 || next == pos {
        return;
    }
    let next = next as u32;
    let _ = list.activate_action("list.select-item", Some(&(next, false, false).to_variant()));
    let _ = list.activate_action("list.scroll-to-item", Some(&next.to_variant()));
}

fn load_preview(uri: &str) -> PreviewData {
    let file = File::for_uri(uri);
    let info = file
        .query_info(
            "standard::display-name,standard::content-type,standard::size,standard::type",
            FileQueryInfoFlags::NONE,
            Cancellable::NONE,
        )
        .ok();
    let name = match &info {
        Some(i) => i.display_name().to_string(),
        None => file
            .basename()
            .map(|b| b.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let content_type = info
        .as_ref()
        .and_then(|i| i.content_type())
        .map(|c| c.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let size = info
        .as_ref()
        .filter(|i| i.file_type() != FileType::Directory)
        .map(|i| i.size() as u64);
    let texture = match size {
        Some(s) if s <= MAX_BYTES && content_type.starts_with("image/") => {
            Texture::from_file(&file).ok()
        }
        _ => None,
    };
    PreviewData {
        name,
        content_type,
        size,
        texture,
    }
}
