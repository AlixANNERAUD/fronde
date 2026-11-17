//! A request's card: questions with options, answered once from any page. The page checks the
//! answer with the server's own rules before sending it; the server stays the judge.

use dioxus::html::Key;
use dioxus::prelude::*;
use fronde_client::RequestStatus;
use fronde_protocol::request::check;
use fronde_protocol::{
    Card, ChoiceAnswer, ChoiceOption, ClientMsg, Field, Invalid, Outcome, RequestId, Response,
};
use std::collections::BTreeMap;

use crate::{
    components::ui::{
        badge::{Badge, BadgeSize, BadgeVariant},
        button::{Button, ButtonVariant},
        card::{Card as CardBox, CardContent, CardDescription, CardFooter, CardHeader, CardSize},
        checkbox::Checkbox,
        field::{
            Field as FieldRow, FieldContent, FieldDescription, FieldError, FieldLabel, FieldLegend,
            FieldSet, FieldTitle, FieldVariant,
        },
        input::Input,
        radio_button::{RadioGroup, RadioGroupItem},
        textarea::Textarea,
    },
    socket::Outgoing,
};

/// The value of the "Autre…" radio: it starts with a control character, which the server
/// refuses in labels.
const OTHER: &str = "\u{1f}autre";
/// Full width on a phone, beside the conversation's margin from `sm:` up.
const CARD_CLASS: &str = "w-full sm:max-w-[85%]";
/// Tap targets of at least 44 px on a phone.
const BUTTON_CLASS: &str = "h-11 w-full sm:h-9 sm:w-auto";

/// What the user did with one question so far.
#[derive(Clone, Debug, Default, PartialEq)]
struct Draft {
    /// The options ticked, for a question with several answers, in the order they were ticked.
    ticked: Vec<String>,
    /// "Autre…" ticked, for a question with several answers.
    other_ticked: bool,
    other: String,
    note: String,
}

#[component]
pub fn RequestCard(
    id: RequestId,
    card: Card,
    from: Option<String>,
    status: RequestStatus,
    refusal: Option<String>,
    /// Who answered, on a closed card, when it was not this page's user.
    by: Option<String>,
    /// The socket is open: an answer can go out.
    open: bool,
    on_send: EventHandler<Outgoing>,
) -> Element {
    let Card::Form {
        message,
        fields,
        submit,
    } = card;
    let title = asker(from.as_deref(), fields.len());
    match status {
        RequestStatus::Open => rsx! {
            FormCard {
                id,
                title,
                message,
                fields,
                submit,
                refusal,
                open,
                on_send,
            }
        },
        RequestStatus::Closed(outcome) => rsx! {
            CardBox { size: CardSize::Sm, class: CARD_CLASS,
                CardHeader {
                    CardDescription { class: "text-xs font-medium", "{title}" }
                }
                CardContent { class: "flex flex-col gap-1",
                    for field in fields.iter() {
                        p { class: "text-muted-foreground text-sm wrap-break-word", {question(field)} }
                    }
                }
                CardFooter { class: "flex-wrap gap-2",
                    {closed_badge(&outcome)}
                    if let Some(by) = by {
                        span { class: "text-muted-foreground text-xs", "par {by}" }
                    }
                }
            }
        },
    }
}

/// Who asks, as the card's header says it. An outside client is quoted and marked as one: it
/// chose its name, which must never read as Fronde's own.
fn asker(from: Option<&str>, questions: usize) -> String {
    let what = if questions > 1 {
        "Questions"
    } else {
        "Question"
    };
    match from {
        None => format!("{what} de Fronde"),
        Some(name) => format!("{what} de « {name} » (client MCP)"),
    }
}

fn question(field: &Field) -> &str {
    let Field::Choice { question, .. } = field;
    question
}

fn closed_badge(outcome: &Outcome) -> Element {
    match outcome {
        Outcome::Answered { summary } if summary.is_empty() => rsx! {
            Badge { "Répondu" }
        },
        Outcome::Answered { summary } => rsx! {
            Badge { class: "whitespace-normal", "Réponse : {summary}" }
        },
        Outcome::Declined => rsx! {
            Badge { variant: BadgeVariant::Destructive, "Refusée" }
        },
        Outcome::Expired => rsx! {
            Badge { variant: BadgeVariant::Muted, "Expirée" }
        },
        Outcome::Cancelled => rsx! {
            Badge { variant: BadgeVariant::Muted, "Annulée" }
        },
    }
}

