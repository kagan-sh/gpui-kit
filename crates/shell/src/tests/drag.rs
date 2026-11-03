//! Script drag and drop, driven with real pointer events.

use std::{ops::Deref as _, rc::Rc};

use gpui::{
    Modifiers, MouseButton, Pixels, Point, TestAppContext, VisualTestContext, WindowHandle, point,
    px,
};

use crate::{ScriptView, ShellRuntime};

/// Two columns. `todo` holds cards `a` and `b`, which are both sources and
/// targets; `done` is a target only. A card's handler says which card heard.
/// Escape is also the board's own `back`, so a test can tell whether a drag
/// kept the key to itself.
const BOARD: &str = r#"
import { View, div } from "gpui-kit";
import { h_flex, v_flex } from "gpui-base";

export default class Board extends View {
  init(_props, cx) {
    this.log = [];
    this.dragged = null;
    this.handle = cx.focus_handle();
    cx.bind_keys([{ keystroke: "escape", action: "back" }]);
  }

  note(line, cx) {
    this.log.push(line);
    cx.notify();
  }

  card(id) {
    return div()
      .id(id)
      .h(40)
      .when(this.dragged === id, (el) => el.opacity(0.5))
      .on_click((_event, cx) => this.note(`click:${id}`, cx))
      .on_drag(`ticket:${id}`, (event, cx) => {
        this.dragged = event.phase === "started" ? id : null;
        this.note(`drag:${event.payload}:${event.phase}`, cx);
      })
      .on_drag_move((event, cx) => this.note(`over:${id}:${event.side}`, cx))
      .on_drop((event, cx) => this.note(`drop:${id}:${event.payload}:${event.side}`, cx))
      .child(id);
  }

  render() {
    return v_flex()
      .id("board")
      .w(400)
      .h(300)
      .track_focus(this.handle)
      .on_action("back", (_event, cx) => this.note("back", cx))
      .child(
        h_flex()
          .h(200)
          .child(
            v_flex()
              .id("todo")
              .w(150)
              .h(200)
              .on_drop((event, cx) => this.note(`drop:todo:${event.payload}:${event.side}`, cx))
              .child(this.card("a"))
              .child(this.card("b")),
          )
          .child(
            v_flex()
              .id("done")
              .w(150)
              .h(200)
              .on_drag_move((event, cx) => this.note(`over:done:${event.side}`, cx))
              .on_drop((event, cx) => this.note(`drop:done:${event.payload}:${event.side}`, cx)),
          ),
      )
      .child(div().child(`log=${this.log.join(" ")}`));
  }
}
"#;

struct Rooted(gpui::Entity<ScriptView>);

impl gpui::Render for Rooted {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        self.0.clone()
    }
}

struct Board {
    context: VisualTestContext,
    window: WindowHandle<Rooted>,
}

impl Board {
    fn open(cx: &mut TestAppContext) -> Self {
        cx.update(|cx| crate::init(cx));
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        cx.update(|cx| runtime.set_global(cx));
        let view_type = runtime.load_source("board", BOARD).expect("load");
        let focus = Rc::clone(&runtime);
        let window = cx.add_window(move |window, cx| {
            let view = Rc::clone(&runtime)
                .instantiate_view(&view_type, window, cx)
                .expect("instantiate");
            Rooted(view)
        });
        let mut context = VisualTestContext::from_window(*window.deref(), cx);
        context.update(|window, cx| window.draw(cx).clear(cx));
        let handles = focus.entities().focus_handles();
        context.update(|window, cx| handles[0].focus(window, cx));
        Self { context, window }
    }

