//! Call rows from the backend, such as "Incoming voice call, 15 minutes 1 second",
//! "Voice call, declined" or "Missed group video call", in the app language.
//!
//! The backend writes them in English (backend/call_log.go, backend/call_live_log.go).
//! They are read back into their pieces here and every piece is a string in the po
//! files, spelled out one by one so `tools/i18n.py` finds it.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
	Voice,
	Video,
	GroupVoice,
	GroupVideo,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Form {
	Incoming,
	Outgoing,
	Missed,
	Upcoming,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
	AnsweredElsewhere,
	DeclinedElsewhere,
	YouDeclined,
	NoAnswer,
	Declined,
	Cancelled,
	Unavailable,
	Failed,
	Abandoned,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CallLine<'a> {
	Directed { form: Form, kind: Kind, duration: Option<&'a str> },
	Ended { kind: Kind, outcome: Outcome },
}

const FORMS: [(&str, Form); 4] = [
	("Incoming ", Form::Incoming),
	("Outgoing ", Form::Outgoing),
	("Missed ", Form::Missed),
	("Upcoming ", Form::Upcoming),
];

const OUTCOMES: [(&str, Outcome); 9] = [
	(", answered on another device", Outcome::AnsweredElsewhere),
	(", declined on another device", Outcome::DeclinedElsewhere),
	(", you declined", Outcome::YouDeclined),
	(", no answer", Outcome::NoAnswer),
	(", declined", Outcome::Declined),
	(", cancelled", Outcome::Cancelled),
	(", unavailable", Outcome::Unavailable),
	(", failed", Outcome::Failed),
	(", abandoned", Outcome::Abandoned),
];

fn parse_kind(text: &str) -> Option<Kind> {
	match text {
		"Voice call" | "voice call" => Some(Kind::Voice),
		"Video call" | "video call" => Some(Kind::Video),
		"Group voice call" | "group voice call" => Some(Kind::GroupVoice),
		"Group video call" | "group video call" => Some(Kind::GroupVideo),
		_ => None,
	}
}

fn parse(text: &str) -> Option<CallLine<'_>> {
	for (prefix, form) in FORMS {
		if let Some(rest) = text.strip_prefix(prefix) {
			let (kind, duration) = match rest.split_once(", ") {
				Some((kind, duration)) => (kind, Some(duration)),
				None => (rest, None),
			};
			return Some(CallLine::Directed { form, kind: parse_kind(kind)?, duration });
		}
	}
	for (suffix, outcome) in OUTCOMES {
		if let Some(kind) = text.strip_suffix(suffix) {
			return Some(CallLine::Ended { kind: parse_kind(kind)?, outcome });
		}
	}
	None
}

fn kind_name(kind: Kind) -> &'static str {
	match kind {
		Kind::Voice => crate::i18n::tr("Voice call"),
		Kind::Video => crate::i18n::tr("Video call"),
		Kind::GroupVoice => crate::i18n::tr("Group voice call"),
		Kind::GroupVideo => crate::i18n::tr("Group video call"),
	}
}

