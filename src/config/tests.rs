use std::fs;

use tempfile::TempDir;

use super::{AgentBackendKind, AgentStage, Profile};

#[test]
fn profile_load_reports_manual_migration_for_legacy_versions() {
    for version in [1, 2] {
        let temporary = TempDir::new().expect("create temporary profile directory");
        let path = temporary.path().join("profile.toml");
        fs::write(&path, format!("schema_version = {version}\n")).expect("write legacy profile");

        let error = Profile::load(&path).expect_err("legacy profile must be rejected");
        let message = error.to_string();
        assert!(message.contains(&format!("profile schema v{version}")));
        assert!(message.contains("migrate manually to schema_version = 3"));
        assert!(message.contains("no automatic migration is available"));
        assert!(message.contains("agent.backend = \"pi\""));
        assert!(message.contains("built-in backend requires agent.model"));
    }
}

#[test]
fn documented_profiles_parse_and_validate() {
    for example in [
        include_str!("../../examples/profiles/basic.toml"),
        include_str!("../../examples/profiles/planned-archive.toml"),
        include_str!("../../examples/profiles/status-update.toml"),
        include_str!("../../.spec-loop.toml"),
    ] {
        let profile: Profile = toml::from_str(example).expect("valid example TOML");
        assert!(profile.validate().is_ok());
    }

    let status_update: Profile =
        toml::from_str(include_str!("../../examples/profiles/status-update.toml"))
            .expect("valid status-update profile");
    assert!(status_update.validate().is_ok());
    assert_eq!(
        status_update.archive.destination.as_deref(),
        Some("specs/implemented")
    );
    let finish_action = status_update
        .actions
        .on_finish
        .first()
        .expect("status-update finish action");
    assert_eq!(
        finish_action.argv,
        [
            "sed",
            "-i",
            r"s/^\*\*Status:\*\* Active$/**Status:** Complete/",
            "{spec_path}"
        ]
    );
}

#[test]
fn builtin_backend_defaults_limits_and_requires_explicit_model() {
    let missing_model: Profile = toml::from_str(
        "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n",
    )
    .expect("parse profile with omitted backend");
    assert_eq!(missing_model.agent.backend, AgentBackendKind::Builtin);
    let error = missing_model
        .validate()
        .expect_err("built-in backend requires explicit model");
    assert!(error.to_string().contains("agent.model is required"));

    let configured: Profile = toml::from_str(
            "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'model-name'\n",
        )
        .expect("parse built-in profile");
    configured.validate().expect("valid built-in profile");
    assert_eq!(configured.agent.max_tool_calls(), 50);
    assert_eq!(configured.agent.max_tool_output_bytes(), 1_048_576);
}

#[test]
fn validates_backend_specific_tool_limits() {
    for (field, value) in [("max_tool_calls", 0), ("max_tool_output_bytes", 0)] {
        let profile: Profile = toml::from_str(&format!(
                "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'model-name'\n{field} = {value}\n"
            ))
            .expect("parse zero builtin limit");
        assert!(profile.validate().is_err());

        let pi_profile: Profile = toml::from_str(&format!(
                "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nbackend = 'pi'\n{field} = 1\n"
            ))
            .expect("parse Pi builtin-only limit");
        let error = pi_profile
            .validate()
            .expect_err("Pi rejects built-in-only tool limits");
        assert!(
            error
                .to_string()
                .contains("only valid for the built-in backend")
        );
    }
}