    fn draw(&mut self) {
        self.context.run_until_parked();
        self.context.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn move_to(&mut self, position: Point<Pixels>, held: bool) {
        let button = held.then_some(MouseButton::Left);
        self.context
            .simulate_mouse_move(position, button, Modifiers::default());
        self.draw();
    }

    fn press(&mut self, position: Point<Pixels>) {
        self.move_to(position, false);
        self.context
            .simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
        self.draw();
    }

    fn release(&mut self, position: Point<Pixels>) {
        self.context
            .simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
        self.draw();
    }

    /// Presses on card `a` and moves far enough for GPUI to call it a drag.
    fn start_on_a(&mut self) {
        self.press(point(px(20.), px(20.)));
        self.move_to(point(px(30.), px(22.)), true);
    }

    fn log(&mut self) -> String {
        let view = self
            .window
            .root(&mut self.context)
            .expect("root view")
            .read_with(&self.context, |root, _| root.0.clone());
        let tree = self.context.update(|_, cx| {
            view.read(cx)
                .snapshot()
                .map(crate::RenderSnapshot::debug_tree)
                .unwrap_or_default()
        });
        let start = tree.find("log=").expect("the log line") + 4;
        let end = tree[start..]
            .find('"')
            .map_or(tree.len(), |end| start + end);
        tree[start..end].to_owned()
    }
}

/// A drag goes past the source, tells each target where the pointer is on it,
/// and drops on the innermost one: a card, not the column around it.
#[gpui::test]
fn a_drag_reports_the_side_of_each_target_and_drops_on_the_innermost(cx: &mut TestAppContext) {
    let mut board = Board::open(cx);
    board.start_on_a();
    // Card `b` is 40..80 down: its top quarter, its middle, then its bottom.
    board.move_to(point(px(20.), px(45.)), true);
    board.move_to(point(px(20.), px(60.)), true);
    board.move_to(point(px(20.), px(78.)), true);
    board.release(point(px(20.), px(78.)));

    assert_eq!(
        board.log(),
        "drag:ticket:a:started over:b:top over:b:middle over:b:bottom \
         drop:b:ticket:a:bottom over:b:null drag:ticket:a:ended"
    );
}

/// The drag keeps going when the pointer leaves the source and the column it
/// sits in, and the target it leaves hears so.
#[gpui::test]
fn a_drag_continues_outside_its_source_and_moves_between_targets(cx: &mut TestAppContext) {
    let mut board = Board::open(cx);
    board.start_on_a();
    board.move_to(point(px(20.), px(60.)), true);
    board.move_to(point(px(200.), px(100.)), true);
    board.release(point(px(200.), px(100.)));

    assert_eq!(
        board.log(),
        "drag:ticket:a:started over:b:middle over:b:null over:done:middle \
         drop:done:ticket:a:middle over:done:null drag:ticket:a:ended"
    );
}

/// The Escape that cancels a drag is the drag's: the view's own binding for it
/// does not run as well.
#[gpui::test]
fn escape_cancels_a_drag_and_the_release_drops_nothing(cx: &mut TestAppContext) {
    let mut board = Board::open(cx);
    board.context.simulate_keystrokes("escape");
    board.draw();
    assert_eq!(board.log(), "back");

    board.start_on_a();
    board.move_to(point(px(20.), px(60.)), true);
    board.context.simulate_keystrokes("escape");
    board.draw();
    board.release(point(px(20.), px(60.)));

    assert_eq!(
        board.log(),
        "back drag:ticket:a:started over:b:middle over:b:null drag:ticket:a:cancelled"
    );
}

#[gpui::test]
fn a_release_outside_every_target_cancels(cx: &mut TestAppContext) {
    let mut board = Board::open(cx);
    board.start_on_a();
    board.move_to(point(px(350.), px(250.)), true);
    board.release(point(px(350.), px(250.)));

    assert_eq!(board.log(), "drag:ticket:a:started drag:ticket:a:cancelled");
}

/// A press and release without moving is a click, on a source as anywhere.
#[gpui::test]
fn a_source_still_clicks(cx: &mut TestAppContext) {
    let mut board = Board::open(cx);
    board.press(point(px(20.), px(20.)));
    board.release(point(px(20.), px(20.)));

    assert_eq!(board.log(), "click:a");
}

/// A host's UI driver finds sources and targets by the script's own ids.
#[gpui::test]
fn the_window_lists_its_sources_and_targets(cx: &mut TestAppContext) {
    let board = Board::open(cx);
    let mut context = board.context;
    let elements = context.update(|window, _| crate::drag_elements(window));
    let named = |name: &str| {
        elements
            .iter()
            .find(|element| {
                element.path.last() == Some(&gpui::ElementId::Name(name.to_owned().into()))
            })
            .unwrap_or_else(|| panic!("{name} is listed: {elements:?}"))
            .clone()
    };
    let a = named("a");
    assert!(a.source && a.target);
    assert_eq!(a.bounds.size.height, px(40.));
    let done = named("done");
    assert!(!done.source && done.target);
    assert!(
        named("b")
            .path
            .contains(&gpui::ElementId::Name("todo".into())),
        "a card's path runs through its column"
    );
}
