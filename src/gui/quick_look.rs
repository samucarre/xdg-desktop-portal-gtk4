//! Quick Look for the open dialog, like the macOS Finder: with the focus in the file list,
//! space opens a bigger preview of the selected file, space or escape closes it and the
//! arrows move through the files while it is open. When several files are selected, the
//! arrows move through those only and the selection is kept.
//!
//! GtkFileChooser has no selection signal of its own; while the preview is open it listens
//! to the selection model of the dialog's file list. Files are decoded off the main thread.

use {
    gtk4::{
        gdk::{
            prelude::{SurfaceExt, TextureExt},
            Key, ModifierType, Texture,
        },
        gdk_pixbuf::Pixbuf,
        gio::{self, Cancellable, File, FileInfo, FileQueryInfoFlags, FileType},
        glib::{self, Propagation, SignalHandlerId},
        pango::WrapMode,
        prelude::{
            BoxExt, Cast, CastNone, DisplayExt, EventControllerExt, FileExt, GtkWindowExt, IsA,
            ListModelExt, MonitorExt, NativeExt, ObjectExt, SelectionModelExt, ToVariant,
            WidgetExt,
        },
        ContentFit, EventControllerKey, FileChooserDialog, GridView, Image, Justification, Label,
        ListScrollFlags, ListView, Native, Orientation, Picture, PropagationPhase, SelectionModel,
        Widget, Window,
    },
    std::{
        cell::RefCell,
        io::Read,
        path::Path,
        rc::{Rc, Weak},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    },
};

// Files bigger than this are never decoded.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
// A small, well compressed file can expand to gigabytes. Images with more pixels than this are
// decoded scaled down (the decoder may still need the whole image for a moment)...
const SCALE_ABOVE_PIXELS: i64 = 40_000_000;
const SCALED_SIDE: i32 = 4096;
// ...and above this they are not decoded at all: the preview shows the type icon.
const MAX_PIXELS: i64 = 100_000_000;

struct PreviewData {
    name: String,
    content_type: String,
    size: Option<u64>,
    dimensions: Option<(i32, i32)>,
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
        // Small images are not blown up to fill the window.
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
    }
}

struct Open {
    window: Window,
    view: PreviewView,
    uri: String,
    position: u32,
    model: SelectionModel,
    handlers: Vec<SignalHandlerId>,
    // With several files selected: their positions, browsed without touching the selection.
    selected: Vec<u32>,
}

#[derive(Default)]
struct State {
    open: Option<Open>,
    // Id of the load that will open the window, 0 when none is pending.
    opening: u64,
    // Bumped by every load; a load that is no longer the latest stops before decoding.
    generation: Arc<AtomicU64>,
    // Where to put the dialog's cursor once it is active again (after the preview closed).
    refocus: Option<u32>,
}

type QuickLook = Rc<RefCell<State>>;

pub fn add_quick_look(dialog: &FileChooserDialog) {
    let state: QuickLook = Default::default();

    let keys = EventControllerKey::new();
    keys.set_propagation_phase(PropagationPhase::Capture);
    let weak = dialog.downgrade();
    let s = state.clone();
    keys.connect_key_pressed(move |_, key, _, mods| {
        let Some(dialog) = weak.upgrade() else {
            return Propagation::Proceed;
        };
        // Only in the file list, so that space still works in the search and location entries.
        if key != Key::space || has_modifiers(mods) || !focus_in_file_list(&dialog) {
            return Propagation::Proceed;
        }
        toggle_quick_look(&dialog, &s);
        Propagation::Stop
    });
    dialog.add_controller(keys);

    // The preview goes away with the dialog (Open, Cancel, a double click...).
    let s = state.clone();
    dialog.connect_unmap(move |_| {
        let open = s.borrow_mut().open.take();
        if let Some(open) = open {
            close(open);
        }
        s.borrow_mut().refocus = None;
    });

    // The arrows in the preview move the selection, not the list's cursor: once the dialog is
    // active again, the cursor goes to the file that was on screen, so that Enter opens it.
    let s = state.clone();
    dialog.connect_is_active_notify(move |dialog| {
        if !dialog.is_active() {
            return;
        }
        let pos = s.borrow_mut().refocus.take();
        if let Some(pos) = pos {
            let weak = dialog.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(dialog) = weak.upgrade() {
                    focus_position(&dialog, pos);
                }
            });
        }
    });
}

