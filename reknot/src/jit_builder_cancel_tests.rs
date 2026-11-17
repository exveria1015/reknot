use super::*;

#[test]
#[ignore = "real Cargo producer: stage cancellation, nonpublication and recovery"]
fn superseded_stages_release_lock_and_do_not_publish() {
    let dir = super::tests::directory();
    for name in ["logic", "support"] {
        fs::create_dir_all(dir.join(name).join("src")).unwrap();
    }
    let sdk = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
    fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers=['logic','support']\nresolver='2'\n[profile.dev]\ndebug=0\ncodegen-units=1\n").unwrap();
    fs::write(dir.join("support/Cargo.toml"), format!("[package]\nname='cancel_support'\nversion='0.0.0'\nedition='2021'\n[lib]\ncrate-type=['dylib']\n[dependencies]\nreknot={{path='{sdk}',default-features=false,features=['typed']}}\n")).unwrap();
    fs::write(dir.join("support/src/lib.rs"), "pub use reknot::*;\n").unwrap();
    fs::write(dir.join("logic/Cargo.toml"), "[package]\nname='cancel_logic'\nversion='0.0.0'\nedition='2021'\n[dependencies]\nreknot={package='cancel_support',path='../support'}\n").unwrap();
    let source = include_str!("../tests/fixtures/jit_typed_logic.rs");
    let logic = dir.join("logic/src/lib.rs");
    fs::write(&logic, source).unwrap();
    let result = std::process::Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut options = BuildOptions::new(
        dir.join("Cargo.toml"),
        "cancel_logic",
        "cancel_support",
        dir.join("cache"),
        dir.join("initial.object"),
    );
    options.offline = true;
    let initial = build(&options).unwrap();
    options.foundation_manifest = Some(initial.manifest);
    let environment = std::env::vars_os().collect();
    for (index, stage) in [
        "before_foundation",
        "before_support_cargo",
        "before_logic_cargo",
        "after_logic_cargo",
        "before_publication",
    ]
    .into_iter()
    .enumerate()
    {
        options.output = dir.join(format!("cancelled-{index}.object"));
        let foundation = if stage == "before_support_cargo" {
            None
        } else {
            options.foundation_manifest.as_deref()
        };
        let outcome = prepare_with_environment(&options, &environment)
            .unwrap()
            .build_while_current(foundation, &|current| current != stage)
            .unwrap();
        assert!(matches!(outcome, BuildOutcome::Superseded(found) if found == stage));
        assert!(!options.output.exists());
        assert!(!options.output.with_extension("object.json").exists());
        let recovered = build(&options).unwrap();
        super::super::LayeredArtifact::read(recovered.manifest).unwrap();
    }
    fs::write(&logic, "compile_error!(\"obsolete compilation\");").unwrap();
    options.output = dir.join("obsolete-error.object");
    let outcome = prepare_with_environment(&options, &environment)
        .unwrap()
        .build_while_current(options.foundation_manifest.as_deref(), &|stage| {
            stage != "after_logic_cargo"
        })
        .unwrap();
    assert!(matches!(
        outcome,
        BuildOutcome::Superseded("after_logic_cargo")
    ));
    assert!(!options.output.exists());
    fs::write(&logic, source).unwrap();
    build(&options).unwrap();
    eprintln!("stage cancellation and recovery fixture: {}", dir.display());
}
