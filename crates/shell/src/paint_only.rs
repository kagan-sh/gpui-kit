//! A view that may only paint.
//!
//! A host draws some script views over its own interface: a decoration over a
//! terminal, say. Such a view must never take a click, a key, the focus, or a
//! place above the host's own popups. Hiding its hitboxes is not enough — a
//! handler such as `on_mouse_down_out` runs without one, a popover paints in a
//! later pass, and a focus handle moves the keyboard at once. So the rule is
//! checked on what the script asks for, not on what GPUI happens to do with it.
//!
//! A paint-only view is mounted with [`crate::ShellRuntime::mount_paint_only_view`].
//! Every description it builds is checked here: boxes, text, images, svgs and
//! paths, with their styles and motion, pass; anything else fails the view for
//! good. Its code may not reach the window either ([`crate::scope::with_current`]).
//! Either breach leaves the view painting nothing, with the reason in
//! [`crate::ScriptView::build_error`].

use crate::{
    snapshot::RenderSnapshot,
    spec::{Component, SpecOp},
    style,
};

pub(crate) const WINDOW: &str = "a paint-only view cannot use the window: no focus, dialogs, \
     sheets, notifications, key bindings, window changes, window reads or input state";

/// The first thing in `snapshot` a paint-only view may not describe, worded for
/// the plugin's author.
pub(crate) fn refusal(snapshot: &RenderSnapshot) -> Option<String> {
    let arena = snapshot.arena();
    let mut pending = vec![snapshot.root()];
    while let Some(id) = pending.pop() {
        let Some(node) = arena.node(id) else {
            continue;
        };
        let refused = node
            .component()
            .and_then(refused_component)
            .or_else(|| node.ops().iter().find_map(refused_op));
        if let Some(what) = refused {
            return Some(format!(
                "a paint-only view cannot use `{what}`; it may only draw boxes, text, images, \
                 svgs and paths, with their styles and motion"
            ));
        }
        pending.extend(node.children().iter().copied());
    }
    None
}

fn refused_component(component: &Component) -> Option<String> {
    match component {
        Component::Div
        | Component::HFlex
        | Component::VFlex
        | Component::Text(_)
        | Component::Svg(_)
        | Component::Image(_)
        | Component::Path { .. } => None,
        other => {
            let named = format!("{other:?}");
            let end = named
                .find(|c: char| !c.is_alphanumeric())
                .unwrap_or(named.len());
            Some(named[..end].to_owned())
        }
    }
}

fn refused_op(op: &SpecOp) -> Option<String> {
    match op {
        SpecOp::NullaryStyle(index) => {
            let name = style::nullary_name(*index);
            takes_input(name).then(|| name.to_owned())
        }
        SpecOp::ParamStyle(name, _) => takes_input(name).then(|| (*name).to_owned()),
        SpecOp::Method(name, _) => (!matches!(
            *name,
            "id" | "transition"
                | "spring"
                | "move_to"
                | "line_to"
                | "curve_to"
                | "cubic_bezier_to"
                | "arc_to"
                | "close"
                | "dash_array"
        ))
        .then(|| (*name).to_owned()),
        SpecOp::RegisteredMethod(method) => Some(method.name().to_owned()),
        SpecOp::Callback(name, _) | SpecOp::StateStyle(name, _) | SpecOp::Slot(name, _) => {
            Some((*name).to_owned())
        }
        SpecOp::ActionCallback(name, _) => Some(format!("on_action({name})")),
    }
}

/// A style that makes GPUI give the element a hitbox: a cursor, a scroll area,
/// or a block on the mouse.
fn takes_input(name: &str) -> bool {
    name.starts_with("cursor")
        || name.contains("scroll")
        || name.contains("occlude")
        || name.contains("block_mouse")
}
