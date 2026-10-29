use {
    crate::{gui::UiProxy, utils::file_chooser_ext::FileChooserExtManualFixed},
    async_channel::{Receiver, Sender},
    gdk4_wayland::WaylandToplevel,
    gtk4::{
        gdk::{prelude::TextureExt, Texture},
        gio::{self, Cancellable, File, FileQueryInfoFlags, FileType},
        glib::{self, ControlFlow, MainContext},
        pango::WrapMode,
        prelude::{
            BoxExt, Cast, DialogExt, FileChooserExt, FileChooserExtManual, FileExt, GtkWindowExt,
            NativeExt, ObjectExt, OrientableExt, RecentManagerExt, WidgetExt,
        },
        FileChooserAction, FileChooserDialog, FileFilter, Image, Justification, Label,
        Orientation, Picture, PolicyType, RecentData, RecentManager, ResponseType,
        ScrolledWindow, Separator, Widget, Window,
    },
    rust_i18n::t,
    std::{
        cell::{Cell, RefCell},
        collections::{HashMap, HashSet},
        rc::Rc,
        time::Duration,
    },
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum FileChooserError {
    #[error("Operation could not be started")]
    Closed,
    #[error("Operation was rejected")]
    Rejected,
}

#[derive(Eq, PartialEq, Clone)]
pub struct Filter {
    pub name: String,
    pub elements: Vec<FilterKind>,
}

#[derive(Eq, PartialEq, Clone)]
pub enum FilterKind {
    Glob(String),
    Mime(String),
}

pub struct Choice {
    pub id: String,
    pub label: String,
    pub default: String,
    pub variants: Vec<ChoiceVariant>,
}

pub struct ChoiceVariant {
    pub id: String,
    pub label: String,
}

pub struct FinalChoice {
    pub id: String,
    pub variant_id: String,
}

pub struct FileChooserUi {
    pub title: String,
    pub multiple: bool,
    pub accept_label: Option<String>,
    pub modal: bool,
    pub directory: bool,
    pub filters: Option<Vec<Filter>>,
    pub current_filter: Option<Filter>,
    pub current_name: Option<String>,
    pub current_folder: Option<String>,
    pub current_filename: Option<String>,
    pub choices: Option<Vec<Choice>>,
    pub save: bool,
    pub parent_window: String,
    pub app_id: String,
}

pub struct FileChooserResult {
    pub uris: Vec<String>,
    pub current_filter: Option<Filter>,
    pub final_choices: Option<Vec<FinalChoice>>,
    pub writeable: bool,
}

struct DialogData {
    dialog: FileChooserDialog,
    read_only_choice: String,
    filters: HashMap<FileFilter, Filter>,
}

impl FileChooserUi {
    pub async fn run(self, proxy: &UiProxy) -> Result<FileChooserResult, FileChooserError> {
        let (send, recv) = async_channel::bounded(1);
        let (_send, close_on_close) = async_channel::bounded(1);
        let context = proxy.context.clone();
        proxy
            .context
            .invoke(move || self.run_impl(send, context, close_on_close));
        recv.recv().await.map_err(|_| FileChooserError::Closed)?
    }

    fn run_impl(
        self,
        send: Sender<Result<FileChooserResult, FileChooserError>>,
        context: MainContext,
        close_on_close: Receiver<()>,
    ) {
        let DialogData {
            dialog,
            read_only_choice,
            filters,
        } = self.build_dialog();
        let current_filter = Rc::new(Cell::new(dialog.filter()));
        let cf = current_filter.clone();
        dialog.connect_filter_notify(move |f| cf.set(f.filter()));
        let cf = current_filter.clone();
        dialog.connect_response(move |dialog, r| {
            let res = match r {
                ResponseType::Ok => {
                    let files: Vec<_> = dialog
                        .files()
                        .into_iter()
                        .map(|f| f.unwrap().downcast::<File>().unwrap().uri().into())
                        .collect();
                    add_recent(&self.app_id, &files);
                    let filter = cf.take().and_then(|f| filters.get(&f).cloned());
                    let choices: Vec<_> = self
                        .choices
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .flat_map(|c| {
                            dialog.choice(&c.id).map(|v| FinalChoice {
                                id: c.id.to_string(),
                                variant_id: v.to_string(),
                            })
                        })
                        .collect();
                    let writeable = dialog
                        .choice(&read_only_choice)
                        .map(|v| v == "false")
                        .unwrap_or(false);
                    Ok(FileChooserResult {
                        uris: files,
                        current_filter: filter,
                        final_choices: self.choices.is_some().then_some(choices),
                        writeable,
                    })
                }
                _ => Err(FileChooserError::Rejected),
            };
            let _ = send.send_blocking(res);
            dialog.close();
        });
        if dialog.action() == FileChooserAction::Open {
            add_preview(&dialog);
        }
        dialog.show();
        context.spawn_local(async move {
            let _ = close_on_close.recv().await;
            dialog.close();
        });
    }

    fn build_dialog(&self) -> DialogData {
        let action = match (self.directory, self.save) {
            (true, _) => FileChooserAction::SelectFolder,
            (_, true) => FileChooserAction::Save,
            (false, _) => FileChooserAction::Open,
        };
        let accept_label = match self.save {
            true => t!("_Save"),
            false => t!("_Open"),
        };
        let buttons = [
            (
                self.accept_label.as_deref().unwrap_or(&accept_label),
                ResponseType::Ok,
            ),
            (&t!("_Cancel"), ResponseType::Cancel),
        ];
        let dialog =
            FileChooserDialog::new(Some(self.title.clone()), Window::NONE, action, &buttons);
        dialog.set_select_multiple(self.multiple);
        dialog.set_modal(self.modal);
        dialog.set_default_response(ResponseType::Ok);
        let mut filters_map = HashMap::new();
        if let Some(f) = &self.filters {
            for filter in f {
                let is_current = self.current_filter.as_ref() == Some(filter);
                let f = map_filter(filter);
                dialog.add_filter(&f);
                if is_current {
                    dialog.set_filter(&f);
                }
                filters_map.insert(f, filter.clone());
            }
        }
        if let Some(f) = &self.current_name {
            dialog.set_current_name(f);
        }
        if let Some(f) = &self.current_folder {
            let _ = dialog.set_current_folder(Some(&File::for_path(f)));
        }
        if let Some(f) = &self.current_filename {
            let _ = dialog.set_file(&File::for_uri(f));
        }
        let mut read_only_id = String::new();
        if action == FileChooserAction::Open {
            let choice_ids: HashSet<_> = self
                .choices
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|c| c.id.as_str())
                .collect();
            read_only_id = "_read_only".to_string();
            while choice_ids.contains(read_only_id.as_str()) {
                read_only_id.push('_');
            }
            dialog.add_choice_fixed(&read_only_id, t!("Open files read-only").as_ref(), &[]);
            dialog.set_choice(&read_only_id, "true");
        }
        if let Some(choices) = &self.choices {
            for choice in choices {
                let mut variants = vec![];
                for variant in &choice.variants {
                    variants.push((variant.id.as_str(), variant.label.as_str()));
                }
                dialog.add_choice_fixed(&choice.id, &choice.label, &variants);
                dialog.set_choice(&choice.id, &choice.default);
            }
        }
        dialog.upcast_ref::<Widget>().realize();
        if let Some(parent) = self.parent_window.strip_prefix("wayland:") {
            if let Some(surface) = dialog.surface() {
                if let Some(toplevel) = surface.downcast_ref::<WaylandToplevel>() {
                    toplevel.set_transient_for_exported(parent);
                }
            }
        }
        DialogData {
            dialog,
            read_only_choice: read_only_id,
            filters: filters_map,
        }
    }
}

