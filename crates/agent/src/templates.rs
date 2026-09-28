use anyhow::Result;
use gpui::SharedString;
use handlebars::Handlebars;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;

// Dev builds read the checkout's templates at runtime instead of embedding
// them; see the `assets` crate for the rationale.
util::fs_embed! {
    struct Assets,
    crate_relative = "src/templates",
    root_relative = "crates/agent/src/templates",
    include = ["*.hbs"],
}

/// The agent's prompt templates (the system prompt / guide, and any follow-up
/// template files).
///
/// Templates can be overridden at runtime: pass an overrides directory to
/// [`Templates::new_with_overrides`] and drop a `*.hbs` file into it named
/// after the built-in template it replaces (e.g. `system_prompt.hbs` replaces
/// the agent guide). Overrides are read at render time, so editing the file
/// takes effect on the next message without a rebuild or restart. A file that
/// fails to render falls back to the built-in template.
pub struct Templates(Handlebars<'static>, Option<PathBuf>);

impl Templates {
    pub fn new() -> Arc<Self> {
        Self::new_with_overrides(None)
    }

    /// Like [`Templates::new`], but consults `overrides_dir` for `*.hbs`
    /// files that replace the built-in templates of the same name.
    pub fn new_with_overrides(overrides_dir: Option<PathBuf>) -> Arc<Self> {
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(true);
        handlebars.register_helper("contains", Box::new(contains));
        handlebars.register_embed_templates::<Assets>().unwrap();
        Arc::new(Self(handlebars, overrides_dir))
    }

    /// Renders `name`, preferring a runtime override file
    /// (`<overrides_dir>/<name>.hbs`) over the built-in template when one is
    /// configured. Falls back to the built-in when the override is missing,
    /// unreadable, or fails to render.
    fn render_with_override(&self, name: &str, context: &impl Serialize) -> Result<String> {
        let Some(overrides_dir) = self.1.as_ref() else {
            return Ok(self.0.render(name, context)?);
        };

        let override_path = overrides_dir.join(name);
        if override_path.is_file() {
            match std::fs::read_to_string(&override_path) {
                Ok(source) => match self.0.render_template(&source, context) {
                    Ok(rendered) => return Ok(rendered),
                    Err(error) => log::error!(
                        "Failed to render prompt override {override_path:?}: {error:#}; \
                         falling back to the built-in template"
                    ),
                },
                Err(error) => {
                    log::error!("Failed to read prompt override {override_path:?}: {error:#}")
                }
            }
        }

        Ok(self.0.render(name, context)?)
    }
}

pub trait Template: Sized {
    const TEMPLATE_NAME: &'static str;

    fn render(&self, templates: &Templates) -> Result<String>
    where
        Self: Serialize + Sized,
    {
        templates.render_with_override(Self::TEMPLATE_NAME, self)
    }
}

#[derive(Serialize)]
pub struct SystemPromptTemplate<'a> {
    #[serde(flatten)]
    pub project: &'a prompt_store::ProjectContext,
    pub available_tools: Vec<SharedString>,
    pub model_name: Option<String>,
    pub date: String,
    /// Contents of the user-global `~/.config/zed/AGENTS.md` file (or the
    /// platform equivalent), if present and non-empty.
    pub user_agents_md: Option<SharedString>,
    /// Whether agent-run terminal commands are wrapped in an OS-level
    /// sandbox for this thread. When `true` — and the `terminal` tool is
    /// in `available_tools` — the rendered prompt describes the sandbox's
    /// read/write/network rules and the per-command flags the model can
    /// request to relax them. Otherwise the prompt omits the sandbox
    /// section entirely.
    pub sandboxing: bool,
    /// Whether the host is Linux. The writable-temp story differs by
    /// platform (Linux exposes an ephemeral `tmpfs` over `/tmp`; other
    /// platforms provide a persistent per-thread `$TMPDIR`), so the sandbox
    /// section describes the right one rather than advertising a `$TMPDIR`
    /// that doesn't behave as stated.
    pub is_linux: bool,
    /// Whether sandboxed terminal commands run through WSL on Windows.
    pub is_windows: bool,
}

impl Template for SystemPromptTemplate<'_> {
    const TEMPLATE_NAME: &'static str = "system_prompt.hbs";
}

