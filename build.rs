use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SMOLWORLD_GIT_SHA");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");

    let sha = std::env::var("SMOLWORLD_GIT_SHA").ok().filter(|value| !value.is_empty()).or_else(|| {
        let output = Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        (!value.is_empty()).then_some(value)
    });

    let Some(sha) = sha.filter(|value| {
        value.len() >= 7 && value.chars().all(|character| character.is_ascii_hexdigit())
    }) else {
        panic!(
            "smolworld requires Git metadata; set SMOLWORLD_GIT_SHA to a non-empty short commit SHA when building from a source archive"
        );
    };

    println!("cargo:rustc-env=SMOLWORLD_GIT_SHA={sha}");
}
