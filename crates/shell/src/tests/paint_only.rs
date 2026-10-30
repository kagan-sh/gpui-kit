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