/// The answer the drafts make: questions left blank are left out, for [`check`] to name.
fn response(fields: &[Field], radios: &[String], drafts: &[Draft]) -> Response {
    let mut values = BTreeMap::new();
    for ((field, radio), draft) in fields.iter().zip(radios).zip(drafts) {
        let Field::Choice {
            key,
            options,
            multi,
            ..
        } = field;
        let (selected, other) = if *multi {
            let selected = options
                .iter()
                .filter(|option| draft.ticked.contains(&option.label))
                .map(|option| option.label.clone())
                .collect();
            (selected, draft.other_ticked.then(|| draft.other.clone()))
        } else {
            match radio.as_str() {
                "" => (Vec::new(), None),
                OTHER => (Vec::new(), Some(draft.other.clone())),
                label => (vec![label.to_owned()], None),
            }
        };
        let note = Some(draft.note.trim().to_owned()).filter(|note| !note.is_empty());
        if selected.is_empty() && other.is_none() && note.is_none() {
            continue;
        }
        values.insert(
            key.clone(),
            ChoiceAnswer {
                selected,
                other,
                note,
            },
        );
    }
    Response::Form { values }
}

#[component]
fn FormCard(
    id: RequestId,
    title: String,
    message: String,
    fields: Vec<Field>,
    submit: Option<String>,
    refusal: Option<String>,
    open: bool,
    on_send: EventHandler<Outgoing>,
) -> Element {
    // One radio value per question, the drafts of the rest; a card's fields never change.
    let radios: Vec<Signal<String>> =
        use_hook(|| fields.iter().map(|_| Signal::new(String::new())).collect());
    let drafts = use_signal(|| vec![Draft::default(); fields.len()]);
    let mut invalid = use_signal(|| None::<Invalid>);
    let mut sent = use_signal(|| false);
    let delivered = use_callback(move |()| sent.set(true));
    // Sent: wait for the server to close the card, unless it refused the answer.
    let disabled = !open || sent() && refusal.is_none();
    let card = Card::Form {
        message: message.clone(),
        fields: fields.clone(),
        submit: submit.clone(),
    };
    let send = use_callback({
        let radios = radios.clone();
        let fields = fields.clone();
        move |()| {
            let values: Vec<String> = radios.iter().map(|radio| radio()).collect();
            let response = response(&fields, &values, &drafts.read());
            if let Err(error) = check(&card, &response) {
                invalid.set(Some(error));
                return;
            }
            invalid.set(None);
            on_send.call(Outgoing {
                message: ClientMsg::Answer { id, response },
                sent: Some(delivered),
            });
        }
    });
    let decline = move |_| {
        invalid.set(None);
        on_send.call(Outgoing {
            message: ClientMsg::Answer {
                id,
                response: Response::Decline,
            },
            sent: Some(delivered),
        });
    };
    let error_for = |key: &str| {
        invalid
            .read()
            .as_ref()
            .filter(|error| error.key.as_deref() == Some(key))
            .map(|error| error.message.clone())
    };
    let card_error = invalid
        .read()
        .as_ref()
        .filter(|error| error.key.is_none())
        .map(|error| error.message.clone())
        .or(refusal);
    rsx! {
        CardBox { size: CardSize::Sm, class: CARD_CLASS,
            CardHeader {
                CardDescription { class: "text-xs font-medium", "{title}" }
            }
            CardContent { class: "flex flex-col gap-6",
                if !message.is_empty() {
                    p { class: "text-sm whitespace-pre-wrap wrap-break-word", "{message}" }
                }
                for (index , field) in fields.iter().enumerate() {
                    ChoiceField {
                        key: "{index}",
                        request: id,
                        index,
                        field: field.clone(),
                        radio: radios[index],
                        drafts,
                        error: error_for(field.key()),
                        disabled,
                        on_submit: send,
                    }
                }
                if let Some(error) = card_error {
                    FieldError { "{error}" }
                }
            }
            CardFooter { class: "flex-col-reverse items-stretch sm:flex-row sm:items-center sm:justify-end",
                Button {
                    id: "r{id}-decline",
                    variant: ButtonVariant::Outline,
                    class: BUTTON_CLASS,
                    disabled,
                    onclick: decline,
                    "Refuser"
                }
                Button {
                    id: "r{id}-send",
                    class: BUTTON_CLASS,
                    disabled,
                    onclick: move |_| send.call(()),
                    {submit.unwrap_or_else(|| "Envoyer".into())}
                }
            }
        }
    }
}

