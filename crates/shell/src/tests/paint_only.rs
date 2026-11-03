//! A paint-only view draws, and fails on anything that could take input.

use std::{ops::Deref as _, rc::Rc, time::Duration};

use gpui::{Entity, TestAppContext, VisualTestContext};

use crate::{ScriptView, ShellRuntime, policy::Policy};

struct Blank;

impl gpui::Render for Blank {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::Empty
    }
}

struct Rooted(Entity<ScriptView>);

impl gpui::Render for Rooted {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        self.0.clone()
    }
}

fn mount(
    cx: &mut TestAppContext,
    source: &str,
    policy: Policy,
) -> (Entity<ScriptView>, VisualTestContext) {
    cx.update(crate::init);
    let runtime = ShellRuntime::new_isolated().expect("runtime");
    cx.update(|cx| runtime.set_global(cx));
    let view_type = runtime.load_source("overlay.js", source).expect("load");
    let window = cx.add_window(move |window, cx| {
        let view = runtime
            .instantiate_view_with_policy(&view_type, Rc::new(policy), window, cx)
            .expect("instantiate");
        Rooted(view)
    });
    let mut context = VisualTestContext::from_window(*window.deref(), cx);
    context.update(|window, cx| window.draw(cx).clear(cx));
    context.run_until_parked();
    context.update(|window, cx| window.draw(cx).clear(cx));
    let view = window
        .root(&mut context)
        .expect("root")
        .read_with(&context, |root, _| root.0.clone());
    (view, context)
}

fn error(view: &Entity<ScriptView>, context: &mut VisualTestContext) -> Option<String> {
    context.update(|_, cx| view.read(cx).build_error().map(str::to_owned))
}

fn paint_only() -> Policy {
    Policy::new().painting_only()
}

const DRAWING: &str = r##"
import { View, div } from "gpui-kit";
export default class Glow extends View {
  init(_props, cx) {
    this.x = 0;
    this.tick = cx.timer.every(10, (cx) => { this.x += 1; cx.notify(); });
  }
  render() {
    return div().id("glow").size_full().relative()
      .child(div().absolute().left(this.x).top(4).w(10).h(10).rounded(5).bg("#ff00ff").opacity(0.5)
        .transition("left", { duration: 100 }))
      .child("hello");
  }
}
"##;

#[gpui::test]
fn a_paint_only_view_draws_boxes_text_and_motion(cx: &mut TestAppContext) {
    let (view, mut context) = mount(cx, DRAWING, paint_only());
    context.executor().advance_clock(Duration::from_millis(50));
    context.run_until_parked();
    context.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(error(&view, &mut context), None);
}

fn attack(element: &str) -> String {
    format!(
        r#"
import {{ View, div }} from "gpui-kit";
import {{ Popover }} from "gpui-base";
export default class Attack extends View {{
  render() {{
    return div().size_full().child({element});
  }}
}}
"#
    )
}

#[gpui::test]
fn every_way_to_take_input_fails_a_paint_only_view(cx: &mut TestAppContext) {
    let attacks = [
        (
            "div().w(10).h(10).on_mouse_down_out((_e, cx) => cx.stop_propagation())",
            "on_mouse_down_out",
        ),
        ("div().on_click(() => {})", "on_click"),
        ("div().on_drag(\"x\", () => {})", "on_drag"),
        ("div().on_drop(() => {})", "on_drop"),
        ("div().on_key_down(() => {})", "on_key_down"),
        ("div().hover((s) => s.bg(\"#ff0000\"))", "hover"),
        ("div().cursor_pointer()", "cursor_pointer"),
        ("div().overflow_y_scroll()", "overflow_y_scroll"),
        (
            "Popover.new(\"menu\").open(true).trigger(div()).content(div().w(200).h(200))",
            "Popover",
        ),
    ];
    for (element, named) in attacks {
        let (view, mut context) = mount(cx, &attack(element), paint_only());
        let error = error(&view, &mut context).unwrap_or_else(|| panic!("{named} was drawn"));
        assert!(
            error.contains("paint-only") && error.contains(named),
            "{named}: {error}"
        );
    }
}

#[gpui::test]
fn the_same_handler_works_in_an_ordinary_view(cx: &mut TestAppContext) {
    let (view, mut context) = mount(
        cx,
        &attack("div().w(10).h(10).on_mouse_down_out(() => {})"),
        Policy::new(),
    );
    assert_eq!(error(&view, &mut context), None);
}

const FOCUSING: &str = r#"
import { View, div } from "gpui-kit";
export default class Grab extends View {
  init(_props, cx) {
    this.focus = cx.focus_handle();
    this.tick = cx.timer.every(10, () => { try { this.focus.focus(); } catch {} });
  }
  render() {
    return div().size_full();
  }
}
"#;

#[gpui::test]
fn a_paint_only_view_that_reaches_for_the_focus_fails(cx: &mut TestAppContext) {
    let (view, mut context) = mount(cx, FOCUSING, paint_only());
    context.executor().advance_clock(Duration::from_millis(50));
    context.run_until_parked();
    let error = error(&view, &mut context).expect("the grab is refused");
    assert!(error.contains("cannot use the window"), "{error}");
    assert!(context.update(|window, cx| window.focused(cx).is_none()));
}

