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
        include_str!("../../examples/profiles/builtin-call-limit.toml"),
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
fn builtin_backend_omits_call_limit_and_requires_explicit_model() {
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
    assert_eq!(configured.agent.max_tool_calls, None);
    assert_eq!(configured.agent.max_tool_output_bytes(), 1_048_576);

    let explicitly_limited: Profile = toml::from_str(
        "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'model-name'\nmax_tool_calls = 3\n",
    )
    .expect("parse explicit call limit");
    explicitly_limited
        .validate()
        .expect("positive explicit call limit is valid");
    assert_eq!(explicitly_limited.agent.max_tool_calls, Some(3));
}

#[test]
fn validates_backend_specific_tool_limits() {
    for (field, value) in [("max_tool_calls", 0), ("max_tool_output_bytes", 0)] {
        let profile: Profile = toml::from_str(&format!(
                "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'model-name'\n{field} = {value}\n"
            ))
            .expect("parse zero builtin limit");
        let error = profile
            .validate()
            .expect_err("zero built-in limits must be rejected");
        assert!(error.to_string().contains("must be positive when set"));

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
fn rejects_unknown_agent_call_limit_field() {
    let profile = "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'model-name'\nmax_tool_callz = 1\n";
    assert!(toml::from_str::<Profile>(profile).is_err());
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

#[test]
fn context_window_limits_must_be_positive_and_builtin_only() {
    for (section, field, expected) in [
        ("", "context_window_tokens", "agent.context_window_tokens"),
        (
            "[agent.stages.planning]\n",
            "context_window_tokens",
            "agent.stages.planning.context_window_tokens",
        ),
        (
            "[agent.stages.implementation]\n",
            "context_window_tokens",
            "agent.stages.implementation.context_window_tokens",
        ),
        (
            "[agent.stages.review]\n",
            "context_window_tokens",
            "agent.stages.review.context_window_tokens",
        ),
    ] {
        let profile: Profile = toml::from_str(&format!(
            "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'model-name'\n{section}{field} = 0\n"
        ))
        .expect("parse zero context limit");
        let error = profile
            .validate()
            .expect_err("zero context limit must fail");
        assert!(error.to_string().contains(expected));
        assert!(error.to_string().contains("must be positive"));

        let pi_profile: Profile = toml::from_str(&format!(
            "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nbackend = 'pi'\n{section}{field} = 100\n"
        ))
        .expect("parse Pi context limit");
        let error = pi_profile
            .validate()
            .expect_err("Pi must reject context limits");
        assert!(error.to_string().contains(expected));
        assert!(
            error
                .to_string()
                .contains("only valid for the built-in backend")
        );
    }
}

#[test]
fn context_window_limit_follows_stage_model_inheritance() {
    let inherited: Profile = toml::from_str(
        "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'global-model'\ncontext_window_tokens = 1000\n",
    )
    .expect("parse inherited-model limits");
    inherited.validate().expect("valid inherited-model limits");
    for stage in [
        AgentStage::Planning,
        AgentStage::Implementation,
        AgentStage::Review,
    ] {
        assert_eq!(inherited.agent.context_window_tokens_for(stage), Some(1000));
    }

    let stage_limit: Profile = toml::from_str(
        "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'global-model'\ncontext_window_tokens = 1000\n[agent.stages.review]\ncontext_window_tokens = 750\n",
    )
    .expect("parse stage limit");
    stage_limit.validate().expect("valid stage limit");
    assert_eq!(
        stage_limit
            .agent
            .context_window_tokens_for(AgentStage::Review),
        Some(750)
    );

    let overridden_model: Profile = toml::from_str(
        "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'global-model'\ncontext_window_tokens = 1000\n[agent.stages.implementation]\nmodel = 'stage-model'\n",
    )
    .expect("parse stage model override");
    overridden_model
        .validate()
        .expect("valid stage model override");
    assert_eq!(
        overridden_model
            .agent
            .context_window_tokens_for(AgentStage::Implementation),
        None
    );

    let override_with_limit: Profile = toml::from_str(
        "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'global-model'\ncontext_window_tokens = 1000\n[agent.stages.planning]\nmodel = 'stage-model'\ncontext_window_tokens = 500\n",
    )
    .expect("parse overridden model with explicit stage limit");
    override_with_limit
        .validate()
        .expect("valid overridden model limit");
    assert_eq!(
        override_with_limit
            .agent
            .context_window_tokens_for(AgentStage::Planning),
        Some(500)
    );

    let no_limit: Profile = toml::from_str(
        "schema_version = 3\n[specs]\nactive_globs = ['specs/*.md']\nid_regex = '(?P<id>.*)'\n[agent]\nmodel = 'global-model'\n",
    )
    .expect("parse absent context limit");
    no_limit.validate().expect("valid absent limit");
    assert_eq!(
        no_limit.agent.context_window_tokens_for(AgentStage::Review),
        None
    );
}