/// One question: its options as radios (one answer) or boxes (several), "Autre…" with its
/// text, a note, and the picked option's preview: under it on a phone, beside the options from
/// `md:`.
#[component]
fn ChoiceField(
    request: RequestId,
    index: usize,
    field: Field,
    radio: Signal<String>,
    drafts: Signal<Vec<Draft>>,
    error: Option<String>,
    disabled: bool,
    /// Enter in the options or in "Autre…": the card's "Envoyer".
    on_submit: Callback<()>,
) -> Element {
    let Field::Choice {
        header,
        question,
        options,
        multi,
        other,
        ..
    } = field;
    let mut drafts = drafts;
    let mut radio = radio;
    let mut noting = use_signal(|| false);
    let draft = drafts.read()[index].clone();
    let prefix = format!("r{request}-q{index}");
    let option_id = |n: usize| format!("{prefix}-o{n}");
    let other_id = format!("{prefix}-other");
    let picked = |option: &ChoiceOption| {
        if multi {
            draft.ticked.contains(&option.label)
        } else {
            radio() == option.label
        }
    };
    // The preview of the option picked last that has one.
    let preview = if multi {
        draft
            .ticked
            .iter()
            .rev()
            .find_map(|label| options.iter().find(|option| option.label == *label))
            .and_then(|option| option.preview.clone())
    } else {
        options
            .iter()
            .find(|option| option.label == radio())
            .and_then(|option| option.preview.clone())
    };
    let previews = options.iter().any(|option| option.preview.is_some());
    let other_open = if multi {
        draft.other_ticked
    } else {
        radio() == OTHER
    };
    // Arrow keys move the pick within a question with one answer, as in a native radio group;
    // Enter sends the card, as in a form. The "Autre…" text box sits outside the options, so
    // its own arrow keys move its caret.
    let values: Vec<String> = options
        .iter()
        .map(|option| option.label.clone())
        .chain(other.then(|| OTHER.to_owned()))
        .collect();
    let ids: Vec<String> = (0..options.len())
        .map(option_id)
        .chain(other.then(|| other_id.clone()))
        .collect();
    let on_key = move |event: KeyboardEvent| {
        if disabled || event.is_composing() {
            return;
        }
        if event.key() == Key::Enter {
            event.prevent_default();
            on_submit.call(());
            return;
        }
        if multi {
            return;
        }
        let step: isize = match event.key() {
            Key::ArrowDown | Key::ArrowRight => 1,
            Key::ArrowUp | Key::ArrowLeft => -1,
            _ => return,
        };
        event.prevent_default();
        let count = values.len() as isize;
        let next = match values.iter().position(|value| *value == radio()) {
            Some(at) => (at as isize + step).rem_euclid(count),
            None if step > 0 => 0,
            None => count - 1,
        } as usize;
        radio.set(values[next].clone());
        focus(&ids[next]);
    };
    let rows = rsx! {
        for (n , option) in options.iter().enumerate() {
            div { key: "{n}", class: "flex flex-col gap-2",
                FieldLabel { html_for: option_id(n), class: "min-h-11 w-full",
                    FieldRow { variant: FieldVariant::Horizontal,
                        if multi {
                            Checkbox {
                                id: option_id(n),
                                checked: picked(option),
                                disabled,
                                aria_label: option.label.clone(),
                                on_checked_change: {
                                    let label = option.label.clone();
                                    move |on: bool| {
                                        let ticked = &mut drafts.write()[index].ticked;
                                        ticked.retain(|other| *other != label);
                                        if on {
                                            ticked.push(label.clone());
                                        }
                                    }
                                },
                            }
                        } else {
                            RadioGroupItem { id: option_id(n), value: option.label.clone(), disabled }
                        }
                        FieldContent {
                            FieldTitle { class: "wrap-break-word",
                                "{option.label}"
                                if option.recommended {
                                    Badge { variant: BadgeVariant::Outline, size: BadgeSize::Sm, "Recommandé" }
                                }
                            }
                            if let Some(description) = &option.description {
                                FieldDescription { "{description}" }
                            }
                        }
                    }
                }
                if let Some(text) = option.preview.as_ref().filter(|_| picked(option)) {
                    pre { class: "bg-muted overflow-x-auto rounded-md p-3 font-mono text-xs md:hidden",
                        "{text}"
                    }
                }
            }
        }
        if other {
            FieldLabel { html_for: other_id.clone(), class: "min-h-11 w-full",
                FieldRow { variant: FieldVariant::Horizontal,
                    if multi {
                        Checkbox {
                            id: other_id.clone(),
                            checked: draft.other_ticked,
                            disabled,
                            aria_label: "Autre",
                            on_checked_change: move |on: bool| drafts.write()[index].other_ticked = on,
                        }
                    } else {
                        RadioGroupItem { id: other_id.clone(), value: OTHER, disabled }
                    }
                    FieldContent {
                        FieldTitle { "Autre…" }
                    }
                }
            }
        }
    };
    let other_text = rsx! {
        if other && other_open {
            // The box's keys bubble here, never to the options' arrow keys.
            div {
                onkeydown: move |event: KeyboardEvent| {
                    if event.key() == Key::Enter && !event.is_composing() && !disabled {
                        event.prevent_default();
                        on_submit.call(());
                    }
                },
                Input {
                    id: "{other_id}-text",
                    value: draft.other.clone(),
                    placeholder: "Ta réponse",
                    aria_label: "Autre réponse",
                    autocomplete: "off",
                    disabled,
                    class: "h-11 md:h-9",
                    oninput: move |event: FormEvent| drafts.write()[index].other = event.value(),
                }
            }
        }
    };
    let note_id = format!("{prefix}-note");
    rsx! {
        FieldSet { class: "gap-3",
            FieldLegend { class: "mb-0 flex flex-col items-start gap-2",
                if let Some(header) = header {
                    Badge { variant: BadgeVariant::Secondary, "{header}" }
                }
                span { class: "wrap-break-word", "{question}" }
            }
            div { class: if previews { "grid gap-3 md:grid-cols-2" } else { "flex flex-col gap-3" },
                div { class: "flex flex-col gap-2",
                    div { id: "{prefix}-options", class: "flex flex-col gap-2", onkeydown: on_key,
                        if multi {
                            div { role: "group", class: "flex flex-col gap-2", {rows} }
                        } else {
                            RadioGroup { value: radio, class: "gap-2", {rows} }
                        }
                    }
                    {other_text}
                }
                if previews {
                    pre { class: "bg-muted hidden overflow-x-auto rounded-md p-3 font-mono text-xs md:block",
                        {preview.unwrap_or_else(|| "Choisis une option pour voir l'aperçu.".into())}
                    }
                }
            }
            if let Some(error) = error {
                FieldError { "{error}" }
            }
            Button {
                id: "{note_id}-toggle",
                variant: ButtonVariant::Ghost,
                class: "text-muted-foreground h-11 self-start px-2 sm:h-8",
                onclick: move |_| noting.toggle(),
                if noting() {
                    "Retirer la précision"
                } else {
                    "Ajouter une précision"
                }
            }
            if noting() {
                label { r#for: "{note_id}", class: "sr-only", "Précision" }
                Textarea {
                    id: note_id.clone(),
                    value: draft.note.clone(),
                    placeholder: "Une précision pour Fronde",
                    rows: 2,
                    disabled,
                    oninput: move |event: FormEvent| drafts.write()[index].note = event.value(),
                }
            }
        }
    }
}

/// Moves the keyboard focus to the element `id`.
#[cfg(target_arch = "wasm32")]
fn focus(id: &str) {
    use wasm_bindgen::JsCast;
    let element = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok());
    if let Some(element) = element {
        let _ = element.focus();
    }
}