fn toggle_quick_look(dialog: &FileChooserDialog, state: &QuickLook) {
    let open = state.borrow_mut().open.take();
    if let Some(open) = open {
        let pos = open.position;
        close(open);
        focus_position(dialog, pos);
        return;
    }
    let mut st = state.borrow_mut();
    if st.opening != 0 {
        // Space again while the file was still loading: forget it.
        st.opening = 0;
        st.generation.fetch_add(1, Ordering::SeqCst);
        return;
    }
    let Some(model) = file_list(dialog.upcast_ref()).and_then(|l| list_model(&l)) else {
        return;
    };
    let selection = model.selection();
    if selection.is_empty() {
        return;
    }
    let position = selection.minimum();
    let selected = match selection.size() {
        1 => vec![],
        n => (0..n as u32).map(|i| selection.nth(i)).collect(),
    };
    let Some(file) = file_at(&model, position) else {
        return;
    };
    let uri = file.uri().to_string();
    let generation = st.generation.clone();
    let id = generation.fetch_add(1, Ordering::SeqCst) + 1;
    st.opening = id;
    drop(st);
    let (weak, state) = (dialog.downgrade(), Rc::downgrade(state));
    glib::spawn_future_local(async move {
        let shown = load(uri.clone(), generation, id).await;
        let Some(state) = state.upgrade() else {
            return;
        };
        if state.borrow().opening != id {
            return;
        }
        state.borrow_mut().opening = 0;
        let (Some(shown), Some(dialog)) = (shown, weak.upgrade()) else {
            return;
        };
        if dialog.is_visible() {
            open_quick_look(&dialog, &shown, uri, position, model, selected, &state);
        }
    });
}

fn open_quick_look(
    dialog: &FileChooserDialog,
    shown: &Shown,
    uri: String,
    position: u32,
    model: SelectionModel,
    selected: Vec<u32>,
    state: &QuickLook,
) {
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
    let (weak, s) = (dialog.downgrade(), Rc::downgrade(state));
    keys.connect_key_pressed(move |controller, key, _, mods| {
        if has_modifiers(mods) {
            return Propagation::Proceed;
        }
        let step = match key {
            Key::space | Key::Escape => {
                if let Some(w) = controller.widget().and_downcast::<Window>() {
                    w.close();
                }
                return Propagation::Stop;
            }
            Key::Left | Key::Up => -1,
            Key::Right | Key::Down => 1,
            _ => return Propagation::Proceed,
        };
        if let (Some(dialog), Some(state)) = (weak.upgrade(), s.upgrade()) {
            move_through_files(&dialog, &state, step);
        }
        Propagation::Stop
    });
    window.add_controller(keys);

    // Closed from the preview itself (space, escape, the compositor): weak, or the window
    // and this closure would keep each other alive.
    let s = Rc::downgrade(state);
    window.connect_close_request(move |_| {
        if let Some(state) = s.upgrade() {
            let open = state.borrow_mut().open.take();
            if let Some(mut open) = open {
                state.borrow_mut().refocus = Some(open.position);
                disconnect(&mut open);
            }
        }
        Propagation::Proceed
    });

    // Follow the selection when it changes in the dialog (a click, or the arrows below).
    let follow = {
        let s = Rc::downgrade(state);
        move || {
            if let Some(state) = s.upgrade() {
                follow_selection(&state);
            }
        }
    };
    let f = follow.clone();
    let handlers = vec![
        model.connect_selection_changed(move |_, _, _| f()),
        model.connect_items_changed(move |_, _, _, _| follow()),
    ];
    state.borrow_mut().open = Some(Open {
        window: window.clone(),
        view,
        uri,
        position,
        model,
        handlers,
        selected,
    });
    window.present();
}

fn disconnect(open: &mut Open) {
    for id in open.handlers.drain(..) {
        open.model.disconnect(id);
    }
}

fn close(mut open: Open) {
    disconnect(&mut open);
    // Its close-request finds no Open in the state any more and does nothing.
    open.window.close();
}

fn follow_selection(state: &QuickLook) {
    let Some(model) = state.borrow().open.as_ref().map(|o| o.model.clone()) else {
        return;
    };
    let selection = model.selection();
    if selection.is_empty() {
        // The folder changed, or the files went away: nothing to show.
        let open = state.borrow_mut().open.take();
        if let Some(open) = open {
            close(open);
        }
        return;
    }
    let mut st = state.borrow_mut();
    let Some(open) = st.open.as_mut() else {
        return;
    };
    if selection.size() > 1 {
        let selected: Vec<u32> = (0..selection.size() as u32)
            .map(|i| selection.nth(i))
            .collect();
        if !selected.contains(&open.position) {
            open.position = selected[0];
        }
        open.selected = selected;
    } else {
        open.selected.clear();
        open.position = selection.minimum();
    }
    let position = open.position;
    drop(st);
    if let Some(file) = file_at(&model, position) {
        preview(state, file);
    }
}

