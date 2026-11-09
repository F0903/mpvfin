//! Pure entries, commands and replies of the watch-together menu.

use jellyfin::GroupSummary;
use serde_json::{Value, json};

use crate::receiver::ReceiverEvent;

/// What a menu entry does.
#[derive(Debug, Eq, PartialEq)]
pub enum MenuChoice {
    /// Join the group with this id.
    Join(String),
    Create,
    Leave,
}

/// One menu entry.
#[derive(Debug, Eq, PartialEq)]
pub struct MenuItem {
    pub title: String,
    pub choice: MenuChoice,
}

/// A callback event of the uosc menu.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UoscEvent {
    /// The entry at this position of the items was chosen.
    Activate(usize),
    Close,
}

/// A reply of the console menu.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleEvent {
    /// The entry at this position of the items was chosen.
    Submit(usize),
    Closed,
}

/// The client message name uosc's callback uses.
pub const UOSC_MESSAGE: &str = "wt-menu";
/// The handler id of the console menu, in the current protocol.
pub const CONSOLE_HANDLER: &str = "input-event/mpvfin-wt";

/// uosc's id of this menu.
const UOSC_MENU_TYPE: &str = "mpvfin-wt";

/// The menu: every joinable group, then creating one, then leaving the current one.
#[must_use]
pub fn menu_items(groups: &[GroupSummary], current: Option<&str>) -> Vec<MenuItem> {
    let joinable = groups
        .iter()
        .filter(|group| Some(group.id.as_str()) != current)
        .map(|group| MenuItem {
            title: format!("{} ({} members)", group.name, group.members),
            choice: MenuChoice::Join(group.id.clone()),
        });
    let create = MenuItem {
        title: "Create new group".to_owned(),
        choice: MenuChoice::Create,
    };
    let leave = current.map(|_| MenuItem {
        title: "Leave group".to_owned(),
        choice: MenuChoice::Leave,
    });
    joinable.chain([create]).chain(leave).collect()
}

/// What a choice feeds the reducer. The server keeps the viewer in the old
/// group when a join is refused, so a join leaves it first: the two travel the
/// same queue, and a refusal then leaves both sides agreeing the viewer is out.
#[must_use]
pub fn choice_events(choice: MenuChoice, grouped: bool, device_name: &str) -> Vec<ReceiverEvent> {
    match choice {
        MenuChoice::Join(group_id) => {
            let join = ReceiverEvent::GroupJoin { group_id };
            if grouped {
                vec![ReceiverEvent::GroupLeave, join]
            } else {
                vec![join]
            }
        }
        MenuChoice::Create => vec![ReceiverEvent::GroupCreate {
            name: device_name.to_owned(),
        }],
        MenuChoice::Leave => vec![ReceiverEvent::GroupLeave],
    }
}

/// Opens the menu in uosc; each entry's `value` is its position in `items`.
#[must_use]
pub fn uosc_open_command(items: &[MenuItem], client: &str) -> Vec<String> {
    let entries: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(value, item)| json!({"title": item.title, "value": value}))
        .collect();
    let menu = json!({
        "type": UOSC_MENU_TYPE,
        "title": "Watch together",
        "callback": [client, UOSC_MESSAGE],
        "items": entries,
    });
    strings(&["script-message-to", "uosc", "open-menu", &menu.to_string()])
}

/// Closes this menu; uosc keeps it open after an activation.
#[must_use]
pub fn uosc_close_command() -> Vec<String> {
    strings(&["script-message-to", "uosc", "close-menu", UOSC_MENU_TYPE])
}

/// Fails if uosc isn't running, and closes no menu of anyone else's when it is.
#[must_use]
pub fn uosc_probe_command() -> Vec<String> {
    strings(&["script-message-to", "uosc", "close-menu", "mpvfin-probe"])
}