/// Natively there is no focus to move.
#[cfg(not(target_arch = "wasm32"))]
fn focus(_id: &str) {}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::{HashMap, HashSet},
        rc::Rc,
    };

    use dioxus::{
        dioxus_core::{AttributeValue, ElementId, Mutation, Mutations},
        html::{
            Code, Location, Modifiers, PlatformEventData, SerializedFormData,
            SerializedKeyboardData, SerializedMouseData,
        },
    };

    use super::*;

    const ID: RequestId = 7;

    thread_local! {
        /// What the card under test sent.
        static SENT: RefCell<Vec<ClientMsg>> = const { RefCell::new(Vec::new()) };
    }

    fn sent() -> Vec<ClientMsg> {
        SENT.with_borrow(Clone::clone)
    }

    fn option(label: &str, description: &str) -> ChoiceOption {
        ChoiceOption {
            label: label.into(),
            description: Some(description.into()),
            preview: None,
            recommended: false,
        }
    }

    fn choice(key: &str, multi: bool, options: Vec<ChoiceOption>) -> Field {
        Field::Choice {
            key: key.into(),
            header: Some("Appli".into()),
            question: "Quelle appli installer ?".into(),
            options,
            multi,
            other: true,
            required: true,
        }
    }

    fn form(fields: Vec<Field>) -> Card {
        Card::Form {
            message: String::new(),
            fields,
            submit: None,
        }
    }

    /// One question with one answer: Mealie (recommended, with a preview) or Seerr.
    fn single() -> Card {
        form(vec![choice(
            "q1",
            false,
            vec![
                ChoiceOption {
                    recommended: true,
                    preview: Some("[ Mealie ]".into()),
                    ..option("Mealie", "Recettes")
                },
                option("Seerr", "Demandes de films"),
            ],
        )])
    }

    fn multi() -> Card {
        form(vec![choice(
            "q1",
            true,
            vec![
                option("Jellyfin", "Films"),
                option("Immich", "Photos"),
                option("Paperless", "Papiers"),
            ],
        )])
    }

    fn answer(selected: &[&str], other: Option<&str>, note: Option<&str>) -> ClientMsg {
        ClientMsg::Answer {
            id: ID,
            response: Response::Form {
                values: [(
                    "q1".to_owned(),
                    ChoiceAnswer {
                        selected: selected.iter().map(|s| (*s).to_owned()).collect(),
                        other: other.map(str::to_owned),
                        note: note.map(str::to_owned),
                    },
                )]
                .into(),
            },
        }
    }

    #[component]
    fn Harness(card: Card, status: RequestStatus, refusal: Option<String>, open: bool) -> Element {
        rsx! {
            RequestCard {
                id: ID,
                card,
                from: None,
                status,
                refusal,
                open,
                on_send: move |outgoing: Outgoing| {
                    SENT.with_borrow_mut(|sent| sent.push(outgoing.message));
                    if let Some(sent) = outgoing.sent {
                        sent.call(());
                    }
                },
            }
        }
    }

    fn props(card: Card) -> HarnessProps {
        HarnessProps {
            card,
            status: RequestStatus::Open,
            refusal: None,
            open: true,
        }
    }

    fn render(props: HarnessProps) -> String {
        let mut dom = VirtualDom::new_with_props(Harness, props);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The card, live: events go to elements by their `id`.
    struct Live {
        dom: VirtualDom,
        ids: HashMap<String, ElementId>,
        listeners: HashSet<(String, ElementId)>,
    }

    impl Live {
        fn new(props: HarnessProps) -> Self {
            dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
            SENT.with_borrow_mut(Vec::clear);
            let mut live = Self {
                dom: VirtualDom::new_with_props(Harness, props),
                ids: HashMap::new(),
                listeners: HashSet::new(),
            };
            let mut mutations = Mutations::default();
            live.dom.rebuild(&mut mutations);
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

        fn send(&mut self, element: &str, name: &str, data: impl std::any::Any) {
            let id = *self
                .ids
                .get(element)
                .unwrap_or_else(|| panic!("no element {element}:\n{}", self.html()));
            assert!(
                self.listeners.contains(&(name.to_owned(), id)),
                "{element} does not listen to {name}"
            );
            let data: Rc<dyn std::any::Any> = Rc::new(PlatformEventData::new(Box::new(data)));
            self.dom
                .runtime()
                .handle_event(name, Event::new(data, true), id);
            let mut mutations = Mutations::default();
            self.dom.render_immediate(&mut mutations);
            self.absorb(mutations);
        }

        fn click(&mut self, element: &str) {
            self.send(element, "click", SerializedMouseData::default());
        }

        fn type_text(&mut self, element: &str, text: &str) {
            self.send(
                element,
                "input",
                SerializedFormData::new(text.to_owned(), Vec::new()),
            );
        }

        fn key(&mut self, element: &str, key: Key, code: Code) {
            self.send(element, "keydown", keyboard(key, code));
        }

        /// A key pressed in `element`, which need not listen itself: the event bubbles up to
        /// whoever does.
        fn key_in(&mut self, element: &str, key: Key, code: Code) {
            let id = *self
                .ids
                .get(element)
                .unwrap_or_else(|| panic!("no element {element}:\n{}", self.html()));
            let data: Rc<dyn std::any::Any> =
                Rc::new(PlatformEventData::new(Box::new(keyboard(key, code))));
            self.dom
                .runtime()
                .handle_event("keydown", Event::new(data, true), id);
            let mut mutations = Mutations::default();
            self.dom.render_immediate(&mut mutations);
            self.absorb(mutations);
        }

        fn html(&self) -> String {
            dioxus_ssr::render(&self.dom)
        }
    }

    fn keyboard(key: Key, code: Code) -> SerializedKeyboardData {
        SerializedKeyboardData::new(
            key,
            code,
            Location::Standard,
            false,
            Modifiers::empty(),
            false,
        )
    }

    /// The opening tag of the element `id`.
    fn tag<'a>(html: &'a str, id: &str) -> Option<&'a str> {
        let at = html.find(&format!(r#"id="{id}""#))?;
        let start = html[..at].rfind('<')?;
        let end = at + html[at..].find('>')?;
        Some(&html[start..end])
    }

    fn checked(html: &str, id: &str) -> bool {
        tag(html, id)
            .unwrap_or_else(|| panic!("no {id}"))
            .contains(r#"aria-checked="true""#)
    }

    fn disabled(html: &str, id: &str) -> bool {
        tag(html, id)
            .unwrap_or_else(|| panic!("no {id}"))
            .contains("disabled=true")
    }

    fn class_of<'a>(html: &'a str, id: &str) -> &'a str {
        let tag = tag(html, id).unwrap_or_else(|| panic!("no {id}"));
        tag.split(r#"class=""#)
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
    }

    #[test]
    fn question_card_renders_options_and_other() {
        let page = render(props(single()));
        for text in [
            "Question de Fronde",
            "Appli",
            "Quelle appli installer ?",
            "Mealie",
            "Recettes",
            "Seerr",
            "Demandes de films",
            "Autre…",
            "Ajouter une précision",
            "Envoyer",
            "Refuser",
        ] {
            assert!(page.contains(text), "{text}: {page}");
        }
        assert!(page.contains(r#"role="radiogroup""#), "{page}");
        // Each row names its radio, so a tap anywhere on it picks the option.
        assert!(page.contains(r#"for="r7-q0-o1""#), "{page}");
        assert!(page.contains(r#"for="r7-q0-other""#), "{page}");
        assert!(!disabled(&page, "r7-send"));
        assert!(!disabled(&page, "r7-decline"));
        // The text box shows once "Autre…" is picked.
        assert!(tag(&page, "r7-q0-other-text").is_none(), "{page}");

        // Asked by an outside client, the card says so.
        let mut dom = VirtualDom::new_with_props(
            |props: HarnessProps| {
                rsx! {
                    RequestCard {
                        id: ID,
                        card: props.card,
                        from: Some("Codex".into()),
                        status: props.status,
                        refusal: props.refusal,
                        open: props.open,
                        on_send: |_| {},
                    }
                }
            },
            props(single()),
        );
        dom.rebuild_in_place();
        assert!(
            dioxus_ssr::render(&dom).contains("Question de « Codex » (client MCP)"),
            "{}",
            dioxus_ssr::render(&dom)
        );
    }

    /// The card, asked by `from`, rendered.
    fn render_from(from: &str, status: RequestStatus, by: Option<&str>) -> String {
        #[derive(Props, Clone, PartialEq)]
        struct FromProps {
            from: String,
            status: RequestStatus,
            by: Option<String>,
        }
        let mut dom = VirtualDom::new_with_props(
            |props: FromProps| {
                rsx! {
                    RequestCard {
                        id: ID,
                        card: single(),
                        from: Some(props.from),
                        status: props.status,
                        refusal: None,
                        by: props.by,
                        open: true,
                        on_send: |_| {},
                    }
                }
            },
            FromProps {
                from: from.into(),
                status,
                by: by.map(str::to_owned),
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn an_outside_client_never_reads_as_fronde() {
        for name in ["Fronde", "fronde", "assistant"] {
            for status in [
                RequestStatus::Open,
                RequestStatus::Closed(Outcome::Declined),
            ] {
                let page = render_from(name, status, None);
                assert!(
                    page.contains(&format!("Question de « {name} » (client MCP)")),
                    "{page}"
                );
                assert!(!page.contains("Question de Fronde"), "{page}");
            }
        }
    }

    #[test]
    fn closed_card_names_who_answered_when_someone_else_did() {
        let answered = RequestStatus::Closed(Outcome::Answered {
            summary: "Seerr".into(),
        });
        let page = render_from("Codex", answered.clone(), Some("Sam"));
        assert!(page.contains("Réponse : Seerr"), "{page}");
        assert!(page.contains("par Sam"), "{page}");
        let page = render_from("Codex", answered, None);
        assert!(!page.contains("par "), "{page}");
    }

    #[test]
    fn single_select_sends_one_choice() {
        let mut card = Live::new(props(single()));
        card.click("r7-q0-o1");
        assert!(checked(&card.html(), "r7-q0-o1"));
        card.click("r7-q0-o0");
        let page = card.html();
        assert!(checked(&page, "r7-q0-o0"));
        assert!(!checked(&page, "r7-q0-o1"), "one answer only");
        card.click("r7-send");
        assert_eq!(sent(), [answer(&["Mealie"], None, None)]);
        // Sent: nothing more goes until the server closes the card or refuses the answer.
        let page = card.html();
        assert!(disabled(&page, "r7-send"));
        assert!(disabled(&page, "r7-decline"));
    }

    #[test]
    fn multi_select_sends_several() {
        let mut card = Live::new(props(multi()));
        assert!(card.html().contains(r#"role="group""#));
        card.click("r7-q0-o2");
        card.click("r7-q0-o0");
        card.click("r7-q0-o1");
        // Unticked again.
        card.click("r7-q0-o0");
        let page = card.html();
        assert!(!checked(&page, "r7-q0-o0"));
        assert!(checked(&page, "r7-q0-o1") && checked(&page, "r7-q0-o2"));
        card.click("r7-q0-note-toggle");
        card.type_text("r7-q0-note", " le soir ");
        card.click("r7-send");
        // In the card's order, whatever the order they were ticked in.
        assert_eq!(
            sent(),
            [answer(&["Immich", "Paperless"], None, Some("le soir"))]
        );
    }

    #[test]
    fn other_reveals_text() {
        let mut card = Live::new(props(single()));
        card.click("r7-q0-other");
        assert!(checked(&card.html(), "r7-q0-other"));
        // Picked but empty: the card says what to fix and sends nothing.
        card.click("r7-send");
        assert_eq!(sent(), []);
        assert!(
            card.html()
                .contains("Écris ta réponse, ou choisis une option."),
            "{}",
            card.html()
        );
        card.type_text("r7-q0-other-text", "Tandoor");
        card.click("r7-send");
        assert_eq!(sent(), [answer(&[], Some("Tandoor"), None)]);
        assert!(!card.html().contains("Écris ta réponse"));

        // Several answers: "Autre…" is a box of its own, beside the options.
        let mut card = Live::new(props(multi()));
        card.click("r7-q0-o0");
        card.click("r7-q0-other");
        card.type_text("r7-q0-other-text", "Nextcloud");
        card.click("r7-send");
        assert_eq!(sent(), [answer(&["Jellyfin"], Some("Nextcloud"), None)]);
    }

    #[test]
    fn nothing_picked_is_not_sent() {
        let mut card = Live::new(props(single()));
        card.click("r7-send");
        assert_eq!(sent(), []);
        assert!(card.html().contains("Réponds à cette question."));
        assert!(!disabled(&card.html(), "r7-send"));
    }

    #[test]
    fn decline_sends_decline() {
        let mut card = Live::new(props(single()));
        card.click("r7-decline");
        assert_eq!(
            sent(),
            [ClientMsg::Answer {
                id: ID,
                response: Response::Decline
            }]
        );
    }

    #[test]
    fn recommended_badge_first_not_selected() {
        let page = render(props(single()));
        let badge = page.find("Recommandé").expect("the badge");
        // On the first option's row, before the second.
        assert!(page.find("Mealie").unwrap() < badge, "{page}");
        assert!(badge < page.find("Seerr").unwrap(), "{page}");
        // Never picked for the user.
        assert!(!page.contains(r#"aria-checked="true""#), "{page}");
    }

    #[test]
    fn preview_beside_on_wide_under_on_narrow() {
        let mut card = Live::new(props(single()));
        let page = card.html();
        // Beside the options from md: up, waiting for a pick.
        assert!(page.contains("md:grid-cols-2"), "{page}");
        assert!(
            page.contains("Choisis une option pour voir l&#39;aperçu."),
            "{page}"
        );
        card.click("r7-q0-o0");
        let page = card.html();
        let pres: Vec<&str> = page.split("<pre").skip(1).collect();
        assert_eq!(pres.len(), 2, "{page}");
        let under = pres.iter().find(|pre| pre.contains("md:hidden")).unwrap();
        let beside = pres.iter().find(|pre| pre.contains("md:block")).unwrap();
        assert!(under.contains("[ Mealie ]"), "{page}");
        assert!(
            beside.contains("hidden") && beside.contains("[ Mealie ]"),
            "{page}"
        );
        // Without a preview, no column for it.
        card.click("r7-q0-o1");
        let page = card.html();
        assert!(!page.contains("[ Mealie ]"), "{page}");
        let page = render(props(multi()));
        assert!(
            !page.contains("md:grid-cols-2") && !page.contains("<pre"),
            "{page}"
        );
    }

    #[test]
    fn card_is_full_width_on_phone() {
        let page = render(props(single()));
        let card = page
            .split(r#"data-name="Card""#)
            .nth(1)
            .and_then(|rest| rest.split(r#"class=""#).nth(1))
            .and_then(|rest| rest.split('"').next())
            .unwrap();
        assert!(
            card.contains("w-full") && card.contains("sm:max-w-[85%]"),
            "{card}"
        );
        for button in ["r7-send", "r7-decline"] {
            let class = class_of(&page, button);
            // 44 px tall and full width on a phone; compact from sm: up.
            for part in ["h-11", "w-full", "sm:h-9", "sm:w-auto"] {
                assert!(class.contains(part), "{button}: {class}");
            }
        }
        // Each option row is a tap target of 44 px at least.
        let rows = page.matches(r#"data-slot="field-label""#).count();
        assert_eq!(rows, 3, "{page}");
        assert_eq!(page.matches("min-h-11").count(), 3, "{page}");
    }

    #[test]
    fn keyboard_answers_a_question() {
        let mut card = Live::new(props(single()));
        let options = "r7-q0-options";
        card.key(options, Key::ArrowDown, Code::ArrowDown);
        assert!(checked(&card.html(), "r7-q0-o0"));
        card.key(options, Key::ArrowDown, Code::ArrowDown);
        assert!(checked(&card.html(), "r7-q0-o1"));
        card.key(options, Key::ArrowRight, Code::ArrowRight);
        assert!(checked(&card.html(), "r7-q0-other"));
        // Round the end, as a native radio group does.
        card.key(options, Key::ArrowDown, Code::ArrowDown);
        assert!(checked(&card.html(), "r7-q0-o0"));
        card.key(options, Key::ArrowUp, Code::ArrowUp);
        card.key(options, Key::ArrowLeft, Code::ArrowLeft);
        assert!(checked(&card.html(), "r7-q0-o1"));
        // Other keys do nothing.
        card.key(options, Key::Character("a".into()), Code::KeyA);
        assert!(checked(&card.html(), "r7-q0-o1"));
        assert_eq!(sent(), []);
        // Enter sends the card, as in a form.
        card.key(options, Key::Enter, Code::Enter);
        assert_eq!(sent(), [answer(&["Seerr"], None, None)]);
        assert!(disabled(&card.html(), "r7-send"));

        // Several answers: Enter sends too, and arrow keys leave the boxes alone.
        let mut card = Live::new(props(multi()));
        card.click("r7-q0-o1");
        card.key(options, Key::ArrowDown, Code::ArrowDown);
        assert!(checked(&card.html(), "r7-q0-o1"));
        card.key(options, Key::Enter, Code::Enter);
        assert_eq!(sent(), [answer(&["Immich"], None, None)]);

        // Nothing picked: Enter shows what is missing and sends nothing.
        let mut card = Live::new(props(single()));
        card.key(options, Key::Enter, Code::Enter);
        assert_eq!(sent(), []);
        assert!(card.html().contains("Réponds à cette question."));
    }

    #[test]
    fn arrow_keys_in_other_move_the_caret_not_the_pick() {
        let mut card = Live::new(props(single()));
        card.click("r7-q0-other");
        card.type_text("r7-q0-other-text", "Tandoor");
        for (key, code) in [
            (Key::ArrowLeft, Code::ArrowLeft),
            (Key::ArrowRight, Code::ArrowRight),
            (Key::ArrowUp, Code::ArrowUp),
            (Key::ArrowDown, Code::ArrowDown),
        ] {
            card.key_in("r7-q0-other-text", key, code);
            let page = card.html();
            assert!(checked(&page, "r7-q0-other"), "{page}");
            assert!(tag(&page, "r7-q0-other-text").is_some(), "{page}");
        }
        assert_eq!(sent(), []);
        // Enter in the box sends the card.
        card.key_in("r7-q0-other-text", Key::Enter, Code::Enter);
        assert_eq!(sent(), [answer(&[], Some("Tandoor"), None)]);
    }

    #[test]
    fn refusal_shows_and_lets_the_user_answer_again() {
        let refused = HarnessProps {
            refusal: Some("Cette demande est déjà close.".into()),
            ..props(single())
        };
        let page = render(refused);
        assert!(page.contains("Cette demande est déjà close."), "{page}");
        assert!(page.contains(r#"role="alert""#), "{page}");
        assert!(!disabled(&page, "r7-send"));

        // Nothing goes out while the socket is closed.
        let closed = HarnessProps {
            open: false,
            ..props(single())
        };
        let page = render(closed);
        assert!(disabled(&page, "r7-send") && disabled(&page, "r7-decline"));
        assert!(disabled(&page, "r7-q0-o0"));
    }

    #[test]
    fn closed_card_shows_how_it_ended() {
        let cases = [
            (
                Outcome::Answered {
                    summary: "Mealie, Seerr".into(),
                },
                "Réponse : Mealie, Seerr",
            ),
            (Outcome::Declined, "Refusée"),
            (Outcome::Expired, "Expirée"),
            (Outcome::Cancelled, "Annulée"),
        ];
        for (outcome, text) in cases {
            let page = render(HarnessProps {
                status: RequestStatus::Closed(outcome),
                ..props(single())
            });
            assert!(page.contains(text), "{text}: {page}");
            assert!(page.contains("Quelle appli installer ?"), "{page}");
            assert!(!page.contains("<button"), "{page}");
        }
    }
}