fn directed_name(form: Form, kind: Kind) -> &'static str {
	match (form, kind) {
		(Form::Incoming, Kind::Voice) => crate::i18n::tr("Incoming voice call"),
		(Form::Incoming, Kind::Video) => crate::i18n::tr("Incoming video call"),
		(Form::Incoming, Kind::GroupVoice) => crate::i18n::tr("Incoming group voice call"),
		(Form::Incoming, Kind::GroupVideo) => crate::i18n::tr("Incoming group video call"),
		(Form::Outgoing, Kind::Voice) => crate::i18n::tr("Outgoing voice call"),
		(Form::Outgoing, Kind::Video) => crate::i18n::tr("Outgoing video call"),
		(Form::Outgoing, Kind::GroupVoice) => crate::i18n::tr("Outgoing group voice call"),
		(Form::Outgoing, Kind::GroupVideo) => crate::i18n::tr("Outgoing group video call"),
		(Form::Missed, Kind::Voice) => crate::i18n::tr("Missed voice call"),
		(Form::Missed, Kind::Video) => crate::i18n::tr("Missed video call"),
		(Form::Missed, Kind::GroupVoice) => crate::i18n::tr("Missed group voice call"),
		(Form::Missed, Kind::GroupVideo) => crate::i18n::tr("Missed group video call"),
		(Form::Upcoming, Kind::Voice) => crate::i18n::tr("Upcoming voice call"),
		(Form::Upcoming, Kind::Video) => crate::i18n::tr("Upcoming video call"),
		(Form::Upcoming, Kind::GroupVoice) => crate::i18n::tr("Upcoming group voice call"),
		(Form::Upcoming, Kind::GroupVideo) => crate::i18n::tr("Upcoming group video call"),
	}
}

fn ended_name(kind: Kind, outcome: Outcome) -> String {
	let kind = kind_name(kind);
	match outcome {
		Outcome::AnsweredElsewhere => crate::i18n::tr_fmt("{}, answered on another device", &[&kind]),
		Outcome::DeclinedElsewhere => crate::i18n::tr_fmt("{}, declined on another device", &[&kind]),
		Outcome::YouDeclined => crate::i18n::tr_fmt("{}, you declined", &[&kind]),
		Outcome::NoAnswer => crate::i18n::tr_fmt("{}, no answer", &[&kind]),
		Outcome::Declined => crate::i18n::tr_fmt("{}, declined", &[&kind]),
		Outcome::Cancelled => crate::i18n::tr_fmt("{}, cancelled", &[&kind]),
		Outcome::Unavailable => crate::i18n::tr_fmt("{}, unavailable", &[&kind]),
		Outcome::Failed => crate::i18n::tr_fmt("{}, failed", &[&kind]),
		Outcome::Abandoned => crate::i18n::tr_fmt("{}, abandoned", &[&kind]),
	}
}

/// "1 hour 5 minutes" as the backend writes it, in the app language. Anything
/// else is returned unchanged.
fn duration_text(duration: &str) -> String {
	duration_parts(duration).unwrap_or_else(|| duration.to_string())
}

fn duration_parts(duration: &str) -> Option<String> {
	let words: Vec<&str> = duration.split(' ').collect();
	if words.len() % 2 != 0 {
		return None;
	}
	let parts = words
		.chunks(2)
		.map(|pair| {
			let n: u64 = pair[0].parse().ok()?;
			match pair[1] {
				"hour" => Some(crate::i18n::tr_fmt("{} hour", &[&n])),
				"hours" => Some(crate::i18n::tr_fmt("{} hours", &[&n])),
				"minute" => Some(crate::i18n::tr_fmt("{} minute", &[&n])),
				"minutes" => Some(crate::i18n::tr_fmt("{} minutes", &[&n])),
				"second" => Some(crate::i18n::tr_fmt("{} second", &[&n])),
				"seconds" => Some(crate::i18n::tr_fmt("{} seconds", &[&n])),
				_ => None,
			}
		})
		.collect::<Option<Vec<String>>>()?;
	Some(parts.join(" "))
}

/// A call row as the chat list and the conversation show it.
pub fn translate_call_text(text: &str) -> String {
	match parse(text) {
		Some(CallLine::Directed { form, kind, duration: Some(duration) }) => {
			format!("{}, {}", directed_name(form, kind), duration_text(duration))
		}
		Some(CallLine::Directed { form, kind, duration: None }) => directed_name(form, kind).to_string(),
		Some(CallLine::Ended { kind, outcome }) => ended_name(kind, outcome),
		None => text.to_string(),
	}
}

