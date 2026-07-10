use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
#[ignore = "requires tailwindcss-language-server on PATH (run under `nix develop`)"]
fn fix_rewrites_noncanonical_then_check_passes_for_that_class() {
    // Copy the fixture into a temp dir so the fix is non-destructive.
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/project");
    let tmp = tempfile::tempdir().unwrap();
    copy_dir(&src, tmp.path());

    let common = [
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
    ];

    let mut fix_args = common.to_vec();
    fix_args.push("--fix");
    Command::new(env!("CARGO_BIN_EXE_tw-lint"))
        .arg("--root")
        .arg(tmp.path())
        .args(&fix_args)
        .status()
        .unwrap();

    let fixed = fs::read_to_string(tmp.path().join("src/sample.rs")).unwrap();
    assert!(
        fixed.contains("w-full"),
        "w-[100%] should be rewritten to w-full, got:\n{fixed}"
    );
    assert!(!fixed.contains("w-[100%]"));
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    for entry in walkdir(from) {
        let rel = entry.strip_prefix(from).unwrap();
        let dest = to.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&dest).unwrap();
        } else {
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::copy(&entry, &dest).unwrap();
        }
    }
}

fn walkdir(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    out
}
