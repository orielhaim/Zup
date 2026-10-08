mod condition {
    //! Unit tests for condition parsing and evaluation.

    use std::collections::BTreeSet;

    use rstest::rstest;
    use zup_core::{ComponentId, Condition, ConditionError};

    fn id(value: &str) -> ComponentId {
        ComponentId::new(value).unwrap()
    }

    fn selected(values: &[&str]) -> BTreeSet<ComponentId> {
        values.iter().map(|value| id(value)).collect()
    }

    #[test]
    fn simple_component() {
        let condition = Condition::parse(r#"component("cli")"#).unwrap();
        assert_eq!(condition, Condition::Component(id("cli")));
        assert!(condition.evaluate(&selected(&["cli"])));
        assert!(!condition.evaluate(&selected(&["core"])));
    }

    #[test]
    fn not_and_or_and_precedence() {
        let condition =
            Condition::parse(r#"component("a") && component("b") || component("c")"#).unwrap();
        assert_eq!(
            condition,
            Condition::Or(
                Box::new(Condition::And(
                    Box::new(Condition::Component(id("a"))),
                    Box::new(Condition::Component(id("b")))
                )),
                Box::new(Condition::Component(id("c")))
            )
        );

        assert!(condition.evaluate(&selected(&["c"])));
        assert!(condition.evaluate(&selected(&["a", "b"])));
        assert!(!condition.evaluate(&selected(&["a"])));
    }

    #[test]
    fn parentheses_override_precedence() {
        let condition =
            Condition::parse(r#"component("a") && (component("b") || component("c"))"#).unwrap();
        assert!(condition.evaluate(&selected(&["a", "c"])));
        assert!(!condition.evaluate(&selected(&["c"])));
    }

    #[test]
    fn not_binds_tighter_than_and() {
        // `!(a && b)` would be false here; only `(!(a) && b)` is true.
        let condition = Condition::parse(r#"!component("a") && component("b")"#).unwrap();
        assert!(condition.evaluate(&selected(&["b"])));
        assert!(!condition.evaluate(&selected(&["a", "b"])));
    }

    #[test]
    fn referenced_components() {
        let condition =
            Condition::parse(r#"!(component("a") || component("b")) && component("c")"#).unwrap();
        let refs = condition.referenced_components();
        assert_eq!(refs, selected(&["a", "b", "c"]), "referenced: {refs:?}");
    }

    #[test]
    fn display_roundtrip() {
        let source = r#"!component("a") && component("b") || component("c")"#;
        let condition = Condition::parse(source).unwrap();
        let reparsed = Condition::parse(&condition.to_string()).unwrap();
        assert_eq!(condition, reparsed);
    }

    #[rstest]
    #[case::empty("")]
    #[case::missing_call("component cli")]
    #[case::missing_id(r#"component()"#)]
    #[case::unbalanced_paren("(component(\"a\")")]
    #[case::unknown_fn("foo(\"a\")")]
    #[case::stray_text("component(\"a\") leftover")]
    fn rejects_malformed(#[case] source: &str) {
        let err = Condition::parse(source).unwrap_err();
        assert!(
            matches!(
                err,
                ConditionError::UnexpectedEnd
                    | ConditionError::UnexpectedToken { .. }
                    | ConditionError::ExpectedComponentId
            ),
            "source: {source}, err: {err:?}"
        );
    }
}

mod ids {
    //! Unit tests for domain identifiers and extensions.

    use rstest::rstest;
    use zup_core::{
        AppId, ComponentId, FileExtension, NonEmptyString, PluginId, ProtocolScheme, ValueError,
    };

    #[rstest]
    #[case::app(AppId::new("com.acme.acme"), "com.acme.acme")]
    #[case::trims(AppId::new("  com.acme.acme  "), "com.acme.acme")]
    fn valid_ids(#[case] result: Result<AppId, ValueError>, #[case] expected: &str) {
        let id = result.unwrap();
        assert_eq!(id.as_str(), expected);
    }

    #[rstest]
    #[case::letter("plugin")]
    #[case::digit("9plugin")]
    #[case::portable("plugin.name_1-x")]
    fn valid_plugin_ids(#[case] value: &str) {
        assert_eq!(PluginId::new(value).unwrap().as_str(), value);
    }

    /// A plugin id has to survive being a path segment and a registry key name, so
    /// it must start alphanumeric and must carry no separator.
    #[rstest]
    #[case::empty("")]
    #[case::bad_first_character(".plugin")]
    #[case::slash("plugin/name")]
    #[case::backslash("plugin\\name")]
    fn invalid_plugin_ids(#[case] value: &str) {
        assert!(PluginId::new(value).is_err(), "value: {value}");
    }

    #[test]
    fn empty_ids_rejected() {
        assert_eq!(
            AppId::new("").unwrap_err(),
            ValueError::Empty { kind: "app id" }
        );
        assert_eq!(
            ComponentId::new("   ").unwrap_err(),
            ValueError::Empty {
                kind: "component id"
            }
        );
        assert_eq!(
            NonEmptyString::new("").unwrap_err(),
            ValueError::Empty { kind: "name" }
        );
    }

    #[rstest]
    #[case::simple("acme")]
    #[case::with_plus("svn+ssh")]
    fn valid_scheme(#[case] scheme: &str) {
        assert!(ProtocolScheme::new(scheme).is_ok(), "scheme: {scheme}");
    }

    #[rstest]
    #[case::empty("")]
    #[case::starts_digit("1acme")]
    #[case::has_slash("ac/me")]
    fn invalid_scheme(#[case] scheme: &str) {
        assert!(ProtocolScheme::new(scheme).is_err(), "scheme: {scheme}");
    }

    #[rstest]
    #[case::simple(".acme")]
    #[case::multi_char(".tar.gz")]
    fn valid_extension(#[case] extension: &str) {
        assert!(
            FileExtension::new(extension).is_ok(),
            "extension: {extension}"
        );
    }

    #[rstest]
    #[case::missing_dot("acme")]
    #[case::only_dot(".")]
    #[case::forward_slash("./acme")]
    fn invalid_extension(#[case] extension: &str) {
        assert!(
            FileExtension::new(extension).is_err(),
            "extension: {extension}"
        );
    }
}

mod path {
    //! Unit tests for portable relative paths.

    use std::path::Path;

    use rstest::rstest;
    use zup_core::{RelativePath, RelativePathError};

    /// Separators are normalized to `/` on the way in, so a Windows-authored path
    /// is byte-identical to its POSIX spelling.
    #[rstest]
    #[case::nested("bin/helpers/foo.dll", "bin/helpers/foo.dll")]
    #[case::backslashes("bin\\helpers\\foo.dll", "bin/helpers/foo.dll")]
    fn paths_normalize_their_separators(#[case] source: &str, #[case] expected: &str) {
        let path = RelativePath::new(source).unwrap();
        assert_eq!(path.as_str(), expected);
        assert_eq!(path.to_string(), expected);
    }

    #[test]
    fn file_name_and_parent() {
        let path = RelativePath::new("bin/helpers/foo.dll").unwrap();
        assert_eq!(path.file_name(), "foo.dll");
        assert_eq!(path.parent().unwrap().as_str(), "bin/helpers");
        assert_eq!(path.component_count(), 3);
    }

    /// An absolute or traversing path must be refused at every entry point, including
    /// the `Path` adapter, or a caller can smuggle one in through `OsStr`.
    #[rstest]
    #[case::empty("")]
    #[case::absolute("/etc/passwd")]
    #[case::parent("../secret")]
    #[case::embedded_parent("a/../b")]
    #[case::double_slash("a//b")]
    fn invalid_paths(#[case] source: &str) {
        assert!(RelativePath::new(source).is_err(), "source: {source}");
    }

    #[test]
    fn from_path_rejects_absolute_and_parent() {
        assert!(matches!(
            RelativePath::from_path(Path::new("/abs")),
            Err(RelativePathError::Absolute { .. })
        ));
        assert!(matches!(
            RelativePath::from_path(Path::new("a/../b")),
            Err(RelativePathError::ParentTraversal { .. })
        ));
        assert!(RelativePath::from_path(Path::new("a/b.txt")).is_ok());
    }
}

mod prerequisite {
    use semver::VersionReq;
    use zup_core::{
        InstalledPackage, InstalledPackageId, PrerequisiteId, PrerequisiteRequirement, Runtime,
        RuntimeRequirementId, Template,
    };

    /// A prerequisite id ends up in a command line, so anything that could be read
    /// as a path, an argument separator, or a flag is refused.
    #[test]
    fn prerequisite_id_rejects_path_and_argument_characters() {
        assert!(PrerequisiteId::new("vc-x64").is_ok());
        assert!(PrerequisiteId::new("../vc").is_err());
        assert!(PrerequisiteId::new("vc x64").is_err());
        assert!(PrerequisiteId::new("vc;calc").is_err());
    }

    #[test]
    fn runtime_requirement_ids_are_namespaced_and_opaque() {
        assert!(RuntimeRequirementId::new("windows.vc.v14").is_ok());
        assert!(RuntimeRequirementId::new("com.example.runtime_2").is_ok());
        assert!(RuntimeRequirementId::new("").is_err());
        assert!(RuntimeRequirementId::new("Windows.Vc").is_err());
        assert!(RuntimeRequirementId::new("windows..vc").is_err());
        assert!(RuntimeRequirementId::new("windows.vc.").is_err());
        assert!(RuntimeRequirementId::new("windows.vc runtime").is_err());
        assert!(RuntimeRequirementId::new("windows-vc").is_err());
        assert!(RuntimeRequirementId::new("windows.vc_").is_err());
        assert!(RuntimeRequirementId::new("7windows.vc").is_err());
    }

    #[test]
    fn installed_package_ids_accept_provider_owned_identities() {
        let guid = InstalledPackageId::new("{F3017226-FE2A-4295-8A7C-971BF3207148}").unwrap();
        assert_eq!(guid.as_str(), "{F3017226-FE2A-4295-8A7C-971BF3207148}");
        assert!(InstalledPackageId::new("org.example.product").is_ok());
        assert!(InstalledPackageId::new("  ").is_err());
        assert!(InstalledPackageId::new("{ spaced }").is_err());
        assert!(InstalledPackageId::new("sub\\key\\product").is_err());
        assert!(InstalledPackageId::new("c:product").is_err());
        assert!(InstalledPackageId::new("product\ncode").is_err());
    }

    /// A requirement must be expressible without a registry hive, key, or product
    /// code, or an installer authored on Windows cannot be evaluated elsewhere.
    #[test]
    fn requirements_expose_only_portable_semantics() {
        let runtime = PrerequisiteRequirement::Runtime(Runtime {
            id: RuntimeRequirementId::new("windows.vc.v14").unwrap(),
            version: Some(VersionReq::parse(">=14.0.0").unwrap()),
        });
        assert_eq!(runtime.kind_name(), "runtime");
        let package = PrerequisiteRequirement::InstalledPackage(InstalledPackage {
            id: InstalledPackageId::new("{F3017226-FE2A-4295-8A7C-971BF3207148}").unwrap(),
            version: None,
        });
        assert_eq!(package.kind_name(), "installed_package");
        let file = PrerequisiteRequirement::FileVersion(zup_core::FileVersion {
            path: Template::parse("C:/Program Files/Acme/host.exe").unwrap(),
            version: Some(VersionReq::parse(">=1.0").unwrap()),
        });
        assert_eq!(file.kind_name(), "file_version");
        for requirement in [&runtime, &package, &file] {
            let json = serde_json::to_value(requirement).unwrap();
            for legacy in ["product_code", "hive", "key"] {
                assert!(
                    json.get(legacy).is_none(),
                    "{legacy} leaked back into {json}"
                );
            }
        }
    }
}

mod target {
    use rstest::rstest;
    use zup_core::{Source, TargetProfile, TargetProfileId, TargetTriple};

    /// An alias is rewritten to the triple the toolchain actually names, so a plan
    /// built on `arm64-` and one built on `aarch64-` are the same target.
    #[rstest]
    #[case::arm64("arm64-pc-windows-msvc", "aarch64-pc-windows-msvc")]
    #[case::x64("x64-pc-windows-msvc", "x86_64-pc-windows-msvc")]
    fn canonicalizes_target_aliases(#[case] alias: &str, #[case] canonical: &str) {
        let target = TargetTriple::parse(alias).unwrap();
        assert_eq!(target.as_str(), canonical);
        assert_eq!(
            target.architecture().to_string(),
            canonical.split('-').next().unwrap()
        );
        assert_eq!(target.operating_system().to_string(), "windows");
        assert_eq!(target, TargetTriple::parse(canonical).unwrap());
    }

    #[rstest]
    #[case::malformed("not-a-target")]
    #[case::unknown_components("unknown-unknown-unknown")]
    fn rejects_invalid_target(#[case] source: &str) {
        assert!(TargetTriple::parse(source).is_err(), "source: {source}");
    }

    #[test]
    fn validates_target_profile_id() {
        let id = TargetProfileId::new("  windows-x64  ").unwrap();
        assert_eq!(id.as_str(), "windows-x64");
        assert!(TargetProfileId::new(" \t ").is_err());
    }

    /// Deserialization is a normalization point: an aliased triple on the wire must
    /// come back canonical, or two records naming the same target compare unequal.
    #[test]
    fn target_profile_deserializes_canonical_target() {
        let profile: TargetProfile = serde_json::from_str(
            r#"{"target":"arm64-pc-windows-msvc","source":{"directory":"dist"}}"#,
        )
        .unwrap();

        assert_eq!(
            profile.target,
            TargetTriple::parse("aarch64-pc-windows-msvc").unwrap()
        );
        assert_eq!(profile.source, Source::new("dist".into()).unwrap());
        assert_eq!(profile.frontend, None);
        assert_eq!(profile.install, None);
    }
}

mod template {
    //! Unit tests for template parsing.

    use rstest::rstest;
    use zup_core::{
        INSTALL_LOCATIONS, InstallLocation, Template, TemplateError, TemplatePart, Variable,
    };

    /// Anything without a `${...}` is one literal, `$` included.
    #[rstest]
    #[case::plain("dist/app.exe")]
    #[case::bare_dollar("price: $5")]
    #[case::empty("")]
    fn literal_only(#[case] source: &str) {
        let template = Template::parse(source).unwrap();
        assert_eq!(template.as_literal(), Some(source));
        assert_eq!(template.to_string(), source);
    }

    #[rstest]
    #[case::install("${install}", Variable::Install)]
    #[case::app_id("${app.id}", Variable::AppId)]
    #[case::app_name("${app.name}", Variable::AppName)]
    #[case::app_version("${app.version}", Variable::AppVersion)]
    #[case::programs("${location.programs}", Variable::Location(InstallLocation::Programs))]
    fn one_variable(#[case] source: &str, #[case] expected: Variable) {
        let template = Template::parse(source).unwrap();
        assert_eq!(
            template.parts(),
            [TemplatePart::Variable(expected)],
            "source: {source}"
        );
        assert_eq!(template.to_string(), source);
    }

    /// The location names are a wire contract: a template authored against one
    /// spelling has to resolve, and every location in the table has to round-trip
    /// through its own name.
    #[test]
    fn install_locations_have_stable_semantic_names() {
        for (location, name) in [
            (InstallLocation::Programs, "programs"),
            (InstallLocation::UserData, "user_data"),
            (InstallLocation::SharedData, "shared_data"),
            (InstallLocation::Menu, "menu"),
            (InstallLocation::Desktop, "desktop"),
        ] {
            assert_eq!(location.as_str(), name);
            assert_eq!(InstallLocation::parse(name), Some(location));
        }
        assert_eq!(INSTALL_LOCATIONS.len(), 5, "one entry per location");
    }

    #[test]
    fn several_variables_and_literals() {
        let template = Template::parse("${location.user_data}/Programs/${app.name}").unwrap();
        assert_eq!(
            template.parts(),
            [
                TemplatePart::Variable(Variable::Location(InstallLocation::UserData)),
                TemplatePart::Literal("/Programs/".to_owned()),
                TemplatePart::Variable(Variable::AppName),
            ]
        );
        assert_eq!(
            template.to_string(),
            "${location.user_data}/Programs/${app.name}"
        );
    }

    #[rstest]
    #[case::unterminated("${install", TemplateError::UnterminatedVariable)]
    #[case::empty_name("${}", TemplateError::EmptyVariable)]
    #[case::unknown_location("${location.nope}", TemplateError::UnknownVariable { name: "location.nope".into() })]
    #[case::empty_location("${location.}", TemplateError::UnknownVariable { name: "location.".into() })]
    #[case::unknown_var("${known.foo}", TemplateError::UnknownVariable { name: "known.foo".into() })]
    // The `${known.*}` family is retired: every one of those names is now an
    // unknown variable, so they are all the same case.
    #[case::legacy("${known.program_files}", TemplateError::UnknownVariable { name: "known.program_files".into() })]
    #[case::unknown_simple("${nope}", TemplateError::UnknownVariable { name: "nope".into() })]
    fn rejects_malformed(#[case] source: &str, #[case] expected: TemplateError) {
        let err = Template::parse(source).unwrap_err();
        assert_eq!(err, expected, "source: {source}");
    }
}

mod template_substitute {
    //! Template substitution unit tests.

    use zup_core::{InstallLocation, Template, Variable, VariableValue};

    #[test]
    fn substitutes_literal_app_variables() {
        let template = Template::parse("${app.id}/${app.name}/${app.version}").unwrap();
        let resolved = template.substitute(|var| match var {
            Variable::AppId => Some(VariableValue::Literal("com.acme".into())),
            Variable::AppName => Some(VariableValue::Literal("Acme".into())),
            Variable::AppVersion => Some(VariableValue::Literal("1.4.0".into())),
            _ => None,
        });
        assert_eq!(resolved.to_string(), "com.acme/Acme/1.4.0");
        assert_eq!(resolved.as_literal(), Some("com.acme/Acme/1.4.0"));
    }

    #[test]
    fn substitutes_install_with_template_and_merges_literals() {
        let install = Template::parse("${location.programs}/Acme").unwrap();
        let template = Template::parse("${install}/bin").unwrap();
        let resolved = template.substitute(|var| match var {
            Variable::Install => Some(VariableValue::Template(install.clone())),
            _ => None,
        });

        assert_eq!(
            resolved.parts(),
            [
                zup_core::TemplatePart::Variable(Variable::Location(InstallLocation::Programs)),
                zup_core::TemplatePart::Literal("/Acme/bin".to_owned()),
            ]
        );
        assert_eq!(resolved.to_string(), "${location.programs}/Acme/bin");
    }

    #[test]
    fn leaves_location_variables_unresolved() {
        let template = Template::parse("${location.user_data}/Programs/${app.name}").unwrap();
        let resolved = template.substitute(|var| match var {
            Variable::AppName => Some(VariableValue::Literal("Acme".into())),
            _ => None,
        });
        assert_eq!(resolved.to_string(), "${location.user_data}/Programs/Acme");
    }

    #[test]
    fn adjacent_literals_normalized() {
        let template = Template::parse("a${app.name}b${app.version}c").unwrap();
        let resolved = template.substitute(|var| match var {
            Variable::AppName => Some(VariableValue::Literal("-".into())),
            Variable::AppVersion => Some(VariableValue::Literal("-".into())),
            _ => None,
        });
        assert_eq!(resolved.parts().len(), 1);
        assert_eq!(resolved.to_string(), "a-b-c");
    }

    #[test]
    fn contains_variable_detects_install() {
        assert!(
            Template::parse("${install}/x")
                .unwrap()
                .contains_variable(Variable::Install)
        );
        assert!(
            !Template::parse("${location.desktop}/x")
                .unwrap()
                .contains_variable(Variable::Install)
        );
    }
}
