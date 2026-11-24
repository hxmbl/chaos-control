//! The cross-project chat list (STRM-737).
//!
//! The sidebar is no longer "the open project's channels": it is every chat
//! core knows, grouped by project. The data comes from one `Core::project_catalog`
//! answer (all projects and their channels) plus app-only facts (which project is
//! open, unread marks, recents order). This module is GPUI-free so the ordering
//! and grouping rules are unit-tested without a window.

use std::{collections::HashMap, sync::Arc};

use model::{ChatEntry, ChatProject, Id};

/// One chat as core reported it, before app ids and unread marks are applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogChat {
    pub channel: Id,
    pub name: Arc<str>,
    pub ordinal: Option<u32>,
}

/// One project as core reported it, plus the recency the sidebar orders by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogSource {
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub path: Option<Arc<str>>,
    pub chats: Vec<CatalogChat>,
    /// Seconds since the Unix epoch when the project was last opened; `0`
    /// when it has never been opened (imported or only known by core).
    pub last_opened: u64,
}

/// The project badge: its first alphanumeric character, upper-cased, or `#`
/// when the name has none. Matches the project picker's old monogram.
pub fn project_mark(name: &str) -> Arc<str> {
    Arc::from(
        name.chars()
            .find(|c| c.is_alphanumeric())
            .map_or_else(|| "#".to_string(), |c| c.to_uppercase().collect()),
    )
}

/// Builds the sidebar's groups: the open project first, then every other
/// project by recency (newest first), then by name. A project with no chats is
/// still returned, because it can still be selected and needs a way back.
///
/// `unread` is only known for the open project's channels (the app tracks
/// read marks for the project in the window, not the others). Channel ids are
/// per project, so applying that map to every group would light up unrelated
/// chats whose channel happens to have the same number.
pub fn build(
    current: Option<&str>,
    sources: Vec<CatalogSource>,
    unread: &HashMap<Id, bool>,
) -> Vec<ChatProject> {
    let mut sources = sources;
    sources.sort_by(|a, b| {
        let a_current = current == Some(a.id.as_ref());
        let b_current = current == Some(b.id.as_ref());
        b_current
            .cmp(&a_current)
            .then_with(|| b.last_opened.cmp(&a.last_opened))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    sources
        .into_iter()
        .map(|source| {
            let current = current == Some(source.id.as_ref());
            ChatProject {
                current,
                mark: project_mark(&source.name),
                id: source.id,
                name: source.name,
                path: source.path,
                chats: source
                    .chats
                    .into_iter()
                    .map(|chat| ChatEntry {
                        channel: chat.channel,
                        name: chat.name,
                        ordinal: chat.ordinal,
                        unread: current && unread.get(&chat.channel).copied().unwrap_or(false),
                    })
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, name: &str, last_opened: u64, chats: &[(u32, &str)]) -> CatalogSource {
        CatalogSource {
            id: Arc::from(id),
            name: Arc::from(name),
            path: Some(Arc::from(format!("/projects/{name}"))),
            chats: chats
                .iter()
                .map(|(id, name)| CatalogChat {
                    channel: Id(*id),
                    name: Arc::from(*name),
                    ordinal: Some(*id),
                })
                .collect(),
            last_opened,
        }
    }

    #[test]
    fn the_open_project_is_first_whatever_its_recency() {
        let groups = build(
            Some("b"),
            vec![
                source("a", "Alpha", 99, &[(1, "main")]),
                source("b", "Beta", 0, &[(2, "main")]),
                source("c", "Gamma", 100, &[]),
            ],
            &HashMap::new(),
        );
        let ids: Vec<&str> = groups.iter().map(|g| g.id.as_ref()).collect();
        assert_eq!(ids, ["b", "c", "a"]);
        assert!(groups[0].current);
        assert!(!groups[1].current);
    }

    #[test]
    fn other_projects_go_newest_first_then_by_name() {
        let groups = build(
            Some("open"),
            vec![
                source("old", "Old", 1, &[]),
                source("new", "New", 20, &[]),
                source("tie-b", "Tie B", 10, &[]),
                source("tie-a", "Tie A", 10, &[]),
            ],
            &HashMap::new(),
        );
        let ids: Vec<&str> = groups.iter().map(|g| g.id.as_ref()).collect();
        assert_eq!(ids, ["new", "tie-a", "tie-b", "old"]);
    }

    #[test]
    fn chats_keep_core_order_and_only_current_unread_marks_survive() {
        let mut unread = HashMap::new();
        unread.insert(Id(7), true);
        unread.insert(Id(9), false);
        let groups = build(
            Some("p"),
            vec![source(
                "p",
                "Proj",
                0,
                &[(7, "main"), (8, "design"), (9, "ops")],
            )],
            &unread,
        );
        let chats = &groups[0].chats;
        assert_eq!(
            chats.iter().map(|c| c.channel.0).collect::<Vec<_>>(),
            [7, 8, 9]
        );
        assert!(chats[0].unread);
        assert!(!chats[1].unread, "unknown is unread=false, not a guess");
        assert!(!chats[2].unread);
    }

    #[test]
    fn unread_never_leaks_into_another_project_with_the_same_channel_number() {
        let mut unread = HashMap::new();
        unread.insert(Id(1), true);
        // Both projects have a #main whose channel number is 1; only the open
        // project's chat is actually unread.
        let groups = build(
            Some("open"),
            vec![
                source("other", "Other", 5, &[(1, "main")]),
                source("open", "Open", 0, &[(1, "main")]),
            ],
            &unread,
        );
        assert!(groups[0].chats[0].unread, "the open project keeps its dot");
        assert!(
            !groups[1].chats[0].unread,
            "the other project's #main is not the one that was read"
        );
    }

    #[test]
    fn a_project_without_chats_is_still_listed() {
        let groups = build(
            None,
            vec![source("empty", "Empty", 0, &[])],
            &HashMap::new(),
        );
        assert_eq!(groups.len(), 1);
        assert!(groups[0].chats.is_empty());
        assert!(!groups[0].current, "no project is open");
    }

    #[test]
    fn marks_fall_back_to_hash_when_there_is_no_letter() {
        assert_eq!(&*project_mark("starpoint"), "S");
        assert_eq!(&*project_mark("  ••  "), "#");
        assert_eq!(&*project_mark("élan"), "É");
        assert_eq!(&*project_mark(""), "#");
    }
}
