use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    for var in ["SVX_TRUSTED_ROOT", "SVX_INDEX_URL", "SVX_EXTRA_HOSTS", "SVX_RELEASE_BUILD"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root_path = env::var("SVX_TRUSTED_ROOT").map(PathBuf::from).unwrap_or_else(|_| manifest_dir.join("../../trust/test/1.root.json"));
    println!("cargo:rerun-if-changed={}", root_path.display());
    let raw = fs::read(&root_path).unwrap_or_else(|e| panic!("trusted root {}: {e}", root_path.display()));
    let root = svxdist::trust::verify_root_self(&raw).unwrap_or_else(|e| panic!("trusted root {} is invalid: {e}", root_path.display()));
    let test = svxdist::testkeys::is_test_root(&root);
    if env::var("SVX_RELEASE_BUILD").is_ok_and(|v| v == "1") && test {
        panic!("SVX_RELEASE_BUILD=1 refuses the test-only trusted root; set SVX_TRUSTED_ROOT to the production 1.root.json");
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("trusted_root.json"), &raw).unwrap();
    let index = env::var("SVX_INDEX_URL").unwrap_or_else(|_| "https://github.com/gicstin/SVX-Distribution/releases/download/index/index.json".into());
    println!("cargo:rustc-env=SVX_BUILD_INDEX_URL={index}");
    println!("cargo:rustc-env=SVX_BUILD_EXTRA_HOSTS={}", env::var("SVX_EXTRA_HOSTS").unwrap_or_default());
    println!("cargo:rustc-env=SVX_BUILD_TEST_ROOT={}", if test { "1" } else { "0" });
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=windows/svx-patcher.manifest");
        embed_resource::compile("windows/svx-patcher.rc", embed_resource::NONE).manifest_required().unwrap();
    }
}