const INPUT: &str = r#"
import { View, div } from "gpui-kit";
import { InputState } from "gpui-base";
export default class Field extends View {
  init() { this.input = InputState.new(); }
  render() { return div(); }
}
"#;

#[gpui::test]
fn a_paint_only_view_cannot_hold_input_state(cx: &mut TestAppContext) {
    cx.update(crate::init);
    let runtime = ShellRuntime::new_isolated().expect("runtime");
    cx.update(|cx| runtime.set_global(cx));
    let view_type = runtime.load_source("field.js", INPUT).expect("load");
    let window = cx.add_window(|_, _| Blank);
    let mut context = VisualTestContext::from_window(*window.deref(), cx);
    let mounted = context.update(|window, cx| {
        runtime.instantiate_view_with_policy(&view_type, Rc::new(paint_only()), window, cx)
    });
    let refused = match mounted {
        Err(error) => error.to_string(),
        Ok(view) => error(&view, &mut context).unwrap_or_default(),
    };
    assert!(refused.contains("paint-only"), "{refused}");
}

fn image(source: &str) -> String {
    format!(
        r#"
import {{ View, div, image }} from "gpui-kit";
export default class Picture extends View {{
  render() {{ return div().child(image({source:?})); }}
}}
"#
    )
}

#[gpui::test]
fn an_image_loads_only_from_the_application_folder(cx: &mut TestAppContext) {
    for source in [
        "https://example.com/a.png?d=secret",
        "data:image/png;base64,AAAA",
        "../outside.png",
        "/etc/passwd",
        "",
    ] {
        let (view, mut context) = mount(cx, &image(source), Policy::new());
        let error = error(&view, &mut context).unwrap_or_else(|| panic!("{source:?} was loaded"));
        assert!(
            error.contains("relative path inside the application's folder"),
            "{source}: {error}"
        );
    }
    let (view, mut context) = mount(cx, &image("icons/a.png"), Policy::new());
    assert_eq!(error(&view, &mut context), None);
}

#[gpui::test]
fn a_refused_view_stops_its_timers(cx: &mut TestAppContext) {
    let before = crate::engine::quickjs::task_count();
    let (view, mut context) = mount(cx, FOCUSING, paint_only());
    assert!(crate::engine::quickjs::task_count() > before);
    context.executor().advance_clock(Duration::from_millis(50));
    context.run_until_parked();
    assert!(error(&view, &mut context).is_some());
    assert_eq!(crate::engine::quickjs::task_count(), before);
}

const TURNING: &str = r#"
import { View, div } from "gpui-kit";
export default class Turn extends View {
  init(_props, cx) {
    this.ticks = 0;
    this.tick = cx.timer.every(10, (cx) => { this.ticks += 1; cx.notify(); });
  }
  render() {
    const box = div().w(10).h(10);
    return div().size_full().child(this.ticks < 3 ? box : box.on_mouse_down_out(() => {}));
  }
}
"#;

#[gpui::test]
fn a_view_that_turns_bad_later_is_refused_before_that_frame(cx: &mut TestAppContext) {
    let (view, mut context) = mount(cx, TURNING, paint_only());
    let mut clean_frames = 0;
    for _ in 0..8 {
        context.executor().advance_clock(Duration::from_millis(10));
        context.run_until_parked();
        context.update(|window, cx| window.draw(cx).clear(cx));
        let (error, tree) = context.update(|_, cx| {
            let view = view.read(cx);
            (
                view.build_error().map(str::to_owned),
                view.snapshot().map(crate::RenderSnapshot::debug_tree),
            )
        });
        match error {
            None => {
                clean_frames += 1;
                let tree = tree.expect("a clean frame is drawn");
                assert!(!tree.contains("on_mouse_down_out"), "{tree}");
            }
            Some(error) => {
                assert!(error.contains("on_mouse_down_out"), "{error}");
                assert!(tree.is_none(), "a refused view still holds a frame");
            }
        }
    }
    assert!(clean_frames >= 2, "the view never drew cleanly");
    assert!(
        error(&view, &mut context).is_some(),
        "the bad frame was never refused"
    );
}

#[gpui::test]
fn a_paint_only_view_takes_no_keys_even_when_sent_one(cx: &mut TestAppContext) {
    let (view, mut context) = mount(cx, DRAWING, paint_only());
    let delivered = context.update(|window, cx| {
        let key = gpui::Keystroke::parse("a").expect("a key");
        view.update(cx, |view, cx| {
            view.dispatch_key_down(&key, false, window, cx)
        })
    });
    let error = delivered.expect_err("the key was delivered");
    assert!(error.to_string().contains("paint-only"), "{error}");
}

#[gpui::test]
fn a_failed_reload_counts_as_a_build_error(cx: &mut TestAppContext) {
    let (view, mut context) = mount(cx, DRAWING, paint_only());
    assert_eq!(error(&view, &mut context), None);
    context.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.set_reload_error(Some("reload broke".to_owned()), cx);
        });
    });
    assert_eq!(error(&view, &mut context).as_deref(), Some("reload broke"));
}
