//! WebView2 hosted in GPUI Fast's DirectComposition visual, between the window and its overlays.

use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use gpui::{
    AnyWindowHandle, AppContext as _, AsyncApp, Bounds, CursorStyle, DispatchEventResult, Hitbox,
    HitboxBehavior, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, PlatformInput, Point, ScrollDelta, Window, point, px, size,
};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{PageButton, PagePointer, WebViewExtWindows as _};

/// The page cursor to apply while the hitbox is hovered.
#[derive(Clone, Copy, Debug)]
pub(super) enum PageCursor {
    Style(CursorStyle),
    /// A CSS image cursor. GPUI has no `CursorStyle` for it, so the host keeps
    /// the `HCURSOR` and re-applies it after each frame and pointer move.
    Custom(*mut std::ffi::c_void),
}

pub(super) fn build(
    builder: wry::WebViewBuilder,
    window: &mut Window,
    cx: &mut gpui::App,
) -> wry::Result<super::WebView> {
    let surface = NativeWebViewSurface::new(window, cx)
        .map_err(|error| wry::Error::Io(std::io::Error::other(format!("{error:#}"))))?;
    let visual = match surface.visual() {
        Ok(visual) => visual,
        Err(error) => return Err(surface.abandon(window, io_error(error))),
    };
    let webview = match builder.build_as_composition(&*window, visual.as_wry()) {
        Ok(webview) => webview,
        Err(error) => return Err(surface.abandon(window, error)),
    };
    let mut view = super::WebView::new(webview, window, cx);
    let page = view.webview.clone();
    if let Err(error) = surface.forward(page.clone(), window) {
        return Err(surface.abandon(window, io_error(error)));
    }
    if let Err(error) = surface.bind(page.clone()) {
        return Err(surface.abandon(window, io_error(error)));
    }
    if let Err(error) = surface.forward_cursor(page) {
        return Err(surface.abandon(window, io_error(error)));
    }
    view.composition = Some(surface);
    Ok(view)
}

/// GPUI's visual is a windows 0.62 `IUnknown`. wry's API takes windows 0.61.
/// The COM vtable is the same. This holds GPUI's reference and a non-owning wrapper.
struct ComposedVisual {
    _keep: windows::core::IUnknown,
    borrowed: ManuallyDrop<windows_061::core::IUnknown>,
}

impl ComposedVisual {
    fn from_handle(handle: Box<dyn std::any::Any>) -> Result<Self> {
        let keep = handle
            .downcast::<windows::core::IUnknown>()
            .map_err(|_| anyhow::anyhow!("native surface is not a DirectComposition visual"))?;
        let raw = windows::core::Interface::as_raw(keep.as_ref());
        // SAFETY: `keep` is GPUI's live composition visual. windows 0.61 and 0.62 share the
        // IUnknown ABI. from_raw does not AddRef. ManuallyDrop skips Release, so this wrapper
        // does not drop GPUI's reference and stays valid while `keep` is held.
        let borrowed = ManuallyDrop::new(unsafe {
            <windows_061::core::IUnknown as windows_061::core::Interface>::from_raw(raw)
        });
        Ok(Self {
            _keep: *keep,
            borrowed,
        })
    }

    fn as_wry(&self) -> &windows_061::core::IUnknown {
        &self.borrowed
    }
}

fn io_error(error: impl std::fmt::Display) -> wry::Error {
    wry::Error::Io(std::io::Error::other(error.to_string()))
}

pub(super) struct NativeWebViewSurface {
    surface: gpui::WindowCompositionSurface,
    origin: Rc<Cell<Point<Pixels>>>,
    scale: Rc<Cell<f32>>,
    /// Page cursor while the hitbox is hovered. Cleared on leave.
    page_cursor: Rc<Cell<Option<PageCursor>>>,
    /// Custom image cursor held for as long as the page hitbox is hovered.
    current_cursor: Rc<Cell<Option<*mut std::ffi::c_void>>>,
    /// Overlay hitbox above the page for the current frame. Refreshed in prepaint.
    pointer_layer: Cell<Option<Hitbox>>,
    /// Whether the overlay pointer layer was hovered on the last forwarded move.
    pointer_over: Rc<Cell<bool>>,
    /// Buttons pressed on the page (Left=1, Right=2, Middle=4). Captures until release.
    pointer_buttons: Rc<Cell<u8>>,
    window: AnyWindowHandle,
    cx: AsyncApp,
}