/// Handlebars helper for checking if an item is in a list
fn contains(
    h: &handlebars::Helper,
    _: &handlebars::Handlebars,
    _: &handlebars::Context,
    _: &mut handlebars::RenderContext,
    out: &mut dyn handlebars::Output,
) -> handlebars::HelperResult {
    let list = h
        .param(0)
        .and_then(|v| v.value().as_array())
        .ok_or_else(|| {
            handlebars::RenderError::new("contains: missing or invalid list parameter")
        })?;
    let query = h.param(1).map(|v| v.value()).ok_or_else(|| {
        handlebars::RenderError::new("contains: missing or invalid query parameter")
    })?;

    if list.contains(query) {
        out.write("true")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_prompt_template() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(rendered.contains("You are the Zed coding agent"));
        assert!(rendered.contains("Today's Date: 2026-01-01"));
        assert!(rendered.contains("## Fixing Diagnostics"));
        assert!(rendered.contains("test-model"));
    }

    #[test]
    fn test_system_prompt_renders_user_agents_md_before_project_rules() {
        use prompt_store::{ProjectContext, RulesFileContext, WorktreeContext};
        use util::rel_path::RelPath;

        let worktrees = vec![WorktreeContext {
            root_name: "my-project".to_string(),
            abs_path: std::path::Path::new("/tmp/my-project").into(),
            rules_file: Some(RulesFileContext {
                path_in_worktree: RelPath::from_unix_str("AGENTS.md").unwrap().into(),
                text: "project-specific guidance".to_string(),
                project_entry_id: 1,
            }),
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: Some("always be concise".into()),
            sandboxing: false,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("### Personal `AGENTS.md`"));
        assert!(rendered.contains("always be concise"));
        assert!(rendered.contains("### Project Rules"));
        assert!(rendered.contains("project-specific guidance"));

        let personal_idx = rendered.find("### Personal `AGENTS.md`").unwrap();
        let project_idx = rendered.find("### Project Rules").unwrap();
        assert!(
            personal_idx < project_idx,
            "personal AGENTS.md should render before project rules so project rules can override it"
        );
    }

    #[test]
    fn test_system_prompt_omits_sandbox_section_when_sandboxing_disabled() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(!rendered.contains("## Terminal sandbox"));
        assert!(!rendered.contains("allow_hosts"));
    }

    #[test]
    fn test_system_prompt_renders_sandbox_section_with_worktrees_when_enabled() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![
            WorktreeContext {
                root_name: "alpha".to_string(),
                abs_path: std::path::Path::new("/tmp/alpha").into(),
                rules_file: None,
            },
            WorktreeContext {
                root_name: "beta".to_string(),
                abs_path: std::path::Path::new("/tmp/beta").into(),
                rules_file: None,
            },
        ];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        assert!(rendered.contains("`/tmp/alpha`"));
        assert!(rendered.contains("`/tmp/beta`"));
        assert!(rendered.contains("allow_hosts"));
        assert!(rendered.contains("allow_all_hosts: true"));
        assert!(rendered.contains("fs_write_paths"));
        assert!(rendered.contains("allow_fs_write_all: true"));
        assert!(rendered.contains("unsandboxed: true"));
        assert!(rendered.contains("`.git` directories remain protected"));
        assert!(rendered.contains("Git metadata writes are never grantable inside the sandbox"));
        assert!(rendered.contains("request `unsandboxed: true` with a reason"));
        assert!(rendered.contains("git --no-optional-locks status"));
        assert!(rendered.contains("for the rest of the thread"));
        // macOS tolerates granting a not-yet-existing path, so the
        // existing-directory requirement must not be stated there; the
        // `create_directory` flow is the preferred guidance instead.
        assert!(!rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_linux_sandbox_section_omits_tmpdir() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![WorktreeContext {
            root_name: "alpha".to_string(),
            abs_path: std::path::Path::new("/tmp/alpha").into(),
            rules_file: None,
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: true,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        // On Linux we must not advertise the special persistent `$TMPDIR`.
        assert!(!rendered.contains("$TMPDIR"));
        assert!(rendered.contains("`/tmp` is writable"));
        assert!(rendered.contains("`/tmp/alpha`"));
        // Linux write grants must already exist (bwrap binds existing paths).
        assert!(rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_windows_sandbox_section_rejects_host_specific_network() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![WorktreeContext {
            root_name: "alpha".to_string(),
            abs_path: std::path::Path::new("C:/Users/me/project").into(),
            rules_file: None,
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: true,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("commands run inside WSL under Bubblewrap"));
        assert!(rendered.contains("Protected Git metadata remains read-only"));
        assert!(rendered.contains("do not use this on Windows"));
        assert!(rendered.contains("such requests are rejected"));
        assert!(rendered.contains("allow_all_hosts: true"));
        assert!(rendered.contains("git --no-optional-locks status"));
        // Out-of-project `create_directory` grants aren't supported on Windows,
        // so the prompt must not recommend that flow; it suggests granting the
        // nearest existing parent instead.
        assert!(rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("nearest existing parent directory"));
        assert!(!rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_sandbox_section_handles_zero_worktrees() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        assert!(rendered.contains("No project directories are currently writable"));
    }

    #[test]
    fn test_system_prompt_omits_sandbox_section_when_terminal_tool_unavailable() {
        // A profile can disable the terminal tool entirely; the prompt must not
        // describe a sandboxed `terminal` tool the model doesn't have.
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(!rendered.contains("## Terminal sandbox"));
        assert!(!rendered.contains("allow_hosts"));
    }

    #[test]
    fn test_system_prompt_omits_user_agents_md_section_when_absent() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(!rendered.contains("### Personal `AGENTS.md`"));
    }

    #[test]
    fn test_system_prompt_does_not_render_legacy_zed_rules_section() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(!rendered.contains("The user has specified the following rules"));
        assert!(!rendered.contains("Rules title:"));
    }
}
