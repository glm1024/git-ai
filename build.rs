fn main() {
    let cargo_version = env!("CARGO_PKG_VERSION");
    // Cargo requires SemVer. Numeric build metadata represents the Git AI
    // release revision, which is exposed as a fourth component.
    let release_version = match cargo_version.split_once('+') {
        Some((base, revision))
            if !revision.is_empty() && revision.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            format!("{base}.{revision}")
        }
        _ => cargo_version.to_owned(),
    };
    println!("cargo:rustc-env=GIT_AI_RELEASE_VERSION={release_version}");
}
