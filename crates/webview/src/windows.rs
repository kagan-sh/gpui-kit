//! WebView2 hosted in GPUI Fast's DirectComposition visual, between the window and its overlays.

use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::rc::Rc;

use anyhow::Result;
use gpui::{
    AnyWindowHandle, AsyncApp, Bounds, DispatchEventResult, MouseButton, Pixels, PlatformInput,
    Point, ScrollDelta, Window, point, px, size,
};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{PageButton, PagePointer, WebViewExtWindows as _};

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
    if let Err(error) = surface.bind(page) {
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
            window: window.window_handle(),
            cx: cx.to_async(),
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
        let id = self.surface.id();
        window
            .enable_window_composition()?
            .on_input(id, move |input| deliver(&webview, origin.get(), input))?;
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

    pub(super) fn release(self, webview: Rc<wry::WebView>) {
        self.cx
            .clone()
            .spawn(async move |cx| {
                drop(webview);
                let result = self.window.update(cx, |_, window, _| {
                    window
                        .enable_window_composition()?
                        .remove_surface(self.surface.id())
                });
                if let Ok(Err(error)) = result {
                    log::warn!("cannot release WebView composition surface: {error:#}");
                }
            })
            .detach();
    }
}

fn deliver(
    webview: &wry::WebView,
    origin: Point<Pixels>,
    input: &PlatformInput,
) -> DispatchEventResult {
    let event = match input {
        PlatformInput::MouseMove(event) => {
            let (x, y) = dip(origin, event.position);
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
            let (x, y) = dip(origin, event.position);
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
            let (x, y) = dip(origin, event.position);
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
            let (x, y) = dip(origin, event.position);
            PagePointer::Wheel {
                x,
                y,
                delta: wheel_delta(event.delta),
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

fn dip(origin: Point<Pixels>, position: Point<Pixels>) -> (i32, i32) {
    let point = position - origin;
    (
        f64::from(point.x).round() as i32,
        f64::from(point.y).round() as i32,
    )
}

// Win32's positive wheel points away from the user.
pub(super) fn wheel_delta(delta: ScrollDelta) -> i32 {
    let lines = match delta {
        ScrollDelta::Lines(point) => f64::from(point.y),
        ScrollDelta::Pixels(point) => f64::from(point.y) / 40.0,
    };
    (-lines * 120.0).round() as i32
}

#[cfg(test)]
mod tests {
    use super::wheel_delta;
    use gpui::{ScrollDelta, point, px};

    #[test]
    fn a_line_down_is_a_notch_toward_the_user() {
        assert_eq!(wheel_delta(ScrollDelta::Lines(point(0.0, 1.0))), -120);
        assert_eq!(
            wheel_delta(ScrollDelta::Pixels(point(px(0.0), px(40.0)))),
            -120
        );
    }
}