fn move_through_files(dialog: &FileChooserDialog, state: &QuickLook, step: i64) {
    let mut st = state.borrow_mut();
    let Some(open) = st.open.as_mut() else {
        return;
    };
    if !open.selected.is_empty() {
        let index = open
            .selected
            .iter()
            .position(|p| *p == open.position)
            .unwrap_or(0) as i64;
        let next = (index + step).clamp(0, open.selected.len() as i64 - 1) as usize;
        let position = open.selected[next];
        if position == open.position {
            return;
        }
        open.position = position;
        let model = open.model.clone();
        drop(st);
        if let Some(file) = file_at(&model, position) {
            preview(state, file);
        }
        if let Some(list) = file_list(dialog.upcast_ref()) {
            let _ = list.activate_action("list.scroll-to-item", Some(&position.to_variant()));
        }
        return;
    }
    drop(st);
    // One file: move the selection itself; the preview follows it (follow_selection).
    let Some(list) = file_list(dialog.upcast_ref()) else {
        return;
    };
    let Some(model) = list_model(&list) else {
        return;
    };
    let n = model.n_items() as i64;
    if n == 0 {
        return;
    }
    let selection = model.selection();
    let pos = match selection.is_empty() {
        true => -1,
        false => selection.minimum() as i64,
    };
    let next = (pos + step).clamp(0, n - 1);
    if next == pos {
        return;
    }
    let next = next as u32;
    let _ = list.activate_action("list.select-item", Some(&(next, false, false).to_variant()));
    let _ = list.activate_action("list.scroll-to-item", Some(&next.to_variant()));
}

/// Shows that file in the open preview, unless it is already there.
fn preview(state: &QuickLook, file: File) {
    let uri = file.uri().to_string();
    let generation = {
        let mut st = state.borrow_mut();
        let Some(open) = st.open.as_mut() else {
            return;
        };
        if open.uri == uri {
            return;
        }
        open.uri = uri.clone();
        st.generation.clone()
    };
    let id = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let state = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        let Some(shown) = load(uri.clone(), generation, id).await else {
            return;
        };
        let Some(state) = Weak::upgrade(&state) else {
            return;
        };
        let st = state.borrow();
        // The preview may have moved on, or closed, while the file was loading.
        if let Some(open) = st.open.as_ref().filter(|o| o.uri == uri) {
            open.window.set_title(Some(&shown.name));
            open.view.show(&shown);
        }
    });
}

async fn load(uri: String, generation: Arc<AtomicU64>, id: u64) -> Option<Shown> {
    gio::spawn_blocking(move || load_preview(&uri, &generation, id))
        .await
        .ok()
        .flatten()
        .map(shown)
}

