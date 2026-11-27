//! Templates for the window title and the chat list rows, in Jinja syntax
//! like Fedra's, so people can choose what is read first.

use std::cell::RefCell;

use minijinja::{AutoEscape, Environment, Error, Value, context};

/// The title whatSnap has always had: "Mum - whatSnap", with the account
/// when there are several, and the unread chats of the other accounts.
pub const DEFAULT_WINDOW_TITLE_TEMPLATE: &str = "{% if chat %}{{ chat }} - {% endif %}{% if account %}{{ account }} - {% endif %}{{ app }}{% if unread_elsewhere %} - {{ unread_elsewhere }}{% endif %}";

/// The chat list row whatSnap has always had: "Mum [Pinned] | 2 unread
/// messages | 10:02 AM | See you".
pub const DEFAULT_CHAT_TEMPLATE: &str = "{{ name }}{% if tags %} {{ tags }}{% endif %}{% if unread %} | {{ unread }}{% endif %}{% if time %} | {{ time }}{% endif %}{% if message %} | {{ message }}{% endif %}";

pub struct WindowTitleVars<'a> {
	pub app: &'a str,
	pub account: &'a str,
	pub chat: &'a str,
	pub unread_elsewhere: &'a str,
}

pub struct ChatVars<'a> {
	pub name: &'a str,
	pub tags: &'a str,
	pub unread: &'a str,
	pub time: &'a str,
	pub message: &'a str,
}

/// A chat row's parts other than its last message, kept so a draft can take
/// the message's place in the same template.
#[derive(Clone, Default, Debug)]
pub struct ChatRowParts {
	pub name: String,
	pub tags: String,
	pub unread: String,
	pub time: String,
}

impl ChatRowParts {
	pub fn render(&self, template: &str, message: &str) -> String {
		chat_row(
			template,
			&ChatVars { name: &self.name, tags: &self.tags, unread: &self.unread, time: &self.time, message },
		)
	}
}

thread_local! {
	// Compiling costs far more than rendering, and every chat row is
	// rendered each time the list changes.
	static COMPILED: RefCell<Environment<'static>> = RefCell::new({
		let mut env = Environment::new();
		env.set_auto_escape_callback(|_| AutoEscape::None);
		env
	});
}

fn render(template: &str, ctx: Value) -> Result<String, Error> {
	COMPILED.with_borrow_mut(|env| {
		if env.get_template(template).is_err() {
			env.add_template_owned(template.to_owned(), template.to_owned())?;
		}
		env.get_template(template)?.render(ctx)
	})
}

/// A template that is empty or doesn't render uses the default instead, so a
/// typo never leaves the title or a row blank.
fn render_or_default(template: &str, default: &str, ctx: Value) -> String {
	let template = if template.trim().is_empty() { default } else { template };
	match render(template, ctx.clone()) {
		Ok(text) if !text.trim().is_empty() => text,
		_ => render(default, ctx).unwrap_or_default(),
	}
}

pub fn window_title(template: &str, vars: &WindowTitleVars) -> String {
	let ctx = context! {
		app => vars.app,
		account => vars.account,
		chat => vars.chat,
		unread_elsewhere => vars.unread_elsewhere,
	};
	render_or_default(template, DEFAULT_WINDOW_TITLE_TEMPLATE, ctx)
}

pub fn chat_row(template: &str, vars: &ChatVars) -> String {
	let ctx = context! {
		name => vars.name,
		tags => vars.tags,
		unread => vars.unread,
		time => vars.time,
		message => vars.message,
	};
	render_or_default(template, DEFAULT_CHAT_TEMPLATE, ctx)
}

/// Whether `template` compiles, for the settings dialog to say so on save.
pub fn check(template: &str) -> Result<(), String> {
	Environment::new().template_from_str(template).map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn chat<'a>(tags: &'a str, unread: &'a str, time: &'a str, message: &'a str) -> ChatVars<'a> {
		ChatVars { name: "Mum <3", tags, unread, time, message }
	}

	#[test]
	fn default_chat_row_matches_the_old_text() {
		let row = chat_row("", &chat("[Pinned] [Muted]", "2 unread messages", "10:02 AM", "You: hi & bye"));
		assert_eq!(row, "Mum <3 [Pinned] [Muted] | 2 unread messages | 10:02 AM | You: hi & bye");
		assert_eq!(chat_row(DEFAULT_CHAT_TEMPLATE, &chat("", "", "", "")), "Mum <3");
	}

	#[test]
	fn custom_and_broken_templates() {
		let vars = chat("", "", "10:02 AM", "See you");
		assert_eq!(chat_row("{{ name }}, {{ message }}, {{ time }}", &vars), "Mum <3, See you, 10:02 AM");
		assert_eq!(chat_row("{{ name ", &vars), "Mum <3 | 10:02 AM | See you");
		assert!(check("{{ name ").is_err());
		assert!(check(DEFAULT_CHAT_TEMPLATE).is_ok());
	}

	#[test]
	fn default_window_title_matches_the_old_text() {
		let vars = WindowTitleVars { app: "whatSnap", account: "", chat: "Mum", unread_elsewhere: "" };
		assert_eq!(window_title("", &vars), "Mum - whatSnap");
		let vars = WindowTitleVars { app: "whatSnap", account: "Work", chat: "", unread_elsewhere: "3 unread chats" };
		assert_eq!(window_title(DEFAULT_WINDOW_TITLE_TEMPLATE, &vars), "Work - whatSnap - 3 unread chats");
	}
}
