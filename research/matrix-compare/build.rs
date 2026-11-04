fn main() {
    println!("cargo:rerun-if-env-changed=L3I_BENCH_OPENBLAS");
    if std::env::var_os("CARGO_FEATURE_OPENBLAS").is_some() {
        let library = std::env::var("L3I_BENCH_OPENBLAS")
            .expect("set L3I_BENCH_OPENBLAS to an absolute OpenBLAS shared-library path");
        assert!(std::path::Path::new(&library).is_absolute());
        println!("cargo:rustc-link-arg={library}");
    }
}
