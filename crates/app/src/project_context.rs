//! Authoritative active project context.
//!
//! Owned at the application/runtime boundary. UI observes and renders it;
//! runtime components consume it. Composer and agents never decide which
//! project is active.

use chaos_core::types::ProjectId;

/// Current filesystem location of the active project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectLocation {
    /// Working directory/root path for the project, when known.
    pub path: Option<String>,
}

/// Lifecycle of the active project context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectLifecycle {
    Open,
    Switching,
    Closed,
    #[allow(dead_code)]
    Recovering,
}

/// Authoritative project context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectContext {
    /// Stable logical project identity, separate from location.
    pub identity: Option<ProjectId>,
    /// Current working directory; may move.
    pub location: ProjectLocation,
    pub lifecycle: ProjectLifecycle,
    /// Bumped on every context transition. Async results must match the
    /// current generation before they are applied.
    pub generation: u64,
}

impl ProjectContext {
    pub fn new() -> Self {
        Self {
            identity: None,
            location: ProjectLocation { path: None },
            lifecycle: ProjectLifecycle::Closed,
            generation: 0,
        }
    }

    pub fn bump(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.generation
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    #[allow(dead_code)]
    pub fn begin_switch(&mut self) -> u64 {
        self.lifecycle = ProjectLifecycle::Switching;
        self.bump()
    }

    #[allow(dead_code)]
    pub fn open(&mut self, identity: ProjectId, path: Option<String>) -> u64 {
        self.identity = Some(identity);
        self.location = ProjectLocation { path };
        self.lifecycle = ProjectLifecycle::Open;
        self.bump()
    }

    #[allow(dead_code)]
    pub fn close(&mut self) -> u64 {
        self.identity = None;
        self.location = ProjectLocation { path: None };
        self.lifecycle = ProjectLifecycle::Closed;
        self.bump()
    }
}

impl Default for ProjectContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ProjectId {
        ProjectId::parse(s).expect("valid project id")
    }

    #[test]
    fn generation_is_a_transition_guard_not_identity() {
        let mut ctx = ProjectContext::new();
        let id = id("0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59");
        let g1 = ctx.open(id.clone(), Some("/a".into()));
        let g2 = ctx.open(id.clone(), Some("/b".into()));
        assert!(g2 > g1);
        assert_eq!(ctx.identity.as_ref(), Some(&id));
        assert_eq!(ctx.location.path.as_deref(), Some("/b"));
        assert!(ctx.is_current(g2));
        assert!(!ctx.is_current(g1));
    }

    #[test]
    fn a_to_b_to_a_switching_bumps_generation_each_time() {
        let mut ctx = ProjectContext::new();
        let a = id("0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59");
        let b = id("5f0c9d1a-7b2e-4e8f-a1c3-9d8e7f6a5b4c");
        let g_a1 = ctx.open(a.clone(), Some("/a".into()));
        let g_b = ctx.open(b.clone(), Some("/b".into()));
        let g_a2 = ctx.open(a.clone(), Some("/a".into()));
        assert!(g_a1 < g_b && g_b < g_a2);
        assert_eq!(ctx.identity.as_ref(), Some(&a));
        assert!(ctx.is_current(g_a2));
        assert!(
            !ctx.is_current(g_a1),
            "delayed async result from earlier A must be dropped"
        );
        assert!(!ctx.is_current(g_b));
    }

    #[test]
    fn switching_marks_lifecycle() {
        let mut ctx = ProjectContext::new();
        let a = id("0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59");
        ctx.open(a.clone(), Some("/a".into()));
        let g = ctx.begin_switch();
        assert_eq!(ctx.lifecycle, ProjectLifecycle::Switching);
        assert!(ctx.is_current(g));
        assert_eq!(ctx.identity.as_ref(), Some(&a));
    }
}
