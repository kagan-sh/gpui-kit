//! Drag and drop for scripts: a text payload carried from one element to
//! another with GPUI's own drag.
//!
//! GPUI already owns the gesture. It starts the drag past its threshold, keeps
//! it while the pointer leaves the source, and hands the release to the
//! innermost element listening for a drop. What a script cannot get from it is
//! the rest of the conversation: which target the pointer is over and where on
//! it, that the drag ended without a drop, and Escape. This module adds those,
//! and nothing else.
//!
//! Handlers are looked up afresh on every delivery. A script re-renders when a
//! drag starts — that is how the source comes to look dragged — and the render
//! retires the callbacks the drag started with. So every source and target
//! files its current callbacks in an entry each prepaint, and a drag holds the
//! entries, never the callbacks.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gpui::{
    App, AppContext as _, Bounds, Context, Div, DragMoveEvent, ElementId, GlobalElementId,
    InteractiveElement as _, IntoElement, Pixels, Point, Render, SharedString, Stateful,
    StatefulInteractiveElement as _, Subscription, Window, WindowId,
};
use gpui_base::ElementExt as _;

use crate::engine::ShellRuntime;
use crate::spec::CallbackId;

/// An element a script made a drag source or a drop target, as it was last
/// painted.
///
/// What a host's UI driver needs to find one and press on it: plain elements
/// carry no role a driver could otherwise look for.
#[derive(Clone, Debug)]
pub struct DragElement {
    /// The element's global id, outermost first. The last part is the script's
    /// `id(...)` when it gave one.
    pub path: Vec<ElementId>,
    pub bounds: Bounds<Pixels>,
    /// `bounds` cut to what the window shows of it.
    pub clipped: Bounds<Pixels>,
    pub source: bool,
    pub target: bool,
}

