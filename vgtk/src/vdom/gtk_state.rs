use std::collections::HashMap;
use std::collections::HashSet;

use glib::{prelude::*, Object, SignalHandlerId};
use gtk::{
    self, prelude::*, Bin, Box as GtkBox, Builder, Container, Dialog, Menu, MenuButton, MenuItem,
    Widget, Window,
};

use super::State;
use crate::component::Component;
use crate::scope::Scope;
use crate::vnode::{VHandler, VNode, VProperty, VWidget};

pub struct GtkState<Model: Component> {
    pub object: Widget,
    handlers: HashMap<(&'static str, &'static str), SignalHandlerId>,
    children: Vec<State<Model>>,
}

fn build_obj<A: IsA<Object>, Model: Component>(spec: &VWidget<Model>) -> A {
    let class = spec.object_type;
    let obj = if let Some(ref cons) = spec.constructor {
        cons()
    } else {
        let mut ui = String::new();
        ui += &format!("<interface><object class=\"{}\"", class);
        ui += "/></interface>";

        let builder = Builder::new_from_string(&ui);
        let objects = builder.get_objects();
        objects
            .last()
            .unwrap_or_else(|| panic!("unknown class {}", class))
            .clone()
    };
    obj.downcast::<A>()
        .unwrap_or_else(|_| panic!("build_obj: cannot cast {} to {}", class, A::static_type()))
}

// Gtk has many strange ways of adding children to a parent.
fn add_child<Model: Component>(
    parent: &Container,
    index: usize,
    total: usize,
    child_spec: &VNode<Model>,
    child: &Widget,
) {
    if let Some(button) = parent.downcast_ref::<MenuButton>() {
        // MenuButton: can only have a single child, either a `Menu` set with
        // `set_popup` or any other `Widget` set with `set_popover`.
        if total > 1 {
            panic!(
                "MenuButton can only have 1 child, but {} were found.",
                total,
            );
        }
        if let Some(menu) = child.downcast_ref::<Menu>() {
            button.set_popup(Some(menu));
        } else {
            button.set_popover(Some(child));
        }
    } else if let Some(item) = parent.downcast_ref::<MenuItem>() {
        // MenuItem: single child, must be a `Menu`, set with `set_submenu`.
        if total > 1 {
            panic!("MenuItem can only have 1 child, but {} were found.", total);
        }
        if let Some(menu) = child.downcast_ref::<Menu>() {
            item.set_submenu(Some(menu));
        } else {
            panic!(
                "MenuItem can only take children of type Menu, but {} was found.",
                child.get_type()
            );
        }
    } else if let Some(dialog) = parent.downcast_ref::<Dialog>() {
        // Dialog: children must be added to the Dialog's content area through
        // get_content_area().
        dialog.get_content_area().add(child);
    } else if let Some(window) = parent.downcast_ref::<Window>() {
        // Window: if 1 child, it's the window's main widget. If 2 children, the
        // first is the title bar and the second is the main widget. More than 2
        // is not ok.
        if total > 2 {
            panic!(
                "Windows can only have 1 or 2 children, but {} were found.",
                total
            );
        }
        if total == 2 && index == 0 {
            window.set_titlebar(Some(child));
        } else {
            window.add(child);
        }
    } else if let Some(parent) = parent.downcast_ref::<Bin>() {
        // Bin: can only have a single child.
        if total > 1 {
            panic!("Bins can only have 1 child, but {} were found.", total);
        }
        parent.add(child);
    } else if let Some(parent) = parent.downcast_ref::<GtkBox>() {
        // Box: added normally, except one widget can be added using
        // set_center_widget() if it has the center_widget=true child property
        // (which is faked in ext.rs). More than one child with this property is
        // undefined behaviour.
        if child_spec.get_child_prop("center_widget").is_some() {
            parent.set_center_widget(Some(child));
        } else {
            parent.add(child);
        }
    } else {
        parent.add(child);
    }
    // Apply child properties
    for prop in child_spec.get_child_props() {
        (prop.set)(child.upcast_ref(), Some(parent), true);
    }
}

