use std::path::Path;

use super::{PromptContext, PromptRole, build_prompt};
use crate::config::Profile;

#[test]
fn finding_audits_receive_skeptical_proportionate_correction_contract() {
    let profile: Profile = toml::from_str(
            "schema_version = 3\n[specs]\nactive_globs=['specs/*.md']\nid_regex='(?P<id>.*)'\n[agent]\nbackend='pi'\n",
        )
        .expect("profile");
    for role in [PromptRole::PlanAudit, PromptRole::ReviewAudit] {
        let prompt = build_prompt(
            Path::new("/project"),
            &profile,
            PromptContext {
                role,
                spec_id: "1",
                spec_path: Path::new("specs/1.md"),
                plan_path: None,
                feedback: None,
                validation_feedback: None,
                checks: "",
            },
        )
        .expect("build audit prompt");
        assert!(prompt.contains("Act as a skeptical gatekeeper, not a second reviewer."));
        assert!(prompt.contains("The simplest sufficient correct fix"));
        assert!(prompt.contains("`classification` reports factual validity only"));
        assert!(
            prompt.contains("A true but unnecessary observation is supported and not_required")
        );
        assert!(prompt.contains("Only supported findings marked required are sent for repair"));
        assert!(prompt.contains("Do not reject a proven, in-scope defect because its smallest correct fix is difficult or substantial"));
    }
}

#[test]
fn plan_roles_require_research_complete_executable_handoffs() {
    let profile: Profile = toml::from_str(
            "schema_version = 3\n[specs]\nactive_globs=['specs/*.md']\nid_regex='(?P<id>.*)'\n[agent]\nbackend='pi'\n",
        )
        .expect("profile");
    let build = |role| {
        build_prompt(
            Path::new("/project"),
            &profile,
            PromptContext {
                role,
                spec_id: "1",
                spec_path: Path::new("specs/1.md"),
                plan_path: Some(Path::new(".spec-loop/plans/1.md")),
                feedback: None,
                validation_feedback: None,
                checks: "",
            },
        )
        .expect("build handoff prompt")
    };

    let author = build(PromptRole::PlanAuthor);
    assert!(author.contains("Plan readiness is an implementation handoff commitment"));
    assert!(author.contains("Run relevant safe baselines, probes, or benchmarks"));
    assert!(author.contains("Do not defer research, consequential design choices"));
    assert!(author.contains("map every acceptance criterion to ordered changes and tests"));
    assert!(author.contains("Research belongs to the PlanAuthor"));
    assert!(author.contains("never return `blocked` because research"));
    assert!(author.contains("Only a genuinely unavailable external input, access, or approval"));
    assert!(author.contains("edit only the selected plan"));

    let reviewer = build(PromptRole::PlanReview);
    assert!(reviewer.contains("complete, evidence-backed implementation handoff"));
    assert!(reviewer.contains("Use `blocked` instead of approving"));

    let implementation = build(PromptRole::Implementation);
    assert!(implementation.contains("A plan's use of words such as `blocked`"));
    assert!(implementation.contains("Do not stop solely because the plan labels work blocked"));
    assert!(implementation.contains("ready_for_review` means the current workspace"));
    assert!(implementation.contains("resume from the current workspace and preserve prior work"));
    assert!(implementation.contains(
        "Unfinished work, its size, or the fact that a finding remains is not by itself a blocker"
    ));
    assert!(implementation.contains("Approved plans are proposed guidance"));
}

#[test]
fn review_context_wording_tracks_selected_backend_for_all_context_roles() {
    for (backend, extra, policy) in [
        (
            "pi",
            "",
            "Pi uses the user's effective configuration, tools, extensions, MCP servers, skills, project trust, context files, and inherited environment",
        ),
        (
            "builtin",
            "model = 'model-name'\n",
            "Shell commands run automatically without per-command approval via `/bin/sh -c`",
        ),
    ] {
        let profile: Profile = toml::from_str(&format!(
                "schema_version = 3\n[specs]\nactive_globs=['specs/*.md']\nid_regex='(?P<id>.*)'\n[agent]\nbackend='{backend}'\n{extra}[review.context]\nargv=['custom-review','argument with spaces']\n"
            ))
            .expect("profile");
        for role in [
            PromptRole::Review,
            PromptRole::ReviewAudit,
            PromptRole::ReviewClarification,
        ] {
            let prompt = build_prompt(
                Path::new("/project root"),
                &profile,
                PromptContext {
                    role,
                    spec_id: "1",
                    spec_path: Path::new("specs/1.md"),
                    plan_path: None,
                    feedback: None,
                    validation_feedback: None,
                    checks: "",
                },
            )
            .expect("build review-context prompt");
            assert!(prompt.contains("[\"custom-review\",\"argument with spaces\"]"));
            assert!(prompt.contains("Review-context working directory: /project root"));
            assert!(prompt.contains("selected backend's available tools"));
            assert!(prompt.contains("engine will not execute or capture it"));
            assert!(prompt.contains(policy));
            if backend == "builtin" {
                assert!(prompt.contains("This is not a sandbox"));
                assert!(prompt.contains("may access or transmit credentials and secrets"));
            }
        }
    }
}

#[test]
fn only_implementation_review_family_receives_context_and_generated_schema() {
    let profile: Profile = toml::from_str(
            "schema_version = 3\n[specs]\nactive_globs=['specs/*.md']\nid_regex='(?P<id>.*)'\n[agent]\nbackend='pi'\n[review.context]\nargv=['custom-review','--full']\n",
        )
        .expect("profile");
    for role in [
        PromptRole::Review,
        PromptRole::ReviewAudit,
        PromptRole::ReviewClarification,
    ] {
        let prompt = build_prompt(
            Path::new("/project"),
            &profile,
            PromptContext {
                role,
                spec_id: "1",
                spec_path: Path::new("specs/1.md"),
                plan_path: None,
                feedback: None,
                validation_feedback: None,
                checks: "",
            },
        )
        .expect("build review prompt");
        assert!(prompt.contains("[\"custom-review\",\"--full\"]"));
        assert!(prompt.contains("Review-context working directory: /project"));
        assert!(prompt.contains("selected backend's available tools"));
        assert!(prompt.contains("engine will not execute or capture it"));
        assert!(prompt.contains("Pi uses the user's effective configuration"));
        assert!(prompt.contains("$schema"));
    }
    let plan = build_prompt(
        Path::new("/project"),
        &profile,
        PromptContext {
            role: PromptRole::PlanReview,
            spec_id: "1",
            spec_path: Path::new("specs/1.md"),
            plan_path: None,
            feedback: None,
            validation_feedback: None,
            checks: "",
        },
    )
    .expect("build plan prompt");
    assert!(!plan.contains("custom-review"));
}
