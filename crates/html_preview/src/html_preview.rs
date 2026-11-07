//! A small viewer for HTML files (ATG, ERP-2147).
//!
//! Opening an `.html` / `.htm` file shows the rendered page in the pane where the file was opened, instead of its
//! text. The page is drawn by the system's WebView2. Nothing is started until a page is opened, and everything is
//! released when its tab is closed, so the memory cost exists only while a page is open.
//!
//! Safety: clicking a link to a website, opening a new window and downloading are all refused, there are no developer
//! tools, and the browsing profile is private and never saved. The page itself still runs in the engine's sandbox
//! and, like any page opened from disk, can load images or scripts it names itself; it cannot read other local files.
//!
//! Known limits: while the page has the keyboard, Zed's own shortcuts do not see the keys (click Zed to get them
//! back), and a Zed menu or popup that overlaps the page is drawn underneath it, because the page is a window of
//! its own that floats over the tab (see `page.rs` for why).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use editor::Editor;
use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, Global, IntoElement, ParentElement, Pixels,
    Render, SharedString, Styled, Window, actions, canvas, div,
};
use ui::prelude::*;
use workspace::item::Item;
use workspace::{OpenOptions, Workspace};

mod page;

actions!(
    html_preview,
    [
        /// Opens the text of the HTML page that is being shown, for editing.
        EditSource
    ]
);

/// Files the user asked to see as text, so that opening them does not turn them back into a page.
#[derive(Default)]
struct SourceRequested(HashSet<PathBuf>);

impl Global for SourceRequested {}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        HtmlPreviewView::register(workspace, window, cx);
    })
    .detach();
}

pub struct HtmlPreviewView {
    focus_handle: FocusHandle,
    path: PathBuf,
    page: Option<page::Page>,
    error: Option<SharedString>,
}

impl HtmlPreviewView {
    fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            path,
            page: None,
            error: None,
        }
    }

    fn is_html_path(path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm"))
    }

    fn local_html_path(editor: &Entity<Editor>, cx: &App) -> Option<PathBuf> {
        let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
        let file = buffer.read(cx).file()?.as_local()?;
        let path = file.abs_path(cx);
        Self::is_html_path(&path).then_some(path)
    }

    fn register(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        cx.default_global::<SourceRequested>();
        let workspace_entity = cx.entity();
        cx.subscribe_in(
            &workspace_entity,
            window,
            |workspace, _, event: &workspace::Event, window, cx| {
                let workspace::Event::ItemAdded { item } = event else {
                    return;
                };
                if workspace.is_restoring() || item.is_dirty(cx) {
                    return;
                }
                let Some(editor) = item.downcast::<Editor>() else {
                    return;
                };
                let Some(path) = Self::local_html_path(&editor, cx) else {
                    return;
                };
                if cx.global_mut::<SourceRequested>().0.remove(&path) {
                    return;
                }
                let Some(pane) = workspace.pane_for_item_id(item.item_id()) else {
                    return;
                };
                let activate = workspace.active_pane() == &pane
                    && pane
                        .read(cx)
                        .active_item()
                        .is_some_and(|active_item| active_item.item_id() == item.item_id());
                let view = cx.new(|cx| Self::new(path, cx));
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(view), activate, activate, None, window, cx);
                    pane.remove_item(item.item_id(), false, false, window, cx);
                });
            },
        )
        .detach();

        workspace.register_action(|workspace, _: &EditSource, window, cx| {
            let Some(view) = workspace.active_item_as::<Self>(cx) else {
                return;
            };
            let path = view.read(cx).path.clone();
            cx.global_mut::<SourceRequested>().0.insert(path.clone());
            workspace
                .open_abs_path(path, OpenOptions::default(), window, cx)
                .detach_and_log_err(cx);
        });
    }

    /// Called every time the page area is drawn: the first call starts the page, later calls keep it in place.
    fn place(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let scale = window.scale_factor();
        if let Some(page) = self.page.as_mut() {
            page.place(bounds, scale);
            return;
        }
        if self.error.is_some() {
            return;
        }
        match page::Page::open(window, &self.path, bounds, scale) {
            Ok(page) => self.page = Some(page),
            Err(error) => {
                log::error!("html preview: could not show {:?}: {error:#}", self.path);
                self.error = Some("This page could not be shown.".into());
                cx.notify();
            }
        }
    }

    fn hide(&mut self) {
        if let Some(page) = self.page.as_mut() {
            page.hide();
        }
    }
}

impl Render for HtmlPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        v_flex()
            .id("HtmlPreview")
            .key_context("HtmlPreview")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .map(|container| match self.error.clone() {
                Some(error) => container.child(div().p_4().child(error)),
                None => container.child(
                    canvas(
                        |bounds, _, _| bounds,
                        move |bounds, _, window, cx| {
                            this.update(cx, |view, cx| view.place(bounds, window, cx)).ok();
                        },
                    )
                    .size_full(),
                ),
            })
    }
}

impl Focusable for HtmlPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for HtmlPreviewView {}

impl Item for HtmlPreviewView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileGeneric))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned().into())
            .unwrap_or_else(|| "HTML page".into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    // A page is a native window laid over Zed, so it must be hidden whenever its tab or workspace is put away.
    fn deactivated(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.hide();
    }

    fn workspace_deactivated(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.hide();
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(workspace::item::ItemEvent)) {}
}