impl NativeWebViewSurface {
    pub(super) fn new(window: &Window, cx: &gpui::App) -> Result<Self> {
        let surface = window
            .enable_window_composition()?
            .create_native_surface()?;
        Ok(Self {
            surface,
            origin: Rc::new(Cell::new(Point::default())),
            scale: Rc::new(Cell::new(window.scale_factor())),
            page_cursor: Rc::new(Cell::new(None)),
            current_cursor: Rc::new(Cell::new(None)),
            pointer_layer: Cell::new(None),
            pointer_over: Rc::new(Cell::new(false)),
            pointer_buttons: Rc::new(Cell::new(0)),
            window: window.window_handle(),
            cx: cx.to_async(),
        })
    }

    pub(super) fn cursor_cells(
        &self,
    ) -> (
        Rc<Cell<Option<PageCursor>>>,
        Rc<Cell<Option<*mut std::ffi::c_void>>>,
    ) {
        (self.page_cursor.clone(), self.current_cursor.clone())
    }

    pub(super) fn pointer_tracking(
        &self,
    ) -> (Rc<Cell<bool>>, Rc<Cell<u8>>, AnyWindowHandle, AsyncApp) {
        (
            self.pointer_over.clone(),
            self.pointer_buttons.clone(),
            self.window,
            self.cx.clone(),
        )
    }

    pub(super) fn origin_and_scale(&self) -> (Point<Pixels>, f32) {
        (self.origin.get(), self.scale.get())
    }

    pub(super) fn take_pointer_layer(&self) -> Option<Hitbox> {
        self.pointer_layer.take()
    }

    pub(super) fn set_pointer_layer(&self, layer: Option<Hitbox>) {
        self.pointer_layer.set(layer);
    }

    /// Hitbox on the overlay surface over the page. Composition leaves the base page
    /// hitbox out of the hover stack on the native surface; this layer is hovered only
    /// while no menu or other overlay sits above the pointer.
    pub(super) fn insert_pointer_layer(
        &self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) -> Result<Hitbox> {
        let overlay = window.enable_window_composition()?.overlay_surface();
        window.with_composition_surface(overlay, |window| {
            window.insert_hitbox(bounds, HitboxBehavior::Normal)
        })
    }

    fn visual(&self) -> Result<ComposedVisual> {
        let handle = self.surface.platform_surface()?.platform_handle()?;
        ComposedVisual::from_handle(handle)
    }

    fn abandon(self, window: &mut Window, error: wry::Error) -> wry::Error {
        let id = self.surface.id();
        if let Ok(composition) = window.enable_window_composition() {
            let _ = composition.remove_surface(id);
        }
        error
    }

    pub(super) fn set_scale_factor(&self, scale_factor: f32) {
        self.scale.set(scale_factor);
    }

    pub(super) fn set_bounds(&self, webview: &wry::WebView, rect: wry::Rect) -> Result<()> {
        let scale = self.scale.get();
        let position = rect.position.to_logical::<f64>(f64::from(scale));
        let extent = rect.size.to_logical::<f64>(f64::from(scale));
        let bounds = Bounds::new(
            point(px(position.x as f32), px(position.y as f32)),
            size(px(extent.width as f32), px(extent.height as f32)),
        );
        self.place(webview, bounds)
    }

    pub(super) fn set_geometry(
        &self,
        webview: &wry::WebView,
        _document: Bounds<Pixels>,
        visible: Bounds<Pixels>,
    ) -> Result<()> {
        self.place(webview, visible)
    }

    fn place(&self, webview: &wry::WebView, bounds: Bounds<Pixels>) -> Result<()> {
        let scale = self.scale.get();
        self.origin.set(bounds.origin);
        self.surface
            .platform_surface()?
            .set_bounds(bounds.to_device_pixels(scale))?;
        webview.set_bounds(wry::Rect {
            position: LogicalPosition::new(0.0, 0.0).into(),
            size: LogicalSize::new(f64::from(bounds.size.width), f64::from(bounds.size.height))
                .into(),
        })?;
        Ok(())
    }

    pub(super) fn set_visible(&self, visible: bool) -> Result<()> {
        self.surface.platform_surface()?.set_visible(visible)
    }

