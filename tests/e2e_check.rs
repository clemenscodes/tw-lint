use std::path::PathBuf;
use std::process::Command;

#[test]
#[ignore = "requires tailwindcss-language-server on PATH (run under `nix develop`)"]
fn check_exits_nonzero_on_the_fixture() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest.join("tests/fixtures/project");
    let status = Command::new(env!("CARGO_BIN_EXE_tw-lint"))
        .args([
            "--root",
            fixture.to_str().unwrap(),
            "--css",
            "tailwind.input.css",
            "--source",
            "src/**/*.rs",
            "--include-lang",
            "rust=html",
            "--class-regex",
            r#"tw!\s*\[([^\]]*)\]"#,
            "--class-regex",
            r#""([^"]*)""#,
        ])
        .status()
        .unwrap();
    assert!(!status.success(), "expected non-zero exit on dirty fixture");
}
