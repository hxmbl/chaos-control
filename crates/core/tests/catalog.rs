//! Cross-project chat list support (STRM-737): core must be able to describe
//! every project and its channels in one answer, with each project's channels
//! in `::N` order, without the app having to switch projects first.

mod common;

use chaos_core::types::ProjectId;
use common::*;

#[tokio::test]
async fn project_catalog_lists_every_project_with_its_channels_in_order() {
    let h = Harness::new().await;

    // A second channel in the test project.
    let design = h.core.channel(&project_id(), "design").await.unwrap();
    assert_eq!(design.ordinal, 2);

    // A second project with its own #main.
    let other_id = ProjectId::parse("11111111-2222-4333-8444-555555555555").unwrap();
    h.core
        .ensure_project(other_id.clone(), "Other", None)
        .await
        .unwrap();
    let other_main = h.core.channel(&other_id, "main").await.unwrap();
    assert_eq!(other_main.ordinal, 1, "a project's first channel is ::1");

    let catalog = h.core.project_catalog().await.unwrap();
    assert_eq!(catalog.len(), 2, "every project is listed");

    let (_, test_channels) = catalog
        .iter()
        .find(|(p, _)| p.id == project_id())
        .expect("the test project");
    let names: Vec<&str> = test_channels.iter().map(|c| c.name.as_ref()).collect();
    assert_eq!(names, ["main", "design"], "::N order, not name order");

    let (other, other_channels) = catalog
        .iter()
        .find(|(p, _)| p.id == other_id)
        .expect("the second project");
    assert_eq!(other.name.as_ref(), "Other");
    assert_eq!(other_channels.len(), 1);
    assert_eq!(other_channels[0].id, other_main.id);
}