    fn forward(&self, webview: Rc<wry::WebView>, window: &Window) -> Result<()> {
        let origin = self.origin.clone();
        let scale = self.scale.clone();
        let page_cursor = self.page_cursor.clone();
        let current_cursor = self.current_cursor.clone();
        let handle = self.window;
        let cx = self.cx.clone();
        let id = self.surface.id();
        window
            .enable_window_composition()?
            .on_input(id, move |input| {
                if matches!(input, PlatformInput::MouseExited(_)) {
                    page_cursor.set(None);
                    current_cursor.set(None);
                    cx.spawn(async move |cx| {
                        let _ = handle.update(cx, |_, window, _| window.refresh());
                    })
                    .detach();
                } else if current_cursor.get().is_some() {
                    set_cursor(current_cursor.get());
                }
                deliver(&webview, origin.get(), scale.get(), input)
            })?;
        Ok(())
    }

    fn bind(&self, webview: Rc<wry::WebView>) -> Result<()> {
        self.surface
            .platform_surface()?
            .set_compositor_recreated_callback(Rc::new(move |visual| {
                let visual = ComposedVisual::from_handle(visual)?;
                webview
                    .set_root_visual(visual.as_wry())
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                Ok(())
            }))
    }

    fn forward_cursor(&self, webview: Rc<wry::WebView>) -> Result<()> {
        let page_cursor = self.page_cursor.clone();
        let current_cursor = self.current_cursor.clone();
        let handle = self.window;
        let cx = self.cx.clone();
        webview
            .on_cursor_changed(Box::new(move |cursor| {
                let next = style_for(cursor);
                match next {
                    Some(PageCursor::Custom(raw)) => current_cursor.set(Some(raw)),
                    _ => current_cursor.set(None),
                }
                page_cursor.set(next);
                if let Some(PageCursor::Custom(raw)) = next {
                    set_cursor(Some(raw));
                }
                cx.spawn(async move |cx| {
                    let _ = handle.update(cx, |_, window, _| window.refresh());
                })
                .detach();
            }))
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    pub(super) fn release(self, webview: Rc<wry::WebView>, user_data: Option<PathBuf>) {
        let surface_id = self.surface.id();
        let window = self.window;
        let cx = self.cx.clone();
        // Drop the surface handle so the recovery callback releases its page Rc.
        drop(self.surface);
        cx.clone()
            .spawn(async move |cx| {
                let exited = user_data
                    .as_ref()
                    .and_then(|_| webview.browser_process_exited().ok());
                // Clear input forwarding before closing the native page.
                super::remove_surface_when_free(window, cx, surface_id).await;
                drop(webview);
                if let Some(path) = user_data {
                    // The environment is not Send; keep it on this executor until the wait ends.
                    if let Some((rx, env)) = exited {
                        cx.background_spawn(async move {
                            let _ = rx.recv_timeout(Duration::from_secs(5));
                        })
                        .await;
                        drop(env);
                    }
                    cx.background_spawn(async move {
                        remove_user_data(&path);
                    })
                    .detach();
                }
            })
            .detach();
    }
}

/// Apply the page cursor during paint, and forward move/click through the overlay
/// pointer layer. That layer is above the native surface and below menus, so it is
/// hovered only while the page owns the pointer; a mapped style then uses the layer
/// hitbox. `set_window_cursor_style` would outrank every menu. The layer takes
/// move/click from composition input (wheel still arrives through `on_input`). A
/// press captures until release so a drag that leaves the page still gets moves
/// and the up; leaving the layer without a capture sends Leave, because the
/// native surface is never hovered while the layer is present.
pub(super) fn paint_cursor_for_page(
    page_cursor: &Rc<Cell<Option<PageCursor>>>,
    current_cursor: &Rc<Cell<Option<*mut std::ffi::c_void>>>,
    pointer_over: &Rc<Cell<bool>>,
    pointer_buttons: &Rc<Cell<u8>>,
    layer: &Hitbox,
    webview: &Rc<wry::WebView>,
    origin: Point<Pixels>,
    scale: f32,
    handle: AnyWindowHandle,
    async_cx: &AsyncApp,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let mouse = window.mouse_position();
    let bounds = layer.bounds.intersect(&layer.content_mask.bounds);
    let in_page = bounds.contains(&mouse);
    let hovered = layer.is_hovered(window);
    if super::page_owns_pointer_cursor(in_page, hovered) {
        if let Some(PageCursor::Style(style)) = page_cursor.get() {
            window.set_cursor_style(style, layer);
        } else if let Some(raw) = current_cursor.get() {
            set_cursor(Some(raw));
            let current_cursor = current_cursor.clone();
            window.defer(cx, move |_, _| {
                if let Some(raw) = current_cursor.get() {
                    set_cursor(Some(raw));
                }
            });
        }
    }

    let webview = webview.clone();
    let layer = layer.clone();
    let page_cursor = page_cursor.clone();
    let current_cursor = current_cursor.clone();
    let pointer_over = pointer_over.clone();
    let pointer_buttons = pointer_buttons.clone();
    let async_cx = async_cx.clone();
    window.on_mouse_event({
        let webview = webview.clone();
        let layer = layer.clone();
        let page_cursor = page_cursor.clone();
        let current_cursor = current_cursor.clone();
        let pointer_over = pointer_over.clone();
        let pointer_buttons = pointer_buttons.clone();
        let async_cx = async_cx.clone();
        move |event: &MouseMoveEvent, phase, window, cx| {
            if !phase.bubble() {
                return;
            }
            let hovered = layer.is_hovered(window);
            let step =
                super::layer_pointer_move(hovered, pointer_over.get(), pointer_buttons.get() != 0);
            if step.deliver {
                let _ = deliver(
                    &webview,
                    origin,
                    scale,
                    &PlatformInput::MouseMove(event.clone()),
                );
                cx.stop_propagation();
            }
            if step.leave {
                leave_page(
                    &webview,
                    origin,
                    scale,
                    &page_cursor,
                    &current_cursor,
                    handle,
                    &async_cx,
                    event.position,
                    event.pressed_button,
                    event.modifiers,
                );
            }
            pointer_over.set(hovered);
        }
    });
    window.on_mouse_event({
        let webview = webview.clone();
        let layer = layer.clone();
        let pointer_over = pointer_over.clone();
        let pointer_buttons = pointer_buttons.clone();
        move |event: &MouseDownEvent, phase, window, cx| {
            if !phase.bubble() || !super::layer_pointer_down(layer.is_hovered(window)) {
                return;
            }
            if let Some(bit) = button_mask(event.button) {
                pointer_buttons.set(pointer_buttons.get() | bit);
            }
            pointer_over.set(true);
            let _ = deliver(
                &webview,
                origin,
                scale,
                &PlatformInput::MouseDown(event.clone()),
            );
            cx.stop_propagation();
        }
    });
    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
        if !phase.bubble() {
            return;
        }
        let hovered = layer.is_hovered(window);
        let bit = button_mask(event.button);
        let button_captured = bit.is_some_and(|bit| pointer_buttons.get() & bit != 0);
        let other_captured = bit.is_some_and(|bit| pointer_buttons.get() & !bit != 0)
            || (bit.is_none() && pointer_buttons.get() != 0);
        let step = super::layer_pointer_up(hovered, button_captured, other_captured);
        if !step.deliver {
            return;
        }
        if let Some(bit) = bit {
            pointer_buttons.set(pointer_buttons.get() & !bit);
        }
        let _ = deliver(
            &webview,
            origin,
            scale,
            &PlatformInput::MouseUp(event.clone()),
        );
        if step.leave {
            leave_page(
                &webview,
                origin,
                scale,
                &page_cursor,
                &current_cursor,
                handle,
                &async_cx,
                event.position,
                None,
                event.modifiers,
            );
            pointer_over.set(false);
        } else {
            pointer_over.set(hovered);
        }
        cx.stop_propagation();
    });
}