/// Opens the menu in mpv's console. The wire format changed after 0.41.0.
#[must_use]
pub fn console_open_command(
    items: &[MenuItem],
    client: &str,
    handler: &str,
    legacy: bool,
) -> Vec<String> {
    let titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
    let mut request = json!({"prompt": "Watch together:", "items": titles});
    let mut command = strings(&["script-message-to", "console", "get-input"]);
    if legacy {
        command.push(client.to_owned());
    } else {
        request["client_name"] = client.into();
        request["handler_id"] = handler.into();
    }
    command.push(request.to_string());
    command
}

/// Whether this `mpv-version` value speaks the console protocol of 0.39
/// through the 0.41.0 release; anything unrecognised is taken as current.
#[must_use]
pub fn console_is_legacy(mpv_version: &str) -> bool {
    let version = mpv_version
        .strip_prefix("mpv ")
        .unwrap_or(mpv_version)
        .trim_start_matches('v');
    let (numbers, suffix) = version.split_at(version.find(['-', '+']).unwrap_or(version.len()));
    let mut parts = numbers.split('.').map(str::parse::<u32>);
    let (Some(Ok(0)), Some(Ok(minor)), Some(Ok(patch))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    match minor {
        39 | 40 => true,
        // A `-dirty` tree or a distro's `+…` is still the release, not a commit past it.
        41 => patch == 0 && (suffix.is_empty() || suffix == "-dirty" || suffix.starts_with('+')),
        _ => false,
    }
}

/// The event in a uosc callback's JSON; `None` for the kinds the menu ignores.
#[must_use]
pub fn parse_uosc_event(json: &str) -> Option<UoscEvent> {
    let event: Value = serde_json::from_str(json).ok()?;
    match event.get("type")?.as_str()? {
        "activate" => Some(UoscEvent::Activate(index(event.get("value")?)?)),
        "close" => Some(UoscEvent::Close),
        _ => None,
    }
}

/// The event in a console client message, in either protocol; `Submit` has
/// already been turned from the console's 1-based number into a position.
#[must_use]
pub fn parse_console_event(args: &[String]) -> Option<ConsoleEvent> {
    let [name, event, rest @ ..] = args else {
        return None;
    };
    if name != CONSOLE_HANDLER && name != "input-event" {
        return None;
    }
    match event.as_str() {
        "submit" => {
            let reply: Value = serde_json::from_str(rest.first()?).ok()?;
            let number = index(reply.get(0)?)?;
            number.checked_sub(1).map(ConsoleEvent::Submit)
        }
        "closed" => Some(ConsoleEvent::Closed),
        _ => None,
    }
}

fn index(value: &Value) -> Option<usize> {
    usize::try_from(value.as_u64()?).ok()
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use crate::receiver::{Receiver, ReceiverAction};
    use jellyfin::GroupRequest;

    use super::*;

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn group(id: &str, name: &str, members: usize) -> GroupSummary {
        GroupSummary {
            id: id.to_owned(),
            name: name.to_owned(),
            members,
            state: "Idle".to_owned(),
        }
    }

    fn items() -> Vec<MenuItem> {
        vec![
            MenuItem {
                title: "Movie night (2 members)".to_owned(),
                choice: MenuChoice::Join("a".to_owned()),
            },
            MenuItem {
                title: "Create new group".to_owned(),
                choice: MenuChoice::Create,
            },
        ]
    }

    fn json_arg(command: &[String], index: usize) -> Value {
        serde_json::from_str(&command[index]).expect("the argument is JSON")
    }

    #[test]
    fn ungrouped_menu_lists_every_group_and_create() {
        assert_eq!(
            menu_items(&[group("a", "Movie night", 2)], None),
            items(),
            "no Leave while ungrouped"
        );
        assert_eq!(
            menu_items(&[], None),
            vec![MenuItem {
                title: "Create new group".to_owned(),
                choice: MenuChoice::Create,
            }]
        );
    }

    #[test]
    fn grouped_menu_hides_the_current_group_and_offers_leave() {
        let groups = [group("a", "Movie night", 2), group("b", "Other", 3)];
        let titles_and_choices: Vec<(String, MenuChoice)> = menu_items(&groups, Some("a"))
            .into_iter()
            .map(|item| (item.title, item.choice))
            .collect();
        assert_eq!(
            titles_and_choices,
            vec![
                (
                    "Other (3 members)".to_owned(),
                    MenuChoice::Join("b".to_owned())
                ),
                ("Create new group".to_owned(), MenuChoice::Create),
                ("Leave group".to_owned(), MenuChoice::Leave),
            ]
        );
    }

    #[test]
    fn uosc_menu_calls_back_with_indexes_into_the_items() {
        let command = uosc_open_command(&items(), "mpvfin");
        assert_eq!(
            command[..3],
            cmd(&["script-message-to", "uosc", "open-menu"])
        );
        assert_eq!(command.len(), 4);
        assert_eq!(
            json_arg(&command, 3),
            json!({
                "type": "mpvfin-wt",
                "title": "Watch together",
                "callback": ["mpvfin", "wt-menu"],
                "items": [
                    {"title": "Movie night (2 members)", "value": 0},
                    {"title": "Create new group", "value": 1},
                ],
            })
        );
    }

    #[test]
    fn a_group_name_cannot_break_out_of_the_menu_json() {
        let hostile = "x\",\"value\":1,\"title\":\"y\n";
        let menu = menu_items(&[group("a", hostile, 2)], None);
        let title = format!("{hostile} (2 members)");
        assert_eq!(
            json_arg(&uosc_open_command(&menu, "mpvfin"), 3)["items"][0]["title"],
            json!(title)
        );
        assert_eq!(
            json_arg(
                &console_open_command(&menu, "mpvfin", CONSOLE_HANDLER, false),
                3
            )["items"][0],
            json!(title)
        );
    }

    #[test]
    fn uosc_probe_and_close_name_only_their_own_menu() {
        assert_eq!(
            uosc_probe_command(),
            cmd(&["script-message-to", "uosc", "close-menu", "mpvfin-probe"])
        );
        assert_eq!(
            uosc_close_command(),
            cmd(&["script-message-to", "uosc", "close-menu", "mpvfin-wt"])
        );
    }

    #[test]
    fn console_current_form_names_client_and_handler_in_the_json() {
        let command = console_open_command(&items(), "mpvfin", CONSOLE_HANDLER, false);
        assert_eq!(
            command[..3],
            cmd(&["script-message-to", "console", "get-input"])
        );
        assert_eq!(command.len(), 4);
        assert_eq!(
            json_arg(&command, 3),
            json!({
                "client_name": "mpvfin",
                "handler_id": "input-event/mpvfin-wt",
                "prompt": "Watch together:",
                "items": ["Movie night (2 members)", "Create new group"],
            })
        );
    }

    #[test]
    fn console_legacy_form_passes_the_client_separately() {
        let command = console_open_command(&items(), "mpvfin", CONSOLE_HANDLER, true);
        assert_eq!(
            command[..4],
            cmd(&["script-message-to", "console", "get-input", "mpvfin"])
        );
        assert_eq!(command.len(), 5);
        assert_eq!(
            json_arg(&command, 4),
            json!({
                "prompt": "Watch together:",
                "items": ["Movie night (2 members)", "Create new group"],
            })
        );
    }

    #[test]
    fn console_is_legacy_from_0_39_through_plain_0_41_0() {
        for version in [
            "mpv 0.39.0",
            "mpv v0.39.0",
            "mpv 0.40.0",
            "mpv v0.40.0-12-gabc1234",
            "mpv v0.41.0",
            "mpv 0.41.0",
            "mpv v0.41.0-dirty",
            "mpv 0.40.0+dfsg",
            "mpv 0.41.0+dfsg-1",
        ] {
            assert!(console_is_legacy(version), "{version}");
        }
        for version in [
            "mpv v0.41.0-923-g7b8915bc1",
            "mpv v0.41.0-923-g7b8915bc1-dirty",
            "mpv v0.41.1",
            "mpv v0.42.0",
            "mpv v1.0.0",
            "mpv 0.38.0",
            "mpv git-abcdef",
            "",
        ] {
            assert!(!console_is_legacy(version), "{version}");
        }
    }

    #[test]
    fn a_choice_feeds_its_events_and_a_join_leaves_any_current_group_first() {
        let join = || MenuChoice::Join("b".to_owned());
        let joined = ReceiverEvent::GroupJoin {
            group_id: "b".to_owned(),
        };
        assert_eq!(choice_events(join(), false, "pc"), vec![joined.clone()]);
        assert_eq!(
            choice_events(join(), true, "pc"),
            vec![ReceiverEvent::GroupLeave, joined]
        );
        assert_eq!(
            choice_events(MenuChoice::Create, true, "pc"),
            vec![ReceiverEvent::GroupCreate {
                name: "pc".to_owned()
            }]
        );
        assert_eq!(
            choice_events(MenuChoice::Leave, true, "pc"),
            vec![ReceiverEvent::GroupLeave]
        );
    }

    /// The server keeps the viewer in the old group when a join is refused, so
    /// the old group has to be left first or the two sides would disagree.
    #[test]
    fn a_refused_join_while_grouped_leaves_both_sides_out() {
        let mut receiver = Receiver::new();
        receiver.handle(ReceiverEvent::GroupJoined {
            group_id: "a".to_owned(),
        });

        let grouped = receiver.group_id().is_some();
        let requests: Vec<GroupRequest> =
            choice_events(MenuChoice::Join("dead".to_owned()), grouped, "pc")
                .into_iter()
                .flat_map(|event| receiver.handle(event))
                .filter_map(|action| match action {
                    ReceiverAction::Group(request) => Some(request),
                    _ => None,
                })
                .collect();
        assert_eq!(
            requests,
            vec![
                GroupRequest::Leave,
                GroupRequest::Join {
                    group_id: "dead".to_owned()
                }
            ],
            "the server hears the leave first, on the same queue"
        );
        assert_eq!(receiver.group_id(), None);

        let message = "That group no longer exists".to_owned();
        let actions = receiver.handle(ReceiverEvent::GroupGone {
            message: message.clone(),
        });
        assert_eq!(receiver.group_id(), None);
        assert!(actions.contains(&ReceiverAction::ShowMessage { text: message }));
    }

    #[test]
    fn uosc_events_read_the_value_and_close() {
        let activate = r#"{"ctrl":false,"is_pointer":false,"type":"activate","menu_id":"{root}","shift":false,"index":2,"value":1,"alt":false}"#;
        assert_eq!(parse_uosc_event(activate), Some(UoscEvent::Activate(1)));
        assert_eq!(
            parse_uosc_event(r#"{"type":"close"}"#),
            Some(UoscEvent::Close)
        );
        for ignored in [
            r#"{"type":"back"}"#,
            r#"{"type":"activate","value":"x"}"#,
            r#"{"type":"activate","value":-1}"#,
            r#"{"type":"activate"}"#,
            "not json",
        ] {
            assert_eq!(parse_uosc_event(ignored), None, "{ignored}");
        }
    }

    #[test]
    fn console_events_read_both_protocols() {
        let parse = |parts: &[&str]| parse_console_event(&cmd(parts));
        for handler in ["input-event/mpvfin-wt", "input-event"] {
            assert_eq!(
                parse(&[handler, "submit", "[3]"]),
                Some(ConsoleEvent::Submit(2)),
                "the reply is 1-based, the event indexes the items"
            );
            assert_eq!(
                parse(&[handler, "closed", "[\"\",1]"]),
                Some(ConsoleEvent::Closed)
            );
            assert_eq!(parse(&[handler, "opened"]), None);
            assert_eq!(parse(&[handler, "submit", "[0]"]), None);
            assert_eq!(parse(&[handler, "submit", "nonsense"]), None);
            assert_eq!(parse(&[handler, "submit"]), None);
        }
        assert_eq!(parse(&["input-event/other", "submit", "[1]"]), None);
        assert_eq!(parse(&[]), None);
    }
}