impl<Model: 'static + Component> GtkState<Model> {
    pub fn build(vobj: &VWidget<Model>, parent: Option<&Container>, scope: &Scope<Model>) -> Self {
        // Build this object
        let object = build_obj::<Widget, _>(vobj);

        // Apply properties
        for prop in &vobj.properties {
            (prop.set)(object.upcast_ref(), parent, true);
        }

        // Apply handlers
        let mut handlers = HashMap::new();
        for handler in &vobj.handlers {
            let handle = (handler.set)(object.upcast_ref(), scope);
            handlers.insert((handler.name, handler.id), handle);
        }

        let mut state = GtkState {
            object: object.clone(),
            handlers,
            children: Vec::new(),
        };

        // Build children
        if let Some(parent) = object.downcast_ref::<Container>() {
            let total_children = vobj.children.len();
            for (index, child_spec) in vobj.children.iter().enumerate() {
                let child = State::build(child_spec, Some(parent), scope);
                let widget = child.object().clone();
                add_child(parent, index, total_children, child_spec, &widget);
                state.children.push(child);
            }
        } else if !vobj.children.is_empty() {
            panic!(
                "vnode has children but object type is {:?} which isn't a Container",
                vobj.object_type
            );
        }

        // Show this object
        state.object.show();

        state
    }

    pub fn patch(
        &mut self,
        vobj: &VWidget<Model>,
        parent: Option<&Container>,
        scope: &Scope<Model>,
    ) -> bool {
        // Patch children
        if let Some(parent) = self.object.downcast_ref::<Container>() {
            let mut to_remove = None;
            let mut to_append = Vec::new();
            let mut reconstruct_from = None;
            for index in 0..(self.children.len().max(vobj.children.len())) {
                match (self.children.get_mut(index), vobj.children.get(index)) {
                    (Some(State::Component(target)), Some(spec_item)) => {
                        match spec_item {
                            VNode::Widget(_) => {
                                // Component has become a widget; reconstruct from here
                                reconstruct_from = Some(index);
                                break;
                            }
                            VNode::Component(ref spec) => {
                                if !target.patch(spec, Some(parent), scope) {
                                    reconstruct_from = Some(index);
                                    break;
                                }
                            }
                        }
                    }
                    (Some(State::Gtk(target)), Some(spec_item)) => {
                        match spec_item {
                            VNode::Widget(ref spec) => {
                                if target.object.get_type() == spec.object_type {
                                    // Objects have same type; patch down
                                    target.patch(spec, Some(&parent), scope);
                                } else {
                                    // Objects are different, need to reconstruct everything from here
                                    reconstruct_from = Some(index);
                                    break;
                                }
                            }
                            VNode::Component(_) => {
                                // Gtk object has turned into a component; reconstruct from here
                                reconstruct_from = Some(index);
                                break;
                            }
                        }
                    }
                    (Some(_), None) => {
                        // Extraneous Gtk object; delete
                        if to_remove.is_none() {
                            to_remove = Some(index);
                        }
                        break;
                    }
                    (None, Some(spec)) => {
                        // New spec; construct
                        let state = State::build(spec, Some(&parent), scope);
                        add_child(parent, index, vobj.children.len(), spec, state.object());
                        to_append.push(state);
                    }
                    (None, None) => break,
                }
            }
            if let Some(index) = reconstruct_from {
                // Remove all previous children from here onwards
                if self.object.is::<Window>() && index == 0 && self.children.len() == 2 {
                    panic!("Can't remove a title bar widget from an existing Window!");
                }
                for child in self.children.drain(index..) {
                    parent.remove(child.object());
                }
                // Rebuild children from new specs
                for (index, child_spec) in vobj.children.iter().enumerate().skip(index) {
                    let state = State::build(child_spec, Some(&parent), scope);
                    add_child(
                        parent,
                        index,
                        vobj.children.len(),
                        child_spec,
                        state.object(),
                    );
                    state.object().show();
                    self.children.push(state);
                }
            } else {
                // Remove children flagged as extraneous
                if let Some(remove_from) = to_remove {
                    if self.object.is::<Window>() && remove_from == 1 && self.children.len() == 2 {
                        panic!("Can't remove a title bar widget from an existing Window!");
                    }
                    for child in self.children.drain(remove_from..) {
                        parent.remove(child.object());
                    }
                }
                // Or append newly constructed children
                if self.object.is::<Window>() && !to_append.is_empty() && self.children.len() == 1 {
                    panic!("Can't add a title bar widget to an existing Window!");
                }
                for child in to_append {
                    child.object().show();
                    self.children.push(child);
                }
            }
        }

        // Patch properties
        self.patch_properties(&vobj.properties, parent);

        // Patch child properties
        self.patch_properties(&vobj.child_props, parent);

        // Patch handlers
        self.patch_handlers(&vobj.handlers, scope);

        true
    }

    fn patch_properties(&mut self, properties: &[VProperty], parent: Option<&Container>) {
        for prop in properties {
            (prop.set)(self.object.upcast_ref(), parent, false);
        }
    }

    fn patch_handlers(&mut self, handlers: &[VHandler<Model>], scope: &Scope<Model>) {
        // FIXME need to store and match IDs
        let mut seen = HashSet::new();
        let mut remove = Vec::new();
        for handler in handlers {
            let key = (handler.name, handler.id);
            seen.insert(key.clone());
            if let std::collections::hash_map::Entry::Vacant(entry) = self.handlers.entry(key) {
                let handle = (handler.set)(self.object.upcast_ref(), scope);
                entry.insert(handle);
            }
        }
        for key in self.handlers.keys() {
            if !seen.contains(key) {
                remove.push(key.clone());
            }
        }
        for key in remove {
            let obj: &Object = self.object.upcast_ref();
            obj.disconnect(self.handlers.remove(&key).unwrap());
        }
    }
}