fn button_mask(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0b001),
        MouseButton::Right => Some(0b010),
        MouseButton::Middle => Some(0b100),
        MouseButton::Navigate(_) => None,
    }
}

fn leave_page(
    webview: &wry::WebView,
    origin: Point<Pixels>,
    scale: f32,
    page_cursor: &Cell<Option<PageCursor>>,
    current_cursor: &Cell<Option<*mut std::ffi::c_void>>,
    handle: AnyWindowHandle,
    cx: &AsyncApp,
    position: Point<Pixels>,
    pressed_button: Option<MouseButton>,
    modifiers: gpui::Modifiers,
) {
    page_cursor.set(None);
    current_cursor.set(None);
    let _ = deliver(
        webview,
        origin,
        scale,
        &PlatformInput::MouseExited(MouseExitEvent {
            position,
            pressed_button,
            modifiers,
        }),
    );
    let handle = handle;
    let cx = cx.clone();
    cx.spawn(async move |cx| {
        let _ = handle.update(cx, |_, window, _| window.refresh());
    })
    .detach();
}

fn set_cursor(cursor: Option<*mut std::ffi::c_void>) {
    use windows::Win32::UI::WindowsAndMessaging::{HCURSOR, SetCursor};
    // SAFETY: WebView2's HCURSOR is valid for SetCursor for the page's lifetime.
    unsafe {
        let _ = SetCursor(cursor.map(HCURSOR));
    }
}