#[test]
fn reasoning_effort_maps_thinking_levels_and_respects_stage_override() {
    let profile: Profile = toml::from_str(
            "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'model-name'\nthinking = 'off'\n[agent.stages.implementation]\nmodel = 'stage-model'\nthinking = 'xhigh'\n",
        )
        .expect("parse reasoning profile");
    let agent = &profile.agent;
    assert_eq!(
        agent
            .thinking_for(AgentStage::Planning)
            .map(|thinking| thinking.provider_values().1),
        Some("none")
    );
    assert_eq!(
        agent
            .thinking_for(AgentStage::Implementation)
            .map(|thinking| thinking.provider_values().1),
        Some("xhigh")
    );
    assert_eq!(agent.model_for(AgentStage::Review), Some("model-name"));
    assert_eq!(
        agent.model_for(AgentStage::Implementation),
        Some("stage-model")
    );
    for (input, expected) in [
        ("minimal", "minimal"),
        ("low", "low"),
        ("medium", "medium"),
        ("high", "high"),
        ("max", "max"),
    ] {
        let profile: Profile = toml::from_str(&format!(
                "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'model-name'\nthinking = {input:?}\n"
            ))
            .expect("parse thinking profile");
        assert_eq!(
            profile
                .agent
                .thinking_for(AgentStage::Review)
                .map(|thinking| thinking.provider_values().1),
            Some(expected)
        );
    }
}

#[test]
fn rejects_pi_resource_overrides_in_profile() {
    for (name, value) in [
        ("trust_project", "false"),
        ("allow_mcp", "false"),
        ("extensions", "[]"),
        ("skills", "[]"),
        ("writer_tools", "['read']"),
    ] {
        let profile = format!(
            "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nbackend = 'pi'\n{name} = {value}\n"
        );
        assert!(
            toml::from_str::<Profile>(&profile).is_err(),
            "agent.{name} unexpectedly accepted"
        );
    }
}

#[test]
fn rejects_unknown_profile_fields() {
    let result = toml::from_str::<Profile>(
        "schema_version = 3\nunknown = true\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nbackend = 'pi'\n",
    );
    assert!(result.is_err());
}

#[test]
fn validates_pi_thinking_levels_for_global_and_stage_settings() {
    for level in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
        let profile: Profile = toml::from_str(&format!(
                "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nbackend = 'pi'\nthinking = {level:?}\n"
            ))
            .expect("parse supported thinking level");
        assert!(profile.validate().is_ok(), "thinking level {level:?}");
    }

    for section in [
        "[agent]",
        "[agent.stages.planning]",
        "[agent.stages.implementation]",
        "[agent.stages.review]",
    ] {
        let agent = if section == "[agent]" {
            format!("{section}\nbackend = 'pi'\nthinking = 'unsupported'\n")
        } else {
            format!("[agent]\nbackend = 'pi'\n{section}\nthinking = 'unsupported'\n")
        };
        let profile = format!(
            "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n{agent}"
        );
        assert!(
            toml::from_str::<Profile>(&profile).is_err(),
            "unsupported thinking level in {section} must fail deserialization"
        );
    }
}

#[test]
fn rejects_unmatched_closing_brace_before_placeholder() {
    for value in ["}{spec_id}", "prefix}{spec_id}"] {
        let profile: Profile = toml::from_str(&format!(
                "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nbackend = 'pi'\n[[actions.on_finish]]\nname = 'placeholder test'\nargv = ['printf', {value:?}]\n"
            ))
            .expect("parse malformed placeholder profile");
        assert!(
            profile
                .validate()
                .is_err_and(|error| error.to_string().contains("unmatched closing brace"))
        );
    }
}

#[test]
fn check_argv_keeps_non_template_braces_as_literal_data() {
    let profile: Profile = toml::from_str(
            "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nbackend = 'pi'\n[[checks]]\nname = 'brace arguments'\nargv = ['grep', '-E', '^[0-9]{3}$', '{unknown', '}']\n",
        )
        .expect("parse check with literal braces");
    profile
        .validate()
        .expect("literal check argv braces are not templates");
}

#[test]
fn minimal_profile_defaults_review_context_and_empty_actions() {
    let profile: Profile = toml::from_str(
            "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nbackend = 'pi'\n",
        )
        .expect("parse minimal profile");
    profile.validate().expect("validate minimal profile");
    assert_eq!(profile.completion.max_passes, 0);
    assert_eq!(profile.review.context.argv, ["git", "diff"]);
    assert!(profile.actions.on_finish.is_empty());
}