fn shown(data: PreviewData) -> Shown {
    let mut lines = vec![gio::content_type_get_description(&data.content_type).to_string()];
    if let Some((w, h)) = data.dimensions {
        lines.push(format!("{w} × {h}"));
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

fn list_model(list: &Widget) -> Option<SelectionModel> {
    match list.downcast_ref::<GridView>() {
        Some(grid) => grid.model(),
        None => list.downcast_ref::<ListView>().and_then(|l| l.model()),
    }
}

/// The items of the file chooser's model are GFileInfo with the file in "standard::file".
fn file_at(model: &SelectionModel, position: u32) -> Option<File> {
    model
        .item(position)?
        .downcast::<FileInfo>()
        .ok()?
        .attribute_object("standard::file")?
        .downcast::<File>()
        .ok()
}

fn focus_in_file_list(dialog: &FileChooserDialog) -> bool {
    let (Some(focus), Some(list)) = (GtkWindowExt::focus(dialog), file_list(dialog.upcast_ref()))
    else {
        return false;
    };
    focus == list || focus.is_ancestor(&list)
}

/// Moves the list's cursor (keyboard focus) to that file without touching the selection.
fn focus_position(dialog: &FileChooserDialog, position: u32) {
    let Some(list) = file_list(dialog.upcast_ref()) else {
        return;
    };
    if list_model(&list).is_none_or(|m| position >= m.n_items()) {
        return;
    }
    if let Some(grid) = list.downcast_ref::<GridView>() {
        grid.scroll_to(position, ListScrollFlags::FOCUS, None);
    } else if let Some(view) = list.downcast_ref::<ListView>() {
        view.scroll_to(position, ListScrollFlags::FOCUS, None);
    }
}

fn load_preview(uri: &str, generation: &AtomicU64, id: u64) -> Option<PreviewData> {
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
    let mut dimensions = None;
    let mut texture = None;
    let image = content_type.starts_with("image/") && size.is_some_and(|s| s <= MAX_BYTES);
    // Another file was asked for in the meantime: skip the decoding.
    if image && generation.load(Ordering::SeqCst) == id {
        let path = file.path();
        let size_in_header = path.as_deref().and_then(image_size);
        let pixels = size_in_header.map(|(w, h)| w as i64 * h as i64);
        texture = match (&path, pixels) {
            (_, Some(p)) if p > MAX_PIXELS => None,
            (Some(path), Some(p)) if p > SCALE_ABOVE_PIXELS => {
                Pixbuf::from_file_at_scale(path, SCALED_SIDE, SCALED_SIDE, true)
                    .ok()
                    .map(|p| Texture::for_pixbuf(&p))
            }
            _ => Texture::from_file(&file).ok(),
        };
        dimensions = size_in_header.or_else(|| texture.as_ref().map(|t| (t.width(), t.height())));
    }
    Some(PreviewData {
        name,
        content_type,
        size,
        dimensions,
        texture,
    })
}

/// Width and height from the file header, without decoding. gdk-pixbuf (with glycin) decodes
/// a whole PNG just to tell its size, which is what the limits above must avoid; for JPEG it
/// only reads the header.
fn image_size(path: &Path) -> Option<(i32, i32)> {
    let mut buf = [0u8; 32];
    let n = std::fs::File::open(path).ok()?.read(&mut buf).ok()?;
    let h = &buf[..n];
    let u16le = |i: usize| {
        h.get(i..i + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as i32)
    };
    let u24le = |i: usize| {
        h.get(i..i + 3)
            .map(|b| b[0] as i32 | (b[1] as i32) << 8 | (b[2] as i32) << 16)
    };
    let u32be = |i: usize| {
        h.get(i..i + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]).min(i32::MAX as u32) as i32)
    };
    if h.starts_with(b"\x89PNG\r\n\x1a\n") && h.get(12..16) == Some(b"IHDR") {
        return Some((u32be(16)?, u32be(20)?));
    }
    if h.starts_with(b"GIF8") {
        return Some((u16le(6)?, u16le(8)?));
    }
    if h.starts_with(b"RIFF") && h.get(8..12) == Some(b"WEBP") {
        return match h.get(12..16)? {
            b"VP8X" => Some((u24le(24)? + 1, u24le(27)? + 1)),
            b"VP8 " => Some((u16le(26)? & 0x3fff, u16le(28)? & 0x3fff)),
            b"VP8L" => {
                let b = h.get(21..25)?;
                let bits = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                Some(((bits & 0x3fff) as i32 + 1, (bits >> 14 & 0x3fff) as i32 + 1))
            }
            _ => None,
        };
    }
    if h.starts_with(&[0xff, 0xd8]) {
        return Pixbuf::file_info(path).map(|(_, w, h)| (w, h));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::image_size;

    fn size_of(name: &str, bytes: &[u8]) -> Option<(i32, i32)> {
        let path = std::env::temp_dir().join(format!("quick-look-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let size = image_size(&path);
        std::fs::remove_file(&path).unwrap();
        size
    }

    fn riff(chunk: &[u8], body: &[u8]) -> Vec<u8> {
        let mut v = b"RIFF\0\0\0\0WEBP".to_vec();
        v.extend_from_slice(chunk);
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn png() {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        v.extend_from_slice(&16000u32.to_be_bytes());
        v.extend_from_slice(&9000u32.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(size_of("png", &v), Some((16000, 9000)));
    }

    #[test]
    fn gif() {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&640u16.to_le_bytes());
        v.extend_from_slice(&481u16.to_le_bytes());
        v.extend_from_slice(&[0; 8]);
        assert_eq!(size_of("gif", &v), Some((640, 481)));
    }

    #[test]
    fn webp() {
        // Extended (alpha, animation): canvas size minus one, 24 bits each.
        let mut x = vec![0x10, 0, 0, 0];
        x.extend_from_slice(&1999u32.to_le_bytes()[..3]);
        x.extend_from_slice(&998u32.to_le_bytes()[..3]);
        assert_eq!(size_of("vp8x", &riff(b"VP8X", &x)), Some((2000, 999)));
        // Lossy: frame tag, start code, 14-bit width and height.
        let mut l = vec![0, 0, 0, 0x9d, 0x01, 0x2a];
        l.extend_from_slice(&1234u16.to_le_bytes());
        l.extend_from_slice(&567u16.to_le_bytes());
        assert_eq!(size_of("vp8", &riff(b"VP8 ", &l)), Some((1234, 567)));
        // Lossless: signature, then width and height minus one in 14 bits each.
        let bits: u32 = 1499 | 800 << 14;
        let mut ll = vec![0x2f];
        ll.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(size_of("vp8l", &riff(b"VP8L", &ll)), Some((1500, 801)));
    }

    #[test]
    fn unknown_or_short() {
        assert_eq!(size_of("txt", b"just some text"), None);
        assert_eq!(size_of("short", b"\x89PNG"), None);
    }
}