/// Map a WebView2 cursor to a GPUI style by comparing with the shared system cursors.
fn style_for(cursor: Option<*mut std::ffi::c_void>) -> Option<PageCursor> {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM, IDC_NO, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE,
        IDC_SIZEWE, LoadCursorW,
    };
    let raw = cursor?;
    // SAFETY: LoadCursorW with a system IDC returns a shared HCURSOR; pointer equality is stable.
    let system = |id| unsafe { LoadCursorW(None, id).ok().map(|c| c.0) };
    let matched = [
        (system(IDC_IBEAM), CursorStyle::IBeam),
        (system(IDC_HAND), CursorStyle::PointingHand),
        (system(IDC_SIZEWE), CursorStyle::ResizeLeftRight),
        (system(IDC_SIZENS), CursorStyle::ResizeUpDown),
        (system(IDC_SIZENWSE), CursorStyle::ResizeUpLeftDownRight),
        (system(IDC_SIZENESW), CursorStyle::ResizeUpRightDownLeft),
        (system(IDC_CROSS), CursorStyle::Crosshair),
        (system(IDC_NO), CursorStyle::OperationNotAllowed),
        (system(IDC_ARROW), CursorStyle::Arrow),
    ]
    .into_iter()
    .find_map(|(handle, style)| (handle == Some(raw)).then_some(style));
    Some(match matched {
        Some(style) => PageCursor::Style(style),
        None => PageCursor::Custom(raw),
    })
}

fn remove_user_data(path: &std::path::Path) {
    for _ in 0..40 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    log::warn!("could not remove WebView2 data at {}", path.display());
}

fn deliver(
    webview: &wry::WebView,
    origin: Point<Pixels>,
    scale: f32,
    input: &PlatformInput,
) -> DispatchEventResult {
    let event = match input {
        PlatformInput::MouseMove(event) => {
            let (x, y) = page_point(origin, event.position, scale);
            PagePointer::Move {
                x,
                y,
                left: event.pressed_button == Some(MouseButton::Left),
                right: event.pressed_button == Some(MouseButton::Right),
                middle: event.pressed_button == Some(MouseButton::Middle),
                control: event.modifiers.control,
                shift: event.modifiers.shift,
            }
        }
        PlatformInput::MouseExited(_) => PagePointer::Leave,
        PlatformInput::MouseDown(event) => {
            if event.button == MouseButton::Left {
                let _ = webview.focus();
            }
            let Some(button) = page_button(event.button) else {
                return DispatchEventResult {
                    propagate: true,
                    default_prevented: false,
                };
            };
            let (x, y) = page_point(origin, event.position, scale);
            PagePointer::Button {
                x,
                y,
                button,
                down: true,
                double: event.click_count >= 2,
                control: event.modifiers.control,
                shift: event.modifiers.shift,
            }
        }
        PlatformInput::MouseUp(event) => {
            let Some(button) = page_button(event.button) else {
                return DispatchEventResult {
                    propagate: true,
                    default_prevented: false,
                };
            };
            let (x, y) = page_point(origin, event.position, scale);
            PagePointer::Button {
                x,
                y,
                button,
                down: false,
                double: false,
                control: event.modifiers.control,
                shift: event.modifiers.shift,
            }
        }
        PlatformInput::Scroll(event) => {
            let (x, y) = page_point(origin, event.position, scale);
            let (lines, chars) = wheel_scroll_amounts();
            let (delta_x, delta_y) = wheel_deltas(event.delta, lines, chars);
            PagePointer::Wheel {
                x,
                y,
                delta_x,
                delta_y,
                control: event.modifiers.control,
                shift: event.modifiers.shift,
            }
        }
        _ => {
            return DispatchEventResult {
                propagate: true,
                default_prevented: false,
            };
        }
    };
    let _ = webview.send_pointer(event);
    DispatchEventResult {
        propagate: false,
        default_prevented: true,
    }
}