fn map_filter(f: &Filter) -> FileFilter {
    let gf = FileFilter::new();
    gf.set_name(Some(&f.name));
    for kind in &f.elements {
        match kind {
            FilterKind::Glob(g) => gf.add_pattern(g),
            FilterKind::Mime(m) => gf.add_mime_type(m),
        }
    }
    gf
}

const PREVIEW_WIDTH: i32 = 320;
// Images bigger than this are not decoded for the preview.
const PREVIEW_MAX_BYTES: u64 = 64 * 1024 * 1024;

struct PreviewData {
    name: String,
    content_type: String,
    size: Option<u64>,
    texture: Option<Texture>,
}

/// Adds a panel on the right that shows the selected file, like the preview column of the
/// macOS Finder. GTK 4 dropped the preview widget and has no selection signal, so the
/// selection is polled.
fn add_preview(dialog: &FileChooserDialog) {
    let picture = Picture::new();
    picture.set_can_shrink(true);
    picture.set_vexpand(true);
    let icon = Image::new();
    icon.set_pixel_size(128);
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
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&picture);
    content.append(&icon);
    content.append(&name);
    content.append(&details);
    content.set_visible(false);
    // The scrolled window keeps the natural size of big images from widening the panel.
    let panel = ScrolledWindow::new();
    panel.set_policy(PolicyType::Never, PolicyType::Never);
    panel.set_propagate_natural_width(false);
    panel.set_size_request(PREVIEW_WIDTH, -1);
    panel.set_child(Some(&content));
    let area = dialog.content_area();
    area.set_orientation(Orientation::Horizontal);
    area.append(&Separator::new(Orientation::Vertical));
    area.append(&panel);

    let shown = Rc::new(RefCell::new(None::<String>));
    let weak = dialog.downgrade();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        let Some(dialog) = weak.upgrade() else {
            return ControlFlow::Break;
        };
        let file = dialog.file();
        let uri = file.as_ref().map(|f| f.uri().to_string());
        if *shown.borrow() == uri {
            return ControlFlow::Continue;
        }
        *shown.borrow_mut() = uri.clone();
        let Some(uri) = uri else {
            content.set_visible(false);
            return ControlFlow::Continue;
        };
        let (content, picture, icon, name, details, shown) = (
            content.clone(),
            picture.clone(),
            icon.clone(),
            name.clone(),
            details.clone(),
            shown.clone(),
        );
        glib::spawn_future_local(async move {
            let u = uri.clone();
            let Ok(data) = gio::spawn_blocking(move || load_preview(&u)).await else {
                return;
            };
            // The selection changed while the file was loading.
            if shown.borrow().as_deref() != Some(uri.as_str()) {
                return;
            }
            let mut lines = vec![gio::content_type_get_description(&data.content_type).to_string()];
            if let Some(t) = &data.texture {
                lines.push(format!("{} × {}", t.width(), t.height()));
            }
            if let Some(s) = data.size {
                lines.push(glib::format_size(s).to_string());
            }
            match &data.texture {
                Some(t) => picture.set_paintable(Some(t)),
                None => icon.set_from_gicon(&gio::content_type_get_icon(&data.content_type)),
            }
            picture.set_visible(data.texture.is_some());
            icon.set_visible(data.texture.is_none());
            name.set_label(&data.name);
            details.set_label(&lines.join("\n"));
            content.set_visible(true);
        });
        ControlFlow::Continue
    });
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
        Some(s) if s <= PREVIEW_MAX_BYTES && content_type.starts_with("image/") => {
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

fn add_recent(app_id: &str, uris: &[String]) {
    let manager = RecentManager::default();
    for uri in uris {
        manager.add_full(
            uri,
            &RecentData::new(
                None,
                None,
                "application/octet-stream",
                app_id,
                "false",
                &[],
                false,
            ),
        );
    }
}