/// A row of the Calls tab: the call and who it was with, without the time.
pub fn call_log_description(text: &str, name: &str, from_me: bool) -> String {
	match parse(text) {
		Some(CallLine::Directed { form: Form::Incoming, kind, duration: Some(duration) }) => {
			crate::i18n::tr_fmt("{} from {}, duration {}", &[&kind_name(kind), &name, &duration_text(duration)])
		}
		Some(CallLine::Directed { form: Form::Outgoing, kind, duration: Some(duration) }) => {
			let call = directed_name(Form::Outgoing, kind);
			crate::i18n::tr_fmt("{} to {}, duration {}", &[&call, &name, &duration_text(duration)])
		}
		Some(CallLine::Directed { form: form @ (Form::Incoming | Form::Missed), kind, duration: None }) => {
			crate::i18n::tr_fmt("{} from {}", &[&directed_name(form, kind), &name])
		}
		Some(CallLine::Directed { form: Form::Outgoing, kind, duration: None }) => {
			crate::i18n::tr_fmt("{} to {}", &[&directed_name(Form::Outgoing, kind), &name])
		}
		_ => {
			let call = translate_call_text(text);
			if from_me {
				crate::i18n::tr_fmt("{} to {}", &[&call, &name])
			} else {
				crate::i18n::tr_fmt("{} from {}", &[&call, &name])
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_every_backend_call_row() {
		assert_eq!(
			parse("Incoming voice call, 15 minutes 1 second"),
			Some(CallLine::Directed { form: Form::Incoming, kind: Kind::Voice, duration: Some("15 minutes 1 second") })
		);
		assert_eq!(
			parse("Outgoing group video call"),
			Some(CallLine::Directed { form: Form::Outgoing, kind: Kind::GroupVideo, duration: None })
		);
		assert_eq!(
			parse("Missed group voice call"),
			Some(CallLine::Directed { form: Form::Missed, kind: Kind::GroupVoice, duration: None })
		);
		assert_eq!(
			parse("Video call, declined on another device"),
			Some(CallLine::Ended { kind: Kind::Video, outcome: Outcome::DeclinedElsewhere })
		);
		assert_eq!(
			parse("Group voice call, declined"),
			Some(CallLine::Ended { kind: Kind::GroupVoice, outcome: Outcome::Declined })
		);
		assert_eq!(
			parse("Voice call, you declined"),
			Some(CallLine::Ended { kind: Kind::Voice, outcome: Outcome::YouDeclined })
		);
		assert_eq!(parse("Incoming fax"), None);
		assert_eq!(parse("Hello"), None);
	}

	#[test]
	fn english_rows_stay_as_the_backend_wrote_them() {
		for text in [
			"Incoming voice call, 15 minutes 1 second",
			"Outgoing group video call, 1 hour 2 minutes",
			"Incoming video call",
			"Missed voice call",
			"Upcoming group voice call",
			"Voice call, no answer",
			"Group video call, answered on another device",
			"Something else",
		] {
			assert_eq!(translate_call_text(text), text);
		}
		assert_eq!(duration_text("3 parsecs"), "3 parsecs");
	}

	#[test]
	fn english_calls_tab_rows_keep_their_wording() {
		assert_eq!(
			call_log_description("Incoming voice call, 15 minutes", "Ana", false),
			"Voice call from Ana, duration 15 minutes"
		);
		assert_eq!(
			call_log_description("Outgoing video call, 2 minutes", "Ana", true),
			"Outgoing video call to Ana, duration 2 minutes"
		);
		assert_eq!(call_log_description("Incoming voice call", "Ana", false), "Incoming voice call from Ana");
		assert_eq!(call_log_description("Outgoing voice call", "Ana", true), "Outgoing voice call to Ana");
		assert_eq!(call_log_description("Missed video call", "Ana", false), "Missed video call from Ana");
		assert_eq!(call_log_description("Voice call, cancelled", "Ana", true), "Voice call, cancelled to Ana");
		assert_eq!(call_log_description("Upcoming voice call", "Ana", false), "Upcoming voice call from Ana");
	}
}
