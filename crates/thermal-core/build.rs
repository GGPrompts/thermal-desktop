use ggl_build::RustCodegenConfig;

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let config = RustCodegenConfig {
        extra_derives: vec![
            "PartialEq".into(),
            "Eq".into(),
            "Hash".into(),
        ],
        // No serde tag needed — TaskState unit variants work without it,
        // and AgentId is a struct (tag only applies to enums).
        enum_serde_tag: None,
        ..Default::default()
    };
    ggl_build::build_ggl_with_config("schemas", &out_dir, &config)
        .expect("ggl codegen failed");
}