fn page_button(button: MouseButton) -> Option<PageButton> {
    match button {
        MouseButton::Left => Some(PageButton::Left),
        MouseButton::Right => Some(PageButton::Right),
        MouseButton::Middle => Some(PageButton::Middle),
        MouseButton::Navigate(_) => None,
    }
}

/// Convert a GPUI logical point into the page's raw-pixel client coordinates.
pub(super) fn page_point(origin: Point<Pixels>, position: Point<Pixels>, scale: f32) -> (i32, i32) {
    let point = position - origin;
    (
        (f64::from(point.x) * f64::from(scale)).round() as i32,
        (f64::from(point.y) * f64::from(scale)).round() as i32,
    )
}

fn wheel_scroll_amounts() -> (f64, f64) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETWHEELSCROLLCHARS, SPI_GETWHEELSCROLLLINES, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        SystemParametersInfoW,
    };
    let mut lines = 3u32;
    let mut chars = 3u32;
    // SAFETY: SPI writes into a local u32; a failed call leaves the defaults.
    let _ = unsafe {
        SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some((&raw mut lines).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    let _ = unsafe {
        SystemParametersInfoW(
            SPI_GETWHEELSCROLLCHARS,
            0,
            Some((&raw mut chars).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    (lines.max(1) as f64, chars.max(1) as f64)
}

/// Undo GPUI's Windows wheel normalization and restore Win32 `WHEEL_DELTA` ticks.
///
/// Positive `delta_y` is away from the user. Positive `delta_x` is tilt right; GPUI's
/// horizontal wheel path negates Win32, so this path negates again.
pub(super) fn wheel_deltas(delta: ScrollDelta, scroll_lines: f64, scroll_chars: f64) -> (i32, i32) {
    let (x, y) = match delta {
        ScrollDelta::Lines(point) => (f64::from(point.x), f64::from(point.y)),
        ScrollDelta::Pixels(point) => (f64::from(point.x) / 40.0, f64::from(point.y) / 40.0),
    };
    let lines = scroll_lines.max(1.0);
    let chars = scroll_chars.max(1.0);
    let delta_y = (y / lines * 120.0).round() as i32;
    let delta_x = (-x / chars * 120.0).round() as i32;
    (delta_x, delta_y)
}

#[cfg(test)]
mod tests {
    use super::{page_point, style_for, wheel_deltas};
    use gpui::{ScrollDelta, point, px};

    #[test]
    fn clearing_the_page_cursor_drops_the_style() {
        assert!(style_for(None).is_none());
    }

    #[test]
    fn a_line_away_keeps_the_win32_sign() {
        assert_eq!(
            wheel_deltas(ScrollDelta::Lines(point(0.0, 3.0)), 3.0, 3.0),
            (0, 120)
        );
        assert_eq!(
            wheel_deltas(ScrollDelta::Pixels(point(px(0.0), px(120.0))), 3.0, 3.0),
            (0, 120)
        );
    }

    #[test]
    fn horizontal_wheel_undoes_gpui_sign() {
        // GPUI stores WM_MOUSEHWHEEL tilt-right as a negative Lines.x after system chars.
        assert_eq!(
            wheel_deltas(ScrollDelta::Lines(point(-3.0, 0.0)), 3.0, 3.0),
            (120, 0)
        );
    }

    #[test]
    fn page_points_scale_with_the_monitor() {
        let origin = point(px(10.0), px(20.0));
        assert_eq!(page_point(origin, point(px(10.0), px(20.0)), 1.0), (0, 0));
        assert_eq!(page_point(origin, point(px(60.0), px(70.0)), 1.5), (75, 75));
        assert_eq!(
            page_point(origin, point(px(60.0), px(70.0)), 2.0),
            (100, 100)
        );
    }
}
