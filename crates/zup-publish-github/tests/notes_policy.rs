use zup_publish_github::NotesPolicy;

#[test]
fn a_notes_policy_decides_what_body_is_written() {
    let rows = vec![zup_publish_github::DownloadRow {
        name: "Acme-Windows-Setup.exe".to_owned(),
        size: 418 * 1024 * 1024,
        role: "install".to_owned(),
    }];
    let body = |policy, generated, file, text| {
        zup_publish_github::notes_compose(&policy, generated, file, text, &rows)
    };

    assert!(body(NotesPolicy::None, Some("text".into()), None, None).is_none());

    for (policy, generated, file, text) in [
        (
            NotesPolicy::Generated,
            Some("generated text\n".into()),
            None,
            None,
        ),
        (
            NotesPolicy::File("CHANGELOG.md".to_owned()),
            None,
            Some("Curated notes.\n".to_owned()),
            None,
        ),
        (
            NotesPolicy::Text("release notes".to_owned()),
            None,
            None,
            Some("Caller text.\n".to_owned()),
        ),
    ] {
        let lede = match &policy {
            NotesPolicy::Generated => "generated text",
            NotesPolicy::File(_) => "Curated notes.",
            NotesPolicy::Text(_) => "Caller text.",
            NotesPolicy::None => unreachable!(),
        };
        let composed = body(policy, generated, file, text).expect("a body");
        assert!(composed.starts_with(lede), "{composed}");
        assert!(composed.contains("Acme-Windows-Setup.exe"), "{composed}");
        assert!(composed.contains("418 MiB"), "{composed}");
    }
}