/// Every drag source and drop target in the window's last frame.
pub fn drag_elements(window: &Window) -> Vec<DragElement> {
    let id = window.window_handle().window_id();
    ENTRIES.with(|entries| {
        entries
            .borrow()
            .get(&id)
            .map(|entries| {
                entries
                    .values()
                    .filter_map(Weak::upgrade)
                    .map(|entry| {
                        let handlers = entry.handlers.get();
                        DragElement {
                            path: entry.path.clone(),
                            bounds: entry.bounds.get(),
                            clipped: entry.clipped.get(),
                            source: handlers.on_drag.is_some(),
                            target: handlers.is_target(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// What a script asked of one element.
#[derive(Clone, Copy, Default)]
pub(crate) struct Handlers {
    pub(crate) on_drag: Option<CallbackId>,
    pub(crate) on_drag_move: Option<CallbackId>,
    pub(crate) on_drop: Option<CallbackId>,
}

impl Handlers {
    fn is_target(self) -> bool {
        self.on_drag_move.is_some() || self.on_drop.is_some()
    }
}

/// Where on a target the pointer is: its top quarter, its middle half, or its
/// bottom quarter. A list ranks above or below at the edges and nests in the
/// middle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Top,
    Middle,
    Bottom,
}

impl Side {
    fn of(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Self {
        let height = f32::from(bounds.size.height);
        if height <= 0. {
            return Self::Middle;
        }
        let fraction = f32::from(position.y - bounds.origin.y) / height;
        if fraction < 0.25 {
            Self::Top
        } else if fraction > 0.75 {
            Self::Bottom
        } else {
            Self::Middle
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Middle => "middle",
            Self::Bottom => "bottom",
        }
    }
}

struct Entry {
    path: Vec<ElementId>,
    runtime: Weak<ShellRuntime>,
    bounds: Cell<Bounds<Pixels>>,
    clipped: Cell<Bounds<Pixels>>,
    handlers: Cell<Handlers>,
}

thread_local! {
    /// Weak, because the entries are owned by GPUI element state: an element
    /// that is not painted in a frame loses its state, and its entry with it.
    static ENTRIES: RefCell<HashMap<WindowId, HashMap<GlobalElementId, Weak<Entry>>>> =
        RefCell::default();
}

/// The value GPUI carries for a script drag. Built every frame, like the rest
/// of the element; only the one a drag started from gets a session.
struct DragSource {
    payload: SharedString,
    entry: Rc<RefCell<Option<Rc<Entry>>>>,
    session: RefCell<Option<Rc<Session>>>,
}

struct Session {
    payload: SharedString,
    source: Weak<Entry>,
    runtime: Weak<ShellRuntime>,
    over: RefCell<Option<(Weak<Entry>, Side)>>,
    dropped: Cell<bool>,
    _escape: Subscription,
}

/// What GPUI draws under the pointer. Nothing: the script paints the source
/// and the target itself, from the events it hears.
struct Preview(Rc<Session>);

impl Render for Preview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Makes an element a drag source, a drop target, or both.
pub(crate) fn with_drag(
    element: Stateful<Div>,
    payload: Option<SharedString>,
    handlers: Handlers,
    runtime: &Rc<ShellRuntime>,
) -> Stateful<Div> {
    let payload = payload.filter(|_| handlers.on_drag.is_some());
    if payload.is_none() && !handlers.is_target() {
        return element;
    }
    let slot: Rc<RefCell<Option<Rc<Entry>>>> = Rc::default();
    let mut element = element.on_prepaint({
        let slot = Rc::clone(&slot);
        let runtime = Rc::downgrade(runtime);
        move |bounds, window, _| register(&slot, runtime, handlers, bounds, window)
    });
    if let Some(payload) = payload {
        let source = DragSource {
            payload,
            entry: slot,
            session: RefCell::default(),
        };
        element = element.on_drag(source, |source, _, window, cx| {
            cx.stop_propagation();
            start(source, window, cx)
        });
    }
    if handlers.is_target() {
        element = element
            .on_drag_move(|event: &DragMoveEvent<DragSource>, window, cx| {
                if let Some(session) = session_of(event.dragged_item()) {
                    hover(&session, window, cx);
                }
            })
            .on_drop(|source: &DragSource, window, cx| {
                if let Some(session) = source.session.borrow().clone() {
                    land(&session, window, cx);
                }
            });
    }
    element
}

fn register(
    slot: &RefCell<Option<Rc<Entry>>>,
    runtime: Weak<ShellRuntime>,
    handlers: Handlers,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    let clipped = bounds
        .intersect(&window.content_mask().bounds)
        .intersect(&Bounds::new(Point::default(), window.viewport_size()));
    let window_id = window.window_handle().window_id();
    let entry = window.with_global_id("gpui-shell-drag".into(), |global_id, window| {
        window.with_element_state(global_id, |state: Option<Rc<Entry>>, _| {
            let entry = state.unwrap_or_else(|| {
                let path = global_id.split_last().map(|(_, path)| path.to_vec());
                Rc::new(Entry {
                    path: path.unwrap_or_default(),
                    runtime: runtime.clone(),
                    bounds: Cell::new(bounds),
                    clipped: Cell::new(clipped),
                    handlers: Cell::new(handlers),
                })
            });
            entry.bounds.set(bounds);
            entry.clipped.set(clipped);
            entry.handlers.set(handlers);
            ENTRIES.with(|entries| {
                entries
                    .borrow_mut()
                    .entry(window_id)
                    .or_default()
                    .insert(global_id.clone(), Rc::downgrade(&entry));
            });
            (Rc::clone(&entry), entry)
        })
    });
    *slot.borrow_mut() = Some(entry);
}

fn start(source: &DragSource, window: &mut Window, cx: &mut App) -> gpui::Entity<Preview> {
    // Elements that are gone leave their keys behind. A drag starts rarely
    // enough to sweep them here rather than on every paint.
    ENTRIES.with(|entries| {
        for window in entries.borrow_mut().values_mut() {
            window.retain(|_, entry| entry.strong_count() > 0);
        }
    });
    let entry = source.entry.borrow().clone();
    let runtime = entry
        .as_ref()
        .map(|entry| entry.runtime.clone())
        .unwrap_or_default();
    let escape = cx.intercept_keystrokes(|event, window, cx| {
        if event.keystroke.key == "escape" && cx.stop_active_drag(window) {
            cx.stop_propagation();
        }
    });
    let session = Rc::new(Session {
        payload: source.payload.clone(),
        source: entry.as_ref().map(Rc::downgrade).unwrap_or_default(),
        runtime,
        over: RefCell::default(),
        dropped: Cell::new(false),
        _escape: escape,
    });
    *source.session.borrow_mut() = Some(Rc::clone(&session));
    let preview = cx.new(|_| Preview(Rc::clone(&session)));
    cx.observe_release_in(&preview, window, |preview, window, cx| {
        finish(&preview.0, window, cx);
    })
    .detach();
    deliver(
        &session,
        &session.source,
        Kind::Drag,
        Some("started"),
        window,
        cx,
    );
    preview
}

fn session_of(item: &dyn std::any::Any) -> Option<Rc<Session>> {
    item.downcast_ref::<DragSource>()?.session.borrow().clone()
}

/// The innermost target of this drag's script under the pointer, and where on
/// it.
fn target_at(session: &Session, window: &Window) -> Option<(Rc<Entry>, Side)> {
    let position = window.mouse_position();
    let id = window.window_handle().window_id();
    ENTRIES.with(|entries| {
        let entries = entries.borrow();
        entries
            .get(&id)?
            .values()
            .filter_map(Weak::upgrade)
            .filter(|entry| {
                entry.handlers.get().is_target()
                    && entry.runtime.ptr_eq(&session.runtime)
                    && entry.clipped.get().contains(&position)
            })
            .max_by_key(|entry| entry.path.len())
            .map(|entry| {
                let side = Side::of(entry.bounds.get(), position);
                (entry, side)
            })
    })
}

/// Every target hears the same pointer move, so the first to hear it resolves
/// it and the rest find nothing changed.
fn hover(session: &Session, window: &mut Window, cx: &mut App) {
    let next = target_at(session, window);
    let previous = session.over.borrow().clone();
    let same_entry = match (&previous, &next) {
        (Some((old, _)), Some((new, _))) => old.as_ptr() == Rc::as_ptr(new),
        (None, None) => true,
        _ => false,
    };
    if same_entry && previous.as_ref().map(|over| over.1) == next.as_ref().map(|over| over.1) {
        return;
    }
    *session.over.borrow_mut() = next
        .as_ref()
        .map(|(entry, side)| (Rc::downgrade(entry), *side));
    if !same_entry && let Some((old, _)) = previous {
        deliver(session, &old, Kind::DragMove, None, window, cx);
    }
    if let Some((new, side)) = next {
        let new = Rc::downgrade(&new);
        deliver(session, &new, Kind::DragMove, Some(side.name()), window, cx);
    }
}

fn land(session: &Session, window: &mut Window, cx: &mut App) {
    let Some((entry, side)) = target_at(session, window) else {
        return;
    };
    session.dropped.set(true);
    let entry = Rc::downgrade(&entry);
    deliver(session, &entry, Kind::Drop, Some(side.name()), window, cx);
}

/// The drag is over: a drop, Escape, or a release anywhere else. The target
/// the pointer was over hears that it no longer is, then the source hears how
/// the drag ended.
fn finish(session: &Session, window: &mut Window, cx: &mut App) {
    if let Some((target, _)) = session.over.take() {
        deliver(session, &target, Kind::DragMove, None, window, cx);
    }
    let phase = if session.dropped.get() {
        "ended"
    } else {
        "cancelled"
    };
    deliver(
        session,
        &session.source,
        Kind::Drag,
        Some(phase),
        window,
        cx,
    );
}

#[derive(Clone, Copy)]
enum Kind {
    Drag,
    DragMove,
    Drop,
}

fn deliver(
    session: &Session,
    entry: &Weak<Entry>,
    kind: Kind,
    value: Option<&'static str>,
    window: &mut Window,
    cx: &mut App,
) {
    let (Some(entry), Some(runtime)) = (entry.upgrade(), session.runtime.upgrade()) else {
        return;
    };
    let handlers = entry.handlers.get();
    let (callback, key) = match kind {
        Kind::Drag => (handlers.on_drag, "phase"),
        Kind::DragMove => (handlers.on_drag_move, "side"),
        Kind::Drop => (handlers.on_drop, "side"),
    };
    if let Some(callback) = callback {
        runtime.dispatch_drag(callback, session.payload.clone(), key, value, window, cx);
    }
}
