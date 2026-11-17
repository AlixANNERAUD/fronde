//! A page under test, live: built natively, its elements found by their `id`, events sent to
//! them as a browser would, and what it sent kept.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use dioxus::{
    dioxus_core::{AttributeValue, ElementId, Mutation, Mutations},
    html::{
        Code, Key, Location, Modifiers, PlatformEventData, SerializedFormData,
        SerializedKeyboardData, SerializedMouseData,
    },
    prelude::*,
};
use fronde_protocol::ClientMsg;

use crate::socket::Outgoing;

thread_local! {
    /// What the page under test sent.
    static SENT: RefCell<Vec<ClientMsg>> = const { RefCell::new(Vec::new()) };
}

/// What the page sent since the last [`Live::new`].
pub fn sent() -> Vec<ClientMsg> {
    SENT.with_borrow(Clone::clone)
}

/// Keeps a message the page sent, and says it went out.
pub fn deliver(outgoing: Outgoing) {
    SENT.with_borrow_mut(|sent| sent.push(outgoing.message));
    if let Some(sent) = outgoing.sent {
        sent.call(());
    }
}

pub struct Live {
    pub dom: VirtualDom,
    ids: HashMap<String, ElementId>,
    listeners: HashSet<(String, ElementId)>,
}

impl Live {
    pub fn new(mut dom: VirtualDom) -> Self {
        // Events are built from plain data, as they would be from a browser's.
        dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
        SENT.with_borrow_mut(Vec::clear);
        let mut mutations = Mutations::default();
        dom.rebuild(&mut mutations);
        let mut live = Self {
            dom,
            ids: HashMap::new(),
            listeners: HashSet::new(),
        };
        live.absorb(mutations);
        live
    }

    /// Learns which element has which `id` and listens to what.
    fn absorb(&mut self, mutations: Mutations) {
        for edit in mutations.edits {
            match edit {
                Mutation::NewEventListener { name, id } => {
                    self.listeners.insert((name, id));
                }
                Mutation::RemoveEventListener { name, id } => {
                    self.listeners.remove(&(name, id));
                }
                Mutation::SetAttribute {
                    name: "id",
                    value: AttributeValue::Text(text),
                    id,
                    ..
                } => {
                    self.ids.insert(text, id);
                }
                _ => {}
            }
        }
    }

    /// The element `id`, as the page made it last.
    pub fn element(&self, id: &str) -> ElementId {
        *self
            .ids
            .get(id)
            .unwrap_or_else(|| panic!("no element {id}:\n{}", self.html()))
    }

    /// Sends the event to element `id`, which must listen to it.
    pub fn send(&mut self, id: &str, name: &str, data: impl std::any::Any) {
        let element = self.element(id);
        assert!(
            self.listeners.contains(&(name.to_owned(), element)),
            "{id} does not listen to {name}"
        );
        let data: Rc<dyn std::any::Any> = Rc::new(PlatformEventData::new(Box::new(data)));
        self.dom
            .runtime()
            .handle_event(name, Event::new(data, true), element);
        self.update();
    }

    /// Renders what changed, runs the tasks and effects it queued, then renders what they
    /// changed, as the browser's event loop would.
    pub fn update(&mut self) {
        for _ in 0..2 {
            let mut mutations = Mutations::default();
            self.dom.render_immediate(&mut mutations);
            self.absorb(mutations);
            self.dom.process_events();
        }
        let mut mutations = Mutations::default();
        self.dom.render_immediate(&mut mutations);
        self.absorb(mutations);
    }

    pub fn click(&mut self, id: &str) {
        self.send(id, "click", SerializedMouseData::default());
    }

    pub fn type_text(&mut self, id: &str, text: &str) {
        self.send(
            id,
            "input",
            SerializedFormData::new(text.to_owned(), Vec::new()),
        );
    }

    pub fn key(&mut self, id: &str, key: Key, code: Code) {
        let data = SerializedKeyboardData::new(
            key,
            code,
            Location::Standard,
            false,
            Modifiers::empty(),
            false,
        );
        self.send(id, "keydown", data);
    }

    /// A key pressed in element `id`, which need not listen itself: the event bubbles up to
    /// whoever does.
    pub fn key_in(&mut self, id: &str, key: Key, code: Code) {
        let element = self.element(id);
        let data = SerializedKeyboardData::new(
            key,
            code,
            Location::Standard,
            false,
            Modifiers::empty(),
            false,
        );
        let data: Rc<dyn std::any::Any> = Rc::new(PlatformEventData::new(Box::new(data)));
        self.dom
            .runtime()
            .handle_event("keydown", Event::new(data, true), element);
        self.update();
    }

    pub fn html(&self) -> String {
        dioxus_ssr::render(&self.dom)
    }
}
