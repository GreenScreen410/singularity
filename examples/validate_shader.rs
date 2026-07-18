// Offline WGSL validation: parse + validate singularity.wgsl without a GPU.
// Run: cargo run --example validate_shader
fn main() {
    let src = include_str!("../src/singularity.wgsl");
    let module = match naga::front::wgsl::parse_str(src) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("WGSL PARSE ERROR:\n{}", e.emit_to_string(src));
            std::process::exit(1);
        }
    };
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    );
    match validator.validate(&module) {
        Ok(_) => println!("WGSL OK — parsed and validated."),
        Err(e) => {
            eprintln!("WGSL VALIDATION ERROR:\n{}", e.emit_to_string(src));
            std::process::exit(1);
        }
    }
}
