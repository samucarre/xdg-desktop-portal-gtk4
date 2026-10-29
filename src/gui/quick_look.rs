//! Quick Look for the open dialog, like the macOS Finder: with the focus in the file list,
//! space opens a bigger preview of the selected file, space or escape closes it and the
//! arrows move through the files while it is open.
//!
//! GTK 4 has no selection signal, so the selection is polled while the preview is open, and
//! files are loaded off the main thread.

use {
    gtk4::{
        gdk::{
            prelude::{SurfaceExt, TextureExt},
            Key, ModifierType, Texture,
        },
        gio::{self, Cancellable, File, FileQueryInfoFlags, FileType},
        glib::{self, ControlFlow, Propagation},
        pango::WrapMode,
        prelude::{
            BoxExt, Cast, DisplayExt, EventControllerExt, FileChooserExt, FileExt, GtkWindowExt,
            IsA, ListModelExt, MonitorExt, NativeExt, ObjectExt, SelectionModelExt, ToVariant,
            WidgetExt,
        },
        ContentFit, EventControllerKey, FileChooserDialog, GridView, Image, Justification, Label,
        ListView, Native, Orientation, Picture, PropagationPhase, Widget, Window,
    },
    std::{cell::RefCell, rc::Rc, time::Duration},
};

// Images bigger than this are not decoded for the preview.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

struct PreviewData {
    name: String,
    content_type: String,
    size: Option<u64>,
    texture: Option<Texture>,
}

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
    fn new() -> Self {
        let picture = Picture::new();
        picture.set_can_shrink(true);
        // Small images keep their size instead of being blown up.
        picture.set_content_fit(ContentFit::ScaleDown);
        picture.set_vexpand(true);
        let icon = Image::new();
        icon.set_pixel_size(256);
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
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        content.set_margin_start(24);
        content.set_margin_end(24);
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

struct QuickLookWindow {
    window: Window,
    view: PreviewView,
    uri: String,
}

type QuickLook = Rc<RefCell<Option<QuickLookWindow>>>;

pub fn add_quick_look(dialog: &FileChooserDialog) {
    let quick_look: QuickLook = Default::default();

    let keys = EventControllerKey::new();
    keys.set_propagation_phase(PropagationPhase::Capture);
    let weak = dialog.downgrade();
    let q = quick_look.clone();
    keys.connect_key_pressed(move |_, key, _, mods| {
        let Some(dialog) = weak.upgrade() else {
            return Propagation::Proceed;
        };
        // Only in the file list, so that space still works in the search and location entries.
        if key != Key::space || has_modifiers(mods) || !focus_in_file_list(&dialog) {
            return Propagation::Proceed;
        }
        toggle_quick_look(&dialog, &q);
        Propagation::Stop
    });
    dialog.add_controller(keys);

    // Follows the selection while the preview is open.
    let weak = dialog.downgrade();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        let Some(dialog) = weak.upgrade() else {
            return ControlFlow::Break;
        };
        let Some(uri) = dialog.file().map(|f| f.uri().to_string()) else {
            return ControlFlow::Continue;
        };
        match quick_look.borrow_mut().as_mut() {
            Some(q) if q.uri != uri => q.uri = uri.clone(),
            _ => return ControlFlow::Continue,
        }
        let quick_look = quick_look.clone();
        glib::spawn_future_local(async move {
            let Some(shown) = load(uri.clone()).await else {
                return;
            };
            // The selection may have moved on while the file was loading.
            if let Some(q) = &*quick_look.borrow() {
                if q.uri == uri {
                    q.window.set_title(Some(&shown.name));
                    q.view.show(&shown);
                }
            }
        });
        ControlFlow::Continue
    });
}

async fn load(uri: String) -> Option<Shown> {
    gio::spawn_blocking(move || load_preview(&uri))
        .await
        .ok()
        .map(shown)
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

fn toggle_quick_look(dialog: &FileChooserDialog, quick_look: &QuickLook) {
    let open = quick_look.borrow_mut().take();
    if let Some(q) = open {
        q.window.close();
        return;
    }
    let Some(uri) = dialog.file().map(|f| f.uri().to_string()) else {
        return;
    };
    let (weak, quick_look) = (dialog.downgrade(), quick_look.clone());
    glib::spawn_future_local(async move {
        let Some(shown) = load(uri.clone()).await else {
            return;
        };
        if quick_look.borrow().is_some() {
            return;
        }
        if let Some(dialog) = weak.upgrade() {
            open_quick_look(&dialog, &shown, uri, &quick_look);
        }
    });
}

fn open_quick_look(dialog: &FileChooserDialog, shown: &Shown, uri: String, quick_look: &QuickLook) {
    let view = PreviewView::new();
    let window = Window::new();
    window.set_transient_for(Some(dialog));
    window.set_destroy_with_parent(true);
    window.set_title(Some(&shown.name));
    window.set_child(Some(&view.content));
    let (width, height) = quick_look_size(dialog);
    window.set_default_size(width, height);
    view.show(shown);

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
    *quick_look.borrow_mut() = Some(QuickLookWindow {
        window: window.clone(),
        view,
        uri,
    });
    window.present();
}

/// A fixed share of the monitor. Each image is fitted inside it: Wayland clients cannot move
/// their windows, so a window that took the shape of each image would drift off centre (and
/// off the screen) while moving through the files.
fn quick_look_size(native: &impl IsA<Native>) -> (i32, i32) {
    let (mw, mh) = native
        .surface()
        .and_then(|s| s.display().monitor_at_surface(&s))
        .map(|m| (m.geometry().width() as f64, m.geometry().height() as f64))
        .unwrap_or((1280.0, 800.0));
    (((mw * 0.7) as i32).max(480), ((mh * 0.8) as i32).max(360))
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
