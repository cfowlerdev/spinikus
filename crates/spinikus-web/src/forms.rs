//! Form pieces. A form posts to its page's URL (or a section's), and on a problem the page
//! is rendered again with what was typed and the problem above the form.

use maud::{Markup, html};

use crate::{
    auth::Signed,
    layout::{BUTTON_PRIMARY, CARD, INPUT, LABEL, csrf_field},
};

/// A text input. `id` must be unique on the page; the field posts as `name`.
pub struct Input<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub label: &'a str,
    pub value: &'a str,
    pub kind: &'a str,
    pub hint: Option<&'a str>,
    pub placeholder: Option<&'a str>,
    pub required: bool,
    pub mono: bool,
    pub readonly: bool,
}

impl<'a> Input<'a> {
    pub fn text(id: &'a str, name: &'a str, label: &'a str, value: &'a str) -> Self {
        Self {
            id,
            name,
            label,
            value,
            kind: "text",
            hint: None,
            placeholder: None,
            required: false,
            mono: false,
            readonly: false,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Shown but not editable, dimmed, when `readonly`.
    pub fn readonly(mut self, readonly: bool) -> Self {
        self.readonly = readonly;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    pub fn hint(mut self, hint: &'a str) -> Self {
        self.hint = Some(hint);
        self
    }

    pub fn placeholder(mut self, placeholder: &'a str) -> Self {
        self.placeholder = Some(placeholder);
        self
    }

    pub fn kind(mut self, kind: &'a str) -> Self {
        self.kind = kind;
        self
    }

    pub fn render(&self) -> Markup {
        html! {
            div class="space-y-1.5" {
                label for=(self.id) class=(LABEL) { (self.label) }
                input id=(self.id) name=(self.name) type=(self.kind) value=(self.value) required[self.required] readonly[self.readonly]
                    placeholder=[self.placeholder] autocomplete="off" class={ (INPUT) @if self.mono { " font-mono" } @if self.readonly { " bg-zinc-50 text-zinc-500 dark:bg-zinc-800/50 dark:text-zinc-400" } };
                @if let Some(hint) = self.hint { p class="text-xs text-zinc-500 dark:text-zinc-400" { (hint) } }
            }
        }
    }
}

/// A multi-line text field. Browsers send its line breaks as CRLF; [`lines`] undoes that.
pub fn textarea(id: &str, name: &str, label: &str, value: &str, hint: Option<&str>, placeholder: Option<&str>) -> Markup {
    html! {
        div class="space-y-1.5" {
            label for=(id) class=(LABEL) { (label) }
            textarea id=(id) name=(name) rows="3" autocomplete="off" spellcheck="false" placeholder=[placeholder]
                class={ (INPUT) " font-mono" } { (value) }
            @if let Some(hint) = hint { p class="text-xs text-zinc-500 dark:text-zinc-400" { (hint) } }
        }
    }
}

/// A textarea's value with the browser's CRLF line breaks made LF.
pub fn lines(value: &str) -> String {
    value.replace("\r\n", "\n")
}

/// A checkbox that posts `on` when checked. A disabled one posts nothing.
pub fn checkbox(id: &str, name: &str, label: &str, checked: bool, disabled: bool, hint: Option<&str>) -> Markup {
    html! {
        div class="flex gap-3" {
            input id=(id) name=(name) type="checkbox" checked[checked] disabled[disabled]
                class="mt-0.5 size-4 rounded border-zinc-300 text-indigo-600 disabled:opacity-60 dark:border-zinc-700";
            div {
                label for=(id) class=(LABEL) { (label) }
                @if let Some(hint) = hint { p class="text-xs text-zinc-500 dark:text-zinc-400" { (hint) } }
            }
        }
    }
}

/// A select of `(value, label)` options.
pub fn select(id: &str, name: &str, label: &str, options: &[(&str, &str)], selected: &str) -> Markup {
    html! {
        div class="space-y-1.5" {
            label for=(id) class=(LABEL) { (label) }
            select id=(id) name=(name) class=(INPUT) {
                @for (value, text) in options { option value=(value) selected[*value == selected] { (text) } }
            }
        }
    }
}

/// A card with a titled form: the fields, then the submit button.
pub fn card(
    signed: &Signed,
    title: &str,
    description: Option<&str>,
    action: &str,
    notice: Notice<'_>,
    fields: Markup,
    submit: &str,
) -> Markup {
    html! {
        section class={ (CARD) " p-6" } {
            h2 class="font-semibold" { (title) }
            @if let Some(description) = description { p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400" { (description) } }
            (notice.render())
            form method="post" action=(action) class="mt-5 space-y-4" {
                (csrf_field(signed))
                (fields)
                div class="pt-1" { button type="submit" class=(BUTTON_PRIMARY) { (submit) } }
            }
        }
    }
}

/// What to say above a form after it was posted.
#[derive(Clone, Copy, Default)]
pub enum Notice<'a> {
    #[default]
    None,
    Saved,
    Problem(&'a str),
}

impl Notice<'_> {
    pub(crate) fn render(self) -> Markup {
        html! {
            @match self {
                Notice::None => {},
                Notice::Saved => p role="status" class="mt-4 rounded-md bg-emerald-50 px-3 py-2 text-sm text-emerald-700 dark:bg-emerald-500/10 dark:text-emerald-400" { "Saved." },
                Notice::Problem(problem) => p role="alert" class="mt-4 rounded-md bg-red-50 px-3 py-2 text-sm text-red-700 dark:bg-red-500/10 dark:text-red-400" { (problem) },
            }
        }
    }
}

/// A card with one destructive button, confirmed in the browser first.
pub fn danger(signed: &Signed, title: &str, description: &str, action: &str, button: &str, confirm: &str) -> Markup {
    html! {
        section class="rounded-lg border border-red-200 bg-white p-6 dark:border-red-500/30 dark:bg-zinc-900" {
            h2 class="font-semibold text-red-700 dark:text-red-400" { (title) }
            p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400" { (description) }
            form method="post" action=(action) hx-confirm=(confirm) class="mt-4" {
                (csrf_field(signed))
                button type="submit" class="inline-flex items-center rounded-md bg-red-600 px-3 py-2 text-sm font-medium text-white shadow-xs hover:bg-red-500" { (button) }
            }
        }
    }
}
